//! Change storage for Atomic VCS
//!
//! This module provides the [`ChangeStore`] abstraction for persisting and retrieving
//! changes from the filesystem. Changes are stored in a two-level directory structure
//! based on their content hash, enabling efficient lookup and avoiding filesystem
//! limitations with too many files in a single directory.
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
//! All write operations use the atomic write pattern:
//! 1. Write to a temporary file in the same directory
//! 2. Rename to the final path
//!
//! This ensures that readers never see partial writes, even in case of
//! crashes or power failures.
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
//! The [`ChangeStore`] uses interior mutability ([`RefCell`]) for the cache,
//! making it `!Sync`. For concurrent access, wrap it in a `Mutex` or use
//! separate instances per thread.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use atomic_core::change::ChangeStore as ChangeStoreTrait;
use atomic_core::change::{Change, ChangeError, ChangeHeader};
use atomic_core::types::{Base32, GraphNode, Hash, NodeId};
use thiserror::Error;

// Constants

/// Default LRU cache capacity for changes.
///
/// This value balances memory usage against cache hit rate. A typical change
/// might be 10-100KB, so 100 changes ≈ 1-10MB of memory.
pub const DEFAULT_CACHE_CAPACITY: usize = 100;

/// File extension for change files.
///
/// Using a distinct extension helps identify change files and prevents
/// conflicts with other file types.
pub const CHANGE_EXTENSION: &str = "change";

/// Number of characters from the hash used for the subdirectory name.
///
/// Using 2 characters gives us 32² = 1024 possible directories for base32,
/// which provides good distribution without excessive directory overhead.
const HASH_PREFIX_LEN: usize = 2;

// Error Types

/// Result type for change store operations.
pub type ChangeStoreResult<T> = Result<T, ChangeStoreError>;

/// Errors that can occur during change store operations.
///
/// These errors cover all failure modes for storing and retrieving changes,
/// including I/O errors, serialization failures, and integrity violations.
#[derive(Debug, Error)]
pub enum ChangeStoreError {
    /// The requested change was not found on disk.
    ///
    /// This can occur when:
    /// - The change was never saved
    /// - The change was deleted
    /// - The hash is incorrect
    #[error("Change not found: {hash}")]
    NotFound {
        /// The base32-encoded hash of the missing change
        hash: String,
    },

    /// An I/O error occurred during a filesystem operation.
    ///
    /// This wraps standard I/O errors and includes context about
    /// what operation was being attempted.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// A change file failed to serialize or deserialize.
    ///
    /// This can occur if:
    /// - The file format version is incompatible
    /// - The file is corrupted
    /// - There's a bug in the serialization code
    #[error("Change serialization error: {0}")]
    Serialization(#[from] ChangeError),

    /// The computed hash doesn't match the expected hash.
    ///
    /// This indicates data corruption or tampering. The change should
    /// not be trusted and may need to be re-downloaded from a remote.
    #[error("Hash mismatch: expected {expected}, computed {computed}")]
    HashMismatch {
        /// The hash we expected (e.g., from the filename)
        expected: String,
        /// The hash we computed from the file contents
        computed: String,
    },

    /// Failed to persist a temporary file.
    ///
    /// This can occur if:
    /// - The target path is on a different filesystem
    /// - Permission denied on the target directory
    /// - Disk is full
    #[error("Failed to persist change file: {0}")]
    Persist(#[from] tempfile::PersistError),

    /// The changes directory doesn't exist and couldn't be created.
    #[error("Changes directory not found: {path}")]
    DirectoryNotFound {
        /// The path that should contain the changes directory
        path: String,
    },

    /// The requested content range is out of bounds.
    ///
    /// This can occur when:
    /// - The span references content beyond the change's content length
    /// - The buffer provided is too small
    #[error("Content out of bounds for change {hash}: requested [{requested_start}..{requested_end}], content length {content_len}")]
    ContentOutOfBounds {
        /// The hash of the change
        hash: String,
        /// The requested start position
        requested_start: usize,
        /// The requested end position
        requested_end: usize,
        /// The actual content length
        content_len: usize,
    },
}

impl ChangeStoreError {
    /// Check if this error indicates the change doesn't exist.
    ///
    /// This is useful for distinguishing "not found" from other errors
    /// when implementing fallback logic.
    pub fn is_not_found(&self) -> bool {
        matches!(self, ChangeStoreError::NotFound { .. })
    }

    /// Check if this error indicates data corruption.
    ///
    /// Corruption errors should trigger re-download from remotes
    /// or error escalation to the user.
    pub fn is_corruption(&self) -> bool {
        matches!(self, ChangeStoreError::HashMismatch { .. })
    }
}

// LRU Cache Implementation

/// A simple LRU (Least Recently Used) cache for changes.
///
/// This cache stores recently accessed changes in memory to avoid
/// repeated disk I/O. When the cache is full, the least recently
/// used entry is evicted.
///
/// # Implementation Notes
///
/// This is a simple implementation using a `HashMap` and access timestamps.
/// For production use with high concurrency, consider using a more
/// sophisticated implementation like `lru` crate.
struct LruCache<K, V> {
    /// The cached entries
    entries: HashMap<K, CacheEntry<V>>,
    /// Maximum number of entries
    capacity: usize,
    /// Monotonic counter for tracking access order
    access_counter: u64,
}

/// An entry in the LRU cache.
struct CacheEntry<V> {
    /// The cached value
    value: V,
    /// When this entry was last accessed (higher = more recent)
    last_access: u64,
}

impl<K: Eq + std::hash::Hash + Clone, V> LruCache<K, V> {
    /// Create a new LRU cache with the given capacity.
    ///
    /// # Arguments
    ///
    /// * `capacity` - Maximum number of entries to store
    fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::with_capacity(capacity),
            capacity,
            access_counter: 0,
        }
    }

    /// Get an entry from the cache, updating its access time.
    ///
    /// Returns `None` if the key is not in the cache.
    fn get(&mut self, key: &K) -> Option<&V> {
        if let Some(entry) = self.entries.get_mut(key) {
            self.access_counter += 1;
            entry.last_access = self.access_counter;
            Some(&entry.value)
        } else {
            None
        }
    }

    /// Insert an entry into the cache.
    ///
    /// If the cache is at capacity, the least recently used entry
    /// is evicted first.
    fn insert(&mut self, key: K, value: V) {
        // Evict if at capacity
        if self.entries.len() >= self.capacity && !self.entries.contains_key(&key) {
            self.evict_lru();
        }

        self.access_counter += 1;
        self.entries.insert(
            key,
            CacheEntry {
                value,
                last_access: self.access_counter,
            },
        );
    }

    /// Remove an entry from the cache.
    ///
    /// Returns `true` if the entry was present.
    fn remove(&mut self, key: &K) -> bool {
        self.entries.remove(key).is_some()
    }

    /// Check if a key is in the cache without updating access time.
    fn contains_key(&self, key: &K) -> bool {
        self.entries.contains_key(key)
    }

    /// Evict the least recently used entry.
    fn evict_lru(&mut self) {
        if let Some(lru_key) = self
            .entries
            .iter()
            .min_by_key(|(_, entry)| entry.last_access)
            .map(|(k, _)| k.clone())
        {
            self.entries.remove(&lru_key);
        }
    }

    /// Get the current number of entries in the cache.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }

    /// Clear all entries from the cache.
    #[cfg(test)]
    fn clear(&mut self) {
        self.entries.clear();
    }
}

// ChangeStore

/// A filesystem-backed store for changes.
///
/// The `ChangeStore` manages the persistence of changes to disk, providing:
///
/// - **Content-addressed storage**: Changes are identified by their hash
/// - **Two-level directory structure**: Efficient filesystem organization
/// - **LRU caching**: Reduced disk I/O for frequently accessed changes
/// - **Atomic writes**: Safe concurrent access and crash recovery
/// - **Integrity verification**: Hash verification on load
///
/// # Thread Safety
///
/// The `ChangeStore` uses `RwLock` for the cache, making it safe for
/// concurrent access. Multiple readers can access the cache simultaneously,
/// and writers get exclusive access when needed.
///
/// # Example
///
/// ```rust,ignore
/// use atomic_repository::ChangeStore;
/// use std::path::PathBuf;
///
/// let store = ChangeStore::new(PathBuf::from(".atomic/changes"), 100)?;
///
/// // Save a change
/// let hash = store.save_change(&change)?;
///
/// // Load it back (may come from cache)
/// let loaded = store.load_change(&hash)?;
///
/// assert_eq!(change.hashed.header.message, loaded.hashed.header.message);
/// ```
pub struct ChangeStore {
    /// Path to the changes directory (`.atomic/changes/`)
    changes_dir: PathBuf,

    /// LRU cache of recently accessed changes.
    ///
    /// We use `RwLock` for thread-safe interior mutability, allowing
    /// concurrent read access while ensuring exclusive write access.
    cache: RwLock<LruCache<Hash, Change>>,
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
            cache: RwLock::new(LruCache::new(cache_capacity)),
        })
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
        // Check cache first
        if self
            .cache
            .read()
            .map(|c| c.contains_key(hash))
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
            cache.insert(hash, change.clone());
        }

        log::debug!(
            "Saved change {} to {}",
            hash.to_base32(),
            target_path.display()
        );

        Ok(hash)
    }

    /// Load a change from disk.
    ///
    /// If the change is in the cache, it's returned directly. Otherwise,
    /// it's loaded from disk, verified, and added to the cache.
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
        // Check cache first
        {
            if let Ok(mut cache) = self.cache.write() {
                if let Some(change) = cache.get(hash) {
                    log::trace!("Cache hit for change {}", hash.to_base32());
                    return Ok(change.clone());
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
            cache.insert(*hash, change.clone());
        }

        Ok(change)
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
        // Remove from cache
        if let Ok(mut cache) = self.cache.write() {
            cache.remove(hash);
        }

        // Remove from disk
        let path = self.change_path(hash);

        if !path.exists() {
            return Ok(false);
        }

        fs::remove_file(&path)?;

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

        Ok(true)
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
        ChangeIterator::new(&self.changes_dir)
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

    // Attestation Storage

    /// Get the filesystem path for an attestation with the given hash.
    ///
    /// Attestations use the same two-level directory structure as changes
    /// but with the `.attest` extension.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let hash = Hash::of(b"test");
    /// let path = store.attest_path(&hash);
    /// // e.g., ".atomic/changes/AB/ABCDEF1234567890....attest"
    /// ```
    pub fn attest_path(&self, hash: &Hash) -> PathBuf {
        let hash_str = hash.to_base32();
        let (prefix, _) = hash_str.split_at(HASH_PREFIX_LEN.min(hash_str.len()));
        self.changes_dir.join(prefix).join(format!(
            "{}.{}",
            hash_str,
            atomic_core::change::ATTESTATION_EXTENSION
        ))
    }

    /// Check if an attestation with the given hash exists on disk.
    pub fn has_attestation(&self, hash: &Hash) -> bool {
        self.attest_path(hash).exists()
    }

    /// Save an attestation to disk.
    ///
    /// Serializes the attestation, computes its hash, and writes it to
    /// the two-level directory structure with an `.attest` extension.
    ///
    /// # Returns
    ///
    /// The content hash of the attestation.
    pub fn save_attestation(
        &self,
        attestation: &atomic_core::change::Attestation,
    ) -> ChangeStoreResult<Hash> {
        let data = attestation.serialize().map_err(|e| {
            ChangeStoreError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Failed to serialize attestation: {}", e),
            ))
        })?;

        let hash = Hash::of(&data);
        let path = self.attest_path(&hash);

        // Create parent directory
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Don't overwrite if already exists (content-addressed = idempotent)
        if path.exists() {
            return Ok(hash);
        }

        // Atomic write via temp file
        let temp_file = tempfile::NamedTempFile::new_in(&self.changes_dir)?;
        temp_file.as_file().write_all(&data)?;
        temp_file.persist(&path).map_err(|e| {
            ChangeStoreError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Failed to persist attestation: {}", e),
            ))
        })?;

        log::debug!(
            "Saved attestation {} ({} bytes) to {}",
            hash.to_base32(),
            data.len(),
            path.display()
        );

        Ok(hash)
    }

    /// Load an attestation from disk by hash.
    ///
    /// # Returns
    ///
    /// The deserialized `Attestation`.
    ///
    /// # Errors
    ///
    /// Returns `ChangeStoreError::NotFound` if the file doesn't exist.
    pub fn load_attestation(
        &self,
        hash: &Hash,
    ) -> ChangeStoreResult<atomic_core::change::Attestation> {
        let path = self.attest_path(hash);

        if !path.exists() {
            return Err(ChangeStoreError::NotFound {
                hash: hash.to_base32(),
            });
        }

        let data = fs::read(&path)?;
        let (attestation, computed_hash) = atomic_core::change::Attestation::deserialize(&data)
            .map_err(|e| {
                ChangeStoreError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("Failed to deserialize attestation: {}", e),
                ))
            })?;

        // Verify hash integrity
        if computed_hash != *hash {
            return Err(ChangeStoreError::HashMismatch {
                expected: hash.to_base32(),
                computed: computed_hash.to_base32(),
            });
        }

        Ok(attestation)
    }

    /// Iterate over all attestation hashes on disk.
    pub fn iter_attestations(&self) -> impl Iterator<Item = ChangeStoreResult<Hash>> + '_ {
        AttestationIterator::new(&self.changes_dir)
    }

    /// Count attestations on disk.
    pub fn count_attestations(&self) -> ChangeStoreResult<usize> {
        let mut count = 0;
        for result in self.iter_attestations() {
            result?;
            count += 1;
        }
        Ok(count)
    }
}

// Attestation Iterator

/// Iterator over attestation hashes in the changes directory.
///
/// Walks the two-level directory structure looking for `.attest` files.
#[allow(dead_code)]
struct AttestationIterator<'a> {
    changes_dir: &'a Path,
    prefix_dirs: Option<fs::ReadDir>,
    current_files: Option<fs::ReadDir>,
}

impl<'a> AttestationIterator<'a> {
    fn new(changes_dir: &'a Path) -> Self {
        let prefix_dirs = fs::read_dir(changes_dir).ok();
        Self {
            changes_dir,
            prefix_dirs,
            current_files: None,
        }
    }

    fn next_from_files(&mut self) -> Option<ChangeStoreResult<Hash>> {
        loop {
            if let Some(ref mut files) = self.current_files {
                match files.next() {
                    Some(Ok(entry)) => {
                        let path = entry.path();
                        if !path.is_file() {
                            continue;
                        }
                        if path
                            .extension()
                            .map_or(true, |e| e != atomic_core::change::ATTESTATION_EXTENSION)
                        {
                            continue;
                        }
                        // Extract hash from filename (strip extension)
                        let stem = match path.file_stem().and_then(|s| s.to_str()) {
                            Some(s) => s.to_string(),
                            None => continue,
                        };
                        match Hash::from_base32(stem.as_bytes()) {
                            Some(hash) => return Some(Ok(hash)),
                            None => continue,
                        }
                    }
                    Some(Err(e)) => return Some(Err(e.into())),
                    None => {
                        self.current_files = None;
                    }
                }
            } else {
                return None;
            }
        }
    }
}

impl<'a> Iterator for AttestationIterator<'a> {
    type Item = ChangeStoreResult<Hash>;

    fn next(&mut self) -> Option<Self::Item> {
        // Try current files first
        if let Some(result) = self.next_from_files() {
            return Some(result);
        }

        // Move to next prefix directory
        loop {
            let prefix_dirs = self.prefix_dirs.as_mut()?;
            match prefix_dirs.next() {
                Some(Ok(entry)) => {
                    let path = entry.path();
                    if !path.is_dir() {
                        continue;
                    }
                    self.current_files = fs::read_dir(&path).ok();
                    if let Some(result) = self.next_from_files() {
                        return Some(result);
                    }
                }
                Some(Err(e)) => return Some(Err(e.into())),
                None => return None,
            }
        }
    }
}

// ChangeStore Trait Implementation

/// Implementation of the `atomic_core::change::ChangeStore` trait.
///
/// This allows the repository's `ChangeStore` to be used with the core
/// library's content retrieval functions (like `retrieve_content`).
impl ChangeStoreTrait for ChangeStore {
    type Error = ChangeStoreError;

    fn get_change(&self, hash: &Hash) -> Result<Change, Self::Error> {
        self.load_change(hash)
    }

    fn has_change(&self, hash: &Hash) -> bool {
        ChangeStore::has_change(self, hash)
    }

    fn get_contents<F>(
        &self,
        hash_fn: F,
        span: GraphNode<NodeId>,
        buf: &mut [u8],
    ) -> Result<usize, Self::Error>
    where
        F: Fn(NodeId) -> Option<Hash>,
    {
        // Handle ROOT span
        if span == GraphNode::ROOT {
            return Ok(0);
        }

        // Get the hash for this span's change
        let hash = match hash_fn(span.change) {
            Some(h) => h,
            None => {
                return Err(ChangeStoreError::NotFound {
                    hash: format!("NodeId({:?})", span.change),
                });
            }
        };

        // Load the change
        let change = self.load_change(&hash)?;

        // Extract content bytes
        let start = span.start.get() as usize;
        let end = span.end.get() as usize;

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

    fn get_contents_ext(
        &self,
        span: GraphNode<Option<Hash>>,
        buf: &mut [u8],
    ) -> Result<usize, Self::Error> {
        // Handle None hash (ROOT span)
        let hash = match span.change {
            Some(h) => h,
            None => return Ok(0),
        };

        // Load the change
        let change = self.load_change(&hash)?;

        // Extract content bytes
        let start = span.start.get() as usize;
        let end = span.end.get() as usize;

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

    fn get_header(&self, hash: &Hash) -> Result<ChangeHeader, Self::Error> {
        let change = self.load_change(hash)?;
        Ok(change.hashed.header)
    }

    fn get_dependencies(&self, hash: &Hash) -> Result<Vec<Hash>, Self::Error> {
        let change = self.load_change(hash)?;
        Ok(change.hashed.dependencies)
    }
}

// Change Iterator

/// Iterator over changes stored in the filesystem.
///
/// This iterator walks the two-level directory structure and yields
/// the hash of each valid change file found.
struct ChangeIterator {
    /// Iterator over subdirectories (the two-character prefixes)
    dir_iter: Option<fs::ReadDir>,
    /// Iterator over files in the current subdirectory
    file_iter: Option<fs::ReadDir>,
}

impl ChangeIterator {
    /// Create a new iterator starting from the changes directory.
    fn new(changes_dir: &Path) -> Self {
        let dir_iter = fs::read_dir(changes_dir).ok();
        Self {
            dir_iter,
            file_iter: None,
        }
    }

    /// Try to get the next hash from the current file iterator.
    fn next_from_files(&mut self) -> Option<ChangeStoreResult<Hash>> {
        let file_iter = self.file_iter.as_mut()?;

        for entry_result in file_iter {
            match entry_result {
                Ok(entry) => {
                    let path = entry.path();

                    // Skip if not a file
                    if !path.is_file() {
                        continue;
                    }

                    // Check for .change extension
                    if path.extension().map_or(true, |e| e != CHANGE_EXTENSION) {
                        continue;
                    }

                    // Extract the hash from the filename
                    if let Some(hash) = Self::hash_from_path(&path) {
                        return Some(Ok(hash));
                    }
                }
                Err(e) => return Some(Err(ChangeStoreError::Io(e))),
            }
        }

        None
    }

    /// Extract a hash from a change file path.
    fn hash_from_path(path: &Path) -> Option<Hash> {
        let stem = path.file_stem()?.to_str()?;
        Hash::from_base32(stem.as_bytes())
    }
}

impl Iterator for ChangeIterator {
    type Item = ChangeStoreResult<Hash>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            // Try to get next from current file iterator
            if let Some(result) = self.next_from_files() {
                return Some(result);
            }

            // Move to next subdirectory
            let dir_iter = self.dir_iter.as_mut()?;

            loop {
                match dir_iter.next()? {
                    Ok(entry) => {
                        let path = entry.path();
                        if path.is_dir() {
                            self.file_iter = fs::read_dir(&path).ok();
                            break;
                        }
                    }
                    Err(e) => return Some(Err(ChangeStoreError::Io(e))),
                }
            }
        }
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::change::{Author, Change, ChangeHeader};
    use tempfile::TempDir;

    // Test Helpers

    /// Create a temporary directory for testing.
    fn create_temp_dir() -> TempDir {
        TempDir::new().expect("Failed to create temp directory")
    }

    /// Create a test change store in a temporary directory.
    fn create_test_store() -> (ChangeStore, TempDir) {
        let temp_dir = create_temp_dir();
        let changes_dir = temp_dir.path().join("changes");
        let store =
            ChangeStore::new(changes_dir, DEFAULT_CACHE_CAPACITY).expect("Failed to create store");
        (store, temp_dir)
    }

    /// Create a simple test change with the given message.
    fn create_test_change(message: &str) -> Change {
        let header = ChangeHeader::builder()
            .message(message)
            .author(Author::new("Test Author", Some("test@example.com")))
            .build();

        Change::new(header, Vec::new(), Vec::new(), Vec::new())
    }

    /// Create a test change with some content.
    fn create_test_change_with_content(message: &str, content: &[u8]) -> Change {
        let header = ChangeHeader::builder()
            .message(message)
            .author(Author::new("Test Author", Some("test@example.com")))
            .build();

        Change::new(header, Vec::new(), content.to_vec(), Vec::new())
    }

    // LRU Cache Tests

    #[test]
    fn test_lru_cache_basic_operations() {
        let mut cache: LruCache<u32, String> = LruCache::new(3);

        // Insert and retrieve
        cache.insert(1, "one".to_string());
        cache.insert(2, "two".to_string());

        assert_eq!(cache.get(&1), Some(&"one".to_string()));
        assert_eq!(cache.get(&2), Some(&"two".to_string()));
        assert_eq!(cache.get(&3), None);
    }

    #[test]
    fn test_lru_cache_eviction() {
        let mut cache: LruCache<u32, String> = LruCache::new(2);

        cache.insert(1, "one".to_string());
        cache.insert(2, "two".to_string());
        cache.insert(3, "three".to_string()); // Should evict 1

        assert_eq!(cache.get(&1), None); // Evicted
        assert_eq!(cache.get(&2), Some(&"two".to_string()));
        assert_eq!(cache.get(&3), Some(&"three".to_string()));
    }

    #[test]
    fn test_lru_cache_access_updates_order() {
        let mut cache: LruCache<u32, String> = LruCache::new(2);

        cache.insert(1, "one".to_string());
        cache.insert(2, "two".to_string());

        // Access 1 to make it more recent
        let _ = cache.get(&1);

        // Insert 3, should evict 2 (now the LRU)
        cache.insert(3, "three".to_string());

        assert_eq!(cache.get(&1), Some(&"one".to_string())); // Still present
        assert_eq!(cache.get(&2), None); // Evicted
        assert_eq!(cache.get(&3), Some(&"three".to_string()));
    }

    #[test]
    fn test_lru_cache_remove() {
        let mut cache: LruCache<u32, String> = LruCache::new(3);

        cache.insert(1, "one".to_string());
        assert!(cache.contains_key(&1));

        let removed = cache.remove(&1);
        assert!(removed);
        assert!(!cache.contains_key(&1));

        let removed_again = cache.remove(&1);
        assert!(!removed_again);
    }

    #[test]
    fn test_lru_cache_update_existing() {
        let mut cache: LruCache<u32, String> = LruCache::new(2);

        cache.insert(1, "one".to_string());
        cache.insert(1, "ONE".to_string()); // Update, not new entry

        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get(&1), Some(&"ONE".to_string()));
    }

    // ChangeStore Path Tests

    #[test]
    fn test_change_path_format() {
        let (store, _temp) = create_test_store();

        let hash = Hash::of(b"test content");
        let path = store.change_path(&hash);

        // Path should be: {changes_dir}/{prefix}/{full_hash}.change
        let hash_str = hash.to_base32();
        let expected_prefix = &hash_str[..2];

        assert!(path.to_string_lossy().contains(expected_prefix));
        assert!(path.to_string_lossy().ends_with(".change"));
        assert!(path.to_string_lossy().contains(&hash_str));
    }

    #[test]
    fn test_change_path_deterministic() {
        let (store, _temp) = create_test_store();

        let hash = Hash::of(b"test");

        // Same hash should always produce same path
        let path1 = store.change_path(&hash);
        let path2 = store.change_path(&hash);

        assert_eq!(path1, path2);
    }

    #[test]
    fn test_change_path_different_hashes_different_paths() {
        let (store, _temp) = create_test_store();

        let hash1 = Hash::of(b"content 1");
        let hash2 = Hash::of(b"content 2");

        let path1 = store.change_path(&hash1);
        let path2 = store.change_path(&hash2);

        assert_ne!(path1, path2);
    }

    // ChangeStore Save/Load Tests

    #[test]
    fn test_save_change() {
        let (store, _temp) = create_test_store();

        let change = create_test_change("Test save change");
        let result = store.save_change(&change);

        assert!(result.is_ok());

        let hash = result.unwrap();

        // Verify the file was created
        let path = store.change_path(&hash);
        assert!(path.exists(), "Change file should exist at {:?}", path);
    }

    #[test]
    fn test_load_change() {
        let (store, _temp) = create_test_store();

        // Save a change first
        let original = create_test_change("Test load change");
        let hash = store.save_change(&original).expect("Failed to save change");

        // Clear cache to force disk read
        store.clear_cache();

        // Load the change
        let loaded = store.load_change(&hash).expect("Failed to load change");

        // Verify the data matches
        assert_eq!(original.hashed.header.message, loaded.hashed.header.message);
    }

    #[test]
    fn test_save_load_roundtrip() {
        let (store, _temp) = create_test_store();

        let original = create_test_change_with_content(
            "Test roundtrip",
            b"Hello, this is test content for the change!",
        );

        // Save
        let hash = store.save_change(&original).expect("Failed to save change");

        // Clear cache to ensure we read from disk
        store.clear_cache();

        // Load
        let loaded = store.load_change(&hash).expect("Failed to load change");

        // Verify all fields
        assert_eq!(original.hashed.header.message, loaded.hashed.header.message);
        assert_eq!(original.contents, loaded.contents);
        assert_eq!(
            original.hashed.header.authors.len(),
            loaded.hashed.header.authors.len()
        );
    }

    #[test]
    fn test_load_nonexistent_change() {
        let (store, _temp) = create_test_store();

        let fake_hash = Hash::of(b"nonexistent");
        let result = store.load_change(&fake_hash);

        assert!(result.is_err());
        assert!(
            result.unwrap_err().is_not_found(),
            "Should return NotFound error"
        );
    }

    #[test]
    fn test_has_change() {
        let (store, _temp) = create_test_store();

        let change = create_test_change("Test has_change");
        let hash = store.save_change(&change).expect("Failed to save change");

        // Should exist
        assert!(store.has_change(&hash));

        // Should not exist
        let fake_hash = Hash::of(b"nonexistent");
        assert!(!store.has_change(&fake_hash));
    }

    #[test]
    fn test_has_change_from_cache() {
        let (store, _temp) = create_test_store();

        let change = create_test_change("Test cache check");
        let hash = store.save_change(&change).expect("Failed to save change");

        // Change should be in cache after save
        assert!(store.cache_size() > 0);
        assert!(store.has_change(&hash));
    }

    // ChangeStore Delete Tests

    #[test]
    fn test_delete_change() {
        let (store, _temp) = create_test_store();

        let change = create_test_change("Test delete change");
        let hash = store.save_change(&change).expect("Failed to save change");

        // Verify it exists
        assert!(store.has_change(&hash));

        // Delete it
        let deleted = store.delete_change(&hash).expect("Failed to delete change");
        assert!(deleted, "delete_change should return true");

        // Verify it's gone
        assert!(!store.has_change(&hash));

        // Verify the file is gone
        let path = store.change_path(&hash);
        assert!(!path.exists());
    }

    #[test]
    fn test_delete_nonexistent_change() {
        let (store, _temp) = create_test_store();

        let fake_hash = Hash::of(b"nonexistent");
        let deleted = store
            .delete_change(&fake_hash)
            .expect("delete_change should succeed");

        assert!(!deleted, "Should return false for nonexistent change");
    }

    #[test]
    fn test_delete_removes_from_cache() {
        let (store, _temp) = create_test_store();

        let change = create_test_change("Test cache removal");
        let hash = store.save_change(&change).expect("Failed to save change");

        // Verify in cache
        assert!(store.cache_size() > 0);

        // Delete
        store.delete_change(&hash).expect("Failed to delete");

        // Load should fail (not found)
        let result = store.load_change(&hash);
        assert!(result.is_err());
        assert!(result.unwrap_err().is_not_found());
    }

    // ChangeStore Iteration Tests

    #[test]
    fn test_iter_changes_empty() {
        let (store, _temp) = create_test_store();

        let changes: Vec<_> = store.iter_changes().collect();
        assert!(changes.is_empty());
    }

    #[test]
    fn test_iter_changes() {
        let (store, _temp) = create_test_store();

        // Save multiple changes
        let mut saved_hashes = Vec::new();
        for i in 0..5 {
            let change = create_test_change(&format!("Change {}", i));
            let hash = store.save_change(&change).expect("Failed to save change");
            saved_hashes.push(hash);
        }

        // Iterate and collect
        let found_hashes: Vec<Hash> = store.iter_changes().filter_map(|r| r.ok()).collect();

        // All saved changes should be found
        assert_eq!(found_hashes.len(), saved_hashes.len());
        for hash in &saved_hashes {
            assert!(
                found_hashes.contains(hash),
                "Should find saved hash {}",
                hash.to_base32()
            );
        }
    }

    #[test]
    fn test_count_changes() {
        let (store, _temp) = create_test_store();

        // Initially empty
        assert_eq!(store.count_changes().unwrap(), 0);

        // Add some changes
        for i in 0..3 {
            let change = create_test_change(&format!("Change {}", i));
            store.save_change(&change).expect("Failed to save change");
        }

        assert_eq!(store.count_changes().unwrap(), 3);
    }

    // Cache Behavior Tests

    #[test]
    fn test_cache_hit() {
        let (store, _temp) = create_test_store();

        let change = create_test_change("Test cache hit");
        let hash = store.save_change(&change).expect("Failed to save change");

        // First load adds to cache
        let _ = store.load_change(&hash).expect("First load failed");
        assert!(store.cache_size() > 0);

        // Second load should be from cache (we can't directly verify this,
        // but we can verify it still works)
        let loaded = store.load_change(&hash).expect("Second load failed");
        assert_eq!(change.hashed.header.message, loaded.hashed.header.message);
    }

    #[test]
    fn test_cache_eviction() {
        // Create store with small cache
        let temp_dir = create_temp_dir();
        let changes_dir = temp_dir.path().join("changes");
        let store = ChangeStore::new(changes_dir, 2).expect("Failed to create store");

        // Save 3 changes (more than cache capacity)
        let mut hashes = Vec::new();
        for i in 0..3 {
            let change = create_test_change(&format!("Change {}", i));
            let hash = store.save_change(&change).expect("Failed to save change");
            hashes.push(hash);
        }

        // Cache should have at most 2 entries
        assert!(store.cache_size() <= 2);

        // All changes should still be loadable from disk
        for hash in &hashes {
            store.clear_cache();
            let result = store.load_change(hash);
            assert!(
                result.is_ok(),
                "Should load change {} from disk",
                hash.to_base32()
            );
        }
    }

    // Error Condition Tests

    #[test]
    fn test_error_is_not_found() {
        let err = ChangeStoreError::NotFound {
            hash: "ABCDEF".to_string(),
        };
        assert!(err.is_not_found());

        let err = ChangeStoreError::Io(std::io::Error::new(std::io::ErrorKind::Other, "test"));
        assert!(!err.is_not_found());
    }

    #[test]
    fn test_error_is_corruption() {
        let err = ChangeStoreError::HashMismatch {
            expected: "ABCDEF".to_string(),
            computed: "123456".to_string(),
        };
        assert!(err.is_corruption());

        let err = ChangeStoreError::NotFound {
            hash: "ABCDEF".to_string(),
        };
        assert!(!err.is_corruption());
    }

    #[test]
    fn test_error_display() {
        let err = ChangeStoreError::NotFound {
            hash: "ABCDEF".to_string(),
        };
        assert!(err.to_string().contains("ABCDEF"));

        let err = ChangeStoreError::HashMismatch {
            expected: "EXPECTED".to_string(),
            computed: "COMPUTED".to_string(),
        };
        assert!(err.to_string().contains("EXPECTED"));
        assert!(err.to_string().contains("COMPUTED"));
    }

    // From Root Tests

    #[test]
    fn test_from_root() {
        let temp_dir = create_temp_dir();
        let root = temp_dir.path();

        let store =
            ChangeStore::from_root(root, DEFAULT_CACHE_CAPACITY).expect("Failed to create store");

        // Verify the changes directory was created in the right place
        let expected_dir = root.join(atomic_core::DOT_DIR).join("changes");
        assert_eq!(store.changes_dir(), expected_dir);
        assert!(expected_dir.exists());
    }

    // Multiple Changes with Same Prefix Tests

    #[test]
    fn test_multiple_changes_same_directory() {
        let (store, _temp) = create_test_store();

        // Save many changes (some will share directory prefixes statistically)
        let mut hashes = Vec::new();
        for i in 0..20 {
            let change = create_test_change(&format!("Change number {}", i));
            let hash = store.save_change(&change).expect("Failed to save change");
            hashes.push(hash);
        }

        // All should be retrievable
        for hash in &hashes {
            store.clear_cache();
            let result = store.load_change(hash);
            assert!(
                result.is_ok(),
                "Failed to load change {}: {:?}",
                hash.to_base32(),
                result.err()
            );
        }

        // Count should match
        assert_eq!(store.count_changes().unwrap(), 20);
    }

    // Content Preservation Tests

    #[test]
    fn test_large_content_preservation() {
        let (store, _temp) = create_test_store();

        // Create a change with large content
        let large_content: Vec<u8> = (0..10000).map(|i| (i % 256) as u8).collect();
        let change = create_test_change_with_content("Large content test", &large_content);

        let hash = store.save_change(&change).expect("Failed to save change");

        store.clear_cache();

        let loaded = store.load_change(&hash).expect("Failed to load change");

        assert_eq!(loaded.contents.len(), large_content.len());
        assert_eq!(loaded.contents, large_content);
    }

    #[test]
    fn test_empty_content_preservation() {
        let (store, _temp) = create_test_store();

        let change = create_test_change_with_content("Empty content test", &[]);

        let hash = store.save_change(&change).expect("Failed to save change");

        store.clear_cache();

        let loaded = store.load_change(&hash).expect("Failed to load change");

        assert!(loaded.contents.is_empty());
    }

    #[test]
    fn test_binary_content_preservation() {
        let (store, _temp) = create_test_store();

        // Create binary content with all byte values
        let binary_content: Vec<u8> = (0..=255).collect();
        let change = create_test_change_with_content("Binary content test", &binary_content);

        let hash = store.save_change(&change).expect("Failed to save change");

        store.clear_cache();

        let loaded = store.load_change(&hash).expect("Failed to load change");

        assert_eq!(loaded.contents, binary_content);
    }
}
