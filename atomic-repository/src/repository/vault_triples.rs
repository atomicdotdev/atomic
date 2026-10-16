//! Canonical vault projection persistence and workflow graph queries.

use super::vault_projection::VaultProjection;
use super::*;
use atomic_core::pristine::tables::tokenize_for_fts;
use atomic_core::pristine::{KgMutTxnT, KgTxnT, VaultEntry, VaultTxnT};
use atomic_core::{
    kg::{
        iri,
        ontology::{Class, VERSION_IRI},
        project_id::MintReason,
    },
    KgEdge, KgNode, KgSubgraph,
};
use std::collections::{BTreeMap, HashMap, HashSet};

fn owned(metadata: Option<&serde_json::Value>, path: &str) -> bool {
    metadata
        .and_then(|m| m.get("derived_from_vault_path"))
        .and_then(|m| m.as_str())
        == Some(path)
}

pub(super) fn delete_vault_projection(
    txn: &mut atomic_core::WriteTxn<'_>,
    path: &str,
) -> Result<(), RepositoryError> {
    if !txn.kg_writes_enabled()? {
        return Ok(());
    }
    for node in txn.all_kg_nodes()? {
        if owned(node.metadata.as_ref(), path) {
            txn.del_kg_node(&node.id)?;
        }
    }
    for edge in txn.all_kg_edges()? {
        if owned(edge.metadata.as_ref(), path) {
            txn.del_kg_edge(&edge.from_id, &edge.to_id, &edge.kind)?;
        }
    }
    Ok(())
}

/// Immutable read snapshot for RDF adaptation; taking one transaction preserves
/// consistency between resources, types, predicates, and the generation marker.
#[derive(Clone, Debug, serde::Serialize)]
pub struct WorkflowSnapshot {
    pub generation: u32,
    pub ontology_version: &'static str,
    pub identity: String,
    pub nodes: Vec<KgNode>,
    pub edges: Vec<KgEdge>,
}

impl Repository {
    pub fn init_kg(&self) -> Result<(), RepositoryError> {
        self.ensure_project_identity(MintReason::Upgrade)?;
        let mut txn = self.pristine.write_txn()?;
        if txn.kg_writes_enabled()? {
            txn.init_kg()?;
        }
        txn.commit()?;
        Ok(())
    }

    fn vault_projection(&self) -> Result<VaultProjection, RepositoryError> {
        self.ensure_project_identity(MintReason::Upgrade)?;
        let hashes = self
            .change_store
            .iter_changes()
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        VaultProjection::new(&self.pristine.read_txn()?, self.project_id()?, hashes)
    }

    pub fn vault_extract_kg(
        &self,
        path: &str,
        entry: &VaultEntry,
    ) -> Result<(Vec<KgNode>, Vec<KgEdge>), RepositoryError> {
        let graph = self.vault_projection()?.extract(path, entry)?;
        Ok((graph.nodes, graph.edges))
    }

    pub fn vault_store_kg(
        &self,
        path: &str,
        nodes: &[KgNode],
        edges: &[KgEdge],
    ) -> Result<usize, RepositoryError> {
        let entry = self
            .vault_retrieve(path)?
            .ok_or_else(|| RepositoryError::FileNotFound { path: path.into() })?;
        let expected = self.vault_extract_kg(path, &entry)?;
        if nodes != expected.0 || edges != expected.1 {
            return Err(RepositoryError::InvalidOperation {
                message: "stale vault projection; re-extract before storing".into(),
            });
        }
        let mut txn = self.pristine.write_txn()?;
        if !txn.kg_writes_enabled()? {
            return Ok(0);
        }
        if txn.get_vault_entry(path)?.as_ref().is_none_or(|current| {
            current.content_hash != entry.content_hash
                || current.frontmatter_json != entry.frontmatter_json
                || current.entry_type != entry.entry_type
        }) {
            return Ok(0);
        }
        store_projection(&mut txn, path, nodes, edges)?;
        txn.commit()?;
        Ok(nodes.len() + edges.len())
    }

    pub fn vault_index_kg(&self, path: &str) -> Result<usize, RepositoryError> {
        self.ensure_project_identity(MintReason::Upgrade)?;
        if self.pristine.read_txn()?.kg_generation()? != atomic_core::kg::KG_GENERATION {
            return Ok(0);
        }
        let entry = self
            .vault_retrieve(path)?
            .ok_or_else(|| RepositoryError::FileNotFound { path: path.into() })?;
        let (nodes, edges) = self.vault_extract_kg(path, &entry)?;
        self.vault_store_kg(path, &nodes, &edges)
    }

    pub fn vault_reindex_kg(&self) -> Result<usize, RepositoryError> {
        self.ensure_project_identity(MintReason::Upgrade)?;
        let hashes = self
            .change_store
            .iter_changes()
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let mut txn = self.pristine.write_txn()?;
        if !txn.kg_writes_enabled()? {
            return Ok(0);
        }
        let context = VaultProjection::new(&txn, txn.kg_project()?, hashes)?;
        let entries = txn.list_vault_entries("", None)?;
        let mut total = 0;
        for meta in entries {
            if let Some(entry) = txn.get_vault_entry(&meta.path)? {
                let graph = context.extract(&meta.path, &entry)?;
                store_projection(&mut txn, &meta.path, &graph.nodes, &graph.edges)?;
                total += graph.nodes.len() + graph.edges.len();
            }
        }
        txn.commit()?;
        self.refresh_session_kg()?;
        Ok(total)
    }

    pub fn workflow_snapshot(&self) -> Result<WorkflowSnapshot, RepositoryError> {
        let txn = self.pristine.read_txn()?;
        let mut nodes = txn.all_kg_nodes()?;
        let mut edges = txn.all_kg_edges()?;
        nodes.sort_by(|a, b| a.id.cmp(&b.id));
        edges.sort_by(|a, b| (&a.from_id, &a.to_id, a.kind).cmp(&(&b.from_id, &b.to_id, b.kind)));
        let bytes =
            serde_json::to_vec(&(atomic_core::kg::KG_GENERATION, VERSION_IRI, &nodes, &edges))
                .expect("snapshot serialization");
        Ok(WorkflowSnapshot {
            generation: atomic_core::kg::KG_GENERATION,
            ontology_version: VERSION_IRI,
            identity: Hash::of(&bytes).to_base32(),
            nodes,
            edges,
        })
    }

    pub fn vault_kg_neighbors(&self, id: &str, depth: u8) -> Result<KgSubgraph, RepositoryError> {
        Ok(self.pristine.read_txn()?.kg_neighbors(id, depth)?)
    }
    pub fn workflow_neighbors(
        &self,
        id: &str,
        depth: u8,
        predicate: Option<atomic_core::kg::ontology::Predicate>,
        direction: atomic_core::kg::Direction,
    ) -> Result<KgSubgraph, RepositoryError> {
        Ok(self
            .pristine
            .read_txn()?
            .kg_neighbors_filtered(id, depth, predicate, direction)?)
    }
    pub fn vault_kg_node(&self, id: &str) -> Result<Option<KgNode>, RepositoryError> {
        Ok(self.pristine.read_txn()?.get_kg_node(id)?)
    }
    pub fn vault_kg_node_count(&self) -> Result<usize, RepositoryError> {
        Ok(self.pristine.read_txn()?.count_kg_nodes()?)
    }
    pub fn vault_kg_edge_count(&self) -> Result<usize, RepositoryError> {
        Ok(self.pristine.read_txn()?.count_kg_edges()?)
    }

    pub fn vault_kg_search_by_kind(
        &self,
        query: &str,
        limit: usize,
        class: Class,
    ) -> Result<Vec<KgNode>, RepositoryError> {
        let txn = self.pristine.read_txn()?;
        let mut rows = Vec::new();
        for (id, _) in txn.kg_fts_match_ids(query)? {
            if let Some(node) = txn.get_kg_node(&id)? {
                if node.has_type(class) {
                    let hits = fts_hits(&node, query);
                    if hits > 0 {
                        rows.push((hits, node));
                    }
                }
            }
        }
        rows.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.id.cmp(&b.1.id)));
        Ok(rows.into_iter().take(limit).map(|(_, node)| node).collect())
    }

    pub fn vault_kg_search(
        &self,
        query: &str,
        limit: usize,
        pool: Option<usize>,
    ) -> Result<Vec<KgNode>, RepositoryError> {
        let txn = self.pristine.read_txn()?;
        let mut rows = BTreeMap::new();
        let mut content = HashMap::<String, usize>::new();
        if crate::content_search::has_content_index(self.root()) {
            if let Ok(result) = crate::content_search::search_content(
                self.root(),
                query,
                crate::content_search::ContentSearchOptions {
                    max_results: Some(2000),
                    ..Default::default()
                },
            ) {
                for result in result.matches {
                    *content.entry(result.path).or_default() += 1;
                }
            }
        }
        for (id, _) in txn.kg_fts_match_ids(query)? {
            if let Some(node) = txn.get_kg_node(&id)? {
                let hits = fts_hits(&node, query);
                if hits > 0 {
                    rows.insert(id, (node, hits));
                }
            }
        }
        let project = self.project_id()?;
        for path in content.keys() {
            let id = iri::mint_file(&project, path);
            rows.entry(id.clone()).or_insert_with(|| {
                (
                    KgNode::new(&id, Class::File, path, "content_search")
                        .with_metadata(serde_json::json!({"path": path})),
                    0,
                )
            });
        }
        let mut ranked = Vec::new();
        for (_, (mut node, hits)) in rows {
            let count = node
                .path()
                .and_then(|path| content.get(path))
                .copied()
                .unwrap_or(0);
            let weight = if node.has_type(Class::File) || node.has_type(Class::Change) {
                400u64
            } else {
                200
            };
            let penalty = node.path().map(path_penalty).unwrap_or(0);
            let score = weight.saturating_sub(penalty)
                + hits.saturating_sub(1) as u64 * 50
                + count.min(100) as u64;
            if count > 0 {
                node.metadata.get_or_insert_with(|| serde_json::json!({}))["content_matches"] =
                    serde_json::json!(count);
            }
            ranked.push((score, node));
        }
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.id.cmp(&b.1.id)));
        ranked.truncate(pool.unwrap_or(limit).max(limit));
        if pool.is_some_and(|pool| pool > limit) {
            let mut per_type = HashMap::<String, usize>::new();
            let mut selected = Vec::new();
            let mut remaining = Vec::new();
            for row in ranked {
                let count = per_type.entry(row.1.primary_type().to_owned()).or_default();
                if *count < 2 {
                    selected.push(row);
                    *count += 1;
                } else {
                    remaining.push(row);
                }
            }
            selected.extend(
                remaining
                    .into_iter()
                    .take(limit.saturating_sub(selected.len())),
            );
            selected.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.id.cmp(&b.1.id)));
            ranked = selected;
        }
        Ok(ranked
            .into_iter()
            .take(limit)
            .map(|(_, node)| node)
            .collect())
    }
}

pub(super) fn store_projection(
    txn: &mut atomic_core::WriteTxn<'_>,
    path: &str,
    nodes: &[KgNode],
    edges: &[KgEdge],
) -> Result<(), RepositoryError> {
    for edge in txn.all_kg_edges()? {
        if owned(edge.metadata.as_ref(), path) {
            txn.del_kg_edge(&edge.from_id, &edge.to_id, &edge.kind)?;
        }
    }
    let retained: HashSet<&str> = nodes.iter().map(|node| node.id.as_str()).collect();
    for old in txn.all_kg_nodes()? {
        if owned(old.metadata.as_ref(), path) && !retained.contains(old.id.as_str()) {
            txn.del_kg_node(&old.id)?;
        }
    }
    for node in nodes {
        if owned(node.metadata.as_ref(), path) || txn.get_kg_node(&node.id)?.is_none() {
            txn.upsert_kg_node(node)?;
        }
    }
    for edge in edges {
        txn.upsert_kg_edge(edge)?;
    }
    Ok(())
}

fn fts_hits(node: &KgNode, query: &str) -> usize {
    let query: HashSet<_> = tokenize_for_fts(query).into_iter().collect();
    let tokens: HashSet<_> = tokenize_for_fts(&format!(
        "{} {} {}",
        node.id,
        node.label,
        node.summary.as_deref().unwrap_or_default()
    ))
    .into_iter()
    .collect();
    query.intersection(&tokens).count()
}
fn path_penalty(path: &str) -> u64 {
    if path.contains("test") {
        80
    } else if path.starts_with("src/") || path.starts_with("lib/") {
        0
    } else if path.ends_with(".md") {
        120
    } else {
        20
    }
}
