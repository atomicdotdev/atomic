//! The shadow-publication pipeline and its CB-4B equivalence capability.
//!
//! `atomic git push`, native push with an active bridge, and shadow projection
//! build an Atomic `ProjectTree`, observe Git index/worktree state read-only,
//! and require an equivalent report before any publication mutation. The
//! already-equivalent index supplies the tree; this module never stages an
//! unchecked candidate merely to make validation pass.
//!
//! Validator rules enforced here (pre-commit):
//! - **V1** — no unresolved conflict markers (shares `record`'s detector).
//! - **V4** — no git-excluded provenance path (`.atomic/`, `.vault/`,
//!   `.atomicignore`) is ever staged.
//!
//! V2 (tree↔view coherence) is the CB-4B joint report. V3 remains the Git
//! history/Atomic-state lineage check in `git::push`.

use std::io::IsTerminal;
use std::path::Path;

use git2::{ObjectType, Repository as GitRepository};

use atomic_config::ContentFilterConfig;
use atomic_core::operation::GitHashAlgorithm;
use atomic_objects::content_key;
use atomic_repository::{
    compare_project_state, compare_project_to_index, compare_project_to_worktree,
    observe_git_index, observe_worktree, ConversionPolicy, EquivalenceClaims, GitAttributesFilter,
    GitObjectKind, ManifestRoot, MismatchKind, ProjectTree, Repository,
};

use crate::error::{CliError, CliResult};
use crate::output::{print_info, print_warning};

/// Marker handling is explicit at the publication boundary; callers cannot
/// accidentally smuggle an unchecked boolean into the validator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConflictMarkerPolicy {
    Refuse,
    AllowExplicitly,
}

/// Private capability proving that the Atomic project tree, Git index, and
/// physical worktree were equivalent under one concrete conversion policy.
///
/// Construction is intentionally restricted to [`verify_git_publication`].
/// Every publication mutation path requires this value and re-observes its
/// leases immediately before its final ref/network effect.
#[derive(Clone, Debug)]
pub(crate) struct VerifiedPublication {
    pub(crate) view: String,
    pub(crate) atomic_state: String,
    pub(crate) policy: ConversionPolicy,
    manifest_root: ManifestRoot,
    index_root: ManifestRoot,
    worktree_root: ManifestRoot,
    pub(crate) git_tree: atomic_core::operation::GitObjectId,
    pub(crate) git_head: Option<git2::Oid>,
    project: ProjectTree,
    require_index_equivalence: bool,
}

impl VerifiedPublication {
    pub(crate) fn view(&self) -> &str {
        &self.view
    }

    pub(crate) fn atomic_state(&self) -> &str {
        &self.atomic_state
    }

    pub(crate) fn manifest_root(&self) -> &ManifestRoot {
        &self.manifest_root
    }

    pub(crate) fn policy_root(&self) -> ManifestRoot {
        self.policy.root()
    }

    pub(crate) fn object_algorithm(&self) -> GitHashAlgorithm {
        self.policy.object_format
    }

    pub(crate) fn git_tree(&self) -> &atomic_core::operation::GitObjectId {
        &self.git_tree
    }

    pub(crate) fn git_tree_oid(&self) -> CliResult<git2::Oid> {
        if self.git_tree.algorithm() != GitHashAlgorithm::Sha1 {
            return Err(git_error(format!(
                "Git publication through libgit2 does not support {:?} repositories",
                self.git_tree.algorithm()
            )));
        }
        git2::Oid::from_bytes(self.git_tree.as_bytes()).map_err(|error| {
            git_error(format!(
                "verified Git tree has an invalid object identity: {error}"
            ))
        })
    }

    pub(crate) fn bind_committed_head(
        &mut self,
        git_repo: &GitRepository,
        commit_oid: git2::Oid,
    ) -> CliResult<()> {
        let commit = git_repo.find_commit(commit_oid).map_err(|error| {
            git_error(format!(
                "cannot bind published Git commit {commit_oid}: {error}"
            ))
        })?;
        if commit.tree_id() != self.git_tree_oid()? {
            return Err(git_error(
                "refusing to bind a Git commit whose tree differs from the verified project tree",
            ));
        }
        let observed = git_repo
            .head()
            .ok()
            .and_then(|head| head.target())
            .ok_or_else(|| {
                git_error("cannot bind publication to an unborn or symbolic-only HEAD")
            })?;
        if observed != commit_oid {
            return Err(git_error(format!(
                "Git HEAD changed while binding publication: expected {commit_oid}, found {observed}"
            )));
        }
        self.git_head = Some(commit_oid);
        Ok(())
    }

    pub(crate) fn reobserve_before_commit(
        &self,
        repo: &Repository,
        repo_root: &Path,
        git_repo: &GitRepository,
    ) -> CliResult<()> {
        self.reobserve(repo, repo_root)?;
        let observed = git_repo.head().ok().and_then(|head| head.target());
        if observed != self.git_head {
            return Err(git_error(format!(
                "Git HEAD lease changed before commit: expected {:?}, found {:?}",
                self.git_head, observed
            )));
        }
        Ok(())
    }

    pub(crate) fn reobserve_for_git_push(
        &self,
        repo: &Repository,
        repo_root: &Path,
        git_repo: &GitRepository,
    ) -> CliResult<()> {
        self.reobserve(repo, repo_root)?;
        let expected = self
            .git_head
            .ok_or_else(|| git_error("cannot publish an unborn Git HEAD"))?;
        let head = git_repo
            .head()
            .and_then(|head| head.peel_to_commit())
            .map_err(|error| git_error(format!("cannot re-observe Git HEAD: {error}")))?;
        if head.id() != expected || head.tree_id() != self.git_tree_oid()? {
            return Err(git_error(format!(
                "Git HEAD lease changed before push: expected commit {expected} with tree {}, found commit {} with tree {}",
                self.git_tree_oid()?,
                head.id(),
                head.tree_id()
            )));
        }
        Ok(())
    }

    /// Re-observe every lease represented by this capability. This rejects an
    /// Atomic view/state change, policy change, index edit, or worktree edit.
    pub(crate) fn reobserve(&self, repo: &Repository, repo_root: &Path) -> CliResult<()> {
        let current = verify_git_publication_mode(
            repo,
            repo_root,
            &self.view,
            self.require_index_equivalence,
        )?;
        if current.policy != self.policy
            || current.atomic_state != self.atomic_state
            || current.manifest_root != self.manifest_root
            || current.index_root != self.index_root
            || current.worktree_root != self.worktree_root
            || current.git_tree != self.git_tree
        {
            return Err(git_error(
                "publication state changed after equivalence verification; retry from fresh observations",
            ));
        }
        Ok(())
    }
}

/// Verify a mandatory CB-4B publication gate for a Git-backed command.
pub(crate) fn verify_git_publication(
    repo: &Repository,
    repo_root: &Path,
    view: &str,
) -> CliResult<VerifiedPublication> {
    verify_git_publication_mode(repo, repo_root, view, true)
}

fn verify_git_publication_mode(
    repo: &Repository,
    repo_root: &Path,
    view: &str,
    require_index_equivalence: bool,
) -> CliResult<VerifiedPublication> {
    let git_repo = GitRepository::discover(repo_root)
        .map_err(|error| git_error(format!("cannot discover Git repository: {error}")))?;
    let (policy, filters) = current_conversion_policy(repo_root, &git_repo)?;
    verify_git_publication_with_policy(
        repo,
        repo_root,
        view,
        policy,
        filters,
        require_index_equivalence,
        None,
    )
}

/// Verify publication equivalence against a caller-supplied project tree.
///
/// The conflict-snapshot path builds a marker-materialized tree (RFC §8.3)
/// that differs from the resolved-side graph projection; equivalence must be
/// judged against exactly the tree that will be published.
fn verify_git_publication_with_project(
    repo: &Repository,
    repo_root: &Path,
    view: &str,
    project: atomic_repository::ProjectTree,
    require_index_equivalence: bool,
) -> CliResult<VerifiedPublication> {
    let git_repo = GitRepository::discover(repo_root)
        .map_err(|error| git_error(format!("cannot discover Git repository: {error}")))?;
    let (policy, filters) = current_conversion_policy(repo_root, &git_repo)?;
    verify_git_publication_with_policy(
        repo,
        repo_root,
        view,
        policy,
        filters,
        require_index_equivalence,
        Some(project),
    )
}

/// Whether native publication must run the Git bridge gate.
pub(crate) fn bridge_publication_required(repo_root: &Path) -> bool {
    GitRepository::discover(repo_root)
        .map(|repository| shadow_sync_active(&repository))
        .unwrap_or(false)
}

/// Native Atomic publication is unchanged without an active Git shadow. Once
/// the shadow marker exists, however, publication is gated fail-closed.
pub(crate) fn verify_bridge_publication(
    repo: &Repository,
    repo_root: &Path,
    view: &str,
) -> CliResult<Option<VerifiedPublication>> {
    let Ok(git_repo) = GitRepository::discover(repo_root) else {
        return Ok(None);
    };
    if !shadow_sync_active(&git_repo) {
        return Ok(None);
    }
    let (policy, filters) = current_conversion_policy(repo_root, &git_repo)?;
    verify_git_publication_with_policy(repo, repo_root, view, policy, filters, true, None).map(Some)
}

fn verify_git_publication_with_policy(
    repo: &Repository,
    repo_root: &Path,
    view: &str,
    policy: ConversionPolicy,
    filters: ContentFilterConfig,
    require_index_equivalence: bool,
    conflict_project: Option<atomic_repository::ProjectTree>,
) -> CliResult<VerifiedPublication> {
    let project = match conflict_project {
        Some(project) => project,
        None => {
            // RFC §8.3 (CB-8B): a view with persisted unresolved conflicts
            // projects its marker-materialized tree, not the resolved-side
            // graph bytes — equivalence must be judged against exactly the
            // tree that will be published. Without persisted conflicts the
            // ordinary graph projection applies.
            let captured = repo
                .capture_view_conflict_set(view)
                .map_err(CliError::Repository)?;
            match captured {
                Some(_) => {
                    let (conflict_policy, _) = current_conversion_policy(
                        repo_root,
                        &GitRepository::discover(repo_root).map_err(|error| {
                            git_error(format!("cannot discover Git repository: {error}"))
                        })?,
                    )?;
                    let projection = repo
                        .prepare_conflict_snapshot_projection(view, &conflict_policy)
                        .map_err(|error| CliError::Repository(
                            atomic_repository::RepositoryError::InvalidOperation {
                                message: error.to_string(),
                            },
                        ))?;
                    projection.project
                }
                None => repo.project_tree(view, &policy).map_err(|error| {
                    git_error(format!(
                        "cannot build Atomic publication project tree: {error}"
                    ))
                })?,
            }
        }
    };
    let filter = GitAttributesFilter::new(repo_root, filters);
    let index = observe_git_index(repo_root, &policy)
        .map_err(|error| git_error(format!("cannot observe Git index: {error}")))?;
    let worktree = observe_worktree(repo_root, Some(&index), &filter, &policy)
        .map_err(|error| git_error(format!("cannot observe Git worktree: {error}")))?;
    let report = if require_index_equivalence {
        let claims = EquivalenceClaims {
            manifest_version: Some(project.manifest.version),
            object_algorithm: Some(project.git.algorithm),
            manifest_root: Some(project.manifest.root().content_key.clone()),
            conversion_policy_root: Some(policy.root().content_key),
            git_tree_root: Some(project.git.root.clone()),
        };
        compare_project_state(&project, &index, &worktree, &policy, &claims)
    } else {
        compare_project_to_worktree(&project, &worktree, &policy)
    };
    if !report.is_equivalent() {
        let details = report
            .mismatches
            .iter()
            .take(8)
            .map(|mismatch| {
                let path = mismatch
                    .path
                    .as_ref()
                    .map(|path| format!(" at '{}'", path.escaped()))
                    .unwrap_or_default();
                format!(
                    "{:?}/{:?}{path}: expected {}, observed {}",
                    mismatch.layer, mismatch.kind, mismatch.expected, mismatch.actual
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        return Err(git_error(format!(
            "CB-4B publication equivalence failed for view '{view}': {details}. No publication mutation was attempted"
        )));
    }
    let atomic_state = repo
        .get_view_info(view)
        .map_err(CliError::Repository)?
        .state_base32();
    let git_head = GitRepository::discover(repo_root)
        .ok()
        .and_then(|repository| repository.head().ok().and_then(|head| head.target()));
    Ok(VerifiedPublication {
        view: view.to_string(),
        atomic_state,
        policy,
        manifest_root: project.manifest.root(),
        index_root: index.root(),
        worktree_root: worktree.root(),
        git_tree: project.git.root.clone(),
        git_head,
        project,
        require_index_equivalence,
    })
}

fn current_conversion_policy(
    repo_root: &Path,
    git_repo: &GitRepository,
) -> CliResult<(ConversionPolicy, ContentFilterConfig)> {
    let config = git_repo
        .config()
        .map_err(|error| git_error(format!("cannot read Git configuration: {error}")))?;
    let object_format = match config.get_string("extensions.objectFormat") {
        Ok(value) if value.eq_ignore_ascii_case("sha1") => GitHashAlgorithm::Sha1,
        Ok(value) if value.eq_ignore_ascii_case("sha256") => GitHashAlgorithm::Sha256,
        Ok(value) => {
            return Err(git_error(format!(
                "unsupported Git object algorithm '{value}'"
            )))
        }
        Err(error) if error.code() == git2::ErrorCode::NotFound => GitHashAlgorithm::Sha1,
        Err(error) => {
            return Err(git_error(format!(
                "cannot read Git object algorithm: {error}"
            )))
        }
    };
    let mut policy = ConversionPolicy::new(object_format);
    policy.platform.executable_bit = config.get_bool("core.filemode").unwrap_or(cfg!(unix));
    policy.platform.symlinks = config.get_bool("core.symlinks").unwrap_or(cfg!(unix));
    policy.platform.case_sensitive = !config.get_bool("core.ignorecase").unwrap_or(false);
    policy.platform.unicode_normalizing =
        config.get_bool("core.precomposeunicode").unwrap_or(false);

    let repo_config = atomic_config::RepoConfig::load(&repo_root.join(".atomic/config.toml"))
        .map_err(|error| git_error(format!("cannot load Atomic content-filter policy: {error}")))?;
    let mut relevant = Vec::new();
    relevant.extend_from_slice(format!("object={object_format:?}\n").as_bytes());
    relevant.extend_from_slice(
        format!(
            "filemode={}\nsymlinks={}\nignorecase={}\nprecomposeunicode={}\ntimeout={}\nmax-output={}\n",
            policy.platform.executable_bit,
            policy.platform.symlinks,
            !policy.platform.case_sensitive,
            policy.platform.unicode_normalizing,
            repo_config.filters.timeout_ms,
            repo_config.filters.max_output_bytes
        )
        .as_bytes(),
    );
    policy.relevant_git_config = content_key(&relevant);
    policy.filter_driver_versions = repo_config
        .filters
        .drivers
        .iter()
        .map(|(name, driver)| {
            content_key(
                format!(
                    "{name}\0{}\0{}\0{}",
                    driver.clean.as_deref().unwrap_or(""),
                    driver.smudge.as_deref().unwrap_or(""),
                    driver.required
                )
                .as_bytes(),
            )
        })
        .collect();
    Ok((policy, repo_config.filters))
}

/// Result of coordinating an Atomic view switch with its local Git shadow.
#[derive(Debug)]
pub(crate) enum ShadowSwitchSync {
    /// The working copy is not inside a Git repository.
    SkippedNoGit,
    /// Git exists, but Atomic shadow sync has not been established.
    SkippedInactive,
    /// The target branch, HEAD, and index now describe the materialized view.
    Synchronized(ShadowSwitchReceipt),
}

impl ShadowSwitchSync {
    pub(crate) fn is_synchronized(&self) -> bool {
        matches!(self, Self::Synchronized(_))
    }

    /// Restore the Git evidence captured before synchronization. This is used
    /// when the final bridge checkpoint cannot be refreshed after Git itself
    /// was aligned successfully.
    pub(crate) fn rollback(self, repo_root: &Path) -> CliResult<()> {
        let Self::Synchronized(receipt) = self else {
            return Ok(());
        };
        let git_repo = GitRepository::discover(repo_root).map_err(|error| {
            git_error(format!(
                "cannot reopen Git repository to roll back shadow switch: {error}"
            ))
        })?;
        let failures = receipt.snapshot.restore(&git_repo, &receipt.target_ref);
        if failures.is_empty() {
            Ok(())
        } else {
            Err(git_error(format!(
                "failed to restore original Git state: {}",
                failures.join("; ")
            )))
        }
    }
}

#[derive(Debug)]
pub(crate) struct ShadowSwitchReceipt {
    target_ref: String,
    snapshot: GitSwitchSnapshot,
}

#[derive(Clone, Debug)]
enum ReferenceSnapshot {
    Missing,
    Direct(git2::Oid),
    Symbolic(String),
}

#[derive(Clone, Debug)]
struct GitSwitchSnapshot {
    head: ReferenceSnapshot,
    target: ReferenceSnapshot,
    index: Option<Vec<u8>>,
}

impl GitSwitchSnapshot {
    fn capture(git_repo: &GitRepository, target_ref: &str) -> CliResult<Self> {
        let index_path = git_repo.path().join("index");
        let index = match std::fs::read(&index_path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(git_error(format!(
                    "cannot snapshot Git index before shadow switch: {error}"
                )))
            }
        };
        Ok(Self {
            head: snapshot_reference(git_repo, "HEAD")?,
            target: snapshot_reference(git_repo, target_ref)?,
            index,
        })
    }

    fn restore(&self, git_repo: &GitRepository, target_ref: &str) -> Vec<String> {
        let mut failures = Vec::new();
        if let Err(error) = restore_head(git_repo, &self.head) {
            failures.push(format!("HEAD: {error}"));
        }
        if let Err(error) = restore_reference(git_repo, target_ref, &self.target) {
            failures.push(format!("{target_ref}: {error}"));
        }
        if let Err(error) = restore_index(git_repo, self.index.as_deref()) {
            failures.push(format!("index: {error}"));
        }
        failures
    }
}

/// Project the just-materialized Atomic view into its local Git mirror.
///
/// The working copy is staged exclusively through [`stage_and_validate_tree`].
/// The Git representation follows the view-scope HEAD policy (RFC §8.1):
///
/// - **Shared** views map to a `refs/heads/<name>` branch and a symbolic
///   HEAD; a local projection commit advances that branch from its prior
///   tip (one parent: the previous projected state on the mapped ref).
/// - **Draft** views (including `wc/<id>` snapshot views) stay off the
///   branch namespace: the projection commit is reachable through the
///   direct ref `refs/atomic/views/<name>` and HEAD is left **detached** at
///   it. Publishing a Draft to a real branch is explicit (`atomic git
///   push`, `git switch -c`), never implicit.
/// - **Ephemeral `git/<oid>`** views retain the original adopted commit
///   detached without inventing a branch; only when their Atomic state has
///   advanced beyond the adopted commit do they project, as Drafts do.
///
/// Plain non-Git and non-shadow repositories are typed no-ops. Once shadow sync
/// is active, every failure is returned to the caller and the original Git
/// HEAD, target ref, and index are restored where possible.
pub(crate) fn sync_git_head_to_view(
    repo: &Repository,
    repo_root: &Path,
    view: &str,
) -> CliResult<ShadowSwitchSync> {
    let git_repo = match GitRepository::discover(repo_root) {
        Ok(repo) => repo,
        Err(_) => return Ok(ShadowSwitchSync::SkippedNoGit),
    };
    if !shadow_sync_active(&git_repo) {
        return Ok(ShadowSwitchSync::SkippedInactive);
    }
    let working_copy = repo
        .require_working_copy_id()
        .map_err(CliError::Repository)?;
    let desired_view = repo
        .desired_view_name(working_copy)
        .map_err(CliError::Repository)?;
    if desired_view != view {
        return Err(git_error(format!(
            "cannot synchronize Git shadow for view '{view}': current Atomic view is '{desired_view}'"
        )));
    }

    // A coordinated switch must not silently skip its projection. Contention is
    // an actionable failure because leaving the old branch/index would publish
    // stale evidence for the newly materialized view.
    let shadow_lock = repo
        .try_lock_shadow_commit()
        .map_err(CliError::Repository)?
        .ok_or_else(|| {
            git_error(
                "another shadow materialize is in flight; retry the view switch after it completes",
            )
        })?;

    let receipt = project_view_to_git_under_shadow_lock(
        repo,
        repo_root,
        &git_repo,
        view,
        None,
        &shadow_lock,
    )?;
    // CB-10A: the projection moved the mapped ref under its expected-old
    // lease; record the fresh Synchronized observation on the held handle.
    // The verified tip is the live mapped ref tip read under the held shadow
    // lock (the lease just wrote it).
    let verified_tip = super::ref_mapping::mapped_ref_tip(&git_repo, repo, view);
    super::ref_mapping::refresh_mapping_observation(
        repo,
        working_copy,
        &git_repo,
        view,
        verified_tip.as_deref(),
        Some(&shadow_lock),
    )?;
    Ok(ShadowSwitchSync::Synchronized(receipt))
}

/// Refresh the verified workspace checkpoint from the live repository
/// handle (CB-8A). redb admits only one writable handle per process, so the
/// bridge verifier's own writable open cannot run while a repository handle
/// is pinned; this re-observes the git HEAD/view state and writes the
/// version-2 checkpoint directly.
pub(crate) fn refresh_checkpoint_from_live_handle(
    repo: &Repository,
    repo_root: &Path,
    view: &str,
) -> CliResult<()> {
    let info = repo.get_view_info(view).map_err(CliError::Repository)?;
    let state = info.state_base32();
    let git_repo = GitRepository::discover(repo_root)
        .map_err(|error| git_error(format!("cannot discover Git repository: {error}")))?;
    let head = git_repo
        .head()
        .map_err(|error| git_error(format!("cannot observe Git HEAD: {error}")))?;
    let oid = head
        .target()
        .ok_or_else(|| git_error("Git HEAD does not point directly at a commit"))?;
    let commit = git_repo
        .find_commit(oid)
        .map_err(|error| git_error(format!("cannot read Git HEAD commit: {error}")))?;
    crate::commands::git::checkpoint::write_verified_checkpoint(
        repo_root,
        crate::commands::git::checkpoint::VerifiedCheckpointInput {
            view,
            atomic_state: &state,
            git_head: &oid.to_string(),
            git_tree: &commit.tree_id().to_string(),
        },
    )
    .map(|_| ())
    .map_err(checkpoint_error_of)
}

fn checkpoint_error_of(error: crate::commands::git::checkpoint::CheckpointError) -> CliError {
    CliError::GitError {
        message: error.to_string(),
    }
}

/// Post-record projection (RFC §7.6 "Atomic view/operation" row).
///
/// Projects the just-recorded durable state into the Git shadow with the
/// view-scope HEAD policy. Deliberately tolerant: partial user staging
/// (`git add` movement between boundaries) is legitimate work that must
/// remain visible, so when the Git index describes anything other than the
/// current HEAD tree the projection is skipped instead of overwritten.
/// Failures inside the pipeline still fail closed and restore Git state.
pub(crate) fn sync_git_projection_after_record(
    repo_root: &Path,
    view: &str,
    change_hash: &atomic_core::types::Hash,
) -> CliResult<()> {
    let git_repo = match GitRepository::discover(repo_root) {
        Ok(repo) => repo,
        Err(_) => return Ok(()),
    };
    if !shadow_sync_active(&git_repo) {
        return Ok(());
    }
    let repo = Repository::open(repo_root).map_err(CliError::Repository)?;
    let working_copy = repo
        .require_working_copy_id()
        .map_err(CliError::Repository)?;
    let desired_view = repo
        .desired_view_name(working_copy)
        .map_err(CliError::Repository)?;
    if desired_view != view {
        return Ok(());
    }
    // RFC §8.1: an ephemeral `git/<oid>` view retains its original adopted
    // commit detached. Recording on it is durable Atomic work (§7.5); the
    // Git-side publication happens through the explicit push path, so a
    // record never rewrites the ephemeral's commit or checkpoint here.
    if resolve_head_policy(&repo, &git_repo, view)
        .map(|policy| matches!(policy, HeadPolicy::Ephemeral { .. }))
        .unwrap_or(false)
    {
        return Ok(());
    }
    // Unresolved conflict markers are an input-validity boundary (RFC §8.3):
    // they must not become Git evidence. The durable record stands; the
    // projection waits until the conflict is resolved.
    if repo
        .first_working_copy_conflict_marker(working_copy)
        .map_err(CliError::Repository)?
        .is_some()
    {
        print_warning(
            "Recorded state still contains conflict markers; skipping the record \
             projection. Resolve them, then run `atomic git push`.",
        );
        return Ok(());
    }

    // An Atomic-origin record is a bridge transition (RFC §7.6): the
    // recorded durable state is the worktree's content, so the projection
    // re-stages the primary index manifest to the projected tree — the same
    // semantics as `git commit -a` — and commits. Staged-but-unrecorded
    // content could not survive the record that captured the worktree.
    let record_projection = Some(RecordProjectionMetadata {
        change_hash: *change_hash,
    });
    // The shadow-commit lock (and the repository handle it pins) must be
    // released before the checkpoint verifier reopens the database: redb
    // admits only one open handle per process. The guard stays held until the
    // CB-10A mapping observation below is recorded.
    let shadow_lock = match repo
        .try_lock_shadow_commit()
        .map_err(CliError::Repository)?
    {
        Some(guard) => guard,
        None => {
            print_info(
                "Another shadow materialize is in flight; skipping the record projection.",
            );
            return Ok(());
        }
    };
    {
        project_view_to_git_under_shadow_lock(
            &repo,
            repo_root,
            &git_repo,
            view,
            record_projection,
            &shadow_lock,
        )?;
        // CB-10A: the record projection moved the mapped ref under its
        // expected-old lease; refresh the mapping observation on the held
        // handle before the checkpoint verifier reopens the database. The
        // verified tip is the live mapped ref tip read under the held lock.
        let working_copy = repo
            .require_working_copy_id()
            .map_err(CliError::Repository)?;
        let verified_tip = super::ref_mapping::mapped_ref_tip(&git_repo, &repo, view);
        super::ref_mapping::refresh_mapping_observation(
            &repo,
            working_copy,
            &git_repo,
            view,
            verified_tip.as_deref(),
            Some(&shadow_lock),
        )?;
    }
    drop(shadow_lock);
    drop(repo);
    // The projection moved Git HEAD; refresh the verified checkpoint so the
    // next workspace entry sees the post-transition baseline (RFC §7.2).
    crate::commands::git::bridge::refresh_checkpoint_if_aligned(repo_root)?;
    Ok(())
}

fn head_tree_of(git_repo: &GitRepository) -> Option<git2::Oid> {
    git_repo
        .head()
        .ok()
        .and_then(|head| head.peel_to_commit().ok())
        .map(|commit| commit.tree_id())
}

/// Whether the Git index is a clean stage-0 image of the view's projected
/// tree. Only `StagingState.index_manifest` projects to the index (RFC §3.4);
/// any other index state is user work between boundaries. Intent-to-add and
/// sparse entries are tolerated: their intent (not content) is recorded and
/// the joint comparison resolves them like the CB-4B push gate does.
fn index_matches_projection(
    repo: &Repository,
    repo_root: &Path,
    git_repo: &GitRepository,
    view: &str,
) -> bool {
    let Ok((policy, _filters)) = current_conversion_policy(repo_root, git_repo) else {
        return false;
    };
    let Ok(project) = repo.project_tree(view, &policy) else {
        return false;
    };
    let Ok(index) = observe_git_index(repo_root, &policy) else {
        return false;
    };
    let report = compare_project_to_index(&project, &index);
    report.mismatches.is_empty()
}

/// Project one Atomic view into its Git mirror (CB-8A RFC §8.1/§8.2).
///
/// Runs the full leased pipeline for the view's HEAD policy: V1 refusal,
/// CB-4B worktree verification, tree-object write, operation-specific
/// projection commit with one parent, expected-old ref update, scoped HEAD
/// placement, and index alignment to the projected tree (stage 0 only).
/// Every step re-observes the publication leases; on any failure the
/// captured Git HEAD/ref/index snapshot is restored where possible.
pub(crate) fn project_view_to_git(
    repo: &Repository,
    repo_root: &Path,
    git_repo: &GitRepository,
    view: &str,
    record_metadata: Option<RecordProjectionMetadata>,
) -> CliResult<ShadowSwitchReceipt> {
    project_view_to_git_with_conflicts(repo, repo_root, git_repo, view, record_metadata, false, None)
}

/// The projection path for a caller that already holds the repo-scoped
/// shadow-commit (common operation) boundary — the record projection nests
/// under it, so the journaled publisher aliases that lock instead of
/// re-acquiring it.
pub(crate) fn project_view_to_git_under_shadow_lock(
    repo: &Repository,
    repo_root: &Path,
    git_repo: &GitRepository,
    view: &str,
    record_metadata: Option<RecordProjectionMetadata>,
    common: &atomic_repository::RepositoryCommonLockGuard,
) -> CliResult<ShadowSwitchReceipt> {
    project_view_to_git_with_conflicts(
        repo,
        repo_root,
        git_repo,
        view,
        record_metadata,
        false,
        Some(common),
    )
}

/// Resolution of a view's conflict state before projection (RFC §8.3, CB-8B).
enum ConflictExport {
    /// No persisted conflicts: the ordinary clean-state projection.
    Clean,
    /// The view's unresolved conflicts project as marker bytes on a Draft ref.
    Snapshot {
        hash: atomic_core::Hash,
        project: atomic_repository::ProjectTree,
        conflicted_paths: Vec<String>,
    },
}

/// Decide how a view's persisted conflict state projects.
///
/// Draft views project conflict snapshots by default (RFC §8.3); Shared and
/// remote export refuse unless `allow_conflicts` is explicit — and even the
/// explicit flag never bypasses representability (case collisions on an
/// incapable filesystem, unresolved attribute registers), trust, signing, or
/// complete-pack availability.
fn resolve_conflict_export(
    repo: &Repository,
    view: &str,
    policy: &ConversionPolicy,
    allow_conflicts: bool,
) -> CliResult<ConflictExport> {
    let captured = repo
        .capture_view_conflict_set(view)
        .map_err(CliError::Repository)?;
    let Some(object) = captured else {
        return Ok(ConflictExport::Clean);
    };
    let scope = repo.get_view_info(view).map_err(CliError::Repository)?.scope;
    if scope.is_shared() && !allow_conflicts {
        return Err(CliError::Repository(
            atomic_repository::RepositoryError::InvalidOperation {
                message: format!(
                    "view '{view}' has {} unresolved Atomic conflict(s); Shared/remote export \
                     is refused. A Draft ref projects conflicts as an ordinary conflict-snapshot \
                     commit; Shared export requires --allow-conflicts (which still cannot bypass \
                     representability, trust, signing, or complete-pack checks)",
                    object.entry_count()
                ),
            },
        ));
    }
    let projection = repo
        .prepare_conflict_snapshot_projection(view, policy)
        .map_err(|error| CliError::Repository(
            atomic_repository::RepositoryError::InvalidOperation { message: error.to_string() },
        ))?;
    if projection.conflict_set != object {
        return Err(git_error(
            "conflict snapshot preparation disagreed with the captured conflict state",
        ));
    }
    Ok(ConflictExport::Snapshot {
        hash: projection.conflict_set_hash,
        project: projection.project,
        conflicted_paths: projection.conflicted_paths,
    })
}

/// The projection path with conflict snapshot support (RFC §8.3, CB-8B).
pub(crate) fn project_view_to_git_with_conflicts(
    repo: &Repository,
    repo_root: &Path,
    git_repo: &GitRepository,
    view: &str,
    record_metadata: Option<RecordProjectionMetadata>,
    allow_conflicts: bool,
    held_common: Option<&atomic_repository::RepositoryCommonLockGuard>,
) -> CliResult<ShadowSwitchReceipt> {
    let head_policy = resolve_head_policy(repo, git_repo, view)?;
    // An ephemeral `git/<oid>` view retains its adopted commit only while the
    // view's projected tree is unchanged from that commit; that decision needs
    // the verified project, so it is made after the publication check below.
    let ephemeral_original = match &head_policy {
        HeadPolicy::Ephemeral { original } => Some(original.clone()),
        _ => None,
    };
    // Rollback evidence. The provisional ref name is replaced by the real
    // target once it is known; restoring HEAD is the critical part of the
    // rollback in every path, and it is captured up front. Ephemeral views
    // keep a valid refname for the capture (their clean path restores HEAD
    // only; the views ref does not exist and stays Missing).
    let provisional = match &head_policy {
        HeadPolicy::SharedBranch { branch_ref } => branch_ref.clone(),
        HeadPolicy::Draft { view_ref } => view_ref.clone(),
        HeadPolicy::Ephemeral { .. } => format!("refs/atomic/views/{view}"),
    };
    let snapshot = GitSwitchSnapshot::capture(git_repo, &provisional)?;
    let mut receipt_target = provisional.clone();
    // Once the publication is journaled (CB-8B), the ref/HEAD effects belong
    // to the operation's lease recovery: the legacy snapshot rollback must
    // not move them behind the journal's back.
    let mut journaled_publish = false;
    let sync_result = (|| -> CliResult<()> {
        // V1 must run before even content-addressed object writes. A clean
        // state must not carry raw marker text; a conflicted state projects
        // markers deliberately and is resolved below.
        let (policy, _) = {
            let discovered = GitRepository::discover(repo_root)
                .map_err(|error| git_error(format!("cannot discover Git repository: {error}")))?;
            current_conversion_policy(repo_root, &discovered)?
        };
        let conflict_export = resolve_conflict_export(repo, view, &policy, allow_conflicts)?;
        if matches!(conflict_export, ConflictExport::Clean) {
            refuse_conflict_markers(repo, repo_root, view)?;
        }
        let publication = match &conflict_export {
            ConflictExport::Clean => {
                verify_git_publication_mode(repo, repo_root, view, false)?
            }
            ConflictExport::Snapshot { project, .. } => verify_git_publication_with_project(
                repo,
                repo_root,
                view,
                project.clone(),
                false,
            )?,
        };

        let target_ref = match &head_policy {
            // A shared view's commit chain is published on its mapped branch.
            HeadPolicy::SharedBranch { branch_ref } => branch_ref.clone(),
            // Drafts and advanced ephemeral views publish through their
            // private reachability ref; the ref is not a branch (RFC §8.1).
            HeadPolicy::Draft { view_ref } => view_ref.clone(),
            HeadPolicy::Ephemeral { .. } => format!("refs/atomic/views/{view}"),
        };
        receipt_target = target_ref.clone();

        // Ephemeral retained path: the adopted commit is still the exact
        // projection — keep HEAD detached at it and never invent a branch.
        // A conflicted state never retains: the snapshot commit must carry
        // the conflict headers. The journaled publisher owns the HEAD move
        // and the index alignment as leased effects (RFC §7.2, CB-8B ac-3).
        if let (Some(original), ConflictExport::Clean) = (&ephemeral_original, &conflict_export) {
            let commit = git_repo
                .revparse_single(original)
                .map_err(|error| {
                    git_error(format!(
                        "ephemeral view '{view}' refers to commit {original}, which is \
                         no longer present in the Git object database: {error}"
                    ))
                })?
                .peel_to_commit()
                .map_err(|error| {
                    git_error(format!(
                        "ephemeral view '{view}' does not refer to a commit: {error}"
                    ))
                })?;
            if commit.tree_id().as_bytes() == publication.git_tree_oid()?.as_bytes() {
                publication.reobserve(repo, repo_root)?;
                journal_projection_publication(
                    repo,
                    git_repo,
                    view,
                    &target_ref,
                    None,
                    Some(atomic_core::operation::GitRefTarget::Direct(to_core_oid(commit.id())?)),
                    Vec::new(),
                    Some(commit.tree_id()),
                    Some(atomic_repository::ProjectionCheckpointPlan {
                        git_head_symref: None,
                        git_head: to_core_oid(commit.id())?,
                        git_tree: to_core_oid(commit.tree_id())?,
                    }),
                    held_common,
                )?;
                return Ok(());
            }
        }

        let conflict_set_hash = match &conflict_export {
            ConflictExport::Clean => None,
            ConflictExport::Snapshot { hash, .. } => Some(*hash),
        };
        // The verified projection's Git objects are journaled content-addressed
        // resources (CB-8B ac-3, RFC §7.2): every absent blob/tree the
        // projected commit and index will reference is journaled with its
        // canonical identity and retained bytes BEFORE any ODB write, and
        // written only by the receipted executor. Already-landed objects are
        // idempotent Recovered receipts; the tree must be durable before
        // staging/validation or the commit builder can read it.
        let project_objects: Vec<(atomic_core::operation::GitObjectId, git2::ObjectType, Vec<u8>)> =
            publication
                .project
                .git
                .objects
                .iter()
                .map(|(oid, object)| {
                    let kind = match object.kind {
                        GitObjectKind::Blob => git2::ObjectType::Blob,
                        GitObjectKind::Tree => git2::ObjectType::Tree,
                    };
                    (oid.clone(), kind, object.bytes.clone())
                })
                .collect();
        if !project_objects.is_empty() {
            journal_projection_publication(
                repo,
                git_repo,
                view,
                &target_ref,
                None,
                None,
                project_objects,
                None,
                None,
                held_common,
            )?;
        }
        let tree_oid = stage_and_validate_tree(
            repo,
            git_repo,
            repo_root,
            &publication,
            if matches!(conflict_export, ConflictExport::Snapshot { .. }) {
                // The snapshot's marker bytes ARE the verified projection;
                // V1 applies only to clean states.
                ConflictMarkerPolicy::AllowExplicitly
            } else {
                ConflictMarkerPolicy::Refuse
            },
        )?;
        publication.reobserve(repo, repo_root)?;
        // Journal the projection publication BEFORE anything becomes visible
        // (RFC §7.2 step 3/4, CB-8B ac-3, review ATOM::aaron::2): the
        // immutable operation carries every effect's expected-old/new lease —
        // the commit object creation, the index replacement, the ref update,
        // the HEAD move, and the verified checkpoint publish — and each
        // effect classifies its lease immediately before a bounded
        // compare-and-swap under the resource's own Git lock, appending an
        // immutable receipt with the observed after-value. A third value
        // (another writer moved the resource under us) records the
        // rejection, preserves the newer work, and fails closed.
        let (commit_oid, commit_object) = build_switch_projection(
            repo,
            repo_root,
            git_repo,
            &target_ref,
            ephemeral_original.as_deref(),
            view,
            publication.atomic_state(),
            tree_oid,
            &publication,
            record_metadata.as_ref(),
            conflict_set_hash,
        )?;
        let mut objects: Vec<(atomic_core::operation::GitObjectId, git2::ObjectType, Vec<u8>)> =
            Vec::new();
        if let Some((commit_oid, commit_bytes)) = &commit_object {
            objects.push((commit_oid.clone(), git2::ObjectType::Commit, commit_bytes.clone()));
        }
        // A shared view's HEAD is symbolic on its mapped branch (the branch
        // advance is the ref effect); a Draft's HEAD is detached at the
        // projection commit. Both are journaled HEAD effects (RFC §8.1).
        let (intended_head, checkpoint_symref) = match &head_policy {
            HeadPolicy::SharedBranch { branch_ref } => (
                Some(atomic_core::operation::GitRefTarget::Symbolic(branch_ref.clone())),
                Some(branch_ref.clone()),
            ),
            HeadPolicy::Draft { .. } | HeadPolicy::Ephemeral { .. } => (
                Some(atomic_core::operation::GitRefTarget::Direct(commit_oid.clone())),
                None,
            ),
        };
        projection_failpoint("ATOMIC_FAIL_PROJECTION_BEFORE_REF")?;
        journal_projection_publication(
            repo,
            git_repo,
            view,
            &target_ref,
            Some(atomic_core::operation::GitRefTarget::Direct(commit_oid.clone())),
            intended_head,
            objects,
            Some(tree_oid),
            Some(atomic_repository::ProjectionCheckpointPlan {
                git_head_symref: checkpoint_symref,
                git_head: commit_oid.clone(),
                git_tree: to_core_oid(tree_oid)?,
            }),
            held_common,
        )?;
        journaled_publish = true;
        projection_failpoint("ATOMIC_FAIL_PROJECTION_AFTER_EFFECTS")?;
        projection_failpoint("ATOMIC_FAIL_PROJECTION_AFTER_VERIFIED")?;
        if let ConflictExport::Snapshot {
            hash,
            conflicted_paths,
            ..
        } = &conflict_export
        {
            // RFC §8.3 / §6.2: the committed markers are a lossy Git
            // representation. The Atomic conflict remains unresolved and is
            // named explicitly, never silently flattened.
            print_warning(&format!(
                "committed Atomic conflict snapshot on '{view}': {} conflicted path(s) \
                 ({}), conflict set {} — the conflict remains unresolved in Atomic; \
                 resolve it (or use an Atomic remote/binding pack to restore it)",
                conflicted_paths.len(),
                conflicted_paths.join(", "),
                atomic_core::types::Base32::to_base32(hash),
            ));
        }
        Ok(())
    })();

    match sync_result {
        Ok(()) => Ok(ShadowSwitchReceipt {
            target_ref: receipt_target,
            snapshot,
        }),
        Err(error) => {
            if journaled_publish {
                // The ref/HEAD effects are journaled with expected-old/new
                // leases: recovery (on the next writable open) completes or
                // rolls them back under classification, never overwriting
                // newer external work. A snapshot rollback here would move
                // the refs behind the journal's back and fabricate a false
                // third-value divergence.
                return Err(error);
            }
            let rollback_failures = snapshot.restore(git_repo, &receipt_target);
            if rollback_failures.is_empty() {
                Err(error)
            } else {
                Err(git_error(format!(
                    "{error}; additionally failed to restore original Git state: {}",
                    rollback_failures.join("; ")
                )))
            }
        }
    }
}

/// Change metadata attached to a record-path projection (RFC §8.2: the
/// projected commit's message names the change(s) it publishes).
#[derive(Debug, Clone, Copy)]
pub(crate) struct RecordProjectionMetadata {
    pub change_hash: atomic_core::types::Hash,
}

/// Git representation for the view being synchronized (RFC §8.1).
#[derive(Debug, Clone)]
pub(crate) enum HeadPolicy {
    /// Shared view: publish on the mapped branch and check out symbolically.
    SharedBranch { branch_ref: String },
    /// Draft view: detached HEAD at the projection commit, reachable through
    /// the private direct ref `refs/atomic/views/<name>` (never a branch).
    Draft { view_ref: String },
    /// Ephemeral `git/<oid>` view: retain the adopted commit detached while
    /// its projected tree is unchanged; project as a Draft once it advances.
    Ephemeral { original: String },
}

/// The 7-hex-character prefix length used for ephemeral `git/<oid>` views.
const EPHEMERAL_PREFIX_LEN: usize = 7;

fn resolve_head_policy(
    repo: &Repository,
    git_repo: &GitRepository,
    view: &str,
) -> CliResult<HeadPolicy> {
    let scope = repo
        .get_view_info(view)
        .map(|info| info.scope)
        .unwrap_or(atomic_core::pristine::ViewScope::Shared);
    // CB-10A: an explicit publication or rename reconciles the mapping first —
    // once a mapping row exists, its local ref is the authoritative projection
    // target (RFC §8.1). Absent mappings keep the pure scope policy.
    if let Ok(Some(mapping)) = repo.get_ref_mapping(view) {
        if let Some(local_ref) = mapping.local_ref.clone() {
            return match scope {
                atomic_core::pristine::ViewScope::Shared => {
                    Ok(HeadPolicy::SharedBranch { branch_ref: local_ref })
                }
                atomic_core::pristine::ViewScope::Draft => {
                    Ok(HeadPolicy::Draft { view_ref: local_ref })
                }
            };
        }
    }
    match scope {
        atomic_core::pristine::ViewScope::Shared => Ok(HeadPolicy::SharedBranch {
            branch_ref: format!("refs/heads/{view}"),
        }),
        atomic_core::pristine::ViewScope::Draft => {
            if let Some(original) = ephemeral_original_commit(git_repo, view) {
                Ok(HeadPolicy::Ephemeral { original })
            } else {
                Ok(HeadPolicy::Draft {
                    view_ref: format!("refs/atomic/views/{view}"),
                })
            }
        }
    }
}

/// Detect an ephemeral `git/<oid>` view and resolve the commit it adopted.
///
/// Returns `Some(<full oid>)` when the view name carries a valid Git OID
/// prefix and that commit still exists in the object database.
pub(crate) fn ephemeral_original_commit(git_repo: &GitRepository, view: &str) -> Option<String> {
    let oid = view.strip_prefix("git/")?;
    if oid.len() != EPHEMERAL_PREFIX_LEN || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    git_repo
        .revparse_single(oid)
        .ok()?
        .peel_to_commit()
        .ok()
        .map(|commit| commit.id().to_string())
}

/// Build, without writing, the switch-path projection commit (RFC §8.2).
///
/// The commit content is fully determined by the verified publication and
/// the observed target-ref tip, so the immutable operation can journal the
/// commit's object creation — bounded canonical hash plus retained bytes —
/// before the object database write. Returns the commit identity and, when a
/// new object must be created, its exact bytes; `None` means the mapped ref
/// already tips at the projection and no object write is needed.
#[allow(clippy::too_many_arguments)]
fn build_switch_projection(
    repo: &Repository,
    repo_root: &Path,
    git_repo: &GitRepository,
    target_ref: &str,
    default_parent: Option<&str>,
    view: &str,
    state: &str,
    tree_oid: git2::Oid,
    publication: &VerifiedPublication,
    record_metadata: Option<&RecordProjectionMetadata>,
    conflict_set_hash: Option<atomic_core::Hash>,
) -> CliResult<(
    atomic_core::operation::GitObjectId,
    Option<(
        atomic_core::operation::GitObjectId,
        Vec<u8>,
    )>,
)> {
    if publication.view() != view || publication.git_tree_oid()? != tree_oid {
        return Err(git_error(
            "shadow projection does not match verified publication",
        ));
    }
    // One parent by default: the previous projected state on the mapped ref
    // (RFC §8.2). An advanced ephemeral view defaults to its adopted commit.
    // A second parent is only produced by an explicit whole-view merge
    // publication, which the switch path never performs.
    let parent_oid = match git_repo.find_reference(target_ref) {
        Ok(reference) => {
            let commit = reference.peel_to_commit().map_err(|error| {
                git_error(format!(
                    "cannot read target projection ref '{target_ref}': {error}"
                ))
            })?;
            if commit.tree_id() == tree_oid {
                // The mapped ref already tips at the projection: the commit
                // object is durable and no ref movement is journaled.
                return Ok((to_core_oid(commit.id())?, None));
            }
            commit.id()
        }
        Err(error) if error.code() == git2::ErrorCode::NotFound => {
            if let Some(original) = default_parent {
                git_repo
                    .revparse_single(original)
                    .and_then(|object| object.peel_to_commit())
                    .map_err(|error| {
                        git_error(format!(
                            "ephemeral default parent {original} is unreadable: {error}"
                        ))
                    })?
                    .id()
            } else {
                git_repo
                    .head()
                    .and_then(|head| head.peel_to_commit())
                    .map_err(|error| {
                        git_error(format!(
                            "cannot create shadow projection for '{view}' without a current Git HEAD commit: {error}"
                        ))
                    })?
                    .id()
            }
        }
        Err(error) => {
            return Err(git_error(format!(
                "cannot inspect target projection ref '{target_ref}': {error}"
            )))
        }
    };
    let tree = git_repo.find_tree(tree_oid).map_err(|error| {
        git_error(format!(
            "cannot read validated tree for shadow projection on '{view}': {error}"
        ))
    })?;
    // The shadow projection's committer identity prefers the Git config
    // (repo/system/global); when the colocated repository has none — a
    // fresh machine or an isolated test HOME — fall back to the recorded
    // change's own author instead of refusing the projection (review
    // CB-9C: hermetic test environments have no global gitconfig). With no
    // recorded author either, the refusal names the remediation.
    let signature = match git_repo.signature() {
        Ok(signature) => signature,
        Err(error) => {
            let loaded = record_metadata
                .as_ref()
                .and_then(|metadata| repo.load_change(&metadata.change_hash).ok());
            if std::env::var_os("ATOMIC_TRACE_APPLY_CRDT").is_some() {
                eprintln!(
                    "[shadow-projection] git signature missing ({error}); fallback author from change {:?}: loaded={} authors={:?}",
                    record_metadata.as_ref().map(|m| format!("{:?}", m.change_hash)),
                    loaded.is_some(),
                    loaded.as_ref().map(|c| c.hashed.header.authors.len()),
                );
            }
            let author = loaded
                .and_then(|change| change.hashed.header.authors.into_iter().next())
                .filter(|author| !author.name.is_empty());
            // Final fallback: the shadow commit is a DERIVED projection
            // artifact, not a user-authored object. With no Git config and
            // no recorded author (an anonymous record), it commits as the
            // system — the Atomic change keeps its real (possibly empty)
            // authorship, and nothing is attributed to a person.
            let (name, email) = author
                .map(|author| {
                    (
                        author.name,
                        author
                            .email
                            .clone()
                            .unwrap_or_else(|| "atomic@invalid".to_string()),
                    )
                })
                .unwrap_or_else(|| ("atomic".to_string(), "atomic@invalid".to_string()));
            if std::env::var_os("ATOMIC_TRACE_APPLY_CRDT").is_some() {
                eprintln!(
                    "[shadow-projection] using projection committer {name} <{email}>"
                );
            }
            git2::Signature::now(&name, &email).map_err(|error| {
                git_error(format!(
                    "cannot build the shadow-projection signature from the recorded author: {error}"
                ))
            })?
        }
    };
    let set_id = repo.view_set_id(view).map_err(CliError::Repository)?;
    let state_merkle =
        atomic_core::types::Base32::from_base32(state.as_bytes()).ok_or_else(|| {
            git_error(format!(
                "verified view state '{state}' is not a valid Merkle value"
            ))
        })?;
    let parent = atomic_core::operation::GitObjectId::new(
        atomic_core::operation::GitHashAlgorithm::Sha1,
        parent_oid.as_bytes().to_vec(),
    )
    .map_err(|error| git_error(error.to_string()))?;
    let projected_tree = atomic_core::operation::GitObjectId::new(
        atomic_core::operation::GitHashAlgorithm::Sha1,
        tree_oid.as_bytes().to_vec(),
    )
    .map_err(|error| git_error(error.to_string()))?;
    let author = projection_author(repo, repo_root, signature.name(), signature.email());
    let _ = &tree;
    let (commit_oid, commit_bytes) = atomic_repository::build_projected_commit(
        git_repo,
        &projected_tree,
        atomic_repository::ProjectionParents::Single { parent },
        &atomic_repository::ProjectionCommitInput {
            message: projection_message_for(record_metadata, view, state),
            set_id,
            state: state_merkle,
            binding_id: None,
            conflict_set_hash: conflict_set_hash,
            author: projection_author(repo, repo_root, signature.name(), signature.email()),
            timestamp: chrono::Utc::now().timestamp(),
            timestamp_offset: local_utc_offset_minutes(),
        },
    )
    .map_err(|error| {
        git_error(format!(
            "cannot construct the local shadow projection for view '{view}': {error}"
        ))
    })?;
    Ok((
        commit_oid.clone(),
        Some((commit_oid, commit_bytes)),
    ))
}

fn projection_message_for(
    record_metadata: Option<&RecordProjectionMetadata>,
    view: &str,
    state: &str,
) -> String {
    match record_metadata {
        Some(RecordProjectionMetadata { change_hash }) => format!(
            "Atomic record projection\n\nAtomic-View: {view}\nAtomic-State: {state}\nAtomic-Changes: {}\n",
            atomic_core::types::Base32::to_base32(change_hash)
        ),
        None => format!(
            "Atomic shadow switch projection\n\nAtomic-View: {view}\nAtomic-State: {state}\n"
        ),
    }
}

/// Journal the complete projection publication before any Git mutation
/// becomes visible (RFC §7.2 step 3/4, CB-8B ac-3, review ATOM::aaron::2),
/// execute every leased effect — object creation, index replacement under
/// the Git index lock, the ref update and HEAD move through real Git ref
/// transactions, and the verified checkpoint publish — and finalize with the
/// operation-level Verified receipt, which requires every effect receipt plus
/// re-observed state.
#[allow(clippy::too_many_arguments)]
fn journal_projection_publication(
    repo: &Repository,
    git_repo: &GitRepository,
    view: &str,
    ref_name: &str,
    intended_ref: Option<atomic_core::operation::GitRefTarget>,
    intended_head: Option<atomic_core::operation::GitRefTarget>,
    objects: Vec<(atomic_core::operation::GitObjectId, git2::ObjectType, Vec<u8>)>,
    intended_index_tree: Option<git2::Oid>,
    checkpoint_plan: Option<atomic_repository::ProjectionCheckpointPlan>,
    held_common: Option<&atomic_repository::RepositoryCommonLockGuard>,
) -> CliResult<Option<atomic_core::OperationId>> {
    let working_copy = repo.require_working_copy_id().map_err(CliError::Repository)?;
    let evidence = atomic_core::Hash::of(
        format!("atomic:projection-publish:{view}:{ref_name}").as_bytes(),
    );
    let prepared = repo
        .prepare_projection_publication(
            working_copy,
            held_common,
            git_repo,
            ref_name,
            intended_ref,
            intended_head,
            objects,
            intended_index_tree,
            checkpoint_plan,
            evidence,
        )
        .map_err(CliError::Repository)?;
    if prepared.operation_id == atomic_core::OperationId::from_bytes([0u8; 32]) {
        // Every lease already held the intended value: an idempotent no-op.
        return Ok(None);
    }
    let operation_id = prepared.operation_id;
    repo.execute_projection_publish(&prepared, git_repo)
        .map_err(CliError::Repository)?;
    repo.finalize_projection_publish(prepared, git_repo)
        .map_err(CliError::Repository)?;
    Ok(Some(operation_id))
}

/// §5.5 identity resolution for projected commits: Atomic identity first,
/// then the configured `[git.identity]` mapping, then a deterministic
/// foreign DID.
pub(crate) fn projection_author(
    repo: &Repository,
    repo_root: &Path,
    fallback_name: Option<&str>,
    fallback_email: Option<&str>,
) -> atomic_repository::ProjectionAuthor {
    let (name, email) = identity_author().unwrap_or_else(|| {
        (
            fallback_name.unwrap_or("Atomic").to_string(),
            fallback_email.unwrap_or("atomic@localhost").to_string(),
        )
    });
    let did = identity_map(repo, repo_root).did_for_email(&email);
    atomic_repository::ProjectionAuthor {
        name,
        email,
        did,
        agent_did: None,
    }
}

fn identity_author() -> Option<(String, String)> {
    use atomic_identity::IdentityStore;
    let store = IdentityStore::open_default().ok()?;
    let identity = store.get_default().ok()??;
    Some((identity.name.clone(), identity.email.unwrap_or_default()))
}

fn identity_map(repo: &Repository, repo_root: &Path) -> atomic_repository::ProjectionIdentityMap {
    let _ = repo;
    let entries = atomic_config::RepoConfig::load(&repo_root.join(".atomic/config.toml"))
        .map(|config| config.git.identity.mappings)
        .unwrap_or_default();
    atomic_repository::ProjectionIdentityMap::new(entries)
}

fn local_utc_offset_minutes() -> i32 {
    chrono::Local::now().offset().local_minus_utc() / 60
}

fn to_git2_oid(oid: &atomic_core::operation::GitObjectId) -> CliResult<git2::Oid> {
    use atomic_core::operation::GitHashAlgorithm;
    if oid.algorithm() != GitHashAlgorithm::Sha1 {
        return Err(git_error(format!(
            "libgit2 cannot address {:?} object ids",
            oid.algorithm()
        )));
    }
    git2::Oid::from_bytes(oid.as_bytes())
        .map_err(|error| git_error(format!("invalid projected Git oid: {error}")))
}

/// The algorithm-tagged core identity of a libgit2 object id.
fn to_core_oid(oid: git2::Oid) -> CliResult<atomic_core::operation::GitObjectId> {
    atomic_core::operation::GitObjectId::new(
        atomic_core::operation::GitHashAlgorithm::Sha1,
        oid.as_bytes().to_vec(),
    )
    .map_err(|error| git_error(error.to_string()))
}

/// Deterministic projection crash points (opt-in instrumentation, CB-8B
/// ac-3): compiled only with the `adoption-test-injection` feature. Each
/// named environment variable turns the phase boundary into a typed failure
/// so recovery can be exercised before and after every journaled effect.
/// Shipping builds compile the no-op stub: no environment value can alter
/// projection behavior.
fn projection_failpoint(name: &str) -> CliResult<()> {
    #[cfg(feature = "adoption-test-injection")]
    {
        if std::env::var_os(name).is_some() {
            return Err(git_error(format!("debug failpoint: {name}")));
        }
    }
    #[cfg(not(feature = "adoption-test-injection"))]
    {
        let _ = name;
    }
    Ok(())
}

fn align_index_to_tree(
    git_repo: &GitRepository,
    tree_oid: git2::Oid,
    publication: &VerifiedPublication,
) -> CliResult<()> {
    if publication.git_tree_oid()? != tree_oid {
        return Err(git_error("refusing to align index to an unverified tree"));
    }
    let tree = git_repo.find_tree(tree_oid).map_err(|error| {
        git_error(format!(
            "cannot read shadow projection tree while aligning Git index: {error}"
        ))
    })?;
    let mut index = git_repo.index().map_err(|error| {
        git_error(format!(
            "cannot open Git index while aligning shadow view: {error}"
        ))
    })?;
    index
        .read_tree(&tree)
        .and_then(|_| index.write())
        .map_err(|error| {
            git_error(format!(
                "cannot align Git index to the shadow projection tree: {error}"
            ))
        })
}

fn snapshot_reference(git_repo: &GitRepository, name: &str) -> CliResult<ReferenceSnapshot> {
    match git_repo.find_reference(name) {
        Ok(reference) => {
            if let Some(target) = reference.target() {
                Ok(ReferenceSnapshot::Direct(target))
            } else if let Some(target) = reference.symbolic_target() {
                Ok(ReferenceSnapshot::Symbolic(target.to_string()))
            } else {
                Err(git_error(format!(
                    "cannot snapshot Git reference '{name}': it has no target"
                )))
            }
        }
        Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(ReferenceSnapshot::Missing),
        Err(error) => Err(git_error(format!(
            "cannot snapshot Git reference '{name}': {error}"
        ))),
    }
}

fn restore_head(git_repo: &GitRepository, snapshot: &ReferenceSnapshot) -> Result<(), git2::Error> {
    match snapshot {
        ReferenceSnapshot::Missing => match git_repo.find_reference("HEAD") {
            Ok(mut reference) => reference.delete(),
            Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(()),
            Err(error) => Err(error),
        },
        ReferenceSnapshot::Direct(target) => git_repo.set_head_detached(*target),
        ReferenceSnapshot::Symbolic(target) => git_repo.set_head(target),
    }
}

fn restore_reference(
    git_repo: &GitRepository,
    name: &str,
    snapshot: &ReferenceSnapshot,
) -> Result<(), git2::Error> {
    match snapshot {
        ReferenceSnapshot::Missing => match git_repo.find_reference(name) {
            Ok(mut reference) => reference.delete(),
            Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(()),
            Err(error) => Err(error),
        },
        ReferenceSnapshot::Direct(target) => git_repo
            .reference(name, *target, true, "restore failed Atomic shadow switch")
            .map(|_| ()),
        ReferenceSnapshot::Symbolic(target) => git_repo
            .reference_symbolic(name, target, true, "restore failed Atomic shadow switch")
            .map(|_| ()),
    }
}

fn restore_index(git_repo: &GitRepository, bytes: Option<&[u8]>) -> std::io::Result<()> {
    let index_path = git_repo.path().join("index");
    match bytes {
        Some(bytes) => std::fs::write(index_path, bytes),
        None => match std::fs::remove_file(index_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        },
    }
}

fn git_error(message: impl Into<String>) -> CliError {
    CliError::GitError {
        message: message.into(),
    }
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
) -> CliResult<Option<atomic_repository::RepositoryCommonLockGuard>> {
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

/// Validate the already-observed index for a shadow commit and return its tree.
///
/// CB-4B requires the index and worktree to be equivalent before this function
/// can be called. This path therefore does not stage or write an ODB tree; it
/// only enforces the marker/provenance rules and resolves the verified tree.
pub(crate) fn stage_and_validate_tree(
    repo: &Repository,
    git_repo: &GitRepository,
    repo_root: &Path,
    publication: &VerifiedPublication,
    conflict_markers: ConflictMarkerPolicy,
) -> CliResult<git2::Oid> {
    let view = publication.view();
    let working_copy = repo
        .require_working_copy_id()
        .map_err(CliError::Repository)?;

    // ── Rule V1 — no unresolved conflict markers ────────────────────────────
    // Shares `atomic record`'s detector so the two paths cannot disagree.
    if conflict_markers == ConflictMarkerPolicy::Refuse {
        refuse_conflict_markers(repo, repo_root, view)?;
    }

    // Prevention: make sure git is configured to exclude Atomic's shadow /
    // provenance paths before staging, so `git add -A` never picks them up.
    // Best-effort (an unwritable .git/info is caught by the V4 guard below).
    let _ = super::import::ensure_git_shadow_excludes(git_repo.path());

    // CB-4B already proved that the read-only index exactly represents the
    // Atomic project tree. Do not restage or write an ODB tree here: doing so
    // would turn validation itself into a mutation and reopen a TOCTOU window.
    let index = git_repo.index().map_err(|e| CliError::GitError {
        message: format!("Failed to open git index: {}", e),
    })?;

    // ── Rule V4 — no provenance / excluded path may be staged ───────────────
    if let Some(bad) = first_forbidden_shadow_path(&index) {
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

    let tree_oid = publication.git_tree_oid()?;
    git_repo.find_tree(tree_oid).map_err(|error| {
        git_error(format!(
            "verified index tree {tree_oid} is absent from the Git object database: {error}"
        ))
    })?;
    Ok(tree_oid)
}

fn refuse_conflict_markers(repo: &Repository, repo_root: &Path, view: &str) -> CliResult<()> {
    let working_copy = repo
        .require_working_copy_id()
        .map_err(CliError::Repository)?;
    let marker = repo
        .first_working_copy_conflict_marker(working_copy)
        .map_err(CliError::Repository)?;
    // Review ::26 R5: the status-filtered scan above cannot see a file whose
    // RECORDED state carries markers (a record made with
    // --allow-conflict-markers leaves the status clean while the view's
    // canonical content still holds the markers). The switch/shadow boundary
    // therefore ALSO scans the materialized view content: unresolved markers
    // must never become Git evidence, whatever their worktree status.
    let marker = marker.or_else(|| {
        repo.first_view_conflict_marker(working_copy)
            .map_err(CliError::Repository)
            .ok()
            .flatten()
    });
    let Some((path, line)) = marker else {
        return Ok(());
    };
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
    Err(CliError::GitError {
        message: format!(
            "'{}' still contains conflict markers at line {} — resolve the conflict \
             (remove the >>>>>>> / ======= / <<<<<<< lines), or pass \
             --allow-conflict-markers to override. No commit was created.",
            path, line
        ),
    })
}

fn write_project_tree(git: &GitRepository, project: &ProjectTree) -> CliResult<git2::Oid> {
    if project.git.algorithm != GitHashAlgorithm::Sha1 {
        return Err(git_error(format!(
            "libgit2 cannot safely publish {:?} Atomic projections",
            project.git.algorithm
        )));
    }
    let odb = git
        .odb()
        .map_err(|error| git_error(format!("cannot open Git object database: {error}")))?;
    for (expected, object) in project.git.objects.iter() {
        let kind = match object.kind {
            GitObjectKind::Blob => ObjectType::Blob,
            GitObjectKind::Tree => ObjectType::Tree,
        };
        let written = odb.write(kind, &object.bytes).map_err(|error| {
            git_error(format!(
                "cannot write projected Git {kind:?} object: {error}"
            ))
        })?;
        if written.as_bytes() != expected.as_bytes() {
            return Err(git_error(format!(
                "Git object database returned {written} for projected object {expected:?}"
            )));
        }
    }
    git2::Oid::from_bytes(project.git.root.as_bytes()).map_err(|error| {
        git_error(format!(
            "projected Git tree identity {:?} is unsupported: {error}",
            project.git.root
        ))
    })
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
