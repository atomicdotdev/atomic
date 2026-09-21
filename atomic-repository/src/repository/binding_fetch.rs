//! Closure acquisition for a verified binding (RFC §5.2 closure-fetch step,
//! §8.6 transport, CB-6B).
//!
//! A binding names its exact ordered change closure. This module recovers
//! that closure **remote-first**: the configured Atomic remote is the
//! preferred source of change objects (it is the only channel allowed to
//! carry private sidecars), and the binding commit's optional bounded
//! `changes.pack` blob is the verified *fallback* for closures not reachable
//! from a remote.
//!
//! # Guarantees
//!
//! - **Explicit incompleteness**: missing objects, shallow/promisor
//!   boundaries (a `LossNote::TruncatedHistory` loss note), or an unavailable
//!   fallback produce a typed `Incomplete` verdict — never silent synthesis,
//!   a truncated closure, or successful adoption.
//! - **Bounded verification**: every object passes the
//!   [`crate::git_binding::pack`] quarantine (limits, content addressing,
//!   format negotiation, privacy) before it is cached.
//! - **Fetch advances nothing**: only content-addressed change files are
//!   cached for retry. No view membership, no refs, no operations, no
//!   working-copy state. A verified cache entry never implies a committed
//!   operation.

use atomic_core::change::Change;
use atomic_core::types::Base32;
use atomic_core::Hash;

use atomic_objects::{ObjectFamily, ObjectRecord};

use crate::git_binding::{
    decode_changes_pack, private_material_in_change, validate_binding_closure,
    BindingPackLimits, ClosureChangeSource, ClosureValidation, GitStateBinding, LossNote,
    QuarantinedPack,
};

use super::Repository;
use super::RepositoryError;

/// A source of change objects on the configured Atomic remote.
///
/// Implementors fetch the named change objects and return records keyed by
/// the change **identity** hash (base32, the sync-plane convention) with
/// family [`ObjectFamily::Change`]. Changes the remote cannot serve are
/// simply absent from the result; a transport-level failure (remote
/// unreachable, protocol error) is an `Err` and degrades to the pack
/// fallback with the detail surfaced in the outcome.
pub trait BindingChangeSource {
    fn fetch_changes(&mut self, wanted: &[Hash]) -> Result<Vec<ObjectRecord>, String>;
}

/// Why a fetched closure is not complete. Every variant is an explicit,
/// reportable refusal — none may be silently adopted (RFC §5.2, §8.6, §12).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IncompletenessReason {
    /// Objects of the ordered closure (or its dependencies) are unavailable
    /// from every configured source.
    MissingObjects,
    /// The Atomic remote was preferred but unreachable; the fallback also
    /// did not close the gap (or was absent).
    RemoteUnavailable { detail: String },
    /// The binding carries no `changes.pack` fallback and the remote could
    /// not complete the closure.
    NoFallbackAvailable,
    /// The binding declares a shallow/promisor boundary
    /// (`LossNote::TruncatedHistory`); history beyond it may be missing, so
    /// completeness can never be claimed from transport alone.
    ShallowBoundary,
    /// The fallback pack exists but failed bounded verification.
    PackRefused { detail: String },
}

/// Whether the fetched closure may be handed to adoption (CB-6C).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClosureReadiness {
    /// Every ordered change is present, hash-verified, and the closure
    /// recomputes; the binding declares no shallow boundary.
    Complete,
    /// Explicit incompleteness. `missing` names every unavailable hash;
    /// `reasons` explains which source failed and why.
    Incomplete {
        missing: Vec<Hash>,
        reasons: Vec<IncompletenessReason>,
    },
}

impl ClosureReadiness {
    /// Whether the closure may proceed to adoption (CB-6C). Fetch alone
    /// never adopts — this only reports transport readiness.
    pub fn is_complete(&self) -> bool {
        matches!(self, ClosureReadiness::Complete)
    }
}

/// The outcome of one closure-acquisition attempt.
#[derive(Debug, Clone)]
pub struct ClosureAcquisition {
    /// The binding whose closure was fetched.
    pub binding_id: crate::git_binding::BindingId,
    /// Transport readiness after every available source was tried.
    pub readiness: ClosureReadiness,
    /// Ordered changes already present (and verified) before fetching.
    pub already_local: usize,
    /// Verified changes cached from the Atomic remote.
    pub from_remote: usize,
    /// Verified changes cached from the binding's `changes.pack`.
    pub from_pack: usize,
    /// The exact ordered closure the binding names (Merkle order).
    pub ordered_hashes: Vec<Hash>,
    /// The binding's `closure_root` (verified by re-computation).
    pub closure_root: Hash,
    /// The verified raw foreign commit bytes carried by the binding, when
    /// present. Byte-identical across all acquisition modes.
    pub raw_commit_object: Option<Vec<u8>>,
    /// The final closure validation (entry budget, dependency completeness,
    /// cycles, depth) over everything available at the end of the fetch.
    pub validation: Option<ClosureValidation>,
    /// Transport failure detail from the Atomic remote, when it was tried
    /// and failed.
    pub remote_error: Option<String>,
}

impl Repository {
    /// The ordered closure entries not present in the local change store.
    ///
    /// This is the "wants" list for remote negotiation; the complement is the
    /// graph-wide "haves" a pull would advertise.
    pub fn missing_closure_objects(
        &self,
        binding: &GitStateBinding,
    ) -> Result<Vec<Hash>, RepositoryError> {
        Ok(binding
            .payload()
            .ordered_changes
            .iter()
            .filter(|hash| !self.has_change(hash))
            .copied()
            .collect())
    }

    /// Quarantine records fetched from the Atomic remote and cache the
    /// verified ones (the retry cache). Records must be identity-keyed
    /// (base32) [`ObjectFamily::Change`] objects; each must deserialize to
    /// exactly its claimed identity and pass the privacy boundary (§12.13).
    ///
    /// Returns how many wanted changes were cached. A refused record fails
    /// the whole batch closed — nothing from a violating source is cached.
    pub fn ingest_verified_changes(
        &self,
        records: &[ObjectRecord],
        wanted: &[Hash],
    ) -> Result<usize, RepositoryError> {
        let mut cached = 0usize;
        for record in records {
            if !matches!(record.family, ObjectFamily::Change) {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "remote served {:?} object {} to a closure fetch; only change objects may close a binding",
                        record.family, record.key
                    ),
                });
            }
            let expected = Hash::from_base32(record.key.as_bytes()).ok_or_else(|| {
                RepositoryError::InvalidOperation {
                    message: format!(
                        "remote change object key '{}' is not a base32 change hash",
                        record.key
                    ),
                }
            })?;
            if !wanted.contains(&expected) {
                // Not wanted: ignore (content-addressed, already verified);
                // never cache objects the closure did not ask for.
                continue;
            }
            let (change, identity) = Change::deserialize(&mut std::io::Cursor::new(&record.bytes))
                .map_err(|error| {
                    RepositoryError::InvalidOperation {
                        message: format!(
                            "remote change {} does not decode: {error}",
                            record.key
                        ),
                    }
                })?;
            if identity != expected {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "remote change object {key} carries identity {identity}",
                        key = record.key,
                        identity = identity.to_base32()
                    ),
                });
            }
            if let Some(detail) = private_material_in_change(&change) {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "remote change {key} carries private material that must travel only via \
                         an authenticated Atomic boundary ({detail})",
                        key = record.key
                    ),
                });
            }
            self.save_change(&change)?;
            cached += 1;
        }
        Ok(cached)
    }

    /// Acquire the binding's closure: Atomic remote first, the binding's
    /// bounded `changes.pack` as verified fallback (RFC §8.6).
    ///
    /// Sequence (every step fails closed before the next):
    /// 1. binding cryptography + Git structural verification (§5.2);
    /// 2. inventory: which ordered changes are already local (haves);
    /// 3. remote phase: fetch wants, quarantine, cache verified objects;
    /// 4. pack phase (fallback): read the confined tree entry, bounded
    ///    decode, quarantine, cache verified objects;
    /// 5. final closure validation (entry budget, dependency completeness,
    ///    cycles, depth) over store ∪ pack;
    /// 6. verdict: `Complete` only when nothing is missing and the binding
    ///    declares no shallow boundary; otherwise `Incomplete` with the
    ///    explicit reasons.
    ///
    /// Only content-addressed change files are written (the retry cache).
    /// Views, refs, bindings, operations, and the working copy are never
    /// touched by fetch alone.
    pub fn fetch_binding_closure(
        &self,
        git: &git2::Repository,
        binding: &crate::git_binding::GitStateBinding,
        remote: Option<&mut dyn BindingChangeSource>,
        limits: &BindingPackLimits,
    ) -> Result<ClosureAcquisition, RepositoryError> {
        use super::observability::{BindingFetchRefusalCode, BridgeEventJournal};
        // Every terminal of this fetch is observable (review R5): the four
        // refusal exits record a `binding_fetch_refused` event; the verdict
        // path records `binding_fetch`. All emissions are consent-gated
        // (`for_repository`) and lossy.
        let binding_id = |binding: &crate::git_binding::GitStateBinding| {
            super::observability::HexBindingId::new(&binding.id().to_hex())
        };
        let emit_refused =
            |binding_id: &Option<super::observability::HexBindingId>,
             reason: BindingFetchRefusalCode| {
                if let Some(binding) = binding_id.clone() {
                    BridgeEventJournal::for_repository(self).emit_lossy(
                        super::observability::BridgeEventKind::BindingFetchRefused {
                            binding,
                            reason,
                        },
                    );
                }
            };
        let binding_id = binding_id(binding);
        // 1. The binding is the source of truth; verify it before use.
        if let Err(error) = crate::git_binding::verify_binding_cryptography(binding) {
            emit_refused(&binding_id, BindingFetchRefusalCode::Cryptography);
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "binding {} fails cryptography, refusing to fetch its closure: {error}",
                    binding.id()
                ),
            });
        }
        if let Err(error) = crate::git_binding::verify_binding_content(git, binding) {
            emit_refused(&binding_id, BindingFetchRefusalCode::Content);
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "binding {} does not match Git, refusing to fetch its closure: {error}",
                    binding.id()
                ),
            });
        }
        let payload = binding.payload();
        let ordered = &payload.ordered_changes;
        if ordered.len() > limits.max_closure_entries {
            emit_refused(&binding_id, BindingFetchRefusalCode::ClosureBudget);
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "binding closure lists {} changes; the transport budget is {}",
                    ordered.len(),
                    limits.max_closure_entries
                ),
            });
        }

        let mut acquisition = ClosureAcquisition {
            binding_id: binding.id(),
            readiness: ClosureReadiness::Incomplete {
                missing: Vec::new(),
                reasons: Vec::new(),
            },
            already_local: 0,
            from_remote: 0,
            from_pack: 0,
            ordered_hashes: ordered.clone(),
            closure_root: payload.closure_root,
            raw_commit_object: payload.raw_commit_object.clone(),
            validation: None,
            remote_error: None,
        };

        // 2. Haves/wants over the local change store.
        let mut missing = self.missing_closure_objects(binding)?;
        acquisition.already_local = ordered.len() - missing.len();

        // 3. Remote first (RFC §8.6: "A configured Atomic remote is preferred
        //    for change objects; the pack is the fallback.").
        if !missing.is_empty() {
            if let Some(source) = remote {
                match source.fetch_changes(&missing) {
                    Ok(records) => match self.ingest_verified_changes(&records, &missing) {
                        Ok(cached) => acquisition.from_remote = cached,
                        // Review R5: an ingest failure is a terminal fetch
                        // failure and must be countable.
                        Err(error) => {
                            emit_refused(&binding_id, BindingFetchRefusalCode::IngestFailed);
                            return Err(error);
                        }
                    },
                    Err(error) => {
                        acquisition.remote_error = Some(error.to_string());
                    }
                }
                missing = self.missing_closure_objects(binding)?;
            }
        }

        // 4. Pack fallback: read the confined tree, bounded-decode, quarantine.
        let mut pack_error: Option<String> = None;
        let mut pack_found = false;
        if !missing.is_empty() {
            match self.load_binding_changes_pack(git, &binding.id()) {
                Ok(Some(bytes)) => {
                    pack_found = true;
                    let quarantined = decode_changes_pack(&bytes, limits)
                        .and_then(|records| QuarantinedPack::from_records(&records, limits))
                        .map_err(|error| error.to_string());
                    match quarantined {
                        Ok(pack) => {
                            let mut cached = 0;
                            for hash in &missing {
                                if let Some(change) = pack.get(hash) {
                                    // Review R5: a cache-write failure is a
                                    // terminal fetch failure and must be
                                    // countable.
                                    if let Err(error) = self.save_change(change) {
                                        emit_refused(
                                            &binding_id,
                                            BindingFetchRefusalCode::IngestFailed,
                                        );
                                        return Err(error);
                                    }
                                    cached += 1;
                                }
                            }
                            acquisition.from_pack = cached;
                        }
                        Err(detail) => pack_error = Some(detail),
                    }
                }
                Ok(None) => {}
                Err(error) => pack_error = Some(error.to_string()),
            }
            missing = self.missing_closure_objects(binding)?;
        }

        // 5. Final validation over everything now available. The pack phase
        //    cached its verified objects into the store, so the store is the
        //    single chained source. Corruption in the local store fails
        //    closed here; incompleteness is a verdict, not an error.
        let mut store = StoreClosureSource { repo: self };
        let validation = match validate_binding_closure(payload, &mut store, limits) {
            Ok(validation) => validation,
            Err(error) => {
                emit_refused(&binding_id, BindingFetchRefusalCode::ClosureValidation);
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "binding {} closure rejected: {error}",
                        binding.id()
                    ),
                });
            }
        };
        acquisition.validation = Some(validation);

        // 6. Verdict — explicit, never silent.
        let mut reasons: Vec<IncompletenessReason> = Vec::new();
        if missing.is_empty() && payload.loss.iter().any(|note| matches!(note, LossNote::TruncatedHistory { .. })) {
            reasons.push(IncompletenessReason::ShallowBoundary);
        }
        if !missing.is_empty() {
            reasons.push(IncompletenessReason::MissingObjects);
            if let Some(detail) = &acquisition.remote_error {
                reasons.push(IncompletenessReason::RemoteUnavailable {
                    detail: detail.clone(),
                });
            }
            if !pack_found {
                reasons.push(IncompletenessReason::NoFallbackAvailable);
            }
        }
        if let Some(detail) = pack_error {
            reasons.push(IncompletenessReason::PackRefused { detail });
        }
        // CB-13C observability: the loss observable (RFC §13 Phase 13
        // task 5). One event per fetch records transport readiness and
        // whether any loss note applied: truncated shallow history,
        // missing closure objects, refused packs or an unavailable source.
        let lossy = reasons.iter().any(|reason| {
            matches!(
                reason,
                IncompletenessReason::ShallowBoundary
                    | IncompletenessReason::MissingObjects
                    | IncompletenessReason::PackRefused { .. }
                    | IncompletenessReason::NoFallbackAvailable
            )
        });
        let ready = reasons.is_empty();
        acquisition.readiness = if ready {
            ClosureReadiness::Complete
        } else {
            ClosureReadiness::Incomplete {
                missing,
                reasons,
            }
        };
        if let Some(binding) = binding_id.clone() {
            super::observability::BridgeEventJournal::for_repository(self).emit_lossy(
                super::observability::BridgeEventKind::BindingFetch {
                    binding,
                    readiness: if ready {
                        super::observability::ReadinessCode::Complete
                    } else {
                        super::observability::ReadinessCode::Incomplete
                    },
                    lossy,
                },
            );
        }
        Ok(acquisition)
    }
}

/// The local change store as a closure object source.
struct StoreClosureSource<'a> {
    repo: &'a Repository,
}

impl ClosureChangeSource for StoreClosureSource<'_> {
    fn get(&mut self, hash: &Hash) -> Result<Option<Change>, crate::git_binding::BindingClosureError> {
        if !self.repo.has_change(hash) {
            return Ok(None);
        }
        self.repo.load_change(hash).map(Some).map_err(|error| {
            crate::git_binding::BindingClosureError::Quarantine {
                hash: hash.to_base32(),
                source: crate::git_binding::BindingPackError::ChangeDecode {
                    key: hash.to_base32(),
                    reason: error.to_string(),
                },
            }
        })
    }
}

#[cfg(test)]
mod tests {
    // Integration coverage for the acquisition flow lives in
    // `atomic-repository/src/repository/tests/binding_fetch_tests.rs`
    // (CB-6B fixtures: remote-only, pack-only, mixed-have, interrupted /
    // resumed, and the adversarial refusals).
}
