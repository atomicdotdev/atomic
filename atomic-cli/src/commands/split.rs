#![allow(dead_code)]
//! The `split` command for creating a new stack from an existing one.
//!
//! This module implements the `atomic split` command, which creates a new
//! stack by forking from an existing stack. This is a convenience wrapper
//! around `atomic stack new --from`.
//!
//! # Usage
//!
//! ```text
//! atomic split [OPTIONS] <NAME>
//!
//! Arguments:
//!   <NAME>  Name of the new stack to create
//!
//! Options:
//!       --stack <SOURCE>  Source stack to split from (default: current stack)
//!   -s, --switch          Switch to the new stack after creating it
//!   -h, --help            Print help information
//! ```
//!
//! # Examples
//!
//! Split from current stack:
//! ```text
//! $ atomic split experimental
//! Created stack: experimental (forked from dev with 5 changes)
//! ```
//!
//! Split from a specific stack:
//! ```text
//! $ atomic split hotfix --stack release-1.0
//! Created stack: hotfix (forked from release-1.0 with 42 changes)
//! ```
//!
//! Split and switch:
//! ```text
//! $ atomic split feature-auth --switch
//! Created stack: feature-auth (forked from dev with 10 changes)
//! Switched to stack: feature-auth
//! ```

use clap::Parser;

use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_success, stack as style_stack};

// =============================================================================
// Constants
// =============================================================================

/// Maximum length for a stack name.
const MAX_STACK_NAME_LENGTH: usize = 255;

/// Characters not allowed in stack names.
const INVALID_CHARS: &[char] = &['/', '\\', '\0', ':', '*', '?', '"', '<', '>', '|', ' '];

// =============================================================================
// Stack Name Validation
// =============================================================================

/// Validate a stack name.
fn validate_stack_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Stack name cannot be empty".to_string());
    }

    if name.len() > MAX_STACK_NAME_LENGTH {
        return Err(format!(
            "Stack name cannot exceed {} characters",
            MAX_STACK_NAME_LENGTH
        ));
    }

    for c in INVALID_CHARS {
        if name.contains(*c) {
            let char_desc = match c {
                ' ' => "spaces".to_string(),
                '\0' => "null characters".to_string(),
                _ => format!("'{}'", c),
            };
            return Err(format!("Stack name cannot contain {}", char_desc));
        }
    }

    if name == "." || name == ".." {
        return Err("Stack name cannot be '.' or '..'".to_string());
    }

    if name.starts_with('.') {
        return Err("Stack name cannot start with a dot".to_string());
    }

    if name.ends_with('.') {
        return Err("Stack name cannot end with a dot".to_string());
    }

    Ok(())
}

// =============================================================================
// Split Command
// =============================================================================

/// Split a stack (create a new stack from an existing one).
///
/// Creates a new stack by forking from an existing stack. All changes from
/// the source stack are copied to the new stack, preserving history.
///
/// This is equivalent to `atomic stack new <NAME> --from <SOURCE>`.
#[derive(Parser, Debug, Clone)]
#[command(name = "split")]
pub struct Split {
    /// Name of the new stack to create.
    ///
    /// Stack names should be descriptive and follow a naming convention
    /// like `feature-*`, `bugfix-*`, `release-*`, etc.
    #[arg(value_name = "NAME")]
    pub name: String,

    /// Source stack to split from.
    ///
    /// If not specified, splits from the current stack.
    #[arg(long = "stack", value_name = "SOURCE")]
    pub source: Option<String>,

    /// Switch to the new stack after creating it.
    ///
    /// By default, the current stack remains unchanged after splitting.
    /// Use this flag to automatically switch to the new stack.
    #[arg(long, short = 's')]
    pub switch: bool,
}

impl Split {
    /// Create a new Split command with the given stack name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            source: None,
            switch: false,
        }
    }

    /// Set the source stack to split from.
    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    /// Set whether to switch to the new stack.
    pub fn with_switch(mut self, switch: bool) -> Self {
        self.switch = switch;
        self
    }
}

impl Default for Split {
    fn default() -> Self {
        Self {
            name: String::new(),
            source: None,
            switch: false,
        }
    }
}

impl Command for Split {
    fn run(&self) -> CliResult<()> {
        // Validate the new stack name
        validate_stack_name(&self.name)
            .map_err(|msg| CliError::InvalidArgument { message: msg })?;

        // Find the repository
        let repo_root = find_repository_root()?;
        let mut repo = Repository::open(&repo_root).map_err(|e| match e {
            atomic_repository::RepositoryError::NotFound { path } => CliError::RepositoryNotFound {
                searched_path: path.into(),
            },
            other => CliError::Repository(other),
        })?;

        // Determine source stack (default to current)
        let source = self
            .source
            .clone()
            .unwrap_or_else(|| repo.current_stack().to_string());

        // Verify source stack exists
        if !repo.stack_exists(&source).map_err(CliError::Repository)? {
            return Err(CliError::StackNotFound {
                name: source.clone(),
            });
        }

        // Check if the new stack already exists
        if repo
            .stack_exists(&self.name)
            .map_err(CliError::Repository)?
        {
            return Err(CliError::StackAlreadyExists {
                name: self.name.clone(),
            });
        }

        // Get source stack info for reporting
        let source_info = repo.get_stack_info(&source).map_err(CliError::Repository)?;
        let change_count = source_info.change_count;

        // Create the new stack by copying change log from source.
        // This does NOT re-apply changes to the graph - it just copies metadata.
        // This avoids conflicts that would occur if we tried to re-apply changes
        // that have already modified the shared graph.
        repo.create_stack_from(&self.name, &source)
            .map_err(CliError::Repository)?;

        if change_count > 0 {
            print_success(&format!(
                "Created stack: {} (split from {} with {} changes)",
                style_stack(&self.name),
                style_stack(&source),
                change_count
            ));
        } else {
            print_success(&format!(
                "Created stack: {} (split from {} - empty)",
                style_stack(&self.name),
                style_stack(&source)
            ));
        }

        // Optionally switch to the new stack
        if self.switch {
            repo.set_current_stack(&self.name)
                .map_err(CliError::Repository)?;
            print_success(&format!("Switched to stack: {}", style_stack(&self.name)));
        } else {
            print_hint(&format!(
                "Use 'atomic stack switch {}' to switch to the new stack",
                self.name
            ));
        }

        Ok(())
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Builder Tests
    // =========================================================================

    #[test]
    fn test_split_new() {
        let cmd = Split::new("feature");
        assert_eq!(cmd.name, "feature");
        assert!(cmd.source.is_none());
        assert!(!cmd.switch);
    }

    #[test]
    fn test_split_default() {
        let cmd = Split::default();
        assert!(cmd.name.is_empty());
        assert!(cmd.source.is_none());
        assert!(!cmd.switch);
    }

    #[test]
    fn test_split_with_source() {
        let cmd = Split::new("feature").with_source("main");
        assert_eq!(cmd.name, "feature");
        assert_eq!(cmd.source, Some("main".to_string()));
    }

    #[test]
    fn test_split_with_switch() {
        let cmd = Split::new("feature").with_switch(true);
        assert!(cmd.switch);
    }

    #[test]
    fn test_split_builder_chain() {
        let cmd = Split::new("hotfix")
            .with_source("release-1.0")
            .with_switch(true);

        assert_eq!(cmd.name, "hotfix");
        assert_eq!(cmd.source, Some("release-1.0".to_string()));
        assert!(cmd.switch);
    }

    // =========================================================================
    // Validation Tests
    // =========================================================================

    #[test]
    fn test_validate_stack_name_valid() {
        assert!(validate_stack_name("feature").is_ok());
        assert!(validate_stack_name("feature-auth").is_ok());
        assert!(validate_stack_name("release-1.0.0").is_ok());
        assert!(validate_stack_name("my_stack").is_ok());
    }

    #[test]
    fn test_validate_stack_name_empty() {
        assert!(validate_stack_name("").is_err());
    }

    #[test]
    fn test_validate_stack_name_too_long() {
        let long_name = "a".repeat(256);
        assert!(validate_stack_name(&long_name).is_err());
    }

    #[test]
    fn test_validate_stack_name_invalid_chars() {
        assert!(validate_stack_name("feature/auth").is_err());
        assert!(validate_stack_name("feature\\auth").is_err());
        assert!(validate_stack_name("feature:auth").is_err());
        assert!(validate_stack_name("feature auth").is_err());
    }

    #[test]
    fn test_validate_stack_name_dots() {
        assert!(validate_stack_name(".").is_err());
        assert!(validate_stack_name("..").is_err());
        assert!(validate_stack_name(".hidden").is_err());
        assert!(validate_stack_name("name.").is_err());
    }

    // =========================================================================
    // Clone Tests
    // =========================================================================

    #[test]
    fn test_split_clone() {
        let cmd = Split::new("feature").with_source("main").with_switch(true);
        let cloned = cmd.clone();

        assert_eq!(cloned.name, cmd.name);
        assert_eq!(cloned.source, cmd.source);
        assert_eq!(cloned.switch, cmd.switch);
    }
}
