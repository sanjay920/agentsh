//! Unit tests for the output windowing and error extraction module.

use agentsh::output::{extract_errors, window};

fn lines(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("line {i}")).collect()
}

// ---------------------------------------------------------------------------
// window() tests
// ---------------------------------------------------------------------------

#[test]
fn test_window_small_output_no_truncation() {
    let input = lines(5);
    let w = window(&input, 200);

    assert_eq!(w.total_lines, 5);
    assert!(!w.truncated);
    assert_eq!(w.head.len(), 5);
    assert!(w.tail.is_empty());
    assert_eq!(w.head[0], "line 0");
    assert_eq!(w.head[4], "line 4");
}

#[test]
fn test_window_exact_fit_no_truncation() {
    let input = lines(200);
    let w = window(&input, 200);

    assert_eq!(w.total_lines, 200);
    assert!(!w.truncated);
    assert_eq!(w.head.len(), 200);
    assert!(w.tail.is_empty());
}

#[test]
fn test_window_large_output_truncated() {
    let input = lines(500);
    let w = window(&input, 50);

    assert_eq!(w.total_lines, 500);
    assert!(w.truncated);
    // Head should be first 10 lines.
    assert_eq!(w.head.len(), 10);
    assert_eq!(w.head[0], "line 0");
    assert_eq!(w.head[9], "line 9");
    // Tail should be last 40 lines (50 - 10 head).
    assert_eq!(w.tail.len(), 40);
    assert_eq!(w.tail[0], "line 460");
    assert_eq!(w.tail[39], "line 499");
}

#[test]
fn test_window_empty_output() {
    let input: Vec<String> = Vec::new();
    let w = window(&input, 200);

    assert_eq!(w.total_lines, 0);
    assert!(!w.truncated);
    assert!(w.head.is_empty());
    assert!(w.tail.is_empty());
    assert!(w.error_lines.is_empty());
}

#[test]
fn test_window_max_lines_smaller_than_head() {
    // If max_lines is 3, head gets 3, tail gets 0.
    let input = lines(100);
    let w = window(&input, 3);

    assert!(w.truncated);
    assert_eq!(w.head.len(), 3);
    assert!(w.tail.is_empty());
    assert_eq!(w.head[0], "line 0");
    assert_eq!(w.head[2], "line 2");
}

#[test]
fn test_window_preserves_error_lines() {
    let input = vec![
        "Starting build...".to_string(),
        "Compiling foo".to_string(),
        "error: cannot find value `x`".to_string(),
        "Compiling bar".to_string(),
        "Build failed".to_string(),
    ];
    let w = window(&input, 200);

    assert!(!w.truncated);
    assert_eq!(w.error_lines.len(), 2); // "error:" and "failed"
}

// ---------------------------------------------------------------------------
// extract_errors() tests
// ---------------------------------------------------------------------------

#[test]
fn test_extract_errors_finds_common_patterns() {
    let input = vec![
        "INFO: starting server".to_string(),
        "error: compilation failed".to_string(),
        "WARNING: deprecated function".to_string(),
        "FAIL: test_login".to_string(),
        "fatal: not a git repository".to_string(),
        "panic at line 42".to_string(),
        "Traceback (most recent call last):".to_string(),
        "Exception: something went wrong".to_string(),
        "Permission denied".to_string(),
        "Operation aborted".to_string(),
        "All tests passed".to_string(),
    ];
    let errors = extract_errors(&input);

    // Should match: error, FAIL, fatal, panic, Traceback, Exception, denied, aborted
    // "failed" is also in the error line, and "All tests passed" should not match
    assert!(errors.iter().any(|e| e.contains("error: compilation")));
    assert!(errors.iter().any(|e| e.contains("FAIL: test_login")));
    assert!(errors.iter().any(|e| e.contains("fatal:")));
    assert!(errors.iter().any(|e| e.contains("panic")));
    assert!(errors.iter().any(|e| e.contains("Traceback")));
    assert!(errors.iter().any(|e| e.contains("Exception")));
    assert!(errors.iter().any(|e| e.contains("denied")));
    assert!(errors.iter().any(|e| e.contains("aborted")));
    // "All tests passed" should NOT be in errors
    assert!(!errors.iter().any(|e| e.contains("All tests passed")));
}

#[test]
fn test_extract_errors_case_insensitive() {
    let input = vec![
        "ERROR: something".to_string(),
        "Error: something else".to_string(),
        "error: lowercase".to_string(),
    ];
    let errors = extract_errors(&input);
    assert_eq!(errors.len(), 3);
}

#[test]
fn test_extract_errors_empty_input() {
    let errors = extract_errors(&[]);
    assert!(errors.is_empty());
}

#[test]
fn test_extract_errors_no_matches() {
    let input = vec![
        "INFO: all good".to_string(),
        "DEBUG: processing".to_string(),
        "OK: done".to_string(),
    ];
    let errors = extract_errors(&input);
    assert!(errors.is_empty());
}
