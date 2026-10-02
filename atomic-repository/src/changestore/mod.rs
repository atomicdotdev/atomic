//! Change storage for Atomic VCS
//!
//! This module provides the [`ChangeStore`] abstraction for persisting and retrieving
//! changes. Repository-owned stores read canonical objects from `atomic.redb`,
//! with filesystem fallback for legacy objects. Standalone stores use the
//! filesystem. Export files use a two-level content-addressed directory structure.
//!
//! # Directory Structure
//!
//! Changes are stored under `.atomic/changes/` with the following structure:
//!
//! ```text
//! .atomic/changes/
//! ├── AB/
//! │   └── CDEF1234567890...change    # Full base32 hash with .change extension
//! ├── XY/
//! │   └── Z789ABCDEF...change
//! └── ...
//! ```
//!
//! The first two characters of the base32-encoded hash form the subdirectory name.
//! This distributes changes across ~1024 possible directories (32² for base32).
//!
//! # Caching
//!
//! The store maintains an LRU cache of recently accessed changes to avoid
//! repeated disk I/O for frequently accessed changes (e.g., during merge
//! operations that need to traverse dependencies).
//!
//! # Atomic Writes
//!
//! Repository writes commit canonical objects in REDB before refreshing export
//! files. Recording stages object and graph writes in the same transaction.
//! Filesystem exports and standalone writes use a temporary file and rename,
//! so readers never observe a partially written export.
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_repository::ChangeStore;
//! use atomic_core::Change;
//!
//! // Create a store
//! let store = ChangeStore::new(changes_dir, 100)?;
//!
//! // Save a change
//! let hash = store.save_change(&change)?;
//!
//! // Load it back
//! let loaded = store.load_change(&hash)?;
//!
//! // Check existence
//! assert!(store.has_change(&hash));
//! ```
//!
//! # Thread Safety
//!
//! The [`ChangeStore`] uses interior mutability ([`std::sync::RwLock`]) for the cache,
//! making it safe for concurrent access. Multiple readers can access the cache
//! simultaneously, and writers get exclusive access when needed.

mod attestation;
mod iterators;
mod provenance;
mod trait_impl;

#[cfg(test)]
mod tests;

use atomic_core::pristine::tables::{CHANGE_BYTES, PROVENANCE_OBJECTS};
use atomic_core::pristine::{MutTxnT, Pristine};
use redb::ReadableTable;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use atomic_core::change::Change;
use atomic_core::types::{Base32, Hash};

// Re-export iterator types used by the public API
pub(crate) use iterators::ChangeIterator;

// Constants

/// Default LRU cache capacity for changes.
///
/// Default number of changes to cache in memory.
///
/// A typical change is 10-100KB. With 1024 entries, the cache uses
/// 10-100MB of memory — a reasonable trade-off for avoiding disk I/O
/// and lock contention during parallel materialization.
pub const DEFAULT_CACHE_CAPACITY: usize = 1024;

/// File extension for change files.
///
/// Using a distinct extension helps identify change files and prevents
/// conflicts with other file types.
pub const CHANGE_EXTENSION: &str = "change";

/// Number of characters from the hash used for the subdirectory name.
///
/// Using 2 characters gives us 32² = 1024 possible directories for base32,
/// which provides good distribution without excessive directory overhead.
pub(crate) const HASH_PREFIX_LEN: usize = 2;

// Error Types

/// Result type for change store operations.
mod error;
pub use error::{ChangeStoreError, ChangeStoreResult};

mod cache;
pub(crate) use cache::LruCache;

pub struct ChangeStore {
    pub(crate) database: Option<Arc<Pristine>>,
    /// Path to the changes directory (`.atomic/changes/`)
    pub(crate) changes_dir: PathBuf,

    /// LRU cache of recently accessed changes.
    ///
    /// We use `RwLock` for thread-safe interior mutability, allowing
    /// concurrent read access while ensuring exclusive write access.
    pub(crate) cache: RwLock<LruCache<Hash, CachedChange>>,
}

pub(crate) struct CachedChange {
    change: Change,
    // The complete V3 object includes mutable unhashed data and signatures.
    // Its fingerprint detects updates through other handles sharing this DB.
    canonical_fingerprint: Option<Hash>,
}

impl std::fmt::Debug for ChangeStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let cache_capacity = self.cache.read().map(|c| c.capacity).unwrap_or(0);
        f.debug_struct("ChangeStore")
            .field("changes_dir", &self.changes_dir)
            .field("cache_capacity", &cache_capacity)
            .finish()
    }
}

fn copy_content_from_change(
    hash: &Hash,
    change: &Change,
    start: usize,
    end: usize,
    buf: &mut [u8],
) -> ChangeStoreResult<usize> {
    if end > change.contents.len() {
        return Err(ChangeStoreError::ContentOutOfBounds {
            hash: hash.to_base32(),
            requested_start: start,
            requested_end: end,
            content_len: change.contents.len(),
        });
    }

    let len = end - start;
    if buf.len() < len {
        return Err(ChangeStoreError::ContentOutOfBounds {
            hash: hash.to_base32(),
            requested_start: start,
            requested_end: end,
            content_len: buf.len(),
        });
    }

    buf[..len].copy_from_slice(&change.contents[start..end]);
    Ok(len)
}

impl ChangeStore {
    /// Create a new change store with the given directory and cache capacity.
    ///
    /// The changes directory will be created if it doesn't exist.
    ///
    /// # Arguments
    ///
    /// * `changes_dir` - Path to the directory where changes will be stored
    /// * `cache_capacity` - Maximum number of changes to keep in memory
    ///
    /// # Errors
    ///
    /// Returns an error if the directory cannot be created.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let store = ChangeStore::new(".atomic/changes".into(), 100)?;
    /// ```
    pub fn new(changes_dir: PathBuf, cache_capacity: usize) -> ChangeStoreResult<Self> {
        // Ensure the directory exists
        fs::create_dir_all(&changes_dir)?;

        Ok(Self {
            changes_dir,
            database: None,
            cache: RwLock::new(LruCache::new(cache_capacity)),
        })
    }

    pub(crate) fn with_database(mut self, database: Arc<Pristine>) -> Self {
        self.database = Some(database);
        self
    }

    pub(crate) fn object_bytes(
        &self,
        table: redb::TableDefinition<&[u8; 32], &[u8]>,
        hash: &Hash,
    ) -> ChangeStoreResult<Option<Vec<u8>>> {
        let Some(db) = &self.database else {
            return Ok(None);
        };
        let read = db
            .read_txn()
            .map_err(|e| ChangeStoreError::Database(e.to_string()))?;
        let table = match read.redb_transaction().open_table(table) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(ChangeStoreError::Database(e.to_string())),
        };
        let bytes = table
            .get(hash.as_bytes())
            .map_err(|e| ChangeStoreError::Database(e.to_string()))?
            .map(|bytes| bytes.value().to_vec());
        Ok(bytes)
    }

    fn has_canonical_change(&self, hash: &Hash) -> ChangeStoreResult<bool> {
        let Some(db) = &self.database else {
            return Ok(false);
        };
        let read = db
            .read_txn()
            .map_err(|e| ChangeStoreError::Database(e.to_string()))?;
        let table = match read.redb_transaction().open_table(CHANGE_BYTES) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
            Err(error) => return Err(ChangeStoreError::Database(error.to_string())),
        };
        let exists = table
            .get(hash.as_bytes())
            .map_err(|e| ChangeStoreError::Database(e.to_string()))?
            .is_some();
        Ok(exists)
    }

    pub(crate) fn object_hashes(
        &self,
        table: redb::TableDefinition<&[u8; 32], &[u8]>,
    ) -> ChangeStoreResult<Vec<Hash>> {
        let Some(db) = &self.database else {
            return Ok(Vec::new());
        };
        let read = db
            .read_txn()
            .map_err(|e| ChangeStoreError::Database(e.to_string()))?;
        let table = match read.redb_transaction().open_table(table) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(ChangeStoreError::Database(e.to_string())),
        };
        let mut hashes = Vec::new();
        for row in table
            .iter()
            .map_err(|e| ChangeStoreError::Database(e.to_string()))?
        {
            let (key, _) = row.map_err(|e| ChangeStoreError::Database(e.to_string()))?;
            hashes.push(Hash::from_bytes(*key.value()));
        }
        Ok(hashes)
    }

    /// Persist a verified object independently of graph publication.
    /// Record uses the same prepared object inside its graph transaction instead.
    pub(crate) fn save_change_bytes(&self, hash: &Hash, bytes: &[u8]) -> ChangeStoreResult<Hash> {
        let prepared = crate::redb_change_store::PreparedChange::from_v3_bytes(bytes)
            .map_err(|e| ChangeStoreError::Database(e.to_string()))?;
        if prepared.hash() != *hash.as_bytes() {
            return Err(ChangeStoreError::HashMismatch {
                expected: hash.to_base32(),
                computed: Hash::from_bytes(prepared.hash()).to_base32(),
            });
        }
        if let Some(db) = &self.database {
            let txn = db
                .write_txn()
                .map_err(|e| ChangeStoreError::Database(e.to_string()))?;
            prepared
                .write(txn.redb_transaction())
                .map_err(|e| ChangeStoreError::Database(e.to_string()))?;
            txn.commit()
                .map_err(|e| ChangeStoreError::Database(e.to_string()))?;
            if let Err(error) = self.cache_change_bytes(hash, bytes) {
                log::warn!(
                    "committed change {}; export cache deferred: {error}",
                    hash.to_base32()
                );
            }
        } else {
            self.cache_change_bytes(hash, bytes)?;
        }
        if let Ok(mut cache) = self.cache.write() {
            cache.remove(hash);
        }
        Ok(*hash)
    }

    pub(crate) fn cache_change_bytes(&self, hash: &Hash, bytes: &[u8]) -> ChangeStoreResult<()> {
        let path = self.change_path(hash);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temp = tempfile::NamedTempFile::new_in(&self.changes_dir)?;
        std::io::Write::write_all(&mut temp.as_file(), bytes)?;
        temp.persist(path)?;
        Ok(())
    }

    /// Read the exact verified V3 bytes, preserving their original encoding.
    ///
    /// Repository objects take precedence over filesystem export copies. A
    /// legacy object with no database row is read from its content-addressed
    /// file. Corrupt database bytes never fall back to an older export.
    pub fn load_change_bytes(&self, hash: &Hash) -> ChangeStoreResult<Vec<u8>> {
        let bytes = match self.object_bytes(CHANGE_BYTES, hash)? {
            Some(bytes) => bytes,
            None => match fs::read(self.change_path(hash)) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(ChangeStoreError::NotFound {
                        hash: hash.to_base32(),
                    });
                }
                Err(error) => return Err(error.into()),
            },
        };
        let (_, actual) = Change::deserialize(&mut bytes.as_slice())?;
        if actual != *hash {
            return Err(ChangeStoreError::HashMismatch {
                expected: hash.to_base32(),
                computed: actual.to_base32(),
            });
        }
        Ok(bytes)
    }

    /// Create a change store from a repository root directory.
    ///
    /// This is a convenience method that constructs the changes directory
    /// path from the repository root (containing `.atomic/`).
    ///
    /// # Arguments
    ///
    /// * `root` - Path to the repository root
    /// * `cache_capacity` - Maximum number of changes to keep in memory
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let store = ChangeStore::from_root("/path/to/repo", 100)?;
    /// // Equivalent to: ChangeStore::new("/path/to/repo/.atomic/changes", 100)
    /// ```
    pub fn from_root<P: AsRef<Path>>(root: P, cache_capacity: usize) -> ChangeStoreResult<Self> {
        let changes_dir = root.as_ref().join(atomic_core::DOT_DIR).join("changes");
        Self::new(changes_dir, cache_capacity)
    }

    /// Get the path to the changes directory.
    pub fn changes_dir(&self) -> &Path {
        &self.changes_dir
    }

    /// Compute the filesystem path for a change with the given hash.
    ///
    /// The path follows the two-level directory structure:
    /// `{changes_dir}/{prefix}/{full_hash}.change`
    ///
    /// # Arguments
    ///
    /// * `hash` - The hash of the change
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let hash = Hash::of(b"test");
    /// let path = store.change_path(&hash);
    /// // e.g., ".atomic/changes/AB/ABCDEF1234567890....change"
    /// ```
    pub fn change_path(&self, hash: &Hash) -> PathBuf {
        let hash_str = hash.to_base32();
        let (prefix, _) = hash_str.split_at(HASH_PREFIX_LEN.min(hash_str.len()));
        self.changes_dir
            .join(prefix)
            .join(format!("{}.{}", hash_str, CHANGE_EXTENSION))
    }

    /// Check if a change with the given hash exists on disk.
    ///
    /// This checks both the cache and the filesystem. Note that this
    /// doesn't verify the integrity of the change file.
    ///
    /// # Arguments
    ///
    /// * `hash` - The hash of the change to check
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// if store.has_change(&hash) {
    ///     let change = store.load_change(&hash)?;
    /// }
    /// ```
    pub fn has_change(&self, hash: &Hash) -> bool {
        if matches!(self.has_canonical_change(hash), Ok(true)) {
            return true;
        }
        // Check cache first
        if self
            .cache
            .read()
            .map(|c| {
                c.peek(hash)
                    .is_some_and(|entry| entry.canonical_fingerprint.is_none())
            })
            .unwrap_or(false)
        {
            return true;
        }

        // Check filesystem
        self.change_path(hash).exists()
    }

    /// Save a change to disk.
    ///
    /// The change is serialized, written to a temporary file, and then
    /// atomically renamed to its final path. The change is also added
    /// to the cache.
    ///
    /// # Arguments
    ///
    /// * `change` - The change to save
    ///
    /// # Returns
    ///
    /// The hash of the saved change.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The directory cannot be created
    /// - The file cannot be written
    /// - Serialization fails
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let change = create_change(...);
    /// let hash = store.save_change(&change)?;
    /// println!("Saved change: {}", hash.to_base32());
    /// ```
    pub fn save_change(&self, change: &Change) -> ChangeStoreResult<Hash> {
        if self.database.is_some() {
            let mut bytes = Vec::new();
            let hash = change.serialize(&mut bytes)?;
            return self.save_change_bytes(&hash, &bytes);
        }
        // Create a temporary file in the changes directory
        let temp_file = tempfile::NamedTempFile::new_in(&self.changes_dir)?;

        // Serialize the change and get its hash
        let hash = {
            let mut writer = BufWriter::new(&temp_file);
            change.serialize(&mut writer)?
        };

        // Ensure the target directory exists
        let target_path = self.change_path(&hash);
        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Atomically move to the final location
        temp_file.persist(&target_path)?;

        // Add to cache
        if let Ok(mut cache) = self.cache.write() {
            cache.insert(
                hash,
                CachedChange {
                    change: change.clone(),
                    canonical_fingerprint: None,
                },
            );
        }

        log::debug!(
            "Saved change {} to {}",
            hash.to_base32(),
            target_path.display()
        );

        Ok(hash)
    }

    /// Load a canonical or legacy change.
    ///
    /// Canonical bytes are fingerprinted before reusing a decoded cache entry,
    /// so optional data changed through another handle is observed. Legacy
    /// files use the filesystem cache until the object is saved canonically.
    ///
    /// # Arguments
    ///
    /// * `hash` - The hash of the change to load
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The change doesn't exist (`NotFound`)
    /// - The file is corrupted (`HashMismatch`)
    /// - Deserialization fails (`Serialization`)
    /// - An I/O error occurs (`Io`)
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let change = store.load_change(&hash)?;
    /// println!("Message: {}", change.hashed.header.message);
    /// ```
    pub fn load_change(&self, hash: &Hash) -> ChangeStoreResult<Change> {
        if let Some(bytes) = self.object_bytes(CHANGE_BYTES, hash)? {
            let fingerprint = Hash::of(&bytes);
            if let Ok(mut cache) = self.cache.write() {
                if let Some(entry) = cache.get(hash) {
                    if entry.canonical_fingerprint == Some(fingerprint) {
                        return Ok(entry.change.clone());
                    }
                }
            }
            let (change, actual) = Change::deserialize(&mut bytes.as_slice())?;
            if actual != *hash {
                return Err(ChangeStoreError::HashMismatch {
                    expected: hash.to_base32(),
                    computed: actual.to_base32(),
                });
            }
            if let Ok(mut cache) = self.cache.write() {
                cache.insert(
                    *hash,
                    CachedChange {
                        change: change.clone(),
                        canonical_fingerprint: Some(fingerprint),
                    },
                );
            }
            return Ok(change);
        }
        // Check cache first
        {
            if let Ok(mut cache) = self.cache.write() {
                if let Some(entry) = cache
                    .get(hash)
                    .filter(|entry| entry.canonical_fingerprint.is_none())
                {
                    log::trace!("Cache hit for change {}", hash.to_base32());
                    return Ok(entry.change.clone());
                }
            }
        }

        // Load from disk
        let path = self.change_path(hash);
        log::debug!(
            "Loading change {} from {}",
            hash.to_base32(),
            path.display()
        );

        if !path.exists() {
            return Err(ChangeStoreError::NotFound {
                hash: hash.to_base32(),
            });
        }

        let file = File::open(&path)?;
        let mut reader = BufReader::new(file);

        let (change, computed_hash) = Change::deserialize(&mut reader)?;

        // Verify the hash
        if computed_hash != *hash {
            return Err(ChangeStoreError::HashMismatch {
                expected: hash.to_base32(),
                computed: computed_hash.to_base32(),
            });
        }

        // Add to cache
        if let Ok(mut cache) = self.cache.write() {
            cache.insert(
                *hash,
                CachedChange {
                    change: change.clone(),
                    canonical_fingerprint: None,
                },
            );
        }

        Ok(change)
    }

    /// Copy a content span from a change without cloning the full `Change`.
    ///
    /// Graph output calls this for every vertex it materializes. Using
    /// `load_change()` here is expensive for imported changes because cache
    /// hits clone the entire change, including potentially large unhashed Git
    /// metadata. This path copies only the requested bytes.
    pub(crate) fn copy_content_span(
        &self,
        hash: &Hash,
        start: usize,
        end: usize,
        buf: &mut [u8],
    ) -> ChangeStoreResult<usize> {
        // Fast path: shared read lock — multiple threads can read concurrently.
        // peek() doesn't update LRU order, which is an acceptable trade-off
        // to avoid serializing all readers on a write lock.
        if let Ok(cache) = self.cache.read() {
            if let Some(entry) = cache.peek(hash) {
                return copy_content_from_change(hash, &entry.change, start, end, buf);
            }
        }

        let change = self.load_change(hash)?;

        copy_content_from_change(hash, &change, start, end, buf)
    }

    /// Delete a change from disk and the cache.
    ///
    /// # Arguments
    ///
    /// * `hash` - The hash of the change to delete
    ///
    /// # Returns
    ///
    /// `true` if the change was deleted, `false` if it didn't exist.
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists but cannot be deleted.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// if store.delete_change(&hash)? {
    ///     println!("Change deleted");
    /// } else {
    ///     println!("Change didn't exist");
    /// }
    /// ```
    pub fn delete_change(&self, hash: &Hash) -> ChangeStoreResult<bool> {
        let mut canonical_store = None;
        if let Some(db) = &self.database {
            if self.object_bytes(CHANGE_BYTES, hash)?.is_some() {
                let handle = db
                    .shared_database()
                    .ok_or_else(|| ChangeStoreError::Database("read-only repository".into()))?;
                let store = crate::redb_change_store::RedbChangeStore::from_database(handle)
                    .map_err(|e| ChangeStoreError::Database(e.to_string()))?;
                canonical_store = Some(store);
            }
        }

        // Remove the legacy fallback before deleting the canonical object.
        // A failed export removal must not leave a deleted object visible;
        // a later database failure still leaves the canonical copy readable.
        let path = self.change_path(hash);
        let removed_export = match fs::remove_file(&path) {
            Ok(()) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        let removed_canonical = match canonical_store {
            Some(store) => store
                .delete_change(hash.as_bytes())
                .map_err(|e| ChangeStoreError::Database(e.to_string()))?,
            None => false,
        };

        if let Ok(mut cache) = self.cache.write() {
            cache.remove(hash);
        }

        // Try to remove the parent directory if it's empty
        // This is best-effort; we don't care if it fails
        if let Some(parent) = path.parent() {
            let _ = fs::remove_dir(parent);
        }

        log::debug!(
            "Deleted change {} from {}",
            hash.to_base32(),
            path.display()
        );

        Ok(removed_canonical || removed_export)
    }

    /// Iterate over all change hashes stored on disk.
    ///
    /// This scans the changes directory and yields the hash of each
    /// change file found. The iteration order is not guaranteed.
    ///
    /// # Performance
    ///
    /// This method reads the filesystem and should be used sparingly
    /// on repositories with many changes.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// for result in store.iter_changes() {
    ///     match result {
    ///         Ok(hash) => println!("Found change: {}", hash.to_base32()),
    ///         Err(e) => eprintln!("Error reading change: {}", e),
    ///     }
    /// }
    /// ```
    pub fn iter_changes(&self) -> impl Iterator<Item = ChangeStoreResult<Hash>> + '_ {
        let mut seen = std::collections::HashSet::new();
        let stored = match self.object_hashes(CHANGE_BYTES) {
            Ok(hashes) => hashes.into_iter().map(Ok).collect::<Vec<_>>(),
            Err(error) => vec![Err(error)],
        };
        stored
            .into_iter()
            .chain(ChangeIterator::new(&self.changes_dir))
            .filter(move |entry| entry.as_ref().map_or(true, |hash| seen.insert(*hash)))
    }

    /// Count the number of changes stored on disk.
    ///
    /// This scans the entire changes directory and counts valid change files.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let count = store.count_changes()?;
    /// println!("Repository has {} changes", count);
    /// ```
    pub fn count_changes(&self) -> ChangeStoreResult<usize> {
        let mut count = 0;
        for result in self.iter_changes() {
            result?;
            count += 1;
        }
        Ok(count)
    }

    /// Clear the in-memory cache.
    ///
    /// This is useful for testing or when memory pressure is high.
    /// It doesn't affect the on-disk storage.
    #[cfg(test)]
    pub fn clear_cache(&self) {
        if let Ok(mut cache) = self.cache.write() {
            cache.clear();
        }
    }

    /// Get the number of entries currently in the cache.
    #[cfg(test)]
    pub fn cache_size(&self) -> usize {
        self.cache.read().map(|c| c.len()).unwrap_or(0)
    }
}
