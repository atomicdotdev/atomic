//! Regression tests for POMO-1: recording a small, targeted edit to a
//! large file should produce a small, proportional diff — not treat most
//! of the file as newly-inserted content, which duplicates already-alive
//! content in the graph once materialized.
//!
//! Reported symptom (pomodoro-2 project, intent POMO-1): `main.go` ended
//! up with every top-level function defined twice after a sequence of
//! small, targeted edits ("increase header size", "match font size",
//! "sort time options ascending"). `atomic change <hash>` stats on that
//! history showed 195-604 hunks per edit, almost all "new content"
//! inserts with only 1-2 replace/delete hunks, for edits that should have
//! touched a handful of lines.

use super::*;
use crate::record::RecordOptions;
use atomic_core::change::ChangeHeader;

fn record_all(repo: &Repository, message: &str) {
    let header = ChangeHeader::new(message);
    let options = RecordOptions::new()
        .with_all(true)
        .save_to_store(true)
        .apply_after_record(true);
    repo.record(header, options).unwrap();
}

fn count_occurrences(content: &str, pattern: &str) -> usize {
    content.matches(pattern).count()
}

/// Build a Go-like source file with `count` distinct top-level functions,
/// each with a unique, greppable marker line. Mirrors the shape of the
/// real-world file (`main.go`) that triggered this investigation.
fn build_go_like_source(count: usize) -> String {
    let mut out = String::new();
    out.push_str("package main\n\n");
    for i in 0..count {
        out.push_str(&format!(
            "func step{i}() int {{\n    // marker[{i}]\n    return {i}\n}}\n\n"
        ));
    }
    out
}

/// Single-view baseline (no cross-view merge involved): record a
/// 120-function file, then make one targeted one-line edit deep in the
/// file and record again. The second record's hunk count should be a
/// handful, and the materialized file should contain each function
/// exactly once — not duplicated because the diff mistook unrelated,
/// unchanged functions for new content.
#[test]
fn test_small_edit_on_large_file_stays_proportional() {
    let (temp_dir, repo) = create_temp_repo();

    let initial = build_go_like_source(120);
    let file = temp_dir.path().join("main.go");
    std::fs::write(&file, &initial).unwrap();
    repo.add("main.go", TrackingOptions::default()).unwrap();
    record_all(&repo, "Add main.go");

    // Targeted edit: change the return value of exactly one function deep
    // in the file (step 60 of 120) — a single-line change.
    let edited = initial.replacen("return 60\n", "return 6000\n", 1);
    assert_ne!(initial, edited, "edit must actually change the file");
    std::fs::write(&file, &edited).unwrap();

    let outcome = repo
        .record(
            ChangeHeader::new("Bump step60 return value"),
            RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true),
        )
        .unwrap();

    let hunk_count = outcome.change().hunks().len();
    assert!(
        hunk_count <= 10,
        "a one-line edit should produce a handful of hunks, not {} \
         (this is the 'almost the whole file is rewritten' bug — POMO-1)",
        hunk_count
    );

    let content = repo
        .get_file_content("main.go")
        .unwrap()
        .expect("file should exist");
    let content = String::from_utf8(content).unwrap();

    assert_eq!(
        content, edited,
        "materialized content should be exactly the edited file, \
         byte-for-byte — any divergence here is duplicated or lost content"
    );

    for i in 0..120 {
        let marker = format!("// marker[{i}]");
        let occurrences = count_occurrences(&content, &marker);
        assert_eq!(
            occurrences, 1,
            "step{} should appear exactly once, found {} (duplication!)",
            i, occurrences
        );
    }
}

/// Same as above, but repeats several small, independent single-line
/// edits in sequence — mirroring the real-world history: "increase header
/// size", "match font size", "sort time options ascending" (a series of
/// small, targeted commits). Each record's hunk count must stay small;
/// this also catches a bug that only compounds after 2+ edits.
#[test]
fn test_sequential_small_edits_do_not_compound_duplication() {
    let (temp_dir, repo) = create_temp_repo();

    let mut current = build_go_like_source(150);
    let file = temp_dir.path().join("main.go");
    std::fs::write(&file, &current).unwrap();
    repo.add("main.go", TrackingOptions::default()).unwrap();
    record_all(&repo, "Add main.go");

    let edits = [
        (10, 1000, "Bump step10"),
        (75, 7500, "Bump step75"),
        (140, 14000, "Bump step140"),
        (2, 200, "Bump step2"),
    ];

    for (idx, new_val, message) in edits {
        let from = format!("return {idx}\n");
        let to = format!("return {new_val}\n");
        let next = current.replacen(&from, &to, 1);
        assert_ne!(current, next, "edit for step{idx} must change the file");
        current = next;
        std::fs::write(&file, &current).unwrap();

        let outcome = repo
            .record(
                ChangeHeader::new(message),
                RecordOptions::new()
                    .with_all(true)
                    .save_to_store(true)
                    .apply_after_record(true),
            )
            .unwrap();

        let hunk_count = outcome.change().hunks().len();
        assert!(
            hunk_count <= 10,
            "'{}' should produce a handful of hunks, not {} \
             (whole-file rewrite detected — POMO-1)",
            message,
            hunk_count
        );
    }

    let content = repo
        .get_file_content("main.go")
        .unwrap()
        .expect("file should exist");
    let content = String::from_utf8(content).unwrap();

    assert_eq!(
        content, current,
        "materialized content should match the final edited file exactly"
    );

    for i in 0..150 {
        let marker = format!("// marker[{i}]");
        let occurrences = count_occurrences(&content, &marker);
        assert_eq!(
            occurrences,
            1,
            "step{} should appear exactly once after {} sequential edits, \
             found {} (duplication compounding — POMO-1)",
            i,
            edits.len(),
            occurrences
        );
    }
}

/// Cross-view variant of the same property, at a scale the existing
/// `cross_view_merge_tests` suite doesn't cover: a large file (well
/// beyond the ~10-20 line fixtures used there) with divergent edits on a
/// draft and its shared parent, merged and then edited again. If
/// duplication is specific to large diffs (e.g. a size-gated code path),
/// this is expected to catch it where the small-file tests do not.
#[test]
fn test_cross_view_merge_large_file_post_merge_record_clean() {
    use crate::apply::CrossViewInsertOptions;

    let (temp_dir, mut repo) = create_temp_repo();

    let initial = build_go_like_source(120);
    let file = temp_dir.path().join("main.go");
    std::fs::write(&file, &initial).unwrap();
    repo.add("main.go", TrackingOptions::default()).unwrap();
    record_all(&repo, "Add main.go");

    repo.create_view_from("feature", "dev").unwrap();

    // Draft: edit step 30.
    repo.switch_view("feature").unwrap();
    let draft = initial.replacen("return 30\n", "return 3000\n", 1);
    std::fs::write(&file, &draft).unwrap();
    record_all(&repo, "Bump step30 on feature");

    // Dev: edit step 90 (non-overlapping section of the file).
    repo.switch_view("dev").unwrap();
    let dev_edit = initial.replacen("return 90\n", "return 9000\n", 1);
    std::fs::write(&file, &dev_edit).unwrap();
    record_all(&repo, "Bump step90 on dev");

    // Merge feature -> dev.
    repo.insert_from_view(CrossViewInsertOptions::new("feature", "dev"))
        .unwrap();
    repo.materialize().unwrap();

    // Post-merge edit on dev: a third, unrelated single-line change.
    repo.switch_view("dev").unwrap();
    let post_merge_base = std::fs::read_to_string(&file).unwrap();
    let post_merge = post_merge_base.replacen("return 60\n", "return 6000\n", 1);
    assert_ne!(
        post_merge_base, post_merge,
        "post-merge edit must actually change the file"
    );
    std::fs::write(&file, &post_merge).unwrap();

    let outcome = repo
        .record(
            ChangeHeader::new("Bump step60 after merge"),
            RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true),
        )
        .unwrap();

    let hunk_count = outcome.change().hunks().len();
    assert!(
        hunk_count <= 10,
        "the post-merge edit should produce a handful of hunks, not {} \
         (whole-file rewrite detected on a large post-merge file — POMO-1)",
        hunk_count
    );

    let content = repo
        .get_file_content_on_view("main.go", "dev")
        .unwrap()
        .expect("file should exist on dev");
    let content = String::from_utf8(content).unwrap();

    for i in 0..120 {
        let marker = format!("// marker[{i}]");
        let occurrences = count_occurrences(&content, &marker);
        assert_eq!(
            occurrences, 1,
            "step{} should appear exactly once after merge + post-merge edit, \
             found {} (duplication!)",
            i, occurrences
        );
    }

    assert!(
        content.contains("return 3000"),
        "feature's edit should survive the merge"
    );
    assert!(
        content.contains("return 9000"),
        "dev's edit should survive the merge"
    );
    assert!(
        content.contains("return 6000"),
        "post-merge edit should be present"
    );
}

/// Build a slice-literal-style block of `count` lines, one per value in
/// `order` (a permutation of `0..count`). Mirrors the real-world change
/// that triggered this investigation ("Sort time options ascending by
/// time value") much more closely than a single-token edit: reordering
/// is a pure permutation of already-alive lines, not a content change.
fn build_ordered_options(order: &[usize]) -> String {
    let mut out = String::new();
    out.push_str("package main\n\nvar timeOptions = []int{\n");
    for v in order {
        out.push_str(&format!("\ttimeOption{v}, // marker[{v}]\n"));
    }
    out.push_str("}\n");
    out
}

/// The specific operation behind the real-world regression: re-sorting a
/// large block of otherwise-unchanged lines. A pure permutation has no
/// content change at all — every line in the "sorted" version already
/// existed, verbatim, in the "scrambled" version. If the record pipeline
/// treats reordered-but-unchanged lines as brand-new content instead of
/// recognizing them, the old (pre-sort) lines never get deleted and the
/// materialized file ends up with every line twice — which is exactly
/// what shipped in this project's `main.go` after "Sort time options
/// ascending by time value" (see intent POMO-1).
#[test]
fn test_sorting_lines_does_not_duplicate_them() {
    let (temp_dir, repo) = create_temp_repo();

    const N: usize = 60;
    // A fixed, deliberately unsorted permutation of 0..N (not reversed —
    // reversed order can trivially collapse to a short LCS in a way that
    // isn't representative; this interleaves low/high values instead).
    let scrambled: Vec<usize> = (0..N)
        .map(|i| if i % 2 == 0 { i / 2 } else { N - 1 - i / 2 })
        .collect();
    let sorted: Vec<usize> = (0..N).collect();

    let initial = build_ordered_options(&scrambled);
    let file = temp_dir.path().join("main.go");
    std::fs::write(&file, &initial).unwrap();
    repo.add("main.go", TrackingOptions::default()).unwrap();
    record_all(&repo, "Add main.go with unsorted time options");

    let resorted = build_ordered_options(&sorted);
    assert_ne!(initial, resorted, "sorting must actually change the file");
    std::fs::write(&file, &resorted).unwrap();

    let outcome = repo
        .record(
            ChangeHeader::new("Sort time options ascending by time value"),
            RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true),
        )
        .unwrap();

    let hunk_count = outcome.change().hunks().len();
    println!(
        "test_sorting_lines_does_not_duplicate_them: sort of {} lines produced {} hunks",
        N, hunk_count
    );

    let content = repo
        .get_file_content("main.go")
        .unwrap()
        .expect("file should exist");
    let content = String::from_utf8(content).unwrap();

    // The real bug: every value's line should appear exactly ONCE after
    // the sort, not twice (old unsorted copy + new sorted copy both alive).
    let mut duplicated = Vec::new();
    for v in 0..N {
        let marker = format!("// marker[{v}]");
        let occurrences = count_occurrences(&content, &marker);
        if occurrences != 1 {
            duplicated.push((v, occurrences));
        }
    }
    assert!(
        duplicated.is_empty(),
        "sorting should not duplicate lines, but these values have the wrong \
         occurrence count (value, count): {:?}\nhunk_count={}\ncontent:\n{}",
        duplicated,
        hunk_count,
        content
    );

    assert_eq!(
        content, resorted,
        "materialized content should be exactly the sorted file, byte-for-byte"
    );
}

/// POMO-2: after a file has been through the orphan-view duplication bug
/// (POMO-1) and the resulting duplicate is merged into a shared view, does a
/// *further*, ordinary in-place edit still get detected and recorded
/// correctly? In the real project, a subsequent edit to the (already
/// duplicated) `main.go` was detected by `atomic status` but produced a
/// change with zero hunks for the file — `atomic record`/`atomic diff`
/// silently disagreed with `status` about whether anything had changed.
///
/// This reproduces the orphan-view mechanism directly at the `Repository`
/// level (bypassing `record_turn()`, which is now self-healing per POMO-1's
/// fix and would refuse to create the orphan) to recreate the exact lineage,
/// then attempts one more plain, ordinary edit on top and checks whether
/// `status` and `record` agree about it.
#[test]
fn test_further_edit_after_orphan_view_merge_is_still_detected() {
    use crate::apply::CrossViewInsertOptions;
    use crate::record::RecordError;
    use crate::status::StatusOptions;

    let (temp_dir, repo) = create_temp_repo();
    let file = temp_dir.path().join("main.go");

    // Step 1: base content, recorded normally on dev.
    let initial = build_go_like_source(80);
    std::fs::write(&file, &initial).unwrap();
    repo.add("main.go", TrackingOptions::default()).unwrap();
    record_all(&repo, "Add main.go");

    // Step 2: simulate an orphaned session view directly — the exact
    // low-level mechanism from POMO-1. `RecordOptions::view("orphan-xyz")`
    // with a view name that doesn't exist yet reaches `open_or_create_view`'s
    // parentless-Shared fallback, since nothing here calls
    // `create_view_from` first (unlike a properly-forked session).
    let edited = initial.replacen("return 40\n", "return 4000\n", 1);
    std::fs::write(&file, &edited).unwrap();
    repo.record(
        ChangeHeader::new("orphan edit"),
        RecordOptions::new()
            .with_all(true)
            .view("orphan-xyz")
            .apply_after_record(true)
            .save_to_store(true),
    )
    .expect("orphan record should succeed (it's the duplication bug, not a crash)");

    // Step 3: merge the orphan view into dev — this is what reproduces the
    // duplication (confirmed already by POMO-1's tests); not re-asserted here.
    repo.insert_from_view(CrossViewInsertOptions::new("orphan-xyz", "dev"))
        .unwrap();
    repo.materialize().unwrap();

    // Step 4: a further, ordinary, targeted edit on top of the now-merged
    // (duplicated) file — exactly the scenario that failed in the real
    // project. Does `status` and `record` agree about it?
    let current = std::fs::read_to_string(&file).unwrap();
    let further_edited = current.replacen("return 4000\n", "return 5000\n", 1);
    assert_ne!(
        current, further_edited,
        "the further edit must actually change the file"
    );
    std::fs::write(&file, &further_edited).unwrap();

    let status = repo
        .status(StatusOptions::default())
        .expect("status failed");
    let modified_paths: Vec<String> = status
        .modified()
        .map(|e| e.path().to_string_lossy().to_string())
        .collect();
    println!(
        "test_further_edit_after_orphan_view_merge_is_still_detected: status.modified = {:?}",
        modified_paths
    );

    let record_result = repo.record(
        ChangeHeader::new("further edit"),
        RecordOptions::new()
            .with_all(true)
            .apply_after_record(true)
            .save_to_store(true),
    );

    match record_result {
        Err(RecordError::NothingToRecord) => {
            panic!(
                "BUG REPRODUCED: record() found nothing to record, but status.modified = {:?} \
                 and the file content genuinely differs from pristine",
                modified_paths
            );
        }
        Err(e) => panic!("unexpected record error: {}", e),
        Ok(outcome) => {
            let recorded = outcome.recorded_files();
            println!(
                "test_further_edit_after_orphan_view_merge_is_still_detected: recorded_files = {:?}, hunk_count={}",
                recorded,
                outcome.change().hunks().len(),
            );
            assert!(
                recorded.iter().any(|p| p == "main.go"),
                "BUG REPRODUCED: record() succeeded but did not include main.go, even though \
                 status flagged it as modified (recorded_files = {:?})",
                recorded
            );
        }
    }

    // Independently of whether record() claimed success, verify the actual
    // materialized content matches the further edit — this is the
    // ground-truth check that would have caught the real bug (record()
    // silently no-op'ing while claiming nothing was wrong).
    repo.materialize().unwrap();
    let final_content = std::fs::read_to_string(&file).unwrap();
    let copies = count_occurrences(&final_content, "func step0() int {");
    println!(
        "test_further_edit_after_orphan_view_merge_is_still_detected: final copies of step0 = {}",
        copies
    );
    assert_eq!(
        final_content, further_edited,
        "BUG REPRODUCED: materialized content after record() does not match the further edit \
         that was actually made on disk"
    );
}

/// Companion to `test_further_edit_after_orphan_view_merge_is_still_detected`,
/// covering `globalize_delete`'s content-vertex deletion path instead of a
/// further edit.
///
/// Note this empties the file's *content* (writes zero bytes) rather than
/// removing it from tracking (`atomic remove` / deleting it off disk).
/// Untracking goes through a completely different mechanism
/// (`record_deleted_file` → a tree-level `GraphOp::FileDel`, driven by a
/// hardcoded single-line hunk) that doesn't exercise `globalize_delete`'s
/// content-vertex cleanup at all — confirmed empirically: instrumenting
/// `delete_all_content` and running that scenario, it was never even
/// called. Editing an already-tracked file down to empty is what actually
/// produces a `record_modified_file` diff with `deleted_lines` spanning the
/// file's true old range, which is what drives `globalize_delete` into its
/// whole-file fallback (`delete_all_content`) when the range exceeds what
/// the (buggy, pre-fix) linear-walk vertex count reports.
///
/// `globalize_delete`'s whole-file fallback shares `delete_all_content` with
/// `globalize_replace_whole_file` — both used to rely on a linear graph walk
/// that only follows one branch of a genuine fork (POMO-2's second bug
/// layer). This confirms emptying a file that went through the orphan-view
/// duplication lineage actually removes *every* alive copy, not just one —
/// which matters because a phantom surviving branch wouldn't show up in the
/// working copy (the file just looks empty either way) but would still be
/// live in the graph, ready to resurface via a later insert, push, or fork
/// from this view.
#[test]
fn test_emptying_file_after_orphan_view_merge_removes_every_copy() {
    use crate::apply::CrossViewInsertOptions;

    let (temp_dir, repo) = create_temp_repo();
    let file = temp_dir.path().join("main.go");

    // Step 1: base content, recorded normally on dev.
    let initial = build_go_like_source(80);
    std::fs::write(&file, &initial).unwrap();
    repo.add("main.go", TrackingOptions::default()).unwrap();
    record_all(&repo, "Add main.go");

    // Step 2: simulate an orphaned session view directly (POMO-1's
    // mechanism), producing a second, duplicate copy of the file's content
    // once merged.
    let edited = initial.replacen("return 40\n", "return 4000\n", 1);
    std::fs::write(&file, &edited).unwrap();
    repo.record(
        ChangeHeader::new("orphan edit"),
        RecordOptions::new()
            .with_all(true)
            .view("orphan-xyz")
            .apply_after_record(true)
            .save_to_store(true),
    )
    .expect("orphan record should succeed (it's the duplication bug, not a crash)");

    // Step 3: merge the orphan view into dev — reproduces the duplication.
    repo.insert_from_view(CrossViewInsertOptions::new("orphan-xyz", "dev"))
        .unwrap();
    repo.materialize().unwrap();

    // Step 4: empty the (now duplicated) file's content — still tracked,
    // just zero bytes — and record it.
    std::fs::write(&file, b"").unwrap();
    let outcome = repo
        .record(
            ChangeHeader::new("empty main.go"),
            RecordOptions::new()
                .with_all(true)
                .apply_after_record(true)
                .save_to_store(true),
        )
        .expect("emptying the file should record successfully");
    println!(
        "test_emptying_file_after_orphan_view_merge_removes_every_copy: recorded_files = {:?}",
        outcome.recorded_files()
    );

    // Step 5: ground truth — read the graph state directly for "dev" (no
    // disk materialization involved, so this can't be confused by whatever
    // the working copy happens to look like). If any copy survived, its
    // content would still be alive in the graph and this read would return
    // it — even though the working copy just looks like an empty file
    // either way, so inspecting disk after emptying it couldn't have caught
    // a phantom surviving branch.
    let content = repo.get_file_content_on_view("main.go", "dev").unwrap();
    let leftover_bytes = content.as_ref().map(|c| c.len()).unwrap_or(0);
    assert_eq!(
        leftover_bytes, 0,
        "BUG: main.go should have zero alive content bytes on 'dev' after being emptied, \
         but found {} — a duplicate copy from the orphan-view merge survived",
        leftover_bytes
    );
}
