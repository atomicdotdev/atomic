//! Persistent operation journal transaction traits.

use crate::operation::{EffectReceipt, Operation, OperationHeads, OperationScope};
use crate::pristine::error::PristineResult;
use crate::types::OperationId;

/// Read access to immutable operations, mutable head sets, and immutable receipts.
pub trait OperationTxnT {
    /// Load and verify one content-addressed operation.
    fn get_operation(&self, id: OperationId) -> PristineResult<Option<Operation>>;

    /// List and verify every operation in canonical `OperationId` table order.
    fn list_operations(&self) -> PristineResult<Vec<Operation>>;

    /// Load the canonical sorted head set for one scope.
    fn get_operation_heads(&self, scope: OperationScope) -> PristineResult<OperationHeads>;

    /// List every persisted scope and its canonical sorted head set.
    fn list_operation_heads(&self) -> PristineResult<Vec<(OperationScope, OperationHeads)>>;

    /// Load and verify every immutable receipt belonging to one operation.
    fn get_effect_receipts(&self, operation: OperationId) -> PristineResult<Vec<EffectReceipt>>;
}

/// Mutation access to the append-only operation journal and CAS-updated heads.
///
/// Before performing external effects, callers must commit prepared operations
/// with [`crate::pristine::Pristine::write_txn_immediate`] so the journal is
/// durable before the external system can diverge.
pub trait OperationMutTxnT: OperationTxnT {
    /// Append an immutable operation, treating identical existing bytes as success.
    fn put_operation(&mut self, operation: &Operation) -> PristineResult<()>;

    /// Replace one head set only when its current canonical set equals `expected`.
    ///
    /// Inputs are treated as sets and canonicalized before comparison/storage.
    fn compare_and_set_operation_heads(
        &mut self,
        scope: OperationScope,
        expected: &[OperationId],
        replacement: &[OperationId],
    ) -> PristineResult<()>;

    /// Append an immutable receipt, treating identical existing bytes as success.
    fn append_effect_receipt(&mut self, receipt: &EffectReceipt) -> PristineResult<()>;
}
