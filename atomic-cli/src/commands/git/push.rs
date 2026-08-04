//! The `atomic git push` command — sync Atomic changes to Git.
//!
//! Materializes the current view's working copy, stages all changes with
//! `git add -A`, commits with Atomic provenance trailers, and optionally
//! pushes to the remote.

use clap::Args;
use git2::Repository as GitRepository;

use atomic_core::types::{Base32, Merkle};
use atomic_repository::{HistoryEntry, HistoryOptions, Repository};

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{print_info, print_success, print_warning};

/// Push Atomic changes to Git.
///
/// Creates a Git commit from the current Atomic view's working copy state,
/// with trailers linking back to the Atomic changes. Optionally pushes to
/// the Git remote.
///
/// # Examples
///
/// ```text
/// # Commit and push current view to Git
/// atomic git push
///
/// # Commit with a custom message
/// atomic git push -m "feat: add authentication"
///
/// # Commit without pushing to remote
/// atomic git push --no-push
/// ```
#[derive(Debug, Args)]
pub struct Push {
    /// Custom commit message. If not provided, synthesizes from
    /// Atomic change messages.
    #[arg(short = 'm', long = "message")]
    pub message: Option<String>,

    /// Don't push to remote after committing.
    #[arg(long = "no-push")]
    pub no_push: bool,

    /// Remote name to push to.
    #[arg(long, default_value = "origin")]
    pub remote: String,
}

impl Default for Push {
    fn default() -> Self {
        Self {
            message: None,
            no_push: false,
            remote: "origin".to_string(),
        }
    }
}

impl Command for Push {
    fn run(&self) -> CliResult<()> {
        // Find and open the Atomic repository
        let repo_root = find_repository_root()?;
        let repo = Repository::open(&repo_root).map_err(CliError::Repository)?;

        // Open the Git repository
        let git_repo = GitRepository::discover(&repo_root).map_err(|_| CliError::GitError {
            message: "Not a git repository (or any parent up to mount point)".to_string(),
        })?;

        // Get current view name for trailers
        let current_view = repo.current_view().to_string();

        // Load change history for commit message + trailers
        let history = repo
            .log(HistoryOptions::default().load_headers(true))
            .map_err(CliError::Repository)?;

        if history.is_empty() {
            print_info("No changes recorded in the current view. Nothing to push.");
            return Ok(());
        }

        // Determine which changes are new since the last `atomic git push`.
        // We walk git history to find the most recent commit whose
        // Atomic-View trailer matches the current view, then use its
        // Atomic-State to locate our position in the view's history.
        let last_pushed_state = self.find_last_pushed_state(&git_repo, &current_view);
        let start_idx = match &last_pushed_state {
            Some(state) => history
                .iter()
                .position(|e| &e.state == state)
                .map(|i| i + 1)
                .unwrap_or(0),
            None => 0,
        };
        let new_history = &history[start_idx..];
        let new_count = new_history.len();

        // Stage everything: git add -A (add_all + update_all handles new files and deletions)
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

        let tree_oid = index.write_tree().map_err(|e| CliError::GitError {
            message: format!("Failed to write tree: {}", e),
        })?;
        let tree = git_repo
            .find_tree(tree_oid)
            .map_err(|e| CliError::GitError {
                message: format!("Failed to find tree: {}", e),
            })?;

        // Check if tree differs from HEAD — skip commit if nothing changed
        let head_tree = git_repo
            .head()
            .ok()
            .and_then(|h| h.peel_to_commit().ok())
            .and_then(|c| c.tree().ok());

        if let Some(ref ht) = head_tree {
            let diff = git_repo
                .diff_tree_to_tree(Some(ht), Some(&tree), None)
                .map_err(|e| CliError::GitError {
                    message: format!("Failed to diff: {}", e),
                })?;
            if diff.deltas().count() == 0 {
                // Nothing to commit — but a previous push may have failed
                // after the commit was created, leaving unpushed commits.
                let branch_name = git_repo
                    .head()
                    .ok()
                    .and_then(|h| h.shorthand().map(str::to_owned))
                    .unwrap_or_else(|| "HEAD".to_string());
                let ahead = self.unpushed_commit_count(&git_repo, &branch_name);
                if ahead == 0 {
                    print_info("Working copy matches Git HEAD. Nothing to commit.");
                    return Ok(());
                }
                if self.no_push {
                    print_info(&format!(
                        "Nothing to commit, but {} unpushed commit{} (--no-push set).",
                        ahead,
                        if ahead == 1 { "" } else { "s" }
                    ));
                    return Ok(());
                }
                print_info(&format!(
                    "Nothing to commit, but {} unpushed commit{}. Pushing...",
                    ahead,
                    if ahead == 1 { "" } else { "s" }
                ));
                self.push_to_remote(&git_repo)?;
                print_success(&format!("Pushed to {}/{}", self.remote, branch_name));
                return Ok(());
            }
        }

        // Build commit message with Atomic provenance trailers.
        // Only the changes new since the last push are included in the body
        // and the Atomic-Changes trailer. The Atomic-State trailer always
        // reflects the latest view state so the next push can find this point.
        let latest_state = history.last().map(|e| e.state).unwrap_or_default();
        let commit_message =
            self.build_commit_message(new_history, &latest_state, &current_view)?;

        // Get git signature
        let sig = git_repo.signature().map_err(|e| CliError::GitError {
            message: format!("Failed to get git signature: {}", e),
        })?;

        // Get parent commit (HEAD), if any
        let parent = git_repo.head().ok().and_then(|h| h.peel_to_commit().ok());
        let parents: Vec<&git2::Commit> = parent.as_ref().map(|p| vec![p]).unwrap_or_default();

        // Create the commit
        let commit_oid = git_repo
            .commit(Some("HEAD"), &sig, &sig, &commit_message, &tree, &parents)
            .map_err(|e| CliError::GitError {
                message: format!("Failed to create commit: {}", e),
            })?;

        let short_oid = &commit_oid.to_string()[..8];
        if new_count > 0 {
            print_success(&format!(
                "Created git commit {} on view '{}' ({} new change{})",
                short_oid,
                current_view,
                new_count,
                if new_count == 1 { "" } else { "s" },
            ));
        } else {
            print_success(&format!(
                "Created git commit {} on view '{}' (working copy changes)",
                short_oid, current_view,
            ));
        }

        // Push to remote unless --no-push
        if !self.no_push {
            match self.push_to_remote(&git_repo) {
                Ok(()) => {
                    print_success(&format!("Pushed to {}/{}", self.remote, current_view));
                }
                Err(e) => {
                    // Keep the local commit, but report the failed push.
                    print_warning("Commit created locally. Run 'git push' manually to retry.");
                    return Err(e);
                }
            }
        }

        Ok(())
    }
}

impl Push {
    /// Build a commit message with Atomic provenance trailers.
    ///
    /// Only the changes in `new_history` (those new since the last push) are
    /// referenced in the body and the `Atomic-Changes` trailer. The
    /// `Atomic-State` trailer always carries the latest view state so the
    /// next push can find this commit as its starting point.
    fn build_commit_message(
        &self,
        new_history: &[HistoryEntry],
        latest_state: &Merkle,
        view: &str,
    ) -> CliResult<String> {
        // Use custom message, synthesize from new change messages, or fall
        // back to a generic label when there are no new atomic changes (e.g.
        // manual working-copy edits).
        let body = if let Some(ref msg) = self.message {
            msg.clone()
        } else if new_history.is_empty() {
            "Working copy changes".to_string()
        } else {
            self.synthesize_message(new_history)
        };

        let mut msg = body;
        msg.push_str("\n\n");
        msg.push_str(&format!("Atomic-View: {}\n", view));
        msg.push_str(&format!("Atomic-State: {}\n", latest_state.to_base32()));

        let change_hashes: Vec<String> = new_history.iter().map(|e| e.hash.to_base32()).collect();

        if !change_hashes.is_empty() {
            msg.push_str(&format!("Atomic-Changes: {}\n", change_hashes.join(", ")));
        }

        Ok(msg)
    }

    /// Synthesize a commit message from the new Atomic change messages.
    ///
    /// The change headers are already loaded (via `load_headers(true)` in
    /// `run`), so we read the message directly from the entry without
    /// redundant `load_change` calls.
    fn synthesize_message(&self, new_history: &[HistoryEntry]) -> String {
        let mut messages: Vec<String> = Vec::new();
        for entry in new_history.iter().rev() {
            if let Some(msg) = entry.message() {
                if !msg.is_empty() {
                    messages.push(msg.to_string());
                }
            }
        }

        if messages.is_empty() {
            return "Atomic changes".to_string();
        }

        if messages.len() == 1 {
            return messages.into_iter().next().unwrap();
        }

        // Multiple messages: bullet list, newest first
        let mut result = format!("{} Atomic changes", messages.len());
        for msg in &messages {
            let first_line = msg.lines().next().unwrap_or(msg);
            result.push_str(&format!("\n\n* {}", first_line));
        }
        result
    }

    /// Walk git history (first-parent mainline) to find the most recent
    /// commit whose `Atomic-View` trailer matches `view` and return its
    /// `Atomic-State`. This lets us determine which atomic changes were
    /// already pushed.
    fn find_last_pushed_state(&self, git_repo: &GitRepository, view: &str) -> Option<Merkle> {
        let mut commit = git_repo.head().ok()?.peel_to_commit().ok()?;
        for _ in 0..1000 {
            let message = commit.message().unwrap_or("");
            let commit_view = parse_trailer(message, "Atomic-View");
            if commit_view.as_deref() == Some(view) {
                if let Some(state) = parse_trailer(message, "Atomic-State")
                    .and_then(|s| Merkle::from_base32(s.as_bytes()))
                {
                    return Some(state);
                }
            }
            commit = match commit.parent(0) {
                Ok(p) => p,
                Err(_) => break,
            };
        }
        None
    }

    /// Count commits on `branch_name` that haven't been pushed to its
    /// upstream. Returns 0 when the branch has no configured upstream.
    fn unpushed_commit_count(&self, git_repo: &GitRepository, branch_name: &str) -> usize {
        let local_ref = format!("refs/heads/{}", branch_name);

        // branch_upstream_name returns the full upstream refname
        // (e.g. "refs/remotes/origin/main"), which resolves directly.
        let upstream_oid = git_repo
            .branch_upstream_name(&local_ref)
            .ok()
            .and_then(|buf| buf.as_str().map(str::to_owned))
            .and_then(|name| git_repo.refname_to_id(&name).ok());

        let local_oid = git_repo.refname_to_id(&local_ref).ok();

        match (local_oid, upstream_oid) {
            (Some(local), Some(upstream)) => git_repo
                .graph_ahead_behind(local, upstream)
                .map(|(ahead, _)| ahead)
                .unwrap_or(0),
            _ => 0,
        }
    }

    /// Push the current branch to the remote.
    ///
    /// Delegates the network operation to the `git` CLI so authentication
    /// behaves exactly like a plain `git push`: OpenSSH client config
    /// (`Host`/`IdentityFile`/`IdentitiesOnly`), ssh-agents, askpass
    /// prompts, and HTTPS credential helpers all work as the user expects.
    ///
    /// Do not "simplify" this back to libgit2's transport: its credential
    /// callback can only consult the ssh-agent, which fails for anyone
    /// whose keys live in `~/.ssh/config` (a common setup, e.g.
    /// `IdentityFile ~/.ssh_keys/github` with an empty agent).
    fn push_to_remote(&self, git_repo: &GitRepository) -> CliResult<()> {
        let head = git_repo.head().map_err(|e| CliError::GitError {
            message: format!("Failed to get HEAD: {}", e),
        })?;
        let branch_name = head.shorthand().unwrap_or("HEAD");
        let refspec = format!("refs/heads/{0}:refs/heads/{0}", branch_name);

        let workdir = git_repo.workdir().ok_or_else(|| CliError::GitError {
            message: "Git repository has no working directory (bare repository?)".to_string(),
        })?;

        // Inherit stdio so the user sees git's native output and any
        // interactive auth prompts (ssh passphrase, askpass) work.
        let status = std::process::Command::new("git")
            .args(["push", &self.remote, &refspec])
            .current_dir(workdir)
            .status()
            .map_err(|e| CliError::GitError {
                message: format!("Failed to run git push: {}", e),
            })?;

        if !status.success() {
            return Err(CliError::GitError {
                message: format!("git push exited with {}", status),
            });
        }

        Ok(())
    }
}

/// Parse a `Key: Value` trailer line from a commit message.
fn parse_trailer(message: &str, key: &str) -> Option<String> {
    let prefix = format!("{}:", key);
    for line in message.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(&prefix) {
            let value = rest.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_push_default() {
        let push = Push::default();
        assert!(push.message.is_none());
        assert!(!push.no_push);
        assert_eq!(push.remote, "origin");
    }

    #[test]
    fn test_push_with_message() {
        let push = Push {
            message: Some("custom message".to_string()),
            no_push: true,
            remote: "upstream".to_string(),
        };
        assert_eq!(push.message.as_deref(), Some("custom message"));
        assert!(push.no_push);
        assert_eq!(push.remote, "upstream");
    }

    #[test]
    fn test_parse_trailer() {
        let msg = "feat: add auth\n\nAtomic-View: dev\nAtomic-State: ABC123\nAtomic-Changes: HASH1, HASH2\n";
        assert_eq!(parse_trailer(msg, "Atomic-View"), Some("dev".to_string()));
        assert_eq!(
            parse_trailer(msg, "Atomic-State"),
            Some("ABC123".to_string())
        );
        assert_eq!(
            parse_trailer(msg, "Atomic-Changes"),
            Some("HASH1, HASH2".to_string())
        );
        assert_eq!(parse_trailer(msg, "Atomic-Unknown"), None);
    }

    #[test]
    fn test_parse_trailer_whitespace() {
        let msg = "msg\n\nAtomic-View:   spaced  \n";
        assert_eq!(
            parse_trailer(msg, "Atomic-View"),
            Some("spaced".to_string())
        );
    }

    #[test]
    fn test_parse_trailer_empty() {
        let msg = "msg\n\nAtomic-State:\n";
        assert_eq!(parse_trailer(msg, "Atomic-State"), None);
    }

    #[test]
    fn test_unpushed_commit_count_no_upstream() {
        let dir = tempfile::tempdir().unwrap();
        let repo = GitRepository::init(dir.path()).unwrap();

        // One commit so HEAD resolves.
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree_oid = repo.index().unwrap().write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();

        let push = Push::default();
        // No upstream configured → nothing counted as unpushed.
        assert_eq!(push.unpushed_commit_count(&repo, "main"), 0);
        // Unknown branch → 0, not an error.
        assert_eq!(push.unpushed_commit_count(&repo, "nonexistent"), 0);
    }

    #[test]
    fn test_unpushed_commit_count_ahead_of_upstream() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init_opts(
            dir.path(),
            git2::RepositoryInitOptions::new()
                .initial_head("main")
                .mkdir(false),
        )
        .unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();

        // First commit — this is what "origin/main" points at.
        let tree_oid = repo.index().unwrap().write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let first = repo
            .commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
        repo.remote("origin", "https://example.com/rocket.git")
            .unwrap();
        repo.reference("refs/remotes/origin/main", first, true, "remote")
            .unwrap();

        // Upstream tracking config for main → origin/main.
        let mut config = repo.config().unwrap();
        config.set_str("branch.main.remote", "origin").unwrap();
        config.set_str("branch.main.merge", "refs/heads/main").unwrap();

        let push = Push::default();
        assert_eq!(push.unpushed_commit_count(&repo, "main"), 0);

        // A second local commit puts us one ahead of the upstream.
        let parent = repo.find_commit(first).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "work", &tree, &[&parent])
            .unwrap();
        assert_eq!(push.unpushed_commit_count(&repo, "main"), 1);
    }
}
