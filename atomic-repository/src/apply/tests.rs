//! Tests for change insertion module.

use super::*;
use atomic_core::apply::LocalApplyError;
use atomic_core::types::{Hash, Merkle};

// InsertOptions Tests

#[test]
fn test_insert_options_default() {
    let opts = InsertOptions::default();
    assert!(!opts.apply_dependencies);
    assert!(opts.allow_conflicts);
    assert_eq!(opts.max_depth, 100);
    assert!(opts.track_conflicts);
}

#[test]
fn test_insert_options_with_dependencies() {
    let opts = InsertOptions::with_dependencies();
    assert!(opts.apply_dependencies);
}

#[test]
fn test_insert_options_strict() {
    let opts = InsertOptions::strict();
    assert!(!opts.allow_conflicts);
}

#[test]
fn test_insert_options_builder() {
    let opts = InsertOptions::default()
        .apply_deps(true)
        .allow_conflict(false)
        .max_recursion(50);

    assert!(opts.apply_dependencies);
    assert!(!opts.allow_conflicts);
    assert_eq!(opts.max_depth, 50);
}

// InsertStats Tests

#[test]
fn test_insert_stats_new() {
    let stats = InsertStats::new();
    assert_eq!(stats.changes_applied, 0);
    assert_eq!(stats.atoms_processed, 0);
    assert!(!stats.has_applied());
    assert!(!stats.has_conflicts());
}

#[test]
fn test_insert_stats_has_applied() {
    let mut stats = InsertStats::new();
    assert!(!stats.has_applied());

    stats.changes_applied = 1;
    assert!(stats.has_applied());
}

#[test]
fn test_insert_stats_has_conflicts() {
    let mut stats = InsertStats::new();
    assert!(!stats.has_conflicts());

    stats.conflicts_detected = 1;
    assert!(stats.has_conflicts());
}

#[test]
fn test_insert_stats_merge() {
    let mut stats1 = InsertStats::new();
    stats1.changes_applied = 2;
    stats1.atoms_processed = 10;

    let mut stats2 = InsertStats::new();
    stats2.changes_applied = 1;
    stats2.atoms_processed = 5;
    stats2.conflicts_detected = 1;

    stats1.merge(stats2);

    assert_eq!(stats1.changes_applied, 3);
    assert_eq!(stats1.atoms_processed, 15);
    assert_eq!(stats1.conflicts_detected, 1);
}

// InsertOutcome Tests

#[test]
fn test_insert_outcome_new() {
    let state = Merkle::of(b"test");
    let stats = InsertStats::new();
    let outcome = InsertOutcome::new(state, 1, false, stats);

    assert_eq!(outcome.new_state, state);
    assert_eq!(outcome.sequence, 1);
    assert!(!outcome.has_conflicts);
}

// Error Tests

#[test]
fn test_insert_error_display() {
    let err = InsertError::ChangeNotFound {
        hash: "ABC123".to_string(),
    };
    assert!(err.to_string().contains("ABC123"));

    let hash1 = Hash::of(b"dep1");
    let err = InsertError::MissingDependencies {
        missing: vec![hash1],
    };
    let msg = err.to_string();
    assert!(msg.contains("Missing dependencies"));

    let err = InsertError::AlreadyApplied {
        hash: "XYZ789".to_string(),
    };
    assert!(err.to_string().contains("already applied"));
}

#[test]
fn test_insert_error_from_local() {
    let local_err = LocalApplyError::ChangeAlreadyApplied {
        hash: Hash::of(b"test"),
    };
    let insert_err: InsertError = local_err.into();
    assert!(matches!(insert_err, InsertError::AlreadyApplied { .. }));

    let local_err = LocalApplyError::DependencyMissing {
        hash: Hash::of(b"dep"),
    };
    let insert_err: InsertError = local_err.into();
    assert!(matches!(
        insert_err,
        InsertError::MissingDependencies { .. }
    ));
}

// Compute Insert Order Tests

#[test]
fn test_compute_insert_order_empty() {
    let changes = std::collections::HashMap::new();
    let order = compute_insert_order(&changes).unwrap();
    assert!(order.is_empty());
}

#[test]
fn test_format_hashes() {
    let hashes = vec![Hash::of(b"a"), Hash::of(b"b")];
    let formatted = format_hashes(&hashes);
    assert!(formatted.contains(","));
}

#[test]
fn test_format_hashes_empty() {
    let hashes: Vec<Hash> = vec![];
    let formatted = format_hashes(&hashes);
    assert!(formatted.is_empty());
}

#[test]
fn test_format_hashes_single() {
    let hashes = vec![Hash::of(b"single")];
    let formatted = format_hashes(&hashes);
    assert!(!formatted.contains(","));
    assert!(!formatted.is_empty());
}

// CrossViewInsertOptions Tests

#[test]
fn test_cross_view_options_new() {
    let opts = CrossViewInsertOptions::new("feature", "main");
    assert_eq!(opts.from_view, "feature");
    assert_eq!(opts.to_view, "main");
    assert!(opts.up_to_tag.is_none());
    assert!(opts.only_changes.is_empty());
    assert!(opts.apply_dependencies);
    assert!(!opts.allow_conflicts);
    assert!(!opts.dry_run);
}

#[test]
fn test_cross_view_options_up_to_tag() {
    let opts = CrossViewInsertOptions::new("feature", "main").up_to_tag("v1.0.0");
    assert_eq!(opts.up_to_tag, Some("v1.0.0".to_string()));
}

#[test]
fn test_cross_view_options_only_changes() {
    let hash1 = Hash::of(b"change1");
    let hash2 = Hash::of(b"change2");
    let opts = CrossViewInsertOptions::new("feature", "main").only_changes(vec![hash1, hash2]);
    assert_eq!(opts.only_changes.len(), 2);
}

#[test]
fn test_cross_view_options_builder() {
    let opts = CrossViewInsertOptions::new("feature", "main")
        .with_dependencies(false)
        .allow_conflicts(true)
        .dry_run(true);

    assert!(!opts.apply_dependencies);
    assert!(opts.allow_conflicts);
    assert!(opts.dry_run);
}

// CrossViewInsertOutcome Tests

#[test]
fn test_cross_view_outcome_new() {
    let outcome = CrossViewInsertOutcome::new();
    assert_eq!(outcome.changes_applied, 0);
    assert!(outcome.applied_hashes.is_empty());
    assert!(outcome.skipped_hashes.is_empty());
    assert_eq!(outcome.new_state, Merkle::ZERO);
    assert_eq!(outcome.sequence, 0);
    assert!(!outcome.has_conflicts);
    assert!(!outcome.was_dry_run);
}

#[test]
fn test_cross_view_outcome_default() {
    let outcome = CrossViewInsertOutcome::default();
    assert!(!outcome.has_applied());
    assert_eq!(outcome.total_processed(), 0);
}

#[test]
fn test_cross_view_outcome_has_applied() {
    let mut outcome = CrossViewInsertOutcome::new();
    assert!(!outcome.has_applied());

    outcome.changes_applied = 1;
    assert!(outcome.has_applied());
}

#[test]
fn test_cross_view_outcome_total_processed() {
    let mut outcome = CrossViewInsertOutcome::new();
    outcome.applied_hashes.push(Hash::of(b"a"));
    outcome.applied_hashes.push(Hash::of(b"b"));
    outcome.skipped_hashes.push(Hash::of(b"c"));

    assert_eq!(outcome.total_processed(), 3);
}
