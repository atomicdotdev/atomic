//! Workflow projection over immutable changes and actual view memberships.
//! Source structure is parsed by atomic-semantic at query time.

use super::*;
use atomic_core::pristine::{KgMutTxnT, KgTxnT};
use atomic_core::{
    kg::{
        iri,
        ontology::{Class as C, Predicate as P},
        project_id::MintReason,
    },
    KgEdge, KgNode,
};

#[derive(Clone, Debug, Default)]
pub struct KgEnrichStats {
    pub views: usize,
    pub files: usize,
    pub changes: usize,
}
impl KgEnrichStats {
    pub fn total(&self) -> usize {
        self.views + self.files + self.changes
    }
}
impl std::fmt::Display for KgEnrichStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} views, {} files, {} changes",
            self.views, self.files, self.changes
        )
    }
}

impl Repository {
    pub fn kg_enrich_from_vcs(
        &self,
        working_copy: WorkingCopyId,
    ) -> Result<KgEnrichStats, RepositoryError> {
        self.validate_working_copy(working_copy)?;
        self.ensure_project_identity(MintReason::Upgrade)?;
        let mut txn = self.pristine.write_txn()?;
        if !txn.kg_writes_enabled()? {
            return Ok(KgEnrichStats::default());
        }
        let stats = self.project_vcs_kg(&mut txn)?;
        txn.commit()?;
        Ok(stats)
    }

    pub fn kg_enrich_views(&self) -> Result<usize, RepositoryError> {
        self.ensure_project_identity(MintReason::Upgrade)?;
        let mut txn = self.pristine.write_txn()?;
        if !txn.kg_writes_enabled()? {
            return Ok(0);
        }
        let count = self.project_view_kg(&mut txn)?;
        txn.commit()?;
        Ok(count)
    }

    pub fn kg_enrich_files(&self, working_copy: WorkingCopyId) -> Result<usize, RepositoryError> {
        Ok(self.kg_enrich_from_vcs(working_copy)?.files)
    }

    pub fn kg_enrich_changes(&self, working_copy: WorkingCopyId) -> Result<usize, RepositoryError> {
        Ok(self.kg_enrich_from_vcs(working_copy)?.changes)
    }

    pub fn kg_enrich_change(
        &self,
        working_copy: WorkingCopyId,
        hash: &Hash,
    ) -> Result<(), RepositoryError> {
        self.validate_working_copy(working_copy)?;
        self.ensure_project_identity(MintReason::Upgrade)?;
        let change = self.load_change(hash)?;
        let mut txn = self.pristine.write_txn()?;
        if !txn.kg_writes_enabled()? {
            return Ok(());
        }
        self.project_change_kg(&mut txn, hash, &change)?;
        self.project_view_kg(&mut txn)?;
        txn.commit()?;
        Ok(())
    }

    pub fn enrich_change(
        &self,
        working_copy: WorkingCopyId,
        hash: &Hash,
    ) -> Result<(), RepositoryError> {
        self.kg_enrich_change(working_copy, hash)?;
        let paths: Vec<String> = self
            .load_change(hash)?
            .file_ops()
            .iter()
            .map(|op| op.path().to_owned())
            .filter(|p| !p.is_empty())
            .collect();
        if !paths.is_empty() {
            if let Err(error) =
                crate::content_search::update_content_index_paths(self.root(), &paths)
            {
                log::debug!("content index refresh: {error}");
            }
        }
        Ok(())
    }

    pub(super) fn project_vcs_kg(
        &self,
        txn: &mut atomic_core::WriteTxn<'_>,
    ) -> Result<KgEnrichStats, RepositoryError> {
        let mut stats = KgEnrichStats::default();
        for hash in self.change_store.iter_changes() {
            let hash = hash.map_err(|e| RepositoryError::Database(e.to_string()))?;
            let change = self.load_change(&hash)?;
            if change.kind().is_snapshot() {
                continue;
            }
            stats.files += self.project_change_kg(txn, &hash, &change)?;
            stats.changes += 1;
        }
        stats.views = self.project_view_kg(txn)?;
        self.refresh_change_availability_kg(txn)?;
        Ok(stats)
    }

    pub(super) fn project_change_kg(
        &self,
        txn: &mut atomic_core::WriteTxn<'_>,
        hash: &Hash,
        change: &Change,
    ) -> Result<usize, RepositoryError> {
        let project = txn.kg_project()?;
        let id = iri::mint_change(hash);
        let header = &change.hashed.header;
        let mut node = KgNode::new(&id, C::Change, &hash.to_base32()[..12], "vcs")
            .with_summary(&header.message).with_metadata(serde_json::json!({"hash": hash.to_base32(), "created_at": header.timestamp.to_rfc3339(),
                "object_availability": "available", "projection_completeness": "projected"}));
        crate::workflow_usage::set_change_records(
            &mut node,
            Some(crate::workflow_usage::change_records(change, hash)),
        )?;
        txn.upsert_kg_node(&node)?;
        for author in &header.authors {
            let author_id = iri::mint_identity(&project, &author.name);
            txn.upsert_kg_node(&KgNode::new(&author_id, C::Identity, &author.name, "vcs"))?;
            txn.upsert_kg_edge(&KgEdge::new(&id, author_id, P::AuthoredBy))?;
        }
        let mut missing_dependency = false;
        for hash in change.dependencies() {
            // Registration and filesystem existence are not integrity evidence.
            let available = match self.load_change(hash) {
                Ok(_) => true,
                Err(RepositoryError::ChangeNotFound { .. }) => false,
                Err(error) => return Err(error),
            };
            if available {
                let target = iri::mint_change(hash);
                if txn.get_kg_node(&target)?.is_none() {
                    txn.upsert_kg_node(&KgNode::new(
                        &target,
                        C::Change,
                        &hash.to_base32()[..12],
                        "vcs",
                    ).with_metadata(serde_json::json!({"hash": hash.to_base32(),
                        "object_availability": "available", "projection_completeness": "reference-only"})))?;
                }
                txn.upsert_kg_edge(&KgEdge::new(&id, target, P::DirectlyDependsOnChange))?;
            } else {
                missing_dependency = true;
                txn.kg_finding(
                    "UnresolvedReference",
                    &id,
                    "A change dependency is not stored locally",
                )?;
            }
        }
        if !missing_dependency {
            txn.del_kg_node(&iri::mint_finding(&project, "UnresolvedReference", &id))?;
        }
        // Recorded operations are immutable impact facts. Current working-copy
        // indexing exclusions must not change their historical projection.
        let mut paths = std::collections::BTreeSet::new();
        for file in change.file_ops() {
            let path = file.path();
            if path.is_empty() {
                continue;
            }
            paths.insert(path);
            let file_id = iri::mint_file(&project, path);
            txn.upsert_kg_node(
                &KgNode::new(
                    &file_id,
                    C::File,
                    path.rsplit('/').next().unwrap_or(path),
                    "vcs",
                )
                .with_metadata(serde_json::json!({"path": path})),
            )?;
            txn.upsert_kg_edge(&KgEdge::new(&id, file_id, P::ModifiesFile))?;
        }
        Ok(paths.len())
    }

    /// Reconcile reference nodes against verified canonical objects. In
    /// particular a session ledger may mention an object never received here.
    /// This preserves node identity and independent edges, and never treats a
    /// malformed/unreadable object as absent or available.
    pub(super) fn refresh_change_availability_kg(
        &self,
        txn: &mut atomic_core::WriteTxn<'_>,
    ) -> Result<(), RepositoryError> {
        for mut node in txn.all_kg_nodes()? {
            if !node.has_type(C::Change) {
                continue;
            }
            let hash = iri::parse_change(&node.id)
                .map_err(|error| RepositoryError::Serialization(error.to_string()))?;
            let availability = match self.load_change(&hash) {
                Ok(change) if !change.kind().is_snapshot() => {
                    if node
                        .metadata
                        .as_ref()
                        .and_then(|metadata| metadata.get("projection_completeness"))
                        .and_then(serde_json::Value::as_str)
                        != Some("projected")
                    {
                        self.project_change_kg(txn, &hash, &change)?;
                    }
                    continue;
                }
                Ok(_) => "available",
                Err(RepositoryError::ChangeNotFound { .. }) => "absent",
                Err(error) => return Err(error),
            };
            let metadata = node.metadata.get_or_insert_with(|| serde_json::json!({}));
            metadata["object_availability"] = availability.into();
            metadata["projection_completeness"] = "reference-only".into();
            if let Some(metadata) = metadata.as_object_mut() {
                metadata.remove("created_at");
                for field in [
                    "ai_usage",
                    "ai_usage_records",
                    "ai_usage_state",
                    "ai_usage_record_version",
                ] {
                    metadata.remove(field);
                }
            }
            node.summary = None;
            for edge in txn.get_kg_edges_from(&node.id)? {
                if matches!(
                    edge.kind,
                    P::AuthoredBy | P::DirectlyDependsOnChange | P::ModifiesFile
                ) {
                    txn.del_kg_edge(&edge.from_id, &edge.to_id, &edge.kind)?;
                }
            }
            txn.upsert_kg_node(&node)?;
        }
        Ok(())
    }

    pub(super) fn repair_change_dependencies_kg(
        &self,
        txn: &mut atomic_core::WriteTxn<'_>,
        arrived: &Hash,
    ) -> Result<(), RepositoryError> {
        let mut pending = std::collections::BTreeSet::new();
        for edge in txn.all_kg_edges()? {
            if edge.kind != P::AboutResource {
                continue;
            }
            let Some(finding) = txn.get_kg_node(&edge.from_id)? else {
                continue;
            };
            if finding
                .metadata
                .as_ref()
                .and_then(|m| m.get("finding_code"))
                .and_then(serde_json::Value::as_str)
                != Some("UnresolvedReference")
            {
                continue;
            }
            if let Ok(hash) = iri::parse_change(&edge.to_id) {
                pending.insert(hash);
            }
        }
        for hash in pending {
            if self.has_change(&hash) {
                let change = self.load_change(&hash)?;
                if change.dependencies().contains(arrived) {
                    self.project_change_kg(txn, &hash, &change)?;
                }
            }
        }
        Ok(())
    }

    pub(super) fn project_view_kg(
        &self,
        txn: &mut atomic_core::WriteTxn<'_>,
    ) -> Result<usize, RepositoryError> {
        let project = txn.kg_project()?;
        let record = txn
            .project_identity()?
            .ok_or(atomic_core::PristineError::MissingProjectIdentity)?;
        let project_iri = iri::mint_project(&project);
        let identity = iri::mint_project_identity(&record.hash);
        txn.upsert_kg_node(&KgNode::new(
            &project_iri,
            C::Project,
            project.to_base32(),
            "vcs",
        ))?;
        txn.upsert_kg_node(&KgNode::new(&identity, C::ProjectIdentity, project.to_base32(), "vcs")
            .with_metadata(serde_json::json!({"created_at": record.created_at, "project_id": project.to_base32(), "schema_version": 1})))?;
        txn.upsert_kg_edge(&KgEdge::new(&project_iri, &identity, P::HasIdentity))?;
        let object: serde_json::Value = serde_json::from_slice(&record.object)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        if let Some(issuer) = object
            .get("atomic:issuer")
            .and_then(|v| v.get("@id"))
            .and_then(|v| v.as_str())
        {
            txn.upsert_kg_node(&KgNode::new(issuer, C::Identity, issuer, "vcs"))?;
            txn.upsert_kg_edge(&KgEdge::new(&identity, issuer, P::Issuer))?;
        }
        let views = txn
            .list_views()?
            .iter()
            .map(|name| txn.get_view(name))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        let mut live = std::collections::HashSet::new();
        for view in &views {
            let id = iri::mint_view(&project, &view.name);
            live.insert(id.clone());
            txn.upsert_kg_node(&KgNode::new(&id, C::View, &view.name, "vcs").with_metadata(
                serde_json::json!({"name": view.name, "scope": format!("{:?}", view.kind)}),
            ))?;
        }
        for edge in txn.all_kg_edges()? {
            if matches!(edge.kind, P::DirectlyOnView | P::ChildViewOf) {
                txn.del_kg_edge(&edge.from_id, &edge.to_id, &edge.kind)?;
            }
        }
        for node in txn.all_kg_nodes()? {
            if node.has_type(C::View) && !live.contains(&node.id) {
                txn.del_kg_node(&node.id)?;
            }
        }
        for view in &views {
            let id = iri::mint_view(&project, &view.name);
            if let Some(parent) = view
                .parent
                .and_then(|parent| views.iter().find(|view| view.id == parent))
            {
                txn.upsert_kg_edge(&KgEdge::new(
                    &id,
                    iri::mint_view(&project, &parent.name),
                    P::ChildViewOf,
                ))?;
            }
            let memberships = txn.iter_changes(view, 0)?.collect::<Result<Vec<_>, _>>()?;
            for (_, change, _) in memberships {
                if let Some(hash) = txn.get_external(change)? {
                    let change = iri::mint_change(&hash);
                    if txn.get_kg_node(&change)?.is_none() {
                        txn.upsert_kg_node(&KgNode::new(
                            &change,
                            C::Change,
                            &hash.to_base32()[..12],
                            "vcs",
                        ))?;
                    }
                    txn.upsert_kg_edge(&KgEdge::new(change, &id, P::DirectlyOnView))?;
                }
            }
        }
        Ok(views.len())
    }
}
