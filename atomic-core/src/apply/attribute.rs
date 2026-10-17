//! Application of graph-backed inode attribute events.

use crate::change::GraphOp;
use crate::pristine::{InodeAttrEvent, InodeAttrMutTxnT, TreeTxnT};
use crate::types::NodeId;
use crate::Hash;

use super::{resolve_position, LocalApplyError, LocalApplyResult};

/// Apply a `SetAttr` operation after structural graph atoms for the change.
///
/// Validation and inode resolution complete before either attribute index is
/// mutated. Reapplication of the same event is idempotent.
pub fn apply_set_attr<T>(
    txn: &mut T,
    change_id: NodeId,
    operation: &GraphOp<Option<Hash>>,
) -> LocalApplyResult<bool>
where
    T: InodeAttrMutTxnT + TreeTxnT,
{
    let GraphOp::SetAttr {
        inode,
        path: _,
        value,
    } = operation
    else {
        return Ok(false);
    };

    value
        .validate()
        .map_err(|error| LocalApplyError::Internal {
            message: format!("invalid SetAttr value: {error}"),
        })?;
    let position = resolve_position(txn, inode, change_id)?;
    if position.change == NodeId::ROOT {
        return Err(LocalApplyError::Internal {
            message: "SetAttr cannot target ROOT".to_string(),
        });
    }
    let repository_inode = txn
        .position_inode(position)
        .map_err(|error| LocalApplyError::Internal {
            message: format!("failed to resolve SetAttr inode: {error}"),
        })?
        .ok_or_else(|| LocalApplyError::Internal {
            message: format!("SetAttr target {position:?} has no repository inode"),
        })?;
    let event =
        InodeAttrEvent::new(change_id, *value).map_err(|error| LocalApplyError::Internal {
            message: error.to_string(),
        })?;
    txn.put_inode_attr_event(repository_inode, position, event)
        .map_err(|error| LocalApplyError::Internal {
            message: format!("failed to persist SetAttr: {error}"),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::InodeAttr;
    use crate::pristine::{InodeAttrTxnT, MutTxnT, Pristine};
    use crate::{ChangePosition, Hash, Inode, Position};

    #[test]
    fn application_indexes_valid_values_and_rejects_malformed_values_first() {
        let temp = tempfile::tempdir().unwrap();
        let pristine = Pristine::open(temp.path().join("apply-attrs.redb")).unwrap();
        let target_hash = Hash::of(b"target inode");
        let change_hash = Hash::of(b"attribute change");
        let mut txn = pristine.write_txn().unwrap();
        let target = txn.register_change(&target_hash).unwrap();
        let change = txn.register_change(&change_hash).unwrap();
        let position = Position::new(target, ChangePosition::new(9));
        let inode = Inode::new(4);
        txn.put_inode(inode, position).unwrap();

        let valid = GraphOp::SetAttr {
            inode: Position::new(Some(target_hash), position.pos),
            path: "script".to_string(),
            value: InodeAttr::Mode(0o755),
        };
        assert!(apply_set_attr(&mut txn, change, &valid).unwrap());
        assert_eq!(
            txn.get_inode_attr_events_by_inode(inode, valid_value_name())
                .unwrap()
                .len(),
            1
        );

        let malformed = GraphOp::SetAttr {
            inode: Position::new(Some(target_hash), position.pos),
            path: "script".to_string(),
            value: InodeAttr::Mode(0o1000),
        };
        assert!(apply_set_attr(&mut txn, change, &malformed).is_err());
        assert_eq!(
            txn.get_inode_attr_events_by_inode(inode, valid_value_name())
                .unwrap()
                .len(),
            1
        );
    }

    fn valid_value_name() -> crate::change::InodeAttrName {
        crate::change::InodeAttrName::Mode
    }
}
