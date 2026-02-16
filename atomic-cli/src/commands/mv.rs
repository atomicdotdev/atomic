//! The `move` command for moving/renaming tracked files.
//!
//! This module implements the `atomic move` (alias `mv`) command, which moves
//! or renames files while preserving their tracking history.
//!
//! # Usage
//!
//! ```text
//! atomic move <SOURCE> <DESTINATION>
//! atomic mv <SOURCE> <DESTINATION>
//!
//! Arguments:
//!   <SOURCE>       File or directory to move
//!   <DESTINATION>  New path for the file or directory
//!
//! Options:
//!   -n, --dry-run  Show what would be moved without doing it
//!   -f, --force    Force move even if destination exists
//!   -h, --help     Print help information
//! ```
//!
//! # Behavior
//!
//! The `move` command:
//! 1. Updates the tracking to reflect the new path
//! 2. Moves the actual file on disk
//! 3. Preserves the file's history (same inode)
//!
//! # Examples
//!
//! Rename a file:
//! ```text
//! $ atomic move old_name.rs new_name.rs
//! Moving: old_name.rs → new_name.rs
//! ✓ Moved 1 file
//! ```
//!
//! Move to a directory:
//! ```text
//! $ atomic move file.txt src/file.txt
//! Moving: file.txt → src/file.txt
//! ✓ Moved 1 file
//! ```
//!
//! Dry run:
//! ```text
//! $ atomic move --dry-run old.rs new.rs
//! Would move: old.rs → new.rs
//! (dry run - no changes made)
//! ```

use std::path::{Path, PathBuf};

use clap::Parser;

use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_success, print_warning};

// Move Command

/// Move or rename tracked files.
///
/// The `move` command moves or renames files while preserving their version
/// history. This is the recommended way to rename files in an Atomic repository.
///
/// # Behavior
///
/// - Renames: Same directory, different name
/// - Moves: Different directory
/// - History is preserved because the inode stays the same
///
/// # Options
///
/// - `--dry-run` / `-n`: Preview what would be moved
/// - `--force` / `-f`: Force move even if destination exists (overwrite tracking)
#[derive(Parser, Debug, Clone)]
#[command(name = "move")]
pub struct Move {
    /// Source file or directory to move.
    ///
    /// Must be a tracked file or directory.
    #[arg(value_name = "SOURCE")]
    pub source: String,

    /// Destination path.
    ///
    /// Can be a new filename (for rename) or a directory (for move).
    #[arg(value_name = "DESTINATION")]
    pub destination: String,

    /// Dry run - show what would be moved without doing it.
    ///
    /// When enabled, displays the move operation but doesn't actually
    /// modify the repository or filesystem.
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,

    /// Force move even if destination tracking exists.
    ///
    /// This will overwrite the destination's tracking entry if it exists.
    /// The destination file on disk is NOT overwritten unless you explicitly
    /// remove it first.
    #[arg(short = 'f', long = "force")]
    pub force: bool,
}

impl Move {

    /// Normalize a path relative to the repository root.
    fn normalize_path(&self, repo_root: &Path, path: &str) -> CliResult<String> {
        let p = Path::new(path);
        if p.is_absolute() {
            match p.strip_prefix(repo_root) {
                Ok(rel) => Ok(rel.to_string_lossy().to_string()),
                Err(_) => Err(CliError::PathOutsideRepository {
                    path: PathBuf::from(path),
                }),
            }
        } else {
            Ok(path.to_string())
        }
    }

    /// Resolve the destination path.
    ///
    /// If destination is a directory, append the source filename.
    fn resolve_destination(&self, repo_root: &Path, source: &str, dest: &str) -> String {
        let dest_path = repo_root.join(dest);

        // If destination is an existing directory, move INTO it
        if dest_path.is_dir() {
            let source_name = Path::new(source)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| source.to_string());
            format!("{}/{}", dest.trim_end_matches('/'), source_name)
        } else {
            dest.to_string()
        }
    }

    /// Move the actual file on disk.
    fn move_file_on_disk(&self, repo_root: &Path, from: &str, to: &str) -> std::io::Result<()> {
        let from_path = repo_root.join(from);
        let to_path = repo_root.join(to);

        // Create parent directories if needed
        if let Some(parent) = to_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        std::fs::rename(&from_path, &to_path)
    }
}

impl Default for Move {
    fn default() -> Self {
        Self {
            source: String::new(),
            destination: String::new(),
            dry_run: false,
            force: false,
        }
    }
}

impl Command for Move {
    /// Execute the move command.
    ///
    /// # Process
    ///
    /// 1. Find and open the repository
    /// 2. Normalize source and destination paths
    /// 3. Check that source is tracked
    /// 4. If not dry-run:
    ///    a. Update tracking (move_file in repository)
    ///    b. Move the actual file on disk
    /// 5. Display results
    fn run(&self) -> CliResult<()> {
        // Find repository
        let repo_root = find_repository_root()?;
        let repo = Repository::open(&repo_root).map_err(CliError::Repository)?;

        // Normalize paths
        let source = self.normalize_path(&repo_root, &self.source)?;
        let dest_raw = self.normalize_path(&repo_root, &self.destination)?;
        let destination = self.resolve_destination(&repo_root, &source, &dest_raw);

        // Check if source exists and is tracked
        if !repo.is_tracked(&source).map_err(CliError::Repository)? {
            return Err(CliError::FileNotTracked {
                path: PathBuf::from(&source),
            });
        }

        // Check if source file exists on disk
        let source_path = repo_root.join(&source);
        if !source_path.exists() {
            return Err(CliError::FileNotFound { path: source_path });
        }

        // Check if destination is already tracked (unless force)
        if !self.force
            && repo
                .is_tracked(&destination)
                .map_err(CliError::Repository)?
        {
            return Err(CliError::FileAlreadyTracked {
                path: PathBuf::from(&destination),
            });
        }

        // Dry run mode
        if self.dry_run {
            println!("Would move: {} → {}", source, destination);
            println!();
            print_hint("(dry run - no changes made)");
            return Ok(());
        }

        // Perform the move
        println!("Moving: {} → {}", source, destination);

        // First, move the file on disk
        if let Err(e) = self.move_file_on_disk(&repo_root, &source, &destination) {
            return Err(CliError::Internal(anyhow::anyhow!(
                "Failed to move file on disk: {}",
                e
            )));
        }

        // Then update tracking
        match repo.move_file(&source, &destination) {
            Ok(_inode) => {
                println!();
                print_success("Moved 1 file");
                println!();
                print_hint("Run 'atomic record' to save this change");
                Ok(())
            }
            Err(e) => {
                // Try to undo the filesystem move
                print_warning("Failed to update tracking, attempting to restore file...");
                if let Err(restore_err) = self.move_file_on_disk(&repo_root, &destination, &source)
                {
                    print_warning(&format!(
                        "Could not restore file: {}. Manual intervention required.",
                        restore_err
                    ));
                }
                Err(CliError::Repository(e))
            }
        }
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    // Builder Tests

    #[test]
    fn test_move_new() {
        let cmd = Move::new("old.rs", "new.rs");
        assert_eq!(cmd.source, "old.rs");
        assert_eq!(cmd.destination, "new.rs");
        assert!(!cmd.dry_run);
        assert!(!cmd.force);
    }

    #[test]
    fn test_move_default() {
        let cmd = Move::default();
        assert!(cmd.source.is_empty());
        assert!(cmd.destination.is_empty());
        assert!(!cmd.dry_run);
        assert!(!cmd.force);
    }

    #[test]
    fn test_move_with_dry_run() {
        let cmd = Move::new("old.rs", "new.rs").with_dry_run(true);
        assert!(cmd.dry_run);
    }

    #[test]
    fn test_move_with_force() {
        let cmd = Move::new("old.rs", "new.rs").with_force(true);
        assert!(cmd.force);
    }

    #[test]
    fn test_move_builder_chain() {
        let cmd = Move::new("src/old.rs", "src/new.rs")
            .with_dry_run(true)
            .with_force(true);

        assert_eq!(cmd.source, "src/old.rs");
        assert_eq!(cmd.destination, "src/new.rs");
        assert!(cmd.dry_run);
        assert!(cmd.force);
    }

    // Path Resolution Tests

    #[test]
    fn test_resolve_destination_rename() {
        let cmd = Move::new("old.rs", "new.rs");
        let temp = tempfile::tempdir().unwrap();
        let result = cmd.resolve_destination(temp.path(), "old.rs", "new.rs");
        assert_eq!(result, "new.rs");
    }

    #[test]
    fn test_resolve_destination_to_directory() {
        let cmd = Move::new("file.txt", "subdir");
        let temp = tempfile::tempdir().unwrap();

        // Create the target directory
        std::fs::create_dir(temp.path().join("subdir")).unwrap();

        let result = cmd.resolve_destination(temp.path(), "file.txt", "subdir");
        assert_eq!(result, "subdir/file.txt");
    }

    #[test]
    fn test_resolve_destination_trailing_slash() {
        let cmd = Move::new("file.txt", "subdir/");
        let temp = tempfile::tempdir().unwrap();

        // Create the target directory
        std::fs::create_dir(temp.path().join("subdir")).unwrap();

        let result = cmd.resolve_destination(temp.path(), "file.txt", "subdir/");
        assert_eq!(result, "subdir/file.txt");
    }

    #[test]
    fn test_resolve_destination_nonexistent_treated_as_file() {
        let cmd = Move::new("old.rs", "nonexistent/new.rs");
        let temp = tempfile::tempdir().unwrap();

        // Don't create the directory - should treat as file path
        let result = cmd.resolve_destination(temp.path(), "old.rs", "nonexistent/new.rs");
        assert_eq!(result, "nonexistent/new.rs");
    }

    // Clone Tests

    #[test]
    fn test_move_clone() {
        let cmd = Move::new("old.rs", "new.rs")
            .with_dry_run(true)
            .with_force(true);
        let cloned = cmd.clone();

        assert_eq!(cloned.source, cmd.source);
        assert_eq!(cloned.destination, cmd.destination);
        assert_eq!(cloned.dry_run, cmd.dry_run);
        assert_eq!(cloned.force, cmd.force);
    }

    // Normalize Path Tests

    #[test]
    fn test_normalize_relative_path() {
        let cmd = Move::new("old.rs", "new.rs");
        let temp = tempfile::tempdir().unwrap();
        let result = cmd.normalize_path(temp.path(), "src/file.rs").unwrap();
        assert_eq!(result, "src/file.rs");
    }

    #[test]
    fn test_normalize_absolute_path_inside_repo() {
        let cmd = Move::new("old.rs", "new.rs");
        let temp = tempfile::tempdir().unwrap();
        let abs_path = temp.path().join("src/file.rs");
        let result = cmd
            .normalize_path(temp.path(), abs_path.to_str().unwrap())
            .unwrap();
        assert_eq!(result, "src/file.rs");
    }

    #[test]
    fn test_normalize_absolute_path_outside_repo() {
        let cmd = Move::new("old.rs", "new.rs");
        let temp = tempfile::tempdir().unwrap();
        let result = cmd.normalize_path(temp.path(), "/completely/different/path.rs");
        assert!(result.is_err());
    }
}
