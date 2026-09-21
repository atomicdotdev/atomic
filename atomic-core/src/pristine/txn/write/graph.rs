use super::*;
use crate::pristine::txn::helpers::{try_collect, try_is_present};

// GraphTxnT Implementation

impl<'a> GraphTxnT for WriteTxn<'a> {
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
        // O(change vertices) and every multi-file import quadratic — review
        // ::15, 2026-09-16). Within one change the non-empty hunks are
        // DISJOINT ranges of that change's contents buffer, so at most one
        // non-empty vertex contains a position, and it is the LAST vertex key
        // ≤ (pos, u64::MAX). The empty-vertex fallback (the shared
        // inode/content start ambiguity, AGENTS.md "Position Ambiguity") is
        // an exact-key lookup. Empty vertices coincide with content-span
        // starts, so they never hide a containing span from the candidate.
        let upper = encode_vertex(change_id, target_pos, u64::MAX);
        if let Some((v_change, v_start, v_end)) = table
            .range::<&[u8; 24]>(&encode_vertex(change_id, 0, 0)..=&upper)?
            .next_back()
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

    /// Find a block that ends at or after the given position.
    ///
    /// This is used for predecessors resolution where we need to find the span
    /// that ENDS at a position, not one that contains it. This is important
    /// when creating edges from an existing span to a new one.
    ///
    /// # Arguments
    ///
    /// * `pos` - The position to find (typically the end of a context span)
    ///
    /// # Returns
    ///
    /// The span that ends at or after the given position, or an error if not found.
    ///
    /// # Special Cases
    ///
    /// - ROOT position returns GraphNode::ROOT
    /// - Empty vertices (start == end == pos) are matched exactly
    /// - For non-empty vertices, finds one where start < pos <= end
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
        // O(change vertices) and every multi-file import quadratic — review
        // ::15, 2026-09-16). Under within-change hunk disjointness:
        // - a span ENDING at pos (unique when it exists) is the span
        //   containing pos-1, so it is the last vertex key ≤ (pos-1, MAX);
        // - a span CONTAINING pos (unique when it exists) is the last vertex
        //   key ≤ (pos, MAX).
        // An ends-at-pos span starts strictly before any contains-pos span
        // (which must start at pos exactly), so checking ends-at-pos first
        // preserves the historical first-match-in-key-order semantics.
        if target_pos > 0 {
            let before_upper = encode_vertex(change_id, target_pos - 1, u64::MAX);
            if let Some((v_change, v_start, v_end)) = table
                .range::<&[u8; 24]>(&encode_vertex(change_id, 0, 0)..=&before_upper)?
                .next_back()
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
            .next_back()
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
        let has = try_is_present(table.get(&key)?)?;
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
        let has = try_is_present(table.range::<&[u8; 24]>(&start_key..=&end_key)?)?;
        Ok(has)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pristine::{MutTxnT, Pristine};
    use tempfile::tempdir;

    /// A brute-force reference for `find_block`/`find_block_end`: the exact
    /// historical full-range scan semantics (first match in (start, end) key
    /// order; empty-vertex fallback rules as documented).
    fn scan_find_block(
        vertices: &[(u64, u64, u64)],
        change_id: u64,
        target_pos: u64,
    ) -> Option<GraphNode<NodeId>> {
        let mut empty = None;
        for &(v_change, v_start, v_end) in vertices {
            if v_change != change_id {
                continue;
            }
            if v_start != v_end && v_start <= target_pos && target_pos < v_end {
                return Some(GraphNode {
                    change: NodeId::new(v_change),
                    start: ChangePosition::new(v_start),
                    end: ChangePosition::new(v_end),
                });
            }
            if v_start == v_end && v_start == target_pos && empty.is_none() {
                empty = Some(GraphNode {
                    change: NodeId::new(v_change),
                    start: ChangePosition::new(v_start),
                    end: ChangePosition::new(v_end),
                });
            }
        }
        empty
    }

    fn scan_find_block_end(
        vertices: &[(u64, u64, u64)],
        change_id: u64,
        target_pos: u64,
    ) -> Option<GraphNode<NodeId>> {
        if vertices
            .iter()
            .any(|&(c, s, e)| c == change_id && s == target_pos && e == target_pos)
        {
            return Some(GraphNode {
                change: NodeId::new(change_id),
                start: ChangePosition::new(target_pos),
                end: ChangePosition::new(target_pos),
            });
        }
        for &(v_change, v_start, v_end) in vertices {
            if v_change != change_id {
                continue;
            }
            if v_end == target_pos && v_start < v_end {
                return Some(GraphNode {
                    change: NodeId::new(v_change),
                    start: ChangePosition::new(v_start),
                    end: ChangePosition::new(v_end),
                });
            }
            if v_start <= target_pos && target_pos < v_end {
                return Some(GraphNode {
                    change: NodeId::new(v_change),
                    start: ChangePosition::new(v_start),
                    end: ChangePosition::new(v_end),
                });
            }
        }
        None
    }

    /// Differential test: the binary-search `find_block`/`find_block_end`
    /// must agree with the historical brute-force scan on every probe over
    /// randomized layouts (deterministic LCG seed). Layouts combine disjoint
    /// content hunks (the graph invariant: a change's hunks are disjoint
    /// ranges of its contents buffer) with empty inode markers that share a
    /// content start (the AGENTS.md "Position Ambiguity" case) and
    /// not-in-change vertices.
    #[test]
    fn find_block_binary_search_matches_scan_over_random_layouts() {
        let mut state = 0x9E3779B97F4A7C15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        for case in 0..200u64 {
            let change_id = (next() % 3) + 1;
            let mut vertices: Vec<(u64, u64, u64)> = Vec::new();
            // Disjoint content hunks inside one change, with gaps.
            let mut cursor = next() % 10;
            for _ in 0..(next() % 24) {
                let len = (next() % 9) + 1;
                vertices.push((change_id, cursor, cursor + len));
                // An empty inode marker sharing the content start (or not).
                if next() % 3 == 0 {
                    vertices.push((change_id, cursor, cursor));
                }
                cursor += len + (next() % 5);
            }
            // A second change's vertices must never leak through.
            let other = if change_id == 1 { 2 } else { 1 };
            for k in 0..4u64 {
                vertices.push((other, k * 4, k * 4 + 3));
            }
            vertices.sort();

            let dir = tempdir().unwrap();
            let pristine = Pristine::open(dir.path().join("pristine")).unwrap();
            let mut txn = pristine.write_txn().unwrap();
            for &(c, s, e) in &vertices {
                let node = GraphNode::new(
                    NodeId::new(c),
                    ChangePosition::new(s),
                    ChangePosition::new(e),
                );
                txn.put_graph(
                    node,
                    SerializedGraphEdge::new(
                        EdgeFlags::BLOCK,
                        Position::new(NodeId::new(c), ChangePosition::new(s + 1)),
                        NodeId::new(c),
                    ),
                )
                .unwrap();
            }

            let probe_max = cursor + 5;
            for pos in 0..=probe_max {
                let pos = Position::new(NodeId::new(change_id), ChangePosition::new(pos));
                let expected = scan_find_block(&vertices, change_id, pos.pos.get());
                let actual = txn.find_block(pos).ok();
                assert_eq!(
                    expected,
                    actual,
                    "case {case} find_block mismatch at pos {}",
                    pos.pos.get()
                );
                let expected_end = scan_find_block_end(&vertices, change_id, pos.pos.get());
                let actual_end = txn.find_block_end(pos).ok();
                assert_eq!(
                    expected_end,
                    actual_end,
                    "case {case} find_block_end mismatch at pos {}",
                    pos.pos.get()
                );
            }
        }
    }

    #[test]
    fn healthy_write_graph_reads_remain_ordered_and_filtered() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("pristine")).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        let change_id = txn.register_change(&Hash::of(b"write graph")).unwrap();
        let node = GraphNode::new(change_id, ChangePosition::new(7), ChangePosition::new(7));

        for (flag, position) in [
            (EdgeFlags::BLOCK, 20),
            (EdgeFlags::FOLDER, 15),
            (EdgeFlags::BLOCK, 10),
        ] {
            txn.put_graph(
                node,
                SerializedGraphEdge::new(
                    flag,
                    Position::new(change_id, ChangePosition::new(position)),
                    change_id,
                ),
            )
            .unwrap();
        }

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
            vec![10, 20]
        );
        assert!(txn.has_vertex(node).unwrap());
        assert!(txn.has_change_in_graph(change_id).unwrap());
        assert_eq!(
            txn.find_block_end(Position::new(change_id, ChangePosition::new(7)))
                .unwrap(),
            node
        );
    }
}
