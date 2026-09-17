//! Local cache of remotely-resolved identity public keys.
//!
//! `atomic intent verify --identity <name>` resolves a signer's public key
//! from the storage server on every invocation. This module caches the
//! resolved key (keyed by identity name) under
//! `~/.atomic/public-key-cache.json` (or `$ATOMIC_CONFIG_DIR`), so repeat
//! verifies — and offline verifies — don't need the network.
//!
//! # Security model
//!
//! * Entries expire (default 24h) so rotated keys are re-fetched.
//! * The cache is only a byte source. Callers MUST still run the
//!   `did:atomic` fingerprint gate against the attestation's recorded
//!   signer before trusting a cached key, and a gate miss should refresh
//!   from the server rather than trust the cache.
//! * The cache holds only public verification material; it is never used
//!   for authentication.
//! * Only `active` entries are served, and a corrupt/unreadable cache
//!   degrades to empty (a cache miss is never fatal).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

use atomic_config::global_config_dir;

use std::path::Path;

/// How long a resolved key is trusted before it must be re-fetched.
pub const DEFAULT_CACHE_TTL: chrono::Duration = chrono::Duration::hours(24);

const CACHE_FILE: &str = "public-key-cache.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CacheEntry {
    /// Canonical base32-encoded Ed25519 public key.
    public_key: String,
    /// Lifecycle status as reported by the server (`active`, …).
    status: String,
    /// When the key was resolved.
    resolved_at: DateTime<Utc>,
    /// When this entry stops being served.
    expires_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CacheFile {
    entries: BTreeMap<String, CacheEntry>,
}

/// A cached resolution of an identity's public key.
#[derive(Debug, Clone)]
pub struct CachedKey {
    /// Canonical base32-encoded Ed25519 public key.
    pub public_key: String,
    /// When this key was resolved from the server.
    pub resolved_at: DateTime<Utc>,
}

/// File-backed cache of name → public key resolutions.
pub struct PublicKeyCache {
    dir: PathBuf,
    path: PathBuf,
    entries: BTreeMap<String, CacheEntry>,
}

impl PublicKeyCache {
    /// Open the cache, degrading to an empty cache on any problem.
    ///
    /// Missing file, unreadable file, or an unparseable/corrupt file all
    /// yield an empty cache — a broken cache must never block verification.
    pub fn open() -> Self {
        let dir = global_config_dir().unwrap_or_else(|| {
            let home = dirs::home_dir().unwrap_or(PathBuf::from("."));
            home.join(".atomic")
        });
        Self::open_at(dir.as_path())
    }

    /// Open the cache rooted at `dir` (used by tests).
    pub fn open_at(dir: &Path) -> Self {
        let path = dir.to_path_buf().join(CACHE_FILE);

        if let Ok(raw) = std::fs::read_to_string(&path) {
            match serde_json::from_str::<CacheFile>(&raw) {
                Ok(file) => {
                    return Self {
                        dir: dir.to_path_buf(),
                        path,
                        entries: file.entries,
                    }
                }
                Err(e) => {
                    log::debug!("key cache unreadable (ignoring): {e}");
                }
            }
        }

        Self {
            dir: dir.to_path_buf(),
            path,
            entries: BTreeMap::new(),
        }
    }

    /// Look up a cached, unexpired, active key by identity name.
    ///
    /// Returns `None` when the name is unknown, the entry is expired, or
    /// the identity is not `active` — in all cases the caller should fall
    /// back to the server.
    pub fn get(&self, name: &str) -> Option<CachedKey> {
        if let Some(entry) = self.entries.get(name) {
            if entry.status != "active" {
                return None;
            }
            if Utc::now() >= entry.expires_at {
                return None;
            }
            return Some(CachedKey {
                public_key: entry.public_key.clone(),
                resolved_at: entry.resolved_at,
            });
        }
        None
    }

    /// Store a resolution and persist it.
    ///
    /// Write failures are logged and swallowed — the cache is
    /// best-effort and never fatal.
    pub fn put(&mut self, name: &str, public_key: &str, status: &str) {
        self.put_with_ttl(name, public_key, status, DEFAULT_CACHE_TTL);
    }

    /// Store a resolution with an explicit TTL (tests use negative TTLs).
    pub fn put_with_ttl(
        &mut self,
        name: &str,
        public_key: &str,
        status: &str,
        ttl: chrono::Duration,
    ) {
        let now = Utc::now();
        let entry = CacheEntry {
            public_key: public_key.to_string(),
            status: status.to_string(),
            resolved_at: now,
            expires_at: now + ttl,
        };
        self.entries.insert(name.to_string(), entry);

        // Best-effort persist. A read-only home dir or a full disk must not
        // fail verification.
        if let Err(e) = std::fs::create_dir_all(self.dir.as_path()) {
            log::debug!("could not create key cache dir: {e}");
            return;
        }
        let file = CacheFile {
            entries: self.entries.clone(),
        };
        match serde_json::to_string_pretty(&file) {
            Ok(json) => {
                if let Err(e) = std::fs::write(self.path.as_path(), json.into_bytes()) {
                    log::debug!("could not write key cache: {e}");
                }
            }
            Err(e) => {
                log::debug!("could not serialize key cache: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(tag: &str) -> PathBuf {
        let tmp = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
        let dir = PathBuf::from(&tmp)
            .join("atomic-key-cache-test")
            .join(format!("{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn empty_cache_serves_nothing() {
        let cache = PublicKeyCache::open_at(scratch_dir("empty").as_path());
        assert!(cache.get("lee").is_none());
    }

    #[test]
    fn put_then_get_round_trips() {
        let dir = scratch_dir("roundtrip");
        let mut cache = PublicKeyCache::open_at(dir.as_path());
        cache.put(
            "lee",
            "H2AM5IDITAWB5LBSKMNOCVKOUL4ZUS5GHGEHHEGESOTUYCOC4HJA",
            "active",
        );

        let hit = cache.get("lee").unwrap();
        assert_eq!(
            hit.public_key,
            "H2AM5IDITAWB5LBSKMNOCVKOUL4ZUS5GHGEHHEGESOTUYCOC4HJA"
        );

        // And a re-open from disk sees the persisted entry.
        let reopened = PublicKeyCache::open_at(dir.as_path());
        assert_eq!(
            reopened.get("lee").map(|c| c.public_key).as_deref(),
            Some("H2AM5IDITAWB5LBSKMNOCVKOUL4ZUS5GHGEHHEGESOTUYCOC4HJA".into())
        );
    }

    #[test]
    fn unknown_name_is_a_miss() {
        let dir = scratch_dir("unknown");
        let mut cache = PublicKeyCache::open_at(dir.as_path());
        cache.put("lee", "ABC", "active");
        assert!(cache.get("other").is_none());
    }

    #[test]
    fn expired_entries_are_not_served() {
        let dir = scratch_dir("expired");
        let mut cache = PublicKeyCache::open_at(dir.as_path());
        cache.put_with_ttl("lee", "ABC", "active", chrono::Duration::seconds(-1));
        assert!(cache.get("lee").is_none());
    }

    #[test]
    fn suspended_entries_are_not_served() {
        let dir = scratch_dir("suspended");
        let mut cache = PublicKeyCache::open_at(dir.as_path());
        cache.put("lee", "ABC", "suspended");
        assert!(cache.get("lee").is_none());
    }

    #[test]
    fn corrupt_cache_degrades_to_empty() {
        let dir = scratch_dir("corrupt");
        std::fs::write(&dir.join(CACHE_FILE), b"not json at all").unwrap();

        let cache = PublicKeyCache::open_at(dir.as_path());
        assert!(cache.get("lee").is_none());
    }
}
