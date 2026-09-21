//! Immutable storage for Git state bindings (RFC-ATOMIC-GIT-CAUSAL-BRIDGE §5.1).
//!
//! Bindings are immutable facts about foreign Git commits. The pristine only
//! stores their canonical signed bytes under the binding's content address and
//! never mutates or deletes a row:
//!
//! - an insert of byte-identical data is an idempotent no-op;
//! - an insert under an existing id with different bytes is refused — that can
//!   only be a hash collision or corruption, never newer data;
//! - there is no delete path. `unrecord`/`insert` create new states and new
//!   bindings; old bindings stay valid for old commits.
//!
//! The value is the complete canonical signed encoding exactly as published,
//! so the stored bytes are what the signer signed. Decode/verify of the
//! payload (including the Ed25519 signature and version checks) belongs to the
//! repository layer, which owns the identity crate and Git object formats.

use super::{PristineError, PristineResult};

/// Outcome of an insert-only binding write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingStoreOutcome {
    /// The binding was not present and has been stored.
    Stored,
    /// A byte-identical binding already existed; nothing changed.
    Idempotent,
}

/// Read access to immutable Git state bindings.
pub trait BindingTxnT {
    /// Return the stored canonical signed bytes for `id`, or `None`.
    fn get_binding_bytes(&self, id: &[u8; 32]) -> PristineResult<Option<Vec<u8>>>;

    /// Every stored binding id in deterministic key order.
    fn iter_binding_ids(&self) -> PristineResult<Vec<[u8; 32]>>;
}

/// Insert-only mutation access to immutable Git state bindings.
pub trait BindingMutTxnT: BindingTxnT {
    /// Insert the canonical signed binding bytes under `id`.
    ///
    /// Fails closed when a different binding already exists under `id`;
    /// never overwrites, never deletes.
    fn insert_binding_bytes(&mut self, id: &[u8; 32], bytes: &[u8])
        -> PristineResult<BindingStoreOutcome>;
}

impl BindingStoreOutcome {
    /// Whether the write landed a previously absent row.
    pub fn stored(self) -> bool {
        matches!(self, BindingStoreOutcome::Stored)
    }
}

pub(super) fn binding_conflict(id_hex: &str) -> PristineError {
    PristineError::Serialization {
        message: format!(
            "immutable binding {id_hex} already exists with different bytes; refusing to overwrite"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pristine::MutTxnT;
    use crate::pristine::Pristine;

    fn hex(id: &[u8; 32]) -> String {
        id.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn bindings_are_insert_only_and_idempotent() {
        let directory = tempfile::tempdir().expect("tempdir");
        let pristine = Pristine::open(directory.path().join("pristine")).expect("open pristine");

        let id = [7u8; 32];
        let bytes = b"atomic:git-state-binding:v1\0payload".to_vec();

        let mut txn = pristine.write_txn().expect("write txn");
        assert_eq!(
            txn.insert_binding_bytes(&id, &bytes).expect("first insert"),
            BindingStoreOutcome::Stored
        );
        assert_eq!(
            txn.insert_binding_bytes(&id, &bytes).expect("replay"),
            BindingStoreOutcome::Idempotent,
            "identical bytes are an idempotent no-op"
        );
        txn.commit().expect("commit");
        drop(pristine);

        let pristine = Pristine::open(directory.path().join("pristine")).expect("reopen pristine");
        let txn = pristine.read_txn().expect("read txn");
        assert_eq!(
            txn.get_binding_bytes(&id).expect("read binding"),
            Some(bytes.clone())
        );
        let ids = txn.iter_binding_ids().expect("iterate bindings");
        assert_eq!(ids, vec![id]);
        assert_eq!(hex(&ids[0]).len(), 64);
    }

    #[test]
    fn conflicting_binding_bytes_fail_closed_without_overwrite() {
        let directory = tempfile::tempdir().expect("tempdir");
        let pristine = Pristine::open(directory.path().join("pristine")).expect("open pristine");

        let id = [9u8; 32];
        let mut txn = pristine.write_txn().expect("write txn");
        txn.insert_binding_bytes(&id, b"first")
            .expect("first insert");
        let conflicting = txn.insert_binding_bytes(&id, b"second");
        assert!(conflicting.is_err(), "conflicting bytes must be refused");
        assert_eq!(
            txn.get_binding_bytes(&id).expect("read binding"),
            Some(b"first".to_vec()),
            "the original binding must survive the refused write"
        );
        txn.commit().expect("commit refused-write txn");
    }

    #[test]
    fn missing_bindings_read_as_none() {
        let directory = tempfile::tempdir().expect("tempdir");
        let pristine = Pristine::open(directory.path().join("pristine")).expect("open pristine");
        let txn = pristine.read_txn().expect("read txn");
        assert_eq!(txn.get_binding_bytes(&[0u8; 32]).expect("read"), None);
        assert!(txn.iter_binding_ids().expect("iterate").is_empty());
    }
}
