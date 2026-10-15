//! Ingestion completion is distinct from registration and vertex ownership.

use redb::{ReadableMultimapTable, ReadableTable, TableError};
use std::collections::HashSet;

use super::attribute::decode_inode_attr_event;
use super::error::PristineResult;
use super::tables::{
    decode_vertex, APPLIED_CHANGES, GRAPH, POSITION_ATTRS, PRISTINE_META, VIEW_CHANGES,
};
use super::{GraphTxnT, ReadTxn, WriteTxn};
use crate::types::NodeId;

const INDEX_VERSION_KEY: &str = "applied_changes_index";

/// One-time compatibility rebuild from persisted canonical effects and
/// committed view logs. A registered/fetched object by itself is not evidence.
/// The marker and recovered entries commit together, so an interrupted rebuild
/// retries. No path ownership or recorded content is inferred from this index.
pub(crate) fn initialize(txn: &redb::WriteTransaction) -> PristineResult<()> {
    let mut index = txn.open_table(APPLIED_CHANGES)?;
    let mut meta = txn.open_table(PRISTINE_META)?;
    if meta.get(INDEX_VERSION_KEY)?.is_some() {
        return Ok(());
    }
    for id in canonical_effect_writers(
        &txn.open_multimap_table(GRAPH)?,
        &txn.open_multimap_table(POSITION_ATTRS)?,
        &txn.open_table(VIEW_CHANGES)?,
    )? {
        index.insert(id, 1)?;
    }
    meta.insert(INDEX_VERSION_KEY, 1)?;
    Ok(())
}

fn canonical_effect_writers(
    graph: &impl ReadableMultimapTable<&'static [u8; 24], &'static [u8; 24]>,
    attrs: &impl ReadableMultimapTable<&'static [u8; 17], &'static [u8; 13]>,
    views: &impl ReadableTable<&'static [u8; 16], u64>,
) -> PristineResult<HashSet<u64>> {
    let mut ids = HashSet::new();
    for entry in graph.iter()? {
        let (key, edges) = entry?;
        let (owner, _, _) = decode_vertex(key.value());
        if owner != 0 {
            ids.insert(owner);
        }
        for edge in edges {
            let edge = super::txn::helpers::deserialize_edge(edge?.value());
            let writer = edge.introduced_by();
            if !writer.is_root() {
                ids.insert(writer.get());
            }
        }
    }
    for entry in attrs.iter()? {
        let (_, events) = entry?;
        for event in events {
            ids.insert(decode_inode_attr_event(event?.value())?.introduced_by.get());
        }
    }
    // View membership is published only after successful ingestion in the
    // same transaction. It also preserves completion evidence for empty
    // changes, which have no graph or attribute effects to discover.
    for entry in views.iter()? {
        let (_, id) = entry?;
        if id.value() != 0 {
            ids.insert(id.value());
        }
    }
    ids.remove(&0);
    Ok(ids)
}

impl ReadTxn {
    /// Whether canonical effects were ingested, independently of current
    /// view membership or vertex ownership. Registration alone is insufficient.
    /// Legacy read-only databases derive evidence once per transaction without
    /// modifying the database; writable opens persist the derived index.
    pub fn was_change_applied(&self, id: NodeId) -> PristineResult<bool> {
        if id.is_root() {
            return Ok(false);
        }
        match self.txn.open_table(APPLIED_CHANGES) {
            Ok(index) => {
                if index.get(id.get())?.is_some() {
                    return Ok(true);
                }
                self.has_change_in_graph(id)
            }
            Err(TableError::TableDoesNotExist(_)) => {
                if self.legacy_applied_changes.get().is_none() {
                    let ids = canonical_effect_writers(
                        &self.txn.open_multimap_table(GRAPH)?,
                        &self.txn.open_multimap_table(POSITION_ATTRS)?,
                        &self.txn.open_table(VIEW_CHANGES)?,
                    )?;
                    let _ = self.legacy_applied_changes.set(ids);
                }
                Ok(self
                    .legacy_applied_changes
                    .get()
                    .expect("initialized above")
                    .contains(&id.get()))
            }
            Err(error) => Err(error.into()),
        }
    }
}

impl WriteTxn<'_> {
    /// Whether canonical effects were ingested. Write transactions initialize
    /// the completion index before any mutation, including on open_existing.
    pub fn was_change_applied(&self, id: NodeId) -> PristineResult<bool> {
        if id.is_root() {
            return Ok(false);
        }
        if self
            .txn
            .open_table(APPLIED_CHANGES)?
            .get(id.get())?
            .is_some()
        {
            return Ok(true);
        }
        self.has_change_in_graph(id)
    }

    /// Mark completion after all graph, attribute and FileOps writes. The
    /// transaction must commit those effects together with this marker.
    pub fn mark_change_applied(&mut self, id: NodeId) -> PristineResult<()> {
        if id.is_root() || self.get_external(id)?.is_none() {
            return Err(super::error::PristineError::ChangeNotFound { id: id.get() });
        }
        self.txn.open_table(APPLIED_CHANGES)?.insert(id.get(), 1)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pristine::{InodeAttrEvent, InodeAttrMutTxnT, MutTxnT, Pristine};
    use crate::types::{
        ChangePosition, EdgeFlags, GraphNode, Hash, Inode, Position, SerializedGraphEdge,
    };

    #[test]
    fn completion_is_atomic_and_registration_does_not_imply_application() {
        let dir = tempfile::tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("db")).unwrap();
        let id = {
            let mut txn = pristine.write_txn().unwrap();
            let id = txn.register_change(&Hash::of(b"empty change")).unwrap();
            txn.commit().unwrap();
            id
        };
        assert!(!pristine.read_txn().unwrap().was_change_applied(id).unwrap());
        {
            let mut txn = pristine.write_txn().unwrap();
            txn.mark_change_applied(id).unwrap();
            assert!(txn.was_change_applied(id).unwrap());
            // Aborting also rolls back the completion marker.
        }
        assert!(!pristine.read_txn().unwrap().was_change_applied(id).unwrap());
        let mut txn = pristine.write_txn().unwrap();
        txn.mark_change_applied(id).unwrap();
        txn.commit().unwrap();
        let read = pristine.read_txn().unwrap();
        assert!(!read.has_change_in_graph(id).unwrap());
        assert!(read.was_change_applied(id).unwrap());
    }

    #[test]
    fn legacy_rebuild_recovers_edge_only_attribute_only_and_empty_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let pristine = Pristine::open(&path).unwrap();
        let (delete, attr, empty, fetched) = {
            let mut txn = pristine.write_txn().unwrap();
            let base = txn.register_change(&Hash::of(b"base")).unwrap();
            let delete = txn.register_change(&Hash::of(b"delete")).unwrap();
            let attr = txn.register_change(&Hash::of(b"attr")).unwrap();
            let empty_hash = Hash::of(b"empty");
            let empty = txn.register_change(&empty_hash).unwrap();
            let fetched = txn.register_change(&Hash::of(b"fetched only")).unwrap();
            let pos = ChangePosition::new(1);
            txn.put_graph(
                GraphNode::new(base, pos, ChangePosition::new(2)),
                SerializedGraphEdge::new(EdgeFlags::DELETED, Position::ROOT, delete),
            )
            .unwrap();
            txn.put_inode_attr_event(
                Inode::new(1),
                Position::new(base, pos),
                InodeAttrEvent::new(attr, crate::change::InodeAttr::Mode(0o755)).unwrap(),
            )
            .unwrap();
            let mut view = txn.open_or_create_view("main").unwrap();
            txn.put_change(&mut view, empty, &empty_hash).unwrap();
            txn.txn.delete_table(APPLIED_CHANGES).unwrap();
            txn.txn
                .open_table(PRISTINE_META)
                .unwrap()
                .remove(INDEX_VERSION_KEY)
                .unwrap();
            txn.commit().unwrap();
            (delete, attr, empty, fetched)
        };
        drop(pristine);
        // Read-only inspection of an old database must work without an
        // upgrade write and cache the canonical evidence for this snapshot.
        let readonly = Pristine::open_readonly(&path).unwrap();
        let read = readonly.read_txn().unwrap();
        for id in [delete, attr, empty] {
            assert!(read.was_change_applied(id).unwrap());
        }
        assert!(!read.was_change_applied(fetched).unwrap());
        assert!(matches!(
            read.txn.open_table(APPLIED_CHANGES),
            Err(TableError::TableDoesNotExist(_))
        ));
        assert!(read.legacy_applied_changes.get().is_some());
        drop(read);
        drop(readonly);
        // open_existing skips table initialization; its first write must
        // still recover old deletions before application/reference routing.
        let existing = Pristine::open_existing(&path).unwrap();
        let write = existing.write_txn().unwrap();
        for id in [delete, attr, empty] {
            assert!(write.was_change_applied(id).unwrap());
        }
        assert!(!write.was_change_applied(fetched).unwrap());
        write.commit().unwrap();
        drop(existing);
        let pristine = Pristine::open(&path).unwrap();
        let read = pristine.read_txn().unwrap();
        for id in [delete, attr, empty] {
            assert!(!read.has_change_in_graph(id).unwrap());
            assert!(read.was_change_applied(id).unwrap());
        }
        assert!(!read.was_change_applied(fetched).unwrap());
    }
}
