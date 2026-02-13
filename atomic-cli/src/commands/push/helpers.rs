//! Helper functions for the push command.
//!
//! This module provides utility functions used by the push command, including:
//!
//! - Delta calculation between local and remote changelists
//! - Error conversion from remote errors to CLI errors
//! - State comparison display
//! - Change data loading and formatting

use std::collections::HashSet;

use bytes::Bytes;

use atomic_core::types::{Base32, Hash};
use atomic_remote::{ChangelistEntry, RemoteError, StateResponse};
use atomic_repository::history::HistoryEntry;
use atomic_repository::Repository;

use crate::error::{CliError, CliResult};
use crate::output::{info, stack as style_stack};

use super::types::PushChange;

// =============================================================================
// Delta Calculation
// =============================================================================

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
    // Build set of remote hashes for quick lookup (changes already in this stack)
    let remote_hashes: HashSet<String> = remote_entries.iter().map(|e| e.hash.clone()).collect();

    let mut to_upload = Vec::new();

    for entry in local_entries {
        let hash_str = entry.hash.to_base32();

        // Skip if already on this stack on the remote (unless pushing all)
        if !push_all && remote_hashes.contains(&hash_str) {
            continue;
        }

        // Try to load the change header to get the message
        let message = load_change_message(repo, &entry.hash);

        // Check if the change is already in the remote graph (via another stack).
        // If so, only stack adoption is needed — no data transfer.
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

// =============================================================================
// Change Data Loading
// =============================================================================

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
    // Load the change
    let change = repo
        .load_change(hash)
        .map_err(|_| CliError::ChangeNotFound {
            hash: hash.to_base32(),
        })?;

    // Serialize to bytes
    let mut buffer = Vec::new();
    change
        .serialize(&mut buffer)
        .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to serialize change: {}", e)))?;

    Ok(Bytes::from(buffer))
}

// =============================================================================
// Error Conversion
// =============================================================================

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
        RemoteError::RepositoryNotFound { .. } => CliError::RemoteError {
            message: "Repository not found on remote".to_string(),
            url: Some(url.to_string()),
        },
        RemoteError::StackNotFound { stack } => CliError::RemoteError {
            message: format!("Stack '{}' not found on remote", stack),
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
        _ => CliError::RemoteError {
            message: err.to_string(),
            url: Some(url.to_string()),
        },
    }
}

// =============================================================================
// Display Helpers
// =============================================================================

/// Display the state comparison between local and remote.
///
/// Shows the user a summary of where local and remote are at,
/// helping them understand what will be pushed.
///
/// # Arguments
///
/// * `local_stack` - Name of the local stack
/// * `local_entries` - Local history entries
/// * `remote_stack` - Name of the remote stack
/// * `remote_state` - Current state of the remote
/// * `remote_entries` - Remote changelist entries
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
        "  Local:  {} at {}",
        style_stack(local_stack),
        info(&local_state_str)
    );
    println!(
        "  Remote: {} at {}",
        style_stack(remote_stack),
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

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Delta Calculation Tests
    // =========================================================================

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

    // =========================================================================
    // Error Conversion Tests
    // =========================================================================

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
        let err = RemoteError::repo_not_found("http://example.com/repo");
        let cli_err = convert_remote_error(err, "http://example.com/repo");

        match cli_err {
            CliError::RemoteError { message, .. } => {
                assert!(message.contains("not found"));
            }
            _ => panic!("Expected RemoteError"),
        }
    }

    #[test]
    fn test_convert_stack_not_found() {
        let err = RemoteError::stack_not_found("main");
        let cli_err = convert_remote_error(err, "http://example.com");

        match cli_err {
            CliError::RemoteError { message, .. } => {
                assert!(message.contains("main"));
                assert!(message.contains("not found"));
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

    // =========================================================================
    // Format Count Tests
    // =========================================================================

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

    // =========================================================================
    // Display Helper Tests
    // =========================================================================

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

    // =========================================================================
    // Test Helpers
    // =========================================================================

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
