//! Merging legacy repository databases into one redb file.
//!
//! Repositories used to keep graph state in `pristine.redb` and the provenance
//! journal in `changes.redb`. [`LegacyDatabases::merge_into`] copies every
//! registered table from those files into a fresh database through its typed
//! definition, then re-reads the result and compares each table's row count
//! and digest with its source. The new file is only marked current once it
//! matches, so a failed merge leaves the legacy files authoritative.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use redb::{
    Builder, Database, Durability, Key, MultimapTableDefinition, MultimapTableHandle,
    ReadTransaction, ReadableDatabase, ReadableMultimapTable, ReadableTable, ReadableTableMetadata,
    TableDefinition, TableError, TableHandle, Value, WriteTransaction,
};

use super::error::{PristineError, PristineResult};
use super::schema::{
    check_schema_version, table_names, visit_tables, TableVisitor, LEGACY_DIR_KEY, RETIRED_TABLES,
    SCHEMA_VERSION, SCHEMA_VERSION_KEY,
};
use super::tables::ATOMIC_META;
use super::txn::open_database;

/// Cache for the handles a merge opens. Merges stream each table once, so a
/// large cache buys nothing and would only inflate the migrating process.
const MERGE_CACHE_BYTES: usize = 256 * 1024 * 1024;

/// Row count and content digest of one table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TableDigest {
    pub rows: u64,
    pub hash: [u8; 32],
}

/// What a merge copied, keyed by table name, and what it left behind.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MergeReport {
    pub tables: BTreeMap<String, TableDigest>,
    /// Retired tables that held rows; they stay in the legacy files.
    pub left_behind: Vec<String>,
}

impl MergeReport {
    /// Total rows copied across all tables.
    pub fn rows(&self) -> u64 {
        self.tables.values().map(|digest| digest.rows).sum()
    }
}

/// Exclusive handles on the legacy database files being merged.
///
/// redb holds an exclusive file lock for each handle, so while this value is
/// alive no other process can open or write the legacy files. Keep it until
/// the merged database is published and the legacy files are moved away.
pub struct LegacyDatabases {
    sources: Vec<(PathBuf, Database)>,
}

impl LegacyDatabases {
    pub fn new() -> Self {
        Self {
            sources: Vec::new(),
        }
    }

    /// Paths of the legacy files held, in the order they were added.
    pub fn paths(&self) -> impl Iterator<Item = &Path> {
        self.sources.iter().map(|(path, _)| path.as_path())
    }

    /// Open one legacy file exclusively, upgrading a redb 2.x file in place.
    ///
    /// # Errors
    ///
    /// Fails with `DatabaseAlreadyOpen` when another handle holds the file,
    /// and with an I/O `NotFound` when the file no longer exists.
    pub fn add(&mut self, path: &Path) -> PristineResult<()> {
        let database = open_database(path, false, MERGE_CACHE_BYTES)?;
        check_schema_version(&database.begin_read()?)?;
        self.sources.push((path.to_path_buf(), database));
        Ok(())
    }

    /// Write the merged database to `target` and verify it.
    ///
    /// `target` must not exist. `legacy_dir` is recorded in `atomic_meta` so
    /// an interrupted retirement of the legacy files can be finished later.
    ///
    /// # Errors
    ///
    /// Refuses to merge when a source holds a non-empty table this build does
    /// not know, when two sources define the same table, or when the merged
    /// copy does not match its sources.
    pub fn merge_into(&self, target: &Path, legacy_dir: &str) -> PristineResult<MergeReport> {
        if target.exists() {
            return Err(merge_error(format!(
                "merge target {} already exists",
                target.display()
            )));
        }
        let known: BTreeSet<_> = table_names().into_iter().collect();
        let mut left_behind = Vec::new();
        for (path, source) in &self.sources {
            left_behind.extend(refuse_unknown_tables(&source.begin_read()?, &known, path)?);
        }

        let mut builder = Builder::new();
        builder.set_cache_size(MERGE_CACHE_BYTES);
        let merged = builder.create(target)?;
        let mut report = MergeReport {
            left_behind,
            ..MergeReport::default()
        };
        let mut source_digests = Vec::new();
        for (path, source) in &self.sources {
            let read = source.begin_read()?;
            let mut write = merged.begin_write()?;
            // Only the final metadata commit needs to be durable: until it
            // lands the target is an unpublished scratch file.
            write
                .set_durability(Durability::None)
                .map_err(|error| merge_error(error.to_string()))?;
            let before: BTreeSet<_> = report.tables.keys().cloned().collect();
            visit_tables(&mut Copier {
                source: &read,
                target: &write,
                report: &mut report,
            })?;
            write.commit()?;
            let copied = report
                .tables
                .iter()
                .filter(|(name, _)| !before.contains(*name));
            source_digests.push((source_key(path)?, fingerprint(copied)));
        }

        if collect_digests(&merged.begin_read()?)? != report.tables {
            return Err(merge_error(
                "merged tables do not match their sources".into(),
            ));
        }

        let write = merged.begin_write()?;
        {
            let mut meta = write.open_table(ATOMIC_META)?;
            meta.insert(SCHEMA_VERSION_KEY, SCHEMA_VERSION.to_le_bytes().as_slice())?;
            meta.insert(LEGACY_DIR_KEY, legacy_dir.as_bytes())?;
            for (key, digest) in source_digests {
                meta.insert(key.as_str(), digest.as_slice())?;
            }
        }
        write.commit()?;
        Ok(report)
    }

    /// Verify surviving legacy sources against the snapshot used for the merge.
    ///
    /// A crash releases source locks before retirement. An older executable
    /// can then acknowledge new writes to those files; never silently archive
    /// them as if they were still the snapshot we copied.
    pub fn verify_unchanged(&self, merged: &Path) -> PristineResult<()> {
        let db = Builder::new().open_read_only(merged)?;
        let read = db.begin_read()?;
        check_schema_version(&read)?;
        let meta = read.open_table(ATOMIC_META)?;
        for (path, source) in &self.sources {
            let key = source_key(path)?;
            let expected = meta.get(key.as_str())?.ok_or_else(|| {
                merge_error(format!(
                    "no source digest for {}; cannot safely resume retirement",
                    path.display()
                ))
            })?;
            let snapshot = source.begin_read()?;
            refuse_unknown_tables(&snapshot, &table_names().into_iter().collect(), path)?;
            let actual = fingerprint(collect_digests(&snapshot)?.iter());
            if expected.value() != actual.as_slice() {
                return Err(merge_error(format!(
                    "{} changed after migration; retained both databases for recovery; do not discard either file", path.display()
                )));
            }
        }
        Ok(())
    }
}

impl Default for LegacyDatabases {
    fn default() -> Self {
        Self::new()
    }
}

/// The legacy directory recorded by [`LegacyDatabases::merge_into`], or
/// `None` for a database that was created directly.
pub fn merged_legacy_dir(database: &Path) -> PristineResult<Option<String>> {
    let database = Builder::new().open_read_only(database)?;
    let txn = database.begin_read()?;
    let meta = match txn.open_table(ATOMIC_META) {
        Ok(meta) => meta,
        Err(TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let Some(stored) = meta.get(LEGACY_DIR_KEY)? else {
        return Ok(None);
    };
    String::from_utf8(stored.value().to_vec())
        .map(Some)
        .map_err(|_| merge_error("legacy directory in atomic_meta is not UTF-8".to_string()))
}

fn merge_error(message: String) -> PristineError {
    PristineError::Merge { message }
}

/// Unknown tables cannot be copied without their types. Dropping an empty one
/// loses nothing, and retired tables are no longer read; any other table must
/// stop the merge. Returns the non-empty retired tables left behind.
fn refuse_unknown_tables(
    txn: &ReadTransaction,
    known: &BTreeSet<String>,
    source: &Path,
) -> PristineResult<Vec<String>> {
    let mut non_empty = Vec::new();
    for handle in txn.list_tables()? {
        let name = handle.name().to_string();
        if !known.contains(name.as_str()) && txn.open_untyped_table(handle)?.len()? > 0 {
            non_empty.push(name);
        }
    }
    for handle in txn.list_multimap_tables()? {
        let name = handle.name().to_string();
        if !known.contains(name.as_str()) && txn.open_untyped_multimap_table(handle)?.len()? > 0 {
            non_empty.push(name);
        }
    }
    let (left_behind, unknown): (Vec<_>, Vec<_>) = non_empty
        .into_iter()
        .partition(|name| RETIRED_TABLES.contains(&name.as_str()));
    if unknown.is_empty() {
        return Ok(left_behind);
    }
    Err(merge_error(format!(
        "{} holds tables this atomic does not know ({}); upgrade atomic before opening this repository",
        source.display(),
        unknown.join(", ")
    )))
}

#[derive(Default)]
struct DigestBuilder {
    rows: u64,
    hasher: blake3::Hasher,
}

impl DigestBuilder {
    fn add(&mut self, key: &[u8], value: &[u8]) {
        self.rows += 1;
        self.hasher.update(&(key.len() as u64).to_le_bytes());
        self.hasher.update(key);
        self.hasher.update(&(value.len() as u64).to_le_bytes());
        self.hasher.update(value);
    }

    fn finish(self) -> TableDigest {
        TableDigest {
            rows: self.rows,
            hash: *self.hasher.finalize().as_bytes(),
        }
    }
}

struct Copier<'a> {
    source: &'a ReadTransaction,
    target: &'a WriteTransaction,
    report: &'a mut MergeReport,
}

impl Copier<'_> {
    fn record(&mut self, name: &str, digest: DigestBuilder) -> PristineResult<()> {
        if self.report.tables.contains_key(name) {
            return Err(merge_error(format!(
                "table {name} exists in more than one legacy database"
            )));
        }
        self.report.tables.insert(name.to_string(), digest.finish());
        Ok(())
    }
}

impl TableVisitor for Copier<'_> {
    fn table<K: Key + 'static, V: Value + 'static>(
        &mut self,
        definition: TableDefinition<'static, K, V>,
    ) -> PristineResult<()> {
        let source = match self.source.open_table(definition) {
            Ok(table) => table,
            Err(TableError::TableDoesNotExist(_)) => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let mut target = self.target.open_table(definition)?;
        let mut digest = DigestBuilder::default();
        for entry in source.iter()? {
            let (key, value) = entry?;
            digest.add(
                K::as_bytes(&key.value()).as_ref(),
                V::as_bytes(&value.value()).as_ref(),
            );
            target.insert(key.value(), value.value())?;
        }
        self.record(definition.name(), digest)
    }

    fn multimap<K: Key + 'static, V: Key + 'static>(
        &mut self,
        definition: MultimapTableDefinition<'static, K, V>,
    ) -> PristineResult<()> {
        let source = match self.source.open_multimap_table(definition) {
            Ok(table) => table,
            Err(TableError::TableDoesNotExist(_)) => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let mut target = self.target.open_multimap_table(definition)?;
        let mut digest = DigestBuilder::default();
        for entry in source.iter()? {
            let (key, values) = entry?;
            for value in values {
                let value = value?;
                digest.add(
                    K::as_bytes(&key.value()).as_ref(),
                    V::as_bytes(&value.value()).as_ref(),
                );
                target.insert(key.value(), value.value())?;
            }
        }
        self.record(definition.name(), digest)
    }
}

fn source_key(path: &Path) -> PristineResult<String> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| merge_error("legacy source must have a UTF-8 filename".into()))?;
    Ok(format!("legacy_source/{name}"))
}

fn fingerprint<'a>(tables: impl Iterator<Item = (&'a String, &'a TableDigest)>) -> [u8; 32] {
    let mut digest = blake3::Hasher::new();
    for (name, table) in tables {
        digest.update(&(name.len() as u64).to_le_bytes());
        digest.update(name.as_bytes());
        digest.update(&table.rows.to_le_bytes());
        digest.update(&table.hash);
    }
    *digest.finalize().as_bytes()
}

fn collect_digests(txn: &ReadTransaction) -> PristineResult<BTreeMap<String, TableDigest>> {
    let mut collector = DigestCollector {
        merged: txn,
        tables: BTreeMap::new(),
    };
    visit_tables(&mut collector)?;
    Ok(collector.tables)
}

struct DigestCollector<'a> {
    merged: &'a ReadTransaction,
    tables: BTreeMap<String, TableDigest>,
}

impl DigestCollector<'_> {
    fn collect(&mut self, name: &str, digest: Option<DigestBuilder>) -> PristineResult<()> {
        if let Some(digest) = digest {
            self.tables.insert(name.to_string(), digest.finish());
        }
        Ok(())
    }
}

impl TableVisitor for DigestCollector<'_> {
    fn table<K: Key + 'static, V: Value + 'static>(
        &mut self,
        definition: TableDefinition<'static, K, V>,
    ) -> PristineResult<()> {
        let table = match self.merged.open_table(definition) {
            Ok(table) => table,
            Err(TableError::TableDoesNotExist(_)) => return self.collect(definition.name(), None),
            Err(error) => return Err(error.into()),
        };
        let mut digest = DigestBuilder::default();
        for entry in table.iter()? {
            let (key, value) = entry?;
            digest.add(
                K::as_bytes(&key.value()).as_ref(),
                V::as_bytes(&value.value()).as_ref(),
            );
        }
        self.collect(definition.name(), Some(digest))
    }

    fn multimap<K: Key + 'static, V: Key + 'static>(
        &mut self,
        definition: MultimapTableDefinition<'static, K, V>,
    ) -> PristineResult<()> {
        let table = match self.merged.open_multimap_table(definition) {
            Ok(table) => table,
            Err(TableError::TableDoesNotExist(_)) => return self.collect(definition.name(), None),
            Err(error) => return Err(error.into()),
        };
        let mut digest = DigestBuilder::default();
        for entry in table.iter()? {
            let (key, values) = entry?;
            for value in values {
                let value = value?;
                digest.add(
                    K::as_bytes(&key.value()).as_ref(),
                    V::as_bytes(&value.value()).as_ref(),
                );
            }
        }
        self.collect(definition.name(), Some(digest))
    }
}
