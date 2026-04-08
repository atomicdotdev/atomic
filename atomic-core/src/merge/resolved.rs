//! Resolved conflict storage for the materialization pipeline.
//!
//! When the semantic merge engine successfully auto-merges a cyclic conflict
//! (an SCC with >1 vertex), the merged content bytes are stored in a
//! [`ResolvedConflicts`] map. The content output stage checks this map
//! before emitting conflict markers — resolved SCCs are written as plain
//! content instead.

use std::collections::{HashMap, HashSet};

use crate::output::alive::VertexId;

/// Merged content for resolved conflict groups.
///
/// Maps the first `VertexId` in each resolved SCC to the merged bytes.
/// Other vertices in the SCC are recorded in a skip set so the content
/// output loop can ignore them.
///
/// # Usage
///
/// ```rust,ignore
/// use atomic_core::merge::ResolvedConflicts;
/// use atomic_core::output::alive::VertexId;
///
/// let mut resolved = ResolvedConflicts::new();
///
/// // After a successful semantic merge of an SCC [V3, V5, V7]:
/// resolved.insert_merged(VertexId::new(3), b"merged line\n".to_vec());
/// resolved.insert_skip(VertexId::new(5));
/// resolved.insert_skip(VertexId::new(7));
///
/// assert_eq!(resolved.resolved_count(), 1);
/// assert!(resolved.get_merged(VertexId::new(3)).is_some());
/// assert!(resolved.should_skip(VertexId::new(5)));
/// ```
#[derive(Debug, Clone)]
pub struct ResolvedConflicts {
    /// VertexId → merged content bytes.
    ///
    /// Keyed on the **first** vertex in each resolved SCC.  During output
    /// the merged bytes are written in place of that vertex's change-store
    /// content, and the remaining vertices in the SCC are skipped.
    merged: HashMap<VertexId, Vec<u8>>,

    /// VertexIds to skip (non-first vertices in resolved SCCs).
    skip: HashSet<VertexId>,
}

impl ResolvedConflicts {
    /// Create an empty resolved-conflicts map.
    pub fn new() -> Self {
        Self {
            merged: HashMap::new(),
            skip: HashSet::new(),
        }
    }

    /// Returns `true` if no conflicts were resolved.
    pub fn is_empty(&self) -> bool {
        self.merged.is_empty()
    }

    /// Record merged content for the lead vertex of a resolved SCC.
    pub fn insert_merged(&mut self, vid: VertexId, content: Vec<u8>) {
        self.merged.insert(vid, content);
    }

    /// Mark a vertex as "skip" — it belongs to a resolved SCC but is not
    /// the lead vertex.
    pub fn insert_skip(&mut self, vid: VertexId) {
        self.skip.insert(vid);
    }

    /// Look up merged content for a vertex.
    ///
    /// Returns `Some(bytes)` if `vid` is the lead vertex of a resolved SCC.
    pub fn get_merged(&self, vid: VertexId) -> Option<&[u8]> {
        self.merged.get(&vid).map(|v| v.as_slice())
    }

    /// Check whether a vertex should be silently skipped during output.
    pub fn should_skip(&self, vid: VertexId) -> bool {
        self.skip.contains(&vid)
    }

    /// How many conflict groups were resolved.
    pub fn resolved_count(&self) -> usize {
        self.merged.len()
    }
}

impl Default for ResolvedConflicts {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_by_default() {
        let r = ResolvedConflicts::new();
        assert!(r.is_empty());
        assert_eq!(r.resolved_count(), 0);
    }

    #[test]
    fn default_is_empty() {
        let r = ResolvedConflicts::default();
        assert!(r.is_empty());
    }

    #[test]
    fn insert_and_retrieve_merged() {
        let mut r = ResolvedConflicts::new();
        let vid = VertexId::new(3);
        r.insert_merged(vid, b"hello world\n".to_vec());

        assert!(!r.is_empty());
        assert_eq!(r.resolved_count(), 1);
        assert_eq!(r.get_merged(vid), Some(b"hello world\n".as_slice()));
    }

    #[test]
    fn get_merged_returns_none_for_unknown() {
        let r = ResolvedConflicts::new();
        assert!(r.get_merged(VertexId::new(42)).is_none());
    }

    #[test]
    fn skip_set() {
        let mut r = ResolvedConflicts::new();
        let v1 = VertexId::new(1);
        let v2 = VertexId::new(2);
        let v3 = VertexId::new(3);

        r.insert_merged(v1, b"merged".to_vec());
        r.insert_skip(v2);
        r.insert_skip(v3);

        assert!(!r.should_skip(v1));
        assert!(r.should_skip(v2));
        assert!(r.should_skip(v3));
    }

    #[test]
    fn multiple_resolved_sccs() {
        let mut r = ResolvedConflicts::new();

        // SCC 1: vertices 2, 4
        r.insert_merged(VertexId::new(2), b"scc1 merged\n".to_vec());
        r.insert_skip(VertexId::new(4));

        // SCC 2: vertices 6, 8, 10
        r.insert_merged(VertexId::new(6), b"scc2 merged\n".to_vec());
        r.insert_skip(VertexId::new(8));
        r.insert_skip(VertexId::new(10));

        assert_eq!(r.resolved_count(), 2);
        assert_eq!(
            r.get_merged(VertexId::new(2)),
            Some(b"scc1 merged\n".as_slice())
        );
        assert_eq!(
            r.get_merged(VertexId::new(6)),
            Some(b"scc2 merged\n".as_slice())
        );
        assert!(r.should_skip(VertexId::new(4)));
        assert!(r.should_skip(VertexId::new(8)));
        assert!(r.should_skip(VertexId::new(10)));
        // Vertices that aren't in either SCC
        assert!(!r.should_skip(VertexId::new(1)));
        assert!(r.get_merged(VertexId::new(1)).is_none());
    }

    #[test]
    fn clone_preserves_data() {
        let mut r = ResolvedConflicts::new();
        r.insert_merged(VertexId::new(1), b"data".to_vec());
        r.insert_skip(VertexId::new(2));

        let r2 = r.clone();
        assert_eq!(r2.resolved_count(), 1);
        assert_eq!(r2.get_merged(VertexId::new(1)), Some(b"data".as_slice()));
        assert!(r2.should_skip(VertexId::new(2)));
    }

    #[test]
    fn debug_format() {
        let r = ResolvedConflicts::new();
        let dbg = format!("{:?}", r);
        assert!(dbg.contains("ResolvedConflicts"));
    }

    #[test]
    fn overwrite_merged_content() {
        let mut r = ResolvedConflicts::new();
        let vid = VertexId::new(5);
        r.insert_merged(vid, b"first".to_vec());
        r.insert_merged(vid, b"second".to_vec());

        assert_eq!(r.resolved_count(), 1);
        assert_eq!(r.get_merged(vid), Some(b"second".as_slice()));
    }

    #[test]
    fn empty_merged_content_allowed() {
        let mut r = ResolvedConflicts::new();
        r.insert_merged(VertexId::new(1), Vec::new());

        assert!(!r.is_empty());
        assert_eq!(r.get_merged(VertexId::new(1)), Some(b"".as_slice()));
    }
}
