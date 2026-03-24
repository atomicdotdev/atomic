use super::*;

// GLOBALIZATION RESULT

/// Result of globalizing a single file.
///
/// Contains the generated hunks and metadata about the globalization process.
#[derive(Debug, Clone)]
pub struct GlobalizedFile {
    /// The file path.
    path: String,

    /// The generated hunks.
    hunks: Vec<GraphOp<Option<Hash>>>,

    /// Number of content bytes added.
    bytes_added: u64,

    /// Number of dependencies tracked.
    dependency_count: usize,

    /// Enriched CRDT file operations with graph positions.
    ///
    /// After globalization, this contains the FileOps with `content_range`
    /// fields populated, linking CRDT branches to graph vertex positions.
    file_ops: Option<crate::change::FileOps>,
}

impl GlobalizedFile {
    /// Create a new globalized file result.
    #[must_use]
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            hunks: Vec::new(),
            bytes_added: 0,
            dependency_count: 0,
            file_ops: None,
        }
    }

    /// Add a graph_op to the result.
    pub fn add_hunk(&mut self, graph_op: GraphOp<Option<Hash>>) {
        self.hunks.push(graph_op);
    }

    /// Set bytes added.
    pub fn set_bytes_added(&mut self, bytes: u64) {
        self.bytes_added = bytes;
    }

    /// Set dependency count.
    pub fn set_dependency_count(&mut self, count: usize) {
        self.dependency_count = count;
    }

    /// Get the file path.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Get the hunks.
    #[must_use]
    pub fn hunks(&self) -> &[GraphOp<Option<Hash>>] {
        &self.hunks
    }

    /// Get bytes added.
    #[must_use]
    pub fn bytes_added(&self) -> u64 {
        self.bytes_added
    }

    /// Get dependency count.
    #[must_use]
    pub fn dependency_count(&self) -> usize {
        self.dependency_count
    }

    /// Check if empty (no hunks).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hunks.is_empty()
    }

    /// Get number of hunks.
    #[must_use]
    pub fn hunk_count(&self) -> usize {
        self.hunks.len()
    }

    /// Take ownership of the hunks.
    #[must_use]
    pub fn into_hunks(self) -> Vec<GraphOp<Option<Hash>>> {
        self.hunks
    }

    /// Set the enriched CRDT file operations.
    ///
    /// This is called during globalization after the FileOps have been
    /// enriched with graph position information.
    pub fn set_file_ops(&mut self, file_ops: crate::change::FileOps) {
        self.file_ops = Some(file_ops);
    }

    /// Get the enriched CRDT file operations.
    #[must_use]
    pub fn file_ops(&self) -> Option<&crate::change::FileOps> {
        self.file_ops.as_ref()
    }

    /// Take ownership of the enriched CRDT file operations.
    #[must_use]
    pub fn into_file_ops(self) -> Option<crate::change::FileOps> {
        self.file_ops
    }

    /// Take ownership of both hunks and file_ops.
    #[must_use]
    pub fn into_parts(self) -> (Vec<GraphOp<Option<Hash>>>, Option<crate::change::FileOps>) {
        (self.hunks, self.file_ops)
    }

    /// Check if this file has enriched CRDT operations.
    #[must_use]
    pub fn has_file_ops(&self) -> bool {
        self.file_ops.is_some()
    }
}
