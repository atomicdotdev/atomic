use super::*;
use atomic_core::{
    kg::{project_id::MintReason, KG_GENERATION},
    pristine::{
        IntentSummary, KgMutTxnT, KgTxnT, VaultEntry, VaultEntryMeta, VaultEntryType,
        VaultManifest, VaultTxnT,
    },
    KgSubgraph, PristineError,
};
use std::collections::BTreeMap;

impl Repository {
    /// Rebuild the derived read model in one immediate transaction. Canonical
    /// database rows cannot change under the write transaction, and objects
    /// loaded from the content store are immutable. An interruption publishes
    /// neither partial tables nor a completed generation marker.
    pub fn rebuild_kg(&self, force: bool) -> Result<bool, RepositoryError> {
        self.ensure_project_identity(MintReason::Upgrade)?;
        if !force && self.pristine.read_txn()?.kg_generation()? == KG_GENERATION {
            return Ok(false);
        }
        let working_copy = self.require_working_copy_id()?;
        let _lock = self.try_lock_operation(working_copy)?;
        let mut txn = self.pristine.write_txn_immediate()?;
        if !force && txn.kg_generation()? == KG_GENERATION {
            return Ok(false);
        }
        self.rebuild_kg_in(&mut txn, None)?;
        txn.commit()?;
        Ok(true)
    }

    /// [`Self::rebuild_kg`], with the vault taken from `entries` instead of
    /// from the vault tables. Each is a vault-relative path and its entry;
    /// [`vault_entry_for_file`] makes one from a file.
    ///
    /// For a repository that holds a view's vault only as that view's files:
    /// the graph is derived from them, and the vault tables are neither read
    /// nor written, so nothing a working vault guards (edit rules, retained
    /// reviews) is touched. The graph then describes `entries`, not the vault
    /// tables; `rebuild_kg(true)` derives it from the tables again.
    ///
    /// A path given twice is refused.
    pub fn rebuild_kg_from_vault_entries(
        &self,
        entries: &[(String, VaultEntry)],
    ) -> Result<(), RepositoryError> {
        let vault = SuppliedVault::new(entries)?;
        self.ensure_project_identity(MintReason::Upgrade)?;
        let working_copy = self.require_working_copy_id()?;
        let _lock = self.try_lock_operation(working_copy)?;
        let mut txn = self.pristine.write_txn_immediate()?;
        self.rebuild_kg_in(&mut txn, Some(&vault))?;
        txn.commit()?;
        Ok(())
    }

    /// Replace the derived tables inside `txn`, reading the vault from the
    /// vault tables or, when one is `supplied`, from it.
    fn rebuild_kg_in(
        &self,
        txn: &mut atomic_core::WriteTxn<'_>,
        supplied: Option<&SuppliedVault>,
    ) -> Result<(), RepositoryError> {
        txn.begin_kg_rebuild()?;
        self.project_vcs_kg(txn)?;
        let hashes = self
            .change_store
            .iter_changes()
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let project = txn.kg_project()?;
        let graphs = match supplied {
            Some(vault) => vault_graphs(vault, project, hashes)?,
            None => vault_graphs(&*txn, project, hashes)?,
        };
        // Nodes before relationships: references can point to later vault rows.
        for graph in &graphs {
            for node in &graph.nodes {
                if node
                    .metadata
                    .as_ref()
                    .is_some_and(|metadata| metadata.get("derived_from_vault_path").is_some())
                    || txn.get_kg_node(&node.id)?.is_none()
                {
                    txn.upsert_kg_node(node)?;
                }
            }
        }
        for graph in &graphs {
            for edge in &graph.edges {
                txn.upsert_kg_edge(edge)?;
            }
        }
        let manifest = supplied.map(|vault| &vault.manifest);
        for (record, turns) in txn.kg_session_rows()? {
            txn.project_session_kg_with(&record, &turns, manifest)?;
        }
        self.project_attestations_kg(txn)?;
        self.refresh_change_availability_kg(txn)?;
        // No canonical endpoint may silently dangle; ontology individuals are
        // intentionally resources defined by the embedded ontology, not rows.
        for edge in txn.all_kg_edges()? {
            if txn.get_kg_node(&edge.from_id)?.is_none()
                || (txn.get_kg_node(&edge.to_id)?.is_none()
                    && !matches!(
                        edge.to_id.as_str(),
                        "https://atomic.dev/ns/workflow#Verified"
                            | "https://atomic.dev/ns/workflow#Failed"
                            | "https://atomic.dev/ns/workflow#Unverifiable"
                    ))
            {
                return Err(RepositoryError::InvalidOperation {
                    message: format!("unclosed KG edge: {} -> {}", edge.from_id, edge.to_id),
                });
            }
        }
        txn.finish_kg_rebuild()?;
        Ok(())
    }

    pub(super) fn refresh_session_kg(&self) -> Result<(), RepositoryError> {
        self.ensure_project_identity(MintReason::Upgrade)?;
        let mut txn = self.pristine.write_txn()?;
        if !txn.kg_writes_enabled()? {
            return Ok(());
        }
        for (record, turns) in txn.kg_session_rows()? {
            txn.project_session_kg(&record, &turns)?;
        }
        self.project_attestations_kg(&mut txn)?;
        self.refresh_change_availability_kg(&mut txn)?;
        txn.commit()?;
        Ok(())
    }
}

/// What each vault entry contributes to the graph, in path order.
fn vault_graphs(
    vault: &impl VaultTxnT,
    project: atomic_core::kg::project_id::ProjectId,
    hashes: Vec<Hash>,
) -> Result<Vec<KgSubgraph>, RepositoryError> {
    let context = super::vault_projection::VaultProjection::new(vault, project, hashes)?;
    let mut graphs = Vec::new();
    for meta in vault.list_vault_entries("", None)? {
        if let Some(entry) = vault.get_vault_entry(&meta.path)? {
            graphs.push(context.extract(&meta.path, &entry)?);
        }
    }
    Ok(graphs)
}

/// A vault held in memory, read the way the vault tables are, so the
/// projection that reads the tables reads it unchanged.
struct SuppliedVault {
    entries: BTreeMap<String, VaultEntry>,
    manifest: VaultManifest,
}

impl SuppliedVault {
    fn new(rows: &[(String, VaultEntry)]) -> Result<Self, RepositoryError> {
        let mut entries = BTreeMap::new();
        for (path, entry) in rows {
            if entries.insert(path.clone(), entry.clone()).is_some() {
                return Err(RepositoryError::InvalidOperation {
                    message: format!("the vault was given {path} more than once"),
                });
            }
        }
        // The projection resolves a reference to an intent through the
        // manifest's intent summaries: an intent's human key and its uid.
        // Both are in the intent's frontmatter, which is where a working
        // vault's summaries get them, so the summaries are rebuilt from it.
        // As there, an intent with no `id` is not listed, and the first file
        // to claim a key keeps it.
        let mut manifest = VaultManifest::default();
        for (path, entry) in &entries {
            if entry.entry_type != VaultEntryType::Intent || !path.starts_with("intents/") {
                continue;
            }
            let Ok(fm) = serde_json::from_str::<serde_json::Value>(&entry.frontmatter_json) else {
                continue;
            };
            let text = |key: &str| fm.get(key).and_then(|v| v.as_str()).map(str::to_owned);
            let Some(human_key) = text("id") else {
                continue;
            };
            manifest
                .intents
                .entry(human_key.clone())
                .or_insert_with(|| IntentSummary {
                    uid: text("uid").unwrap_or_default(),
                    vault_path: path.clone(),
                    human_key,
                    ..Default::default()
                });
        }
        Ok(Self { entries, manifest })
    }
}

impl VaultTxnT for SuppliedVault {
    fn get_vault_entry(&self, path: &str) -> Result<Option<VaultEntry>, PristineError> {
        Ok(self.entries.get(path).cloned())
    }

    fn list_vault_entries(
        &self,
        prefix: &str,
        entry_type_filter: Option<VaultEntryType>,
    ) -> Result<Vec<VaultEntryMeta>, PristineError> {
        Ok(self
            .entries
            .iter()
            .filter(|(path, _)| path.starts_with(prefix))
            .filter(|(_, entry)| entry_type_filter.is_none_or(|kind| entry.entry_type == kind))
            .map(|(path, entry)| VaultEntryMeta {
                path: path.clone(),
                entry_type: entry.entry_type,
                content_hash: entry.content_hash,
                content_size: entry.content_bytes.len(),
                updated_at: entry.updated_at.clone(),
            })
            .collect())
    }

    fn get_vault_manifest(&self) -> Result<VaultManifest, PristineError> {
        Ok(self.manifest.clone())
    }

    fn has_vault(&self) -> Result<bool, PristineError> {
        Ok(true)
    }
}

#[cfg(test)]
mod supplied_vault_tests {
    use crate::{IntentCreateOptions, Repository};
    use atomic_core::change::ProvenanceGraph;
    use atomic_core::kg::ontology::{Class, Predicate};
    use atomic_core::pristine::VaultEntry;
    use tempfile::tempdir;

    /// A session turn governed by the intent a turn calls `plan`.
    fn governed_by(plan: &str) -> ProvenanceGraph {
        ProvenanceGraph::builder("sess-1", "claude-code")
            .plan_id(plan.to_string())
            .timestamp(1_700_000_000)
            .build()
    }

    /// A working vault with one intent, and a session turn governed by it.
    fn working() -> (tempfile::TempDir, Repository, String) {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();
        let intent = repo
            .vault_intent_create(IntentCreateOptions {
                title: "Ship billing export".into(),
                priority: Some("high".into()),
                assignee: Some("dana".into()),
                labels: vec!["billing".into()],
                session_id: None,
                turn_id: None,
                kind: None,
            })
            .unwrap();
        repo.save_provenance_graph(&governed_by(&intent.id))
            .unwrap();
        (dir, repo, intent.id)
    }

    fn rows(repo: &Repository) -> Vec<(String, VaultEntry)> {
        repo.vault_list("", None)
            .unwrap()
            .into_iter()
            .map(|meta| {
                let entry = repo.vault_retrieve(&meta.path).unwrap().unwrap();
                (meta.path, entry)
            })
            .collect()
    }

    /// Every node and edge of the graph, in a comparable order.
    fn graph(repo: &Repository) -> (Vec<String>, Vec<String>) {
        let snapshot = repo.workflow_snapshot().unwrap();
        let mut nodes: Vec<String> = snapshot
            .nodes
            .iter()
            .map(|node| serde_json::to_string(node).unwrap())
            .collect();
        let mut edges: Vec<String> = snapshot
            .edges
            .iter()
            .map(|edge| serde_json::to_string(edge).unwrap())
            .collect();
        nodes.sort();
        edges.sort();
        (nodes, edges)
    }

    fn intents(repo: &Repository) -> usize {
        repo.workflow_snapshot()
            .unwrap()
            .nodes
            .iter()
            .filter(|node| node.has_type(Class::WorkIntent))
            .count()
    }

    fn governed_turns(repo: &Repository) -> usize {
        repo.workflow_snapshot()
            .unwrap()
            .edges
            .iter()
            .filter(|edge| edge.kind == Predicate::HasPlan)
            .count()
    }

    #[test]
    fn the_vault_tables_rows_supplied_give_the_graph_the_tables_give() {
        let (_dir, repo, _) = working();
        repo.rebuild_kg(true).unwrap();
        let from_tables = graph(&repo);
        assert_eq!(intents(&repo), 1);
        assert_eq!(governed_turns(&repo), 1, "the fixture links a turn");

        repo.rebuild_kg_from_vault_entries(&rows(&repo)).unwrap();
        assert_eq!(graph(&repo), from_tables);
    }

    #[test]
    fn a_supplied_vault_leaves_the_vault_tables_alone() {
        let (_dir, repo, _) = working();
        repo.rebuild_kg(true).unwrap();
        let from_tables = graph(&repo);
        let stored = serde_json::to_string(&rows(&repo)).unwrap();
        let manifest = serde_json::to_value(repo.vault_manifest().unwrap()).unwrap();

        repo.rebuild_kg_from_vault_entries(&[]).unwrap();
        assert_eq!(intents(&repo), 0, "the graph describes the empty vault");
        assert_eq!(governed_turns(&repo), 0);
        assert_eq!(serde_json::to_string(&rows(&repo)).unwrap(), stored);
        assert_eq!(
            serde_json::to_value(repo.vault_manifest().unwrap()).unwrap(),
            manifest
        );

        repo.rebuild_kg(true).unwrap();
        assert_eq!(graph(&repo), from_tables, "and the tables give it back");
    }

    /// The case this exists for: a repository that never had a vault is given
    /// a view's vault files.
    #[test]
    fn a_repository_with_no_vault_derives_the_graph_from_files() {
        let (_source_dir, source, intent) = working();
        let files: Vec<(String, String)> = rows(&source)
            .into_iter()
            .map(|(path, _)| {
                let content = std::fs::read_to_string(source.vault_dir().join(&path)).unwrap();
                (path, content)
            })
            .collect();
        // The intent's file parses to what the intent API stored for it.
        for ((path, content), (_, stored)) in files
            .iter()
            .zip(rows(&source))
            .filter(|((path, _), _)| path.starts_with("intents/"))
        {
            let parsed = crate::vault_entry_for_file(path, content, &stored.created_at);
            assert_eq!(parsed.entry_type, stored.entry_type, "{path}");
            assert_eq!(parsed.content_bytes, stored.content_bytes, "{path}");
        }

        let dir = tempdir().unwrap();
        let replica = Repository::init(dir.path()).unwrap();
        replica
            .save_provenance_graph(&governed_by(&intent))
            .unwrap();
        let entries: Vec<(String, VaultEntry)> = files
            .iter()
            .map(|(path, content)| {
                let entry = crate::vault_entry_for_file(path, content, "2026-01-01T00:00:00Z");
                (path.clone(), entry)
            })
            .collect();
        replica.rebuild_kg_from_vault_entries(&entries).unwrap();

        assert_eq!(intents(&replica), 1);
        assert_eq!(governed_turns(&replica), 1);
        assert!(!replica.has_vault().unwrap(), "no vault table was created");
    }

    #[test]
    fn a_path_given_twice_is_refused_and_the_graph_is_kept() {
        let (_dir, repo, _) = working();
        repo.rebuild_kg(true).unwrap();
        let before = graph(&repo);
        let mut twice = rows(&repo);
        twice.push(twice[0].clone());

        let refused = repo.rebuild_kg_from_vault_entries(&twice).unwrap_err();
        assert!(refused.to_string().contains(&twice[0].0), "{refused}");
        assert_eq!(graph(&repo), before);
    }
}
