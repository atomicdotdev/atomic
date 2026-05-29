#![cfg(feature = "remote")]
//! Integration tests for the push command.
//!
//! These tests verify the push command's behavior in realistic scenarios,
//! including error handling and edge cases. They test the command construction
//! and validation, but not actual network operations (which would require a
//! running `atomic-api` server).
//!
//! For end-to-end tests against a real server, see the `integration-tests`
//! feature in the `atomic-remote-client` crate.
//!
//! Note: Since the atomic crate is a binary, these tests use the underlying
//! library crates directly rather than importing from `atomic`.

use atomic_core::types::{Base32, Hash, Merkle, NodeId};
use atomic_remote::{ChangelistEntry, RemoteError, StateResponse};
use atomic_repository::history::HistoryEntry;
use std::collections::HashSet;

// Types Tests (replicated from push/types.rs for integration testing)

/// A change to be pushed to the remote (test version).
#[derive(Debug, Clone, PartialEq, Eq)]
struct PushChange {
    hash: Hash,
    sequence: u64,
    state: Merkle,
    tagged: bool,
    message: Option<String>,
}

impl PushChange {
    fn new(hash: Hash, sequence: u64, state: Merkle) -> Self {
        Self {
            hash,
            sequence,
            state,
            tagged: false,
            message: None,
        }
    }

    fn with_tagged(mut self, tagged: bool) -> Self {
        self.tagged = tagged;
        self
    }

    fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    fn has_message(&self) -> bool {
        self.message.is_some()
    }

    fn message_or_default(&self) -> &str {
        self.message.as_deref().unwrap_or("(no message)")
    }
}

/// Statistics about a push operation (test version).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct PushStats {
    changes_uploaded: usize,
    tags_uploaded: usize,
    bytes_transferred: u64,
    changes_skipped: usize,
    changes_failed: usize,
}

impl PushStats {
    fn new() -> Self {
        Self::default()
    }

    fn total_uploaded(&self) -> usize {
        self.changes_uploaded + self.tags_uploaded
    }

    fn has_uploads(&self) -> bool {
        self.total_uploaded() > 0
    }

    fn is_noop(&self) -> bool {
        self.total_uploaded() == 0 && self.changes_skipped > 0
    }

    fn has_failures(&self) -> bool {
        self.changes_failed > 0
    }

    fn record_change_uploaded(&mut self, bytes: u64) {
        self.changes_uploaded += 1;
        self.bytes_transferred += bytes;
    }

    fn record_tag_uploaded(&mut self, bytes: u64) {
        self.tags_uploaded += 1;
        self.bytes_transferred += bytes;
    }

    fn record_skipped(&mut self) {
        self.changes_skipped += 1;
    }

    fn record_failed(&mut self) {
        self.changes_failed += 1;
    }
}

// Helper Functions (replicated from push/helpers.rs)

/// Check if local and remote histories have diverged.
fn has_diverged(local_entries: &[HistoryEntry], remote_entries: &[ChangelistEntry]) -> bool {
    if remote_entries.is_empty() {
        return false;
    }

    let local_hashes: HashSet<String> = local_entries.iter().map(|e| e.hash.to_base32()).collect();

    remote_entries
        .iter()
        .any(|e| !local_hashes.contains(&e.hash))
}

/// Format a count with singular/plural suffix.
fn format_count(count: usize, singular: &str) -> String {
    if count == 1 {
        format!("{} {}", count, singular)
    } else {
        format!("{} {}s", count, singular)
    }
}

// Test Helpers

fn make_history_entry(sequence: u64, hash_seed: &str) -> HistoryEntry {
    HistoryEntry {
        sequence,
        hash: Hash::of(hash_seed.as_bytes()),
        state: Merkle::ZERO,
        node_id: NodeId::from(sequence),
        header: None,
        is_tagged: false,
    }
}

fn make_changelist_entry(sequence: u64, hash_seed: &str) -> ChangelistEntry {
    let hash = Hash::of(hash_seed.as_bytes());
    ChangelistEntry::new(
        sequence,
        hash.to_base32(),
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        false,
    )
}

// PushChange Tests

#[test]
fn test_push_change_new() {
    let hash = Hash::of(b"test");
    let state = Merkle::ZERO;
    let change = PushChange::new(hash, 42, state);

    assert_eq!(change.hash, hash);
    assert_eq!(change.sequence, 42);
    assert_eq!(change.state, state);
    assert!(!change.tagged);
    assert!(change.message.is_none());
}

#[test]
fn test_push_change_with_tagged() {
    let hash = Hash::of(b"test");
    let change = PushChange::new(hash, 0, Merkle::ZERO).with_tagged(true);
    assert!(change.tagged);
}

#[test]
fn test_push_change_with_message() {
    let hash = Hash::of(b"test");
    let change = PushChange::new(hash, 0, Merkle::ZERO).with_message("Add feature");

    assert_eq!(change.message, Some("Add feature".to_string()));
    assert!(change.has_message());
    assert_eq!(change.message_or_default(), "Add feature");
}

#[test]
fn test_push_change_message_or_default() {
    let hash = Hash::of(b"test");
    let change = PushChange::new(hash, 0, Merkle::ZERO);

    assert!(!change.has_message());
    assert_eq!(change.message_or_default(), "(no message)");
}

#[test]
fn test_push_change_builder_chain() {
    let hash = Hash::of(b"test");
    let change = PushChange::new(hash, 5, Merkle::ZERO)
        .with_tagged(true)
        .with_message("Fix bug");

    assert_eq!(change.sequence, 5);
    assert!(change.tagged);
    assert_eq!(change.message.as_deref(), Some("Fix bug"));
}

// PushStats Tests

#[test]
fn test_push_stats_new() {
    let stats = PushStats::new();

    assert_eq!(stats.changes_uploaded, 0);
    assert_eq!(stats.tags_uploaded, 0);
    assert_eq!(stats.bytes_transferred, 0);
    assert_eq!(stats.changes_skipped, 0);
    assert_eq!(stats.changes_failed, 0);
}

#[test]
fn test_push_stats_total_uploaded() {
    let mut stats = PushStats::new();
    assert_eq!(stats.total_uploaded(), 0);

    stats.changes_uploaded = 3;
    assert_eq!(stats.total_uploaded(), 3);

    stats.tags_uploaded = 2;
    assert_eq!(stats.total_uploaded(), 5);
}

#[test]
fn test_push_stats_has_uploads() {
    let mut stats = PushStats::new();
    assert!(!stats.has_uploads());

    stats.changes_uploaded = 1;
    assert!(stats.has_uploads());
}

#[test]
fn test_push_stats_is_noop() {
    let mut stats = PushStats::new();
    assert!(!stats.is_noop());

    stats.changes_skipped = 5;
    assert!(stats.is_noop());

    stats.changes_uploaded = 1;
    assert!(!stats.is_noop());
}

#[test]
fn test_push_stats_has_failures() {
    let mut stats = PushStats::new();
    assert!(!stats.has_failures());

    stats.changes_failed = 1;
    assert!(stats.has_failures());
}

#[test]
fn test_push_stats_record_change_uploaded() {
    let mut stats = PushStats::new();
    stats.record_change_uploaded(1024);

    assert_eq!(stats.changes_uploaded, 1);
    assert_eq!(stats.bytes_transferred, 1024);

    stats.record_change_uploaded(512);
    assert_eq!(stats.changes_uploaded, 2);
    assert_eq!(stats.bytes_transferred, 1536);
}

#[test]
fn test_push_stats_record_tag_uploaded() {
    let mut stats = PushStats::new();
    stats.record_tag_uploaded(256);

    assert_eq!(stats.tags_uploaded, 1);
    assert_eq!(stats.bytes_transferred, 256);
}

#[test]
fn test_push_stats_record_skipped() {
    let mut stats = PushStats::new();
    stats.record_skipped();
    stats.record_skipped();

    assert_eq!(stats.changes_skipped, 2);
}

#[test]
fn test_push_stats_record_failed() {
    let mut stats = PushStats::new();
    stats.record_failed();

    assert_eq!(stats.changes_failed, 1);
    assert!(stats.has_failures());
}

// has_diverged Tests

#[test]
fn test_has_diverged_with_empty_remote() {
    let local = vec![
        make_history_entry(0, "change1"),
        make_history_entry(1, "change2"),
    ];
    let remote: Vec<ChangelistEntry> = vec![];

    assert!(!has_diverged(&local, &remote));
}

#[test]
fn test_has_diverged_with_matching_history() {
    let local = vec![
        make_history_entry(0, "change1"),
        make_history_entry(1, "change2"),
    ];
    let remote = vec![
        make_changelist_entry(0, "change1"),
        make_changelist_entry(1, "change2"),
    ];

    assert!(!has_diverged(&local, &remote));
}

#[test]
fn test_has_diverged_with_extra_local() {
    let local = vec![
        make_history_entry(0, "change1"),
        make_history_entry(1, "change2"),
        make_history_entry(2, "change3"),
    ];
    let remote = vec![
        make_changelist_entry(0, "change1"),
        make_changelist_entry(1, "change2"),
    ];

    // Remote is a subset of local - no divergence
    assert!(!has_diverged(&local, &remote));
}

#[test]
fn test_has_diverged_with_different_changes() {
    let local = vec![
        make_history_entry(0, "change1"),
        make_history_entry(1, "local_change"),
    ];
    let remote = vec![
        make_changelist_entry(0, "change1"),
        make_changelist_entry(1, "remote_change"),
    ];

    // Remote has a change not in local - diverged
    assert!(has_diverged(&local, &remote));
}

#[test]
fn test_has_diverged_with_empty_local() {
    let local: Vec<HistoryEntry> = vec![];
    let remote = vec![make_changelist_entry(0, "change1")];

    // Remote has changes not in local - diverged
    assert!(has_diverged(&local, &remote));
}

// format_count Tests

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

// Remote Error Tests

#[test]
fn test_remote_error_is_retryable() {
    let timeout_err = RemoteError::timeout(30);
    assert!(timeout_err.is_retryable());

    let auth_err = RemoteError::auth_failed("url", "message");
    assert!(!auth_err.is_retryable());
}

#[test]
fn test_remote_error_is_auth_error() {
    let auth_err = RemoteError::auth_failed("url", "message");
    assert!(auth_err.is_auth_error());

    let timeout_err = RemoteError::timeout(30);
    assert!(!timeout_err.is_auth_error());
}

#[test]
fn test_remote_error_is_not_found() {
    let repo_err = RemoteError::repo_not_found("url");
    assert!(repo_err.is_not_found());

    let view_err = RemoteError::view_not_found("main");
    assert!(view_err.is_not_found());

    let change_err = RemoteError::change_not_found("ABC123");
    assert!(change_err.is_not_found());

    let tag_err = RemoteError::tag_not_found("DEF456");
    assert!(tag_err.is_not_found());

    let timeout_err = RemoteError::timeout(30);
    assert!(!timeout_err.is_not_found());
}

#[test]
fn test_remote_error_suggestions() {
    let conn_err = RemoteError::connection_failed(
        "url",
        std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused"),
    );
    assert!(conn_err.suggestion().is_some());

    let auth_err = RemoteError::auth_failed("url", "message");
    assert!(auth_err.suggestion().is_some());

    let timeout_err = RemoteError::timeout(30);
    assert!(timeout_err.suggestion().is_some());
}

// StateResponse Tests

#[test]
fn test_state_response_empty() {
    let state = StateResponse::empty();
    assert!(state.is_empty());
    assert!(state.position().is_none());
    assert!(state.merkle().is_none());
}

#[test]
fn test_state_response_with_state() {
    let state = StateResponse::state(
        42,
        "ABCD1234ABCD1234ABCD1234ABCD1234ABCD1234ABCD1234ABCD",
        "EFGH5678EFGH5678EFGH5678EFGH5678EFGH5678EFGH5678EFGH",
    );
    assert!(!state.is_empty());
    assert_eq!(state.position(), Some(42));
    assert!(state.merkle().is_some());
}

// ChangelistEntry Tests

#[test]
fn test_changelist_entry_new() {
    let entry = ChangelistEntry::new(5, "ABCD1234", "EFGH5678", true);

    assert_eq!(entry.sequence, 5);
    assert_eq!(entry.hash, "ABCD1234");
    assert_eq!(entry.merkle, "EFGH5678");
    assert!(entry.tagged);
}

#[test]
fn test_changelist_entry_to_protocol_line() {
    let entry = ChangelistEntry::new(5, "ABCD1234", "EFGH5678", false);
    let line = entry.to_protocol_line();
    assert_eq!(line, "5.ABCD1234.EFGH5678");

    let tagged_entry = ChangelistEntry::new(5, "ABCD1234", "EFGH5678", true);
    let tagged_line = tagged_entry.to_protocol_line();
    assert_eq!(tagged_line, "5.ABCD1234.EFGH5678.");
}

#[test]
fn test_changelist_entry_parse() {
    let entry = ChangelistEntry::parse("5.ABCD1234.EFGH5678").unwrap();
    assert_eq!(entry.sequence, 5);
    assert_eq!(entry.hash, "ABCD1234");
    assert_eq!(entry.merkle, "EFGH5678");
    assert!(!entry.tagged);

    let tagged_entry = ChangelistEntry::parse("5.ABCD1234.EFGH5678.").unwrap();
    assert!(tagged_entry.tagged);
}

#[test]
fn test_changelist_entry_roundtrip() {
    let original = ChangelistEntry::new(42, "HASH123", "MERKLE456", true);
    let line = original.to_protocol_line();
    let parsed = ChangelistEntry::parse(&line).unwrap();

    assert_eq!(original.sequence, parsed.sequence);
    assert_eq!(original.hash, parsed.hash);
    assert_eq!(original.merkle, parsed.merkle);
    assert_eq!(original.tagged, parsed.tagged);
}
