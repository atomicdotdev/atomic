//! The `atomic git push` command — sync Atomic changes to Git.
//!
//! Requires the current Atomic project tree, Git index, and worktree to be
//! CB-4B equivalent, commits that already-verified tree with Atomic provenance
//! trailers, and optionally pushes it to the remote.

use std::io::IsTerminal;

use clap::Args;
use git2::Repository as GitRepository;

use atomic_core::pristine::ViewScope;
use atomic_core::types::{Base32, Merkle};
use atomic_repository::{HistoryEntry, HistoryOptions, Repository};

use crate::commands::workspace_txn::{enter_workspace, remediation_error};
use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{print_info, print_success, print_warning};
use atomic_repository::WorkspaceTxnMode;

/// Push Atomic changes to Git.
///
/// Creates a Git commit from a CB-4B-verified Atomic/Git tree, with trailers
/// binding the Atomic state, manifest, policy, object algorithm, and Git tree.
/// Optionally pushes to the Git remote.
///
/// When the current view is a **draft** and `--branch` is not given, the push
/// targets a Git branch named after the view (`HEAD:refs/heads/<view>`),
/// creating the remote ref on first push so each draft publishes to its own
/// branch. Shared views land on the current Git branch as before.
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
#[derive(Debug, clap::Parser)]
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

    /// Target branch to push to on the remote. Defaults to the current
    /// branch. The commit is still created on the current branch; this
    /// only changes the remote destination (useful for pushing the
    /// current view's state to a PR branch).
    #[arg(long, short = 'b', value_name = "BRANCH")]
    pub branch: Option<String>,

    /// Commit even if the working copy still contains unresolved conflict
    /// markers. Off by default, mirroring `atomic record`; the shadow-sync
    /// turn-end hook must never pass this.
    #[arg(long = "allow-conflict-markers")]
    pub allow_conflict_markers: bool,

    /// Explicitly allow exporting a view with unresolved Atomic conflicts to
    /// a remote (RFC §8.3, CB-8B). This is refused by default for every
    /// remote export. The flag does NOT bypass any other gate: the complete
    /// conflict-object pack must already be published and verify against the
    /// current conflict set, and signing, trust, lease, and publication
    /// policy all still apply. Never passed by automation.
    #[arg(long = "allow-conflicts")]
    pub allow_conflicts: bool,
}

impl Default for Push {
    fn default() -> Self {
        Self {
            message: None,
            no_push: false,
            remote: "origin".to_string(),
            branch: None,
            allow_conflict_markers: false,
            allow_conflicts: false,
        }
    }
}

impl Command for Push {
    fn run(&self) -> CliResult<()> {
        // Preflight through a read-only Atomic handle. This CB-4B gate happens
        // before writable repository open can recover/create a local operation,
        // and before any Git exclude/index/ODB/ref mutation.
        let repo_root = find_repository_root()?;
        let readonly_repo = Repository::open_readonly(&repo_root).map_err(CliError::Repository)?;
        let working_copy = readonly_repo
            .require_working_copy_id()
            .map_err(CliError::Repository)?;
        let current_view = readonly_repo
            .desired_view_name(working_copy)
            .map_err(CliError::Repository)?;
        let mut publication =
            super::shadow::verify_git_publication(&readonly_repo, &repo_root, &current_view)?;
        // CB-12B: trusted provenance publication gate for Git ref export
        // (RFC §10.4). Before any commit/ref/ODB mutation, the complete
        // reachable closure of the exported view must carry trusted
        // managed-session evidence. This is fail-closed with exact
        // diagnostics; content correctness is verified independently by the
        // publication check above.
        {
            let view_changes: Vec<atomic_core::types::Hash> = readonly_repo
                .get_view_changes(Some(&current_view))
                .map_err(CliError::Repository)?
                .into_iter()
                .map(|(_, hash)| hash)
                .collect();
            let closure = atomic_repository::repository::provenance_gate::reachable_closure(
                &readonly_repo,
                &view_changes,
            )
            .map_err(CliError::Repository)?;
            let provider = atomic_repository::repository::provenance_gate::
                local_session_mac_key_provider(&readonly_repo);
            readonly_repo
                .enforce_publication_gate("Git ref export", &closure, Some(&provider))
                .map_err(CliError::Repository)?;
        }
        // RFC §8.3 (CB-8B): remote export of a view with unresolved Atomic
        // conflicts is refused by default. Even with --allow-conflicts the
        // complete conflict-object pack must already be published and verify
        // against the CURRENT conflict set — markers plus a hash are
        // insufficient, and the flag cannot bless a missing or stale pack.
        {
            let conflicted = readonly_repo
                .capture_view_conflict_set(&current_view)
                .map_err(CliError::Repository)?
                .is_some();
            if conflicted {
                if !self.allow_conflicts {
                    return Err(CliError::Repository(
                        atomic_repository::RepositoryError::InvalidOperation {
                            message: format!(
                                "view '{current_view}' has unresolved Atomic conflicts; remote \
                                 export is refused. Project the conflict snapshot on a Draft ref \
                                 and publish its binding with --with-conflicts-pack first, or pass \
                                 --allow-conflicts explicitly (which still requires a complete, \
                                 current conflict pack and never bypasses trust/signing/lease gates)"
                            ),
                        },
                    ));
                }
                let git_for_conflicts =
                    GitRepository::discover(&repo_root).map_err(|error| CliError::GitError {
                        message: format!("cannot discover Git repository: {error}"),
                    })?;
                let object = readonly_repo
                    .capture_view_conflict_set(&current_view)
                    .map_err(CliError::Repository)?
                    .expect("conflict presence checked above");
                let set_hash = object.hash().map_err(|error| {
                    CliError::Repository(
                        atomic_repository::RepositoryError::InvalidOperation {
                            message: format!("cannot hash the conflict set: {error}"),
                        },
                    )
                })?;
                let mut pack_current = false;
                for id in readonly_repo.binding_ids().map_err(CliError::Repository)? {
                    if let Some(pack) = readonly_repo
                        .load_binding_conflicts_pack(&git_for_conflicts, &id)
                        .map_err(CliError::Repository)?
                    {
                        if atomic_repository::git_binding::validate_conflicts_pack(
                            &pack,
                            &set_hash,
                        )
                        .is_ok()
                        {
                            pack_current = true;
                            break;
                        }
                    }
                }
                if !pack_current {
                    return Err(CliError::Repository(
                        atomic_repository::RepositoryError::InvalidOperation {
                            message: format!(
                                "--allow-conflicts requires a published binding whose \
                                 conflicts.pack verifies against the CURRENT conflict set \
                                 ({}); no such pack exists, so the export stays refused",
                                atomic_core::types::Base32::to_base32(&set_hash)
                            ),
                        },
                    ));
                }
            }
        }
        drop(readonly_repo);

        let mut repo = Repository::open(&repo_root).map_err(CliError::Repository)?;
        // Enter the shared workspace transaction and retain its authority for
        // the whole export; the working copy and view come from the
        // working-copy record, never current_view.
        let workspace = enter_workspace(&mut repo, WorkspaceTxnMode::Reconcile)?;
        let working_copy = workspace.working_copy();
        let current_view = workspace.view().name.clone();
        drop(workspace);
        let git_repo = GitRepository::discover(&repo_root).map_err(|_| CliError::GitError {
            message: "Not a git repository (or any parent up to mount point)".to_string(),
        })?;

        // Serialize the shadow-commit pipeline (SPEC §4.3): acquire the
        // repo-scoped lock OUTERMOST, before any staging or git mutation. If
        // another shadow materialize/commit is in flight this is a logged no-op
        // — the in-flight operation owns this commit. Held for the whole run.
        let _shadow_lock =
            match super::shadow::acquire_shadow_lock(&repo, &repo_root, &current_view)? {
                Some(guard) => guard,
                None => return Ok(()),
            };
        publication.reobserve(&repo, &repo_root)?;

        // Resolve the git branch this push targets. Explicit `--branch` wins;
        // otherwise a draft view maps to a git branch named after the view so
        // each draft publishes to its own remote ref. Shared views keep the
        // historical behavior (commit lands on the current git branch).
        let view_scope = repo
            .get_view_info(&current_view)
            .map_err(CliError::Repository)?
            .scope;
        let branch_override =
            Self::resolve_branch_override(self.branch.as_deref(), view_scope, &current_view)?;
        if self.branch.is_none() {
            if let Some(ref branch) = branch_override {
                print_info(&format!(
                    "Draft view '{}' publishes to git branch '{}' (created on the remote if absent).",
                    current_view, branch
                ));
            }
        }
        let target_override = branch_override.as_deref();

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
            Some(state) => match history.iter().position(|e| &e.state == state) {
                // Fast-forward: the branch's last published state is in the
                // view's recorded lineage. Push the changes recorded since.
                Some(i) => i + 1,
                // Validator Rule V3 (SPEC §6.3): the branch's last published
                // state is NOT in this view's lineage — genuine drift (e.g. the
                // branch was reset, or published from a foreign view/state).
                // Refuse with **no bypass**; reconcile-then-push.
                None => {
                    if !std::io::stderr().is_terminal() {
                        super::shadow::append_shadow_validate_log(
                            &repo_root,
                            "V3",
                            &current_view,
                            &format!(
                                "branch state {} not in view lineage",
                                &state.to_base32()[..12.min(state.to_base32().len())]
                            ),
                        );
                    }
                    print_warning(&format!(
                        "Refusing to shadow-commit: git branch's last published state is \
                         not in view '{}' history (drift).",
                        current_view
                    ));
                    return Err(CliError::GitError {
                        message: format!(
                            "git shadow drift (V3): the branch was last published from a \
                             state view '{}' cannot reach. Reconcile first, then re-push — \
                             `git reset --hard` the branch to its last coherent shadow \
                             commit, or `atomic git import` to onboard the git-side work. \
                             No commit was created.",
                            current_view
                        ),
                    });
                }
            },
            // First publish for this view/branch — nothing to drift from.
            None => 0,
        };
        let new_history = &history[start_idx..];
        let new_count = new_history.len();

        // Single shadow-publication pipeline: CB-4B has already proved the
        // index/worktree/project tree equivalent. Resolve that verified tree and
        // enforce V1/V4 without staging or writing a replacement index/tree.
        let marker_policy = if self.allow_conflict_markers {
            super::shadow::ConflictMarkerPolicy::AllowExplicitly
        } else {
            super::shadow::ConflictMarkerPolicy::Refuse
        };
        let tree_oid = super::shadow::stage_and_validate_tree(
            &repo,
            &git_repo,
            &repo_root,
            &publication,
            marker_policy,
        )?;
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
                let target = target_override.unwrap_or(&branch_name);
                let ahead = self.unpushed_commit_count(&git_repo, &branch_name, target_override);
                if ahead == 0 && target_override.is_none() {
                    print_info("Working copy matches Git HEAD. Nothing to commit.");
                    return Ok(());
                }
                if self.no_push {
                    if ahead > 0 {
                        print_info(&format!(
                            "Nothing to commit, but {} unpushed commit{} (--no-push set).",
                            ahead,
                            if ahead == 1 { "" } else { "s" }
                        ));
                    } else {
                        print_info("Nothing to commit (--no-push set).");
                    }
                    return Ok(());
                }
                if ahead > 0 {
                    print_info(&format!(
                        "Nothing to commit, but {} unpushed commit{}. Pushing...",
                        ahead,
                        if ahead == 1 { "" } else { "s" }
                    ));
                } else {
                    // Explicit --branch with nothing new: push the current
                    // state to the target (may create the remote branch).
                    print_info(&format!(
                        "Nothing to commit. Pushing current state to {}/{}...",
                        self.remote, target
                    ));
                }
                self.push_to_remote(&repo, &repo_root, &git_repo, target_override, &publication, &_shadow_lock)?;
                print_success(&format!("Pushed to {}/{}", self.remote, target));
                return Ok(());
            }
        }

        // Build commit message with Atomic provenance trailers.
        // Only the changes new since the last push are included in the body
        // and the Atomic-Changes trailer. The Atomic-State trailer always
        // reflects the latest view state so the next push can find this point.
        let latest_state = history.last().map(|e| e.state).unwrap_or_default();
        let (commit_message, sig) = self.build_commit_message(
            &repo,
            &repo_root,
            new_history,
            &latest_state,
            &current_view,
            &publication,
            &git_repo,
        )?;

        // Get parent commit (HEAD), if any
        let parent = git_repo.head().ok().and_then(|h| h.peel_to_commit().ok());
        let parents: Vec<&git2::Commit> = parent.as_ref().map(|p| vec![p]).unwrap_or_default();

        // Close the observation-to-commit window. `commit(Some("HEAD"), ...)`
        // writes both the commit object and the local ref, so no mutation is
        // allowed unless every CB-4B lease still matches.
        publication.reobserve_before_commit(&repo, &repo_root, &git_repo)?;

        // Create the commit
        let commit_oid = git_repo
            .commit(Some("HEAD"), &sig, &sig, &commit_message, &tree, &parents)
            .map_err(|e| CliError::GitError {
                message: format!("Failed to create commit: {}", e),
            })?;

        publication.bind_committed_head(&git_repo, commit_oid)?;
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
            match self.push_to_remote(&repo, &repo_root, &git_repo, target_override, &publication, &_shadow_lock) {
                Ok(()) => {
                    let target = self.target_branch(&git_repo, target_override);
                    print_success(&format!("Pushed to {}/{}", self.remote, target));
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
    /// next push can find this commit as its starting point. The RFC §8.2
    /// headers (`atomic-set`, `atomic-state`, `atomic-author`) are emitted
    /// alongside them, with identity resolved through the §5.5 map.
    fn build_commit_message(
        &self,
        repo: &Repository,
        repo_root: &std::path::Path,
        new_history: &[HistoryEntry],
        latest_state: &Merkle,
        view: &str,
        publication: &super::shadow::VerifiedPublication,
        git_repo: &GitRepository,
    ) -> CliResult<(String, git2::Signature<'static>)> {
        let latest_state = latest_state.to_base32();
        if publication.view() != view || publication.atomic_state() != latest_state {
            return Err(CliError::GitError {
                message: "Atomic view/state changed after publication verification".to_string(),
            });
        }
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
        msg.push_str(&format!("Atomic-View: {}\n", publication.view()));
        msg.push_str(&format!("Atomic-State: {}\n", publication.atomic_state()));
        msg.push_str(&format!(
            "Atomic-Manifest: {}\n",
            publication.manifest_root().content_key
        ));
        msg.push_str(&format!(
            "Atomic-Policy: {}\n",
            publication.policy_root().content_key
        ));
        msg.push_str(&format!(
            "Atomic-Algorithm: {}\n",
            match publication.object_algorithm() {
                atomic_core::operation::GitHashAlgorithm::Sha1 => "sha1",
                atomic_core::operation::GitHashAlgorithm::Sha256 => "sha256",
            }
        ));
        msg.push_str(&format!("Atomic-Tree: {}\n", publication.git_tree_oid()?));

        // RFC §8.2 operation-identity headers. The projection SetId comes
        // from the canonical effective closure; the author DID from the
        // §5.5 email mapping.
        let set_id = repo.view_set_id(view).map_err(CliError::Repository)?;
        let state =
            Merkle::from_base32(latest_state.as_bytes()).ok_or_else(|| CliError::GitError {
                message: "verified view state is not a valid Merkle value".to_string(),
            })?;
        msg.push_str(&format!("atomic-set {}\n", set_id.to_base32()));
        msg.push_str(&format!("atomic-state {latest_state}\n"));
        let git_sig = git_repo.signature().map_err(|e| CliError::GitError {
            message: format!("Failed to get git signature: {}", e),
        })?;
        let author =
            super::shadow::projection_author(repo, repo_root, git_sig.name(), git_sig.email());
        msg.push_str(&format!("atomic-author {}\n", author.did));
        if let Some(agent) = &author.agent_did {
            msg.push_str(&format!("atomic-agent {agent}\n"));
            msg.push_str(&format!("Co-authored-by: {} <{}>\n", author.name, agent));
        }

        let change_hashes: Vec<String> = new_history.iter().map(|e| e.hash.to_base32()).collect();

        if !change_hashes.is_empty() {
            msg.push_str(&format!("Atomic-Changes: {}\n", change_hashes.join(", ")));
        }

        let owned_sig =
            git2::Signature::now(&author.name, &author.email).map_err(|e| CliError::GitError {
                message: format!("Failed to build projected commit signature: {}", e),
            })?;
        Ok((msg, owned_sig))
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

    /// Resolve the git branch this push targets, applying draft-view
    /// auto-mapping.
    ///
    /// Explicit `--branch` (`explicit`) always wins. Otherwise, when the
    /// current Atomic view is a draft, the target defaults to a git branch
    /// named after the view, so each draft publishes to its own remote ref
    /// (created on first push). Shared views keep the historical behavior of
    /// landing on the current git branch, signalled by returning `None`.
    ///
    /// A draft view whose name is not a valid git refname is a hard error:
    /// the name is used verbatim (never silently rewritten), so the caller is
    /// told to pass an explicit `--branch` instead.
    fn resolve_branch_override(
        explicit: Option<&str>,
        scope: ViewScope,
        view: &str,
    ) -> CliResult<Option<String>> {
        if let Some(branch) = explicit {
            return Ok(Some(branch.to_string()));
        }
        if scope.is_draft() {
            let refname = format!("refs/heads/{}", view);
            if !git2::Reference::is_valid_name(&refname) {
                return Err(CliError::GitError {
                    message: format!(
                        "Draft view '{}' is not a valid git branch name; \
                         pass --branch to choose an explicit target.",
                        view
                    ),
                });
            }
            return Ok(Some(view.to_string()));
        }
        Ok(None)
    }

    /// The git branch the push will land on: the resolved `override_branch`
    /// (explicit `--branch` or a draft view's mapped branch) if present, else
    /// the currently checked-out branch.
    fn target_branch(&self, git_repo: &GitRepository, override_branch: Option<&str>) -> String {
        if let Some(branch) = override_branch {
            return branch.to_string();
        }
        git_repo
            .head()
            .ok()
            .and_then(|h| h.shorthand().map(str::to_owned))
            .unwrap_or_else(|| "HEAD".to_string())
    }

    /// Count commits on `branch_name` that haven't been pushed to the push
    /// target. The comparison point is the branch's configured upstream,
    /// or `refs/remotes/<remote>/<override_branch>` when a resolved target
    /// (explicit `--branch` or a draft view's mapped branch) redirects the
    /// push. Returns 0 when there is no remote ref to compare against.
    fn unpushed_commit_count(
        &self,
        git_repo: &GitRepository,
        branch_name: &str,
        override_branch: Option<&str>,
    ) -> usize {
        let local_ref = format!("refs/heads/{}", branch_name);

        let upstream_oid = match override_branch {
            // Redirected target: compare against the remote-tracking ref.
            Some(target) => git_repo
                .refname_to_id(&format!("refs/remotes/{}/{}", self.remote, target))
                .ok(),
            // branch_upstream_name returns the full upstream refname
            // (e.g. "refs/remotes/origin/main"), which resolves directly.
            None => git_repo
                .branch_upstream_name(&local_ref)
                .ok()
                .and_then(|buf| buf.as_str().map(str::to_owned))
                .and_then(|name| git_repo.refname_to_id(&name).ok()),
        };

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
    ///
    /// CB-10B (RFC §8.5): a mapped remote update is leased against the
    /// mapping's `last_observed_remote` with
    /// `--force-with-lease=<ref>:<expected>` — a stale push refuses and
    /// newer external remote work is never overwritten. A view without a
    /// persisted mapping gets its baseline mapping created here, and an
    /// unobserved remote is observed explicitly first (`git ls-remote`);
    /// the new remote tip is verified after the push and recorded back into
    /// the mapping, so the next push is leased against it.
    fn push_to_remote(
        &self,
        repo: &Repository,
        repo_root: &std::path::Path,
        git_repo: &GitRepository,
        override_branch: Option<&str>,
        publication: &super::shadow::VerifiedPublication,
        common: &atomic_repository::RepositoryCommonLockGuard,
    ) -> CliResult<()> {
        let head = git_repo.head().map_err(|e| CliError::GitError {
            message: format!("Failed to get HEAD: {}", e),
        })?;
        let current = head.shorthand().unwrap_or("HEAD");
        let target = override_branch.unwrap_or(current);
        let destination = format!("refs/heads/{}", target);
        if !super::is_publishable_git_ref(destination.as_bytes()) {
            return Err(CliError::GitError {
                message: format!("refusing to publish local recovery ref '{destination}'"),
            });
        }
        // CB-10B review R3: push the PINNED verified commit, never the
        // mutable HEAD — the refspec names the exact commit the publication
        // snapshot verified, so a branch that moves after verification
        // cannot change what is pushed.
        let verified_head = publication.git_head.ok_or_else(|| {
            CliError::GitError {
                message: "the verified publication carries no Git commit to push".to_string(),
            }
        })?;
        let refspec = format!("{}:{destination}", verified_head);

        let workdir = git_repo.workdir().ok_or_else(|| CliError::GitError {
            message: "Git repository has no working directory (bare repository?)".to_string(),
        })?;

        // Re-observe immediately before handing the ref effect to `git push`.
        publication.reobserve_for_git_push(repo, repo_root, git_repo)?;

        // CB-10B remote lease (RFC §8.5). Every mapped view push runs under
        // an expected-old lease: the mapping's last_observed_remote pins the
        // expected remote tip, an unobserved remote is observed explicitly
        // first, and the verified new tip is recorded back for the next
        // push. The working copy comes from the publication boundary.
        let current_view = publication.view().to_string();
        let working_copy = repo.require_working_copy_id().map_err(CliError::Repository)?;
        let mapping = match repo.get_ref_mapping(&current_view).map_err(CliError::Repository)? {
            Some(mapping) => Some(mapping),
            None => {
                // First push for this view: create the baseline mapping so
                // the remote observation below has a durable row to advance.
                super::ref_mapping::ensure_baseline_mapping(repo, working_copy, git_repo, &current_view)?.0;
                repo.get_ref_mapping(&current_view).map_err(CliError::Repository)?
            }
        };
        // The tracked remote pair: the mapping's pair when it matches this
        // remote; otherwise the pushed destination becomes the pair (first
        // push or re-target).
        let tracked = mapping
            .as_ref()
            .and_then(|mapping| mapping.remote.as_ref())
            .filter(|(name, _)| name == &self.remote)
            .map(|(_, reference)| reference.clone())
            .unwrap_or_else(|| destination.clone());
        let lease = match mapping
            .as_ref()
            .and_then(|mapping| mapping.last_observed_remote.as_deref())
            .filter(|_| tracked == destination)
        {
            Some(expected) => super::transport::remote_lease_refspec(Some(expected), &destination),
            None if tracked == destination => {
                // No prior remote observation: observe explicitly. An ABSENT
                // remote ref is leased as expected-absence (create-only). An
                // OBSERVED remote ref carries someone's work — possibly
                // AHEAD of us — and is never leased: leasing the observed
                // tip would grant force authority to overwrite exactly that
                // unrequested work (CB-10B review R2). The push runs
                // without a lease, so Git's own fast-forward check refuses
                // a non-descendant update.
                let tip = super::transport::observe_remote_ref(&workdir, &self.remote, &destination)?;
                match tip {
                    None => Some(format!("--force-with-lease={destination}:")),
                    Some(_) => None,
                }
            }
            None => None,
        };

        // CB-10B review R3 race fixture: a concurrent writer moves the local
        // branch AFTER the publication snapshot (a ref move only — the
        // worktree stays clean). With the pinned-OID refspec the push still
        // publishes the exact verified commit; the mutable-HEAD refspec
        // would have published the moved branch instead.
        #[cfg(feature = "adoption-test-injection")]
        if std::env::var_os("ATOMIC_FAIL_PUSH_MOVE_BRANCH_AFTER_VERIFY").is_some() {
            // The race moves the LOCAL branch HEAD names (the override
            // target is the REMOTE destination only).
            let branch_ref = format!(
                "refs/heads/{}",
                git_repo
                    .head()
                    .ok()
                    .and_then(|head| head.shorthand().map(str::to_string))
                    .unwrap_or_else(|| "master".to_string())
            );
            let verified = publication.git_head.ok_or_else(|| CliError::GitError {
                message: "race fixture: no verified commit".to_string(),
            })?;
            let signature = git2::Signature::now("Racer", "racer@example.com")
                .map_err(|error| CliError::GitError { message: error.to_string() })?;
            let tree_id = {
                let mut builder = git_repo
                    .treebuilder(None)
                    .map_err(|error| CliError::GitError { message: error.to_string() })?;
                builder
                    .write()
                    .map_err(|error| CliError::GitError { message: error.to_string() })?
            };
            let tree = git_repo
                .find_tree(tree_id)
                .map_err(|error| CliError::GitError { message: error.to_string() })?;
            let parent = git_repo
                .find_commit(verified)
                .map_err(|error| CliError::GitError { message: error.to_string() })?;
            let moved = git_repo
                .commit(None, &signature, &signature, "raced branch move", &tree, &[&parent])
                .map_err(|error| CliError::GitError { message: error.to_string() })?;
            let mut reference = git_repo
                .find_reference(&branch_ref)
                .map_err(|error| CliError::GitError { message: error.to_string() })?;
            reference
                .set_target(moved, "raced branch move after the publication snapshot")
                .map_err(|error| CliError::GitError { message: error.to_string() })?;
        }

        let mut args: Vec<String> = vec!["push".to_string(), self.remote.clone()];
        if let Some(lease) = &lease {
            args.push(lease.clone());
        }
        args.push(refspec.clone());

        // CB-12B (RFC §10.4/§11.1): local gates and hooks are advisory to
        // the Git-publication guarantee. State plainly — before the push —
        // that this push is NOT blocked by Atomic unless the receiving
        // remote itself enforces provenance. Never claim protection that
        // the boundary does not provide.
        print_warning(&format!(
            "Local provenance verification is advisory: this push is only protected if '{}' \
             enforces managed provenance server-side (Atomic-controlled remote, a \
             pre-receive/update hook running 'atomic git bridge verify-receive', or a required \
             CI status check). Pushing to a remote without such enforcement is NOT blocked by \
             Atomic; the push will proceed regardless of what this client verified.",
            self.remote
        ));

        // Inherit stdio so the user sees git's native output and any
        // interactive auth prompts (ssh passphrase, askpass) work.
        let status = std::process::Command::new("git")
            .args(&args)
            .current_dir(workdir)
            .status()
            .map_err(|e| CliError::GitError {
                message: format!("Failed to run git push: {}", e),
            })?;

        if !status.success() {
            let stale = if lease.is_some() {
                " (the remote ref moved since it was last observed; the lease refused \
                 to overwrite newer external work — fetch/reconcile and retry)"
            } else {
                ""
            };
            return Err(CliError::GitError {
                message: format!("git push exited with {}{stale}", status),
            });
        }

        // Verify the remote tip equals the pushed commit before recording
        // the observation (RFC §8.5 lease semantics, CB-10B AC-1). The
        // expected value is the PINNED verified commit — the same OID the
        // refspec carried — never a mutable HEAD reread (CB-10B review R3:
        // a concurrent branch move between the push and this observation
        // must not corrupt the verification).
        let pushed_oid = verified_head.to_string();
        let remote_tip = super::transport::observe_remote_ref(&workdir, &self.remote, &destination)?;
        match remote_tip {
            Some(tip) if tip == pushed_oid => {
                super::ref_mapping::record_remote_push_observation(
                    repo,
                    working_copy,
                    &current_view,
                    &self.remote,
                    &destination,
                    &tip,
                    Some(common),
                )?;
            }
            other => {
                return Err(CliError::GitError {
                    message: format!(
                        "remote '{}/{}' holds {:?} but the pushed commit is {pushed_oid}; \
                         the remote update is NOT recorded as observed",
                        self.remote, destination, other
                    ),
                });
            }
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
    use clap::Parser;

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
            branch: Some("pr-branch".to_string()),
            allow_conflict_markers: false,
            allow_conflicts: false,
        };
        assert_eq!(push.message.as_deref(), Some("custom message"));
        assert!(push.no_push);
        assert_eq!(push.remote, "upstream");
        assert_eq!(push.branch.as_deref(), Some("pr-branch"));
    }

    #[test]
    fn test_push_branch_flag() {
        let push = Push::try_parse_from(["push", "--branch", "pr-42"])
            .map_err(|e| e.to_string())
            .unwrap();
        assert_eq!(push.branch.as_deref(), Some("pr-42"));

        let push = Push::try_parse_from(["push", "-b", "pr-42"])
            .map_err(|e| e.to_string())
            .unwrap();
        assert_eq!(push.branch.as_deref(), Some("pr-42"));

        let push = Push::try_parse_from(["push"])
            .map_err(|e| e.to_string())
            .unwrap();
        assert!(push.branch.is_none());
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
        assert_eq!(push.unpushed_commit_count(&repo, "main", None), 0);
        // Unknown branch → 0, not an error.
        assert_eq!(push.unpushed_commit_count(&repo, "nonexistent", None), 0);
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
        config
            .set_str("branch.main.merge", "refs/heads/main")
            .unwrap();

        let push = Push::default();
        assert_eq!(push.unpushed_commit_count(&repo, "main", None), 0);

        // A second local commit puts us one ahead of the upstream.
        let parent = repo.find_commit(first).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "work", &tree, &[&parent])
            .unwrap();
        assert_eq!(push.unpushed_commit_count(&repo, "main", None), 1);

        // With a redirected target branch that has nothing, the same local
        // state counts differently: refs/remotes/origin/feature doesn't
        // exist → 0; after creating it at the first commit → 1.
        assert_eq!(
            push.unpushed_commit_count(&repo, "main", Some("feature")),
            0
        );
        repo.reference("refs/remotes/origin/feature", first, true, "remote")
            .unwrap();
        assert_eq!(
            push.unpushed_commit_count(&repo, "main", Some("feature")),
            1
        );
    }

    #[test]
    fn test_target_branch() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init_opts(
            dir.path(),
            git2::RepositoryInitOptions::new()
                .initial_head("main")
                .mkdir(false),
        )
        .unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree_oid = repo.index().unwrap().write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();

        // Default (no override): the checked-out branch.
        assert_eq!(Push::default().target_branch(&repo, None), "main");

        // A resolved override wins.
        assert_eq!(Push::default().target_branch(&repo, Some("pr-42")), "pr-42");
    }

    #[test]
    fn test_resolve_branch_override_explicit_wins() {
        // Explicit --branch wins regardless of scope.
        assert_eq!(
            Push::resolve_branch_override(Some("pr-42"), ViewScope::Draft, "feature").unwrap(),
            Some("pr-42".to_string())
        );
        assert_eq!(
            Push::resolve_branch_override(Some("pr-42"), ViewScope::Shared, "main").unwrap(),
            Some("pr-42".to_string())
        );
    }

    #[test]
    fn test_resolve_branch_override_draft_maps_to_view() {
        assert_eq!(
            Push::resolve_branch_override(None, ViewScope::Draft, "feature-login").unwrap(),
            Some("feature-login".to_string())
        );
    }

    #[test]
    fn test_resolve_branch_override_shared_is_none() {
        // Shared views keep the historical behavior (land on current branch).
        assert_eq!(
            Push::resolve_branch_override(None, ViewScope::Shared, "main").unwrap(),
            None
        );
    }

    #[test]
    fn test_resolve_branch_override_invalid_refname_errors() {
        // A draft name that is not a valid git refname is a hard error, not a
        // silent rename.
        let err = Push::resolve_branch_override(None, ViewScope::Draft, "bad~name").unwrap_err();
        match err {
            CliError::GitError { message } => {
                assert!(message.contains("bad~name"));
                assert!(message.contains("--branch"));
            }
            other => panic!("expected GitError, got {:?}", other),
        }
    }
}
