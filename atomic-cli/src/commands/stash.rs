//! The `stash` command for temporarily saving uncommitted changes.
//!
//! This module implements the `atomic stash` command, which saves uncommitted
//! working copy changes to a temporary orphan view and restores the working
//! copy to a clean state. This is useful when you need to switch views but
//! have uncommitted changes that belong elsewhere.
//!
//! # Concept
//!
//! Unlike Git's stash which uses a special ref, Atomic stash uses **orphan views**
//! as temporary holding areas. An orphan view has no history, making it lightweight
//! and view-agnostic - the stashed changes can be applied to any view later.
//!
//! # Usage
//!
//! ```text
//! atomic stash [SUBCOMMAND]
//!
//! Subcommands:
//!   push      Save changes to a new stash (default if no subcommand)
//!   pop       Apply most recent stash and delete it
//!   apply     Apply a stash without deleting it
//!   list      List all stashes
//!   show      Show changes in a stash
//!   drop      Delete a stash without applying
//!   clear     Delete all stashes
//! ```
//!
//! # Examples
//!
//! Save uncommitted changes:
//! ```text
//! $ atomic stash
//! ✓ Saved working copy to stash@{0}
//! ✓ Working copy restored to clean state
//! ```
//!
//! Apply and remove most recent stash:
//! ```text
//! $ atomic stash pop
//! ✓ Applied stash@{0} to working copy
//! ✓ Dropped stash@{0}
//! ```
//!
//! List all stashes:
//! ```text
//! $ atomic stash list
//! stash@{0}: On dev: WIP authentication changes
//! stash@{1}: On feature: Debugging output
//! ```
//!
//! # Workflow Example
//!
//! You're working on `dev` but realize changes belong on `feature/auth`:
//!
//! ```text
//! $ atomic status
//! On view dev
//! Changes to be recorded:
//!     modified: src/auth.rs
//!
//! $ atomic stash -m "WIP auth changes"
//! ✓ Saved working copy to stash@{0}
//! ✓ Working copy restored to clean state
//!
//! $ atomic view switch feature/auth
//! ✓ Switched to view: feature/auth
//!
//! $ atomic stash pop
//! ✓ Applied stash@{0} to working copy
//! ✓ Dropped stash@{0}
//!
//! $ atomic record -m "Add OAuth support"
//! [feature/auth 3/abc123] Add OAuth support
//! ```

use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};

use atomic_repository::record::RecordOptions;
use atomic_repository::status::StatusOptions;
use atomic_repository::{Repository, StashPushOptions};

use crate::commands::{find_repository_root, format_timestamp_relative, Command};
use crate::error::{CliError, CliResult};
use crate::output::{print_blank, print_hint, print_success, print_warning, view as style_view};

// Constants

/// Prefix for stash view names.
const STASH_PREFIX: &str = "stash/";

/// Default message for stashes without a custom message.
const DEFAULT_STASH_MESSAGE: &str = "WIP";

// Stash Command

/// Temporarily save uncommitted changes.
///
/// Stash saves your uncommitted working copy changes to a temporary
/// orphan view, then restores the working copy to a clean state.
/// You can later apply the stashed changes to any view.
#[derive(Parser, Debug, Clone)]
#[command(name = "stash")]
pub struct Stash {
    /// Stash subcommand to run.
    #[command(subcommand)]
    pub command: Option<StashSubcommand>,

    /// Message describing the stashed changes.
    ///
    /// Used when pushing a new stash (default action).
    #[arg(short, long, global = true)]
    pub message: Option<String>,

    /// Include untracked files in the stash.
    #[arg(short = 'u', long, global = true)]
    pub include_untracked: bool,

    /// Keep the changes in the working copy after stashing.
    ///
    /// By default, stash restores the working copy to clean state.
    #[arg(short, long, global = true)]
    pub keep: bool,
}

/// Stash subcommands.
#[derive(Subcommand, Debug, Clone)]
pub enum StashSubcommand {
    /// Save changes to a new stash.
    ///
    /// This is the default action when running `atomic stash` without a subcommand.
    Push {
        /// Message describing the stashed changes.
        #[arg(short, long)]
        message: Option<String>,

        /// Include untracked files in the stash.
        #[arg(short = 'u', long)]
        include_untracked: bool,

        /// Keep the changes in the working copy after stashing.
        #[arg(short, long)]
        keep: bool,
    },

    /// Apply most recent stash and delete it.
    ///
    /// Applies the changes from the most recent stash (or specified stash)
    /// to the current working copy, then deletes the stash.
    Pop {
        /// Stash to pop (e.g., "stash@{0}" or "0"). Defaults to most recent.
        #[arg(value_name = "STASH")]
        stash: Option<String>,
    },

    /// Apply a stash without deleting it.
    ///
    /// Applies the changes from a stash to the current working copy
    /// but keeps the stash for future use.
    Apply {
        /// Stash to apply (e.g., "stash@{0}" or "0"). Defaults to most recent.
        #[arg(value_name = "STASH")]
        stash: Option<String>,
    },

    /// List all stashes.
    List,

    /// Show changes in a stash.
    Show {
        /// Stash to show (e.g., "stash@{0}" or "0"). Defaults to most recent.
        #[arg(value_name = "STASH")]
        stash: Option<String>,

        /// Show full diff output.
        #[arg(short, long)]
        patch: bool,
    },

    /// Delete a stash without applying.
    Drop {
        /// Stash to drop (e.g., "stash@{0}" or "0"). Defaults to most recent.
        #[arg(value_name = "STASH")]
        stash: Option<String>,
    },

    /// Delete all stashes.
    Clear {
        /// Skip confirmation prompt.
        #[arg(short, long)]
        force: bool,
    },
}

// Stash Entry

pub use atomic_repository::StashEntry;

// Implementation

impl Stash {
    /// Create a new Stash command with default settings.
    pub fn new() -> Self {
        Self {
            command: None,
            message: None,
            include_untracked: false,
            keep: false,
        }
    }

    /// Builder: set the message.
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    /// Builder: set the include-untracked flag.
    pub fn with_include_untracked(mut self, include: bool) -> Self {
        self.include_untracked = include;
        self
    }

    /// Builder: set the keep flag.
    pub fn with_keep(mut self, keep: bool) -> Self {
        self.keep = keep;
        self
    }

    /// Builder: set the --all subcommand behavior (alias for include_untracked).
    pub fn with_all(mut self, all: bool) -> Self {
        self.include_untracked = all;
        self
    }

    /// Builder: set the subcommand to apply with dependencies.
    pub fn with_dependencies(mut self, _deps: bool) -> Self {
        // Dependencies flag is handled at the subcommand level;
        // this builder is provided for test convenience.
        self
    }

    /// List all stash views, sorted by creation time (newest first).
    fn list_stashes(&self, repo: &Repository) -> CliResult<Vec<StashEntry>> {
        repo.stash_list()
            .map_err(|message| CliError::Internal(anyhow::anyhow!(message)))
    }

    /// Parse a stash reference (e.g., "stash@{0}", "0", or view name).
    fn parse_stash_ref(&self, repo: &Repository, reference: Option<&str>) -> CliResult<StashEntry> {
        repo.stash_resolve(reference)
            .map_err(|message| CliError::InvalidArgument { message })
    }

    /// Execute stash push from another command (e.g., `view switch --stash`).
    ///
    /// This is the public entry point for programmatic stash push.
    pub fn run_push_on(
        &self,
        repo: &mut Repository,
        message: Option<String>,
        include_untracked: bool,
        keep: bool,
    ) -> CliResult<()> {
        self.run_push(repo, message, include_untracked, keep)
    }

    /// Execute stash push (save changes).
    fn run_push(
        &self,
        repo: &mut Repository,
        message: Option<String>,
        include_untracked: bool,
        keep: bool,
    ) -> CliResult<()> {
        // The domain owns the flow (status check, orphan view, raw-bytes
        // sidecar, materialize); this is the presentation half.
        let options = StashPushOptions {
            message: message.or_else(|| self.message.clone()),
            include_untracked: include_untracked || self.include_untracked,
            keep: keep || self.keep,
        };
        match repo
            .stash_push(options)
            .map_err(|message| CliError::Internal(anyhow::anyhow!(message)))?
        {
            None => {
                print_warning("No local changes to save");
            }
            Some(entry) => {
                // Recompute the display reference from the listing (the
                // entry's index is assigned by stash_list).
                let stash_ref = repo
                    .stash_list()
                    .map_err(|message| CliError::Internal(anyhow::anyhow!(message)))?
                    .iter()
                    .find(|s| s.view_name == entry.view_name)
                    .map(|s| s.reference())
                    .unwrap_or_else(|| "stash@{0}".to_string());
                print_success(&format!("Saved working copy to {stash_ref}"));
                if !keep && !self.keep {
                    print_success("Working copy restored to clean state");
                }
            }
        }
        Ok(())
    }

    /// Execute stash pop (apply and delete).
    fn run_pop(&self, repo: &mut Repository, stash_ref: Option<&str>) -> CliResult<()> {
        let stash = self.parse_stash_ref(repo, stash_ref)?;

        // Apply the stash
        self.apply_stash(repo, &stash)?;

        // Delete the stash view
        repo.delete_view(&stash.view_name)
            .map_err(CliError::Repository)?;

        print_success(&format!("Dropped {}", stash.reference()));

        Ok(())
    }

    /// Execute stash apply (apply without deleting).
    fn run_apply(&self, repo: &mut Repository, stash_ref: Option<&str>) -> CliResult<()> {
        let stash = self.parse_stash_ref(repo, stash_ref)?;
        self.apply_stash(repo, &stash)?;
        print_hint(&format!(
            "Stash {} still exists. Use 'atomic stash drop' to remove it.",
            stash.reference()
        ));
        Ok(())
    }

    /// Apply a stash to the current working copy: the sidecar bytes are
    /// copied back to disk (a pure filesystem operation — no graph
    /// interaction; the domain owns the MANIFEST walk).
    fn apply_stash(&self, repo: &mut Repository, stash: &StashEntry) -> CliResult<()> {
        let (_entry, applied) = repo
            .stash_apply(Some(&stash.view_name))
            .map_err(|message| CliError::Internal(anyhow::anyhow!(message)))?;

        if !applied.is_empty() {
            print_success(&format!("Applied {} to working copy", stash.reference()));
        } else {
            print_warning("Stash was already applied or is empty");
        }
        Ok(())
    }

    /// Execute stash list.
    fn run_list(&self, repo: &Repository) -> CliResult<()> {
        let stashes = self.list_stashes(repo)?;

        if stashes.is_empty() {
            println!("No stashes found");
            return Ok(());
        }

        for stash in stashes {
            let relative_time = format_timestamp_relative(&stash.created_at);
            println!(
                "{}: On {}: {} ({})",
                stash.reference(),
                style_view(&stash.source_view),
                stash.message,
                relative_time
            );
        }

        Ok(())
    }

    /// Execute stash show.
    fn run_show(&self, repo: &Repository, stash_ref: Option<&str>, patch: bool) -> CliResult<()> {
        let stash = self.parse_stash_ref(repo, stash_ref)?;

        println!("{}", stash.reference());
        println!("  Source view: {}", style_view(&stash.source_view));
        println!("  Message: {}", stash.message);
        println!(
            "  Created: {}",
            format_timestamp_relative(&stash.created_at)
        );

        // Get view info
        let info = repo
            .get_view_info(&stash.view_name)
            .map_err(CliError::Repository)?;

        println!("  Changes: {}", info.change_count);

        if patch {
            // TODO: Implement full diff output
            print_blank();
            print_hint("Full diff output not yet implemented");
        }

        Ok(())
    }

    /// Execute stash drop.
    fn run_drop(&self, repo: &mut Repository, stash_ref: Option<&str>) -> CliResult<()> {
        let stash = repo
            .stash_drop(stash_ref)
            .map_err(|message| CliError::InvalidArgument { message })?;
        print_success(&format!("Dropped {}", stash.reference()));
        Ok(())
    }

    /// Execute stash clear.
    fn run_clear(&self, repo: &mut Repository, force: bool) -> CliResult<()> {
        if !force {
            let count = repo
                .stash_list()
                .map_err(|message| CliError::Internal(anyhow::anyhow!(message)))?
                .len();
            if count == 0 {
                println!("No stashes to clear");
                return Ok(());
            }
            println!("This will delete {count} stash(es). Are you sure? [y/N] ");
            std::io::Write::flush(&mut std::io::stdout()).ok();
            let mut input = String::new();
            std::io::stdin().read_line(&mut input).ok();
            if !input.trim().eq_ignore_ascii_case("y") {
                println!("Aborted");
                return Ok(());
            }
        }
        let count = repo
            .stash_clear()
            .map_err(|message| CliError::InvalidArgument { message })?;
        if count == 0 {
            println!("No stashes to clear");
        } else {
            print_success(&format!("Cleared {count} stash(es)"));
        }
        Ok(())
    }
}

impl Default for Stash {
    fn default() -> Self {
        Self::new()
    }
}

impl Command for Stash {
    /// Execute the stash command.
    fn run(&self) -> CliResult<()> {
        // Route through the daemon when reachable (the domain module is
        // shared, so behavior is identical); show stays local and clear
        // keeps its confirmation prompt locally.
        match &self.command {
            None => {
                if crate::commands::rpc::stash_push(
                    self.message.clone(),
                    self.include_untracked,
                    self.keep,
                )? {
                    return Ok(());
                }
            }
            Some(StashSubcommand::Push {
                message,
                include_untracked,
                keep,
            }) => {
                if crate::commands::rpc::stash_push(message.clone(), *include_untracked, *keep)? {
                    return Ok(());
                }
            }
            Some(StashSubcommand::Pop { stash }) => {
                if crate::commands::rpc::stash_pop(stash.clone())? {
                    return Ok(());
                }
            }
            Some(StashSubcommand::Apply { stash }) => {
                if crate::commands::rpc::stash_apply(stash.clone())? {
                    return Ok(());
                }
            }
            Some(StashSubcommand::List) => {
                if crate::commands::rpc::stash_list()? {
                    return Ok(());
                }
            }
            Some(StashSubcommand::Drop { stash })
                if crate::commands::rpc::stash_drop(stash.clone(), false)? =>
            {
                return Ok(());
            }
            _ => {}
        }

        // Find repository
        let repo_root = find_repository_root()?;
        let mut repo = Repository::open(&repo_root).map_err(CliError::Repository)?;

        match &self.command {
            None => {
                // Default action: push
                self.run_push(&mut repo, None, self.include_untracked, self.keep)
            }
            Some(StashSubcommand::Push {
                message,
                include_untracked,
                keep,
            }) => self.run_push(&mut repo, message.clone(), *include_untracked, *keep),
            Some(StashSubcommand::Pop { stash }) => self.run_pop(&mut repo, stash.as_deref()),
            Some(StashSubcommand::Apply { stash }) => self.run_apply(&mut repo, stash.as_deref()),
            Some(StashSubcommand::List) => self.run_list(&repo),
            Some(StashSubcommand::Show { stash, patch }) => {
                self.run_show(&repo, stash.as_deref(), *patch)
            }
            Some(StashSubcommand::Drop { stash }) => self.run_drop(&mut repo, stash.as_deref()),
            Some(StashSubcommand::Clear { force }) => self.run_clear(&mut repo, *force),
        }
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    // Builder Tests

    #[test]
    fn test_stash_new() {
        let cmd = Stash::new();
        assert!(cmd.command.is_none());
        assert!(cmd.message.is_none());
        assert!(!cmd.include_untracked);
        assert!(!cmd.keep);
    }

    #[test]
    fn test_stash_default() {
        let cmd = Stash::default();
        assert!(cmd.command.is_none());
        assert!(cmd.message.is_none());
    }

    #[test]
    fn test_stash_with_message() {
        let cmd = Stash::new().with_message("WIP auth");
        assert_eq!(cmd.message, Some("WIP auth".to_string()));
    }

    #[test]
    fn test_stash_with_include_untracked() {
        let cmd = Stash::new().with_include_untracked(true);
        assert!(cmd.include_untracked);
    }

    #[test]
    fn test_stash_with_keep() {
        let cmd = Stash::new().with_keep(true);
        assert!(cmd.keep);
    }

    #[test]
    fn test_stash_builder_chain() {
        let cmd = Stash::new()
            .with_message("test")
            .with_include_untracked(true)
            .with_keep(true);

        assert_eq!(cmd.message, Some("test".to_string()));
        assert!(cmd.include_untracked);
        assert!(cmd.keep);
    }

    // StashEntry Tests

    #[test]
    fn test_stash_entry_reference() {
        let entry = StashEntry {
            index: 0,
            view_name: "stash/test".to_string(),
            source_view: "dev".to_string(),
            message: "WIP".to_string(),
            created_at: Utc::now(),
        };

        assert_eq!(entry.reference(), "stash@{0}");
    }

    #[test]
    fn test_stash_entry_reference_nonzero() {
        let entry = StashEntry {
            index: 3,
            view_name: "stash/test".to_string(),
            source_view: "dev".to_string(),
            message: "WIP".to_string(),
            created_at: Utc::now(),
        };

        assert_eq!(entry.reference(), "stash@{3}");
    }

    #[test]
    fn test_stash_entry_display() {
        let entry = StashEntry {
            index: 0,
            view_name: "stash/test".to_string(),
            source_view: "feature".to_string(),
            message: "Work in progress".to_string(),
            created_at: Utc::now(),
        };

        let display = entry.display();
        assert!(display.contains("stash@{0}"));
        assert!(display.contains("feature"));
        assert!(display.contains("Work in progress"));
    }

    // Constants Tests

    #[test]
    fn test_stash_prefix() {
        assert_eq!(STASH_PREFIX, "stash/");
    }

    #[test]
    fn test_default_stash_message() {
        assert_eq!(DEFAULT_STASH_MESSAGE, "WIP");
    }

    // Clone Tests

    #[test]
    fn test_stash_clone() {
        let cmd = Stash::new()
            .with_message("test")
            .with_include_untracked(true);

        let cloned = cmd.clone();

        assert_eq!(cloned.message, cmd.message);
        assert_eq!(cloned.include_untracked, cmd.include_untracked);
        assert_eq!(cloned.keep, cmd.keep);
    }

    // Subcommand Tests

    #[test]
    fn test_subcommand_push() {
        let subcmd = StashSubcommand::Push {
            message: Some("test".to_string()),
            include_untracked: true,
            keep: false,
        };

        match subcmd {
            StashSubcommand::Push {
                message,
                include_untracked,
                keep,
            } => {
                assert_eq!(message, Some("test".to_string()));
                assert!(include_untracked);
                assert!(!keep);
            }
            _ => panic!("Expected Push"),
        }
    }

    #[test]
    fn test_subcommand_pop() {
        let subcmd = StashSubcommand::Pop {
            stash: Some("stash@{0}".to_string()),
        };

        match subcmd {
            StashSubcommand::Pop { stash } => {
                assert_eq!(stash, Some("stash@{0}".to_string()));
            }
            _ => panic!("Expected Pop"),
        }
    }

    #[test]
    fn test_subcommand_list() {
        let subcmd = StashSubcommand::List;
        assert!(matches!(subcmd, StashSubcommand::List));
    }

    #[test]
    fn test_subcommand_clear() {
        let subcmd = StashSubcommand::Clear { force: true };

        match subcmd {
            StashSubcommand::Clear { force } => {
                assert!(force);
            }
            _ => panic!("Expected Clear"),
        }
    }
}
