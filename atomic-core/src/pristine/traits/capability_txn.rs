//! Read and mutate access to repository capability requirements.
//!
//! Capability requirements live in `PRISTINE_META` under
//! `required-capability/<id>` (CB-13B). The lease machinery in
//! `atomic-repository` observes and moves these rows as
//! [`crate::operation::MetadataTarget::Capability`] transitions, so both the
//! read and write transactions expose the same narrow surface.

use crate::pristine::capability::{RepositoryCapability, REQUIRED_CAPABILITY_PREFIX};
use crate::pristine::error::PristineResult;

/// Read access to durable repository capability requirements.
pub trait CapabilityTxnT {
    /// The currently required minimum version for capability `id`, or `None`
    /// when the repository does not require it.
    fn required_capability_version(&self, id: &str) -> PristineResult<Option<u32>>;
}

/// Mutate access to durable repository capability requirements.
///
/// Writes are raise-only and delete-only; the caller supplies the exact
/// [`RepositoryCapability`] it means to require, and an existing higher
/// requirement is never lowered by an insert.
pub trait CapabilityMutTxnT: CapabilityTxnT {
    /// Require `capability`, raising an existing lower requirement. Returns
    /// the durable requirement version after the write.
    fn put_required_capability(&mut self, capability: RepositoryCapability) -> PristineResult<u32>;

    /// Require exactly `version` for capability `id`, replacing any existing
    /// row. Returns the durable requirement version after the write.
    ///
    /// Unlike [`Self::put_required_capability`] this write is not raise-only.
    /// The journaled metadata lease path has already observed the row's
    /// expected-old value under its operation lease, so replay and inverse
    /// recovery must be able to rewrite the row byte-exactly — including
    /// lowering it during an inverse — instead of silently substituting the
    /// build's maximum supported version for the leased one. Callers that do
    /// not hold such a lease must use [`Self::put_required_capability`].
    fn put_required_capability_exact(&mut self, id: &str, version: u32) -> PristineResult<u32>;

    /// Remove the requirement for capability `id`. Returns whether a row was
    /// removed.
    fn del_required_capability(&mut self, id: &str) -> PristineResult<bool>;
}

/// The `PRISTINE_META` key for one capability requirement.
pub(crate) fn capability_metadata_key(id: &str) -> String {
    format!("{REQUIRED_CAPABILITY_PREFIX}{id}")
}
