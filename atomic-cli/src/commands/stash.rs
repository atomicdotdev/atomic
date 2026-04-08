//! The `stash` command for temporarily saving uncommitted changes.
//!
//! This module implements the `atomic stash` command, which saves uncommitted
//! working copy changes to a temporary orphan stack and restores the working
//! copy to a clean state. This is useful when you need to switch stacks but
//! have uncommitted changes that belong elsewhere.
//!
//! # Concept
//!
//! Unlike Git's stash which uses a special ref, Atomic stash uses **orphan stacks**
//! as temporary holding areas. An orphan stack has no history, making it lightweight
//! and stack-agnostic - the stashed changes can be applied to any stack later.
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
//! On stack dev
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
use atomic_repository::Repository;

use crate::commands::{find_repository_root, format_timestamp_relative, Command};
use crate::error::{CliError, CliResult};
use crate::output::{print_blank, print_hint, print_success, print_warning, stack as style_stack};

// Constants

/// Prefix for stash stack names.
const STASH_PREFIX: &str = "stash/";

/// Default message for stashes without a custom message.
const DEFAULT_STASH_MESSAGE: &str = "WIP";

// Stash Command

/// Temporarily save uncommitted changes.
///
/// Stash saves your uncommitted working copy changes to a temporary
/// orphan stack, then restores the working copy to a clean state.
/// You can later apply the stashed changes to any stack.
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

/// Information about a stash entry.
#[derive(Debug, Clone)]
pub struct StashEntry {
    /// Index of the stash (0 = most recent).
    pub index: usize,
    /// Name of the stash stack.
    pub stack_name: String,
    /// Stack the stash was created from.
    pub source_stack: String,
    /// Message describing the stash.
    pub message: String,
    /// When the stash was created.
    pub created_at: DateTime<Utc>,
}

impl StashEntry {
    /// Format the stash reference (e.g., "stash@{0}").
    pub fn reference(&self) -> String {
        format!("stash@{{{}}}", self.index)
    }

    /// Format the stash entry for display.
    ///
    /// Returns a human-readable string like:
    /// `stash@{0}: On dev: Fix authentication bug`
    pub fn display(&self) -> String {
        format!(
            "{}: On {}: {}",
            self.reference(),
            self.source_stack,
            self.message
        )
    }
}

impl std::fmt::Display for StashEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.display())
    }
}

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

    /// List all stash stacks, sorted by creation time (newest first).
    fn list_stashes(&self, repo: &Repository) -> CliResult<Vec<StashEntry>> {
        let stacks = repo.list_views().map_err(CliError::Repository)?;

        let mut stashes: Vec<StashEntry> = stacks
            .into_iter()
            .filter(|name| name.starts_with(STASH_PREFIX))
            .filter_map(|name| {
                // Parse stash metadata from stack info
                let _info = repo.get_view_info(&name).ok()?;

                // Extract source stack and message from stack name or metadata
                // Format: stash/auto_{timestamp} or stash/{source}_{timestamp}_{message}
                let parts: Vec<&str> = name
                    .trim_start_matches(STASH_PREFIX)
                    .splitn(3, '_')
                    .collect();

                let (source_stack, message, timestamp) = if parts.len() >= 2 {
                    let ts: i64 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
                    let src = if parts[0] == "auto" {
                        "unknown".to_string()
                    } else {
                        parts[0].to_string()
                    };
                    let msg = parts
                        .get(2)
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| DEFAULT_STASH_MESSAGE.to_string());
                    (src, msg, ts)
                } else {
                    ("unknown".to_string(), DEFAULT_STASH_MESSAGE.to_string(), 0)
                };

                let created_at =
                    DateTime::from_timestamp_millis(timestamp).unwrap_or_else(Utc::now);

                Some(StashEntry {
                    index: 0, // Will be set after sorting
                    stack_name: name,
                    source_stack,
                    message,
                    created_at,
                })
            })
            .collect();

        // Sort by creation time, newest first
        stashes.sort_by(|a, b| b.created_at.cmp(&a.created_at));

        // Assign indices after sorting
        for (i, stash) in stashes.iter_mut().enumerate() {
            stash.index = i;
        }

        Ok(stashes)
    }

    /// Parse a stash reference (e.g., "stash@{0}", "0", or stack name).
    fn parse_stash_ref(&self, repo: &Repository, reference: Option<&str>) -> CliResult<StashEntry> {
        let stashes = self.list_stashes(repo)?;

        if stashes.is_empty() {
            return Err(CliError::InvalidArgument {
                message: "No stashes found".to_string(),
            });
        }

        let reference = reference.unwrap_or("0");

        // Try to parse as index
        let index = if reference.starts_with("stash@{") && reference.ends_with('}') {
            // Format: stash@{N}
            reference[7..reference.len() - 1].parse::<usize>().ok()
        } else {
            // Try as plain number
            reference.parse::<usize>().ok()
        };

        if let Some(idx) = index {
            return stashes.into_iter().find(|s| s.index == idx).ok_or_else(|| {
                CliError::InvalidArgument {
                    message: format!("stash@{{{}}} does not exist", idx),
                }
            });
        }

        // Try as stack name
        let full_name = if reference.starts_with(STASH_PREFIX) {
            reference.to_string()
        } else {
            format!("{}{}", STASH_PREFIX, reference)
        };

        stashes
            .into_iter()
            .find(|s| s.stack_name == full_name)
            .ok_or_else(|| CliError::InvalidArgument {
                message: format!("Stash '{}' not found", reference),
            })
    }

    /// Execute stash push (save changes).
    fn run_push(
        &self,
        repo: &mut Repository,
        message: Option<String>,
        include_untracked: bool,
        keep: bool,
    ) -> CliResult<()> {
        // Check for uncommitted changes
        let status = repo
            .status(StatusOptions::default())
            .map_err(CliError::Repository)?;

        if status.is_clean() {
            print_warning("No local changes to save");
            return Ok(());
        }

        // Get current view for metadata
        let source_stack = repo.current_view().to_string();

        // Generate stash message
        let stash_message = message
            .or_else(|| self.message.clone())
            .unwrap_or_else(|| DEFAULT_STASH_MESSAGE.to_string());

        // Create stash stack name with metadata
        let timestamp = Utc::now().timestamp_millis();
        let safe_source = source_stack.replace('/', "-");
        let safe_message = stash_message
            .chars()
            .take(30)
            .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_' || *c == ' ')
            .collect::<String>()
            .replace(' ', "-");

        let stash_name = format!(
            "{}{}_{}_{}",
            STASH_PREFIX, safe_source, timestamp, safe_message
        );

        // Create orphan stash view
        repo.create_view(&stash_name)
            .map_err(CliError::Repository)?;

        // ── Save dirty files as raw bytes to a sidecar directory ────────
        //
        // Stash is a **pure working-copy operation**.  We save the exact
        // bytes of every dirty file to `.atomic/stashes/<stash_name>/`.
        // This is completely immune to graph mutations — even if the user
        // records new changes between push and pop, pop will restore the
        // exact content that was stashed.
        //
        // We do NOT go through the graph/record/apply machinery at all.

        let stash_dir = repo.dot_dir().join("stashes").join(&stash_name);
        std::fs::create_dir_all(&stash_dir).map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create stash dir: {}", e))
        })?;

        // Collect every dirty file (modified + added)
        let mut stashed_paths: Vec<String> = Vec::new();

        for entry in status.modified() {
            let p = entry.path().to_string_lossy().to_string();
            let abs = repo.root().join(&p);
            if abs.is_file() {
                let rel_dir = stash_dir.join(
                    std::path::Path::new(&p)
                        .parent()
                        .unwrap_or(std::path::Path::new("")),
                );
                let _ = std::fs::create_dir_all(&rel_dir);
                let _ = std::fs::copy(&abs, stash_dir.join(&p));
                stashed_paths.push(p);
            }
        }

        for entry in status.added() {
            let p = entry.path().to_string_lossy().to_string();
            let abs = repo.root().join(&p);
            if abs.is_file() {
                let rel_dir = stash_dir.join(
                    std::path::Path::new(&p)
                        .parent()
                        .unwrap_or(std::path::Path::new("")),
                );
                let _ = std::fs::create_dir_all(&rel_dir);
                let _ = std::fs::copy(&abs, stash_dir.join(&p));
                stashed_paths.push(p);
            }
        }

        if include_untracked || self.include_untracked {
            for entry in status.untracked() {
                let p = entry.path().to_string_lossy().to_string();
                let abs = repo.root().join(&p);
                if abs.is_file() {
                    let rel_dir = stash_dir.join(
                        std::path::Path::new(&p)
                            .parent()
                            .unwrap_or(std::path::Path::new("")),
                    );
                    let _ = std::fs::create_dir_all(&rel_dir);
                    let _ = std::fs::copy(&abs, stash_dir.join(&p));
                    stashed_paths.push(p);
                }
            }
        }

        // Write a manifest so we know which files were stashed
        let manifest_path = stash_dir.join("MANIFEST");
        let manifest_content = stashed_paths.join("\n");
        std::fs::write(&manifest_path, &manifest_content).map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to write stash manifest: {}", e))
        })?;

        // Get stash index for display
        let stashes = self.list_stashes(repo)?;
        let stash_ref = stashes
            .iter()
            .find(|s| s.stack_name == stash_name)
            .map(|s| s.reference())
            .unwrap_or_else(|| "stash@{0}".to_string());

        print_success(&format!("Saved working copy to {}", stash_ref));

        // Restore working copy to clean state (unless --keep)
        if !keep && !self.keep {
            repo.materialize().map_err(CliError::Repository)?;
            print_success("Working copy restored to clean state");
        }

        Ok(())
    }

    /// Execute stash pop (apply and delete).
    fn run_pop(&self, repo: &mut Repository, stash_ref: Option<&str>) -> CliResult<()> {
        let stash = self.parse_stash_ref(repo, stash_ref)?;

        // Apply the stash
        self.apply_stash(repo, &stash)?;

        // Delete the stash view
        repo.delete_view(&stash.stack_name)
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

    /// Apply a stash to the current working copy.
    ///
    /// Reads raw file bytes from the sidecar directory
    /// (`.atomic/stashes/<stash_name>/`) and writes them to disk.
    /// This is a pure filesystem operation — no graph interaction.
    fn apply_stash(&self, repo: &mut Repository, stash: &StashEntry) -> CliResult<()> {
        let stash_dir = repo.dot_dir().join("stashes").join(&stash.stack_name);
        let manifest_path = stash_dir.join("MANIFEST");

        if !manifest_path.exists() {
            return Err(CliError::Internal(anyhow::anyhow!(
                "Stash sidecar not found: {}",
                manifest_path.display()
            )));
        }

        let manifest = std::fs::read_to_string(&manifest_path).map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to read stash manifest: {}", e))
        })?;

        let repo_root = repo.root().to_path_buf();
        let mut files_restored = 0usize;

        for line in manifest.lines() {
            let path = line.trim();
            if path.is_empty() {
                continue;
            }
            let src = stash_dir.join(path);
            let dst = repo_root.join(path);

            if src.is_file() {
                if let Some(parent) = dst.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if std::fs::copy(&src, &dst).is_ok() {
                    files_restored += 1;
                }
            }
        }

        if files_restored > 0 {
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
                style_stack(&stash.source_stack),
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
        println!("  Source stack: {}", style_stack(&stash.source_stack));
        println!("  Message: {}", stash.message);
        println!(
            "  Created: {}",
            format_timestamp_relative(&stash.created_at)
        );

        // Get view info
        let info = repo
            .get_view_info(&stash.stack_name)
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
        let stash = self.parse_stash_ref(repo, stash_ref)?;

        // Delete the sidecar directory
        let stash_dir = repo.dot_dir().join("stashes").join(&stash.stack_name);
        if stash_dir.exists() {
            let _ = std::fs::remove_dir_all(&stash_dir);
        }

        // Delete the stash view
        repo.delete_view(&stash.stack_name)
            .map_err(CliError::Repository)?;

        print_success(&format!("Dropped {}", stash.reference()));
        Ok(())
    }

    /// Execute stash clear.
    fn run_clear(&self, repo: &mut Repository, force: bool) -> CliResult<()> {
        let stashes = self.list_stashes(repo)?;

        if stashes.is_empty() {
            println!("No stashes to clear");
            return Ok(());
        }

        if !force {
            println!(
                "This will delete {} stash(es). Are you sure? [y/N] ",
                stashes.len()
            );
            std::io::Write::flush(&mut std::io::stdout()).ok();

            let mut input = String::new();
            std::io::stdin().read_line(&mut input).ok();

            if !input.trim().eq_ignore_ascii_case("y") {
                println!("Aborted");
                return Ok(());
            }
        }

        let count = stashes.len();
        for stash in stashes {
            repo.delete_view(&stash.stack_name)
                .map_err(CliError::Repository)?;
        }

        print_success(&format!("Cleared {} stash(es)", count));

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
            stack_name: "stash/test".to_string(),
            source_stack: "dev".to_string(),
            message: "WIP".to_string(),
            created_at: Utc::now(),
        };

        assert_eq!(entry.reference(), "stash@{0}");
    }

    #[test]
    fn test_stash_entry_reference_nonzero() {
        let entry = StashEntry {
            index: 3,
            stack_name: "stash/test".to_string(),
            source_stack: "dev".to_string(),
            message: "WIP".to_string(),
            created_at: Utc::now(),
        };

        assert_eq!(entry.reference(), "stash@{3}");
    }

    #[test]
    fn test_stash_entry_display() {
        let entry = StashEntry {
            index: 0,
            stack_name: "stash/test".to_string(),
            source_stack: "feature".to_string(),
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
