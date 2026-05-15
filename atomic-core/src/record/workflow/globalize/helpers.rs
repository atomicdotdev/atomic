use super::*;

// HELPER FUNCTIONS

/// Convert a Position<NodeId> to Position<Option<Hash>>.
///
/// This converts from internal representation to the format used in
/// serializable hunks.
///
/// # Hash Semantics
///
/// - `None` means "this change" (self-reference) - the actual hash will be
///   filled in during serialization
/// - `Some(Hash::NONE)` means the ROOT position - the special virtual root
///   span that all top-level files reference
/// - `Some(hash)` means a reference to a specific existing change
///
/// # Arguments
///
/// * `pos` - The position with internal NodeId
///
/// # Returns
///
/// A position with Option<Hash>.
#[inline]
pub(crate) fn position_to_option_hash(pos: Position<NodeId>) -> Position<Option<Hash>> {
    Position {
        change: if pos.change.is_root() {
            // ROOT is the special virtual root span - use Hash::NONE
            Some(Hash::NONE)
        } else {
            // Non-root positions that we're creating are self-references
            // The actual hash will be filled in during serialization
            None
        },
        pos: pos.pos,
    }
}

/// Convert a Position<NodeId> to Position<Option<Hash>>, resolving external change hashes.
///
/// Unlike `position_to_option_hash`, this function looks up the actual hash for
/// external change references using the transaction. This is necessary when
/// creating predecessors or successors references that point to vertices in
/// previously applied changes.
///
/// # Hash Semantics
///
/// - `None` means "this change" (self-reference) - used for positions within the current change
/// - `Some(Hash::NONE)` means the ROOT span
/// - `Some(hash)` means a specific existing change
///
/// # Arguments
///
/// * `txn` - Transaction for looking up external hashes
/// * `pos` - The position with internal NodeId
/// * `current_change_id` - The NodeId of the change being created (if known), or None
///
/// # Returns
///
/// A position with Option<Hash> where external changes have their hashes resolved.
pub(crate) fn position_to_option_hash_resolved<T: GraphTxnT>(
    txn: &T,
    pos: Position<NodeId>,
    current_change_id: Option<NodeId>,
) -> Position<Option<Hash>> {
    Position {
        change: if pos.change.is_root() {
            // ROOT is the special virtual root span - use Hash::NONE
            Some(Hash::NONE)
        } else if current_change_id == Some(pos.change) {
            // Self-reference to the change being created
            None
        } else {
            // External change - look up its hash
            match txn.get_external(pos.change) {
                Ok(Some(hash)) => Some(hash),
                _ => {
                    // If we can't find the hash, treat as self-reference
                    // This shouldn't happen in normal operation
                    None
                }
            }
        },
        pos: pos.pos,
    }
}

/// Convert a GraphNode<NodeId> to GraphNode<Option<Hash>>.
///
/// Similar to position_to_option_hash, but for vertices.
///
/// # Hash Semantics
///
/// - `None` means "this change" (self-reference)
/// - `Some(Hash::NONE)` means the ROOT span
/// - `Some(hash)` means a specific existing change
#[inline]
pub(crate) fn vertex_to_option_hash(node: GraphNode<NodeId>) -> GraphNode<Option<Hash>> {
    GraphNode {
        change: if node.change.is_root() {
            // ROOT span - use Hash::NONE
            Some(Hash::NONE)
        } else {
            // Self-reference - hash filled in during serialization
            None
        },
        start: node.start,
        end: node.end,
    }
}

/// Convert a NodeId to Option<Hash>.
///
/// # Hash Semantics
///
/// - Returns `Some(Hash::NONE)` for the ROOT node
/// - Returns `None` for non-root nodes (self-references, hash filled in during serialization)
#[inline]
pub(crate) fn node_id_to_option_hash(node_id: NodeId) -> Option<Hash> {
    if node_id.is_root() {
        // ROOT node - use Hash::NONE
        Some(Hash::NONE)
    } else {
        // Self-reference - hash will be filled in during serialization
        None
    }
}

/// Extract the filename from a path.
///
/// # Arguments
///
/// * `path` - The full file path
///
/// # Returns
///
/// The filename portion of the path, or the full path if no separator found.
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::globalize::extract_filename;
///
/// assert_eq!(extract_filename("src/lib/mod.rs"), "mod.rs");
/// assert_eq!(extract_filename("Cargo.toml"), "Cargo.toml");
/// ```
#[must_use]
pub fn extract_filename(path: &str) -> &str {
    std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path)
}

/// Split a byte slice into lines, preserving the trailing `\n` on each.
///
/// The final slice may not end with `\n` if the input doesn't.  An empty
/// input produces an empty `Vec`.
///
/// This is used by the FileAdd path to create one graph vertex per line,
/// giving the graph line-level granularity from the very first record.
#[must_use]
pub fn split_into_lines(content: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    for (i, &b) in content.iter().enumerate() {
        if b == b'\n' {
            lines.push(&content[start..=i]);
            start = i + 1;
        }
    }
    if start < content.len() {
        lines.push(&content[start..]);
    }
    lines
}

/// Extract the parent directory from a path.
///
/// # Arguments
///
/// * `path` - The full file path
///
/// # Returns
///
/// The parent directory, or empty string for root-level files.
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::globalize::extract_parent;
///
/// assert_eq!(extract_parent("src/lib/mod.rs"), "src/lib");
/// assert_eq!(extract_parent("Cargo.toml"), "");
/// ```
#[must_use]
pub fn extract_parent(path: &str) -> &str {
    std::path::Path::new(path)
        .parent()
        .and_then(|p| p.to_str())
        .unwrap_or("")
}
