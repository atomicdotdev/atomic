//! Position probes over INODE_GRAPH rows, shared by every transaction type.
//!
//! INODE_GRAPH keys are `(inode, change, start, end)`, so the vertices one
//! file holds in one change sort by position, and spans of one file within
//! one change do not overlap. Both probes therefore read backwards from the
//! greatest start at or before the target and decide on the first non-empty
//! row instead of scanning the change from offset zero.

use redb::ReadableMultimapTable;

use crate::pristine::error::PristineError;
use crate::pristine::tables::{decode_inode_vertex, encode_inode_vertex};
use crate::types::{ChangePosition, GraphNode, Inode, NodeId, Position};

type InodeKey = &'static [u8; 32];
type InodeValue = &'static [u8; 24];

/// The non-empty vertex of `inode` containing `pos` (a vertex starting
/// exactly at `pos` first), else the empty marker at exactly `pos`.
pub(crate) fn find_block_in_inode<T>(
    table: &T,
    inode: Inode,
    pos: Position<NodeId>,
) -> Result<Option<GraphNode<NodeId>>, PristineError>
where
    T: ReadableMultimapTable<InodeKey, InodeValue>,
{
    let (inode_id, change_id, target) = (inode.get(), pos.change.get(), pos.pos.get());
    let first = encode_inode_vertex(inode_id, change_id, 0, 0);
    let last = encode_inode_vertex(inode_id, change_id, target, u64::MAX);
    for row in table.range::<&[u8; 32]>(&first..=&last)?.rev() {
        let (key, _) = row?;
        let (_, _, start, end) = decode_inode_vertex(key.value());
        if start == end {
            if start == target {
                return Ok(Some(vertex(change_id, start, end)));
            }
            continue;
        }
        return Ok((target < end).then(|| vertex(change_id, start, end)));
    }
    Ok(None)
}

/// The vertex of `inode` ending at `pos`: the empty marker at exactly `pos`
/// first, else the non-empty vertex ending at `pos`, else the one
/// containing it.
pub(crate) fn find_block_end_in_inode<T>(
    table: &T,
    inode: Inode,
    pos: Position<NodeId>,
) -> Result<Option<GraphNode<NodeId>>, PristineError>
where
    T: ReadableMultimapTable<InodeKey, InodeValue>,
{
    let (inode_id, change_id, target) = (inode.get(), pos.change.get(), pos.pos.get());
    let marker = encode_inode_vertex(inode_id, change_id, target, target);
    if table.get(&marker)?.next().transpose()?.is_some() {
        return Ok(Some(vertex(change_id, target, target)));
    }
    let first = encode_inode_vertex(inode_id, change_id, 0, 0);
    let last = encode_inode_vertex(inode_id, change_id, target, u64::MAX);
    // A span starting at `target` only wins when no span ends there.
    let mut starting_here = None;
    for row in table.range::<&[u8; 32]>(&first..=&last)?.rev() {
        let (key, _) = row?;
        let (_, _, start, end) = decode_inode_vertex(key.value());
        if start == end {
            continue;
        }
        if end == target {
            return Ok(Some(vertex(change_id, start, end)));
        }
        if start == target {
            starting_here = Some(vertex(change_id, start, end));
            continue;
        }
        if target < end {
            return Ok(Some(vertex(change_id, start, end)));
        }
        break;
    }
    Ok(starting_here)
}

fn vertex(change: u64, start: u64, end: u64) -> GraphNode<NodeId> {
    GraphNode::new(
        NodeId::new(change),
        ChangePosition::new(start),
        ChangePosition::new(end),
    )
}
