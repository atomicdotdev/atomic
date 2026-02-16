//! Change store trait for content retrieval
//!
//! This module defines the [`ChangeStore`] trait, which abstracts the storage
//! and retrieval of changes. The key operation is reading span content from
//! change files, which is needed for outputting files from the repository graph.
//!
//! # Overview
//!
//! When outputting a file from the repository, we need to:
//! 1. Traverse the alive graph to find vertices
//! 2. For each span, retrieve its content bytes from the change that introduced it
//! 3. Write the content to the working copy
//!
//! The `ChangeStore` trait provides the content retrieval functionality.
//!
//! # Content Addressing
//!
//! Vertices in the graph are identified by `(change_id, start, end)` where:
//! - `change_id` is a repository-local identifier for a change
//! - `start` and `end` are byte offsets into the change's content section
//!
//! The `ChangeStore` maps `change_id` to the actual change (via hash lookup)
//! and retrieves the specified byte range.
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                       Content Retrieval Flow                             │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │   Graph Span                 ChangeStore                 Result       │
//! │   ┌──────────────────┐        ┌────────────────────┐     ┌────────┐    │
//! │   │ change: NodeId(5)│  hash  │ NodeId(5) → Hash   │ get │ bytes  │    │
//! │   │ start: 100       │ ─────► │ Load change file   │ ──► │ [u8]   │    │
//! │   │ end: 150         │        │ Read bytes 100-150 │     │        │    │
//! │   └──────────────────┘        └────────────────────┘     └────────┘    │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_core::change::ChangeStore;
//!
//! fn read_vertex_content<S: ChangeStore>(
//!     store: &S,
//!     txn: &impl GraphTxnT,
//!     span: GraphNode<NodeId>,
//! ) -> Result<Vec<u8>, S::Error> {
//!     let len = (span.end.as_u64() - span.start.as_u64()) as usize;
//!     let mut buf = vec![0u8; len];
//!
//!     let hash_fn = |id: NodeId| txn.get_external(&id).ok().flatten();
//!     store.get_contents(hash_fn, node, &mut buf)?;
//!
//!     Ok(buf)
//! }
//! ```
//!
//! # Implementations
//!
//! This crate provides a memory-based implementation for testing. The
//! `atomic-repository` crate provides a filesystem-based implementation.

use crate::change::{Change, ChangeError, ChangeHeader};
#[allow(unused_imports)]
use crate::types::{Base32, ChangePosition, Hash, NodeId, GraphNode};
use std::collections::HashMap;
use std::sync::RwLock;

// CHANGE STORE TRAIT

/// A trait for storing and retrieving changes.
///
/// The primary operations are:
/// - Loading complete changes by hash
/// - Reading content bytes for specific vertices
/// - Checking for change/content existence
///
/// # Type Parameters
///
/// Implementations define their own `Error` type, which must be convertible
/// from `ChangeError` for change loading operations.
///
/// # Thread Safety
///
/// Implementations should be safe for concurrent read access. The trait
/// methods take `&self`, enabling shared references across threads.
pub trait ChangeStore: Send + Sync {
    /// Error type for change store operations.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Load a complete change by its hash.
    ///
    /// # Arguments
    ///
    /// * `hash` - The content hash of the change
    ///
    /// # Returns
    ///
    /// The complete [`Change`] if found.
    ///
    /// # Errors
    ///
    /// Returns an error if the change is not found or cannot be loaded.
    fn get_change(&self, hash: &Hash) -> Result<Change, Self::Error>;

    /// Check if a change exists.
    ///
    /// # Arguments
    ///
    /// * `hash` - The content hash of the change
    ///
    /// # Returns
    ///
    /// `true` if the change exists in the store.
    fn has_change(&self, hash: &Hash) -> bool {
        self.get_change(hash).is_ok()
    }

    /// Check if a change has content bytes.
    ///
    /// Some changes may have empty content sections (e.g., metadata-only changes).
    ///
    /// # Arguments
    ///
    /// * `hash` - The content hash of the change
    ///
    /// # Returns
    ///
    /// `true` if the change exists and has non-empty content.
    fn has_contents(&self, hash: &Hash) -> bool {
        self.get_change(hash)
            .map(|c| !c.contents.is_empty())
            .unwrap_or(false)
    }

    /// Read content bytes for a span.
    ///
    /// This is the core method for retrieving file content during output.
    /// It reads the byte range `[span.start, span.end)` from the change
    /// that introduced this span.
    ///
    /// # Arguments
    ///
    /// * `hash_fn` - Function to map `NodeId` to `Hash`
    /// * `span` - The span specifying `(change_id, start, end)`
    /// * `buf` - Buffer to fill with content (must be exactly `span.end - span.start` bytes)
    ///
    /// # Returns
    ///
    /// The number of bytes read (should equal buffer length).
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The change cannot be found
    /// - The byte range is out of bounds
    /// - The `hash_fn` returns `None` for the span's change
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let node = GraphNode::new(change_id, ChangePosition::new(0), ChangePosition::new(10));
    /// let mut buf = vec![0u8; 10];
    /// store.get_contents(|id| txn.get_external(&id), span, &mut buf)?;
    /// ```
    fn get_contents<F>(
        &self,
        hash_fn: F,
        node: GraphNode<NodeId>,
        buf: &mut [u8],
    ) -> Result<usize, Self::Error>
    where
        F: Fn(NodeId) -> Option<Hash>;

    /// Read content bytes for a span with external hash references.
    ///
    /// Similar to `get_contents`, but the span uses `Option<Hash>` directly
    /// instead of requiring a hash lookup function.
    ///
    /// # Arguments
    ///
    /// * `span` - The span with optional hash reference
    /// * `buf` - Buffer to fill with content
    ///
    /// # Returns
    ///
    /// The number of bytes read.
    fn get_contents_ext(
        &self,
        node: GraphNode<Option<Hash>>,
        buf: &mut [u8],
    ) -> Result<usize, Self::Error>;

    /// Get the change header without loading the full change.
    ///
    /// This is more efficient when only metadata is needed.
    ///
    /// # Arguments
    ///
    /// * `hash` - The content hash of the change
    ///
    /// # Returns
    ///
    /// The change header (message, authors, timestamp, etc.)
    fn get_header(&self, hash: &Hash) -> Result<ChangeHeader, Self::Error> {
        Ok(self.get_change(hash)?.hashed.header)
    }

    /// Get the dependencies of a change.
    ///
    /// # Arguments
    ///
    /// * `hash` - The content hash of the change
    ///
    /// # Returns
    ///
    /// The list of dependency hashes.
    fn get_dependencies(&self, hash: &Hash) -> Result<Vec<Hash>, Self::Error> {
        Ok(self.get_change(hash)?.hashed.dependencies)
    }

    /// Check if a change knows about another change.
    ///
    /// A change "knows" another if it's either a direct dependency or
    /// in the extra_known list.
    ///
    /// # Arguments
    ///
    /// * `change_hash` - The change to check
    /// * `other_hash` - The change to check for knowledge of
    ///
    /// # Returns
    ///
    /// `true` if `change_hash` knows about `other_hash`.
    fn knows(&self, change_hash: &Hash, other_hash: &Hash) -> Result<bool, Self::Error> {
        let change = self.get_change(change_hash)?;
        Ok(change.knows(other_hash))
    }
}

// MEMORY CHANGE STORE

/// Error type for the in-memory change store.
#[derive(Debug, Clone)]
pub enum MemoryStoreError {
    /// The requested change was not found.
    NotFound(String),
    /// The byte range was out of bounds.
    OutOfBounds {
        /// Requested start position
        start: u64,
        /// Requested end position
        end: u64,
        /// Actual content length
        len: usize,
    },
    /// Change loading error
    ChangeError(String),
}

impl std::fmt::Display for MemoryStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(hash) => write!(f, "Change not found: {}", hash),
            Self::OutOfBounds { start, end, len } => {
                write!(
                    f,
                    "Byte range [{}, {}) out of bounds for content of length {}",
                    start, end, len
                )
            }
            Self::ChangeError(msg) => write!(f, "Change error: {}", msg),
        }
    }
}

impl std::error::Error for MemoryStoreError {}

impl From<ChangeError> for MemoryStoreError {
    fn from(err: ChangeError) -> Self {
        Self::ChangeError(err.to_string())
    }
}

/// An in-memory change store for testing.
///
/// This implementation stores changes in a `HashMap` and is useful for:
/// - Unit tests that don't need disk I/O
/// - Temporary repositories
/// - Testing change store implementations
///
/// # Thread Safety
///
/// Uses `RwLock` for interior mutability, allowing concurrent reads
/// with exclusive writes.
///
/// # Example
///
/// ```rust
/// use atomic_core::change::{Change, ChangeHeader, MemoryChangeStore, ChangeStore};
/// use atomic_core::types::{Hash, Base32};
///
/// let store = MemoryChangeStore::new();
///
/// // Create and store a change
/// let change = Change::empty(ChangeHeader::new("Test change"));
/// let hash = change.hash().unwrap();
/// store.insert(hash, change.clone());
///
/// // Retrieve it
/// let loaded = store.get_change(&hash).unwrap();
/// assert_eq!(loaded.message(), "Test change");
/// ```
pub struct MemoryChangeStore {
    /// The stored changes
    changes: RwLock<HashMap<Hash, Change>>,
}

impl MemoryChangeStore {
    /// Create a new empty memory store.
    pub fn new() -> Self {
        Self {
            changes: RwLock::new(HashMap::new()),
        }
    }

    /// Insert a change into the store.
    ///
    /// # Arguments
    ///
    /// * `hash` - The hash to use as the key
    /// * `change` - The change to store
    pub fn insert(&self, hash: Hash, change: Change) {
        self.changes.write().unwrap().insert(hash, change);
    }

    /// Insert a change, computing its hash automatically.
    ///
    /// # Arguments
    ///
    /// * `change` - The change to store
    ///
    /// # Returns
    ///
    /// The computed hash.
    ///
    /// # Errors
    ///
    /// Returns an error if hash computation fails.
    pub fn insert_change(&self, change: Change) -> Result<Hash, ChangeError> {
        let hash = change.hash()?;
        self.insert(hash, change);
        Ok(hash)
    }

    /// Remove a change from the store.
    ///
    /// # Arguments
    ///
    /// * `hash` - The hash of the change to remove
    ///
    /// # Returns
    ///
    /// The removed change, if it existed.
    pub fn remove(&self, hash: &Hash) -> Option<Change> {
        self.changes.write().unwrap().remove(hash)
    }

    /// Get the number of changes in the store.
    pub fn len(&self) -> usize {
        self.changes.read().unwrap().len()
    }

    /// Check if the store is empty.
    pub fn is_empty(&self) -> bool {
        self.changes.read().unwrap().is_empty()
    }

    /// Clear all changes from the store.
    pub fn clear(&self) {
        self.changes.write().unwrap().clear();
    }

    /// Get all hashes in the store.
    pub fn hashes(&self) -> Vec<Hash> {
        self.changes.read().unwrap().keys().copied().collect()
    }
}

impl Default for MemoryChangeStore {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for MemoryChangeStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let len = self.changes.read().unwrap().len();
        f.debug_struct("MemoryChangeStore")
            .field("count", &len)
            .finish()
    }
}

impl Clone for MemoryChangeStore {
    fn clone(&self) -> Self {
        Self {
            changes: RwLock::new(self.changes.read().unwrap().clone()),
        }
    }
}

impl ChangeStore for MemoryChangeStore {
    type Error = MemoryStoreError;

    fn get_change(&self, hash: &Hash) -> Result<Change, Self::Error> {
        self.changes
            .read()
            .unwrap()
            .get(hash)
            .cloned()
            .ok_or_else(|| MemoryStoreError::NotFound(hash.to_base32()))
    }

    fn has_change(&self, hash: &Hash) -> bool {
        self.changes.read().unwrap().contains_key(hash)
    }

    fn has_contents(&self, hash: &Hash) -> bool {
        self.changes
            .read()
            .unwrap()
            .get(hash)
            .map(|c| !c.contents.is_empty())
            .unwrap_or(false)
    }

    fn get_contents<F>(
        &self,
        hash_fn: F,
        node: GraphNode<NodeId>,
        buf: &mut [u8],
    ) -> Result<usize, Self::Error>
    where
        F: Fn(NodeId) -> Option<Hash>,
    {
        // Handle empty vertices
        if node.end <= node.start || node.is_root() {
            return Ok(0);
        }

        // Get the hash for this change
        let hash = hash_fn(node.change)
            .ok_or_else(|| MemoryStoreError::NotFound(format!("NodeId({})", node.change.0)))?;

        // Load the change
        let changes = self.changes.read().unwrap();
        let change = changes
            .get(&hash)
            .ok_or_else(|| MemoryStoreError::NotFound(hash.to_base32()))?;

        // Extract the content range
        let start = node.start.0.as_u64() as usize;
        let end = node.end.0.as_u64() as usize;

        if end > change.contents.len() {
            return Err(MemoryStoreError::OutOfBounds {
                start: start as u64,
                end: end as u64,
                len: change.contents.len(),
            });
        }

        let content = &change.contents[start..end];
        let len = content.len().min(buf.len());
        buf[..len].copy_from_slice(&content[..len]);

        Ok(len)
    }

    fn get_contents_ext(
        &self,
        node: GraphNode<Option<Hash>>,
        buf: &mut [u8],
    ) -> Result<usize, Self::Error> {
        // Handle empty vertices or no hash
        if node.end <= node.start {
            return Ok(0);
        }

        let hash = match node.change {
            Some(h) => h,
            None => return Ok(0),
        };

        // Load the change
        let changes = self.changes.read().unwrap();
        let change = changes
            .get(&hash)
            .ok_or_else(|| MemoryStoreError::NotFound(hash.to_base32()))?;

        // Extract the content range
        let start = node.start.0.as_u64() as usize;
        let end = node.end.0.as_u64() as usize;

        if end > change.contents.len() {
            return Err(MemoryStoreError::OutOfBounds {
                start: start as u64,
                end: end as u64,
                len: change.contents.len(),
            });
        }

        let content = &change.contents[start..end];
        let len = content.len().min(buf.len());
        buf[..len].copy_from_slice(&content[..len]);

        Ok(len)
    }
}

// TESTS

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::ChangeHeader;
    use crate::types::{Base32, ChangePosition};

    fn create_test_change(message: &str, content: &[u8]) -> Change {
        let mut change = Change::empty(ChangeHeader::new(message));
        change.contents = content.to_vec();
        change.finalize();
        change
    }

    // ------------------------------------------------------------------------
    // MemoryStoreError Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_error_not_found_display() {
        let err = MemoryStoreError::NotFound("ABC123".to_string());
        assert!(err.to_string().contains("ABC123"));
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn test_error_out_of_bounds_display() {
        let err = MemoryStoreError::OutOfBounds {
            start: 10,
            end: 20,
            len: 15,
        };
        let msg = err.to_string();
        assert!(msg.contains("10"));
        assert!(msg.contains("20"));
        assert!(msg.contains("15"));
    }

    #[test]
    fn test_error_change_error_display() {
        let err = MemoryStoreError::ChangeError("test error".to_string());
        assert!(err.to_string().contains("test error"));
    }

    #[test]
    fn test_error_from_change_error() {
        let change_err = ChangeError::Invalid("test".to_string());
        let store_err: MemoryStoreError = change_err.into();
        assert!(matches!(store_err, MemoryStoreError::ChangeError(_)));
    }

    // ------------------------------------------------------------------------
    // MemoryChangeStore Basic Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_new_store_is_empty() {
        let store = MemoryChangeStore::new();
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn test_default_store_is_empty() {
        let store = MemoryChangeStore::default();
        assert!(store.is_empty());
    }

    #[test]
    fn test_insert_and_get() {
        let store = MemoryChangeStore::new();
        let change = create_test_change("Test", b"content");
        let hash = change.hash().unwrap();

        store.insert(hash, change.clone());

        assert!(!store.is_empty());
        assert_eq!(store.len(), 1);
        assert!(store.has_change(&hash));

        let loaded = store.get_change(&hash).unwrap();
        assert_eq!(loaded.message(), "Test");
    }

    #[test]
    fn test_insert_change_computes_hash() {
        let store = MemoryChangeStore::new();
        let change = create_test_change("Auto hash", b"data");

        let hash = store.insert_change(change).unwrap();

        assert!(store.has_change(&hash));
    }

    #[test]
    fn test_remove() {
        let store = MemoryChangeStore::new();
        let change = create_test_change("To remove", b"bye");
        let hash = change.hash().unwrap();

        store.insert(hash, change);
        assert!(store.has_change(&hash));

        let removed = store.remove(&hash);
        assert!(removed.is_some());
        assert!(!store.has_change(&hash));
        assert!(store.is_empty());
    }

    #[test]
    fn test_remove_nonexistent() {
        let store = MemoryChangeStore::new();
        let hash = Hash::of(b"nonexistent");

        let removed = store.remove(&hash);
        assert!(removed.is_none());
    }

    #[test]
    fn test_clear() {
        let store = MemoryChangeStore::new();

        for i in 0..5 {
            let change = create_test_change(&format!("Change {}", i), &[i as u8]);
            store.insert_change(change).unwrap();
        }

        assert_eq!(store.len(), 5);

        store.clear();

        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn test_hashes() {
        let store = MemoryChangeStore::new();
        let change1 = create_test_change("One", b"1");
        let change2 = create_test_change("Two", b"2");
        let hash1 = change1.hash().unwrap();
        let hash2 = change2.hash().unwrap();

        store.insert(hash1, change1);
        store.insert(hash2, change2);

        let hashes = store.hashes();
        assert_eq!(hashes.len(), 2);
        assert!(hashes.contains(&hash1));
        assert!(hashes.contains(&hash2));
    }

    #[test]
    fn test_clone() {
        let store = MemoryChangeStore::new();
        let change = create_test_change("Clone test", b"data");
        let hash = change.hash().unwrap();
        store.insert(hash, change);

        let cloned = store.clone();

        assert!(cloned.has_change(&hash));
        assert_eq!(cloned.len(), 1);

        // Modifications to clone don't affect original
        cloned.clear();
        assert!(store.has_change(&hash));
    }

    #[test]
    fn test_debug() {
        let store = MemoryChangeStore::new();
        let debug = format!("{:?}", store);
        assert!(debug.contains("MemoryChangeStore"));
        assert!(debug.contains("count"));
    }

    // ------------------------------------------------------------------------
    // ChangeStore Trait Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_get_change_not_found() {
        let store = MemoryChangeStore::new();
        let hash = Hash::of(b"nonexistent");

        let result = store.get_change(&hash);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            MemoryStoreError::NotFound(_)
        ));
    }

    #[test]
    fn test_has_contents() {
        let store = MemoryChangeStore::new();

        // Change with content
        let with_content = create_test_change("Has content", b"data");
        let hash1 = with_content.hash().unwrap();
        store.insert(hash1, with_content);

        // Change without content
        let without_content = create_test_change("No content", b"");
        let hash2 = without_content.hash().unwrap();
        store.insert(hash2, without_content);

        assert!(store.has_contents(&hash1));
        assert!(!store.has_contents(&hash2));
        assert!(!store.has_contents(&Hash::of(b"nonexistent")));
    }

    #[test]
    fn test_get_header() {
        let store = MemoryChangeStore::new();
        let change = create_test_change("Header test", b"content");
        let hash = change.hash().unwrap();
        store.insert(hash, change);

        let header = store.get_header(&hash).unwrap();
        assert_eq!(header.message, "Header test");
    }

    #[test]
    fn test_get_dependencies() {
        let store = MemoryChangeStore::new();

        let dep_hash = Hash::of(b"dependency");
        let mut change = Change::empty(ChangeHeader::new("With deps"));
        change.hashed.dependencies = vec![dep_hash];

        let hash = change.hash().unwrap();
        store.insert(hash, change);

        let deps = store.get_dependencies(&hash).unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0], dep_hash);
    }

    #[test]
    fn test_knows() {
        let store = MemoryChangeStore::new();

        let known_hash = Hash::of(b"known");
        let extra_hash = Hash::of(b"extra");
        let unknown_hash = Hash::of(b"unknown");

        let mut change = Change::empty(ChangeHeader::new("Knows test"));
        change.hashed.dependencies = vec![known_hash];
        change.hashed.extra_known = vec![extra_hash];

        let hash = change.hash().unwrap();
        store.insert(hash, change);

        assert!(store.knows(&hash, &known_hash).unwrap());
        assert!(store.knows(&hash, &extra_hash).unwrap());
        assert!(!store.knows(&hash, &unknown_hash).unwrap());
    }

    // ------------------------------------------------------------------------
    // Content Retrieval Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_get_contents_basic() {
        let store = MemoryChangeStore::new();
        let content = b"Hello, World!";
        let change = create_test_change("Content test", content);
        let hash = change.hash().unwrap();
        store.insert(hash, change);

        // Create a mock hash function
        let node_id = NodeId::new(1);
        let hash_fn = move |id: NodeId| {
            if id == node_id {
                Some(hash)
            } else {
                None
            }
        };

        // Read the full content
        let node = GraphNode::new(
            node_id,
            ChangePosition::new(0),
            ChangePosition::new(content.len() as u64),
        );
        let mut buf = vec![0u8; content.len()];

        let n = store.get_contents(hash_fn, node, &mut buf).unwrap();

        assert_eq!(n, content.len());
        assert_eq!(&buf, content);
    }

    #[test]
    fn test_get_contents_partial() {
        let store = MemoryChangeStore::new();
        let content = b"0123456789";
        let change = create_test_change("Partial", content);
        let hash = change.hash().unwrap();
        store.insert(hash, change);

        let node_id = NodeId::new(1);
        let hash_fn = move |id: NodeId| {
            if id == node_id {
                Some(hash)
            } else {
                None
            }
        };

        // Read middle portion
        let node = GraphNode::new(node_id, ChangePosition::new(3), ChangePosition::new(7));
        let mut buf = vec![0u8; 4];

        let n = store.get_contents(hash_fn, node, &mut buf).unwrap();

        assert_eq!(n, 4);
        assert_eq!(&buf, b"3456");
    }

    #[test]
    fn test_get_contents_empty_vertex() {
        let store = MemoryChangeStore::new();
        let change = create_test_change("Empty node", b"data");
        let hash = change.hash().unwrap();
        store.insert(hash, change);

        let node_id = NodeId::new(1);
        let hash_fn = move |id: NodeId| {
            if id == node_id {
                Some(hash)
            } else {
                None
            }
        };

        // Empty range (start == end)
        let node = GraphNode::new(node_id, ChangePosition::new(5), ChangePosition::new(5));
        let mut buf = vec![0u8; 0];

        let n = store.get_contents(hash_fn, node, &mut buf).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn test_get_contents_root_vertex() {
        let store = MemoryChangeStore::new();

        let hash_fn = |_: NodeId| -> Option<Hash> { None };

        // ROOT span should return 0
        let mut buf = vec![0u8; 10];
        let n = store.get_contents(hash_fn, GraphNode::ROOT, &mut buf).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn test_get_contents_out_of_bounds() {
        let store = MemoryChangeStore::new();
        let content = b"short";
        let change = create_test_change("OOB", content);
        let hash = change.hash().unwrap();
        store.insert(hash, change);

        let node_id = NodeId::new(1);
        let hash_fn = move |id: NodeId| {
            if id == node_id {
                Some(hash)
            } else {
                None
            }
        };

        // Request beyond content length
        let node = GraphNode::new(node_id, ChangePosition::new(0), ChangePosition::new(100));
        let mut buf = vec![0u8; 100];

        let result = store.get_contents(hash_fn, node, &mut buf);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            MemoryStoreError::OutOfBounds { .. }
        ));
    }

    #[test]
    fn test_get_contents_hash_not_found() {
        let store = MemoryChangeStore::new();

        // Hash function returns None
        let hash_fn = |_: NodeId| -> Option<Hash> { None };

        let node = GraphNode::new(NodeId::new(1), ChangePosition::new(0), ChangePosition::new(10));
        let mut buf = vec![0u8; 10];

        let result = store.get_contents(hash_fn, node, &mut buf);
        assert!(result.is_err());
    }

    #[test]
    fn test_get_contents_ext_basic() {
        let store = MemoryChangeStore::new();
        let content = b"External hash content";
        let change = create_test_change("Ext test", content);
        let hash = change.hash().unwrap();
        store.insert(hash, change);

        let node = GraphNode::new(
            Some(hash),
            ChangePosition::new(0),
            ChangePosition::new(content.len() as u64),
        );
        let mut buf = vec![0u8; content.len()];

        let n = store.get_contents_ext(node, &mut buf).unwrap();

        assert_eq!(n, content.len());
        assert_eq!(&buf, content);
    }

    #[test]
    fn test_get_contents_ext_none_hash() {
        let store = MemoryChangeStore::new();

        let node: GraphNode<Option<Hash>> =
            GraphNode::new(None, ChangePosition::new(0), ChangePosition::new(10));
        let mut buf = vec![0u8; 10];

        let n = store.get_contents_ext(node, &mut buf).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn test_get_contents_ext_empty_range() {
        let store = MemoryChangeStore::new();
        let change = create_test_change("Empty ext", b"data");
        let hash = change.hash().unwrap();
        store.insert(hash, change);

        let node = GraphNode::new(Some(hash), ChangePosition::new(5), ChangePosition::new(5));
        let mut buf = vec![0u8; 0];

        let n = store.get_contents_ext(node, &mut buf).unwrap();
        assert_eq!(n, 0);
    }

    // ------------------------------------------------------------------------
    // Thread Safety Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_concurrent_reads() {
        use std::sync::Arc;
        use std::thread;

        let store = Arc::new(MemoryChangeStore::new());
        let change = create_test_change("Concurrent", b"shared data");
        let hash = change.hash().unwrap();
        store.insert(hash, change);

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let store = Arc::clone(&store);
                thread::spawn(move || {
                    for _ in 0..100 {
                        assert!(store.has_change(&hash));
                        let _ = store.get_change(&hash).unwrap();
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }
    }

    // ------------------------------------------------------------------------
    // Edge Case Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_single_byte_content() {
        let store = MemoryChangeStore::new();
        let content = b"X";
        let change = create_test_change("Single byte", content);
        let hash = change.hash().unwrap();
        store.insert(hash, change);

        let node_id = NodeId::new(1);
        let hash_fn = move |id: NodeId| {
            if id == node_id {
                Some(hash)
            } else {
                None
            }
        };

        let node = GraphNode::new(node_id, ChangePosition::new(0), ChangePosition::new(1));
        let mut buf = vec![0u8; 1];

        let n = store.get_contents(hash_fn, node, &mut buf).unwrap();
        assert_eq!(n, 1);
        assert_eq!(buf[0], b'X');
    }

    #[test]
    fn test_unicode_content() {
        let store = MemoryChangeStore::new();
        let content = "Hello, 世界! 🌍".as_bytes();
        let change = create_test_change("Unicode", content);
        let hash = change.hash().unwrap();
        store.insert(hash, change);

        let node_id = NodeId::new(1);
        let hash_fn = move |id: NodeId| {
            if id == node_id {
                Some(hash)
            } else {
                None
            }
        };

        let node = GraphNode::new(
            node_id,
            ChangePosition::new(0),
            ChangePosition::new(content.len() as u64),
        );
        let mut buf = vec![0u8; content.len()];

        let n = store.get_contents(hash_fn, node, &mut buf).unwrap();
        assert_eq!(n, content.len());
        assert_eq!(std::str::from_utf8(&buf).unwrap(), "Hello, 世界! 🌍");
    }

    #[test]
    fn test_binary_content() {
        let store = MemoryChangeStore::new();
        let content: Vec<u8> = (0..=255).collect();
        let change = create_test_change("Binary", &content);
        let hash = change.hash().unwrap();
        store.insert(hash, change);

        let node_id = NodeId::new(1);
        let hash_fn = move |id: NodeId| {
            if id == node_id {
                Some(hash)
            } else {
                None
            }
        };

        let node = GraphNode::new(
            node_id,
            ChangePosition::new(0),
            ChangePosition::new(content.len() as u64),
        );
        let mut buf = vec![0u8; content.len()];

        let n = store.get_contents(hash_fn, node, &mut buf).unwrap();
        assert_eq!(n, content.len());
        assert_eq!(buf, content);
    }

    #[test]
    fn test_multiple_changes() {
        let store = MemoryChangeStore::new();

        let changes: Vec<_> = (0..10)
            .map(|i| {
                let content = format!("Content {}", i);
                let change = create_test_change(&format!("Change {}", i), content.as_bytes());
                let hash = change.hash().unwrap();
                store.insert(hash, change);
                (hash, content)
            })
            .collect();

        // Verify all can be retrieved
        for (hash, expected_content) in &changes {
            let loaded = store.get_change(hash).unwrap();
            assert_eq!(
                std::str::from_utf8(&loaded.contents).unwrap(),
                expected_content
            );
        }

        assert_eq!(store.len(), 10);
    }
}
