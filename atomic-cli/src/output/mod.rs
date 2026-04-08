//! Output formatting utilities for the Atomic CLI.
//!
//! This module provides a comprehensive set of utilities for consistent,
//! user-friendly CLI output. It includes color theming, progress indicators,
//! and table formatting, all designed to guide users through their workflow.
//!
//! # Module Organization
//!
//! - [`colors`]: Terminal color utilities for consistent theming
//! - [`progress`]: Progress bars and spinners for long-running operations
//! - [`table`]: Tabular data formatting for lists and summaries
//!
//! # Design Philosophy
//!
//! The output utilities follow these principles:
//!
//! 1. **Consistency**: All commands use the same color scheme and styling
//! 2. **Accessibility**: Colors are optional and can be disabled
//! 3. **Clarity**: Output is designed to guide users, not confuse them
//! 4. **Performance**: Progress indicators don't impact operation speed
//!
//! # Quick Start
//!
//! ```rust,ignore
//! use atomic::output::{colors, progress, print_success, print_error};
//!
//! // Print colored status messages
//! print_success("Repository initialized successfully!");
//! print_error("Failed to connect to remote");
//!
//! // Show progress for long operations
//! let spinner = progress::create_spinner("Processing files...");
//! // ... do work ...
//! progress::finish_success(&spinner, "Processed 42 files");
//! ```
//!
//! # Color Support
//!
//! Colors are automatically detected based on terminal capabilities and
//! environment variables. Users can override this with `--color=always`,
//! `--color=never`, or by setting the `NO_COLOR` environment variable.
//!
//! ```rust,ignore
//! use atomic::output::colors::ColorMode;
//!
//! // Parse from command-line flag
//! let mode: ColorMode = "always".parse().unwrap();
//!
//! // Check if colors should be used
//! if mode.should_colorize() {
//!     println!("{}", colors::success("Colorful output!"));
//! }
//! ```

pub mod colors;
pub mod progress;
pub mod table;

// Re-export commonly used items for convenience
pub use colors::{
    added, author, command, conflict, deleted, emphasis, error, hash, hint, info, modified, path,
    renamed, success, timestamp, untracked, view, warning, ColorMode, StatusChar,
};

pub use table::{Alignment, Column, KeyValueTable, Row, Table};

pub use progress::{create_progress_bar, create_spinner, finish_error, finish_success};

// Convenience Print Functions

/// Print a success message to stdout.
///
/// The message is formatted in green with a checkmark prefix.
///
/// # Arguments
///
/// * `message` - The message to print
///
/// # Example
///
/// ```rust,ignore
/// print_success("Repository initialized successfully!");
/// // Output: ✓ Repository initialized successfully!
/// ```
pub fn print_success(message: &str) {
    println!("{} {}", success("✓"), success(message));
}

/// Print an error message to stderr.
///
/// The message is formatted in red with an X prefix.
///
/// # Arguments
///
/// * `message` - The message to print
///
/// # Example
///
/// ```rust,ignore
/// print_error("Failed to read file");
/// // Output: ✗ Failed to read file
/// ```
pub fn print_error(message: &str) {
    eprintln!("{} {}", error("✗"), error(message));
}

/// Print a warning message to stderr.
///
/// The message is formatted in yellow with a warning prefix.
///
/// # Arguments
///
/// * `message` - The message to print
///
/// # Example
///
/// ```rust,ignore
/// print_warning("File will be overwritten");
/// // Output: ⚠ File will be overwritten
/// ```
pub fn print_warning(message: &str) {
    eprintln!("{} {}", warning("⚠"), warning(message));
}

/// Print an informational message to stdout.
///
/// The message is formatted in cyan with an info prefix.
///
/// # Arguments
///
/// * `message` - The message to print
///
/// # Example
///
/// ```rust,ignore
/// print_info("Processing 42 files...");
/// // Output: ℹ Processing 42 files...
/// ```
pub fn print_info(message: &str) {
    println!("{} {}", info("ℹ"), info(message));
}

/// Print a hint/suggestion message to stdout.
///
/// The message is formatted in dim style, typically used for
/// guiding users on what to do next.
///
/// # Arguments
///
/// * `message` - The hint message to print
///
/// # Example
///
/// ```rust,ignore
/// print_hint("Run 'atomic add <file>' to track new files");
/// ```
pub fn print_hint(message: &str) {
    println!("{}", hint(message));
}

// Structured Output Helpers

/// Print a section header.
///
/// Use this to visually separate different sections of output.
///
/// # Arguments
///
/// * `title` - The section title
///
/// # Example
///
/// ```rust,ignore
/// print_section("Changes to be recorded:");
/// ```
pub fn print_section(title: &str) {
    println!("{}", emphasis(title));
}

/// Print a blank line for visual separation.
pub fn print_blank() {
    println!();
}

// Next Steps Helper

/// Print a "next steps" section to guide users.
///
/// This function formats a list of suggested commands that the user
/// might want to run next. It's designed to help users learn the
/// workflow and discover related commands.
///
/// # Arguments
///
/// * `steps` - A slice of (command, description) pairs
///
/// # Example
///
/// ```rust,ignore
/// print_next_steps(&[
///     ("atomic add <files>", "Add files to track"),
///     ("atomic record -m \"...\"", "Record your changes"),
///     ("atomic status", "See what's changed"),
/// ]);
/// ```
///
/// # Output
///
/// ```text
/// Next steps:
///   atomic add <files>      Add files to track
///   atomic record -m "..."  Record your changes
///   atomic status           See what's changed
/// ```
pub fn print_next_steps(steps: &[(&str, &str)]) {
    if steps.is_empty() {
        return;
    }

    println!();
    println!("{}", hint("Next steps:"));

    // Calculate max command width for alignment
    let max_cmd_width = steps.iter().map(|(cmd, _)| cmd.len()).max().unwrap_or(0);

    for (cmd, desc) in steps {
        let padding = max_cmd_width - cmd.len() + 2;
        println!("  {}{}{}", command(cmd), " ".repeat(padding), hint(desc));
    }
}

// Confirmation Prompt

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_color_mode_re_exported() {
        let _mode = ColorMode::Auto;
    }

    #[test]
    fn test_status_char_re_exported() {
        let _char = StatusChar::Added;
    }

    #[test]
    fn test_alignment_re_exported() {
        let _align = Alignment::Left;
    }

    #[test]
    fn test_column_re_exported() {
        let _col = Column::new("Test");
    }

    #[test]
    fn test_table_re_exported() {
        let _table = Table::new();
    }

    #[test]
    fn test_kv_table_re_exported() {
        let _table = KeyValueTable::new();
    }

    #[test]
    fn test_row_re_exported() {
        let _row = Row::new();
    }

    // Note: The print functions are difficult to test without capturing stdout,
    // but we can at least verify they compile and don't panic with basic input.

    #[test]
    fn test_color_functions_re_exported() {
        // Just verify these compile
        let _ = success("test");
        let _ = warning("test");
        let _ = error("test");
        let _ = info("test");
        let _ = hint("test");
        let _ = added("test");
        let _ = deleted("test");
        let _ = modified("test");
        let _ = path("test");
        let _ = hash("test");
        let _ = view("test");
        let _ = timestamp("test");
        let _ = author("test");
        let _ = emphasis("test");
        let _ = command("test");
        let _ = conflict("test");
        let _ = untracked("test");
        let _ = renamed("test");
    }
}
