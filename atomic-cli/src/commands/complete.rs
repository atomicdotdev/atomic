//! Dynamic shell-completion helpers.
//!
//! These back the `clap_complete` dynamic engine (see `main.rs`'s
//! `CompleteEnv`). Commands attach them to individual arguments with
//! `#[arg(add = ArgValueCompleter::new(...))]` so that, at completion time,
//! the binary offers live values from the repository — view names and change
//! hashes — instead of nothing.
//!
//! The pure `*_candidates` functions take a `&Repository` and are unit-tested
//! directly. The `complete_*` wrappers open the repository from the current
//! working directory and return an empty list (never an error) when there is
//! no repository there, so completion is always safe to invoke.

use std::ffi::OsStr;

use clap_complete::engine::CompletionCandidate;

use atomic_core::types::Base32;
use atomic_repository::Repository;

use crate::commands::require_repository;

/// Maximum number of change-hash completion candidates to return.
///
/// Bounds the per-keystroke cost (we load a header per candidate) on repos
/// with large histories. Users typically type a hash prefix, which filters
/// the set down well before this cap.
const MAX_CHANGE_CANDIDATES: usize = 50;

/// View-name completion candidates matching `prefix`, sorted.
///
/// Pure over the repository so it can be unit-tested without the shell
/// completion machinery.
pub(crate) fn view_name_candidates(repo: &Repository, prefix: &str) -> Vec<String> {
    let mut names = match repo.list_views() {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    names.retain(|n| n.starts_with(prefix));
    names.sort();
    names
}

/// Change-hash completion candidates drawn from the change store.
///
/// Commands that address a change by hash can reference *any* change that
/// exists in the repo, so the candidate source is the change store rather than
/// a single view's log. Returns `(base32_hash, optional_message)` pairs
/// filtered by `prefix` and capped at [`MAX_CHANGE_CANDIDATES`]. Pure over the
/// repository for unit testing.
pub(crate) fn change_hash_candidates(
    repo: &Repository,
    prefix: &str,
) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    for result in repo.iter_changes() {
        let Ok(hash) = result else { continue };
        let b32 = hash.to_base32();
        if !prefix.is_empty() && !b32.starts_with(prefix) {
            continue;
        }
        let message = repo
            .load_change(&hash)
            .ok()
            .map(|c| c.hashed.header.message.clone());
        out.push((b32, message));
        if out.len() >= MAX_CHANGE_CANDIDATES {
            break;
        }
    }
    out
}

/// Dynamic completer for view-name arguments.
///
/// Runs in the user's CWD during completion; if there is no repository there
/// it returns no candidates rather than erroring.
pub(crate) fn complete_view_names(current: &OsStr) -> Vec<CompletionCandidate> {
    let prefix = current.to_string_lossy();
    let Ok(repo) = require_repository(None) else {
        return Vec::new();
    };
    view_name_candidates(&repo, &prefix)
        .into_iter()
        .map(CompletionCandidate::new)
        .collect()
}

/// Dynamic completer for change-hash arguments, annotating each hash with its
/// commit message as the completion description.
pub(crate) fn complete_change_hashes(current: &OsStr) -> Vec<CompletionCandidate> {
    let prefix = current.to_string_lossy();
    let Ok(repo) = require_repository(None) else {
        return Vec::new();
    };
    change_hash_candidates(&repo, &prefix)
        .into_iter()
        .map(|(hash, msg)| {
            let cand = CompletionCandidate::new(hash);
            match msg {
                Some(m) => cand.help(Some(m.into())),
                None => cand,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_name_candidates_filter_and_sort() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();

        // A freshly initialised repo has the default `dev` view.
        let all = view_name_candidates(&repo, "");
        assert!(
            all.iter().any(|v| v == "dev"),
            "expected default view: {all:?}"
        );

        // Prefix filtering.
        let d = view_name_candidates(&repo, "d");
        assert!(d.iter().all(|v| v.starts_with('d')));
        assert!(d.iter().any(|v| v == "dev"));

        // Non-matching prefix yields nothing.
        assert!(view_name_candidates(&repo, "zzz").is_empty());
    }

    #[test]
    fn change_hash_candidates_empty_repo_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::init(tmp.path()).unwrap();
        // No changes recorded yet -> no candidates, and no error/panic.
        assert!(change_hash_candidates(&repo, "").is_empty());
    }
}
