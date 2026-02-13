#![allow(dead_code)]
//! Remote repository management commands.
//!
//! This module provides commands for managing remote repository configurations,
//! allowing users to add, remove, list, and modify named remotes.
//!
//! # Available Subcommands
//!
//! - `remote` (no args) - List all configured remotes
//! - `remote add <name> <url>` - Add a new remote
//! - `remote remove <name>` - Remove a remote
//! - `remote set-url <name> <url>` - Change a remote's URL
//! - `remote rename <old> <new>` - Rename a remote
//! - `remote default <name>` - Set the default remote
//!
//! # Examples
//!
//! ```bash
//! # List all remotes
//! atomic remote
//!
//! # Add a new remote
//! atomic remote add origin https://api.example.com/tenant/portfolio/project/code
//!
//! # Change remote URL
//! atomic remote set-url origin https://new-url.example.com/repo
//!
//! # Remove a remote
//! atomic remote remove upstream
//!
//! # Set default remote for push/pull
//! atomic remote default origin
//! ```

use clap::{Parser, Subcommand};

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_success};

use atomic_repository::Repository;

// =============================================================================
// Remote Command
// =============================================================================

/// Manage remote repositories.
///
/// Remotes are named references to remote repository URLs. They allow you
/// to use short names like "origin" instead of full URLs when pushing,
/// pulling, or cloning.
#[derive(Debug, Clone, Parser)]
#[command(name = "remote")]
pub struct Remote {
    /// Subcommand to execute.
    #[command(subcommand)]
    pub command: Option<RemoteSubcommand>,

    /// Show verbose output (full URLs).
    #[arg(short, long)]
    pub verbose: bool,
}

/// Remote subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum RemoteSubcommand {
    /// Add a new remote.
    #[command(name = "add")]
    Add(RemoteAdd),

    /// Remove a remote.
    #[command(name = "remove", visible_alias = "rm")]
    Remove(RemoteRemove),

    /// Change the URL of a remote.
    #[command(name = "set-url")]
    SetUrl(RemoteSetUrl),

    /// Rename a remote.
    #[command(name = "rename")]
    Rename(RemoteRename),

    /// Set the default remote.
    #[command(name = "default")]
    Default(RemoteDefault),
}

// =============================================================================
// Subcommand Structs
// =============================================================================

/// Add a new remote.
#[derive(Debug, Clone, Parser)]
pub struct RemoteAdd {
    /// Name for the new remote.
    #[arg(value_name = "NAME")]
    pub name: String,

    /// URL of the remote repository.
    #[arg(value_name = "URL")]
    pub url: String,

    /// Set this remote as the default.
    #[arg(short, long)]
    pub default: bool,
}

/// Remove a remote.
#[derive(Debug, Clone, Parser)]
pub struct RemoteRemove {
    /// Name of the remote to remove.
    #[arg(value_name = "NAME")]
    pub name: String,
}

/// Change the URL of a remote.
#[derive(Debug, Clone, Parser)]
pub struct RemoteSetUrl {
    /// Name of the remote to update.
    #[arg(value_name = "NAME")]
    pub name: String,

    /// New URL for the remote.
    #[arg(value_name = "URL")]
    pub url: String,
}

/// Rename a remote.
#[derive(Debug, Clone, Parser)]
pub struct RemoteRename {
    /// Current name of the remote.
    #[arg(value_name = "OLD")]
    pub old_name: String,

    /// New name for the remote.
    #[arg(value_name = "NEW")]
    pub new_name: String,
}

/// Set the default remote.
#[derive(Debug, Clone, Parser)]
pub struct RemoteDefault {
    /// Name of the remote to set as default.
    #[arg(value_name = "NAME")]
    pub name: String,
}

// =============================================================================
// Implementation
// =============================================================================

impl Remote {
    /// Create a new Remote command (for testing).
    pub fn new() -> Self {
        Self {
            command: None,
            verbose: false,
        }
    }

    /// Set verbose mode.
    pub fn with_verbose(mut self, verbose: bool) -> Self {
        self.verbose = verbose;
        self
    }

    /// Set the subcommand.
    pub fn with_command(mut self, command: RemoteSubcommand) -> Self {
        self.command = Some(command);
        self
    }

    /// List all configured remotes.
    fn list_remotes(&self) -> CliResult<()> {
        let repo_root = find_repository_root()?;
        let repo = Repository::open(&repo_root).map_err(CliError::Repository)?;

        let remotes = repo.list_remotes().map_err(CliError::Repository)?;

        if remotes.is_empty() {
            print_hint("No remotes configured. Use 'atomic remote add <name> <url>' to add one.");
            return Ok(());
        }

        for (name, entry) in &remotes {
            if self.verbose {
                let default_marker = if entry.default { " (default)" } else { "" };
                println!("{}\t{}{}", name, entry.url, default_marker);
            } else {
                println!("{}", name);
            }
        }

        Ok(())
    }

    /// Add a new remote.
    fn add_remote(&self, add: &RemoteAdd) -> CliResult<()> {
        let repo_root = find_repository_root()?;
        let repo = Repository::open(&repo_root).map_err(CliError::Repository)?;

        // Check if URL is valid
        if !add.url.contains("://") {
            return Err(CliError::InvalidArgument {
                message: format!(
                    "Invalid URL '{}': URL must include a scheme (e.g., https://)",
                    add.url
                ),
            });
        }

        if add.default {
            repo.add_remote_default(&add.name, &add.url)
                .map_err(CliError::Repository)?;
        } else {
            repo.add_remote(&add.name, &add.url)
                .map_err(CliError::Repository)?;
        }

        print_success(&format!("Remote '{}' added", add.name));
        if self.verbose {
            println!("  URL: {}", add.url);
        }

        Ok(())
    }

    /// Remove a remote.
    fn remove_remote(&self, remove: &RemoteRemove) -> CliResult<()> {
        let repo_root = find_repository_root()?;
        let repo = Repository::open(&repo_root).map_err(CliError::Repository)?;

        repo.remove_remote(&remove.name)
            .map_err(CliError::Repository)?;

        print_success(&format!("Remote '{}' removed", remove.name));

        Ok(())
    }

    /// Update a remote's URL.
    fn set_url(&self, set_url: &RemoteSetUrl) -> CliResult<()> {
        let repo_root = find_repository_root()?;
        let repo = Repository::open(&repo_root).map_err(CliError::Repository)?;

        // Check if URL is valid
        if !set_url.url.contains("://") {
            return Err(CliError::InvalidArgument {
                message: format!(
                    "Invalid URL '{}': URL must include a scheme (e.g., https://)",
                    set_url.url
                ),
            });
        }

        repo.set_remote_url(&set_url.name, &set_url.url)
            .map_err(CliError::Repository)?;

        print_success(&format!("Remote '{}' URL updated", set_url.name));
        if self.verbose {
            println!("  New URL: {}", set_url.url);
        }

        Ok(())
    }

    /// Rename a remote.
    fn rename_remote(&self, rename: &RemoteRename) -> CliResult<()> {
        let repo_root = find_repository_root()?;
        let repo = Repository::open(&repo_root).map_err(CliError::Repository)?;

        repo.rename_remote(&rename.old_name, &rename.new_name)
            .map_err(CliError::Repository)?;

        print_success(&format!(
            "Remote '{}' renamed to '{}'",
            rename.old_name, rename.new_name
        ));

        Ok(())
    }

    /// Set the default remote.
    fn set_default(&self, default: &RemoteDefault) -> CliResult<()> {
        let repo_root = find_repository_root()?;
        let repo = Repository::open(&repo_root).map_err(CliError::Repository)?;

        repo.set_default_remote(&default.name)
            .map_err(CliError::Repository)?;

        print_success(&format!("Remote '{}' set as default", default.name));

        Ok(())
    }
}

impl Default for Remote {
    fn default() -> Self {
        Self::new()
    }
}

impl Command for Remote {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            None => self.list_remotes(),
            Some(RemoteSubcommand::Add(add)) => self.add_remote(add),
            Some(RemoteSubcommand::Remove(remove)) => self.remove_remote(remove),
            Some(RemoteSubcommand::SetUrl(set_url)) => self.set_url(set_url),
            Some(RemoteSubcommand::Rename(rename)) => self.rename_remote(rename),
            Some(RemoteSubcommand::Default(default)) => self.set_default(default),
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_remote_new() {
        let cmd = Remote::new();
        assert!(cmd.command.is_none());
        assert!(!cmd.verbose);
    }

    #[test]
    fn test_remote_with_verbose() {
        let cmd = Remote::new().with_verbose(true);
        assert!(cmd.verbose);
    }

    #[test]
    fn test_remote_default() {
        let cmd = Remote::default();
        assert!(cmd.command.is_none());
        assert!(!cmd.verbose);
    }

    #[test]
    fn test_remote_add_struct() {
        let add = RemoteAdd {
            name: "origin".to_string(),
            url: "https://example.com/repo".to_string(),
            default: false,
        };
        assert_eq!(add.name, "origin");
        assert_eq!(add.url, "https://example.com/repo");
        assert!(!add.default);
    }

    #[test]
    fn test_remote_add_with_default() {
        let add = RemoteAdd {
            name: "origin".to_string(),
            url: "https://example.com/repo".to_string(),
            default: true,
        };
        assert!(add.default);
    }

    #[test]
    fn test_remote_remove_struct() {
        let remove = RemoteRemove {
            name: "origin".to_string(),
        };
        assert_eq!(remove.name, "origin");
    }

    #[test]
    fn test_remote_set_url_struct() {
        let set_url = RemoteSetUrl {
            name: "origin".to_string(),
            url: "https://new.example.com/repo".to_string(),
        };
        assert_eq!(set_url.name, "origin");
        assert_eq!(set_url.url, "https://new.example.com/repo");
    }

    #[test]
    fn test_remote_rename_struct() {
        let rename = RemoteRename {
            old_name: "origin".to_string(),
            new_name: "upstream".to_string(),
        };
        assert_eq!(rename.old_name, "origin");
        assert_eq!(rename.new_name, "upstream");
    }

    #[test]
    fn test_remote_default_struct() {
        let default = RemoteDefault {
            name: "origin".to_string(),
        };
        assert_eq!(default.name, "origin");
    }

    #[test]
    fn test_remote_clone() {
        let cmd = Remote::new().with_verbose(true);
        let cloned = cmd.clone();
        assert_eq!(cmd.verbose, cloned.verbose);
    }

    #[test]
    fn test_remote_debug() {
        let cmd = Remote::new();
        let debug = format!("{:?}", cmd);
        assert!(debug.contains("Remote"));
    }
}
