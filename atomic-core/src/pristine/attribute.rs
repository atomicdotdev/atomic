//! Additive causal multi-value registers for inode attributes.

use std::collections::{BTreeSet, HashMap, HashSet};

use redb::ReadableMultimapTable;

use crate::change::{InodeAttr, InodeAttrName};
use crate::types::{Inode, NodeId, Position};

use super::error::{PristineError, PristineResult};
use super::tables::{encode_position, INODE_ATTRS, POSITION_ATTRS};
use super::{GraphTxnT, ReadTxn, WriteTxn};

pub const INODE_ATTR_EVENT_VERSION: u8 = 1;
pub const INODE_ATTR_EVENT_SIZE: usize = 13;

/// One immutable value written to an inode attribute register.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InodeAttrEvent {
    pub introduced_by: NodeId,
    pub value: InodeAttr,
}

impl InodeAttrEvent {
    pub fn new(introduced_by: NodeId, value: InodeAttr) -> Result<Self, PristineError> {
        value.validate().map_err(invalid_attr)?;
        if introduced_by == NodeId::ROOT {
            return Err(PristineError::Inconsistent {
                message: "inode attribute event cannot be introduced by ROOT".to_string(),
            });
        }
        Ok(Self {
            introduced_by,
            value,
        })
    }
}

/// The causally maximal values of one attribute.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InodeAttrState {
    events: Vec<InodeAttrEvent>,
}

impl InodeAttrState {
    pub fn events(&self) -> &[InodeAttrEvent] {
        &self.events
    }

    pub fn value(&self) -> Option<InodeAttr> {
        (self.events.len() == 1).then(|| self.events[0].value)
    }

    pub fn is_conflict(&self) -> bool {
        self.events.len() > 1
    }
}

fn invalid_attr(error: impl std::fmt::Display) -> PristineError {
    PristineError::Serialization {
        message: format!("invalid inode attribute: {error}"),
    }
}

pub fn encode_inode_attr_event(
    event: InodeAttrEvent,
) -> PristineResult<[u8; INODE_ATTR_EVENT_SIZE]> {
    let (len, value) = event.value.canonical_bytes().map_err(invalid_attr)?;
    let mut bytes = [0; INODE_ATTR_EVENT_SIZE];
    bytes[0] = INODE_ATTR_EVENT_VERSION;
    bytes[1] = event.value.name().as_byte();
    bytes[2..10].copy_from_slice(&event.introduced_by.get().to_le_bytes());
    bytes[10] = len;
    bytes[11..13].copy_from_slice(&value);
    Ok(bytes)
}

pub fn decode_inode_attr_event(
    bytes: &[u8; INODE_ATTR_EVENT_SIZE],
) -> PristineResult<InodeAttrEvent> {
    if bytes[0] != INODE_ATTR_EVENT_VERSION {
        return Err(invalid_attr(format!(
            "unsupported event version {}",
            bytes[0]
        )));
    }
    let name = InodeAttrName::from_byte(bytes[1])
        .ok_or_else(|| invalid_attr(format!("unsupported attribute name {}", bytes[1])))?;
    let introduced_by = NodeId::new(u64::from_le_bytes(bytes[2..10].try_into().unwrap()));
    let len = usize::from(bytes[10]);
    if len > 2 || bytes[11 + len..13].iter().any(|byte| *byte != 0) {
        return Err(invalid_attr("non-canonical value padding"));
    }
    let value = InodeAttr::from_canonical(name, &bytes[11..11 + len]).map_err(invalid_attr)?;
    InodeAttrEvent::new(introduced_by, value)
}

fn position_attr_key(position: Position<NodeId>, name: InodeAttrName) -> [u8; 17] {
    let mut key = [0; 17];
    key[..16].copy_from_slice(&encode_position(position.change.get(), position.pos.get()));
    key[16] = name.as_byte();
    key
}

fn inode_attr_key(inode: Inode, name: InodeAttrName) -> [u8; 9] {
    let mut key = [0; 9];
    key[..8].copy_from_slice(&inode.get().to_le_bytes());
    key[8] = name.as_byte();
    key
}

/// Read access to graph-backed inode attributes.
pub trait InodeAttrTxnT: GraphTxnT {
    fn get_inode_attr_events(
        &self,
        position: Position<NodeId>,
        name: InodeAttrName,
    ) -> PristineResult<Vec<InodeAttrEvent>>;

    fn get_inode_attr_events_by_inode(
        &self,
        inode: Inode,
        name: InodeAttrName,
    ) -> PristineResult<Vec<InodeAttrEvent>>;

    fn resolve_inode_attr(
        &self,
        position: Position<NodeId>,
        name: InodeAttrName,
        visible_changes: &HashSet<NodeId>,
    ) -> PristineResult<InodeAttrState> {
        let candidates: Vec<InodeAttrEvent> = self
            .get_inode_attr_events(position, name)?
            .into_iter()
            .filter(|event| visible_changes.contains(&event.introduced_by))
            .collect();
        let mut maxima = attr_event_dependency_frontier(self, candidates)?;
        maxima.dedup_by_key(|event| event.value);
        Ok(InodeAttrState { events: maxima })
    }
}

/// Mutable access to graph-backed inode attributes.
pub trait InodeAttrMutTxnT: InodeAttrTxnT {
    /// Append an event to both the graph-position and repository-inode indexes.
    fn put_inode_attr_event(
        &mut self,
        inode: Inode,
        position: Position<NodeId>,
        event: InodeAttrEvent,
    ) -> PristineResult<bool>;
}

macro_rules! impl_attr_read {
    ($ty:ty) => {
        impl InodeAttrTxnT for $ty {
            fn get_inode_attr_events(
                &self,
                position: Position<NodeId>,
                name: InodeAttrName,
            ) -> PristineResult<Vec<InodeAttrEvent>> {
                let table = self.txn.open_multimap_table(POSITION_ATTRS)?;
                let key = position_attr_key(position, name);
                let mut result = Vec::new();
                for value in table.get(&key)? {
                    result.push(decode_inode_attr_event(value?.value())?);
                }
                result.sort_unstable();
                Ok(result)
            }

            fn get_inode_attr_events_by_inode(
                &self,
                inode: Inode,
                name: InodeAttrName,
            ) -> PristineResult<Vec<InodeAttrEvent>> {
                let table = self.txn.open_multimap_table(INODE_ATTRS)?;
                let key = inode_attr_key(inode, name);
                let mut result = Vec::new();
                for value in table.get(&key)? {
                    result.push(decode_inode_attr_event(value?.value())?);
                }
                result.sort_unstable();
                Ok(result)
            }
        }
    };
}

impl_attr_read!(ReadTxn);
impl_attr_read!(WriteTxn<'_>);

impl InodeAttrMutTxnT for WriteTxn<'_> {
    fn put_inode_attr_event(
        &mut self,
        inode: Inode,
        position: Position<NodeId>,
        event: InodeAttrEvent,
    ) -> PristineResult<bool> {
        event.value.validate().map_err(invalid_attr)?;
        let existing = self.get_inode_attr_events(position, event.value.name())?;
        if let Some(previous) = existing
            .iter()
            .find(|previous| previous.introduced_by == event.introduced_by)
        {
            if previous.value == event.value {
                return Ok(false);
            }
            return Err(PristineError::Inconsistent {
                message: format!(
                    "change {:?} writes two values to {:?}",
                    event.introduced_by,
                    event.value.name()
                ),
            });
        }

        let bytes = encode_inode_attr_event(event)?;
        let position_key = position_attr_key(position, event.value.name());
        let inode_key = inode_attr_key(inode, event.value.name());
        let mut position_table = self.txn.open_multimap_table(POSITION_ATTRS)?;
        position_table.insert(&position_key, &bytes)?;
        drop(position_table);
        let mut inode_table = self.txn.open_multimap_table(INODE_ATTRS)?;
        inode_table.insert(&inode_key, &bytes)?;
        Ok(true)
    }
}

/// The causally maximal writers among already-selected register events.
///
/// An event is dominated when another selected event's introducing change has
/// this event's introducing change in its dependency closure; dominated
/// writers are already transitive dependencies of their dominator, so only
/// the frontier needs to become a direct dependency of a new write. Unlike
/// [`InodeAttrState`] (whose register projection deduplicates equal values),
/// the dependency frontier keeps every maximal writer: two concurrent writers
/// of the same value are both causality a new write must dominate.
///
/// Visibility filtering is the caller's or the view-scoped transaction's
/// responsibility — this helper only reduces ancestry among the events it is
/// given (review CB-9C R1).
pub fn attr_event_dependency_frontier<T: GraphTxnT + ?Sized>(
    txn: &T,
    events: Vec<InodeAttrEvent>,
) -> PristineResult<Vec<InodeAttrEvent>> {
    let mut ancestry = HashMap::<NodeId, BTreeSet<NodeId>>::new();
    for event in &events {
        ancestry.insert(
            event.introduced_by,
            causal_ancestors(txn, event.introduced_by)?,
        );
    }
    let mut maxima = Vec::new();
    'candidate: for candidate in &events {
        for other in &events {
            if other.introduced_by != candidate.introduced_by
                && ancestry[&other.introduced_by].contains(&candidate.introduced_by)
            {
                continue 'candidate;
            }
        }
        maxima.push(*candidate);
    }
    maxima.sort_unstable();
    Ok(maxima)
}

fn causal_ancestors<T: GraphTxnT + ?Sized>(
    txn: &T,
    change: NodeId,
) -> PristineResult<BTreeSet<NodeId>> {
    let mut result = BTreeSet::new();
    let mut pending = vec![change];
    while let Some(current) = pending.pop() {
        for hash in txn.get_indexed_change_deps(current)? {
            let dependency =
                txn.get_internal(&hash)?
                    .ok_or(PristineError::MissingRegisteredDependency {
                        change_id: current.get(),
                        dependency: hash.to_string(),
                    })?;
            if result.insert(dependency) {
                pending.push(dependency);
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pristine::{MutTxnT, Pristine};
    use crate::{ChangePosition, Hash};

    #[test]
    fn causal_register_keeps_concurrent_maxima_and_both_indexes() {
        let temp = tempfile::tempdir().unwrap();
        let pristine = Pristine::open(temp.path().join("attrs.redb")).unwrap();
        let h1 = Hash::of(b"first mode");
        let h2 = Hash::of(b"concurrent mode");
        let h3 = Hash::of(b"later mode");
        let inode = Inode::new(7);
        let position = Position::new(NodeId::new(99), ChangePosition::new(4));

        let (id1, id2, id3) = {
            let mut txn = pristine.write_txn().unwrap();
            let id1 = txn.register_change(&h1).unwrap();
            let id2 = txn.register_change(&h2).unwrap();
            let id3 = txn.register_change(&h3).unwrap();
            txn.put_change_deps(id1, &[]).unwrap();
            txn.put_change_deps(id2, &[]).unwrap();
            txn.put_change_deps(id3, &[h1]).unwrap();
            txn.put_inode_attr_event(
                inode,
                position,
                InodeAttrEvent::new(id1, InodeAttr::Mode(0o644)).unwrap(),
            )
            .unwrap();
            txn.put_inode_attr_event(
                inode,
                position,
                InodeAttrEvent::new(id2, InodeAttr::Mode(0o755)).unwrap(),
            )
            .unwrap();
            txn.put_inode_attr_event(
                inode,
                position,
                InodeAttrEvent::new(id3, InodeAttr::Mode(0o700)).unwrap(),
            )
            .unwrap();
            txn.commit().unwrap();
            (id1, id2, id3)
        };

        let txn = pristine.read_txn().unwrap();
        let visible = HashSet::from([id1, id2, id3]);
        let state = txn
            .resolve_inode_attr(position, InodeAttrName::Mode, &visible)
            .unwrap();
        assert!(state.is_conflict());
        assert_eq!(state.events().len(), 2);
        assert!(state
            .events()
            .iter()
            .any(|event| event.value == InodeAttr::Mode(0o755)));
        assert!(state
            .events()
            .iter()
            .any(|event| event.value == InodeAttr::Mode(0o700)));
        assert_eq!(
            txn.get_inode_attr_events_by_inode(inode, InodeAttrName::Mode)
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn event_codec_rejects_noncanonical_values() {
        assert!(InodeAttrEvent::new(NodeId::new(1), InodeAttr::Mode(0o1000)).is_err());
        let mut bytes = encode_inode_attr_event(
            InodeAttrEvent::new(NodeId::new(1), InodeAttr::Mode(0o755)).unwrap(),
        )
        .unwrap();
        bytes[10] = 1;
        assert!(decode_inode_attr_event(&bytes).is_err());
    }

    /// Review CB-9C R1: the dependency frontier keeps only causally maximal
    /// writers. A dominated writer is already a transitive dependency of its
    /// dominator; concurrent writers (even of the same value) all remain.
    #[test]
    fn dependency_frontier_keeps_maximal_writers_only() {
        let temp = tempfile::tempdir().unwrap();
        let pristine = Pristine::open(temp.path().join("frontier.redb")).unwrap();
        let h1 = Hash::of(b"frontier base");
        let h2 = Hash::of(b"frontier concurrent");
        let h3 = Hash::of(b"frontier later dependent");

        let (id1, id2, id3) = {
            let mut txn = pristine.write_txn().unwrap();
            let id1 = txn.register_change(&h1).unwrap();
            let id2 = txn.register_change(&h2).unwrap();
            let id3 = txn.register_change(&h3).unwrap();
            txn.put_change_deps(id1, &[]).unwrap();
            txn.put_change_deps(id2, &[]).unwrap();
            txn.put_change_deps(id3, &[h1]).unwrap();
            txn.commit().unwrap();
            (id1, id2, id3)
        };

        let txn = pristine.read_txn().unwrap();
        let event = |id, mode| InodeAttrEvent {
            introduced_by: id,
            value: InodeAttr::Mode(mode),
        };

        // id1 is an ancestor of id3: id3 dominates it.
        let frontier = attr_event_dependency_frontier(
            &txn,
            vec![event(id1, 0o644), event(id2, 0o755), event(id3, 0o700)],
        )
        .unwrap();
        let writers: std::collections::HashSet<NodeId> =
            frontier.iter().map(|event| event.introduced_by).collect();
        assert_eq!(writers, HashSet::from([id2, id3]));

        // Concurrent writers both stay, even when their values match: a new
        // write must dominate both writers.
        let frontier =
            attr_event_dependency_frontier(&txn, vec![event(id1, 0o644), event(id2, 0o644)])
                .unwrap();
        assert_eq!(frontier.len(), 2);

        // A single writer is its own frontier.
        let frontier = attr_event_dependency_frontier(&txn, vec![event(id3, 0o700)]).unwrap();
        assert_eq!(frontier.len(), 1);
        assert_eq!(frontier[0].introduced_by, id3);
    }
}
