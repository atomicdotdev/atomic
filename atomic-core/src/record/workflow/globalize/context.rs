use super::*;

// CONTEXT

/// Context for globalization operations.
///
/// Holds state needed during the globalization process, including:
/// - A reference to the transaction for graph lookups
/// - The content buffer for accumulating new content
/// - Dependency tracking
/// - Position caching for performance
///
/// # Lifetime Parameters
///
/// - `'txn`: The lifetime of the transaction reference
///
/// # Example
///
/// ```rust,ignore
/// use atomic_core::record::workflow::globalize::GlobalizeContext;
///
/// let mut ctx = GlobalizeContext::new(&txn);
///
/// // Append content and get position
/// let (start, end) = ctx.append_content(b"Hello, world!");
///
/// // Track a dependency
/// ctx.add_dependency(existing_change_hash);
/// ```
pub struct GlobalizeContext<'txn, T> {
    /// Reference to the transaction for graph lookups.
    pub(super) txn: &'txn T,

    /// Content buffer for new content.
    ///
    /// Hunks reference byte ranges within this buffer.
    pub(super) content: Vec<u8>,

    /// Current position in the content buffer.
    pub(super) content_position: u64,

    /// Dependencies collected during globalization.
    ///
    /// These are hashes of changes that the new change depends on.
    pub(super) dependencies: HashSet<Hash>,

    /// Cache of resolved inodes.
    ///
    /// Maps paths to their resolved inodes for performance.
    pub(super) inode_cache: std::collections::HashMap<String, Inode>,

    /// Cache of inode positions.
    ///
    /// Maps inodes to their graph positions.
    pub(super) position_cache: std::collections::HashMap<Inode, Position<NodeId>>,
}

impl<'txn, T> GlobalizeContext<'txn, T>
where
    T: GraphTxnT + TreeTxnT,
{
    /// Create a new globalization context.
    ///
    /// # Arguments
    ///
    /// * `txn` - Transaction for graph lookups
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let ctx = GlobalizeContext::new(&txn);
    /// assert!(ctx.dependencies().is_empty());
    /// ```
    pub fn new(txn: &'txn T) -> Self {
        Self {
            txn,
            content: Vec::new(),
            content_position: 0,
            dependencies: HashSet::new(),
            inode_cache: std::collections::HashMap::new(),
            position_cache: std::collections::HashMap::new(),
        }
    }

    /// Create a context with pre-allocated content buffer.
    ///
    /// # Arguments
    ///
    /// * `txn` - Transaction for graph lookups
    /// * `capacity` - Initial capacity for content buffer
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let ctx = GlobalizeContext::with_capacity(&txn, 1024 * 1024);
    /// ```
    pub fn with_capacity(txn: &'txn T, capacity: usize) -> Self {
        Self {
            txn,
            content: Vec::with_capacity(capacity),
            content_position: 0,
            dependencies: HashSet::new(),
            inode_cache: std::collections::HashMap::new(),
            position_cache: std::collections::HashMap::new(),
        }
    }

    /// Append content to the buffer and return the position range.
    ///
    /// # Arguments
    ///
    /// * `data` - Content bytes to append
    ///
    /// # Returns
    ///
    /// A tuple of (start_position, end_position) for the appended content.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let (start, end) = ctx.append_content(b"Hello");
    /// assert_eq!(end - start, 5);
    /// ```
    pub fn append_content(&mut self, data: &[u8]) -> (ChangePosition, ChangePosition) {
        let start = ChangePosition::new(self.content_position);
        self.content.extend_from_slice(data);
        self.content_position += data.len() as u64;
        let end = ChangePosition::new(self.content_position);
        (start, end)
    }

    /// Add a dependency on an existing change.
    ///
    /// Dependencies are automatically deduplicated.
    ///
    /// # Arguments
    ///
    /// * `hash` - Hash of the change to depend on
    pub fn add_dependency(&mut self, hash: Hash) {
        self.dependencies.insert(hash);
    }

    /// Add a dependency by node ID, looking up the hash.
    ///
    /// # Arguments
    ///
    /// * `node_id` - Internal ID of the change
    ///
    /// # Returns
    ///
    /// Ok(()) if the dependency was added, or an error if the node ID
    /// has no associated hash.
    pub fn add_dependency_by_id(&mut self, node_id: NodeId) -> GlobalizeResult<()> {
        if node_id == NodeId::ROOT {
            // Root node has no hash dependency
            return Ok(());
        }
        if let Some(hash) = self.txn.get_external(node_id)? {
            self.dependencies.insert(hash);
        }
        Ok(())
    }

    /// Get the collected dependencies.
    ///
    /// # Returns
    ///
    /// A reference to the set of dependency hashes.
    #[must_use]
    pub fn dependencies(&self) -> &HashSet<Hash> {
        &self.dependencies
    }

    /// Get the dependencies as a sorted vector.
    ///
    /// Sorting ensures deterministic change hashes.
    ///
    /// # Returns
    ///
    /// A vector of dependency hashes in sorted order.
    #[must_use]
    pub fn dependencies_sorted(&self) -> Vec<Hash> {
        let mut deps: Vec<Hash> = self.dependencies.iter().copied().collect();
        deps.sort();
        deps
    }

    /// Take ownership of the content buffer.
    ///
    /// After calling this, the context's content buffer is empty.
    ///
    /// # Returns
    ///
    /// The accumulated content bytes.
    #[must_use]
    pub fn take_content(self) -> Vec<u8> {
        self.content
    }

    /// Get a reference to the content buffer.
    #[must_use]
    pub fn content(&self) -> &[u8] {
        &self.content
    }

    /// Get the current content position (total bytes appended).
    #[must_use]
    pub fn content_len(&self) -> u64 {
        self.content_position
    }

    /// Get a reference to the transaction.
    #[must_use]
    pub fn txn(&self) -> &'txn T {
        self.txn
    }

    /// Get the external hash for a node ID.
    ///
    /// # Arguments
    ///
    /// * `node_id` - Internal node ID to look up
    ///
    /// # Returns
    ///
    /// The external hash if found, None if the node ID is ROOT or not found.
    #[must_use]
    pub fn get_external(&self, node_id: NodeId) -> Option<Hash> {
        if node_id == NodeId::ROOT {
            return None;
        }
        self.txn.get_external(node_id).ok().flatten()
    }

    /// Clear the caches.
    ///
    /// Call this if the underlying graph has changed.
    pub fn clear_caches(&mut self) {
        self.inode_cache.clear();
        self.position_cache.clear();
    }

    /// Get cache statistics for debugging.
    #[must_use]
    pub fn cache_stats(&self) -> CacheStats {
        CacheStats {
            inode_cache_size: self.inode_cache.len(),
            position_cache_size: self.position_cache.len(),
        }
    }
}

/// Statistics about the globalization context caches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheStats {
    /// Number of entries in the inode cache.
    pub inode_cache_size: usize,
    /// Number of entries in the position cache.
    pub position_cache_size: usize,
}

impl fmt::Display for CacheStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "CacheStats {{ inodes: {}, positions: {} }}",
            self.inode_cache_size, self.position_cache_size
        )
    }
}
