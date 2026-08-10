use super::*;

// ViewTxnT Implementation

impl<'a> ViewTxnT for WriteTxn<'a> {
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

    fn list_views(&self) -> PristineResult<Vec<String>> {
        let table = self.txn.open_table(VIEWS)?;
        let mut names = Vec::new();
        for (k, _) in table.iter()?.filter_map(|r| r.ok()) {
            names.push(k.value().to_string());
        }
        Ok(names)
    }

    fn get_conflicts(&self, view_id: u64, inode: u64) -> PristineResult<Vec<StoredConflict>> {
        let table = self.txn.open_table(CONFLICTS)?;
        let key = encode_view_seq(view_id, inode);
        let result = table.get(&key)?;
        match result {
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
