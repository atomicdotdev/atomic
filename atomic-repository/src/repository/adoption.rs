//! CB-7B: snapshot-safe, shelf-safe, interruption-safe bound HEAD adoption.
//!
//! Normative source: RFC-ATOMIC-GIT-CAUSAL-BRIDGE §3.3–3.4, §6.1–6.4, §7.1–7.4,
//! §11, §12, Phase 7 (tracker unit CB-7B).
//!
//! # Pre-transition evidence (§7.3 branch 2)
//!
//! Every verified workspace boundary that still has pending edits captures a
//! complete baseline-relative snapshot change plus a durable evidence record
//! tying together old HEAD, primary index digest, working-copy identity, and
//! the conversion policy. When a later bound HEAD adoption finds a worktree
//! that is not clean relative to the new HEAD, this record is the proof that
//! turns unexplained differences into a *known carried edit* which is
//! re-assembled against the new durable baseline (RFC §7.3 branch 2) — instead
//! of being refused (the CB-7A behavior) or invented (§12.3).
//!
//! # Unknown-origin fallback (§7.3 branch 3)
//!
//! Without that proof, post-checkout differences are captured as a fresh
//! opaque snapshot whose hashed metadata marks *unknown pre-checkout
//! attribution*; they are never claimed as old-baseline edits.
//!
//! # Safety properties (§12)
//!
//! - Known-edit reassembly writes only through journaled, leased filesystem
//!   effects; a third lease value (newer editor content) is `SyncConflict`
//!   and is never overwritten (§12.6, §7.6).
//! - The old snapshot object stays durable until the replacement snapshot is
//!   verified; failure anywhere preserves both (§14.1 retention roots).
//! - The new snapshot never depends on a superseded snapshot hash (§12.3) and
//!   is verified re-readable after reopen.
//! - WIP refs (§6.4, CB-0B machinery) protect tracked bytes before the first
//!   filesystem effect and are dropped only after the replacement snapshot is
//!   verified durable.

use std::path::PathBuf;

use atomic_core::change::{ChangeKind, InodeKind};
use atomic_core::operation::{
    ActorRef, CheckpointKind, DigestKind, EffectPlan, EffectTarget, EffectValue, FileKind,
    FileState, GitHeadState, GitIndexState, GitObjectId, GitRefTarget, GitStateRef, OperationKind,
    RepoStateRef,
};
use atomic_core::types::Base32;

use atomic_core::pristine::{GraphTxnT, OperationTxnT, ViewTxnT};
use atomic_core::types::OperationId;

use super::locks::WorkingCopyOperationLockGuard;
use super::workspace_txn::read_workspace_checkpoint;
use super::{Repository, RepositoryError, WorkspaceGitObservation};

pub const PRE_TRANSITION_EVIDENCE_RELATIVE: &str = ".atomic/bridge/pre-transition.json";
const PRE_TRANSITION_EVIDENCE_VERSION: u32 = 1;

/// The system actor that journals every pre-transition capture (R2). The
/// capture's immutable journal operation is the local authenticated record:
/// its payload is content-addressed (`OPERATIONS`), its verified receipt is
/// append-only (`EFFECT_RECEIPTS`), and the evidence facts are hashed into
/// the payload's evidence set.
pub(super) const PRE_TRANSITION_CAPTURE_ACTOR: &str = "bridge-pre-transition-capture";

/// Hashed metadata marker embedded in bridge adoption snapshots. The marker
/// lives in the change's hashed metadata, so an attribution claim is
/// content-addressed and can never be edited after the fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdoptionOrigin {
    /// Re-assembled from pre-transition capture evidence (§7.3 branch 2).
    KnownCarriedEdit,
    /// Captured after checkout without pre-transition proof (§7.3 branch 3).
    UnknownPreCheckout,
}

impl AdoptionOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::KnownCarriedEdit => "known-carried-edit",
            Self::UnknownPreCheckout => "unknown-pre-checkout",
        }
    }
}

/// Durable pre-transition evidence: the authenticated facts that prove what
/// the workspace looked like before a Git-owned transition. Stored outside the
/// operation hash like the checkpoint (derived evidence, RFC §7.2 step 6).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PreTransitionEvidence {
    pub version: u32,
    /// Physical working copy the snapshot was captured for.
    pub working_copy: String,
    /// The durable baseline view the snapshot is relative to.
    pub baseline_view: String,
    /// The exact baseline state (matches the verified checkpoint).
    pub baseline_state: String,
    /// The snapshot change covering the pending edits (base32), when one was
    /// captured.
    pub snapshot: Option<String>,
    /// A durable remainder change (base32), when one exists.
    pub remainder: Option<String>,
    /// Old HEAD commit observed at capture time (matches the checkpoint).
    pub git_head: String,
    /// Old HEAD tree at capture time.
    pub git_tree: String,
    /// Primary index tree at capture time.
    pub git_index_tree: Option<String>,
    /// Primary index canonical digest at capture time.
    pub git_index_digest: Option<String>,
    /// Resolved primary index path (alternate-index evidence guard).
    pub git_index_path: Option<String>,
    /// Conversion policy content key at capture time.
    pub policy_root: String,
    pub created_at_ms: i64,
    /// R2: the content-addressed `OperationId` (base32) of the immutable
    /// journal operation that authenticated this capture. `None` only for
    /// evidence written before journal binding existed — such evidence is
    /// never provable (fail closed to unknown origin).
    #[serde(default)]
    pub journal_operation: Option<String>,
}

/// Canonical serialization of the capture facts for the journal binding: the
/// evidence with its journal pointer cleared, serialized compactly. The hash
/// of these bytes is carried inside the immutable journal operation's
/// evidence set, so any tampered or fabricated JSON field changes the hash
/// and fails the proof.
fn canonical_capture_facts(evidence: &PreTransitionEvidence) -> Result<Vec<u8>, RepositoryError> {
    let mut facts = evidence.clone();
    facts.journal_operation = None;
    serde_json::to_vec(&facts).map_err(|error| RepositoryError::Serialization(error.to_string()))
}

/// Canonical bytes of a checkpoint's FACTS (R3): the parsed checkpoint's
/// deterministic wire serialization with the index digest normalized to
/// base32. The Checkpoint lease digests these facts — not the raw file
/// bytes — so the CLI's richer v2 writer (which records the index digest as
/// hex) and the simple writer authenticate identically when the underlying
/// facts match, and a fact-preserving rewrite is idempotent.
pub(super) fn checkpoint_facts_bytes(
    checkpoint: &super::workspace_txn::WorkspaceCheckpoint,
) -> Result<Vec<u8>, RepositoryError> {
    let mut normalized = checkpoint.clone();
    if let Some(digest) = &checkpoint.git_index_digest {
        if atomic_core::types::Merkle::from_base32(digest.as_bytes()).is_none() {
            // The CLI checkpoint writer stores the canonical index digest as
            // hex; normalize it to base32 so both encodings lease the same
            // facts.
            let bytes = hex_decode_bytes(digest)?;
            let bytes: [u8; 32] =
                bytes
                    .try_into()
                    .map_err(|_| RepositoryError::InvalidRepository {
                        reason: format!("'{digest}' is neither base32 nor a 32-byte hex digest"),
                    })?;
            normalized.git_index_digest = Some(atomic_core::types::Merkle(bytes).to_base32());
        }
    }
    super::workspace_txn::workspace_checkpoint_bytes(&normalized)
}

/// Build the hashed metadata marker for an adoption snapshot.
pub fn adoption_snapshot_metadata(
    origin: AdoptionOrigin,
    evidence: &PreTransitionEvidence,
) -> Vec<u8> {
    let marker = serde_json::json!({
        "atomic-bridge": {
            "v": 1,
            "origin": origin.as_str(),
            "evidence": {
                "working_copy": evidence.working_copy,
                "baseline_view": evidence.baseline_view,
                "baseline_state": evidence.baseline_state,
                "git_head": evidence.git_head,
                "git_tree": evidence.git_tree,
                "policy_root": evidence.policy_root,
            },
        }
    });
    marker.to_string().into_bytes()
}

/// Read an adoption attribution marker back out of hashed change metadata.
pub fn origin_from_change_metadata(metadata: &[u8]) -> Option<AdoptionOrigin> {
    let value: serde_json::Value = serde_json::from_slice(metadata).ok()?;
    let origin = value.get("atomic-bridge")?.get("origin")?.as_str()?;
    match origin {
        "known-carried-edit" => Some(AdoptionOrigin::KnownCarriedEdit),
        "unknown-pre-checkout" => Some(AdoptionOrigin::UnknownPreCheckout),
        _ => None,
    }
}

pub(crate) fn pre_transition_evidence_path(root: &std::path::Path) -> PathBuf {
    root.join(PRE_TRANSITION_EVIDENCE_RELATIVE)
}

/// Read the durable pre-transition evidence. A missing file reads as `None`;
/// malformed or future-versioned evidence fails closed.
pub fn read_pre_transition_evidence(
    root: &std::path::Path,
) -> Result<Option<PreTransitionEvidence>, RepositoryError> {
    let path = pre_transition_evidence_path(root);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(RepositoryError::InvalidRepository {
                reason: format!(
                    "cannot read bridge pre-transition evidence '{}': {error}",
                    path.display()
                ),
            })
        }
    };
    let evidence: PreTransitionEvidence =
        serde_json::from_slice(&bytes).map_err(|error| RepositoryError::InvalidRepository {
            reason: format!(
                "bridge pre-transition evidence '{}' is malformed: {error}",
                path.display()
            ),
        })?;
    if evidence.version != PRE_TRANSITION_EVIDENCE_VERSION {
        return Err(RepositoryError::InvalidRepository {
            reason: format!(
                "unsupported bridge pre-transition evidence version {}",
                evidence.version
            ),
        });
    }
    Ok(Some(evidence))
}

/// Persist the pre-transition evidence atomically (same discipline as the
/// verified checkpoint).
pub(crate) fn write_pre_transition_evidence(
    root: &std::path::Path,
    evidence: &PreTransitionEvidence,
) -> Result<(), RepositoryError> {
    let path = pre_transition_evidence_path(root);
    let parent = path
        .parent()
        .ok_or_else(|| RepositoryError::InvalidRepository {
            reason: "pre-transition evidence path has no parent directory".to_string(),
        })?;
    std::fs::create_dir_all(parent).map_err(RepositoryError::Io)?;
    let mut bytes = serde_json::to_vec_pretty(evidence)
        .map_err(|error| RepositoryError::Serialization(error.to_string()))?;
    bytes.push(b'\n');
    let temporary = parent.join(format!(".pre-transition.json.{}.tmp", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        std::fs::rename(&temporary, &path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.map_err(|error| RepositoryError::InvalidRepository {
        reason: format!(
            "cannot write bridge pre-transition evidence '{}': {error}",
            path.display()
        ),
    })
}

/// The per-path decision of the carried-edit reassembly plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CarriedEntry {
    /// Nothing to write: the checkout content already equals the target.
    AlreadyCurrent,
    /// Write the given repository bytes with the given kind/mode.
    Write {
        repository_bytes: Vec<u8>,
        mode: u16,
        kind: InodeKind,
    },
    /// Remove the path (a carried deletion wins on an unchanged baseline).
    Remove,
}

/// One inseparable per-path conflict (ac-2: fail structurally, never
/// approximate by file path).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CarriedConflict {
    pub path: String,
    pub reason: String,
}

/// The complete reassembly plan: journaled writes plus structural refusals.
#[derive(Debug, Default)]
pub struct CarriedReassemblyPlan {
    pub entries: Vec<(String, CarriedEntry)>,
    pub conflicts: Vec<CarriedConflict>,
}

impl CarriedReassemblyPlan {
    pub fn is_separable(&self) -> bool {
        self.conflicts.is_empty()
    }
}

fn observation_map(error: super::ObservationError) -> RepositoryError {
    RepositoryError::InvalidRepository {
        reason: error.to_string(),
    }
}

fn record_map(error: super::RecordError) -> RepositoryError {
    match error {
        super::RecordError::Repository(inner) => inner,
        other => RepositoryError::InvalidOperation {
            message: format!("bridge adoption snapshot capture failed: {other}"),
        },
    }
}

/// The conversion policy content key for one capture boundary.
fn policy_root_for_capture(root: &std::path::Path) -> Result<String, RepositoryError> {
    let policy =
        super::conversion_policy_for_git(&git2::Repository::discover(root).map_err(|error| {
            RepositoryError::InvalidRepository {
                reason: format!("cannot open the Git repository for capture: {error}"),
            }
        })?)
        .map_err(|error| RepositoryError::InvalidOperation {
            message: format!("cannot compute conversion policy for capture: {error}"),
        })?;
    Ok(policy.root().content_key)
}

impl Repository {
    /// Whether the working copy's private snapshot view holds an active
    /// change parented to a view other than `baseline_view`.
    fn stale_snapshot_against_baseline(
        &self,
        working_copy: atomic_core::WorkingCopyId,
        baseline_view: &str,
    ) -> Result<bool, RepositoryError> {
        let snapshot_view = Self::snapshot_view_name(working_copy);
        let txn = self
            .pristine
            .read_txn()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        let Some(view) = txn
            .get_view(&snapshot_view)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
        else {
            return Ok(false);
        };
        let Some(baseline) = txn
            .get_view(baseline_view)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
        else {
            return Ok(false);
        };
        Ok(view.parent != Some(baseline.id) && view.change_count > 0)
    }

    /// Remove an absorbed snapshot from the active slot: the workspace is
    /// clean relative to the snapshot's own baseline, so the pending edits
    /// were consumed by a durable record or an explicit transition. The
    /// change objects stay in the store as retained recovery roots; only the
    /// leased view membership moves (§14.1, §12.12).
    pub(super) fn clear_absorbed_snapshot(
        &self,
        working_copy: atomic_core::WorkingCopyId,
    ) -> Result<(), RepositoryError> {
        let snapshot_status = self.snapshot_status(working_copy)?;
        if snapshot_status.snapshot.is_none() && snapshot_status.remainder.is_none() {
            return Ok(());
        }
        // Post-adoption capture discipline (R2): an unknown-pre-checkout
        // snapshot's content is UNEXPLAINED — it was never absorbed by a
        // durable record, so a clean-looking boundary must not strip its
        // opaque snapshot from the active slot (that would lose the
        // attribution and let a later capture re-record the same content as
        // a known carried edit).
        if let Some(hash) = snapshot_status.snapshot {
            let unknown = self
                .load_change(&hash)
                .ok()
                .and_then(|change| origin_from_change_metadata(&change.hashed.metadata))
                .is_some_and(|origin| origin == AdoptionOrigin::UnknownPreCheckout);
            if unknown {
                return Ok(());
            }
        }
        let stale =
            self.stale_snapshot_against_baseline(working_copy, &snapshot_status.baseline_view)?;
        // A snapshot against the CURRENT baseline with a clean worktree is
        // absorbed; a stale-baseline snapshot with a clean worktree is also
        // resolved (its baseline moved on). Both leave the active slot.
        let _ = stale;
        self.remove_active_snapshot_locked(working_copy, &snapshot_status.baseline_view)
    }

    /// Journaled removal of every direct change from the private snapshot
    /// view plus its re-parent onto `baseline_view`.
    pub(super) fn remove_active_snapshot_locked(
        &self,
        working_copy: atomic_core::WorkingCopyId,
        baseline_view: &str,
    ) -> Result<(), RepositoryError> {
        let snapshot_view = Self::snapshot_view_name(working_copy);
        let operation_lock = self.try_lock_operation(working_copy)?;
        if let super::OperationHeadState::Diverged(heads) =
            self.consolidate_operation_heads_locked(&operation_lock)?
        {
            return Err(RepositoryError::OperationHeadsDiverged {
                scope: atomic_core::operation::OperationScope::WorkingCopy(working_copy)
                    .to_string(),
                heads: heads.iter().map(ToString::to_string).collect(),
            });
        }
        let (view_state, baseline_id, direct) = {
            let txn = self
                .pristine
                .read_txn()
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
            let view = txn
                .get_view(&snapshot_view)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or(RepositoryError::ViewNotFound {
                    name: snapshot_view.clone(),
                })?;
            let baseline = txn
                .get_view(baseline_view)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or(RepositoryError::ViewNotFound {
                    name: baseline_view.to_string(),
                })?;
            let mut direct = Vec::new();
            for row in txn
                .iter_changes(&view, 0)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
            {
                let (_, node_id, _) =
                    row.map_err(|error| RepositoryError::Database(error.to_string()))?;
                direct.push(
                    txn.get_external(node_id)
                        .map_err(|error| RepositoryError::Database(error.to_string()))?
                        .ok_or(RepositoryError::ChangeNotFound {
                            hash: node_id.to_string(),
                        })?,
                );
            }
            (view, baseline.id, direct)
        };
        if direct.is_empty() && view_state.parent == Some(baseline_id) {
            return Ok(());
        }
        let reparent_needed = view_state.parent != Some(baseline_id);

        let before_record = self.working_copy_record(working_copy)?;
        let before_state = atomic_core::operation::RepoStateRef {
            view: Some(atomic_core::operation::ViewStateRef {
                name: baseline_view.to_string(),
                state: new_state_for_view(self, baseline_view)?,
                set_id: None,
            }),
            working_copy: Some(super::operation::working_copy_state_ref(
                before_record.clone(),
            )),
            git: None,
        };
        let after_state = before_state.clone();

        let mut metadata = Vec::new();
        let encode = |view: &atomic_core::pristine::ViewState| {
            postcard::to_allocvec(&super::operation::ViewLeaseValue {
                parent: view.parent,
            })
            .map_err(|error| RepositoryError::Serialization(error.to_string()))
            .map(atomic_core::operation::MetadataValue::Bytes)
        };
        for hash in &direct {
            let hash_id = {
                let txn = self
                    .pristine
                    .read_txn()
                    .map_err(|error| RepositoryError::Database(error.to_string()))?;
                txn.get_internal(hash)
                    .map_err(|error| RepositoryError::Database(error.to_string()))?
                    .ok_or(RepositoryError::ChangeNotFound {
                        hash: hash.to_base32(),
                    })?
            };
            let sequence = {
                let txn = self
                    .pristine
                    .read_txn()
                    .map_err(|error| RepositoryError::Database(error.to_string()))?;
                txn.get_change_seq(&view_state, hash_id)
                    .map_err(|error| RepositoryError::Database(error.to_string()))?
                    .ok_or(RepositoryError::ChangeNotInView {
                        hash: hash.to_base32(),
                        view: snapshot_view.clone(),
                    })?
            };
            metadata.push(atomic_core::operation::MetadataTransition {
                target: atomic_core::operation::MetadataTarget::ViewChange {
                    view: snapshot_view.clone(),
                    change: *hash,
                },
                expected_old: atomic_core::operation::MetadataValue::Sequence(sequence),
                expected_new: atomic_core::operation::MetadataValue::Absent,
            });
        }
        if reparent_needed {
            let mut expected_new = view_state.clone();
            expected_new.parent = Some(baseline_id);
            metadata.push(atomic_core::operation::MetadataTransition {
                target: atomic_core::operation::MetadataTarget::View {
                    name: snapshot_view.clone(),
                },
                expected_old: encode(&view_state)?,
                expected_new: encode(&expected_new)?,
            });
        }

        let operation = self.prepare_metadata_operation(
            &operation_lock,
            OperationKind::Record,
            None,
            before_state,
            after_state,
            metadata,
            Vec::new(),
            ActorRef::System {
                name: "bridge-snapshot-rebase".to_string(),
            },
            super::operation::current_operation_timestamp_ms(),
        )?;
        self.apply_operation_metadata_locked(&operation_lock, operation.id())?;
        self.finalize_operation_verified(&operation_lock, operation.id())?;
        Ok(())
    }

    /// Rebase a stale active snapshot onto the current baseline before a
    /// fresh capture: the old membership is leased out (object retained in
    /// the store) and the view re-parents, so the next record supersedes
    /// against the new baseline.
    pub(super) fn rebase_stale_snapshot_locked(
        &self,
        working_copy: atomic_core::WorkingCopyId,
        baseline_view: &str,
    ) -> Result<(), RepositoryError> {
        self.remove_active_snapshot_locked(working_copy, baseline_view)
    }
}

/// Plan the reassembly of the proven carried edit against the new durable
/// baseline (free function so manifest-level semantics are unit-testable).
///
/// `old_manifest` is the pre-checkout durable baseline, `carried_manifest`
/// the snapshot view (old baseline + carried edit), and `new_manifest` the
/// adopted target view. Per path:
/// - no carried edit → the new baseline content (checkout output),
/// - carried deletion on an unchanged baseline → Remove (R1),
/// - carried deletion plus baseline change → delete/modify refusal (R1),
/// - delete/delete (both sides removed the path) → converge (R1),
/// - carried edit on an unchanged baseline → the carried content verbatim,
/// - carried edit on a changed baseline → a three-way text merge
///   (`atomic_core::diff::merge_text`); overlapping edits are structural
///   refusals (ac-2), binary and kind-mismatched both-sides changes too,
/// - non-UTF-8 paths refuse before any mutation (R5).
pub(super) fn plan_carried_reassembly_trees<'a>(
    old_manifest: &'a super::ProjectTree,
    carried_manifest: &'a super::ProjectTree,
    new_manifest: &'a super::ProjectTree,
) -> CarriedReassemblyPlan {
    use std::collections::BTreeMap;

    // R5: raw Git path identity must never collapse through lossy UTF-8
    // decoding. Two distinct non-UTF-8 names decode to the same lossy
    // string and would alias inside this plan (and downstream in shelf
    // effects), so every unsupported path is refused here — before any
    // mutation, lease, or filesystem effect exists.
    let mut plan = CarriedReassemblyPlan::default();
    let mut refused: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
    for tree in [old_manifest, carried_manifest, new_manifest] {
        for entry in &tree.manifest.entries {
            if entry.disposition != super::ManifestDisposition::Included {
                continue;
            }
            if std::str::from_utf8(entry.path.as_bytes()).is_err()
                && refused.insert(entry.path.as_bytes().to_vec())
            {
                plan.conflicts.push(CarriedConflict {
                    path: String::from_utf8_lossy(entry.path.as_bytes()).into_owned(),
                    reason: "path is not valid UTF-8; raw path identity cannot be preserved \
                                 through the reassembly plan, so adoption refuses before any \
                                 mutation"
                        .to_string(),
                });
            }
        }
    }
    if !plan.conflicts.is_empty() {
        return plan;
    }

    let key_of = |tree: &'a super::ProjectTree| -> BTreeMap<String, &'a super::RepositoryEntry> {
        tree.manifest
            .entries
            .iter()
            .filter(|entry| entry.disposition == super::ManifestDisposition::Included)
            .map(|entry| {
                (
                    String::from_utf8_lossy(entry.path.as_bytes()).into_owned(),
                    entry,
                )
            })
            .collect()
    };
    let old = key_of(old_manifest);
    let carried = key_of(carried_manifest);
    let new = key_of(new_manifest);

    let mut paths: std::collections::BTreeSet<String> = old.keys().cloned().collect();
    paths.extend(carried.keys().cloned());
    paths.extend(new.keys().cloned());

    for path in paths {
        let old_entry = old.get(&path).copied();
        let carried_entry = carried.get(&path).copied();
        let new_entry = new.get(&path).copied();
        if std::env::var_os("ATOMIC_TRACE_ADOPTION").is_some() {
            eprintln!(
                "[adoption] {path}: old={:?} carried={:?} new={:?}",
                old_entry.map(|entry| (&entry.kind, entry.mode, entry.repository_bytes.len())),
                carried_entry.map(|entry| (&entry.kind, entry.mode, entry.repository_bytes.len())),
                new_entry.map(|entry| (&entry.kind, entry.mode, entry.repository_bytes.len())),
            );
        }

        let unchanged = |a: &super::RepositoryEntry, b: &super::RepositoryEntry| {
            a.repository_bytes == b.repository_bytes && a.kind == b.kind && a.mode == b.mode
        };
        match (old_entry, carried_entry, new_entry) {
            (old_e, carried_e, new_e) if carried_e == old_e => match new_e {
                // No carried edit: the checkout output stands.
                Some(_) => plan.entries.push((path, CarriedEntry::AlreadyCurrent)),
                None => plan.entries.push((path, CarriedEntry::Remove)),
            },
            (old_e, Some(carried_e), new_e) => {
                let carried_changed = match old_e {
                    Some(old_e) => !unchanged(old_e, carried_e),
                    None => true,
                };
                let new_changed = match (&old_e, &new_e) {
                    (Some(old_e), Some(new_e)) => !unchanged(old_e, new_e),
                    (Some(_), None) => true,
                    (None, Some(_)) => true,
                    (None, None) => false,
                };
                // R1: identical add/add (no baseline entry, both sides
                // created the same content) converges instead of
                // refusing.
                if old_e.is_none()
                    && new_e
                        .as_ref()
                        .is_some_and(|new_e| unchanged(carried_e, new_e))
                {
                    plan.entries.push((path, CarriedEntry::AlreadyCurrent));
                    continue;
                }
                if !carried_changed {
                    match new_e {
                        Some(_) => plan.entries.push((path, CarriedEntry::AlreadyCurrent)),
                        None => plan.entries.push((path, CarriedEntry::Remove)),
                    }
                    continue;
                }
                if !new_changed {
                    // Baseline unchanged: carry verbatim.
                    match old_e {
                        Some(_) => plan.entries.push((
                            path,
                            CarriedEntry::Write {
                                repository_bytes: carried_e.repository_bytes.clone(),
                                mode: carried_e.mode,
                                kind: carried_e.kind,
                            },
                        )),
                        None => plan.entries.push((
                            path,
                            CarriedEntry::Write {
                                repository_bytes: carried_e.repository_bytes.clone(),
                                mode: carried_e.mode,
                                kind: carried_e.kind,
                            },
                        )),
                    }
                    continue;
                }
                // Both sides changed relative to the baseline.
                let (Some(old_e), Some(new_e)) = (old_e, new_e) else {
                    plan.conflicts.push(CarriedConflict {
                        path: path.clone(),
                        reason: "carried edit and baseline change disagree on path existence"
                            .to_string(),
                    });
                    continue;
                };
                if carried_e.kind != old_e.kind
                    || new_e.kind != old_e.kind
                    || carried_e.kind != new_e.kind
                {
                    plan.conflicts.push(CarriedConflict {
                        path: path.clone(),
                        reason: format!(
                "kind change on both sides ({:?} baseline, {:?} carried, {:?} baseline)",
                old_e.kind, carried_e.kind, new_e.kind
                ),
                    });
                    continue;
                }
                if byte_is_binary(&old_e.repository_bytes)
                    || byte_is_binary(&carried_e.repository_bytes)
                    || byte_is_binary(&new_e.repository_bytes)
                {
                    plan.conflicts.push(CarriedConflict {
                        path: path.clone(),
                        reason: "binary content changed on both sides; no textual merge"
                            .to_string(),
                    });
                    continue;
                }
                match atomic_core::diff::merge_text(
                    &old_e.repository_bytes,
                    &carried_e.repository_bytes,
                    &new_e.repository_bytes,
                ) {
                    Ok(merged) => plan.entries.push((
                        path,
                        CarriedEntry::Write {
                            repository_bytes: merged,
                            mode: if carried_e.mode != old_e.mode {
                                carried_e.mode
                            } else {
                                new_e.mode
                            },
                            kind: new_e.kind,
                        },
                    )),
                    Err(reason) => plan.conflicts.push(CarriedConflict {
                        path: path.clone(),
                        reason: format!("intra-file merge conflict: {reason}"),
                    }),
                }
            }
            (Some(old_e), None, Some(new_e)) => {
                // R1: the snapshot manifest has no entry for a path the
                // old baseline had — a captured deletion.
                if unchanged(old_e, new_e) {
                    // The new baseline is unchanged: the carried deletion
                    // wins and must stay deleted (never silently dropped
                    // by the checkout output).
                    plan.entries.push((path, CarriedEntry::Remove));
                } else {
                    // Delete/modify: refuse without losing any version.
                    // The checkout content (the baseline modification)
                    // stays on disk and the deletion stays recorded in
                    // the snapshot; nothing is approximated.
                    plan.conflicts.push(CarriedConflict {
                        path: path.clone(),
                        reason: "carried deletion conflicts with a baseline modification; \
                     refusing without losing either version"
                            .to_string(),
                    });
                }
            }
            (Some(_), None, None) => {
                // R1: delete/delete — the snapshot removed the path and
                // the new baseline no longer has it either. Both sides
                // agree on deletion; this converges instead of
                // conflict-refusing.
                plan.entries.push((path, CarriedEntry::AlreadyCurrent));
            }
            (None, None, Some(_)) => {
                // New baseline file with no carried state: checkout output.
                plan.entries.push((path, CarriedEntry::AlreadyCurrent));
            }
            (None, None, None) => unreachable!("path came from one of the manifests"),
        }
    }
    plan
}

/// The shelf plan for one adoption: paths shelved out of the working copy
/// into `old_view`'s shelf, and paths restored from `new_view`'s shelf into
/// the working copy (R4: both transition directions execute).
#[derive(Debug, Default)]
pub(super) struct AdoptionShelfPlan {
    /// The view the `ignored` paths are shelved into (`None` when no shelf
    /// transition applies, e.g. a same-view adoption).
    pub old_view: Option<String>,
    pub ignored: Vec<String>,
    pub restored: Vec<String>,
}

fn byte_is_binary(bytes: &[u8]) -> bool {
    bytes.contains(&0)
}

/// R2: the per-path worktree observations proven immediately before the
/// mutation plan (the retained return of
/// [`Repository::prove_pre_adoption_worktree`]).
///
/// These are the only legal expected-old lease values for the adoption's
/// carried filesystem effects. Re-observing a path at execution time may
/// confirm the lease (the proven value still holds) or recognize the
/// expected-new value (the effect already landed); any third value is a
/// typed lease divergence, never a fresh lease. An editor or Git write that
/// lands between the proof and the mutation is therefore refused with every
/// byte preserved instead of being accepted and overwritten.
#[derive(Debug, Clone)]
pub(super) struct ProvenWorktree {
    observations: std::collections::BTreeMap<String, EffectValue>,
}

impl ProvenWorktree {
    /// The proven observation for `path` — its only legal expected-old value.
    fn expected_old(&self, path: &str) -> Result<EffectValue, AdoptKnownEditError> {
        self.observations.get(path).cloned().ok_or_else(|| {
            AdoptKnownEditError::Repository(RepositoryError::InvalidOperation {
                message: format!(
                    "the pre-adoption proof did not observe carried path '{path}'; \
                     refusing to plan a lease from an unproven observation"
                ),
            })
        })
    }
}

impl Repository {
    // ── R2: journal-bound authenticated capture ────────────────────────────

    /// Record the pre-transition capture as an immutable, verified journal
    /// operation (RFC §7.2 step 6 authentication via the existing operation
    /// journal — no new identity scheme).
    ///
    /// The operation payload is content-addressed (`OPERATIONS`) and carries
    /// the hash of the canonical capture facts in its evidence set; its
    /// verified receipt (`EFFECT_RECEIPTS`) is append-only. The mutable JSON
    /// record becomes a pointer to this operation: the adoption proof
    /// re-derives the facts hash against the authoritative stored payload, so
    /// tampering with or fabricating the JSON can never authenticate.
    ///
    /// Runs under the reentrant operation lock (the entry protocol owns the
    /// outer workspace acquisition; nested repository operations reuse it).
    pub(super) fn journal_pre_transition_capture(
        &self,
        working_copy: atomic_core::WorkingCopyId,
        evidence: &PreTransitionEvidence,
    ) -> Result<OperationId, RepositoryError> {
        let operation_lock = self.try_lock_operation(working_copy)?;
        self.journal_pre_transition_capture_locked(&operation_lock, working_copy, evidence)
    }

    pub(super) fn journal_pre_transition_capture_locked(
        &self,
        operation_lock: &WorkingCopyOperationLockGuard,
        working_copy: atomic_core::WorkingCopyId,
        evidence: &PreTransitionEvidence,
    ) -> Result<OperationId, RepositoryError> {
        let facts = canonical_capture_facts(evidence)?;
        let record = self.working_copy_record(working_copy)?;
        let state = RepoStateRef {
            view: None,
            working_copy: Some(super::operation::working_copy_state_ref(record)),
            git: None,
        };
        let prepared = self.prepare_working_copy_transition(
            operation_lock,
            OperationKind::Record,
            None,
            state.clone(),
            state,
            Vec::new(),
            vec![atomic_core::Hash::of(&facts)],
            ActorRef::System {
                name: PRE_TRANSITION_CAPTURE_ACTOR.to_string(),
            },
            super::operation::current_operation_timestamp_ms(),
        )?;
        self.finalize_operation_verified(operation_lock, prepared.operation().id())?;
        Ok(prepared.operation().id())
    }

    /// Whether the evidence's journal binding authenticates it against the
    /// authoritative stored operation (R2 proof, part of
    /// `pre_transition_evidence_proves`).
    fn journal_binding_proves(
        &self,
        evidence: &PreTransitionEvidence,
        working_copy: atomic_core::WorkingCopyId,
    ) -> bool {
        let Some(hex) = &evidence.journal_operation else {
            return false;
        };
        let Some(operation_id) = OperationId::from_base32(hex.as_bytes()) else {
            return false;
        };
        let operation = match self.load_operation(operation_id) {
            Ok(operation) => operation,
            Err(_) => return false,
        };
        let payload = operation.payload();
        let capture_actor = ActorRef::System {
            name: PRE_TRANSITION_CAPTURE_ACTOR.to_string(),
        };
        if payload.kind != OperationKind::Record
            || payload.working_copy != Some(working_copy)
            || payload.actor != capture_actor
        {
            return false;
        }
        let Ok(facts) = canonical_capture_facts(evidence) else {
            return false;
        };
        let facts_hash = atomic_core::Hash::of(&facts);
        if !payload.evidence.contains(&facts_hash) {
            return false;
        }
        // The journal record is authenticated only when the operation's
        // verified receipt exists (append-only, content-addressed).
        let receipts = match self.pristine.read_txn() {
            Ok(txn) => match txn.get_effect_receipts(operation_id) {
                Ok(receipts) => receipts,
                Err(_) => return false,
            },
            Err(_) => return false,
        };
        super::operation::has_operation_verified_receipt(&receipts)
    }

    // ── Entry-boundary capture (RFC §7.1 step 6) ─────────────────────────

    /// Capture pending-edit evidence at a verified workspace boundary.
    ///
    /// Called by the workspace entry protocol after the workspace is verified
    /// and aligned: a bridge-checkpointed workspace with pending edits gets a
    /// baseline-relative snapshot change plus refreshed evidence, so any
    /// later bound HEAD adoption can prove the carried edit (§7.1 step 6,
    /// §12.3). Content that the active snapshot already covers is not
    /// re-recorded; capture failure refuses the boundary (fail closed).
    pub(super) fn capture_pending_workspace_evidence(
        &self,
        working_copy: atomic_core::WorkingCopyId,
    ) -> Result<(), RepositoryError> {
        let Some(checkpoint) = read_workspace_checkpoint(self.root())? else {
            return Ok(());
        };
        let observation = crate::observe_git_metadata(self.root()).map_err(observation_map)?;
        let WorkspaceGitObservation::Repository(git) = &observation else {
            return Ok(());
        };
        // Only an observation that still matches the verified checkpoint may
        // produce evidence: the evidence must prove the checkpointed state.
        let head_matches = match &git.head {
            super::GitHeadObservation::Attached { oid, .. }
            | super::GitHeadObservation::Detached { oid } => {
                oid.eq_ignore_ascii_case(&checkpoint.git_head)
            }
            _ => false,
        };
        if !head_matches || git.head_tree.as_deref() != Some(checkpoint.git_tree.as_str()) {
            return Ok(());
        }

        let status = self.status(working_copy, super::StatusOptions::default())?;
        if std::env::var_os("ATOMIC_TRACE_CAPTURE").is_some() {
            eprintln!(
                "[capture] clean={} entries={:?}",
                status.is_clean(),
                status
                    .entries()
                    .iter()
                    .map(|entry| (
                        entry.path().display().to_string(),
                        entry.status().short_code()
                    ))
                    .collect::<Vec<_>>()
            );
        }
        // Pending work includes UNTRACKED files: snapshots record them
        // (include_untracked), so a boundary with untracked content is not a
        // clean boundary even though `FileStatus::is_dirty` only classifies
        // tracked modifications.
        if status.is_clean() && !status.has_untracked() {
            // Nothing pending: a stale evidence record would misattribute the
            // checkpointed content to a carried edit, so it is removed, and a
            // stale active snapshot (its edits absorbed by a durable record
            // against the same baseline, or superseded by an explicit
            // transition) leaves the active slot. The snapshot object itself
            // stays in the store as a retained recovery root; only the view
            // membership lease moves (§14.1, §12.12).
            let path = pre_transition_evidence_path(self.root());
            if path.exists() {
                std::fs::remove_file(&path).map_err(RepositoryError::Io)?;
            }
            self.clear_absorbed_snapshot(working_copy)?;
            // A snapshot that SURVIVES the boundary (an unknown-pre-checkout
            // opaque snapshot is never auto-absorbed — post-adoption capture
            // discipline) still needs boundary evidence pointing at it: the
            // content is durably captured and any later bound adoption must
            // find it (§7.1 step 6).
            let snapshot_status = self.snapshot_status(working_copy)?;
            if snapshot_status.snapshot.is_some() || snapshot_status.remainder.is_some() {
                let evidence = PreTransitionEvidence {
                    version: PRE_TRANSITION_EVIDENCE_VERSION,
                    working_copy: working_copy.to_string(),
                    baseline_view: checkpoint.view.clone(),
                    baseline_state: checkpoint.atomic_state.clone(),
                    snapshot: snapshot_status.snapshot.map(|hash| hash.to_base32()),
                    remainder: snapshot_status.remainder.map(|hash| hash.to_base32()),
                    git_head: checkpoint.git_head.clone(),
                    git_tree: checkpoint.git_tree.clone(),
                    git_index_tree: checkpoint.git_index_tree.clone(),
                    git_index_digest: checkpoint.git_index_digest.clone(),
                    git_index_path: Some(git.index_path.display().to_string()),
                    policy_root: policy_root_for_capture(self.root())?,
                    created_at_ms: super::operation::current_operation_timestamp_ms(),
                    journal_operation: None,
                };
                let journal_operation =
                    self.journal_pre_transition_capture(working_copy, &evidence)?;
                let mut bound = evidence;
                bound.journal_operation = Some(journal_operation.to_base32());
                return write_pre_transition_evidence(self.root(), &bound);
            }
            return Ok(());
        }

        let policy =
            super::conversion_policy_for_git(&git2::Repository::discover(self.root()).map_err(
                |error| RepositoryError::InvalidRepository {
                    reason: format!("cannot open the Git repository for capture: {error}"),
                },
            )?)
            .map_err(|error| RepositoryError::InvalidOperation {
                message: format!("cannot compute conversion policy for capture: {error}"),
            })?;
        if std::env::var_os("ATOMIC_TRACE_CAPTURE").is_some() {
            eprintln!("[capture] policy ok; reading snapshot status");
        }

        let snapshot_status = self.snapshot_status(working_copy)?;
        let snapshot_active =
            snapshot_status.snapshot.is_some() || snapshot_status.remainder.is_some();
        // The active snapshot's attribution is inherited by a superseding
        // capture: an unknown-pre-checkout snapshot is never re-recorded as
        // a known carried edit just because the boundary refreshed.
        let active_origin = snapshot_status
            .snapshot
            .and_then(|hash| self.load_change(&hash).ok())
            .and_then(|change| origin_from_change_metadata(&change.hashed.metadata));
        if snapshot_active {
            // A snapshot whose parent view is no longer the workspace's
            // baseline is stale: its edits were either absorbed by a durable
            // record (worktree clean — cleared above) or survive on a
            // different baseline. Dirty worktree: rebase the active slot
            // onto the current baseline before recording the fresh capture
            // (the membership lease moves; the object stays retained).
            let stale = self.stale_snapshot_against_baseline(working_copy, &checkpoint.view)?;
            if stale {
                self.rebase_stale_snapshot_locked(working_copy, &checkpoint.view)?;
            }
        }
        let snapshot_status = self.snapshot_status(working_copy)?;
        if std::env::var_os("ATOMIC_TRACE_CAPTURE").is_some() {
            eprintln!(
                "[capture] active={:?} remainder={:?}",
                snapshot_status.snapshot.map(|h| h.to_base32()),
                snapshot_status.remainder.map(|h| h.to_base32())
            );
        }
        let snapshot_hash = match snapshot_status.snapshot {
            Some(existing)
                if {
                    let covers = self.snapshot_covers_worktree(working_copy, existing);
                    if std::env::var_os("ATOMIC_TRACE_CAPTURE").is_some() {
                        eprintln!("[capture] covers({existing:?})={covers}");
                    }
                    covers
                } =>
            {
                // The active snapshot already covers the current content;
                // keep it and refresh only the evidence facts.
                existing
            }
            active => {
                // No snapshot yet, or the content moved on since the last
                // capture: record a superseding snapshot (NothingToRecord
                // means the status and record layers disagree about
                // snapshot-worthy content; nothing to capture). A superseded
                // snapshot's attribution is preserved: an unknown-pre-checkout
                // snapshot is never re-captured under a known-carried claim
                // just because the entry boundary refreshed it.
                let superseded_origin = active_origin.unwrap_or(AdoptionOrigin::KnownCarriedEdit);
                match self.snapshot(
                    working_copy,
                    atomic_core::change::ChangeHeader::new("Bridge pre-transition capture"),
                    super::RecordOptions::new()
                        .with_all(true)
                        .include_untracked(true)
                        .save_to_store(true)
                        .apply_after_record(true)
                        .sync_vault(false)
                        .enrich_kg(false)
                        .metadata_bytes(adoption_snapshot_metadata(
                            superseded_origin,
                            &PreTransitionEvidence {
                                version: PRE_TRANSITION_EVIDENCE_VERSION,
                                working_copy: working_copy.to_string(),
                                baseline_view: checkpoint.view.clone(),
                                baseline_state: checkpoint.atomic_state.clone(),
                                snapshot: active.map(|hash| hash.to_base32()),
                                remainder: snapshot_status.remainder.map(|hash| hash.to_base32()),
                                git_head: checkpoint.git_head.clone(),
                                git_tree: checkpoint.git_tree.clone(),
                                git_index_tree: checkpoint.git_index_tree.clone(),
                                git_index_digest: checkpoint.git_index_digest.clone(),
                                git_index_path: Some(git.index_path.display().to_string()),
                                policy_root: policy.root().content_key,
                                created_at_ms: super::operation::current_operation_timestamp_ms(),
                                journal_operation: None,
                            },
                        )),
                ) {
                    Ok(outcome) => *outcome.hash(),
                    Err(super::RecordError::NothingToRecord) => match active {
                        Some(existing) => existing,
                        None => return Ok(()),
                    },
                    Err(error) => return Err(record_map(error)),
                }
            }
        };

        let snapshot_status = self.snapshot_status(working_copy)?;
        let evidence = PreTransitionEvidence {
            version: PRE_TRANSITION_EVIDENCE_VERSION,
            working_copy: working_copy.to_string(),
            baseline_view: checkpoint.view.clone(),
            baseline_state: checkpoint.atomic_state.clone(),
            snapshot: Some(snapshot_hash.to_base32()),
            remainder: snapshot_status.remainder.map(|hash| hash.to_base32()),
            git_head: checkpoint.git_head.clone(),
            git_tree: checkpoint.git_tree.clone(),
            git_index_tree: checkpoint.git_index_tree.clone(),
            git_index_digest: checkpoint.git_index_digest.clone(),
            git_index_path: Some(git.index_path.display().to_string()),
            policy_root: policy.root().content_key,
            created_at_ms: super::operation::current_operation_timestamp_ms(),
            journal_operation: None,
        };
        let journal_operation = self.journal_pre_transition_capture(working_copy, &evidence)?;
        let mut bound = evidence;
        bound.journal_operation = Some(journal_operation.to_base32());
        write_pre_transition_evidence(self.root(), &bound)
    }

    /// Re-parent the private snapshot view onto `baseline_view` while KEEPING
    /// its active changes (the crashed-adoption retry path: the journal may
    /// already have re-parented the view onto the adopted ephemeral baseline).
    pub(super) fn reparent_snapshot_view_locked(
        &self,
        working_copy: atomic_core::WorkingCopyId,
        baseline_view: &str,
    ) -> Result<(), RepositoryError> {
        let snapshot_view = Self::snapshot_view_name(working_copy);
        let (view_state, baseline_id) = {
            let txn = self
                .pristine
                .read_txn()
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
            let view = txn
                .get_view(&snapshot_view)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or(RepositoryError::ViewNotFound {
                    name: snapshot_view.clone(),
                })?;
            let baseline = txn
                .get_view(baseline_view)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or(RepositoryError::ViewNotFound {
                    name: baseline_view.to_string(),
                })?;
            (view, baseline.id)
        };
        if view_state.parent == Some(baseline_id) {
            return Ok(());
        }
        let operation_lock = self.try_lock_operation(working_copy)?;
        if let super::OperationHeadState::Diverged(heads) =
            self.consolidate_operation_heads_locked(&operation_lock)?
        {
            return Err(RepositoryError::OperationHeadsDiverged {
                scope: atomic_core::operation::OperationScope::WorkingCopy(working_copy)
                    .to_string(),
                heads: heads.iter().map(ToString::to_string).collect(),
            });
        }
        let before_record = self.working_copy_record(working_copy)?;
        let state_ref = atomic_core::operation::RepoStateRef {
            view: Some(atomic_core::operation::ViewStateRef {
                name: baseline_view.to_string(),
                state: new_state_for_view(self, baseline_view)?,
                set_id: None,
            }),
            working_copy: Some(super::operation::working_copy_state_ref(
                before_record.clone(),
            )),
            git: None,
        };
        let encode = |view: &atomic_core::pristine::ViewState| {
            postcard::to_allocvec(&super::operation::ViewLeaseValue {
                parent: view.parent,
            })
            .map_err(|error| RepositoryError::Serialization(error.to_string()))
            .map(atomic_core::operation::MetadataValue::Bytes)
        };
        let mut expected_new = view_state.clone();
        expected_new.parent = Some(baseline_id);
        let operation = self.prepare_metadata_operation(
            &operation_lock,
            OperationKind::Record,
            None,
            state_ref.clone(),
            state_ref,
            vec![atomic_core::operation::MetadataTransition {
                target: atomic_core::operation::MetadataTarget::View {
                    name: snapshot_view,
                },
                expected_old: encode(&view_state)?,
                expected_new: encode(&expected_new)?,
            }],
            Vec::new(),
            ActorRef::System {
                name: "bridge-snapshot-reparent".to_string(),
            },
            super::operation::current_operation_timestamp_ms(),
        )?;
        self.apply_operation_metadata_locked(&operation_lock, operation.id())?;
        self.finalize_operation_verified(&operation_lock, operation.id())?;
        Ok(())
    }

    // ── Reassembly planning (§7.3 branch 2) ──────────────────────────────

    /// Plan the reassembly of the proven carried edit against the new durable
    /// baseline. Delegates to [`plan_carried_reassembly_trees`].
    pub(super) fn plan_carried_reassembly<'a>(
        &self,
        old_manifest: &'a super::ProjectTree,
        carried_manifest: &'a super::ProjectTree,
        new_manifest: &'a super::ProjectTree,
    ) -> CarriedReassemblyPlan {
        plan_carried_reassembly_trees(old_manifest, carried_manifest, new_manifest)
    }

    // ── Reassembly planning (§7.3 branch 2) ──────────────────────────────

    // ── Journaled adoption execution (§7.3 branches 2–3) ─────────────────

    /// Whether the durable evidence proves the pre-transition state for this
    /// adoption (§7.3 branch 2 preconditions, ac-1, review blocker R2):
    ///
    /// - old HEAD/tree, baseline view AND state, and the conversion policy
    ///   must match the verified checkpoint, and the captured primary-index
    ///   facts must equal the checkpoint's recorded index facts (a tampered
    ///   or stale evidence record is not proof);
    /// - the observed Git state must still be the same repository, HEAD, and
    ///   primary index path (alternate-index evidence is never proof);
    /// - the referenced snapshot must be a durable, re-readable change owned
    ///   by this working copy AND must be the working copy's ACTIVE snapshot
    ///   (a different active snapshot means the captured state moved on);
    /// - the baseline view must still sit at the evidence's state (an
    ///   advanced baseline invalidates the proof).
    ///
    /// Unsupported exact attribution fails closed: any mismatch routes the
    /// adoption to the unknown-origin branch instead of pretending a partial
    /// match is proof.
    pub(super) fn pre_transition_evidence_proves(
        &self,
        evidence: &PreTransitionEvidence,
        checkpoint: &super::workspace_txn::WorkspaceCheckpoint,
        working_copy: atomic_core::WorkingCopyId,
        policy: &super::project_tree::ConversionPolicy,
        observation: &WorkspaceGitObservation,
    ) -> bool {
        let WorkspaceGitObservation::Repository(git) = observation else {
            return false;
        };
        if evidence.working_copy != working_copy.to_string()
            || !evidence.git_head.eq_ignore_ascii_case(&checkpoint.git_head)
            || !evidence.git_tree.eq_ignore_ascii_case(&checkpoint.git_tree)
            || evidence.baseline_view != checkpoint.view
            || evidence.baseline_state != checkpoint.atomic_state
        {
            return false;
        }
        if evidence.policy_root != policy.root().content_key {
            return false;
        }
        // R2: the captured index facts are part of the proof. The evidence
        // must have been captured against exactly the index state the
        // verified checkpoint records (a tampered digest/tree is not proof),
        // and the observed index must still be the primary index the
        // evidence was captured against (alternate-index guard, ac-2).
        if evidence.git_index_tree != checkpoint.git_index_tree
            || evidence.git_index_digest != checkpoint.git_index_digest
        {
            return false;
        }
        let observed_index_path = git.index_path.display().to_string();
        match (&evidence.git_index_path, Some(&observed_index_path)) {
            (Some(captured), Some(observed)) if captured == observed => {}
            _ => return false,
        }
        // R2: an alternate index (GIT_INDEX_FILE) is never the primary index
        // the evidence was captured against — the metadata observation above
        // resolves libgit2's default index path, so an in-use alternate must
        // fail the proof explicitly.
        if std::env::var_os("GIT_INDEX_FILE").is_some() {
            return false;
        }
        let Some(snapshot_hex) = &evidence.snapshot else {
            return false;
        };
        let Some(snapshot) = atomic_core::Hash::from_base32(snapshot_hex.as_bytes()) else {
            return false;
        };
        let snapshot_owned = match self.load_change(&snapshot) {
            Ok(change) => change.kind() == &ChangeKind::Snapshot { working_copy },
            Err(_) => false,
        };
        if !snapshot_owned {
            return false;
        }
        // R2: the proof must describe the working copy's ACTIVE snapshot, not
        // merely any owned snapshot object.
        self.active_snapshot_is(working_copy, snapshot)
            // R2: the baseline view must still sit at the evidence's state — an
            // advanced baseline (a durable record landed after capture) makes the
            // evidence stale.
            && self.baseline_view_is_at(evidence)
            // R2 journal binding: the evidence is authenticated only through its
            // immutable, verified journal operation. A mutable JSON field
            // comparison alone is not authentication — every tampered,
            // fabricated, or pre-journal evidence record fails here and the
            // caller falls back to unknown origin preserving the bytes.
            && self.journal_binding_proves(evidence, working_copy)
    }

    /// Whether the working copy's active snapshot view holds exactly `hash`
    /// as its snapshot-kind direct change (evidence-freshness, R2).
    fn active_snapshot_is(
        &self,
        working_copy: atomic_core::WorkingCopyId,
        hash: atomic_core::Hash,
    ) -> bool {
        match self.snapshot_status(working_copy) {
            Ok(status) => status.snapshot == Some(hash),
            Err(_) => false,
        }
    }

    /// Whether the baseline view's current state still equals the evidence's
    /// captured baseline state (advanced-baseline guard, R2).
    fn baseline_view_is_at(&self, evidence: &PreTransitionEvidence) -> bool {
        let Ok(state) = new_state_for_view(self, &evidence.baseline_view) else {
            return false;
        };
        use atomic_core::types::Base32;
        state.to_base32() == evidence.baseline_state
    }

    /// Whether the working copy's active snapshot is already the published
    /// replacement snapshot of an adoption against `new_head` (post-adoption
    /// capture discipline, R2/R3): its hashed marker records the adopted HEAD.
    /// A crashed adoption that already published its snapshot must not be
    /// re-captured as unknown pre-checkout origin on the retry entry.
    pub(super) fn adoption_snapshot_already_published(
        &self,
        working_copy: atomic_core::WorkingCopyId,
        new_head: &str,
    ) -> Result<bool, RepositoryError> {
        let status = self.snapshot_status(working_copy)?;
        let Some(hash) = status.snapshot else {
            return Ok(false);
        };
        let change = self.load_change(&hash)?;
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&change.hashed.metadata) else {
            return Ok(false);
        };
        let Some(published_head) = value
            .get("atomic-bridge")
            .and_then(|marker| marker.get("evidence"))
            .and_then(|evidence| evidence.get("git_head"))
            .and_then(serde_json::Value::as_str)
        else {
            return Ok(false);
        };
        Ok(published_head.eq_ignore_ascii_case(new_head))
    }

    /// Deterministic WIP-ref operation identity for one adoption attempt
    /// (create-only reflog names, CB-0B machinery).
    pub(super) fn adoption_wip_operation(old_head: &str, new_head: &str) -> String {
        format!("adopt-{old_head}-to-{new_head}")
    }

    /// Plan the shelf effect set for a mapped-view adoption (§7.3 branch 4:
    /// collision-safe shelf plan; tracked and indexed paths are never shelved).
    ///
    /// Returns the ignored paths planned for shelving into `old_view` and the
    /// paths planned for restoration from `new_view`'s shelf. Index
    /// observation and raw path identity fail closed: an unenumerable index
    /// or a non-UTF-8 tracked/indexed path refuses planning before any
    /// mutation (R4/R5).
    #[allow(unused_variables)]
    pub(super) fn plan_adoption_shelf_effects(
        &self,
        operation_lock: &WorkingCopyOperationLockGuard,
        working_copy: atomic_core::WorkingCopyId,
        old_view: &str,
        new_view: &str,
        effects: &mut Vec<EffectPlan>,
    ) -> Result<AdoptionShelfPlan, RepositoryError> {
        if old_view == new_view {
            return Ok(AdoptionShelfPlan::default());
        } // Tracked candidates: every path graph-visible on either view plus
          // every Git-indexed path (stage 0). Indexed paths are never shelved.
        let mut tracked: std::collections::HashSet<String> = std::collections::HashSet::new();
        {
            let txn = self
                .pristine
                .read_txn()
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
            for view_name in [old_view, new_view] {
                let Some(view) = txn
                    .get_view(view_name)
                    .map_err(|error| RepositoryError::Database(error.to_string()))?
                else {
                    continue;
                };
                let visibility = super::graph_visibility_from_membership(
                    &txn,
                    &super::view_membership(&txn, &view)?,
                )?;
                let projection = self.project_tree_for_visibility(&txn, &visibility)?;
                for path in projection.present.keys() {
                    let Some(path) = std::str::from_utf8(path.as_bytes()).ok() else {
                        return Err(RepositoryError::InvalidOperation {
                            message: format!(
                                "graph-visible path {:?} is not valid UTF-8; adoption shelf \
                                 planning refuses before any mutation so tracked content can \
                                 never be shelved or aliased",
                                path.as_bytes(),
                            ),
                        });
                    };
                    tracked.insert(path.to_string());
                }
            }
        }
        let policy =
            super::conversion_policy_for_git(&git2::Repository::discover(&self.root).map_err(
                |error| RepositoryError::InvalidRepository {
                    reason: format!("cannot open the Git repository for the shelf plan: {error}"),
                },
            )?)
            .map_err(|error| RepositoryError::InvalidOperation {
                message: format!("cannot compute conversion policy for the shelf plan: {error}"),
            })?;
        // R4: index enumeration is load-bearing (indexed paths must be
        // excluded from shelving), so an observation failure fails closed
        // instead of planning against an unknown protection set.
        let index = crate::observe_git_index(&self.root, &policy).map_err(observation_map)?;
        for entry in &index.entries {
            if entry.stage != 0 {
                continue;
            }
            let Some(path) = std::str::from_utf8(entry.path.as_bytes()).ok() else {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "indexed path {:?} is not valid UTF-8; adoption shelf planning refuses \
                         before any mutation so indexed content can never be shelved or aliased",
                        entry.path.as_bytes(),
                    ),
                });
            };
            tracked.insert(path.to_string());
        }
        let ignored = self.collect_switch_ignored_paths(&tracked);
        self.plan_shelve_ignored_paths(working_copy, old_view, &ignored, effects)?;
        let mut restored = Vec::new();
        let new_ws =
            super::switch::working_copy_workspace_path(&self.dot_dir, working_copy, new_view);
        if new_ws.is_dir() {
            for path in self.collect_ignored_paths_in_workspace(&new_ws) {
                let before = effects.len();
                self.plan_restore_ignored_paths(working_copy, new_view, &path, effects)?;
                if effects.len() > before {
                    restored.push(path);
                }
            }
        }
        Ok(AdoptionShelfPlan {
            old_view: Some(old_view.to_string()),
            ignored,
            restored,
        })
    }

    /// Execute one planned filesystem effect and append its leased receipt.
    pub(super) fn execute_and_record_effect(
        &self,
        operation_lock: &WorkingCopyOperationLockGuard,
        operation_id: atomic_core::OperationId,
        ordinal: u32,
        content: Option<&[u8]>,
    ) -> Result<(), RepositoryError> {
        let pending =
            self.execute_filesystem_effect(operation_lock, operation_id, ordinal, content)?;
        self.record_pending_filesystem_effect(operation_lock, operation_id, pending)?;
        Ok(())
    }

    /// R2: prove the pre-adoption worktree state before any mutation, and
    /// return the proven per-path observations.
    ///
    /// For every path in the union of the old-baseline, snapshot, and adopted
    /// manifests, the observed worktree value must be either the snapshot's
    /// own materialization (the interrupted checkout never rewrote the path)
    /// or the adopted baseline's content (the checkout rewrote it). Anything
    /// else — a newer post-capture edit, a post-capture deletion, or an
    /// unexplained extra file — fails the known-edit proof
    /// ([`AdoptKnownEditError::CarriedStateUnproven`]) so the caller captures
    /// the differences as unknown pre-checkout origin.
    ///
    /// The proven observations are RETAINED and returned: they are the only
    /// legal expected-old lease values for this adoption's carried effects.
    /// A later re-observation of the same path (the executor's lease check)
    /// may confirm the lease or refuse it with a typed divergence, but no
    /// code path may refresh `expected_old` from bytes observed after the
    /// proof — a newer editor write can therefore never be accepted as a
    /// lease and overwritten (R2 proof-to-lease TOCTOU).
    fn prove_pre_adoption_worktree<'a>(
        &self,
        working_copy: atomic_core::WorkingCopyId,
        old_manifest: &'a super::ProjectTree,
        carried_manifest: &'a super::ProjectTree,
        new_manifest: &'a super::ProjectTree,
        filter: &crate::content_filter::GitAttributesFilter,
    ) -> Result<ProvenWorktree, AdoptKnownEditError> {
        let mut proven: std::collections::BTreeMap<String, EffectValue> =
            std::collections::BTreeMap::new();
        let included = |tree: &'a super::ProjectTree| -> Vec<&'a super::RepositoryEntry> {
            tree.manifest
                .entries
                .iter()
                .filter(|entry| entry.disposition == super::ManifestDisposition::Included)
                .collect()
        };
        let snapshot_entries: std::collections::HashMap<String, &super::RepositoryEntry> =
            included(carried_manifest)
                .into_iter()
                .map(|entry| {
                    (
                        String::from_utf8_lossy(entry.path.as_bytes()).into_owned(),
                        entry,
                    )
                })
                .collect();
        let baseline_entries: std::collections::HashMap<String, &super::RepositoryEntry> =
            included(new_manifest)
                .into_iter()
                .map(|entry| {
                    (
                        String::from_utf8_lossy(entry.path.as_bytes()).into_owned(),
                        entry,
                    )
                })
                .collect();
        let mut paths: std::collections::BTreeSet<String> = included(old_manifest)
            .into_iter()
            .map(|entry| String::from_utf8_lossy(entry.path.as_bytes()).into_owned())
            .collect();
        paths.extend(snapshot_entries.keys().cloned());
        paths.extend(baseline_entries.keys().cloned());

        let effect_value =
            |entry: &super::RepositoryEntry| -> Result<EffectValue, AdoptKnownEditError> {
                let path_string = String::from_utf8_lossy(entry.path.as_bytes()).into_owned();
                let smudged = crate::content_filter::ContentFilter::smudge(
                    filter,
                    std::path::Path::new(&path_string),
                    &entry.repository_bytes,
                )
                .map_err(|error| {
                    AdoptKnownEditError::Repository(RepositoryError::InvalidOperation {
                        message: format!(
                            "cannot smudge expected materialization for '{}': {error}",
                            entry.path.escaped(),
                        ),
                    })
                })?;
                Ok(EffectValue::File(FileState {
                    kind: match entry.kind {
                        InodeKind::Regular => FileKind::Regular,
                        InodeKind::Symlink => FileKind::Symlink,
                        InodeKind::Gitlink => FileKind::Gitlink,
                    },
                    mode: u32::from(entry.mode),
                    content: atomic_core::Hash::of(&smudged.bytes),
                }))
            };

        for path in &paths {
            let mut allowed: Vec<EffectValue> = Vec::with_capacity(2);
            match snapshot_entries.get(path) {
                Some(entry) => allowed.push(effect_value(entry)?),
                None => allowed.push(EffectValue::Absent),
            }
            match baseline_entries.get(path) {
                Some(entry) => allowed.push(effect_value(entry)?),
                // A path absent from the adopted baseline was removed by the
                // checkout: an absent worktree value is its legal output.
                None => allowed.push(EffectValue::Absent),
            }
            let observed = self
                .observe_filesystem_effect(
                    working_copy,
                    &EffectTarget::FilesystemPath { path: path.clone() },
                )
                .map_err(AdoptKnownEditError::Repository)?;
            // R2: the observation is proven the instant it is checked. It is
            // retained verbatim as this path's only legal expected-old lease
            // value; nothing after this point may replace it with a fresher
            // observation of the same path.
            proven.insert(path.clone(), observed.clone());
            if allowed.contains(&observed) {
                continue;
            }
            return Err(AdoptKnownEditError::CarriedStateUnproven(format!(
                "path '{path}' holds post-capture content that the pre-transition snapshot and \
                 the adopted baseline cannot explain (observed {observed:?}); the known-carried \
                 attribution is unsupported and the difference must be captured as unknown \
                 pre-checkout origin"
            )));
        }

        // Unexplained extra files: anything on disk that is neither ignored
        // nor part of the captured state cannot be attributed to the carried
        // edit (a file created after the capture would silently flip the
        // replacement snapshot's attribution).
        //
        // R8: the gitlink boundary is manifest-authoritative — every path
        // declared as a submodule in ANY of the three manifests is never
        // descended into, independent of any on-disk `.git` marker (an
        // initialized submodule's gitfile, an embedded `.git` directory, or
        // an uninitialized submodule with no marker at all).
        let known_gitlinks =
            manifest_gitlink_paths(&[old_manifest, carried_manifest, new_manifest]);
        let unexplained = self
            .unexplained_worktree_paths(&paths, &known_gitlinks)
            .map_err(|error| {
                AdoptKnownEditError::CarriedStateUnproven(format!(
                    "the unexplained-worktree scan failed ({error}); a partial scan cannot \
                     establish the known-carried attribution, so the differences must be \
                     captured as unknown pre-checkout origin"
                ))
            })?;
        if let Some(first) = unexplained.first() {
            return Err(AdoptKnownEditError::CarriedStateUnproven(format!(
                "worktree holds {} unexplained path(s) outside the captured state (first: \
                 '{first}'); the known-carried attribution is unsupported and the difference \
                 must be captured as unknown pre-checkout origin",
                unexplained.len()
            )));
        }
        Ok(ProvenWorktree {
            observations: proven,
        })
    }

    /// Relative non-ignored worktree paths that are outside `known` (R2):
    /// the post-capture untracked complement of the manifest union.
    ///
    /// R7: the scan is a total observation, never a best-effort one — a
    /// directory or entry that cannot be observed is a typed error, because
    /// a partial scan cannot establish the known-carried attribution. The
    /// walk never follows symbolic links: directory entries are classified
    /// with no-follow file types, symlinked directories are treated as
    /// leaf entries (never recursed — a cycle cannot loop and the walk
    /// cannot leave the root). R8: gitlink identity is manifest-authoritative
    /// — `known_gitlinks` (every path whose manifest entry kind is
    /// `InodeKind::Gitlink`) is never descended into whether or not it owns
    /// an on-disk `.git` marker. `.atomic/` and `.git/` stay excluded.
    fn unexplained_worktree_paths(
        &self,
        known: &std::collections::BTreeSet<String>,
        known_gitlinks: &std::collections::BTreeSet<String>,
    ) -> Result<Vec<String>, RepositoryError> {
        let rules = self.load_ignore_rules();
        // R7 deterministic error injection — opt-in test instrumentation
        // (feature `adoption-test-injection`): when set to a root-relative
        // directory path, the walk reports an enumeration failure at that
        // directory so tests can prove error propagation without depending
        // on permission bits (which do not fail for the root user). The
        // shipping build never reads the variable and always walks totally.
        #[cfg(feature = "adoption-test-injection")]
        let inject_read_dir_error = std::env::var_os("ATOMIC_INJECT_WALK_READ_DIR_ERROR");
        #[cfg(not(feature = "adoption-test-injection"))]
        let inject_read_dir_error: Option<std::ffi::OsString> = None;
        let mut unexplained = Vec::new();
        walk_unexplained_worktree(
            &self.root,
            &self.root,
            &rules,
            known,
            known_gitlinks,
            inject_read_dir_error.as_deref(),
            &mut unexplained,
        )?;
        unexplained.sort();
        unexplained.dedup();
        Ok(unexplained)
    }

    /// The known-carried-edit branch (§7.3 branch 2): reassemble the proven
    /// carried edit against the new durable baseline through one journaled
    /// operation (WIP ref → shelf effects → carried writes → leased record
    /// advance + view re-parent), then record the replacement snapshot
    /// against the new baseline and verify the complete result.
    ///
    /// Errors split in two: [`AdoptKnownEditError::Repository`] is a typed
    /// preserving failure, while [`AdoptKnownEditError::CarriedStateUnproven`]
    /// means the pre-captured materialization no longer describes the
    /// worktree (R2) — the caller must fall back to the unknown-origin
    /// branch, never overwrite the newer bytes under a known attribution.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn adopt_known_edit_locked(
        &mut self,
        operation_lock: &WorkingCopyOperationLockGuard,
        working_copy: atomic_core::WorkingCopyId,
        evidence: &PreTransitionEvidence,
        checkpoint: &super::workspace_txn::WorkspaceCheckpoint,
        policy: &super::project_tree::ConversionPolicy,
        new_view: &str,
        new_state: atomic_core::types::Merkle,
        new_head: &str,
    ) -> Result<AdoptedEditOutcome, AdoptKnownEditError> {
        let old_view = checkpoint.view.clone();
        // A previous interrupted attempt may have re-parented the snapshot
        // view onto the adopted baseline already; restore it onto the
        // evidence baseline (keeping its changes) before planning, so the
        // carried manifest is the snapshot applied over the proven baseline.
        self.reparent_snapshot_view_locked(working_copy, &old_view)?;
        let old_manifest = self.project_tree(&old_view, policy).map_err(|error| {
            RepositoryError::InvalidOperation {
                message: format!("cannot project the pre-checkout baseline: {error}"),
            }
        })?;
        let snapshot_view = Self::snapshot_view_name(working_copy);
        let carried_manifest = self.project_tree(&snapshot_view, policy).map_err(|error| {
            RepositoryError::InvalidOperation {
                message: format!("cannot project the snapshot view: {error}"),
            }
        })?;
        let new_manifest = self.project_tree(new_view, policy).map_err(|error| {
            RepositoryError::InvalidOperation {
                message: format!("cannot project the adopted baseline: {error}"),
            }
        })?;

        let plan = self.plan_carried_reassembly(&old_manifest, &carried_manifest, &new_manifest);
        if !plan.is_separable() {
            // ac-2: an inseparable split fails structurally and preserves the
            // snapshot and evidence; nothing is approximated by file path.
            let detail = plan
                .conflicts
                .iter()
                .map(|conflict| format!("{}: {}", conflict.path, conflict.reason))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(RepositoryError::HeadAdoptionRefused {
                head: new_head.to_string(),
                reason: format!(
                    "the pre-captured edit cannot be re-assembled against the new baseline \
                     without losing content; preserved as-is: {detail}"
                ),
            }
            .into());
        }

        // R2: prove the post-checkout worktree before any mutation. The only
        // legal pre-adoption states are the snapshot's own materialization
        // (the interrupted checkout never rewrote the path) and the adopted
        // baseline's content (the checkout rewrote it). Anything else — in
        // particular newer editor bytes — means the captured materialization
        // no longer describes the worktree: the known attribution is
        // unsupported and the caller must capture the differences as unknown
        // pre-checkout origin instead of overwriting them. The proven
        // observations are retained: they are the only values the leases
        // below may use as expected-old.
        let filter = crate::content_filter::GitAttributesFilter::for_repository(&self.root);
        let proof_observation = crate::observe_git_metadata(&self.root).map_err(observation_map)?;
        let proven = self.prove_pre_adoption_worktree(
            working_copy,
            &old_manifest,
            &carried_manifest,
            &new_manifest,
            &filter,
        )?;

        // R2 deterministic race injection — opt-in test instrumentation
        // (feature `adoption-test-injection`): when compiled in, the
        // enumerated action named by ATOMIC_INJECT_ADOPTION_AFTER_PROOF runs
        // between the pre-adoption proof and the mutation plan so a test can
        // land an external editor/Git write inside that window
        // deterministically instead of racing a scheduler. The shipping
        // build compiles a no-op stub: the variable is never read and
        // nothing can execute here.
        adoption_injection(&self.root, "ATOMIC_INJECT_ADOPTION_AFTER_PROOF")?;

        // §6.4: the WIP ref protects tracked bytes before the first effect.
        let wip = crate::wip::capture_or_reuse_tracked_wip(crate::wip::WipCaptureRequest::new(
            &self.root,
            &working_copy.to_string(),
            &Self::adoption_wip_operation(&checkpoint.git_head, new_head),
        ))
        .map_err(|error| {
            AdoptKnownEditError::Repository(RepositoryError::InvalidOperation {
                message: format!("cannot protect pending work with a WIP ref: {error}"),
            })
        })?;

        // Journaled operation: shelf plans + carried writes + leased record
        // advance (+ view re-parent when the snapshot view needs it).
        let before_record = self.working_copy_record(working_copy)?;
        let (new_view_id, _new_view_state) = {
            let txn = self
                .pristine
                .read_txn()
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
            let view = txn
                .get_view(new_view)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| RepositoryError::ViewNotFound {
                    name: new_view.to_string(),
                })?;
            if view.state != new_state {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "adopted view '{new_view}' advanced past the binding state during reassembly"
                    ),
                }
                .into());
            }
            (view.id, view.state)
        };

        let mut after_record = before_record.clone();
        after_record.desired_view = new_view_id;
        after_record.desired_state = new_state;
        after_record.materialized_state = Some(new_state);

        let mut effects = Vec::new();
        let shelf = self.plan_adoption_shelf_effects(
            operation_lock,
            working_copy,
            &old_view,
            new_view,
            &mut effects,
        )?;
        // R4: the shelf plan owns exactly the effects above; the carried
        // effects planned below start at the first non-shelf ordinal. The
        // execution loop uses this boundary to skip every shelf effect —
        // they execute through their own leased transitions exactly once
        // (execute_shelve_path/execute_restore_path) and never demand
        // carried content or run again in the carried loop.
        let shelf_effect_count = effects.len() as u32;

        for (path, entry) in &plan.entries {
            let target = EffectTarget::FilesystemPath { path: path.clone() };
            match entry {
                CarriedEntry::AlreadyCurrent => {}
                CarriedEntry::Remove => {
                    // R2: the lease is the value proven before the mutation
                    // plan — never a fresh observation. If the path moved on
                    // after the proof (newer editor bytes at a path the
                    // carried edit deletes), the executor classifies the
                    // third value as diverged and refuses; the newer bytes
                    // are preserved.
                    let expected_old = proven.expected_old(path)?;
                    if expected_old == EffectValue::Absent {
                        continue;
                    }
                    effects.push(EffectPlan {
                        ordinal: effects.len() as u32,
                        target,
                        expected_old,
                        expected_new: EffectValue::Absent,
                    });
                }
                CarriedEntry::Write {
                    repository_bytes,
                    mode,
                    kind,
                } => {
                    let smudged = crate::content_filter::ContentFilter::smudge(
                        &filter,
                        std::path::Path::new(path),
                        repository_bytes,
                    )
                    .map_err(|error| RepositoryError::InvalidOperation {
                        message: format!("cannot smudge reassembled '{path}': {error}"),
                    })?;
                    let expected_new = EffectValue::File(FileState {
                        kind: match kind {
                            InodeKind::Regular => FileKind::Regular,
                            InodeKind::Symlink => FileKind::Symlink,
                            InodeKind::Gitlink => FileKind::Gitlink,
                        },
                        mode: u32::from(*mode),
                        content: atomic_core::Hash::of(&smudged.bytes),
                    });
                    // R2: same lease discipline as Remove — expected-old is
                    // the proven observation, so a post-proof edit is a
                    // divergence, never a freshly accepted lease.
                    let expected_old = proven.expected_old(path)?;
                    if expected_old == expected_new {
                        continue;
                    }
                    effects.push(EffectPlan {
                        ordinal: effects.len() as u32,
                        target,
                        expected_old,
                        expected_new,
                    });
                }
            }
        }

        // R2: revalidate every proven worktree observation immediately
        // before the mutation is prepared. Re-observation may CONFIRM the
        // proven lease or REFUSE it — a third value (an editor write that
        // landed inside the proof-to-plan window) makes the known
        // attribution unsupported: the caller falls back to unknown origin,
        // the newer bytes are preserved and truthfully attributed, never
        // overwritten, never recorded under the known marker. Without this
        // pass, a path whose reassembled output already equals the carried
        // content plans no effect at all, so its post-proof mutation would
        // otherwise flow silently into the replacement snapshot.
        for (path, proven_value) in &proven.observations {
            let observed = self
                .observe_filesystem_effect(
                    working_copy,
                    &EffectTarget::FilesystemPath { path: path.clone() },
                )
                .map_err(AdoptKnownEditError::Repository)?;
            if &observed != proven_value {
                return Err(AdoptKnownEditError::CarriedStateUnproven(format!(
                    "path '{path}' changed between the pre-adoption proof and the mutation \
                     plan (proven {proven_value:?}, now {observed:?}); the known-carried \
                     attribution is unsupported and the difference must be captured as \
                     unknown pre-checkout origin"
                )));
            }
        }

        // R2: revalidate the Git facts before the mutation is prepared. The
        // proof bound the worktree; HEAD, the index path/digest, and the
        // repository state must still be the observation the proof ran
        // against. An external `git add`/`checkout` inside the window makes
        // the known attribution unsupported — the caller falls back to the
        // unknown-origin branch instead of adopting against moved facts.
        // (This refuses; it never re-anchors the leases to the newer state.)
        let mutation_observation =
            crate::observe_git_metadata(&self.root).map_err(observation_map)?;
        if mutation_observation.token() != proof_observation.token() {
            return Err(AdoptKnownEditError::CarriedStateUnproven(format!(
                "git state changed between the pre-adoption proof and the mutation plan \
                 (first {:?}, then {:?}); the known-carried attribution is unsupported and \
                 the differences must be captured as unknown pre-checkout origin",
                proof_observation.token(),
                mutation_observation.token(),
            )));
        }

        // Leased record advance as the last effect (mirror journal_import_git_head).
        let working_copy_target = EffectTarget::WorkingCopy { working_copy };
        effects.push(EffectPlan {
            ordinal: effects.len() as u32,
            target: working_copy_target.clone(),
            expected_old: EffectValue::WorkingCopy(super::operation::working_copy_state_ref(
                before_record.clone(),
            )),
            expected_new: EffectValue::WorkingCopy(super::operation::working_copy_state_ref(
                after_record.clone(),
            )),
        });

        // View re-parent lease when the snapshot view still points at the old
        // baseline (ac-1 supersession across baselines).
        let mut metadata = Vec::new();
        {
            let txn = self
                .pristine
                .read_txn()
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
            if let Some(snapshot_state) = txn
                .get_view(&snapshot_view)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
            {
                if snapshot_state.parent != Some(new_view_id) {
                    let mut expected_new = snapshot_state.clone();
                    expected_new.parent = Some(new_view_id);
                    let encode = |view: &atomic_core::pristine::ViewState| {
                        postcard::to_allocvec(&super::operation::ViewLeaseValue {
                            parent: view.parent,
                        })
                        .map_err(|error| RepositoryError::Serialization(error.to_string()))
                        .map(atomic_core::operation::MetadataValue::Bytes)
                    };
                    metadata.push(atomic_core::operation::MetadataTransition {
                        target: atomic_core::operation::MetadataTarget::View {
                            name: snapshot_view.clone(),
                        },
                        expected_old: encode(&snapshot_state)?,
                        expected_new: encode(&expected_new)?,
                    });
                }
            }
        }

        let before_state = atomic_core::operation::RepoStateRef {
            view: Some(atomic_core::operation::ViewStateRef {
                name: old_view.clone(),
                state: new_state_for_view(self, &old_view)?,
                set_id: None,
            }),
            working_copy: Some(super::operation::working_copy_state_ref(
                before_record.clone(),
            )),
            git: None,
        };
        let after_state = atomic_core::operation::RepoStateRef {
            view: Some(atomic_core::operation::ViewStateRef {
                name: new_view.to_string(),
                state: new_state,
                set_id: None,
            }),
            working_copy: Some(super::operation::working_copy_state_ref(
                after_record.clone(),
            )),
            git: None,
        };

        let evidence_hash = atomic_core::Hash::of(
            serde_json::to_vec(evidence)
                .map_err(|error| RepositoryError::Serialization(error.to_string()))?
                .as_slice(),
        );
        let prepared = self.prepare_working_copy_transition_with_metadata(
            operation_lock,
            OperationKind::ImportGitHead,
            None,
            before_state,
            after_state,
            effects,
            metadata,
            vec![
                evidence_hash,
                atomic_core::Hash::of(wip.tree_oid.as_bytes()),
            ],
            ActorRef::System {
                name: "git-head-adoption-reassembly".to_string(),
            },
            super::operation::current_operation_timestamp_ms(),
        )?;
        let operation_id = prepared.operation().id();

        let execute_result = (|| -> Result<(), RepositoryError> {
            // ac-4 failpoint: crash before the shelf effects.
            adoption_failpoint("ATOMIC_FAIL_ADOPTION_BEFORE_SHELF")?;
            // 1. Shelf moves first (ignored artifacts never carry the carried
            //    edit; they swap per view) — and the restore direction runs
            //    too (R4): artifacts shelved for the adopted view come back
            //    through their own leased transitions, never through carried
            //    content and never as empty bytes.
            for (index, path) in shelf.ignored.iter().enumerate() {
                self.execute_shelve_path(operation_lock, &prepared, &old_view, path)?;
                adoption_shelf_ordinal_failpoint("shelve", index)?;
            }
            for (index, path) in shelf.restored.iter().enumerate() {
                self.execute_restore_path(operation_lock, &prepared, new_view, path)?;
                adoption_shelf_ordinal_failpoint("restore", index)?;
            }
            // ac-4 failpoint: crash after the shelf effects, before the
            // carried writes.
            adoption_failpoint("ATOMIC_FAIL_ADOPTION_AFTER_SHELF")?;
            // 2. Carried writes through leased effects. The merged content
            //    exists only in the plan, so each write carries its smudged
            //    bytes explicitly; an effect without planned carried content
            //    fails closed (R4) — empty bytes are never substituted.
            //    R4 partition: effects below the shelf boundary are shelf
            //    work — they already executed through their own leased
            //    transitions above and are never re-scanned or re-executed
            //    here; only carried filesystem effects reach this loop.
            for effect in &prepared.operation().payload().delta.effects {
                if effect.ordinal < shelf_effect_count {
                    continue;
                }
                let path = match &effect.target {
                    EffectTarget::FilesystemPath { path } => path,
                    _ => continue,
                };
                let content = match &effect.expected_new {
                    EffectValue::File(_) => {
                        let Some((
                            _,
                            CarriedEntry::Write {
                                repository_bytes, ..
                            },
                        )) = plan.entries.iter().find(|(candidate, entry)| {
                            candidate == path && matches!(entry, CarriedEntry::Write { .. })
                        })
                        else {
                            return Err(RepositoryError::InvalidOperation {
                                message: format!(
                                    "adoption effect '{path}' has no planned carried content; \
                                     refusing to substitute empty bytes"
                                ),
                            });
                        };
                        let smudged = crate::content_filter::ContentFilter::smudge(
                            &filter,
                            std::path::Path::new(path),
                            repository_bytes,
                        )
                        .map_err(|error| {
                            RepositoryError::InvalidOperation {
                                message: format!("cannot smudge reassembled '{path}': {error}"),
                            }
                        })?;
                        Some(smudged.bytes)
                    }
                    _ => None,
                };
                self.execute_and_record_effect(
                    operation_lock,
                    operation_id,
                    effect.ordinal,
                    content.as_deref(),
                )?;
            }
            // ac-4 failpoint: crash after the filesystem effects, before the
            // record advance.
            adoption_failpoint("ATOMIC_FAIL_ADOPTION_AFTER_FS")?;
            // 3. Leased record advance + view re-parent: observe before,
            //    apply the metadata leases and the record transition, then
            //    append the advance's receipt.
            let advance = prepared
                .operation()
                .payload()
                .delta
                .effects
                .iter()
                .find(|effect| effect.target == working_copy_target)
                .cloned()
                .ok_or_else(|| RepositoryError::InvalidOperation {
                    message: "adoption operation lost its record-advance effect".to_string(),
                })?;
            let observed_before =
                self.observe_operation_effect(working_copy, &working_copy_target)?;
            self.apply_operation_metadata_locked(operation_lock, operation_id)?;
            let observed_after =
                self.observe_operation_effect(working_copy, &working_copy_target)?;
            self.record_effect_outcome(
                operation_lock,
                operation_id,
                advance.ordinal,
                observed_before,
                observed_after,
            )?;
            Ok(())
        })();
        if let Err(error) = execute_result {
            // Recovery is left to the immutable journal; typed error out.
            let _ = self.recover_incomplete_operation(operation_lock);
            return Err(AdoptKnownEditError::Repository(error));
        }
        self.finalize_operation_verified(operation_lock, operation_id)?;

        // ac-4 failpoint: crash after the journaled reassembly, before the
        // replacement snapshot. Recovery on reopen must leave every version
        // in place (old snapshot, reassembled bytes, re-parented view).
        adoption_failpoint("ATOMIC_FAIL_ADOPTION_BEFORE_SNAPSHOT_REPLACE")?;

        // 4. Replacement snapshot against the new baseline (supersedes the
        //    old snapshot; never depends on it — §12.3).
        let origin = AdoptionOrigin::KnownCarriedEdit;
        let snapshot_record =
            self.record_adoption_snapshot(working_copy, origin, evidence, new_head)?;

        // 5. Verify the complete result: carried content matches the plan and
        //    the snapshot is re-readable (ac-1).
        self.verify_reassembly(working_copy, &snapshot_view, &plan)?;

        // ac-4 failpoint: crash after the replacement snapshot is durable but
        // before the WIP ref is dropped. Reopen must be idempotent and the
        // WIP ref stays a retained recovery root.
        adoption_failpoint("ATOMIC_FAIL_ADOPTION_AFTER_SNAPSHOT_REPLACE")?;

        // 6. R3: the §6.4 WIP-ref drop and the §7.1 step 8 verified
        //    checkpoint publish run as leased, journaled effects of a linked
        //    completion operation — a crash here leaves an incomplete head
        //    that gates ordinary entry until the completion verifies or is
        //    rolled back. The evidence hash ties the completion to the same
        //    authenticated capture the reassembly used.
        self.run_adoption_completion(
            operation_lock,
            working_copy,
            checkpoint,
            new_view,
            new_state,
            &wip,
            vec![evidence_hash],
        )?;

        Ok(AdoptedEditOutcome {
            origin,
            snapshot: snapshot_record,
            reassembly: operation_id,
            ignored: shelf.ignored,
        })
    }

    /// Record the adoption snapshot against the new baseline view with hashed
    /// attribution metadata. Returns the new snapshot hash; `None` when the
    /// reassembled content equals the baseline (nothing to snapshot).
    pub(super) fn record_adoption_snapshot(
        &mut self,
        working_copy: atomic_core::WorkingCopyId,
        origin: AdoptionOrigin,
        evidence: &PreTransitionEvidence,
        new_head: &str,
    ) -> Result<Option<atomic_core::Hash>, RepositoryError> {
        let mut fresh = evidence.clone();
        fresh.git_head = new_head.to_ascii_lowercase();
        let options = super::RecordOptions::new()
            .with_all(true)
            .include_untracked(true)
            .save_to_store(true)
            .apply_after_record(true)
            .sync_vault(false)
            .enrich_kg(false)
            .metadata_bytes(adoption_snapshot_metadata(origin, &fresh));
        match self.snapshot(
            working_copy,
            atomic_core::change::ChangeHeader::new(match origin {
                AdoptionOrigin::KnownCarriedEdit => {
                    "Re-assembled carried edit after bound HEAD adoption"
                }
                AdoptionOrigin::UnknownPreCheckout => {
                    "Unknown post-checkout working-copy differences"
                }
            }),
            options,
        ) {
            Ok(outcome) => {
                let hash = *outcome.hash();
                // ac-1: no new snapshot may depend on a superseded snapshot.
                // The check runs after the record applied the change (R3
                // notes the window), so refusal also restores the superseded
                // snapshot's view membership through a journaled metadata
                // operation: the evidence stays provable and the refused
                // candidate object stays retained in the store — never
                // published, never deleted.
                let change = self.load_change(&hash)?;
                let refused = change.dependencies().iter().copied().find(|dependency| {
                    self.load_change(dependency)
                        .map(|change| change.kind().is_snapshot())
                        .unwrap_or(false)
                });
                if let Some(dependency) = refused {
                    let restore = self.restore_superseded_snapshot_membership(working_copy, hash);
                    if let Err(error) = restore {
                        log::warn!(
                            "adoption snapshot {} depends on snapshot {} and the superseded \
                             snapshot membership could not be restored: {error}",
                            hash.to_base32(),
                            dependency.to_base32()
                        );
                    }
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "adoption snapshot {} depends on snapshot {}; refusing to publish \
                             and restoring the superseded snapshot's membership",
                            hash.to_base32(),
                            dependency.to_base32()
                        ),
                    });
                }
                Ok(Some(hash))
            }
            Err(super::RecordError::NothingToRecord) => Ok(None),
            Err(error) => Err(record_map(error)),
        }
    }

    /// Journaled restoration of the superseded snapshot as the private view's
    /// direct member after a refused publication (R3): the record operation
    /// leased the old membership away when the candidate was applied, so the
    /// refusal must move it back under its own leases before failing.
    fn restore_superseded_snapshot_membership(
        &self,
        working_copy: atomic_core::WorkingCopyId,
        refused: atomic_core::Hash,
    ) -> Result<(), RepositoryError> {
        let snapshot_view = Self::snapshot_view_name(working_copy);
        let operation_lock = self.try_lock_operation(working_copy)?;
        if let super::OperationHeadState::Diverged(heads) =
            self.consolidate_operation_heads_locked(&operation_lock)?
        {
            return Err(RepositoryError::OperationHeadsDiverged {
                scope: atomic_core::operation::OperationScope::WorkingCopy(working_copy)
                    .to_string(),
                heads: heads.iter().map(ToString::to_string).collect(),
            });
        }
        let (_refused_id, already_member, change_count, view_state) = {
            let txn = self
                .pristine
                .read_txn()
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
            let refused_id = txn
                .get_internal(&refused)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or(RepositoryError::ChangeNotFound {
                    hash: refused.to_base32(),
                })?;
            let view = txn
                .get_view(&snapshot_view)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or(RepositoryError::ViewNotFound {
                    name: snapshot_view.clone(),
                })?;
            let member = txn
                .get_change_seq(&view, refused_id)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .is_some();
            (refused_id, member, view.change_count, view.clone())
        };
        if already_member {
            return Ok(());
        }
        let record = self.working_copy_record(working_copy)?;
        let state_ref = atomic_core::operation::RepoStateRef {
            view: Some(atomic_core::operation::ViewStateRef {
                name: snapshot_view.clone(),
                state: view_state.state,
                set_id: None,
            }),
            working_copy: Some(super::operation::working_copy_state_ref(record)),
            git: None,
        };
        let operation = self.prepare_metadata_operation(
            &operation_lock,
            OperationKind::Record,
            None,
            state_ref.clone(),
            state_ref,
            vec![atomic_core::operation::MetadataTransition {
                target: atomic_core::operation::MetadataTarget::ViewChange {
                    view: snapshot_view,
                    change: refused,
                },
                expected_old: atomic_core::operation::MetadataValue::Absent,
                expected_new: atomic_core::operation::MetadataValue::Sequence(change_count),
            }],
            Vec::new(),
            ActorRef::System {
                name: "bridge-snapshot-refusal-restore".to_string(),
            },
            super::operation::current_operation_timestamp_ms(),
        )?;
        self.apply_operation_metadata_locked(&operation_lock, operation.id())?;
        self.finalize_operation_verified(&operation_lock, operation.id())?;
        Ok(())
    }

    /// Verify the reassembled worktree against the plan (§7.3 "adopt only
    /// after verification"): every planned write's repository bytes must be
    /// exactly what the snapshot view projects, and the snapshot must be
    /// re-readable after a fresh repository handle.
    pub(super) fn verify_reassembly(
        &self,
        _working_copy: atomic_core::WorkingCopyId,
        snapshot_view: &str,
        plan: &CarriedReassemblyPlan,
    ) -> Result<(), RepositoryError> {
        let policy =
            super::conversion_policy_for_git(&git2::Repository::discover(&self.root).map_err(
                |error| RepositoryError::InvalidRepository {
                    reason: format!("cannot open the Git repository for verification: {error}"),
                },
            )?)
            .map_err(|error| RepositoryError::InvalidOperation {
                message: format!("cannot compute conversion policy for verification: {error}"),
            })?;
        let snapshot_manifest = self.project_tree(snapshot_view, &policy).map_err(|error| {
            RepositoryError::InvalidOperation {
                message: format!("cannot verify the reassembled snapshot: {error}"),
            }
        })?;
        for (path, entry) in &plan.entries {
            let CarriedEntry::Write {
                repository_bytes, ..
            } = entry
            else {
                continue;
            };
            let projected = snapshot_manifest
                .manifest
                .entries
                .iter()
                .find(|candidate| {
                    candidate.disposition == super::ManifestDisposition::Included
                        && String::from_utf8_lossy(candidate.path.as_bytes()) == path.as_str()
                })
                .map(|candidate| &candidate.repository_bytes);
            match projected {
                Some(projected) if projected == repository_bytes => {}
                other => {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "reassembly verification failed for '{path}': snapshot projects {:?}",
                            other.map(|bytes| bytes.len()),
                        ),
                    });
                }
            }
        }
        Ok(())
    }

    /// The unknown-origin branch (§7.3 branch 3): after the Git-owned
    /// checkout, protect tracked bytes with a WIP ref, advance the record to
    /// the new baseline, and capture the unexplained differences as a fresh
    /// opaque snapshot marked unknown pre-checkout attribution. The prior
    /// snapshot object (if any) stays durable; the new snapshot supersedes it
    /// in the private view.
    pub(super) fn adopt_unknown_origin_locked(
        &mut self,
        operation_lock: &WorkingCopyOperationLockGuard,
        working_copy: atomic_core::WorkingCopyId,
        checkpoint: &super::workspace_txn::WorkspaceCheckpoint,
        new_view: &str,
        new_state: atomic_core::types::Merkle,
        new_head: &str,
    ) -> Result<AdoptedEditOutcome, RepositoryError> {
        let wip = crate::wip::capture_or_reuse_tracked_wip(crate::wip::WipCaptureRequest::new(
            &self.root,
            &working_copy.to_string(),
            &Self::adoption_wip_operation(&checkpoint.git_head, new_head),
        ))
        .map_err(|error| RepositoryError::InvalidOperation {
            message: format!("cannot protect pending work with a WIP ref: {error}"),
        })?;

        let before_record = self.working_copy_record(working_copy)?;
        let (new_view_id, _) = {
            let txn = self
                .pristine
                .read_txn()
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
            let view = txn
                .get_view(new_view)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| RepositoryError::ViewNotFound {
                    name: new_view.to_string(),
                })?;
            (view.id, view.state)
        };
        let mut after_record = before_record.clone();
        after_record.desired_view = new_view_id;
        after_record.desired_state = new_state;
        after_record.materialized_state = Some(new_state);

        // R4: the unknown-origin branch swaps shelves too — the adoption is a
        // view transition, so ignored artifacts move into `old_view`'s shelf
        // and `new_view`'s shelved artifacts are restored, through the same
        // collision-safe planner and executor as the known branch.
        let mut effects = Vec::new();
        let shelf = self.plan_adoption_shelf_effects(
            operation_lock,
            working_copy,
            &checkpoint.view,
            new_view,
            &mut effects,
        )?;
        effects.push(EffectPlan {
            ordinal: effects.len() as u32,
            target: EffectTarget::WorkingCopy { working_copy },
            expected_old: EffectValue::WorkingCopy(super::operation::working_copy_state_ref(
                before_record.clone(),
            )),
            expected_new: EffectValue::WorkingCopy(super::operation::working_copy_state_ref(
                after_record.clone(),
            )),
        });

        let mut metadata = Vec::new();
        let snapshot_view = Self::snapshot_view_name(working_copy);
        {
            let txn = self
                .pristine
                .read_txn()
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
            if let Some(snapshot_state) = txn
                .get_view(&snapshot_view)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
            {
                if snapshot_state.parent != Some(new_view_id) {
                    let mut expected_new = snapshot_state.clone();
                    expected_new.parent = Some(new_view_id);
                    let encode = |view: &atomic_core::pristine::ViewState| {
                        postcard::to_allocvec(&super::operation::ViewLeaseValue {
                            parent: view.parent,
                        })
                        .map_err(|error| RepositoryError::Serialization(error.to_string()))
                        .map(atomic_core::operation::MetadataValue::Bytes)
                    };
                    metadata.push(atomic_core::operation::MetadataTransition {
                        target: atomic_core::operation::MetadataTarget::View {
                            name: snapshot_view.clone(),
                        },
                        expected_old: encode(&snapshot_state)?,
                        expected_new: encode(&expected_new)?,
                    });
                }
            }
        }

        let before_state = atomic_core::operation::RepoStateRef {
            view: Some(atomic_core::operation::ViewStateRef {
                name: checkpoint.view.clone(),
                state: new_state_for_view(self, &checkpoint.view)?,
                set_id: None,
            }),
            working_copy: Some(super::operation::working_copy_state_ref(
                before_record.clone(),
            )),
            git: None,
        };
        let after_state = atomic_core::operation::RepoStateRef {
            view: Some(atomic_core::operation::ViewStateRef {
                name: new_view.to_string(),
                state: new_state,
                set_id: None,
            }),
            working_copy: Some(super::operation::working_copy_state_ref(
                after_record.clone(),
            )),
            git: None,
        };

        let evidence = PreTransitionEvidence {
            version: PRE_TRANSITION_EVIDENCE_VERSION,
            working_copy: working_copy.to_string(),
            baseline_view: checkpoint.view.clone(),
            baseline_state: checkpoint.atomic_state.clone(),
            snapshot: None,
            remainder: None,
            git_head: checkpoint.git_head.clone(),
            git_tree: checkpoint.git_tree.clone(),
            git_index_tree: checkpoint.git_index_tree.clone(),
            git_index_digest: checkpoint.git_index_digest.clone(),
            git_index_path: None,
            policy_root: String::new(),
            created_at_ms: super::operation::current_operation_timestamp_ms(),
            journal_operation: None,
        };
        let prepared = self.prepare_working_copy_transition_with_metadata(
            operation_lock,
            OperationKind::ImportGitHead,
            None,
            before_state,
            after_state,
            effects,
            metadata,
            vec![atomic_core::Hash::of(b"atomic:unknown-origin-adoption")],
            ActorRef::System {
                name: "git-head-adoption-unknown-origin".to_string(),
            },
            super::operation::current_operation_timestamp_ms(),
        )?;
        let operation_id = prepared.operation().id();
        // R4: execute the planned shelf transitions before the record advance,
        // each under its own lease (third values fail closed).
        let execute_result = (|| -> Result<(), RepositoryError> {
            for (index, path) in shelf.ignored.iter().enumerate() {
                self.execute_shelve_path(operation_lock, &prepared, &checkpoint.view, path)?;
                adoption_shelf_ordinal_failpoint("shelve", index)?;
            }
            for (index, path) in shelf.restored.iter().enumerate() {
                self.execute_restore_path(operation_lock, &prepared, new_view, path)?;
                adoption_shelf_ordinal_failpoint("restore", index)?;
            }
            Ok(())
        })();
        if let Err(error) = execute_result {
            let _ = self.recover_incomplete_operation(operation_lock);
            return Err(error);
        }
        self.apply_operation_metadata_locked(operation_lock, operation_id)?;
        let target = EffectTarget::WorkingCopy { working_copy };
        let observed_before = self.observe_operation_effect(working_copy, &target)?;
        let observed_after = self.observe_operation_effect(working_copy, &target)?;
        if observed_before != observed_after {
            return Err(RepositoryError::InvalidOperation {
                message: "unknown-origin record advance diverged".to_string(),
            });
        }
        let advance_ordinal = prepared
            .operation()
            .payload()
            .delta
            .effects
            .iter()
            .find(|effect| effect.target == target)
            .map(|effect| effect.ordinal)
            .ok_or_else(|| RepositoryError::InvalidOperation {
                message: "unknown-origin operation lost its record-advance effect".to_string(),
            })?;
        self.record_effect_outcome(
            operation_lock,
            operation_id,
            advance_ordinal,
            observed_before,
            observed_after,
        )?;
        self.finalize_operation_verified(operation_lock, operation_id)?;

        let origin = AdoptionOrigin::UnknownPreCheckout;
        let snapshot_record =
            self.record_adoption_snapshot(working_copy, origin, &evidence, new_head)?;
        // R3: the same journaled completion — the WIP drop and the verified
        // checkpoint publish are leased effects of a linked operation, not
        // unjournaled cleanup.
        self.run_adoption_completion(
            operation_lock,
            working_copy,
            checkpoint,
            new_view,
            new_state,
            &wip,
            vec![atomic_core::Hash::of(b"atomic:unknown-origin-adoption")],
        )?;
        Ok(AdoptedEditOutcome {
            origin,
            snapshot: snapshot_record,
            reassembly: operation_id,
            ignored: shelf.ignored,
        })
    }

    // ── R3: journaled adoption completion ──────────────────────────────────

    /// The journaled adoption completion (RFC §6.4 + §7.1 step 8).
    ///
    /// The reassembly operation and the replacement-snapshot record
    /// operation are journaled; the §6.4 WIP-ref drop and the verified
    /// checkpoint publish previously ran outside the journal, so a crash in
    /// that window left a verified head with an un-checkpointed adoption
    /// (a false completed adoption). This completion operation closes the
    /// window: ordinary entry stays gated until its leased effects verify or
    /// roll back.
    ///
    /// Effects, in order:
    /// 0. the WIP-ref drop, leased by the captured commit OID (a third value
    ///    — another writer's ref — appends a durable rejection receipt and
    ///    fails closed);
    /// 1. the verified checkpoint publish, leased by the checkpoint content
    ///    digest derived at prepare time and re-derived at execute time (a
    ///    moved HEAD or index fails the lease instead of publishing moved
    ///    facts).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn run_adoption_completion(
        &mut self,
        operation_lock: &WorkingCopyOperationLockGuard,
        working_copy: atomic_core::WorkingCopyId,
        old_checkpoint: &super::workspace_txn::WorkspaceCheckpoint,
        new_view: &str,
        new_state: atomic_core::types::Merkle,
        wip: &crate::wip::WipCapture,
        extra_evidence: Vec<atomic_core::Hash>,
    ) -> Result<(), RepositoryError> {
        let git = git2::Repository::open(&self.root).map_err(|error| {
            RepositoryError::InvalidRepository {
                reason: format!("cannot open the Git repository for adoption completion: {error}"),
            }
        })?;

        // before-state: the OLD checkpoint's view + Git facts, so an
        // interrupted completion can be rolled back to the exact
        // pre-completion checkpoint without retaining file bytes.
        let before_git = checkpoint_git_state(old_checkpoint)?;
        let before_state = RepoStateRef {
            view: Some(atomic_core::operation::ViewStateRef {
                name: old_checkpoint.view.clone(),
                state: atomic_core::types::Merkle::from_base32(
                    old_checkpoint.atomic_state.as_bytes(),
                )
                .ok_or_else(|| RepositoryError::InvalidRepository {
                    reason: "bridge checkpoint carries an invalid atomic state".to_string(),
                })?,
                set_id: None,
            }),
            working_copy: Some(super::operation::working_copy_state_ref(
                self.working_copy_record(working_copy)?,
            )),
            git: Some(before_git),
        };
        let derived = self.derive_workspace_checkpoint(&git, new_view, &new_state)?;
        let _derived_bytes = super::workspace_txn::workspace_checkpoint_bytes(&derived)?;
        let after_state = RepoStateRef {
            view: Some(atomic_core::operation::ViewStateRef {
                name: new_view.to_string(),
                state: new_state,
                set_id: None,
            }),
            working_copy: Some(super::operation::working_copy_state_ref(
                self.working_copy_record(working_copy)?,
            )),
            git: Some(checkpoint_git_state(&derived)?),
        };

        let checkpoint_target = EffectTarget::Checkpoint {
            working_copy,
            kind: CheckpointKind::Bridge,
        };
        let wip_target = EffectTarget::GitRef {
            name: wip.ref_name.clone(),
        };
        // The Checkpoint lease digests FACTS (the parsed checkpoint's
        // canonical serialization), not raw file bytes: the CLI's richer v2
        // writer and the simple writer carry the same facts, and a rollback
        // re-derives the pre-completion checkpoint from the recorded
        // before-state.
        let effects = vec![
            EffectPlan {
                ordinal: 0,
                target: wip_target.clone(),
                expected_old: EffectValue::GitRef(GitRefTarget::Direct(git_object_id_from_hex(
                    &wip.commit_oid,
                )?)),
                expected_new: EffectValue::Absent,
            },
            EffectPlan {
                ordinal: 1,
                target: checkpoint_target.clone(),
                expected_old: EffectValue::Digest {
                    kind: DigestKind::Checkpoint,
                    hash: self.read_workspace_checkpoint_facts_digest()?,
                },
                expected_new: EffectValue::Digest {
                    kind: DigestKind::Checkpoint,
                    hash: atomic_core::Hash::of(checkpoint_facts_bytes(&derived)?.as_slice()),
                },
            },
        ];

        let mut evidence = vec![
            atomic_core::Hash::of(b"atomic:adoption-completion"),
            atomic_core::Hash::of(wip.tree_oid.as_bytes()),
        ];
        evidence.extend(extra_evidence);
        let prepared = self.prepare_working_copy_transition_with_metadata(
            operation_lock,
            OperationKind::ImportGitHead,
            None,
            before_state,
            after_state,
            effects,
            Vec::new(),
            evidence,
            ActorRef::System {
                name: "git-head-adoption-completion".to_string(),
            },
            super::operation::current_operation_timestamp_ms(),
        )?;
        let operation_id = prepared.operation().id();
        let payload_effects = prepared.operation().payload().delta.effects.clone();
        eprintln!("[adoption-completion] prepared {operation_id}");

        let execute_result = (|| -> Result<(), RepositoryError> {
            eprintln!("[adoption-completion] executing");
            // 0. WIP-ref drop under its OID lease (§6.4: the replacement
            //    snapshot is already durable — this operation runs after the
            //    record operation verified).
            {
                let observed = self.observe_operation_effect(working_copy, &wip_target)?;
                let after = if observed == EffectValue::Absent {
                    EffectValue::Absent
                } else if observed == payload_effects[0].expected_old {
                    let repo = git2::Repository::open(&self.root).map_err(|error| {
                        RepositoryError::InvalidRepository {
                            reason: format!(
                                "cannot open the Git repository to drop the WIP ref: {error}"
                            ),
                        }
                    })?;
                    let mut reference = repo.find_reference(&wip.ref_name).map_err(|error| {
                        RepositoryError::InvalidRepository {
                            reason: format!(
                                "WIP ref '{}' diverged before the drop: {error}",
                                wip.ref_name
                            ),
                        }
                    })?;
                    reference
                        .delete()
                        .map_err(|error| RepositoryError::InvalidRepository {
                            reason: format!("cannot drop the WIP ref '{}': {error}", wip.ref_name),
                        })?;
                    EffectValue::Absent
                } else {
                    observed.clone()
                };
                self.record_effect_outcome(operation_lock, operation_id, 0, observed, after)?;
            }
            adoption_failpoint("ATOMIC_FAIL_ADOPTION_AFTER_WIP_DROP")?;
            // 1. Verified checkpoint publish under its digest lease.
            {
                let observed_before =
                    self.observe_operation_effect(working_copy, &checkpoint_target)?;
                self.write_verified_checkpoint(&git, new_view, &new_state)?;
                let observed_after =
                    self.observe_operation_effect(working_copy, &checkpoint_target)?;
                self.record_effect_outcome(
                    operation_lock,
                    operation_id,
                    1,
                    observed_before,
                    observed_after,
                )?;
            }
            adoption_failpoint("ATOMIC_FAIL_ADOPTION_AFTER_CHECKPOINT")?;
            Ok(())
        })();
        if let Err(error) = execute_result {
            if let Err(recovery) = self.recover_incomplete_operation(operation_lock) {
                log::warn!("adoption completion recovery failed: {recovery}");
            }
            return Err(error);
        }
        self.finalize_operation_verified(operation_lock, operation_id)?;
        Ok(())
    }
}

/// Why the known-carried-edit branch did not adopt (R2).
#[derive(Debug)]
pub(super) enum AdoptKnownEditError {
    /// A typed preserving repository failure.
    Repository(RepositoryError),
    /// The pre-captured materialization no longer describes the worktree
    /// (newer post-capture edits or unexplained extra files). The caller
    /// must capture the differences as unknown pre-checkout origin; the
    /// known attribution is unsupported and nothing was mutated.
    CarriedStateUnproven(String),
}

impl From<RepositoryError> for AdoptKnownEditError {
    fn from(error: RepositoryError) -> Self {
        Self::Repository(error)
    }
}

/// The outcome of a dirty bound adoption (§7.3 branches 2–3).
#[derive(Debug)]
#[allow(dead_code)] // diagnostic payload; consumers read Debug output today
pub struct AdoptedEditOutcome {
    pub origin: AdoptionOrigin,
    /// The replacement snapshot, when the carried/unexplained content differs
    /// from the new baseline.
    pub snapshot: Option<atomic_core::Hash>,
    /// The journaled adoption operation.
    pub reassembly: atomic_core::OperationId,
    /// Ignored paths the shelf plan moved.
    pub ignored: Vec<String>,
}

fn new_state_for_view(
    repo: &Repository,
    view_name: &str,
) -> Result<atomic_core::types::Merkle, RepositoryError> {
    let txn = repo
        .pristine
        .read_txn()
        .map_err(|error| RepositoryError::Database(error.to_string()))?;
    let view = txn
        .get_view(view_name)
        .map_err(|error| RepositoryError::Database(error.to_string()))?
        .ok_or_else(|| RepositoryError::ViewNotFound {
            name: view_name.to_string(),
        })?;
    Ok(view.state)
}

/// §6.4: drop the local WIP ref once the replacement snapshot is durable.
///
/// The drop is leased by the captured commit OID (R3): the ref is deleted
/// only when it still resolves to exactly the OID this adoption captured.
/// A ref that moved externally (or disappeared) is another writer's recovery
/// root and is retained — the safe failure direction never deletes a
/// recovery ref it does not own. The reflog retains the drop evidence.
/// Returns `Ok(true)` when the leased drop executed, `Ok(false)` when the
/// ref was retained (absent or externally moved).
#[allow(dead_code)] // kept for upcoming cleanup CLI
pub(crate) fn drop_wip_ref(
    root: &std::path::Path,
    ref_name: &str,
    expected_commit_oid: &str,
) -> Result<bool, RepositoryError> {
    let resolve = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--verify", "--quiet", ref_name])
        .output()
        .map_err(RepositoryError::Io)?;
    if !resolve.status.success() {
        // Already gone (or unreadable): nothing this adoption owns to drop.
        return Ok(false);
    }
    let current = String::from_utf8_lossy(&resolve.stdout).trim().to_string();
    if !current.eq_ignore_ascii_case(expected_commit_oid) {
        // R3: the ref moved externally after the capture. Deleting it would
        // destroy another writer's recovery root — retain it and surface the
        // lease mismatch.
        log::warn!(
            "WIP recovery ref '{ref_name}' moved externally ({current}); retaining it instead \
             of the leased drop of {expected_commit_oid}"
        );
        return Ok(false);
    }
    let delete = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["update-ref", "-d", ref_name, expected_commit_oid])
        .output()
        .map_err(RepositoryError::Io)?;
    if !delete.status.success() {
        // Fail safe: a failed drop retains the recovery ref.
        log::warn!(
            "WIP recovery ref '{ref_name}' leased drop failed: {}; the ref is retained",
            String::from_utf8_lossy(&delete.stderr).trim()
        );
        return Ok(false);
    }
    Ok(true)
}

/// Crash-injection failpoints for the adoption phases (ac-4) — opt-in test
/// instrumentation, compiled ONLY with the `adoption-test-injection` feature
/// (R8). Each named environment variable turns the phase boundary into a
/// typed IO failure so recovery can be exercised before and after every
/// external effect. Shipping builds compile the no-op stub below: no
/// environment value can alter adoption behavior.
#[cfg(feature = "adoption-test-injection")]
fn adoption_failpoint(name: &str) -> Result<(), RepositoryError> {
    if std::env::var_os(name).is_some() {
        return Err(RepositoryError::Io(std::io::Error::other(format!(
            "debug failpoint: {name}"
        ))));
    }
    Ok(())
}

/// Shipping build (`adoption-test-injection` disabled, R8): the failpoint
/// seam does not exist — no environment variable is read and nothing can
/// fail here.
#[cfg(not(feature = "adoption-test-injection"))]
fn adoption_failpoint(name: &str) -> Result<(), RepositoryError> {
    let _ = name;
    Ok(())
}

/// Per-shelf-ordinal crash failpoint (ac-4): crashes after the `index`-th
/// shelve or restore path completed, so every shelf transition's interruption
/// window is testable. Value format: `<kind>:<index>` (e.g. `shelve:0`).
#[cfg(feature = "adoption-test-injection")]
fn adoption_shelf_ordinal_failpoint(kind: &str, index: usize) -> Result<(), RepositoryError> {
    if let Some(value) = std::env::var_os("ATOMIC_FAIL_ADOPTION_AFTER_SHELF_ORDINAL") {
        if value.to_string_lossy() == format!("{kind}:{index}") {
            return Err(RepositoryError::Io(std::io::Error::other(format!(
                "debug failpoint: ATOMIC_FAIL_ADOPTION_AFTER_SHELF_ORDINAL {kind}:{index}"
            ))));
        }
    }
    Ok(())
}

/// Shipping build (`adoption-test-injection` disabled, R8): the seam does not
/// exist — no environment variable is read and nothing can fail here.
#[cfg(not(feature = "adoption-test-injection"))]
fn adoption_shelf_ordinal_failpoint(_kind: &str, _index: usize) -> Result<(), RepositoryError> {
    Ok(())
}

/// Deterministic proof-to-mutation race injection for the R2 regressions —
/// opt-in test instrumentation, compiled ONLY with the
/// `adoption-test-injection` feature (R8). When `name` is set, its value
/// must name an ENUMERATED deterministic action, never a shell command:
///
/// - `write-file:<relative-path>:<literal bytes>` overwrites the path under
///   the repository root with the literal bytes, then the adoption CONTINUES
///   (unlike [`adoption_failpoint`], which aborts).
/// - `git-add:<relative-path>` stages the path through a fixed `git add`
///   invocation (no shell, no option parsing — `--` precedes the path).
///
/// Any other value is a typed error. Tests use this to land an external
/// editor/Git write inside the window between the pre-adoption proof and the
/// mutation plan deterministically, without racing a scheduler.
///
/// The shipping build compiles the no-op stub below: the variable is never
/// read, so the ordinary configuration cannot execute any injection.
#[cfg(feature = "adoption-test-injection")]
fn adoption_injection(root: &std::path::Path, name: &str) -> Result<(), RepositoryError> {
    let Some(spec) = std::env::var_os(name) else {
        return Ok(());
    };
    let spec = spec.to_string_lossy().into_owned();
    let Some((action, argument)) = spec.split_once(':') else {
        return Err(RepositoryError::InvalidOperation {
            message: format!(
                "test injection {name} must name an enumerated action \
                 ('write-file:<path>:<content>' or 'git-add:<path>'), got {spec:?}"
            ),
        });
    };
    match action {
        "write-file" => {
            let Some((relative, content)) = argument.split_once(':') else {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "test injection write-file requires '<path>:<content>', got {argument:?}"
                    ),
                });
            };
            let path = injection_relative_path(root, relative)?;
            std::fs::write(path, content).map_err(RepositoryError::Io)?;
        }
        "git-add" => {
            let path = injection_relative_path(root, argument)?;
            let status = std::process::Command::new("git")
                .current_dir(root)
                .args(["add", "--"])
                .arg(&path)
                .status()
                .map_err(RepositoryError::Io)?;
            if !status.success() {
                return Err(RepositoryError::Io(std::io::Error::other(format!(
                    "test injection git-add failed: {status}"
                ))));
            }
        }
        other => {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "test injection {name}: unknown action {other:?}; the seam executes \
                     only enumerated deterministic operations, never a shell"
                ),
            });
        }
    }
    Ok(())
}

/// Shipping build (`adoption-test-injection` disabled, R8): the injection
/// seam does not exist — no environment variable is read, no process is
/// started, nothing can execute.
#[cfg(not(feature = "adoption-test-injection"))]
fn adoption_injection(root: &std::path::Path, name: &str) -> Result<(), RepositoryError> {
    let _ = (root, name);
    Ok(())
}

/// Test-seam path validation: the argument must name a path inside the
/// repository root (relative, no parent traversal), so even the opt-in
/// feature build cannot reach outside the worktree or smuggle options.
#[cfg(feature = "adoption-test-injection")]
fn injection_relative_path(
    root: &std::path::Path,
    relative: &str,
) -> Result<std::path::PathBuf, RepositoryError> {
    let path = std::path::Path::new(relative);
    if relative.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| component == std::path::Component::ParentDir)
    {
        return Err(RepositoryError::InvalidOperation {
            message: format!("test injection path must stay inside the worktree: {relative:?}"),
        });
    }
    Ok(root.join(path))
}

/// R7: one level of the unexplained-worktree scan. Every observation error
/// is propagated (`Err`), never swallowed: a partial scan cannot establish
/// the known-carried attribution. Directory classification uses the
/// directory-entry's own (no-follow) file type, so symbolic links are leaf
/// entries and are never recursed — a symlink cycle cannot loop and the
/// walk cannot leave `root`.
///
/// R8: gitlink identity is manifest-authoritative, never a disk marker.
/// `known_gitlinks` holds every path whose manifest entry kind is
/// `InodeKind::Gitlink` (in any compared manifest); those paths are leaf
/// entries whether they own a `.git` gitfile, an embedded `.git` DIRECTORY,
/// or no marker at all (an uninitialized submodule). A directory that does
/// own a `.git` entry of any kind is still conservatively treated as an
/// embedded repository leaf (its contents belong to that repository, never
/// to this scan); the marker probe uses no-follow `symlink_metadata`, so a
/// `.git` symlink counts as a marker without the walk leaving the root.
#[allow(clippy::too_many_arguments)]
/// The Git facts of a checkpoint, as recorded in a completion operation's
/// `before`/`after` state (R3: the rollback re-derives the exact
/// pre-completion checkpoint from these facts).
pub(super) fn checkpoint_git_state(
    checkpoint: &super::workspace_txn::WorkspaceCheckpoint,
) -> Result<GitStateRef, RepositoryError> {
    let oid = git_object_id_from_hex(&checkpoint.git_head)?;
    let head = match &checkpoint.git_head_symref {
        Some(symref) => GitHeadState::Attached {
            symref: symref.clone(),
            oid,
        },
        None => GitHeadState::Detached { oid },
    };
    let index = match (&checkpoint.git_index_digest, &checkpoint.git_index_tree) {
        (Some(digest), tree) => {
            let digest = digest_from_evidence_string(digest)?;
            let tree = tree
                .as_ref()
                .map(|hex| git_object_id_from_hex(hex))
                .transpose()?;
            Some(GitIndexState { digest, tree })
        }
        (None, _) => None,
    };
    Ok(GitStateRef {
        head,
        index,
        refs_digest: atomic_core::Hash::of(b"atomic:bridge-checkpoint-state"),
    })
}

/// Parse a digest evidence string written by either checkpoint writer: the
/// repository writer records base32 (`Hash::to_base32`), the CLI checkpoint
/// writer records the canonical index digest as hex. Both decode to the same
/// 32-byte fact.
fn digest_from_evidence_string(value: &str) -> Result<atomic_core::types::Merkle, RepositoryError> {
    if let Some(hash) = atomic_core::types::Merkle::from_base32(value.as_bytes()) {
        return Ok(hash);
    }
    let bytes = hex_decode_bytes(value)?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| RepositoryError::InvalidRepository {
            reason: format!("'{value}' is neither base32 nor a 32-byte hex digest"),
        })?;
    Ok(atomic_core::types::Merkle(bytes))
}

/// A Git object id parsed from its hex form, algorithm-tagged by width.
fn git_object_id_from_hex(hex: &str) -> Result<GitObjectId, RepositoryError> {
    let bytes = hex_decode_bytes(hex)?;
    let algorithm = match bytes.len() {
        20 => atomic_core::operation::GitHashAlgorithm::Sha1,
        32 => atomic_core::operation::GitHashAlgorithm::Sha256,
        other => {
            return Err(RepositoryError::InvalidRepository {
                reason: format!("unexpected Git OID width {other} in '{hex}'"),
            })
        }
    };
    GitObjectId::new(algorithm, bytes).map_err(|error| RepositoryError::InvalidRepository {
        reason: error.to_string(),
    })
}

fn hex_decode_bytes(hex: &str) -> Result<Vec<u8>, RepositoryError> {
    if !hex.len().is_multiple_of(2) || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(RepositoryError::InvalidRepository {
            reason: format!("'{hex}' is not a hex Git object id"),
        });
    }
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    let byte_values = hex.as_bytes();
    for pair in byte_values.chunks(2) {
        let high =
            (pair[0] as char)
                .to_digit(16)
                .ok_or_else(|| RepositoryError::InvalidRepository {
                    reason: format!("'{hex}' is not a hex Git object id"),
                })?;
        let low =
            (pair[1] as char)
                .to_digit(16)
                .ok_or_else(|| RepositoryError::InvalidRepository {
                    reason: format!("'{hex}' is not a hex Git object id"),
                })?;
        bytes.push(((high << 4) | low) as u8);
    }
    Ok(bytes)
}

fn walk_unexplained_worktree(
    root: &std::path::Path,
    dir: &std::path::Path,
    rules: &crate::ignore::IgnoreRules,
    known: &std::collections::BTreeSet<String>,
    known_gitlinks: &std::collections::BTreeSet<String>,
    inject_read_dir_error: Option<&std::ffi::OsStr>,
    out: &mut Vec<String>,
) -> Result<(), RepositoryError> {
    if let Some(injected) = inject_read_dir_error {
        if let Ok(relative) = dir.strip_prefix(root) {
            if relative.as_os_str() == injected {
                return Err(RepositoryError::Io(std::io::Error::from_raw_os_error(
                    13, // EACCES: deterministic stand-in for an unreadable directory
                )));
            }
        }
    }
    let entries = std::fs::read_dir(dir).map_err(RepositoryError::Io)?;
    for entry in entries {
        // R7: an entry that cannot be observed is an error, not a skip.
        let entry = entry.map_err(RepositoryError::Io)?;
        let path = entry.path();
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        // R7: no-follow classification — `entry.file_type` reports the link
        // itself for symlinks (never the target's type), so a symlinked
        // directory is a leaf entry: it is never recursed, the walk never
        // leaves `root`, and a symlink cycle cannot loop.
        let file_type = entry.file_type().map_err(|error| {
            RepositoryError::Io(std::io::Error::new(
                error.kind(),
                format!("cannot classify '{}': {error}", path.display()),
            ))
        })?;
        let is_directory = file_type.is_dir();
        if rules.is_ignored(relative, is_directory) {
            continue;
        }
        #[cfg(unix)]
        let relative_bytes: Vec<u8> = {
            use std::os::unix::ffi::OsStrExt;
            relative.as_os_str().as_bytes().to_vec()
        };
        #[cfg(not(unix))]
        let relative_bytes: Vec<u8> = relative
            .as_os_str()
            .to_string_lossy()
            .into_owned()
            .into_bytes();
        if relative_bytes == b".atomic"
            || relative_bytes.starts_with(b".atomic/")
            || relative_bytes == b".git"
            || relative_bytes.starts_with(b".git/")
        {
            continue;
        }
        let Some(relative_str) = relative.to_str() else {
            // R5: a raw non-UTF-8 disk path cannot be attributed to
            // any captured state — it is unexplained by definition.
            out.push(format!(
                "{} (non-UTF-8 raw path)",
                String::from_utf8_lossy(&relative_bytes)
            ));
            continue;
        };
        // R8: a manifest-declared gitlink is a leaf regardless of any on-disk
        // marker; a directory owning a `.git` entry of any kind (probed with
        // no-follow metadata — file, directory, or symlink) is conservatively
        // an embedded repository leaf too.
        let is_gitlink = is_directory
            && (known_gitlinks.contains(relative_str)
                || std::fs::symlink_metadata(path.join(".git")).is_ok());
        let is_leaf = !is_directory || is_gitlink;
        if !known.contains(relative_str)
            && !known
                .iter()
                .any(|entry| entry.starts_with(&format!("{relative_str}/")))
            && is_leaf
        {
            out.push(relative_str.to_string());
        }
        if is_directory && !is_gitlink {
            walk_unexplained_worktree(
                root,
                &path,
                rules,
                known,
                known_gitlinks,
                inject_read_dir_error,
                out,
            )?;
        }
    }
    Ok(())
}

/// R8: the manifest-authoritative gitlink set — every included entry whose
/// kind is `InodeKind::Gitlink`, keyed by the same lossy path form the
/// walker uses. The scanner never descends into these paths, whether the
/// submodule owns a `.git` gitfile, an embedded `.git` directory, or no
/// on-disk marker at all (uninitialized submodule).
fn manifest_gitlink_paths(trees: &[&super::ProjectTree]) -> std::collections::BTreeSet<String> {
    trees
        .iter()
        .flat_map(|tree| tree.manifest.entries.iter())
        .filter(|entry| entry.disposition == super::ManifestDisposition::Included)
        .filter(|entry| entry.kind == InodeKind::Gitlink)
        .map(|entry| String::from_utf8_lossy(entry.path.as_bytes()).into_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_bound_capture_authenticates_and_tampered_facts_do_not() {
        let directory = tempfile::TempDir::new().unwrap();
        let repo = Repository::init(directory.path()).unwrap();
        let working_copy = repo.require_working_copy_id().unwrap();
        let evidence = PreTransitionEvidence {
            version: PRE_TRANSITION_EVIDENCE_VERSION,
            working_copy: working_copy.to_string(),
            baseline_view: "dev".to_string(),
            baseline_state: "AAA".to_string(),
            snapshot: None,
            remainder: None,
            git_head: format!("{:040}", 0),
            git_tree: format!("{:040}", 1),
            git_index_tree: None,
            git_index_digest: Some(
                "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB".to_string(),
            ),
            git_index_path: Some(".git/index".to_string()),
            policy_root: "p".to_string(),
            created_at_ms: 1,
            journal_operation: None,
        };
        // R2: the capture is bound to an immutable, verified journal
        // operation whose payload carries the facts hash.
        let operation_id = repo
            .journal_pre_transition_capture(working_copy, &evidence)
            .expect("journal the capture");
        let mut bound = evidence.clone();
        bound.journal_operation = Some(operation_id.to_base32());
        assert!(
            repo.journal_binding_proves(&bound, working_copy),
            "the stored journal operation authenticates the capture"
        );
        // The stored payload is authoritative: every tampered fact fails.
        let mut tampered = bound.clone();
        tampered.git_head = format!("{:040}", 2);
        assert!(
            !repo.journal_binding_proves(&tampered, working_copy),
            "a tampered fact cannot re-derive the stored facts hash"
        );
        // Pre-journal evidence (no binding) is never proof.
        let mut unbound = bound.clone();
        unbound.journal_operation = None;
        assert!(!repo.journal_binding_proves(&unbound, working_copy));
        // A fabricated journal pointer names no stored operation.
        let mut fabricated = bound.clone();
        fabricated.journal_operation = Some("A".repeat(52));
        assert!(!repo.journal_binding_proves(&fabricated, working_copy));
        // The journal operation itself has a verified receipt (the
        // append-only authenticated record).
        let receipts = repo
            .pristine
            .read_txn()
            .unwrap()
            .get_effect_receipts(operation_id)
            .unwrap();
        assert!(super::super::operation::has_operation_verified_receipt(
            &receipts
        ));
    }

    #[test]
    fn adoption_metadata_markers_distinguish_attribution() {
        let evidence = PreTransitionEvidence {
            version: PRE_TRANSITION_EVIDENCE_VERSION,
            working_copy: "01M23PY0DQGFTE2DZWYNF7VP74".to_string(),
            baseline_view: "main".to_string(),
            baseline_state: "AAA".to_string(),
            snapshot: Some("BBB".to_string()),
            remainder: None,
            git_head: "c0".to_string(),
            git_tree: "t0".to_string(),
            git_index_tree: None,
            git_index_digest: None,
            git_index_path: Some(".git/index".to_string()),
            policy_root: "p".to_string(),
            created_at_ms: 1,
            journal_operation: None,
        };
        let known = adoption_snapshot_metadata(AdoptionOrigin::KnownCarriedEdit, &evidence);
        let unknown = adoption_snapshot_metadata(AdoptionOrigin::UnknownPreCheckout, &evidence);
        let known = String::from_utf8(known).unwrap();
        let unknown = String::from_utf8(unknown).unwrap();
        assert!(known.contains("known-carried-edit"), "{known}");
        assert!(unknown.contains("unknown-pre-checkout"), "{unknown}");
        assert!(!unknown.contains("known-carried-edit"));
        assert!(!known.contains("unknown-pre-checkout"));
        // The evidence facts are hashed into the marker (content-addressed
        // attribution, RFC §12.3/§12.8).
        assert!(known.contains("c0") && known.contains("main"));
    }

    // ── R1: carried-deletion reassembly semantics ─────────────────────────

    use crate::repository::{
        ConversionPolicy, GitTree, ManifestDisposition, ProjectTree, RepoPath, RepositoryEntry,
        RepositoryManifest,
    };
    use atomic_core::operation::GitHashAlgorithm;
    use atomic_core::types::SetId;

    fn plan_entry(path: &[u8], bytes: &[u8]) -> RepositoryEntry {
        RepositoryEntry::new(
            RepoPath::from_bytes(path).unwrap(),
            bytes.to_vec(),
            0o644,
            InodeKind::Regular,
            None,
            ManifestDisposition::Included,
        )
        .unwrap()
    }

    fn plan_tree(entries: Vec<RepositoryEntry>) -> ProjectTree {
        let policy = ConversionPolicy::new(GitHashAlgorithm::Sha1);
        let manifest =
            RepositoryManifest::new(SetId::ZERO, policy.root().content_key.clone(), entries)
                .unwrap();
        ProjectTree {
            manifest,
            git: GitTree {
                algorithm: GitHashAlgorithm::Sha1,
                root: atomic_core::operation::GitObjectId::new(GitHashAlgorithm::Sha1, vec![0; 20])
                    .unwrap(),
                objects: Default::default(),
            },
        }
    }

    fn planned<'a>(plan: &'a CarriedReassemblyPlan, path: &str) -> Option<&'a CarriedEntry> {
        plan.entries
            .iter()
            .find(|(candidate, _)| candidate == path)
            .map(|(_, entry)| entry)
    }

    #[test]
    fn carried_deletion_wins_on_an_unchanged_baseline() {
        let old = plan_tree(vec![plan_entry(b"gone.txt", b"baseline\n")]);
        let carried = plan_tree(vec![]);
        let new = plan_tree(vec![plan_entry(b"gone.txt", b"baseline\n")]);
        let plan = plan_carried_reassembly_trees(&old, &carried, &new);
        assert!(plan.is_separable(), "{:?}", plan.conflicts);
        assert_eq!(planned(&plan, "gone.txt"), Some(&CarriedEntry::Remove));
    }

    #[test]
    fn carried_deletion_plus_baseline_modification_refuses_without_losing_versions() {
        let old = plan_tree(vec![plan_entry(b"f.txt", b"v1\n")]);
        let carried = plan_tree(vec![]);
        let new = plan_tree(vec![plan_entry(b"f.txt", b"v2\n")]);
        let plan = plan_carried_reassembly_trees(&old, &carried, &new);
        assert!(!plan.is_separable(), "delete/modify must refuse");
        assert!(plan.conflicts[0]
            .reason
            .contains("without losing either version"));
    }

    #[test]
    fn delete_delete_converges_instead_of_conflicting() {
        let old = plan_tree(vec![plan_entry(b"f.txt", b"v1\n")]);
        let carried = plan_tree(vec![]);
        let new = plan_tree(vec![]);
        let plan = plan_carried_reassembly_trees(&old, &carried, &new);
        assert!(plan.is_separable(), "{:?}", plan.conflicts);
        assert_eq!(planned(&plan, "f.txt"), Some(&CarriedEntry::AlreadyCurrent));
    }

    #[test]
    fn identical_add_add_converges() {
        let old = plan_tree(vec![]);
        let carried = plan_tree(vec![plan_entry(b"new.txt", b"same\n")]);
        let new = plan_tree(vec![plan_entry(b"new.txt", b"same\n")]);
        let plan = plan_carried_reassembly_trees(&old, &carried, &new);
        assert!(plan.is_separable(), "{:?}", plan.conflicts);
        assert_eq!(
            planned(&plan, "new.txt"),
            Some(&CarriedEntry::AlreadyCurrent)
        );
    }

    #[test]
    fn carried_deletions_and_edits_coexist_in_one_plan() {
        // old: gone.txt + kept.txt; carried: kept.txt edited, gone.txt deleted;
        // new: gone.txt deleted by the baseline too, kept.txt unchanged.
        let old = plan_tree(vec![
            plan_entry(b"gone.txt", b"v1\n"),
            plan_entry(b"kept.txt", b"kept\n"),
        ]);
        let carried = plan_tree(vec![plan_entry(b"kept.txt", b"kept\ncarried\n")]);
        let new = plan_tree(vec![plan_entry(b"kept.txt", b"kept\n")]);
        let plan = plan_carried_reassembly_trees(&old, &carried, &new);
        assert!(plan.is_separable(), "{:?}", plan.conflicts);
        assert_eq!(
            planned(&plan, "gone.txt"),
            Some(&CarriedEntry::AlreadyCurrent)
        );
        match planned(&plan, "kept.txt") {
            Some(CarriedEntry::Write {
                repository_bytes, ..
            }) => {
                assert_eq!(repository_bytes, b"kept\ncarried\n");
            }
            other => panic!("expected carried write for kept.txt, got {other:?}"),
        }
    }

    // ── R5: raw path identity ─────────────────────────────────────────────

    #[test]
    fn distinct_invalid_utf8_paths_are_refused_before_any_mutation() {
        // Two distinct raw names that collapse to the SAME lossy UTF-8
        // string: lossy decoding must never alias them.
        let first = b"dup-\xff.txt".to_vec();
        let second = b"dup-\xfd.txt".to_vec();
        assert_eq!(
            String::from_utf8_lossy(&first),
            String::from_utf8_lossy(&second),
            "the fixture names must collide under lossy decoding"
        );
        let old = plan_tree(vec![
            plan_entry(&first, b"first\n"),
            plan_entry(&second, b"second\n"),
        ]);
        let carried = plan_tree(vec![
            plan_entry(&first, b"first\nedited\n"),
            plan_entry(&second, b"second\n"),
        ]);
        let new = plan_tree(vec![]);
        let plan = plan_carried_reassembly_trees(&old, &carried, &new);
        assert!(!plan.is_separable(), "non-UTF-8 paths must refuse");
        assert!(
            plan.entries.is_empty(),
            "a refusal must exist before any planned effect: {:?}",
            plan.entries
        );
        assert_eq!(plan.conflicts.len(), 2, "{:?}", plan.conflicts);
        assert!(plan
            .conflicts
            .iter()
            .all(|conflict| { conflict.reason.contains("not valid UTF-8") }));
    }

    // ── R3: the WIP drop lease ────────────────────────────────────────────

    fn wip_lease_repo() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(root.path())
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.name", "Lease Test"]);
        run(&["config", "user.email", "lease@example.com"]);
        std::fs::write(root.path().join("f.txt"), b"one\n").unwrap();
        run(&["add", "f.txt"]);
        run(&["commit", "-qm", "one"]);
        root
    }

    #[test]
    fn wip_ref_drop_is_leased_by_the_captured_oid() {
        let root = wip_lease_repo();
        let ref_name = "refs/atomic/wip/lease-test";
        let run = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(root.path())
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_string()
        };
        let original = run(&["rev-parse", "HEAD"]);
        run(&["update-ref", ref_name, &original]);

        // An externally moved ref (another writer's recovery root) is
        // retained, never deleted by our lease.
        std::fs::write(root.path().join("f.txt"), b"two\n").unwrap();
        run(&["commit", "-qam", "two"]);
        let foreign = run(&["rev-parse", "HEAD"]);
        assert_ne!(foreign, original);
        run(&["update-ref", ref_name, &foreign]);
        let dropped = drop_wip_ref(root.path(), ref_name, &original).unwrap();
        assert!(
            !dropped,
            "a moved ref must not be deleted under a stale lease"
        );
        assert_eq!(run(&["rev-parse", ref_name]), foreign);

        // The leased drop deletes only the exact captured OID.
        let dropped = drop_wip_ref(root.path(), ref_name, &foreign).unwrap();
        assert!(dropped);
        let missing = std::process::Command::new("git")
            .arg("-C")
            .arg(root.path())
            .args(["rev-parse", "--verify", "--quiet", ref_name])
            .output()
            .unwrap();
        assert!(!missing.status.success(), "the ref must be gone");

        // Dropping an already-absent ref is a no-op success.
        let dropped = drop_wip_ref(root.path(), ref_name, &foreign).unwrap();
        assert!(!dropped);
    }

    // ── R7: the unexplained-worktree scan is a total, no-follow observation ─

    use std::collections::BTreeSet;
    use std::ffi::OsStr;
    use std::path::Path;

    fn walk_scan(
        root: &Path,
        dir: &Path,
        known: &BTreeSet<String>,
        known_gitlinks: &BTreeSet<String>,
        inject: Option<&OsStr>,
    ) -> Result<Vec<String>, RepositoryError> {
        let rules = crate::ignore::IgnoreRules::load(root);
        let mut out = Vec::new();
        walk_unexplained_worktree(root, dir, &rules, known, known_gitlinks, inject, &mut out)?;
        out.sort();
        out.dedup();
        Ok(out)
    }

    #[test]
    fn unexplained_walk_propagates_enumeration_failure_instead_of_partial_scan() {
        let directory = tempfile::TempDir::new().unwrap();
        // A regular file, not a directory: read_dir must fail with ENOTDIR.
        std::fs::write(directory.path().join("plain-file"), b"not a dir\n").unwrap();
        let result = walk_scan(
            directory.path(),
            &directory.path().join("plain-file"),
            &BTreeSet::new(),
            &BTreeSet::new(),
            None,
        );
        assert!(
            result.is_err(),
            "R7: an enumeration failure must propagate as a typed refusal, \
             never a silent partial scan: {:?}",
            result
        );
    }

    #[test]
    fn unexplained_walk_injected_directory_error_is_typed() {
        let directory = tempfile::TempDir::new().unwrap();
        let root = directory.path();
        std::fs::create_dir_all(root.join("probe")).unwrap();
        std::fs::write(root.join("probe/hidden.txt"), b"inside\n").unwrap();
        let injected = walk_scan(
            root,
            root,
            &BTreeSet::new(),
            &BTreeSet::new(),
            Some(OsStr::new("probe")),
        );
        assert!(
            injected.is_err(),
            "R7: an unreadable directory must be a typed error, never a silently \
             skipped subtree"
        );
    }

    #[test]
    fn unexplained_walk_never_follows_symlinks_or_leaves_the_root() {
        let directory = tempfile::TempDir::new().unwrap();
        let root = directory.path();
        let outside = tempfile::TempDir::new().unwrap();
        std::fs::write(outside.path().join("secret.txt"), b"outside bytes\n").unwrap();

        // A real directory containing (a) a symlink loop — the link resolves
        // to an ancestor of itself — and (b) a link pointing OUTSIDE the
        // worktree. Both are leaf entries: never recursed.
        std::fs::create_dir_all(root.join("loop-dir")).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("..", root.join("loop-dir/up")).unwrap();
            std::os::unix::fs::symlink(outside.path(), root.join("ext-link")).unwrap();
        }
        // A real nested directory is still traversed.
        std::fs::create_dir_all(root.join("real/deeper")).unwrap();
        std::fs::write(root.join("real/deeper.txt"), b"shown\n").unwrap();
        std::fs::write(root.join("real/deeper/inner.txt"), b"shown too\n").unwrap();

        let known: BTreeSet<String> = ["real/deeper/inner.txt".to_string()].into();
        let unexplained = walk_scan(root, root, &known, &BTreeSet::new(), None)
            .expect("walk must not hang or fail");

        // The links themselves are reported (they are unexplained leaves),
        // but their TARGETS are never read: the outside file never appears
        // and the loop never produced entries above the link.
        assert!(
            unexplained.iter().any(|path| path == "loop-dir/up"),
            "the in-root symlink is a leaf entry: {unexplained:?}"
        );
        assert!(
            unexplained.iter().any(|path| path == "ext-link"),
            "the external symlink is a leaf entry: {unexplained:?}"
        );
        assert!(
            !unexplained.iter().any(|path| path.contains("secret")),
            "R7: the scan must never follow a symlink outside the root: {unexplained:?}"
        );
        assert!(
            !unexplained.iter().any(|path| path.starts_with("ext-link/")),
            "R7: symlinked directories must not be recursed: {unexplained:?}"
        );
        assert!(
            !unexplained
                .iter()
                .any(|path| path.starts_with("loop-dir/up/")),
            "R7: a symlink must never be recursed (loop or not): {unexplained:?}"
        );
        assert_eq!(
            std::fs::read(outside.path().join("secret.txt")).unwrap(),
            b"outside bytes\n",
            "the scan must be read-only for everything outside the worktree"
        );
        // A known path is not reported even though the walk passed it.
        assert!(!unexplained.iter().any(|path| path.contains("inner.txt")));
        assert!(
            unexplained.iter().any(|path| path == "real/deeper.txt"),
            "sanity: the walker reaches real directories and reports unexplained \
             files inside them: {unexplained:?}"
        );
    }

    #[test]
    fn unexplained_walk_excludes_git_atomic_paths_and_never_descends_gitlinks() {
        let directory = tempfile::TempDir::new().unwrap();
        let root = directory.path();
        // Atomic/Git administrative paths stay excluded even when a stray
        // file appears inside them.
        std::fs::create_dir_all(root.join(".atomic")).unwrap();
        std::fs::write(root.join(".atomic/stray.txt"), b"x\n").unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/stray.txt"), b"x\n").unwrap();
        // A gitlink (embedded repository with a `.git` FILE) is a leaf: its
        // internal files are that repository's business, never scanned.
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/.git"), b"gitdir: ../.git/modules/sub\n").unwrap();
        std::fs::write(root.join("sub/inner.txt"), b"submodule bytes\n").unwrap();

        let known: BTreeSet<String> = ["sub".to_string()].into();
        let gitlinks: BTreeSet<String> = ["sub".to_string()].into();
        let unexplained = walk_scan(root, root, &known, &gitlinks, None).expect("walk");
        assert!(
            unexplained.is_empty(),
            "R7: administrative paths are excluded and a known gitlink is never \
             descended into: {unexplained:?}"
        );

        // Without the known entry the gitlink itself is unexplained — but its
        // INTERNALS still are not reported.
        let unexplained =
            walk_scan(root, root, &BTreeSet::new(), &BTreeSet::new(), None).expect("walk");
        assert!(
            unexplained.iter().any(|path| path == "sub"),
            "an unknown gitlink is itself unexplained: {unexplained:?}"
        );
        assert!(
            !unexplained.iter().any(|path| path.contains("inner.txt")),
            "R7: a gitlink's contents must never be scanned: {unexplained:?}"
        );
    }

    // ── R8: the gitlink boundary is manifest-authoritative, not marker-based ─

    #[test]
    fn known_gitlink_with_embedded_git_directory_is_never_scanned() {
        let directory = tempfile::TempDir::new().unwrap();
        let root = directory.path();
        // A manifest-declared gitlink whose on-disk marker is a `.git`
        // DIRECTORY (an embedded clone) instead of the gitfile. The `.git`
        // FILE probe alone would misclassify this as an ordinary directory
        // and scan inside it.
        std::fs::create_dir_all(root.join("sub/.git")).unwrap();
        std::fs::write(root.join("sub/.git/HEAD"), b"ref: refs/heads/main\n").unwrap();
        std::fs::write(root.join("sub/inner.txt"), b"submodule bytes\n").unwrap();

        // Manifest-declared gitlink: a leaf, contents never scanned.
        let known: BTreeSet<String> = ["sub".to_string()].into();
        let gitlinks: BTreeSet<String> = ["sub".to_string()].into();
        let unexplained = walk_scan(root, root, &known, &gitlinks, None).expect("walk");
        assert!(
            unexplained.is_empty(),
            "R8: a known gitlink owning a .git DIRECTORY is never descended \
             into or misattributed: {unexplained:?}"
        );

        // Unknown variant: the directory itself is unexplained, its
        // internals still are not.
        let unexplained =
            walk_scan(root, root, &BTreeSet::new(), &BTreeSet::new(), None).expect("walk");
        assert!(
            unexplained.iter().any(|path| path == "sub"),
            "an unknown embedded repository is itself unexplained: {unexplained:?}"
        );
        assert!(
            !unexplained.iter().any(|path| path.contains("inner.txt")),
            "R8: the embedded repository's contents must never be scanned: {unexplained:?}"
        );
    }

    #[test]
    fn known_gitlink_without_on_disk_marker_is_never_scanned() {
        let directory = tempfile::TempDir::new().unwrap();
        let root = directory.path();
        // An uninitialized submodule: the manifest declares a gitlink but
        // the directory carries NO `.git` marker at all. The old
        // `.git`-marker probe would treat it as an ordinary directory and
        // scan (and misattribute) its contents.
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(
            root.join("sub/inner.txt"),
            b"uninitialized submodule bytes\n",
        )
        .unwrap();

        let known: BTreeSet<String> = ["sub".to_string()].into();
        let gitlinks: BTreeSet<String> = ["sub".to_string()].into();
        let unexplained = walk_scan(root, root, &known, &gitlinks, None).expect("walk");
        assert!(
            unexplained.is_empty(),
            "R8: a manifest-known gitlink without any on-disk marker is a leaf — \
             its contents are never read or misattributed: {unexplained:?}"
        );

        // Sanity: an unknown ordinary directory (no marker, not a declared
        // gitlink) is still scanned — the R7 unknown-directory coverage is
        // preserved.
        let unexplained =
            walk_scan(root, root, &BTreeSet::new(), &BTreeSet::new(), None).expect("walk");
        assert!(
            unexplained.iter().any(|path| path == "sub/inner.txt"),
            "an unknown ordinary directory is still scanned: {unexplained:?}"
        );
        // A known NON-gitlink directory is also still scanned: only its own
        // path is known, its unknown contents remain unexplained.
        let unexplained = walk_scan(root, root, &known, &BTreeSet::new(), None).expect("walk");
        assert!(
            unexplained.iter().any(|path| path == "sub/inner.txt"),
            "a known ordinary directory does not hide unknown contents: {unexplained:?}"
        );
    }

    /// R8: in the shipping configuration (feature
    /// `adoption-test-injection` disabled) the injection/failpoint seams are
    /// no-op stubs — setting the variables has no effect at all.
    #[cfg(not(feature = "adoption-test-injection"))]
    #[test]
    fn adoption_injection_and_failpoint_variables_are_inert_in_shipping_builds() {
        let directory = tempfile::TempDir::new().unwrap();
        let root = directory.path();
        std::env::set_var(
            "ATOMIC_INJECT_ADOPTION_AFTER_PROOF",
            "write-file:pwned-by-env.txt:must never be written\n",
        );
        std::env::set_var("ATOMIC_FAIL_ADOPTION_BEFORE_SNAPSHOT_REPLACE", "1");
        let injected = adoption_injection(root, "ATOMIC_INJECT_ADOPTION_AFTER_PROOF");
        let failed = adoption_failpoint("ATOMIC_FAIL_ADOPTION_BEFORE_SNAPSHOT_REPLACE");
        std::env::remove_var("ATOMIC_INJECT_ADOPTION_AFTER_PROOF");
        std::env::remove_var("ATOMIC_FAIL_ADOPTION_BEFORE_SNAPSHOT_REPLACE");
        assert!(
            injected.is_ok(),
            "R8: the shipping build must not execute the injection variable: {injected:?}"
        );
        assert!(
            failed.is_ok(),
            "R8: the shipping build must not fail on the failpoint variable: {failed:?}"
        );
        assert!(
            !root.join("pwned-by-env.txt").exists(),
            "R8: no enumerated action may run in the shipping build"
        );
    }
}
