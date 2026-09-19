//! View-visible namespace bindings, independent of file content aliveness.

use crate::output::RetrieveOptions;
use crate::pristine::{GraphTxnT, PristineError};
use crate::types::{ChangePosition, EdgeFlags, GraphNode, NodeId, Position};

/// Find live name vertices attached to an inode under a view's change filter.
///
/// A name resolution or rename can remove a name without deleting the inode's
/// content. Callers can inspect these vertices' bytes to match a filename.
///
/// # Examples
///
/// ```rust,ignore
/// let options = RetrieveOptions::new().with_change_filter(visible_changes);
/// let names = live_inode_names(&txn, inode_position, &options)?;
/// ```
pub fn live_inode_names<T: GraphTxnT>(
    txn: &T,
    inode: Position<NodeId>,
    options: &RetrieveOptions,
) -> Result<Vec<GraphNode<NodeId>>, PristineError> {
    let mut names = Vec::new();
    for edge in txn.get_edges(inode.inode_node())? {
        let flags = edge.flag();
        if !flags.contains(EdgeFlags::FOLDER | EdgeFlags::PARENT)
            || flags.intersects(EdgeFlags::DELETED | EdgeFlags::PSEUDO)
            || !options.passes_filter(edge.introduced_by())
        {
            continue;
        }
        let end = edge.dest();
        if end.change.is_root() || end.pos.get() == 0 {
            continue;
        }
        // The predecessor name ends at this position. find_block_end would
        // prefer the empty inode marker at the same position for a FileAdd.
        let name = txn.find_block(Position::new(
            end.change,
            ChangePosition::new(end.pos.get() - 1),
        ))?;
        let mut linked = false;
        let mut unlinked = false;
        // Namespace edges carry BLOCK|FOLDER together. Inspect those flags
        // directly; the content-oriented typed parent iterator can omit them.
        for parent in txn.get_edges(name)? {
            let flags = parent.flag();
            if flags.contains(EdgeFlags::PARENT | EdgeFlags::FOLDER)
                && !flags.contains(EdgeFlags::PSEUDO)
                && options.passes_filter(parent.introduced_by())
            {
                if flags.contains(EdgeFlags::DELETED) {
                    unlinked = true;
                } else {
                    linked = true;
                }
            }
        }
        if options.passes_filter(name.change) && linked && !unlinked && !names.contains(&name) {
            names.push(name);
        }
    }
    Ok(names)
}
