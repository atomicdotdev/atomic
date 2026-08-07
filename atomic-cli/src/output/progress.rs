//! Progress bar utilities for long-running CLI operations.
//!
//! This module provides helpers for creating and managing progress indicators
//! in the Atomic CLI. Progress bars help users understand that operations are
//! in progress and how much work remains to be done.
//!
//! # Design Philosophy
//!
//! Progress indicators should:
//! 1. Be unobtrusive for quick operations
//! 2. Provide meaningful feedback for long operations
//! 3. Support both determinate (known total) and indeterminate (unknown total) progress
//! 4. Have consistent styling across all commands
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic::output::progress;
//!
//! // For operations with unknown duration
//! let spinner = progress::create_spinner("Processing files...");
//! // ... do work ...
//! progress::finish_success(&spinner, "Processed 42 files");
//!
//! // For operations with known total
//! let bar = progress::create_progress_bar(100, "Downloading changes");
//! for i in 0..100 {
//!     // ... do work ...
//!     bar.inc(1);
//! }
//! progress::finish_success(&bar, "Download complete");
//! ```
//!
//! # Timing Behavior
//!
//! By default, spinners and progress bars are configured with a "steady tick"
//! that updates periodically. For very quick operations, consider using
//! [`create_hidden_progress`] to avoid visual noise.

use std::time::Duration;

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

// Style Constants

/// Default spinner characters for indeterminate progress.
///
/// These characters create a smooth spinning animation that works well
/// in most terminals.
const SPINNER_CHARS: &str = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏";

/// Alternative spinner for terminals that don't support Unicode.
#[allow(dead_code)]
const ASCII_SPINNER_CHARS: &str = "|/-\\";

/// Default tick interval for spinners (in milliseconds).
const SPINNER_TICK_MS: u64 = 80;

/// Default tick interval for progress bars (in milliseconds).
const PROGRESS_TICK_MS: u64 = 100;

// Progress Bar Creation

/// Create a spinner for operations with unknown duration.
///
/// Spinners are used when we don't know how long an operation will take
/// or how much work remains. They provide visual feedback that something
/// is happening without showing percentage progress.
///
/// # Arguments
///
/// * `message` - The message to display next to the spinner
///
/// # Returns
///
/// A configured [`ProgressBar`] with spinner styling.
///
/// # Example
///
/// ```rust,ignore
/// let spinner = progress::create_spinner("Connecting to remote...");
/// // ... perform operation ...
/// progress::finish_success(&spinner, "Connected!");
/// ```
pub fn create_spinner(message: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(spinner_style());
    pb.set_message(message.to_string());
    pb.enable_steady_tick(Duration::from_millis(SPINNER_TICK_MS));
    pb
}

/// Create a progress bar for operations with known total.
///
/// Use this when you know ahead of time how many items will be processed
/// or how many bytes will be transferred.
///
/// # Arguments
///
/// * `total` - The total number of units to process
/// * `message` - The message to display with the progress bar
///
/// # Returns
///
/// A configured [`ProgressBar`] with progress styling.
///
/// # Example
///
/// ```rust,ignore
/// let files = vec!["a.rs", "b.rs", "c.rs"];
/// let bar = progress::create_progress_bar(files.len() as u64, "Processing files");
/// for file in files {
///     process(file);
///     bar.inc(1);
/// }
/// progress::finish_success(&bar, "All files processed");
/// ```
pub fn create_progress_bar(total: u64, message: &str) -> ProgressBar {
    let pb = ProgressBar::new(total);
    pb.set_style(progress_style());
    pb.set_message(message.to_string());
    pb.enable_steady_tick(Duration::from_millis(PROGRESS_TICK_MS));
    pb
}

/// Create a progress bar for byte-based operations (downloads, transfers).
///
/// Similar to [`create_progress_bar`] but uses byte-friendly formatting
/// (KB, MB, GB) instead of raw counts.
///
/// # Arguments
///
/// * `total_bytes` - The total number of bytes to process
/// * `message` - The message to display with the progress bar
///
/// # Returns
///
/// A configured [`ProgressBar`] with byte-progress styling.
pub fn create_byte_progress_bar(total_bytes: u64, message: &str) -> ProgressBar {
    let pb = ProgressBar::new(total_bytes);
    pb.set_style(byte_progress_style());
    pb.set_message(message.to_string());
    pb.enable_steady_tick(Duration::from_millis(PROGRESS_TICK_MS));
    pb
}

/// Create a hidden (non-visual) progress bar for tracking without display.
///
/// Useful for operations where you want to track progress programmatically
/// but don't want visual output (e.g., in non-interactive contexts).
///
/// # Arguments
///
/// * `total` - The total number of units to track
///
/// # Returns
///
/// A [`ProgressBar`] that tracks position but produces no output.
pub fn create_hidden_progress(total: u64) -> ProgressBar {
    let pb = ProgressBar::hidden();
    pb.set_length(total);
    pb
}

// Multi-Progress Support

/// Create a multi-progress container for parallel progress bars.
///
/// A [`MultiProgress`] allows displaying multiple progress bars
/// simultaneously, useful for parallel operations like downloading
/// multiple files.
pub fn create_multi_progress() -> MultiProgress {
    MultiProgress::new()
}

/// Add a spinner to a multi-progress container.
///
/// # Arguments
///
/// * `mp` - The multi-progress container
/// * `message` - The spinner message
pub fn add_spinner(mp: &MultiProgress, message: &str) -> ProgressBar {
    let pb = mp.add(ProgressBar::new_spinner());
    pb.set_style(spinner_style());
    pb.set_message(message.to_string());
    pb.enable_steady_tick(Duration::from_millis(SPINNER_TICK_MS));
    pb
}

/// Add a progress bar to a multi-progress container.
///
/// # Arguments
///
/// * `mp` - The multi-progress container
/// * `total` - The total number of units
/// * `message` - The progress bar message
pub fn add_progress_bar(mp: &MultiProgress, total: u64, message: &str) -> ProgressBar {
    let pb = mp.add(ProgressBar::new(total));
    pb.set_style(progress_style());
    pb.set_message(message.to_string());
    pb.enable_steady_tick(Duration::from_millis(PROGRESS_TICK_MS));
    pb
}

/// Suspend multi-progress rendering to run a closure.
///
/// This temporarily hides the progress bars so that other output
/// (e.g., log messages, prompts) can be displayed cleanly.
///
/// # Arguments
///
/// * `mp` - The multi-progress container
/// * `f` - The closure to run while progress is suspended
pub fn suspend<R>(mp: &MultiProgress, f: impl FnOnce() -> R) -> R {
    mp.suspend(f)
}

// Progress Bar Completion

/// Write a completion message directly to stderr when the progress bar is
/// hidden.
///
/// `indicatif` defaults to a stderr draw target that suppresses itself when
/// stderr is not a terminal. That is the right call for the *animation* — a
/// spinner in a log file is noise — but `finish_with_message` goes through the
/// same draw target, so the final outcome line disappeared along with it. A
/// piped `atomic view switch` printed nothing at all: no confirmation, and no
/// "N conflicts detected" warning either.
///
/// So when the bar is hidden, emit the message ourselves. Plain text, no ANSI:
/// the consumer is a pipe, a CI log, or an agent.
fn emit_if_hidden(pb: &ProgressBar, prefix: &str, message: &str) {
    if pb.is_hidden() {
        eprintln!("{} {}", prefix, message);
    }
}

/// Finish a progress bar with a success message.
///
/// This clears the progress bar and displays a success message in green.
///
/// # Arguments
///
/// * `pb` - The progress bar to finish
/// * `message` - The success message to display
///
/// # Example
///
/// ```rust,ignore
/// let spinner = progress::create_spinner("Working...");
/// // ... do work ...
/// progress::finish_success(&spinner, "Done!");
/// ```
pub fn finish_success(pb: &ProgressBar, message: &str) {
    pb.set_style(success_style());
    pb.finish_with_message(message.to_string());
    emit_if_hidden(pb, "✓", message);
}

/// Finish a progress bar with a warning message.
///
/// This clears the progress bar and displays a warning message in yellow.
///
/// # Arguments
///
/// * `pb` - The progress bar to finish
/// * `message` - The warning message to display
pub fn finish_warning(pb: &ProgressBar, message: &str) {
    pb.set_style(warning_style());
    pb.finish_with_message(message.to_string());
    emit_if_hidden(pb, "⚠", message);
}

/// Finish and completely clear a progress bar from the terminal.
///
/// Unlike `finish_success`/`finish_error`, this removes the progress bar
/// entirely without leaving a message.
///
/// # Arguments
///
/// * `pb` - The progress bar to clear
pub fn finish_and_clear(pb: &ProgressBar) {
    pb.finish_and_clear();
}

/// Finish a progress bar with an error message.
///
/// This clears the progress bar and displays an error message in red.
///
/// # Arguments
///
/// * `pb` - The progress bar to finish
/// * `message` - The error message to display
///
/// # Example
///
/// ```rust,ignore
/// let spinner = progress::create_spinner("Connecting...");
/// if let Err(e) = connect() {
///     progress::finish_error(&spinner, &format!("Connection failed: {}", e));
/// }
/// ```
pub fn finish_error(pb: &ProgressBar, message: &str) {
    pb.set_style(error_style());
    pb.finish_with_message(message.to_string());
    emit_if_hidden(pb, "✗", message);
}

// Progress Styles

/// Get the default spinner style.
///
/// Format: `⠋ Message...`
fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner:.cyan} {msg}")
        .expect("Invalid spinner template")
        .tick_chars(SPINNER_CHARS)
}

/// Get the default progress bar style.
///
/// Format: `[████████░░░░░░░░] 50/100 Message (50%)`
fn progress_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{spinner:.cyan} [{bar:30.cyan/dim}] {pos}/{len} {msg} ({percent}%)",
    )
    .expect("Invalid progress template")
    .tick_chars(SPINNER_CHARS)
    .progress_chars("█▓░")
}

/// Get the success completion style.
///
/// Format: `✓ Message` (in green)
fn success_style() -> ProgressStyle {
    ProgressStyle::with_template("{prefix:.green} {msg:.green}")
        .expect("Invalid success template")
        .tick_chars("✓ ")
}

/// Get the error completion style.
///
/// Format: `✗ Message` (in red)
fn error_style() -> ProgressStyle {
    ProgressStyle::with_template("{prefix:.red} {msg:.red}")
        .expect("Invalid error template")
        .tick_chars("✗ ")
}

/// Get the byte-progress bar style.
///
/// Format: `⠋ [████████░░░░░░░░] 1.5 MB/3.0 MB Message (50%)`
fn byte_progress_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{spinner:.cyan} [{bar:30.cyan/dim}] {bytes}/{total_bytes} {msg} ({percent}%)",
    )
    .expect("Invalid byte progress template")
    .tick_chars(SPINNER_CHARS)
    .progress_chars("█▓░")
}

/// Get the warning completion style.
///
/// Format: `⚠ Message` (in yellow)
fn warning_style() -> ProgressStyle {
    ProgressStyle::with_template("{prefix:.yellow} {msg:.yellow}")
        .expect("Invalid warning template")
        .tick_chars("⚠ ")
}

// Utility Functions

/// Set the position of a progress bar.
///
/// # Arguments
///
/// * `pb` - The progress bar
/// * `pos` - The new position
pub fn set_position(pb: &ProgressBar, pos: u64) {
    pb.set_position(pos);
}

/// Set the message of a progress bar.
///
/// # Arguments
///
/// * `pb` - The progress bar
/// * `message` - The new message
pub fn set_message(pb: &ProgressBar, message: &str) {
    pb.set_message(message.to_string());
}

/// Check if a progress bar is finished.
///
/// # Arguments
///
/// * `pb` - The progress bar to check
pub fn is_finished(pb: &ProgressBar) -> bool {
    pb.is_finished()
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------------
    // Creation Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_create_spinner() {
        let spinner = create_spinner("Testing...");
        assert!(!spinner.is_finished());
        spinner.finish();
        assert!(spinner.is_finished());
    }

    #[test]
    fn test_create_progress_bar() {
        let bar = create_progress_bar(100, "Testing...");
        assert_eq!(bar.length(), Some(100));
        assert_eq!(bar.position(), 0);
        bar.finish();
    }

    #[test]
    fn test_create_byte_progress_bar() {
        let bar = create_byte_progress_bar(1024 * 1024, "Downloading...");
        assert_eq!(bar.length(), Some(1024 * 1024));
        bar.finish();
    }

    #[test]
    fn test_create_hidden_progress() {
        let bar = create_hidden_progress(50);
        assert_eq!(bar.length(), Some(50));
        bar.inc(10);
        assert_eq!(bar.position(), 10);
        bar.finish();
    }

    // -------------------------------------------------------------------------
    // Progress Operations Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_progress_bar_increment() {
        let bar = create_progress_bar(100, "Working...");
        bar.inc(1);
        assert_eq!(bar.position(), 1);
        bar.inc(5);
        assert_eq!(bar.position(), 6);
        bar.finish();
    }

    #[test]
    fn test_progress_bar_set_position() {
        let bar = create_progress_bar(100, "Working...");
        set_position(&bar, 50);
        assert_eq!(bar.position(), 50);
        set_position(&bar, 75);
        assert_eq!(bar.position(), 75);
        bar.finish();
    }

    #[test]
    fn test_set_message() {
        let bar = create_progress_bar(100, "Initial message");
        set_message(&bar, "Updated message");
        // We can't easily verify the message content, but this ensures it doesn't panic
        bar.finish();
    }

    // -------------------------------------------------------------------------
    // Completion Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_finish_success() {
        let spinner = create_spinner("Working...");
        finish_success(&spinner, "Done!");
        assert!(spinner.is_finished());
    }

    #[test]
    fn test_finish_error() {
        let spinner = create_spinner("Working...");
        finish_error(&spinner, "Failed!");
        assert!(spinner.is_finished());
    }

    #[test]
    fn test_finish_warning() {
        let spinner = create_spinner("Working...");
        finish_warning(&spinner, "Completed with warnings");
        assert!(spinner.is_finished());
    }

    #[test]
    fn test_finish_and_clear() {
        let spinner = create_spinner("Working...");
        finish_and_clear(&spinner);
        assert!(spinner.is_finished());
    }

    // -------------------------------------------------------------------------
    // Multi-Progress Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_create_multi_progress() {
        let mp = create_multi_progress();
        let _ = mp; // Just ensure it can be created
    }

    #[test]
    fn test_add_spinner_to_multi() {
        let mp = create_multi_progress();
        let spinner = add_spinner(&mp, "Task 1");
        assert!(!spinner.is_finished());
        spinner.finish();
    }

    #[test]
    fn test_add_progress_bar_to_multi() {
        let mp = create_multi_progress();
        let bar = add_progress_bar(&mp, 100, "Task 1");
        assert_eq!(bar.length(), Some(100));
        bar.finish();
    }

    #[test]
    fn test_multiple_bars_in_multi() {
        let mp = create_multi_progress();
        let bar1 = add_progress_bar(&mp, 100, "Task 1");
        let bar2 = add_progress_bar(&mp, 50, "Task 2");
        let spinner = add_spinner(&mp, "Task 3");

        bar1.inc(10);
        bar2.inc(5);

        assert_eq!(bar1.position(), 10);
        assert_eq!(bar2.position(), 5);

        bar1.finish();
        bar2.finish();
        spinner.finish();
    }

    // -------------------------------------------------------------------------
    // Suspend Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_suspend() {
        let mp = create_multi_progress();
        let _ = add_spinner(&mp, "Working...");

        let result = suspend(&mp, || {
            // Simulate some work that needs progress suspended
            42
        });

        assert_eq!(result, 42);
    }

    // -------------------------------------------------------------------------
    // Is Finished Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_is_finished() {
        let bar = create_progress_bar(100, "Working...");
        assert!(!is_finished(&bar));
        bar.finish();
        assert!(is_finished(&bar));
    }

    // -------------------------------------------------------------------------
    // Style Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_styles_dont_panic() {
        // These functions are called internally, but let's ensure the templates are valid
        let _ = spinner_style();
        let _ = progress_style();
        let _ = byte_progress_style();
        let _ = success_style();
        let _ = error_style();
        let _ = warning_style();
    }

    // -------------------------------------------------------------------------
    // Edge Case Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_zero_length_progress() {
        let bar = create_progress_bar(0, "Empty");
        bar.finish();
        assert!(bar.is_finished());
    }

    #[test]
    fn test_large_progress() {
        let bar = create_progress_bar(u64::MAX, "Large");
        bar.inc(1);
        assert_eq!(bar.position(), 1);
        bar.finish();
    }

    #[test]
    fn test_empty_message() {
        let spinner = create_spinner("");
        spinner.finish();
    }

    #[test]
    fn test_unicode_message() {
        let spinner = create_spinner("処理中... 🚀");
        spinner.finish();
    }

    #[test]
    fn test_progress_beyond_total() {
        let bar = create_progress_bar(10, "Testing");
        for _ in 0..20 {
            bar.inc(1);
        }
        // Should handle overflow gracefully
        assert_eq!(bar.position(), 20);
        bar.finish();
    }
}
