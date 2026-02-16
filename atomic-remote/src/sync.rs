//! Remote synchronization with dichotomy (binary search) divergence detection.
//!
//! This module implements the core sync algorithm for efficiently determining
//! what needs to be pushed or pulled between a local repository and a remote.
//! The algorithm was originally implemented in `atomic-pijul/atomic-remote` and
//! has been ported to work with the clean-room Atomic types.
//!
//! # How It Works
//!
//! When syncing with a remote, we need to answer: "Where did our local view
//! of the remote diverge from reality?" The naive approach compares every
//! change one-by-one (O(n)). We do better:
//!
//! 1. **Cache the remote state locally** — After each sync, we remember what
//!    the remote looked like (a sequence of `(hash, merkle_state)` pairs).
//!
//! 2. **Binary search for divergence** — Compare Merkle states at the midpoint.
//!    If they match, the divergence is later; if not, it's earlier. This finds
//!    the divergence point in O(log n) remote round-trips.
//!
//! 3. **Compute the delta** — Once we know where things diverged, we only need
//!    to transfer changes after that point.
//!
//! ```text
//! Local cache of remote:   [A, B, C, D, E, F, G, H, I, J]
//! Actual remote state:     [A, B, C, D, E, X, Y, Z]
//!                                          ^
//!                              Divergence at position 5
//!                              Found in ~4 comparisons (log2(10))
//!                              instead of 10
//! ```
//!
//! # Architecture
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────────┐
//! │                         sync.rs                                  │
//! │                                                                  │
//! │  RemoteState (local cache)                                       │
//! │       │                                                          │
//! │       ▼                                                          │
//! │  dichotomy_changelist()  ← O(log n) binary search                │
//! │       │                                                          │
//! │       ▼                                                          │
//! │  RemoteDelta { to_download, ours_ge_dichotomy, theirs_ge_... }  │
//! │       │                                                          │
//! │       ├──▶ compute_pull_delta() → PullDelta                     │
//! │       └──▶ compute_push_delta() → PushDelta                     │
//! │                                                                  │
//! │  Uses: HttpRemote.get_state(), HttpRemote.get_changelist()      │
//! └──────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```ignore
//! use atomic_remote::sync::{RemoteState, SyncEngine};
//! use atomic_remote::http::HttpRemote;
//!
//! async fn sync_example() -> Result<(), Box<dyn std::error::Error>> {
//!     let remote = HttpRemote::new("https://api.example.com/repo")?;
//!     let mut cache = RemoteState::empty();
//!
//!     // Compute what needs to be pulled
//!     let mut engine = SyncEngine::new(&remote, &mut cache);
//!     let delta = engine.compute_pull_delta("main").await?;
//!
//!     for node in &delta.to_download {
//!         println!("Need to download: {}", node.hash);
//!     }
//!     Ok(())
//! }
//! ```

use std::collections::{HashMap, HashSet};
use std::fmt;

use log::debug;

use crate::error::RemoteResult;
use crate::http::HttpRemote;
use crate::types::{ChangelistEntry, Node, PullDelta, PushDelta, StateResponse};

// Remote State Cache

/// A cached entry representing one position in the remote's history.
///
/// Each entry records the change hash and the cumulative Merkle state
/// at that sequence position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedEntry {
    /// Sequence number in the remote's history.
    pub sequence: u64,
    /// The change/tag node at this position.
    pub node: Node,
}

/// Local cache of a remote repository's state.
///
/// After each successful sync, we store the remote's changelist so that
/// future syncs can use the dichotomy algorithm to find the divergence
/// point without re-downloading the entire history.
///
/// The cache is keyed by sequence number and stores the Merkle state
/// at each position. This allows O(1) lookup of "what was the remote's
/// state at position N?" and O(log n) binary search for divergence.
#[derive(Debug, Clone)]
pub struct RemoteState {
    /// Cached entries ordered by sequence number.
    ///
    /// Key: sequence number, Value: the node at that position.
    entries: HashMap<u64, Node>,

    /// The highest sequence number we've cached.
    last_sequence: Option<u64>,

    /// The Merkle state at the last cached position.
    last_state: Option<String>,

    /// The tag Merkle state at the last cached position.
    last_tag_state: Option<String>,

    /// Name of the remote this cache belongs to.
    remote_name: String,
}

impl RemoteState {
    /// Create an empty cache (no prior sync history).
    pub fn empty() -> Self {
        Self {
            entries: HashMap::new(),
            last_sequence: None,
            last_state: None,
            last_tag_state: None,
            remote_name: String::new(),
        }
    }

    /// Create a cache with a name.
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            remote_name: name.into(),
            ..Self::empty()
        }
    }

    /// Build a cache from a list of changelist entries.
    ///
    /// This is used to seed the cache from a full changelist download
    /// (e.g., on the first sync).
    pub fn from_changelist(entries: &[ChangelistEntry]) -> Self {
        let mut cache = Self::empty();
        for entry in entries {
            let node = entry.to_node();
            cache.entries.insert(entry.sequence, node);
            cache.last_sequence = Some(entry.sequence);
            cache.last_state = Some(entry.merkle.clone());
        }
        cache
    }

    /// Get the cached node at a given sequence number.
    pub fn get(&self, sequence: u64) -> Option<&Node> {
        self.entries.get(&sequence)
    }

    /// Get the Merkle state at a given sequence number.
    pub fn get_state(&self, sequence: u64) -> Option<&str> {
        self.entries.get(&sequence).map(|n| n.state.as_str())
    }

    /// Get the highest cached sequence number.
    pub fn last_sequence(&self) -> Option<u64> {
        self.last_sequence
    }

    /// Get the Merkle state at the last cached position.
    pub fn last_merkle_state(&self) -> Option<&str> {
        self.last_state.as_deref()
    }

    /// Check if the cache is empty (no prior sync).
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Number of cached entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Insert a new entry into the cache.
    pub fn insert(&mut self, sequence: u64, node: Node) {
        self.last_state = Some(node.state.clone());
        self.entries.insert(sequence, node);
        match self.last_sequence {
            Some(last) if sequence > last => self.last_sequence = Some(sequence),
            None => self.last_sequence = Some(sequence),
            _ => {}
        }
    }

    /// Remove all entries at or after a given sequence number.
    ///
    /// Used when the remote has diverged and we need to discard
    /// our stale cache entries.
    pub fn truncate_from(&mut self, from_sequence: u64) {
        self.entries.retain(|&seq, _| seq < from_sequence);
        self.last_sequence = self.entries.keys().copied().max();
        self.last_state = self
            .last_sequence
            .and_then(|s| self.entries.get(&s).map(|n| n.state.clone()));
    }

    /// Clear all cached state.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.last_sequence = None;
        self.last_state = None;
        self.last_tag_state = None;
    }

    /// Get all entries at or after a given sequence number.
    pub fn entries_from(&self, from_sequence: u64) -> Vec<(u64, &Node)> {
        let mut result: Vec<_> = self
            .entries
            .iter()
            .filter(|(&seq, _)| seq >= from_sequence)
            .map(|(&seq, node)| (seq, node))
            .collect();
        result.sort_by_key(|(seq, _)| *seq);
        result
    }

    /// Update the cache by replacing entries from the divergence point
    /// with new entries from the remote.
    pub fn update_from_changelist(&mut self, divergence: u64, entries: &[ChangelistEntry]) {
        // Remove stale entries at or after divergence
        self.truncate_from(divergence);

        // Insert new entries
        for entry in entries {
            let node = entry.to_node();
            self.insert(entry.sequence, node);
        }
    }
}

impl fmt::Display for RemoteState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "RemoteState({}, {} entries, last={})",
            if self.remote_name.is_empty() {
                "<unnamed>"
            } else {
                &self.remote_name
            },
            self.entries.len(),
            self.last_sequence
                .map(|s| s.to_string())
                .unwrap_or_else(|| "none".to_string()),
        )
    }
}

// Remote Delta

/// The difference between our cached view of a remote and its actual state.
///
/// This is the output of the dichotomy algorithm. It captures everything
/// needed to compute a `PushDelta` or `PullDelta`:
///
/// - **to_download**: Changes the remote has that we don't (need to pull)
/// - **ours_ge_dichotomy**: Our cached entries at or after the divergence point
/// - **theirs_ge_dichotomy**: The remote's actual entries at or after divergence
/// - **remote_unrecords**: Changes that were in our cache but are no longer
///   on the remote (they were unrecorded remotely)
///
/// For a push, we additionally compute `to_upload` by comparing our local
/// stack state against the remote's actual state.
#[derive(Debug, Clone)]
pub struct RemoteDelta {
    /// Changes that exist on the remote but not locally — need to download.
    pub to_download: Vec<Node>,

    /// Our cached entries at or after the divergence point.
    ///
    /// These represent what we *thought* the remote had. Comparing with
    /// `theirs_ge_dichotomy` reveals unrecords and new changes.
    pub ours_ge_dichotomy: Vec<(u64, Node)>,

    /// Set version of `ours_ge_dichotomy` for O(1) membership tests.
    pub ours_ge_dichotomy_set: HashSet<Node>,

    /// The remote's actual entries at or after the divergence point.
    pub theirs_ge_dichotomy: Vec<(u64, Node)>,

    /// Set version of `theirs_ge_dichotomy` for O(1) membership tests.
    pub theirs_ge_dichotomy_set: HashSet<Node>,

    /// Changes that were in our cache but have been unrecorded on the remote.
    ///
    /// These are changes that we knew the remote had (they're in our cache)
    /// but that no longer appear in the remote's changelist.
    pub remote_unrecords: Vec<(u64, Node)>,

    /// The sequence number where divergence was detected.
    pub divergence_point: u64,
}

impl RemoteDelta {
    /// Create an empty delta (everything is in sync).
    pub fn in_sync() -> Self {
        Self {
            to_download: Vec::new(),
            ours_ge_dichotomy: Vec::new(),
            ours_ge_dichotomy_set: HashSet::new(),
            theirs_ge_dichotomy: Vec::new(),
            theirs_ge_dichotomy_set: HashSet::new(),
            remote_unrecords: Vec::new(),
            divergence_point: 0,
        }
    }

    /// Convert this delta into a `PullDelta`.
    ///
    /// The pull delta contains:
    /// - `to_download`: Changes we need to fetch from the remote
    /// - `remote_state`: The remote's current state
    /// - `local_only`: Changes we have that the remote doesn't (informational)
    pub fn into_pull_delta(self, remote_state: Option<StateResponse>) -> PullDelta {
        let mut delta = PullDelta::new();
        delta.to_download = self
            .to_download
            .into_iter()
            .map(|n| crate::types::Node::change(&n.hash, &n.state))
            .collect();
        delta.remote_state = remote_state;
        delta
    }

    /// Convert this delta into a `PushDelta`.
    ///
    /// # Arguments
    ///
    /// * `local_changes` - All changes in the local stack that should be
    ///   considered for upload. The function filters out changes already
    ///   known to the remote.
    pub fn into_push_delta(self, local_changes: &[Node]) -> PushDelta {
        let mut delta = PushDelta::new();

        // Upload local changes that the remote doesn't have.
        // A change is "theirs" if it appears in theirs_ge_dichotomy_set.
        for node in local_changes {
            if !self.theirs_ge_dichotomy_set.contains(node) {
                delta
                    .to_upload
                    .push(crate::types::Node::change(&node.hash, &node.state));
            }
        }

        // Unknown changes: things in theirs_ge_dichotomy that aren't in
        // our cache (ours_ge_dichotomy) and aren't in our local stack.
        let local_set: HashSet<&str> = local_changes.iter().map(|n| n.hash.as_str()).collect();
        for (_, node) in &self.theirs_ge_dichotomy {
            if !self.ours_ge_dichotomy_set.contains(node) && !local_set.contains(node.hash.as_str())
            {
                delta.unknown_changes.push(node.hash.clone());
            }
        }

        // Remote unrecords
        for (_, node) in &self.remote_unrecords {
            delta.remote_unrecords.push(node.hash.clone());
        }

        delta
    }

    /// Check if everything is in sync (no changes to transfer).
    pub fn is_in_sync(&self) -> bool {
        self.to_download.is_empty()
            && self.ours_ge_dichotomy.is_empty()
            && self.theirs_ge_dichotomy.is_empty()
            && self.remote_unrecords.is_empty()
    }

    /// Total number of items that need to be transferred.
    pub fn transfer_count(&self) -> usize {
        self.to_download.len()
    }
}

impl fmt::Display for RemoteDelta {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "RemoteDelta(divergence={}, download={}, ours={}, theirs={}, unrecords={})",
            self.divergence_point,
            self.to_download.len(),
            self.ours_ge_dichotomy.len(),
            self.theirs_ge_dichotomy.len(),
            self.remote_unrecords.len(),
        )
    }
}

// Sync Engine

/// Statistics from a sync operation.
#[derive(Debug, Clone, Default)]
pub struct SyncStats {
    /// Number of remote round-trips for the dichotomy search.
    pub dichotomy_comparisons: u32,
    /// The divergence point found by the dichotomy.
    pub divergence_point: u64,
    /// Number of entries downloaded from the remote's changelist.
    pub changelist_entries_fetched: usize,
    /// Whether this was a from-scratch sync (no prior cache).
    pub from_scratch: bool,
}

impl fmt::Display for SyncStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.from_scratch {
            write!(
                f,
                "sync from scratch: fetched {} entries",
                self.changelist_entries_fetched,
            )
        } else {
            write!(
                f,
                "sync: {} comparisons, divergence at {}, fetched {} entries",
                self.dichotomy_comparisons, self.divergence_point, self.changelist_entries_fetched,
            )
        }
    }
}

/// The sync engine orchestrates the dichotomy algorithm and delta computation.
///
/// It takes a reference to the HTTP remote (for querying state) and a mutable
/// reference to the local cache (for reading/updating cached state).
pub struct SyncEngine<'a> {
    remote: &'a HttpRemote,
    cache: &'a mut RemoteState,
    stats: SyncStats,
}

impl<'a> SyncEngine<'a> {
    /// Create a new sync engine.
    pub fn new(remote: &'a HttpRemote, cache: &'a mut RemoteState) -> Self {
        Self {
            remote,
            cache,
            stats: SyncStats::default(),
        }
    }

    /// Get the stats from the last sync operation.
    pub fn stats(&self) -> &SyncStats {
        &self.stats
    }

    // Dichotomy Algorithm — O(log n) divergence detection

    /// Find the divergence point between our cached view and the remote's
    /// actual state using binary search on Merkle states.
    ///
    /// Returns the sequence number at which divergence begins. All entries
    /// before this point are identical between cache and remote.
    ///
    /// # Algorithm
    ///
    /// 1. Check if the last cached state matches the remote's current state.
    ///    If yes, we're already in sync — return `last + 1`.
    ///
    /// 2. Otherwise, binary search: compare the Merkle state at the midpoint.
    ///    - If states match, divergence is later → search upper half
    ///    - If states differ, divergence is earlier → search lower half
    ///
    /// 3. Return the first position where states differ.
    ///
    /// # Complexity
    ///
    /// O(log n) remote round-trips where n = number of cached entries.
    /// Each round-trip is a single `GET ?stack={s}&state=` request that
    /// returns three values: `(position, merkle, tag_merkle)`.
    async fn dichotomy_changelist(&mut self, stack: &str) -> RemoteResult<u64> {
        // If cache is empty, divergence is at the beginning
        let last_seq = match self.cache.last_sequence() {
            Some(seq) => seq,
            None => {
                debug!("dichotomy: cache is empty, starting from 0");
                return Ok(0);
            }
        };

        let last_state = match self.cache.last_merkle_state() {
            Some(s) => s.to_string(),
            None => return Ok(0),
        };

        debug!(
            "dichotomy: cache has {} entries, last_seq={}, last_state={}",
            self.cache.len(),
            last_seq,
            &last_state[..8.min(last_state.len())]
        );

        // Check if we're already in sync by comparing the last state
        let remote_state = self.remote.get_state(stack).await?;
        self.stats.dichotomy_comparisons += 1;

        match &remote_state {
            StateResponse::State { merkle, .. } if *merkle == last_state => {
                debug!("dichotomy: already in sync at seq {}", last_seq);
                return Ok(last_seq + 1);
            }
            StateResponse::Empty => {
                debug!("dichotomy: remote stack is empty");
                return Ok(0);
            }
            _ => {
                debug!("dichotomy: states differ, starting binary search");
            }
        }

        // Binary search for the divergence point
        let mut lo: u64 = 0;
        let mut hi: u64 = last_seq;

        while lo < hi {
            let mid = (lo + hi) / 2;

            // Get the Merkle state at the midpoint from our cache
            let cached_state = match self.cache.get_state(mid) {
                Some(s) => s.to_string(),
                None => {
                    // Gap in cache — we can't compare, assume divergence is here or earlier
                    debug!(
                        "dichotomy: gap in cache at {}, narrowing to [{}, {}]",
                        mid, lo, mid
                    );
                    hi = mid;
                    continue;
                }
            };

            // Query the remote for its state at this position
            // We use get_changelist with a narrow range to check the state
            let remote_entries = self.remote.get_changelist(stack, mid).await?;
            self.stats.dichotomy_comparisons += 1;

            // Find the entry at exactly `mid` in the response
            let remote_state_at_mid = remote_entries
                .iter()
                .find(|e| e.sequence == mid)
                .map(|e| e.merkle.as_str());

            match remote_state_at_mid {
                Some(remote_merkle) if remote_merkle == cached_state => {
                    // States match at midpoint — divergence is later
                    debug!(
                        "dichotomy: match at {}, searching [{}, {}]",
                        mid,
                        mid + 1,
                        hi
                    );
                    if lo == mid {
                        // Prevent infinite loop: lo and mid are the same
                        return Ok(lo + 1);
                    }
                    lo = mid;
                }
                _ => {
                    // States differ at midpoint — divergence is here or earlier
                    debug!(
                        "dichotomy: mismatch at {}, searching [{}, {}]",
                        mid, lo, mid
                    );
                    if hi == mid {
                        break;
                    }
                    hi = mid;
                }
            }
        }

        debug!("dichotomy: divergence at {}", lo);
        self.stats.divergence_point = lo;
        Ok(lo)
    }

    // Delta Computation

    /// Compute the full `RemoteDelta` for a given stack.
    ///
    /// This is the main entry point for sync operations. It:
    ///
    /// 1. Runs the dichotomy algorithm to find the divergence point
    /// 2. Downloads the remote's changelist from the divergence point
    /// 3. Compares "ours" (cached) vs "theirs" (actual) after divergence
    /// 4. Identifies changes to download, unrecords, and unknowns
    /// 5. Updates the local cache
    ///
    /// # Arguments
    ///
    /// * `stack` - The stack name to sync
    /// * `local_hashes` - Set of change hashes present in the local repository.
    ///   Used to determine which remote changes need to be downloaded.
    pub async fn compute_delta(
        &mut self,
        stack: &str,
        local_hashes: &HashSet<String>,
    ) -> RemoteResult<RemoteDelta> {
        // Handle from-scratch sync (no cache)
        if self.cache.is_empty() {
            return self.compute_delta_from_scratch(stack, local_hashes).await;
        }

        // Find divergence point
        let divergence = self.dichotomy_changelist(stack).await?;
        self.stats.divergence_point = divergence;

        // Collect our cached entries at or after divergence
        let ours_ge_dichotomy: Vec<(u64, Node)> = self
            .cache
            .entries_from(divergence)
            .into_iter()
            .map(|(seq, node)| (seq, node.clone()))
            .collect();

        let ours_ge_dichotomy_set: HashSet<Node> = ours_ge_dichotomy
            .iter()
            .map(|(_, node)| node.clone())
            .collect();

        // Download the remote's changelist from the divergence point
        let remote_entries = self.remote.get_changelist(stack, divergence).await?;
        self.stats.changelist_entries_fetched = remote_entries.len();

        // Build "theirs" sets
        let mut theirs_ge_dichotomy = Vec::new();
        let mut theirs_ge_dichotomy_set = HashSet::new();
        let mut to_download = Vec::new();

        for entry in &remote_entries {
            let node = entry.to_node();
            theirs_ge_dichotomy_set.insert(node.clone());
            theirs_ge_dichotomy.push((entry.sequence, node.clone()));

            // Need to download if we don't have it locally
            if !local_hashes.contains(&entry.hash) {
                to_download.push(node);
            }
        }

        // Compute remote unrecords: entries in our cache that are no longer
        // on the remote. These are changes that were unrecorded remotely.
        let remote_unrecords: Vec<(u64, Node)> = ours_ge_dichotomy
            .iter()
            .filter(|(_, node)| !theirs_ge_dichotomy_set.contains(node))
            .cloned()
            .collect();

        if !remote_unrecords.is_empty() {
            debug!("sync: {} remote unrecords detected", remote_unrecords.len());
        }

        // Update the cache: remove stale entries, add new ones
        self.cache
            .update_from_changelist(divergence, &remote_entries);

        Ok(RemoteDelta {
            to_download,
            ours_ge_dichotomy,
            ours_ge_dichotomy_set,
            theirs_ge_dichotomy,
            theirs_ge_dichotomy_set,
            remote_unrecords,
            divergence_point: divergence,
        })
    }

    /// Compute delta when we have no cached state (first sync).
    ///
    /// Downloads the entire changelist from position 0 and builds the
    /// cache from scratch.
    async fn compute_delta_from_scratch(
        &mut self,
        stack: &str,
        local_hashes: &HashSet<String>,
    ) -> RemoteResult<RemoteDelta> {
        debug!("sync: no cache, downloading full changelist");
        self.stats.from_scratch = true;

        let remote_entries = self.remote.get_changelist(stack, 0).await?;
        self.stats.changelist_entries_fetched = remote_entries.len();

        let mut theirs_ge_dichotomy = Vec::new();
        let mut theirs_ge_dichotomy_set = HashSet::new();
        let mut to_download = Vec::new();

        for entry in &remote_entries {
            let node = entry.to_node();
            theirs_ge_dichotomy_set.insert(node.clone());
            theirs_ge_dichotomy.push((entry.sequence, node.clone()));

            if !local_hashes.contains(&entry.hash) {
                to_download.push(node);
            }
        }

        // Seed the cache from the full changelist
        *self.cache = RemoteState::from_changelist(&remote_entries);

        Ok(RemoteDelta {
            to_download,
            ours_ge_dichotomy: Vec::new(),
            ours_ge_dichotomy_set: HashSet::new(),
            theirs_ge_dichotomy,
            theirs_ge_dichotomy_set,
            remote_unrecords: Vec::new(),
            divergence_point: 0,
        })
    }

    // Convenience: Pull and Push

    /// Compute a pull delta for a given stack.
    ///
    /// This is a convenience method that runs `compute_delta` and converts
    /// the result into a `PullDelta`.
    pub async fn compute_pull_delta(
        &mut self,
        stack: &str,
        local_hashes: &HashSet<String>,
    ) -> RemoteResult<PullDelta> {
        let remote_state = self.remote.get_state(stack).await?;
        let delta = self.compute_delta(stack, local_hashes).await?;
        Ok(delta.into_pull_delta(Some(remote_state)))
    }

    /// Compute a push delta for a given stack.
    ///
    /// # Arguments
    ///
    /// * `stack` - The stack to push to
    /// * `local_hashes` - Hashes present in the local repository
    /// * `local_changes` - All changes in the local stack (ordered)
    ///   that are candidates for upload
    pub async fn compute_push_delta(
        &mut self,
        stack: &str,
        local_hashes: &HashSet<String>,
        local_changes: &[Node],
    ) -> RemoteResult<PushDelta> {
        let delta = self.compute_delta(stack, local_hashes).await?;
        Ok(delta.into_push_delta(local_changes))
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    // RemoteState tests

    #[test]
    fn test_remote_state_empty() {
        let cache = RemoteState::empty();
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.last_sequence(), None);
        assert_eq!(cache.last_merkle_state(), None);
    }

    #[test]
    fn test_remote_state_named() {
        let cache = RemoteState::named("origin");
        assert!(cache.is_empty());
        assert_eq!(cache.remote_name, "origin");
    }

    #[test]
    fn test_remote_state_insert() {
        let mut cache = RemoteState::empty();
        let node = Node::change("ABC", "DEF");
        cache.insert(0, node.clone());

        assert!(!cache.is_empty());
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.last_sequence(), Some(0));
        assert_eq!(cache.get(0), Some(&node));
        assert_eq!(cache.get_state(0), Some("DEF"));
    }

    #[test]
    fn test_remote_state_insert_multiple() {
        let mut cache = RemoteState::empty();
        cache.insert(0, Node::change("A", "S0"));
        cache.insert(1, Node::change("B", "S1"));
        cache.insert(2, Node::change("C", "S2"));

        assert_eq!(cache.len(), 3);
        assert_eq!(cache.last_sequence(), Some(2));
        assert_eq!(cache.get_state(1), Some("S1"));
    }

    #[test]
    fn test_remote_state_insert_out_of_order() {
        let mut cache = RemoteState::empty();
        cache.insert(5, Node::change("E", "S5"));
        cache.insert(2, Node::change("B", "S2"));
        cache.insert(8, Node::change("H", "S8"));

        assert_eq!(cache.last_sequence(), Some(8));
        assert_eq!(cache.last_merkle_state(), Some("S8"));
    }

    #[test]
    fn test_remote_state_truncate_from() {
        let mut cache = RemoteState::empty();
        cache.insert(0, Node::change("A", "S0"));
        cache.insert(1, Node::change("B", "S1"));
        cache.insert(2, Node::change("C", "S2"));
        cache.insert(3, Node::change("D", "S3"));

        cache.truncate_from(2);

        assert_eq!(cache.len(), 2);
        assert_eq!(cache.last_sequence(), Some(1));
        assert!(cache.get(0).is_some());
        assert!(cache.get(1).is_some());
        assert!(cache.get(2).is_none());
        assert!(cache.get(3).is_none());
    }

    #[test]
    fn test_remote_state_truncate_from_zero() {
        let mut cache = RemoteState::empty();
        cache.insert(0, Node::change("A", "S0"));
        cache.insert(1, Node::change("B", "S1"));

        cache.truncate_from(0);

        assert!(cache.is_empty());
        assert_eq!(cache.last_sequence(), None);
    }

    #[test]
    fn test_remote_state_clear() {
        let mut cache = RemoteState::empty();
        cache.insert(0, Node::change("A", "S0"));
        cache.insert(1, Node::change("B", "S1"));

        cache.clear();

        assert!(cache.is_empty());
        assert_eq!(cache.last_sequence(), None);
        assert_eq!(cache.last_merkle_state(), None);
    }

    #[test]
    fn test_remote_state_entries_from() {
        let mut cache = RemoteState::empty();
        cache.insert(0, Node::change("A", "S0"));
        cache.insert(1, Node::change("B", "S1"));
        cache.insert(2, Node::change("C", "S2"));
        cache.insert(3, Node::change("D", "S3"));

        let from_2 = cache.entries_from(2);
        assert_eq!(from_2.len(), 2);
        assert_eq!(from_2[0].0, 2);
        assert_eq!(from_2[1].0, 3);
    }

    #[test]
    fn test_remote_state_entries_from_past_end() {
        let mut cache = RemoteState::empty();
        cache.insert(0, Node::change("A", "S0"));

        let result = cache.entries_from(5);
        assert!(result.is_empty());
    }

    #[test]
    fn test_remote_state_from_changelist() {
        let entries = vec![
            ChangelistEntry::new(0, "A", "S0", false),
            ChangelistEntry::new(1, "B", "S1", false),
            ChangelistEntry::new(2, "C", "S2", true),
        ];

        let cache = RemoteState::from_changelist(&entries);
        assert_eq!(cache.len(), 3);
        assert_eq!(cache.last_sequence(), Some(2));
        assert_eq!(cache.last_merkle_state(), Some("S2"));
        assert!(cache.get(2).unwrap().is_tag());
    }

    #[test]
    fn test_remote_state_update_from_changelist() {
        let mut cache = RemoteState::empty();
        cache.insert(0, Node::change("A", "S0"));
        cache.insert(1, Node::change("B", "S1"));
        cache.insert(2, Node::change("C", "S2"));

        // Remote diverged at position 2 with different changes
        let new_entries = vec![
            ChangelistEntry::new(2, "X", "SX", false),
            ChangelistEntry::new(3, "Y", "SY", false),
        ];

        cache.update_from_changelist(2, &new_entries);

        assert_eq!(cache.len(), 4); // 0, 1, 2(new), 3(new)
        assert_eq!(cache.get(0).unwrap().hash, "A");
        assert_eq!(cache.get(1).unwrap().hash, "B");
        assert_eq!(cache.get(2).unwrap().hash, "X"); // replaced
        assert_eq!(cache.get(3).unwrap().hash, "Y"); // new
    }

    #[test]
    fn test_remote_state_display() {
        let mut cache = RemoteState::named("origin");
        cache.insert(0, Node::change("A", "S0"));
        let display = format!("{}", cache);
        assert!(display.contains("origin"));
        assert!(display.contains("1 entries"));
    }

    // RemoteDelta tests

    #[test]
    fn test_remote_delta_in_sync() {
        let delta = RemoteDelta::in_sync();
        assert!(delta.is_in_sync());
        assert_eq!(delta.transfer_count(), 0);
    }

    #[test]
    fn test_remote_delta_with_downloads() {
        let mut delta = RemoteDelta::in_sync();
        delta.to_download.push(Node::change("A", "S0"));
        delta.to_download.push(Node::change("B", "S1"));

        assert!(!delta.is_in_sync());
        assert_eq!(delta.transfer_count(), 2);
    }

    #[test]
    fn test_remote_delta_into_pull_delta() {
        let mut delta = RemoteDelta::in_sync();
        delta.to_download.push(Node::change("A", "S0"));
        delta.to_download.push(Node::change("B", "S1"));

        let pull = delta.into_pull_delta(Some(StateResponse::empty()));
        assert_eq!(pull.download_count(), 2);
    }

    #[test]
    fn test_remote_delta_into_push_delta_filters_known() {
        let mut delta = RemoteDelta::in_sync();
        // The remote already has change "B"
        delta
            .theirs_ge_dichotomy_set
            .insert(Node::change("B", "SB"));
        delta.theirs_ge_dichotomy.push((1, Node::change("B", "SB")));

        // Local has changes A, B, C
        let local_changes = vec![
            Node::change("A", "SA"),
            Node::change("B", "SB"),
            Node::change("C", "SC"),
        ];

        let push = delta.into_push_delta(&local_changes);

        // Should upload A and C but NOT B (remote already has it)
        assert_eq!(push.upload_count(), 2);
        let upload_hashes: Vec<&str> = push.to_upload.iter().map(|n| n.hash.as_str()).collect();
        assert!(upload_hashes.contains(&"A"));
        assert!(!upload_hashes.contains(&"B"));
        assert!(upload_hashes.contains(&"C"));
    }

    #[test]
    fn test_remote_delta_into_push_delta_detects_unrecords() {
        let mut delta = RemoteDelta::in_sync();
        delta
            .remote_unrecords
            .push((5, Node::change("GONE", "S_GONE")));

        let push = delta.into_push_delta(&[]);
        assert!(push.has_remote_unrecords());
        assert_eq!(push.remote_unrecords.len(), 1);
        assert_eq!(push.remote_unrecords[0], "GONE");
    }

    #[test]
    fn test_remote_delta_into_push_delta_detects_unknown() {
        let mut delta = RemoteDelta::in_sync();
        // Remote has a change we've never seen
        let unknown = Node::change("SURPRISE", "S_UNK");
        delta.theirs_ge_dichotomy_set.insert(unknown.clone());
        delta.theirs_ge_dichotomy.push((10, unknown));

        let push = delta.into_push_delta(&[]);
        assert!(push.has_unknown_changes());
        assert_eq!(push.unknown_changes.len(), 1);
        assert_eq!(push.unknown_changes[0], "SURPRISE");
    }

    #[test]
    fn test_remote_delta_display() {
        let mut delta = RemoteDelta::in_sync();
        delta.divergence_point = 42;
        delta.to_download.push(Node::change("A", "SA"));
        let display = format!("{}", delta);
        assert!(display.contains("divergence=42"));
        assert!(display.contains("download=1"));
    }

    // SyncStats tests

    #[test]
    fn test_sync_stats_default() {
        let stats = SyncStats::default();
        assert_eq!(stats.dichotomy_comparisons, 0);
        assert_eq!(stats.divergence_point, 0);
        assert_eq!(stats.changelist_entries_fetched, 0);
        assert!(!stats.from_scratch);
    }

    #[test]
    fn test_sync_stats_display_from_scratch() {
        let stats = SyncStats {
            from_scratch: true,
            changelist_entries_fetched: 50,
            ..Default::default()
        };
        let display = format!("{}", stats);
        assert!(display.contains("from scratch"));
        assert!(display.contains("50"));
    }

    #[test]
    fn test_sync_stats_display_incremental() {
        let stats = SyncStats {
            dichotomy_comparisons: 5,
            divergence_point: 42,
            changelist_entries_fetched: 10,
            from_scratch: false,
        };
        let display = format!("{}", stats);
        assert!(display.contains("5 comparisons"));
        assert!(display.contains("divergence at 42"));
        assert!(display.contains("10 entries"));
    }

    // Node helper tests (verifying Hash/Eq for HashSet usage)

    #[test]
    fn test_node_equality_in_hashset() {
        let mut set = HashSet::new();
        set.insert(Node::change("ABC", "S1"));
        set.insert(Node::change("DEF", "S2"));
        set.insert(Node::change("ABC", "S1")); // duplicate

        assert_eq!(set.len(), 2);
        assert!(set.contains(&Node::change("ABC", "S1")));
        assert!(!set.contains(&Node::change("GHI", "S3")));
    }

    #[test]
    fn test_node_tag_vs_change_in_hashset() {
        let mut set = HashSet::new();
        set.insert(Node::change("ABC", "S1"));
        set.insert(Node::tag("ABC", "S1"));

        // Tag and change with same hash should be different nodes
        assert_eq!(set.len(), 2);
    }
}
