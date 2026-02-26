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
        session
            .raw_send(
                "stty -echo\nexport PS1='' PS2='' PROMPT_COMMAND=''\nshopt -s expand_aliases\n",
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
    async fn drain_initial_output(&mut self) {
        let drain_id = uuid::Uuid::new_v4().to_string();
        let drain_cmd = format!("echo '{MARKER_PREFIX}DRAIN_{drain_id}__'\n");
        if self.raw_send(&drain_cmd).await.is_err() {
            return;
        }

        let target = format!("{MARKER_PREFIX}DRAIN_{drain_id}__");
        let mut line = String::new();
        loop {
            line.clear();
            match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                self.reader.read_line(&mut line),
            )
            .await
            {
                Ok(Ok(0)) => break,
                Ok(Ok(_)) => {
                    let cleaned = clean_line(&line);
                    if cleaned == target {
                        break;
                    }
                }
                _ => break,
            }
        }
    }

    /// Execute a command in this session and return the result.
    async fn exec(
        &mut self,
        command: &str,
        timeout_seconds: Option<u64>,
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
        let mut line = String::new();

        loop {
            line.clear();
            let read_result = tokio::time::timeout(
                std::time::Duration::from_secs(timeout),
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
                        if cleaned == start_marker {
                            found_start = true;
                        }
                        continue;
                    }

                    // Check for end marker.
                    if cleaned.starts_with(&end_marker_prefix) && cleaned.ends_with("__") {
                        let code_str = &cleaned[end_marker_prefix.len()..cleaned.len() - 2];
                        exit_code = code_str.parse::<i32>().unwrap_or(-1);
                        break;
                    }

                    // Skip lines that look like our internal markers/commands.
                    if cleaned.starts_with(MARKER_PREFIX) {
                        continue;
                    }

                    // Regular output line.
                    if lines.len() < MAX_OUTPUT_LINES {
                        lines.push(cleaned);
                    }
                }
                Ok(Err(e)) => {
                    return Err(format!("error reading PTY output: {e}"));
                }
                Err(_) => {
                    // Timeout -- send SIGINT to interrupt the foreground command.
                    if let Some(pid) = self.child.id() {
                        let _ = nix::sys::signal::kill(
                            nix::unistd::Pid::from_raw(pid as i32),
                            nix::sys::signal::Signal::SIGINT,
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        let _ = nix::sys::signal::kill(
                            nix::unistd::Pid::from_raw(-(pid as i32)),
                            nix::sys::signal::Signal::SIGTERM,
                        );
                    }
                    timed_out = true;

                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

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
                                if cleaned == recovery_marker {
                                    break;
                                }
                                if cleaned.starts_with(&end_marker_prefix) {
                                    break;
                                }
                                if lines.len() < MAX_OUTPUT_LINES
                                    && !cleaned.starts_with(MARKER_PREFIX)
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

    /// Check if the bash process is still alive.
    fn is_alive(&mut self) -> bool {
        match self.child.try_wait() {
            Ok(None) => true,
            Ok(Some(_)) => false,
            Err(_) => false,
        }
    }
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
    ) -> Result<SessionExecResult, String> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(id)
            .ok_or_else(|| format!("no session with id '{id}'"))?;

        if !session.is_alive() {
            return Err(format!("session '{id}' is dead (bash process exited)"));
        }

        let mut result = session.exec(command, timeout_seconds).await?;
        result.session_id = id.to_string();
        Ok(result)
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
