use std::cell::RefCell;
use std::collections::BTreeSet;

use redb::{MultimapTable, ReadableMultimapTable};

use crate::pristine::span_index::VertexSpanIndex;
use crate::pristine::tables::{
    decode_vertex, encode_inode_vertex, encode_vertex, GRAPH, INODE_GRAPH,
};
use crate::pristine::{
    AdjIterator, FileIndexEntry, FileIndexMetadata, GraphTxnT, PristineError, PristineResult,
    TreeTxnT, WriteTxn,
};
use crate::types::{
    ChangePosition, EdgeFlags, GraphNode, Hash, Inode, NodeId, Position, SerializedGraphEdge,
};

use super::error::LocalApplyError;

/// Cached write-side graph transaction.
///
/// Opens GRAPH and INODE_GRAPH **once** at construction and serves both reads
/// and writes through the same handles. This eliminates the per-operation
/// `open_multimap_table` overhead that dominated the apply phase (a single
/// change with ~1,500 hunks would otherwise open the tables ~18,000 times).
///
/// redb does not allow opening a table as a writable `MultimapTable` and a
/// read-only table simultaneously within the same `WriteTransaction`. So ALL
/// GRAPH/INODE_GRAPH access — reads and writes — must go through these cached
/// handles. The struct therefore implements [`GraphTxnT`] (graph reads come
/// from the same handle, so they observe edges written earlier in the batch)
/// and delegates every non-graph query to the underlying [`WriteTxn`].
pub struct CachedWriteGraphTxn<'txn, 'a> {
    graph: MultimapTable<'txn, &'static [u8; 24], &'static [u8; 24]>,
    inode_graph: MultimapTable<'txn, &'static [u8; 32], &'static [u8; 24]>,
    txn: &'txn WriteTxn<'a>,
    index: RefCell<VertexSpanIndex>,
}

impl<'txn, 'a> CachedWriteGraphTxn<'txn, 'a> {
    pub fn new(txn: &'txn WriteTxn<'a>) -> PristineResult<Self> {
        Ok(Self {
            graph: txn.txn.open_multimap_table(GRAPH)?,
            inode_graph: txn.txn.open_multimap_table(INODE_GRAPH)?,
            txn,
            index: RefCell::new(VertexSpanIndex::default()),
        })
    }

    /// Ensure all of `change_id`'s spans are loaded into the in-memory index.
    ///
    /// Runs a single GRAPH range scan the first time a change is queried; later
    /// calls are no-ops. Writes during the batch keep the set current via
    /// [`VertexSpanIndex::note_write`], so the scan never has to be repeated.
    fn ensure_loaded(&self, change_id: u64) -> PristineResult<()> {
        if self.index.borrow().contains_change(change_id) {
            return Ok(());
        }
        let start_key = encode_vertex(change_id, 0, 0);
        let end_key = encode_vertex(change_id, u64::MAX, u64::MAX);
        let mut set = BTreeSet::new();
        for result in self.graph.range::<&[u8; 24]>(&start_key..=&end_key)? {
            let (key, _) = result?;
            let (v_change, v_start, v_end) = decode_vertex(key.value());
            if v_change != change_id {
                continue;
            }
            set.insert((v_start, v_end));
        }
        self.index.borrow_mut().insert_change(change_id, set);
        Ok(())
    }

    pub fn put_graph(
        &mut self,
        node: GraphNode<NodeId>,
        edge: SerializedGraphEdge,
    ) -> PristineResult<bool> {
        let key = encode_vertex(node.change.get(), node.start.get(), node.end.get());
        let value = serialize_graph_edge(&edge);
        let inserted = self.graph.insert(&key, &value)?;
        self.index
            .borrow_mut()
            .note_write(node.change.get(), node.start.get(), node.end.get());
        Ok(inserted)
    }

    pub fn put_inode_graph(
        &mut self,
        inode: Inode,
        node: GraphNode<NodeId>,
        edge: SerializedGraphEdge,
    ) -> PristineResult<bool> {
        let key = encode_inode_vertex(
            inode.get(),
            node.change.get(),
            node.start.get(),
            node.end.get(),
        );
        let value = serialize_graph_edge(&edge);
        Ok(self.inode_graph.insert(&key, &value)?)
    }

    /// Add a canonical bidirectional GRAPH edge pair (forward + PARENT reverse),
    /// mirroring both rows into INODE_GRAPH when an inode is provided.
    pub fn add_edge_with_reverse(
        &mut self,
        inode: Option<Inode>,
        flag: EdgeFlags,
        source: GraphNode<NodeId>,
        dest: GraphNode<NodeId>,
        introduced_by: NodeId,
    ) -> PristineResult<()> {
        let forward_edge = SerializedGraphEdge::new(flag, dest.start_pos(), introduced_by);
        let reverse_flag = flag | EdgeFlags::PARENT;
        let reverse_edge = SerializedGraphEdge::new(reverse_flag, source.end_pos(), introduced_by);

        self.put_graph(source, forward_edge)?;
        self.put_graph(dest, reverse_edge)?;

        if let Some(inode_val) = inode {
            self.put_inode_graph(inode_val, source, forward_edge)?;
            self.put_inode_graph(inode_val, dest, reverse_edge)?;
        }

        Ok(())
    }

    /// Add a canonical bidirectional GRAPH edge pair, but only the forward
    /// adjacency row to INODE_GRAPH.
    ///
    /// This is useful for bulk import paths that can add terminal inode rows
    /// after building a linear chain. The global GRAPH remains fully
    /// bidirectional; only the file-local secondary index is compacted.
    pub fn add_edge_with_reverse_inode_forward_only(
        &mut self,
        inode: Option<Inode>,
        flag: EdgeFlags,
        source: GraphNode<NodeId>,
        dest: GraphNode<NodeId>,
        introduced_by: NodeId,
    ) -> PristineResult<()> {
        let forward_edge = SerializedGraphEdge::new(flag, dest.start_pos(), introduced_by);
        let reverse_flag = flag | EdgeFlags::PARENT;
        let reverse_edge = SerializedGraphEdge::new(reverse_flag, source.end_pos(), introduced_by);

        self.put_graph(source, forward_edge)?;
        self.put_graph(dest, reverse_edge)?;

        if let Some(inode_val) = inode {
            self.put_inode_graph(inode_val, source, forward_edge)?;
        }

        Ok(())
    }
}

// ── Read access via the cached GRAPH handle ──────────────────────────────
//
// Implementing `GraphTxnT` means the existing generic apply helpers
// (`resolve_context_vertex`, `check_deleted_context`, `collect_zombie_context`,
// …) work unchanged against the cached writer, and every read observes edges
// written earlier in the same batch.

impl<'txn, 'a> GraphTxnT for CachedWriteGraphTxn<'txn, 'a> {
    type Adj = AdjIterator;

    fn get_external(&self, id: NodeId) -> PristineResult<Option<Hash>> {
        self.txn.get_external(id)
    }

    fn get_internal(&self, hash: &Hash) -> PristineResult<Option<NodeId>> {
        self.txn.get_internal(hash)
    }

    fn list_registered_changes(&self) -> PristineResult<Vec<(NodeId, Hash)>> {
        self.txn.list_registered_changes()
    }

    fn iter_adjacent(
        &self,
        node: GraphNode<NodeId>,
        min_flag: EdgeFlags,
        max_flag: EdgeFlags,
    ) -> PristineResult<Self::Adj> {
        let key = encode_vertex(node.change.get(), node.start.get(), node.end.get());
        let mut edges = Vec::new();
        for result in self.graph.get(&key)? {
            let value = result?;
            let edge = deserialize_graph_edge(value.value());
            let flag = edge.flag();
            if flag >= min_flag && flag <= max_flag {
                edges.push(edge);
            }
        }
        Ok(AdjIterator::new(edges))
    }

    fn find_block(&self, pos: Position<NodeId>) -> PristineResult<GraphNode<NodeId>> {
        if pos.change.is_root() {
            return Ok(GraphNode::ROOT);
        }

        let change_id = pos.change.get();
        let target_pos = pos.pos.get();
        self.ensure_loaded(change_id)?;

        if let Some((s, e)) = self.index.borrow().find_block(change_id, target_pos) {
            return Ok(GraphNode {
                change: NodeId::new(change_id),
                start: ChangePosition::new(s),
                end: ChangePosition::new(e),
            });
        }

        Err(PristineError::BlockNotFound {
            change: change_id,
            pos: target_pos,
        })
    }

    fn find_block_end(&self, pos: Position<NodeId>) -> PristineResult<GraphNode<NodeId>> {
        if pos.change.is_root() {
            return Ok(GraphNode::ROOT);
        }

        let change_id = pos.change.get();
        let target_pos = pos.pos.get();
        self.ensure_loaded(change_id)?;

        if let Some((s, e)) = self.index.borrow().find_block_end(change_id, target_pos) {
            return Ok(GraphNode {
                change: NodeId::new(change_id),
                start: ChangePosition::new(s),
                end: ChangePosition::new(e),
            });
        }

        Err(PristineError::BlockNotFound {
            change: change_id,
            pos: target_pos,
        })
    }

    fn has_vertex(&self, node: GraphNode<NodeId>) -> PristineResult<bool> {
        let key = encode_vertex(node.change.get(), node.start.get(), node.end.get());
        Ok(self.graph.get(&key)?.next().is_some())
    }

    fn get_node_type(&self, node_id: NodeId) -> PristineResult<Option<u8>> {
        self.txn.get_node_type(node_id)
    }

    fn get_rev_deps(&self, dep_id: NodeId) -> PristineResult<Vec<NodeId>> {
        self.txn.get_rev_deps(dep_id)
    }

    fn get_change_deps(&self, change_id: NodeId) -> PristineResult<Vec<Hash>> {
        self.txn.get_change_deps(change_id)
    }

    fn is_change_deps_indexed(&self, change_id: NodeId) -> PristineResult<bool> {
        self.txn.is_change_deps_indexed(change_id)
    }

    fn get_rev_change_deps(&self, dep_hash: &Hash) -> PristineResult<Vec<NodeId>> {
        self.txn.get_rev_change_deps(dep_hash)
    }

    fn has_change_in_graph(&self, change_id: NodeId) -> PristineResult<bool> {
        let start_key = encode_vertex(change_id.get(), 0, 0);
        let end_key = encode_vertex(change_id.get(), u64::MAX, u64::MAX);
        Ok(self
            .graph
            .range::<&[u8; 24]>(&start_key..=&end_key)?
            .next()
            .is_some())
    }
}

impl<'txn, 'a> TreeTxnT for CachedWriteGraphTxn<'txn, 'a> {
    fn get_inode(&self, path: &str) -> PristineResult<Option<Inode>> {
        self.txn.get_inode(path)
    }

    fn get_directory_flags(&self, inode: Inode) -> PristineResult<Option<u8>> {
        self.txn.get_directory_flags(inode)
    }

    fn get_path(&self, inode: Inode) -> PristineResult<Option<String>> {
        self.txn.get_path(inode)
    }

    fn inode_position(&self, inode: Inode) -> PristineResult<Option<Position<NodeId>>> {
        self.txn.inode_position(inode)
    }

    fn position_inode(&self, pos: Position<NodeId>) -> PristineResult<Option<Inode>> {
        self.txn.position_inode(pos)
    }

    fn iter_tree(
        &self,
    ) -> PristineResult<Box<dyn Iterator<Item = Result<(String, Inode), PristineError>> + '_>> {
        self.txn.iter_tree()
    }

    fn iter_inode_vertices(
        &self,
        inode: Inode,
    ) -> PristineResult<
        Box<
            dyn Iterator<Item = Result<(GraphNode<NodeId>, SerializedGraphEdge), PristineError>>
                + '_,
        >,
    > {
        self.txn.iter_inode_vertices(inode)
    }

    fn get_file_index(&self, path: &str) -> PristineResult<Option<FileIndexMetadata>> {
        self.txn.get_file_index(path)
    }

    fn iter_file_index(&self) -> PristineResult<Vec<FileIndexEntry>> {
        self.txn.iter_file_index()
    }
}

/// Add a bidirectional edge pair through the cached writer, mapping storage
/// errors into the apply error type.
///
/// This is the single edge-writing entry point shared by both insertion and
/// edge-update application.
pub(crate) fn add_edge_with_reverse(
    txn: &mut CachedWriteGraphTxn<'_, '_>,
    inode: Option<Inode>,
    flag: EdgeFlags,
    source: GraphNode<NodeId>,
    dest: GraphNode<NodeId>,
    introduced_by: NodeId,
) -> Result<(), LocalApplyError> {
    txn.add_edge_with_reverse(inode, flag, source, dest, introduced_by)
        .map_err(|e| LocalApplyError::Internal {
            message: format!("Failed to add edge pair: {}", e),
        })
}

#[inline]
fn deserialize_graph_edge(bytes: &[u8; 24]) -> SerializedGraphEdge {
    let flag_and_pos = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
    let change_id = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    let introduced_by = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
    let flag = EdgeFlags::from_bits_truncate((flag_and_pos >> 56) as u8);
    let pos = flag_and_pos & ((1 << 56) - 1);
    let dest = Position::new(NodeId::new(change_id), ChangePosition::new(pos));
    SerializedGraphEdge::new(flag, dest, NodeId::new(introduced_by))
}

#[inline]
fn serialize_graph_edge(edge: &SerializedGraphEdge) -> [u8; 24] {
    let mut bytes = [0u8; 24];
    let dest: Position<NodeId> = edge.dest();
    let flag = edge.flag();
    let introduced_by = edge.introduced_by();

    let flag_and_pos = ((flag.bits() as u64) << 56) | (dest.pos.get() & ((1 << 56) - 1));
    bytes[0..8].copy_from_slice(&flag_and_pos.to_le_bytes());
    bytes[8..16].copy_from_slice(&dest.change.get().to_le_bytes());
    bytes[16..24].copy_from_slice(&introduced_by.get().to_le_bytes());
    bytes
}
