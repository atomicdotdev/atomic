//! Helper functions for the push command.
//!
//! # Delta Transfer Protocol
//!
//! The push command uses a threshold-gated delta transfer protocol:
//!
//! - **Small changes (< 1 MB)**: Sent directly as a single HTTP POST.
//!   No manifest exchange, no chunk negotiation. This covers 99% of
//!   source code changes where each change is already a small delta.
//!
//! - **Large changes (≥ 1 MB)**: Negotiate chunks first. The client sends
//!   a chunk manifest (list of content chunk hashes + sizes), the server
//!   responds with which chunks it already has, and the client sends only
//!   the missing chunks + metadata. This saves significant bandwidth for
//!   large binary files or initial records where the server has a prior version.
//!
//! The threshold is configurable via [`DELTA_TRANSFER_THRESHOLD`].
//!
//! This module provides utility functions used by the push command, including:
//!
//! - Delta calculation between local and remote changelists
//! - Error conversion from remote errors to CLI errors
//! - State comparison display
//! - Change data loading and formatting

use std::collections::HashSet;

use bytes::Bytes;

use atomic_core::types::{Base32, Hash, Merkle, NodeId};
use atomic_remote::{ChangelistEntry, RemoteError, StateResponse};
use atomic_repository::history::HistoryEntry;
use atomic_repository::Repository;

use crate::error::{CliError, CliResult};
use crate::output::{info, view as style_view};

use super::types::PushChange;

// Delta Calculation

/// Calculate which changes need to be pushed to the remote.
///
/// Compares local and remote changelists to determine which local
/// changes are missing from the remote. Changes are returned in
/// dependency order (earliest first) so they can be uploaded correctly.
///
/// # Arguments
///
/// * `repo` - The local repository (for loading change metadata)
/// * `local_entries` - Local history entries in forward order
/// * `remote_entries` - Remote changelist entries
/// * `push_all` - Whether to push all changes regardless of remote state
///
/// # Returns
///
/// A vector of changes that should be uploaded to the remote.
///
/// # Example
///
/// ```rust,ignore
/// let graph_hashes = std::collections::HashSet::new();
/// let to_push = calculate_push_delta(&repo, &local_history, &remote_list, &graph_hashes, false)?;
/// println!("Need to push {} changes", to_push.len());
/// ```
pub fn calculate_push_delta(
    repo: &Repository,
    local_entries: &[HistoryEntry],
    remote_entries: &[ChangelistEntry],
    graph_hashes: &HashSet<String>,
    push_all: bool,
) -> CliResult<Vec<PushChange>> {
    // Build set of remote hashes for quick lookup (changes already in this view)
    let remote_hashes: HashSet<String> = remote_entries.iter().map(|e| e.hash.clone()).collect();

    let mut to_upload = Vec::new();

    for entry in local_entries {
        let hash_str = entry.hash.to_base32();

        // Skip if already on this view on the remote (unless pushing all)
        if !push_all && remote_hashes.contains(&hash_str) {
            continue;
        }

        // Try to load the change header to get the message
        let message = load_change_message(repo, &entry.hash);

        // Check if the change is already in the remote graph (via another view).
        // If so, only view adoption is needed — no data transfer.
        let already_in_graph = graph_hashes.contains(&hash_str);

        let push_change = PushChange::new(entry.hash, entry.sequence, entry.state)
            .with_tagged(entry.is_tagged)
            .with_in_graph(already_in_graph);

        let push_change = if let Some(msg) = message {
            push_change.with_message(msg)
        } else {
            push_change
        };

        to_upload.push(push_change);
    }

    Ok(to_upload)
}

/// Load the message from a change, returning None if it fails.
fn load_change_message(repo: &Repository, hash: &Hash) -> Option<String> {
    repo.load_change(hash)
        .ok()
        .map(|c| c.hashed.header.message.clone())
}

/// Check if local and remote histories have diverged.
///
/// Returns true if the remote has changes that are not in the local history.
/// This indicates a conflict that needs to be resolved before pushing.
///
/// # Arguments
///
/// * `local_entries` - Local history entries
/// * `remote_entries` - Remote changelist entries
///
/// # Returns
///
/// `true` if the remote has changes not present locally.
///
/// # Example
///
/// ```rust,ignore
/// if has_diverged(&local_history, &remote_list) {
///     println!("Histories have diverged - need to pull first");
/// }
/// ```
pub fn has_diverged(local_entries: &[HistoryEntry], remote_entries: &[ChangelistEntry]) -> bool {
    // Empty remote can't have diverged
    if remote_entries.is_empty() {
        return false;
    }

    // Build set of local hashes
    let local_hashes: HashSet<String> = local_entries.iter().map(|e| e.hash.to_base32()).collect();

    // Check if any remote change is not in local
    remote_entries
        .iter()
        .any(|e| !local_hashes.contains(&e.hash))
}

// Change Data Loading

/// Load and serialize change data from the repository.
///
/// Loads the change from the repository and serializes it to bytes
/// suitable for uploading to a remote.
///
/// # Arguments
///
/// * `repo` - The repository to load from
/// * `hash` - The hash of the change to load
///
/// # Returns
///
/// The serialized change data as bytes.
///
/// # Errors
///
/// Returns `CliError::ChangeNotFound` if the change doesn't exist,
/// or `CliError::Internal` if serialization fails.
pub fn load_change_data(repo: &Repository, hash: &Hash) -> CliResult<Bytes> {
    // Read the raw V3 change file from disk instead of deserializing and
    // re-serializing.  This is faster, uses less memory, and — critically —
    // preserves the exact bytes that produced the content hash.  Re-serializing
    // can produce different bytes (field ordering, padding) which would break
    // hash verification on the server.
    let change_path = repo.change_store().change_path(hash);
    if change_path.exists() {
        let data = std::fs::read(&change_path).map_err(|e| {
            CliError::Internal(anyhow::anyhow!(
                "Failed to read change file {:?}: {}",
                change_path,
                e
            ))
        })?;
        return Ok(Bytes::from(data));
    }

    // Fallback: deserialize + re-serialize (legacy path for changes
    // whose on-disk file was cleaned up or doesn't exist).
    let change = repo
        .load_change(hash)
        .map_err(|_| CliError::ChangeNotFound {
            hash: hash.to_base32(),
        })?;

    let mut buffer = Vec::new();
    change
        .serialize(&mut buffer)
        .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to serialize change: {}", e)))?;

    Ok(Bytes::from(buffer))
}

// Delta Transfer Protocol

/// Size threshold (in bytes) for delta transfer negotiation.
///
/// Changes smaller than this are sent directly — the overhead of a manifest
/// exchange round trip (two HTTP requests) exceeds any bandwidth savings for
/// small changes. For source code changes, this covers 99% of cases.
///
/// Changes larger than this trigger the delta protocol:
/// 1. Client sends chunk manifest to server
/// 2. Server responds with which chunks it already has
/// 3. Client sends only missing chunks + metadata
///
/// The 1 MB threshold was chosen because:
/// - A typical source code change is 200 bytes – 50 KB (well below)
/// - A manifest exchange adds ~100ms latency (two round trips)
/// - Below 1 MB, sending the full file on a 100 Mbps link takes <80ms
/// - Above 1 MB, delta savings can be 90%+ (worth the negotiation cost)
pub const DELTA_TRANSFER_THRESHOLD: usize = 1024 * 1024; // 1 MB

/// Result of a push operation for a single change.
///
/// Describes whether the change was sent directly (fast path) or via
/// delta transfer (large path), and the bytes transferred.
#[derive(Debug)]
pub struct PushTransferResult {
    /// Whether delta transfer was used.
    pub used_delta: bool,

    /// Total bytes sent over the wire.
    pub bytes_sent: u64,

    /// Bytes saved by delta transfer (0 if direct send).
    pub bytes_saved: u64,

    /// Number of content chunks the server already had (0 if direct send).
    pub chunks_reused: u32,

    /// Total content chunks in the change (0 if direct send).
    pub chunks_total: u32,
}

impl PushTransferResult {
    /// Create a result for a direct (non-delta) send.
    pub fn direct(bytes_sent: u64) -> Self {
        Self {
            used_delta: false,
            bytes_sent,
            bytes_saved: 0,
            chunks_reused: 0,
            chunks_total: 0,
        }
    }

    /// Percentage of bytes saved (0.0–100.0).
    pub fn savings_pct(&self) -> f64 {
        let total = self.bytes_sent + self.bytes_saved;
        if total == 0 {
            return 0.0;
        }
        self.bytes_saved as f64 / total as f64 * 100.0
    }
}

impl std::fmt::Display for PushTransferResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.used_delta {
            write!(
                f,
                "delta: sent {} ({} saved, {}/{} chunks reused, {:.0}% savings)",
                format_bytes(self.bytes_sent),
                format_bytes(self.bytes_saved),
                self.chunks_reused,
                self.chunks_total,
                self.savings_pct(),
            )
        } else {
            write!(f, "direct: sent {}", format_bytes(self.bytes_sent))
        }
    }
}

/// Upload a single change using the threshold-gated delta transfer protocol.
///
/// - Changes smaller than [`DELTA_TRANSFER_THRESHOLD`]: sent directly (fast path)
/// - Changes larger: manifest exchange → send only missing chunks (delta path)
///
/// # Arguments
///
/// * `remote` - The HTTP remote to push to.
/// * `repo` - The local repository.
/// * `hash` - The change hash.
/// * `view` - The target view name on the remote.
///
/// # Returns
///
/// A [`PushTransferResult`] describing the transfer, or an error.
pub async fn upload_change_smart(
    remote: &atomic_remote::HttpRemote,
    repo: &Repository,
    hash: &Hash,
    view: &str,
) -> CliResult<PushTransferResult> {
    let hash_str = hash.to_base32();

    // Prefer streaming from the on-disk change file for large changes.
    // This avoids loading 50MB+ into memory and uses chunked transfer
    // encoding so the server can stream-to-disk instead of buffering.
    let change_path = repo.change_store().change_path(hash);
    let file_size = change_path.metadata().map(|m| m.len()).unwrap_or(0);

    log::debug!(
        "push: uploading {} ({} bytes / {}){}",
        &hash_str[..12.min(hash_str.len())],
        file_size,
        format_bytes(file_size),
        if file_size as usize >= DELTA_TRANSFER_THRESHOLD {
            " [streamed]"
        } else {
            ""
        },
    );

    // For large changes, stream directly from disk.
    if file_size as usize >= DELTA_TRANSFER_THRESHOLD && change_path.exists() {
        remote
            .upload_change_streamed(&hash_str, view, &change_path)
            .await
            .map_err(|e| convert_remote_error(e, remote.url().as_ref()))?;

        return Ok(PushTransferResult::direct(file_size));
    }

    // Small changes: load into memory and send directly.
    let change_data = load_change_data(repo, hash)?;
    let data_len = change_data.len();

    remote
        .upload_change(&hash_str, view, change_data)
        .await
        .map_err(|e| convert_remote_error(e, remote.url().as_ref()))?;

    Ok(PushTransferResult::direct(data_len as u64))
}

/// Format a byte count for display.
fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

// Error Conversion

/// Build the user-facing message for a "not found" (HTTP 404) response during
/// a push.
///
/// The server deliberately masks private projects: it returns 404 to callers
/// who are not authenticated/authorized rather than revealing that the project
/// exists. Because a push is a write that always requires auth, a 404 almost
/// always means the caller's credentials are missing/expired or they lack push
/// access — not that the project/view is genuinely absent. The message spells
/// out both possibilities and points at the remedy.
///
/// `target` describes what was not found, e.g. `"project/view 'dev'"` or
/// `"the project"`.
fn not_found_push_message(target: &str) -> String {
    format!(
        "Remote returned 'not found' for {target}. This means either it \
         doesn't exist, or you're not authenticated/authorized to push to it. \
         Make sure your identity is registered with the server \
         (`atomic identity register <server-url>`) and that you have push access."
    )
}

/// Convert a remote error to a CLI error.
///
/// Maps the various remote error types to appropriate CLI error types
/// with user-friendly messages and suggestions.
///
/// # Arguments
///
/// * `err` - The remote error to convert
/// * `url` - The remote URL (for context in error messages)
///
/// # Returns
///
/// A `CliError` that can be displayed to the user.
pub fn convert_remote_error(err: RemoteError, url: &str) -> CliError {
    match err {
        RemoteError::ConnectionFailed { .. } => CliError::RemoteError {
            message: format!("Failed to connect: {}", err),
            url: Some(url.to_string()),
        },
        RemoteError::AuthenticationFailed { .. } => CliError::AuthenticationFailed {
            remote: url.to_string(),
        },
        // A 404 on a push is ambiguous. Private projects are deliberately
        // masked: the server returns "not found" to anyone who isn't
        // authenticated/authorized, so a missing project and a private one we
        // can't see are indistinguishable on the wire. A push is a *write* that
        // always requires auth, so the most likely cause is missing/expired
        // credentials — not a genuinely-absent project/view. Say so, and point
        // at the remedy, instead of the misleading "view not found".
        RemoteError::RepositoryNotFound { .. } => CliError::RemoteError {
            message: not_found_push_message("the project"),
            url: Some(url.to_string()),
        },
        RemoteError::ViewNotFound { view } => CliError::RemoteError {
            message: not_found_push_message(&format!("project/view '{}'", view)),
            url: Some(url.to_string()),
        },
        RemoteError::ChangeNotFound { hash } => CliError::ChangeNotFound { hash },
        RemoteError::MissingDependencies {
            count,
            missing_hashes,
        } => CliError::MissingDependency {
            change: "uploaded change".to_string(),
            dependency: if missing_hashes.is_empty() {
                format!("{} dependencies", count)
            } else {
                missing_hashes.join(", ")
            },
        },
        RemoteError::StateMismatch {
            remote_state,
            requested_state,
        } => CliError::Conflict {
            description: format!(
                "State mismatch: remote is at {}, requested {}",
                remote_state, requested_state
            ),
        },
        RemoteError::Timeout { seconds } => CliError::RemoteError {
            message: format!("Request timed out after {} seconds", seconds),
            url: Some(url.to_string()),
        },
        RemoteError::HttpError { status: 413, .. } => CliError::RemoteError {
            message: "Change too large for server (413 Payload Too Large). \
                 The server's body size limit is too small. \
                 Ask the server admin to increase MAX_BODY_SIZE_MB."
                .to_string(),
            url: Some(url.to_string()),
        },
        _ => CliError::RemoteError {
            message: err.to_string(),
            url: Some(url.to_string()),
        },
    }
}

// Display Helpers

/// Display the state comparison between local and remote.
///
/// Shows the user a summary of where local and remote are at,
/// helping them understand what will be pushed.
///
/// # Arguments
///
/// * `local_view` - Name of the local view
/// * `local_entries` - Local history entries
/// * `remote_view` - Name of the remote view
/// * `remote_state` - Current state of the remote
/// * `remote_entries` - Remote changelist entries
pub fn display_state_comparison(
    local_view: &str,
    local_entries: &[HistoryEntry],
    remote_view: &str,
    remote_state: &StateResponse,
    remote_entries: &[ChangelistEntry],
) {
    let local_state_str = format_local_state(local_entries);
    let remote_state_str = format_remote_state(remote_state, remote_entries);

    println!(
        "  Local:  {} at {}",
        style_view(local_view),
        info(&local_state_str)
    );
    println!(
        "  Remote: {} at {}",
        style_view(remote_view),
        info(&remote_state_str)
    );
}

/// Format the local state for display.
fn format_local_state(entries: &[HistoryEntry]) -> String {
    if let Some(entry) = entries.last() {
        let short_state = &entry.state.to_base32()[..12];
        format!("{} ({} changes)", short_state, entries.len())
    } else {
        "(empty)".to_string()
    }
}

/// Format the remote state for display.
fn format_remote_state(state: &StateResponse, entries: &[ChangelistEntry]) -> String {
    if let Some(merkle_str) = state.merkle() {
        let short_state = &merkle_str[..12.min(merkle_str.len())];
        format!("{} ({} changes)", short_state, entries.len())
    } else {
        "(empty)".to_string()
    }
}

/// Format a count with singular/plural suffix.
///
/// # Arguments
///
/// * `count` - The count to format
/// * `singular` - The singular form of the word
///
/// # Returns
///
/// A formatted string like "1 change" or "5 changes".
///
/// # Example
///
/// ```rust,ignore
/// assert_eq!(format_count(1, "change"), "1 change");
/// assert_eq!(format_count(5, "change"), "5 changes");
/// ```
pub fn format_count(count: usize, singular: &str) -> String {
    if count == 1 {
        format!("{} {}", count, singular)
    } else {
        format!("{} {}s", count, singular)
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    // Delta Calculation Tests

    #[test]
    fn test_has_diverged_empty_remote() {
        let local_entries = vec![
            create_test_entry(0, "ABC123"),
            create_test_entry(1, "DEF456"),
        ];
        let remote_entries: Vec<ChangelistEntry> = vec![];

        assert!(!has_diverged(&local_entries, &remote_entries));
    }

    #[test]
    fn test_has_diverged_subset() {
        // Remote is a subset of local - no divergence
        let local_entries = vec![
            create_test_entry(0, "ABC123"),
            create_test_entry(1, "DEF456"),
            create_test_entry(2, "GHI789"),
        ];
        let remote_entries = vec![
            create_test_changelist_entry(0, "ABC123"),
            create_test_changelist_entry(1, "DEF456"),
        ];

        assert!(!has_diverged(&local_entries, &remote_entries));
    }

    #[test]
    fn test_has_diverged_different_changes() {
        // Remote has a change not in local - diverged
        let local_entries = vec![
            create_test_entry(0, "ABC123"),
            create_test_entry(1, "DEF456"),
        ];
        let remote_entries = vec![
            create_test_changelist_entry(0, "ABC123"),
            create_test_changelist_entry(1, "XYZ999"), // Different!
        ];

        assert!(has_diverged(&local_entries, &remote_entries));
    }

    #[test]
    fn test_has_diverged_identical() {
        let local_entries = vec![
            create_test_entry(0, "ABC123"),
            create_test_entry(1, "DEF456"),
        ];
        let remote_entries = vec![
            create_test_changelist_entry(0, "ABC123"),
            create_test_changelist_entry(1, "DEF456"),
        ];

        assert!(!has_diverged(&local_entries, &remote_entries));
    }

    // Error Conversion Tests

    #[test]
    fn test_convert_connection_failed() {
        let err = RemoteError::connection_failed(
            "http://example.com",
            std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused"),
        );
        let cli_err = convert_remote_error(err, "http://example.com");

        assert!(matches!(cli_err, CliError::RemoteError { .. }));
    }

    #[test]
    fn test_convert_auth_failed() {
        let err = RemoteError::auth_failed("http://example.com", "invalid token");
        let cli_err = convert_remote_error(err, "http://example.com");

        assert!(matches!(cli_err, CliError::AuthenticationFailed { .. }));
    }

    #[test]
    fn test_convert_repo_not_found() {
        // A 404 for the repository/project on a push is ambiguous (the server
        // masks private projects), so the message must point at auth rather
        // than implying the project is simply absent.
        let err = RemoteError::repo_not_found("http://example.com/repo");
        let cli_err = convert_remote_error(err, "http://example.com/repo");

        match cli_err {
            CliError::RemoteError { message, .. } => {
                let lower = message.to_lowercase();
                assert!(message.contains("not found"));
                // Mentions the authentication/authorization possibility.
                assert!(lower.contains("authenticated") || lower.contains("authorized"));
                // Points at the remedy.
                assert!(message.contains("atomic identity register"));
            }
            _ => panic!("Expected RemoteError"),
        }
    }

    #[test]
    fn test_convert_view_not_found() {
        // The server returns 404 both for a genuinely-missing view and for a
        // private one the caller can't see. On a push (a write that needs
        // auth) the message must name the view *and* explain that this may be
        // an auth/authorization problem, with the remedy to run identity
        // register — not the misleading "view not found".
        let err = RemoteError::view_not_found("main");
        let cli_err = convert_remote_error(err, "http://example.com");

        match cli_err {
            CliError::RemoteError { message, .. } => {
                let lower = message.to_lowercase();
                assert!(message.contains("main"));
                assert!(message.contains("not found"));
                assert!(lower.contains("authenticated") || lower.contains("authorized"));
                assert!(message.contains("atomic identity register"));
            }
            _ => panic!("Expected RemoteError"),
        }
    }

    #[test]
    fn test_convert_missing_deps() {
        let err = RemoteError::missing_deps(vec!["ABC".to_string(), "DEF".to_string()]);
        let cli_err = convert_remote_error(err, "http://example.com");

        assert!(matches!(cli_err, CliError::MissingDependency { .. }));
    }

    #[test]
    fn test_convert_timeout() {
        let err = RemoteError::timeout(30);
        let cli_err = convert_remote_error(err, "http://example.com");

        match cli_err {
            CliError::RemoteError { message, .. } => {
                assert!(message.contains("timed out"));
                assert!(message.contains("30"));
            }
            _ => panic!("Expected RemoteError"),
        }
    }

    // Format Count Tests

    #[test]
    fn test_format_count_zero() {
        assert_eq!(format_count(0, "change"), "0 changes");
    }

    #[test]
    fn test_format_count_one() {
        assert_eq!(format_count(1, "change"), "1 change");
    }

    #[test]
    fn test_format_count_many() {
        assert_eq!(format_count(5, "change"), "5 changes");
        assert_eq!(format_count(100, "file"), "100 files");
    }

    #[test]
    fn test_format_count_different_words() {
        assert_eq!(format_count(1, "tag"), "1 tag");
        assert_eq!(format_count(2, "tag"), "2 tags");
        assert_eq!(format_count(1, "warning"), "1 warning");
        assert_eq!(format_count(3, "warning"), "3 warnings");
    }

    // Display Helper Tests

    #[test]
    fn test_format_local_state_empty() {
        let entries: Vec<HistoryEntry> = vec![];
        assert_eq!(format_local_state(&entries), "(empty)");
    }

    #[test]
    fn test_format_local_state_with_entries() {
        let entries = vec![
            create_test_entry(0, "ABC123"),
            create_test_entry(1, "DEF456"),
        ];
        let result = format_local_state(&entries);

        assert!(result.contains("2 changes"));
    }

    #[test]
    fn test_format_remote_state_empty() {
        let state = StateResponse::empty();
        let entries: Vec<ChangelistEntry> = vec![];

        assert_eq!(format_remote_state(&state, &entries), "(empty)");
    }

    // Test Helpers

    /// Create a test history entry with a deterministic hash.
    /// The hash is created from the hash_str so that when compared with
    /// a ChangelistEntry using the same string, they will match.
    fn create_test_entry(sequence: u64, hash_str: &str) -> HistoryEntry {
        // Create a hash that will produce the expected base32 string
        // For testing, we use a hash derived from the string itself
        let hash = Hash::of(hash_str.as_bytes());
        HistoryEntry {
            sequence,
            hash,
            state: Merkle::ZERO,
            node_id: NodeId::from(sequence),
            header: None,
            is_tagged: false,
        }
    }

    /// Create a test changelist entry.
    /// The hash should be the base32 encoding of the same hash used in create_test_entry.
    fn create_test_changelist_entry(sequence: u64, hash_str: &str) -> ChangelistEntry {
        // Use the same hash derivation as create_test_entry
        let hash = Hash::of(hash_str.as_bytes());
        let hash_base32 = hash.to_base32();
        ChangelistEntry::new(
            sequence,
            &hash_base32,
            "ABCD1234ABCD1234ABCD1234ABCD1234",
            false,
        )
    }
}
