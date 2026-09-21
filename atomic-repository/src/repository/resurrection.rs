//! CB-6C: exact resurrection and the checked Git SHA cache.
//!
//! A valid binding plus a verified closure restores the **original** Atomic
//! history — exact change hashes and bytes, context dependencies, causal
//! frontiers, inode/trunk/branch/leaf identities, attributes, persisted
//! conflicts, provenance roots, ordered membership, Merkle state, and SetId —
//! instead of manufacturing an approximation from Git snapshots
//! (RFC §5.1–5.2, §8.2–8.3, §12.4).
//!
//! The proof is mandatory and isolated (RFC §5.2, §12.5):
//!
//! 1. the binding verifies cryptographically and against the live Git object
//!    database (a lost commit is restored from the binding's preserved raw
//!    bytes — digest-checked first, never re-signed or reconstructed);
//! 2. the closure is completed through CB-6B's
//!    [`Repository::fetch_binding_closure`] and re-validated (dependency
//!    completeness, cycles, depth, `closure_root` recomputation);
//! 3. a `ResurrectBinding` operation is journaled with per-change view
//!    membership leases before any visible effect;
//! 4. the isolated builder applies the closure with the normal graph writers
//!    inside one pristine write transaction **without any view membership**,
//!    then recomputes the full projected tree under the current conversion
//!    policy from that same transaction;
//! 5. the recomputed Git tree must equal the binding's bound tree and the
//!    recomputed SetId must equal the binding's SetId; only then is
//!    membership published in binding Merkle order inside the same
//!    transaction, landing exactly on the binding's Merkle state.
//!
//! Any mismatch aborts the transaction: no membership, no checkpoint, no
//! worktree change, no blessing of untrusted provenance. The prepared
//! operation is inverted through the immutable recovery path, so the
//! rejection is durable, recoverable evidence and retries obey leases.
//! Contended attempts (Git changed under the proof) retry under a
//! three-attempt guard (RFC §7.1 step 8).
//!
//! The `GIT_SHA_INDEX` table and legacy trailers/tags are **checked lookup
//! candidates only** ([`GitShaResolution`]): a cache hit may shortlist a
//! candidate, but adoption is always tied to a validated immutable binding
//! and an independently recomputed closure/tree proof. SetId alone, a
//! view-name hint, a trailer, or an index row can never authorize identity.

use atomic_core::operation::{
    ActorRef, MetadataTarget, MetadataTransition, RepoStateRef, ViewStateRef, OperationKind,
};
use atomic_core::pristine::{GraphTxnT, MutTxnT, ViewTxnT};
use atomic_core::types::{Base32, Hash};
use atomic_core::verify_causal_frontier;

use crate::git_binding::{
    evaluate_binding_trust, validate_binding_closure, verify_binding_cryptography,
    verify_binding_content, BindingClosureError, BindingPackError, BindingPackLimits,
    ClosureChangeSource, ClosureValidation, GitObjectFormat, GitStateBinding,
};
use crate::repository::conflict_object::ConflictSetObject;
use crate::repository::project_tree::ConversionPolicy;
use crate::repository::set_id::ViewIdentity;

use super::{Repository, RepositoryError};

/// The maximum number of contended attempts before refusing (RFC §7.1).
pub(crate) const RESURRECTION_MAX_ATTEMPTS: u8 = 3;

/// A prepared conflict-snapshot restore: the decoded complete conflict
/// object, its verified identity, and the marker bytes read from the bound
/// snapshot tree.
pub(super) struct ConflictSnapshotRestore {
    pub(super) object: ConflictSetObject,
    pub(super) hash: atomic_core::Hash,
    pub(super) marker_bytes: std::collections::BTreeMap<String, Vec<u8>>,
}

/// Parse the `atomic-conflict <base32>` header of a binding's bound commit.
fn bound_commit_conflict_hash(
    git: &git2::Repository,
    binding: &GitStateBinding,
) -> Result<Option<atomic_core::Hash>, RepositoryError> {
    let commit_oid = git2::Oid::from_bytes(binding.payload().git_commit.as_bytes())
        .map_err(|error| RepositoryError::ResurrectionRejected {
            id: binding.id().to_hex(),
            reason: format!("the bound commit OID is invalid: {error}"),
        })?;
    let commit = git.find_commit(commit_oid).map_err(|error| {
        RepositoryError::ResurrectionRejected {
            id: binding.id().to_hex(),
            reason: format!("the bound commit is unreadable: {error}"),
        }
    })?;
    let message = commit.message().unwrap_or("");
    for line in message.lines() {
        if let Some(value) = line.trim().strip_prefix("atomic-conflict ") {
            return Ok(atomic_core::types::Merkle::from_base32(value.trim().as_bytes()));
        }
    }
    Ok(None)
}

fn git2_oid_of(oid: &atomic_core::operation::GitObjectId) -> Result<git2::Oid, RepositoryError> {
    git2::Oid::from_bytes(oid.as_bytes()).map_err(|error| {
        RepositoryError::InvalidOperation {
            message: format!("invalid Git OID: {error}"),
        }
    })
}

/// How the resurrection compared the recomputed projection to the binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionProof {
    /// The recomputed Git tree OID of the projected closure.
    pub projected_tree: atomic_core::operation::GitObjectId,
    /// The binding's bound tree (must equal `projected_tree`).
    pub bound_tree: atomic_core::operation::GitObjectId,
    /// The recomputed additive SetId over the closure.
    pub set_id: atomic_core::types::SetId,
    /// The final Merkle state after publishing membership in binding order.
    pub merkle_state: atomic_core::types::Merkle,
}

/// Outcome of one exact resurrection (CB-6C).
#[derive(Debug, Clone)]
pub struct ExactResurrection {
    /// The binding that was resurrected.
    pub binding_id: crate::git_binding::BindingId,
    /// Changes newly referenced into the view, in binding Merkle order.
    pub inserted: Vec<Hash>,
    /// Closure changes that were already members of the view.
    pub already_present: Vec<Hash>,
    /// The bound commit was absent from the Git ODB and was restored from the
    /// binding's preserved raw bytes (after digest verification).
    pub restored_raw_commit: bool,
    /// Provenance root hashes carried by the binding (§5.6: hashes only).
    pub provenance_roots: Vec<Hash>,
    /// Attestation root hashes carried by the binding (§5.6: hashes only).
    pub attestation_roots: Vec<Hash>,
    /// Whether the binding's signer is trusted under the configured policy.
    /// Untrusted signers resurrect verified content; their provenance claims
    /// stay untrusted and can never satisfy publication gates (§12.4).
    pub provenance_trusted: bool,
    /// The journaled `ResurrectBinding` operation, when this call advanced
    /// the view. Idempotent re-runs return `None`.
    pub operation: Option<atomic_core::types::OperationId>,
    /// The final joint identity of the target view.
    pub identity: ViewIdentity,
    /// The projection proof that gated publication.
    pub proof: ProjectionProof,
}

/// What a Git SHA lookup may conclude (checked cache only, RFC §5.4/§12.8).
#[derive(Debug, Clone, PartialEq)]
pub enum GitShaResolution {
    /// No binding candidate and no usable index row: the cold-cache result.
    Cold,
    /// A validated immutable binding covers this commit. `index_candidate`
    /// records whether the index contributed the tie; the verification is
    /// identical either way, which is what makes the cache checked.
    VerifiedBinding {
        binding: GitStateBinding,
        index_candidate: bool,
    },
    /// A candidate existed (index row or stored binding) but identity could
    /// not be resolved: explicit refusal for adoption, never a silent skip.
    Unresolved { detail: String },
}

/// The local change store as a closure object source.
struct LocalClosureSource<'a> {
    repo: &'a Repository,
}

impl ClosureChangeSource for LocalClosureSource<'_> {
    fn get(
        &mut self,
        hash: &Hash,
    ) -> Result<Option<atomic_core::change::Change>, BindingClosureError> {
        if !self.repo.has_change(hash) {
            return Ok(None);
        }
        self.repo.load_change(hash).map(Some).map_err(|error| {
            BindingClosureError::Quarantine {
                hash: hash.to_base32(),
                source: BindingPackError::ChangeDecode {
                    key: hash.to_base32(),
                    reason: error.to_string(),
                },
            }
        })
    }
}

fn pristine_map(error: atomic_core::pristine::PristineError) -> RepositoryError {
    RepositoryError::Database(error.to_string())
}

fn database_map(error: impl std::fmt::Display) -> RepositoryError {
    RepositoryError::Database(error.to_string())
}

/// Deterministic fault switch for tests only: when set, the isolated build
/// fails after the `ResurrectBinding` intent was journaled, so rejection and
/// recovery can be exercised end to end.
#[cfg(test)]
thread_local! {
    pub(crate) static RESURRECTION_APPLY_FAULT: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

/// Deterministic contention hook for tests only: a positive counter makes the
/// first `n` attempts observe a Git lease mismatch after the projection
/// proof, exercising the three-attempt guard.
#[cfg(test)]
thread_local! {
    pub(crate) static RESURRECTION_CONTENTION_ATTEMPTS: std::cell::Cell<u8> =
        const { std::cell::Cell::new(0) };
}

/// How one attempt ended.
enum AttemptOutcome {
    /// Nothing to do: the view already carries exactly the bound membership.
    Noop {
        identity: ViewIdentity,
        proof: ProjectionProof,
    },
    /// The closure was applied, proven, and published.
    Applied(AppliedResurrection),
}

/// The outcome of a successful guarded attempt.
struct AppliedResurrection {
    inserted: Vec<Hash>,
    already_present: Vec<Hash>,
    proof: ProjectionProof,
    identity: ViewIdentity,
    operation: Option<atomic_core::types::OperationId>,
}

/// Why one attempt ended. Contended attempts retry (bounded); rejections are
/// terminal for this call and surfaced as typed errors.
enum AttemptFailure {
    Contended,
    Rejected(RepositoryError),
}

impl From<RepositoryError> for AttemptFailure {
    fn from(error: RepositoryError) -> Self {
        if matches!(error, RepositoryError::ResurrectionLeaseChanged) {
            AttemptFailure::Contended
        } else {
            AttemptFailure::Rejected(error)
        }
    }
}

impl Repository {
    /// Restore the exact bound state of a verified binding into `view`.
    ///
    /// Restores the original change hashes/bytes, context dependencies,
    /// causal frontiers, semantic identities, attributes, persisted
    /// conflicts, provenance roots, ordered membership, Merkle state, and
    /// SetId. No synthesized substitute is ever produced for a valid known
    /// binding (RFC §12.4).
    pub fn resurrect_binding_exact(
        &self,
        git: &git2::Repository,
        binding: &GitStateBinding,
        view: &str,
        remote: Option<&mut dyn crate::BindingChangeSource>,
    ) -> Result<ExactResurrection, RepositoryError> {
        let restored_raw_commit = self.verify_bound_binding(git, binding)?;
        let trust = self.evaluate_trust(binding);
        let conflicts = self.prepare_conflict_snapshot_restore(git, binding)?;
        self.acquire_closure(git, binding, remote)?;
        let policy = ConversionPolicy::new(git_algorithm(binding.payload().git_object_format));
        let mut attempts: u8 = 0;
        loop {
            attempts += 1;
            let lease = self.observe_git_lease(git)?;
            match self.resurrect_attempt(None, git, binding, view, &policy, lease, conflicts.as_ref()) {
                Ok(outcome) => {
                    return Ok(exact_outcome(
                        binding, outcome, restored_raw_commit, trust, None,
                    ));
                }
                Err(AttemptFailure::Contended) if attempts < RESURRECTION_MAX_ATTEMPTS => continue,
                Err(AttemptFailure::Contended) => {
                    return Err(RepositoryError::ResurrectionContended {
                        id: binding.id().to_hex(),
                        attempts,
                    });
                }
                Err(AttemptFailure::Rejected(error)) => return Err(error),
            }
        }
    }

    /// [`Self::resurrect_binding_exact`] under an already-held working-copy
    /// operation lock — the CB-7A §7.3 adoption path (CB-5C boundary
    /// contract). The caller owns the lock acquisition order.
    pub(super) fn resurrect_binding_exact_locked(
        &self,
        operation_lock: &super::locks::WorkingCopyOperationLockGuard,
        git: &git2::Repository,
        binding: &GitStateBinding,
        view: &str,
        remote: Option<&mut dyn crate::BindingChangeSource>,
    ) -> Result<ExactResurrection, RepositoryError> {
        let restored_raw_commit = self.verify_bound_binding(git, binding)?;
        let trust = self.evaluate_trust(binding);
        let conflicts = self.prepare_conflict_snapshot_restore(git, binding)?;
        self.acquire_closure(git, binding, remote)?;
        let policy = ConversionPolicy::new(git_algorithm(binding.payload().git_object_format));
        let mut attempts: u8 = 0;
        loop {
            attempts += 1;
            let lease = self.observe_git_lease(git)?;
            match self.resurrect_attempt_locked(
                operation_lock, git, binding, view, &policy, lease, conflicts.as_ref(),
            ) {
                Ok(outcome) => {
                    return Ok(exact_outcome(
                        binding, outcome, restored_raw_commit, trust, None,
                    ));
                }
                Err(AttemptFailure::Contended) if attempts < RESURRECTION_MAX_ATTEMPTS => continue,
                Err(AttemptFailure::Contended) => {
                    return Err(RepositoryError::ResurrectionContended {
                        id: binding.id().to_hex(),
                        attempts,
                    });
                }
                Err(AttemptFailure::Rejected(error)) => return Err(error),
            }
        }
    }

    /// Prepare the complete conflict-object restore for a binding whose bound
    /// commit is an RFC §8.3 conflict snapshot.
    ///
    /// A conflict snapshot commit names `atomic-conflict <hash>`; exact
    /// restoration requires the binding tree to carry the complete conflict
    /// object whose identity hashes to exactly that value. Markers plus a
    /// hash alone are refused; a structurally invalid or hash-divergent pack
    /// is refused; a conflicted path absent from the bound tree (missing
    /// side) is refused.
    pub(super) fn prepare_conflict_snapshot_restore(
        &self,
        git: &git2::Repository,
        binding: &GitStateBinding,
    ) -> Result<Option<ConflictSnapshotRestore>, RepositoryError> {
        let Some(pack) = self.load_binding_conflicts_pack(git, &binding.id())? else {
            // No pack. Refuse only when the bound commit claims to be a
            // conflict snapshot: the header alone is never sufficient.
            if bound_commit_conflict_hash(git, binding)?.is_some() {
                return Err(RepositoryError::ResurrectionRejected {
                    id: binding.id().to_hex(),
                    reason: "the bound commit is a conflict snapshot (atomic-conflict header) \
                             but the binding carries no conflicts.pack; marker bytes plus a \
                             hash alone cannot restore the conflict state"
                        .to_string(),
                });
            }
            return Ok(None);
        };
        let object = ConflictSetObject::decode(&pack)
            .map_err(|error| RepositoryError::ResurrectionRejected {
                id: binding.id().to_hex(),
                reason: format!("the binding's conflicts.pack failed to decode: {error}"),
            })?;
        let hash = object.hash().map_err(|error| RepositoryError::ResurrectionRejected {
            id: binding.id().to_hex(),
            reason: format!("the binding's conflicts.pack failed to hash: {error}"),
        })?;
        let claimed = bound_commit_conflict_hash(git, binding)?.ok_or_else(|| {
            RepositoryError::ResurrectionRejected {
                id: binding.id().to_hex(),
                reason: "the binding carries a conflicts.pack but the bound commit names no \
                         atomic-conflict header; refusing an unanchored pack"
                    .to_string(),
            }
        })?;
        if claimed != hash {
            return Err(RepositoryError::ResurrectionRejected {
                id: binding.id().to_hex(),
                reason: format!(
                    "the conflicts.pack identity {} does not match the bound commit's \
                     atomic-conflict header {}; the pack is stale or forged",
                    hash.to_base32(),
                    claimed.to_base32()
                ),
            });
        }
        object.validate().map_err(|error| RepositoryError::ResurrectionRejected {
            id: binding.id().to_hex(),
            reason: format!("the conflicts.pack is structurally invalid: {error}"),
        })?;
        // Marker bytes come from the bound tree itself: the snapshot commit's
        // tree IS the marker materialization.
        let tree_oid = bound_tree_oid(binding)?;
        let tree = git
            .find_tree(git2_oid_of(&tree_oid)?)
            .map_err(|error| RepositoryError::ResurrectionRejected {
                id: binding.id().to_hex(),
                reason: format!("the bound tree is unreadable: {error}"),
            })?;
        let mut marker_bytes = std::collections::BTreeMap::new();
        for file in &object.files {
            let path = String::from_utf8_lossy(&file.path).to_string();
            let entry = tree.get_path(std::path::Path::new(&path)).map_err(|error| {
                RepositoryError::ResurrectionRejected {
                    id: binding.id().to_hex(),
                    reason: format!(
                        "conflicted path '{path}' is absent from the bound tree (missing side): {error}"
                    ),
                }
            })?;
            if entry.filemode() != i32::from(git2::FileMode::Blob) {
                return Err(RepositoryError::ResurrectionRejected {
                    id: binding.id().to_hex(),
                    reason: format!(
                        "conflicted path '{path}' is not a regular blob in the bound tree \
                         (mode {:o}); mixed kind/name conflicts cannot be restored exactly",
                        entry.filemode()
                    ),
                });
            }
            let blob = git
                .find_blob(entry.id())
                .map_err(|error| RepositoryError::ResurrectionRejected {
                    id: binding.id().to_hex(),
                    reason: format!("conflicted path '{path}' blob is unreadable: {error}"),
                })?;
            marker_bytes.insert(path, blob.content().to_vec());
        }
        Ok(Some(ConflictSnapshotRestore {
            object,
            hash,
            marker_bytes,
        }))
    }

    /// Verify that a freshly restored view carries EXACTLY the packed
    /// conflict state (RFC §8.3 round-trip, CB-8B ac-1).
    ///
    /// The restored view is materialized (persisting its `CONFLICTS` rows),
    /// the complete conflict object is re-captured from the fresh store, and
    /// both are compared in their store-independent normalized form: the same
    /// identities, base/sides, modes/kinds, claimant/name-edge identities,
    /// and graph metadata must come back. Any divergence — flattened sides,
    /// missing claimants, changed modes — refuses.
    pub fn verify_restored_conflict_state(
        &self,
        view_name: &str,
        expected: &crate::repository::ConflictSetObject,
    ) -> Result<(), RepositoryError> {
        let working_copy = self.require_working_copy_id()?;
        if self.desired_view_name(working_copy)? == view_name {
            // Persist the restored view's conflict rows exactly like an
            // ordinary materialize would.
            self.materialize(working_copy)?;
        }
        let captured = self.capture_view_conflict_set(view_name)?;
        match captured {
            Some(actual) if actual.normalized() == expected.normalized() => Ok(()),
            Some(actual) => Err(RepositoryError::ResurrectionRejected {
                id: expected
                    .hash()
                    .map(|hash| hash.to_base32())
                    .unwrap_or_default(),
                reason: format!(
                    "restored conflict state diverges from the packed conflict object: \
                     restored {} file(s)/{} entr{}, packed {} file(s)/{} entr{}",
                    actual.files.len(),
                    actual.entry_count(),
                    if actual.entry_count() == 1 { "y" } else { "ies" },
                    expected.files.len(),
                    expected.entry_count(),
                    if expected.entry_count() == 1 { "y" } else { "ies" },
                ),
            }),
            None => Err(RepositoryError::ResurrectionRejected {
                id: expected
                    .hash()
                    .map(|hash| hash.to_base32())
                    .unwrap_or_default(),
                reason: "the packed conflict object names conflicts but the restored state \
                         has none; sides were flattened during restoration"
                    .to_string(),
            }),
        }
    }

    /// [`Self::resurrect_binding_exact`] under an existing workspace
    /// transaction's ordered operation lock (CB-5C boundary contract).
    pub fn resurrect_binding_exact_under_workspace(
        &self,
        workspace: &crate::WorkspaceTxn,
        git: &git2::Repository,
        binding: &GitStateBinding,
        view: &str,
        remote: Option<&mut dyn crate::BindingChangeSource>,
    ) -> Result<ExactResurrection, RepositoryError> {
        self.resurrect_binding_exact_locked(workspace.operation_lock(), git, binding, view, remote)
    }

    /// Resolve what a Git SHA lookup may conclude (checked cache contract).
    ///
    /// The index and stored bindings only ever *shortlist* candidates; every
    /// returned binding is fully re-verified (cryptography + live Git
    /// content), so injected missing, stale, ambiguous, or malicious index
    /// entries cannot change the outcome relative to a cold cache.
    pub fn resolve_git_sha(
        &self,
        git: &git2::Repository,
        commit: &atomic_core::operation::GitObjectId,
    ) -> Result<GitShaResolution, RepositoryError> {
        // Candidates from stored bindings whose bound commit matches.
        let mut candidates: Vec<GitStateBinding> = Vec::new();
        let mut index_candidate;
        for id in self.binding_ids()? {
            let Ok(Some(binding)) = self.load_binding(&id) else {
                continue;
            };
            let Ok(bound_commit) = binding.payload().git_commit.to_git_object_id() else {
                continue;
            };
            if &bound_commit == commit {
                candidates.push(binding);
            }
        }
        // A cache row may also tie the SHA to a change; when a stored
        // binding's closure contains exactly that change, it is a candidate.
        // The row itself is never adoption evidence — every candidate is
        // still fully re-verified below, which is what makes the cache
        // checked rather than authoritative.
        let sha = crate::repository::synthesis::git_oid_hex(commit);
        index_candidate = self.raw_git_sha_row(&sha)?.is_some();
        if candidates.is_empty() {
            if let Some(change) = self.raw_git_sha_row(&sha)? {
                for id in self.binding_ids()? {
                    let Ok(Some(binding)) = self.load_binding(&id) else {
                        continue;
                    };
                    if binding.payload().ordered_changes.contains(&change) {
                        candidates.push(binding);
                    }
                }
            }
        }
        if candidates.is_empty() {
            return Ok(GitShaResolution::Cold);
        }
        for binding in &candidates {
            if verify_binding_cryptography(binding).is_err() {
                continue;
            }
            if verify_binding_content(git, binding).is_err() {
                continue;
            }
            return Ok(GitShaResolution::VerifiedBinding {
                binding: binding.clone(),
                index_candidate,
            });
        }
        Ok(GitShaResolution::Unresolved {
            detail: format!(
                "{} stored binding(s) name commit {} and all failed verification",
                candidates.len(),
                crate::repository::synthesis::git_oid_hex(commit)
            ),
        })
    }

    /// The checked `GIT_SHA_INDEX` row for one SHA.
    ///
    /// A row is usable only when the mapped change is registered, loadable,
    /// decodes to exactly its claimed identity, and its unhashed provenance
    /// names this SHA. Anything else is a stale/missing/ambiguous/malicious
    /// entry and reports `None` — the same treatment as a cold cache.
    pub fn checked_git_sha(&self, sha: &str) -> Result<Option<Hash>, RepositoryError> {
        use atomic_core::pristine::{GitShaIndexTxnT, GraphTxnT};
        let txn = self.pristine.read_txn().map_err(pristine_map)?;
        let Some(change_id) = txn.get_by_git_sha(sha).map_err(pristine_map)? else {
            return Ok(None);
        };
        let Some(hash) = txn.get_external(change_id).map_err(pristine_map)? else {
            return Ok(None);
        };
        drop(txn);
        if !self.has_change(&hash) {
            return Ok(None);
        }
        let Ok(change) = self.load_change(&hash) else {
            return Ok(None);
        };
        let names_sha = change
            .unhashed
            .as_ref()
            .and_then(|unhashed| unhashed.get("git"))
            .and_then(|git| git.get("sha"))
            .and_then(|value| value.as_str())
            .map(|claimed| claimed == sha)
            .unwrap_or(false);
        if names_sha {
            Ok(Some(hash))
        } else {
            Ok(None)
        }
    }

    /// The raw `GIT_SHA_INDEX` row for one SHA: the mapped change must be
    /// registered, loadable, and decode to exactly its claimed identity.
    /// Used only to *shortlist* binding candidates — never as identity.
    fn raw_git_sha_row(&self, sha: &str) -> Result<Option<Hash>, RepositoryError> {
        use atomic_core::pristine::{GitShaIndexTxnT, GraphTxnT};
        let txn = self.pristine.read_txn().map_err(pristine_map)?;
        let Some(change_id) = txn.get_by_git_sha(sha).map_err(pristine_map)? else {
            return Ok(None);
        };
        let Some(hash) = txn.get_external(change_id).map_err(pristine_map)? else {
            return Ok(None);
        };
        drop(txn);
        if !self.has_change(&hash) {
            return Ok(None);
        }
        match self.load_change(&hash) {
            Ok(_) => Ok(Some(hash)),
            Err(_) => Ok(None),
        }
    }

    /// The checked import skip markers over the whole `GIT_SHA_INDEX`.
    ///
    /// Returns the verified SHAs and the rows that failed validation. Rows
    /// whose change is no longer registered can never become valid again and
    /// are repaired out of the cache; every dropped row makes the importer
    /// treat its commit exactly as a cold cache would.
    pub fn checked_git_sha_markers(&self) -> Result<(Vec<String>, Vec<String>), RepositoryError> {
        use atomic_core::pristine::{GitShaIndexMutTxnT, GitShaIndexTxnT};

        let shas = {
            let txn = self.pristine.read_txn().map_err(pristine_map)?;
            txn.list_git_shas().map_err(pristine_map)?
        };
        let mut verified = Vec::with_capacity(shas.len());
        let mut stale = Vec::new();
        for sha in shas {
            match self.checked_git_sha(&sha)? {
                Some(_) => verified.push(sha),
                None => stale.push(sha),
            }
        }
        // Repair rows whose change is unregistered — they can never become
        // valid again. Rows whose change exists but whose provenance does not
        // name the SHA are reported (and skipped) but left in place.
        let repairable: Vec<String> = stale
            .iter()
            .filter(|sha| {
                // A row is repairable when its mapped change is unregistered:
                // such a row can never verify again.
                let txn = match self.pristine.read_txn().map_err(pristine_map) {
                    Ok(txn) => txn,
                    Err(_) => return false,
                };
                let change_id = match txn.get_by_git_sha(sha) {
                    Ok(value) => value,
                    Err(_) => return false,
                };
                let hash: Option<Hash> = change_id.and_then(|id| match txn.get_external(id) {
                    Ok(value) => value,
                    Err(_) => None,
                });
                match hash {
                    Some(hash) => !self.has_change(&hash),
                    None => true,
                }
            })
            .cloned()
            .collect();
        if !repairable.is_empty() {
            let mut txn = self.pristine.write_txn().map_err(pristine_map)?;
            for sha in &repairable {
                txn.del_git_sha(sha).map_err(pristine_map)?;
            }
            txn.commit().map_err(database_map)?;
        }
        Ok((verified, stale))
    }

    // ── Internals ───────────────────────────────────────────────────────

    /// Cryptography + Git structural verification of one binding, restoring
    /// a lost commit from the binding's preserved raw bytes first.
    fn verify_bound_binding(
        &self,
        git: &git2::Repository,
        binding: &GitStateBinding,
    ) -> Result<bool, RepositoryError> {
        verify_binding_cryptography(binding).map_err(|error| RepositoryError::BindingRejected {
            id: binding.id().to_hex(),
            commit: binding.payload().git_commit.to_hex(),
            reason: format!("cryptography: {error}"),
        })?;
        let restored = self.ensure_bound_commit_readable(git, binding)?;
        verify_binding_content(git, binding).map_err(|error| RepositoryError::BindingRejected {
            id: binding.id().to_hex(),
            commit: binding.payload().git_commit.to_hex(),
            reason: format!("git content: {error}"),
        })?;
        Ok(restored)
    }

    /// Ensure the bound commit is readable from the Git ODB, restoring the
    /// preserved raw bytes when it is absent.
    ///
    /// The raw bytes are written only after their digest re-derives the bound
    /// commit OID. The write is content-addressed (same bytes → same OID →
    /// same object), so an attempt interrupted between the ODB write and the
    /// pristine commit is idempotent under retry. The commit is never
    /// re-signed or reconstructed, and no Git object is ever deleted.
    fn ensure_bound_commit_readable(
        &self,
        git: &git2::Repository,
        binding: &GitStateBinding,
    ) -> Result<bool, RepositoryError> {
        let payload = binding.payload();
        let oid = git2::Oid::from_bytes(payload.git_commit.as_bytes()).map_err(|error| {
            RepositoryError::BindingRejected {
                id: binding.id().to_hex(),
                commit: payload.git_commit.to_hex(),
                reason: format!("malformed bound commit OID: {error}"),
            }
        })?;
        let odb = git.odb().map_err(|error| RepositoryError::BindingRejected {
            id: binding.id().to_hex(),
            commit: payload.git_commit.to_hex(),
            reason: format!("git odb unavailable: {error}"),
        })?;
        if odb.exists(oid) {
            return Ok(false);
        }
        let Some(raw) = &payload.raw_commit_object else {
            return Err(RepositoryError::BindingRejected {
                id: binding.id().to_hex(),
                commit: payload.git_commit.to_hex(),
                reason:
                    "the bound commit is absent from the Git object database and the binding \
                     preserves no raw commit bytes; the identity cannot be restored"
                        .to_string(),
            });
        };
        let digest = crate::git_binding::commit_object_digest(payload.git_object_format, raw)
            .map_err(|error| RepositoryError::BindingRejected {
                id: binding.id().to_hex(),
                commit: payload.git_commit.to_hex(),
                reason: format!("preserved raw commit bytes are malformed: {error}"),
            })?;
        if digest != payload.git_commit {
            return Err(RepositoryError::BindingRejected {
                id: binding.id().to_hex(),
                commit: payload.git_commit.to_hex(),
                reason: format!(
                    "preserved raw commit bytes digest {} does not match the bound commit",
                    digest.to_hex()
                ),
            });
        }
        odb.write(git2::ObjectType::Commit, raw)
            .map_err(|error| RepositoryError::BindingRejected {
                id: binding.id().to_hex(),
                commit: payload.git_commit.to_hex(),
                reason: format!("cannot restore the raw commit bytes into the Git odb: {error}"),
            })?;
        Ok(true)
    }

    fn evaluate_trust(
        &self,
        binding: &GitStateBinding,
    ) -> crate::git_binding::BindingTrustEvaluation {
        let config = atomic_config::RepoConfig::load(&self.root.join(".atomic/config.toml"))
            .unwrap_or_default();
        let repository_identity = config.author.as_ref().and_then(|a| a.identity.as_deref());
        evaluate_binding_trust(
            binding,
            &config.git.trust,
            crate::git_binding::BindingVerificationInput {
                repository_identity,
            },
            true,
        )
    }

    /// Complete the closure (CB-6B) and re-validate it over the local store.
    fn acquire_closure(
        &self,
        git: &git2::Repository,
        binding: &GitStateBinding,
        mut remote: Option<&mut dyn crate::BindingChangeSource>,
    ) -> Result<ClosureValidation, RepositoryError> {
        let limits = BindingPackLimits::default_limits();
        let source: Option<&mut dyn crate::BindingChangeSource> = match remote.as_mut() {
            Some(source) => Some(&mut **source),
            None => None,
        };
        let acquisition = self.fetch_binding_closure(git, binding, source, &limits)?;
        if let crate::ClosureReadiness::Incomplete { missing, reasons } = &acquisition.readiness {
            return Err(RepositoryError::BindingClosureIncomplete {
                id: binding.id().to_hex(),
                count: missing.len(),
                reasons: reasons
                    .iter()
                    .map(|reason| format!("{reason:?}"))
                    .collect::<Vec<_>>()
                    .join("; "),
            });
        }
        let mut source = LocalClosureSource { repo: self };
        validate_binding_closure(binding.payload(), &mut source, &limits).map_err(|error| {
            RepositoryError::ResurrectionRejected {
                id: binding.id().to_hex(),
                reason: format!("closure re-validation refused: {error}"),
            }
        })
    }

    /// The Git lease the proof is bound to: the HEAD target at proof time.
    fn observe_git_lease(
        &self,
        git: &git2::Repository,
    ) -> Result<Option<git2::Oid>, RepositoryError> {
        Ok(git.head().ok().and_then(|head| head.target()))
    }

    /// One guarded attempt: membership analysis, isolated build, proof,
    /// publication, under an already-held ordered operation lock.
    #[allow(clippy::too_many_arguments)]
    fn resurrect_attempt_locked(
        &self,
        operation_lock: &super::locks::WorkingCopyOperationLockGuard,
        git: &git2::Repository,
        binding: &GitStateBinding,
        view_name: &str,
        policy: &ConversionPolicy,
        lease: Option<git2::Oid>,
        conflicts: Option<&ConflictSnapshotRestore>,
    ) -> Result<AttemptOutcome, AttemptFailure> {
        let payload = binding.payload();
        let ordered = &payload.ordered_changes;

        // Membership analysis (read-only): which closure changes are already
        // members, and do the members form a prefix of the binding order?
        let (view_id, base_state, base_count, to_insert, already_present) = {
            let txn = self.pristine.read_txn().map_err(database_map)?;
            let view = txn
                .get_view(view_name)
                .map_err(database_map)?
                .ok_or(RepositoryError::ViewNotFound {
                    name: view_name.to_string(),
                })?;
            let mut to_insert = Vec::with_capacity(ordered.len());
            let mut already_present = Vec::with_capacity(ordered.len());
            for hash in ordered {
                let member = match txn.get_internal(hash) {
                    Ok(Some(change_id)) => txn
                        .get_change_seq(&view, change_id)
                        .map_err(database_map)?
                        .is_some(),
                    _ => false,
                };
                if member {
                    already_present.push(*hash);
                } else {
                    to_insert.push(*hash);
                }
            }
            (view.id, view.state, view.change_count, to_insert, already_present)
        };
        let mut saw_gap = false;
        for hash in ordered.iter() {
            let member = !to_insert.contains(hash);
            if member {
                if saw_gap {
                    return Err(AttemptFailure::Rejected(
                        RepositoryError::ResurrectionRejected {
                            id: binding.id().to_hex(),
                            reason: format!(
                                "view '{view_name}' already references closure members out of \
                                 binding order ({hash}); order restoration is impossible without \
                                 rewriting published membership"
                            ),
                        },
                    ));
                }
            } else {
                saw_gap = true;
            }
        }

        // Idempotent no-op: the view already carries exactly the bound
        // membership. Verify the claims before reporting success.
        if to_insert.is_empty() {
            let identity = self.view_identity(view_name)?;
            if identity.merkle != payload.merkle_state || identity.set_id != payload.set_id {
                return Err(AttemptFailure::Rejected(
                    RepositoryError::ResurrectionRejected {
                        id: binding.id().to_hex(),
                        reason: format!(
                            "view '{view_name}' identity (merkle {}, set {}) diverges from the \
                             binding's (merkle {}, set {}); refusing to bless a foreign membership",
                            identity.merkle,
                            identity.set_id,
                            payload.merkle_state,
                            payload.set_id
                        ),
                    },
                ));
            }
            let project = match conflicts {
                Some(restore) => {
                    let txn = self.pristine.read_txn().map_err(database_map)?;
                    let view = txn
                        .get_view(view_name)
                        .map_err(database_map)?
                        .ok_or(RepositoryError::ViewNotFound {
                            name: view_name.to_string(),
                        })?;
                    self.project_change_closure_with_conflict_markers(
                        &txn,
                        &view,
                        ordered,
                        policy,
                        &restore.marker_bytes,
                    )
                    .map_err(|error| RepositoryError::ResurrectionRejected {
                        id: binding.id().to_hex(),
                        reason: format!("conflict projection failed: {error}"),
                    })?
                }
                None => self
                    .project_tree_for_change_closure(view_name, ordered, policy)
                    .map_err(|error| RepositoryError::ResurrectionRejected {
                        id: binding.id().to_hex(),
                        reason: format!("projection failed: {error}"),
                    })?,
            };
            let bound_tree = bound_tree_oid(binding)?;
            if project.git.root != bound_tree {
                return Err(AttemptFailure::Rejected(
                    RepositoryError::ResurrectionRejected {
                        id: binding.id().to_hex(),
                        reason: format!(
                            "projected tree {} does not match the bound Git tree {}",
                            hex_oid(&project.git.root),
                            hex_oid(&bound_tree)
                        ),
                    },
                ));
            }
            let proof = ProjectionProof {
                projected_tree: project.git.root.clone(),
                bound_tree: bound_tree.clone(),
                set_id: identity.set_id,
                merkle_state: identity.merkle,
            };
            return Ok(AttemptOutcome::Noop { identity, proof });
        }

        self.resurrect_locked(
            operation_lock,
            git,
            binding,
            view_name,
            policy,
            lease,
            view_id,
            base_state,
            base_count,
            to_insert,
            already_present,
            conflicts,
        )
        .map(AttemptOutcome::Applied)
        .map_err(AttemptFailure::from)
    }

    /// One guarded attempt with the legacy optional-workspace dispatch: the
    /// caller either supplies the workspace transaction (its lock) or lets
    /// this call acquire a fresh lock in the documented order.
    #[allow(clippy::too_many_arguments)]
    fn resurrect_attempt(
        &self,
        workspace: Option<&crate::WorkspaceTxn>,
        git: &git2::Repository,
        binding: &GitStateBinding,
        view_name: &str,
        policy: &ConversionPolicy,
        lease: Option<git2::Oid>,
        conflicts: Option<&ConflictSnapshotRestore>,
    ) -> Result<AttemptOutcome, AttemptFailure> {
        match workspace {
            Some(workspace) => self.resurrect_attempt_locked(
                workspace.operation_lock(),
                git,
                binding,
                view_name,
                policy,
                lease,
                conflicts,
            ),
            None => {
                let working_copy = self.require_working_copy_id()?;
                let operation_lock = self.try_lock_operation(working_copy)?;
                self.resurrect_attempt_locked(
                    &operation_lock, git, binding, view_name, policy, lease, conflicts,
                )
            }
        }
    }

    /// Journal the `ResurrectBinding` intent, run the isolated builder, and
    /// finalize — or invert the operation through immutable recovery.
    #[allow(clippy::too_many_arguments)]
    fn resurrect_locked(
        &self,
        operation_lock: &super::locks::WorkingCopyOperationLockGuard,
        git: &git2::Repository,
        binding: &GitStateBinding,
        view_name: &str,
        policy: &ConversionPolicy,
        lease: Option<git2::Oid>,
        view_id: u64,
        base_state: atomic_core::types::Merkle,
        base_count: u64,
        to_insert: Vec<Hash>,
        _already_present: Vec<Hash>,
        conflicts: Option<&ConflictSnapshotRestore>,
    ) -> Result<AppliedResurrection, RepositoryError> {
        let payload = binding.payload();
        self.pristine
            .require_repository_capability(super::CHANGE_FORMAT_VNEXT_CAPABILITY)
            .map_err(RepositoryError::from)?;

        // Membership leases for the journaled effect: every to-insert change
        // lands at base_count + position + 1 in the view log.
        let transitions: Vec<MetadataTransition> = to_insert
            .iter()
            .enumerate()
            .map(|(index, hash)| MetadataTransition {
                target: MetadataTarget::ViewChange {
                    view: view_name.to_string(),
                    change: *hash,
                },
                expected_old: atomic_core::operation::MetadataValue::Absent,
                expected_new: atomic_core::operation::MetadataValue::Sequence(
                    base_count + index as u64,
                ),
            })
            .collect();

        // Working-copy checkpoint advance: only when this working copy
        // targets the resurrected view.
        let working_copy = operation_lock.working_copy();
        let before_record = self.working_copy_record(working_copy)?;
        let mut after_record = before_record.clone();
        if before_record.desired_view == view_id {
            after_record.desired_state = payload.merkle_state;
        }
        let before_state = RepoStateRef {
            view: Some(ViewStateRef {
                name: view_name.to_string(),
                state: base_state,
                set_id: None,
            }),
            working_copy: Some(super::operation::working_copy_state_ref(
                before_record.clone(),
            )),
            git: None,
        };
        let after_state = RepoStateRef {
            view: Some(ViewStateRef {
                name: view_name.to_string(),
                state: payload.merkle_state,
                set_id: None,
            }),
            working_copy: Some(super::operation::working_copy_state_ref(after_record)),
            git: None,
        };

        let operation = self.prepare_metadata_operation(
            operation_lock,
            OperationKind::ResurrectBinding,
            None,
            before_state,
            after_state,
            transitions,
            payload.ordered_changes.clone(),
            ActorRef::System {
                name: "git-binding-resurrection".to_string(),
            },
            super::operation::current_operation_timestamp_ms(),
        )?;

        let apply_result = self.apply_resurrection_locked(
            operation_lock.begin_write_immediate()?,
            git,
            binding,
            view_name,
            policy,
            lease,
            &to_insert,
            conflicts,
        );

        match apply_result {
            Ok(applied) => {
                // The membership leases classify as already applied (the
                // builder wrote VIEW_CHANGES), and the working-copy record
                // advances to the bound checkpoint.
                self.apply_operation_metadata_locked(operation_lock, operation.id())?;
                self.finalize_operation_verified(operation_lock, operation.id())?;
                Ok(AppliedResurrection {
                    operation: Some(operation.id()),
                    ..applied
                })
            }
            Err(error) => {
                // Immutable recovery: invert the prepared operation so no
                // unverified membership/checkpoint advance survives, and
                // surface the rejection as recoverable evidence.
                self.abort_prepared_metadata_operation(operation_lock, &operation)?;
                Err(error)
            }
        }
    }

    /// The isolated resurrection builder: apply the closure to the global
    /// graph, prove the projection, publish membership — one transaction.
    #[allow(clippy::too_many_arguments)]
    fn apply_resurrection_locked(
        &self,
        mut txn: super::locks::OrderedPristineWriteTxn<'_>,
        git: &git2::Repository,
        binding: &GitStateBinding,
        view_name: &str,
        policy: &ConversionPolicy,
        lease: Option<git2::Oid>,
        to_insert: &[Hash],
        conflicts: Option<&ConflictSnapshotRestore>,
    ) -> Result<AppliedResurrection, RepositoryError> {
        use crate::apply::apply_change_hunks_without_membership;
        use atomic_core::pristine::GraphTxnT;

        let payload = binding.payload();
        let ordered = payload.ordered_changes.clone();

        // Test fault: fail the isolated build after journaling so rejection
        // + recovery can be exercised end to end.
        #[cfg(test)]
        if RESURRECTION_APPLY_FAULT.with(std::cell::Cell::get) {
            return Err(RepositoryError::InvalidOperation {
                message: "injected resurrection apply fault (test)".to_string(),
            });
        }

        // ── Isolated build: every closure change lands in the global graph
        //    via the normal writers, without any view membership. The
        //    transaction is the isolation boundary: a failed proof aborts it
        //    wholesale, so a rejected proof publishes nothing.
        //
        // Pass 0 registers the whole closure first: the shared deferred TREE
        // journal (a dot-dir file) is validated against the live graph and
        // may carry rows from a previously aborted attempt, so it must never
        // reference an unregistered change.
        for hash in &ordered {
            if txn.get_internal(hash).map_err(pristine_map)?.is_none() {
                let change = self.load_change(hash)?;
                let change_id = txn.register_change(hash).map_err(pristine_map)?;
                txn.put_change_deps(change_id, change.dependencies())
                    .map_err(pristine_map)?;
            }
        }

        // Pass 1 mirrors `insert_change` per change: plan the tree projection,
        // apply its prerequisites (which install the position→inode mapping
        // SetAttr events resolve), then apply the hunks.
        for hash in &ordered {
            let change_id = txn
                .get_internal(hash)
                .map_err(pristine_map)?
                .ok_or(RepositoryError::ChangeNotFound {
                    hash: hash.to_base32(),
                })?;
            let change = self.load_change(hash)?;
            let already_applied = txn
                .has_change_in_graph(change_id)
                .map_err(pristine_map)?;
            let tree_projection =
                self.plan_tree_projection(&mut *txn, change_id, *hash, &change, &[], false)?;
            tree_projection.apply_prerequisites(&mut *txn)?;
            if already_applied {
                continue;
            }
            let verified_frontier = verify_causal_frontier(&*txn, &change).map_err(|error| {
                RepositoryError::ResurrectionRejected {
                    id: binding.id().to_hex(),
                    reason: format!(
                        "change {} carries an unverifiable causal frontier: {error}",
                        hash.to_base32()
                    ),
                }
            })?;
            apply_change_hunks_without_membership(
                &mut txn,
                change_id,
                &change,
                &verified_frontier,
                &crate::InsertOptions::default(),
            )
            .map_err(|error| RepositoryError::ResurrectionRejected {
                id: binding.id().to_hex(),
                reason: format!(
                    "change {} failed to apply in isolation: {error}",
                    hash.to_base32()
                ),
            })?;
        }

        // Pass 2: publish each change's tree projection once every hunk pass
        // has run, so the shared deferred journal's rows always resolve.
        for hash in &ordered {
            let change_id = txn
                .get_internal(hash)
                .map_err(pristine_map)?
                .ok_or(RepositoryError::ChangeNotFound {
                    hash: hash.to_base32(),
                })?;
            let change = self.load_change(hash)?;
            let tree_projection =
                self.plan_tree_projection(&mut *txn, change_id, *hash, &change, &[], false)?;
            self.apply_tree_projection(&mut *txn, &tree_projection, view_name, false)?;
        }

        // ── The mandatory projection proof (RFC §5.2, §12.5): recompute the
        //    full projected tree under the current conversion policy from
        //    the restored closure and compare tree OID and SetId.
        let view = txn
            .get_view(view_name)
            .map_err(pristine_map)?
            .ok_or(RepositoryError::ViewNotFound {
                name: view_name.to_string(),
            })?;
        let project = match conflicts {
            Some(restore) => self
                .project_change_closure_with_conflict_markers(
                    &*txn,
                    &view,
                    &ordered,
                    policy,
                    &restore.marker_bytes,
                )
                .map_err(|error| RepositoryError::ResurrectionRejected {
                    id: binding.id().to_hex(),
                    reason: format!("conflict projection failed: {error}"),
                })?,
            None => self
                .project_change_closure_with_txn(&*txn, &view, &ordered, policy)
                .map_err(|error| RepositoryError::ResurrectionRejected {
                    id: binding.id().to_hex(),
                    reason: format!("projection failed: {error}"),
                })?,
        };
        let bound_tree = bound_tree_oid(binding)?;
        let projected_tree = project.git.root.clone();
        if &projected_tree != &bound_tree {
            return Err(RepositoryError::ResurrectionRejected {
                id: binding.id().to_hex(),
                reason: format!(
                    "projected tree {} does not match the bound Git tree {} \
                     (extra/missing path, empty file, mode/kind/link/gitlink change, raw path, \
                     sparse/filter policy mismatch, forged header, or missing conflict object)",
                    hex_oid(&projected_tree),
                    hex_oid(&bound_tree)
                ),
            });
        }
        if project.manifest.set_id != payload.set_id {
            return Err(RepositoryError::ResurrectionRejected {
                id: binding.id().to_hex(),
                reason: format!(
                    "recomputed SetId {} does not match the bound SetId {} (wrong Merkle/SetId data)",
                    project.manifest.set_id, payload.set_id
                ),
            });
        }

        // ── Publish membership in binding Merkle order, inside the same
        //    transaction. The final state must land exactly on the binding's.
        let mut view = txn
            .open_or_create_view(view_name)
            .map_err(pristine_map)?;
        for hash in to_insert {
            let change_id = txn
                .get_internal(hash)
                .map_err(pristine_map)?
                .ok_or(RepositoryError::ChangeNotFound {
                    hash: hash.to_base32(),
                })?;
            txn.put_change(&mut view, change_id, hash)
                .map_err(pristine_map)?;
        }
        if view.state != payload.merkle_state {
            return Err(RepositoryError::ResurrectionRejected {
                id: binding.id().to_hex(),
                reason: format!(
                    "published Merkle state {} does not match the bound state {}; the target \
                     view must be a fresh or exact-prefix base for order restoration",
                    view.state, payload.merkle_state
                ),
            });
        }
        txn.update_view(&view).map_err(pristine_map)?;

        // Lease re-check before the mutation becomes visible: the Git lease
        // observed before the proof must still hold (the three-attempt guard
        // upstream turns a mismatch into a fresh attempt).
        if lease.is_some() {
            #[cfg(test)]
            {
                let pending = RESURRECTION_CONTENTION_ATTEMPTS.with(std::cell::Cell::get);
                if pending > 0 {
                    RESURRECTION_CONTENTION_ATTEMPTS.with(|cell| cell.set(pending - 1));
                    return Err(RepositoryError::ResurrectionLeaseChanged);
                }
            }
            let current = git.head().ok().and_then(|head| head.target());
            if current != lease {
                return Err(RepositoryError::ResurrectionLeaseChanged);
            }
        }

        txn.commit().map_err(database_map)?;

        let identity = self.view_identity(view_name)?;
        let already_present: Vec<Hash> = ordered
            .iter()
            .filter(|hash| !to_insert.contains(hash))
            .copied()
            .collect();
        let proof = ProjectionProof {
            projected_tree,
            bound_tree: bound_tree.clone(),
            set_id: identity.set_id,
            merkle_state: identity.merkle,
        };
        Ok(AppliedResurrection {
            inserted: to_insert.to_vec(),
            already_present,
            proof,
            identity,
            operation: None,
        })
    }
}

/// Assemble the public outcome from one successful attempt.
fn exact_outcome(
    binding: &GitStateBinding,
    outcome: AttemptOutcome,
    restored_raw_commit: bool,
    trust: crate::git_binding::BindingTrustEvaluation,
    operation: Option<atomic_core::types::OperationId>,
) -> ExactResurrection {
    let payload = binding.payload();
    match outcome {
        AttemptOutcome::Noop { identity, proof } => ExactResurrection {
            binding_id: binding.id(),
            inserted: Vec::new(),
            already_present: payload.ordered_changes.clone(),
            restored_raw_commit,
            provenance_roots: payload.provenance_roots.clone(),
            attestation_roots: payload.attestation_roots.clone(),
            provenance_trusted: trust.provenance_trusted(),
            operation,
            identity,
            proof,
        },
        AttemptOutcome::Applied(applied) => ExactResurrection {
            binding_id: binding.id(),
            inserted: applied.inserted,
            already_present: applied.already_present,
            restored_raw_commit,
            provenance_roots: payload.provenance_roots.clone(),
            attestation_roots: payload.attestation_roots.clone(),
            provenance_trusted: trust.provenance_trusted(),
            operation: applied.operation.or(operation),
            identity: applied.identity,
            proof: applied.proof,
        },
    }
}

/// Map a binding payload's Git object format onto the core algorithm tag.
fn git_algorithm(format: GitObjectFormat) -> atomic_core::operation::GitHashAlgorithm {
    match format {
        GitObjectFormat::Sha1 => atomic_core::operation::GitHashAlgorithm::Sha1,
        GitObjectFormat::Sha256 => atomic_core::operation::GitHashAlgorithm::Sha256,
    }
}

/// The bound tree as a core-tagged Git object id.
fn bound_tree_oid(
    binding: &GitStateBinding,
) -> Result<atomic_core::operation::GitObjectId, RepositoryError> {
    binding
        .payload()
        .git_tree
        .to_git_object_id()
        .map_err(|error| RepositoryError::BindingRejected {
            id: binding.id().to_hex(),
            commit: binding.payload().git_commit.to_hex(),
            reason: format!("malformed bound tree OID: {error}"),
        })
}

/// Lowercase hex of a core-tagged Git object id.
fn hex_oid(oid: &atomic_core::operation::GitObjectId) -> String {
    oid.as_bytes().iter().map(|byte| format!("{byte:02x}")).collect()
}
