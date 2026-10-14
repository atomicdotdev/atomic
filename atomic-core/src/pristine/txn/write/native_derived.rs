use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::pristine::path_claim::{
    decode_path_claim_event, encode_path_claim_event, PathClaimState, PATH_CLAIM_EVENT_SIZE,
    PATH_CLAIM_SCHEMA_KEY, PATH_CLAIM_SCHEMA_VERSION,
};
use crate::pristine::traits::{
    NativeDerivedIndexes, NativeDerivedIndexesMutTxnT, StoredConflictKind,
};

struct ValidatedNativeDerivedIndexes {
    path_claims: BTreeMap<String, BTreeSet<[u8; PATH_CLAIM_EVENT_SIZE]>>,
    tree: BTreeMap<String, Inode>,
    rev_tree: BTreeMap<Inode, String>,
    inodes: BTreeMap<Inode, Position<NodeId>>,
    rev_inodes: BTreeMap<Position<NodeId>, Inode>,
    directories: BTreeMap<Inode, u8>,
    conflicts: BTreeMap<(u64, Inode), Vec<u8>>,
    next_inode: u64,
}

fn invalid_replacement(message: impl Into<String>) -> PristineError {
    PristineError::Inconsistent {
        message: format!("invalid native derived indexes: {}", message.into()),
    }
}

fn conflict_kind_order(kind: StoredConflictKind) -> u8 {
    match kind {
        StoredConflictKind::Order => 0,
        StoredConflictKind::Cyclic => 1,
        StoredConflictKind::Zombie => 2,
        StoredConflictKind::Name => 3,
    }
}

impl WriteTxn<'_> {
    fn validate_native_derived_indexes(
        &self,
        replacement: &NativeDerivedIndexes,
    ) -> PristineResult<ValidatedNativeDerivedIndexes> {
        let mut path_claims = BTreeMap::<String, BTreeSet<_>>::new();
        let mut alive_path_claimants = BTreeSet::new();
        for entry in &replacement.path_claims {
            let encoded = encode_path_claim_event(&entry.event);
            let decoded = decode_path_claim_event(&encoded)?;
            if decoded != entry.event {
                return Err(invalid_replacement(format!(
                    "PATH_CLAIMS event for '{}' does not round-trip through schema version {}",
                    entry.path, PATH_CLAIM_SCHEMA_VERSION
                )));
            }
            if decoded.state == PathClaimState::Alive {
                alive_path_claimants.insert((entry.path.clone(), decoded.claim.claimant));
            }
            path_claims
                .entry(entry.path.clone())
                .or_default()
                .insert(encoded);
        }

        let mut tree = BTreeMap::new();
        for (path, inode) in &replacement.tree {
            if let Some(previous) = tree.insert(path.clone(), *inode) {
                if previous != *inode {
                    return Err(invalid_replacement(format!(
                        "TREE maps '{}' to both inode {} and inode {}",
                        path,
                        previous.get(),
                        inode.get()
                    )));
                }
            }
        }

        let mut rev_tree = BTreeMap::new();
        for (inode, path) in &replacement.rev_tree {
            if let Some(previous) = rev_tree.insert(*inode, path.clone()) {
                if previous != *path {
                    return Err(invalid_replacement(format!(
                        "REV_TREE maps inode {} to both '{}' and '{}'",
                        inode.get(),
                        previous,
                        path
                    )));
                }
            }
        }
        if tree.len() != rev_tree.len() {
            return Err(invalid_replacement(format!(
                "TREE contains {} rows but REV_TREE contains {} rows",
                tree.len(),
                rev_tree.len()
            )));
        }
        for (path, inode) in &tree {
            if rev_tree.get(inode) != Some(path) {
                return Err(invalid_replacement(format!(
                    "TREE maps '{}' to inode {}, but REV_TREE does not contain the exact inverse",
                    path,
                    inode.get()
                )));
            }
        }
        for (inode, path) in &rev_tree {
            if tree.get(path) != Some(inode) {
                return Err(invalid_replacement(format!(
                    "REV_TREE maps inode {} to '{}', but TREE does not contain the exact inverse",
                    inode.get(),
                    path
                )));
            }
        }

        let mut inodes = BTreeMap::new();
        for (inode, position) in &replacement.inodes {
            if let Some(previous) = inodes.insert(*inode, *position) {
                if previous != *position {
                    return Err(invalid_replacement(format!(
                        "INODES maps inode {} to both {}:{} and {}:{}",
                        inode.get(),
                        previous.change.get(),
                        previous.pos.get(),
                        position.change.get(),
                        position.pos.get()
                    )));
                }
            }
        }

        let mut rev_inodes = BTreeMap::new();
        for (position, inode) in &replacement.rev_inodes {
            if let Some(previous) = rev_inodes.insert(*position, *inode) {
                if previous != *inode {
                    return Err(invalid_replacement(format!(
                        "REV_INODES maps {}:{} to both inode {} and inode {}",
                        position.change.get(),
                        position.pos.get(),
                        previous.get(),
                        inode.get()
                    )));
                }
            }
        }
        if inodes.len() != rev_inodes.len() {
            return Err(invalid_replacement(format!(
                "INODES contains {} rows but REV_INODES contains {} rows",
                inodes.len(),
                rev_inodes.len()
            )));
        }
        for (inode, position) in &inodes {
            if rev_inodes.get(position) != Some(inode) {
                return Err(invalid_replacement(format!(
                    "INODES maps inode {} to {}:{}, but REV_INODES does not contain the exact inverse",
                    inode.get(),
                    position.change.get(),
                    position.pos.get()
                )));
            }
        }
        for (position, inode) in &rev_inodes {
            if inodes.get(inode) != Some(position) {
                return Err(invalid_replacement(format!(
                    "REV_INODES maps {}:{} to inode {}, but INODES does not contain the exact inverse",
                    position.change.get(),
                    position.pos.get(),
                    inode.get()
                )));
            }
        }

        let known_inodes: BTreeSet<Inode> = tree.values().chain(inodes.keys()).copied().collect();
        let allowed_directory_flags = directory_flags::DIR_EXPLICIT | directory_flags::DIR_EMPTY;
        let mut directories = BTreeMap::new();
        for (inode, flags) in &replacement.directories {
            if flags & !allowed_directory_flags != 0 {
                return Err(invalid_replacement(format!(
                    "DIRECTORIES inode {} has unsupported flags {flags:#04x}",
                    inode.get()
                )));
            }
            if !known_inodes.contains(inode) {
                return Err(invalid_replacement(format!(
                    "DIRECTORIES references unknown inode {}",
                    inode.get()
                )));
            }
            if let Some(previous) = directories.insert(*inode, *flags) {
                if previous != *flags {
                    return Err(invalid_replacement(format!(
                        "DIRECTORIES gives inode {} both {previous:#04x} and {flags:#04x}",
                        inode.get()
                    )));
                }
            }
        }

        let known_views: BTreeSet<u64> = self
            .snapshot_views()?
            .into_iter()
            .map(|(_, view)| view.id)
            .collect();

        let mut conflicts = BTreeMap::<(u64, Inode), Vec<StoredConflict>>::new();
        for (view_id, inode, records) in &replacement.conflicts {
            if records.is_empty() {
                return Err(invalid_replacement(format!(
                    "CONFLICTS row for view {view_id}, inode {} is empty",
                    inode.get()
                )));
            }
            if !known_views.contains(view_id) {
                return Err(invalid_replacement(format!(
                    "CONFLICTS references unknown view {view_id}"
                )));
            }
            let tree_path = rev_tree.get(inode);
            for conflict in records {
                if tree_path.is_some_and(|expected_path| conflict.path == *expected_path) {
                    continue;
                }

                let claimant = inodes.get(inode).ok_or_else(|| {
                    invalid_replacement(format!(
                        "CONFLICTS references unknown inode {}",
                        inode.get()
                    ))
                })?;
                let has_matching_alive_claim =
                    alive_path_claimants.contains(&(conflict.path.clone(), *claimant));
                if !has_matching_alive_claim {
                    return Err(invalid_replacement(format!(
                        "CONFLICTS view {view_id}, inode {} names unrelated path '{}'",
                        inode.get(),
                        conflict.path
                    )));
                }
            }
            conflicts
                .entry((*view_id, *inode))
                .or_default()
                .extend(records.iter().cloned());
        }

        let mut serialized_conflicts = BTreeMap::new();
        for (key, mut records) in conflicts {
            records.sort_by(|left, right| {
                conflict_kind_order(left.kind)
                    .cmp(&conflict_kind_order(right.kind))
                    .then_with(|| left.path.cmp(&right.path))
                    .then_with(|| left.line.cmp(&right.line))
                    .then_with(|| left.sides.cmp(&right.sides))
            });
            serialized_conflicts.insert(key, serialize_conflicts(&records)?);
        }

        let max_inode_graph_owner = self
            .snapshot_inode_graph_keys()?
            .into_iter()
            .map(|(inode, _)| inode.get())
            .max()
            .unwrap_or(0);
        let max_inode = known_inodes
            .iter()
            .chain(directories.keys())
            .chain(serialized_conflicts.keys().map(|(_, inode)| inode))
            .map(|inode| inode.get())
            .max()
            .unwrap_or(0)
            .max(max_inode_graph_owner);
        let next_inode = max_inode.saturating_add(1).max(1);

        Ok(ValidatedNativeDerivedIndexes {
            path_claims,
            tree,
            rev_tree,
            inodes,
            rev_inodes,
            directories,
            conflicts: serialized_conflicts,
            next_inode,
        })
    }

    fn clear_native_derived_indexes(&mut self) -> PristineResult<()> {
        {
            let mut metadata = self.txn.open_table(PRISTINE_META)?;
            metadata.remove(PATH_CLAIM_SCHEMA_KEY)?;
        }
        {
            let mut table = self.txn.open_multimap_table(PATH_CLAIMS)?;
            let paths = table
                .iter()?
                .map(|row| row.map(|(path, _)| path.value().to_string()))
                .collect::<Result<Vec<_>, _>>()?;
            for path in paths {
                table.remove_all(path.as_str())?;
            }
        }
        {
            let mut table = self.txn.open_table(TREE)?;
            let keys = table
                .iter()?
                .map(|row| row.map(|(key, _)| key.value().to_string()))
                .collect::<Result<Vec<_>, _>>()?;
            for key in keys {
                table.remove(key.as_str())?;
            }
        }
        {
            let mut table = self.txn.open_table(REV_TREE)?;
            let keys = table
                .iter()?
                .map(|row| row.map(|(key, _)| key.value()))
                .collect::<Result<Vec<_>, _>>()?;
            for key in keys {
                table.remove(key)?;
            }
        }
        {
            let mut table = self.txn.open_table(INODES)?;
            let keys = table
                .iter()?
                .map(|row| row.map(|(key, _)| key.value()))
                .collect::<Result<Vec<_>, _>>()?;
            for key in keys {
                table.remove(key)?;
            }
        }
        {
            let mut table = self.txn.open_table(REV_INODES)?;
            let keys = table
                .iter()?
                .map(|row| row.map(|(key, _)| *key.value()))
                .collect::<Result<Vec<_>, _>>()?;
            for key in keys {
                table.remove(&key)?;
            }
        }
        {
            let mut table = self.txn.open_table(DIRECTORIES)?;
            let keys = table
                .iter()?
                .map(|row| row.map(|(key, _)| key.value()))
                .collect::<Result<Vec<_>, _>>()?;
            for key in keys {
                table.remove(key)?;
            }
        }
        {
            let mut table = self.txn.open_table(CONFLICTS)?;
            let keys = table
                .iter()?
                .map(|row| row.map(|(key, _)| *key.value()))
                .collect::<Result<Vec<_>, _>>()?;
            for key in keys {
                table.remove(&key)?;
            }
        }
        Ok(())
    }
}

impl NativeDerivedIndexesMutTxnT for WriteTxn<'_> {
    fn replace_native_derived_indexes(
        &mut self,
        replacement: &NativeDerivedIndexes,
    ) -> PristineResult<()> {
        let expected_next_inode = self
            .pending_next_inode
            .map(PendingInodeReset::expected)
            .unwrap_or_else(|| self.next_inode.load(Ordering::SeqCst));
        let replacement = self.validate_native_derived_indexes(replacement)?;
        self.clear_native_derived_indexes()?;

        {
            let mut table = self.txn.open_multimap_table(PATH_CLAIMS)?;
            for (path, events) in &replacement.path_claims {
                for event in events {
                    table.insert(path.as_str(), event)?;
                }
            }
        }
        {
            let mut table = self.txn.open_table(TREE)?;
            for (path, inode) in &replacement.tree {
                table.insert(path.as_str(), inode.get())?;
            }
        }
        {
            let mut table = self.txn.open_table(REV_TREE)?;
            for (inode, path) in &replacement.rev_tree {
                table.insert(inode.get(), path.as_str())?;
            }
        }
        {
            let mut table = self.txn.open_table(INODES)?;
            for (inode, position) in &replacement.inodes {
                let encoded = encode_position(position.change.get(), position.pos.get());
                table.insert(inode.get(), &encoded)?;
            }
        }
        {
            let mut table = self.txn.open_table(REV_INODES)?;
            for (position, inode) in &replacement.rev_inodes {
                let encoded = encode_position(position.change.get(), position.pos.get());
                table.insert(&encoded, inode.get())?;
            }
        }
        {
            let mut table = self.txn.open_table(DIRECTORIES)?;
            for (inode, flags) in &replacement.directories {
                table.insert(inode.get(), *flags)?;
            }
        }
        {
            let mut table = self.txn.open_table(CONFLICTS)?;
            for ((view_id, inode), records) in &replacement.conflicts {
                let key = encode_view_seq(*view_id, inode.get());
                table.insert(&key, records.as_slice())?;
            }
        }

        self.pending_next_inode = Some(PendingInodeReset::new(
            expected_next_inode,
            replacement.next_inode,
        ));

        // This completion marker is intentionally the final database write.
        let mut metadata = self.txn.open_table(PRISTINE_META)?;
        metadata.insert(PATH_CLAIM_SCHEMA_KEY, PATH_CLAIM_SCHEMA_VERSION)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use redb::ReadableMultimapTable;
    use tempfile::tempdir;

    use super::*;
    use crate::pristine::{
        CachedGraphTxn, GraphTxnT, GraphVisibilityClosure, InodePreloadTxn, MutTxnT,
        PathClaimEntry, PathClaimEvent, PathClaimId, PathClaimKind, PathClaimMutTxnT,
        PathClaimState, PathClaimTxnT, Pristine, ReadTxn, StoredConflict, TreeTxnT, ViewGraph,
        ViewTxnT,
    };

    fn position(change: NodeId, pos: u64) -> Position<NodeId> {
        Position::new(change, ChangePosition::new(pos))
    }

    fn node(change: NodeId, start: u64, end: u64) -> GraphNode<NodeId> {
        GraphNode::new(change, ChangePosition::new(start), ChangePosition::new(end))
    }

    fn edge(change: NodeId, dest: u64) -> SerializedGraphEdge {
        SerializedGraphEdge::new(EdgeFlags::BLOCK, position(change, dest), change)
    }

    fn path_claim(change: NodeId, claimant: Position<NodeId>) -> PathClaimEvent {
        PathClaimEvent::new(
            change,
            0,
            PathClaimKind::Directory,
            PathClaimState::Alive,
            PathClaimId::new(claimant, GraphNode::ROOT, node(change, 0, 8), change),
        )
    }

    fn graph_rows(txn: &ReadTxn) -> Vec<(Vec<u8>, Vec<u8>)> {
        let table = txn.txn.open_multimap_table(GRAPH).unwrap();
        let mut rows = Vec::new();
        for row in table.iter().unwrap() {
            let (key, values) = row.unwrap();
            for value in values {
                rows.push((key.value().to_vec(), value.unwrap().value().to_vec()));
            }
        }
        rows
    }

    fn inode_graph_rows(txn: &ReadTxn) -> Vec<(Vec<u8>, Vec<u8>)> {
        let table = txn.txn.open_multimap_table(INODE_GRAPH).unwrap();
        let mut rows = Vec::new();
        for row in table.iter().unwrap() {
            let (key, values) = row.unwrap();
            for value in values {
                rows.push((key.value().to_vec(), value.unwrap().value().to_vec()));
            }
        }
        rows
    }

    fn view_history(txn: &ReadTxn, view: &ViewState) -> Vec<(u64, NodeId, Merkle)> {
        txn.iter_changes(view, 0)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    #[test]
    fn complete_snapshots_include_all_rows_and_delegate() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("pristine")).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        let change = txn.register_change(&Hash::of(b"snapshot rows")).unwrap();
        let first_inode = Inode::new(7);
        let second_inode = Inode::new(11);
        let first_position = position(change, 4);
        let second_position = position(change, 9);
        let first_node = node(change, 4, 4);
        let second_node = node(change, 9, 12);

        txn.put_inode(second_inode, second_position).unwrap();
        txn.put_inode(first_inode, first_position).unwrap();
        txn.put_directory(
            second_inode,
            directory_flags::DIR_EXPLICIT | directory_flags::DIR_EMPTY,
        )
        .unwrap();
        txn.put_directory(first_inode, directory_flags::DIR_EXPLICIT)
            .unwrap();
        txn.put_inode_graph(second_inode, second_node, edge(change, 11))
            .unwrap();
        txn.put_inode_graph(first_inode, first_node, edge(change, 5))
            .unwrap();
        txn.put_inode_graph(first_inode, first_node, edge(change, 6))
            .unwrap();

        let expected_inodes = vec![
            (first_inode, first_position),
            (second_inode, second_position),
        ];
        let expected_rev_inodes = vec![
            (first_position, first_inode),
            (second_position, second_inode),
        ];
        let expected_directories = vec![
            (first_inode, directory_flags::DIR_EXPLICIT),
            (
                second_inode,
                directory_flags::DIR_EXPLICIT | directory_flags::DIR_EMPTY,
            ),
        ];
        let expected_keys = vec![(first_inode, first_node), (second_inode, second_node)];

        assert_eq!(txn.snapshot_inodes().unwrap(), expected_inodes);
        assert_eq!(txn.snapshot_rev_inodes().unwrap(), expected_rev_inodes);
        assert_eq!(txn.snapshot_directories().unwrap(), expected_directories);
        assert_eq!(txn.snapshot_inode_graph_keys().unwrap(), expected_keys);
        txn.commit().unwrap();

        let txn = pristine.read_txn().unwrap();
        assert_eq!(txn.snapshot_inodes().unwrap(), expected_inodes);
        assert_eq!(txn.snapshot_rev_inodes().unwrap(), expected_rev_inodes);
        assert_eq!(txn.snapshot_directories().unwrap(), expected_directories);
        assert_eq!(txn.snapshot_inode_graph_keys().unwrap(), expected_keys);

        let cached = CachedGraphTxn::new(&txn).unwrap();
        assert_eq!(cached.snapshot_inodes().unwrap(), expected_inodes);
        assert_eq!(cached.snapshot_rev_inodes().unwrap(), expected_rev_inodes);
        assert_eq!(cached.snapshot_directories().unwrap(), expected_directories);
        assert_eq!(cached.snapshot_inode_graph_keys().unwrap(), expected_keys);

        let preload = InodePreloadTxn::new(&txn, first_inode).unwrap();
        assert_eq!(preload.snapshot_inodes().unwrap(), expected_inodes);
        assert_eq!(preload.snapshot_rev_inodes().unwrap(), expected_rev_inodes);
        assert_eq!(
            preload.snapshot_directories().unwrap(),
            expected_directories
        );
        assert_eq!(preload.snapshot_inode_graph_keys().unwrap(), expected_keys);
    }

    #[test]
    fn conflict_snapshot_includes_known_and_orphan_view_rows_through_wrappers() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("pristine")).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        let view = txn.open_or_create_view("main").unwrap();
        let orphan_view_id = view.id + 100;
        let known_inode = Inode::new(4);
        let orphan_inode = Inode::new(9);
        let known_conflicts = vec![StoredConflict {
            kind: StoredConflictKind::Order,
            path: "known.txt".to_string(),
            line: Some(2),
            sides: vec!["A".to_string(), "B".to_string()],
        }];
        let orphan_conflicts = vec![StoredConflict {
            kind: StoredConflictKind::Name,
            path: "orphan.txt".to_string(),
            line: None,
            sides: vec!["C".to_string(), "D".to_string()],
        }];

        txn.put_conflicts(orphan_view_id, orphan_inode.get(), &orphan_conflicts)
            .unwrap();
        txn.put_conflicts(view.id, known_inode.get(), &known_conflicts)
            .unwrap();
        let expected = vec![
            (view.id, known_inode, known_conflicts),
            (orphan_view_id, orphan_inode, orphan_conflicts),
        ];
        assert_eq!(txn.snapshot_conflicts().unwrap(), expected);
        txn.commit().unwrap();

        let txn = pristine.read_txn().unwrap();
        assert!(txn.get_view_by_id(orphan_view_id).unwrap().is_none());
        assert_eq!(txn.snapshot_conflicts().unwrap(), expected);

        let expected_views = vec![("main".to_string(), view.clone())];
        let cached = CachedGraphTxn::new(&txn).unwrap();
        assert_eq!(cached.snapshot_conflicts().unwrap(), expected);
        assert_eq!(cached.snapshot_views().unwrap(), expected_views);

        let preload = InodePreloadTxn::new(&txn, known_inode).unwrap();
        assert_eq!(preload.snapshot_conflicts().unwrap(), expected);
        assert_eq!(preload.snapshot_views().unwrap(), expected_views);

        let view_graph = ViewGraph::new(&txn, GraphVisibilityClosure::empty());
        assert_eq!(view_graph.snapshot_conflicts().unwrap(), expected);
        assert_eq!(view_graph.snapshot_views().unwrap(), expected_views);
    }

    #[test]
    fn view_snapshot_is_deterministic_and_list_propagates_malformed_rows() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("pristine")).unwrap();
        let (alpha, zeta) = {
            let mut txn = pristine.write_txn().unwrap();
            let zeta = txn.open_or_create_view("zeta").unwrap();
            let alpha = txn.open_or_create_view("alpha").unwrap();
            assert_eq!(
                txn.snapshot_views().unwrap(),
                vec![
                    ("alpha".to_string(), alpha.clone()),
                    ("zeta".to_string(), zeta.clone()),
                ]
            );
            assert_eq!(
                txn.list_views().unwrap(),
                vec!["alpha".to_string(), "zeta".to_string()]
            );
            txn.commit().unwrap();
            (alpha, zeta)
        };

        {
            let txn = pristine.read_txn().unwrap();
            assert_eq!(
                txn.snapshot_views().unwrap(),
                vec![
                    ("alpha".to_string(), alpha.clone()),
                    ("zeta".to_string(), zeta.clone()),
                ]
            );
        }

        let txn = pristine.write_txn().unwrap();
        {
            let mut table = txn.txn.open_table(VIEWS).unwrap();
            table
                .insert("broken", b"not a serialized view".as_slice())
                .unwrap();
        }
        assert!(matches!(
            txn.snapshot_views().unwrap_err(),
            PristineError::Serialization { .. }
        ));
        assert!(matches!(
            txn.list_views().unwrap_err(),
            PristineError::Serialization { .. }
        ));
        txn.commit().unwrap();

        let txn = pristine.read_txn().unwrap();
        assert!(matches!(
            txn.snapshot_views().unwrap_err(),
            PristineError::Serialization { .. }
        ));
        assert!(matches!(
            txn.list_views().unwrap_err(),
            PristineError::Serialization { .. }
        ));
    }

    #[test]
    fn conflict_snapshot_rejects_malformed_stored_payload() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("pristine")).unwrap();
        let key = encode_view_seq(7, 11);
        let txn = pristine.write_txn().unwrap();
        {
            let mut table = txn.txn.open_table(CONFLICTS).unwrap();
            table
                .insert(&key, b"not valid conflict json".as_slice())
                .unwrap();
        }
        assert!(matches!(
            txn.snapshot_conflicts().unwrap_err(),
            PristineError::Serialization { .. }
        ));
        txn.commit().unwrap();

        let txn = pristine.read_txn().unwrap();
        assert!(matches!(
            txn.snapshot_conflicts().unwrap_err(),
            PristineError::Serialization { .. }
        ));
    }

    #[test]
    fn contested_name_conflict_requires_matching_alive_claimant() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("pristine")).unwrap();
        let view = {
            let mut txn = pristine.write_txn().unwrap();
            let view = txn.open_or_create_view("main").unwrap();
            txn.commit().unwrap();
            view
        };
        let inode = Inode::new(30);
        let claimant = position(NodeId::new(8), 13);
        let path = "contested.txt".to_string();
        let conflict = StoredConflict {
            kind: StoredConflictKind::Name,
            path: path.clone(),
            line: None,
            sides: vec!["LEFT".to_string(), "RIGHT".to_string()],
        };
        let valid = NativeDerivedIndexes {
            path_claims: vec![PathClaimEntry::new(
                path.clone(),
                path_claim(NodeId::new(8), claimant),
            )],
            tree: Vec::new(),
            rev_tree: Vec::new(),
            inodes: vec![(inode, claimant)],
            rev_inodes: vec![(claimant, inode)],
            directories: Vec::new(),
            conflicts: vec![(view.id, inode, vec![conflict.clone()])],
        };
        {
            let mut txn = pristine.write_txn_immediate().unwrap();
            txn.replace_native_derived_indexes(&valid).unwrap();
            txn.commit().unwrap();
        }
        let txn = pristine.read_txn().unwrap();
        assert_eq!(
            txn.snapshot_conflicts().unwrap(),
            vec![(view.id, inode, vec![conflict])]
        );
        drop(txn);

        let mut unrelated = valid.clone();
        unrelated.path_claims = vec![PathClaimEntry::new(
            path,
            path_claim(NodeId::new(8), position(NodeId::new(8), 99)),
        )];
        let mut txn = pristine.write_txn_immediate().unwrap();
        let error = txn.replace_native_derived_indexes(&unrelated).unwrap_err();
        assert!(error.to_string().contains("unrelated path"));
        txn.commit().unwrap();

        let txn = pristine.read_txn().unwrap();
        assert_eq!(txn.snapshot_inodes().unwrap(), valid.inodes);
        assert_eq!(txn.iter_path_claims().unwrap(), valid.path_claims);
    }

    #[test]
    fn sibling_conflict_path_can_differ_from_current_rev_tree_with_claim_proof() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("pristine")).unwrap();
        let sibling_view = {
            let mut txn = pristine.write_txn().unwrap();
            txn.open_or_create_view("current").unwrap();
            let sibling = txn.open_or_create_view("sibling").unwrap();
            txn.commit().unwrap();
            sibling
        };
        let inode = Inode::new(35);
        let claimant = position(NodeId::new(9), 17);
        let conflict = StoredConflict {
            kind: StoredConflictKind::Zombie,
            path: "new.txt".to_string(),
            line: Some(4),
            sides: vec!["SIBLING".to_string()],
        };
        let replacement = NativeDerivedIndexes {
            path_claims: vec![PathClaimEntry::new(
                "new.txt",
                path_claim(NodeId::new(9), claimant),
            )],
            tree: vec![("old.txt".to_string(), inode)],
            rev_tree: vec![(inode, "old.txt".to_string())],
            inodes: vec![(inode, claimant)],
            rev_inodes: vec![(claimant, inode)],
            directories: Vec::new(),
            conflicts: vec![(sibling_view.id, inode, vec![conflict.clone()])],
        };

        let mut txn = pristine.write_txn_immediate().unwrap();
        txn.replace_native_derived_indexes(&replacement).unwrap();
        txn.commit().unwrap();

        let txn = pristine.read_txn().unwrap();
        assert_eq!(txn.get_path(inode).unwrap().as_deref(), Some("old.txt"));
        assert_eq!(
            txn.snapshot_conflicts().unwrap(),
            vec![(sibling_view.id, inode, vec![conflict])]
        );
    }

    #[test]
    fn sibling_view_content_conflict_accepts_matching_alive_claimant() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("pristine")).unwrap();
        let (current_view, sibling_view) = {
            let mut txn = pristine.write_txn().unwrap();
            let current = txn.open_or_create_view("current").unwrap();
            let sibling = txn.open_or_create_view("sibling").unwrap();
            txn.commit().unwrap();
            (current, sibling)
        };
        let current_inode = Inode::new(40);
        let sibling_inode = Inode::new(41);
        let current_position = position(NodeId::new(10), 4);
        let sibling_position = position(NodeId::new(11), 7);
        let sibling_path = "sibling.txt".to_string();
        let conflict = StoredConflict {
            kind: StoredConflictKind::Order,
            path: sibling_path.clone(),
            line: Some(5),
            sides: vec!["SIBLING_A".to_string(), "SIBLING_B".to_string()],
        };
        let replacement = NativeDerivedIndexes {
            path_claims: vec![PathClaimEntry::new(
                sibling_path,
                path_claim(NodeId::new(11), sibling_position),
            )],
            tree: vec![("current.txt".to_string(), current_inode)],
            rev_tree: vec![(current_inode, "current.txt".to_string())],
            inodes: vec![
                (current_inode, current_position),
                (sibling_inode, sibling_position),
            ],
            rev_inodes: vec![
                (current_position, current_inode),
                (sibling_position, sibling_inode),
            ],
            directories: Vec::new(),
            conflicts: vec![(sibling_view.id, sibling_inode, vec![conflict.clone()])],
        };

        let mut txn = pristine.write_txn_immediate().unwrap();
        txn.replace_native_derived_indexes(&replacement).unwrap();
        txn.commit().unwrap();

        let txn = pristine.read_txn().unwrap();
        assert_eq!(
            txn.get_path(current_inode).unwrap().as_deref(),
            Some("current.txt")
        );
        assert_eq!(txn.get_path(sibling_inode).unwrap(), None);
        assert_eq!(
            txn.snapshot_conflicts().unwrap(),
            vec![(sibling_view.id, sibling_inode, vec![conflict])]
        );
        assert!(txn.get_view_by_id(current_view.id).unwrap().is_some());
    }

    #[test]
    fn replacement_repairs_stale_rows_without_touching_authoritative_state() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("pristine")).unwrap();
        let candidate_inode = Inode::new(50);
        let candidate_path = "src/lib.rs".to_string();
        let candidate_position;
        let candidate_claim;
        let conflict = StoredConflict {
            kind: StoredConflictKind::Order,
            path: candidate_path.clone(),
            line: Some(3),
            sides: vec!["LEFT".to_string(), "RIGHT".to_string()],
        };
        let view;

        {
            let mut txn = pristine.write_txn_immediate().unwrap();
            let mut created_view = txn.open_or_create_view("main").unwrap();
            let hash = Hash::of(b"authoritative change");
            let change = txn.register_change(&hash).unwrap();
            let graph_node = node(change, 8, 8);
            candidate_position = position(change, 8);
            candidate_claim = path_claim(change, candidate_position);
            let graph_edge = edge(change, 9);
            txn.put_graph(graph_node, graph_edge).unwrap();
            txn.put_inode_graph(candidate_inode, graph_node, graph_edge)
                .unwrap();
            txn.put_change(&mut created_view, change, &hash).unwrap();
            txn.update_view(&created_view).unwrap();
            txn.put_file_index("cache-only", 1, 2, 3, &Hash::of(b"file index"))
                .unwrap();

            txn.put_path_claim("stale.txt", &path_claim(change, position(change, 99)))
                .unwrap();
            {
                let mut table = txn.txn.open_table(TREE).unwrap();
                table.insert("tree-only.txt", 90).unwrap();
            }
            {
                let mut table = txn.txn.open_table(REV_TREE).unwrap();
                table.insert(91, "reverse-only.txt").unwrap();
            }
            {
                let encoded = encode_position(change.get(), 90);
                let mut table = txn.txn.open_table(INODES).unwrap();
                table.insert(92, &encoded).unwrap();
            }
            {
                let encoded = encode_position(change.get(), 91);
                let mut table = txn.txn.open_table(REV_INODES).unwrap();
                table.insert(&encoded, 93).unwrap();
            }
            {
                let mut table = txn.txn.open_table(DIRECTORIES).unwrap();
                table.insert(94, 0xff).unwrap();
            }
            txn.put_conflicts(
                created_view.id,
                95,
                &[StoredConflict {
                    kind: StoredConflictKind::Zombie,
                    path: "stale.txt".to_string(),
                    line: None,
                    sides: Vec::new(),
                }],
            )
            .unwrap();
            view = created_view;
            txn.commit().unwrap();
        }

        let (graph_before, inode_graph_before, changes_before, history_before, file_index_before) = {
            let txn = pristine.read_txn().unwrap();
            (
                graph_rows(&txn),
                inode_graph_rows(&txn),
                txn.list_registered_changes().unwrap(),
                view_history(&txn, &view),
                txn.get_file_index("cache-only").unwrap(),
            )
        };

        let replacement = NativeDerivedIndexes {
            path_claims: vec![PathClaimEntry::new(candidate_path.clone(), candidate_claim)],
            tree: vec![(candidate_path.clone(), candidate_inode)],
            rev_tree: vec![(candidate_inode, candidate_path.clone())],
            inodes: vec![(candidate_inode, candidate_position)],
            rev_inodes: vec![(candidate_position, candidate_inode)],
            directories: vec![(candidate_inode, directory_flags::DIR_EXPLICIT)],
            conflicts: vec![(view.id, candidate_inode, vec![conflict.clone()])],
        };
        let mut txn = pristine.write_txn_immediate().unwrap();
        txn.replace_native_derived_indexes(&replacement).unwrap();
        txn.commit().unwrap();

        let txn = pristine.read_txn().unwrap();
        assert_eq!(txn.iter_path_claims().unwrap(), replacement.path_claims);
        assert_eq!(
            txn.iter_tree()
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            replacement.tree
        );
        assert_eq!(txn.iter_rev_tree_pairs().unwrap(), replacement.rev_tree);
        assert_eq!(txn.snapshot_inodes().unwrap(), replacement.inodes);
        assert_eq!(txn.snapshot_rev_inodes().unwrap(), replacement.rev_inodes);
        assert_eq!(txn.snapshot_directories().unwrap(), replacement.directories);
        assert_eq!(
            txn.iter_conflicts(view.id).unwrap(),
            vec![(candidate_inode.get(), vec![conflict])]
        );
        assert_eq!(
            txn.path_claim_schema_version().unwrap(),
            Some(PATH_CLAIM_SCHEMA_VERSION)
        );

        assert_eq!(graph_rows(&txn), graph_before);
        assert_eq!(inode_graph_rows(&txn), inode_graph_before);
        assert_eq!(txn.list_registered_changes().unwrap(), changes_before);
        assert_eq!(view_history(&txn, &view), history_before);
        assert_eq!(txn.get_file_index("cache-only").unwrap(), file_index_before);
    }

    #[test]
    fn aborted_replacement_does_not_publish_lower_inode_counter() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let existing_inode = Inode::new(50);
        {
            let pristine = Pristine::open(&db_path).unwrap();
            let mut txn = pristine.write_txn().unwrap();
            txn.put_tree("existing.txt", existing_inode).unwrap();
            txn.commit().unwrap();
        }

        let pristine = Pristine::open_existing(&db_path).unwrap();
        {
            let mut txn = pristine.write_txn_immediate().unwrap();
            txn.replace_native_derived_indexes(&NativeDerivedIndexes::default())
                .unwrap();
            assert_eq!(txn.alloc_inode().unwrap().get(), 1);
            assert_eq!(txn.alloc_inode().unwrap().get(), 2);
            txn.abort().unwrap();
        }

        let txn = pristine.read_txn().unwrap();
        assert_eq!(txn.get_inode("existing.txt").unwrap(), Some(existing_inode));
        drop(txn);

        let mut txn = pristine.write_txn().unwrap();
        assert_eq!(txn.alloc_inode().unwrap().get(), 51);
        txn.abort().unwrap();
    }

    #[test]
    fn orphan_inode_graph_scope_sets_committed_allocator_floor() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("pristine")).unwrap();
        let orphan_inode = Inode::new(100);
        let graph_node;
        {
            let mut txn = pristine.write_txn().unwrap();
            let change = txn
                .register_change(&Hash::of(b"orphan inode graph scope"))
                .unwrap();
            graph_node = node(change, 3, 3);
            txn.put_inode_graph(orphan_inode, graph_node, edge(change, 4))
                .unwrap();
            txn.commit().unwrap();
        }

        {
            let mut txn = pristine.write_txn_immediate().unwrap();
            txn.replace_native_derived_indexes(&NativeDerivedIndexes::default())
                .unwrap();
            txn.commit().unwrap();
        }

        let txn = pristine.read_txn().unwrap();
        assert_eq!(
            txn.snapshot_inode_graph_keys().unwrap(),
            vec![(orphan_inode, graph_node)]
        );
        drop(txn);

        let mut txn = pristine.write_txn().unwrap();
        assert_eq!(txn.alloc_inode().unwrap().get(), 101);
        txn.abort().unwrap();
    }

    #[test]
    fn invalid_replacement_is_rejected_before_any_mutation() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("pristine")).unwrap();
        let inode = Inode::new(8);
        let original_position = position(NodeId::new(2), 5);
        let original = NativeDerivedIndexes {
            path_claims: Vec::new(),
            tree: vec![("original.txt".to_string(), inode)],
            rev_tree: vec![(inode, "original.txt".to_string())],
            inodes: vec![(inode, original_position)],
            rev_inodes: vec![(original_position, inode)],
            directories: vec![(inode, directory_flags::DIR_EXPLICIT)],
            conflicts: Vec::new(),
        };
        {
            let mut txn = pristine.write_txn_immediate().unwrap();
            txn.replace_native_derived_indexes(&original).unwrap();
            txn.commit().unwrap();
        }

        let mut invalid_bijection = original.clone();
        invalid_bijection.tree = vec![("replacement.txt".to_string(), Inode::new(9))];
        let mut txn = pristine.write_txn_immediate().unwrap();
        let error = txn
            .replace_native_derived_indexes(&invalid_bijection)
            .unwrap_err();
        assert!(error.to_string().contains("TREE"));
        txn.commit().unwrap();

        let txn = pristine.read_txn().unwrap();
        assert_eq!(
            txn.iter_tree()
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            original.tree
        );
        assert_eq!(txn.iter_rev_tree_pairs().unwrap(), original.rev_tree);
        assert_eq!(txn.snapshot_inodes().unwrap(), original.inodes);
        assert_eq!(txn.snapshot_rev_inodes().unwrap(), original.rev_inodes);
        assert_eq!(txn.snapshot_directories().unwrap(), original.directories);
        drop(txn);

        let mut invalid_flags = original.clone();
        invalid_flags.directories = vec![(inode, 0x80)];
        let mut txn = pristine.write_txn_immediate().unwrap();
        let error = txn
            .replace_native_derived_indexes(&invalid_flags)
            .unwrap_err();
        assert!(error.to_string().contains("unsupported flags"));
        txn.abort().unwrap();

        let txn = pristine.read_txn().unwrap();
        assert_eq!(txn.snapshot_directories().unwrap(), original.directories);
        assert_eq!(
            txn.path_claim_schema_version().unwrap(),
            Some(PATH_CLAIM_SCHEMA_VERSION)
        );
    }
}
