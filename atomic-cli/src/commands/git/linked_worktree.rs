//! Linked `git worktree` detection for the colocated shim.
//!
//! Atomic's advisory dispatchers are installed in Git's common directory, so
//! Git also runs them inside linked worktrees created with `git worktree
//! add`. Such a worktree has no Atomic working copy of its own and the shim
//! does not manage it yet: every `hook-*` entry point passes through after
//! one advisory line, and the commands that would otherwise register a second
//! working copy there (`git shim reconcile`, `status`) refuse with one clear
//! error naming the main worktree.

use std::fs;
use std::path::{Path, PathBuf};

use atomic_repository::REPOSITORY_POINTER;
use git2::Repository as GitRepository;

use super::observation::resolve_common_dir;
use crate::commands::DOT_DIR;
use crate::error::{CliError, CliResult};
use crate::output::print_warning;

/// Git's administrative directory name; the main worktree is its parent.
const GIT_DIR_NAME: &str = ".git";

/// Git runs this hook several times per operation, so it never prints the
/// pass-through advisory.
const SILENT_HOOK: &str = "reference-transaction";

/// A linked worktree and the main worktree that owns its Git common directory.
#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct LinkedWorktree {
    /// The linked worktree's own root.
    pub(crate) root: PathBuf,
    /// The main worktree the shim manages.
    pub(crate) main_root: PathBuf,
}

/// Detect whether `path` lies inside a linked `git worktree`.
///
/// Returns `None` for the main worktree, for a bare repository, and for any
/// path outside Git. Detection compares the per-worktree Git directory with
/// the shared common directory (`git rev-parse --git-dir` versus
/// `--git-common-dir`), both resolved through libgit2 and the `commondir`
/// pointer file.
pub(crate) fn detect_linked_worktree(path: &Path) -> Option<LinkedWorktree> {
    let repository = GitRepository::discover(path).ok()?;
    let git_dir = canonical_or_raw(repository.path());
    let common_dir = canonical_or_raw(&resolve_common_dir(&git_dir).ok()?);
    if git_dir == common_dir {
        return None;
    }
    let root = repository
        .workdir()
        .map(canonical_or_raw)
        .unwrap_or_else(|| path.to_path_buf());
    Some(LinkedWorktree {
        root,
        main_root: main_worktree_root(&common_dir),
    })
}

/// A linked worktree without an Atomic repository of its own. The shim cannot
/// reconcile such a worktree: its import path would initialize a second
/// repository over the common one. A linked worktree that was initialized as
/// its own repository resolves to that repository and is handled like any
/// other.
fn linked_worktree_without_repository(path: &Path) -> Option<LinkedWorktree> {
    let linked = detect_linked_worktree(path)?;
    if atomic_repository::has_database(&linked.root.join(DOT_DIR)) {
        return None;
    }
    Some(linked)
}

/// A linked worktree the shim does not manage at all: no repository of its
/// own and no `.atomic/repository` pointer either. The pointer marks a linked
/// working copy registered against the common repository, which native
/// commands and the hooks handle like any other working copy.
pub(crate) fn detect_unmanaged_linked_worktree(path: &Path) -> Option<LinkedWorktree> {
    let linked = linked_worktree_without_repository(path)?;
    if linked.root.join(DOT_DIR).join(REPOSITORY_POINTER).is_file() {
        return None;
    }
    Some(linked)
}

/// Hook entry points: inside an unmanaged linked worktree the dispatcher
/// passes through (returns `true`) after one advisory line on stderr.
/// Detection starts at the current directory because Git runs hooks from
/// the worktree root.
pub(crate) fn hook_passes_through(hook_name: &str) -> bool {
    let Ok(cwd) = std::env::current_dir() else {
        return false;
    };
    let Some(linked) = detect_unmanaged_linked_worktree(&cwd) else {
        return false;
    };
    if hook_name != SILENT_HOOK {
        print_warning(&format!(
            "Atomic shim: linked worktree '{}' is not managed; hooks pass through",
            linked.root.display()
        ));
    }
    true
}

/// `git shim` commands refuse inside an unmanaged linked worktree: one with
/// neither a repository of its own nor the `.atomic/repository` pointer.
/// A registered linked worktree (the sandbox-style pointer, CB-13D) adopts
/// its own ephemeral view and reconciles like any working copy.
pub(crate) fn refuse_shim_command(path: &Path) -> CliResult<()> {
    match detect_unmanaged_linked_worktree(path) {
        Some(linked) => Err(refusal(&linked)),
        None => Ok(()),
    }
}

/// Native commands that reconcile through the shim refuse inside a linked
/// worktree only when the main worktree is shim-enrolled; a repository that
/// never enabled the shim keeps its native linked-worktree support.
pub(crate) fn refuse_when_shim_enrolled(path: &Path) -> CliResult<()> {
    let Some(linked) = detect_unmanaged_linked_worktree(path) else {
        return Ok(());
    };
    if main_worktree_shim_enrolled(&linked.main_root)? {
        return Err(refusal(&linked));
    }
    Ok(())
}

/// `refuse_when_shim_enrolled` for the current directory, before any
/// repository constructor can write into the linked worktree.
pub(crate) fn refuse_when_shim_enrolled_from_cwd() -> CliResult<()> {
    let cwd = std::env::current_dir().map_err(CliError::Io)?;
    refuse_when_shim_enrolled(&cwd)
}

/// Whether the main worktree's repository is enrolled in the shim (a verified
/// checkpoint or recorded consent), read through a read-only open of the
/// MAIN root so nothing is written into the linked worktree.
fn main_worktree_shim_enrolled(main_root: &Path) -> CliResult<bool> {
    if !atomic_repository::has_database(&main_root.join(DOT_DIR)) {
        return Ok(false);
    }
    let repository = atomic_repository::Repository::open_readonly_wait(
        main_root,
        atomic_repository::database_lock_wait(),
    )
    .map_err(CliError::Repository)?;
    repository
        .shim_workspace_active()
        .map_err(CliError::Repository)
}

fn main_worktree_root(common_dir: &Path) -> PathBuf {
    match (common_dir.file_name(), common_dir.parent()) {
        (Some(name), Some(parent)) if name == GIT_DIR_NAME => parent.to_path_buf(),
        _ => common_dir.to_path_buf(),
    }
}

fn canonical_or_raw(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn refusal(linked: &LinkedWorktree) -> CliError {
    CliError::GitError {
        message: format!(
            "linked git worktrees are not supported by the shim yet; run from the main worktree '{}'",
            linked.main_root.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_AUTHOR_NAME", "Linked Worktree Tests")
            .env("GIT_AUTHOR_EMAIL", "linked@example.com")
            .env("GIT_COMMITTER_NAME", "Linked Worktree Tests")
            .env("GIT_COMMITTER_EMAIL", "linked@example.com")
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// A committed main worktree plus one linked worktree on its own branch.
    fn repository_with_linked_worktree() -> (TempDir, TempDir) {
        let main = TempDir::new().unwrap();
        git(main.path(), &["init", "-q", "-b", "main"]);
        fs::write(main.path().join("f.txt"), b"f\n").unwrap();
        git(main.path(), &["add", "-A"]);
        git(main.path(), &["commit", "-qm", "base"]);
        let parent = TempDir::new().unwrap();
        let linked = parent.path().join("linked");
        git(
            main.path(),
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "linked",
                linked.to_str().unwrap(),
            ],
        );
        (main, parent)
    }

    #[test]
    fn a_linked_worktree_with_its_own_repository_is_not_unmanaged() {
        let (_main, parent) = repository_with_linked_worktree();
        let linked = parent.path().join("linked");
        assert!(detect_unmanaged_linked_worktree(&linked).is_some());
        drop(atomic_repository::Repository::init(&linked).unwrap());
        assert_eq!(detect_unmanaged_linked_worktree(&linked), None);
        refuse_shim_command(&linked).expect("an independent repository is not refused");
    }

    #[test]
    fn a_registered_linked_worktree_is_managed() {
        let (_main, parent) = repository_with_linked_worktree();
        let linked = parent.path().join("linked");
        let dot_dir = linked.join(DOT_DIR);
        fs::create_dir_all(&dot_dir).unwrap();
        fs::write(dot_dir.join(REPOSITORY_POINTER), b"/elsewhere/.atomic").unwrap();
        assert_eq!(detect_unmanaged_linked_worktree(&linked), None);
        refuse_when_shim_enrolled(&linked).expect("a registered working copy is handled natively");
        refuse_shim_command(&linked).expect("a registered linked worktree reconciles natively");
    }

    #[test]
    fn main_worktree_and_non_git_paths_are_not_linked() {
        let (main, _parent) = repository_with_linked_worktree();
        assert_eq!(detect_linked_worktree(main.path()), None);
        let plain = TempDir::new().unwrap();
        assert_eq!(detect_linked_worktree(plain.path()), None);
    }

    #[test]
    fn linked_worktree_resolves_its_main_worktree() {
        let (main, parent) = repository_with_linked_worktree();
        let linked = parent.path().join("linked");
        let detected = detect_linked_worktree(&linked.join("sub-dir-does-not-exist"))
            .or_else(|| detect_linked_worktree(&linked))
            .expect("a linked worktree is detected");
        assert_eq!(detected.root, fs::canonicalize(&linked).unwrap());
        assert_eq!(detected.main_root, fs::canonicalize(main.path()).unwrap());
    }

    #[test]
    fn shim_commands_refuse_only_inside_the_linked_worktree() {
        let (main, parent) = repository_with_linked_worktree();
        refuse_shim_command(main.path()).expect("the main worktree is managed");
        let error = refuse_shim_command(&parent.path().join("linked")).unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("linked git worktrees are not supported by the shim yet")
                && message.contains(&format!(
                    "run from the main worktree '{}'",
                    fs::canonicalize(main.path()).unwrap().display()
                )),
            "got: {message}"
        );
    }

    #[test]
    fn a_repository_that_never_enabled_the_shim_keeps_native_linked_worktrees() {
        let (_main, parent) = repository_with_linked_worktree();
        refuse_when_shim_enrolled(&parent.path().join("linked"))
            .expect("no Atomic repository means no shim enrollment");
    }
}
