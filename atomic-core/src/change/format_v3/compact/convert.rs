//! Compact ↔ full conversion for [`CompactGraphOp`] / [`GraphOp`].
//!
//! These are the two largest methods on [`Compactor`] — they recursively
//! compact or expand every nested position, node, and hash in a graph
//! operation. They live in a separate file to keep `compactor.rs` under
//! the 500-line limit.

use super::super::error::FormatResult;
use super::compactor::Compactor;
use super::graph_op::CompactGraphOp;

use crate::change::graph_op::GraphOp;
use crate::Hash;

impl<'t> Compactor<'t> {
    /// Convert a `GraphOp<Option<Hash>>` to a [`CompactGraphOp`].
    ///
    /// This is the main entry point for compacting a full graph operation
    /// before serialization. It recursively compacts all nested positions,
    /// nodes, and hashes.
    ///
    /// # Errors
    ///
    /// Returns an error if any hash referenced by the graph operation
    /// is not found in the dedup table.
    pub fn compact_graph_op(&self, op: &GraphOp<Option<Hash>>) -> FormatResult<CompactGraphOp> {
        match op {
            GraphOp::FileAdd {
                add_name,
                add_inode,
                contents,
                path,
                encoding,
            } => Ok(CompactGraphOp::FileAdd {
                add_name: self.compact_insertion(add_name)?,
                add_inode: self.compact_insertion(add_inode)?,
                contents: contents
                    .as_ref()
                    .map(|c| self.compact_insertion(c))
                    .transpose()?,
                path: path.clone(),
                encoding: *encoding,
            }),

            GraphOp::DirAdd {
                add_name,
                add_inode,
                path,
            } => Ok(CompactGraphOp::DirAdd {
                add_name: self.compact_insertion(add_name)?,
                add_inode: self.compact_insertion(add_inode)?,
                path: path.clone(),
            }),

            GraphOp::DirDel { del, path } => Ok(CompactGraphOp::DirDel {
                del: self.compact_edge_update(del)?,
                path: path.clone(),
            }),

            GraphOp::DirUndel { undel, path } => Ok(CompactGraphOp::DirUndel {
                undel: self.compact_edge_update(undel)?,
                path: path.clone(),
            }),

            GraphOp::FileDel {
                del,
                contents,
                path,
                encoding,
            } => Ok(CompactGraphOp::FileDel {
                del: self.compact_edge_update(del)?,
                contents: contents
                    .as_ref()
                    .map(|c| self.compact_edge_update(c))
                    .transpose()?,
                path: path.clone(),
                encoding: *encoding,
            }),

            GraphOp::FileUndel {
                undel,
                contents,
                path,
                encoding,
            } => Ok(CompactGraphOp::FileUndel {
                undel: self.compact_edge_update(undel)?,
                contents: contents
                    .as_ref()
                    .map(|c| self.compact_edge_update(c))
                    .transpose()?,
                path: path.clone(),
                encoding: *encoding,
            }),

            GraphOp::FileMove { del, add, path } => Ok(CompactGraphOp::FileMove {
                del: self.compact_edge_update(del)?,
                add: self.compact_insertion(add)?,
                path: path.clone(),
            }),

            GraphOp::Edit {
                change,
                local,
                encoding,
            } => Ok(CompactGraphOp::Edit {
                change: self.compact_atom(change)?,
                local: local.clone(),
                encoding: *encoding,
            }),

            GraphOp::Replacement {
                change,
                replacement,
                local,
                encoding,
            } => Ok(CompactGraphOp::Replacement {
                change: self.compact_edge_update(change)?,
                replacement: self.compact_insertion(replacement)?,
                local: local.clone(),
                encoding: *encoding,
            }),

            GraphOp::SolveNameConflict { name, path } => Ok(CompactGraphOp::SolveNameConflict {
                name: self.compact_edge_update(name)?,
                path: path.clone(),
            }),

            GraphOp::UnsolveNameConflict { name, path } => {
                Ok(CompactGraphOp::UnsolveNameConflict {
                    name: self.compact_edge_update(name)?,
                    path: path.clone(),
                })
            }

            GraphOp::SolveOrderConflict { change, local } => {
                Ok(CompactGraphOp::SolveOrderConflict {
                    change: self.compact_edge_update(change)?,
                    local: local.clone(),
                })
            }

            GraphOp::UnsolveOrderConflict { change, local } => {
                Ok(CompactGraphOp::UnsolveOrderConflict {
                    change: self.compact_edge_update(change)?,
                    local: local.clone(),
                })
            }

            GraphOp::ResurrectZombies {
                change,
                local,
                encoding,
            } => Ok(CompactGraphOp::ResurrectZombies {
                change: self.compact_edge_update(change)?,
                local: local.clone(),
                encoding: *encoding,
            }),

            GraphOp::AddRoot { name, inode } => Ok(CompactGraphOp::AddRoot {
                name: self.compact_insertion(name)?,
                inode: self.compact_insertion(inode)?,
            }),

            GraphOp::DelRoot { name, inode } => Ok(CompactGraphOp::DelRoot {
                name: self.compact_edge_update(name)?,
                inode: self.compact_edge_update(inode)?,
            }),
        }
    }

    /// Convert a [`CompactGraphOp`] to a `GraphOp<Option<Hash>>`.
    ///
    /// This is the main entry point for expanding a compact graph operation
    /// after deserialization. It recursively expands all nested positions,
    /// nodes, and hash indices.
    ///
    /// # Errors
    ///
    /// Returns an error if any hash index in the compact operation
    /// is out of bounds for the dedup table.
    pub fn expand_graph_op(&self, op: &CompactGraphOp) -> FormatResult<GraphOp<Option<Hash>>> {
        match op {
            CompactGraphOp::FileAdd {
                add_name,
                add_inode,
                contents,
                path,
                encoding,
            } => Ok(GraphOp::FileAdd {
                add_name: self.expand_insertion(add_name)?,
                add_inode: self.expand_insertion(add_inode)?,
                contents: contents
                    .as_ref()
                    .map(|c| self.expand_insertion(c))
                    .transpose()?,
                path: path.clone(),
                encoding: *encoding,
            }),

            CompactGraphOp::DirAdd {
                add_name,
                add_inode,
                path,
            } => Ok(GraphOp::DirAdd {
                add_name: self.expand_insertion(add_name)?,
                add_inode: self.expand_insertion(add_inode)?,
                path: path.clone(),
            }),

            CompactGraphOp::DirDel { del, path } => Ok(GraphOp::DirDel {
                del: self.expand_edge_update(del)?,
                path: path.clone(),
            }),

            CompactGraphOp::DirUndel { undel, path } => Ok(GraphOp::DirUndel {
                undel: self.expand_edge_update(undel)?,
                path: path.clone(),
            }),

            CompactGraphOp::FileDel {
                del,
                contents,
                path,
                encoding,
            } => Ok(GraphOp::FileDel {
                del: self.expand_edge_update(del)?,
                contents: contents
                    .as_ref()
                    .map(|c| self.expand_edge_update(c))
                    .transpose()?,
                path: path.clone(),
                encoding: *encoding,
            }),

            CompactGraphOp::FileUndel {
                undel,
                contents,
                path,
                encoding,
            } => Ok(GraphOp::FileUndel {
                undel: self.expand_edge_update(undel)?,
                contents: contents
                    .as_ref()
                    .map(|c| self.expand_edge_update(c))
                    .transpose()?,
                path: path.clone(),
                encoding: *encoding,
            }),

            CompactGraphOp::FileMove { del, add, path } => Ok(GraphOp::FileMove {
                del: self.expand_edge_update(del)?,
                add: self.expand_insertion(add)?,
                path: path.clone(),
            }),

            CompactGraphOp::Edit {
                change,
                local,
                encoding,
            } => Ok(GraphOp::Edit {
                change: self.expand_atom(change)?,
                local: local.clone(),
                encoding: *encoding,
            }),

            CompactGraphOp::Replacement {
                change,
                replacement,
                local,
                encoding,
            } => Ok(GraphOp::Replacement {
                change: self.expand_edge_update(change)?,
                replacement: self.expand_insertion(replacement)?,
                local: local.clone(),
                encoding: *encoding,
            }),

            CompactGraphOp::SolveNameConflict { name, path } => Ok(GraphOp::SolveNameConflict {
                name: self.expand_edge_update(name)?,
                path: path.clone(),
            }),

            CompactGraphOp::UnsolveNameConflict { name, path } => {
                Ok(GraphOp::UnsolveNameConflict {
                    name: self.expand_edge_update(name)?,
                    path: path.clone(),
                })
            }

            CompactGraphOp::SolveOrderConflict { change, local } => {
                Ok(GraphOp::SolveOrderConflict {
                    change: self.expand_edge_update(change)?,
                    local: local.clone(),
                })
            }

            CompactGraphOp::UnsolveOrderConflict { change, local } => {
                Ok(GraphOp::UnsolveOrderConflict {
                    change: self.expand_edge_update(change)?,
                    local: local.clone(),
                })
            }

            CompactGraphOp::ResurrectZombies {
                change,
                local,
                encoding,
            } => Ok(GraphOp::ResurrectZombies {
                change: self.expand_edge_update(change)?,
                local: local.clone(),
                encoding: *encoding,
            }),

            CompactGraphOp::AddRoot { name, inode } => Ok(GraphOp::AddRoot {
                name: self.expand_insertion(name)?,
                inode: self.expand_insertion(inode)?,
            }),

            CompactGraphOp::DelRoot { name, inode } => Ok(GraphOp::DelRoot {
                name: self.expand_edge_update(name)?,
                inode: self.expand_edge_update(inode)?,
            }),
        }
    }
}
