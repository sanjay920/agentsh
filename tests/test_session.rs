//! Tests for persistent shell sessions.
//!
//! These tests verify the core session mechanism: output delimiting, exit codes,
//! state persistence (cwd, env vars), timeout handling, and session lifecycle.

use agentsh::session::SessionManager;

fn manager() -> SessionManager {
    SessionManager::new()
}

// ---------------------------------------------------------------------------
// Basic session lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_create_and_close_session() {
    let mgr = manager();
    let info = mgr.create("s1".into(), None).await.unwrap();
    assert_eq!(info.id, "s1");
    assert!(info.alive);

    mgr.close("s1").await.unwrap();

    // Session should be gone.
    let list = mgr.list().await;
    assert!(list.is_empty());
}

#[tokio::test]
async fn test_create_replaces_existing_session() {
    let mgr = manager();
    mgr.create("dup".into(), None).await.unwrap();

    // Set state in the first session.
    mgr.exec("dup", "export MARKER=old", None, None)
        .await
        .unwrap();

    // Creating again with the same ID replaces it (idempotent).
    let info = mgr.create("dup".into(), None).await.unwrap();
    assert_eq!(info.id, "dup");
    assert!(info.alive);

    // State should be gone (fresh session).
    let result = mgr.exec("dup", "echo ${MARKER:-empty}", None, None)
        .await
        .unwrap();
    assert!(
        result.lines.iter().any(|l| l.contains("empty")),
        "replaced session should have fresh state, got: {:?}",
        result.lines
    );
}

// ---------------------------------------------------------------------------
// Command execution basics
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_session_exec_echo() {
    let mgr = manager();
    mgr.create("t1".into(), None).await.unwrap();

    let result = mgr.exec("t1", "echo hello session", None, None).await.unwrap();
    assert_eq!(result.exit_code, 0);
    assert!(result.lines.iter().any(|l| l.contains("hello session")));
    assert!(!result.timed_out);
}

#[tokio::test]
async fn test_session_exec_failure() {
    let mgr = manager();
    mgr.create("t2".into(), None).await.unwrap();

    // Use `false` which returns exit code 1 without killing the session.
    // (`exit 42` would kill the bash process since commands run in the current shell.)
    let result = mgr.exec("t2", "false", None, None).await.unwrap();
    assert_eq!(result.exit_code, 1);
}

#[tokio::test]
async fn test_session_exec_custom_exit_code() {
    let mgr = manager();
    mgr.create("t2b".into(), None).await.unwrap();

    // Use a subshell to get a specific exit code without killing the session.
    let result = mgr.exec("t2b", "(exit 42)", None, None).await.unwrap();
    assert_eq!(result.exit_code, 42);

    // Session should still be alive.
    let result = mgr.exec("t2b", "echo alive", None, None).await.unwrap();
    assert_eq!(result.exit_code, 0);
    assert!(result.lines.iter().any(|l| l.contains("alive")));
}

#[tokio::test]
async fn test_session_exec_no_output() {
    let mgr = manager();
    mgr.create("t3".into(), None).await.unwrap();

    let result = mgr.exec("t3", "true", None, None).await.unwrap();
    assert_eq!(result.exit_code, 0);
    assert!(result.lines.is_empty());
}

#[tokio::test]
async fn test_session_exec_multiline_output() {
    let mgr = manager();
    mgr.create("t4".into(), None).await.unwrap();

    let result = mgr.exec("t4", "seq 1 10", None, None).await.unwrap();
    assert_eq!(result.exit_code, 0);
    assert_eq!(result.lines.len(), 10);
    assert_eq!(result.lines[0], "1");
    assert_eq!(result.lines[9], "10");
}

#[tokio::test]
async fn test_session_exec_stderr_captured() {
    let mgr = manager();
    mgr.create("t5".into(), None).await.unwrap();

    let result = mgr.exec("t5", "echo err_msg >&2", None, None).await.unwrap();
    assert_eq!(result.exit_code, 0);
    assert!(
        result.lines.iter().any(|l| l.contains("err_msg")),
        "stderr should be captured, got: {:?}",
        result.lines
    );
}

// ---------------------------------------------------------------------------
// STATE PERSISTENCE -- the whole point of sessions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_session_cwd_persists() {
    let mgr = manager();
    mgr.create("cwd".into(), None).await.unwrap();

    // cd somewhere
    let result = mgr.exec("cwd", "cd /tmp", None, None).await.unwrap();
    assert_eq!(result.exit_code, 0);

    // pwd should reflect the cd
    let result = mgr.exec("cwd", "pwd", None, None).await.unwrap();
    assert_eq!(result.exit_code, 0);
    assert!(
        result
            .lines
            .iter()
            .any(|l| l.contains("/tmp") || l.contains("/private/tmp")),
        "cwd should be /tmp after cd, got: {:?}",
        result.lines
    );
}

#[tokio::test]
async fn test_session_env_var_persists() {
    let mgr = manager();
    mgr.create("env".into(), None).await.unwrap();

    // Set an env var
    mgr.exec("env", "export MY_SESSION_VAR=persistent_value", None, None)
        .await
        .unwrap();

    // Read it back in a subsequent command
    let result = mgr.exec("env", "echo $MY_SESSION_VAR", None, None).await.unwrap();
    assert_eq!(result.exit_code, 0);
    assert!(
        result.lines.iter().any(|l| l.contains("persistent_value")),
        "env var should persist across commands, got: {:?}",
        result.lines
    );
}

#[tokio::test]
async fn test_session_shell_function_persists() {
    let mgr = manager();
    mgr.create("func".into(), None).await.unwrap();

    // Define a function
    mgr.exec("func", "greet() { echo \"hello $1\"; }", None, None)
        .await
        .unwrap();

    // Call it later
    let result = mgr.exec("func", "greet world", None, None).await.unwrap();
    assert_eq!(result.exit_code, 0);
    assert!(
        result.lines.iter().any(|l| l.contains("hello world")),
        "shell function should persist, got: {:?}",
        result.lines
    );
}

#[tokio::test]
async fn test_session_alias_persists() {
    let mgr = manager();
    mgr.create("alias".into(), None).await.unwrap();

    mgr.exec("alias", "alias ll='ls -la'", None, None).await.unwrap();

    // Using the alias
    let result = mgr.exec("alias", "ll /tmp", None, None).await.unwrap();
    assert_eq!(result.exit_code, 0);
    // Should produce ls -la output (drwx... lines)
    assert!(!result.lines.is_empty(), "alias should work, got no output");
}

// ---------------------------------------------------------------------------
// Working directory on creation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_session_initial_working_directory() {
    let mgr = manager();
    mgr.create("wd".into(), Some("/tmp".into())).await.unwrap();

    let result = mgr.exec("wd", "pwd", None, None).await.unwrap();
    assert!(
        result
            .lines
            .iter()
            .any(|l| l.contains("/tmp") || l.contains("/private/tmp")),
        "initial cwd should be /tmp, got: {:?}",
        result.lines
    );
}

// ---------------------------------------------------------------------------
// Multiple sequential commands
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_session_many_sequential_commands() {
    let mgr = manager();
    mgr.create("seq".into(), None).await.unwrap();

    for i in 0..20 {
        let result = mgr
            .exec("seq", &format!("echo command_{i}"), None, None)
            .await
            .unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(
            result
                .lines
                .iter()
                .any(|l| l.contains(&format!("command_{i}"))),
            "command {i} output missing"
        );
    }
}

// ---------------------------------------------------------------------------
// Timeout
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_session_timeout() {
    let mgr = manager();
    mgr.create("timeout".into(), None).await.unwrap();

    // Run a command that will timeout.
    // Note: timeout kills the foreground process. The session may or may not
    // survive depending on how bash handles the signal. We verify the timeout
    // is detected and the result is correct.
    let result = mgr.exec("timeout", "sleep 30", Some(2), None).await.unwrap();
    assert!(result.timed_out, "command should have timed out");
    assert_eq!(result.exit_code, 124, "exit code should be 124 for timeout");
    assert!(
        result.duration_seconds < 10.0,
        "duration should be close to timeout, not full command: {}s",
        result.duration_seconds
    );
}

// ---------------------------------------------------------------------------
// Security: dangerous commands blocked in sessions too
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_session_blocks_dangerous_commands() {
    let mgr = manager();
    mgr.create("sec".into(), None).await.unwrap();

    let result = mgr.exec("sec", "rm -rf /", None, None).await.unwrap();
    assert_eq!(result.exit_code, -1);
    assert!(result.lines[0].contains("blocked"));

    // Session should still work after a blocked command
    let result = mgr.exec("sec", "echo safe", None, None).await.unwrap();
    assert_eq!(result.exit_code, 0);
    assert!(result.lines.iter().any(|l| l.contains("safe")));
}

// ---------------------------------------------------------------------------
// Multiple sessions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_multiple_independent_sessions() {
    let mgr = manager();
    mgr.create("a".into(), None).await.unwrap();
    mgr.create("b".into(), None).await.unwrap();

    // Set different state in each
    mgr.exec("a", "export WHICH=session_a", None, None).await.unwrap();
    mgr.exec("b", "export WHICH=session_b", None, None).await.unwrap();

    // Verify they're independent
    let ra = mgr.exec("a", "echo $WHICH", None, None).await.unwrap();
    let rb = mgr.exec("b", "echo $WHICH", None, None).await.unwrap();

    assert!(ra.lines.iter().any(|l| l.contains("session_a")));
    assert!(rb.lines.iter().any(|l| l.contains("session_b")));
}

// ---------------------------------------------------------------------------
// List sessions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_list_sessions() {
    let mgr = manager();
    mgr.create("x".into(), None).await.unwrap();
    mgr.create("y".into(), None).await.unwrap();

    let list = mgr.list().await;
    assert_eq!(list.len(), 2);
    let ids: Vec<&str> = list.iter().map(|s| s.id.as_str()).collect();
    assert!(ids.contains(&"x"));
    assert!(ids.contains(&"y"));
}

// ---------------------------------------------------------------------------
// Nonexistent session
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_exec_nonexistent_session() {
    let mgr = manager();
    let err = mgr.exec("nope", "echo hi", None, None).await;
    assert!(err.is_err());
    assert!(err.unwrap_err().contains("no session"));
}

// ---------------------------------------------------------------------------
// PTY: isatty verification -- the whole reason we added PTY support
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_session_pty_isatty() {
    let mgr = manager();
    mgr.create("tty".into(), None).await.unwrap();

    let result = mgr
        .exec(
            "tty",
            "python3 -c \"import os; print(os.isatty(0), os.isatty(1), os.isatty(2))\"",
            None,
            None,
        )
        .await
        .unwrap();

    assert_eq!(result.exit_code, 0);
    assert!(
        result.lines.iter().any(|l| l.contains("True True True")),
        "all FDs should report isatty=True with PTY, got: {:?}",
        result.lines
    );
}
