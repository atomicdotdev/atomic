use super::*;
use crate::pristine::txn::helpers::collect_until_error;

// TreeTxnT Implementation

impl<'a> TreeTxnT for WriteTxn<'a> {
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
        let guard = table.get(path)?;
        match guard {
            Some(value) => {
                let bytes = value.value();
                let (secs, nanos, size, hash) = decode_file_index(bytes);
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

impl FileIndexV2TxnT for WriteTxn<'_> {
    fn get_file_index_v2_batch(
        &self,
        keys: &[FileIndexV2Key],
    ) -> PristineResult<Vec<Option<FileIndexV2Entry>>> {
        let table = self.txn.open_table(FILE_INDEX_V2)?;
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
        let table = self.txn.open_table(FILE_INDEX_V2)?;
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

impl FileIndexV2MutTxnT for WriteTxn<'_> {
    fn put_file_index_v2_batch(
        &mut self,
        entries: &[(FileIndexV2Key, FileIndexV2Entry)],
    ) -> PristineResult<()> {
        let mut table = self.txn.open_table(FILE_INDEX_V2)?;
        for (key, entry) in entries {
            let key = key.encode();
            let value = encode_file_index_v2(entry)?;
            table.insert(key.as_slice(), value.as_slice())?;
        }
        Ok(())
    }

    fn del_file_index_v2_batch(&mut self, keys: &[FileIndexV2Key]) -> PristineResult<()> {
        let mut table = self.txn.open_table(FILE_INDEX_V2)?;
        for key in keys {
            let key = key.encode();
            table.remove(key.as_slice())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::InodeKind;
    use crate::pristine::{FileIndexTimestamp, MutTxnT, PathClaimTxnT, Pristine};
    use tempfile::tempdir;

    #[test]
    fn healthy_write_tree_and_inode_iterators_return_all_values() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("pristine")).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        let inode = Inode::new(11);
        txn.put_tree("src/lib.rs", inode).unwrap();

        let change_id = txn
            .register_change(&Hash::of(b"write inode graph"))
            .unwrap();
        let node = GraphNode::new(change_id, ChangePosition::new(0), ChangePosition::new(4));
        for position in [2, 1] {
            txn.put_inode_graph(
                inode,
                node,
                SerializedGraphEdge::new(
                    EdgeFlags::BLOCK,
                    Position::new(change_id, ChangePosition::new(position)),
                    change_id,
                ),
            )
            .unwrap();
        }

        let tree = txn
            .iter_tree()
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(tree, vec![("src/lib.rs".to_string(), inode)]);

        let inode_entries = txn
            .iter_inode_vertices(inode)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            inode_entries
                .iter()
                .map(|(_, edge)| edge.dest().pos.get())
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        txn.validate_tree_bijection().unwrap();
    }

    #[test]
    fn put_tree_rejects_path_overwrite_and_inode_rebind() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("pristine")).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        let first = Inode::new(1);
        let second = Inode::new(2);

        txn.put_tree("same.txt", first).unwrap();
        assert!(txn.put_tree("same.txt", second).is_err());
        assert!(txn.put_tree("other.txt", first).is_err());
        assert_eq!(txn.get_inode("same.txt").unwrap(), Some(first));
        assert_eq!(txn.get_inode("other.txt").unwrap(), None);
        assert_eq!(txn.get_path(first).unwrap().as_deref(), Some("same.txt"));
        assert_eq!(txn.get_path(second).unwrap(), None);
        txn.validate_tree_bijection().unwrap();
    }

    #[test]
    fn repair_rev_tree_bijection_removes_stale_and_backfills_missing() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("pristine")).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        let live = Inode::new(7);
        let stale = Inode::new(3);

        txn.put_tree("kept.txt", live).unwrap();
        // Simulate the 0.17.x re-bind residue: a reverse row for a dead inode
        // that no forward row maps, plus a forward row whose inverse is
        // missing. Both fail the bijection validation a later tree write
        // performs.
        {
            let mut table = txn
                .open_table_for_test(crate::pristine::tables::REV_TREE)
                .unwrap();
            table.insert(stale.get(), "kept.txt").unwrap();
        }
        txn.put_tree("gap.txt", Inode::new(9)).unwrap();
        // Reverse row for inode 9 exists (put_tree writes both sides); drop
        // it to model a missing inverse instead.
        {
            let mut table = txn
                .open_table_for_test(crate::pristine::tables::REV_TREE)
                .unwrap();
            table.remove(Inode::new(9).get()).unwrap();
        }
        assert!(txn.validate_tree_bijection().is_err());

        let (removed, inserted) = txn.repair_rev_tree_bijection().unwrap();
        assert_eq!(removed, 1, "the stale dead-inode row is removed");
        assert_eq!(inserted, 1, "the missing inverse is backfilled");
        assert_eq!(txn.get_inode("kept.txt").unwrap(), Some(live));
        assert_eq!(txn.get_path(live).unwrap().as_deref(), Some("kept.txt"));
        txn.validate_tree_bijection().unwrap();
    }

    #[test]
    fn file_index_v2_reopens_and_coexists_with_unchanged_legacy_rows() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("pristine");
        let working_copy = WorkingCopyId::from_bytes([8; 16]);
        let key = FileIndexV2Key::new(working_copy, b"src/lib.rs".to_vec()).unwrap();
        let timestamp = FileIndexTimestamp::new(123, 456_789_012).unwrap();
        let entry = FileIndexV2Entry::complete(
            1,
            2,
            timestamp,
            FileIndexTimestamp::new(124, 7).unwrap(),
            9,
            Hash::of(b"content"),
            0o755,
            InodeKind::Regular,
            timestamp,
            Hash::of(b"policy"),
        )
        .unwrap();
        let legacy_hash = Hash::of(b"legacy");

        {
            let pristine = Pristine::open(&path).unwrap();
            let mut txn = pristine.write_txn().unwrap();
            txn.put_file_index("src/lib.rs", 10, 20, 30, &legacy_hash)
                .unwrap();
            txn.put_file_index_v2(&key, &entry).unwrap();
            txn.commit().unwrap();
        }
        {
            let pristine = Pristine::open(&path).unwrap();
            let txn = pristine.read_txn().unwrap();
            assert_eq!(
                txn.get_file_index("src/lib.rs").unwrap(),
                Some((10, 20, 30, legacy_hash))
            );
            assert_eq!(
                txn.get_file_index_v2(working_copy, b"src/lib.rs").unwrap(),
                Some(entry)
            );
        }
    }

    #[test]
    fn file_index_v2_batch_get_iterate_and_delete_are_working_copy_scoped() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("pristine")).unwrap();
        let first_copy = WorkingCopyId::from_bytes([1; 16]);
        let second_copy = WorkingCopyId::from_bytes([2; 16]);
        let timestamp = FileIndexTimestamp::new(1, 2).unwrap();
        let entry = FileIndexV2Entry::complete(
            3,
            4,
            timestamp,
            timestamp,
            5,
            Hash::of(b"content"),
            0o644,
            InodeKind::Regular,
            FileIndexTimestamp::new(2, 0).unwrap(),
            Hash::of(b"policy"),
        )
        .unwrap();
        let first = FileIndexV2Key::new(first_copy, b"a".to_vec()).unwrap();
        let second = FileIndexV2Key::new(first_copy, b"b".to_vec()).unwrap();
        let other = FileIndexV2Key::new(second_copy, b"a".to_vec()).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        txn.put_file_index_v2_batch(&[
            (first.clone(), entry.clone()),
            (second.clone(), entry.clone()),
            (other, entry.clone()),
        ])
        .unwrap();
        assert_eq!(
            txn.get_file_index_v2_batch(&[first.clone(), second.clone()])
                .unwrap(),
            vec![Some(entry.clone()), Some(entry.clone())]
        );
        assert_eq!(
            txn.iter_file_index_v2(first_copy)
                .unwrap()
                .into_iter()
                .map(|(path, _)| path)
                .collect::<Vec<_>>(),
            vec![b"a".to_vec(), b"b".to_vec()]
        );
        txn.del_file_index_v2_batch(&[first, second]).unwrap();
        assert!(txn.iter_file_index_v2(first_copy).unwrap().is_empty());
        assert_eq!(txn.iter_file_index_v2(second_copy).unwrap().len(), 1);
        txn.abort().unwrap();
    }

    #[test]
    fn malformed_persisted_file_index_v2_row_fails_closed() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("pristine")).unwrap();
        let working_copy = WorkingCopyId::from_bytes([9; 16]);
        let key = FileIndexV2Key::new(working_copy, b"bad".to_vec()).unwrap();
        let txn = pristine.write_txn().unwrap();
        {
            let mut table = txn.txn.open_table(FILE_INDEX_V2).unwrap();
            table
                .insert(key.encode().as_slice(), &[2u8, 0, 0][..])
                .unwrap();
        }
        assert!(txn.get_file_index_v2(working_copy, b"bad").is_err());
        txn.abort().unwrap();
    }

    #[test]
    fn exhaustive_bijection_validation_rejects_reverse_only_rows() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("pristine")).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        let primary = Inode::new(1);
        let reverse_only = Inode::new(2);
        txn.put_tree("same.txt", primary).unwrap();
        {
            let mut reverse = txn.txn.open_table(REV_TREE).unwrap();
            reverse.insert(reverse_only.get(), "same.txt").unwrap();
        }

        // Full-bijection consistency is enforced by `validate_tree_bijection`
        // (and by `TreeProjectionPlan::plan`/`apply` in atomic-repository,
        // the production removal path — see its batched-projection test).
        let error = txn.validate_tree_bijection().unwrap_err();
        assert!(error.to_string().contains("invariant violation"));

        // `del_tree` verifies its own point-invariant pair: TREE maps the
        // path to the inode AND REV_TREE maps that inode back to the path.
        // A point-level mismatch is refused without a full-table scan
        // (RFC §21 measured budgets, CB-13C AC-3 — the former per-delete
        // full REV_TREE scan was O(n) per delete, O(n²) per batch).
        txn.put_tree("other.txt", Inode::new(3)).unwrap();
        {
            let mut reverse = txn.txn.open_table(REV_TREE).unwrap();
            reverse.insert(3u64, "not-other.txt").unwrap();
        }
        let mismatch = txn.del_tree("other.txt").unwrap_err();
        assert!(
            mismatch.to_string().contains("REV_TREE maps that inode"),
            "the point-level reverse mismatch must be refused, got: {mismatch}"
        );
        assert_eq!(txn.get_inode("other.txt").unwrap(), Some(Inode::new(3)));
        txn.abort().unwrap();
    }
}
