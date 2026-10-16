use super::*;
use atomic_core::{
    kg::{project_id::MintReason, KG_GENERATION},
    pristine::{KgMutTxnT, KgTxnT, VaultTxnT},
};

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
        txn.begin_kg_rebuild()?;
        self.project_vcs_kg(&mut txn)?;
        let hashes = self
            .change_store
            .iter_changes()
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let context =
            super::vault_projection::VaultProjection::new(&txn, txn.kg_project()?, hashes)?;
        let mut graphs = Vec::new();
        for meta in txn.list_vault_entries("", None)? {
            if let Some(entry) = txn.get_vault_entry(&meta.path)? {
                graphs.push(context.extract(&meta.path, &entry)?);
            }
        }
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
        for (record, turns) in txn.kg_session_rows()? {
            txn.project_session_kg(&record, &turns)?;
        }
        self.project_attestations_kg(&mut txn)?;
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
        txn.commit()?;
        Ok(true)
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
        txn.commit()?;
        Ok(())
    }
}
