//! The `atomic git push` command — sync Atomic changes to Git.
//!
//! Materializes the current view's working copy, stages all changes with
//! `git add -A`, commits with Atomic provenance trailers, and optionally
//! pushes to the remote.

use clap::Args;
use git2::Repository as GitRepository;

use atomic_core::types::Base32;
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
                print_info("Working copy matches Git HEAD. Nothing to commit.");
                return Ok(());
            }
        }

        // Build commit message with Atomic provenance trailers
        let commit_message = self.build_commit_message(&repo, &history, &current_view)?;

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
        print_success(&format!(
            "Created git commit {} on view '{}'",
            short_oid, current_view
        ));

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
    fn build_commit_message(
        &self,
        repo: &Repository,
        history: &[HistoryEntry],
        view: &str,
    ) -> CliResult<String> {
        // Use custom message or synthesize from change messages
        let body = if let Some(ref msg) = self.message {
            msg.clone()
        } else {
            self.synthesize_message(repo, history)
        };

        // Collect change hashes for trailers
        let change_hashes: Vec<String> = history.iter().map(|e| e.hash.to_base32()).collect();

        let merkle_state = history
            .last()
            .map(|e| e.state.to_base32())
            .unwrap_or_default();

        // Format: message + blank line + trailers
        let mut msg = body;
        msg.push_str("\n\n");
        msg.push_str(&format!("Atomic-View: {}\n", view));
        msg.push_str(&format!("Atomic-State: {}\n", merkle_state));

        // For large change sets, only include the last 10 hashes inline
        if change_hashes.len() <= 10 {
            msg.push_str(&format!("Atomic-Changes: {}\n", change_hashes.join(", ")));
        } else {
            let last_10: Vec<&str> = change_hashes[change_hashes.len() - 10..]
                .iter()
                .map(|s| s.as_str())
                .collect();
            msg.push_str(&format!(
                "Atomic-Changes: {} (+{} earlier)\n",
                last_10.join(", "),
                change_hashes.len() - 10
            ));
        }

        Ok(msg)
    }

    /// Synthesize a commit message from Atomic change messages.
    fn synthesize_message(&self, repo: &Repository, history: &[HistoryEntry]) -> String {
        let mut messages: Vec<String> = Vec::new();
        for entry in history.iter().rev().take(20) {
            if let Ok(change) = repo.load_change(&entry.hash) {
                let msg = change.hashed.header.message.clone();
                if !msg.is_empty() {
                    messages.push(msg);
                }
            }
        }

        if messages.is_empty() {
            return "Atomic changes".to_string();
        }

        if messages.len() == 1 {
            return messages.into_iter().next().unwrap();
        }

        // Multiple messages: bullet list
        let mut result = format!("{} Atomic changes", messages.len());
        for msg in &messages {
            let first_line = msg.lines().next().unwrap_or(msg);
            result.push_str(&format!("\n\n* {}", first_line));
        }
        result
    }

    /// Push the current branch to the remote.
    fn push_to_remote(&self, git_repo: &GitRepository) -> CliResult<()> {
        let head = git_repo.head().map_err(|e| CliError::GitError {
            message: format!("Failed to get HEAD: {}", e),
        })?;
        let branch_name = head.shorthand().unwrap_or("HEAD");
        let refspec = format!("refs/heads/{}:refs/heads/{}", branch_name, branch_name);

        let mut remote = git_repo
            .find_remote(&self.remote)
            .map_err(|e| CliError::GitError {
                message: format!("Remote '{}' not found: {}", self.remote, e),
            })?;

        let mut push_options = git2::PushOptions::new();
        let mut callbacks = git2::RemoteCallbacks::new();
        callbacks.credentials(|_url, username_from_url, allowed_types| {
            if allowed_types.contains(git2::CredentialType::SSH_KEY) {
                let user = username_from_url.unwrap_or("git");
                git2::Cred::ssh_key_from_agent(user)
            } else if allowed_types.contains(git2::CredentialType::DEFAULT) {
                git2::Cred::default()
            } else {
                Err(git2::Error::from_str("no suitable credential type"))
            }
        });
        push_options.remote_callbacks(callbacks);

        remote
            .push(&[&refspec], Some(&mut push_options))
            .map_err(|e| CliError::GitError {
                message: format!("Push failed: {}", e),
            })?;

        Ok(())
    }
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
}
