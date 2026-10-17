//! Persistent ref/view mappings and three-way reconciliation (RFC §8.1/§8.5,
//! CB-10A).
//!
//! The mapping is durable bookkeeping keyed by the internal view id: it
//! records which Git ref the view maps to and the last mutually observed
//! tips, so reconciliation decides *which side advanced* instead of assuming
//! from ref names or current tips. Mappings are mutable and journaled through
//! the shared operation journal (each write is a `RefMapping` metadata
//! operation with an old→new lease); immutable Git state bindings stay in
//! their own table and never prove containment by themselves.

use atomic_core::pristine::{
    GitCommitClosureTxnT, MutTxnT, RefMapping, RefMappingTxnT, RefSyncStatus, ViewScope, ViewTxnT,
    REF_MAPPING_VERSION,
};
use atomic_core::{Hash, WorkingCopyId};
use git2::Repository as GitRepository;

use super::operation::current_operation_timestamp_ms;
use super::Repository;
use crate::RepositoryError;

/// The observation lease a mapping write is pinned to (CB-10A review R6).
pub(super) enum MappingLease<'a> {
    /// Legacy entry: no caller-side pin; the write is still leased against
    /// the current observation (re-read under the ordered locks).
    Unconditional,
    /// The caller observed ABSENCE: the write refuses if a row appeared.
    ExpectedAbsent,
    /// The caller pinned the exact row it observed.
    Row(Option<&'a RefMapping>),
}

pub(super) mod decision;

pub use decision::{
    classify_three_way, status_for, Containment, RefMappingDecision, ThreeWayAction,
    ThreeWayObservation,
};

/// Upper bound for the "commits Git added" walk so a rewritten/foreign ref
/// cannot make reconciliation walk unbounded history (CB-10A: fail closed,
/// never silently assume).
const MAX_ADDED_COMMITS: usize = 10_000;

/// Scope-specific Git representation of a view (RFC §8.1).
pub fn mapped_local_ref(scope: ViewScope, view: &str) -> Option<String> {
    match scope {
        // Shared views publish on a real branch (symbolic HEAD).
        ViewScope::Shared => Some(format!("refs/heads/{view}")),
        // Drafts publish on the private namespace (detached HEAD).
        ViewScope::Draft => Some(format!("refs/atomic/views/{view}")),
    }
}

/// Ephemeral `git/<oid>` views have no extra ref beyond the original commit
/// (RFC §8.1): their mapping deliberately carries `local_ref: None`.
pub fn is_ephemeral_view_name(view: &str) -> bool {
    let Some(oid) = view.strip_prefix("git/") else {
        return false;
    };
    // The ephemeral name carries a 7-hex-character oid prefix (shadow.rs).
    oid.len() == 7 && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
}

impl Repository {
    /// The stored mapping for `view`, or `None`.
    pub fn get_ref_mapping(&self, view: &str) -> Result<Option<RefMapping>, RepositoryError> {
        let txn = self.pristine.read_txn().map_err(pristine_error)?;
        let Some(view_state) = txn.get_view(view).map_err(pristine_error)? else {
            return Ok(None);
        };
        let Some(bytes) = txn
            .get_ref_mapping_bytes(view_state.id)
            .map_err(pristine_error)?
        else {
            return Ok(None);
        };
        Ok(Some(decode_mapping(&bytes)?))
    }

    /// Every stored mapping in view-id order.
    pub fn list_ref_mappings(&self) -> Result<Vec<RefMapping>, RepositoryError> {
        let txn = self.pristine.read_txn().map_err(pristine_error)?;
        txn.iter_ref_mapping_bytes()
            .map_err(pristine_error)?
            .into_iter()
            .map(|(_, bytes)| decode_mapping(&bytes))
            .collect()
    }

    /// Upsert (or remove, with `None`) the mapping for `view` under a
    /// journaled `RefMapping` metadata operation (CB-10A task 1).
    ///
    /// The lease compares the complete canonical mapping encoding, so a
    /// concurrent mapping move between prepare and apply fails closed.
    pub fn set_ref_mapping(
        &self,
        working_copy: WorkingCopyId,
        view: &str,
        mapping: Option<RefMapping>,
    ) -> Result<Option<atomic_core::OperationId>, RepositoryError> {
        self.set_ref_mapping_under_lease(
            working_copy,
            view,
            MappingLease::Unconditional,
            mapping,
            None,
        )
    }

    /// [`Self::set_ref_mapping`] for a caller that already holds the common
    /// operation lock: the working-copy lock aliases the held guard instead
    /// of contending with itself.
    pub fn set_ref_mapping_with_common(
        &self,
        working_copy: WorkingCopyId,
        view: &str,
        mapping: Option<RefMapping>,
        common: Option<&super::locks::RepositoryCommonLockGuard>,
    ) -> Result<Option<atomic_core::OperationId>, RepositoryError> {
        self.set_ref_mapping_from_observation_with_common(working_copy, view, None, mapping, common)
    }

    /// [`Self::set_ref_mapping`] pinned to the observation the replacement
    /// was built from (CB-10A review R6).
    ///
    /// `built_from` is the exact mapping row the caller read before building
    /// `mapping` (or the tombstone replacement). The row is re-observed under
    /// the ordered operation locks; if it no longer equals that observation —
    /// another working copy published or refreshed in between — the write is
    /// refused with [`RepositoryError::RefMappingObservationMoved`] and
    /// nothing is written, so a stale replacement can never clobber a newer
    /// row under an invented lease.
    pub fn set_ref_mapping_from_observation(
        &self,
        working_copy: WorkingCopyId,
        view: &str,
        built_from: Option<&RefMapping>,
        mapping: Option<RefMapping>,
    ) -> Result<Option<atomic_core::OperationId>, RepositoryError> {
        self.set_ref_mapping_under_lease(
            working_copy,
            view,
            MappingLease::Row(built_from),
            mapping,
            None,
        )
    }

    /// Create the mapping row for `view` ONLY while the row is still absent
    /// under the ordered operation locks (CB-10A review R6: atomic
    /// expected-absence). The A-observes-absent / B-writes / A-stores-stale
    /// race refuses closed: a caller that observed absence can never store a
    /// stale baseline over a row another working copy wrote first.
    pub fn create_ref_mapping_expected_absent(
        &self,
        working_copy: WorkingCopyId,
        view: &str,
        mapping: RefMapping,
    ) -> Result<Option<atomic_core::OperationId>, RepositoryError> {
        self.set_ref_mapping_under_lease(
            working_copy,
            view,
            MappingLease::ExpectedAbsent,
            Some(mapping),
            None,
        )
    }

    /// [`Self::set_ref_mapping_from_observation`] for a caller that already
    /// holds the common operation lock.
    pub fn set_ref_mapping_from_observation_with_common(
        &self,
        working_copy: WorkingCopyId,
        view: &str,
        built_from: Option<&RefMapping>,
        mapping: Option<RefMapping>,
        common: Option<&super::locks::RepositoryCommonLockGuard>,
    ) -> Result<Option<atomic_core::OperationId>, RepositoryError> {
        self.set_ref_mapping_under_lease(
            working_copy,
            view,
            MappingLease::Row(built_from),
            mapping,
            common,
        )
    }

    /// The lease-locked mapping writer behind every public write entry.
    fn set_ref_mapping_under_lease(
        &self,
        working_copy: WorkingCopyId,
        view: &str,
        lease: MappingLease<'_>,
        mapping: Option<RefMapping>,
        common: Option<&super::locks::RepositoryCommonLockGuard>,
    ) -> Result<Option<atomic_core::OperationId>, RepositoryError> {
        // The lease covers the observation (CB-10A review R6): acquire the
        // ordered operation locks BEFORE reading the row, so the observed
        // value this transition is built and leased against cannot move under
        // this process's feet, and observation cannot block behind a pristine
        // writer that does not hold the locks.
        let operation_lock = match common {
            Some(common) => common.alias().try_lock_working_copy(working_copy)?,
            None => self.try_lock_operation(working_copy)?,
        };
        if let crate::OperationHeadState::Diverged(heads) =
            self.consolidate_operation_heads_locked(&operation_lock)?
        {
            return Err(RepositoryError::OperationHeadsDiverged {
                scope: atomic_core::operation::OperationScope::WorkingCopy(working_copy)
                    .to_string(),
                heads: heads.iter().map(ToString::to_string).collect(),
            });
        }
        let observed = self.observe_ref_mapping_bytes_for_view(view)?;
        let observed_mapping = match &observed {
            Some(bytes) => Some(decode_mapping(bytes)?),
            None => None,
        };
        match &lease {
            MappingLease::Unconditional => {}
            MappingLease::ExpectedAbsent => {
                if observed_mapping.is_some() {
                    return Err(RepositoryError::RefMappingObservationMoved {
                        view: view.to_string(),
                    });
                }
            }
            MappingLease::Row(Some(built_from)) => {
                if observed_mapping.as_ref() != Some(built_from) {
                    return Err(RepositoryError::RefMappingObservationMoved {
                        view: view.to_string(),
                    });
                }
            }
            // `built_from=None` means the caller pinned nothing: the write is
            // still leased against the CURRENT observation (re-read under the
            // locks above), never unconditional over a foreign transition.
            MappingLease::Row(None) => {}
        }
        let intended_new = mapping
            .as_ref()
            .map(|mapping| mapping.encode())
            .transpose()
            .map_err(|error| RepositoryError::Serialization(error.to_string()))?;
        if observed == intended_new {
            // Idempotent (and the canonicalization write): there is no
            // transition to journal.
            return Ok(None);
        }
        let target = atomic_core::operation::MetadataTarget::RefMapping {
            view: view.to_string(),
        };
        let expected_old = observed
            .map(atomic_core::operation::MetadataValue::Bytes)
            .unwrap_or(atomic_core::operation::MetadataValue::Absent);
        let expected_new = intended_new
            .map(atomic_core::operation::MetadataValue::Bytes)
            .unwrap_or(atomic_core::operation::MetadataValue::Absent);
        let evidence = Hash::of(
            format!(
                "atomic:ref-mapping:v1\0{view}\0{:?}\0{:?}",
                expected_old, expected_new
            )
            .as_bytes(),
        );
        let state = self.current_working_copy_state(working_copy)?;
        let operation = self.prepare_metadata_operation(
            &operation_lock,
            atomic_core::operation::OperationKind::RefMapping,
            None,
            state.clone(),
            state,
            vec![atomic_core::operation::MetadataTransition {
                target,
                expected_old,
                expected_new,
            }],
            vec![evidence],
            atomic_core::operation::ActorRef::System {
                name: "repository-ref-mapping".to_string(),
            },
            current_operation_timestamp_ms(),
        )?;
        self.apply_operation_metadata_locked(&operation_lock, operation.id())?;
        self.finalize_operation_verified(&operation_lock, operation.id())?;
        Ok(Some(operation.id()))
    }

    /// Acquire the repository-common operation lock for a mapping
    /// observation→write span (CB-10A review R6). Returns `None` when another
    /// operation holds it: callers must retry instead of interleaving.
    pub fn try_lock_ref_mapping_observation(
        &self,
    ) -> Result<Option<super::locks::RepositoryCommonLockGuard>, RepositoryError> {
        match self.try_lock_common_operation() {
            Ok(guard) => Ok(Some(guard)),
            Err(error) if error.is_lock_contended() => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Read the stored raw mapping bytes for `view` through a write
    /// transaction so the lease observation matches the metadata executor
    /// exactly.
    fn observe_ref_mapping_bytes_for_view(
        &self,
        view: &str,
    ) -> Result<Option<Vec<u8>>, RepositoryError> {
        let txn = self.pristine.write_txn().map_err(pristine_error)?;
        let observed = super::operation::observe_ref_mapping_value(&txn, view)?;
        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(match observed {
            atomic_core::operation::MetadataValue::Bytes(bytes) => Some(bytes),
            _ => None,
        })
    }

    /// Whether the view's effective change filter contains every hash in
    /// `changes` (CB-10A: closure containment, never SetId/name similarity).
    pub fn view_contains_changes(
        &self,
        view: &str,
        changes: &[Hash],
    ) -> Result<bool, RepositoryError> {
        let txn = self.pristine.read_txn().map_err(pristine_error)?;
        let view_state = txn.get_view(view).map_err(pristine_error)?.ok_or_else(|| {
            RepositoryError::ViewNotFound {
                name: view.to_string(),
            }
        })?;
        let visibility = super::operation::visible_change_hashes(&txn, &view_state)?;
        Ok(changes.iter().all(|change| visibility.contains(change)))
    }

    /// The complete closure a Git commit is bound to (verified interpretation,
    /// CB-9B R1), or `None` when the commit has no persisted closure.
    pub fn bound_closure_for_commit(
        &self,
        sha: &str,
    ) -> Result<Option<Vec<Hash>>, RepositoryError> {
        let txn = self.pristine.read_txn().map_err(pristine_error)?;
        txn.get_git_commit_closure(sha).map_err(pristine_error)
    }

    /// Mark `view`'s mapping `Unrepresentable` (deleted Shared view, branch
    /// untouched), or remove a Draft/ephemeral mapping row entirely.
    ///
    /// The replacement is pinned to the row observed here (CB-10A review R6),
    /// and a Shared tombstone keeps the persisted `local_ref`: the ref name is
    /// the only durable association a tombstone diagnostic can show (review
    /// R5), so it is never erased.
    pub fn reconcile_mapping_after_view_delete(
        &self,
        working_copy: WorkingCopyId,
        view: &str,
        scope: ViewScope,
    ) -> Result<Option<atomic_core::OperationId>, RepositoryError> {
        let observed = self.get_ref_mapping(view)?;
        match scope {
            ViewScope::Shared => {
                let mut mapping = observed
                    .clone()
                    .unwrap_or_else(|| fallback_mapping(view, scope));
                mapping.status = RefSyncStatus::Unrepresentable;
                self.set_ref_mapping_from_observation(
                    working_copy,
                    view,
                    observed.as_ref(),
                    Some(mapping),
                )
            }
            ViewScope::Draft => {
                self.set_ref_mapping_from_observation(working_copy, view, observed.as_ref(), None)
            }
        }
    }

    /// Prove whether Atomic's effective filter contains the bound closure of
    /// every commit Git added (RFC §8.5 "Atomic contains Git's bound closure").
    ///
    /// `Some(true)`/`Some(false)` are positive/negative proofs; `None` means an
    /// added commit carries no persisted verified closure, so containment
    /// cannot be proven either way (ref movement is storage evidence, not
    /// identity).
    pub fn atomic_contains_git_closure(
        &self,
        view: &str,
        added_commits: &[String],
    ) -> Result<Option<bool>, RepositoryError> {
        if std::env::var_os("ATOMIC_TRACE_MAPPING").is_some() {
            for sha in added_commits {
                let closure = self.bound_closure_for_commit(sha)?;
                eprintln!(
                    "[mapping-trace] sha={sha} closure={:?}",
                    closure.map(|c| c.len())
                );
            }
        }
        if added_commits.is_empty() {
            // No Git-side movement: containment is trivially satisfied.
            return Ok(Some(true));
        }
        let mut contains = Some(true);
        for sha in added_commits {
            match self.bound_closure_for_commit(sha)? {
                Some(closure) if contains == Some(true) => {
                    if !self.view_contains_changes(view, &closure)? {
                        contains = Some(false);
                    }
                }
                Some(_) => {}
                None => return Ok(None),
            }
        }
        Ok(contains)
    }
}

fn decode_mapping(bytes: &[u8]) -> Result<RefMapping, RepositoryError> {
    RefMapping::decode(bytes).map_err(|error| RepositoryError::Serialization(error.to_string()))
}

/// The exact Git-side movement between the last mutually observed tip and the
/// current mapped-ref tip (CB-10A containment input, review R2).
///
/// The walk is bounded: it stops at the last mutually observed tip, at a
/// missing commit object, or at `MAX_ADDED_COMMITS` commits (a
/// rewritten/foreign ref must not make reconciliation walk unbounded history).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddedCommits {
    /// The mapped ref sits exactly at the last observed tip (or neither side
    /// is observable): there is no Git movement to account for.
    NoMovement,
    /// The walk reached the observed baseline: the accumulated set is exactly
    /// `reachable(tip) − reachable(observed)`.
    Added(Vec<String>),
    /// The exact movement cannot be established (invalid/missing tip, no
    /// reachable baseline, missing object, budget exhaustion). Containment is
    /// unprovable — never a positive proof.
    Unprovable(&'static str),
}

pub fn commits_added_since(
    git: &GitRepository,
    observed: Option<&str>,
    tip: Option<&str>,
) -> AddedCommits {
    let tip_oid = match tip.and_then(|hex| git2::Oid::from_str(hex).ok()) {
        Some(oid) => oid,
        None => {
            if observed.is_none() {
                // Neither side is observable (fresh draft baseline): nothing
                // moved as far as the mapping can tell.
                return AddedCommits::NoMovement;
            }
            // The mapped ref vanished or is unreadable while the mapping
            // observed a tip: the movement cannot be characterized. A
            // malformed tip hex lands here too (CB-10A review R2: the helper
            // never reports NoMovement for an unreadable tip).
            return AddedCommits::Unprovable("the mapped ref tip is missing or unreadable");
        }
    };
    let observed_oid = match observed.and_then(|hex| git2::Oid::from_str(hex).ok()) {
        Some(oid) => oid,
        None => {
            // No mutually observed baseline: the walk cannot be bounded, so no
            // added-commit set is provable (fail closed to unprovable).
            return AddedCommits::Unprovable("no mutually observed baseline tip");
        }
    };
    if tip_oid == observed_oid {
        return AddedCommits::Added(Vec::new());
    }
    // CB-10A review R2: the added set is the complete set difference
    // `reachable(tip) − reachable(observed)`. A one-sided walk that merely
    // stops at the baseline admits EXCLUDED ANCESTORS on merge shapes
    // (parents reachable from both sides whose path to the baseline does not
    // pass through the observed commit itself), so the baseline closure is
    // collected too — under the same bound — and subtracted. A missing object
    // or budget exhaustion on EITHER side is `Unprovable`, never a partial
    // positive.
    let mut baseline = std::collections::BTreeSet::new();
    {
        let mut pending = vec![observed_oid];
        while let Some(oid) = pending.pop() {
            if !baseline.insert(oid) {
                continue;
            }
            if baseline.len() > MAX_ADDED_COMMITS {
                return AddedCommits::Unprovable("the bounded baseline-closure walk was exhausted");
            }
            let Ok(commit) = git.find_commit(oid) else {
                return AddedCommits::Unprovable(
                    "a commit below the observed baseline is unreadable",
                );
            };
            for parent in commit.parent_ids() {
                pending.push(parent);
            }
        }
    }
    let mut added = Vec::new();
    let mut visited = std::collections::BTreeSet::new();
    let mut baseline_reached = false;
    let mut pending = vec![tip_oid];
    while let Some(oid) = pending.pop() {
        if !visited.insert(oid) {
            continue;
        }
        if baseline.contains(&oid) {
            // In the observed closure: excluded from the added set; do not
            // descend further (everything below is excluded too).
            if oid == observed_oid {
                baseline_reached = true;
            }
            continue;
        }
        let Ok(commit) = git.find_commit(oid) else {
            // A missing object breaks the walk: partial results are not a
            // complete proof.
            return AddedCommits::Unprovable(
                "a commit between the tip and the observed baseline is unreadable",
            );
        };
        added.push(commit.id().to_string());
        if added.len() >= MAX_ADDED_COMMITS {
            // Bounded walk exceeded: the caller must not claim containment.
            return AddedCommits::Unprovable("the bounded added-commit walk was exhausted");
        }
        for parent in commit.parent_ids() {
            pending.push(parent);
        }
    }
    if !baseline_reached {
        return AddedCommits::Unprovable("the observed baseline is not an ancestor of the tip");
    }
    added.sort();
    AddedCommits::Added(added)
}

/// Prove whether Git's added commits carry Atomic's exported projection (RFC
/// §8.5 "Git contains Atomic's"), CB-10A review R1.
///
/// The only proof is a **state-bound export**: `last_exported` names an added
/// commit and `last_exported_state` is exactly the current Atomic state, i.e.
/// the mapping records a verified export of the current state to that commit.
/// Commit message headers (`Atomic-State:`) are lookup hints at most and are
/// no longer proof sources, and an export taken at an older state is obsolete.
///
/// `None` means the proof cannot be made.
pub fn git_contains_atomic_export(
    added_commits: &[String],
    last_exported: Option<&str>,
    last_exported_state: Option<&str>,
    current_atomic: &str,
) -> Option<bool> {
    if added_commits.is_empty() {
        // CB-10A review R2: an empty walk (Git sits at the observed baseline)
        // proves containment ONLY when a verified export binding covers the
        // current state — Git shows the exported projection because the
        // export was bound to exactly this state. An unbound or stale export
        // proves nothing: `None`, never a bare `true`.
        return match (last_exported, last_exported_state) {
            (Some(_), Some(state)) if state == current_atomic => Some(true),
            _ => None,
        };
    }
    let (exported, exported_state) = match (last_exported, last_exported_state) {
        (Some(exported), Some(state)) => (exported, state),
        // An unbound export can never prove containment.
        _ => return None,
    };
    if exported_state != current_atomic {
        // The cached export represents an older Atomic state: obsolete.
        return None;
    }
    if added_commits.iter().any(|sha| sha == exported) {
        return Some(true);
    }
    None
}

fn pristine_error(error: atomic_core::pristine::PristineError) -> RepositoryError {
    RepositoryError::Database(error.to_string())
}

/// A minimal `Unrepresentable` mapping for a view that has no stored row but
/// must be represented in mapping-scoped output (deleted Shared view).
fn fallback_mapping(view: &str, scope: ViewScope) -> RefMapping {
    RefMapping {
        version: REF_MAPPING_VERSION,
        view_id: 0,
        view_name: view.to_string(),
        scope: scope as u8,
        local_ref: None,
        remote: None,
        last_observed_local: None,
        last_observed_remote: None,
        last_exported: None,
        last_exported_state: None,
        last_observed_atomic: None,
        status: RefSyncStatus::Unrepresentable,
    }
}

/// Shared sample-mapping builder for decision tests.
pub mod tests_support {
    use super::*;

    pub fn sample_mapping(
        view: &str,
        local_ref: Option<&str>,
        last_observed_local: Option<&str>,
        last_observed_atomic: Option<&str>,
    ) -> RefMapping {
        RefMapping {
            version: REF_MAPPING_VERSION,
            view_id: 1,
            view_name: view.to_string(),
            scope: ViewScope::Shared as u8,
            local_ref: local_ref.map(str::to_string),
            remote: None,
            last_observed_local: last_observed_local.map(str::to_string),
            last_observed_remote: None,
            last_exported: None,
            last_exported_state: None,
            last_observed_atomic: last_observed_atomic.map(str::to_string),
            status: RefSyncStatus::Synchronized,
        }
    }
}
#[cfg(test)]
mod containment_tests {
    use super::*;
    use git2::Signature;

    fn fixture_repo() -> (tempfile::TempDir, GitRepository) {
        let dir = tempfile::TempDir::new().unwrap();
        let git = GitRepository::init(dir.path()).unwrap();
        {
            let mut config = git.config().unwrap();
            config.set_str("user.name", "CB-10A Tests").unwrap();
            config.set_str("user.email", "cb10a@example.com").unwrap();
        }
        (dir, git)
    }

    fn commit_on(git: &GitRepository, message: &str, parents: &[git2::Oid]) -> git2::Oid {
        let tree_id = if parents.is_empty() {
            let builder = git.treebuilder(None).unwrap();
            builder.write().unwrap()
        } else {
            git.find_commit(parents[0]).unwrap().tree_id()
        };
        let tree = git.find_tree(tree_id).unwrap();
        let signature = Signature::now("CB-10A Tests", "cb10a@example.com").unwrap();
        let parents: Vec<git2::Commit> = parents
            .iter()
            .map(|oid| git.find_commit(*oid).unwrap())
            .collect();
        let parents: Vec<&git2::Commit> = parents.iter().collect();
        // No ref update: the probe builds arbitrary shapes.
        git.commit(None, &signature, &signature, message, &tree, &parents)
            .unwrap()
    }

    /// CB-10A review R2: the four-commit A→B probe. With observed = B
    /// (A ← B) and tip = M (a merge of B and C, where C descends from A),
    /// the added set is EXACTLY {M, C} — the naive one-sided walk admitted
    /// the excluded ancestor A (reachable from B) and failed closed instead.
    #[test]
    fn merge_shape_probe_yields_exactly_the_set_difference() {
        let (_dir, git) = fixture_repo();
        let a = commit_on(&git, "A", &[]);
        let b = commit_on(&git, "B", &[a]);
        let c = commit_on(&git, "C", &[a]);
        let m = commit_on(&git, "M", &[b, c]);

        let added = commits_added_since(
            &git,
            Some(b.to_string().as_str()),
            Some(m.to_string().as_str()),
        );
        let mut expected = vec![m.to_string(), c.to_string()];
        expected.sort();
        assert_eq!(
            added,
            AddedCommits::Added(expected),
            "the added set must be exactly reachable(M) − reachable(B), with no \
             excluded ancestors"
        );
    }

    /// R2: a malformed tip is `Unprovable`, never `NoMovement`.
    #[test]
    fn malformed_tip_is_unprovable_not_no_movement() {
        let (_dir, git) = fixture_repo();
        let a = commit_on(&git, "A", &[]);
        let added = commits_added_since(&git, Some(a.to_string().as_str()), Some("not-a-hex-oid"));
        assert!(
            matches!(added, AddedCommits::Unprovable(_)),
            "an unreadable tip must fail closed, got {added:?}"
        );
    }

    /// R2: missing-ODB e2e — a commit between the tip and the baseline whose
    /// object is unreadable makes the movement unprovable (never partial).
    #[test]
    fn missing_object_breaks_the_walk_to_unprovable() {
        let (dir, git) = fixture_repo();
        let a = commit_on(&git, "A", &[]);
        let b = commit_on(&git, "B", &[a]);
        let c = commit_on(&git, "C", &[b]);
        // Corrupt the ODB: remove B's loose object file, then reopen the
        // repository (libgit2 caches the ODB on the handle).
        let objects = dir.path().join(".git/objects");
        let hex = b.to_string();
        let object_path = objects.join(&hex[..2]).join(&hex[2..]);
        std::fs::remove_file(&object_path).expect("remove the commit object");
        drop(git);
        let git = GitRepository::open(dir.path()).unwrap();
        let added = commits_added_since(
            &git,
            Some(a.to_string().as_str()),
            Some(c.to_string().as_str()),
        );
        assert!(
            matches!(added, AddedCommits::Unprovable(_)),
            "a missing object must fail closed, got {added:?}"
        );
    }

    /// R2: budget exhaustion on excluded history is `Unprovable` without
    /// walking forever (bounded on both sides).
    #[test]
    fn bounded_walk_exhaustion_is_unprovable() {
        let (_dir, git) = fixture_repo();
        // A chain with a shallow observed baseline and a tip far beyond the
        // bound: the tip-side walk exceeds MAX_ADDED_COMMITS and must fail
        // closed instead of returning a partial set.
        let root = commit_on(&git, "root", &[]);
        let mut deep = root;
        for index in 0..(MAX_ADDED_COMMITS + 10) {
            deep = commit_on(&git, &format!("deep {index}"), &[deep]);
        }
        let observed = commit_on(&git, "observed", &[root]);
        let added = commits_added_since(
            &git,
            Some(observed.to_string().as_str()),
            Some(deep.to_string().as_str()),
        );
        assert!(
            matches!(added, AddedCommits::Unprovable(_)),
            "an over-budget walk must be unprovable, got {added:?}"
        );
    }

    /// R2: an empty added set proves `git contains atomic` ONLY through a
    /// bound export of the current state — never a bare `true`.
    #[test]
    fn empty_added_set_requires_a_bound_export() {
        let state = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        // Unbound export: cannot prove containment.
        assert_eq!(
            git_contains_atomic_export(&[], None, None, state),
            None,
            "an empty slice with no export binding must not report true"
        );
        // Stale binding: obsolete.
        assert_eq!(
            git_contains_atomic_export(&[], Some("a"), Some("OLDER"), state),
            None
        );
        // Bound to the current state: provable.
        assert_eq!(
            git_contains_atomic_export(&[], Some("a"), Some(state), state),
            Some(true)
        );
        // Non-empty added set without the exported commit: unprovable.
        assert_eq!(
            git_contains_atomic_export(&["x".to_string()], Some("a"), Some(state), state),
            None
        );
    }
}
