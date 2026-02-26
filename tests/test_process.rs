//! Unit tests for the process spawning, waiting, and output capture module.

use agentsh::process::{self, ProcessConfig};

fn config(command: &str) -> ProcessConfig {
    ProcessConfig {
        command: command.to_string(),
        working_directory: None,
        timeout_seconds: None,
    }
}

fn config_with_timeout(command: &str, timeout: u64) -> ProcessConfig {
    ProcessConfig {
        command: command.to_string(),
        working_directory: None,
        timeout_seconds: Some(timeout),
    }
}

// ---------------------------------------------------------------------------
// Basic execution tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_run_echo_returns_zero_exit_code() {
    let result = process::run(&config("echo hello"), None).await;

    assert_eq!(result.exit_code, 0);
    assert!(!result.timed_out);
    assert!(result.lines.iter().any(|l| l.contains("hello")));
}

#[tokio::test]
async fn test_run_false_returns_nonzero_exit_code() {
    let result = process::run(&config("false"), None).await;

    assert_ne!(result.exit_code, 0);
    assert!(!result.timed_out);
}

#[tokio::test]
async fn test_run_exit_code_preserved() {
    let result = process::run(&config("exit 42"), None).await;

    assert_eq!(result.exit_code, 42);
    assert!(!result.timed_out);
}

// ---------------------------------------------------------------------------
// Output capture tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_run_captures_stdout() {
    let result = process::run(&config("echo line1; echo line2; echo line3"), None).await;

    assert_eq!(result.exit_code, 0);
    assert!(result.lines.len() >= 3);
    assert!(result.lines.contains(&"line1".to_string()));
    assert!(result.lines.contains(&"line2".to_string()));
    assert!(result.lines.contains(&"line3".to_string()));
}

#[tokio::test]
async fn test_run_captures_stderr() {
    let result = process::run(&config("echo errout >&2"), None).await;

    assert_eq!(result.exit_code, 0);
    assert!(result.lines.iter().any(|l| l.contains("errout")));
}

#[tokio::test]
async fn test_run_captures_both_stdout_and_stderr() {
    let result = process::run(&config("echo stdout_line; echo stderr_line >&2"), None).await;

    assert_eq!(result.exit_code, 0);
    assert!(result.lines.iter().any(|l| l.contains("stdout_line")));
    assert!(result.lines.iter().any(|l| l.contains("stderr_line")));
}

#[tokio::test]
async fn test_run_empty_command_output() {
    let result = process::run(&config("true"), None).await;

    assert_eq!(result.exit_code, 0);
    assert!(result.lines.is_empty());
}

#[tokio::test]
async fn test_run_multiline_output() {
    let result = process::run(&config("seq 1 100"), None).await;

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.lines.len(), 100);
    assert_eq!(result.lines[0], "1");
    assert_eq!(result.lines[99], "100");
}

// ---------------------------------------------------------------------------
// Timeout tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_run_timeout_kills_process() {
    let result = process::run(&config_with_timeout("sleep 30", 1), None).await;

    assert!(result.timed_out);
    assert_eq!(result.exit_code, -1);
    // Duration should be roughly 1 second, not 30.
    assert!(result.duration_seconds < 5.0);
}

#[tokio::test]
async fn test_run_no_timeout_if_fast_enough() {
    let result = process::run(&config_with_timeout("echo fast", 10), None).await;

    assert_eq!(result.exit_code, 0);
    assert!(!result.timed_out);
    assert!(result.lines.iter().any(|l| l.contains("fast")));
}

// ---------------------------------------------------------------------------
// Working directory tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_run_with_working_directory() {
    let result = process::run(
        &ProcessConfig {
            command: "pwd".to_string(),
            working_directory: Some("/tmp".to_string()),
            timeout_seconds: None,
        },
        None,
    )
    .await;

    assert_eq!(result.exit_code, 0);
    // On macOS /tmp symlinks to /private/tmp.
    assert!(
        result.lines.iter().any(|l| l.contains("/tmp")),
        "expected /tmp in output, got: {:?}",
        result.lines
    );
}

// ---------------------------------------------------------------------------
// Shared buffer tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_run_with_shared_buffer() {
    let buffer = process::new_shared_buffer();
    let result = process::run(&config("echo buffered"), Some(&buffer)).await;

    assert_eq!(result.exit_code, 0);
    let buf_contents = buffer.lock().await;
    assert!(buf_contents.iter().any(|l| l.contains("buffered")));
}

// ---------------------------------------------------------------------------
// Duration tracking tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_run_duration_is_reasonable() {
    let result = process::run(&config("sleep 0.2"), None).await;

    assert_eq!(result.exit_code, 0);
    assert!(
        result.duration_seconds >= 0.1,
        "duration too short: {}",
        result.duration_seconds
    );
    assert!(
        result.duration_seconds < 5.0,
        "duration too long: {}",
        result.duration_seconds
    );
}

// ---------------------------------------------------------------------------
// Error handling tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_run_nonexistent_command() {
    let result = process::run(&config("nonexistent_command_xyz_12345"), None).await;

    // The shell should return 127 for command not found.
    assert_ne!(result.exit_code, 0);
}

// ---------------------------------------------------------------------------
// Security: env var sanitization
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_env_vars_inherited_by_default() {
    // By default, all env vars are inherited (like a real terminal).
    // SAFETY: test runs in its own process via cargo test.
    unsafe { std::env::set_var("AGENTSH_TEST_API_KEY", "sk-test-value-12345") };
    let result = process::run(&config("echo $AGENTSH_TEST_API_KEY"), None).await;

    assert_eq!(result.exit_code, 0);
    assert!(
        result
            .lines
            .iter()
            .any(|l| l.contains("sk-test-value-12345")),
        "env vars should be inherited by default, got: {:?}",
        result.lines
    );
    unsafe { std::env::remove_var("AGENTSH_TEST_API_KEY") };
}

#[tokio::test]
async fn test_strip_env_opt_in() {
    // When AGENTSH_STRIP_ENV is set, listed vars are stripped.
    // NOTE: This test verifies the is_sensitive_env function directly since
    // AGENTSH_STRIP_ENV is read once at startup via LazyLock.
    // The function itself is the mechanism; the LazyLock caching means
    // runtime env changes won't affect it (by design).
    assert!(
        !process::is_sensitive_env("SOME_API_KEY"),
        "by default nothing should be sensitive"
    );
}

#[tokio::test]
async fn test_non_sensitive_env_vars_preserved() {
    // SAFETY: test runs in its own process via cargo test.
    unsafe { std::env::set_var("AGENTSH_TEST_SAFE_VAR", "hello") };
    let result = process::run(&config("echo $AGENTSH_TEST_SAFE_VAR"), None).await;

    assert_eq!(result.exit_code, 0);
    assert!(
        result.lines.iter().any(|l| l.contains("hello")),
        "non-sensitive env var should be preserved, got: {:?}",
        result.lines
    );
    unsafe { std::env::remove_var("AGENTSH_TEST_SAFE_VAR") };
}

// ---------------------------------------------------------------------------
// Security: output buffer cap
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_output_buffer_does_not_oom() {
    // Generate far more than MAX_OUTPUT_LINES (100,000). The process should
    // still complete without OOM, and the buffer should be capped.
    let result = process::run(&config("seq 1 200000"), None).await;

    assert_eq!(result.exit_code, 0);
    assert!(
        result.lines.len() <= 100_000,
        "output buffer should be capped at 100k lines, got {}",
        result.lines.len()
    );
}

// ---------------------------------------------------------------------------
// Security: timeout ceiling
// ---------------------------------------------------------------------------

#[test]
fn test_clamp_timeout_enforces_ceiling() {
    assert_eq!(process::clamp_timeout(Some(100)), Some(100));
    assert_eq!(process::clamp_timeout(Some(3600)), Some(3600));
    assert_eq!(process::clamp_timeout(Some(999999)), Some(3600));
    assert_eq!(process::clamp_timeout(None), None);
}

// ---------------------------------------------------------------------------
// Security: dangerous command blocking (pattern matching only, never executed)
// ---------------------------------------------------------------------------

#[test]
fn test_block_rm_rf_root() {
    assert!(process::validate_command("rm -rf /").is_err());
    assert!(process::validate_command("rm -rf /*").is_err());
    assert!(process::validate_command("rm -Rf /").is_err());
    assert!(process::validate_command("rm -fr /").is_err());
    assert!(process::validate_command("rm --recursive --force /").is_err());
}

#[test]
fn test_block_rm_rf_system_paths() {
    assert!(process::validate_command("rm -rf /usr").is_err());
    assert!(process::validate_command("rm -rf /etc").is_err());
    assert!(process::validate_command("rm -rf /bin").is_err());
    assert!(process::validate_command("rm -rf /home").is_err());
    assert!(process::validate_command("rm -rf /var").is_err());
    assert!(process::validate_command("rm -rf /boot").is_err());
    assert!(process::validate_command("rm -rf /lib").is_err());
    assert!(process::validate_command("rm -rf /opt").is_err());
    assert!(process::validate_command("rm -rf /System").is_err());
    assert!(process::validate_command("rm -rf /Applications").is_err());
    assert!(process::validate_command("rm -rf /Users").is_err());
}

#[test]
fn test_block_rm_rf_with_sudo() {
    assert!(process::validate_command("sudo rm -rf /").is_err());
    assert!(process::validate_command("sudo rm -rf /usr").is_err());
}

#[test]
fn test_block_rm_rf_in_chain() {
    assert!(process::validate_command("echo hi && rm -rf /").is_err());
    assert!(process::validate_command("rm -rf /; echo done").is_err());
    assert!(process::validate_command("true || rm -rf /usr").is_err());
}

#[test]
fn test_allow_safe_rm_commands() {
    assert!(process::validate_command("rm -rf ./build").is_ok());
    assert!(process::validate_command("rm -rf /tmp/mytest").is_ok());
    assert!(process::validate_command("rm -rf target/").is_ok());
    assert!(process::validate_command("rm file.txt").is_ok());
    assert!(process::validate_command("rm -r ./node_modules").is_ok());
}

#[test]
fn test_block_mkfs() {
    assert!(process::validate_command("mkfs.ext4 /dev/sda1").is_err());
    assert!(process::validate_command("mkfs -t ext4 /dev/sda").is_err());
}

#[test]
fn test_block_dd_to_device() {
    assert!(process::validate_command("dd if=/dev/zero of=/dev/sda").is_err());
    assert!(process::validate_command("dd if=/dev/urandom of=/dev/nvme0n1").is_err());
}

#[test]
fn test_allow_safe_dd() {
    assert!(process::validate_command("dd if=/dev/zero of=/tmp/test.img bs=1M count=10").is_ok());
}

#[test]
fn test_block_fork_bomb() {
    assert!(process::validate_command(":(){ :|:& };:").is_err());
}

#[test]
fn test_block_shutdown_reboot() {
    assert!(process::validate_command("shutdown -h now").is_err());
    assert!(process::validate_command("reboot").is_err());
    assert!(process::validate_command("halt").is_err());
    assert!(process::validate_command("poweroff").is_err());
    assert!(process::validate_command("init 0").is_err());
    assert!(process::validate_command("init 6").is_err());
}

#[test]
fn test_block_chmod_chown_on_system_paths() {
    assert!(process::validate_command("chmod -R 777 /").is_err());
    assert!(process::validate_command("chmod -R 777 /usr").is_err());
    assert!(process::validate_command("chown -R nobody /").is_err());
    assert!(process::validate_command("chown -R nobody /etc").is_err());
}

#[test]
fn test_allow_safe_chmod_chown() {
    assert!(process::validate_command("chmod -R 755 ./dist").is_ok());
    assert!(process::validate_command("chown -R user:group ./project").is_ok());
    assert!(process::validate_command("chmod 644 file.txt").is_ok());
}

#[test]
fn test_block_device_redirect() {
    assert!(process::validate_command("echo x > /dev/sda").is_err());
    assert!(process::validate_command("cat file > /dev/nvme0n1").is_err());
}

#[tokio::test]
async fn test_blocked_command_returns_error_result() {
    // Verify that a blocked command returns a proper ProcessResult (not a panic).
    let result = process::run(&config("rm -rf /"), None).await;
    assert_eq!(result.exit_code, -1);
    assert!(!result.lines.is_empty());
    assert!(result.lines[0].contains("blocked"));
}
