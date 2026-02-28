//! Output windowing and error pattern extraction for LLM-friendly command output.
//!
//! This module provides pure functions that take raw command output lines and produce
//! structured, token-efficient summaries suitable for LLM consumption.

use regex::Regex;
use serde::Serialize;
use std::sync::LazyLock;

/// The number of lines reserved for the "head" portion of windowed output.
const HEAD_LINES: usize = 10;

/// Default error patterns that match common build/test failure output.
static ERROR_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    let patterns = [
        r"(?i)\berror\b",
        r"(?i)\bfailed\b",
        r"(?i)\bfailure\b",
        r"(?i)\bfatal\b",
        r"(?i)\bpanic\b",
        r"(?i)\bexception\b",
        r"(?i)\btraceback\b",
        r"(?i)\bFAIL\b",
        r"(?i)\bdenied\b",
        r"(?i)\baborted\b",
    ];
    patterns
        .iter()
        .map(|p| Regex::new(p).expect("invalid error pattern regex"))
        .collect()
});

/// A windowed view of command output, optimized for LLM token efficiency.
#[derive(Debug, Clone, Serialize)]
pub struct OutputWindow {
    /// First N lines of output (usually invocation context / setup).
    pub head: Vec<String>,
    /// Last M lines of output (usually the result / error summary).
    pub tail: Vec<String>,
    /// Lines that matched error patterns, extracted from the full output.
    pub error_lines: Vec<String>,
    /// Total number of lines in the original output.
    pub total_lines: usize,
    /// Whether the output was truncated (head+tail < total).
    pub truncated: bool,
}

/// Window command output into head + tail sections for LLM consumption.
///
/// If the output fits within `max_lines`, returns it as-is in `head` with an empty `tail`.
/// Otherwise, splits into the first [`HEAD_LINES`] lines (head) and the remaining budget
/// as the tail from the end of output.
#[must_use]
pub fn window(lines: &[String], max_lines: usize) -> OutputWindow {
    let total_lines = lines.len();

    if total_lines <= max_lines {
        return OutputWindow {
            head: lines.to_vec(),
            tail: Vec::new(),
            error_lines: extract_errors(lines),
            total_lines,
            truncated: false,
        };
    }

    let head_count = HEAD_LINES.min(max_lines);
    let tail_count = max_lines.saturating_sub(head_count);

    let head = lines[..head_count].to_vec();
    let tail = if tail_count > 0 {
        let start = total_lines.saturating_sub(tail_count);
        lines[start..].to_vec()
    } else {
        Vec::new()
    };

    OutputWindow {
        head,
        tail,
        error_lines: extract_errors(lines),
        total_lines,
        truncated: true,
    }
}

/// Extract lines that match common error patterns from command output.
///
/// Scans each line against a set of regex patterns for errors, failures, panics,
/// exceptions, and other common failure indicators.
#[must_use]
pub fn extract_errors(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .filter(|line| ERROR_PATTERNS.iter().any(|re| re.is_match(line)))
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// ANSI escape code stripping (for PTY output)
// ---------------------------------------------------------------------------

/// Regex matching ANSI escape sequences (CSI sequences, OSC sequences, etc.).
static ANSI_ESCAPE: LazyLock<Regex> = LazyLock::new(|| {
    // Matches:
    // - CSI sequences: \x1b[ ... final_byte  (parameters can include 0-9;?<=>!)
    //   Covers standard ANSI, DEC private modes, and Kitty keyboard protocol
    // - OSC sequences: \x1b] ... ST          (e.g., terminal title)
    // - Simple escapes: \x1b followed by a single character
    // - Backspace sequences: char \x08 (used by some programs for bold/overstrike)
    Regex::new(
        r"\x1b\[[0-9;?<=>!]*[a-zA-Z~]|\x1b\][^\x07]*\x07|\x1b[()][0-9A-B]|\x1b[a-zA-Z]|.\x08",
    )
    .expect("invalid ANSI regex")
});

/// Strip ANSI escape codes from a string.
///
/// PTY output contains terminal formatting (colors, cursor movement, etc.)
/// that is meaningless to an LLM. This function removes it, leaving only
/// the visible text content.
#[must_use]
pub fn strip_ansi(s: &str) -> String {
    ANSI_ESCAPE.replace_all(s, "").to_string()
}
