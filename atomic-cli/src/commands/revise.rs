//! Revise command - modify a change in-place
//!
//! The `revise` command allows modifying a previously recorded change without
//! losing its position in the view. This is similar to `git commit --amend`
//! but more powerful because it can target any change, not just HEAD.
//!
//! # Overview
//!
//! Revising a change involves:
//! 1. Unrecording changes from the target to the top of the view
//! 2. Letting the user modify the working copy (or just the message)
//! 3. Recording a new change to replace the target
//! 4. Re-applying the subsequent changes on top
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                         Revise Workflow                                  │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  Before:  [A] ← [B] ← [C] ← @                                          │
//! │                  ↑                                                       │
//! │             target (@~1)                                                 │
//! │                                                                         │
//! │  Step 1: Unrecord C (saved as pending)                                  │
//! │           [A] ← [B] ← @                                                 │
//! │                                                                         │
//! │  Step 2: Unrecord B (target)                                            │
//! │           [A] ← @    (working copy has B's changes)                     │
//! │                                                                         │
//! │  Step 3: User edits, record as B'                                       │
//! │           [A] ← [B'] ← @                                                │
//! │                                                                         │
//! │  Step 4: Re-apply pending (C)                                           │
//! │           [A] ← [B'] ← [C] ← @                                          │
//! │                                                                         │
//! │  Result: B revised to B', C re-applied on top                          │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Reference System
//!
//! Changes are referenced using a Git-like syntax:
//!
//! - `@` or no argument: The last (most recent) change
//! - `@~1`: The previous change (one before last)
//! - `@~N`: N changes back from the last
//!
//! # Examples
//!
//! ```text
//! # Revise the last change (like git commit --amend)
//! atomic revise
//!
//! # Revise the last change with a new message
//! atomic revise -m "Better commit message"
//!
//! # Only change the message (don't modify files)
//! atomic revise --reword
//!
//! # Revise a previous change
//! atomic revise @~1
//!
//! # Revise two changes back with a new message
//! atomic revise @~2 -m "Fixed the bug properly"
//! ```
//!
//! # Differences from Git
//!
//! | Aspect | Git | Atomic |
//! |--------|-----|--------|
//! | Target | Only HEAD | Any change via `@~N` |
//! | History | Rewrites commits | Preserves graph structure |
//! | Subsequent | Must rebase manually | Automatically re-applied |
//! | Safety | Can lose commits | Changes always preserved |

use std::path::PathBuf;

use atomic_core::change::{Change, ChangeHeader};
use atomic_core::types::{Base32, Hash};
use atomic_repository::{
    HistoryEntry, HistoryOptions, RecordOptions, Repository, StatusOptions, UnrecordOptions,
};
use clap::Parser;

use crate::commands::{find_repository_root, format_hash, Command};
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_success, print_warning};

// Reference Parsing

/// A parsed change reference.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ChangeRef {
    /// The last change (`@` or no argument)
    #[default]
    Last,
    /// N changes back from the last (`@~N`)
    Relative(u64),
    /// A specific change by hash or hash prefix
    Hash(String),
}

impl ChangeRef {
    /// Parse a change reference string.
    ///
    /// Supported formats:
    /// - `@` or empty: Last change
    /// - `@~1`, `@~2`, etc.: Relative to last
    /// - Anything else: Treated as hash/hash prefix
    pub fn parse(s: &str) -> Self {
        let s = s.trim();

        if s.is_empty() || s == "@" {
            return Self::Last;
        }

        // Check for @~N pattern
        if let Some(rest) = s.strip_prefix("@~") {
            if let Ok(n) = rest.parse::<u64>() {
                return Self::Relative(n);
            }
        }

        // Treat as hash
        Self::Hash(s.to_string())
    }

    /// Resolve this reference to a sequence number.
    ///
    /// Returns the 0-indexed sequence number in the view.
    pub fn resolve(&self, change_count: u64) -> Result<u64, String> {
        if change_count == 0 {
            return Err("View is empty".to_string());
        }

        match self {
            Self::Last => Ok(change_count - 1),
            Self::Relative(n) => {
                if *n >= change_count {
                    return Err(format!(
                        "Reference @~{} is out of range (view has {} changes)",
                        n, change_count
                    ));
                }
                Ok(change_count - 1 - n)
            }
            Self::Hash(_) => {
                // Hash resolution requires repository access, handled separately
                Err("Hash references must be resolved through the repository".to_string())
            }
        }
    }

    /// Get a human-readable description of this reference.
    pub fn description(&self) -> String {
        match self {
            Self::Last => "@".to_string(),
            Self::Relative(n) => format!("@~{}", n),
            Self::Hash(h) => h.clone(),
        }
    }
}

// Revise Command

/// Revise a change in-place.
///
/// Modifies a previously recorded change without losing its position in the
/// view. This is useful for fixing typos, updating commit messages, or
/// making small corrections to recent changes.
///
/// # Reference System
///
/// Use `@` for the last change, `@~1` for the previous, `@~2` for two back, etc.
///
/// # Examples
///
/// ```text
/// # Revise the last change
/// atomic revise
///
/// # Revise with a new message
/// atomic revise -m "Better message"
///
/// # Only change the message
/// atomic revise --reword
///
/// # Revise a previous change
/// atomic revise @~1
/// ```
#[derive(Parser, Debug, Clone)]
#[command(name = "revise")]
pub struct Revise {
    /// The change to revise (default: @ = last change).
    ///
    /// Use @ for the last change, @~1 for the previous, @~2 for two back, etc.
    #[arg(default_value = "@")]
    pub reference: String,

    /// New message for the revised change.
    ///
    /// If not provided, the original message is used (unless --reword is set,
    /// which opens an editor).
    #[arg(short, long)]
    pub message: Option<String>,

    /// Only change the message, don't modify files.
    ///
    /// Opens an editor with the current message for modification.
    /// Any working copy changes are ignored.
    #[arg(long)]
    pub reword: bool,

    /// Don't open an editor for the message.
    ///
    /// Use the original message unchanged (unless -m is provided).
    #[arg(long)]
    pub no_edit: bool,

    /// Author for the revised change (format: `"Name <email>"`).
    ///
    /// If not provided, uses the original author.
    #[arg(short, long)]
    pub author: Option<String>,

    /// Show what would be done without making changes.
    #[arg(long)]
    pub dry_run: bool,

    /// Include all untracked files in the revision.
    #[arg(short, long)]
    pub all: bool,

    /// Specific files to include in the revision.
    #[arg(last = true)]
    pub files: Vec<PathBuf>,
}

impl Revise {
    /// Create a new Revise command with default settings.
    pub fn new() -> Self {
        Self {
            reference: "@".to_string(),
            message: None,
            reword: false,
            no_edit: false,
            author: None,
            dry_run: false,
            all: false,
            files: Vec::new(),
        }
    }

    /// Builder: set the reference.
    pub fn with_reference(mut self, reference: impl Into<String>) -> Self {
        self.reference = reference.into();
        self
    }

    /// Builder: set the message.
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    /// Builder: set the reword flag.
    pub fn with_reword(mut self, reword: bool) -> Self {
        self.reword = reword;
        self
    }

    /// Builder: set the no-edit flag.
    pub fn with_no_edit(mut self, no_edit: bool) -> Self {
        self.no_edit = no_edit;
        self
    }

    /// Builder: set the dry-run flag.
    pub fn with_dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }

    /// Parse the reference string into a ChangeRef.
    fn parse_reference(&self) -> ChangeRef {
        ChangeRef::parse(&self.reference)
    }

    /// Resolve the reference to a sequence number and entry.
    fn resolve_reference(&self, repo: &Repository) -> CliResult<(u64, HistoryEntry)> {
        let change_ref = self.parse_reference();

        // Get current view info
        let stack_name = repo.current_view();
        let stack_info = repo
            .get_view_info(stack_name)
            .map_err(CliError::Repository)?;

        if stack_info.change_count == 0 {
            return Err(CliError::Internal(anyhow::anyhow!(
                "Cannot revise: view '{}' is empty",
                stack_info.name
            )));
        }

        // Resolve reference to sequence number
        let sequence = match &change_ref {
            ChangeRef::Last | ChangeRef::Relative(_) => change_ref
                .resolve(stack_info.change_count)
                .map_err(|e| CliError::Internal(anyhow::anyhow!("{}", e)))?,
            ChangeRef::Hash(hash_str) => {
                // Find the change by hash prefix
                self.find_by_hash(repo, hash_str)?
            }
        };

        // Get the history entry at that sequence using log
        let history = repo
            .log(HistoryOptions::default())
            .map_err(CliError::Repository)?;

        let entry = history
            .into_iter()
            .find(|e| e.sequence == sequence)
            .ok_or_else(|| {
                CliError::Internal(anyhow::anyhow!("Change at sequence {} not found", sequence))
            })?;

        Ok((sequence, entry))
    }

    /// Find a change by hash prefix.
    fn find_by_hash(&self, repo: &Repository, hash_str: &str) -> CliResult<u64> {
        // Get history and search for matching hash
        let history = repo
            .log(HistoryOptions::default())
            .map_err(CliError::Repository)?;

        let hash_lower = hash_str.to_lowercase();
        let mut found: Option<u64> = None;

        for entry in history {
            let entry_hash = entry.hash.to_base32().to_lowercase();
            if entry_hash.starts_with(&hash_lower) {
                if found.is_some() {
                    return Err(CliError::AmbiguousHash {
                        hash: hash_str.to_string(),
                    });
                }
                found = Some(entry.sequence);
            }
        }

        found.ok_or_else(|| CliError::ChangeNotFound {
            hash: hash_str.to_string(),
        })
    }

    /// Get the message for the revised change.
    fn get_message(&self, original_message: &str) -> CliResult<String> {
        // If explicit message provided, use it
        if let Some(ref msg) = self.message {
            return Ok(msg.clone());
        }

        // If reword mode and not no-edit, open editor
        if self.reword && !self.no_edit {
            return self.get_message_from_editor(original_message);
        }

        // Otherwise use original message
        Ok(original_message.to_string())
    }

    /// Open an editor for the user to edit the message.
    fn get_message_from_editor(&self, original_message: &str) -> CliResult<String> {
        use std::io::Read;

        // Create a temporary file with the original message
        let temp_dir = std::env::temp_dir();
        let temp_file = temp_dir.join("ATOMIC_REVISE_MSG");

        // Write original message with instructions
        let content = format!(
            "{}\n\n# Revising change. Lines starting with '#' will be ignored.\n# An empty message aborts the revision.\n",
            original_message
        );

        std::fs::write(&temp_file, &content).map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create temp file: {}", e))
        })?;

        // Get editor from environment
        let editor = std::env::var("EDITOR")
            .or_else(|_| std::env::var("VISUAL"))
            .unwrap_or_else(|_| "vi".to_string());

        // Open editor
        let status = std::process::Command::new(&editor)
            .arg(&temp_file)
            .status()
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to open editor: {}", e)))?;

        if !status.success() {
            return Err(CliError::Cancelled);
        }

        // Read the edited message
        let mut edited = String::new();
        std::fs::File::open(&temp_file)
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to read temp file: {}", e)))?
            .read_to_string(&mut edited)
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to read temp file: {}", e)))?;

        // Clean up
        let _ = std::fs::remove_file(&temp_file);

        // Filter out comment lines and trim
        let message: String = edited
            .lines()
            .filter(|line| !line.starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string();

        if message.is_empty() {
            return Err(CliError::Cancelled);
        }

        Ok(message)
    }

    /// Parse author string into name and email.
    fn parse_author(&self) -> Option<(String, Option<String>)> {
        self.author.as_ref().map(|author_str| {
            // Try to parse "Name <email>" format
            if let Some(start) = author_str.find('<') {
                if let Some(end) = author_str.find('>') {
                    let name = author_str[..start].trim().to_string();
                    let email = author_str[start + 1..end].trim().to_string();
                    return (name, Some(email));
                }
            }
            // Just a name
            (author_str.trim().to_string(), None)
        })
    }

    /// Display what would be done in dry-run mode.
    fn display_dry_run(
        &self,
        repo: &Repository,
        sequence: u64,
        entry: &HistoryEntry,
    ) -> CliResult<()> {
        let stack_name = repo.current_view();
        let stack_info = repo
            .get_view_info(stack_name)
            .map_err(CliError::Repository)?;
        let changes_to_unrecord = stack_info.change_count - sequence;

        println!(
            "Would revise change {} (sequence #{})",
            format_hash(&entry.hash, false),
            sequence
        );
        println!();

        if changes_to_unrecord > 1 {
            println!(
                "This will temporarily unrecord {} changes:",
                changes_to_unrecord
            );

            // Get history entries for display
            let history = repo
                .log(HistoryOptions::default())
                .map_err(CliError::Repository)?;

            // Filter and display entries from sequence to end (in reverse order)
            let mut entries_to_show: Vec<_> = history
                .into_iter()
                .filter(|e| e.sequence >= sequence)
                .collect();
            entries_to_show.sort_by_key(|x| std::cmp::Reverse(x.sequence));

            for e in entries_to_show {
                let marker = if e.sequence == sequence {
                    " (target)"
                } else {
                    ""
                };
                println!(
                    "  #{}: {}{}",
                    e.sequence,
                    format_hash(&e.hash, false),
                    marker
                );
            }
            println!();
            println!(
                "After revision, {} changes will be re-applied.",
                changes_to_unrecord - 1
            );
        } else {
            println!("This is the last change - no re-application needed.");
        }

        if self.reword {
            println!();
            println!("Mode: --reword (only change message, preserve file changes)");
        }

        Ok(())
    }

    /// Execute the revise operation.
    fn execute_revise(
        &self,
        repo: &Repository,
        sequence: u64,
        entry: &HistoryEntry,
    ) -> CliResult<Hash> {
        let stack_name = repo.current_view();
        let stack_info = repo
            .get_view_info(stack_name)
            .map_err(CliError::Repository)?;

        // Load the original change to get its message
        let original_change = repo
            .load_change(&entry.hash)
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to load change: {}", e)))?;

        let original_message = original_change.hashed.header.message.clone();

        // Determine how many changes need to be unrecorded
        let changes_to_unrecord = stack_info.change_count - sequence;

        // Collect changes that will need to be re-applied (everything after target)
        let mut pending_changes: Vec<Hash> = Vec::new();

        if changes_to_unrecord > 1 {
            // Get history and collect changes after the target
            let history = repo
                .log(HistoryOptions::default())
                .map_err(CliError::Repository)?;

            // Collect entries after the target sequence, sorted by sequence
            let mut entries_after: Vec<_> = history
                .into_iter()
                .filter(|e| e.sequence > sequence)
                .collect();
            entries_after.sort_by_key(|e| e.sequence);

            // Extract hashes in order (oldest first for re-application)
            pending_changes = entries_after.into_iter().map(|e| e.hash).collect();
        }

        // Step 1: Unrecord changes from top down to (and including) target
        if !pending_changes.is_empty() {
            print_hint(&format!(
                "Temporarily unrecording {} changes after target...",
                pending_changes.len()
            ));
        }

        // Unrecord in reverse order (from newest to target)
        for _ in 0..changes_to_unrecord {
            repo.unrecord_last(UnrecordOptions::default())
                .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to unrecord: {}", e)))?;
        }

        // Step 2: Get the message for the new change
        let message = self.get_message(&original_message)?;

        // Step 3: Record the new change
        let author = if let Some((name, email)) = self.parse_author() {
            Some(atomic_core::change::Author {
                name,
                email,
                identity: None,
            })
        } else {
            // Preserve original authors
            original_change.hashed.header.authors.first().cloned()
        };

        // Handle --reword mode differently: create a new change with same content
        // but different header, rather than going through working copy
        if self.reword {
            return self.execute_reword(repo, &original_change, &message, author, &pending_changes);
        }

        // Build header for content modification mode
        let mut header_builder = ChangeHeader::builder().message(&message);
        if let Some(author) = author {
            header_builder = header_builder.author(author);
        }
        let header = header_builder.build();

        // Build record options for content modification mode
        let mut options = RecordOptions::default();

        if !self.files.is_empty() {
            options = options.paths(
                self.files
                    .iter()
                    .map(|p| p.to_string_lossy().to_string())
                    .collect::<Vec<_>>(),
            );
        }

        // Record the revised change
        let outcome = repo.record(header, options).map_err(|e| {
            // If recording fails, we should try to restore the state
            print_warning(&format!(
                "Recording failed: {}. Attempting to restore...",
                e
            ));

            // Try to re-apply all the unrecorded changes
            for hash in pending_changes.iter().rev() {
                if let Err(restore_err) = repo.reinsert_change(hash, None) {
                    print_warning(&format!(
                        "Failed to restore change {}: {}",
                        format_hash(hash, false),
                        restore_err
                    ));
                }
            }

            CliError::Internal(anyhow::anyhow!("Failed to record revised change: {}", e))
        })?;

        let new_hash = *outcome.hash();

        // Step 4: Re-apply pending changes
        if !pending_changes.is_empty() {
            print_hint(&format!("Re-applying {} changes...", pending_changes.len()));

            for hash in &pending_changes {
                repo.reinsert_change(hash, None).map_err(|e| {
                    print_warning(&format!(
                        "Failed to re-apply change {}: {}",
                        format_hash(hash, false),
                        e
                    ));
                    CliError::Internal(anyhow::anyhow!(
                        "Failed to re-apply change {}: {}",
                        format_hash(hash, false),
                        e
                    ))
                })?;
            }
        }

        Ok(new_hash)
    }

    /// Execute reword operation - creates new change with same content but new header.
    ///
    /// This is cleaner than going through working copy because:
    /// 1. We preserve exact same hunks/content
    /// 2. No risk of capturing unintended working copy changes
    /// 3. Works correctly even with pending changes to re-apply
    fn execute_reword(
        &self,
        repo: &Repository,
        original_change: &Change,
        message: &str,
        author: Option<atomic_core::change::Author>,
        pending_changes: &[Hash],
    ) -> CliResult<Hash> {
        // Build new header with updated message
        let mut new_header = original_change.hashed.header.clone();
        new_header.message = message.to_string();

        // Update author if provided
        if let Some(new_author) = author {
            new_header.authors = vec![new_author];
        }

        // Create new change with same hunks but new header
        let new_change = Change::with_file_ops(
            new_header,
            original_change.hashed.hunks.clone(),
            original_change.hashed.file_ops.clone(),
            original_change.contents.clone(),
            original_change.hashed.dependencies.clone(),
        );

        // Save the new change
        let new_hash = repo.save_change(&new_change).map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to save reworded change: {}", e))
        })?;

        // Insert the new change into the view (it replaces the unrecorded one)
        repo.insert_change(&new_hash, Default::default())
            .map_err(|e| {
                CliError::Internal(anyhow::anyhow!("Failed to apply reworded change: {}", e))
            })?;

        // Re-apply pending changes
        if !pending_changes.is_empty() {
            print_hint(&format!("Re-applying {} changes...", pending_changes.len()));

            for hash in pending_changes {
                repo.reinsert_change(hash, None).map_err(|e| {
                    print_warning(&format!(
                        "Failed to re-apply change {}: {}",
                        format_hash(hash, false),
                        e
                    ));
                    CliError::Internal(anyhow::anyhow!(
                        "Failed to re-apply change {}: {}",
                        format_hash(hash, false),
                        e
                    ))
                })?;
            }
        }

        Ok(new_hash)
    }
}

impl Default for Revise {
    fn default() -> Self {
        Self::new()
    }
}

impl Command for Revise {
    /// Execute the revise command.
    ///
    /// # Process
    ///
    /// 1. Parse and resolve the change reference
    /// 2. If dry-run, show what would happen and exit
    /// 3. Unrecord changes from target to top
    /// 4. Get new message (from arg, editor, or original)
    /// 5. Record the revised change
    /// 6. Re-apply subsequent changes
    fn run(&self) -> CliResult<()> {
        // Find and open repository
        let repo_root = find_repository_root()?;
        let repo = Repository::open(&repo_root).map_err(CliError::Repository)?;

        // Resolve the reference
        let (sequence, entry) = self.resolve_reference(&repo)?;

        // Dry run mode
        if self.dry_run {
            return self.display_dry_run(&repo, sequence, &entry);
        }

        // Check for uncommitted changes if not in reword mode
        if !self.reword {
            let status = repo
                .status(StatusOptions::default())
                .map_err(CliError::Repository)?;

            if status.is_clean() && self.message.is_none() {
                print_warning("No changes to revise. Use --reword to only change the message.");
                return Ok(());
            }
        }

        // Execute the revise
        let change_ref = self.parse_reference();
        println!(
            "Revising change {} (sequence #{})...",
            change_ref.description(),
            sequence
        );

        let new_hash = self.execute_revise(&repo, sequence, &entry)?;

        // Success message
        println!();
        print_success(&format!(
            "Revised {} → {}",
            format_hash(&entry.hash, false),
            format_hash(&new_hash, false)
        ));

        Ok(())
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    // ChangeRef Tests

    #[test]
    fn test_change_ref_parse_empty() {
        assert_eq!(ChangeRef::parse(""), ChangeRef::Last);
    }

    #[test]
    fn test_change_ref_parse_at() {
        assert_eq!(ChangeRef::parse("@"), ChangeRef::Last);
    }

    #[test]
    fn test_change_ref_parse_at_with_whitespace() {
        assert_eq!(ChangeRef::parse("  @  "), ChangeRef::Last);
    }

    #[test]
    fn test_change_ref_parse_relative_1() {
        assert_eq!(ChangeRef::parse("@~1"), ChangeRef::Relative(1));
    }

    #[test]
    fn test_change_ref_parse_relative_5() {
        assert_eq!(ChangeRef::parse("@~5"), ChangeRef::Relative(5));
    }

    #[test]
    fn test_change_ref_parse_relative_0() {
        assert_eq!(ChangeRef::parse("@~0"), ChangeRef::Relative(0));
    }

    #[test]
    fn test_change_ref_parse_relative_large() {
        assert_eq!(ChangeRef::parse("@~100"), ChangeRef::Relative(100));
    }

    #[test]
    fn test_change_ref_parse_hash() {
        assert_eq!(
            ChangeRef::parse("ABC123"),
            ChangeRef::Hash("ABC123".to_string())
        );
    }

    #[test]
    fn test_change_ref_parse_invalid_relative() {
        // Invalid relative syntax treated as hash
        assert_eq!(
            ChangeRef::parse("@~abc"),
            ChangeRef::Hash("@~abc".to_string())
        );
    }

    #[test]
    fn test_change_ref_resolve_last() {
        assert_eq!(ChangeRef::Last.resolve(5), Ok(4));
        assert_eq!(ChangeRef::Last.resolve(1), Ok(0));
    }

    #[test]
    fn test_change_ref_resolve_last_empty_view() {
        assert!(ChangeRef::Last.resolve(0).is_err());
    }

    #[test]
    fn test_change_ref_resolve_relative() {
        assert_eq!(ChangeRef::Relative(0).resolve(5), Ok(4));
        assert_eq!(ChangeRef::Relative(1).resolve(5), Ok(3));
        assert_eq!(ChangeRef::Relative(4).resolve(5), Ok(0));
    }

    #[test]
    fn test_change_ref_resolve_relative_out_of_range() {
        assert!(ChangeRef::Relative(5).resolve(5).is_err());
        assert!(ChangeRef::Relative(10).resolve(5).is_err());
    }

    #[test]
    fn test_change_ref_resolve_hash_error() {
        // Hash references can't be resolved without repo
        assert!(ChangeRef::Hash("ABC".to_string()).resolve(5).is_err());
    }

    #[test]
    fn test_change_ref_description() {
        assert_eq!(ChangeRef::Last.description(), "@");
        assert_eq!(ChangeRef::Relative(1).description(), "@~1");
        assert_eq!(ChangeRef::Relative(5).description(), "@~5");
        assert_eq!(
            ChangeRef::Hash("ABC123".to_string()).description(),
            "ABC123"
        );
    }

    #[test]
    fn test_change_ref_default() {
        assert_eq!(ChangeRef::default(), ChangeRef::Last);
    }

    // Revise Builder Tests

    #[test]
    fn test_revise_new() {
        let revise = Revise::new();
        assert_eq!(revise.reference, "@");
        assert!(revise.message.is_none());
        assert!(!revise.reword);
        assert!(!revise.no_edit);
        assert!(!revise.dry_run);
    }

    #[test]
    fn test_revise_default() {
        let revise = Revise::default();
        assert_eq!(revise.reference, "@");
        assert!(revise.message.is_none());
        assert!(!revise.reword);
    }

    #[test]
    fn test_revise_with_reference() {
        let revise = Revise::new().with_reference("@~1");
        assert_eq!(revise.reference, "@~1");
    }

    #[test]
    fn test_revise_with_message() {
        let revise = Revise::new().with_message("New message");
        assert_eq!(revise.message, Some("New message".to_string()));
    }

    #[test]
    fn test_revise_with_reword() {
        let revise = Revise::new().with_reword(true);
        assert!(revise.reword);
    }

    #[test]
    fn test_revise_with_no_edit() {
        let revise = Revise::new().with_no_edit(true);
        assert!(revise.no_edit);
    }

    #[test]
    fn test_revise_with_dry_run() {
        let revise = Revise::new().with_dry_run(true);
        assert!(revise.dry_run);
    }

    #[test]
    fn test_revise_builder_chain() {
        let revise = Revise::new()
            .with_reference("@~2")
            .with_message("Updated")
            .with_reword(false)
            .with_dry_run(true);

        assert_eq!(revise.reference, "@~2");
        assert_eq!(revise.message, Some("Updated".to_string()));
        assert!(!revise.reword);
        assert!(revise.dry_run);
    }

    #[test]
    fn test_revise_parse_reference() {
        let revise = Revise::new().with_reference("@~3");
        assert_eq!(revise.parse_reference(), ChangeRef::Relative(3));
    }

    // Author Parsing Tests

    #[test]
    fn test_parse_author_none() {
        let revise = Revise::new();
        assert!(revise.parse_author().is_none());
    }

    #[test]
    fn test_parse_author_name_only() {
        let mut revise = Revise::new();
        revise.author = Some("Alice".to_string());
        let (name, email) = revise.parse_author().unwrap();
        assert_eq!(name, "Alice");
        assert!(email.is_none());
    }

    #[test]
    fn test_parse_author_full() {
        let mut revise = Revise::new();
        revise.author = Some("Alice <alice@example.com>".to_string());
        let (name, email) = revise.parse_author().unwrap();
        assert_eq!(name, "Alice");
        assert_eq!(email, Some("alice@example.com".to_string()));
    }

    #[test]
    fn test_parse_author_with_spaces() {
        let mut revise = Revise::new();
        revise.author = Some("  Alice Bob  <alice@example.com>  ".to_string());
        let (name, email) = revise.parse_author().unwrap();
        assert_eq!(name, "Alice Bob");
        assert_eq!(email, Some("alice@example.com".to_string()));
    }

    // Message Tests

    #[test]
    fn test_get_message_explicit() {
        let revise = Revise::new().with_message("Explicit message");
        let msg = revise.get_message("Original").unwrap();
        assert_eq!(msg, "Explicit message");
    }

    #[test]
    fn test_get_message_original() {
        let revise = Revise::new().with_no_edit(true);
        let msg = revise.get_message("Original message").unwrap();
        assert_eq!(msg, "Original message");
    }

    #[test]
    fn test_get_message_reword_no_edit() {
        // When reword is set but no_edit is also set, use original
        let revise = Revise::new().with_reword(true).with_no_edit(true);
        let msg = revise.get_message("Original").unwrap();
        assert_eq!(msg, "Original");
    }

    #[test]
    fn test_get_message_explicit_overrides_reword() {
        // Explicit message takes precedence
        let revise = Revise::new().with_message("Explicit").with_reword(true);
        let msg = revise.get_message("Original").unwrap();
        assert_eq!(msg, "Explicit");
    }
}
