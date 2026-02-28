//! Persistent shell sessions backed by a pseudo-terminal (PTY).
//!
//! Each session is a long-lived bash process attached to a real PTY. Child
//! processes see `isatty()=true`, so tools like `claude` CLI, colored output,
//! and interactive programs work correctly. Commands are delimited with UUID
//! markers, and ANSI escape codes are stripped from output for clean LLM
//! consumption. The session preserves working directory, environment variables,
//! and shell state between commands.

use crate::output;
use crate::process::{self, MAX_TIMEOUT_SECONDS};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

/// Maximum number of concurrent sessions.
const MAX_SESSIONS: usize = 10;

/// Maximum output lines per command within a session.
const MAX_OUTPUT_LINES: usize = 100_000;

/// Marker prefix used to delimit command output in the session's PTY stream.
const MARKER_PREFIX: &str = "__AGENTSH_";

/// Result of executing a command in a session.
#[derive(Debug, Clone, Serialize)]
pub struct SessionExecResult {
    pub session_id: String,
    pub exit_code: i32,
    pub duration_seconds: f64,
    pub lines: Vec<String>,
    pub timed_out: bool,
}

/// Information about a session.
#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub id: String,
    pub alive: bool,
}

/// A persistent shell session backed by a PTY.
///
/// The bash process is attached to a pseudo-terminal, so child processes see
/// a real terminal (`isatty()=true`). We read and write to the PTY master fd.
struct ShellSession {
    child: tokio::process::Child,
    writer: pty_process::OwnedWritePty,
    reader: BufReader<pty_process::OwnedReadPty>,
}

impl ShellSession {
    /// Spawn a new PTY-backed bash session.
    async fn new(working_directory: Option<&str>) -> Result<Self, String> {
        // Allocate a PTY pair (master + slave).
        let (pty, pts) = pty_process::open().map_err(|e| format!("failed to open PTY: {e}"))?;

        // Set a wide terminal to avoid line wrapping in output.
        pty.resize(pty_process::Size::new(24, 250))
            .map_err(|e| format!("failed to resize PTY: {e}"))?;

        // Build the command. pty_process::Command uses a consuming builder pattern.
        let mut cmd = pty_process::Command::new("/bin/bash")
            .arg("--norc")
            .arg("--noprofile");

        // Sanitize environment (opt-in stripping via AGENTSH_STRIP_ENV).
        for (key, _) in std::env::vars() {
            if process::is_sensitive_env(&key) {
                cmd = cmd.env_remove(&key);
            }
        }

        if let Some(dir) = working_directory {
            cmd = cmd.current_dir(dir);
        }

        // Spawn with the PTY slave -- child gets it as its controlling terminal.
        let child = cmd
            .spawn(pts)
            .map_err(|e| format!("failed to spawn bash with PTY: {e}"))?;

        // Split the PTY master into owned read and write halves.
        let (read_pty, write_pty) = pty.into_split();
        let reader = BufReader::new(read_pty);

        let mut session = Self {
            child,
            writer: write_pty,
            reader,
        };

        // Disable terminal echo so our commands don't appear in the output.
        // Disable PS1/PS2 prompts. Enable alias expansion.
        // Set PAGER=cat so programs don't launch interactive pagers (like less)
        // even though isatty()=true -- the PTY is for tool compatibility, not
        // human interaction.
        session
            .raw_send(
                "stty -echo\nexport PS1='' PS2='' PROMPT_COMMAND='' PAGER=cat GIT_PAGER=cat\nshopt -s expand_aliases\n",
            )
            .await?;

        // Drain any initial output (bash startup, stty response, etc.).
        session.drain_initial_output().await;

        Ok(session)
    }

    /// Send raw text to the PTY (bash's stdin).
    async fn raw_send(&mut self, text: &str) -> Result<(), String> {
        self.writer
            .write_all(text.as_bytes())
            .await
            .map_err(|e| format!("failed to write to PTY: {e}"))?;
        self.writer
            .flush()
            .await
            .map_err(|e| format!("failed to flush PTY: {e}"))
    }

    /// Drain initial output after session creation.
    ///
    /// Uses a total timeout (not per-line) because `read_line` on a PTY can
    /// block indefinitely when bash emits a prompt without a trailing newline
    /// -- the per-read timeout never fires since bytes ARE arriving.
    async fn drain_initial_output(&mut self) {
        let drain_id = uuid::Uuid::new_v4().to_string();
        let drain_cmd = format!("echo '{MARKER_PREFIX}DRAIN_{drain_id}__'\n");
        if self.raw_send(&drain_cmd).await.is_err() {
            return;
        }

        let target = format!("{MARKER_PREFIX}DRAIN_{drain_id}__");

        // Wrap the entire drain in a total timeout so we never hang.
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            Self::read_until_marker(&mut self.reader, &target),
        )
        .await;
    }

    /// Read lines from the PTY until the marker is found or EOF.
    async fn read_until_marker(
        reader: &mut BufReader<pty_process::OwnedReadPty>,
        target: &str,
    ) {
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {
                    if clean_line(&line) == target {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    }

    /// Execute a command in this session and return the result.
    async fn exec(
        &mut self,
        command: &str,
        timeout_seconds: Option<u64>,
        idle_timeout_seconds: Option<u64>,
    ) -> Result<SessionExecResult, String> {
        // Validate dangerous commands.
        if let Err(reason) = process::validate_command(command) {
            return Ok(SessionExecResult {
                session_id: String::new(),
                exit_code: -1,
                duration_seconds: 0.0,
                lines: vec![reason],
                timed_out: false,
            });
        }

        let cmd_id = uuid::Uuid::new_v4().to_string();
        let start_marker = format!("{MARKER_PREFIX}START_{cmd_id}__");
        let end_marker_prefix = format!("{MARKER_PREFIX}END_{cmd_id}_");

        // Wrapper: run command in { } group (not subshell) so state persists.
        // stderr is merged with stdout via 2>&1.
        // With PTY + `stty -echo`, our commands are NOT echoed back.
        let wrapper = format!(
            "echo '{start_marker}'\n\
             {{ {command}; }} 2>&1\n\
             __agentsh_ec=$?\n\
             echo '{end_marker_prefix}'\"$__agentsh_ec\"'__'\n"
        );

        self.raw_send(&wrapper).await?;

        let start = Instant::now();
        let timeout = timeout_seconds
            .map(|t| t.min(MAX_TIMEOUT_SECONDS))
            .unwrap_or(300);

        let mut lines: Vec<String> = Vec::new();
        #[allow(unused_assignments)]
        let mut exit_code: i32 = -1;
        let mut found_start = false;
        let mut timed_out = false;
        let mut has_output = false;
        let mut line = String::new();

        loop {
            // After receiving output, use idle timeout if set. This lets us
            // return early for commands that produce output but don't exit
            // (e.g. `claude -p`).
            let read_timeout = if has_output && idle_timeout_seconds.is_some() {
                idle_timeout_seconds.unwrap()
            } else {
                timeout
            };

            line.clear();
            let read_result = tokio::time::timeout(
                std::time::Duration::from_secs(read_timeout),
                self.reader.read_line(&mut line),
            )
            .await;

            match read_result {
                Ok(Ok(0)) => {
                    return Err("session bash process exited unexpectedly".to_string());
                }
                Ok(Ok(_)) => {
                    let cleaned = clean_line(&line);

                    // Skip empty lines from PTY noise before start marker.
                    if cleaned.is_empty() && !found_start {
                        continue;
                    }

                    if !found_start {
                        if cleaned.contains(&start_marker) {
                            found_start = true;
                        }
                        continue;
                    }

                    // Check for end marker. Use `find` instead of `starts_with`
                    // because programs (e.g. claude CLI) can leave stray ANSI
                    // escape sequences that the stripper doesn't fully remove.
                    if let Some(pos) = cleaned.find(&end_marker_prefix) {
                        let after = &cleaned[pos + end_marker_prefix.len()..];
                        if after.ends_with("__") {
                            let code_str = &after[..after.len() - 2];
                            exit_code = code_str.parse::<i32>().unwrap_or(-1);
                            break;
                        }
                    }

                    // Skip lines that look like our internal markers/commands.
                    if cleaned.contains(MARKER_PREFIX) {
                        continue;
                    }

                    // Regular output line.
                    has_output = true;
                    if lines.len() < MAX_OUTPUT_LINES {
                        lines.push(cleaned);
                    }
                }
                Ok(Err(e)) => {
                    return Err(format!("error reading PTY output: {e}"));
                }
                Err(_) => {
                    // Timeout -- send Ctrl+C via the PTY to interrupt the
                    // foreground command. This is more surgical than kill():
                    // only the foreground process receives SIGINT, bash stays
                    // alive so the session remains usable after timeout.
                    let _ = self.raw_send("\x03").await;
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    let _ = self.raw_send("\x03").await; // double Ctrl+C for stubborn processes
                    timed_out = true;

                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

                    // Recovery marker to re-sync.
                    let recovery_id = uuid::Uuid::new_v4().to_string();
                    let recovery_marker = format!("{MARKER_PREFIX}RECOVER_{recovery_id}__");
                    let _ = self
                        .raw_send(&format!("\necho '{recovery_marker}'\n"))
                        .await;

                    loop {
                        line.clear();
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(3),
                            self.reader.read_line(&mut line),
                        )
                        .await
                        {
                            Ok(Ok(n)) if n > 0 => {
                                let cleaned = clean_line(&line);
                                if cleaned.contains(&recovery_marker) {
                                    break;
                                }
                                if cleaned.contains(&end_marker_prefix) {
                                    break;
                                }
                                if lines.len() < MAX_OUTPUT_LINES
                                    && !cleaned.contains(MARKER_PREFIX)
                                    && !cleaned.is_empty()
                                {
                                    lines.push(cleaned);
                                }
                            }
                            _ => break,
                        }
                    }

                    exit_code = 124;
                    break;
                }
            }
        }

        Ok(SessionExecResult {
            session_id: String::new(),
            exit_code,
            duration_seconds: start.elapsed().as_secs_f64(),
            lines,
            timed_out,
        })
    }

    /// Send raw input to the PTY and read output until it settles.
    ///
    /// Unlike `exec`, this does not wrap commands in markers or extract exit
    /// codes. It interacts with the terminal the way a human would: type
    /// something, wait for the screen to stop changing, read what's there.
    async fn send(&mut self, input: Option<&str>, idle_timeout_secs: u64) -> Vec<String> {
        use tokio::io::AsyncReadExt;

        if let Some(text) = input {
            let bytes = process_escapes(text);
            self.writer
                .write_all(&bytes)
                .await
                .ok();
            self.writer.flush().await.ok();
        }

        let mut accumulated = Vec::<u8>::new();
        let mut buf = [0u8; 4096];
        let idle_timeout = std::time::Duration::from_secs(idle_timeout_secs);
        let chunk_timeout = std::time::Duration::from_millis(200);
        let mut last_meaningful_change = Instant::now();
        let mut prev_len: usize = 0;
        let start = Instant::now();

        loop {
            match tokio::time::timeout(chunk_timeout, self.reader.read(&mut buf)).await {
                Ok(Ok(0)) => break,
                Ok(Ok(n)) => {
                    accumulated.extend_from_slice(&buf[..n]);
                    // Track when output meaningfully grew (not just cursor noise).
                    // TUI apps send tiny updates (1-5 bytes) for cursor/status;
                    // real output tends to arrive in larger chunks.
                    if accumulated.len() - prev_len > 10 {
                        last_meaningful_change = Instant::now();
                        prev_len = accumulated.len();
                    }
                }
                Ok(Err(_)) => break,
                Err(_) => {} // no data this chunk, fall through to checks below
            }

            // Return once output has settled (no meaningful change for idle_timeout).
            if !accumulated.is_empty() && last_meaningful_change.elapsed() >= idle_timeout {
                break;
            }

            // Hard cap: never wait longer than 5x the idle timeout.
            let max_total = idle_timeout.saturating_mul(5).max(std::time::Duration::from_secs(30));
            if start.elapsed() >= max_total {
                break;
            }
        }

        let raw = String::from_utf8_lossy(&accumulated);
        raw.lines()
            .map(|l| clean_line(l))
            .filter(|l| !l.is_empty())
            .collect()
    }

    /// Check if the bash process is still alive.
    fn is_alive(&mut self) -> bool {
        match self.child.try_wait() {
            Ok(None) => true,
            Ok(Some(_)) => false,
            Err(_) => false,
        }
    }
}

/// Process escape sequences in input text so agents can send control characters.
///
/// MCP tool parameters arrive as literal strings -- `\n` is two characters
/// (backslash + n), not a newline byte. This converts common escape sequences
/// to their byte values so agents can press Enter, send Ctrl+C, etc.
fn process_escapes(input: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push(b'\n'),
                Some('r') => out.push(b'\r'),
                Some('t') => out.push(b'\t'),
                Some('\\') => out.push(b'\\'),
                Some('x') => {
                    // \xNN -- two hex digits
                    let mut hex = String::new();
                    if let Some(h1) = chars.next() {
                        hex.push(h1);
                    }
                    if let Some(h2) = chars.next() {
                        hex.push(h2);
                    }
                    if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                        out.push(byte);
                    }
                }
                Some(other) => {
                    out.push(b'\\');
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
                }
                None => out.push(b'\\'),
            }
        } else {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
    }
    out
}

/// Clean a line read from the PTY: strip ANSI escape codes and trailing whitespace.
fn clean_line(raw: &str) -> String {
    let stripped = output::strip_ansi(raw);
    stripped
        .trim_end_matches('\n')
        .trim_end_matches('\r')
        .to_string()
}

/// Manager for multiple shell sessions.
#[derive(Clone)]
pub struct SessionManager {
    sessions: Arc<Mutex<HashMap<String, ShellSession>>>,
}

impl SessionManager {
    /// Create a new session manager.
    #[must_use]
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Create a new session with the given ID.
    pub async fn create(
        &self,
        id: String,
        working_directory: Option<String>,
    ) -> Result<SessionInfo, String> {
        let mut sessions = self.sessions.lock().await;

        if sessions.contains_key(&id) {
            return Err(format!("session '{id}' already exists"));
        }

        if sessions.len() >= MAX_SESSIONS {
            return Err(format!(
                "too many sessions ({}/{}). Close some sessions first.",
                sessions.len(),
                MAX_SESSIONS
            ));
        }

        let session = ShellSession::new(working_directory.as_deref()).await?;
        sessions.insert(id.clone(), session);

        Ok(SessionInfo { id, alive: true })
    }

    /// Execute a command in a session.
    pub async fn exec(
        &self,
        id: &str,
        command: &str,
        timeout_seconds: Option<u64>,
        idle_timeout_seconds: Option<u64>,
    ) -> Result<SessionExecResult, String> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(id)
            .ok_or_else(|| format!("no session with id '{id}'"))?;

        if !session.is_alive() {
            return Err(format!("session '{id}' is dead (bash process exited)"));
        }

        let mut result = session.exec(command, timeout_seconds, idle_timeout_seconds).await?;
        result.session_id = id.to_string();
        Ok(result)
    }

    /// Send raw input and read output from a session.
    pub async fn send(
        &self,
        id: &str,
        input: Option<&str>,
        idle_timeout_secs: u64,
    ) -> Result<Vec<String>, String> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(id)
            .ok_or_else(|| format!("no session with id '{id}'"))?;

        if !session.is_alive() {
            return Err(format!("session '{id}' is dead (bash process exited)"));
        }

        Ok(session.send(input, idle_timeout_secs).await)
    }

    /// List all sessions.
    pub async fn list(&self) -> Vec<SessionInfo> {
        let mut sessions = self.sessions.lock().await;
        sessions
            .iter_mut()
            .map(|(id, session)| SessionInfo {
                id: id.clone(),
                alive: session.is_alive(),
            })
            .collect()
    }

    /// Close a session.
    pub async fn close(&self, id: &str) -> Result<(), String> {
        let mut sessions = self.sessions.lock().await;
        let mut session = sessions
            .remove(id)
            .ok_or_else(|| format!("no session with id '{id}'"))?;

        // Ask bash to exit gracefully.
        let _ = session.raw_send("exit\n").await;

        // Destructure to drop PTY handles before waiting -- closing the master
        // fd sends SIGHUP to bash, which unblocks the wait below. Without this,
        // child.wait() can hang indefinitely because the PTY keeps the process
        // alive.
        let ShellSession {
            mut child,
            writer,
            reader,
        } = session;
        drop(writer);
        drop(reader);

        // Wait for graceful exit with a bounded timeout.
        if tokio::time::timeout(std::time::Duration::from_secs(2), child.wait())
            .await
            .is_err()
        {
            let _ = child.start_kill();
            let _ =
                tokio::time::timeout(std::time::Duration::from_secs(1), child.wait()).await;
        }

        Ok(())
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}
