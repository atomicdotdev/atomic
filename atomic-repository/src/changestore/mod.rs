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
//! The [`ChangeStore`] uses interior mutability ([`std::sync::RwLock`]) for the cache,
//! making it safe for concurrent access. Multiple readers can access the cache
//! simultaneously, and writers get exclusive access when needed.

mod attestation;
mod iterators;
mod provenance;
mod trait_impl;

#[cfg(test)]
mod tests;

use std::fs::{self, File};
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use atomic_core::change::Change;
use atomic_core::types::{Base32, Hash};

// Re-export iterator types used by the public API
pub(crate) use iterators::ChangeIterator;

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
pub(crate) const HASH_PREFIX_LEN: usize = 2;

// Error Types

/// Result type for change store operations.
mod error;
pub use error::{ChangeStoreError, ChangeStoreResult};

mod cache;
pub(crate) use cache::LruCache;

pub struct ChangeStore {
    /// Path to the changes directory (`.atomic/changes/`)
    pub(crate) changes_dir: PathBuf,

    /// LRU cache of recently accessed changes.
    ///
    /// We use `RwLock` for thread-safe interior mutability, allowing
    /// concurrent read access while ensuring exclusive write access.
    pub(crate) cache: RwLock<LruCache<Hash, Change>>,
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
        {
            if let Ok(mut cache) = self.cache.write() {
                if let Some(change) = cache.get(hash) {
                    return copy_content_from_change(hash, change, start, end, buf);
                }
            }
        }

        let path = self.change_path(hash);
        log::debug!(
            "Loading change content {} from {}",
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

        if computed_hash != *hash {
            return Err(ChangeStoreError::HashMismatch {
                expected: hash.to_base32(),
                computed: computed_hash.to_base32(),
            });
        }

        let copied = copy_content_from_change(hash, &change, start, end, buf)?;

        if let Ok(mut cache) = self.cache.write() {
            cache.insert(*hash, change);
        }

        Ok(copied)
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
}
