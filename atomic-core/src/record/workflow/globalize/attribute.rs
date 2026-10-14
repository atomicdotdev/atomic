use super::*;
use crate::change::InodeAttr;

/// Globalize an inode attribute write and collect its inode change dependency.
pub fn globalize_set_attr<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    inode: Position<NodeId>,
    path: impl Into<String>,
    value: InodeAttr,
) -> GlobalizeResult<GraphOp<Option<Hash>>>
where
    T: GraphTxnT + TreeTxnT,
{
    let path = path.into();
    if path.is_empty() {
        return Err(GlobalizeError::InvalidAttribute {
            path,
            reason: "SetAttr requires a non-empty path".to_string(),
        });
    }
    if inode.change == NodeId::ROOT {
        return Err(GlobalizeError::InvalidAttribute {
            path,
            reason: "SetAttr cannot target ROOT".to_string(),
        });
    }
    value
        .validate()
        .map_err(|error| GlobalizeError::InvalidAttribute {
            path: path.clone(),
            reason: error.to_string(),
        })?;
    ctx.add_dependency_by_id(inode.change)?;
    let hash = ctx
        .txn
        .get_external(inode.change)?
        .ok_or(GlobalizeError::MissingExternalHash {
            node_id: inode.change,
        })?;
    Ok(GraphOp::SetAttr {
        inode: Position {
            change: Some(hash),
            pos: inode.pos,
        },
        path,
        value,
    })
}
