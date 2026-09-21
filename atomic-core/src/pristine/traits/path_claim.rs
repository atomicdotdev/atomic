//! Read and migration APIs for durable path-claim events.

use std::collections::HashMap;

use crate::pristine::path_claim::{
    tree_bijection_error, PathClaimEntry, PathClaimEvent, PATH_CLAIM_SCHEMA_VERSION,
};
use crate::types::Inode;

use super::super::error::PristineError;
use super::tree::TreeTxnT;

/// Read access to the additive, unfiltered path-claim event index.
///
/// Callers are responsible for filtering `event_change` through the desired
/// visibility closure and reducing transitions causally.
pub trait PathClaimTxnT: TreeTxnT {
    /// Completed path-claim schema version, or `None` when backfill is required.
    fn path_claim_schema_version(&self) -> Result<Option<u32>, PristineError>;

    /// Return every event stored for one path, without visibility filtering.
    fn get_path_claims(&self, path: &str) -> Result<Vec<PathClaimEvent>, PristineError>;

    /// Return every path/event pair, without visibility filtering.
    fn iter_path_claims(&self) -> Result<Vec<PathClaimEntry>, PristineError>;

    /// Enumerate the complete reverse TREE index for invariant validation.
    fn iter_rev_tree_pairs(&self) -> Result<Vec<(Inode, String)>, PristineError>;

    /// Whether the durable claim index has completed its current backfill.
    fn path_claim_schema_is_complete(&self) -> Result<bool, PristineError> {
        Ok(self.path_claim_schema_version()? == Some(PATH_CLAIM_SCHEMA_VERSION))
    }

    /// Validate that TREE and REV_TREE are exhaustive one-to-one inverses.
    fn validate_tree_bijection(&self) -> Result<(), PristineError> {
        let mut forward_by_inode = HashMap::<Inode, String>::new();
        let mut forward_count = 0usize;
        for entry in self.iter_tree()? {
            let (path, inode) = entry?;
            forward_count += 1;
            if let Some(previous) = forward_by_inode.insert(inode, path.clone()) {
                return Err(tree_bijection_error(format!(
                    "inode {} is mapped from both '{}' and '{}'",
                    inode.get(),
                    previous,
                    path
                )));
            }
            let reverse = self.get_path(inode)?;
            if reverse.as_deref() != Some(path.as_str()) {
                return Err(tree_bijection_error(format!(
                    "TREE maps '{}' to inode {}, but REV_TREE maps it to {:?}",
                    path,
                    inode.get(),
                    reverse
                )));
            }
        }

        let reverse = self.iter_rev_tree_pairs()?;
        for (inode, path) in &reverse {
            let forward = self.get_inode(path)?;
            if forward != Some(*inode) {
                return Err(tree_bijection_error(format!(
                    "REV_TREE maps inode {} to '{}', but TREE maps it to {:?}",
                    inode.get(),
                    path,
                    forward
                )));
            }
        }
        if forward_count != reverse.len() {
            return Err(tree_bijection_error(format!(
                "TREE contains {forward_count} rows but REV_TREE contains {} rows",
                reverse.len()
            )));
        }
        Ok(())
    }
}

/// Atomic write/backfill access to durable path-claim events.
pub trait PathClaimMutTxnT: PathClaimTxnT {
    /// Add one event. Identical `(path, event)` pairs are idempotent.
    fn put_path_claim(&mut self, path: &str, event: &PathClaimEvent)
        -> Result<bool, PristineError>;

    /// Remove one event (CB-13A follow-up R3: the stored repair inverse's
    /// undo removes exactly the rows a repair added). Returns whether the
    /// event was present. Identical `(path, event)` pairs are idempotent.
    fn del_path_claim(&mut self, path: &str, event: &PathClaimEvent)
        -> Result<bool, PristineError>;

    /// Clear all events and the completion marker before an atomic rebuild.
    fn reset_path_claim_migration(&mut self) -> Result<(), PristineError>;

    /// Validate TREE/REV_TREE and mark the current claim schema complete.
    ///
    /// Call this in the same write transaction that performed the full backfill.
    fn complete_path_claim_migration(&mut self) -> Result<(), PristineError>;
}
