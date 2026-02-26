//! Integration tests for the MCP server tools via duplex transport.
//!
//! Uses `tokio::io::duplex` to create an in-process transport, connects a test
//! client to the agentsh server, and exercises all tools through the MCP protocol.

use agentsh::server::AgentshServer;
use rmcp::model::*;
use rmcp::{ClientHandler, ServiceExt};
use serde_json::Value;

/// Minimal test client that implements ClientHandler with defaults.
#[derive(Default, Clone)]
struct TestClient;

impl ClientHandler for TestClient {}

/// Helper: start a server+client pair connected via duplex transport.
async fn setup() -> rmcp::service::RunningService<
    rmcp::service::RoleClient,
    impl rmcp::service::Service<rmcp::service::RoleClient>,
> {
    let (server_transport, client_transport) = tokio::io::duplex(65536);

    let server = AgentshServer::new();
    tokio::spawn(async move {
        let service = server.serve(server_transport).await.unwrap();
        let _ = service.waiting().await;
    });

    let client = TestClient::default();
    client.serve(client_transport).await.unwrap()
}

/// Helper: call a tool and parse the JSON text content from the response.
async fn call_tool(
    client: &rmcp::service::RunningService<
        rmcp::service::RoleClient,
        impl rmcp::service::Service<rmcp::service::RoleClient>,
    >,
    name: &str,
    args: Value,
) -> Value {
    let params = CallToolRequestParams {
        meta: None,
        name: name.to_string().into(),
        arguments: Some(serde_json::from_value(args).unwrap()),
        task: None,
    };
    let request = ClientRequest::CallToolRequest(Request::new(params));
    let response = client.send_request(request).await.unwrap();

    let ServerResult::CallToolResult(result) = response else {
        panic!("expected CallToolResult, got {response:?}");
    };

    // Parse the JSON text content.
    let text = result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            RawContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");

    serde_json::from_str(&text).unwrap_or_else(|_| Value::String(text))
}

// ---------------------------------------------------------------------------
// run_command tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_run_command_echo() {
    let client = setup().await;
    let result = call_tool(
        &client,
        "run_command",
        serde_json::json!({"command": "echo hello_world"}),
    )
    .await;

    assert_eq!(result["exit_code"], 0);
    assert!(!result["timed_out"].as_bool().unwrap());

    let head = result["output_head"].as_array().unwrap();
    assert!(
        head.iter()
            .any(|l| l.as_str().unwrap().contains("hello_world"))
    );

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn test_run_command_failure() {
    let client = setup().await;
    let result = call_tool(
        &client,
        "run_command",
        serde_json::json!({"command": "exit 1"}),
    )
    .await;

    assert_eq!(result["exit_code"], 1);
    assert!(!result["timed_out"].as_bool().unwrap());

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn test_run_command_with_timeout() {
    let client = setup().await;
    let result = call_tool(
        &client,
        "run_command",
        serde_json::json!({"command": "sleep 30", "timeout_seconds": 1}),
    )
    .await;

    assert!(result["timed_out"].as_bool().unwrap());
    assert_eq!(result["exit_code"], -1);

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn test_run_command_large_output_truncated() {
    let client = setup().await;
    let result = call_tool(
        &client,
        "run_command",
        serde_json::json!({
            "command": "seq 1 500",
            "max_output_lines": 30
        }),
    )
    .await;

    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["total_lines"], 500);
    assert!(result["truncated"].as_bool().unwrap());

    let head = result["output_head"].as_array().unwrap();
    let tail = result["output_tail"].as_array().unwrap();
    assert_eq!(head.len(), 10); // HEAD_LINES = 10
    assert_eq!(tail.len(), 20); // 30 - 10

    client.cancel().await.unwrap();
}

// ---------------------------------------------------------------------------
// start_command + wait_command tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_start_and_wait() {
    let client = setup().await;

    // Start a command.
    let start_result = call_tool(
        &client,
        "start_command",
        serde_json::json!({"command": "echo async_hello", "id": "test-1"}),
    )
    .await;

    assert_eq!(start_result["id"], "test-1");
    assert_eq!(start_result["status"], "running");

    // Wait for it.
    let wait_result = call_tool(&client, "wait_command", serde_json::json!({"id": "test-1"})).await;

    assert_eq!(wait_result["exit_code"], 0);
    let head = wait_result["output_head"].as_array().unwrap();
    assert!(
        head.iter()
            .any(|l| l.as_str().unwrap().contains("async_hello"))
    );

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn test_start_duplicate_id_error() {
    let client = setup().await;

    // Start first command.
    call_tool(
        &client,
        "start_command",
        serde_json::json!({"command": "sleep 5", "id": "dup-1"}),
    )
    .await;

    // Start second command with same ID -- should get error message.
    let result = call_tool(
        &client,
        "start_command",
        serde_json::json!({"command": "echo x", "id": "dup-1"}),
    )
    .await;

    // The result should be a string error, not a JSON object with exit_code.
    let text = result.as_str().unwrap_or("");
    assert!(
        text.contains("already exists"),
        "expected 'already exists' error, got: {result}"
    );

    client.cancel().await.unwrap();
}

// ---------------------------------------------------------------------------
// get_status tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_get_status_running() {
    let client = setup().await;

    call_tool(
        &client,
        "start_command",
        serde_json::json!({"command": "sleep 10", "id": "status-1"}),
    )
    .await;

    // Small delay to let the process start.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let status = call_tool(&client, "get_status", serde_json::json!({"id": "status-1"})).await;

    assert_eq!(status["status"], "running");

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn test_get_status_completed() {
    let client = setup().await;

    call_tool(
        &client,
        "start_command",
        serde_json::json!({"command": "echo done", "id": "status-2"}),
    )
    .await;

    // Wait for it to complete.
    call_tool(
        &client,
        "wait_command",
        serde_json::json!({"id": "status-2"}),
    )
    .await;

    let status = call_tool(&client, "get_status", serde_json::json!({"id": "status-2"})).await;

    assert_eq!(status["status"], "completed");

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn test_get_status_nonexistent() {
    let client = setup().await;

    let result = call_tool(
        &client,
        "get_status",
        serde_json::json!({"id": "nonexistent"}),
    )
    .await;

    let text = result.as_str().unwrap_or("");
    assert!(
        text.contains("no process"),
        "expected 'no process' error, got: {result}"
    );

    client.cancel().await.unwrap();
}

// ---------------------------------------------------------------------------
// kill_command tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_kill_running_command() {
    let client = setup().await;

    call_tool(
        &client,
        "start_command",
        serde_json::json!({"command": "sleep 60", "id": "kill-1"}),
    )
    .await;

    // Small delay to let the process start.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let kill_result = call_tool(&client, "kill_command", serde_json::json!({"id": "kill-1"})).await;

    assert_eq!(kill_result["id"], "kill-1");
    assert!(kill_result["killed"].as_bool().unwrap());

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn test_kill_completed_command_error() {
    let client = setup().await;

    call_tool(
        &client,
        "start_command",
        serde_json::json!({"command": "echo x", "id": "kill-2"}),
    )
    .await;

    // Wait for it to complete.
    call_tool(&client, "wait_command", serde_json::json!({"id": "kill-2"})).await;

    let result = call_tool(&client, "kill_command", serde_json::json!({"id": "kill-2"})).await;

    let text = result.as_str().unwrap_or("");
    assert!(
        text.contains("already completed"),
        "expected 'already completed' error, got: {result}"
    );

    client.cancel().await.unwrap();
}

// ---------------------------------------------------------------------------
// list_commands tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_list_commands_empty() {
    let client = setup().await;

    let result = call_tool(&client, "list_commands", serde_json::json!({})).await;

    let list = result.as_array().unwrap();
    assert!(list.is_empty());

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn test_list_commands_shows_entries() {
    let client = setup().await;

    call_tool(
        &client,
        "start_command",
        serde_json::json!({"command": "sleep 10", "id": "list-1"}),
    )
    .await;

    call_tool(
        &client,
        "start_command",
        serde_json::json!({"command": "echo hi", "id": "list-2"}),
    )
    .await;

    // Small delay for second command to complete.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let result = call_tool(&client, "list_commands", serde_json::json!({})).await;

    let list = result.as_array().unwrap();
    assert_eq!(list.len(), 2);

    let ids: Vec<&str> = list.iter().filter_map(|e| e["id"].as_str()).collect();
    assert!(ids.contains(&"list-1"));
    assert!(ids.contains(&"list-2"));

    client.cancel().await.unwrap();
}

// ---------------------------------------------------------------------------
// run_command returns an ID for get_output
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_run_command_returns_id() {
    let client = setup().await;
    let result = call_tool(
        &client,
        "run_command",
        serde_json::json!({"command": "echo tracked"}),
    )
    .await;

    // run_command now returns an id field.
    let id = result["id"].as_str();
    assert!(
        id.is_some(),
        "run_command should return an id, got: {result}"
    );
    assert!(!id.unwrap().is_empty());

    client.cancel().await.unwrap();
}

// ---------------------------------------------------------------------------
// get_output tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_get_output_full_from_run_command() {
    let client = setup().await;

    // Run a command that produces 50 lines, windowed to 20.
    let result = call_tool(
        &client,
        "run_command",
        serde_json::json!({
            "command": "seq 1 50",
            "max_output_lines": 20
        }),
    )
    .await;

    assert!(result["truncated"].as_bool().unwrap());
    let id = result["id"].as_str().unwrap();

    // Now use get_output to retrieve the FULL output.
    let output = call_tool(&client, "get_output", serde_json::json!({"id": id})).await;

    assert_eq!(output["total_lines"], 50);
    let lines = output["lines"].as_array().unwrap();
    assert_eq!(lines.len(), 50);
    assert_eq!(lines[0], "1");
    assert_eq!(lines[49], "50");

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn test_get_output_line_range() {
    let client = setup().await;

    let result = call_tool(
        &client,
        "run_command",
        serde_json::json!({"command": "seq 1 100"}),
    )
    .await;

    let id = result["id"].as_str().unwrap();

    // Get lines 10-20 (0-indexed).
    let output = call_tool(
        &client,
        "get_output",
        serde_json::json!({"id": id, "start_line": 10, "end_line": 20}),
    )
    .await;

    assert_eq!(output["start_line"], 10);
    assert_eq!(output["end_line"], 20);
    assert_eq!(output["total_lines"], 100);
    let lines = output["lines"].as_array().unwrap();
    assert_eq!(lines.len(), 10);
    assert_eq!(lines[0], "11"); // seq is 1-based, line 10 (0-indexed) = "11"
    assert_eq!(lines[9], "20");

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn test_get_output_from_start_wait() {
    let client = setup().await;

    call_tool(
        &client,
        "start_command",
        serde_json::json!({"command": "seq 1 30", "id": "output-test"}),
    )
    .await;

    call_tool(
        &client,
        "wait_command",
        serde_json::json!({"id": "output-test"}),
    )
    .await;

    let output = call_tool(
        &client,
        "get_output",
        serde_json::json!({"id": "output-test"}),
    )
    .await;

    assert_eq!(output["total_lines"], 30);
    let lines = output["lines"].as_array().unwrap();
    assert_eq!(lines.len(), 30);
    assert_eq!(lines[0], "1");
    assert_eq!(lines[29], "30");

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn test_get_output_nonexistent_id() {
    let client = setup().await;

    let result = call_tool(
        &client,
        "get_output",
        serde_json::json!({"id": "does-not-exist"}),
    )
    .await;

    let text = result.as_str().unwrap_or("");
    assert!(
        text.contains("no process"),
        "expected 'no process' error, got: {result}"
    );

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn test_run_command_tracked_in_list() {
    let client = setup().await;

    // run_command results should now appear in list_commands.
    call_tool(
        &client,
        "run_command",
        serde_json::json!({"command": "echo listed"}),
    )
    .await;

    let list = call_tool(&client, "list_commands", serde_json::json!({})).await;
    let entries = list.as_array().unwrap();
    assert!(
        entries
            .iter()
            .any(|e| { e["command"].as_str().unwrap_or("").contains("echo listed") }),
        "run_command result should appear in list_commands, got: {list}"
    );

    client.cancel().await.unwrap();
}

// ---------------------------------------------------------------------------
// Security: max concurrent processes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_max_concurrent_processes_enforced() {
    let client = setup().await;

    // Start 20 long-running commands (the limit).
    for i in 0..20 {
        let result = call_tool(
            &client,
            "start_command",
            serde_json::json!({"command": "sleep 60", "id": format!("flood-{i}")}),
        )
        .await;
        assert_eq!(result["status"], "running", "command {i} should start");
    }

    // The 21st should be rejected.
    let result = call_tool(
        &client,
        "start_command",
        serde_json::json!({"command": "sleep 60", "id": "flood-overflow"}),
    )
    .await;

    let text = result.as_str().unwrap_or("");
    assert!(
        text.contains("too many concurrent processes"),
        "expected concurrent limit error, got: {result}"
    );

    client.cancel().await.unwrap();
}
