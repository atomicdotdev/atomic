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
//!   -f, --force    Overwrite an existing untracked destination
//!   -h, --help     Print help information
//! ```
//!
//! # Behavior
//!
//! The `move` command:
//! 1. Refuses tracked or unsafe destinations
//! 2. Moves the file on disk and stages its original inode at the new path
//! 3. Preserves the file's history when the move is recorded
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

/// Move or rename a tracked file.
///
/// The `move` command moves or renames a file while preserving its version
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
/// - `--force` / `-f`: Overwrite an existing untracked destination
#[derive(Parser, Debug, Clone)]
#[command(name = "move")]
#[derive(Default)]
pub struct Move {
    /// Source file to move.
    ///
    /// Must be a tracked regular file.
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

    /// Overwrite an existing untracked destination on disk.
    ///
    /// A tracked destination is always refused because replacing its identity
    /// requires an explicit remove/resolve operation.
    #[arg(short = 'f', long = "force")]
    pub force: bool,
}

impl Move {
    /// Create a new Move command.
    pub fn new<S1: Into<String>, S2: Into<String>>(source: S1, destination: S2) -> Self {
        Self {
            source: source.into(),
            destination: destination.into(),
            dry_run: false,
            force: false,
        }
    }

    /// Builder: set the dry-run flag.
    pub fn with_dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }

    /// Builder: set the force flag.
    pub fn with_force(mut self, force: bool) -> Self {
        self.force = force;
        self
    }

    /// Normalize a path relative to the repository root.
    fn normalize_path(&self, repo_root: &Path, path: &str) -> CliResult<String> {
        let p = Path::new(path);
        // On Windows, Path::is_absolute() returns false for Unix-style "/foo"
        // paths (they're drive-relative). Treat a leading '/' as absolute on
        // all platforms so cross-platform behaviour is consistent.
        let looks_absolute = p.is_absolute() || path.starts_with('/');
        if looks_absolute {
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

    fn restore_after_staging_failure(
        source: &Path,
        destination: &Path,
        destination_backup: Option<&Path>,
    ) -> std::io::Result<()> {
        // Restore the source first. If that fails, leave its bytes at the
        // destination and the original destination in the backup; restoring the
        // backup at that point could overwrite the only surviving source copy.
        std::fs::rename(destination, source)?;
        if let Some(backup) = destination_backup {
            std::fs::rename(backup, destination)?;
        }
        Ok(())
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
        let working_copy = repo
            .require_working_copy_id()
            .map_err(CliError::Repository)?;

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

        if source_path.is_dir() {
            return Err(CliError::InvalidArgument {
                message: "directory moves are not yet supported; move tracked files individually"
                    .to_string(),
            });
        }

        // Never overwrite another tracked identity, even with --force. For an
        // untracked filesystem destination, require --force before rename(2)
        // can replace its bytes on platforms where that is the default.
        if repo
            .is_tracked(&destination)
            .map_err(CliError::Repository)?
        {
            return Err(CliError::FileAlreadyTracked {
                path: PathBuf::from(&destination),
            });
        }
        let destination_path = repo_root.join(&destination);
        let destination_metadata = std::fs::symlink_metadata(&destination_path).ok();
        if destination_metadata.is_some() && !self.force {
            return Err(CliError::InvalidArgument {
                message: format!(
                    "destination '{}' already exists; use --force only for untracked content",
                    destination
                ),
            });
        }
        if destination_metadata
            .as_ref()
            .is_some_and(|metadata| !metadata.file_type().is_file())
        {
            return Err(CliError::InvalidArgument {
                message: format!(
                    "destination '{}' is not a regular file and cannot be replaced safely",
                    destination
                ),
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

        // Preserve a forced destination until both filesystem movement and
        // stable-inode staging succeed. This makes a later staging failure fully
        // reversible instead of losing the overwritten bytes.
        let destination_backup = if destination_metadata.is_some() {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let backup = destination_path.with_file_name(format!(
                ".atomic-mv-backup-{}-{}",
                std::process::id(),
                nonce
            ));
            if std::fs::symlink_metadata(&backup).is_ok() {
                return Err(CliError::Internal(anyhow::anyhow!(
                    "refusing move because rollback path already exists: {}",
                    backup.display()
                )));
            }
            std::fs::rename(&destination_path, &backup).map_err(|error| {
                CliError::Internal(anyhow::anyhow!(
                    "Failed to preserve forced destination '{}': {}",
                    destination,
                    error
                ))
            })?;
            Some(backup)
        } else {
            None
        };

        // Move the file first, then stage the same stable inode at the new path.
        // Repository::record compares this staged TREE relationship with the
        // graph-backed source claim, so the next record can authoritatively emit
        // FileMove plus any intervening content edits.
        if let Err(error) = self.move_file_on_disk(&repo_root, &source, &destination) {
            if let Some(backup) = &destination_backup {
                if let Err(restore_error) = std::fs::rename(backup, &destination_path) {
                    return Err(CliError::Internal(anyhow::anyhow!(
                        "Failed to move '{} -> {}': {}; restoring destination also failed: {}",
                        source,
                        destination,
                        error,
                        restore_error
                    )));
                }
            }
            return Err(CliError::Internal(anyhow::anyhow!(
                "Failed to move file on disk: {}",
                error
            )));
        }
        if let Err(error) = repo.move_file(working_copy, &source, &destination) {
            if let Err(rollback_error) = Self::restore_after_staging_failure(
                &source_path,
                &destination_path,
                destination_backup.as_deref(),
            ) {
                return Err(CliError::Internal(anyhow::anyhow!(
                    "Failed to stage move '{} -> {}': {}; filesystem rollback also failed: {}",
                    source,
                    destination,
                    error,
                    rollback_error
                )));
            }
            return Err(CliError::Repository(error));
        }
        if let Some(backup) = destination_backup {
            if let Err(error) = std::fs::remove_file(&backup) {
                print_warning(&format!(
                    "Move succeeded, but preserved destination backup could not be removed: {} ({})",
                    backup.display(),
                    error
                ));
            }
        }

        println!();
        print_success("Moved 1 file");
        println!();
        print_hint("Run 'atomic record' to capture this move (inode/history preserved)");
        Ok(())
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
    fn test_staging_failure_restores_source_and_forced_destination() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.txt");
        let destination = temp.path().join("destination.txt");
        let backup = temp.path().join(".destination.backup");
        std::fs::write(&source, b"source bytes").unwrap();
        std::fs::write(&destination, b"destination bytes").unwrap();

        // Reproduce the filesystem state immediately before stable-inode
        // staging: the old destination is preserved and source occupies dest.
        std::fs::rename(&destination, &backup).unwrap();
        std::fs::rename(&source, &destination).unwrap();
        Move::restore_after_staging_failure(&source, &destination, Some(&backup)).unwrap();

        assert_eq!(std::fs::read(&source).unwrap(), b"source bytes");
        assert_eq!(std::fs::read(&destination).unwrap(), b"destination bytes");
        assert!(!backup.exists());
    }

    #[test]
    fn test_failed_source_restore_preserves_both_payloads() {
        let temp = tempfile::tempdir().unwrap();
        let missing_source = temp.path().join("missing/source.txt");
        let destination = temp.path().join("destination.txt");
        let backup = temp.path().join(".destination.backup");
        std::fs::write(&destination, b"source bytes").unwrap();
        std::fs::write(&backup, b"destination bytes").unwrap();

        assert!(
            Move::restore_after_staging_failure(&missing_source, &destination, Some(&backup),)
                .is_err()
        );
        assert_eq!(std::fs::read(&destination).unwrap(), b"source bytes");
        assert_eq!(std::fs::read(&backup).unwrap(), b"destination bytes");
    }

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
