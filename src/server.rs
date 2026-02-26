//! MCP server: tool definitions using rmcp macros.
//!
//! Defines `LlmNotifyServer` with MCP tools for:
//! - Stateless commands: `run_command`, `start_command`, `wait_command`, `get_status`,
//!   `kill_command`, `list_commands`, `get_output`
//! - Persistent sessions: `create_session`, `session_exec`, `list_sessions`, `close_session`

use crate::output;
use crate::process::{self, ProcessConfig};
use crate::registry::ProcessRegistry;
use crate::session::SessionManager;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::schemars;
use rmcp::tool;
use rmcp::tool_handler;
use rmcp::tool_router;
use rmcp::{ErrorData as McpError, ServerHandler};
use serde::Serialize;

/// Default timeout for commands in seconds (5 minutes).
const DEFAULT_TIMEOUT_SECONDS: u64 = 300;

/// Default max output lines returned to the agent.
const DEFAULT_MAX_OUTPUT_LINES: usize = 200;

// ---------------------------------------------------------------------------
// Parameter structs (deserialized from MCP tool call arguments)
// ---------------------------------------------------------------------------

/// Parameters for the `run_command` tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RunCommandParams {
    /// The shell command to execute (passed to /bin/sh -c).
    pub command: String,
    /// Working directory for the command. Defaults to the server's cwd.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<String>,
    /// Maximum execution time in seconds. Defaults to 300 (5 minutes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
    /// Maximum number of output lines to return. Defaults to 200.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_lines: Option<usize>,
}

/// Parameters for the `start_command` tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct StartCommandParams {
    /// The shell command to execute.
    pub command: String,
    /// Optional ID for the process. Auto-generated UUID if omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Working directory for the command.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<String>,
    /// Maximum execution time in seconds. Defaults to 300.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
    /// Maximum number of output lines to return on completion.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_lines: Option<usize>,
}

/// Parameters for the `wait_command` tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct WaitCommandParams {
    /// ID of the process to wait for.
    pub id: String,
    /// Additional timeout for the wait itself (on top of the process timeout).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
}

/// Parameters for the `get_status` tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct GetStatusParams {
    /// ID of the process to check.
    pub id: String,
}

/// Parameters for the `kill_command` tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct KillCommandParams {
    /// ID of the process to kill.
    pub id: String,
}

/// Parameters for the `get_output` tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct GetOutputParams {
    /// ID of the command to retrieve output from.
    pub id: String,
    /// Start line (0-indexed, inclusive). Defaults to 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_line: Option<usize>,
    /// End line (0-indexed, exclusive). Defaults to all remaining lines.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<usize>,
}

// ---------------------------------------------------------------------------
// Session parameter structs
// ---------------------------------------------------------------------------

/// Parameters for the `create_session` tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CreateSessionParams {
    /// Unique ID for the session.
    pub id: String,
    /// Initial working directory for the session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<String>,
}

/// Parameters for the `session_exec` tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SessionExecParams {
    /// ID of the session to execute in.
    pub id: String,
    /// The shell command to execute.
    pub command: String,
    /// Maximum execution time in seconds. Defaults to 300.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
    /// Maximum number of output lines to return. Defaults to 200.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_lines: Option<usize>,
}

/// Parameters for the `close_session` tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CloseSessionParams {
    /// ID of the session to close.
    pub id: String,
}

// ---------------------------------------------------------------------------
// Result structs (serialized to JSON and returned as tool content)
// ---------------------------------------------------------------------------

/// Structured result of a completed command, optimized for LLM consumption.
///
/// The `id` field can be used with `get_output` to retrieve the full output
/// or a specific line range if the windowed head/tail isn't enough.
#[derive(Debug, Clone, Serialize)]
pub struct CommandResult {
    pub id: String,
    pub exit_code: i32,
    pub duration_seconds: f64,
    pub output_head: Vec<String>,
    pub output_tail: Vec<String>,
    pub output_error_lines: Vec<String>,
    pub total_lines: usize,
    pub truncated: bool,
    pub timed_out: bool,
}

/// Result of starting an async command.
#[derive(Debug, Clone, Serialize)]
struct StartResult {
    id: String,
    status: &'static str,
}

/// Result of killing a command.
#[derive(Debug, Clone, Serialize)]
struct KillResult {
    id: String,
    killed: bool,
}

// ---------------------------------------------------------------------------
// MCP Server
// ---------------------------------------------------------------------------

/// The agentsh MCP server.
///
/// Holds a [`ProcessRegistry`] for stateless commands and a [`SessionManager`]
/// for persistent shell sessions.
#[derive(Clone)]
pub struct LlmNotifyServer {
    registry: ProcessRegistry,
    sessions: SessionManager,
    tool_router: ToolRouter<LlmNotifyServer>,
}

impl LlmNotifyServer {
    /// Create a new server instance.
    #[must_use]
    pub fn new() -> Self {
        Self {
            registry: ProcessRegistry::new(),
            sessions: SessionManager::new(),
            tool_router: Self::tool_router(),
        }
    }
}

impl Default for LlmNotifyServer {
    fn default() -> Self {
        Self::new()
    }
}

fn json_content<T: Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    let json = serde_json::to_string_pretty(value)
        .map_err(|e| McpError::internal_error(format!("JSON serialization error: {e}"), None))?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

fn err_result(msg: impl Into<String>) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::error(vec![Content::text(msg.into())]))
}

fn build_command_result(
    id: &str,
    result: &process::ProcessResult,
    max_output_lines: usize,
) -> CommandResult {
    let windowed = output::window(&result.lines, max_output_lines);
    CommandResult {
        id: id.to_string(),
        exit_code: result.exit_code,
        duration_seconds: result.duration_seconds,
        output_head: windowed.head,
        output_tail: windowed.tail,
        output_error_lines: windowed.error_lines,
        total_lines: windowed.total_lines,
        truncated: windowed.truncated,
        timed_out: result.timed_out,
    }
}

#[tool_router]
impl LlmNotifyServer {
    #[tool(
        description = "Execute a command in a fresh shell (no state between calls, no PTY). Best for quick one-off commands like `git status`, `ls`, `which`. Blocks until done. Returns structured output with exit_code, duration, windowed output (head/tail/error_lines). The returned `id` can be used with get_output to retrieve full output if truncated. For commands needing persistent state (cd, export) or a terminal (claude CLI, interactive tools), use create_session + session_exec instead."
    )]
    async fn run_command(
        &self,
        Parameters(params): Parameters<RunCommandParams>,
    ) -> Result<CallToolResult, McpError> {
        let max_lines = params.max_output_lines.unwrap_or(DEFAULT_MAX_OUTPUT_LINES);
        let command_str = params.command.clone();
        let timeout = process::clamp_timeout(Some(
            params.timeout_seconds.unwrap_or(DEFAULT_TIMEOUT_SECONDS),
        ));

        tracing::info!(command = %command_str, "run_command");

        let config = ProcessConfig {
            command: params.command,
            working_directory: params.working_directory,
            timeout_seconds: timeout,
        };

        let result = process::run(&config, None).await;

        tracing::info!(
            command = %command_str,
            exit_code = result.exit_code,
            duration = result.duration_seconds,
            timed_out = result.timed_out,
            lines = result.lines.len(),
            "run_command completed"
        );

        // Store in registry so output can be retrieved later via get_output.
        let id = uuid::Uuid::new_v4().to_string();
        self.registry
            .store_result(id.clone(), command_str, result.clone(), max_lines)
            .await;

        let cmd_result = build_command_result(&id, &result, max_lines);
        json_content(&cmd_result)
    }

    #[tool(
        description = "Start a command in the background (no PTY, stateless). Returns immediately with an ID. Use wait_command to block until it completes, get_status to check progress, or kill_command to terminate it. Useful for long builds or parallel tasks."
    )]
    async fn start_command(
        &self,
        Parameters(params): Parameters<StartCommandParams>,
    ) -> Result<CallToolResult, McpError> {
        let id = params
            .id
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let timeout = process::clamp_timeout(Some(
            params.timeout_seconds.unwrap_or(DEFAULT_TIMEOUT_SECONDS),
        ));

        tracing::info!(id = %id, command = %params.command, "start_command");

        let config = ProcessConfig {
            command: params.command,
            working_directory: params.working_directory,
            timeout_seconds: timeout,
        };
        let max_lines = params.max_output_lines.unwrap_or(DEFAULT_MAX_OUTPUT_LINES);

        match self.registry.start(id.clone(), config, max_lines).await {
            Ok((id, _pid)) => json_content(&StartResult {
                id,
                status: "running",
            }),
            Err(e) => err_result(e),
        }
    }

    #[tool(
        description = "Block until a previously started command completes and return its structured output. Use the ID returned by start_command. Returns immediately if already finished."
    )]
    async fn wait_command(
        &self,
        Parameters(params): Parameters<WaitCommandParams>,
    ) -> Result<CallToolResult, McpError> {
        match self.registry.wait(&params.id, params.timeout_seconds).await {
            Ok((result, max_output_lines)) => {
                let cmd_result = build_command_result(&params.id, &result, max_output_lines);
                json_content(&cmd_result)
            }
            Err(e) => err_result(e),
        }
    }

    #[tool(
        description = "Check the status of a background command without blocking. Returns status (running/completed/failed/timed_out), runtime, and the last 20 output lines. Use to monitor long-running commands started with start_command."
    )]
    async fn get_status(
        &self,
        Parameters(params): Parameters<GetStatusParams>,
    ) -> Result<CallToolResult, McpError> {
        match self.registry.status(&params.id).await {
            Ok(status) => json_content(&status),
            Err(e) => err_result(e),
        }
    }

    #[tool(
        description = "Kill a running background command by its ID. Returns whether the kill was successful. Only works for commands started with start_command."
    )]
    async fn kill_command(
        &self,
        Parameters(params): Parameters<KillCommandParams>,
    ) -> Result<CallToolResult, McpError> {
        tracing::info!(id = %params.id, "kill_command");
        match self.registry.kill(&params.id).await {
            Ok(()) => json_content(&KillResult {
                id: params.id,
                killed: true,
            }),
            Err(e) => err_result(e),
        }
    }

    #[tool(
        description = "Retrieve full output or a line range from a completed command. Use the `id` from run_command/start_command results. Returns up to 500 lines per call. Omit start_line/end_line to get all output. Use this when the windowed head/tail wasn't enough and you need to see specific lines (e.g., error context in the middle of build output)."
    )]
    async fn get_output(
        &self,
        Parameters(params): Parameters<GetOutputParams>,
    ) -> Result<CallToolResult, McpError> {
        match self
            .registry
            .get_output(&params.id, params.start_line, params.end_line)
            .await
        {
            Ok(slice) => json_content(&slice),
            Err(e) => err_result(e),
        }
    }

    #[tool(
        description = "List all tracked background commands (from run_command and start_command) with their ID, command string, status, and runtime."
    )]
    async fn list_commands(&self) -> Result<CallToolResult, McpError> {
        let commands = self.registry.list().await;
        json_content(&commands)
    }

    // -----------------------------------------------------------------------
    // Session tools -- persistent shell sessions with state preservation
    // -----------------------------------------------------------------------

    #[tool(
        description = "Create a persistent shell session (long-lived bash process with a real PTY). Working directory, env vars, functions, and aliases persist across commands. Use session_exec to run commands in the session. Sessions provide a real terminal (isatty=true), so tools like claude CLI, docker, ssh, and programs with colored output work correctly. Set working_directory to start in a specific project."
    )]
    async fn create_session(
        &self,
        Parameters(params): Parameters<CreateSessionParams>,
    ) -> Result<CallToolResult, McpError> {
        tracing::info!(id = %params.id, "create_session");
        match self
            .sessions
            .create(params.id, params.working_directory)
            .await
        {
            Ok(info) => json_content(&info),
            Err(e) => err_result(e),
        }
    }

    #[tool(
        description = "Execute a command in a persistent session. Working directory, env vars, functions, and aliases from previous commands persist. Has a real PTY so interactive tools and CLIs that require a terminal work. Returns structured output with exit_code, duration, and windowed output lines. For long-running commands (builds, tests, AI tools), increase timeout_seconds (default 300s, max 3600s). If a command might take more than 5 minutes, set timeout_seconds accordingly."
    )]
    async fn session_exec(
        &self,
        Parameters(params): Parameters<SessionExecParams>,
    ) -> Result<CallToolResult, McpError> {
        let max_lines = params.max_output_lines.unwrap_or(DEFAULT_MAX_OUTPUT_LINES);

        tracing::info!(session = %params.id, command = %params.command, "session_exec");

        match self
            .sessions
            .exec(&params.id, &params.command, params.timeout_seconds)
            .await
        {
            Ok(result) => {
                tracing::info!(
                    session = %params.id,
                    exit_code = result.exit_code,
                    duration = result.duration_seconds,
                    lines = result.lines.len(),
                    "session_exec completed"
                );

                // Window the output for LLM consumption.
                let windowed = output::window(&result.lines, max_lines);
                let cmd_result = CommandResult {
                    id: result.session_id,
                    exit_code: result.exit_code,
                    duration_seconds: result.duration_seconds,
                    output_head: windowed.head,
                    output_tail: windowed.tail,
                    output_error_lines: windowed.error_lines,
                    total_lines: windowed.total_lines,
                    truncated: windowed.truncated,
                    timed_out: result.timed_out,
                };
                json_content(&cmd_result)
            }
            Err(e) => err_result(e),
        }
    }

    #[tool(description = "List all active shell sessions with their ID and alive status.")]
    async fn list_sessions(&self) -> Result<CallToolResult, McpError> {
        let sessions = self.sessions.list().await;
        json_content(&sessions)
    }

    #[tool(
        description = "Close a persistent shell session and terminate its bash process. Use when done with a session to free resources."
    )]
    async fn close_session(
        &self,
        Parameters(params): Parameters<CloseSessionParams>,
    ) -> Result<CallToolResult, McpError> {
        tracing::info!(id = %params.id, "close_session");
        match self.sessions.close(&params.id).await {
            Ok(()) => json_content(&serde_json::json!({"id": params.id, "closed": true})),
            Err(e) => err_result(e),
        }
    }
}

#[tool_handler]
impl ServerHandler for LlmNotifyServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::V_2024_11_05,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "agentsh".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                ..Default::default()
            },
            instructions: Some(
                "agentsh is a shell for AI agents with two modes:\n\n\
                 SESSIONS (preferred for most work):\n\
                 Sessions are persistent bash processes with a real PTY (pseudo-terminal). \
                 Use create_session to start one, then session_exec to run commands. \
                 Working directory, env vars, shell functions, and aliases persist across commands. \
                 Programs that require a terminal (claude CLI, interactive tools, colored output) \
                 work correctly in sessions because isatty()=true. \
                 For long-running commands, set timeout_seconds appropriately (default 300s, max 3600s). \
                 If a command might take more than 5 minutes, increase timeout_seconds.\n\n\
                 STATELESS (for quick one-off commands):\n\
                 run_command executes a single command in a fresh shell -- no state persists between calls. \
                 Faster for simple checks (git status, ls, which). No PTY -- programs see pipes. \
                 start_command + wait_command lets you run a command in the background and wait later.\n\n\
                 OUTPUT: All commands return structured JSON with exit_code, duration, windowed output \
                 (head + tail + error_lines), and total_lines. If output is truncated, use get_output \
                 with the returned id to retrieve specific line ranges."
                    .to_string(),
            ),
        }
    }
}
