#![allow(unused_imports)]
use super::*;
use crate::crdt::BranchId;
use crate::diff::Algorithm;
// ------------------------------------------------------------------------
// AnalysisOptions Tests
// ------------------------------------------------------------------------

#[test]
fn test_analysis_options_default() {
    let opts = AnalysisOptions::default();
    assert_eq!(opts.algorithm(), Algorithm::Myers);
    assert!(!opts.detect_moves());
    assert!(opts.whitespace_significant());
    assert!(opts.analyze_tokens());
}

#[test]
fn test_analysis_options_new() {
    let opts = AnalysisOptions::new();
    assert_eq!(opts.algorithm(), Algorithm::Myers);
}

#[test]
fn test_analysis_options_builder_algorithm() {
    let opts = AnalysisOptions::new().with_algorithm(Algorithm::Patience);
    assert_eq!(opts.algorithm(), Algorithm::Patience);
}

#[test]
fn test_analysis_options_builder_detect_moves() {
    let opts = AnalysisOptions::new().with_detect_moves(true);
    assert!(opts.detect_moves());
}

#[test]
fn test_analysis_options_builder_whitespace_significant() {
    let opts = AnalysisOptions::new().with_whitespace_significant(false);
    assert!(!opts.whitespace_significant());
}

#[test]
fn test_analysis_options_builder_analyze_tokens() {
    let opts = AnalysisOptions::new().with_analyze_tokens(false);
    assert!(!opts.analyze_tokens());
}

#[test]
fn test_analysis_options_builder_modification_threshold() {
    let opts = AnalysisOptions::new().with_modification_threshold(0.75);
    assert!((opts.modification_threshold() - 0.75).abs() < 0.001);
}

#[test]
#[should_panic(expected = "modification_threshold must be in range")]
fn test_analysis_options_invalid_threshold_high() {
    let _ = AnalysisOptions::new().with_modification_threshold(1.5);
}

#[test]
#[should_panic(expected = "modification_threshold must be in range")]
fn test_analysis_options_invalid_threshold_low() {
    let _ = AnalysisOptions::new().with_modification_threshold(-0.1);
}

#[test]
fn test_analysis_options_builder_chain() {
    let opts = AnalysisOptions::new()
        .with_algorithm(Algorithm::Patience)
        .with_detect_moves(true)
        .with_whitespace_significant(false)
        .with_analyze_tokens(false)
        .with_modification_threshold(0.8);

    assert_eq!(opts.algorithm(), Algorithm::Patience);
    assert!(opts.detect_moves());
    assert!(!opts.whitespace_significant());
    assert!(!opts.analyze_tokens());
    assert!((opts.modification_threshold() - 0.8).abs() < 0.001);
}

// ------------------------------------------------------------------------
// LineChangeKind Tests
// ------------------------------------------------------------------------

#[test]
fn test_line_change_kind_is_methods() {
    assert!(LineChangeKind::Equal.is_equal());
    assert!(!LineChangeKind::Equal.is_insert());

    assert!(LineChangeKind::Insert.is_insert());
    assert!(!LineChangeKind::Insert.is_delete());

    assert!(LineChangeKind::Delete.is_delete());
    assert!(!LineChangeKind::Delete.is_modify());

    assert!(LineChangeKind::Modify.is_modify());
    assert!(!LineChangeKind::Modify.is_move());

    assert!(LineChangeKind::Move.is_move());
    assert!(!LineChangeKind::Move.is_equal());
}

#[test]
fn test_line_change_kind_affects_old() {
    assert!(LineChangeKind::Equal.affects_old());
    assert!(!LineChangeKind::Insert.affects_old());
    assert!(LineChangeKind::Delete.affects_old());
    assert!(LineChangeKind::Modify.affects_old());
    assert!(LineChangeKind::Move.affects_old());
}

#[test]
fn test_line_change_kind_affects_new() {
    assert!(LineChangeKind::Equal.affects_new());
    assert!(LineChangeKind::Insert.affects_new());
    assert!(!LineChangeKind::Delete.affects_new());
    assert!(LineChangeKind::Modify.affects_new());
    assert!(LineChangeKind::Move.affects_new());
}

#[test]
fn test_line_change_kind_name() {
    assert_eq!(LineChangeKind::Equal.name(), "equal");
    assert_eq!(LineChangeKind::Insert.name(), "insert");
    assert_eq!(LineChangeKind::Delete.name(), "delete");
    assert_eq!(LineChangeKind::Modify.name(), "modify");
    assert_eq!(LineChangeKind::Move.name(), "move");
}

#[test]
fn test_line_change_kind_as_char() {
    assert_eq!(LineChangeKind::Equal.as_char(), '=');
    assert_eq!(LineChangeKind::Insert.as_char(), '+');
    assert_eq!(LineChangeKind::Delete.as_char(), '-');
    assert_eq!(LineChangeKind::Modify.as_char(), '~');
    assert_eq!(LineChangeKind::Move.as_char(), '>');
}

#[test]
fn test_line_change_kind_display() {
    assert_eq!(format!("{}", LineChangeKind::Insert), "insert");
}

// ------------------------------------------------------------------------
// LineChange Tests
// ------------------------------------------------------------------------

#[test]
fn test_line_change_equal() {
    let change = LineChange::equal(5, 5, b"content".to_vec());
    assert!(change.kind().is_equal());
    assert_eq!(change.old_index(), Some(5));
    assert_eq!(change.new_index(), Some(5));
    assert_eq!(change.old_content(), Some(b"content".as_slice()));
    assert_eq!(change.new_content(), Some(b"content".as_slice()));
}

#[test]
fn test_line_change_insert() {
    let change = LineChange::insert(3, b"new line".to_vec());
    assert!(change.kind().is_insert());
    assert_eq!(change.old_index(), None);
    assert_eq!(change.new_index(), Some(3));
    assert_eq!(change.old_content(), None);
    assert_eq!(change.new_content(), Some(b"new line".as_slice()));
}

#[test]
fn test_line_change_delete() {
    let change = LineChange::delete(7, b"old line".to_vec());
    assert!(change.kind().is_delete());
    assert_eq!(change.old_index(), Some(7));
    assert_eq!(change.new_index(), None);
    assert_eq!(change.old_content(), Some(b"old line".as_slice()));
    assert_eq!(change.new_content(), None);
}

#[test]
fn test_line_change_modify() {
    let change = LineChange::modify(2, 2, b"old".to_vec(), b"new".to_vec());
    assert!(change.kind().is_modify());
    assert_eq!(change.old_index(), Some(2));
    assert_eq!(change.new_index(), Some(2));
    assert_eq!(change.old_content(), Some(b"old".as_slice()));
    assert_eq!(change.new_content(), Some(b"new".as_slice()));
}

#[test]
fn test_line_change_moved() {
    let change = LineChange::moved(1, 5, b"moved line".to_vec());
    assert!(change.kind().is_move());
    assert_eq!(change.old_index(), Some(1));
    assert_eq!(change.new_index(), Some(5));
}

#[test]
fn test_line_change_with_existing_branch() {
    use crate::types::NodeId;
    let branch_id = BranchId::new(NodeId::new(1), 0);
    let change = LineChange::delete(0, b"line".to_vec()).with_existing_branch(branch_id);
    assert_eq!(change.existing_branch(), Some(branch_id));
}

#[test]
fn test_line_change_content_str() {
    let change = LineChange::modify(0, 0, b"old text".to_vec(), b"new text".to_vec());
    assert_eq!(change.old_content_str().unwrap(), "old text");
    assert_eq!(change.new_content_str().unwrap(), "new text");
}

#[test]
fn test_line_change_needs_content() {
    let insert = LineChange::insert(0, b"new".to_vec());
    let delete = LineChange::delete(0, b"old".to_vec());
    let modify = LineChange::modify(0, 0, b"old".to_vec(), b"new".to_vec());
    let equal = LineChange::equal(0, 0, b"same".to_vec());

    assert!(insert.needs_content());
    assert!(!delete.needs_content());
    assert!(modify.needs_content());
    assert!(!equal.needs_content());
}

#[test]
fn test_line_change_content_to_store() {
    let insert = LineChange::insert(0, b"new content".to_vec());
    assert_eq!(insert.content_to_store(), Some(b"new content".as_slice()));

    let delete = LineChange::delete(0, b"old".to_vec());
    assert_eq!(delete.content_to_store(), None);
}

#[test]
fn test_line_change_display() {
    let insert = LineChange::insert(5, b"x".to_vec());
    let display = format!("{}", insert);
    assert!(display.contains("+"));
    assert!(display.contains("new:5"));

    let delete = LineChange::delete(3, b"y".to_vec());
    let display = format!("{}", delete);
    assert!(display.contains("-"));
    assert!(display.contains("old:3"));
}

// ------------------------------------------------------------------------
// AnalysisStats Tests
// ------------------------------------------------------------------------

#[test]
fn test_analysis_stats_new() {
    let stats = AnalysisStats::new();
    assert_eq!(stats.old_lines, 0);
    assert_eq!(stats.total_changes, 0);
}

#[test]
fn test_analysis_stats_add_change() {
    let mut stats = AnalysisStats::new();

    stats.add_change(&LineChange::equal(0, 0, b"x".to_vec()));
    assert_eq!(stats.equal_lines, 1);
    assert_eq!(stats.total_changes, 0);

    stats.add_change(&LineChange::insert(1, b"y".to_vec()));
    assert_eq!(stats.inserted_lines, 1);
    assert_eq!(stats.total_changes, 1);

    stats.add_change(&LineChange::delete(2, b"z".to_vec()));
    assert_eq!(stats.deleted_lines, 1);
    assert_eq!(stats.total_changes, 2);

    stats.add_change(&LineChange::modify(3, 3, b"a".to_vec(), b"b".to_vec()));
    assert_eq!(stats.modified_lines, 1);
    assert_eq!(stats.total_changes, 3);
}

#[test]
fn test_analysis_stats_change_percentage() {
    let mut stats = AnalysisStats::new();
    stats.old_lines = 10;
    stats.new_lines = 10;
    stats.total_changes = 5;

    assert!((stats.change_percentage() - 50.0).abs() < 0.001);
}

#[test]
fn test_analysis_stats_change_percentage_empty() {
    let stats = AnalysisStats::new();
    assert!((stats.change_percentage() - 0.0).abs() < 0.001);
}

#[test]
fn test_analysis_stats_has_changes() {
    let mut stats = AnalysisStats::new();
    assert!(!stats.has_changes());

    stats.total_changes = 1;
    assert!(stats.has_changes());
}

#[test]
fn test_analysis_stats_net_line_change() {
    let mut stats = AnalysisStats::new();
    stats.old_lines = 5;
    stats.new_lines = 8;
    assert_eq!(stats.net_line_change(), 3);

    stats.new_lines = 3;
    assert_eq!(stats.net_line_change(), -2);
}

#[test]
fn test_analysis_stats_display() {
    let mut stats = AnalysisStats::new();
    stats.total_changes = 5;
    stats.inserted_lines = 2;
    stats.deleted_lines = 1;
    stats.modified_lines = 2;
    stats.equal_lines = 10;
    stats.old_lines = 15;
    stats.new_lines = 16;

    let display = format!("{}", stats);
    assert!(display.contains("5 changes"));
    assert!(display.contains("+2"));
    assert!(display.contains("-1"));
}

// ------------------------------------------------------------------------
// LineAnalysis Tests
// ------------------------------------------------------------------------

#[test]
fn test_line_analysis_accessors() {
    let changes = vec![
        LineChange::equal(0, 0, b"a".to_vec()),
        LineChange::insert(1, b"b".to_vec()),
    ];
    let mut stats = AnalysisStats::new();
    for c in &changes {
        stats.add_change(c);
    }
    let analysis = LineAnalysis::new(changes, stats, AnalysisOptions::default());

    assert_eq!(analysis.change_count(), 2);
    assert!(analysis.has_changes());
    assert_eq!(analysis.stats().inserted_lines, 1);
}

#[test]
fn test_line_analysis_filters() {
    let changes = vec![
        LineChange::equal(0, 0, b"a".to_vec()),
        LineChange::insert(1, b"b".to_vec()),
        LineChange::delete(2, b"c".to_vec()),
        LineChange::modify(3, 2, b"d".to_vec(), b"e".to_vec()),
    ];
    let stats = AnalysisStats::new();
    let analysis = LineAnalysis::new(changes, stats, AnalysisOptions::default());

    assert_eq!(analysis.equals().count(), 1);
    assert_eq!(analysis.inserts().count(), 1);
    assert_eq!(analysis.deletes().count(), 1);
    assert_eq!(analysis.modifies().count(), 1);
}

#[test]
fn test_line_analysis_into_changes() {
    let changes = vec![LineChange::insert(0, b"x".to_vec())];
    let analysis = LineAnalysis::new(changes, AnalysisStats::new(), AnalysisOptions::default());
    let owned = analysis.into_changes();
    assert_eq!(owned.len(), 1);
}

// ------------------------------------------------------------------------
// LineAnalyzer Tests
// ------------------------------------------------------------------------

#[test]
fn test_line_analyzer_split_lines_empty() {
    let lines = LineAnalyzer::split_lines(b"");
    assert!(lines.is_empty());
}

#[test]
fn test_line_analyzer_split_lines_single() {
    let lines = LineAnalyzer::split_lines(b"hello");
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0], b"hello");
}

#[test]
fn test_line_analyzer_split_lines_multiple() {
    let lines = LineAnalyzer::split_lines(b"a\nb\nc");
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0], b"a");
    assert_eq!(lines[1], b"b");
    assert_eq!(lines[2], b"c");
}

#[test]
fn test_line_analyzer_split_lines_trailing_newline() {
    let lines = LineAnalyzer::split_lines(b"a\nb\n");
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0], b"a");
    assert_eq!(lines[1], b"b");
    assert_eq!(lines[2], b"");
}

#[test]
fn test_line_analyzer_identical_content() {
    let content = b"line one\nline two\n";
    let analyzer = LineAnalyzer::new(content, content, AnalysisOptions::default());
    let analysis = analyzer.analyze();

    assert!(!analysis.has_changes());
    // Two lines of content (trailing newline doesn't create empty line in diff)
    assert!(analysis.stats().equal_lines >= 2);
}

#[test]
fn test_line_analyzer_all_inserted() {
    let old = b"";
    let new = b"new line\n";
    let analyzer = LineAnalyzer::new(old, new, AnalysisOptions::default());
    let analysis = analyzer.analyze();

    assert!(analysis.has_changes());
    // At least one line inserted
    assert!(analysis.stats().inserted_lines >= 1);
}

#[test]
fn test_line_analyzer_all_deleted() {
    let old = b"old line\n";
    let new = b"";
    let analyzer = LineAnalyzer::new(old, new, AnalysisOptions::default());
    let analysis = analyzer.analyze();

    assert!(analysis.has_changes());
    // At least one line deleted
    assert!(analysis.stats().deleted_lines >= 1);
}

#[test]
fn test_line_analyzer_simple_modification() {
    let old = b"unchanged\nold line\nunchanged\n";
    let new = b"unchanged\nnew line\nunchanged\n";
    let analyzer = LineAnalyzer::new(old, new, AnalysisOptions::default());
    let analysis = analyzer.analyze();

    assert!(analysis.has_changes());
    // The middle line is modified (detected as delete+insert or modify)
    assert!(
        analysis.stats().modified_lines >= 1
            || (analysis.stats().deleted_lines >= 1 && analysis.stats().inserted_lines >= 1)
    );
}

#[test]
fn test_line_analyzer_insert_at_end() {
    let old = b"line one\n";
    let new = b"line one\nline two\n";
    let analyzer = LineAnalyzer::new(old, new, AnalysisOptions::default());
    let analysis = analyzer.analyze();

    assert!(analysis.has_changes());
    assert!(analysis.stats().inserted_lines >= 1);
}

#[test]
fn test_line_analyzer_delete_from_middle() {
    let old = b"a\nb\nc\n";
    let new = b"a\nc\n";
    let analyzer = LineAnalyzer::new(old, new, AnalysisOptions::default());
    let analysis = analyzer.analyze();

    assert!(analysis.has_changes());
    assert_eq!(analysis.stats().deleted_lines, 1);
}

#[test]
fn test_line_analyzer_with_defaults() {
    let analyzer = LineAnalyzer::with_defaults(b"a", b"b");
    assert_eq!(analyzer.options().algorithm(), Algorithm::Myers);
}

#[test]
fn test_line_analyzer_content_accessors() {
    let old = b"old";
    let new = b"new";
    let analyzer = LineAnalyzer::new(old, new, AnalysisOptions::default());

    assert_eq!(analyzer.old_content(), old);
    assert_eq!(analyzer.new_content(), new);
}

// ------------------------------------------------------------------------
// Integration Tests
// ------------------------------------------------------------------------

#[test]
fn test_integration_code_change() {
    let old = b"fn foo() {\n    return 1;\n}\n";
    let new = b"fn foo() {\n    return 2;\n}\n";

    let analyzer = LineAnalyzer::new(old, new, AnalysisOptions::default());
    let analysis = analyzer.analyze();

    assert!(analysis.has_changes());
    // First and last lines unchanged, middle modified
    assert_eq!(analysis.stats().equal_lines, 2);
}

#[test]
fn test_integration_multiple_changes() {
    let old = b"a\nb\nc\nd\ne\n";
    let new = b"a\nB\nc\nD\ne\nnew\n";

    let analyzer = LineAnalyzer::new(old, new, AnalysisOptions::default());
    let analysis = analyzer.analyze();

    assert!(analysis.has_changes());
    // a, c, e unchanged = 3, plus 1 empty from trailing newline matched
    // b->B, d->D modified or delete+insert
    // "new" inserted
}

#[test]
fn test_integration_patience_algorithm() {
    let old = b"fn main() {\n}\n";
    let new = b"fn main() {\n    println!(\"hello\");\n}\n";

    let options = AnalysisOptions::default().with_algorithm(Algorithm::Patience);
    let analyzer = LineAnalyzer::new(old, new, options);
    let analysis = analyzer.analyze();

    assert!(analysis.has_changes());
    assert!(analysis.stats().inserted_lines >= 1);
}
