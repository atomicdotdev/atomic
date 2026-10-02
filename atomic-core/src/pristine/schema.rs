//! Repository database schema: the table registry and the schema version.
//!
//! A repository keeps every redb table in one file. [`visit_tables`] is the
//! single list of those tables with their key and value types; merging legacy
//! databases copies exactly these, and a test fails when a table definition is
//! added without being listed here.

use redb::{
    Key, MultimapTableDefinition, MultimapTableHandle, ReadTransaction, ReadableTable,
    TableDefinition, TableError, TableHandle, Value, WriteTransaction,
};

use crate::crdt::tables as crdt;
use crate::pristine::error::{PristineError, PristineResult};
use crate::pristine::tables::*;

/// Schema version written by this build. Opening a database stamped with a
/// newer version fails instead of misreading tables this build does not know.
pub const SCHEMA_VERSION: u64 = 1;

/// [`ATOMIC_META`] key for the schema version (little-endian u64).
pub const SCHEMA_VERSION_KEY: &str = "schema_version";

/// [`ATOMIC_META`] key naming the `.atomic/legacy/` subdirectory that holds
/// the files a merged database was built from.
pub const LEGACY_DIR_KEY: &str = "legacy_dir";

/// Tables that earlier releases created and no code reads any more. Merging
/// a legacy database leaves their rows in the retained legacy files instead
/// of refusing, as it does for tables it has never heard of.
pub(crate) const RETIRED_TABLES: &[&str] = &[
    // Views were called channels, then stacks.
    "channels",
    "channel_changes",
    "rev_channel_changes",
    "stacks",
    "stack_changes",
    "rev_stack_changes",
    "stack_graph",
    // Replaced by `file_index`.
    "file_mtimes",
];

/// Receives every registered table with its concrete key and value types.
pub(crate) trait TableVisitor {
    fn table<K: Key + 'static, V: Value + 'static>(
        &mut self,
        definition: TableDefinition<'static, K, V>,
    ) -> PristineResult<()>;

    fn multimap<K: Key + 'static, V: Key + 'static>(
        &mut self,
        definition: MultimapTableDefinition<'static, K, V>,
    ) -> PristineResult<()>;
}

/// Visit every table a repository database may contain.
pub(crate) fn visit_tables(visitor: &mut impl TableVisitor) -> PristineResult<()> {
    // ID mappings
    visitor.table(EXTERNAL)?;
    visitor.table(INTERNAL)?;
    visitor.table(NODE_TYPES)?;

    // Graph
    visitor.multimap(GRAPH)?;
    visitor.multimap(INODE_GRAPH)?;

    // Views
    visitor.table(VIEWS)?;
    visitor.table(VIEW_CHANGES)?;
    visitor.table(REV_VIEW_CHANGES)?;
    visitor.table(CONFLICTS)?;

    // File tree
    visitor.table(TREE)?;
    visitor.table(REV_TREE)?;
    visitor.table(INODES)?;
    visitor.table(REV_INODES)?;
    visitor.table(DIRECTORIES)?;
    visitor.table(FILE_INDEX)?;

    // Dependencies
    visitor.multimap(DEPS)?;
    visitor.multimap(REV_DEPS)?;
    visitor.multimap(CHANGE_DEPS)?;
    visitor.multimap(REV_CHANGE_DEPS)?;
    visitor.table(CHANGE_DEPS_INDEXED)?;

    // State and tags
    visitor.table(STATES)?;
    visitor.table(MERKLE_CHAIN)?;
    visitor.table(TAG_RECORDS)?;
    visitor.table(TAG_NAME_INDEX)?;
    visitor.table(GIT_SHA_INDEX)?;

    // Redb-native change store
    visitor.table(CHANGE_META)?;
    visitor.table(CHANGE_GRAPH)?;
    visitor.table(CHANGE_SEMANTIC)?;
    visitor.table(CONTENT_CHUNKS)?;
    visitor.table(CHANGE_CHUNKS)?;
    visitor.table(CHANGE_UNHASHED)?;
    visitor.table(CHANGE_SIGNATURES)?;

    // Provenance journal
    visitor.table(PROVENANCE_STORE_META)?;
    visitor.table(PROVENANCE_TURN_INDEX)?;
    visitor.table(PROVENANCE_TURNS)?;
    visitor.table(PROVENANCE_JOURNAL_EVENTS)?;
    visitor.table(PROVENANCE_EVENT_INDEX)?;
    visitor.table(PROVENANCE_FINAL_HASHES)?;

    // Sessions
    visitor.table(SESSION_EVENTS)?;
    visitor.table(SESSION_TODOS)?;
    visitor.table(SESSION_PHASES)?;
    visitor.table(SESSION_INTENTS)?;
    visitor.table(SESSIONS)?;
    visitor.table(SESSION_TURNS)?;
    visitor.table(SESSION_PROVENANCE)?;
    visitor.table(SESSION_MANIFESTS)?;
    visitor.table(SESSION_HEADS)?;

    // Vault and knowledge graph
    visitor.table(VAULT_ENTRIES)?;
    visitor.table(VAULT_MANIFEST)?;
    visitor.table(KG_NODES)?;
    visitor.table(KG_EDGES)?;
    visitor.multimap(KG_EDGES_FROM)?;
    visitor.multimap(KG_EDGES_TO)?;
    visitor.multimap(KG_FTS)?;
    visitor.multimap(KG_FTS_BY_NODE)?;
    visitor.table(KG_INDEX_META)?;
    visitor.table(EMBEDDINGS)?;

    // Semantic CRDT
    visitor.table(crdt::TRUNKS)?;
    visitor.table(crdt::BRANCHES)?;
    visitor.table(crdt::LEAVES)?;
    visitor.multimap(crdt::TRUNK_BRANCHES)?;
    visitor.multimap(crdt::BRANCH_LEAVES)?;
    visitor.table(crdt::INODE_TRUNK)?;
    visitor.table(crdt::PATH_TRUNK)?;
    visitor.table(crdt::BRANCH_VERTEX)?;
    visitor.table(crdt::VERTEX_BRANCH)?;
    visitor.table(crdt::BRANCH_AFTER)?;

    // Database metadata
    visitor.table(ATOMIC_META)?;
    Ok(())
}

/// Names of every registered table.
pub(crate) fn table_names() -> Vec<String> {
    struct Names(Vec<String>);

    impl TableVisitor for Names {
        fn table<K: Key + 'static, V: Value + 'static>(
            &mut self,
            definition: TableDefinition<'static, K, V>,
        ) -> PristineResult<()> {
            self.0.push(definition.name().to_string());
            Ok(())
        }

        fn multimap<K: Key + 'static, V: Key + 'static>(
            &mut self,
            definition: MultimapTableDefinition<'static, K, V>,
        ) -> PristineResult<()> {
            self.0.push(definition.name().to_string());
            Ok(())
        }
    }

    let mut names = Names(Vec::new());
    visit_tables(&mut names).expect("collecting table names cannot fail");
    names.0
}

/// Record [`SCHEMA_VERSION`] unless the database already carries a version.
pub(crate) fn stamp_schema_version(txn: &WriteTransaction) -> PristineResult<()> {
    let mut meta = txn.open_table(ATOMIC_META)?;
    if meta.get(SCHEMA_VERSION_KEY)?.is_none() {
        meta.insert(SCHEMA_VERSION_KEY, SCHEMA_VERSION.to_le_bytes().as_slice())?;
    }
    Ok(())
}

/// Fail if the database was stamped by a newer schema than this build reads.
pub(crate) fn check_schema_version(txn: &ReadTransaction) -> PristineResult<()> {
    let meta = match txn.open_table(ATOMIC_META) {
        Ok(meta) => meta,
        Err(TableError::TableDoesNotExist(_)) => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let Some(stored) = meta.get(SCHEMA_VERSION_KEY)? else {
        return Ok(());
    };
    let found = <[u8; 8]>::try_from(stored.value())
        .map(u64::from_le_bytes)
        .map_err(|_| PristineError::Inconsistent {
            message: "schema version in atomic_meta is not 8 bytes".to_string(),
        })?;
    if found > SCHEMA_VERSION {
        return Err(PristineError::UnsupportedSchema {
            found,
            supported: SCHEMA_VERSION,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use redb::ReadableDatabase;

    use super::*;

    fn defined_names(source: &'static str) -> impl Iterator<Item = &'static str> {
        source
            .split("TableDefinition::new(\"")
            .skip(1)
            .filter_map(|rest| rest.split('"').next())
    }

    #[test]
    fn registry_lists_every_table_definition() {
        let defined: BTreeSet<_> = defined_names(include_str!("tables.rs"))
            .chain(defined_names(include_str!("../crdt/tables.rs")))
            .collect();
        let registered = table_names();
        let unique: BTreeSet<_> = registered.iter().map(String::as_str).collect();

        assert_eq!(
            unique.len(),
            registered.len(),
            "a table is registered twice"
        );
        assert_eq!(defined, unique);
    }

    #[test]
    fn retired_tables_are_not_registered() {
        let registered = table_names();
        for retired in RETIRED_TABLES {
            assert!(
                !registered.iter().any(|name| name == retired),
                "{retired} is registered again; remove it from RETIRED_TABLES"
            );
        }
    }

    #[test]
    fn newer_schema_versions_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let db = redb::Database::create(dir.path().join("db")).unwrap();
        let txn = db.begin_write().unwrap();
        stamp_schema_version(&txn).unwrap();
        txn.open_table(ATOMIC_META)
            .unwrap()
            .insert(
                SCHEMA_VERSION_KEY,
                (SCHEMA_VERSION + 1).to_le_bytes().as_slice(),
            )
            .unwrap();
        txn.commit().unwrap();

        let error = check_schema_version(&db.begin_read().unwrap()).unwrap_err();
        assert!(matches!(
            error,
            PristineError::UnsupportedSchema { found, supported }
                if found == SCHEMA_VERSION + 1 && supported == SCHEMA_VERSION
        ));
    }

    #[test]
    fn stamping_keeps_an_existing_version() {
        let dir = tempfile::tempdir().unwrap();
        let db = redb::Database::create(dir.path().join("db")).unwrap();
        let txn = db.begin_write().unwrap();
        stamp_schema_version(&txn).unwrap();
        stamp_schema_version(&txn).unwrap();
        txn.commit().unwrap();

        let txn = db.begin_read().unwrap();
        check_schema_version(&txn).unwrap();
        let meta = txn.open_table(ATOMIC_META).unwrap();
        let stored = meta.get(SCHEMA_VERSION_KEY).unwrap().unwrap();
        assert_eq!(stored.value(), SCHEMA_VERSION.to_le_bytes().as_slice());
    }
}
