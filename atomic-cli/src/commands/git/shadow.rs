//! The shadow-commit pipeline — the single path that stages a shadow commit and
//! runs the pre-commit Validator before a git tree is produced.
//!
//! Per `SPEC-single-materializer-validator.md` (§5), exactly one code path may
//! stage the shadow working copy and hand a candidate to the Validator.
//! `atomic git push` and the turn-end hook both go through
//! [`stage_and_validate_tree`], so no other path independently
//! `git add -A`/`write_tree`s the tree. Any Validator rule failure aborts
//! atomically: the git index is restored from HEAD and nothing is committed.
//!
//! Validator rules enforced here (pre-commit):
//! - **V1** — no unresolved conflict markers (shares `record`'s detector).
//! - **V4** — no git-excluded provenance path (`.atomic/`, `.vault/`,
//!   `.atomicignore`) is ever staged.
//!
//! V2 (tree↔view coherence) and V3 (git↔state agreement) are added here in later
//! phases, ahead of `write_tree`, so every shadow commit passes the same gate.

use std::io::IsTerminal;
use std::path::Path;

use git2::Repository as GitRepository;

use atomic_repository::Repository;

use crate::error::{CliError, CliResult};
use crate::output::{print_info, print_warning};

/// After an `atomic view switch`, point the git shadow's HEAD at the branch that
/// mirrors the new view (Direction A of §5.4 — "git shadows Atomic"). Atomic is
/// upstream, so it drives the downstream git shadow's branch selection.
///
/// This is a **ref move**, never a `git checkout`: it updates HEAD (and realigns
/// the index to the new branch's tree) but never re-renders the working copy,
/// which Atomic just materialized. It is **best-effort** — any git error is a
/// warning, never a failure of the view switch — and a **no-op** outside a
/// shadow-sync repo or when HEAD is already on the branch.
pub(crate) fn sync_git_head_to_view(repo_root: &Path, view: &str) {
    let git_repo = match GitRepository::discover(repo_root) {
        Ok(r) => r,
        Err(_) => return, // not a git repo — nothing to shadow
    };
    // Only touch git in repos where shadow sync is actually established.
    if !shadow_sync_active(&git_repo) {
        return;
    }

    let branch_ref = format!("refs/heads/{}", view);

    // Idempotent: already on the mirror branch (no loop, no churn).
    if git_repo
        .head()
        .ok()
        .and_then(|h| h.name().map(str::to_owned))
        .as_deref()
        == Some(branch_ref.as_str())
    {
        return;
    }

    // Create the mirror branch at the current commit if it doesn't exist yet
    // (a new draft view branches from wherever HEAD currently points).
    if git_repo.find_reference(&branch_ref).is_err() {
        match git_repo.head().and_then(|h| h.peel_to_commit()) {
            Ok(commit) => {
                if let Err(e) = git_repo.branch(view, &commit, false) {
                    print_warning(&format!(
                        "Could not create git branch '{}' to mirror the view: {}",
                        view, e
                    ));
                    return;
                }
            }
            // Unborn HEAD / no commits yet — nothing to anchor a branch to.
            Err(_) => return,
        }
    }

    // Move HEAD to the mirror branch WITHOUT touching the working copy, then
    // realign the index to the new branch tip so `git status` cleanly shows the
    // view's content as the delta the next shadow push will commit.
    if let Err(e) = git_repo.set_head(&branch_ref) {
        print_warning(&format!(
            "Could not point git HEAD at branch '{}': {}",
            view, e
        ));
        return;
    }
    restore_index_from_head(&git_repo);
    print_info(&format!("git shadow now tracks branch '{}'.", view));
}

/// Whether git shadow sync is established for this repo (the `.git/info/exclude`
/// carries Atomic's shadow patterns, written by import/push). Used to gate the
/// view-switch git-follow so we never touch HEAD in a plain (non-shadow) repo.
fn shadow_sync_active(git_repo: &GitRepository) -> bool {
    let exclude = git_repo.path().join("info").join("exclude");
    std::fs::read_to_string(exclude)
        .map(|c| c.lines().any(|l| l.trim() == "/.atomic/"))
        .unwrap_or(false)
}

/// Acquire the repo-scoped shadow-commit lock, or return `None` (a no-op skip)
/// if a shadow materialize/commit is already in flight (SPEC §4.3 / Principle 5).
///
/// Non-blocking: rather than queueing (which would hang a turn-end hook), the
/// contended case is a logged no-op — the in-flight operation owns this commit.
/// The returned guard must be held for the whole stage → validate → commit
/// sequence; dropping it releases the lock. Acquire it **outermost**, before any
/// staging or DB write.
pub(crate) fn acquire_shadow_lock(
    repo: &Repository,
    repo_root: &Path,
    view: &str,
) -> CliResult<Option<std::fs::File>> {
    match repo
        .try_lock_shadow_commit()
        .map_err(CliError::Repository)?
    {
        Some(guard) => Ok(Some(guard)),
        None => {
            if std::io::stderr().is_terminal() {
                print_info("Another shadow materialize is in flight; skipping this push.");
            } else {
                append_shadow_log(
                    repo_root,
                    "shadow-lock:contended",
                    view,
                    "another shadow materialize in flight",
                );
            }
            Ok(None)
        }
    }
}

/// Stage the current working copy for a shadow commit, run the pre-commit
/// Validator, and return the candidate git tree OID.
///
/// This is the sole shadow-commit staging path (SPEC §5.2). On any Validator
/// failure it aborts atomically — the index is restored from HEAD so git is left
/// byte-identical — and returns an error naming the failing rule.
pub(crate) fn stage_and_validate_tree(
    repo: &Repository,
    git_repo: &GitRepository,
    repo_root: &Path,
    view: &str,
    allow_conflict_markers: bool,
) -> CliResult<git2::Oid> {
    // ── Rule V1 — no unresolved conflict markers ────────────────────────────
    // Shares `atomic record`'s detector so the two paths cannot disagree.
    if !allow_conflict_markers {
        if let Some((path, line)) = repo
            .first_working_copy_conflict_marker()
            .map_err(CliError::Repository)?
        {
            if !std::io::stderr().is_terminal() {
                append_shadow_validate_log(
                    repo_root,
                    "V1",
                    view,
                    &format!("file={} line={}", path, line),
                );
            }
            print_warning(&format!(
                "Refusing to commit '{}': unresolved conflict marker at line {}.",
                path, line
            ));
            return Err(CliError::GitError {
                message: format!(
                    "'{}' still contains conflict markers at line {} — resolve the \
                     conflict (remove the >>>>>>> / ======= / <<<<<<< lines), or pass \
                     --allow-conflict-markers to override. No commit was created.",
                    path, line
                ),
            });
        }
    }

    // Prevention: make sure git is configured to exclude Atomic's shadow /
    // provenance paths before staging, so `git add -A` never picks them up.
    // Best-effort (an unwritable .git/info is caught by the V4 guard below).
    let _ = super::import::ensure_git_shadow_excludes(git_repo.path());

    // Stage everything: git add -A (add_all + update_all handles new files and
    // deletions).
    let mut index = git_repo.index().map_err(|e| CliError::GitError {
        message: format!("Failed to open git index: {}", e),
    })?;
    index
        .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
        .map_err(|e| CliError::GitError {
            message: format!("Failed to stage files: {}", e),
        })?;
    index
        .update_all(["*"].iter(), None)
        .map_err(|e| CliError::GitError {
            message: format!("Failed to update index: {}", e),
        })?;
    index.write().map_err(|e| CliError::GitError {
        message: format!("Failed to write index: {}", e),
    })?;

    // ── Rule V4 — no provenance / excluded path may be staged ───────────────
    if let Some(bad) = first_forbidden_shadow_path(&index) {
        restore_index_from_head(git_repo);
        if !std::io::stderr().is_terminal() {
            append_shadow_validate_log(repo_root, "V4", view, &format!("path={}", bad));
        }
        print_warning(&format!(
            "Refusing to shadow-commit: provenance/excluded path '{}' was staged.",
            bad
        ));
        return Err(CliError::GitError {
            message: format!(
                "'{}' is a git-excluded Atomic shadow path (.atomic/, .vault/, \
                 .atomicignore) and must never be committed to git. Aborting; no \
                 commit was created and the index was restored. Ensure \
                 `.git/info/exclude` carries the Atomic shadow patterns.",
                bad
            ),
        });
    }

    let tree_oid = index.write_tree().map_err(|e| CliError::GitError {
        message: format!("Failed to write tree: {}", e),
    })?;

    // ── Rule V2 — tree ↔ view coherence (SPEC §6.2) ─────────────────────────
    // The staged tree must correspond to what the current view materializes.
    // Cost-safe / incremental: only paths that differ between the candidate
    // tree and git HEAD are checked, each against the view's recorded content.
    if let Some((path, reason)) = first_incoherent_path(repo, git_repo, tree_oid, view)? {
        restore_index_from_head(git_repo);
        if !std::io::stderr().is_terminal() {
            append_shadow_validate_log(
                repo_root,
                "V2",
                view,
                &format!("path={} reason={}", path, reason),
            );
        }
        print_warning(&format!(
            "Refusing to shadow-commit: '{}' {} (SPEC V2).",
            path, reason
        ));
        return Err(CliError::GitError {
            message: format!(
                "'{}' {} — the working copy diverges from the current view '{}'. Record \
                 your changes (or reconcile the view) so the shadow tree matches the \
                 recorded state. No commit was created.",
                path, reason, view
            ),
        });
    }

    Ok(tree_oid)
}

/// Return the first changed path whose staged content does not correspond to the
/// current view's recorded content, as `(path, reason)`, or `None` if the
/// candidate tree is coherent with the view (Rule V2, SPEC §6.2).
///
/// Only paths that differ between the candidate tree and git HEAD are examined
/// (the incremental form), so the check costs one `get_file_content_on_view` per
/// changed path rather than a full-view materialize. Provenance / excluded paths
/// are skipped — Rule V4 owns them.
fn first_incoherent_path(
    repo: &Repository,
    git_repo: &GitRepository,
    candidate_tree_oid: git2::Oid,
    view: &str,
) -> CliResult<Option<(String, String)>> {
    let candidate_tree =
        git_repo
            .find_tree(candidate_tree_oid)
            .map_err(|e| CliError::GitError {
                message: format!("Failed to load candidate tree: {}", e),
            })?;
    let head_tree = git_repo
        .head()
        .ok()
        .and_then(|h| h.peel_to_commit().ok())
        .and_then(|c| c.tree().ok());

    let diff = git_repo
        .diff_tree_to_tree(head_tree.as_ref(), Some(&candidate_tree), None)
        .map_err(|e| CliError::GitError {
            message: format!("Failed to diff candidate tree: {}", e),
        })?;

    for delta in diff.deltas() {
        let (path, in_candidate) = match delta.status() {
            git2::Delta::Deleted => match delta.old_file().path().and_then(|p| p.to_str()) {
                Some(p) => (p.to_string(), false),
                None => continue,
            },
            _ => match delta.new_file().path().and_then(|p| p.to_str()) {
                Some(p) => (p.to_string(), true),
                None => continue,
            },
        };

        // Rule V4 owns provenance / git-excluded paths; V2 ignores them.
        if is_forbidden_shadow_path(&path) {
            continue;
        }

        let view_content = repo
            .get_file_content_on_view(&path, view)
            .map_err(CliError::Repository)?;

        if in_candidate {
            // The staged blob must equal what the view materializes for this path.
            let staged = git_repo.find_blob(delta.new_file().id()).ok();
            match (
                staged.as_ref().map(|b| b.content()),
                view_content.as_deref(),
            ) {
                (Some(s), Some(v)) if s == v => {}
                (Some(_), Some(_)) => {
                    return Ok(Some((
                        path,
                        "staged content differs from the view's recorded content".to_string(),
                    )));
                }
                (Some(_), None) => {
                    return Ok(Some((
                        path,
                        "is not recorded by the view (record it first)".to_string(),
                    )));
                }
                // Non-blob entries (submodules/symlinks) carry no textual
                // content to reconcile; leave them to git's own handling.
                (None, _) => {}
            }
        } else if view_content.is_some() {
            // The path was dropped from the tree, but the view still records it:
            // the candidate omits a change the view accounts for.
            return Ok(Some((
                path,
                "is still recorded by the view but missing from the tree".to_string(),
            )));
        }
    }

    Ok(None)
}

/// Append a `shadow-validate:<rule>` entry to `.atomic/hook-errors.log` (SPEC
/// §6.5) so a non-interactive shadow push that a Validator rule aborts leaves a
/// durable, greppable trail instead of failing silently.
pub(crate) fn append_shadow_validate_log(repo_root: &Path, rule: &str, view: &str, detail: &str) {
    append_shadow_log(
        repo_root,
        &format!("shadow-validate:{}", rule),
        view,
        detail,
    );
}

/// Append one tagged `.atomic/hook-errors.log` line. Best-effort: log I/O errors
/// are ignored (the operation already surfaces its own outcome).
fn append_shadow_log(repo_root: &Path, tag: &str, view: &str, detail: &str) {
    use std::io::Write;
    let log_path = repo_root.join(".atomic").join("hook-errors.log");
    let entry = format!(
        "{} {} view={} {}\n",
        chrono::Utc::now().to_rfc3339(),
        tag,
        view,
        detail
    );
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .and_then(|mut f| f.write_all(entry.as_bytes()));
}

/// Return the first staged index path that is a git-excluded shadow / provenance
/// path (`.atomic/`, `.vault/`, or `.atomicignore`), or `None` if the candidate
/// is clean. Validator Rule V4 (SPEC §6.4): these paths must never enter a git
/// commit — `.vault` (intents/memories/attestations) and `.atomic` (the change
/// graph) are git-excluded and unbacked; committing or reconciling them risks
/// the provenance layer.
fn first_forbidden_shadow_path(index: &git2::Index) -> Option<String> {
    index.iter().find_map(|entry| {
        let path = String::from_utf8_lossy(&entry.path).into_owned();
        is_forbidden_shadow_path(&path).then_some(path)
    })
}

/// Whether `path` (a repo-relative git path) is a git-excluded Atomic shadow /
/// provenance path that Rule V4 forbids from any shadow commit.
fn is_forbidden_shadow_path(path: &str) -> bool {
    path == ".atomicignore" || path.starts_with(".atomic/") || path.starts_with(".vault/")
}

/// Discard a candidate staging by restoring the git index from HEAD's tree,
/// leaving git byte-identical to its pre-operation state (the working copy is
/// never touched). Best-effort.
fn restore_index_from_head(git_repo: &GitRepository) {
    if let Ok(tree) = git_repo
        .head()
        .and_then(|h| h.peel_to_commit())
        .and_then(|c| c.tree())
    {
        if let Ok(mut index) = git_repo.index() {
            let _ = index.read_tree(&tree);
            let _ = index.write();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_forbidden_shadow_path as forbidden;

    #[test]
    fn forbids_provenance_and_excluded_paths() {
        assert!(forbidden(".atomicignore"));
        assert!(forbidden(".atomic/pristine.redb"));
        assert!(forbidden(".vault/intents/foo.md"));
    }

    #[test]
    fn allows_ordinary_source_paths() {
        assert!(!forbidden("src/main.rs"));
        assert!(!forbidden("README.md"));
        // A file that merely *contains* the substring is not forbidden.
        assert!(!forbidden("docs/.atomicignore.md"));
        assert!(!forbidden("my.vault/keep.txt"));
    }
}
