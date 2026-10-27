//! Git interoperability for Atomic, independently buildable from the CLI.
//!
//! The CLI enables this crate through its `git-shim` Cargo feature. Commands
//! preserve the existing clap surface; repository/graph semantics remain in
//! `atomic-repository` and `atomic-core`. No dependency points back to the CLI.

pub mod commands;
pub mod error;
pub mod output;
mod workspace_txn;

use atomic_repository::SANDBOX_POINTER;
use dirs::home_dir;
use error::{ShimError, ShimResult};
use std::path::{Path, PathBuf};
const DOT_DIR: &str = ".atomic";

pub fn find_repository_root() -> ShimResult<PathBuf> {
    let cwd = std::env::current_dir().map_err(ShimError::Io)?;
    find_repository_root_from(&cwd)
}

/// Find the repository root starting from a specific path.
///
/// Like [`find_repository_root`], but starts from the given path instead
/// of the current working directory.
///
/// # Arguments
///
/// * `start_path` - The path to start searching from
///
/// # Returns
///
/// The path to the repository root.
///
/// # Errors
///
/// Returns [`ShimError::RepositoryNotFound`] if no repository is found.
///
/// # Example
///
/// ```rust,ignore
/// let repo_root = find_repository_root_from("/home/user/project/src")?;
/// ```
pub fn find_repository_root_from(start_path: &Path) -> ShimResult<PathBuf> {
    let mut current = start_path.to_path_buf();

    // Canonicalize if possible to handle symlinks correctly
    if let Ok(canonical) = current.canonicalize() {
        current = canonical;
    }

    // Never walk above $HOME: ~/.atomic holds global configuration (identity,
    // config.toml, logs) but no repository database, so treating it as a
    // search boundary prevents `atomic status`/`record`/etc. from silently
    // failing when run in the home directory, and keeps home from being
    // mistaken for a repo root.
    let home = home_dir();

    loop {
        let dot_dir = current.join(DOT_DIR);
        // A bare `.atomic/` may hold global configuration (e.g. ~/.atomic),
        // not a repository. Require the repository's graph database too.
        if dot_dir.is_dir() && atomic_repository::has_database(&dot_dir) {
            return Ok(current);
        }

        // A sandbox working tree has no `.atomic/` of its own — it carries a
        // pointer to the canonical graph. Treat the sandbox root as a valid
        // repository root; `Repository::open*` resolves the pointer.
        if current.join(SANDBOX_POINTER).is_file() {
            return Ok(current);
        }

        // CB-13D ::24 R1/R5: a LINKED worktree also has no local `.atomic/`
        // — its `.git` pointer resolves to the common repository's `.atomic`.
        // The repository layer's layout detection is authoritative here; a
        // plain `git worktree` used to fall through to "not a repository".
        if current.join(".git").is_file() {
            if let Ok(Some(root)) = atomic_repository::detect_repository_root(&current) {
                return Ok(root);
            }
        }

        // Stop at $HOME rather than walking into / — home is not a repo.
        if home.as_ref().is_some_and(|h| current == *h) {
            return Err(ShimError::RepositoryNotFound {
                searched_path: start_path.to_path_buf(),
            });
        }

        // Move to parent directory
        if !current.pop() {
            // Reached the root without finding a repository
            return Err(ShimError::RepositoryNotFound {
                searched_path: start_path.to_path_buf(),
            });
        }
    }
}
