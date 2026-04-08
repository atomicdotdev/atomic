//! Simple LRU cache for the change store.

use std::collections::HashMap;

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
pub(crate) struct LruCache<K, V> {
    /// The cached entries
    entries: HashMap<K, CacheEntry<V>>,
    /// Maximum number of entries
    pub(crate) capacity: usize,
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
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::with_capacity(capacity),
            capacity,
            access_counter: 0,
        }
    }

    /// Get an entry from the cache, updating its access time.
    ///
    /// Returns `None` if the key is not in the cache.
    pub(crate) fn get(&mut self, key: &K) -> Option<&V> {
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
    pub(crate) fn insert(&mut self, key: K, value: V) {
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
    pub(crate) fn remove(&mut self, key: &K) -> bool {
        self.entries.remove(key).is_some()
    }

    /// Check if a key is in the cache without updating access time.
    pub(crate) fn contains_key(&self, key: &K) -> bool {
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
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Clear all entries from the cache.
    #[cfg(test)]
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }
}
