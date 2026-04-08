//! Tests for the history module.

use super::*;

use atomic_core::change::ChangeHeader;
use atomic_core::pristine::ViewState;
use atomic_core::types::{Hash, Inode, Merkle, NodeId};

// StateBeforeChange Tests

#[test]
fn test_state_before_change_new() {
    let parent_state = Merkle::of(b"parent");
    let change_state = Merkle::of(b"change");

    let state_info = StateBeforeChange::new(Some(5), parent_state, 6, change_state);

    assert_eq!(state_info.parent_sequence, Some(5));
    assert_eq!(state_info.parent_state, parent_state);
    assert_eq!(state_info.change_sequence, 6);
    assert_eq!(state_info.change_state, change_state);
}

#[test]
fn test_state_before_change_first_change() {
    let change_state = Merkle::of(b"first");

    let state_info = StateBeforeChange::new(None, Merkle::ZERO, 0, change_state);

    assert!(state_info.is_first_change());
    assert_eq!(state_info.parent_sequence, None);
    assert_eq!(state_info.parent_state, Merkle::ZERO);
}

#[test]
fn test_state_before_change_not_first() {
    let parent_state = Merkle::of(b"parent");
    let change_state = Merkle::of(b"change");

    let state_info = StateBeforeChange::new(Some(10), parent_state, 11, change_state);

    assert!(!state_info.is_first_change());
}

#[test]
fn test_state_before_change_parent_max_sequence_first() {
    let state_info = StateBeforeChange::new(None, Merkle::ZERO, 0, Merkle::of(b"first"));

    // First change has no parent, so max sequence should be 0
    assert_eq!(state_info.parent_max_sequence_exclusive(), 0);
}

#[test]
fn test_state_before_change_parent_max_sequence_later() {
    let state_info =
        StateBeforeChange::new(Some(5), Merkle::of(b"parent"), 6, Merkle::of(b"change"));

    // Parent at sequence 5, so max exclusive is 6 (includes 0-5)
    assert_eq!(state_info.parent_max_sequence_exclusive(), 6);
}

#[test]
fn test_state_before_change_display_first() {
    let state_info = StateBeforeChange::new(None, Merkle::ZERO, 0, Merkle::of(b"first"));

    let display = format!("{}", state_info);
    assert!(display.contains("First change"));
    assert!(display.contains("seq 0"));
}

#[test]
fn test_state_before_change_display_later() {
    let parent_state = Merkle::of(b"parent");
    let change_state = Merkle::of(b"change");

    let state_info = StateBeforeChange::new(Some(5), parent_state, 6, change_state);

    let display = format!("{}", state_info);
    assert!(display.contains("seq 5"));
    assert!(display.contains("seq 6"));
}

#[test]
fn test_state_before_change_equality() {
    let state1 = StateBeforeChange::new(Some(1), Merkle::of(b"a"), 2, Merkle::of(b"b"));
    let state2 = StateBeforeChange::new(Some(1), Merkle::of(b"a"), 2, Merkle::of(b"b"));
    let state3 = StateBeforeChange::new(Some(2), Merkle::of(b"a"), 3, Merkle::of(b"b"));

    assert_eq!(state1, state2);
    assert_ne!(state1, state3);
}

#[test]
fn test_state_before_change_clone() {
    let original = StateBeforeChange::new(Some(1), Merkle::of(b"a"), 2, Merkle::of(b"b"));
    let cloned = original.clone();

    assert_eq!(original, cloned);
}

#[test]
fn test_state_before_change_debug() {
    let state_info = StateBeforeChange::new(Some(1), Merkle::of(b"a"), 2, Merkle::of(b"b"));
    let debug = format!("{:?}", state_info);

    assert!(debug.contains("StateBeforeChange"));
    assert!(debug.contains("parent_sequence"));
}

// Existing Tests (below this line)

// HistoryEntry Tests

#[test]
fn test_history_entry_new() {
    let hash = Hash::of(b"test change");
    let state = Merkle::of(b"test state");
    let entry = HistoryEntry::new(42, NodeId::new(1), hash, state);

    assert_eq!(entry.sequence, 42);
    assert_eq!(entry.node_id, NodeId::new(1));
    assert_eq!(entry.hash, hash);
    assert_eq!(entry.state, state);
    assert!(entry.header.is_none());
    assert!(!entry.is_tagged);
}

#[test]
fn test_history_entry_with_header() {
    let hash = Hash::of(b"test");
    let state = Merkle::of(b"state");
    let header = ChangeHeader::default();
    let entry = HistoryEntry::with_header(1, NodeId::new(2), hash, state, header.clone(), true);

    assert_eq!(entry.sequence, 1);
    assert!(entry.header.is_some());
    assert!(entry.is_tagged);
}

#[test]
fn test_history_entry_builder_pattern() {
    let hash = Hash::of(b"test");
    let state = Merkle::of(b"state");
    let header = ChangeHeader::default();

    let entry = HistoryEntry::new(0, NodeId::new(1), hash, state)
        .with_tagged(true)
        .with_change_header(header);

    assert!(entry.is_tagged);
    assert!(entry.header.is_some());
}

#[test]
fn test_history_entry_accessors() {
    let hash = Hash::of(b"test");
    let state = Merkle::of(b"state");
    let mut header = ChangeHeader::default();
    header.message = "Test message".to_string();
    header.description = Some("Test description".to_string());

    let entry = HistoryEntry::with_header(0, NodeId::new(1), hash, state, header, false);

    assert_eq!(entry.message(), Some("Test message"));
    assert_eq!(entry.description(), Some("Test description"));
    assert!(entry.timestamp().is_some());
    assert!(entry.authors().is_some());
}

#[test]
fn test_history_entry_no_header_accessors() {
    let hash = Hash::of(b"test");
    let state = Merkle::of(b"state");
    let entry = HistoryEntry::new(0, NodeId::new(1), hash, state);

    assert!(entry.message().is_none());
    assert!(entry.description().is_none());
    assert!(entry.timestamp().is_none());
    assert!(entry.authors().is_none());
}

#[test]
fn test_history_entry_display() {
    let hash = Hash::of(b"test");
    let state = Merkle::of(b"state");
    let entry = HistoryEntry::new(5, NodeId::new(1), hash, state);

    let display = format!("{}", entry);
    assert!(display.contains("#5"));
    assert!(display.contains("state:"));
}

#[test]
fn test_history_entry_display_tagged() {
    let hash = Hash::of(b"test");
    let state = Merkle::of(b"state");
    let entry = HistoryEntry::new(5, NodeId::new(1), hash, state).with_tagged(true);

    let display = format!("{}", entry);
    assert!(display.contains("[tagged]"));
}

#[test]
fn test_history_entry_equality() {
    let hash = Hash::of(b"test");
    let state = Merkle::of(b"state");
    let entry1 = HistoryEntry::new(5, NodeId::new(1), hash, state);
    let entry2 = HistoryEntry::new(5, NodeId::new(1), hash, state);

    assert_eq!(entry1, entry2);
}

// HistoryOptions Tests

#[test]
fn test_history_options_default() {
    let options = HistoryOptions::default();

    assert_eq!(options.from_sequence, 0);
    assert!(options.limit.is_none());
    assert!(!options.load_headers);
    assert!(options.view.is_none());
    assert!(!options.tagged_only);
}

#[test]
fn test_history_options_builder() {
    let options = HistoryOptions::new()
        .from_sequence(10)
        .limit(50)
        .load_headers(true)
        .view("feature")
        .tagged_only(true);

    assert_eq!(options.from_sequence, 10);
    assert_eq!(options.limit, Some(50));
    assert!(options.load_headers);
    assert_eq!(options.view, Some("feature".to_string()));
    assert!(options.tagged_only);
}

#[test]
fn test_history_options_last() {
    let options = HistoryOptions::last(10);

    assert_eq!(options.from_sequence, 0);
    assert_eq!(options.limit, Some(10));
}

#[test]
fn test_history_options_with_headers() {
    let options = HistoryOptions::with_headers();

    assert!(options.load_headers);
}

// HistorySummary Tests

#[test]
fn test_history_summary_new() {
    let view_state = ViewState::new(1, "main".to_string());
    let summary = HistorySummary::new("main", &view_state);

    assert_eq!(summary.view_name, "main");
    assert_eq!(summary.change_count, 0);
    assert!(summary.first_change.is_none());
    assert!(summary.last_change.is_none());
}

#[test]
fn test_history_summary_is_empty() {
    let view_state = ViewState::new(1, "main".to_string());
    let summary = HistorySummary::new("main", &view_state);

    assert!(summary.is_empty());
}

#[test]
fn test_history_summary_with_bounds() {
    let view_state = ViewState::new(1, "main".to_string());
    let first = Hash::of(b"first");
    let last = Hash::of(b"last");

    let summary = HistorySummary::new("main", &view_state).with_bounds(Some(first), Some(last));

    assert_eq!(summary.first_change, Some(first));
    assert_eq!(summary.last_change, Some(last));
}

#[test]
fn test_history_summary_with_tagged_count() {
    let view_state = ViewState::new(1, "main".to_string());
    let summary = HistorySummary::new("main", &view_state).with_tagged_count(5);

    assert_eq!(summary.tagged_count, 5);
}

#[test]
fn test_history_summary_display() {
    let view_state = ViewState::new(1, "main".to_string());
    let summary = HistorySummary::new("main", &view_state).with_tagged_count(3);

    let display = format!("{}", summary);
    assert!(display.contains("main"));
    assert!(display.contains("0 changes"));
    assert!(display.contains("3 tagged"));
}

// PathHistoryEntry Tests

#[test]
fn test_path_history_entry_new() {
    let hash = Hash::of(b"test");
    let state = Merkle::of(b"state");
    let entry = HistoryEntry::new(1, NodeId::new(1), hash, state);
    let path_entry = PathHistoryEntry::new(entry, "src/main.rs", PathModificationType::Modified);

    assert_eq!(path_entry.path, "src/main.rs");
    assert_eq!(path_entry.modification_type, PathModificationType::Modified);
    assert!(path_entry.inode.is_none());
}

#[test]
fn test_path_history_entry_with_inode() {
    let hash = Hash::of(b"test");
    let state = Merkle::of(b"state");
    let entry = HistoryEntry::new(1, NodeId::new(1), hash, state);
    let path_entry = PathHistoryEntry::new(entry, "src/main.rs", PathModificationType::Created)
        .with_inode(Inode::new(42));

    assert_eq!(path_entry.inode, Some(Inode::new(42)));
}

#[test]
fn test_path_history_entry_accessors() {
    let hash = Hash::of(b"test");
    let state = Merkle::of(b"state");
    let entry = HistoryEntry::new(5, NodeId::new(1), hash, state);
    let path_entry = PathHistoryEntry::new(entry, "src/main.rs", PathModificationType::Modified);

    assert_eq!(path_entry.sequence(), 5);
    assert_eq!(*path_entry.hash(), hash);
}

// PathModificationType Tests

#[test]
fn test_path_modification_type_display() {
    assert_eq!(format!("{}", PathModificationType::Created), "created");
    assert_eq!(format!("{}", PathModificationType::Modified), "modified");
    assert_eq!(format!("{}", PathModificationType::Deleted), "deleted");
    assert_eq!(format!("{}", PathModificationType::Moved), "moved");
    assert_eq!(format!("{}", PathModificationType::Unknown), "unknown");
}

#[test]
fn test_path_modification_type_equality() {
    assert_eq!(PathModificationType::Created, PathModificationType::Created);
    assert_ne!(
        PathModificationType::Created,
        PathModificationType::Modified
    );
}

// HistoryError Tests

#[test]
fn test_history_error_view_not_found() {
    let error = HistoryError::ViewNotFound {
        name: "missing".to_string(),
    };
    let msg = format!("{}", error);
    assert!(msg.contains("missing"));
}

#[test]
fn test_history_error_sequence_out_of_range() {
    let error = HistoryError::SequenceOutOfRange {
        sequence: 100,
        max: 50,
    };
    let msg = format!("{}", error);
    assert!(msg.contains("100"));
    assert!(msg.contains("50"));
}

#[test]
fn test_history_error_change_not_found() {
    let error = HistoryError::ChangeNotFound {
        hash: "ABC123".to_string(),
    };
    let msg = format!("{}", error);
    assert!(msg.contains("ABC123"));
}

#[test]
fn test_history_error_path_not_found() {
    let error = HistoryError::PathNotFound {
        path: "src/missing.rs".to_string(),
    };
    let msg = format!("{}", error);
    assert!(msg.contains("src/missing.rs"));
}

#[test]
fn test_history_error_database() {
    let error = HistoryError::Database("connection failed".to_string());
    let msg = format!("{}", error);
    assert!(msg.contains("connection failed"));
}
