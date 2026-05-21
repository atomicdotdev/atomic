use redb::{MultimapTable, ReadableMultimapTable};

use crate::pristine::tables::{encode_inode_vertex, encode_vertex, GRAPH, INODE_GRAPH};
use crate::pristine::{PristineResult, WriteTxn};
use crate::types::{EdgeFlags, GraphNode, Inode, NodeId, Position, SerializedGraphEdge};

/// Batched graph writer that keeps GRAPH and INODE_GRAPH open for the full
/// apply pass instead of reopening them per edge.
pub struct GraphWriteBatch<'txn> {
    graph: MultimapTable<'txn, &'static [u8; 24], &'static [u8; 24]>,
    inode_graph: MultimapTable<'txn, &'static [u8; 32], &'static [u8; 24]>,
}

impl<'txn> GraphWriteBatch<'txn> {
    pub fn new(txn: &'txn WriteTxn<'_>) -> PristineResult<Self> {
        Ok(Self {
            graph: txn.txn.open_multimap_table(GRAPH)?,
            inode_graph: txn.txn.open_multimap_table(INODE_GRAPH)?,
        })
    }

    pub fn put_graph(
        &mut self,
        node: GraphNode<NodeId>,
        edge: SerializedGraphEdge,
    ) -> PristineResult<bool> {
        let key = encode_vertex(node.change.get(), node.start.get(), node.end.get());
        let value = serialize_graph_edge(&edge);
        Ok(self.graph.insert(&key, &value)?)
    }

    pub fn has_graph_vertex(&self, node: GraphNode<NodeId>) -> PristineResult<bool> {
        let key = encode_vertex(node.change.get(), node.start.get(), node.end.get());
        Ok(self.graph.get(&key)?.next().is_some())
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
