//! The `split` command for creating a new view from an existing one.
//!
//! This module implements the `atomic split` command, which creates a new
//! view by forking from an existing view. This is a convenience wrapper
//! around `atomic view create --from`.
//!
//! # Usage
//!
//! ```text
//! atomic split [OPTIONS] <NAME>
//!
//! Arguments:
//!   <NAME>  Name of the new view to create
//!
//! Options:
//!       --view <SOURCE>   Source view to split from (default: current view)
//!   -s, --switch          Switch to the new view after creating it
//!   -h, --help            Print help information
//! ```
//!
//! # Examples
//!
//! Split from current view:
//! ```text
//! $ atomic split experimental
//! Created view: experimental (forked from dev with 5 changes)
//! ```
//!
//! Split from a specific view:
//! ```text
//! $ atomic split hotfix --view release-1.0
//! Created view: hotfix (forked from release-1.0 with 42 changes)
//! ```
//!
//! Split and switch:
//! ```text
//! $ atomic split feature-auth --switch
//! Created view: feature-auth (forked from dev with 10 changes)
//! Switched to view: feature-auth
//! ```

use clap::Parser;

use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_success, view as style_view};

// Constants

/// Maximum length for a view name.
const MAX_VIEW_NAME_LENGTH: usize = 255;

/// Characters not allowed in view names.
const INVALID_CHARS: &[char] = &['/', '\\', '\0', ':', '*', '?', '"', '<', '>', '|', ' '];

// View Name Validation

/// Validate a view name.
fn validate_view_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("View name cannot be empty".to_string());
    }

    if name.len() > MAX_VIEW_NAME_LENGTH {
        return Err(format!(
            "View name cannot exceed {} characters",
            MAX_VIEW_NAME_LENGTH
        ));
    }

    for c in INVALID_CHARS {
        if name.contains(*c) {
            let char_desc = match c {
                ' ' => "spaces".to_string(),
                '\0' => "null characters".to_string(),
                _ => format!("'{}'", c),
            };
            return Err(format!("View name cannot contain {}", char_desc));
        }
    }

    if name == "." || name == ".." {
        return Err("View name cannot be '.' or '..'".to_string());
    }

    if name.starts_with('.') {
        return Err("View name cannot start with a dot".to_string());
    }

    if name.ends_with('.') {
        return Err("View name cannot end with a dot".to_string());
    }

    Ok(())
}

// Split Command

/// Split a view (create a new view from an existing one).
///
/// Creates a new view by forking from an existing view. All changes from
/// the source view are copied to the new view, preserving history.
///
/// This is equivalent to `atomic view create <NAME> --from <SOURCE>`.
#[derive(Parser, Debug, Clone)]
#[command(name = "split")]
#[derive(Default)]
pub struct Split {
    /// Name of the new view to create.
    ///
    /// View names should be descriptive and follow a naming convention
    /// like `feature-*`, `bugfix-*`, `release-*`, etc.
    #[arg(value_name = "NAME")]
    pub name: String,

    /// Source view to split from.
    ///
    /// If not specified, splits from the current view.
    #[arg(long = "view", value_name = "SOURCE")]
    pub source: Option<String>,

    /// Switch to the new view after creating it.
    ///
    /// By default, the current view remains unchanged after splitting.
    /// Use this flag to automatically switch to the new view.
    #[arg(long, short = 's')]
    pub switch: bool,
}

impl Split {
    /// Create a new Split command with the given name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            source: None,
            switch: false,
        }
    }

    /// Builder: set the source view to split from.
    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    /// Builder: set the switch flag.
    pub fn with_switch(mut self, switch: bool) -> Self {
        self.switch = switch;
        self
    }
}

impl Command for Split {
    fn run(&self) -> CliResult<()> {
        // Validate the new view name
        validate_view_name(&self.name).map_err(|msg| CliError::InvalidArgument { message: msg })?;

        // Find the repository
        let repo_root = find_repository_root()?;
        let mut repo = Repository::open(&repo_root).map_err(|e| match e {
            atomic_repository::RepositoryError::NotFound { path } => CliError::RepositoryNotFound {
                searched_path: path.into(),
            },
            other => CliError::Repository(other),
        })?;

        // Determine source view (default to current)
        let source = self
            .source
            .clone()
            .unwrap_or_else(|| repo.current_view().to_string());

        // Verify source view exists
        if !repo.view_exists(&source).map_err(CliError::Repository)? {
            return Err(CliError::ViewNotFound {
                name: source.clone(),
            });
        }

        // Check if the new view already exists
        if repo.view_exists(&self.name).map_err(CliError::Repository)? {
            return Err(CliError::ViewAlreadyExists {
                name: self.name.clone(),
            });
        }

        // Get source view info for reporting
        let source_info = repo.get_view_info(&source).map_err(CliError::Repository)?;
        let change_count = source_info.change_count;

        // Create the new view by copying change log from source.
        // This does NOT re-insert changes into the graph - it just copies metadata.
        // This avoids conflicts that would occur if we tried to re-insert changes
        // that have already modified the shared graph.
        repo.create_view_from(&self.name, &source)
            .map_err(CliError::Repository)?;

        if change_count > 0 {
            print_success(&format!(
                "Created view: {} (split from {} with {} changes)",
                style_view(&self.name),
                style_view(&source),
                change_count
            ));
        } else {
            print_success(&format!(
                "Created view: {} (split from {} - empty)",
                style_view(&self.name),
                style_view(&source)
            ));
        }

        // Optionally switch to the new view
        if self.switch {
            repo.set_current_view(&self.name)
                .map_err(CliError::Repository)?;
            print_success(&format!("Switched to view: {}", style_view(&self.name)));
        } else {
            print_hint(&format!(
                "Use 'atomic view switch {}' to switch to the new view",
                self.name
            ));
        }

        Ok(())
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    // Builder Tests

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

    // Validation Tests

    #[test]
    fn test_validate_view_name_valid() {
        assert!(validate_view_name("feature").is_ok());
        assert!(validate_view_name("feature-auth").is_ok());
        assert!(validate_view_name("release-1.0.0").is_ok());
        assert!(validate_view_name("my_stack").is_ok());
    }

    #[test]
    fn test_validate_view_name_empty() {
        assert!(validate_view_name("").is_err());
    }

    #[test]
    fn test_validate_view_name_too_long() {
        let long_name = "a".repeat(256);
        assert!(validate_view_name(&long_name).is_err());
    }

    #[test]
    fn test_validate_view_name_invalid_chars() {
        assert!(validate_view_name("feature/auth").is_err());
        assert!(validate_view_name("feature\\auth").is_err());
        assert!(validate_view_name("feature:auth").is_err());
        assert!(validate_view_name("feature auth").is_err());
    }

    #[test]
    fn test_validate_view_name_dots() {
        assert!(validate_view_name(".").is_err());
        assert!(validate_view_name("..").is_err());
        assert!(validate_view_name(".hidden").is_err());
        assert!(validate_view_name("name.").is_err());
    }

    // Clone Tests

    #[test]
    fn test_split_clone() {
        let cmd = Split::new("feature").with_source("main").with_switch(true);
        let cloned = cmd.clone();

        assert_eq!(cloned.name, cmd.name);
        assert_eq!(cloned.source, cmd.source);
        assert_eq!(cloned.switch, cmd.switch);
    }
}
