//! Helper functions for the pull command.
//!
//! This module provides utility functions used by the pull command, including:
//!
//! - Delta calculation between remote and local changelists
//! - Error conversion from remote errors to CLI errors
//! - State comparison display formatting
//! - Change data saving and loading
//!
//! # Overview
//!
//! The pull command needs to:
//!
//! 1. Compare local and remote changelists to determine what to download
//! 2. Detect diverged history (local-only changes)
//! 3. Download and save changes to the local repository
//! 4. Display progress and state information to the user
//!
//! This module provides the helper functions that support these operations.

use std::collections::HashSet;
use std::io::Cursor;

use bytes::Bytes;

use atomic_core::change::Change;
use atomic_core::types::{Base32, Hash, Merkle};
use atomic_remote::{ChangelistEntry, RemoteError, StateResponse};
use atomic_repository::history::HistoryEntry;
use atomic_repository::Repository;

use crate::error::{CliError, CliResult};
use crate::output::{hint, info, stack as style_stack};

use super::types::PullChange;

// Delta Calculation

/// Calculate which changes need to be pulled from the remote.
///
/// Compares remote and local changelists to determine which remote
/// changes are missing locally. Changes are returned in dependency
/// order (earliest first) so they can be downloaded and applied correctly.
///
/// # Arguments
///
/// * `remote_entries` - Remote changelist entries (in sequence order)
/// * `local_entries` - Local history entries
/// * `pull_all` - Whether to pull all changes regardless of local state
///
/// # Returns
///
/// A vector of changes that should be downloaded from the remote.
///
/// # Algorithm
///
/// 1. Build a hash set of all local change hashes for O(1) lookup
/// 2. Iterate through remote entries in sequence order
/// 3. Include any change whose hash is not in the local set
/// 4. If `pull_all` is true, include all remote changes regardless
///
/// # Example
///
/// ```rust,ignore
/// let to_pull = calculate_pull_delta(&remote_list, &local_history, false)?;
/// println!("Need to pull {} changes", to_pull.len());
/// ```
pub fn calculate_pull_delta(
    remote_entries: &[ChangelistEntry],
    local_entries: &[HistoryEntry],
    pull_all: bool,
) -> Vec<PullChange> {
    // Build set of local hashes for quick lookup
    let local_hashes: HashSet<String> = local_entries.iter().map(|e| e.hash.to_base32()).collect();

    let mut to_download = Vec::new();

    for entry in remote_entries {
        // Skip if already have this change locally (unless pulling all)
        if !pull_all && local_hashes.contains(&entry.hash) {
            continue;
        }

        // Parse the hash from base32
        let hash = match Hash::from_base32(entry.hash.as_bytes()) {
            Some(h) => h,
            None => continue, // Skip invalid hashes
        };

        // Parse the merkle state from base32
        let state = match Merkle::from_base32(entry.merkle.as_bytes()) {
            Some(m) => m,
            None => continue, // Skip invalid merkle states
        };

        let pull_change = PullChange::new(hash, entry.sequence, state).with_tagged(entry.tagged);

        to_download.push(pull_change);
    }

    to_download
}

/// Check if there are local-only changes (changes not on the remote).
///
/// Returns true if the local repository has changes that are not present
/// on the remote. This indicates a potential divergence in history.
///
/// # Arguments
///
/// * `local_entries` - Local history entries
/// * `remote_entries` - Remote changelist entries
///
/// # Returns
///
/// `true` if there are local changes not present on the remote.
///
/// # Example
///
/// ```rust,ignore
/// if has_local_only_changes(&local_history, &remote_list) {
///     println!("Warning: You have local changes not on the remote");
/// }
/// ```
pub fn has_local_only_changes(
    local_entries: &[HistoryEntry],
    remote_entries: &[ChangelistEntry],
) -> bool {
    // Empty local means no local-only changes
    if local_entries.is_empty() {
        return false;
    }

    // Build set of remote hashes
    let remote_hashes: HashSet<String> = remote_entries.iter().map(|e| e.hash.clone()).collect();

    // Check if any local change is not on remote
    local_entries
        .iter()
        .any(|e| !remote_hashes.contains(&e.hash.to_base32()))
}

/// Find all local-only change hashes.
///
/// Returns the base32-encoded hashes of all changes that exist locally
/// but not on the remote.
///
/// # Arguments
///
/// * `local_entries` - Local history entries
/// * `remote_entries` - Remote changelist entries
///
/// # Returns
///
/// A vector of base32-encoded hashes of local-only changes.
pub fn find_local_only_changes(
    local_entries: &[HistoryEntry],
    remote_entries: &[ChangelistEntry],
) -> Vec<String> {
    let remote_hashes: HashSet<String> = remote_entries.iter().map(|e| e.hash.clone()).collect();

    local_entries
        .iter()
        .filter(|e| !remote_hashes.contains(&e.hash.to_base32()))
        .map(|e| e.hash.to_base32())
        .collect()
}

// Change Data Operations

/// Save a downloaded change to the repository's change store.
///
/// Writes the change data to the appropriate location in the repository's
/// `.atomic/changes/` directory structure.
///
/// # Arguments
///
/// * `repo` - The repository to save to
/// * `hash` - The expected hash of the change (for verification)
/// * `data` - The raw change file data
///
/// # Returns
///
/// `Ok(())` on success, or an appropriate error.
///
/// # Errors
///
/// - `CliError::Internal` if the hash doesn't match the data
/// - `CliError::Internal` if the save fails for any reason
///
/// # Example
///
/// ```rust,ignore
/// let data = remote.download_change(&hash_str).await?;
/// save_downloaded_change(&repo, &expected_hash, data)?;
/// ```
pub fn save_downloaded_change(repo: &Repository, hash: &Hash, data: Bytes) -> CliResult<()> {
    // Deserialize the change from bytes
    let mut cursor = Cursor::new(&data[..]);
    let (change, computed_hash) = Change::deserialize(&mut cursor)
        .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to deserialize change: {}", e)))?;

    // Verify the hash matches what we expected
    if computed_hash != *hash {
        return Err(CliError::Internal(anyhow::anyhow!(
            "Hash mismatch: expected {}, got {}",
            hash.to_base32(),
            computed_hash.to_base32()
        )));
    }

    // Save to the change store
    repo.save_change(&change)
        .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to save change: {}", e)))?;

    Ok(())
}

// Error Conversion

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
///
/// # Example
///
/// ```rust,ignore
/// let result = remote.get_state("main").await;
/// if let Err(e) = result {
///     return Err(convert_remote_error(e, &remote_url));
/// }
/// ```
pub fn convert_remote_error(err: RemoteError, url: &str) -> CliError {
    match err {
        RemoteError::ConnectionFailed { .. } => CliError::RemoteError {
            message: format!("Failed to connect: {}", err),
            url: Some(url.to_string()),
        },
        RemoteError::AuthenticationFailed { .. } => CliError::AuthenticationFailed {
            remote: url.to_string(),
        },
        RemoteError::RepositoryNotFound { .. } => CliError::RemoteError {
            message: "Repository not found on remote".to_string(),
            url: Some(url.to_string()),
        },
        RemoteError::StackNotFound { stack } => CliError::RemoteError {
            message: format!("Stack '{}' not found on remote", stack),
            url: Some(url.to_string()),
        },
        RemoteError::ChangeNotFound { hash } => CliError::ChangeNotFound { hash },
        RemoteError::TagNotFound { state } => CliError::RemoteError {
            message: format!("Tag not found for state: {}", state),
            url: Some(url.to_string()),
        },
        RemoteError::MissingDependencies {
            count,
            missing_hashes,
        } => CliError::MissingDependency {
            change: "requested change".to_string(),
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
        RemoteError::EmptyStack { stack } => CliError::RemoteError {
            message: format!("Stack '{}' is empty", stack),
            url: Some(url.to_string()),
        },
        _ => CliError::RemoteError {
            message: err.to_string(),
            url: Some(url.to_string()),
        },
    }
}

// Display Helpers

/// Display a comparison of local and remote state.
///
/// Shows the user where both repositories are in their history,
/// making it easier to understand what will be pulled.
///
/// # Arguments
///
/// * `local_stack` - The name of the local stack
/// * `local_entries` - Local history entries
/// * `remote_stack` - The name of the remote stack
/// * `remote_state` - The remote's current state response
/// * `remote_entries` - The remote's changelist entries
///
/// # Output Format
///
/// ```text
///   Remote: main at DEF456... (15 changes)
///   Local:  main at ABC123... (10 changes)
/// ```
pub fn display_state_comparison(
    local_stack: &str,
    local_entries: &[HistoryEntry],
    remote_stack: &str,
    remote_state: &StateResponse,
    remote_entries: &[ChangelistEntry],
) {
    let local_state_str = format_local_state(local_entries);
    let remote_state_str = format_remote_state(remote_state, remote_entries);

    println!(
        "  {}: {} {}",
        info("Remote"),
        style_stack(remote_stack),
        remote_state_str
    );
    println!(
        "  {}: {} {}",
        info("Local"),
        style_stack(local_stack),
        local_state_str
    );
}

/// Format the local state for display.
///
/// # Arguments
///
/// * `entries` - Local history entries
///
/// # Returns
///
/// A formatted string like "at ABC123... (10 changes)" or "(empty)".
fn format_local_state(entries: &[HistoryEntry]) -> String {
    if entries.is_empty() {
        return hint("(empty)").to_string();
    }

    if let Some(last) = entries.last() {
        let hash_str = last.hash.to_base32();
        let hash_short = &hash_str[..12.min(hash_str.len())];
        format!(
            "at {}... ({} {})",
            hint(hash_short),
            entries.len(),
            if entries.len() == 1 {
                "change"
            } else {
                "changes"
            }
        )
    } else {
        hint("(empty)").to_string()
    }
}

/// Format the remote state for display.
///
/// # Arguments
///
/// * `state` - The remote's state response
/// * `entries` - The remote's changelist entries
///
/// # Returns
///
/// A formatted string like "at DEF456... (15 changes)" or "(empty)".
fn format_remote_state(state: &StateResponse, entries: &[ChangelistEntry]) -> String {
    if state.is_empty() || entries.is_empty() {
        return hint("(empty)").to_string();
    }

    if let Some(merkle) = state.merkle() {
        let merkle_short = &merkle[..12.min(merkle.len())];
        format!(
            "at {}... ({} {})",
            hint(merkle_short),
            entries.len(),
            if entries.len() == 1 {
                "change"
            } else {
                "changes"
            }
        )
    } else {
        hint("(empty)").to_string()
    }
}

// Formatting Utilities

/// Format a count with proper pluralization.
///
/// # Arguments
///
/// * `count` - The number of items
/// * `singular` - The singular form of the word (e.g., "change")
///
/// # Returns
///
/// A string like "1 change" or "5 changes".
///
/// # Example
///
/// ```rust
/// use atomic::commands::pull::helpers::format_count;
///
/// assert_eq!(format_count(0, "change"), "0 changes");
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

/// Format bytes in a human-readable way.
///
/// # Arguments
///
/// * `bytes` - The number of bytes
///
/// # Returns
///
/// A formatted string like "1.5 KB" or "2.3 MB".
///
/// # Example
///
/// ```rust,ignore
/// assert_eq!(format_bytes(512), "512 B");
/// assert_eq!(format_bytes(1536), "1.5 KB");
/// assert_eq!(format_bytes(2_500_000), "2.4 MB");
/// ```
pub fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::types::NodeId;

    // Delta Calculation Tests

    /// Create a test history entry for testing.
    fn create_test_history_entry(hash_bytes: &[u8], sequence: u64) -> HistoryEntry {
        let hash = Hash::of(hash_bytes);
        let state = Merkle::of(hash_bytes);
        HistoryEntry::new(sequence, NodeId::ROOT, hash, state)
    }

    /// Create a test changelist entry for testing.
    fn create_test_changelist_entry(
        hash_bytes: &[u8],
        sequence: u64,
        tagged: bool,
    ) -> ChangelistEntry {
        let hash = Hash::of(hash_bytes);
        let merkle = Merkle::of(hash_bytes);
        ChangelistEntry::new(sequence, hash.to_base32(), merkle.to_base32(), tagged)
    }

    /// Test calculating delta with empty remote.
    #[test]
    fn test_calculate_pull_delta_empty_remote() {
        let remote_entries: Vec<ChangelistEntry> = vec![];
        let local_entries = vec![create_test_history_entry(b"local1", 0)];

        let delta = calculate_pull_delta(&remote_entries, &local_entries, false);

        assert!(delta.is_empty());
    }

    /// Test calculating delta with empty local.
    #[test]
    fn test_calculate_pull_delta_empty_local() {
        let remote_entries = vec![
            create_test_changelist_entry(b"remote1", 0, false),
            create_test_changelist_entry(b"remote2", 1, false),
        ];
        let local_entries: Vec<HistoryEntry> = vec![];

        let delta = calculate_pull_delta(&remote_entries, &local_entries, false);

        assert_eq!(delta.len(), 2);
        assert_eq!(delta[0].sequence, 0);
        assert_eq!(delta[1].sequence, 1);
    }

    /// Test calculating delta when local is subset of remote.
    #[test]
    fn test_calculate_pull_delta_local_subset() {
        let remote_entries = vec![
            create_test_changelist_entry(b"change1", 0, false),
            create_test_changelist_entry(b"change2", 1, false),
            create_test_changelist_entry(b"change3", 2, false),
        ];
        let local_entries = vec![create_test_history_entry(b"change1", 0)];

        let delta = calculate_pull_delta(&remote_entries, &local_entries, false);

        // Should only need changes 2 and 3
        assert_eq!(delta.len(), 2);
        assert_eq!(delta[0].sequence, 1);
        assert_eq!(delta[1].sequence, 2);
    }

    /// Test calculating delta when already up to date.
    #[test]
    fn test_calculate_pull_delta_up_to_date() {
        let remote_entries = vec![
            create_test_changelist_entry(b"change1", 0, false),
            create_test_changelist_entry(b"change2", 1, false),
        ];
        let local_entries = vec![
            create_test_history_entry(b"change1", 0),
            create_test_history_entry(b"change2", 1),
        ];

        let delta = calculate_pull_delta(&remote_entries, &local_entries, false);

        assert!(delta.is_empty());
    }

    /// Test calculating delta with pull_all flag.
    #[test]
    fn test_calculate_pull_delta_pull_all() {
        let remote_entries = vec![
            create_test_changelist_entry(b"change1", 0, false),
            create_test_changelist_entry(b"change2", 1, true), // tagged
        ];
        let local_entries = vec![
            create_test_history_entry(b"change1", 0),
            create_test_history_entry(b"change2", 1),
        ];

        let delta = calculate_pull_delta(&remote_entries, &local_entries, true);

        // Should include all changes when pull_all is true
        assert_eq!(delta.len(), 2);
    }

    /// Test that tagged flag is preserved in delta.
    #[test]
    fn test_calculate_pull_delta_preserves_tagged() {
        let remote_entries = vec![
            create_test_changelist_entry(b"tagged", 0, true),
            create_test_changelist_entry(b"untagged", 1, false),
        ];
        let local_entries: Vec<HistoryEntry> = vec![];

        let delta = calculate_pull_delta(&remote_entries, &local_entries, false);

        assert_eq!(delta.len(), 2);
        assert!(delta[0].tagged);
        assert!(!delta[1].tagged);
    }

    // Local-Only Change Detection Tests

    /// Test detecting local-only changes when there are none.
    #[test]
    fn test_has_local_only_changes_none() {
        let local_entries = vec![
            create_test_history_entry(b"shared1", 0),
            create_test_history_entry(b"shared2", 1),
        ];
        let remote_entries = vec![
            create_test_changelist_entry(b"shared1", 0, false),
            create_test_changelist_entry(b"shared2", 1, false),
        ];

        assert!(!has_local_only_changes(&local_entries, &remote_entries));
    }

    /// Test detecting local-only changes when they exist.
    #[test]
    fn test_has_local_only_changes_present() {
        let local_entries = vec![
            create_test_history_entry(b"shared", 0),
            create_test_history_entry(b"local_only", 1),
        ];
        let remote_entries = vec![create_test_changelist_entry(b"shared", 0, false)];

        assert!(has_local_only_changes(&local_entries, &remote_entries));
    }

    /// Test with empty local history.
    #[test]
    fn test_has_local_only_changes_empty_local() {
        let local_entries: Vec<HistoryEntry> = vec![];
        let remote_entries = vec![create_test_changelist_entry(b"remote", 0, false)];

        assert!(!has_local_only_changes(&local_entries, &remote_entries));
    }

    /// Test finding specific local-only changes.
    #[test]
    fn test_find_local_only_changes() {
        let local_entries = vec![
            create_test_history_entry(b"shared", 0),
            create_test_history_entry(b"local1", 1),
            create_test_history_entry(b"local2", 2),
        ];
        let remote_entries = vec![create_test_changelist_entry(b"shared", 0, false)];

        let local_only = find_local_only_changes(&local_entries, &remote_entries);

        assert_eq!(local_only.len(), 2);
        assert!(local_only.contains(&Hash::of(b"local1").to_base32()));
        assert!(local_only.contains(&Hash::of(b"local2").to_base32()));
    }

    // Error Conversion Tests

    /// Test converting connection failed error.
    #[test]
    fn test_convert_connection_failed() {
        let err = RemoteError::connection_failed(
            "http://example.com",
            std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused"),
        );
        let cli_err = convert_remote_error(err, "http://example.com");

        match cli_err {
            CliError::RemoteError { message, url } => {
                assert!(message.contains("Failed to connect"));
                assert_eq!(url, Some("http://example.com".to_string()));
            }
            _ => panic!("Expected RemoteError"),
        }
    }

    /// Test converting authentication failed error.
    #[test]
    fn test_convert_auth_failed() {
        let err = RemoteError::auth_failed("http://example.com", "bad token");
        let cli_err = convert_remote_error(err, "http://example.com");

        assert!(matches!(cli_err, CliError::AuthenticationFailed { .. }));
    }

    /// Test converting repository not found error.
    #[test]
    fn test_convert_repo_not_found() {
        let err = RemoteError::repo_not_found("http://example.com/repo");
        let cli_err = convert_remote_error(err, "http://example.com/repo");

        match cli_err {
            CliError::RemoteError { message, .. } => {
                assert!(message.contains("not found"));
            }
            _ => panic!("Expected RemoteError"),
        }
    }

    /// Test converting stack not found error.
    #[test]
    fn test_convert_stack_not_found() {
        let err = RemoteError::stack_not_found("missing-stack");
        let cli_err = convert_remote_error(err, "http://example.com");

        match cli_err {
            CliError::RemoteError { message, .. } => {
                assert!(message.contains("missing-stack"));
            }
            _ => panic!("Expected RemoteError"),
        }
    }

    /// Test converting change not found error.
    #[test]
    fn test_convert_change_not_found() {
        let err = RemoteError::change_not_found("ABC123");
        let cli_err = convert_remote_error(err, "http://example.com");

        match cli_err {
            CliError::ChangeNotFound { hash } => {
                assert_eq!(hash, "ABC123");
            }
            _ => panic!("Expected ChangeNotFound"),
        }
    }

    /// Test converting missing dependencies error.
    #[test]
    fn test_convert_missing_deps() {
        let err = RemoteError::missing_deps(vec!["DEP1".to_string(), "DEP2".to_string()]);
        let cli_err = convert_remote_error(err, "http://example.com");

        assert!(matches!(cli_err, CliError::MissingDependency { .. }));
    }

    /// Test converting timeout error.
    #[test]
    fn test_convert_timeout() {
        let err = RemoteError::timeout(30);
        let cli_err = convert_remote_error(err, "http://example.com");

        match cli_err {
            CliError::RemoteError { message, .. } => {
                assert!(message.contains("30 seconds"));
            }
            _ => panic!("Expected RemoteError"),
        }
    }

    /// Test converting empty stack error.
    #[test]
    fn test_convert_empty_stack() {
        let err = RemoteError::empty_stack("main");
        let cli_err = convert_remote_error(err, "http://example.com");

        match cli_err {
            CliError::RemoteError { message, .. } => {
                assert!(message.contains("main"));
                assert!(message.contains("empty"));
            }
            _ => panic!("Expected RemoteError"),
        }
    }

    // Formatting Tests

    /// Test format_count with zero.
    #[test]
    fn test_format_count_zero() {
        assert_eq!(format_count(0, "change"), "0 changes");
    }

    /// Test format_count with one (singular).
    #[test]
    fn test_format_count_one() {
        assert_eq!(format_count(1, "change"), "1 change");
    }

    /// Test format_count with many (plural).
    #[test]
    fn test_format_count_many() {
        assert_eq!(format_count(5, "change"), "5 changes");
        assert_eq!(format_count(100, "file"), "100 files");
    }

    /// Test format_count with different words.
    #[test]
    fn test_format_count_different_words() {
        assert_eq!(format_count(1, "tag"), "1 tag");
        assert_eq!(format_count(2, "tag"), "2 tags");
        assert_eq!(format_count(1, "dependency"), "1 dependency");
        assert_eq!(format_count(3, "dependency"), "3 dependencys"); // Note: simple pluralization
    }

    /// Test format_bytes with small values.
    #[test]
    fn test_format_bytes_small() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1023), "1023 B");
    }

    /// Test format_bytes with kilobytes.
    #[test]
    fn test_format_bytes_kb() {
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1536), "1.5 KB");
    }

    /// Test format_bytes with megabytes.
    #[test]
    fn test_format_bytes_mb() {
        assert_eq!(format_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(format_bytes(2 * 1024 * 1024 + 512 * 1024), "2.5 MB");
    }

    /// Test format_bytes with gigabytes.
    #[test]
    fn test_format_bytes_gb() {
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.0 GB");
    }

    // State Display Tests

    /// Test format_local_state with empty entries.
    #[test]
    fn test_format_local_state_empty() {
        let entries: Vec<HistoryEntry> = vec![];
        let result = format_local_state(&entries);
        assert!(result.contains("empty"));
    }

    /// Test format_local_state with entries.
    #[test]
    fn test_format_local_state_with_entries() {
        let entries = vec![
            create_test_history_entry(b"first", 0),
            create_test_history_entry(b"second", 1),
        ];
        let result = format_local_state(&entries);
        assert!(result.contains("2"));
        assert!(result.contains("changes"));
    }

    /// Test format_remote_state with empty state.
    #[test]
    fn test_format_remote_state_empty() {
        let state = StateResponse::empty();
        let entries: Vec<ChangelistEntry> = vec![];
        let result = format_remote_state(&state, &entries);
        assert!(result.contains("empty"));
    }

    /// Test format_local_state with single change (singular).
    #[test]
    fn test_format_local_state_singular() {
        let entries = vec![create_test_history_entry(b"only", 0)];
        let result = format_local_state(&entries);
        assert!(result.contains("1"));
        assert!(result.contains("change"));
        // Should be singular "change", not "changes"
    }

    /// Test that delta calculation handles diverged history correctly.
    #[test]
    fn test_calculate_pull_delta_diverged() {
        // Local has: A, B, C (C is local-only)
        // Remote has: A, B, D (D is remote-only)
        let remote_entries = vec![
            create_test_changelist_entry(b"A", 0, false),
            create_test_changelist_entry(b"B", 1, false),
            create_test_changelist_entry(b"D", 2, false),
        ];
        let local_entries = vec![
            create_test_history_entry(b"A", 0),
            create_test_history_entry(b"B", 1),
            create_test_history_entry(b"C", 2),
        ];

        let delta = calculate_pull_delta(&remote_entries, &local_entries, false);

        // Should only include D (remote-only)
        assert_eq!(delta.len(), 1);
        assert_eq!(delta[0].hash, Hash::of(b"D"));
    }

    /// Test that local-only detection works with diverged history.
    #[test]
    fn test_find_local_only_changes_diverged() {
        let local_entries = vec![
            create_test_history_entry(b"A", 0),
            create_test_history_entry(b"B", 1),
            create_test_history_entry(b"C", 2), // local-only
        ];
        let remote_entries = vec![
            create_test_changelist_entry(b"A", 0, false),
            create_test_changelist_entry(b"B", 1, false),
            create_test_changelist_entry(b"D", 2, false),
        ];

        let local_only = find_local_only_changes(&local_entries, &remote_entries);

        assert_eq!(local_only.len(), 1);
        assert!(local_only.contains(&Hash::of(b"C").to_base32()));
    }

    /// Test converting tag not found error.
    #[test]
    fn test_convert_tag_not_found() {
        let err = RemoteError::tag_not_found("STATE123");
        let cli_err = convert_remote_error(err, "http://example.com");

        match cli_err {
            CliError::RemoteError { message, .. } => {
                assert!(message.contains("Tag not found"));
                assert!(message.contains("STATE123"));
            }
            _ => panic!("Expected RemoteError"),
        }
    }

    /// Test converting state mismatch error.
    #[test]
    fn test_convert_state_mismatch() {
        let err = RemoteError::state_mismatch("ABC", "DEF");
        let cli_err = convert_remote_error(err, "http://example.com");

        assert!(matches!(cli_err, CliError::Conflict { .. }));
    }
}
