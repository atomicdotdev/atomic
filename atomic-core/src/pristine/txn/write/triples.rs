//! Knowledge graph operations for WriteTxn.

use redb::{ReadableMultimapTable, ReadableTable, ReadableTableMetadata};

use crate::pristine::error::{PristineError, PristineResult};
use crate::pristine::tables::*;
use crate::pristine::traits::{KgMutTxnT, KgTxnT};
use crate::pristine::vault::{KgEdge, KgNode};

use super::WriteTxn;

const KG_FTS_REVERSE_SCHEMA_KEY: &str = "fts_reverse_schema";
const KG_FTS_REVERSE_SCHEMA_VERSION: u32 = 2;

fn kg_node_fts_text(node: &KgNode) -> String {
    let mut text = String::with_capacity(node.id.len() + node.label.len() + 64);
    text.push_str(&node.id);
    text.push(' ');
    text.push_str(&node.label);
    if let Some(summary) = &node.summary {
        text.push(' ');
        text.push_str(summary);
    }
    text
}

impl WriteTxn<'_> {
    /// Ensure the reverse FTS index exists and rebuild both directions once for
    /// databases created before replacement-safe indexing was introduced.
    fn ensure_kg_fts_schema(&mut self) -> PristineResult<()> {
        self.txn.open_multimap_table(KG_FTS)?;
        self.txn.open_multimap_table(KG_FTS_BY_NODE)?;

        let current_version = {
            let metadata = self.txn.open_table(KG_INDEX_META)?;
            let stored_version = metadata.get(KG_FTS_REVERSE_SCHEMA_KEY)?;
            match stored_version {
                Some(value) => value.value(),
                None => 0,
            }
        };
        if current_version >= KG_FTS_REVERSE_SCHEMA_VERSION {
            return Ok(());
        }

        // Legacy KG FTS only ever appended postings. Rebuild from KG_NODES so
        // both orphan postings and obsolete label/summary tokens disappear.
        let nodes_to_index = {
            let nodes = self.txn.open_table(KG_NODES)?;
            let mut indexed = Vec::new();
            for entry in nodes.iter()? {
                let (node_id, node_bytes) = entry?;
                let node: KgNode = serde_json::from_slice(node_bytes.value()).map_err(|e| {
                    PristineError::Serialization {
                        message: e.to_string(),
                    }
                })?;
                let tokens: std::collections::HashSet<String> =
                    tokenize_for_fts(&kg_node_fts_text(&node))
                        .into_iter()
                        .collect();
                indexed.push((node_id.value().to_string(), tokens));
            }
            indexed
        };

        let forward_keys = {
            let fts = self.txn.open_multimap_table(KG_FTS)?;
            let mut keys = Vec::new();
            for entry in fts.iter()? {
                keys.push(entry?.0.value().to_string());
            }
            keys
        };
        {
            let mut fts = self.txn.open_multimap_table(KG_FTS)?;
            for token in forward_keys {
                fts.remove_all(token.as_str())?;
            }
        }

        let reverse_keys = {
            let reverse = self.txn.open_multimap_table(KG_FTS_BY_NODE)?;
            let mut keys = Vec::new();
            for entry in reverse.iter()? {
                keys.push(entry?.0.value().to_string());
            }
            keys
        };
        {
            let mut reverse = self.txn.open_multimap_table(KG_FTS_BY_NODE)?;
            for node_id in reverse_keys {
                reverse.remove_all(node_id.as_str())?;
            }
        }

        {
            let mut fts = self.txn.open_multimap_table(KG_FTS)?;
            let mut reverse = self.txn.open_multimap_table(KG_FTS_BY_NODE)?;
            for (node_id, tokens) in nodes_to_index {
                for token in tokens {
                    fts.insert(token.as_str(), node_id.as_str())?;
                    reverse.insert(node_id.as_str(), token.as_str())?;
                }
            }
        }
        {
            let mut metadata = self.txn.open_table(KG_INDEX_META)?;
            metadata.insert(KG_FTS_REVERSE_SCHEMA_KEY, KG_FTS_REVERSE_SCHEMA_VERSION)?;
        }

        Ok(())
    }

    fn remove_kg_fts_entries(&mut self, node_id: &str) -> PristineResult<()> {
        self.ensure_kg_fts_schema()?;

        let tokens = {
            let mut reverse = self.txn.open_multimap_table(KG_FTS_BY_NODE)?;
            let removed = reverse.remove_all(node_id)?;
            let mut tokens = Vec::new();
            for token in removed {
                tokens.push(token?.value().to_string());
            }
            tokens
        };

        let mut fts = self.txn.open_multimap_table(KG_FTS)?;
        for token in tokens {
            fts.remove(token.as_str(), node_id)?;
        }
        Ok(())
    }

    fn add_kg_fts_entries(&mut self, node_id: &str, text: &str) -> PristineResult<()> {
        self.ensure_kg_fts_schema()?;
        let tokens: std::collections::HashSet<String> =
            tokenize_for_fts(text).into_iter().collect();

        {
            let mut fts = self.txn.open_multimap_table(KG_FTS)?;
            for token in &tokens {
                fts.insert(token.as_str(), node_id)?;
            }
        }
        {
            let mut reverse = self.txn.open_multimap_table(KG_FTS_BY_NODE)?;
            for token in &tokens {
                reverse.insert(node_id, token.as_str())?;
            }
        }
        Ok(())
    }
}

impl<'a> KgTxnT for WriteTxn<'a> {
    fn get_kg_node(&self, id: &str) -> PristineResult<Option<KgNode>> {
        let table = match self.txn.open_table(KG_NODES) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(PristineError::from(e)),
        };
        let result = match table.get(id)? {
            Some(value) => {
                let bytes: &[u8] = value.value();
                let node: KgNode =
                    serde_json::from_slice(bytes).map_err(|e| PristineError::Serialization {
                        message: e.to_string(),
                    })?;
                Some(node)
            }
            None => None,
        };
        drop(table);
        Ok(result)
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
            let edge_key_guard: redb::AccessGuard<'_, &str> = result?;
            let edge_key: &str = edge_key_guard.value();
            if let Some(edge_data) = edges_table.get(edge_key)? {
                let bytes: &[u8] = edge_data.value();
                let edge: KgEdge =
                    serde_json::from_slice(bytes).map_err(|e| PristineError::Serialization {
                        message: e.to_string(),
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
            let edge_key_guard: redb::AccessGuard<'_, &str> = result?;
            let edge_key: &str = edge_key_guard.value();
            if let Some(edge_data) = edges_table.get(edge_key)? {
                let bytes: &[u8] = edge_data.value();
                let edge: KgEdge =
                    serde_json::from_slice(bytes).map_err(|e| PristineError::Serialization {
                        message: e.to_string(),
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
                let node_id_guard: redb::AccessGuard<'_, &str> = result?;
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

    fn count_kg_edges(&self) -> PristineResult<usize> {
        let table = match self.txn.open_table(KG_EDGES) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
            Err(e) => return Err(PristineError::from(e)),
        };
        Ok(table.len()? as usize)
    }
}

impl<'a> KgMutTxnT for WriteTxn<'a> {
    fn upsert_kg_node(&mut self, node: &KgNode) -> PristineResult<()> {
        // Replacement semantics are important: old ID/label/summary tokens
        // must disappear before the same node ID is re-indexed.
        self.remove_kg_fts_entries(&node.id)?;

        let mut table = self.txn.open_table(KG_NODES)?;
        let bytes = serde_json::to_vec(node).map_err(|e| PristineError::Serialization {
            message: e.to_string(),
        })?;
        table.insert(node.id.as_str(), bytes.as_slice())?;
        drop(table);

        self.add_kg_fts_entries(&node.id, &kg_node_fts_text(node))?;

        Ok(())
    }

    fn upsert_kg_edge(&mut self, edge: &KgEdge) -> PristineResult<()> {
        let edge_key = encode_edge_key(&edge.from_id, &edge.to_id, &edge.kind);

        let mut edges_table = self.txn.open_table(KG_EDGES)?;
        let bytes = serde_json::to_vec(edge).map_err(|e| PristineError::Serialization {
            message: e.to_string(),
        })?;
        edges_table.insert(edge_key.as_str(), bytes.as_slice())?;
        drop(edges_table);

        // Update indexes
        let mut from_table = self.txn.open_multimap_table(KG_EDGES_FROM)?;
        from_table.insert(edge.from_id.as_str(), edge_key.as_str())?;
        drop(from_table);

        let mut to_table = self.txn.open_multimap_table(KG_EDGES_TO)?;
        to_table.insert(edge.to_id.as_str(), edge_key.as_str())?;

        Ok(())
    }

    fn del_kg_node(&mut self, id: &str) -> PristineResult<bool> {
        // First delete all edges for this node
        self.del_kg_edges_for_node(id)?;

        // Remove indexed ID/label/summary tokens before the node ID can be
        // reused. Otherwise old metadata can reappear in search results.
        self.remove_kg_fts_entries(id)?;

        // Delete the node
        let mut table = match self.txn.open_table(KG_NODES) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
            Err(e) => return Err(PristineError::from(e)),
        };

        let result = table.remove(id)?.is_some();
        drop(table);
        Ok(result)
    }

    fn del_kg_edge(&mut self, from_id: &str, to_id: &str, kind: &str) -> PristineResult<bool> {
        let edge_key = encode_edge_key(from_id, to_id, kind);

        let mut edges_table = match self.txn.open_table(KG_EDGES) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
            Err(e) => return Err(PristineError::from(e)),
        };

        let existed = edges_table.remove(edge_key.as_str())?.is_some();
        drop(edges_table);

        if existed {
            // Clean up indexes
            let mut from_table = self.txn.open_multimap_table(KG_EDGES_FROM)?;
            from_table.remove(from_id, edge_key.as_str())?;
            drop(from_table);

            let mut to_table = self.txn.open_multimap_table(KG_EDGES_TO)?;
            to_table.remove(to_id, edge_key.as_str())?;
        }

        Ok(existed)
    }

    fn del_kg_edges_for_node(&mut self, node_id: &str) -> PristineResult<usize> {
        // Collect all edge keys for this node (both directions)
        let outgoing = self.get_kg_edges_from(node_id)?;
        let incoming = self.get_kg_edges_to(node_id)?;

        let mut count = 0;
        for edge in outgoing.iter().chain(incoming.iter()) {
            if self.del_kg_edge(&edge.from_id, &edge.to_id, &edge.kind)? {
                count += 1;
            }
        }
        Ok(count)
    }

    fn init_kg(&mut self) -> PristineResult<()> {
        self.txn.open_table(KG_NODES)?;
        self.txn.open_table(KG_EDGES)?;
        self.txn.open_multimap_table(KG_EDGES_FROM)?;
        self.txn.open_multimap_table(KG_EDGES_TO)?;
        self.ensure_kg_fts_schema()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pristine::traits::MutTxnT;
    use crate::pristine::Pristine;
    use tempfile::tempdir;

    #[test]
    fn test_kg_init_and_node_crud() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("db")).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        txn.init_kg().unwrap();

        let node = KgNode::new("change:abc123", "change", "abc123", "change_graph")
            .with_summary("Fix auth bug");
        txn.upsert_kg_node(&node).unwrap();

        let retrieved = txn.get_kg_node("change:abc123").unwrap().unwrap();
        assert_eq!(retrieved.id, "change:abc123");
        assert_eq!(retrieved.kind, "change");
        assert_eq!(retrieved.summary, Some("Fix auth bug".to_string()));

        assert!(txn.get_kg_node("nonexistent").unwrap().is_none());

        assert!(txn.del_kg_node("change:abc123").unwrap());
        assert!(txn.get_kg_node("change:abc123").unwrap().is_none());

        txn.commit().unwrap();
    }

    #[test]
    fn test_kg_edge_crud() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("db")).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        txn.init_kg().unwrap();

        // Create nodes
        txn.upsert_kg_node(&KgNode::new("change:abc", "change", "abc", "graph"))
            .unwrap();
        txn.upsert_kg_node(&KgNode::new("file:auth.rs", "file", "auth.rs", "graph"))
            .unwrap();
        txn.upsert_kg_node(&KgNode::new("file:main.rs", "file", "main.rs", "graph"))
            .unwrap();

        // Create edges
        txn.upsert_kg_edge(&KgEdge::new("change:abc", "file:auth.rs", "MODIFIES"))
            .unwrap();
        txn.upsert_kg_edge(&KgEdge::new("change:abc", "file:main.rs", "MODIFIES"))
            .unwrap();

        // Query outgoing
        let outgoing = txn.get_kg_edges_from("change:abc").unwrap();
        assert_eq!(outgoing.len(), 2);

        // Query incoming
        let incoming = txn.get_kg_edges_to("file:auth.rs").unwrap();
        assert_eq!(incoming.len(), 1);
        assert_eq!(incoming[0].from_id, "change:abc");

        // Delete edge
        assert!(txn
            .del_kg_edge("change:abc", "file:auth.rs", "MODIFIES")
            .unwrap());
        assert_eq!(txn.get_kg_edges_from("change:abc").unwrap().len(), 1);

        // Counts
        assert_eq!(txn.count_kg_nodes().unwrap(), 3);
        assert_eq!(txn.count_kg_edges().unwrap(), 1);

        txn.commit().unwrap();
    }

    #[test]
    fn test_kg_neighbors() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("db")).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        txn.init_kg().unwrap();

        txn.upsert_kg_node(&KgNode::new("a", "x", "A", "test"))
            .unwrap();
        txn.upsert_kg_node(&KgNode::new("b", "x", "B", "test"))
            .unwrap();
        txn.upsert_kg_node(&KgNode::new("c", "x", "C", "test"))
            .unwrap();
        txn.upsert_kg_edge(&KgEdge::new("a", "b", "LINK")).unwrap();
        txn.upsert_kg_edge(&KgEdge::new("b", "c", "LINK")).unwrap();

        // Depth 1 from "a"
        let sg = txn.kg_neighbors("a", 1).unwrap();
        assert_eq!(sg.nodes.len(), 2); // a, b
        assert_eq!(sg.edges.len(), 1); // a→b

        // Depth 2 from "a"
        let sg = txn.kg_neighbors("a", 2).unwrap();
        assert_eq!(sg.nodes.len(), 3); // a, b, c
        assert_eq!(sg.edges.len(), 2); // a→b, b→c

        txn.commit().unwrap();
    }

    #[test]
    fn test_kg_fts_search() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("db")).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        txn.init_kg().unwrap();

        txn.upsert_kg_node(
            &KgNode::new("change:abc", "change", "abc123", "graph")
                .with_summary("Fix authentication bug"),
        )
        .unwrap();
        txn.upsert_kg_node(
            &KgNode::new("change:def", "change", "def456", "graph").with_summary("Add logging"),
        )
        .unwrap();

        // Search for "authentication"
        let results = txn.kg_fts_search("authentication", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "change:abc");

        // Search for "fix" matches abc's summary
        let results = txn.kg_fts_search("fix", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "change:abc");

        // Search for "logging" matches def
        let results = txn.kg_fts_search("logging", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "change:def");

        txn.commit().unwrap();
    }

    #[test]
    fn test_kg_upsert_replaces_fts_tokens() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("db")).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        txn.init_kg().unwrap();

        let old = KgNode::new("memory:build", "memory", "Build", "vault")
            .with_summary("obsoletecapybara summary");
        txn.upsert_kg_node(&old).unwrap();
        assert_eq!(txn.kg_fts_match_ids("obsoletecapybara").unwrap().len(), 1);

        let updated = KgNode::new("memory:build", "memory", "Build", "vault")
            .with_summary("currentwombat summary");
        txn.upsert_kg_node(&updated).unwrap();

        assert!(txn.kg_fts_match_ids("obsoletecapybara").unwrap().is_empty());
        assert_eq!(txn.kg_fts_match_ids("currentwombat").unwrap().len(), 1);

        txn.commit().unwrap();
    }

    #[test]
    fn test_kg_delete_prevents_stale_tokens_reappearing_when_id_is_reused() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("db")).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        txn.init_kg().unwrap();

        let node = KgNode::new("memory:reused", "memory", "Original", "vault")
            .with_summary("ghostplatypus");
        txn.upsert_kg_node(&node).unwrap();
        assert!(txn.del_kg_node(&node.id).unwrap());

        let replacement = KgNode::new("memory:reused", "memory", "Replacement", "vault");
        txn.upsert_kg_node(&replacement).unwrap();
        assert!(txn.kg_fts_match_ids("ghostplatypus").unwrap().is_empty());

        txn.commit().unwrap();
    }

    #[test]
    fn test_kg_init_rebuilds_current_entries_and_prunes_stale_postings() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("db")).unwrap();
        let mut txn = pristine.write_txn().unwrap();

        // Simulate an index written before replacement-safe indexing existed,
        // including obsolete metadata and a deleted node's orphan posting.
        {
            let node = KgNode::new("memory:legacy", "memory", "Legacy", "vault")
                .with_summary("legacyechidna");
            let bytes = serde_json::to_vec(&node).unwrap();
            let mut nodes = txn.txn.open_table(KG_NODES).unwrap();
            nodes.insert(node.id.as_str(), bytes.as_slice()).unwrap();

            let mut legacy_fts = txn.txn.open_multimap_table(KG_FTS).unwrap();
            legacy_fts.insert("legacyechidna", "memory:legacy").unwrap();
            legacy_fts
                .insert("obsoletecapybara", "memory:legacy")
                .unwrap();
            legacy_fts
                .insert("orphanpangolin", "memory:deleted")
                .unwrap();
        }

        txn.init_kg().unwrap();
        assert_eq!(txn.kg_fts_match_ids("legacyechidna").unwrap().len(), 1);
        assert!(txn.kg_fts_match_ids("obsoletecapybara").unwrap().is_empty());
        assert!(txn.kg_fts_match_ids("orphanpangolin").unwrap().is_empty());

        assert!(txn.del_kg_node("memory:legacy").unwrap());
        assert!(txn.kg_fts_match_ids("legacyechidna").unwrap().is_empty());

        txn.commit().unwrap();
    }

    #[test]
    fn test_kg_del_node_cascades_edges() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("db")).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        txn.init_kg().unwrap();

        txn.upsert_kg_node(&KgNode::new("a", "x", "A", "t"))
            .unwrap();
        txn.upsert_kg_node(&KgNode::new("b", "x", "B", "t"))
            .unwrap();
        txn.upsert_kg_edge(&KgEdge::new("a", "b", "LINK")).unwrap();
        txn.upsert_kg_edge(&KgEdge::new("b", "a", "BACK")).unwrap();

        txn.del_kg_node("a").unwrap();

        assert_eq!(txn.count_kg_edges().unwrap(), 0);
        assert_eq!(txn.count_kg_nodes().unwrap(), 1); // only b remains

        txn.commit().unwrap();
    }

    #[test]
    fn test_kg_persistence() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("db")).unwrap();
        {
            let mut txn = pristine.write_txn().unwrap();
            txn.init_kg().unwrap();
            txn.upsert_kg_node(&KgNode::new("x", "t", "X", "s"))
                .unwrap();
            txn.upsert_kg_edge(&KgEdge::new("x", "y", "LINK")).unwrap();
            txn.commit().unwrap();
        }
        {
            let txn = pristine.read_txn().unwrap();
            assert!(txn.get_kg_node("x").unwrap().is_some());
            assert_eq!(txn.get_kg_edges_from("x").unwrap().len(), 1);
        }
    }
}
