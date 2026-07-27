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
#[derive(Parser, Debug, Clone)]
#[command(name = "move")]
#[derive(Default)]
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
}

impl Move {
    /// Create a new Move command.
    pub fn new<S1: Into<String>, S2: Into<String>>(source: S1, destination: S2) -> Self {
        Self {
            source: source.into(),
            destination: destination.into(),
            dry_run: false,
        }
    }

    /// Builder: set the dry-run flag.
    pub fn with_dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
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

        // The repository cannot move onto an already tracked path.
        if repo
            .is_tracked(&destination)
            .map_err(CliError::Repository)?
        {
            return Err(CliError::FileAlreadyTracked {
                path: PathBuf::from(&destination),
            });
        }

        // `rename` overwrites existing files, so reject any destination entry.
        // `symlink_metadata` also detects dangling symlinks.
        let destination_disk = repo_root.join(&destination);
        if std::fs::symlink_metadata(&destination_disk).is_ok()
            && !is_same_file(&source_path, &destination_disk)
        {
            return Err(CliError::InvalidArgument {
                message: format!(
                    "destination '{}' already exists on disk; move it away or choose another name",
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

/// Returns true when both paths resolve to the same entry.
///
/// This allows case-only renames. Errors return false so moves fail safely.
fn is_same_file(source: &Path, destination: &Path) -> bool {
    match (
        std::fs::canonicalize(source),
        std::fs::canonicalize(destination),
    ) {
        (Ok(source), Ok(destination)) => source == destination,
        _ => false,
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
    }

    #[test]
    fn test_move_default() {
        let cmd = Move::default();
        assert!(cmd.source.is_empty());
        assert!(cmd.destination.is_empty());
        assert!(!cmd.dry_run);
    }

    #[test]
    fn test_move_with_dry_run() {
        let cmd = Move::new("old.rs", "new.rs").with_dry_run(true);
        assert!(cmd.dry_run);
    }

    #[test]
    fn test_move_builder_chain() {
        let cmd = Move::new("src/old.rs", "src/new.rs").with_dry_run(true);

        assert_eq!(cmd.source, "src/old.rs");
        assert_eq!(cmd.destination, "src/new.rs");
        assert!(cmd.dry_run);
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
        let cmd = Move::new("old.rs", "new.rs").with_dry_run(true);
        let cloned = cmd.clone();

        assert_eq!(cloned.source, cmd.source);
        assert_eq!(cloned.destination, cmd.destination);
        assert_eq!(cloned.dry_run, cmd.dry_run);
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

    // Run-level Data-Safety Tests

    struct DirGuard {
        original: PathBuf,
    }

    impl DirGuard {
        fn new() -> Self {
            Self {
                original: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            }
        }
    }

    impl Drop for DirGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original);
        }
    }

    fn init_repo_with_tracked(files: &[(&str, &str)]) -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        {
            let _repo = atomic_repository::Repository::init(temp.path()).unwrap();
        }
        for (name, content) in files {
            std::fs::write(temp.path().join(name), content).unwrap();
        }
        std::env::set_current_dir(temp.path()).unwrap();
        let add = crate::commands::add::Add::new()
            .with_files(files.iter().map(|(n, _)| n.to_string()).collect::<Vec<_>>());
        add.run().unwrap();
        temp
    }

    #[test]
    #[serial_test::serial]
    fn test_move_refuses_existing_untracked_destination() {
        let _guard = DirGuard::new();
        let temp = init_repo_with_tracked(&[("src.txt", "SOURCE")]);
        std::fs::write(temp.path().join("dest.txt"), "PRECIOUS-UNTRACKED").unwrap();

        let result = Move::new("src.txt", "dest.txt").run();

        assert!(result.is_err(), "move onto an existing file must fail");
        let dest = std::fs::read_to_string(temp.path().join("dest.txt")).unwrap();
        assert_eq!(dest, "PRECIOUS-UNTRACKED", "destination must be untouched");
        let src = std::fs::read_to_string(temp.path().join("src.txt")).unwrap();
        assert_eq!(src, "SOURCE", "source must still exist");
    }

    #[test]
    #[serial_test::serial]
    fn test_move_refuses_tracked_destination() {
        let _guard = DirGuard::new();
        let temp = init_repo_with_tracked(&[("a.txt", "A"), ("b.txt", "TRACKED-B")]);

        let result = Move::new("a.txt", "b.txt").run();

        assert!(result.is_err(), "move onto a tracked file must fail");
        assert_eq!(
            std::fs::read_to_string(temp.path().join("b.txt")).unwrap(),
            "TRACKED-B"
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("a.txt")).unwrap(),
            "A"
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_move_into_existing_directory_works() {
        let _guard = DirGuard::new();
        let temp = init_repo_with_tracked(&[("file.txt", "CONTENT")]);
        std::fs::create_dir(temp.path().join("sub")).unwrap();

        let result = Move::new("file.txt", "sub").run();

        assert!(
            result.is_ok(),
            "move into a directory failed: {:?}",
            result.err()
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("sub/file.txt")).unwrap(),
            "CONTENT"
        );
        assert!(!temp.path().join("file.txt").exists());
    }

    #[test]
    #[serial_test::serial]
    fn test_move_allows_case_only_rename() {
        let _guard = DirGuard::new();
        let temp = init_repo_with_tracked(&[("File.txt", "CONTENT")]);

        // The destination is the source on case-insensitive filesystems.
        let result = Move::new("File.txt", "file.txt").run();

        assert!(
            result.is_ok(),
            "case-only rename must be allowed: {:?}",
            result.err()
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("file.txt")).unwrap(),
            "CONTENT"
        );
    }

    #[test]
    #[serial_test::serial]
    #[cfg(unix)]
    fn test_move_refuses_dangling_symlink_destination() {
        let _guard = DirGuard::new();
        let temp = init_repo_with_tracked(&[("src.txt", "SOURCE")]);
        let dangling = temp.path().join("dangling.txt");
        std::os::unix::fs::symlink(temp.path().join("no-such-target"), &dangling).unwrap();

        let result = Move::new("src.txt", "dangling.txt").run();

        assert!(
            result.is_err(),
            "move onto a dangling symlink must fail — Path::exists() reports \
             false for it, which let rename destroy the link"
        );
        assert!(
            std::fs::symlink_metadata(&dangling).unwrap().is_symlink(),
            "the symlink must survive"
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("src.txt")).unwrap(),
            "SOURCE",
            "source must still exist"
        );
    }
}
