//! Process registry: tracks running and completed processes by ID.
//!
//! The registry stores process entries keyed by a string ID, allowing the MCP server
//! to start commands asynchronously and later wait for, check status of, or kill them.
//! Completed entries are retained so output can be retrieved via `get_output`, and
//! are automatically cleaned up after a configurable TTL.

use crate::output;
use crate::process::{self, ProcessConfig, ProcessResult, SharedOutputBuffer};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

/// How long completed entries are retained before automatic cleanup.
const COMPLETED_TTL: Duration = Duration::from_secs(30 * 60); // 30 minutes

/// Maximum number of concurrently running processes. Prevents resource exhaustion
/// from an agent calling start_command in a tight loop.
const MAX_CONCURRENT_PROCESSES: usize = 20;

/// The status of a tracked process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessStatus {
    Running,
    Completed,
    Failed,
    TimedOut,
}

/// Summary information about a tracked process (for list_commands).
#[derive(Debug, Clone, Serialize)]
pub struct ProcessSummary {
    pub id: String,
    pub command: String,
    pub status: ProcessStatus,
    pub runtime_seconds: f64,
}

/// Internal entry for a tracked process.
struct ProcessEntry {
    command: String,
    start_time: Instant,
    /// When the process completed (for TTL cleanup).
    completed_at: Option<Instant>,
    /// Shared output buffer, written to in real time by the process runner.
    output_buffer: SharedOutputBuffer,
    /// Handle to the async task running the process. `None` once awaited.
    join_handle: Option<JoinHandle<ProcessResult>>,
    /// Cached result after process completes.
    result: Option<ProcessResult>,
    /// Max output lines for windowing.
    max_output_lines: usize,
}

/// Thread-safe registry of running and completed processes.
#[derive(Clone)]
pub struct ProcessRegistry {
    entries: Arc<Mutex<HashMap<String, ProcessEntry>>>,
}

impl ProcessRegistry {
    /// Create a new empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Prune completed entries older than the TTL.
    async fn prune_expired(&self) {
        let mut entries = self.entries.lock().await;
        entries.retain(|_id, entry| {
            if let Some(completed_at) = entry.completed_at {
                completed_at.elapsed() < COMPLETED_TTL
            } else {
                true // Keep running entries.
            }
        });
    }

    /// Store a completed result directly (used by `run_command` to make output retrievable).
    pub async fn store_result(
        &self,
        id: String,
        command: String,
        result: ProcessResult,
        max_output_lines: usize,
    ) {
        self.prune_expired().await;
        let mut entries = self.entries.lock().await;
        entries.insert(
            id,
            ProcessEntry {
                command,
                start_time: Instant::now(),
                completed_at: Some(Instant::now()),
                output_buffer: process::new_shared_buffer(),
                join_handle: None,
                result: Some(result),
                max_output_lines,
            },
        );
    }

    /// Start a command asynchronously, returning the assigned ID and PID.
    ///
    /// The process runs in a background tokio task. Use [`wait`] to block until
    /// it completes, or [`status`] to check without blocking.
    pub async fn start(
        &self,
        id: String,
        config: ProcessConfig,
        max_output_lines: usize,
    ) -> Result<(String, Option<u32>), String> {
        self.prune_expired().await;
        let mut entries = self.entries.lock().await;
        if entries.contains_key(&id) {
            return Err(format!("process with id '{id}' already exists"));
        }

        // Enforce concurrent process limit.
        let running_count = entries.values().filter(|e| e.result.is_none()).count();
        if running_count >= MAX_CONCURRENT_PROCESSES {
            return Err(format!(
                "too many concurrent processes ({running_count}/{MAX_CONCURRENT_PROCESSES}). \
                 Wait for some to complete or kill running processes."
            ));
        }

        let shared_buffer = process::new_shared_buffer();
        let buffer_clone = shared_buffer.clone();
        let config_clone = config.clone();

        let handle =
            tokio::spawn(async move { process::run(&config_clone, Some(&buffer_clone)).await });

        let entry = ProcessEntry {
            command: config.command,
            start_time: Instant::now(),
            completed_at: None,
            output_buffer: shared_buffer,
            join_handle: Some(handle),
            result: None,
            max_output_lines,
        };

        entries.insert(id.clone(), entry);
        Ok((id, None))
    }

    /// Wait for a started process to complete, returning the result and the
    /// `max_output_lines` configured at start time.
    ///
    /// This blocks (via `.await`) until the process exits or the optional timeout
    /// expires. After completion, the result is cached for future retrieval.
    pub async fn wait(
        &self,
        id: &str,
        timeout_seconds: Option<u64>,
    ) -> Result<(ProcessResult, usize), String> {
        // Take the join handle out of the entry.
        let (handle, max_output_lines) = {
            let mut entries = self.entries.lock().await;
            let entry = entries
                .get_mut(id)
                .ok_or_else(|| format!("no process with id '{id}'"))?;

            // If already completed, return cached result.
            if let Some(ref result) = entry.result {
                return Ok((result.clone(), entry.max_output_lines));
            }

            let h = entry
                .join_handle
                .take()
                .ok_or_else(|| format!("process '{id}' is already being waited on"))?;
            (h, entry.max_output_lines)
        };

        // Await the handle, optionally with a timeout.
        let result = if let Some(secs) = timeout_seconds {
            match tokio::time::timeout(std::time::Duration::from_secs(secs), handle).await {
                Ok(Ok(result)) => result,
                Ok(Err(e)) => {
                    return Err(format!("task join error: {e}"));
                }
                Err(_) => {
                    return Err(format!("wait timed out after {secs}s"));
                }
            }
        } else {
            handle.await.map_err(|e| format!("task join error: {e}"))?
        };

        // Cache the result.
        {
            let mut entries = self.entries.lock().await;
            if let Some(entry) = entries.get_mut(id) {
                entry.result = Some(result.clone());
                entry.completed_at = Some(Instant::now());
            }
        }

        Ok((result, max_output_lines))
    }

    /// Retrieve a range of output lines from a completed (or running) command.
    ///
    /// If `start_line` and `end_line` are `None`, returns all lines.
    /// Lines are 0-indexed. Returns up to 500 lines per call.
    pub async fn get_output(
        &self,
        id: &str,
        start_line: Option<usize>,
        end_line: Option<usize>,
    ) -> Result<OutputSlice, String> {
        let entries = self.entries.lock().await;
        let entry = entries
            .get(id)
            .ok_or_else(|| format!("no process with id '{id}'"))?;

        // Get the lines from the result (completed) or the shared buffer (running).
        let lines: Vec<String> = if let Some(ref result) = entry.result {
            result.lines.clone()
        } else {
            entry.output_buffer.lock().await.clone()
        };

        let total_lines = lines.len();
        let start = start_line.unwrap_or(0).min(total_lines);
        let end = end_line.unwrap_or(total_lines).min(total_lines);

        // Cap at 500 lines per call to avoid blowing up the context.
        let max_slice = 500;
        let effective_end = end.min(start + max_slice);

        let slice = if start < effective_end {
            lines[start..effective_end].to_vec()
        } else {
            Vec::new()
        };

        Ok(OutputSlice {
            id: id.to_string(),
            start_line: start,
            end_line: effective_end,
            total_lines,
            lines: slice,
        })
    }

    /// Get the current status of a tracked process without blocking.
    pub async fn status(&self, id: &str) -> Result<StatusResponse, String> {
        let entries = self.entries.lock().await;
        let entry = entries
            .get(id)
            .ok_or_else(|| format!("no process with id '{id}'"))?;

        let runtime_seconds = entry.start_time.elapsed().as_secs_f64();

        if let Some(ref result) = entry.result {
            let status = if result.timed_out {
                ProcessStatus::TimedOut
            } else if result.exit_code == 0 {
                ProcessStatus::Completed
            } else {
                ProcessStatus::Failed
            };
            let windowed = output::window(&result.lines, entry.max_output_lines);
            Ok(StatusResponse {
                status,
                runtime_seconds: result.duration_seconds,
                tail_lines: windowed.tail.into_iter().chain(windowed.head).collect(),
            })
        } else {
            // Still running -- read the shared buffer for latest output.
            let buf = entry.output_buffer.lock().await;
            let tail_count = 20.min(buf.len());
            let tail_lines = buf[buf.len() - tail_count..].to_vec();
            Ok(StatusResponse {
                status: ProcessStatus::Running,
                runtime_seconds,
                tail_lines,
            })
        }
    }

    /// Kill a running process by aborting its task.
    pub async fn kill(&self, id: &str) -> Result<(), String> {
        let mut entries = self.entries.lock().await;
        let entry = entries
            .get_mut(id)
            .ok_or_else(|| format!("no process with id '{id}'"))?;

        if entry.result.is_some() {
            return Err(format!("process '{id}' has already completed"));
        }

        if let Some(handle) = entry.join_handle.take() {
            handle.abort();
            // Store a synthetic result.
            let lines = entry.output_buffer.lock().await.clone();
            entry.result = Some(ProcessResult {
                exit_code: -1,
                duration_seconds: entry.start_time.elapsed().as_secs_f64(),
                lines,
                timed_out: false,
            });
            entry.completed_at = Some(Instant::now());
        }

        Ok(())
    }

    /// List all tracked processes.
    pub async fn list(&self) -> Vec<ProcessSummary> {
        self.prune_expired().await;
        let entries = self.entries.lock().await;
        entries
            .iter()
            .map(|(id, entry)| {
                let status = match &entry.result {
                    Some(r) if r.timed_out => ProcessStatus::TimedOut,
                    Some(r) if r.exit_code == 0 => ProcessStatus::Completed,
                    Some(_) => ProcessStatus::Failed,
                    None => ProcessStatus::Running,
                };
                let runtime_seconds = match &entry.result {
                    Some(r) => r.duration_seconds,
                    None => entry.start_time.elapsed().as_secs_f64(),
                };
                ProcessSummary {
                    id: id.clone(),
                    command: entry.command.clone(),
                    status,
                    runtime_seconds,
                }
            })
            .collect()
    }
}

impl Default for ProcessRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Response from a status check.
#[derive(Debug, Clone, Serialize)]
pub struct StatusResponse {
    pub status: ProcessStatus,
    pub runtime_seconds: f64,
    pub tail_lines: Vec<String>,
}

/// A slice of output lines from a tracked command.
#[derive(Debug, Clone, Serialize)]
pub struct OutputSlice {
    pub id: String,
    pub start_line: usize,
    pub end_line: usize,
    pub total_lines: usize,
    pub lines: Vec<String>,
}
