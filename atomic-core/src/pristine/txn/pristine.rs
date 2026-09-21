//! Pristine database handle
//!
//! This module provides the main `Pristine` struct which manages the redb
//! database and provides methods for creating transactions.
//!
//! # Read-Only Mode
//!
//! For commands that only need to read data (like `status`, `diff`, `log`),
//! use `Pristine::open_readonly()` which doesn't acquire a write lock and
//! can run concurrently with other readers or a single writer.
//!
//! ```ignore
//! // For read-only operations (status, diff, log, change)
//! let pristine = Pristine::open_readonly("path/to/pristine")?;
//! let txn = pristine.read_txn()?;
//!
//! // For read-write operations (record, add, apply)
//! let pristine = Pristine::open("path/to/pristine")?;
//! let mut txn = pristine.write_txn()?;
//! ```

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use redb::{Builder, Database, ReadTransaction, ReadableMultimapTable, ReadableTable};

use crate::pristine::capability::{
    unsupported_requirements, RepositoryCapability, RequiredRepositoryCapability,
};
use crate::pristine::error::{PristineError, PristineResult};
use crate::pristine::path_claim::{
    path_claim_schema_error, PATH_CLAIM_SCHEMA_KEY, PATH_CLAIM_SCHEMA_VERSION,
};
use crate::pristine::tables::*;
use crate::pristine::traits::PathClaimTxnT;

use super::helpers::deserialize_view_state;
use super::read::ReadTxn;
use super::write::WriteTxn;

/// Return `max_id + 1`, or error if the ID space is exhausted.
fn next_id(max_id: u64) -> PristineResult<u64> {
    max_id.checked_add(1).ok_or(PristineError::IdSpaceExhausted)
}

fn collect_required_repository_capabilities(
    db: &Database,
) -> PristineResult<Vec<RequiredRepositoryCapability>> {
    let read_txn = db.begin_read()?;
    let metadata = match read_txn.open_table(PRISTINE_META) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut requirements = Vec::new();
    for row in metadata.iter()? {
        let (key, version) = row?;
        if let Some(requirement) =
            RequiredRepositoryCapability::from_metadata(key.value(), version.value())
        {
            requirements.push(requirement);
        }
    }
    requirements.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(requirements)
}

fn ensure_supported_repository_capabilities(
    requirements: &[RequiredRepositoryCapability],
) -> PristineResult<()> {
    let capabilities = unsupported_requirements(requirements);
    if capabilities.is_empty() {
        Ok(())
    } else {
        Err(PristineError::UnsupportedRequiredCapabilities { capabilities })
    }
}

fn require_supported_repository_capabilities(db: &Database) -> PristineResult<()> {
    ensure_supported_repository_capabilities(&collect_required_repository_capabilities(db)?)
}

fn require_path_claim_schema(db: &Database) -> PristineResult<()> {
    let read_txn = db.begin_read()?;
    let metadata = match read_txn.open_table(PRISTINE_META) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => {
            return Err(path_claim_schema_error(None));
        }
        Err(error) => return Err(error.into()),
    };
    let version_guard = metadata.get(PATH_CLAIM_SCHEMA_KEY)?;
    let version = version_guard.map(|version| version.value());
    if version != Some(PATH_CLAIM_SCHEMA_VERSION) {
        return Err(path_claim_schema_error(version));
    }
    match read_txn.open_multimap_table(PATH_CLAIMS) {
        Ok(_) => Ok(()),
        Err(redb::TableError::TableDoesNotExist(_)) => Err(path_claim_schema_error(version)),
        Err(error) => Err(error.into()),
    }
}

fn next_inode_id(read_txn: &ReadTransaction) -> PristineResult<u64> {
    let mut max_id = 0u64;

    for result in read_txn.open_table(INODES)?.iter()? {
        let (inode, _) = result?;
        max_id = max_id.max(inode.value());
    }
    for result in read_txn.open_table(REV_INODES)?.iter()? {
        let (_, inode) = result?;
        max_id = max_id.max(inode.value());
    }
    for result in read_txn.open_table(TREE)?.iter()? {
        let (_, inode) = result?;
        max_id = max_id.max(inode.value());
    }
    for result in read_txn.open_table(REV_TREE)?.iter()? {
        let (inode, _) = result?;
        max_id = max_id.max(inode.value());
    }
    for result in read_txn.open_table(DIRECTORIES)?.iter()? {
        let (inode, _) = result?;
        max_id = max_id.max(inode.value());
    }
    for result in read_txn.open_table(CONFLICTS)?.iter()? {
        let (key, _) = result?;
        let (_, inode) = decode_view_seq(key.value());
        max_id = max_id.max(inode);
    }
    for result in read_txn.open_multimap_table(INODE_GRAPH)?.iter()? {
        let (key, _values) = result?;
        let (inode, _, _, _) = decode_inode_vertex(key.value());
        max_id = max_id.max(inode);
    }

    Ok(max_id.saturating_add(1).max(1))
}

/// The pristine database handle
///
/// This is the main entry point for all database operations. It wraps a redb
/// database and provides methods for creating transactions.
///
/// # Example
///
/// ```ignore
/// let pristine = Pristine::open("path/to/pristine")?;
///
/// // Read-only access
/// let read_txn = pristine.read_txn()?;
/// let view = read_txn.get_view("main")?;
///
/// // Read-write access
/// let mut write_txn = pristine.write_txn()?;
/// let view = write_txn.open_or_create_view("main")?;
/// write_txn.commit()?;
/// ```
pub struct Pristine {
    db: Database,
    /// Counter for allocating node IDs
    pub(crate) next_node_id: AtomicU64,
    /// Counter for allocating view IDs
    pub(crate) next_view_id: AtomicU64,
    /// Counter for allocating inodes
    pub(crate) next_inode: AtomicU64,
}

impl Pristine {
    /// Open or create a pristine database at the given path
    ///
    /// This will create all necessary tables if they don't exist.
    pub fn open<P: AsRef<Path>>(path: P) -> PristineResult<Self> {
        let path = path.as_ref();
        let is_new_database = !path.exists();
        // Use 8 GB cache for machines with plenty of RAM.  The default
        // redb cache is 1 GB which causes excessive page eviction when
        // the GRAPH table grows beyond that during large imports.
        let cache_bytes = 8 * 1024 * 1024 * 1024; // 8 GiB
        let db = Builder::new().set_cache_size(cache_bytes).create(path)?;

        // Existing repositories must be checked before the additive table-init
        // transaction starts. Binaries predating this generic fence cannot honor
        // an additive marker; fence-aware binaries always fail closed here.
        if !is_new_database {
            require_supported_repository_capabilities(&db)?;
        }

        // Initialize all tables. Re-check in the write transaction so a
        // concurrently raised requirement cannot race additive initialization.
        let write_txn = db.begin_write()?;
        {
            let metadata = write_txn.open_table(PRISTINE_META)?;
            let mut requirements = Vec::new();
            for row in metadata.iter()? {
                let (key, version) = row?;
                if let Some(requirement) =
                    RequiredRepositoryCapability::from_metadata(key.value(), version.value())
                {
                    requirements.push(requirement);
                }
            }
            ensure_supported_repository_capabilities(&requirements)?;
            drop(metadata);

            // ID mapping tables
            write_txn.open_table(EXTERNAL)?;
            write_txn.open_table(INTERNAL)?;
            write_txn.open_table(NODE_TYPES)?;

            // Graph tables
            write_txn.open_multimap_table(GRAPH)?;
            write_txn.open_multimap_table(INODE_GRAPH)?;
            write_txn.open_multimap_table(POSITION_ATTRS)?;
            write_txn.open_multimap_table(INODE_ATTRS)?;

            // View tables
            write_txn.open_table(VIEWS)?;
            write_txn.open_table(WORKING_COPIES)?;
            write_txn.open_table(OPERATIONS)?;
            write_txn.open_table(OP_HEADS)?;
            write_txn.open_table(EFFECT_RECEIPTS)?;
            write_txn.open_table(VIEW_CHANGES)?;
            write_txn.open_table(REV_VIEW_CHANGES)?;
            write_txn.open_table(VIEW_SET_ID_INDEX)?;
            write_txn.open_table(CONFLICTS)?;

            // Tree tables
            write_txn.open_table(PRISTINE_META)?;
            write_txn.open_multimap_table(PATH_CLAIMS)?;
            write_txn.open_table(TREE)?;
            write_txn.open_table(REV_TREE)?;
            write_txn.open_table(INODES)?;
            write_txn.open_table(REV_INODES)?;
            write_txn.open_table(DIRECTORIES)?;
            if is_new_database {
                let mut metadata = write_txn.open_table(PRISTINE_META)?;
                metadata.insert(PATH_CLAIM_SCHEMA_KEY, PATH_CLAIM_SCHEMA_VERSION)?;
            }

            // File index cache (mtime + size + content hash for fast status detection)
            write_txn.open_table(FILE_INDEX)?;

            // Dependency tables
            write_txn.open_multimap_table(DEPS)?;
            write_txn.open_multimap_table(REV_DEPS)?;
            write_txn.open_multimap_table(CHANGE_DEPS)?;
            write_txn.open_multimap_table(REV_CHANGE_DEPS)?;
            write_txn.open_table(CHANGE_DEPS_INDEXED)?;

            // State tables
            write_txn.open_table(STATES)?;
            write_txn.open_table(MERKLE_CHAIN)?;

            // Tag record tables
            write_txn.open_table(TAG_RECORDS)?;
            write_txn.open_table(TAG_NAME_INDEX)?;

            // Git SHA index (git commit SHA → entity_id)
            write_txn.open_table(GIT_SHA_INDEX)?;

            // Persistent Git commit interpretation closures (CB-9B review R1)
            write_txn.open_table(GIT_COMMIT_CLOSURES)?;

            // Immutable captured bridge hook events (CB-9B review R4)
            write_txn.open_table(BRIDGE_EVENT_CAPTURES)?;

            // Immutable operation anchors for captured bridge hook events
            // (CB-9B review C2)
            write_txn.open_table(BRIDGE_EVENT_CAPTURE_ANCHORS)?;

            // Immutable capture-token bindings for prepared bridge Git-ref
            // operations (CB-9B review E2)
            write_txn.open_table(BRIDGE_REF_CAPTURE_TOKENS)?;

            // Immutable Git state bindings (RFC-ATOMIC-GIT-CAUSAL-BRIDGE §5.1)
            write_txn.open_table(BINDINGS)?;

            // Mutable view ↔ Git ref mappings (RFC §8.1, CB-10A)
            write_txn.open_table(REF_MAPPINGS)?;
            // Session tables (provenance-derived session data, any agent)
            write_txn.open_table(SESSION_EVENTS)?;
            write_txn.open_table(SESSION_TODOS)?;
            write_txn.open_table(SESSION_PHASES)?;
            write_txn.open_table(SESSION_INTENTS)?;
            write_txn.open_table(SESSIONS)?;
            write_txn.open_table(SESSION_TURNS)?;
            write_txn.open_table(SESSION_PROVENANCE)?;
            write_txn.open_table(SESSION_MANIFESTS)?;
            write_txn.open_table(SESSION_HEADS)?;
        }
        write_txn.commit()?;

        // Existing repositories intentionally remain unmarked until their
        // repository-level change files have been replayed into PATH_CLAIMS.
        // `open` returns a writable handle so that backfill can happen in one
        // transaction; read-only/open-existing modes reject the incomplete schema.
        Self::scan_ids(db, false)
    }

    /// Open an existing pristine database without the table-init write lock.
    ///
    /// Unlike [`open`](Self::open), this method skips the `begin_write()` +
    /// table-initialization transaction.  It assumes all tables already exist
    /// (true for any database previously created by `open` or `init`).
    ///
    /// The returned `Pristine` still supports [`write_txn`](Self::write_txn)
    /// — the write lock is deferred until you actually need one.  This is
    /// critical for the agent hook path where `Repository::open()` is called
    /// from a short-lived process: `begin_write()` blocks **indefinitely**
    /// if another process holds a write transaction, so skipping the
    /// init-only write eliminates the most common cause of hook hangs.
    ///
    /// # Errors
    ///
    /// Returns an error if the database file doesn't exist, is corrupted,
    /// or the ID-scan read transaction fails.
    pub fn open_existing<P: AsRef<Path>>(path: P) -> PristineResult<Self> {
        Self::open_existing_with_schema_check(path, true)
    }

    /// Open an existing database for repair without initializing tables or
    /// requiring a completed PATH_CLAIMS schema.
    ///
    /// Opening performs no writes. The returned handle permits a later explicit
    /// repair write transaction.
    pub fn open_existing_for_repair<P: AsRef<Path>>(path: P) -> PristineResult<Self> {
        Self::open_existing_with_schema_check(path, false)
    }

    /// Open an existing pristine database in read-only mode
    ///
    /// This method opens the database without acquiring a write lock, allowing
    /// concurrent read access from multiple processes. It's suitable for
    /// read-only operations like `status`, `diff`, `log`, and `change`.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The database file doesn't exist
    /// - The database is corrupted
    /// - Read access cannot be obtained
    ///
    /// # Example
    ///
    /// ```ignore
    /// let pristine = Pristine::open_readonly("path/to/pristine.redb")?;
    /// let txn = pristine.read_txn()?;
    /// // Read operations...
    /// ```
    pub fn open_readonly<P: AsRef<Path>>(path: P) -> PristineResult<Self> {
        Self::open_existing_with_schema_check(path, true)
    }

    /// Open an existing database for read-only repair inspection without
    /// initializing tables or requiring a completed PATH_CLAIMS schema.
    ///
    /// This method performs no writes.
    pub fn open_readonly_for_repair<P: AsRef<Path>>(path: P) -> PristineResult<Self> {
        Self::open_existing_with_schema_check(path, false)
    }

    fn open_existing_with_schema_check<P: AsRef<Path>>(
        path: P,
        require_complete_path_claims: bool,
    ) -> PristineResult<Self> {
        let cache_bytes = 8 * 1024 * 1024 * 1024; // 8 GiB
        let db = Builder::new().set_cache_size(cache_bytes).open(path)?;
        Self::scan_ids(db, require_complete_path_claims)
    }

    /// Scan existing tables for the next available IDs.
    ///
    /// Shared implementation for `open_existing` and `open_readonly` — both
    /// skip the table-init write transaction and only need a read pass to
    /// discover the max allocated node, view, and inode IDs.
    fn scan_ids(db: Database, require_complete_path_claims: bool) -> PristineResult<Self> {
        require_supported_repository_capabilities(&db)?;
        if require_complete_path_claims {
            require_path_claim_schema(&db)?;
        }
        let read_txn = db.begin_read()?;

        let next_node_id = {
            let table = read_txn.open_table(EXTERNAL)?;
            let mut max_id = 0u64;
            for result in table.iter()? {
                let (k, _) = result?;
                max_id = max_id.max(k.value());
            }
            AtomicU64::new(next_id(max_id)?)
        };

        let next_view_id = {
            let table = read_txn.open_table(VIEWS)?;
            let mut max_id = 0u64;
            for result in table.iter()? {
                let (_, value) = result?;
                let state = deserialize_view_state(value.value())?;
                max_id = max_id.max(state.id);
            }
            AtomicU64::new(next_id(max_id)?)
        };

        let next_inode = AtomicU64::new(next_inode_id(&read_txn)?);

        Ok(Self {
            db,
            next_node_id,
            next_view_id,
            next_inode,
        })
    }

    /// Return repository capability requirements in stable identifier order.
    pub fn required_repository_capabilities(
        &self,
    ) -> PristineResult<Vec<RequiredRepositoryCapability>> {
        collect_required_repository_capabilities(&self.db)
    }

    /// Reject requirements unsupported by this Atomic build.
    pub fn ensure_supported_repository_capabilities(&self) -> PristineResult<()> {
        require_supported_repository_capabilities(&self.db)
    }

    /// Durably declare or raise repository capability requirements.
    ///
    /// Existing requirements are revalidated in the same write transaction
    /// before any marker is changed. The immediate commit completes before this
    /// method returns, allowing callers to order object persistence after it.
    pub fn require_repository_capabilities(
        &self,
        capabilities: &[RepositoryCapability],
    ) -> PristineResult<()> {
        let requested = capabilities
            .iter()
            .map(|capability| RequiredRepositoryCapability {
                id: capability.id().to_string(),
                minimum_version: capability.minimum_version(),
            })
            .collect::<Vec<_>>();

        let mut write_txn = self.db.begin_write()?;
        write_txn.set_durability(redb::Durability::Immediate);
        let mut metadata = write_txn.open_table(PRISTINE_META)?;

        let mut existing_requirements = Vec::new();
        for row in metadata.iter()? {
            let (key, version) = row?;
            if let Some(requirement) =
                RequiredRepositoryCapability::from_metadata(key.value(), version.value())
            {
                existing_requirements.push(requirement);
            }
        }
        ensure_supported_repository_capabilities(&existing_requirements)?;
        ensure_supported_repository_capabilities(&requested)?;

        for capability in capabilities {
            let key = capability.metadata_key();
            let existing = metadata.get(key.as_str())?.map(|version| version.value());
            if existing.is_none_or(|version| version < capability.minimum_version()) {
                metadata.insert(key.as_str(), capability.minimum_version())?;
            }
        }
        drop(metadata);
        write_txn.commit()?;
        Ok(())
    }

    /// Durably declare or raise one repository capability requirement.
    pub fn require_repository_capability(
        &self,
        capability: RepositoryCapability,
    ) -> PristineResult<()> {
        self.require_repository_capabilities(&[capability])
    }

    /// Return the completed path-claim schema version, if any.
    pub fn path_claim_schema_version(&self) -> PristineResult<Option<u32>> {
        self.read_txn()?.path_claim_schema_version()
    }

    /// Whether repository-level path-claim backfill is still required.
    pub fn path_claim_migration_required(&self) -> PristineResult<bool> {
        match self.path_claim_schema_version()? {
            Some(PATH_CLAIM_SCHEMA_VERSION) => Ok(false),
            Some(version) if version > PATH_CLAIM_SCHEMA_VERSION => {
                Err(path_claim_schema_error(Some(version)))
            }
            _ => Ok(true),
        }
    }

    /// Begin a read-only transaction
    ///
    /// Read transactions can run concurrently with other read transactions
    /// and with a single write transaction.
    pub fn read_txn(&self) -> PristineResult<ReadTxn> {
        let txn = self.db.begin_read()?;
        Ok(ReadTxn::new(txn))
    }

    /// Begin a read-write transaction
    ///
    /// Only one write transaction can be active at a time. The transaction
    /// must be explicitly committed with `commit()` or it will be rolled back
    /// when dropped.
    pub fn write_txn(&self) -> PristineResult<WriteTxn<'_>> {
        self.write_txn_with_durability(redb::Durability::Eventual)
    }

    /// Begin a repair write transaction with immediate durability.
    ///
    /// Immediate durability fsyncs the commit before returning, which is
    /// appropriate for destructive derived-index replacement.
    pub fn write_txn_immediate(&self) -> PristineResult<WriteTxn<'_>> {
        self.write_txn_with_durability(redb::Durability::Immediate)
    }

    fn write_txn_with_durability(
        &self,
        durability: redb::Durability,
    ) -> PristineResult<WriteTxn<'_>> {
        let mut txn = self.db.begin_write()?;
        txn.set_durability(durability);
        Ok(WriteTxn::new(
            txn,
            &self.next_node_id,
            &self.next_view_id,
            &self.next_inode,
        ))
    }

    /// Allocate a new node ID
    ///
    /// This is thread-safe and guaranteed to return unique IDs.
    pub fn alloc_node_id(&self) -> u64 {
        self.next_node_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Get the current next node ID (for testing/debugging)
    pub fn peek_next_node_id(&self) -> u64 {
        self.next_node_id.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operation::OperationScope;
    use crate::pristine::{
        MutTxnT, NativeDerivedIndexes, NativeDerivedIndexesMutTxnT, OperationTxnT,
        PathClaimMutTxnT, PathClaimTxnT, TreeTxnT, WorkingCopyTxnT, CHANGE_FORMAT_VNEXT_CAPABILITY,
        PATH_CLAIM_SCHEMA_VERSION, REQUIRED_CAPABILITY_PREFIX,
    };
    use tempfile::tempdir;

    #[test]
    fn test_pristine_open() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        assert_eq!(
            pristine.path_claim_schema_version().unwrap(),
            Some(PATH_CLAIM_SCHEMA_VERSION)
        );
        assert!(!pristine.path_claim_migration_required().unwrap());

        // Should be able to create transactions
        let _read = pristine.read_txn().unwrap();
        let _write = pristine.write_txn().unwrap();
    }

    fn metadata_only_repository_with_requirement(db_path: &Path, capability: &str, version: u32) {
        let db = Database::create(db_path).unwrap();
        let write_txn = db.begin_write().unwrap();
        {
            let mut metadata = write_txn.open_table(PRISTINE_META).unwrap();
            let key = format!("{REQUIRED_CAPABILITY_PREFIX}{capability}");
            metadata.insert(key.as_str(), version).unwrap();
        }
        write_txn.commit().unwrap();
    }

    fn expect_unsupported(result: PristineResult<Pristine>) -> PristineError {
        match result {
            Ok(_) => panic!("open must reject unsupported repository capabilities"),
            Err(error @ PristineError::UnsupportedRequiredCapabilities { .. }) => error,
            Err(error) => panic!("expected unsupported capability error, got {error}"),
        }
    }

    #[test]
    fn unsupported_capability_rejects_all_open_modes_before_table_initialization() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        metadata_only_repository_with_requirement(&db_path, "future-format", 1);

        let error = expect_unsupported(Pristine::open(&db_path));
        assert!(error.to_string().contains("future-format"));
        assert!(error.to_string().contains("upgrade Atomic"));

        // An unsupported existing repository is rejected before additive table
        // initialization commits any mutation.
        let db = Database::open(&db_path).unwrap();
        let read_txn = db.begin_read().unwrap();
        assert!(matches!(
            read_txn.open_table(EXTERNAL),
            Err(redb::TableError::TableDoesNotExist(_))
        ));
        drop(read_txn);
        drop(db);

        expect_unsupported(Pristine::open_existing(&db_path));
        expect_unsupported(Pristine::open_readonly(&db_path));
        expect_unsupported(Pristine::open_existing_for_repair(&db_path));
        expect_unsupported(Pristine::open_readonly_for_repair(&db_path));
    }

    #[test]
    fn higher_known_capability_version_is_rejected() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        metadata_only_repository_with_requirement(
            &db_path,
            CHANGE_FORMAT_VNEXT_CAPABILITY.id(),
            CHANGE_FORMAT_VNEXT_CAPABILITY.minimum_version() + 1,
        );

        let error = expect_unsupported(Pristine::open_readonly(&db_path));
        let message = error.to_string();
        assert!(message.contains("change-format-vnext"));
        assert!(message.contains("version 2"));
        assert!(message.contains("supports through version 1"));
    }

    #[test]
    fn capability_declaration_is_durable_and_idempotent() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        pristine
            .require_repository_capability(CHANGE_FORMAT_VNEXT_CAPABILITY)
            .unwrap();
        pristine
            .require_repository_capability(CHANGE_FORMAT_VNEXT_CAPABILITY)
            .unwrap();
        assert_eq!(
            pristine.required_repository_capabilities().unwrap(),
            vec![RequiredRepositoryCapability {
                id: CHANGE_FORMAT_VNEXT_CAPABILITY.id().to_string(),
                minimum_version: CHANGE_FORMAT_VNEXT_CAPABILITY.minimum_version(),
            }]
        );
        drop(pristine);

        let reopened = Pristine::open_readonly(&db_path).unwrap();
        assert_eq!(
            reopened.required_repository_capabilities().unwrap(),
            vec![RequiredRepositoryCapability {
                id: "change-format-vnext".to_string(),
                minimum_version: 1,
            }]
        );
    }

    #[test]
    fn capability_declaration_raises_a_lower_requirement() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();
        let write_txn = pristine.db.begin_write().unwrap();
        {
            let mut metadata = write_txn.open_table(PRISTINE_META).unwrap();
            metadata
                .insert("required-capability/change-format-vnext", 0)
                .unwrap();
        }
        write_txn.commit().unwrap();

        pristine
            .require_repository_capability(CHANGE_FORMAT_VNEXT_CAPABILITY)
            .unwrap();
        assert_eq!(
            pristine.required_repository_capabilities().unwrap(),
            vec![RequiredRepositoryCapability {
                id: "change-format-vnext".to_string(),
                minimum_version: 1,
            }]
        );
    }

    #[test]
    fn capability_declaration_revalidates_existing_requirements() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();
        let write_txn = pristine.db.begin_write().unwrap();
        {
            let mut metadata = write_txn.open_table(PRISTINE_META).unwrap();
            metadata
                .insert("required-capability/future-format", 1)
                .unwrap();
        }
        write_txn.commit().unwrap();

        let error = pristine
            .ensure_supported_repository_capabilities()
            .expect_err("a pre-opened pristine handle must revalidate requirements");
        assert!(matches!(
            error,
            PristineError::UnsupportedRequiredCapabilities { .. }
        ));

        let error = match pristine.require_repository_capability(CHANGE_FORMAT_VNEXT_CAPABILITY) {
            Ok(()) => panic!("declaration must reject existing unknown requirements"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            PristineError::UnsupportedRequiredCapabilities { .. }
        ));
    }

    #[test]
    fn normal_open_adds_additive_storage_to_legacy_schema() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");

        {
            let db = Database::create(&db_path).unwrap();
            let write_txn = db.begin_write().unwrap();
            {
                write_txn.open_table(EXTERNAL).unwrap();
                write_txn.open_table(VIEWS).unwrap();
                write_txn.open_table(INODES).unwrap();
                write_txn.open_table(REV_INODES).unwrap();
                write_txn.open_table(TREE).unwrap();
                write_txn.open_table(REV_TREE).unwrap();
                write_txn.open_table(DIRECTORIES).unwrap();
                write_txn.open_table(CONFLICTS).unwrap();
                write_txn.open_multimap_table(INODE_GRAPH).unwrap();
                write_txn.open_multimap_table(PATH_CLAIMS).unwrap();
                let mut metadata = write_txn.open_table(PRISTINE_META).unwrap();
                metadata
                    .insert(PATH_CLAIM_SCHEMA_KEY, PATH_CLAIM_SCHEMA_VERSION)
                    .unwrap();
            }
            write_txn.commit().unwrap();
        }

        {
            let pristine = Pristine::open_readonly(&db_path).unwrap();
            let txn = pristine.read_txn().unwrap();
            assert!(matches!(
                txn.list_working_copies(),
                Err(PristineError::WorkingCopySchemaUnavailable)
            ));
            assert!(matches!(
                txn.get_operation_heads(OperationScope::Repository),
                Err(PristineError::OperationSchemaUnavailable)
            ));
        }

        {
            let pristine = Pristine::open(&db_path).unwrap();
            let txn = pristine.read_txn().unwrap();
            assert!(txn.list_working_copies().unwrap().is_empty());
            assert!(txn
                .get_operation_heads(OperationScope::Repository)
                .unwrap()
                .is_empty());
        }

        {
            let pristine = Pristine::open_readonly(&db_path).unwrap();
            let txn = pristine.read_txn().unwrap();
            assert!(txn.list_working_copies().unwrap().is_empty());
            assert!(txn
                .get_operation_heads(OperationScope::Repository)
                .unwrap()
                .is_empty());
        }
    }

    #[test]
    fn test_pristine_reopen() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");

        // Create and populate
        {
            let pristine = Pristine::open(&db_path).unwrap();
            assert_eq!(pristine.peek_next_node_id(), 1);

            // Allocate some IDs
            pristine.alloc_node_id();
            pristine.alloc_node_id();
            pristine.alloc_node_id();
        }

        // Reopen - counters won't persist unless we actually write data
        // This is expected - the counters are derived from the data
        {
            let pristine = Pristine::open(&db_path).unwrap();
            // Since we didn't actually write any changes, counter resets
            assert_eq!(pristine.peek_next_node_id(), 1);
        }
    }

    #[test]
    fn test_reopen_allocates_after_unbound_tree_inodes() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");

        {
            let pristine = Pristine::open(&db_path).unwrap();
            let mut txn = pristine.write_txn().unwrap();
            let staged = txn.alloc_inode().unwrap();
            assert_eq!(staged.get(), 1);
            txn.put_tree("staged.txt", staged).unwrap();
            assert_eq!(txn.inode_position(staged).unwrap(), None);
            txn.commit().unwrap();
        }

        let reopened = Pristine::open(&db_path).unwrap();
        let mut txn = reopened.write_txn().unwrap();
        assert_eq!(txn.alloc_inode().unwrap().get(), 2);
        txn.abort().unwrap();
    }

    #[test]
    fn incomplete_path_claim_schema_requires_writable_migration() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");

        {
            let pristine = Pristine::open(&db_path).unwrap();
            let mut txn = pristine.write_txn().unwrap();
            txn.reset_path_claim_migration().unwrap();
            txn.commit().unwrap();
        }

        let repair_readonly = Pristine::open_readonly_for_repair(&db_path).unwrap();
        assert_eq!(repair_readonly.path_claim_schema_version().unwrap(), None);
        drop(repair_readonly);

        let repair_existing = Pristine::open_existing_for_repair(&db_path).unwrap();
        assert_eq!(repair_existing.path_claim_schema_version().unwrap(), None);
        drop(repair_existing);

        let readonly_error = match Pristine::open_readonly(&db_path) {
            Ok(_) => panic!("read-only open must reject an incomplete claim schema"),
            Err(error) => error,
        };
        assert!(readonly_error
            .to_string()
            .contains("PATH_CLAIMS migration required"));

        let existing_error = match Pristine::open_existing(&db_path) {
            Ok(_) => panic!("open-existing must reject an incomplete claim schema"),
            Err(error) => error,
        };
        assert!(existing_error
            .to_string()
            .contains("PATH_CLAIMS migration required"));

        {
            let pristine = Pristine::open(&db_path).unwrap();
            assert!(pristine.path_claim_migration_required().unwrap());
            let mut txn = pristine.write_txn().unwrap();
            txn.complete_path_claim_migration().unwrap();
            txn.commit().unwrap();
        }

        let readonly = Pristine::open_readonly(&db_path).unwrap();
        assert!(readonly
            .read_txn()
            .unwrap()
            .path_claim_schema_is_complete()
            .unwrap());
    }

    #[test]
    fn poisoned_inode_maxima_open_repair_reset_and_allocate_without_wrapping() {
        for poisoned_inode in [u64::MAX - 1, u64::MAX] {
            let dir = tempdir().unwrap();
            let db_path = dir.path().join("pristine");
            {
                let pristine = Pristine::open(&db_path).unwrap();
                let txn = pristine.write_txn().unwrap();
                {
                    let mut tree = txn.txn.open_table(TREE).unwrap();
                    tree.insert("poisoned.txt", poisoned_inode).unwrap();
                }
                txn.commit().unwrap();
            }

            {
                let pristine = Pristine::open_existing(&db_path).unwrap();
                let mut txn = pristine.write_txn().unwrap();
                assert!(matches!(
                    txn.alloc_inode(),
                    Err(PristineError::IdSpaceExhausted)
                ));
                txn.abort().unwrap();
            }
            {
                let pristine = Pristine::open_readonly(&db_path).unwrap();
                assert_eq!(
                    pristine
                        .read_txn()
                        .unwrap()
                        .get_inode("poisoned.txt")
                        .unwrap(),
                    Some(crate::types::Inode::new(poisoned_inode))
                );
            }

            {
                let pristine = Pristine::open_existing_for_repair(&db_path).unwrap();
                let mut txn = pristine.write_txn_immediate().unwrap();
                txn.replace_native_derived_indexes(&NativeDerivedIndexes::default())
                    .unwrap();
                txn.commit().unwrap();

                let mut txn = pristine.write_txn().unwrap();
                let first = txn.alloc_inode().unwrap();
                let second = txn.alloc_inode().unwrap();
                assert_eq!(first.get(), 1);
                assert_eq!(second.get(), 2);
                assert!(!first.is_root());
                assert!(!second.is_root());
                txn.put_tree("first.txt", first).unwrap();
                txn.put_tree("second.txt", second).unwrap();
                txn.commit().unwrap();
            }

            let pristine = Pristine::open_existing(&db_path).unwrap();
            let mut txn = pristine.write_txn().unwrap();
            let third = txn.alloc_inode().unwrap();
            assert_eq!(third.get(), 3);
            assert!(!third.is_root());
            txn.abort().unwrap();
        }
    }

    #[test]
    fn test_alloc_node_id() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let id1 = pristine.alloc_node_id();
        let id2 = pristine.alloc_node_id();
        let id3 = pristine.alloc_node_id();

        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert_eq!(id3, 3);
    }

    #[test]
    fn test_concurrent_read_transactions() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        // Multiple read transactions should work concurrently
        let _read1 = pristine.read_txn().unwrap();
        let _read2 = pristine.read_txn().unwrap();
        let _read3 = pristine.read_txn().unwrap();
    }
}
