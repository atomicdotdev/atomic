use super::{ReadTxn, WriteTxn};
use crate::kg::project_id::ProjectIdentityRecord;
use crate::pristine::tables::*;
use crate::pristine::{
    tables::{PROJECT_IDENTITY, SIGNER_KEYS},
    PristineError, PristineResult,
};
use redb::{ReadableMultimapTable, ReadableTable};

fn serialization(error: impl std::fmt::Display) -> PristineError {
    PristineError::Serialization {
        message: error.to_string(),
    }
}

macro_rules! reads {
    () => {
        pub fn all_kg_nodes(&self) -> PristineResult<Vec<crate::KgNode>> {
            self.require_current_kg()?;
            let table = match self.txn.open_table(KG_NODES) {
                Ok(table) => table,
                Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
                Err(error) => return Err(error.into()),
            };
            table
                .iter()?
                .map(|row| crate::KgNode::from_json(row?.1.value()).map_err(serialization))
                .collect()
        }
        pub fn all_kg_edges(&self) -> PristineResult<Vec<crate::KgEdge>> {
            self.require_current_kg()?;
            let table = match self.txn.open_table(KG_EDGES) {
                Ok(table) => table,
                Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
                Err(error) => return Err(error.into()),
            };
            table
                .iter()?
                .map(|row| serde_json::from_slice(row?.1.value()).map_err(serialization))
                .collect()
        }
        pub fn kg_node_types(&self, node: &str) -> PristineResult<Vec<String>> {
            self.require_current_kg()?;
            let table = self.txn.open_multimap_table(KG_TYPES)?;
            let result = table
                .get(node)?
                .map(|row| {
                    row.map(|row| row.value().to_owned())
                        .map_err(PristineError::from)
                })
                .collect();
            result
        }
        pub fn kg_nodes_of_type(
            &self,
            class: crate::kg::ontology::Class,
        ) -> PristineResult<Vec<String>> {
            self.require_current_kg()?;
            let table = self.txn.open_multimap_table(KG_TYPES_REV)?;
            let result = table
                .get(class.iri())?
                .map(|row| {
                    row.map(|row| row.value().to_owned())
                        .map_err(PristineError::from)
                })
                .collect();
            result
        }
        pub fn kg_generation(&self) -> PristineResult<u32> {
            let table = match self.txn.open_table(KG_INDEX_META) {
                Ok(table) => table,
                Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
                Err(error) => return Err(error.into()),
            };
            let row = table.get("kg_generation")?;
            Ok(row.map(|row| row.value()).unwrap_or(0))
        }
        pub fn project_identity(&self) -> PristineResult<Option<ProjectIdentityRecord>> {
            let table = match self.txn.open_table(PROJECT_IDENTITY) {
                Ok(table) => table,
                Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
                Err(error) => return Err(error.into()),
            };
            let row = table.get(0)?;
            row.map(|row| serde_json::from_slice(row.value()).map_err(serialization))
                .transpose()
        }
        pub fn signer_key(&self, did: &str) -> PristineResult<Option<String>> {
            let table = match self.txn.open_table(SIGNER_KEYS) {
                Ok(table) => table,
                Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
                Err(error) => return Err(error.into()),
            };
            let row = table.get(did)?;
            Ok(row.map(|row| row.value().to_owned()))
        }
    };
}
impl ReadTxn {
    reads!();
    pub fn require_current_kg(&self) -> PristineResult<()> {
        if self.kg_generation()? != crate::kg::KG_GENERATION {
            return Err(PristineError::KgNeedsRebuild);
        }
        Ok(())
    }
}
impl WriteTxn<'_> {
    reads!();

    pub fn kg_session_rows(
        &self,
    ) -> PristineResult<
        Vec<(
            crate::change::session::SessionRecord,
            Vec<crate::change::session::SessionTurn>,
        )>,
    > {
        use crate::change::session::{SessionRecord, SessionTurn};
        let mut turns = std::collections::BTreeMap::<String, Vec<SessionTurn>>::new();
        for row in self.txn.open_table(SESSION_TURNS)?.iter()? {
            let turn = SessionTurn::from_bytes(row?.1.value()).map_err(serialization)?;
            turns.entry(turn.session_id.clone()).or_default().push(turn);
        }
        let mut result = Vec::new();
        for row in self.txn.open_table(SESSIONS)?.iter()? {
            let record = SessionRecord::from_bytes(row?.1.value()).map_err(serialization)?;
            let mut rows = turns.remove(&record.session_id).unwrap_or_default();
            rows.sort_by_key(|row| row.turn_number);
            result.push((record, rows));
        }
        Ok(result)
    }

    pub fn require_current_kg(&self) -> PristineResult<()> {
        if !self.kg_rebuilding && self.kg_generation()? != crate::kg::KG_GENERATION {
            return Err(PristineError::KgNeedsRebuild);
        }
        Ok(())
    }

    pub fn kg_writes_enabled(&self) -> PristineResult<bool> {
        Ok(self.kg_rebuilding || self.kg_generation()? == crate::kg::KG_GENERATION)
    }

    /// All derived tables are replaced atomically; abort leaves the old graph
    /// and generation intact. No partially rebuilt graph becomes readable.
    pub fn begin_kg_rebuild(&mut self) -> PristineResult<()> {
        self.kg_rebuilding = true;
        self.txn.delete_table(KG_NODES)?;
        self.txn.delete_table(KG_EDGES)?;
        self.txn.delete_multimap_table(KG_EDGES_FROM)?;
        self.txn.delete_multimap_table(KG_EDGES_TO)?;
        self.txn.delete_multimap_table(KG_FTS)?;
        self.txn.delete_multimap_table(KG_FTS_BY_NODE)?;
        self.txn.delete_multimap_table(KG_TYPES)?;
        self.txn.delete_multimap_table(KG_TYPES_REV)?;
        self.txn
            .open_table(KG_INDEX_META)?
            .remove("fts_reverse_schema")?;
        crate::pristine::KgMutTxnT::init_kg(self)?;
        Ok(())
    }

    pub fn finish_kg_rebuild(&mut self) -> PristineResult<()> {
        self.txn
            .open_table(KG_INDEX_META)?
            .insert("kg_generation", crate::kg::KG_GENERATION)?;
        self.txn
            .open_table(PRISTINE_META)?
            .insert("required-capability/kg-semweb-v1", 1)?;
        self.kg_rebuilding = false;
        Ok(())
    }

    /// Compare-and-insert; concurrent minting keeps the first committed identity.
    pub fn put_project_identity_if_absent(
        &mut self,
        record: &ProjectIdentityRecord,
    ) -> PristineResult<ProjectIdentityRecord> {
        if let Some(existing) = self.project_identity()? {
            return Ok(existing);
        }
        record.project_id().map_err(serialization)?;
        let bytes = serde_json::to_vec(record).map_err(serialization)?;
        self.txn
            .open_table(PROJECT_IDENTITY)?
            .insert(0, bytes.as_slice())?;
        Ok(record.clone())
    }

    /// Callers must check DID/key correspondence before caching key material.
    pub fn put_signer_key(&mut self, did: &str, public_key: &str) -> PristineResult<()> {
        self.txn.open_table(SIGNER_KEYS)?.insert(did, public_key)?;
        Ok(())
    }
}
