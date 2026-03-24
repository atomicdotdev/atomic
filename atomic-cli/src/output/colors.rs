//! Terminal color utilities for consistent CLI output theming.
//!
//! This module provides a centralized color scheme for the Atomic CLI,
//! ensuring consistent visual feedback across all commands. Colors are
//! designed to be intuitive and accessible, with support for disabling
//! colors when needed (e.g., in non-TTY contexts or via `--no-color`).
//!
//! # Design Philosophy
//!
//! The color scheme follows common conventions:
//! - **Green**: Success, additions, positive outcomes
//! - **Red**: Errors, deletions, warnings
//! - **Yellow**: Warnings, modifications, attention needed
//! - **Cyan**: Informational messages, stack/branch names
//! - **Blue**: Links, references, secondary information
//! - **Dim**: Supplementary information, hashes, timestamps
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic::output::colors;
//!
//! // Print a success message
//! println!("{}", colors::success("Operation completed successfully!"));
//!
//! // Print an error
//! eprintln!("{}", colors::error("Something went wrong"));
//!
//! // Format a file path
//! println!("Modified: {}", colors::path("src/main.rs"));
//! ```
//!
//! # Color Support Detection
//!
//! The module respects the `NO_COLOR` environment variable and terminal
//! capabilities. Use [`ColorMode::should_colorize`] to check if colors should be used,
//! or use the [`ColorMode`] enum to explicitly control color output.

use console::{style, StyledObject};

// Color Mode

/// Controls when colors should be used in output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorMode {
    /// Automatically detect whether to use colors (based on terminal support).
    #[default]
    Auto,
    /// Always use colors, even when output is not a terminal.
    Always,
    /// Never use colors.
    Never,
}

impl ColorMode {
    /// Check if colors should be used based on this mode.
    pub fn should_colorize(&self) -> bool {
        match self {
            Self::Auto => console::colors_enabled(),
            Self::Always => true,
            Self::Never => false,
        }
    }
}

impl std::fmt::Display for ColorMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auto => write!(f, "auto"),
            Self::Always => write!(f, "always"),
            Self::Never => write!(f, "never"),
        }
    }
}

impl std::str::FromStr for ColorMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "always" | "yes" | "true" | "1" => Ok(Self::Always),
            "never" | "no" | "false" | "0" => Ok(Self::Never),
            other => Err(format!(
                "Invalid color mode: '{}'. Use auto, always, or never.",
                other
            )),
        }
    }
}

// Status Characters

/// Single-character status indicators for file status display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusChar {
    /// File was added.
    Added,
    /// File was modified.
    Modified,
    /// File was deleted.
    Deleted,
    /// File was renamed.
    Renamed,
    /// File is untracked.
    Untracked,
    /// File has a conflict.
    Conflict,
    /// File is clean (unchanged).
    Clean,
}

impl StatusChar {
    /// Get the raw character for this status.
    pub fn char(&self) -> char {
        match self {
            Self::Added => 'A',
            Self::Modified => 'M',
            Self::Deleted => 'D',
            Self::Renamed => 'R',
            Self::Untracked => '?',
            Self::Conflict => 'C',
            Self::Clean => ' ',
        }
    }

    /// Get a styled version of this status character.
    pub fn styled(&self) -> StyledObject<char> {
        match self {
            Self::Added => style(self.char()).green(),
            Self::Modified => style(self.char()).yellow(),
            Self::Deleted => style(self.char()).red(),
            Self::Renamed => style(self.char()).cyan(),
            Self::Untracked => style(self.char()).red(),
            Self::Conflict => style(self.char()).red().bold(),
            Self::Clean => style(self.char()),
        }
    }
}

impl std::fmt::Display for StatusChar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.char())
    }
}

// Color Detection

// Status Colors

/// Format a success message in green.
///
/// Use this for positive outcomes, confirmations, and success states.
///
/// # Arguments
///
/// * `text` - The text to format
///
/// # Returns
///
/// A styled object that displays in green when printed.
///
/// # Example
///
/// ```rust,ignore
/// println!("{}", colors::success("Repository initialized successfully!"));
/// ```
pub fn success<D: std::fmt::Display>(text: D) -> StyledObject<D> {
    style(text).green()
}

/// Format a warning message in yellow.
///
/// Use this for warnings, cautions, or situations that need attention
/// but aren't errors.
///
/// # Arguments
///
/// * `text` - The text to format
///
/// # Returns
///
/// A styled object that displays in yellow when printed.
///
/// # Example
///
/// ```rust,ignore
/// println!("{}", colors::warning("File will be overwritten"));
/// ```
pub fn warning<D: std::fmt::Display>(text: D) -> StyledObject<D> {
    style(text).yellow()
}

/// Format an error message in red.
///
/// Use this for error messages, failures, and critical issues.
///
/// # Arguments
///
/// * `text` - The text to format
///
/// # Returns
///
/// A styled object that displays in red when printed.
///
/// # Example
///
/// ```rust,ignore
/// eprintln!("{}", colors::error("Failed to read file"));
/// ```
pub fn error<D: std::fmt::Display>(text: D) -> StyledObject<D> {
    style(text).red()
}

/// Format an informational message in cyan.
///
/// Use this for informational messages, tips, and neutral status updates.
///
/// # Arguments
///
/// * `text` - The text to format
///
/// # Returns
///
/// A styled object that displays in cyan when printed.
///
/// # Example
///
/// ```rust,ignore
/// println!("{}", colors::info("Processing files..."));
/// ```
pub fn info<D: std::fmt::Display>(text: D) -> StyledObject<D> {
    style(text).cyan()
}

/// Format a hint/suggestion message in dim style.
///
/// Use this for supplementary information, suggestions, or less important details.
///
/// # Arguments
///
/// * `text` - The text to format
///
/// # Returns
///
/// A styled object that displays in dim when printed.
///
/// # Example
///
/// ```rust,ignore
/// println!("{}", colors::hint("Tip: Use --help for more options"));
/// ```
pub fn hint<D: std::fmt::Display>(text: D) -> StyledObject<D> {
    style(text).dim()
}

// File Status Colors

/// Format added content in green.
///
/// Use this for newly added files, lines, or content.
///
/// # Arguments
///
/// * `text` - The text to format
///
/// # Returns
///
/// A styled object that displays in green when printed.
///
/// # Example
///
/// ```rust,ignore
/// println!("{}  {}", colors::added("+"), colors::added("new line"));
/// ```
pub fn added<D: std::fmt::Display>(text: D) -> StyledObject<D> {
    style(text).green()
}

/// Format deleted content in red.
///
/// Use this for removed files, lines, or content.
///
/// # Arguments
///
/// * `text` - The text to format
///
/// # Returns
///
/// A styled object that displays in red when printed.
///
/// # Example
///
/// ```rust,ignore
/// println!("{}  {}", colors::deleted("-"), colors::deleted("removed line"));
/// ```
pub fn deleted<D: std::fmt::Display>(text: D) -> StyledObject<D> {
    style(text).red()
}

/// Format modified content in yellow.
///
/// Use this for changed files or content that has been modified.
///
/// # Arguments
///
/// * `text` - The text to format
///
/// # Returns
///
/// A styled object that displays in yellow when printed.
///
/// # Example
///
/// ```rust,ignore
/// println!("{}  {}", colors::modified("M"), colors::modified("src/main.rs"));
/// ```
pub fn modified<D: std::fmt::Display>(text: D) -> StyledObject<D> {
    style(text).yellow()
}

/// Format untracked content in red (same as deleted for visibility).
///
/// Use this for files that are not yet tracked by the repository.
///
/// # Arguments
///
/// * `text` - The text to format
///
/// # Returns
///
/// A styled object that displays in red when printed.
///
/// # Example
///
/// ```rust,ignore
/// println!("{}  {}", colors::untracked("?"), colors::untracked("new_file.txt"));
/// ```
pub fn untracked<D: std::fmt::Display>(text: D) -> StyledObject<D> {
    style(text).red()
}

// Reference Colors

/// Format a file path in bold.
///
/// Use this for displaying file paths to make them stand out.
///
/// # Arguments
///
/// * `text` - The path to format
///
/// # Returns
///
/// A styled object that displays in bold when printed.
///
/// # Example
///
/// ```rust,ignore
/// println!("Modified: {}", colors::path("src/main.rs"));
/// ```
pub fn path<D: std::fmt::Display>(text: D) -> StyledObject<D> {
    style(text).bold()
}

/// Format a hash in dim style.
///
/// Use this for change hashes, state hashes, or other identifiers.
///
/// # Arguments
///
/// * `text` - The hash to format
///
/// # Returns
///
/// A styled object that displays in dim when printed.
///
/// # Example
///
/// ```rust,ignore
/// println!("Change: {}", colors::hash("ABC123DEF456..."));
/// ```
pub fn hash<D: std::fmt::Display>(text: D) -> StyledObject<D> {
    style(text).dim()
}

/// Format a stack/branch name in cyan and bold.
///
/// Use this for stack names to make them prominent.
///
/// # Arguments
///
/// * `text` - The stack name to format
///
/// # Returns
///
/// A styled object that displays in cyan and bold when printed.
///
/// # Example
///
/// ```rust,ignore
/// println!("On stack: {}", colors::stack("main"));
/// ```
pub fn stack<D: std::fmt::Display>(text: D) -> StyledObject<D> {
    style(text).cyan().bold()
}

/// Format a timestamp in dim style.
///
/// Use this for dates, times, and timestamps.
///
/// # Arguments
///
/// * `text` - The timestamp to format
///
/// # Returns
///
/// A styled object that displays in dim when printed.
///
/// # Example
///
/// ```rust,ignore
/// println!("Created: {}", colors::timestamp("2024-01-15 10:30:00"));
/// ```
pub fn timestamp<D: std::fmt::Display>(text: D) -> StyledObject<D> {
    style(text).dim()
}

/// Format an author name in default style.
///
/// Use this for displaying author names in logs and history.
///
/// # Arguments
///
/// * `text` - The author name to format
///
/// # Returns
///
/// A styled object (currently unstyled for readability).
///
/// # Example
///
/// ```rust,ignore
/// println!("Author: {}", colors::author("Jane Doe <jane@example.com>"));
/// ```
pub fn author<D: std::fmt::Display>(text: D) -> StyledObject<D> {
    style(text)
}

// Semantic Colors

/// Format emphasized text in bold.
///
/// Use this to emphasize important text.
///
/// # Arguments
///
/// * `text` - The text to emphasize
///
/// # Returns
///
/// A styled object that displays in bold when printed.
///
/// # Example
///
/// ```rust,ignore
/// println!("{} files changed", colors::emphasis(5));
/// ```
pub fn emphasis<D: std::fmt::Display>(text: D) -> StyledObject<D> {
    style(text).bold()
}

/// Format a command or code snippet in bold cyan.
///
/// Use this for displaying commands the user should run.
///
/// # Arguments
///
/// * `text` - The command to format
///
/// # Returns
///
/// A styled object that displays in bold cyan when printed.
///
/// # Example
///
/// ```rust,ignore
/// println!("Run {} to continue", colors::command("atomic add <file>"));
/// ```
pub fn command<D: std::fmt::Display>(text: D) -> StyledObject<D> {
    style(text).cyan().bold()
}

/// Format renamed content in cyan.
///
/// Use this for renamed files or moved content.
pub fn renamed<D: std::fmt::Display>(text: D) -> StyledObject<D> {
    style(text).cyan()
}

/// Format conflict content in bold red.
///
/// Use this for conflict markers or conflicted file indicators.
pub fn conflict<D: std::fmt::Display>(text: D) -> StyledObject<D> {
    style(text).red().bold()
}

// File Status Prefix

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------------
    // ColorMode Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_color_mode_default() {
        let mode = ColorMode::default();
        assert_eq!(mode, ColorMode::Auto);
    }

    #[test]
    fn test_color_mode_always() {
        let mode = ColorMode::Always;
        assert!(mode.should_colorize());
    }

    #[test]
    fn test_color_mode_never() {
        let mode = ColorMode::Never;
        assert!(!mode.should_colorize());
    }

    #[test]
    fn test_color_mode_from_str() {
        assert_eq!("auto".parse::<ColorMode>().unwrap(), ColorMode::Auto);
        assert_eq!("always".parse::<ColorMode>().unwrap(), ColorMode::Always);
        assert_eq!("never".parse::<ColorMode>().unwrap(), ColorMode::Never);
        assert_eq!("yes".parse::<ColorMode>().unwrap(), ColorMode::Always);
        assert_eq!("no".parse::<ColorMode>().unwrap(), ColorMode::Never);
        assert_eq!("true".parse::<ColorMode>().unwrap(), ColorMode::Always);
        assert_eq!("false".parse::<ColorMode>().unwrap(), ColorMode::Never);
        assert_eq!("1".parse::<ColorMode>().unwrap(), ColorMode::Always);
        assert_eq!("0".parse::<ColorMode>().unwrap(), ColorMode::Never);
    }

    #[test]
    fn test_color_mode_from_str_invalid() {
        let result = "invalid".parse::<ColorMode>();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid color mode"));
    }

    #[test]
    fn test_color_mode_display() {
        assert_eq!(ColorMode::Auto.to_string(), "auto");
        assert_eq!(ColorMode::Always.to_string(), "always");
        assert_eq!(ColorMode::Never.to_string(), "never");
    }

    // -------------------------------------------------------------------------
    // Status Color Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_success_returns_styled() {
        let styled = success("test");
        // We can't easily test the actual color, but we can ensure it doesn't panic
        let _ = styled.to_string();
    }

    #[test]
    fn test_warning_returns_styled() {
        let styled = warning("test");
        let _ = styled.to_string();
    }

    #[test]
    fn test_error_returns_styled() {
        let styled = error("test");
        let _ = styled.to_string();
    }

    #[test]
    fn test_info_returns_styled() {
        let styled = info("test");
        let _ = styled.to_string();
    }

    #[test]
    fn test_hint_returns_styled() {
        let styled = hint("test");
        let _ = styled.to_string();
    }

    // -------------------------------------------------------------------------
    // File Status Color Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_added_returns_styled() {
        let styled = added("new_file.rs");
        let _ = styled.to_string();
    }

    #[test]
    fn test_deleted_returns_styled() {
        let styled = deleted("old_file.rs");
        let _ = styled.to_string();
    }

    #[test]
    fn test_modified_returns_styled() {
        let styled = modified("changed.rs");
        let _ = styled.to_string();
    }

    #[test]
    fn test_untracked_returns_styled() {
        let styled = untracked("new.txt");
        let _ = styled.to_string();
    }

    #[test]
    fn test_renamed_returns_styled() {
        let styled = renamed("old.rs -> new.rs");
        let _ = styled.to_string();
    }

    // -------------------------------------------------------------------------
    // Reference Color Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_path_returns_styled() {
        let styled = path("src/main.rs");
        let _ = styled.to_string();
    }

    #[test]
    fn test_hash_returns_styled() {
        let styled = hash("ABC123DEF456");
        let _ = styled.to_string();
    }

    #[test]
    fn test_stack_returns_styled() {
        let styled = stack("main");
        let _ = styled.to_string();
    }

    #[test]
    fn test_timestamp_returns_styled() {
        let styled = timestamp("2024-01-15 10:30:00");
        let _ = styled.to_string();
    }

    #[test]
    fn test_author_returns_styled() {
        let styled = author("Jane Doe");
        let _ = styled.to_string();
    }

    // -------------------------------------------------------------------------
    // Semantic Color Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_emphasis_returns_styled() {
        let styled = emphasis("important");
        let _ = styled.to_string();
    }

    #[test]
    fn test_command_returns_styled() {
        let styled = command("atomic add");
        let _ = styled.to_string();
    }

    #[test]
    fn test_conflict_returns_styled() {
        let styled = conflict("CONFLICT");
        let _ = styled.to_string();
    }

    // -------------------------------------------------------------------------
    // StatusChar Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_status_char_chars() {
        assert_eq!(StatusChar::Added.char(), 'A');
        assert_eq!(StatusChar::Modified.char(), 'M');
        assert_eq!(StatusChar::Deleted.char(), 'D');
        assert_eq!(StatusChar::Renamed.char(), 'R');
        assert_eq!(StatusChar::Untracked.char(), '?');
        assert_eq!(StatusChar::Conflict.char(), 'C');
        assert_eq!(StatusChar::Clean.char(), ' ');
    }

    #[test]
    fn test_status_char_styled() {
        // Just ensure styling doesn't panic
        let _ = StatusChar::Added.styled().to_string();
        let _ = StatusChar::Modified.styled().to_string();
        let _ = StatusChar::Deleted.styled().to_string();
        let _ = StatusChar::Renamed.styled().to_string();
        let _ = StatusChar::Untracked.styled().to_string();
        let _ = StatusChar::Conflict.styled().to_string();
        let _ = StatusChar::Clean.styled().to_string();
    }

    #[test]
    fn test_status_char_display() {
        // StatusChar Display should work
        let _ = format!("{}", StatusChar::Added);
        let _ = format!("{}", StatusChar::Modified);
    }

    // -------------------------------------------------------------------------
    // Generic Type Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_color_functions_accept_string() {
        let _ = success(String::from("test"));
        let _ = warning(String::from("test"));
        let _ = error(String::from("test"));
    }

    #[test]
    fn test_color_functions_accept_numbers() {
        let _ = success(42);
        let _ = emphasis(100);
    }

    #[test]
    fn test_color_functions_accept_string_refs() {
        let s = "test";
        let _ = success(s);
        let _ = path(s);
    }
}
