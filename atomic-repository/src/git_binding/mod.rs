//! Signed Git state bindings (RFC-ATOMIC-GIT-CAUSAL-BRIDGE §5.1, 5.2, 5.6, 8.2, 8.6).
//!
//! A [`GitStateBinding`] is an immutable, independently verifiable statement
//! that one Git commit corresponds to one exact Atomic state. Content
//! correctness is *never* derived from the signature: the bound Git tree is
//! always recomputed and compared (§5.2). The Ed25519 signature only
//! authenticates provenance — who claims this binding — and gates whether the
//! provenance/attestation roots may be trusted (§5.6, [`crate::git_binding::trust`]).
//!
//! # Canonical encoding and the non-circular identity contract
//!
//! This is the explicitly reviewed identity contract for CB-6A. It resolves
//! how hashes, signatures, and the commit/binding/operation references fit
//! together without circular identity:
//!
//! 1. **Payload bytes** (`payload_v1`): every binding field except the
//!    signature and the derived id, encoded with the canonical codec
//!    (`"GSB1"` magic + postcard with a fixed V1 field order; deterministic
//!    varints; no maps with nondeterministic iteration order).
//! 2. **Signature** = Ed25519 over `SIGN_DOMAIN || payload_v1`, where
//!    SIGN_DOMAIN is domain-separated. The signer (`did` + verifying key) is
//!    *inside* the payload, so the signature covers it.
//! 3. **Binding id** = Blake3 over `ID_DOMAIN || payload_v1 || signature`.
//!    The payload never contains the id or the signature, so the id is a pure
//!    function of bytes that are themselves a pure function of the payload.
//! 4. **Operation reference** (`payload.operation`) names the *causal
//!    operation that produced the bound Atomic state* — an id computed before
//!    the binding exists. The publication operation that journals the
//!    create-only `refs/atomic/bindings/<shard>/<id>` ref is a *child* of the
//!    binding: its effect names the binding id, while the binding never names
//!    the publication operation. Both reference edges therefore point
//!    forward in time; no identifier depends on itself.
//! 5. **closure_root** = Blake3 over `CLOSURE_DOMAIN` followed by each change
//!    hash of `ordered_changes` in order. It is a bound field: any reordering
//!    or substitution of the closure fails verification.
//! 6. **Raw foreign commit bytes** (`raw_commit_object`) are preserved
//!    verbatim and bound by requiring the Git object digest of the raw bytes
//!    (format-tagged SHA-1/SHA-256 commit digest) to equal `git_commit`, at
//!    encoding and at every verification. Raw bytes are never reconstructed
//!    or re-signed.
//!
//! Tampering with *any* field (including the signature, the signer, or the
//! ordering of ordered data) is rejected by recomputation; unknown or
//! unsupported versions fail closed.
//!
//! # Privacy boundary (§5.6, §12.13)
//!
//! Binding trees carry only required public binding data, provenance/
//! attestation *hashes*, and a separately signed summary
//! ([`crate::git_binding::summary`]) containing model vendor/name, token
//! counts, cost, and session id. Transcripts, prompts, decision graphs, and
//! unhashed private bodies never enter Git objects — see
//! [`crate::git_binding::privacy`] for the enforced serialization boundary.

mod codec;
mod pack;
mod privacy;
mod summary;
mod transport;
mod trust;
mod verify;

pub use codec::{
    commit_object_digest, BindingDecodeError, BindingEncodeError, BindingId, BindingSigner,
    CausalOrigin, GitObjectFormat, GitOid, GitStateBinding, GitStateBindingPayload, LossNote,
    BINDING_MAGIC, BINDING_SIGN_DOMAIN, BINDING_VERSION, CLOSURE_ROOT_DOMAIN, ID_DOMAIN,
};
pub use pack::{
    assemble_changes_pack, decode_changes_pack, private_material_in_change, quarantine_change_record,
    validate_binding_closure, BindingClosureError, BindingPackError, BindingPackLimits,
    ChangesPack, ChangesPackV1, ClosureChangeSource, ClosureValidation, QuarantinedPack,
};
pub use privacy::{
    binding_pack_records, binding_tree_entries, conflicts_pack_self_validates,
    validate_conflicts_pack, BindingTreeError, ConflictPackError, ATTESTATION_SUMMARY_BLOB_NAME,
    BINDING_BLOB_NAME, CHANGES_PACK_BLOB_NAME, CONFLICTS_PACK_BLOB_NAME,
};
pub use summary::{
    BindingAttestationError, BindingAttestationSummary, SignedAttestationSummary,
    SummaryModelUsage, ATTESTATION_SUMMARY_MAGIC,
};
pub use transport::{
    binding_id_from_ref_name, binding_ref_name, is_transferable_ref, is_wip_ref,
    namespace_rejection_diagnostic, transferable_binding_refs, BindingTransportDiagnostic,
    DEGRADED_HEAD_BINDING_PREFIX, WIP_REF_PREFIX,
};
pub use trust::{
    evaluate_binding_trust, signer_trust_for, BindingTrustEvaluation, BindingVerificationInput,
    UntrustedProvenance, UntrustedReason,
};
pub use verify::{
    binding_hint_mismatches_commit, verify_binding_content, verify_binding_cryptography,
    verify_binding_hint, BindingVerificationError,
};
