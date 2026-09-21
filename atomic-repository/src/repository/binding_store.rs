//! Immutable Git state binding storage and create-only ref publication.
//!
//! Storage (RFC §5.1) is insert-only: byte-identical inserts are idempotent
//! no-ops, conflicting bytes for an existing id are refused, and there is no
//! delete path — `unrecord`/`insert`/new projections create new bindings and
//! old ones remain valid for their old commits.
//!
//! Publication (RFC §7.2, §8.6, §12) goes through the journaled operation
//! machinery: the immutable operation (with its expected-old/new Git-ref
//! lease) is durable *before* the ref write, so journal or signing failure
//! can never expose a valid-looking publication. Refs
//! `refs/atomic/bindings/<shard>/<id>` are create-only: an existing ref with
//! the same target is an idempotent replay; an existing ref with any other
//! target is refused without overwriting newer work.

use atomic_core::operation::{GitHashAlgorithm, GitObjectId, GitRefTarget};
use atomic_core::pristine::{BindingMutTxnT, BindingStoreOutcome, BindingTxnT};
use atomic_core::{Hash, OperationId, WorkingCopyId};

use git2::Repository as GitRepository;

use crate::git_binding::{BindingId, GitStateBinding};
use crate::git_binding::{
    binding_tree_entries, conflicts_pack_self_validates, decode_changes_pack,
    verify_binding_cryptography, BindingPackLimits, QuarantinedPack, SignedAttestationSummary,
    ATTESTATION_SUMMARY_BLOB_NAME, BINDING_BLOB_NAME, CHANGES_PACK_BLOB_NAME,
    CONFLICTS_PACK_BLOB_NAME,
};
use crate::RepositoryError;

use super::Repository;

/// The create-only ref namespace for published bindings.
pub const BINDING_REF_PREFIX: &str = "refs/atomic/bindings/";

/// Outcome of one publication attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingPublication {
    /// The binding ref was created for the first time.
    Published {
        /// The journaled publication operation.
        operation: OperationId,
        /// The binding commit the ref now points at.
        binding_commit: GitObjectId,
    },
    /// The ref already pointed at exactly this binding's commit; the retry
    /// changed nothing and no new operation was needed (the original
    /// publication operation remains verified).
    Idempotent {
        /// The binding commit the ref already pointed at.
        binding_commit: GitObjectId,
    },
}

impl BindingPublication {
    /// The journaled publication operation, when this attempt published.
    pub fn operation(&self) -> Option<OperationId> {
        match self {
            BindingPublication::Published { operation, .. } => Some(*operation),
            BindingPublication::Idempotent { .. } => None,
        }
    }
}

/// A published binding whose carrier passed full pre-transfer validation
/// ([`Repository::verify_published_binding_carrier`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedPublishedBinding {
    /// The binding decoded from the carrier's canonical `binding.cbor`.
    pub binding: GitStateBinding,
    /// The exact verified carrier commit: the ref target, which equals the
    /// deterministic publication commit of the validated public payloads.
    pub carrier_oid: GitObjectId,
}

impl Repository {
    /// Store a binding immutably (RFC §5.1).
    ///
    /// Identical bytes are an idempotent no-op; conflicting bytes under the
    /// same id are a hash collision or corruption and are refused.
    pub fn store_binding(
        &self,
        binding: &GitStateBinding,
    ) -> Result<BindingStoreOutcome, RepositoryError> {
        let bytes = binding.encode();
        let id = *binding.id().as_bytes();
        let mut txn = self.pristine.write_txn().map_err(pristine_map)?;
        let outcome = txn
            .insert_binding_bytes(&id, &bytes)
            .map_err(pristine_map)?;
        use atomic_core::pristine::MutTxnT;
        txn.commit().map_err(pristine_map)?;
        Ok(outcome)
    }

    /// Load a stored binding, fully revalidated.
    ///
    /// Malformed stored bytes fail closed: bindings are immutable facts, and
    /// silently returning `None` for corrupt data would hide tampering.
    pub fn load_binding(&self, id: &BindingId) -> Result<Option<GitStateBinding>, RepositoryError> {
        let txn = self.pristine.read_txn().map_err(pristine_map)?;
        let bytes = txn.get_binding_bytes(id.as_bytes()).map_err(pristine_map)?;
        match bytes {
            None => Ok(None),
            Some(bytes) => GitStateBinding::decode(&bytes)
                .map(Some)
                .map_err(|error| RepositoryError::InvalidOperation {
                    message: format!(
                        "stored binding {} failed revalidation: {error}",
                        id.to_hex()
                    ),
                }),
        }
    }

    /// Every stored binding id in deterministic order.
    pub fn binding_ids(&self) -> Result<Vec<BindingId>, RepositoryError> {
        let txn = self.pristine.read_txn().map_err(pristine_map)?;
        let ids = txn.iter_binding_ids().map_err(pristine_map)?;
        Ok(ids.into_iter().map(BindingId::from_bytes).collect())
    }

    /// The create-only publication ref for a binding id (RFC §8.6):
    /// `refs/atomic/bindings/<shard>/<id>` with a two-hex-character shard.
    pub fn binding_ref_name(id: &BindingId) -> String {
        let hex = id.to_hex();
        format!("{BINDING_REF_PREFIX}{}/{hex}", &hex[..2])
    }

    /// The create-only publication ref for this binding.
    pub fn publication_ref_name(&self, binding: &GitStateBinding) -> String {
        Self::binding_ref_name(&binding.id())
    }
}

fn pristine_map(error: atomic_core::pristine::PristineError) -> RepositoryError {
    RepositoryError::Database(error.to_string())
}

fn git_map(error: impl std::fmt::Display) -> RepositoryError {
    RepositoryError::InvalidOperation {
        message: error.to_string(),
    }
}

impl Repository {
    /// Publish a signed binding (RFC §7.2 exit, §8.6, §12).
    ///
    /// Sequence:
    /// 1. validate the signed object (refuse before anything is written);
    /// 2. write the unreferenced binding commit whose tree holds only the
    ///    Git-safe public blobs ([`binding_tree_entries`]);
    /// 3. refuse if the create-only ref exists with a different target;
    /// 4. store the binding immutably (a local durable fact, not a
    ///    publication);
    /// 5. journal the ref effect with its expected-old lease, then write the
    ///    ref create-only, then record the leased receipt and verify.
    ///
    /// The optional attestation `summary` must be signed by the binding
    /// signer's key; it is the only attestation-derived data allowed into
    /// the Git tree.
    pub fn publish_binding(
        &self,
        working_copy: WorkingCopyId,
        git: &GitRepository,
        binding: &GitStateBinding,
        summary: Option<&SignedAttestationSummary>,
    ) -> Result<BindingPublication, RepositoryError> {
        self.publish_binding_inner(working_copy, git, binding, summary, None, None)
    }

    /// Publish a signed binding together with its optional complete
    /// conflict-object pack (RFC §8.3, CB-8B).
    ///
    /// The pack bytes must be canonical [`ConflictSetObject`] encoding; they
    /// are structurally revalidated here and identity-bound by the conflict
    /// snapshot commit's `atomic-conflict <hash>` header at restore time.
    /// Everything else behaves exactly like [`publish_binding`].
    pub fn publish_binding_with_conflicts_pack(
        &self,
        working_copy: WorkingCopyId,
        git: &GitRepository,
        binding: &GitStateBinding,
        summary: Option<&SignedAttestationSummary>,
        conflicts_pack: &[u8],
    ) -> Result<BindingPublication, RepositoryError> {
        if conflicts_pack.is_empty() {
            return Err(RepositoryError::InvalidOperation {
                message: "an empty conflicts.pack cannot be published; omit it instead".to_string(),
            });
        }
        self.publish_binding_inner(
            working_copy,
            git,
            binding,
            summary,
            None,
            Some(conflicts_pack),
        )
    }

    fn publish_binding_inner(
        &self,
        working_copy: WorkingCopyId,
        git: &GitRepository,
        binding: &GitStateBinding,
        summary: Option<&SignedAttestationSummary>,
        changes_pack: Option<&[u8]>,
        conflicts_pack: Option<&[u8]>,
    ) -> Result<BindingPublication, RepositoryError> {
        // 1. Validate before anything is written. A tampered or malformed
        //    binding (including a bad signature) is refused here and never
        //    becomes a valid-looking publication.
        if let Err(error) = binding.payload().validate() {
            return Err(RepositoryError::InvalidOperation {
                message: format!("refusing to publish an invalid binding: {error}"),
            });
        }
        if let Err(error) = binding.verify_signature() {
            return Err(RepositoryError::InvalidOperation {
                message: format!("refusing to publish a binding whose signature fails: {error}"),
            });
        }
        if let Some(summary) = summary {
            let key = binding_summary_key(binding)?;
            summary.verify_signature(&key).map_err(|error| {
                RepositoryError::InvalidOperation {
                    message: format!(
                        "refusing to publish a binding whose attestation summary does not verify: {error}"
                    ),
                }
            })?;
        }
        if let Some(pack) = conflicts_pack {
            // RFC §8.3: the pack must decode as a complete conflict object.
            // Its identity is bound by the projection commit's
            // `atomic-conflict <hash>` header and re-verified at every
            // restore; a structurally invalid pack is never published.
            if let Err(error) = crate::git_binding::conflicts_pack_self_validates(pack) {
                return Err(RepositoryError::InvalidOperation {
                    message: format!("refusing to publish an invalid conflicts.pack: {error}"),
                });
            }
        }

        let id = binding.id();
        let ref_name = Self::binding_ref_name(&id);
        let binding_commit =
            self.create_binding_commit(git, binding, summary, changes_pack, conflicts_pack)?;

        // Create-only: an existing ref may only already hold this target.
        let observed_old = read_ref_target(git, &ref_name)?;
        if let Some(old) = &observed_old {
            if old.as_bytes() != binding_commit.as_bytes() {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "binding ref '{ref_name}' already exists with a different target; bindings are create-only and are never overwritten"
                    ),
                });
            }
            // Already published: an identical retry is a typed no-op. The
            // original publication operation remains verified; journaling a
            // fresh effect would describe an old/new-identical no-op, which
            // the operation codec refuses.
            return Ok(BindingPublication::Idempotent { binding_commit });
        }

        // 4. Store before publish: local durable fact, not a publication.
        self.store_binding(binding)?;

        // 5. Journal before visibility (the ref is known absent here).
        let evidence = Hash::of(binding.encode().as_slice());
        let prepared = self.prepare_binding_ref_write(
            working_copy,
            &ref_name,
            None,
            binding_commit.clone(),
            evidence,
        )?;
        let operation = prepared.operation_id;

        let commit_oid = git2_oid(&binding_commit)?;
        git.reference(
            &ref_name,
            commit_oid,
            false,
            &format!("atomic binding {}", id.to_hex()),
        )
        .map_err(|error| git_map(format!("cannot create binding ref '{ref_name}': {error}")))?;

        let observed_after = read_ref_target(git, &ref_name)?;
        let observed_after_ref = observed_after.clone().map(GitRefTarget::Direct);
        self.record_bridge_git_ref_receipt(&prepared, observed_after_ref.clone())?;
        self.finalize_bridge_git_write(prepared, observed_after_ref)?;

        Ok(BindingPublication::Published {
            operation,
            binding_commit,
        })
    }

    /// Read back a published binding through its create-only ref.
    ///
    /// The ref's commit must carry the canonical binding blob, the decoded
    /// binding must be the requested id, and the object is revalidated.
    pub fn load_published_binding(
        &self,
        git: &GitRepository,
        id: &BindingId,
    ) -> Result<Option<GitStateBinding>, RepositoryError> {
        let ref_name = Self::binding_ref_name(id);
        let Some(target) = git.find_reference(&ref_name).ok().and_then(|r| r.target())
        else {
            return Ok(None);
        };
        let commit = git.find_commit(target).map_err(|error| {
            git_map(format!("binding ref '{ref_name}' points at a missing commit: {error}"))
        })?;
        let tree = commit
            .tree()
            .map_err(|error| git_map(format!("binding ref '{ref_name}' commit has no tree: {error}")))?;
        let entry = tree.get_name(BINDING_BLOB_NAME).ok_or_else(|| {
            git_map(format!(
                "binding ref '{ref_name}' tree does not contain {BINDING_BLOB_NAME}"
            ))
        })?;
        let blob = git
            .find_blob(entry.id())
            .map_err(|error| git_map(format!("binding ref '{ref_name}' blob is missing: {error}")))?;
        let binding = GitStateBinding::decode(blob.content()).map_err(|error| {
            git_map(format!(
                "binding ref '{ref_name}' carries undecodable binding bytes: {error}"
            ))
        })?;
        if binding.id() != *id {
            return Err(git_map(format!(
                "binding ref '{ref_name}' carries binding {} but {} was requested",
                binding.id(),
                id.to_hex()
            )));
        }
        Ok(Some(binding))
    }

    /// Validate the exact immutable carrier of a published binding and its
    /// full transitive publication object surface BEFORE any network write
    /// (RFC §8.6 privacy boundary; CB-10B review R1).
    ///
    /// A binding ref may only ever point at the exact parentless commit that
    /// publication deterministically builds from the binding's public
    /// payloads. Everything reachable from such a ref is bounded to the
    /// reviewed publication surface: the carrier commit, its allowlisted
    /// tree, and the public blobs inside it. Namespace allowlisting alone is
    /// not a privacy boundary for Git object traversal, so every other
    /// outcome is a typed refusal naming the defect:
    ///
    /// - unexpected parentage (a private WIP/snapshot parent would make every
    ///   ancestor object reachable through transfer),
    /// - non-allowlisted or non-regular tree entries (subtrees, symlinks,
    ///   gitlinks, extra blobs),
    /// - unregistered or mismatched binding ids, non-canonical binding bytes,
    ///   failed signature/payload verification,
    /// - invalid public payloads: an attestation summary that does not verify
    ///   against the binding signer's key, or packs that fail quarantine,
    /// - a ref target that is not the deterministic republication of the
    ///   validated payloads (the exact immutable published carrier).
    pub fn verify_published_binding_carrier(
        &self,
        git: &GitRepository,
        id: &BindingId,
    ) -> Result<Option<VerifiedPublishedBinding>, RepositoryError> {
        let ref_name = Self::binding_ref_name(id);
        let Some(target) = read_ref_target(git, &ref_name)? else {
            return Ok(None);
        };
        let commit = git.find_commit(git2_oid(&target)?).map_err(|error| {
            git_map(format!("binding ref '{ref_name}' points at a missing commit: {error}"))
        })?;
        let tree = commit.tree().map_err(|error| {
            git_map(format!("binding ref '{ref_name}' commit has no tree: {error}"))
        })?;

        // Transitive surface, part 1 — parentage: the published carrier is
        // always a parentless infrastructure commit. Any parent would place
        // its entire ancestor closure (private WIP recovery objects, working
        // snapshots, …) on the wire with the binding.
        let parent_count = commit.parent_count();
        if parent_count != 0 {
            return Err(git_map(format!(
                "binding ref '{ref_name}' carrier has {parent_count} parent(s); binding carriers \
                 are parentless commits, and unexpected parentage would transfer every reachable \
                 object — including private ones"
            )));
        }

        // Transitive surface, part 2 — tree: ONLY the allowlisted public
        // blobs, each a plain regular blob, refused before any content is
        // interpreted.
        let mut binding_bytes: Option<Vec<u8>> = None;
        let mut summary_bytes: Option<Vec<u8>> = None;
        let mut changes_pack: Option<Vec<u8>> = None;
        let mut conflicts_pack: Option<Vec<u8>> = None;
        for entry in tree.iter() {
            let name = entry.name().ok_or_else(|| {
                git_map(format!("binding ref '{ref_name}' tree holds a non-UTF-8 entry name"))
            })?;
            let allowlisted = matches!(
                name,
                BINDING_BLOB_NAME
                    | ATTESTATION_SUMMARY_BLOB_NAME
                    | CHANGES_PACK_BLOB_NAME
                    | CONFLICTS_PACK_BLOB_NAME
            );
            if !allowlisted {
                return Err(git_map(format!(
                    "binding ref '{ref_name}' tree carries non-allowlisted entry '{name}'; \
                     publication transfers every reachable object, so binding trees hold \
                     only the public blobs"
                )));
            }
            let mode = entry.filemode();
            if mode != i32::from(git2::FileMode::Blob) {
                return Err(git_map(format!(
                    "binding ref '{ref_name}' entry '{name}' is not a regular blob (mode {mode:o}); \
                     binding trees confine to plain blobs"
                )));
            }
            let blob = git.find_blob(entry.id()).map_err(|error| {
                git_map(format!("binding ref '{ref_name}' blob is missing: {error}"))
            })?;
            match name {
                BINDING_BLOB_NAME => binding_bytes = Some(blob.content().to_vec()),
                ATTESTATION_SUMMARY_BLOB_NAME => summary_bytes = Some(blob.content().to_vec()),
                CHANGES_PACK_BLOB_NAME | CONFLICTS_PACK_BLOB_NAME => {
                    // CB-10B review R12: the oversize rejection happens
                    // BEFORE the bulk copy — the blob's size is checked
                    // against the transport budget first, so an oversized
                    // pack is never copied into memory at all.
                    let limits = BindingPackLimits::default();
                    if blob.size() as usize > limits.max_pack_bytes {
                        return Err(git_map(format!(
                            "binding ref '{ref_name}' carries an oversized {name}: {} bytes \
                             exceeds the {} byte transport budget and is refused before any \
                             copy",
                            blob.size(),
                            limits.max_pack_bytes
                        )));
                    }
                    if name == CHANGES_PACK_BLOB_NAME {
                        changes_pack = Some(blob.content().to_vec());
                    } else {
                        conflicts_pack = Some(blob.content().to_vec());
                    }
                }
                _ => unreachable!("entry allowlisted above"),
            }
        }
        let Some(binding_bytes) = binding_bytes else {
            return Err(git_map(format!(
                "binding ref '{ref_name}' tree does not contain {BINDING_BLOB_NAME}"
            )));
        };

        // The binding itself must be the canonical, signed public payload.
        let binding = GitStateBinding::decode(&binding_bytes).map_err(|error| {
            git_map(format!(
                "binding ref '{ref_name}' carries undecodable binding bytes: {error}"
            ))
        })?;
        if binding.id() != *id {
            return Err(git_map(format!(
                "binding ref '{ref_name}' carries binding {} but {} was requested",
                binding.id(),
                id.to_hex()
            )));
        }
        if binding_bytes != binding.encode() {
            return Err(git_map(format!(
                "binding ref '{ref_name}' carries non-canonical binding bytes; only the \
                 canonical encoding of a stored binding may ever transfer"
            )));
        }
        if let Err(error) = verify_binding_cryptography(&binding) {
            return Err(git_map(format!(
                "binding ref '{ref_name}' fails signature/structure verification: {error}"
            )));
        }

        // Public payloads: a signed summary must verify against the binding
        // signer's key; packs must self-validate (changes.pack through the
        // full CB-6B quarantine) before their bytes may leave for a remote.
        let summary = match summary_bytes {
            None => None,
            Some(bytes) => {
                let summary = SignedAttestationSummary::decode(&bytes).map_err(|error| {
                    git_map(format!(
                        "binding ref '{ref_name}' carries an undecodable attestation \
                         summary: {error}"
                    ))
                })?;
                let key = binding_summary_key(&binding)?;
                if let Err(error) = summary.verify_signature(&key) {
                    return Err(git_map(format!(
                        "binding ref '{ref_name}' attestation summary does not verify against \
                         the binding signer's key: {error}"
                    )));
                }
                Some(summary)
            }
        };
        if let Some(pack) = &conflicts_pack {
            if let Err(error) = conflicts_pack_self_validates(pack) {
                return Err(git_map(format!(
                    "binding ref '{ref_name}' carries an invalid conflicts.pack: {error}"
                )));
            }
        }
        if let Some(pack) = &changes_pack {
            let limits = BindingPackLimits::default();
            decode_changes_pack(pack, &limits)
                .and_then(|records| QuarantinedPack::from_records(&records, &limits))
                .map_err(|error| {
                    git_map(format!(
                        "binding ref '{ref_name}' carries a changes.pack that fails quarantine: \
                         {error}"
                    ))
                })?;
        }

        // Exact immutable carrier: republishing the validated public payloads
        // must deterministically rebuild the exact commit the ref points at.
        // A different OID means unexpected parentage, extra entries, altered
        // infrastructure fields, or tampered payloads — none of which may be
        // transferred, and none of which is ever overwritten.
        let expected = self.create_binding_commit(
            git,
            &binding,
            summary.as_ref(),
            changes_pack.as_deref(),
            conflicts_pack.as_deref(),
        )?;
        if expected != target {
            let expected_hex = git2_oid(&expected)?.to_string();
            let actual_hex = git2_oid(&target)?.to_string();
            return Err(git_map(format!(
                "binding ref '{ref_name}' points at {actual_hex} but the exact deterministic \
                 publication of binding {} is {expected_hex}; the carrier is not the immutable \
                 published commit and is never transferred or overwritten",
                id.to_hex()
            )));
        }
        Ok(Some(VerifiedPublishedBinding {
            binding,
            carrier_oid: target,
        }))
    }

    /// Publish a signed binding together with its optional bounded
    /// `changes.pack` fallback (RFC §8.6, CB-6B).
    ///
    /// The pack bytes must already be the output of
    /// [`crate::git_binding::assemble_changes_pack`] — this function only
    /// places the blob into the binding tree; it performs no additional
    /// quarantine. Everything else behaves exactly like [`publish_binding`]:
    /// the create-only ref, the journal-before-visibility sequence, and
    /// idempotent retries are unchanged.
    pub fn publish_binding_with_changes_pack(
        &self,
        working_copy: WorkingCopyId,
        git: &GitRepository,
        binding: &GitStateBinding,
        summary: Option<&SignedAttestationSummary>,
        changes_pack: &[u8],
    ) -> Result<BindingPublication, RepositoryError> {
        if changes_pack.is_empty() {
            return Err(RepositoryError::InvalidOperation {
                message: "an empty changes.pack cannot be published; omit it instead".to_string(),
            });
        }
        self.publish_binding_inner(
            working_copy,
            git,
            binding,
            summary,
            Some(changes_pack),
            None,
        )
    }

    /// Publish a signed binding with BOTH the bounded `changes.pack` fallback
    /// and the complete `conflicts.pack` (RFC §8.3 + §8.6, CB-8B).
    pub fn publish_binding_with_changes_and_conflicts_pack(
        &self,
        working_copy: WorkingCopyId,
        git: &GitRepository,
        binding: &GitStateBinding,
        summary: Option<&SignedAttestationSummary>,
        changes_pack: &[u8],
        conflicts_pack: &[u8],
    ) -> Result<BindingPublication, RepositoryError> {
        if changes_pack.is_empty() {
            return Err(RepositoryError::InvalidOperation {
                message: "an empty changes.pack cannot be published; omit it instead".to_string(),
            });
        }
        if conflicts_pack.is_empty() {
            return Err(RepositoryError::InvalidOperation {
                message: "an empty conflicts.pack cannot be published; omit it instead".to_string(),
            });
        }
        self.publish_binding_inner(
            working_copy,
            git,
            binding,
            summary,
            Some(changes_pack),
            Some(conflicts_pack),
        )
    }

    /// Read the optional `changes.pack` blob of a published binding, with
    /// strict path confinement (RFC §8.6, §6.2).
    ///
    /// The binding commit's tree may contain ONLY the allowlisted public
    /// blobs (`binding.cbor`, `attestation-summary.cbor`, `changes.pack`),
    /// and each must be a regular blob: subtrees, symlinks (mode 120000),
    /// and gitlinks are refused before any bytes are read, so a malicious
    /// binding ref cannot smuggle traversal or symlink-escape payloads
    /// through the pack reader.
    pub fn load_binding_changes_pack(
        &self,
        git: &GitRepository,
        id: &BindingId,
    ) -> Result<Option<Vec<u8>>, RepositoryError> {
        let ref_name = Self::binding_ref_name(id);
        let Some(target) = git.find_reference(&ref_name).ok().and_then(|r| r.target()) else {
            return Ok(None);
        };
        let commit = git.find_commit(target).map_err(|error| {
            git_map(format!("binding ref '{ref_name}' points at a missing commit: {error}"))
        })?;
        let tree = commit
            .tree()
            .map_err(|error| git_map(format!("binding ref '{ref_name}' commit has no tree: {error}")))?;

        let mut pack: Option<Vec<u8>> = None;
        for entry in tree.iter() {
            let name = entry.name().ok_or_else(|| {
                git_map(format!(
                    "binding ref '{ref_name}' tree holds a non-UTF-8 entry name"
                ))
            })?;
            let allowlisted = matches!(
                name,
                BINDING_BLOB_NAME
                    | ATTESTATION_SUMMARY_BLOB_NAME
                    | CHANGES_PACK_BLOB_NAME
                    | CONFLICTS_PACK_BLOB_NAME
            );
            if !allowlisted {
                return Err(git_map(format!(
                    "binding ref '{ref_name}' tree carries non-allowlisted entry '{name}'; \
                     binding trees hold only the public blobs"
                )));
            }
            // Path confinement: every allowlisted entry must be a regular
            // blob. Symlinks (0o120000) and gitlinks (0o160000) are refused
            // before any content is read.
            let mode = entry.filemode();
            if mode != i32::from(git2::FileMode::Blob) {
                return Err(git_map(format!(
                    "binding ref '{ref_name}' entry '{name}' is not a regular blob (mode {mode:o}); \
                     binding trees confine to plain blobs"
                )));
            }
            if name == CHANGES_PACK_BLOB_NAME {
                let blob = git
                    .find_blob(entry.id())
                    .map_err(|error| {
                        git_map(format!(
                            "binding ref '{ref_name}' pack blob is missing: {error}"
                        ))
                    })?;
                pack = Some(blob.content().to_vec());
            }
        }
        Ok(pack)
    }

    /// Read the optional complete `conflicts.pack` blob of a published
    /// binding (RFC §8.3, CB-8B), with the same strict path confinement as
    /// [`Self::load_binding_changes_pack`]: allowlisted names only, plain
    /// blobs only.
    ///
    /// The returned pack is structurally validated and its identity hash is
    /// returned alongside so callers can compare it with the conflict
    /// snapshot commit's `atomic-conflict <hash>` header.
    pub fn load_binding_conflicts_pack(
        &self,
        git: &GitRepository,
        id: &BindingId,
    ) -> Result<Option<Vec<u8>>, RepositoryError> {
        let ref_name = Self::binding_ref_name(id);
        let Some(target) = git.find_reference(&ref_name).ok().and_then(|r| r.target()) else {
            return Ok(None);
        };
        let commit = git.find_commit(target).map_err(|error| {
            git_map(format!("binding ref '{ref_name}' points at a missing commit: {error}"))
        })?;
        let tree = commit
            .tree()
            .map_err(|error| git_map(format!("binding ref '{ref_name}' commit has no tree: {error}")))?;

        let mut pack: Option<Vec<u8>> = None;
        for entry in tree.iter() {
            let name = entry.name().ok_or_else(|| {
                git_map(format!(
                    "binding ref '{ref_name}' tree holds a non-UTF-8 entry name"
                ))
            })?;
            let allowlisted = matches!(
                name,
                BINDING_BLOB_NAME
                    | ATTESTATION_SUMMARY_BLOB_NAME
                    | CHANGES_PACK_BLOB_NAME
                    | CONFLICTS_PACK_BLOB_NAME
            );
            if !allowlisted {
                return Err(git_map(format!(
                    "binding ref '{ref_name}' tree carries non-allowlisted entry '{name}'; \
                     binding trees hold only the public blobs"
                )));
            }
            let mode = entry.filemode();
            if mode != i32::from(git2::FileMode::Blob) {
                return Err(git_map(format!(
                    "binding ref '{ref_name}' entry '{name}' is not a regular blob (mode {mode:o}); \
                     binding trees confine to plain blobs"
                )));
            }
            if name == CONFLICTS_PACK_BLOB_NAME {
                let blob = git
                    .find_blob(entry.id())
                    .map_err(|error| {
                        git_map(format!(
                            "binding ref '{ref_name}' conflicts.pack blob is missing: {error}"
                        ))
                    })?;
                pack = Some(blob.content().to_vec());
            }
        }
        Ok(pack)
    }

    /// Create the parentless binding commit: a tree holding exactly the
    /// Git-safe public blobs (binding bytes + optional signed summary +
    /// optional bounded changes.pack + optional complete conflicts.pack).
    ///
    /// The commit is written unreferenced; the create-only ref is the actual
    /// publication and is journaled separately, so a crash here leaves an
    /// invisible orphan object, never a published claim.
    fn create_binding_commit(
        &self,
        git: &GitRepository,
        binding: &GitStateBinding,
        summary: Option<&SignedAttestationSummary>,
        changes_pack: Option<&[u8]>,
        conflicts_pack: Option<&[u8]>,
    ) -> Result<GitObjectId, RepositoryError> {
        let mut entries =
            binding_tree_entries(binding, summary).map_err(|error| git_map(error.to_string()))?;
        if let Some(pack) = changes_pack {
            entries.push((CHANGES_PACK_BLOB_NAME, pack.to_vec()));
        }
        if let Some(pack) = conflicts_pack {
            entries.push((CONFLICTS_PACK_BLOB_NAME, pack.to_vec()));
        }

        let mut builder = git.treebuilder(None).map_err(git_map)?;
        for (name, bytes) in &entries {
            let blob = git.blob(bytes).map_err(git_map)?;
            builder
                .insert(name, blob, git2::FileMode::Blob.into())
                .map_err(git_map)?;
        }
        let tree_oid = builder.write().map_err(git_map)?;
        let tree = git.find_tree(tree_oid).map_err(git_map)?;

        // Deterministic author/committer: the binding commit is
        // infrastructure, not authorship. A fixed epoch keeps the commit id a
        // pure function of the tree contents, so republishing the same
        // binding always targets the same commit (create-only idempotency).
        let time = git2::Time::new(0, 0);
        let signature =
            git2::Signature::new("Atomic Binding", "binding@atomic.local", &time)
                .map_err(git_map)?;
        let id = binding.id();
        let payload = binding.payload();
        let message = format!(
            "Atomic Git state binding {}\n\natomic-binding {}\natomic-set {}\natomic-state {}\n",
            id.to_hex(),
            id.to_hex(),
            atomic_core::Base32::to_base32(&payload.set_id),
            atomic_core::Base32::to_base32(&payload.merkle_state),
        );
        let oid = git
            .commit(None, &signature, &signature, &message, &tree, &[])
            .map_err(git_map)?;
        git_object_id(&oid)
    }

    fn prepare_binding_ref_write(
        &self,
        working_copy: WorkingCopyId,
        ref_name: &str,
        observed_old: Option<GitObjectId>,
        intended_new: GitObjectId,
        evidence: Hash,
    ) -> Result<super::operation::PreparedBridgeGitWrite, RepositoryError> {
        self.prepare_bridge_git_ref_write(
            working_copy,
            ref_name,
            observed_old.map(GitRefTarget::Direct),
            GitRefTarget::Direct(intended_new),
            evidence,
        )
    }
}

/// Read the current direct target of `ref_name`, if it exists.
fn read_ref_target(
    git: &GitRepository,
    ref_name: &str,
) -> Result<Option<GitObjectId>, RepositoryError> {
    match git.find_reference(ref_name) {
        Ok(reference) => match reference.target() {
            Some(oid) => Ok(Some(git_object_id(&oid)?)),
            None => Err(git_map(format!(
                "binding ref '{ref_name}' is symbolic; bindings publish to direct refs only"
            ))),
        },
        Err(_) => Ok(None),
    }
}

fn git_object_id(oid: &git2::Oid) -> Result<GitObjectId, RepositoryError> {
    let bytes = oid.as_bytes().to_vec();
    let algorithm = match bytes.len() {
        20 => GitHashAlgorithm::Sha1,
        32 => GitHashAlgorithm::Sha256,
        other => return Err(git_map(format!("unexpected Git OID width {other}"))),
    };
    GitObjectId::new(algorithm, bytes).map_err(|error| git_map(error.to_string()))
}

fn git2_oid(oid: &GitObjectId) -> Result<git2::Oid, RepositoryError> {
    git2::Oid::from_bytes(oid.as_bytes()).map_err(git_map)
}

fn binding_summary_key(
    binding: &GitStateBinding,
) -> Result<atomic_identity::keypair::PublicKey, RepositoryError> {
    atomic_identity::keypair::PublicKey::from_bytes(&binding.payload().signer.verifying_key)
        .map_err(|error| git_map(format!("binding signer key is invalid: {error}")))
}
