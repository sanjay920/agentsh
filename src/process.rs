//! Process spawning, waiting, and output capture via tokio.
//!
//! This module handles the core responsibility of agentsh: running shell commands
//! efficiently with async I/O, capturing stdout+stderr, and respecting timeouts.
//! Includes security hardening: env var sanitization, output buffer caps,
//! timeout ceilings, and dangerous command blocking.

use regex::Regex;
use serde::Serialize;
use std::process::Stdio;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Mutex;

/// Maximum number of output lines kept in the buffer. Prevents OOM from
/// commands that produce infinite output (e.g., `yes`, `cat /dev/urandom`).
const MAX_OUTPUT_LINES: usize = 100_000;

/// Maximum allowed timeout in seconds (1 hour).
pub const MAX_TIMEOUT_SECONDS: u64 = 3600;

/// Returns the set of env var names to strip, if any.
///
/// By default, child processes inherit the FULL environment from agentsh (which
/// inherits from the user's terminal). This matches how iTerm2, Terminal.app,
/// and Cursor's built-in shell work -- the user's PATH, API keys, and all env
/// vars are available.
///
/// Set `AGENTSH_STRIP_ENV` to a comma-separated list of env var names to
/// explicitly strip from child processes. Example:
///   `AGENTSH_STRIP_ENV=OPENAI_API_KEY,DATABASE_URL`
fn stripped_env_vars() -> &'static std::collections::HashSet<String> {
    static STRIPPED: LazyLock<std::collections::HashSet<String>> = LazyLock::new(|| {
        std::env::var("AGENTSH_STRIP_ENV")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_uppercase())
            .filter(|s| !s.is_empty())
            .collect()
    });
    &STRIPPED
}

/// Returns true if an environment variable should be stripped from child processes.
///
/// Only strips vars explicitly listed in `AGENTSH_STRIP_ENV`. By default, nothing
/// is stripped -- the full environment is inherited, just like a real terminal.
pub fn is_sensitive_env(name: &str) -> bool {
    let stripped = stripped_env_vars();
    if stripped.is_empty() {
        return false;
    }
    stripped.contains(&name.to_uppercase())
}

/// Clamp a timeout value to the allowed ceiling.
#[must_use]
pub fn clamp_timeout(timeout: Option<u64>) -> Option<u64> {
    timeout.map(|t| t.min(MAX_TIMEOUT_SECONDS))
}

// ---------------------------------------------------------------------------
// Dangerous command detection
// ---------------------------------------------------------------------------

/// System-critical paths that should never be the target of recursive delete,
/// chmod, or chown operations.
const PROTECTED_PATHS: &[&str] = &[
    "/",
    "/*",
    "/bin",
    "/sbin",
    "/usr",
    "/etc",
    "/var",
    "/home",
    "/root",
    "/lib",
    "/lib64",
    "/opt",
    "/boot",
    "/dev",
    "/sys",
    "/proc",
    "/System",
    "/Library",
    "/Applications",
    "/Users",
    "/private",
    "/private/var",
    "/private/etc",
];

/// Compiled patterns for dangerous commands. Built once, reused on every check.
static DANGEROUS_PATTERNS: LazyLock<Vec<DangerousPattern>> = LazyLock::new(|| {
    vec![
        // Fork bombs
        DangerousPattern {
            regex: Regex::new(r":\(\)\s*\{.*\|.*&\s*\}\s*;").unwrap(),
            description: "fork bomb",
        },
        // mkfs on any device
        DangerousPattern {
            regex: Regex::new(r"\bmkfs\b").unwrap(),
            description: "filesystem format (mkfs)",
        },
        // dd writing to block devices
        DangerousPattern {
            regex: Regex::new(r"\bdd\b.*\bof=/dev/").unwrap(),
            description: "raw write to block device (dd of=/dev/...)",
        },
        // Overwrite block devices via redirect
        DangerousPattern {
            regex: Regex::new(r">\s*/dev/(sd|nvme|hd|vd|xvd|disk|mapper/)").unwrap(),
            description: "redirect to block device",
        },
        // shutdown / reboot / halt / poweroff
        DangerousPattern {
            regex: Regex::new(r"\b(shutdown|reboot|halt|poweroff)\b").unwrap(),
            description: "system shutdown/reboot",
        },
        // init 0 or init 6
        DangerousPattern {
            regex: Regex::new(r"\binit\s+[06]\b").unwrap(),
            description: "system halt/reboot via init",
        },
    ]
});

struct DangerousPattern {
    regex: Regex,
    description: &'static str,
}

/// Validate a command against dangerous patterns. Returns `Ok(())` if safe,
/// or `Err(description)` if the command matches a dangerous pattern.
pub fn validate_command(command: &str) -> Result<(), String> {
    // Check regex-based patterns (fork bombs, mkfs, dd, shutdown, etc.)
    for pattern in DANGEROUS_PATTERNS.iter() {
        if pattern.regex.is_match(command) {
            return Err(format!(
                "blocked: command matches dangerous pattern ({}): {}",
                pattern.description, command
            ));
        }
    }

    // Check for recursive delete/chmod/chown targeting protected paths.
    check_destructive_on_protected_paths(command)?;

    Ok(())
}

/// Check if a command performs recursive destructive operations on protected paths.
fn check_destructive_on_protected_paths(command: &str) -> Result<(), String> {
    // Normalize: collapse multiple spaces, trim.
    let normalized = command.trim();

    // Split on common command separators to check each subcommand.
    for subcmd in split_subcommands(normalized) {
        let subcmd = subcmd.trim();
        if subcmd.is_empty() {
            continue;
        }

        // rm -rf / rm -fr / rm --recursive --force targeting protected paths
        if is_dangerous_rm(subcmd) {
            return Err(format!(
                "blocked: recursive delete targeting a protected system path: {subcmd}"
            ));
        }

        // chmod -R on protected paths
        if is_dangerous_chmod_chown(subcmd, "chmod") {
            return Err(format!(
                "blocked: recursive chmod on a protected system path: {subcmd}"
            ));
        }

        // chown -R on protected paths
        if is_dangerous_chmod_chown(subcmd, "chown") {
            return Err(format!(
                "blocked: recursive chown on a protected system path: {subcmd}"
            ));
        }
    }

    Ok(())
}

/// Split a command string on shell operators (;, &&, ||, |) to get individual commands.
fn split_subcommands(cmd: &str) -> Vec<&str> {
    // Simple split on ; && || -- good enough for catching obvious patterns.
    // Not a full shell parser, but catches the common cases.
    let mut parts = Vec::new();
    let mut remaining = cmd;
    while !remaining.is_empty() {
        if let Some(pos) = remaining
            .find("&&")
            .into_iter()
            .chain(remaining.find("||"))
            .chain(remaining.find(';'))
            .min()
        {
            parts.push(&remaining[..pos]);
            // Skip the separator (1 for ;, 2 for && or ||)
            let sep_len =
                if remaining[pos..].starts_with("&&") || remaining[pos..].starts_with("||") {
                    2
                } else {
                    1
                };
            remaining = &remaining[pos + sep_len..];
        } else {
            parts.push(remaining);
            break;
        }
    }
    parts
}

/// Check if a subcmd is a dangerous `rm` invocation targeting protected paths.
fn is_dangerous_rm(subcmd: &str) -> bool {
    let words: Vec<&str> = subcmd.split_whitespace().collect();

    // Find `rm` (possibly prefixed with sudo, env, etc.)
    let rm_pos = words.iter().position(|w| *w == "rm");
    let rm_pos = match rm_pos {
        Some(p) => p,
        None => return false,
    };

    let args = &words[rm_pos + 1..];

    // Check if -r/-R/--recursive and -f/--force are present.
    let has_recursive = args.iter().any(|a| {
        *a == "-r"
            || *a == "-R"
            || *a == "--recursive"
            || a.starts_with('-') && !a.starts_with("--") && (a.contains('r') || a.contains('R'))
    });

    if !has_recursive {
        return false;
    }

    // Check if any argument is a protected path.
    for arg in args {
        if arg.starts_with('-') {
            continue;
        }
        let path = arg.trim_end_matches('/');
        let path_with_slash = if path.is_empty() { "/" } else { path };
        for protected in PROTECTED_PATHS {
            let protected_trimmed = protected.trim_end_matches('/');
            let protected_cmp = if protected_trimmed.is_empty() {
                "/"
            } else {
                protected_trimmed
            };
            if path_with_slash == protected_cmp || *arg == "/*" {
                return true;
            }
        }
    }

    false
}

/// Check if a subcmd is a dangerous recursive chmod/chown on protected paths.
fn is_dangerous_chmod_chown(subcmd: &str, cmd_name: &str) -> bool {
    let words: Vec<&str> = subcmd.split_whitespace().collect();

    let cmd_pos = words.iter().position(|w| *w == cmd_name);
    let cmd_pos = match cmd_pos {
        Some(p) => p,
        None => return false,
    };

    let args = &words[cmd_pos + 1..];

    let has_recursive = args.iter().any(|a| {
        *a == "-R"
            || *a == "--recursive"
            || a.starts_with('-') && !a.starts_with("--") && a.contains('R')
    });

    if !has_recursive {
        return false;
    }

    for arg in args {
        if arg.starts_with('-') {
            continue;
        }
        let path = arg.trim_end_matches('/');
        let path_with_slash = if path.is_empty() { "/" } else { path };
        for protected in PROTECTED_PATHS {
            let protected_trimmed = protected.trim_end_matches('/');
            let protected_cmp = if protected_trimmed.is_empty() {
                "/"
            } else {
                protected_trimmed
            };
            if path_with_slash == protected_cmp {
                return true;
            }
        }
    }

    false
}

/// Result of a completed process execution.
#[derive(Debug, Clone, Serialize)]
pub struct ProcessResult {
    /// Process exit code, or -1 if killed/unknown.
    pub exit_code: i32,
    /// Wall-clock duration of execution in seconds.
    pub duration_seconds: f64,
    /// All captured output lines (stdout + stderr interleaved).
    pub lines: Vec<String>,
    /// Whether the process was killed due to timeout.
    pub timed_out: bool,
}

/// Configuration for spawning a process.
#[derive(Debug, Clone)]
pub struct ProcessConfig {
    /// Shell command to execute (passed to `/bin/sh -c`).
    pub command: String,
    /// Working directory for the command. `None` uses the server's cwd.
    pub working_directory: Option<String>,
    /// Maximum execution time in seconds. `None` means no timeout.
    pub timeout_seconds: Option<u64>,
}

/// A shared output buffer that can be read while the process is still running.
pub type SharedOutputBuffer = Arc<Mutex<Vec<String>>>;

/// Create a new shared output buffer.
#[must_use]
pub fn new_shared_buffer() -> SharedOutputBuffer {
    Arc::new(Mutex::new(Vec::new()))
}

/// Spawn a process and wait for it to complete, capturing all output.
///
/// Output is captured line-by-line from both stdout and stderr into a single
/// interleaved buffer. If a `shared_buffer` is provided, lines are also
/// appended there in real time (for status checks while running).
pub async fn run(
    config: &ProcessConfig,
    shared_buffer: Option<&SharedOutputBuffer>,
) -> ProcessResult {
    let start = Instant::now();

    // Block dangerous commands before they execute.
    if let Err(reason) = validate_command(&config.command) {
        tracing::warn!(command = %config.command, reason = %reason, "dangerous command blocked");
        return ProcessResult {
            exit_code: -1,
            duration_seconds: start.elapsed().as_secs_f64(),
            lines: vec![reason],
            timed_out: false,
        };
    }

    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(&config.command);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);

    // Sanitize environment: remove sensitive variables (API keys, tokens, etc.)
    // so prompt-injected commands can't exfiltrate them.
    for (key, _) in std::env::vars() {
        if is_sensitive_env(&key) {
            cmd.env_remove(&key);
        }
    }

    // Start a new process group so we can kill the whole tree.
    // SAFETY: pre_exec runs before exec in the child process.
    unsafe {
        cmd.pre_exec(|| {
            nix::unistd::setsid().map_err(std::io::Error::other)?;
            Ok(())
        });
    }

    if let Some(dir) = &config.working_directory {
        cmd.current_dir(dir);
    }

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            return ProcessResult {
                exit_code: -1,
                duration_seconds: start.elapsed().as_secs_f64(),
                lines: vec![format!("Failed to spawn process: {e}")],
                timed_out: false,
            };
        }
    };

    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");

    let buffer: SharedOutputBuffer = shared_buffer.cloned().unwrap_or_else(new_shared_buffer);

    // Spawn tasks to read stdout and stderr concurrently.
    // Output is capped at MAX_OUTPUT_LINES to prevent OOM.
    let buf_stdout = buffer.clone();
    let stdout_task = tokio::spawn(async move {
        let reader = BufReader::new(stdout);
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let mut buf = buf_stdout.lock().await;
            if buf.len() < MAX_OUTPUT_LINES {
                buf.push(line);
            }
            // Past the cap we still drain the pipe (so the child doesn't block)
            // but discard the data.
        }
    });

    let buf_stderr = buffer.clone();
    let stderr_task = tokio::spawn(async move {
        let reader = BufReader::new(stderr);
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let mut buf = buf_stderr.lock().await;
            if buf.len() < MAX_OUTPUT_LINES {
                buf.push(line);
            }
        }
    });

    // Wait for the child process, with optional timeout.
    let (timed_out, exit_code) = if let Some(secs) = config.timeout_seconds {
        match tokio::time::timeout(Duration::from_secs(secs), child.wait()).await {
            Ok(Ok(status)) => (false, status.code().unwrap_or(-1)),
            Ok(Err(_)) => (false, -1),
            Err(_) => {
                // Timeout: kill the process group.
                let _ = kill_process(&child);
                let _ = child.wait().await;
                (true, -1)
            }
        }
    } else {
        match child.wait().await {
            Ok(status) => (false, status.code().unwrap_or(-1)),
            Err(_) => (false, -1),
        }
    };

    // Wait for output readers to finish.
    let _ = stdout_task.await;
    let _ = stderr_task.await;

    let lines = buffer.lock().await.clone();

    ProcessResult {
        exit_code,
        duration_seconds: start.elapsed().as_secs_f64(),
        lines,
        timed_out,
    }
}

/// Send a signal to the process group of a child process.
///
/// Uses the child's PID as the process group ID (since we called `setsid`).
pub fn kill_process(child: &tokio::process::Child) -> Result<(), String> {
    let pid = child
        .id()
        .ok_or_else(|| "process has no PID (already exited?)".to_string())?;

    // Kill the entire process group (negative PID).
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(-(pid as i32)),
        nix::sys::signal::Signal::SIGKILL,
    )
    .map_err(|e| format!("failed to kill process group: {e}"))
}
