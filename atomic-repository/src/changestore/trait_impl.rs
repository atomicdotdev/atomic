//! Implementation of the `atomic_core::change::ChangeStore` trait.
//!
//! This bridges the repository's filesystem-backed `ChangeStore` with the
//! core library's content retrieval functions (like `retrieve_content`).

use atomic_core::change::ChangeHeader;
use atomic_core::change::ChangeStore as ChangeStoreTrait;
use atomic_core::types::{GraphNode, Hash, NodeId};

use super::{ChangeStore, ChangeStoreError};

impl ChangeStoreTrait for ChangeStore {
    type Error = ChangeStoreError;

    fn get_change(&self, hash: &Hash) -> Result<atomic_core::change::Change, Self::Error> {
        self.load_change(hash)
    }

    fn has_change(&self, hash: &Hash) -> bool {
        ChangeStore::has_change(self, hash)
    }

    fn get_contents<F>(
        &self,
        hash_fn: F,
        span: GraphNode<NodeId>,
        buf: &mut [u8],
    ) -> Result<usize, Self::Error>
    where
        F: Fn(NodeId) -> Option<Hash>,
    {
        // Handle ROOT span
        if span == GraphNode::ROOT {
            return Ok(0);
        }

        // Get the hash for this span's change
        let hash = match hash_fn(span.change) {
            Some(h) => h,
            None => {
                return Err(ChangeStoreError::NotFound {
                    hash: format!("NodeId({:?})", span.change),
                });
            }
        };

        let start = span.start.get() as usize;
        let end = span.end.get() as usize;

        self.copy_content_span(&hash, start, end, buf)
    }

    fn get_contents_ext(
        &self,
        span: GraphNode<Option<Hash>>,
        buf: &mut [u8],
    ) -> Result<usize, Self::Error> {
        // Handle None hash (ROOT span)
        let hash = match span.change {
            Some(h) => h,
            None => return Ok(0),
        };

        let start = span.start.get() as usize;
        let end = span.end.get() as usize;

        self.copy_content_span(&hash, start, end, buf)
    }

    fn get_header(&self, hash: &Hash) -> Result<ChangeHeader, Self::Error> {
        let change = self.load_change(hash)?;
        Ok(change.hashed.header)
    }

    fn get_dependencies(&self, hash: &Hash) -> Result<Vec<Hash>, Self::Error> {
        let change = self.load_change(hash)?;
        Ok(change.hashed.dependencies)
    }
}
