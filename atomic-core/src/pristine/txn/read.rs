//! Read-only transaction implementation
//!
//! This module provides the `ReadTxn` struct which implements read-only
//! access to the pristine database.

use redb::{ReadTransaction, ReadableMultimapTable, ReadableTable, ReadableTableMetadata};

use crate::operation::{
    decode_effect_receipt, decode_operation, decode_operation_heads, decode_operation_scope,
    EffectReceipt, Operation, OperationHeads, OperationScope,
};
use crate::pristine::tables::{BRIDGE_EVENT_CAPTURES, BRIDGE_EVENT_CAPTURE_ANCHORS, BRIDGE_REF_CAPTURE_TOKENS, REF_MAPPINGS, VAULT_ENTRIES, VAULT_MANIFEST};
use crate::pristine::traits::tag::{BridgeEventCaptureTxnT, GitCommitClosureTxnT, GitShaIndexTxnT};
use crate::pristine::traits::tag::TagRecord;
use crate::pristine::traits::capability_txn::{capability_metadata_key, CapabilityTxnT};
use crate::pristine::traits::{EmbeddingsTxnT, KgTxnT, TagTxnT, VaultEntryMeta, VaultTxnT};
use crate::pristine::ref_mapping::RefMappingTxnT;
use crate::pristine::vault::{EmbeddingRecord, KgEdge, KgNode, SearchResult};
use crate::pristine::{
    decode_file_index_v2, decode_set_id_index_entry, FileIndexV2Entry, FileIndexV2Key,
    SetIdIndexTxnT, VaultEntry, VaultEntryType, VaultManifest,
};
use crate::types::{
    ChangePosition, EdgeFlags, EffectReceiptId, GraphNode, Hash, Inode, Merkle, NodeId,
    OperationId, Position, SerializedGraphEdge, WorkingCopyId,
};

use crate::pristine::error::{PristineError, PristineResult};
use crate::pristine::path_claim::{
    decode_path_claim_event, PathClaimEntry, PathClaimEvent, PATH_CLAIM_SCHEMA_KEY,
};
use crate::pristine::tables::*;
use crate::pristine::tables::{TAG_NAME_INDEX, TAG_RECORDS};
use crate::pristine::traits::{
    decode_working_copy_record, FileIndexEntry, FileIndexMetadata, FileIndexV2TxnT, GraphTxnT,
    GraphVisibilityClosure, OperationTxnT, PathClaimTxnT, StoredConflict, TreeTxnT, ViewState,
    ViewTxnT, WorkingCopyRecord, WorkingCopyTxnT,
};

use super::helpers::{
    collect_preload_edges, collect_until_error, deserialize_conflicts, deserialize_edge,
    deserialize_view_state, graph_presence, try_collect, try_is_present, AdjIterator,
};

/// Read-only transaction
///
/// Provides read access to the pristine database. Multiple read transactions
/// can be active simultaneously.
pub struct ReadTxn {
    pub(crate) txn: ReadTransaction,
}

fn operation_serialization_error(error: impl std::fmt::Display) -> PristineError {
    PristineError::Serialization {
        message: error.to_string(),
    }
}

fn decode_operation_row(key: &[u8; OperationId::SIZE], bytes: &[u8]) -> PristineResult<Operation> {
    let key_id = OperationId::from_bytes(*key);
    let operation = decode_operation(bytes).map_err(operation_serialization_error)?;
    if operation.id() != key_id {
        return Err(PristineError::Inconsistent {
            message: format!(
                "OPERATIONS key {} contains canonical operation {}",
                key_id,
                operation.id()
            ),
        });
    }
    Ok(operation)
}

fn decode_effect_receipt_row(key: &[u8; 64], bytes: &[u8]) -> PristineResult<EffectReceipt> {
    let (key_operation, key_receipt) = decode_effect_receipt_key(key);
    let receipt = decode_effect_receipt(bytes).map_err(operation_serialization_error)?;
    if receipt.id() != key_receipt || receipt.payload().operation != key_operation {
        return Err(PristineError::Inconsistent {
            message: format!(
                "EFFECT_RECEIPTS key ({key_operation}, {key_receipt}) contains receipt ({}, {})",
                receipt.payload().operation,
                receipt.id()
            ),
        });
    }
    Ok(receipt)
}

fn decode_working_copy_row(
    key: &[u8; WorkingCopyId::SIZE],
    bytes: &[u8],
) -> PristineResult<WorkingCopyRecord> {
    let key_id = WorkingCopyId::from_bytes(*key);
    let record = decode_working_copy_record(bytes)?;
    if record.id != key_id {
        return Err(PristineError::Inconsistent {
            message: format!(
                "WORKING_COPIES key {} contains record for {}",
                key_id, record.id
            ),
        });
    }
    Ok(record)
}

impl ReadTxn {
    /// Create a new read transaction
    pub(crate) fn new(txn: ReadTransaction) -> Self {
        Self { txn }
    }

    /// Open the INODE_GRAPH table for shared use across multiple operations.
    ///
    /// The returned handle can be passed to [`InodePreloadTxn::from_table`]
    /// to avoid reopening the table for each file during parallel materialize.
    pub fn open_inode_graph_table(
        &self,
    ) -> PristineResult<redb::ReadOnlyMultimapTable<&'static [u8; 32], &'static [u8; 24]>> {
        Ok(self.txn.open_multimap_table(INODE_GRAPH)?)
    }

    /// Enumerate every `(inode, path)` pair in `REV_TREE`.
    ///
    /// Healthy databases contain the exact inverse of `TREE`. Legacy
    /// reverse-only rows may be inspected during migration, but durable name
    /// claims belong in `PATH_CLAIMS` and must not be created here.
    pub fn iter_rev_tree(&self) -> PristineResult<Vec<(Inode, String)>> {
        let table = self.txn.open_table(REV_TREE)?;
        let mut results = Vec::new();
        for entry in table.iter()? {
            let (k, v) = entry?;
            results.push((Inode::new(k.value()), v.value().to_string()));
        }
        Ok(results)
    }
}

impl SetIdIndexTxnT for ReadTxn {
    fn get_set_id_index(
        &self,
        view_id: u64,
    ) -> PristineResult<Option<crate::pristine::SetIdIndexEntry>> {
        let table = match self.txn.open_table(VIEW_SET_ID_INDEX) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let result = match table.get(view_id)? {
            Some(value) => decode_set_id_index_entry(value.value()).map(Some),
            None => Ok(None),
        };
        result
    }
}

impl CapabilityTxnT for ReadTxn {
    fn required_capability_version(&self, id: &str) -> PristineResult<Option<u32>> {
        let table = self.txn.open_table(PRISTINE_META)?;
        let version = table
            .get(capability_metadata_key(id).as_str())?
            .map(|version| version.value());
        Ok(version)
    }
}

impl PathClaimTxnT for ReadTxn {
    fn path_claim_schema_version(&self) -> PristineResult<Option<u32>> {
        let table = self.txn.open_table(PRISTINE_META)?;
        let version = table
            .get(PATH_CLAIM_SCHEMA_KEY)?
            .map(|version| version.value());
        Ok(version)
    }

    fn get_path_claims(&self, path: &str) -> PristineResult<Vec<PathClaimEvent>> {
        let table = self.txn.open_multimap_table(PATH_CLAIMS)?;
        let mut events = Vec::new();
        for value in table.get(path)? {
            let value = value?;
            events.push(decode_path_claim_event(value.value())?);
        }
        Ok(events)
    }

    fn iter_path_claims(&self) -> PristineResult<Vec<PathClaimEntry>> {
        let table = self.txn.open_multimap_table(PATH_CLAIMS)?;
        let mut entries = Vec::new();
        for row in table.iter()? {
            let (path, values) = row?;
            let path = path.value().to_string();
            for value in values {
                let value = value?;
                entries.push(PathClaimEntry::new(
                    path.clone(),
                    decode_path_claim_event(value.value())?,
                ));
            }
        }
        Ok(entries)
    }

    fn iter_rev_tree_pairs(&self) -> PristineResult<Vec<(Inode, String)>> {
        self.iter_rev_tree()
    }
}

// GraphTxnT Implementation

impl GraphTxnT for ReadTxn {
    type Adj = AdjIterator;

    fn get_external(&self, id: NodeId) -> PristineResult<Option<Hash>> {
        let table = self.txn.open_table(EXTERNAL)?;
        let result = table.get(id.get())?;
        match result {
            Some(value) => {
                let bytes: &[u8; 32] = value.value();
                Ok(Some(Hash::from_bytes(*bytes)))
            }
            None => Ok(None),
        }
    }

    fn get_internal(&self, hash: &Hash) -> PristineResult<Option<NodeId>> {
        let table = self.txn.open_table(INTERNAL)?;
        let result = table.get(hash.as_bytes())?;
        match result {
            Some(value) => Ok(Some(NodeId::new(value.value()))),
            None => Ok(None),
        }
    }

    fn list_registered_changes(&self) -> PristineResult<Vec<(NodeId, Hash)>> {
        let external = self.txn.open_table(EXTERNAL)?;
        let node_types = self.txn.open_table(NODE_TYPES)?;
        let mut changes = Vec::new();
        for result in external.iter()? {
            let (key, value) = result?;
            let node_id = NodeId::new(key.value());
            let is_change = node_types
                .get(node_id.get())?
                .map(|node_type| node_type.value() == node_type::CHANGE)
                .unwrap_or(true);
            if is_change {
                changes.push((node_id, Hash::from_bytes(*value.value())));
            }
        }
        Ok(changes)
    }

    fn iter_adjacent(
        &self,
        node: GraphNode<NodeId>,
        min_flag: EdgeFlags,
        max_flag: EdgeFlags,
    ) -> PristineResult<Self::Adj> {
        let table = self.txn.open_multimap_table(GRAPH)?;
        let key = encode_vertex(node.change.get(), node.start.get(), node.end.get());

        let mut edges = Vec::new();
        for v in try_collect(table.get(&key)?)? {
            let bytes: &[u8; 24] = v.value();
            let edge = deserialize_edge(bytes);
            let flag = edge.flag();
            if flag >= min_flag && flag <= max_flag {
                edges.push(edge);
            }
        }

        Ok(AdjIterator::new(edges))
    }

    fn find_block(&self, pos: Position<NodeId>) -> PristineResult<GraphNode<NodeId>> {
        // Handle ROOT position specially - ROOT is a virtual span that doesn't
        // exist in the database. It represents the repository root and is the
        // parent of all top-level files and directories.
        if pos.change.is_root() {
            return Ok(GraphNode::ROOT);
        }

        let table = self.txn.open_multimap_table(GRAPH)?;

        let change_id = pos.change.get();
        let target_pos = pos.pos.get();

        // Binary-search candidates instead of scanning every vertex of the
        // change (the historical full-range scan made each call
        // O(change vertices); within one change the non-empty hunks are
        // DISJOINT ranges of that change's contents buffer, so the containing
        // vertex is the LAST vertex key ≤ (pos, u64::MAX) and the
        // empty-vertex fallback is an exact-key lookup — review ::15,
        // 2026-09-16). Semantics identical to the scan, including the
        // inode/content start ambiguity (AGENTS.md "Position Ambiguity").
        let upper = encode_vertex(change_id, target_pos, u64::MAX);
        if let Some((v_change, v_start, v_end)) = table
            .range::<&[u8; 24]>(&encode_vertex(change_id, 0, 0)..=&upper)?
            .rev()
            .next()
            .transpose()?
            .map(|(key, _values)| decode_vertex(key.value()))
        {
            if v_change == change_id
                && v_start != v_end
                && v_start <= target_pos
                && target_pos < v_end
            {
                return Ok(GraphNode {
                    change: NodeId::new(change_id),
                    start: ChangePosition::new(v_start),
                    end: ChangePosition::new(v_end),
                });
            }
        }

        // Empty-vertex fallback: the exact key (change, pos, pos).
        let empty_key = encode_vertex(change_id, target_pos, target_pos);
        if try_is_present(table.get(&empty_key)?)? {
            return Ok(GraphNode {
                change: NodeId::new(change_id),
                start: ChangePosition::new(target_pos),
                end: ChangePosition::new(target_pos),
            });
        }

        Err(PristineError::BlockNotFound {
            change: change_id,
            pos: target_pos,
        })
    }

    fn find_block_end(&self, pos: Position<NodeId>) -> PristineResult<GraphNode<NodeId>> {
        // Handle ROOT position specially
        if pos.change.is_root() {
            return Ok(GraphNode::ROOT);
        }

        let table = self.txn.open_multimap_table(GRAPH)?;

        let change_id = pos.change.get();
        let target_pos = pos.pos.get();

        // FIRST: Check for empty span at exact position using direct lookup.
        // This is important because empty vertices like inode markers (e.g., V[9:9])
        // must be found when predecessors references position 9, even if there's
        // another span like V[0:9] that also ends at position 9.
        // Without this direct lookup, iteration would return V[0:9] first since
        // it has a lower start position.
        let empty_key = encode_vertex(change_id, target_pos, target_pos);
        if try_is_present(table.get(&empty_key)?)? {
            return Ok(GraphNode {
                change: NodeId::new(change_id),
                start: ChangePosition::new(target_pos),
                end: ChangePosition::new(target_pos),
            });
        }

        // SECOND: binary-search the vertices that end at or contain this
        // position (the historical full-range scan made each call
        // O(change vertices) — review ::15, 2026-09-16). Under within-change
        // hunk disjointness: an ends-at-pos span (unique when it exists) is
        // the last vertex key ≤ (pos-1, MAX); a contains-pos span (unique
        // when it exists) is the last vertex key ≤ (pos, MAX); an
        // ends-at-pos span starts strictly before any contains-pos span, so
        // this order preserves the historical first-match semantics.
        if target_pos > 0 {
            let before_upper = encode_vertex(change_id, target_pos - 1, u64::MAX);
            if let Some((v_change, v_start, v_end)) = table
                .range::<&[u8; 24]>(&encode_vertex(change_id, 0, 0)..=&before_upper)?
                .rev()
                .next()
                .transpose()?
                .map(|(key, _values)| decode_vertex(key.value()))
            {
                if v_change == change_id && v_end == target_pos && v_start < v_end {
                    return Ok(GraphNode {
                        change: NodeId::new(change_id),
                        start: ChangePosition::new(v_start),
                        end: ChangePosition::new(v_end),
                    });
                }
            }
        }

        let contains_upper = encode_vertex(change_id, target_pos, u64::MAX);
        if let Some((v_change, v_start, v_end)) = table
            .range::<&[u8; 24]>(&encode_vertex(change_id, 0, 0)..=&contains_upper)?
            .rev()
            .next()
            .transpose()?
            .map(|(key, _values)| decode_vertex(key.value()))
        {
            if v_change == change_id && v_start <= target_pos && target_pos < v_end {
                return Ok(GraphNode {
                    change: NodeId::new(change_id),
                    start: ChangePosition::new(v_start),
                    end: ChangePosition::new(v_end),
                });
            }
        }

        Err(PristineError::BlockNotFound {
            change: change_id,
            pos: target_pos,
        })
    }

    fn has_vertex(&self, node: GraphNode<NodeId>) -> PristineResult<bool> {
        let table = self.txn.open_multimap_table(GRAPH)?;
        let key = encode_vertex(node.change.get(), node.start.get(), node.end.get());
        let has = graph_presence(table.get(&key)?)?;
        Ok(has)
    }

    fn get_node_type(&self, node_id: NodeId) -> PristineResult<Option<u8>> {
        let table = self.txn.open_table(NODE_TYPES)?;
        let result = table.get(node_id.get())?;
        Ok(result.map(|v| v.value()))
    }

    fn get_rev_deps(&self, dep_id: NodeId) -> PristineResult<Vec<NodeId>> {
        let table = self.txn.open_multimap_table(REV_DEPS)?;
        let mut result = Vec::new();
        let iter = table.get(dep_id.get())?;
        for item in iter {
            let value = item?;
            result.push(NodeId::new(value.value()));
        }
        Ok(result)
    }

    fn get_change_deps(&self, change_id: NodeId) -> PristineResult<Vec<Hash>> {
        let table = self.txn.open_multimap_table(CHANGE_DEPS)?;
        let mut result = Vec::new();
        let iter = table.get(change_id.get())?;
        for item in iter {
            let value = item?;
            result.push(Hash::from_bytes(*value.value()));
        }
        Ok(result)
    }

    fn change_deps_indexed_count(&self, change_id: NodeId) -> PristineResult<Option<u64>> {
        let table = self.txn.open_table(CHANGE_DEPS_INDEXED)?;
        let count = table.get(change_id.get())?.map(|value| value.value());
        Ok(count)
    }

    fn get_rev_change_deps(&self, dep_hash: &Hash) -> PristineResult<Vec<NodeId>> {
        let table = self.txn.open_multimap_table(REV_CHANGE_DEPS)?;
        let mut result = Vec::new();
        let iter = table.get(dep_hash.as_bytes())?;
        for item in iter {
            let value = item?;
            result.push(NodeId::new(value.value()));
        }
        Ok(result)
    }

    fn has_change_in_graph(&self, change_id: NodeId) -> PristineResult<bool> {
        let table = self.txn.open_multimap_table(GRAPH)?;
        let start_key = encode_vertex(change_id.get(), 0, 0);
        let end_key = encode_vertex(change_id.get(), u64::MAX, u64::MAX);
        let has = graph_presence(table.range::<&[u8; 24]>(&start_key..=&end_key)?)?;
        Ok(has)
    }
}

// ViewTxnT Implementation

impl ViewTxnT for ReadTxn {
    fn get_view_by_id(&self, id: u64) -> PristineResult<Option<ViewState>> {
        let table = self.txn.open_table(VIEWS)?;
        for result in table.iter()? {
            let (_key, value) = result?;
            let state = deserialize_view_state(value.value())?;
            if state.id == id {
                return Ok(Some(state));
            }
        }
        Ok(None)
    }

    fn get_view(&self, name: &str) -> PristineResult<Option<ViewState>> {
        let table = self.txn.open_table(VIEWS)?;
        let result = table.get(name)?;
        match result {
            Some(value) => {
                let bytes = value.value();
                let state = deserialize_view_state(bytes)?;
                Ok(Some(state))
            }
            None => Ok(None),
        }
    }

    fn snapshot_views(&self) -> PristineResult<Vec<(String, ViewState)>> {
        let table = self.txn.open_table(VIEWS)?;
        let mut rows = Vec::new();
        for entry in table.iter()? {
            let (name, value) = entry?;
            rows.push((
                name.value().to_string(),
                deserialize_view_state(value.value())?,
            ));
        }
        rows.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(rows)
    }

    fn list_views(&self) -> PristineResult<Vec<String>> {
        Ok(self
            .snapshot_views()?
            .into_iter()
            .map(|(name, _)| name)
            .collect())
    }

    fn get_conflicts(&self, view_id: u64, inode: u64) -> PristineResult<Vec<StoredConflict>> {
        let table = self.txn.open_table(CONFLICTS)?;
        let key = encode_view_seq(view_id, inode);
        match table.get(&key)? {
            Some(value) => deserialize_conflicts(value.value()),
            None => Ok(Vec::new()),
        }
    }

    fn iter_conflicts(&self, view_id: u64) -> PristineResult<Vec<(u64, Vec<StoredConflict>)>> {
        let table = self.txn.open_table(CONFLICTS)?;
        let start = encode_view_seq(view_id, 0);
        let end = encode_view_seq(view_id, u64::MAX);
        let mut out = Vec::new();
        for entry in table.range::<&[u8; 16]>(&start..=&end)? {
            let (key, value) = entry?;
            let (_vid, inode) = decode_view_seq(key.value());
            let conflicts = deserialize_conflicts(value.value())?;
            if !conflicts.is_empty() {
                out.push((inode, conflicts));
            }
        }
        Ok(out)
    }

    fn snapshot_conflicts(&self) -> PristineResult<Vec<(u64, Inode, Vec<StoredConflict>)>> {
        let table = self.txn.open_table(CONFLICTS)?;
        let mut rows = Vec::new();
        for entry in table.iter()? {
            let (key, value) = entry?;
            let (view_id, inode) = decode_view_seq(key.value());
            rows.push((
                view_id,
                Inode::new(inode),
                deserialize_conflicts(value.value())?,
            ));
        }
        rows.sort_by_key(|(view_id, inode, _)| (*view_id, *inode));
        Ok(rows)
    }

    fn get_change_seq(&self, view: &ViewState, change_id: NodeId) -> PristineResult<Option<u64>> {
        let table = self.txn.open_table(REV_VIEW_CHANGES)?;
        let key = encode_view_seq(view.id, change_id.get());
        let result = table.get(&key)?;
        match result {
            Some(value) => Ok(Some(value.value())),
            None => Ok(None),
        }
    }

    fn get_change_at_seq(&self, view: &ViewState, seq: u64) -> PristineResult<Option<NodeId>> {
        let table = self.txn.open_table(VIEW_CHANGES)?;
        let key = encode_view_seq(view.id, seq);
        let result = table.get(&key)?;
        match result {
            Some(value) => Ok(Some(NodeId::new(value.value()))),
            None => Ok(None),
        }
    }

    fn iter_changes(
        &self,
        view: &ViewState,
        from_seq: u64,
    ) -> PristineResult<Box<dyn Iterator<Item = Result<(u64, NodeId, Merkle), PristineError>> + '_>>
    {
        let changes_table = self.txn.open_table(VIEW_CHANGES)?;
        let tags_table = self.txn.open_table(MERKLE_CHAIN)?;

        let view_id = view.id;
        let start_key = encode_view_seq(view_id, from_seq);
        let end_key = encode_view_seq(view_id, u64::MAX);

        // Collect into a Vec to avoid lifetime issues
        let mut results = Vec::new();
        for result in changes_table.range::<&[u8; 16]>(&start_key..=&end_key)? {
            match result {
                Ok((key, value)) => {
                    let (_, seq) = decode_view_seq(key.value());
                    let change_id = NodeId::new(value.value());

                    let tag_key = encode_view_seq(view_id, seq);
                    let merkle = match tags_table.get(&tag_key) {
                        Ok(Some(m)) => Merkle::from_bytes(*m.value()),
                        _ => Merkle::ZERO,
                    };

                    results.push(Ok((seq, change_id, merkle)));
                }
                Err(e) => {
                    results.push(Err(PristineError::Storage(Box::new(e))));
                }
            }
        }

        Ok(Box::new(results.into_iter()))
    }
}

// OperationTxnT Implementation

impl OperationTxnT for ReadTxn {
    fn get_operation(&self, id: OperationId) -> PristineResult<Option<Operation>> {
        let table = match self.txn.open_table(OPERATIONS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Err(PristineError::OperationSchemaUnavailable);
            }
            Err(error) => return Err(error.into()),
        };
        match table.get(id.as_bytes())? {
            Some(value) => decode_operation_row(id.as_bytes(), value.value()).map(Some),
            None => Ok(None),
        }
    }

    fn list_operations(&self) -> PristineResult<Vec<Operation>> {
        let table = match self.txn.open_table(OPERATIONS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Err(PristineError::OperationSchemaUnavailable);
            }
            Err(error) => return Err(error.into()),
        };
        let mut operations = Vec::new();
        for entry in table.iter()? {
            let (key, value) = entry?;
            operations.push(decode_operation_row(key.value(), value.value())?);
        }
        Ok(operations)
    }

    fn get_operation_heads(&self, scope: OperationScope) -> PristineResult<OperationHeads> {
        let table = match self.txn.open_table(OP_HEADS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Err(PristineError::OperationSchemaUnavailable);
            }
            Err(error) => return Err(error.into()),
        };
        let key = crate::operation::encode_operation_scope(scope);
        let heads = match table.get(&key)? {
            Some(value) => {
                decode_operation_heads(value.value()).map_err(operation_serialization_error)?
            }
            None => OperationHeads::new(Vec::new()),
        };
        for head in heads.as_slice() {
            if OperationTxnT::get_operation(self, *head)?.is_none() {
                return Err(PristineError::OperationNotFound {
                    id: head.to_string(),
                });
            }
        }
        Ok(heads)
    }

    fn list_operation_heads(&self) -> PristineResult<Vec<(OperationScope, OperationHeads)>> {
        let table = match self.txn.open_table(OP_HEADS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Err(PristineError::OperationSchemaUnavailable);
            }
            Err(error) => return Err(error.into()),
        };
        let mut rows = Vec::new();
        for entry in table.iter()? {
            let (key, value) = entry?;
            let scope =
                decode_operation_scope(key.value()).map_err(operation_serialization_error)?;
            let heads =
                decode_operation_heads(value.value()).map_err(operation_serialization_error)?;
            for head in heads.as_slice() {
                if OperationTxnT::get_operation(self, *head)?.is_none() {
                    return Err(PristineError::OperationNotFound {
                        id: head.to_string(),
                    });
                }
            }
            rows.push((scope, heads));
        }
        Ok(rows)
    }

    fn get_effect_receipts(&self, operation: OperationId) -> PristineResult<Vec<EffectReceipt>> {
        if OperationTxnT::get_operation(self, operation)?.is_none() {
            return Err(PristineError::OperationNotFound {
                id: operation.to_string(),
            });
        }
        let table = match self.txn.open_table(EFFECT_RECEIPTS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Err(PristineError::OperationSchemaUnavailable);
            }
            Err(error) => return Err(error.into()),
        };
        let start = encode_effect_receipt_key(operation, EffectReceiptId::from_bytes([0; 32]));
        let end = encode_effect_receipt_key(operation, EffectReceiptId::from_bytes([u8::MAX; 32]));
        let mut receipts = Vec::new();
        for entry in table.range::<&[u8; 64]>(&start..=&end)? {
            let (key, value) = entry?;
            receipts.push(decode_effect_receipt_row(key.value(), value.value())?);
        }
        Ok(receipts)
    }
}

// WorkingCopyTxnT Implementation

impl WorkingCopyTxnT for ReadTxn {
    fn get_working_copy(&self, id: WorkingCopyId) -> PristineResult<Option<WorkingCopyRecord>> {
        let table = match self.txn.open_table(WORKING_COPIES) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Err(PristineError::WorkingCopySchemaUnavailable);
            }
            Err(error) => return Err(error.into()),
        };
        let result = match table.get(id.as_bytes())? {
            Some(value) => decode_working_copy_row(id.as_bytes(), value.value()).map(Some),
            None => Ok(None),
        };
        result
    }

    fn list_working_copies(&self) -> PristineResult<Vec<WorkingCopyRecord>> {
        let table = match self.txn.open_table(WORKING_COPIES) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Err(PristineError::WorkingCopySchemaUnavailable);
            }
            Err(error) => return Err(error.into()),
        };
        let mut records = Vec::new();
        for entry in table.iter()? {
            let (key, value) = entry?;
            records.push(decode_working_copy_row(key.value(), value.value())?);
        }
        Ok(records)
    }
}

// TreeTxnT Implementation

impl TreeTxnT for ReadTxn {
    fn get_inode(&self, path: &str) -> PristineResult<Option<Inode>> {
        let table = self.txn.open_table(TREE)?;
        let result = table.get(path)?;
        match result {
            Some(value) => Ok(Some(Inode::new(value.value()))),
            None => Ok(None),
        }
    }

    fn get_directory_flags(&self, inode: Inode) -> PristineResult<Option<u8>> {
        let table = self.txn.open_table(DIRECTORIES)?;
        let result = table.get(inode.get())?;
        Ok(result.map(|v| v.value()))
    }

    fn get_path(&self, inode: Inode) -> PristineResult<Option<String>> {
        let table = self.txn.open_table(REV_TREE)?;
        let result = table.get(inode.get())?;
        match result {
            Some(value) => Ok(Some(value.value().to_string())),
            None => Ok(None),
        }
    }

    fn inode_position(&self, inode: Inode) -> PristineResult<Option<Position<NodeId>>> {
        let table = self.txn.open_table(INODES)?;
        let result = table.get(inode.get())?;
        match result {
            Some(value) => {
                let (change_id, pos) = decode_position(value.value());
                Ok(Some(Position::new(
                    NodeId::new(change_id),
                    ChangePosition::new(pos),
                )))
            }
            None => Ok(None),
        }
    }

    fn position_inode(&self, pos: Position<NodeId>) -> PristineResult<Option<Inode>> {
        let table = self.txn.open_table(REV_INODES)?;
        let key = encode_position(pos.change.get(), pos.pos.get());
        let result = table.get(&key)?;
        match result {
            Some(value) => Ok(Some(Inode::new(value.value()))),
            None => Ok(None),
        }
    }

    fn snapshot_inodes(&self) -> PristineResult<Vec<(Inode, Position<NodeId>)>> {
        let table = self.txn.open_table(INODES)?;
        let mut rows = Vec::new();
        for entry in table.iter()? {
            let (inode, position) = entry?;
            let (change_id, pos) = decode_position(position.value());
            rows.push((
                Inode::new(inode.value()),
                Position::new(NodeId::new(change_id), ChangePosition::new(pos)),
            ));
        }
        rows.sort_unstable();
        Ok(rows)
    }

    fn snapshot_rev_inodes(&self) -> PristineResult<Vec<(Position<NodeId>, Inode)>> {
        let table = self.txn.open_table(REV_INODES)?;
        let mut rows = Vec::new();
        for entry in table.iter()? {
            let (position, inode) = entry?;
            let (change_id, pos) = decode_position(position.value());
            rows.push((
                Position::new(NodeId::new(change_id), ChangePosition::new(pos)),
                Inode::new(inode.value()),
            ));
        }
        rows.sort_unstable();
        Ok(rows)
    }

    fn snapshot_directories(&self) -> PristineResult<Vec<(Inode, u8)>> {
        let table = self.txn.open_table(DIRECTORIES)?;
        let mut rows = Vec::new();
        for entry in table.iter()? {
            let (inode, flags) = entry?;
            rows.push((Inode::new(inode.value()), flags.value()));
        }
        rows.sort_unstable();
        Ok(rows)
    }

    fn snapshot_inode_graph_keys(&self) -> PristineResult<Vec<(Inode, GraphNode<NodeId>)>> {
        let table = self.txn.open_multimap_table(INODE_GRAPH)?;
        let mut rows = Vec::new();
        for entry in table.iter()? {
            let (key, _values) = entry?;
            let (inode, change_id, start, end) = decode_inode_vertex(key.value());
            rows.push((
                Inode::new(inode),
                GraphNode::new(
                    NodeId::new(change_id),
                    ChangePosition::new(start),
                    ChangePosition::new(end),
                ),
            ));
        }
        rows.sort_unstable();
        Ok(rows)
    }

    fn iter_tree(
        &self,
    ) -> PristineResult<Box<dyn Iterator<Item = Result<(String, Inode), PristineError>> + '_>> {
        let table = self.txn.open_table(TREE)?;
        // Collect to avoid lifetime issues while preserving lazy iterator errors.
        let results = collect_until_error(table.iter()?.map(|result| {
            result
                .map(|(key, value)| (key.value().to_string(), Inode::new(value.value())))
                .map_err(|error| PristineError::Storage(Box::new(error)))
        }));
        Ok(Box::new(results.into_iter()))
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
        let table = self.txn.open_multimap_table(INODE_GRAPH)?;

        let inode_id = inode.get();
        let start_key = encode_inode_vertex(inode_id, 0, 0, 0);
        let end_key = encode_inode_vertex(inode_id, u64::MAX, u64::MAX, u64::MAX);

        // Collect to avoid lifetime issues while preserving lazy iterator errors.
        let mut results = Vec::new();
        'entries: for result in table.range::<&[u8; 32]>(&start_key..=&end_key)? {
            let (key, values) = match result {
                Ok(entry) => entry,
                Err(error) => {
                    results.push(Err(PristineError::Storage(Box::new(error))));
                    break;
                }
            };
            let (_, change_id, start, end) = decode_inode_vertex(key.value());
            let node = GraphNode {
                change: NodeId::new(change_id),
                start: ChangePosition::new(start),
                end: ChangePosition::new(end),
            };

            for value in values {
                match value {
                    Ok(value) => {
                        let edge = deserialize_edge(value.value());
                        results.push(Ok((node, edge)));
                    }
                    Err(error) => {
                        results.push(Err(PristineError::Storage(Box::new(error))));
                        break 'entries;
                    }
                }
            }
        }

        Ok(Box::new(results.into_iter()))
    }

    fn get_file_index(&self, path: &str) -> PristineResult<Option<FileIndexMetadata>> {
        let table = self.txn.open_table(FILE_INDEX)?;
        match table.get(path)? {
            Some(value) => {
                let (secs, nanos, size, hash) = decode_file_index(value.value());
                Ok(Some((secs, nanos, size, hash)))
            }
            None => Ok(None),
        }
    }

    fn iter_file_index(&self) -> PristineResult<Vec<FileIndexEntry>> {
        let table = match self.txn.open_table(FILE_INDEX) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(PristineError::from(e)),
        };
        let mut entries = Vec::new();
        for result in table.iter()? {
            let (key, value) = result?;
            let path = key.value().to_string();
            let (secs, nanos, size, hash) = decode_file_index(value.value());
            entries.push((path, secs, nanos, size, hash));
        }
        Ok(entries)
    }
}

impl FileIndexV2TxnT for ReadTxn {
    fn get_file_index_v2_batch(
        &self,
        keys: &[FileIndexV2Key],
    ) -> PristineResult<Vec<Option<FileIndexV2Entry>>> {
        let table = match self.txn.open_table(FILE_INDEX_V2) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(vec![None; keys.len()]),
            Err(error) => return Err(error.into()),
        };
        let mut rows = Vec::with_capacity(keys.len());
        for key in keys {
            let encoded = key.encode();
            rows.push(match table.get(encoded.as_slice())? {
                Some(value) => Some(decode_file_index_v2(value.value())?),
                None => None,
            });
        }
        Ok(rows)
    }

    fn iter_file_index_v2(
        &self,
        working_copy: WorkingCopyId,
    ) -> PristineResult<Vec<(Vec<u8>, FileIndexV2Entry)>> {
        let table = match self.txn.open_table(FILE_INDEX_V2) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut rows = Vec::new();
        for result in table.iter()? {
            let (key, value) = result?;
            let key = FileIndexV2Key::decode(key.value())?;
            let entry = decode_file_index_v2(value.value())?;
            if key.working_copy == working_copy {
                rows.push((key.path, entry));
            }
        }
        Ok(rows)
    }
}

// Session data queries

impl ReadTxn {
    /// Sequence number recorded for a view's Merkle state, if that state is
    /// present (CB-12B receiving-side verification uses this to locate the
    /// ingested boundary between an old and a new view state).
    pub fn get_state_seq(
        &self,
        view_id: u64,
        merkle: &crate::types::Merkle,
    ) -> PristineResult<Option<u64>> {
        use crate::pristine::tables::{encode_view_merkle, STATES};

        let table = self.txn.open_table(STATES)?;
        match table.get(&encode_view_merkle(view_id, merkle.as_bytes()))? {
            Some(value) => Ok(Some(value.value())),
            None => Ok(None),
        }
    }

    /// Read the indexed metadata for an external session ID.
    pub fn get_session_record(
        &self,
        session_id: &str,
    ) -> PristineResult<Option<crate::change::session::SessionRecord>> {
        use crate::change::session::SessionRecord;

        let table = self.txn.open_table(SESSIONS)?;
        match table.get(session_id)? {
            Some(value) => SessionRecord::from_bytes(value.value())
                .map(Some)
                .map_err(|e| PristineError::Serialization {
                    message: format!("session record decode: {}", e),
                }),
            None => Ok(None),
        }
    }

    /// Read every indexed session record. Ordering is left to callers because
    /// recency depends on turn timestamps, not the session-id table key.
    pub fn list_session_records(
        &self,
    ) -> PristineResult<Vec<crate::change::session::SessionRecord>> {
        use crate::change::session::SessionRecord;

        let table = self.txn.open_table(SESSIONS)?;
        let mut records = Vec::new();
        for result in table.iter()? {
            let (_key, value) = result?;
            let record = SessionRecord::from_bytes(value.value()).map_err(|e| {
                PristineError::Serialization {
                    message: format!("session record decode: {}", e),
                }
            })?;
            records.push(record);
        }
        Ok(records)
    }

    /// Read all indexed turns for an external session ID in turn order.
    pub fn get_session_turns(
        &self,
        session_id: &str,
    ) -> PristineResult<Vec<crate::change::session::SessionTurn>> {
        use crate::change::session::{session_turn_namespace, SessionTurn};

        let table = self.txn.open_table(SESSION_TURNS)?;
        let namespace = session_turn_namespace(session_id);
        let start = crate::change::session::encode_session_turn_key(namespace, 0);
        let end = crate::change::session::encode_session_turn_key(namespace, u32::MAX);
        let mut turns = Vec::new();

        for result in table.range::<&[u8; 40]>(&start..=&end)? {
            let (_key, value) = result?;
            let turn = SessionTurn::from_bytes(value.value()).map_err(|e| {
                PristineError::Serialization {
                    message: format!("session turn decode: {}", e),
                }
            })?;
            if turn.session_id == session_id {
                turns.push(turn);
            }
        }

        turns.sort_by_key(|turn| turn.turn_number);
        Ok(turns)
    }

    /// Load an immutable session manifest by content hash.
    pub fn get_session_manifest(
        &self,
        hash: &crate::types::Hash,
    ) -> PristineResult<Option<crate::change::session::SessionManifest>> {
        use crate::change::session::SessionManifest;

        let table = self.txn.open_table(SESSION_MANIFESTS)?;
        match table.get(hash.as_bytes())? {
            Some(value) => SessionManifest::from_bytes(value.value())
                .map(Some)
                .map_err(|e| PristineError::Serialization {
                    message: format!("session manifest decode: {}", e),
                }),
            None => Ok(None),
        }
    }

    /// Resolve the latest manifest hash for an external session ID.
    pub fn get_session_head(&self, session_id: &str) -> PristineResult<Option<crate::types::Hash>> {
        let table = self.txn.open_table(SESSION_HEADS)?;
        Ok(table
            .get(session_id)?
            .map(|value| crate::types::Hash::from_bytes(*value.value())))
    }

    /// Read every mutable session head for index rebuilds.
    pub fn list_session_heads(&self) -> PristineResult<Vec<(String, crate::types::Hash)>> {
        let table = self.txn.open_table(SESSION_HEADS)?;
        let mut heads = Vec::new();
        for result in table.iter()? {
            let (session_id, hash) = result?;
            heads.push((
                session_id.value().to_string(),
                crate::types::Hash::from_bytes(*hash.value()),
            ));
        }
        Ok(heads)
    }

    /// Get all session events for a provenance graph.
    ///
    /// Returns events in sequence order (one per provenance node, for any
    /// agent). Empty only if the provenance has no nodes / no session data.
    pub fn get_session_events(
        &self,
        provenance_id: u64,
    ) -> PristineResult<Vec<crate::change::session::SessionEvent>> {
        use crate::change::session::{encode_session_prefix, SessionEvent};

        let table = self.txn.open_table(SESSION_EVENTS)?;
        let mut events = Vec::new();

        // Use a bounded prefix range instead of a full-table scan.
        // All keys for `provenance_id` lie in [prefix(id), prefix(id+1)).
        let start = encode_session_prefix(provenance_id);
        let end = encode_session_prefix(provenance_id.saturating_add(1));
        for result in table.range::<&[u8; 16]>(&start..&end)? {
            let (_key, value) = result?;
            match SessionEvent::from_bytes(value.value()) {
                Ok(event) => events.push(event),
                Err(e) => {
                    log::warn!("Failed to deserialize session event: {}", e);
                }
            }
        }

        // Events are already in seq order because keys encode (provenance_id,
        // seq) with the same byte layout, but sort explicitly to be safe.
        events.sort_by_key(|e| e.seq);
        Ok(events)
    }

    /// Get all todos for a provenance graph.
    ///
    /// Returns snapshots of all todo items from the turn, for any agent.
    /// Empty if the provenance recorded no todos / no session data.
    pub fn get_session_todos(
        &self,
        provenance_id: u64,
    ) -> PristineResult<Vec<crate::change::session::TodoSnapshot>> {
        use crate::change::session::{encode_session_prefix, TodoSnapshot};

        let table = self.txn.open_table(SESSION_TODOS)?;
        let mut todos = Vec::new();

        // Bounded prefix scan: all todo keys for this provenance_id lie in
        // [prefix(id), prefix(id+1)).
        let start = encode_session_prefix(provenance_id);
        let end = encode_session_prefix(provenance_id.saturating_add(1));
        for result in table.range::<&[u8; 16]>(&start..&end)? {
            let (_key, value) = result?;
            match TodoSnapshot::from_bytes(value.value()) {
                Ok(snapshot) => todos.push(snapshot),
                Err(e) => {
                    log::warn!("Failed to deserialize todo snapshot: {}", e);
                }
            }
        }

        Ok(todos)
    }

    /// Get phase timing breakdown for a provenance graph.
    ///
    /// Returns timing data for each phase in the turn. Populated from the
    /// per-phase token breakdown that Sherpa graphs carry; empty for agents
    /// that do not emit phase timing.
    pub fn get_session_phases(
        &self,
        provenance_id: u64,
    ) -> PristineResult<Vec<crate::change::session::PhaseTimingEntry>> {
        use crate::change::session::{encode_session_prefix, PhaseTimingEntry};

        let table = self.txn.open_table(SESSION_PHASES)?;
        let mut phases = Vec::new();

        // Bounded prefix scan: all phase keys for this provenance_id lie in
        // [prefix(id), prefix(id+1)).
        let start = encode_session_prefix(provenance_id);
        let end = encode_session_prefix(provenance_id.saturating_add(1));
        for result in table.range::<&[u8; 16]>(&start..&end)? {
            let (_key, value) = result?;
            match PhaseTimingEntry::from_bytes(value.value()) {
                Ok(entry) => phases.push(entry),
                Err(e) => {
                    log::warn!("Failed to deserialize phase timing entry: {}", e);
                }
            }
        }

        Ok(phases)
    }

    /// Get intent metadata for a provenance graph.
    ///
    /// Returns the intent entry when the graph carries a Goal node with intent
    /// `detail` (Sherpa graphs always do; other agents when available),
    /// `None` otherwise.
    pub fn get_session_intent(
        &self,
        provenance_id: u64,
    ) -> PristineResult<Option<crate::change::session::IntentEntry>> {
        use crate::change::session::IntentEntry;

        let table = self.txn.open_table(SESSION_INTENTS)?;

        match table.get(provenance_id)? {
            Some(guard) => match IntentEntry::from_bytes(guard.value()) {
                Ok(entry) => Ok(Some(entry)),
                Err(e) => {
                    log::warn!("Failed to deserialize intent entry: {}", e);
                    Ok(None)
                }
            },
            None => Ok(None),
        }
    }
}

// VaultTxnT Implementation

impl VaultTxnT for ReadTxn {
    fn get_vault_entry(&self, path: &str) -> PristineResult<Option<VaultEntry>> {
        let table = match self.txn.open_table(VAULT_ENTRIES) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(PristineError::from(e)),
        };

        let result = match table.get(path)? {
            Some(guard) => {
                let bytes = guard.value();
                let entry: VaultEntry =
                    postcard::from_bytes(bytes).map_err(|e| PristineError::Serialization {
                        message: format!("failed to deserialize VaultEntry at '{}': {}", path, e),
                    })?;
                Ok(Some(entry))
            }
            None => Ok(None),
        };
        result
    }

    fn list_vault_entries(
        &self,
        prefix: &str,
        entry_type_filter: Option<VaultEntryType>,
    ) -> PristineResult<Vec<VaultEntryMeta>> {
        let table = match self.txn.open_table(VAULT_ENTRIES) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(PristineError::from(e)),
        };

        let mut results = Vec::new();

        let iter = if prefix.is_empty() {
            table.iter()?
        } else {
            table.range(prefix..)?
        };

        for item in iter {
            let (key, value) = item?;
            let key_str = key.value();

            // Stop iterating once we pass the prefix range
            if !prefix.is_empty() && !key_str.starts_with(prefix) {
                break;
            }

            let entry: VaultEntry =
                postcard::from_bytes(value.value()).map_err(|e| PristineError::Serialization {
                    message: format!("failed to deserialize VaultEntry at '{}': {}", key_str, e),
                })?;

            // Apply type filter
            if let Some(ref filter) = entry_type_filter {
                if entry.entry_type != *filter {
                    continue;
                }
            }

            results.push(VaultEntryMeta {
                path: key_str.to_string(),
                entry_type: entry.entry_type,
                content_hash: entry.content_hash,
                content_size: entry.content_bytes.len(),
                updated_at: entry.updated_at,
            });
        }

        Ok(results)
    }

    fn get_vault_manifest(&self) -> PristineResult<VaultManifest> {
        let table = match self.txn.open_table(VAULT_MANIFEST) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(VaultManifest::default()),
            Err(e) => return Err(PristineError::from(e)),
        };

        let result = match table.get("manifest")? {
            Some(guard) => {
                let bytes = guard.value();
                let manifest: VaultManifest =
                    serde_json::from_slice(bytes).map_err(|e| PristineError::Serialization {
                        message: format!("failed to deserialize VaultManifest: {}", e),
                    })?;
                Ok(manifest)
            }
            None => Ok(VaultManifest::default()),
        };
        result
    }

    fn has_vault(&self) -> PristineResult<bool> {
        let table = match self.txn.open_table(VAULT_MANIFEST) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
            Err(e) => return Err(PristineError::from(e)),
        };

        match table.get("manifest")? {
            Some(_) => Ok(true),
            None => Ok(false),
        }
    }
}

// KgTxnT Implementation

impl KgTxnT for ReadTxn {
    fn get_kg_node(&self, id: &str) -> PristineResult<Option<KgNode>> {
        let table = match self.txn.open_table(KG_NODES) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(PristineError::from(e)),
        };
        match table.get(id)? {
            Some(value) => {
                let node: KgNode = serde_json::from_slice(value.value()).map_err(|e| {
                    PristineError::Serialization {
                        message: e.to_string(),
                    }
                })?;
                Ok(Some(node))
            }
            None => Ok(None),
        }
    }

    fn get_kg_edges_from(&self, node_id: &str) -> PristineResult<Vec<KgEdge>> {
        let from_table = match self.txn.open_multimap_table(KG_EDGES_FROM) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(PristineError::from(e)),
        };
        let edges_table = match self.txn.open_table(KG_EDGES) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(PristineError::from(e)),
        };

        let mut edges = Vec::new();
        let iter = from_table.get(node_id)?;
        for result in iter {
            let edge_key_guard = result?;
            let edge_key = edge_key_guard.value();
            if let Some(edge_data) = edges_table.get(edge_key)? {
                let edge: KgEdge = serde_json::from_slice(edge_data.value()).map_err(|e| {
                    PristineError::Serialization {
                        message: e.to_string(),
                    }
                })?;
                edges.push(edge);
            }
        }
        Ok(edges)
    }

    fn get_kg_edges_to(&self, node_id: &str) -> PristineResult<Vec<KgEdge>> {
        let to_table = match self.txn.open_multimap_table(KG_EDGES_TO) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(PristineError::from(e)),
        };
        let edges_table = match self.txn.open_table(KG_EDGES) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(PristineError::from(e)),
        };

        let mut edges = Vec::new();
        let iter = to_table.get(node_id)?;
        for result in iter {
            let edge_key_guard = result?;
            let edge_key = edge_key_guard.value();
            if let Some(edge_data) = edges_table.get(edge_key)? {
                let edge: KgEdge = serde_json::from_slice(edge_data.value()).map_err(|e| {
                    PristineError::Serialization {
                        message: e.to_string(),
                    }
                })?;
                edges.push(edge);
            }
        }
        Ok(edges)
    }

    fn kg_fts_search(&self, query: &str, limit: usize) -> PristineResult<Vec<KgNode>> {
        let fts_table = match self.txn.open_multimap_table(KG_FTS) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(PristineError::from(e)),
        };

        let tokens = tokenize_for_fts(query);
        if tokens.is_empty() {
            return Ok(Vec::new());
        }

        // Collect node IDs that match any token, count matches per node
        let mut hit_counts: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for token in &tokens {
            let iter = match fts_table.get(token.as_str()) {
                Ok(iter) => iter,
                Err(_) => continue,
            };
            for result in iter {
                let node_id_guard = result?;
                let node_id = node_id_guard.value().to_string();
                *hit_counts.entry(node_id).or_insert(0) += 1;
            }
        }

        // Sort by relevance: boost entity nodes (3x) and file nodes (2x)
        // over change nodes (1x). Entities and files are more useful for
        // code exploration than individual change records.
        let mut ranked: Vec<(String, usize)> = hit_counts.into_iter().collect();
        ranked.sort_by(|a, b| {
            let boost_a = if a.0.starts_with("entity:") {
                a.1 * 3
            } else if a.0.starts_with("file:") {
                a.1 * 2
            } else {
                a.1
            };
            let boost_b = if b.0.starts_with("entity:") {
                b.1 * 3
            } else if b.0.starts_with("file:") {
                b.1 * 2
            } else {
                b.1
            };
            boost_b.cmp(&boost_a)
        });
        ranked.truncate(limit);

        // Fetch full nodes
        let mut nodes = Vec::new();
        for (id, _) in &ranked {
            if let Some(node) = self.get_kg_node(id)? {
                nodes.push(node);
            }
        }
        Ok(nodes)
    }

    fn kg_fts_match_ids(&self, query: &str) -> PristineResult<Vec<(String, usize)>> {
        let fts_table = match self.txn.open_multimap_table(KG_FTS) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(PristineError::from(e)),
        };

        let tokens = tokenize_for_fts(query);
        if tokens.is_empty() {
            return Ok(Vec::new());
        }

        let mut hit_counts: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for token in &tokens {
            let iter = match fts_table.get(token.as_str()) {
                Ok(iter) => iter,
                Err(_) => continue,
            };
            for result in iter {
                let node_id_guard = result?;
                let node_id = node_id_guard.value().to_string();
                *hit_counts.entry(node_id).or_insert(0) += 1;
            }
        }

        Ok(hit_counts.into_iter().collect())
    }

    fn count_kg_nodes(&self) -> PristineResult<usize> {
        let table = match self.txn.open_table(KG_NODES) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
            Err(e) => return Err(PristineError::from(e)),
        };
        Ok(table.len()? as usize)
    }

    fn kg_node_ids_by_source(&self, sources: &[&str]) -> PristineResult<Vec<String>> {
        let table = match self.txn.open_table(KG_NODES) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(PristineError::from(e)),
        };
        let mut ids = Vec::new();
        for entry in table.iter()? {
            let (key, value) = entry?;
            if let Ok(node) = serde_json::from_slice::<KgNode>(value.value()) {
                if sources.contains(&node.source.as_str()) {
                    ids.push(key.value().to_string());
                }
            }
        }
        Ok(ids)
    }

    fn count_kg_edges(&self) -> PristineResult<usize> {
        let table = match self.txn.open_table(KG_EDGES) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
            Err(e) => return Err(PristineError::from(e)),
        };
        Ok(table.len()? as usize)
    }
}

// EmbeddingsTxnT Implementation

/// Compute cosine similarity between two vectors.
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        norm_a += a[i] * a[i];
        norm_b += b[i] * b[i];
    }
    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom == 0.0 {
        0.0
    } else {
        dot / denom
    }
}

impl EmbeddingsTxnT for ReadTxn {
    fn get_embedding(&self, path: &str, chunk_idx: u32) -> PristineResult<Option<EmbeddingRecord>> {
        let table = match self.txn.open_table(EMBEDDINGS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(PristineError::from(e)),
        };

        let key = encode_embedding_key(path, chunk_idx);
        let result = match table.get(key.as_str())? {
            Some(guard) => {
                let bytes = guard.value();
                let record: EmbeddingRecord =
                    postcard::from_bytes(bytes).map_err(|e| PristineError::Serialization {
                        message: format!(
                            "failed to deserialize EmbeddingRecord at '{}': {}",
                            key, e
                        ),
                    })?;
                Ok(Some(record))
            }
            None => Ok(None),
        };
        result
    }

    fn list_embeddings(&self, path: &str) -> PristineResult<Vec<(u32, EmbeddingRecord)>> {
        let table = match self.txn.open_table(EMBEDDINGS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(PristineError::from(e)),
        };

        let mut results = Vec::new();
        let prefix = format!("{}\0", path);

        let iter = table.range::<&str>(prefix.as_str()..)?;

        for item in iter {
            let (key_guard, value_guard) = item?;
            let key_str = key_guard.value();

            if !key_str.starts_with(&prefix) {
                break;
            }

            let (_, chunk_idx) = match decode_embedding_key(key_str) {
                Some(decoded) => decoded,
                None => continue,
            };

            let bytes = value_guard.value();
            let record: EmbeddingRecord =
                postcard::from_bytes(bytes).map_err(|e| PristineError::Serialization {
                    message: format!(
                        "failed to deserialize EmbeddingRecord at '{}': {}",
                        key_str, e
                    ),
                })?;
            results.push((chunk_idx, record));
        }

        Ok(results)
    }

    fn count_embeddings(&self) -> PristineResult<usize> {
        let table = match self.txn.open_table(EMBEDDINGS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
            Err(e) => return Err(PristineError::from(e)),
        };

        let count = table.len()? as usize;
        Ok(count)
    }

    fn search_embeddings(
        &self,
        query_vector: &[f32],
        top_k: usize,
    ) -> PristineResult<Vec<SearchResult>> {
        let table = match self.txn.open_table(EMBEDDINGS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(PristineError::from(e)),
        };

        let mut scored: Vec<SearchResult> = Vec::new();

        for item in table.iter()? {
            let (key_guard, value_guard) = item?;
            let key_str = key_guard.value();

            let (path, chunk_idx) = match decode_embedding_key(key_str) {
                Some(decoded) => decoded,
                None => continue,
            };

            let bytes = value_guard.value();
            let record: EmbeddingRecord =
                postcard::from_bytes(bytes).map_err(|e| PristineError::Serialization {
                    message: format!(
                        "failed to deserialize EmbeddingRecord at '{}': {}",
                        key_str, e
                    ),
                })?;

            let score = cosine_similarity(query_vector, &record.vector);

            scored.push(SearchResult {
                path: path.to_string(),
                chunk_idx,
                score,
                preview: record.preview,
            });
        }

        // Sort by descending score
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Return top-k
        scored.truncate(top_k);
        Ok(scored)
    }
}

// CrdtTxnT (read accessors) implementation for ReadTxn.
//
// `ReadTransaction::open_table` errors with `TableError::TableDoesNotExist`
// when the table hasn't been created yet — which on a fresh DB is the
// common case before any CRDT writes have landed.  Each method below
// treats that as "no data" and returns the empty answer, so callers can
// poll for CRDT state without first checking whether the apply layer has
// initialized the tables.

/// Helper: open a redb table, mapping `TableDoesNotExist` to `Ok(None)`.
macro_rules! crdt_table_opt {
    ($self:ident, $tbl:expr) => {
        match $self.txn.open_table($tbl) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        }
    };
}

macro_rules! crdt_multimap_opt {
    ($self:ident, $tbl:expr, $empty:expr) => {
        match $self.txn.open_multimap_table($tbl) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return $empty,
            Err(e) => return Err(e.into()),
        }
    };
}

impl crate::pristine::traits::CrdtTxnT for ReadTxn {
    fn get_crdt_trunk(
        &self,
        key: &[u8; 12],
    ) -> PristineResult<Option<crate::crdt::tables::SerializedTrunk>> {
        use crate::crdt::tables::{decode_trunk_value, TRUNKS};
        let table = crdt_table_opt!(self, TRUNKS);
        let result = match table.get(key)? {
            Some(guard) => {
                let bytes_copy: Vec<u8> = guard.value().to_vec();
                decode_trunk_value(&bytes_copy)
            }
            None => None,
        };
        Ok(result)
    }

    fn get_crdt_inode_trunk(&self, inode: u64) -> PristineResult<Option<[u8; 12]>> {
        use crate::crdt::tables::INODE_TRUNK;
        let table = crdt_table_opt!(self, INODE_TRUNK);
        let result = table.get(inode)?.map(|v| *v.value());
        Ok(result)
    }

    fn get_crdt_branch(
        &self,
        key: &[u8; 12],
    ) -> PristineResult<Option<crate::crdt::tables::SerializedBranch>> {
        use crate::crdt::tables::{decode_branch_value, BRANCHES};
        let table = crdt_table_opt!(self, BRANCHES);
        let result = match table.get(key)? {
            Some(guard) => {
                let bytes: [u8; 24] = *guard.value();
                Some(decode_branch_value(&bytes))
            }
            None => None,
        };
        Ok(result)
    }

    fn get_crdt_branch_after(&self, branch_key: &[u8; 12]) -> PristineResult<Option<[u8; 12]>> {
        use crate::crdt::tables::BRANCH_AFTER;
        let table = crdt_table_opt!(self, BRANCH_AFTER);
        let result = table.get(branch_key)?.map(|v| *v.value());
        Ok(result)
    }

    fn get_crdt_leaf(
        &self,
        key: &[u8; 12],
    ) -> PristineResult<Option<crate::crdt::tables::SerializedLeaf>> {
        use crate::crdt::tables::{decode_leaf_value, LEAVES};
        let table = crdt_table_opt!(self, LEAVES);
        let result = match table.get(key)? {
            Some(guard) => {
                let bytes: [u8; 22] = *guard.value();
                Some(decode_leaf_value(&bytes))
            }
            None => None,
        };
        Ok(result)
    }

    fn get_trunk_by_path(&self, path: &str) -> PristineResult<Option<crate::crdt::TrunkId>> {
        use crate::crdt::tables::{decode_trunk_id, PATH_TRUNK};
        let table = crdt_table_opt!(self, PATH_TRUNK);
        let result = table.get(path)?;
        match result {
            Some(guard) => {
                let key: [u8; 12] = *guard.value();
                drop(guard);
                Ok(Some(decode_trunk_id(&key)))
            }
            None => Ok(None),
        }
    }

    fn iter_trunk_branches(
        &self,
        trunk_key: &[u8; 12],
    ) -> PristineResult<Box<dyn Iterator<Item = Result<[u8; 12], PristineError>> + '_>> {
        use crate::crdt::tables::TRUNK_BRANCHES;
        let table = crdt_multimap_opt!(self, TRUNK_BRANCHES, Ok(Box::new(std::iter::empty())));
        let mut results: Vec<Result<[u8; 12], PristineError>> = Vec::new();
        let values = table.get(trunk_key)?;
        for value_result in values {
            match value_result {
                Ok(access) => results.push(Ok(*access.value())),
                Err(e) => results.push(Err(PristineError::Storage(Box::new(e)))),
            }
        }
        Ok(Box::new(results.into_iter()))
    }

    fn iter_branch_leaves(
        &self,
        branch_key: &[u8; 12],
    ) -> PristineResult<Box<dyn Iterator<Item = Result<[u8; 12], PristineError>> + '_>> {
        use crate::crdt::tables::BRANCH_LEAVES;
        let table = crdt_multimap_opt!(self, BRANCH_LEAVES, Ok(Box::new(std::iter::empty())));
        let mut results: Vec<Result<[u8; 12], PristineError>> = Vec::new();
        let values = table.get(branch_key)?;
        for value_result in values {
            match value_result {
                Ok(access) => results.push(Ok(*access.value())),
                Err(e) => results.push(Err(PristineError::Storage(Box::new(e)))),
            }
        }
        Ok(Box::new(results.into_iter()))
    }

    fn get_crdt_branch_vertex(
        &self,
        branch_key: &[u8; 12],
    ) -> PristineResult<Option<GraphNode<NodeId>>> {
        use crate::crdt::tables::{decode_vertex_position, BRANCH_VERTEX};
        let table = crdt_table_opt!(self, BRANCH_VERTEX);
        let result = table.get(branch_key)?;
        match result {
            Some(value) => {
                let bytes: [u8; 24] = *value.value();
                drop(value);
                Ok(Some(decode_vertex_position(&bytes)))
            }
            None => Ok(None),
        }
    }

    fn get_crdt_vertex_branch(
        &self,
        vertex_key: &[u8; 24],
    ) -> PristineResult<Option<crate::crdt::BranchId>> {
        use crate::crdt::tables::{decode_branch_id, VERTEX_BRANCH};
        let table = crdt_table_opt!(self, VERTEX_BRANCH);
        let result = table.get(vertex_key)?;
        match result {
            Some(value) => {
                let bytes: [u8; 12] = *value.value();
                drop(value);
                Ok(Some(decode_branch_id(&bytes)))
            }
            None => Ok(None),
        }
    }
}

// CachedGraphTxn Implementation

/// A graph transaction wrapper that caches the opened GRAPH table handle.
///
/// redb's `open_multimap_table` acquires a mutex, looks up the table
/// in the system catalog, and constructs a new handle on every call.
/// For `retrieve_graph` which calls `find_block` / `iter_adjacent`
/// hundreds of times per file, this overhead dominates — ~20ms per
/// table open under parallel contention.
///
/// `CachedGraphTxn` opens the GRAPH table once at construction and
/// reuses the handle for all subsequent operations, eliminating the
/// per-call overhead entirely.
pub struct CachedGraphTxn<'txn> {
    txn: &'txn ReadTxn,
    graph_table: redb::ReadOnlyMultimapTable<&'static [u8; 24], &'static [u8; 24]>,
    inode_graph_table: redb::ReadOnlyMultimapTable<&'static [u8; 32], &'static [u8; 24]>,
    /// Lazily-loaded per-change span index, so `find_block` / `find_block_end`
    /// are O(log n) instead of an O(n) GRAPH range scan per call. A `Mutex`
    /// (not `RefCell`) keeps the type `Sync`, since a `&CachedGraphTxn` is
    /// shared across rayon threads during parallel record/materialize; the
    /// parallel paths use the inode-linear walk and don't hit these finders,
    /// so the lock is effectively uncontended.
    span_index: std::sync::Mutex<crate::pristine::span_index::VertexSpanIndex>,
}

impl<'txn> CachedGraphTxn<'txn> {
    /// Create a cached graph transaction by opening GRAPH and INODE_GRAPH once.
    pub fn new(txn: &'txn ReadTxn) -> PristineResult<Self> {
        let graph_table = txn.txn.open_multimap_table(GRAPH)?;
        let inode_graph_table = txn.txn.open_multimap_table(INODE_GRAPH)?;
        Ok(Self {
            txn,
            graph_table,
            inode_graph_table,
            span_index: std::sync::Mutex::new(Default::default()),
        })
    }

    /// Load `change_id`'s spans into the in-memory index on first access.
    fn ensure_span_index(&self, change_id: u64) -> PristineResult<()> {
        if self.span_index.lock().unwrap().contains_change(change_id) {
            return Ok(());
        }
        let start_key = encode_vertex(change_id, 0, 0);
        let end_key = encode_vertex(change_id, u64::MAX, u64::MAX);
        let mut set = std::collections::BTreeSet::new();
        for result in self.graph_table.range::<&[u8; 24]>(&start_key..=&end_key)? {
            let (key, _values) = result?;
            let (v_change, v_start, v_end) = decode_vertex(key.value());
            if v_change != change_id {
                continue;
            }
            set.insert((v_start, v_end));
        }
        self.span_index
            .lock()
            .unwrap()
            .insert_change(change_id, set);
        Ok(())
    }
}

impl<'txn> GraphTxnT for CachedGraphTxn<'txn> {
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
        let table = &self.graph_table;
        let key = encode_vertex(node.change.get(), node.start.get(), node.end.get());

        let mut edges = Vec::new();
        for v in try_collect(table.get(&key)?)? {
            let bytes: &[u8; 24] = v.value();
            let edge = deserialize_edge(bytes);
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
        self.ensure_span_index(change_id)?;

        if let Some((s, e)) = self
            .span_index
            .lock()
            .unwrap()
            .find_block(change_id, target_pos)
        {
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
        self.ensure_span_index(change_id)?;

        if let Some((s, e)) = self
            .span_index
            .lock()
            .unwrap()
            .find_block_end(change_id, target_pos)
        {
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
        let table = &self.graph_table;
        let key = encode_vertex(node.change.get(), node.start.get(), node.end.get());
        let has = graph_presence(table.get(&key)?)?;
        Ok(has)
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

    fn change_deps_indexed_count(&self, change_id: NodeId) -> PristineResult<Option<u64>> {
        self.txn.change_deps_indexed_count(change_id)
    }

    fn get_rev_change_deps(&self, dep_hash: &Hash) -> PristineResult<Vec<NodeId>> {
        self.txn.get_rev_change_deps(dep_hash)
    }

    fn has_change_in_graph(&self, change_id: NodeId) -> PristineResult<bool> {
        let table = &self.graph_table;
        let start_key = encode_vertex(change_id.get(), 0, 0);
        let end_key = encode_vertex(change_id.get(), u64::MAX, u64::MAX);
        let has = graph_presence(table.range::<&[u8; 24]>(&start_key..=&end_key)?)?;
        Ok(has)
    }
}

impl<'txn> TreeTxnT for CachedGraphTxn<'txn> {
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

    fn snapshot_inodes(&self) -> PristineResult<Vec<(Inode, Position<NodeId>)>> {
        self.txn.snapshot_inodes()
    }

    fn snapshot_rev_inodes(&self) -> PristineResult<Vec<(Position<NodeId>, Inode)>> {
        self.txn.snapshot_rev_inodes()
    }

    fn snapshot_directories(&self) -> PristineResult<Vec<(Inode, u8)>> {
        self.txn.snapshot_directories()
    }

    fn snapshot_inode_graph_keys(&self) -> PristineResult<Vec<(Inode, GraphNode<NodeId>)>> {
        self.txn.snapshot_inode_graph_keys()
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

// ─────────────────────────────────────────────────────────────────────────
// CrdtTxnT — delegate to the inner ReadTxn (CRDT tables are global)
// ─────────────────────────────────────────────────────────────────────────

impl<'txn> crate::pristine::CrdtTxnT for CachedGraphTxn<'txn> {
    fn get_crdt_trunk(
        &self,
        key: &[u8; 12],
    ) -> PristineResult<Option<crate::crdt::tables::SerializedTrunk>> {
        self.txn.get_crdt_trunk(key)
    }

    fn get_crdt_inode_trunk(&self, inode: u64) -> PristineResult<Option<[u8; 12]>> {
        self.txn.get_crdt_inode_trunk(inode)
    }

    fn get_crdt_branch(
        &self,
        key: &[u8; 12],
    ) -> PristineResult<Option<crate::crdt::tables::SerializedBranch>> {
        self.txn.get_crdt_branch(key)
    }

    fn get_crdt_branch_after(&self, branch_key: &[u8; 12]) -> PristineResult<Option<[u8; 12]>> {
        self.txn.get_crdt_branch_after(branch_key)
    }

    fn get_crdt_leaf(
        &self,
        key: &[u8; 12],
    ) -> PristineResult<Option<crate::crdt::tables::SerializedLeaf>> {
        self.txn.get_crdt_leaf(key)
    }

    fn get_trunk_by_path(&self, path: &str) -> PristineResult<Option<crate::crdt::TrunkId>> {
        self.txn.get_trunk_by_path(path)
    }

    fn iter_trunk_branches(
        &self,
        trunk_key: &[u8; 12],
    ) -> PristineResult<Box<dyn Iterator<Item = PristineResult<[u8; 12]>> + '_>> {
        self.txn.iter_trunk_branches(trunk_key)
    }

    fn iter_branch_leaves(
        &self,
        branch_key: &[u8; 12],
    ) -> PristineResult<Box<dyn Iterator<Item = PristineResult<[u8; 12]>> + '_>> {
        self.txn.iter_branch_leaves(branch_key)
    }

    fn get_crdt_branch_vertex(
        &self,
        branch_key: &[u8; 12],
    ) -> PristineResult<Option<crate::types::GraphNode<crate::types::NodeId>>> {
        self.txn.get_crdt_branch_vertex(branch_key)
    }

    fn get_crdt_vertex_branch(
        &self,
        vertex_key: &[u8; 24],
    ) -> PristineResult<Option<crate::crdt::BranchId>> {
        self.txn.get_crdt_vertex_branch(vertex_key)
    }
}

// ─────────────────────────────────────────────────────────────────────────
// InodeAttrTxnT — delegate to the inner ReadTxn (attribute registers are global)
// ─────────────────────────────────────────────────────────────────────────

impl<'txn> crate::pristine::InodeAttrTxnT for CachedGraphTxn<'txn> {
    fn get_inode_attr_events(
        &self,
        position: Position<NodeId>,
        name: crate::change::InodeAttrName,
    ) -> PristineResult<Vec<crate::pristine::InodeAttrEvent>> {
        self.txn.get_inode_attr_events(position, name)
    }

    fn get_inode_attr_events_by_inode(
        &self,
        inode: Inode,
        name: crate::change::InodeAttrName,
    ) -> PristineResult<Vec<crate::pristine::InodeAttrEvent>> {
        self.txn.get_inode_attr_events_by_inode(inode, name)
    }
}

impl<'txn> ViewTxnT for CachedGraphTxn<'txn> {
    fn get_view_by_id(&self, id: u64) -> PristineResult<Option<ViewState>> {
        self.txn.get_view_by_id(id)
    }

    fn get_conflicts(&self, view_id: u64, inode: u64) -> PristineResult<Vec<StoredConflict>> {
        self.txn.get_conflicts(view_id, inode)
    }

    fn iter_conflicts(&self, view_id: u64) -> PristineResult<Vec<(u64, Vec<StoredConflict>)>> {
        self.txn.iter_conflicts(view_id)
    }

    fn snapshot_conflicts(&self) -> PristineResult<Vec<(u64, Inode, Vec<StoredConflict>)>> {
        self.txn.snapshot_conflicts()
    }

    fn get_view(&self, name: &str) -> PristineResult<Option<ViewState>> {
        self.txn.get_view(name)
    }

    fn snapshot_views(&self) -> PristineResult<Vec<(String, ViewState)>> {
        self.txn.snapshot_views()
    }

    fn list_views(&self) -> PristineResult<Vec<String>> {
        self.txn.list_views()
    }

    fn get_change_seq(&self, view: &ViewState, change_id: NodeId) -> PristineResult<Option<u64>> {
        self.txn.get_change_seq(view, change_id)
    }

    fn get_change_at_seq(&self, view: &ViewState, seq: u64) -> PristineResult<Option<NodeId>> {
        self.txn.get_change_at_seq(view, seq)
    }

    fn iter_changes(
        &self,
        view: &ViewState,
        from_seq: u64,
    ) -> PristineResult<Box<dyn Iterator<Item = Result<(u64, NodeId, Merkle), PristineError>> + '_>>
    {
        self.txn.iter_changes(view, from_seq)
    }
}

impl<'txn> crate::pristine::InodeGraphOps for CachedGraphTxn<'txn> {
    type InodeError = crate::pristine::PristineError;

    fn init_inode_adj(
        &self,
        inode: Inode,
        node: GraphNode<NodeId>,
        min_flag: EdgeFlags,
        max_flag: EdgeFlags,
    ) -> Result<crate::pristine::InodeAdjState, Self::InodeError> {
        // Use the cached INODE_GRAPH table handle directly instead of
        // delegating to self.txn (which would reopen the table).
        let mut adj = crate::pristine::InodeAdjState::new(inode, node, min_flag, max_flag);

        // Pre-load edges from the cached table on first init
        // (same logic as ReadTxn::next_inode_adj but using cached handle)
        let inode_id = inode.get();
        let key = encode_inode_vertex(
            inode_id,
            node.change.get(),
            node.start.get(),
            node.end.get(),
        );
        let mut edges = Vec::new();
        for v in try_collect(self.inode_graph_table.get(&key)?)? {
            let bytes: &[u8; 24] = v.value();
            let edge = deserialize_edge(bytes);
            let flag = edge.flag();
            if flag >= min_flag && flag <= max_flag {
                edges.push(edge);
            }
        }
        adj.set_edges(edges);

        Ok(adj)
    }

    fn next_inode_adj(
        &self,
        adj: &mut crate::pristine::InodeAdjState,
    ) -> Option<Result<SerializedGraphEdge, Self::InodeError>> {
        // Edges were pre-loaded in init_inode_adj, just iterate
        if adj.is_exhausted() {
            return None;
        }
        if adj.position < adj.edges.len() {
            let edge = adj.edges[adj.position];
            adj.advance();
            Some(Ok(edge))
        } else {
            adj.mark_exhausted();
            None
        }
    }

    fn find_block_in_inode(
        &self,
        inode: Inode,
        pos: Position<NodeId>,
    ) -> Result<Option<GraphNode<NodeId>>, Self::InodeError> {
        let table = &self.inode_graph_table;
        let inode_id = inode.get();
        let change_id = pos.change.get();
        let target_pos = pos.pos.get();

        // Fast path: probe exact start position
        let exact_start = encode_inode_vertex(inode_id, change_id, target_pos, 0);
        let exact_end = encode_inode_vertex(inode_id, change_id, target_pos, u64::MAX);
        let mut empty_match = None;

        for result in table.range::<&[u8; 32]>(&exact_start..=&exact_end)? {
            let (key, _) = result?;
            let (_, v_change, v_start, v_end) = decode_inode_vertex(key.value());
            if v_change != change_id || v_start != target_pos {
                continue;
            }
            if v_start != v_end {
                return Ok(Some(GraphNode {
                    change: NodeId::new(v_change),
                    start: ChangePosition::new(v_start),
                    end: ChangePosition::new(v_end),
                }));
            }
            if empty_match.is_none() {
                empty_match = Some(GraphNode {
                    change: NodeId::new(v_change),
                    start: ChangePosition::new(v_start),
                    end: ChangePosition::new(v_end),
                });
            }
        }

        if let Some(m) = empty_match {
            return Ok(Some(m));
        }

        // Slow path: scan all vertices for this change within the inode
        let start_key = encode_inode_vertex(inode_id, change_id, 0, 0);
        let end_key = encode_inode_vertex(inode_id, change_id, u64::MAX, u64::MAX);

        for result in table.range::<&[u8; 32]>(&start_key..=&end_key)? {
            let (key, _) = result?;
            let (_, v_change, v_start, v_end) = decode_inode_vertex(key.value());
            if v_change == change_id && v_start <= target_pos && target_pos < v_end {
                return Ok(Some(GraphNode {
                    change: NodeId::new(v_change),
                    start: ChangePosition::new(v_start),
                    end: ChangePosition::new(v_end),
                }));
            }
        }

        Ok(None)
    }

    fn find_block_end_in_inode(
        &self,
        inode: Inode,
        pos: Position<NodeId>,
    ) -> Result<Option<GraphNode<NodeId>>, Self::InodeError> {
        let table = &self.inode_graph_table;
        let inode_id = inode.get();
        let change_id = pos.change.get();
        let target_pos = pos.pos.get();

        // Check for empty vertex at exact position
        let empty_key = encode_inode_vertex(inode_id, change_id, target_pos, target_pos);
        if try_is_present(table.get(&empty_key)?)? {
            return Ok(Some(GraphNode {
                change: NodeId::new(change_id),
                start: ChangePosition::new(target_pos),
                end: ChangePosition::new(target_pos),
            }));
        }

        // Scan for vertex ending at this position
        let start_key = encode_inode_vertex(inode_id, change_id, 0, 0);
        let end_key = encode_inode_vertex(inode_id, change_id, target_pos, u64::MAX);

        for result in table.range::<&[u8; 32]>(&start_key..=&end_key)? {
            let (key, _) = result?;
            let (_, v_change, v_start, v_end) = decode_inode_vertex(key.value());
            if v_change != change_id {
                continue;
            }
            if v_end == target_pos && v_start < v_end {
                return Ok(Some(GraphNode {
                    change: NodeId::new(v_change),
                    start: ChangePosition::new(v_start),
                    end: ChangePosition::new(v_end),
                }));
            }
            if v_start <= target_pos && target_pos < v_end {
                return Ok(Some(GraphNode {
                    change: NodeId::new(v_change),
                    start: ChangePosition::new(v_start),
                    end: ChangePosition::new(v_end),
                }));
            }
        }

        Ok(None)
    }

    fn count_inode_vertices(&self, inode: Inode) -> Result<usize, Self::InodeError> {
        let table = &self.inode_graph_table;
        let inode_id = inode.get();
        let start_key = encode_inode_vertex(inode_id, 0, 0, 0);
        let end_key = encode_inode_vertex(inode_id, u64::MAX, u64::MAX, u64::MAX);

        let mut count = 0;
        let mut last: Option<(u64, u64, u64)> = None;

        for result in table.range::<&[u8; 32]>(&start_key..=&end_key)? {
            let (key, _) = result?;
            let (_, cid, s, e) = decode_inode_vertex(key.value());
            let current = (cid, s, e);
            if last != Some(current) {
                count += 1;
                last = Some(current);
            }
        }

        Ok(count)
    }

    fn inode_graph_is_populated(&self, inode: Inode) -> Result<bool, Self::InodeError> {
        let table = &self.inode_graph_table;
        let inode_id = inode.get();
        let start_key = encode_inode_vertex(inode_id, 0, 0, 0);
        let end_key = encode_inode_vertex(inode_id, u64::MAX, u64::MAX, u64::MAX);
        let populated = try_is_present(table.range::<&[u8; 32]>(&start_key..=&end_key)?)?;
        Ok(populated)
    }

    fn inode_graph_needs_view_filter(&self) -> bool {
        false
    }
}

// InodePreloadTxn Implementation

/// Pre-loaded inode graph for O(1) edge lookups during file traversal.
///
/// Instead of hitting the B-tree for every `find_block` and `iter_adjacent`
/// call (O(log N) per probe, thousands of probes per file), this struct
/// does ONE sequential range scan of `INODE_GRAPH` at construction and
/// loads all edges into a `HashMap`. The existing `retrieve_graph` DFS
/// then runs over this HashMap with O(1) lookups.
///
/// For a file with 21,867 vertices: scan = ~25ms, vs 14.2s of individual probes.
pub struct InodePreloadTxn<'txn> {
    txn: &'txn ReadTxn,
    /// All edges for this inode, keyed by source vertex.
    edges: std::collections::HashMap<GraphNode<NodeId>, Vec<SerializedGraphEdge>>,
    /// All vertices for this inode (for find_block), sorted by (change, start, end).
    vertices: Vec<GraphNode<NodeId>>,
}

impl<'txn> InodePreloadTxn<'txn> {
    /// Pre-load all edges for the given inode from INODE_GRAPH.
    pub fn new(txn: &'txn ReadTxn, inode: Inode) -> PristineResult<Self> {
        let table = txn.txn.open_multimap_table(INODE_GRAPH)?;
        Self::from_table(txn, inode, &table)
    }

    /// Pre-load using an already-opened INODE_GRAPH table handle.
    ///
    /// This avoids the per-file `open_multimap_table` overhead when
    /// processing many files in parallel — the caller opens the table
    /// once and passes the handle to each file's preloader.
    pub fn from_table(
        txn: &'txn ReadTxn,
        inode: Inode,
        table: &redb::ReadOnlyMultimapTable<&'static [u8; 32], &'static [u8; 24]>,
    ) -> PristineResult<Self> {
        let inode_id = inode.get();
        let start_key = encode_inode_vertex(inode_id, 0, 0, 0);
        let end_key = encode_inode_vertex(inode_id, u64::MAX, u64::MAX, u64::MAX);

        let mut edges: std::collections::HashMap<GraphNode<NodeId>, Vec<SerializedGraphEdge>> =
            std::collections::HashMap::new();
        let mut vertex_set: std::collections::HashSet<GraphNode<NodeId>> =
            std::collections::HashSet::new();

        for result in table.range::<&[u8; 32]>(&start_key..=&end_key)? {
            let (key, values) = result?;
            let (_, v_change, v_start, v_end) = decode_inode_vertex(key.value());

            let vertex = GraphNode {
                change: NodeId::new(v_change),
                start: ChangePosition::new(v_start),
                end: ChangePosition::new(v_end),
            };
            vertex_set.insert(vertex);

            let edge_list = collect_preload_edges(values, |value| {
                let bytes: &[u8; 24] = value.value();
                deserialize_edge(bytes)
            })?;
            edges.entry(vertex).or_default().extend(edge_list);
        }

        let mut vertices: Vec<GraphNode<NodeId>> = vertex_set.into_iter().collect();
        vertices.sort_by(|a, b| {
            a.change
                .get()
                .cmp(&b.change.get())
                .then(a.start.get().cmp(&b.start.get()))
                .then(a.end.get().cmp(&b.end.get()))
        });

        Ok(Self {
            txn,
            edges,
            vertices,
        })
    }

    /// Whether any pre-loaded edge for this inode is a DELETED edge whose
    /// introducing change is in the validated visibility closure.
    ///
    /// Distinguishes "content was deleted on this view" from "file is
    /// empty / has no visible content": a file recorded empty carries no
    /// visible DELETED edges, while a (partially or fully) deleted file
    /// does. Used by materialize to decide when a stale on-disk file whose
    /// visible content vanished should be removed.
    pub fn has_visible_deleted_edge(&self, visibility: &GraphVisibilityClosure) -> bool {
        self.edges.values().flatten().any(|edge| {
            edge.flag().contains(crate::types::EdgeFlags::DELETED)
                && visibility.contains(edge.introduced_by())
        })
    }
}

impl<'txn> GraphTxnT for InodePreloadTxn<'txn> {
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
        let filtered = if let Some(edge_list) = self.edges.get(&node) {
            edge_list
                .iter()
                .filter(|edge| {
                    let flag = edge.flag();
                    flag >= min_flag && flag <= max_flag
                })
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        Ok(AdjIterator::new(filtered))
    }

    fn find_block(&self, pos: Position<NodeId>) -> PristineResult<GraphNode<NodeId>> {
        if pos.change.is_root() {
            return Ok(GraphNode::ROOT);
        }

        let target_change = pos.change.get();
        let target_pos = pos.pos.get();
        let mut empty_match: Option<GraphNode<NodeId>> = None;

        for v in &self.vertices {
            if v.change.get() != target_change {
                continue;
            }
            let v_start = v.start.get();
            let v_end = v.end.get();

            // Prefer non-empty vertex containing this position
            if v_start != v_end && v_start <= target_pos && target_pos < v_end {
                return Ok(*v);
            }
            // Track empty vertex as fallback
            if v_start == v_end && v_start == target_pos && empty_match.is_none() {
                empty_match = Some(*v);
            }
        }

        if let Some(found) = empty_match {
            return Ok(found);
        }

        Err(PristineError::BlockNotFound {
            change: target_change,
            pos: target_pos,
        })
    }

    fn find_block_end(&self, pos: Position<NodeId>) -> PristineResult<GraphNode<NodeId>> {
        if pos.change.is_root() {
            return Ok(GraphNode::ROOT);
        }

        let target_change = pos.change.get();
        let target_pos = pos.pos.get();

        // Check for empty vertex at exact position first
        for v in &self.vertices {
            if v.change.get() == target_change
                && v.start.get() == target_pos
                && v.end.get() == target_pos
            {
                return Ok(*v);
            }
        }

        // Then check for vertex ending at this position
        for v in &self.vertices {
            if v.change.get() != target_change {
                continue;
            }
            let v_start = v.start.get();
            let v_end = v.end.get();

            if v_end == target_pos && v_start < v_end {
                return Ok(*v);
            }
            if v_start <= target_pos && target_pos < v_end {
                return Ok(*v);
            }
        }

        Err(PristineError::BlockNotFound {
            change: target_change,
            pos: target_pos,
        })
    }

    fn has_vertex(&self, node: GraphNode<NodeId>) -> PristineResult<bool> {
        Ok(self.edges.contains_key(&node))
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

    fn change_deps_indexed_count(&self, change_id: NodeId) -> PristineResult<Option<u64>> {
        self.txn.change_deps_indexed_count(change_id)
    }

    fn get_rev_change_deps(&self, dep_hash: &Hash) -> PristineResult<Vec<NodeId>> {
        self.txn.get_rev_change_deps(dep_hash)
    }

    fn has_change_in_graph(&self, change_id: NodeId) -> PristineResult<bool> {
        self.txn.has_change_in_graph(change_id)
    }
}

impl<'txn> TreeTxnT for InodePreloadTxn<'txn> {
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

    fn snapshot_inodes(&self) -> PristineResult<Vec<(Inode, Position<NodeId>)>> {
        self.txn.snapshot_inodes()
    }

    fn snapshot_rev_inodes(&self) -> PristineResult<Vec<(Position<NodeId>, Inode)>> {
        self.txn.snapshot_rev_inodes()
    }

    fn snapshot_directories(&self) -> PristineResult<Vec<(Inode, u8)>> {
        self.txn.snapshot_directories()
    }

    fn snapshot_inode_graph_keys(&self) -> PristineResult<Vec<(Inode, GraphNode<NodeId>)>> {
        self.txn.snapshot_inode_graph_keys()
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

impl<'txn> ViewTxnT for InodePreloadTxn<'txn> {
    fn get_view_by_id(&self, id: u64) -> PristineResult<Option<ViewState>> {
        self.txn.get_view_by_id(id)
    }

    fn get_conflicts(&self, view_id: u64, inode: u64) -> PristineResult<Vec<StoredConflict>> {
        self.txn.get_conflicts(view_id, inode)
    }

    fn iter_conflicts(&self, view_id: u64) -> PristineResult<Vec<(u64, Vec<StoredConflict>)>> {
        self.txn.iter_conflicts(view_id)
    }

    fn snapshot_conflicts(&self) -> PristineResult<Vec<(u64, Inode, Vec<StoredConflict>)>> {
        self.txn.snapshot_conflicts()
    }

    fn get_view(&self, name: &str) -> PristineResult<Option<ViewState>> {
        self.txn.get_view(name)
    }

    fn snapshot_views(&self) -> PristineResult<Vec<(String, ViewState)>> {
        self.txn.snapshot_views()
    }

    fn list_views(&self) -> PristineResult<Vec<String>> {
        self.txn.list_views()
    }

    fn get_change_seq(&self, view: &ViewState, change_id: NodeId) -> PristineResult<Option<u64>> {
        self.txn.get_change_seq(view, change_id)
    }

    fn get_change_at_seq(&self, view: &ViewState, seq: u64) -> PristineResult<Option<NodeId>> {
        self.txn.get_change_at_seq(view, seq)
    }

    fn iter_changes(
        &self,
        view: &ViewState,
        from_seq: u64,
    ) -> PristineResult<Box<dyn Iterator<Item = Result<(u64, NodeId, Merkle), PristineError>> + '_>>
    {
        self.txn.iter_changes(view, from_seq)
    }
}

// TagTxnT Implementation

impl TagTxnT for ReadTxn {
    fn get_tag(&self, view: &str, name: &str) -> PristineResult<Option<TagRecord>> {
        let key = format!("{}\0{}", view, name);
        let entity_id = {
            let table = match self.txn.open_table(TAG_NAME_INDEX) {
                Ok(table) => table,
                Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
                Err(e) => return Err(PristineError::from(e)),
            };
            match table.get(key.as_str())? {
                Some(value) => value.value(),
                None => return Ok(None),
            }
        };
        let table = match self.txn.open_table(TAG_RECORDS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(PristineError::from(e)),
        };
        match table.get(entity_id)? {
            Some(value) => {
                let record: TagRecord = postcard::from_bytes(value.value()).map_err(|e| {
                    PristineError::Serialization {
                        message: format!(
                            "failed to deserialize TagRecord for '{}\\0{}': {}",
                            view, name, e
                        ),
                    }
                })?;
                Ok(Some(record))
            }
            None => Ok(None),
        }
    }

    fn list_tags(&self, view: &str) -> PristineResult<Vec<TagRecord>> {
        let prefix = format!("{}\0", view);
        let index_table = match self.txn.open_table(TAG_NAME_INDEX) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(PristineError::from(e)),
        };
        let records_table = match self.txn.open_table(TAG_RECORDS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(PristineError::from(e)),
        };

        let mut results = Vec::new();
        for item in index_table.range(prefix.as_str()..)? {
            let (key, value) = item?;
            let key_str = key.value();
            if !key_str.starts_with(prefix.as_str()) {
                break;
            }
            let entity_id = value.value();
            if let Some(record_guard) = records_table.get(entity_id)? {
                let record: TagRecord =
                    postcard::from_bytes(record_guard.value()).map_err(|e| {
                        PristineError::Serialization {
                            message: format!(
                                "failed to deserialize TagRecord (entity_id={}): {}",
                                entity_id, e
                            ),
                        }
                    })?;
                results.push(record);
            }
        }
        Ok(results)
    }

    fn list_all_tags(&self) -> PristineResult<Vec<TagRecord>> {
        let table = match self.txn.open_table(TAG_RECORDS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(PristineError::from(e)),
        };

        let mut results = Vec::new();
        for item in table.iter()? {
            let (_key, value) = item?;
            let record: TagRecord =
                postcard::from_bytes(value.value()).map_err(|e| PristineError::Serialization {
                    message: format!("failed to deserialize TagRecord: {}", e),
                })?;
            results.push(record);
        }
        Ok(results)
    }

    fn find_tag_by_hash(&self, hash: &Hash) -> PristineResult<Option<TagRecord>> {
        // Look up the hash in INTERNAL to get entity_id
        let entity_id = {
            let table = self.txn.open_table(INTERNAL)?;
            match table.get(hash.as_bytes())? {
                Some(value) => value.value(),
                None => return Ok(None),
            }
        };

        // Check NODE_TYPES to verify this is a TAG entity
        {
            let table = self.txn.open_table(NODE_TYPES)?;
            match table.get(entity_id)? {
                Some(value) if value.value() == node_type::TAG => {}
                _ => return Ok(None),
            }
        }

        // Look up the tag record
        let table = match self.txn.open_table(TAG_RECORDS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(PristineError::from(e)),
        };
        match table.get(entity_id)? {
            Some(value) => {
                let record: TagRecord = postcard::from_bytes(value.value()).map_err(|e| {
                    PristineError::Serialization {
                        message: format!(
                            "failed to deserialize TagRecord (entity_id={}): {}",
                            entity_id, e
                        ),
                    }
                })?;
                Ok(Some(record))
            }
            None => Ok(None),
        }
    }
}

// ============================================================================
// IMMUTABLE GIT STATE BINDINGS
// ============================================================================

impl crate::pristine::BindingTxnT for ReadTxn {
    fn get_binding_bytes(&self, id: &[u8; 32]) -> PristineResult<Option<Vec<u8>>> {
        let table = match self.txn.open_table(BINDINGS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(error) => return Err(PristineError::from(error)),
        };
        Ok(table.get(id)?.map(|value| value.value().to_vec()))
    }

    fn iter_binding_ids(&self) -> PristineResult<Vec<[u8; 32]>> {
        let table = match self.txn.open_table(BINDINGS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(error) => return Err(PristineError::from(error)),
        };
        let mut ids = Vec::new();
        for row in table.iter()? {
            let (key, _) = row?;
            ids.push(*key.value());
        }
        Ok(ids)
    }
}

// ============================================================================
// GIT SHA INDEX
// ============================================================================

impl GitCommitClosureTxnT for ReadTxn {
    fn get_git_commit_closure(&self, sha: &str) -> PristineResult<Option<Vec<Hash>>> {
        let table = match self.txn.open_table(GIT_COMMIT_CLOSURES) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(error) => return Err(PristineError::from(error)),
        };
        match table.get(sha)? {
            Some(bytes) => {
                let value = bytes.value();
                if value.len() < 4 {
                    return Err(PristineError::Serialization {
                        message: format!(
                            "git commit closure for {sha} is truncated: {} byte(s)",
                            value.len()
                        ),
                    });
                }
                let count = u32::from_le_bytes([value[0], value[1], value[2], value[3]]) as usize;
                if value.len() != 4 + count * 32 {
                    return Err(PristineError::Serialization {
                        message: format!(
                            "git commit closure for {sha} claims {count} hashes but holds {} bytes",
                            value.len()
                        ),
                    });
                }
                let mut closure = Vec::with_capacity(count);
                for index in 0..count {
                    let start = 4 + index * 32;
                    let mut hash = [0u8; 32];
                    hash.copy_from_slice(&value[start..start + 32]);
                    closure.push(Hash::from_bytes(hash));
                }
                Ok(Some(closure))
            }
            None => Ok(None),
        }
    }

    fn has_git_commit_closure(&self, sha: &str) -> PristineResult<bool> {
        let table = match self.txn.open_table(GIT_COMMIT_CLOSURES) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
            Err(error) => return Err(PristineError::from(error)),
        };
        Ok(table.get(sha)?.is_some())
    }
}

impl BridgeEventCaptureTxnT for ReadTxn {
    fn get_bridge_event_capture(&self, hash: &[u8; 32]) -> PristineResult<Option<Vec<u8>>> {
        let table = match self.txn.open_table(BRIDGE_EVENT_CAPTURES) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(error) => return Err(PristineError::from(error)),
        };
        Ok(table.get(hash)?.map(|v| v.value().to_vec()))
    }

    fn get_bridge_event_capture_anchor(&self, hash: &[u8; 32]) -> PristineResult<Option<[u8; 32]>> {
        let table = match self.txn.open_table(BRIDGE_EVENT_CAPTURE_ANCHORS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(error) => return Err(PristineError::from(error)),
        };
        Ok(table.get(hash)?.map(|v| *v.value()))
    }

    fn get_bridge_ref_capture_token(
        &self,
        operation: &[u8; 32],
    ) -> PristineResult<Option<[u8; 32]>> {
        let table = match self.txn.open_table(BRIDGE_REF_CAPTURE_TOKENS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(error) => return Err(PristineError::from(error)),
        };
        Ok(table.get(operation)?.map(|v| *v.value()))
    }
}

impl RefMappingTxnT for ReadTxn {
    fn get_ref_mapping_bytes(&self, view_id: u64) -> PristineResult<Option<Vec<u8>>> {
        let table = match self.txn.open_table(REF_MAPPINGS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(error) => return Err(PristineError::from(error)),
        };
        Ok(table.get(view_id)?.map(|guard| guard.value().to_vec()))
    }

    fn iter_ref_mapping_bytes(&self) -> PristineResult<Vec<(u64, Vec<u8>)>> {
        let table = match self.txn.open_table(REF_MAPPINGS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(error) => return Err(PristineError::from(error)),
        };
        let mut rows = Vec::new();
        for row in table.iter()? {
            let (key, value) = row?;
            rows.push((key.value(), value.value().to_vec()));
        }
        Ok(rows)
    }
}

impl GitShaIndexTxnT for ReadTxn {
    fn get_by_git_sha(&self, sha: &str) -> PristineResult<Option<NodeId>> {
        let table = match self.txn.open_table(GIT_SHA_INDEX) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(PristineError::from(e)),
        };
        match table.get(sha)? {
            Some(v) => Ok(Some(NodeId::new(v.value()))),
            None => Ok(None),
        }
    }

    fn has_git_sha(&self, sha: &str) -> PristineResult<bool> {
        Ok(self.get_by_git_sha(sha)?.is_some())
    }

    fn list_git_shas(&self) -> PristineResult<Vec<String>> {
        let table = match self.txn.open_table(GIT_SHA_INDEX) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(error) => return Err(PristineError::from(error)),
        };
        let mut shas = Vec::new();
        for item in table.iter()? {
            let (key, _) = item?;
            shas.push(key.value().to_string());
        }
        Ok(shas)
    }

    fn find_by_git_sha_prefix(&self, prefix: &str) -> PristineResult<Option<NodeId>> {
        let table = match self.txn.open_table(GIT_SHA_INDEX) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(PristineError::from(e)),
        };
        // Range scan: prefix to prefix + "g" (one past hex range 0-f)
        let upper = format!("{}g", prefix);
        let mut matches = Vec::new();
        for item in table.range(prefix..upper.as_str())? {
            let (key, value) = item?;
            matches.push((key.value().to_string(), NodeId::new(value.value())));
            if matches.len() > 1 {
                return Err(PristineError::AmbiguousPrefix {
                    prefix: prefix.to_string(),
                    matches: matches.iter().map(|(k, _)| k.clone()).collect(),
                });
            }
        }
        Ok(matches.into_iter().next().map(|(_, id)| id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pristine::{InodeGraphOps, MutTxnT, Pristine};
    use tempfile::tempdir;

    #[test]
    fn test_read_empty_database() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let txn = pristine.read_txn().unwrap();

        // Empty database should return None for lookups
        assert!(txn.get_external(NodeId::new(1)).unwrap().is_none());
        assert!(txn.get_view("main").unwrap().is_none());
        assert!(txn.get_inode("test.txt").unwrap().is_none());
        assert!(txn.list_views().unwrap().is_empty());
    }

    #[test]
    fn healthy_graph_and_inode_multimap_reads_remain_ordered_and_filtered() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();
        let inode = Inode::new(7);

        let (change_id, node, global_only_change) = {
            let mut txn = pristine.write_txn().unwrap();
            let change_id = txn.register_change(&Hash::of(b"inode change")).unwrap();
            let node = GraphNode::new(change_id, ChangePosition::new(7), ChangePosition::new(7));
            let block_10 = SerializedGraphEdge::new(
                EdgeFlags::BLOCK,
                Position::new(change_id, ChangePosition::new(10)),
                change_id,
            );
            let folder_15 = SerializedGraphEdge::new(
                EdgeFlags::FOLDER,
                Position::new(change_id, ChangePosition::new(15)),
                change_id,
            );
            let block_20 = SerializedGraphEdge::new(
                EdgeFlags::BLOCK,
                Position::new(change_id, ChangePosition::new(20)),
                change_id,
            );

            for edge in [block_20, folder_15, block_10] {
                txn.put_graph(node, edge).unwrap();
                txn.put_inode_graph(inode, node, edge).unwrap();
            }

            let global_only_change = txn
                .register_change(&Hash::of(b"global-only change"))
                .unwrap();
            let global_node = GraphNode::new(
                global_only_change,
                ChangePosition::new(0),
                ChangePosition::new(1),
            );
            txn.put_graph(
                global_node,
                SerializedGraphEdge::new(
                    EdgeFlags::BLOCK,
                    Position::new(global_only_change, ChangePosition::new(0)),
                    global_only_change,
                ),
            )
            .unwrap();
            txn.commit().unwrap();
            (change_id, node, global_only_change)
        };

        let txn = pristine.read_txn().unwrap();
        let cached = CachedGraphTxn::new(&txn).unwrap();
        let expected_positions = vec![10, 20];

        let edges = txn
            .iter_adjacent(node, EdgeFlags::BLOCK, EdgeFlags::BLOCK)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            edges
                .iter()
                .map(|edge| edge.dest().pos.get())
                .collect::<Vec<_>>(),
            expected_positions
        );
        let cached_edges = cached
            .iter_adjacent(node, EdgeFlags::BLOCK, EdgeFlags::BLOCK)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(cached_edges, edges);

        assert!(txn.has_vertex(node).unwrap());
        assert!(cached.has_vertex(node).unwrap());
        assert!(txn.has_change_in_graph(change_id).unwrap());
        assert!(cached.has_change_in_graph(change_id).unwrap());
        assert_eq!(
            txn.find_block_end(Position::new(change_id, ChangePosition::new(7)))
                .unwrap(),
            node
        );
        assert_eq!(
            cached
                .find_block_end(Position::new(change_id, ChangePosition::new(7)))
                .unwrap(),
            node
        );

        let inode_edges = txn
            .iter_inode_vertices(inode)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(inode_edges.len(), 3);
        assert!(txn.inode_graph_is_populated(inode).unwrap());
        assert!(cached.inode_graph_is_populated(inode).unwrap());
        assert_eq!(
            txn.find_block_end_in_inode(inode, Position::new(change_id, ChangePosition::new(7)))
                .unwrap(),
            Some(node)
        );
        assert_eq!(
            cached
                .find_block_end_in_inode(inode, Position::new(change_id, ChangePosition::new(7)))
                .unwrap(),
            Some(node)
        );

        let mut cached_adj = cached
            .init_inode_adj(inode, node, EdgeFlags::BLOCK, EdgeFlags::BLOCK)
            .unwrap();
        let mut cached_inode_edges = Vec::new();
        while let Some(edge) = cached.next_inode_adj(&mut cached_adj) {
            cached_inode_edges.push(edge.unwrap());
        }
        assert_eq!(cached_inode_edges, edges);

        let preload = InodePreloadTxn::new(&txn, inode).unwrap();
        let preload_edges = preload
            .iter_adjacent(node, EdgeFlags::BLOCK, EdgeFlags::BLOCK)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(preload_edges, edges);
        assert!(preload.has_change_in_graph(global_only_change).unwrap());
    }
}
