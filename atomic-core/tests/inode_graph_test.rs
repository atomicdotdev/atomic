//! Tests for the inode-scoped graph (two-level B-tree) functionality.
//!
//! These tests verify the core infrastructure for the graph scaling optimization
//! that groups edges by file (Inode) for efficient file-local traversal.
//!
//! The two-level B-tree design:
//! 1. GRAPH table: Span → [Edge] - global graph
//! 2. INODE_GRAPH table: (Inode, Span) → [Edge] - file-scoped index
//!
//! This enables O(log N + n) file operations instead of O(n × log N).

use atomic_core::types::{
    ChangePosition, EdgeFlags, GraphNode, Inode, NodeId, Position, SerializedGraphEdge, L64,
};

// Helper Functions

/// Helper to create a test span.
fn make_vertex(change: u64, start: u64, end: u64) -> GraphNode<NodeId> {
    GraphNode {
        change: NodeId::new(change),
        start: ChangePosition::new(start),
        end: ChangePosition::new(end),
    }
}

/// Helper to create a test inode.
fn make_inode(id: u64) -> Inode {
    Inode::new(id)
}

/// Helper to create a test edge.
fn make_edge(flag: EdgeFlags, change: u64, pos: u64, intro: u64) -> SerializedGraphEdge {
    let dest = Position::new(NodeId::new(change), ChangePosition::new(pos));
    SerializedGraphEdge::new(flag, dest, NodeId::new(intro))
}

// InodeVertex Type Tests

/// Composite key for the inode-scoped graph index.
/// This combines an Inode with a Span for the secondary B-tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InodeVertex {
    pub inode: Inode,
    pub span: GraphNode<NodeId>,
}

impl InodeVertex {
    pub fn new(inode: Inode, span: GraphNode<NodeId>) -> Self {
        Self { inode, span }
    }

    /// Create the minimum possible InodeVertex for a given inode.
    /// Used for range queries to find all vertices in a file.
    pub fn min_for_inode(inode: Inode) -> Self {
        Self {
            inode,
            span: GraphNode::ROOT,
        }
    }

    /// Create the maximum possible InodeVertex for a given inode.
    /// Used for range queries to find all vertices in a file.
    pub fn max_for_inode(inode: Inode) -> Self {
        Self {
            inode,
            span: GraphNode::MAX,
        }
    }

    /// Encode as bytes for use as a redb key.
    /// Layout: [inode: 8 bytes][change: 8 bytes][start: 8 bytes][end: 8 bytes]
    pub fn to_bytes(&self) -> [u8; 32] {
        let mut bytes = [0u8; 32];
        bytes[0..8].copy_from_slice(&self.inode.0.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.span.change.0.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.span.start.0.to_le_bytes());
        bytes[24..32].copy_from_slice(&self.span.end.0.to_le_bytes());
        bytes
    }

    /// Decode from bytes.
    pub fn from_bytes(bytes: &[u8; 32]) -> Self {
        let inode = Inode(L64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]));
        let change = NodeId(L64::from_le_bytes([
            bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
        ]));
        let start = ChangePosition(L64::from_le_bytes([
            bytes[16], bytes[17], bytes[18], bytes[19], bytes[20], bytes[21], bytes[22], bytes[23],
        ]));
        let end = ChangePosition(L64::from_le_bytes([
            bytes[24], bytes[25], bytes[26], bytes[27], bytes[28], bytes[29], bytes[30], bytes[31],
        ]));
        Self {
            inode,
            span: GraphNode { change, start, end },
        }
    }
}

#[test]
fn test_inode_vertex_creation() {
    let inode = make_inode(42);
    let v = make_vertex(1, 0, 10);

    let iv = InodeVertex::new(inode, v);
    assert_eq!(iv.inode, inode);
    assert_eq!(iv.span, v);
}

#[test]
fn test_inode_vertex_ordering() {
    // Vertices in the same inode should be ordered by span
    let inode = make_inode(100);
    let iv1 = InodeVertex::new(inode, make_vertex(1, 0, 10));
    let iv2 = InodeVertex::new(inode, make_vertex(2, 0, 10));
    assert!(iv1 < iv2, "Same inode, lower change should come first");

    // Different inodes should be ordered by inode first
    let iv3 = InodeVertex::new(make_inode(50), make_vertex(999, 0, 10));
    let iv4 = InodeVertex::new(make_inode(100), make_vertex(1, 0, 10));
    assert!(
        iv3 < iv4,
        "Lower inode should come first regardless of span"
    );
}

#[test]
fn test_inode_vertex_min_max() {
    let inode = make_inode(42);
    let min = InodeVertex::min_for_inode(inode);
    let max = InodeVertex::max_for_inode(inode);

    assert!(min < max, "Min should be less than max");
    assert_eq!(min.inode, inode);
    assert_eq!(max.inode, inode);

    // Any span in this inode should be between min and max
    let mid = InodeVertex::new(inode, make_vertex(500, 100, 200));
    assert!(min <= mid && mid <= max, "Mid should be in range");
}

#[test]
fn test_inode_vertex_bytes_roundtrip() {
    let inode = make_inode(42);
    let v = make_vertex(123, 456, 789);
    let iv = InodeVertex::new(inode, v);

    let bytes = iv.to_bytes();
    let recovered = InodeVertex::from_bytes(&bytes);

    assert_eq!(iv, recovered);
}

#[test]
fn test_inode_vertex_bytes_ordering_preserved() {
    // The byte encoding must preserve ordering for B-tree range queries to work
    let iv1 = InodeVertex::new(make_inode(1), make_vertex(1, 0, 10));
    let iv2 = InodeVertex::new(make_inode(1), make_vertex(2, 0, 10));
    let iv3 = InodeVertex::new(make_inode(2), make_vertex(1, 0, 10));

    let bytes1 = iv1.to_bytes();
    let bytes2 = iv2.to_bytes();
    let bytes3 = iv3.to_bytes();

    // Lexicographic comparison of bytes should match Ord comparison
    assert!(
        bytes1 < bytes2,
        "Same inode, lower change: bytes should be less"
    );
    assert!(bytes2 < bytes3, "Lower inode should have lower bytes");
}

// Edge Tests (relevant to graph operations)

#[test]
fn test_edge_creation() {
    let edge = make_edge(EdgeFlags::BLOCK, 2, 5, 1);

    assert_eq!(edge.flag(), EdgeFlags::BLOCK);
    assert_eq!(edge.dest().change, NodeId::new(2));
    assert_eq!(edge.dest().pos, ChangePosition::new(5));
    assert_eq!(edge.introduced_by(), NodeId::new(1));
}

#[test]
fn test_edge_flags_combinations() {
    // Block edge
    let block = make_edge(EdgeFlags::BLOCK, 1, 0, 1);
    assert!(block.flag().is_block());
    assert!(!block.flag().is_parent());
    assert!(!block.flag().is_deleted());

    // Parent edge (reverse direction)
    let parent = make_edge(EdgeFlags::BLOCK | EdgeFlags::PARENT, 1, 0, 1);
    assert!(parent.flag().is_block());
    assert!(parent.flag().is_parent());

    // Deleted edge
    let deleted = make_edge(EdgeFlags::DELETED | EdgeFlags::BLOCK, 1, 0, 1);
    assert!(deleted.flag().is_deleted());
    assert!(!deleted.flag().is_alive());

    // Folder edge
    let folder = make_edge(EdgeFlags::FOLDER, 1, 0, 1);
    assert!(folder.flag().is_folder());
}

#[test]
fn test_edge_roundtrip() {
    let original = make_edge(EdgeFlags::BLOCK | EdgeFlags::PARENT, 42, 100, 7);
    let edge = original.to_edge();

    assert_eq!(edge.flag, EdgeFlags::BLOCK | EdgeFlags::PARENT);
    assert_eq!(edge.dest.change, NodeId::new(42));
    assert_eq!(edge.dest.pos, ChangePosition::new(100));
    assert_eq!(edge.introduced_by, NodeId::new(7));

    // Convert back
    let serialized = SerializedGraphEdge::from(edge);
    assert_eq!(serialized.flag(), original.flag());
    assert_eq!(serialized.dest(), original.dest());
    assert_eq!(serialized.introduced_by(), original.introduced_by());
}

// Span Key Encoding Tests

/// Encode a Span as 24 bytes for use as a redb key.
fn vertex_to_bytes(v: &GraphNode<NodeId>) -> [u8; 24] {
    let mut bytes = [0u8; 24];
    bytes[0..8].copy_from_slice(&v.change.0.to_le_bytes());
    bytes[8..16].copy_from_slice(&v.start.0.to_le_bytes());
    bytes[16..24].copy_from_slice(&v.end.0.to_le_bytes());
    bytes
}

/// Decode a Span from 24 bytes.
fn vertex_from_bytes(bytes: &[u8; 24]) -> GraphNode<NodeId> {
    let change = NodeId(L64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]));
    let start = ChangePosition(L64::from_le_bytes([
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    ]));
    let end = ChangePosition(L64::from_le_bytes([
        bytes[16], bytes[17], bytes[18], bytes[19], bytes[20], bytes[21], bytes[22], bytes[23],
    ]));
    GraphNode { change, start, end }
}

#[test]
fn test_vertex_bytes_roundtrip() {
    let v = make_vertex(123, 456, 789);
    let bytes = vertex_to_bytes(&v);
    let recovered = vertex_from_bytes(&bytes);

    assert_eq!(v, recovered);
}

#[test]
fn test_vertex_bytes_ordering_preserved() {
    let v1 = make_vertex(1, 0, 10);
    let v2 = make_vertex(1, 5, 10);
    let v3 = make_vertex(2, 0, 10);

    let bytes1 = vertex_to_bytes(&v1);
    let bytes2 = vertex_to_bytes(&v2);
    let bytes3 = vertex_to_bytes(&v3);

    // Lexicographic comparison should match Ord
    assert!(bytes1 < bytes2, "Same change, lower start should be less");
    assert!(bytes2 < bytes3, "Lower change should be less");
}

#[test]
fn test_root_vertex() {
    let root = GraphNode::<NodeId>::ROOT;
    assert!(root.is_root());
    assert!(root.is_empty());
    assert_eq!(root.len(), 0);

    let bytes = vertex_to_bytes(&root);
    // Root should be all zeros
    assert_eq!(bytes, [0u8; 24]);
}

// Two-Level B-Tree Conceptual Tests

/// These tests verify the conceptual properties that make the optimization work.
/// Actual redb integration tests will be in pristine module.

#[test]
fn test_btree_range_query_concept() {
    // The key insight: for a given inode, all its vertices are contiguous
    // in the INODE_GRAPH B-tree because InodeVertex sorts by (inode, span).

    let inode = make_inode(42);

    // Generate vertices for this inode
    let vertices: Vec<InodeVertex> = (1..=10)
        .map(|i| InodeVertex::new(inode, make_vertex(i, 0, i * 10)))
        .collect();

    // All should be >= min and <= max
    let min = InodeVertex::min_for_inode(inode);
    let max = InodeVertex::max_for_inode(inode);

    for v in &vertices {
        assert!(v >= &min, "Span should be >= min");
        assert!(v <= &max, "Span should be <= max");
    }

    // Vertices from other inodes should NOT be in range
    let other_inode = make_inode(43);
    let other = InodeVertex::new(other_inode, make_vertex(1, 0, 10));

    // other_inode (43) > inode (42), so other should be > max
    assert!(other > max, "Span from higher inode should be > max");
}

#[test]
fn test_inode_isolation() {
    // Changes to one file's graph should not require scanning another file's graph.
    // This is achieved by having separate key prefixes in INODE_GRAPH.

    let file_a = make_inode(1);
    let file_b = make_inode(2);

    let iv_a = InodeVertex::new(file_a, make_vertex(100, 0, 10));
    let iv_b = InodeVertex::new(file_b, make_vertex(1, 0, 10));

    // Even though file_a has change 100 and file_b has change 1,
    // file_a's vertices come first because inode 1 < inode 2
    assert!(iv_a < iv_b);

    // Range query for file_a won't touch file_b's entries
    let min_a = InodeVertex::min_for_inode(file_a);
    let max_a = InodeVertex::max_for_inode(file_a);

    assert!(iv_a >= min_a && iv_a <= max_a);
    assert!(!(iv_b >= min_a && iv_b <= max_a)); // file_b NOT in file_a's range
}

// Migration Stats (for populating inode_graph from existing graph)

#[derive(Debug, Default, Clone)]
pub struct MigrationStats {
    pub inodes_processed: usize,
    pub vertices_migrated: usize,
    pub edges_added: usize,
    pub errors: usize,
}

impl MigrationStats {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn merge(&mut self, other: &Self) {
        self.inodes_processed += other.inodes_processed;
        self.vertices_migrated += other.vertices_migrated;
        self.edges_added += other.edges_added;
        self.errors += other.errors;
    }
}

#[test]
fn test_migration_stats() {
    let stats = MigrationStats::new();
    assert_eq!(stats.inodes_processed, 0);
    assert_eq!(stats.vertices_migrated, 0);
    assert_eq!(stats.edges_added, 0);
    assert_eq!(stats.errors, 0);

    let mut stats1 = MigrationStats {
        inodes_processed: 1,
        vertices_migrated: 10,
        edges_added: 20,
        errors: 0,
    };

    let stats2 = MigrationStats {
        inodes_processed: 2,
        vertices_migrated: 15,
        edges_added: 30,
        errors: 1,
    };

    stats1.merge(&stats2);
    assert_eq!(stats1.inodes_processed, 3);
    assert_eq!(stats1.vertices_migrated, 25);
    assert_eq!(stats1.edges_added, 50);
    assert_eq!(stats1.errors, 1);
}

// Migration Config

#[derive(Debug, Clone)]
pub struct MigrationConfig {
    /// Number of vertices to process per transaction
    pub batch_size: usize,
    /// Skip inodes that already have entries in inode_graph
    pub skip_populated: bool,
    /// Continue processing if individual span migration fails
    pub continue_on_error: bool,
}

impl Default for MigrationConfig {
    fn default() -> Self {
        Self {
            batch_size: 1000,
            skip_populated: true,
            continue_on_error: true,
        }
    }
}

impl MigrationConfig {
    pub fn fast() -> Self {
        Self {
            batch_size: 10000,
            skip_populated: true,
            continue_on_error: true,
        }
    }

    pub fn strict() -> Self {
        Self {
            batch_size: 1000,
            skip_populated: false,
            continue_on_error: false,
        }
    }
}

#[test]
fn test_migration_config_defaults() {
    let config = MigrationConfig::default();
    assert_eq!(config.batch_size, 1000);
    assert!(config.skip_populated);
    assert!(config.continue_on_error);

    let fast = MigrationConfig::fast();
    assert_eq!(fast.batch_size, 10000);

    let strict = MigrationConfig::strict();
    assert!(!strict.continue_on_error);
}

// Populate Inode Request

#[derive(Debug, Clone)]
pub struct PopulateInodeRequest {
    pub inode: Inode,
    pub start_position: Option<Position<NodeId>>,
}

impl PopulateInodeRequest {
    pub fn new(inode: Inode) -> Self {
        Self {
            inode,
            start_position: None,
        }
    }

    pub fn from_position(inode: Inode, pos: Position<NodeId>) -> Self {
        Self {
            inode,
            start_position: Some(pos),
        }
    }
}

#[test]
fn test_populate_inode_request() {
    let inode = make_inode(42);

    // Basic request
    let req = PopulateInodeRequest::new(inode);
    assert_eq!(req.inode, inode);
    assert!(req.start_position.is_none());

    // Request with position
    let pos = Position::new(NodeId::new(1), ChangePosition::new(5));
    let req = PopulateInodeRequest::from_position(inode, pos);
    assert_eq!(req.inode, inode);
    assert_eq!(req.start_position, Some(pos));
}

// Integration-style test: verify ordering with many entries

#[test]
fn test_inode_vertex_btree_ordering_property() {
    // Simulate inserting edges in random order for different inodes
    // Verify that the ordering is correct for range queries

    let test_data: Vec<(u64, u64, u64, u64)> = vec![
        (3, 5, 0, 10),  // inode 3, change 5
        (1, 2, 0, 10),  // inode 1, change 2
        (3, 1, 0, 10),  // inode 3, change 1
        (2, 10, 0, 10), // inode 2, change 10
        (1, 1, 0, 10),  // inode 1, change 1
        (2, 5, 0, 10),  // inode 2, change 5
    ];

    // Convert to InodeVertex and sort (simulating B-tree ordering)
    let mut entries: Vec<InodeVertex> = test_data
        .iter()
        .map(|(inode_id, change, start, end)| {
            InodeVertex::new(make_inode(*inode_id), make_vertex(*change, *start, *end))
        })
        .collect();

    entries.sort();

    // Expected order: inode 1 (changes 1, 2), inode 2 (changes 5, 10), inode 3 (changes 1, 5)
    let expected_order: Vec<(u64, u64)> = vec![(1, 1), (1, 2), (2, 5), (2, 10), (3, 1), (3, 5)];

    for (i, entry) in entries.iter().enumerate() {
        let (expected_inode, expected_change) = expected_order[i];
        assert_eq!(
            entry.inode.get(),
            expected_inode,
            "Entry {} should have inode {}",
            i,
            expected_inode
        );
        assert_eq!(
            entry.span.change.get(),
            expected_change,
            "Entry {} should have change {}",
            i,
            expected_change
        );
    }
}

#[test]
fn test_range_query_simulation() {
    // Simulate a range query for a specific inode

    let all_entries: Vec<InodeVertex> = vec![
        InodeVertex::new(make_inode(1), make_vertex(1, 0, 10)),
        InodeVertex::new(make_inode(1), make_vertex(2, 0, 20)),
        InodeVertex::new(make_inode(2), make_vertex(1, 0, 10)),
        InodeVertex::new(make_inode(2), make_vertex(2, 0, 20)),
        InodeVertex::new(make_inode(2), make_vertex(3, 0, 30)),
        InodeVertex::new(make_inode(3), make_vertex(1, 0, 10)),
    ];

    // Query for inode 2
    let target_inode = make_inode(2);
    let min = InodeVertex::min_for_inode(target_inode);
    let max = InodeVertex::max_for_inode(target_inode);

    // Filter (simulating B-tree range scan)
    let results: Vec<_> = all_entries
        .iter()
        .filter(|e| *e >= &min && *e <= &max)
        .collect();

    assert_eq!(results.len(), 3, "Should find 3 entries for inode 2");
    for entry in &results {
        assert_eq!(entry.inode, target_inode);
    }
}
