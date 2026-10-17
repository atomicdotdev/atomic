//! CB-11A: `atomic stage` and `atomic unstage` — explicit staging mutations.
//!
//! `atomic stage` stages worktree content into the primary Git index of a
//! colocated repository. `atomic unstage` moves the index entry back to the
//! Git baseline (HEAD) — or removes it when the baseline lacks the path —
//! without touching worktree bytes. Both are pure index operations: durable
//! Atomic tracking (`TREE`) is never read for mutation and never changed by
//! index movement (RFC §9.3).

use std::path::{Path, PathBuf};

use clap::Parser;
use git2::{IndexEntry, IndexTime, ObjectType, Oid, Repository as GitRepository};

use atomic_repository::{Repository, WorkspaceTxnMode};

use crate::commands::workspace_txn::enter_workspace;
use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{print_blank, print_hint, print_info, print_success};

/// Stage worktree content into the primary Git index (colocated mode).
///
/// Staging never decides what a future managed commit will contain; it only
/// selects content. Paths that are neither durably tracked nor recorded as
/// intent-to-add are refused: tracking decisions belong to `atomic add`.
#[derive(Parser, Debug)]
pub struct Stage {
    /// Paths to stage content for.
    #[arg(value_name = "PATH")]
    pub paths: Vec<String>,

    /// Show what would be staged without writing the index.
    #[arg(long)]
    pub dry_run: bool,
}

/// Move index entries back to the Git baseline without touching the worktree.
#[derive(Parser, Debug)]
pub struct Unstage {
    /// Paths to unstage.
    #[arg(value_name = "PATH")]
    pub paths: Vec<String>,

    /// Show what would be unstaged without writing the index.
    #[arg(long)]
    pub dry_run: bool,
}

impl Stage {
    pub fn new() -> Self {
        Self {
            paths: Vec::new(),
            dry_run: false,
        }
    }

    pub fn with_paths<I, S>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.paths = paths.into_iter().map(Into::into).collect();
        self
    }
}

impl Default for Stage {
    fn default() -> Self {
        Self::new()
    }
}

impl Unstage {
    pub fn new() -> Self {
        Self {
            paths: Vec::new(),
            dry_run: false,
        }
    }
}

impl Default for Unstage {
    fn default() -> Self {
        Self::new()
    }
}

/// A path selected for staging, resolved against the worktree and tracking state.
struct StageCandidate {
    /// Repository-relative raw path (forward slashes).
    raw: Vec<u8>,
    native: PathBuf,
}

fn colocated_git_repository(repo_root: &Path) -> CliResult<GitRepository> {
    if !repo_root.join(".git").exists() {
        return Err(CliError::InvalidArgument {
            message: "staging requires a colocated Git repository (no .git found); \
                      use `atomic record` without staging in non-colocated repositories"
                .to_string(),
        });
    }
    GitRepository::open(repo_root).map_err(|error| CliError::GitError {
        message: format!("cannot open Git repository: {error}"),
    })
}

/// Normalize a user path into raw repository-relative bytes (forward slashes).
fn normalize_repo_path(path: &str) -> CliResult<Vec<u8>> {
    let normalized = path.replace('\\', "/");
    let trimmed = normalized.trim_end_matches('/');
    if trimmed.is_empty() || trimmed.starts_with('/') || trimmed.split('/').any(|part| part == "..")
    {
        return Err(CliError::InvalidArgument {
            message: format!("invalid repository path '{path}'"),
        });
    }
    Ok(trimmed.as_bytes().to_vec())
}

fn resolve_candidates(repo: &Repository, paths: &[String]) -> CliResult<Vec<StageCandidate>> {
    let mut candidates = Vec::new();
    for path in paths {
        let native = repo.root().join(path);
        let metadata =
            std::fs::symlink_metadata(&native).map_err(|_| CliError::InvalidArgument {
                message: format!("path '{path}' does not exist in the worktree"),
            })?;
        if metadata.is_dir() {
            return Err(CliError::InvalidArgument {
                message: format!("path '{path}' is a directory; stage individual files"),
            });
        }
        candidates.push(StageCandidate {
            raw: normalize_repo_path(path)?,
            native,
        });
    }
    Ok(candidates)
}

fn index_entry_flags(raw: &[u8], index: &git2::Index) -> (bool, bool) {
    // Returns (intent_to_add, unmerged) for the path.
    let mut intent_to_add = false;
    let mut unmerged = false;
    for entry in index.iter() {
        if entry.path != raw {
            continue;
        }
        let stage = ((entry.flags >> 12) & 0x3) as u8;
        if stage != 0 {
            unmerged = true;
        }
        if entry.flags_extended & 0x2000 != 0 {
            intent_to_add = true;
        }
    }
    (intent_to_add, unmerged)
}

impl Command for Stage {
    fn run(&self) -> CliResult<()> {
        if self.paths.is_empty() {
            return Err(CliError::InvalidArgument {
                message: "stage requires at least one path".to_string(),
            });
        }
        let repo_root = find_repository_root()?;
        let mut repo = Repository::open_for_workspace_transaction(&repo_root).map_err(|e| {
            CliError::InvalidRepository {
                reason: e.to_string(),
            }
        })?;
        let workspace = enter_workspace(&mut repo, WorkspaceTxnMode::Reconcile)?;
        let working_copy = workspace.working_copy();

        let git = colocated_git_repository(&repo_root)?;
        let candidates = resolve_candidates(&repo, &self.paths)?;

        // Tracking gate: stage stages content, it does not decide tracking.
        // Durable tracking (`TREE`) is read for the gate and never mutated.
        let tracked_files = repo
            .list_tracked_files()
            .map_err(|e| CliError::Internal(e.into()))?;
        let durably_tracked: std::collections::BTreeSet<Vec<u8>> = tracked_files
            .iter()
            .filter(|file| !file.is_directory)
            .map(|file| file.path.display().to_string().into_bytes())
            .collect();

        let mut index = git.index().map_err(|error| CliError::GitError {
            message: format!("cannot read Git index: {error}"),
        })?;

        let mut staged = Vec::new();
        let mut refused = Vec::new();
        for candidate in candidates {
            let (intent_to_add, unmerged) = index_entry_flags(&candidate.raw, &index);
            if unmerged {
                return Err(CliError::InvalidArgument {
                    message: format!(
                        "path '{}' is unmerged in the Git index; finish or abort the Git operation in Git first",
                        String::from_utf8_lossy(&candidate.raw)
                    ),
                });
            }
            let tracked = durably_tracked.contains(&candidate.raw) || intent_to_add;
            if !tracked {
                refused.push(String::from_utf8_lossy(&candidate.raw).into_owned());
                continue;
            }
            if self.dry_run {
                staged.push(String::from_utf8_lossy(&candidate.raw).into_owned());
                continue;
            }
            let relative = path_relative_to_root(&candidate.native, repo.root())?;
            index
                .add_path(&relative)
                .map_err(|error| CliError::GitError {
                    message: format!(
                        "cannot stage '{}': {error}",
                        String::from_utf8_lossy(&candidate.raw)
                    ),
                })?;
            staged.push(String::from_utf8_lossy(&candidate.raw).into_owned());
        }

        if !refused.is_empty() {
            return Err(CliError::InvalidArgument {
                message: format!(
                    "refusing to stage untracked path(s) {}: content staging does not create tracking; run `atomic add` first",
                    refused.join(", ")
                ),
            });
        }

        if !self.dry_run {
            index.write().map_err(|error| CliError::GitError {
                message: format!("cannot write Git index: {error}"),
            })?;
        }

        print_success(&format!("staged {} path(s)", staged.len()));
        for path in &staged {
            print_info(&format!("A  {path}"));
        }
        print_blank();
        print_hint("staging selects content only; durable tracking is unchanged");
        Ok(())
    }
}

impl Command for Unstage {
    fn run(&self) -> CliResult<()> {
        if self.paths.is_empty() {
            return Err(CliError::InvalidArgument {
                message: "unstage requires at least one path".to_string(),
            });
        }
        let repo_root = find_repository_root()?;
        let mut repo = Repository::open_for_workspace_transaction(&repo_root).map_err(|e| {
            CliError::InvalidRepository {
                reason: e.to_string(),
            }
        })?;
        let workspace = enter_workspace(&mut repo, WorkspaceTxnMode::Reconcile)?;
        let _ = workspace;

        let git = colocated_git_repository(&repo_root)?;
        let candidates = resolve_candidates(&repo, &self.paths)?;

        let mut index = git.index().map_err(|error| CliError::GitError {
            message: format!("cannot read Git index: {error}"),
        })?;

        // The unstage baseline is the Git HEAD tree (or nothing when unborn).
        let baseline: std::collections::BTreeMap<Vec<u8>, (Oid, u32)> = match git.head() {
            Ok(head) => {
                let commit = head.peel_to_commit().map_err(|error| CliError::GitError {
                    message: format!("cannot read Git HEAD commit: {error}"),
                })?;
                let tree = commit.tree().map_err(|error| CliError::GitError {
                    message: format!("cannot read Git HEAD tree: {error}"),
                })?;
                let mut map = std::collections::BTreeMap::new();
                collect_tree_entries(&git, &tree, &mut Vec::new(), &mut map)?;
                map
            }
            Err(_) => std::collections::BTreeMap::new(),
        };

        let mut unstaged = Vec::new();
        for candidate in candidates {
            let (_, unmerged) = index_entry_flags(&candidate.raw, &index);
            if unmerged {
                return Err(CliError::InvalidArgument {
                    message: format!(
                        "path '{}' is unmerged in the Git index; finish or abort the Git operation in Git first",
                        String::from_utf8_lossy(&candidate.raw)
                    ),
                });
            }
            if self.dry_run {
                unstaged.push(String::from_utf8_lossy(&candidate.raw).into_owned());
                continue;
            }
            let relative = path_relative_to_root(&candidate.native, repo.root())?;
            match baseline.get(&candidate.raw) {
                Some((oid, mode)) => {
                    // Restore the index entry from the Git baseline.
                    let blob = git.find_blob(*oid).map_err(|error| CliError::GitError {
                        message: format!(
                            "cannot read baseline blob for '{}': {error}",
                            String::from_utf8_lossy(&candidate.raw)
                        ),
                    })?;
                    let entry = IndexEntry {
                        ctime: IndexTime::new(0, 0),
                        mtime: IndexTime::new(0, 0),
                        dev: 0,
                        ino: 0,
                        mode: *mode,
                        uid: 0,
                        gid: 0,
                        file_size: blob.content().len().min(u32::MAX as usize) as u32,
                        id: *oid,
                        flags: 0,
                        flags_extended: 0,
                        path: candidate.raw.clone(),
                    };
                    index
                        .add_frombuffer(&entry, blob.content())
                        .map_err(|error| CliError::GitError {
                            message: format!(
                                "cannot reset index entry '{}': {error}",
                                String::from_utf8_lossy(&candidate.raw)
                            ),
                        })?;
                }
                None => {
                    index.remove_path(Path::new(&relative)).map_err(|error| {
                        CliError::GitError {
                            message: format!(
                                "cannot remove index entry '{}': {error}",
                                String::from_utf8_lossy(&candidate.raw)
                            ),
                        }
                    })?;
                }
            }
            unstaged.push(String::from_utf8_lossy(&candidate.raw).into_owned());
        }

        if !self.dry_run {
            index.write().map_err(|error| CliError::GitError {
                message: format!("cannot write Git index: {error}"),
            })?;
        }

        print_success(&format!("unstaged {} path(s)", unstaged.len()));
        for path in &unstaged {
            print_info(&format!(" {path} (worktree bytes untouched)"));
        }
        print_blank();
        print_hint("unstaging moves the index to the Git baseline; durable tracking is unchanged");
        Ok(())
    }
}

fn path_relative_to_root(native: &Path, root: &Path) -> CliResult<PathBuf> {
    native
        .strip_prefix(root)
        .map(Path::to_path_buf)
        .map_err(|_| CliError::InvalidArgument {
            message: format!("path '{}' is outside the repository", native.display()),
        })
}

fn collect_tree_entries(
    repository: &GitRepository,
    tree: &git2::Tree<'_>,
    #[allow(clippy::ptr_arg)] // recursive tree walker shares the Vec
    prefix: &mut Vec<u8>,
    output: &mut std::collections::BTreeMap<Vec<u8>, (Oid, u32)>,
) -> CliResult<()> {
    for entry in tree.iter() {
        let mut path = prefix.clone();
        if !path.is_empty() {
            path.push(b'/');
        }
        path.extend_from_slice(entry.name_bytes());
        if entry.kind() == Some(ObjectType::Tree) {
            let child = repository
                .find_tree(entry.id())
                .map_err(|error| CliError::GitError {
                    message: format!("cannot read subtree {}: {error}", entry.id()),
                })?;
            collect_tree_entries(repository, &child, &mut path, output)?;
        } else {
            output.insert(path, (entry.id(), entry.filemode() as u32));
        }
    }
    Ok(())
}
