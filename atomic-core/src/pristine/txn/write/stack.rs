use super::*;
use crate::pristine::tables::{decode_stack_graph_key, encode_stack_graph_key};

// StackTxnT Implementation

impl<'a> StackTxnT for WriteTxn<'a> {
    fn get_stack_by_id(&self, id: u64) -> PristineResult<Option<StackState>> {
        let table = self.txn.open_table(STACKS)?;
        for result in table.iter()? {
            let (_key, value) = result?;
            let state = deserialize_stack_state(value.value())?;
            if state.id == id {
                return Ok(Some(state));
            }
        }
        Ok(None)
    }

    fn iter_stack_graph_adjacent(
        &self,
        stack_id: u64,
        node: GraphNode<NodeId>,
        min_flag: EdgeFlags,
        max_flag: EdgeFlags,
    ) -> PristineResult<Box<dyn Iterator<Item = Result<SerializedGraphEdge, PristineError>> + '_>>
    {
        let table = self.txn.open_multimap_table(STACK_GRAPH)?;
        let key = encode_stack_graph_key(
            stack_id,
            node.change.get(),
            node.start.get(),
            node.end.get(),
        );

        let mut edges = Vec::new();
        for v in table.get(&key)?.filter_map(|r| r.ok()) {
            let bytes: &[u8; 24] = v.value();
            let edge = deserialize_edge(bytes);
            let flag = edge.flag();
            if flag >= min_flag && flag <= max_flag {
                edges.push(edge);
            }
        }

        Ok(Box::new(edges.into_iter().map(Ok)))
    }

    fn iter_stack_graph_vertices_for_change(
        &self,
        stack_id: u64,
        change_id: u64,
    ) -> PristineResult<Vec<(u64, u64)>> {
        let table = self.txn.open_multimap_table(STACK_GRAPH)?;

        // Range scan: (stack_id, change_id, 0, 0) .. (stack_id, change_id+1, 0, 0)
        let start_key = encode_stack_graph_key(stack_id, change_id, 0, 0);
        let end_key = encode_stack_graph_key(stack_id, change_id, u64::MAX, u64::MAX);

        let mut vertices: Vec<(u64, u64)> = Vec::new();
        let mut seen = std::collections::HashSet::new();

        for result in table.range::<&[u8; 32]>(&start_key..=&end_key)? {
            let (key, _values) = result?;
            let (_, v_change, v_start, v_end) = decode_stack_graph_key(key.value());
            if v_change != change_id {
                continue;
            }
            if seen.insert((v_start, v_end)) {
                vertices.push((v_start, v_end));
            }
        }

        Ok(vertices)
    }

    fn get_stack(&self, name: &str) -> PristineResult<Option<StackState>> {
        let table = self.txn.open_table(STACKS)?;
        let result = table.get(name)?;
        match result {
            Some(value) => {
                let bytes = value.value();
                let state = deserialize_stack_state(bytes)?;
                Ok(Some(state))
            }
            None => Ok(None),
        }
    }

    fn list_stacks(&self) -> PristineResult<Vec<String>> {
        let table = self.txn.open_table(STACKS)?;
        let mut names = Vec::new();
        for (k, _) in table.iter()?.filter_map(|r| r.ok()) {
            names.push(k.value().to_string());
        }
        Ok(names)
    }

    fn get_change_seq(&self, stack: &StackState, change_id: NodeId) -> PristineResult<Option<u64>> {
        let table = self.txn.open_table(REV_STACK_CHANGES)?;
        let key = encode_stack_seq(stack.id, change_id.get());
        let result = table.get(&key)?;
        match result {
            Some(value) => Ok(Some(value.value())),
            None => Ok(None),
        }
    }

    fn get_change_at_seq(&self, stack: &StackState, seq: u64) -> PristineResult<Option<NodeId>> {
        let table = self.txn.open_table(STACK_CHANGES)?;
        let key = encode_stack_seq(stack.id, seq);
        let result = table.get(&key)?;
        match result {
            Some(value) => Ok(Some(NodeId::new(value.value()))),
            None => Ok(None),
        }
    }

    fn iter_changes(
        &self,
        stack: &StackState,
        from_seq: u64,
    ) -> PristineResult<Box<dyn Iterator<Item = Result<(u64, NodeId, Merkle), PristineError>> + '_>>
    {
        let changes_table = self.txn.open_table(STACK_CHANGES)?;
        let tags_table = self.txn.open_table(TAGS)?;

        let stack_id = stack.id;
        let start_key = encode_stack_seq(stack_id, from_seq);
        let end_key = encode_stack_seq(stack_id, u64::MAX);

        let mut results = Vec::new();
        for result in changes_table.range::<&[u8; 16]>(&start_key..=&end_key)? {
            match result {
                Ok((key, value)) => {
                    let (_, seq) = decode_stack_seq(key.value());
                    let change_id = NodeId::new(value.value());

                    let tag_key = encode_stack_seq(stack_id, seq);
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
