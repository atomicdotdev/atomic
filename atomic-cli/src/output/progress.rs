#![allow(dead_code)]
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

// =============================================================================
// Style Constants
// =============================================================================

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

// =============================================================================
// Progress Bar Creation
// =============================================================================

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

/// Create a progress bar for byte-based operations (like downloads).
///
/// This is similar to [`create_progress_bar`] but uses byte-appropriate
/// formatting (e.g., "1.5 MiB / 10 MiB").
///
/// # Arguments
///
/// * `total_bytes` - The total number of bytes to transfer
/// * `message` - The message to display with the progress bar
///
/// # Returns
///
/// A configured [`ProgressBar`] with byte progress styling.
///
/// # Example
///
/// ```rust,ignore
/// let bar = progress::create_byte_progress_bar(10_000_000, "Downloading");
/// // ... transfer data, calling bar.inc(bytes_received) ...
/// progress::finish_success(&bar, "Download complete");
/// ```
pub fn create_byte_progress_bar(total_bytes: u64, message: &str) -> ProgressBar {
    let pb = ProgressBar::new(total_bytes);
    pb.set_style(byte_progress_style());
    pb.set_message(message.to_string());
    pb.enable_steady_tick(Duration::from_millis(PROGRESS_TICK_MS));
    pb
}

/// Create a hidden progress bar that doesn't display anything.
///
/// Use this for operations that might be quick enough not to need
/// visual feedback, or when running in non-interactive mode.
/// The returned progress bar can still be incremented and finished,
/// but won't produce any output.
///
/// # Arguments
///
/// * `total` - The total number of units (used for percentage tracking)
///
/// # Returns
///
/// A hidden [`ProgressBar`] that produces no output.
///
/// # Example
///
/// ```rust,ignore
/// let bar = if verbose {
///     progress::create_progress_bar(100, "Working...")
/// } else {
///     progress::create_hidden_progress(100)
/// };
/// ```
pub fn create_hidden_progress(total: u64) -> ProgressBar {
    let pb = ProgressBar::hidden();
    pb.set_length(total);
    pb
}

// =============================================================================
// Multi-Progress Support
// =============================================================================

/// Create a multi-progress container for concurrent progress bars.
///
/// When running parallel operations, use a [`MultiProgress`] to manage
/// multiple progress bars that update simultaneously without interfering
/// with each other.
///
/// # Returns
///
/// A new [`MultiProgress`] container.
///
/// # Example
///
/// ```rust,ignore
/// let mp = progress::create_multi_progress();
/// let bar1 = mp.add(progress::create_progress_bar(100, "Task 1"));
/// let bar2 = mp.add(progress::create_progress_bar(50, "Task 2"));
///
/// // ... run tasks in parallel ...
///
/// progress::finish_success(&bar1, "Task 1 complete");
/// progress::finish_success(&bar2, "Task 2 complete");
/// ```
pub fn create_multi_progress() -> MultiProgress {
    MultiProgress::new()
}

/// Add a spinner to a multi-progress container.
///
/// # Arguments
///
/// * `mp` - The multi-progress container
/// * `message` - The message to display next to the spinner
///
/// # Returns
///
/// A configured [`ProgressBar`] with spinner styling, added to the container.
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
/// * `total` - The total number of units to process
/// * `message` - The message to display with the progress bar
///
/// # Returns
///
/// A configured [`ProgressBar`] with progress styling, added to the container.
pub fn add_progress_bar(mp: &MultiProgress, total: u64, message: &str) -> ProgressBar {
    let pb = mp.add(ProgressBar::new(total));
    pb.set_style(progress_style());
    pb.set_message(message.to_string());
    pb.enable_steady_tick(Duration::from_millis(PROGRESS_TICK_MS));
    pb
}

// =============================================================================
// Progress Bar Completion
// =============================================================================

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
}

/// Finish a progress bar with a warning message.
///
/// This clears the progress bar and displays a warning message in yellow.
///
/// # Arguments
///
/// * `pb` - The progress bar to finish
/// * `message` - The warning message to display
///
/// # Example
///
/// ```rust,ignore
/// let bar = progress::create_progress_bar(100, "Processing...");
/// // ... some items failed ...
/// progress::finish_warning(&bar, "Completed with 3 warnings");
/// ```
pub fn finish_warning(pb: &ProgressBar, message: &str) {
    pb.set_style(warning_style());
    pb.finish_with_message(message.to_string());
}

/// Finish and clear a progress bar without leaving a message.
///
/// Use this when you want the progress bar to disappear completely
/// after the operation is done.
///
/// # Arguments
///
/// * `pb` - The progress bar to clear
///
/// # Example
///
/// ```rust,ignore
/// let spinner = progress::create_spinner("Checking...");
/// // ... check completes quickly ...
/// progress::finish_and_clear(&spinner);
/// // No message is left behind
/// ```
pub fn finish_and_clear(pb: &ProgressBar) {
    pb.finish_and_clear();
}

// =============================================================================
// Progress Styles
// =============================================================================

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

/// Get the byte progress bar style.
///
/// Format: `[████████░░░░░░░░] 5.2 MiB / 10.0 MiB Message (52%)`
fn byte_progress_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{spinner:.cyan} [{bar:30.cyan/dim}] {bytes}/{total_bytes} {msg} ({percent}%)",
    )
    .expect("Invalid byte progress template")
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

/// Get the warning completion style.
///
/// Format: `⚠ Message` (in yellow)
fn warning_style() -> ProgressStyle {
    ProgressStyle::with_template("{prefix:.yellow} {msg:.yellow}")
        .expect("Invalid warning template")
        .tick_chars("⚠ ")
}

// =============================================================================
// Utility Functions
// =============================================================================

/// Suspend progress bars while executing a closure.
///
/// This is useful when you need to print something that shouldn't be
/// mixed with progress bar output, or when prompting for user input.
///
/// # Arguments
///
/// * `mp` - The multi-progress container to suspend
/// * `f` - The closure to execute while progress is suspended
///
/// # Returns
///
/// The result of the closure.
///
/// # Example
///
/// ```rust,ignore
/// let mp = progress::create_multi_progress();
/// let spinner = progress::add_spinner(&mp, "Working...");
///
/// // Need to ask the user something
/// let answer = progress::suspend(&mp, || {
///     dialoguer::Confirm::new()
///         .with_prompt("Continue?")
///         .interact()
/// });
/// ```
pub fn suspend<F, R>(mp: &MultiProgress, f: F) -> R
where
    F: FnOnce() -> R,
{
    mp.suspend(f)
}

/// Update the message of a progress bar.
///
/// This is a convenience function that handles the string conversion.
///
/// # Arguments
///
/// * `pb` - The progress bar to update
/// * `message` - The new message
pub fn set_message(pb: &ProgressBar, message: &str) {
    pb.set_message(message.to_string());
}

/// Update the position of a progress bar.
///
/// Use this to set an absolute position rather than incrementing.
///
/// # Arguments
///
/// * `pb` - The progress bar to update
/// * `position` - The new position
pub fn set_position(pb: &ProgressBar, position: u64) {
    pb.set_position(position);
}

/// Check if a progress bar is finished.
///
/// # Arguments
///
/// * `pb` - The progress bar to check
///
/// # Returns
///
/// `true` if the progress bar is finished, `false` otherwise.
pub fn is_finished(pb: &ProgressBar) -> bool {
    pb.is_finished()
}

// =============================================================================
// Tests
// =============================================================================

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
