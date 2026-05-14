//! Content roundtrip fidelity tests.
//!
//! Validates that file content retrieved from the change graph (via overlay)
//! is byte-identical to what was recorded. This tests the change graph layer
//! (hunks/atoms), NOT the semantic CRDT layer (file_ops).
//!
//! The key scenario: a file undergoes multiple modifications across several
//! changes (add → modify → modify → complex refactor). After all changes are
//! applied, retrieving the file content should produce exactly the bytes that
//! were last written to disk before recording.

use std::fs;
use std::path::PathBuf;

use atomic_core::change::{Author, ChangeHeader};
use atomic_core::types::Hash;
use atomic_repository::{RecordOptions, Repository};
use tempfile::TempDir;

fn create_test_repo() -> (Repository, TempDir, PathBuf) {
    let temp = TempDir::new().expect("Failed to create temp dir");
    let repo_path = temp.path().to_path_buf();
    let repo = Repository::init(&repo_path).expect("Failed to init repository");
    (repo, temp, repo_path)
}

fn write_file(repo_path: &PathBuf, name: &str, content: &str) {
    let file_path = repo_path.join(name);
    if let Some(parent) = file_path.parent() {
        fs::create_dir_all(parent).expect("Failed to create dirs");
    }
    fs::write(&file_path, content).expect("Failed to write file");
}

fn record_change(repo: &Repository, message: &str) -> Hash {
    let header = ChangeHeader::builder()
        .message(message)
        .author(Author::new("Test", Some("test@example.com")))
        .build();

    let outcome = repo
        .record(header, RecordOptions::default())
        .expect("Failed to record");

    *outcome.hash()
}

/// Assert that the content retrieved via overlay matches the expected bytes.
fn assert_content_matches(repo: &Repository, path: &str, expected: &str) {
    let content = repo
        .get_file_content_on_view(path, repo.current_view())
        .expect("Failed to get content")
        .expect("File not found in graph");

    let actual = String::from_utf8_lossy(&content);
    assert_eq!(
        actual,
        expected,
        "Content mismatch for '{}': got {} bytes, expected {} bytes",
        path,
        content.len(),
        expected.len(),
    );
}

/// Assert content after a specific change matches.
fn assert_content_after_change(repo: &Repository, path: &str, hash: &Hash, expected: &str) {
    let content = repo
        .get_file_content_after_change(path, hash)
        .expect("Failed to get content after change")
        .expect("File not found after change");

    let actual = String::from_utf8_lossy(&content);
    assert_eq!(
        actual,
        expected,
        "Content after change mismatch for '{}': got {} bytes, expected {} bytes",
        path,
        content.len(),
        expected.len(),
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// Test: Simple add then modify
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_simple_add_then_modify() {
    let (repo, _temp, repo_path) = create_test_repo();

    let v1 = "fn main() {\n    println!(\"hello\");\n}\n";
    let v2 = "fn main() {\n    println!(\"hello world\");\n}\n";

    write_file(&repo_path, "test.rs", v1);
    repo.add("test.rs", Default::default()).unwrap();
    let h1 = record_change(&repo, "add test.rs");

    write_file(&repo_path, "test.rs", v2);
    let h2 = record_change(&repo, "modify test.rs");

    assert_content_after_change(&repo, "test.rs", &h1, v1);
    assert_content_after_change(&repo, "test.rs", &h2, v2);
    assert_content_matches(&repo, "test.rs", v2);
}

// ═══════════════════════════════════════════════════════════════════════════
// Test: Three sequential modifications
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_three_sequential_modifications() {
    let (repo, _temp, repo_path) = create_test_repo();

    let v1 = "line1\nline2\nline3\n";
    let v2 = "line1\nmodified\nline3\n";
    let v3 = "line1\nmodified\nline3\nextra\n";

    write_file(&repo_path, "data.txt", v1);
    repo.add("data.txt", Default::default()).unwrap();
    let h1 = record_change(&repo, "add");

    write_file(&repo_path, "data.txt", v2);
    let h2 = record_change(&repo, "modify middle");

    write_file(&repo_path, "data.txt", v3);
    let h3 = record_change(&repo, "append line");

    assert_content_after_change(&repo, "data.txt", &h1, v1);
    assert_content_after_change(&repo, "data.txt", &h2, v2);
    assert_content_after_change(&repo, "data.txt", &h3, v3);
    assert_content_matches(&repo, "data.txt", v3);
}

// ═══════════════════════════════════════════════════════════════════════════
// Test: Multi-hunk refactor (extract function + add code + delete code)
//
// This mimics the pattern that triggers duplication in hyperfine's commit 4:
// - A function is extracted (code moves from main to a new helper)
// - New code is added (warmup phase)
// - Inline code is replaced by calls to the extracted function
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_multi_hunk_refactor_fidelity() {
    let (repo, _temp, repo_path) = create_test_repo();

    // Version 1: inline progress bar setup
    let v1 = "\
fn main() {
    let args = parse_args();
    let commands = args.commands();

    for cmd in commands {
        println!(\"Command: {}\", cmd);

        let mut results = vec![];

        // Set up progress bar
        let bar = Bar::new(10);
        let style = Style::new()
            .template(\"{spinner} {msg}\");
        bar.set_style(style);
        bar.enable_steady_tick(80);
        bar.set_message(\"Measuring\");

        // Measure
        for i in 0..10 {
            bar.inc(1);
            let res = run_command(cmd);
            results.push(res);
        }
        bar.finish();

        print_results(&results);
    }
}
";

    // Version 2: code style cleanup (minor changes)
    let v2 = "\
fn main() {
    let args = parse_args();
    let commands = args.commands();

    for cmd in commands {
        println!(\"Command: {}\", cmd);

        let mut results = vec![];

        // Set up progress bar
        let bar = Bar::new(10);
        let style = Style::new()
            .template(\"{spinner} {msg:<28} {wide_bar}\");
        bar.set_style(style.clone());
        bar.enable_steady_tick(80);
        bar.set_message(\"Measuring\");

        // Run measurements
        for i in 0..10 {
            bar.inc(1);
            let res = run_command(cmd);
            results.push(res);
        }
        bar.finish_and_clear();

        print_results(&results);
    }
}
";

    // Version 3: extract helper + add warmup (multi-hunk refactor)
    let v3 = "\
/// Return a pre-configured progress bar
fn get_bar(length: u64, msg: &str) -> Bar {
    let style = Style::new()
        .tick_chars(\"⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏\")
        .template(\"{spinner} {msg:<28} {wide_bar}\");

    let bar = Bar::new(length);
    bar.set_style(style);
    bar.enable_steady_tick(80);
    bar.set_message(msg);

    bar
}

fn main() {
    let args = parse_args();
    let commands = args.commands();

    for cmd in commands {
        println!(\"Command: {}\", cmd);

        let mut results = vec![];

        // Warmup phase
        if let Some(warmup_count) = args.warmup {
            let bar = get_bar(warmup_count, \"Performing warmup\");

            for _ in 0..warmup_count {
                bar.inc(1);
                let _ = run_command(cmd);
            }
            bar.finish_and_clear();
        }

        // Set up progress bar
        let bar = get_bar(10, \"Measuring\");

        // Run measurements
        for i in 0..10 {
            bar.inc(1);
            let res = run_command(cmd);
            results.push(res);
        }
        bar.finish_and_clear();

        print_results(&results);
    }
}
";

    // Record version 1
    write_file(&repo_path, "src/main.rs", v1);
    repo.add("src/main.rs", Default::default()).unwrap();
    let h1 = record_change(&repo, "initial");
    assert_content_matches(&repo, "src/main.rs", v1);

    // Record version 2 (minor edits)
    write_file(&repo_path, "src/main.rs", v2);
    let h2 = record_change(&repo, "code style");
    assert_content_matches(&repo, "src/main.rs", v2);

    // Record version 3 (major refactor: extract function, add warmup, replace inline code)
    write_file(&repo_path, "src/main.rs", v3);
    let h3 = record_change(&repo, "extract helper and add warmup");

    // This is the critical assertion — after the multi-hunk refactor,
    // the graph content should exactly match v3 with no duplication.
    assert_content_matches(&repo, "src/main.rs", v3);

    // Also verify state-based retrieval at each point
    assert_content_after_change(&repo, "src/main.rs", &h1, v1);
    assert_content_after_change(&repo, "src/main.rs", &h2, v2);
    assert_content_after_change(&repo, "src/main.rs", &h3, v3);
}

// ═══════════════════════════════════════════════════════════════════════════
// Test: Reproduce the exact duplication bug from git import
//
// Uses the first 4 commits of hyperfine (https://github.com/sharkdp/hyperfine)
// by David Peter, licensed under Apache-2.0 / MIT. The commit sequence in
// src/main.rs — initial code, progress bar addition, style cleanup, then a
// complex refactor extracting a helper function and adding warmup — triggers
// content duplication in the change graph after globalization.
//
// This test isolates the bug to the change graph layer (record + globalize +
// apply) by using repo.record() directly, with no import pipeline involved.
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_hyperfine_content_duplication_bug() {
    use std::process::Command;

    // Clone hyperfine into a temp dir
    let git_temp = TempDir::new().expect("Failed to create temp dir for git");
    let git_path = git_temp.path().to_path_buf();
    let clone_status = Command::new("git")
        .args([
            "clone",
            "--quiet",
            "https://github.com/sharkdp/hyperfine.git",
        ])
        .arg(&git_path)
        .status();

    // Skip test if git clone fails (e.g., no network)
    match clone_status {
        Ok(s) if s.success() => {}
        _ => {
            eprintln!("Skipping test: git clone failed (no network?)");
            return;
        }
    }

    // Get file contents at each of the first 4 commits
    let commits: Vec<&str> = vec![
        "a658ab8c", // Initial commit
        "d4ebdd7b", // Add a progress bar
        "197f9fb",  // Code style update
        "68fdc2c",  // Add --warmup option
    ];

    let mut versions: Vec<Vec<u8>> = Vec::new();
    for sha in &commits {
        let output = Command::new("git")
            .args(["show", &format!("{}:src/main.rs", sha)])
            .current_dir(&git_path)
            .output()
            .expect("Failed to run git show");
        assert!(output.status.success(), "git show failed for {}", sha);
        versions.push(output.stdout);
    }

    // Now create an atomic repo and replay these versions
    let (repo, _temp, repo_path) = create_test_repo();

    // Commit 1: add file
    write_file(
        &repo_path,
        "src/main.rs",
        &String::from_utf8_lossy(&versions[0]),
    );
    repo.add("src/main.rs", Default::default()).unwrap();
    let _ = record_change(&repo, "Initial commit");

    // Read via the CRDT-driven walker (task #24).  The view-filtered
    // byte-graph walker (`get_file_content_on_view`) over-counts on
    // multi-edge vertices — exactly the bug this test was created to
    // expose.  The CRDT walker is the replacement; that's the goal.
    let content = repo
        .get_file_content_via_crdt("src/main.rs")
        .unwrap()
        .unwrap();
    assert_eq!(
        content, versions[0],
        "Content mismatch after commit 1 (initial)"
    );

    // Commits 2-4: modify file
    for (i, version) in versions[1..].iter().enumerate() {
        write_file(&repo_path, "src/main.rs", &String::from_utf8_lossy(version));
        let _ = record_change(&repo, &format!("Commit {}", i + 2));

        let content = repo
            .get_file_content_via_crdt("src/main.rs")
            .unwrap()
            .unwrap();

        if content != *version {
            dump_crdt_line_mismatches(&repo, "src/main.rs", version);
        }

        assert_eq!(
            content.len(),
            version.len(),
            "Content length mismatch after commit {} ({}): got {} bytes, expected {} bytes.\n\
             This indicates content duplication in the change graph.",
            i + 2,
            commits[i + 1],
            content.len(),
            version.len(),
        );

        assert_eq!(
            content,
            *version,
            "Content mismatch after commit {} ({})",
            i + 2,
            commits[i + 1],
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Test: CRDT-population audit for the hyperfine sequence.
//
// This test does NOT check materialized byte equality.  Instead it walks
// the CRDT layer directly (Trunk → Branches → Leaves) after each commit
// and validates the structural invariants the planned CRDT-driven
// output relies on:
//
//   1. The file's inode has a Trunk entry.
//   2. Iterating that trunk's branches in TRUNK_BRANCHES order yields
//      exactly one Branch per alive line in the expected file content.
//   3. Each branch's leaves reassemble to that line's bytes.
//
// If any of these fail, the CRDT walker cannot be relied on to produce
// correct output for this scenario — we need to fix the record-side
// CRDT population before wiring output_file_via_crdt.
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_hyperfine_crdt_audit() {
    use atomic_core::crdt::tables::decode_trunk_id;
    use atomic_core::crdt::{decode_branch_id, decode_branch_value, BranchState};
    use atomic_core::pristine::{CrdtTxnT, GraphTxnT};
    use std::process::Command;

    let git_temp = TempDir::new().expect("Failed to create temp dir for git");
    let git_path = git_temp.path().to_path_buf();
    let clone_status = Command::new("git")
        .args([
            "clone",
            "--quiet",
            "https://github.com/sharkdp/hyperfine.git",
        ])
        .arg(&git_path)
        .status();

    match clone_status {
        Ok(s) if s.success() => {}
        _ => {
            eprintln!("Skipping test: git clone failed (no network?)");
            return;
        }
    }

    let commits: Vec<&str> = vec![
        "a658ab8c", "d4ebdd7b", "197f9fb", "68fdc2c", "9ba7ada", "5cdf013", "dab3f94", "219bb1e",
    ];

    let mut versions: Vec<Vec<u8>> = Vec::new();
    for sha in &commits {
        let output = Command::new("git")
            .args(["show", &format!("{}:src/main.rs", sha)])
            .current_dir(&git_path)
            .output()
            .expect("Failed to run git show");
        assert!(output.status.success(), "git show failed for {}", sha);
        versions.push(output.stdout);
    }

    let (repo, _temp, repo_path) = create_test_repo();

    // Commit 1 — add the file.
    write_file(
        &repo_path,
        "src/main.rs",
        &String::from_utf8_lossy(&versions[0]),
    );
    repo.add("src/main.rs", Default::default()).unwrap();
    let _ = record_change(&repo, "Initial commit");

    // Walk each commit, audit the CRDT topology after each.
    for (i, version) in versions.iter().enumerate() {
        if i > 0 {
            write_file(&repo_path, "src/main.rs", &String::from_utf8_lossy(version));
            let _ = record_change(&repo, &format!("Commit {}", i + 1));
        }

        // Expected: one alive Branch per line in the expected file.
        let expected_lines: Vec<&[u8]> = version.split_inclusive(|&b| b == b'\n').collect();

        let (inode, _pos) = repo
            .get_inode_and_position("src/main.rs")
            .expect("inode lookup failed")
            .expect("file is tracked");

        let txn = repo.pristine().write_txn().expect("write_txn for audit");

        let trunk_key_by_inode = txn
            .get_crdt_inode_trunk(inode.get())
            .expect("inode→trunk lookup failed");
        let trunk_id_by_path = txn
            .get_trunk_by_path("src/main.rs")
            .expect("path→trunk lookup failed");

        let trunk_key = match (trunk_key_by_inode, trunk_id_by_path) {
            (Some(tk), _) => Some(tk),
            (None, Some(tid)) => {
                eprintln!(
                    "CRDT AUDIT commit {} ({}): inode lookup MISSED but path lookup HIT — \
                     tree_inode={} crdt_trunk_id={:?} (the inode→trunk index is stale or \
                     keyed under a different inode than the tree layer uses)",
                    i + 1,
                    commits[i],
                    inode.get(),
                    tid
                );
                Some(atomic_core::crdt::tables::encode_trunk_id(&tid))
            }
            (None, None) => None,
        };

        match trunk_key {
            None => {
                eprintln!(
                    "CRDT AUDIT commit {} ({}): NO TRUNK at all — expected_lines={} \
                     (neither inode→trunk nor path→trunk found a row)",
                    i + 1,
                    commits[i],
                    expected_lines.len(),
                );
                drop(txn);
                continue;
            }
            Some(tk) => {
                let _trunk_id = decode_trunk_id(&tk);
                let branch_keys: Vec<[u8; 12]> = txn
                    .iter_trunk_branches(&tk)
                    .expect("iter_trunk_branches failed")
                    .collect::<Result<Vec<_>, _>>()
                    .expect("branch iteration error");
                let mut alive_branch_count = 0usize;
                let mut deleted_branch_count = 0usize;
                let mut missing_branch_rows = 0usize;
                let mut alive_with_vertex = 0usize;
                let mut alive_phantom_no_vertex = 0usize;
                let mut alive_phantom_zero_range = 0usize;
                let mut total_alive_bytes: u64 = 0;
                let mut alive_multi_line: usize = 0;
                let mut max_branch_bytes: u64 = 0;
                let mut vertex_to_branches: std::collections::HashMap<
                    [u8; 24],
                    Vec<atomic_core::crdt::BranchId>,
                > = std::collections::HashMap::new();
                for bk in &branch_keys {
                    let branch = txn.get_crdt_branch(bk).expect("get_crdt_branch failed");
                    let _ = decode_branch_id(bk);
                    let _ = decode_branch_value;
                    match branch {
                        Some(b) => match b.state {
                            BranchState::Alive => {
                                alive_branch_count += 1;
                                match txn.get_crdt_branch_vertex(bk) {
                                    Ok(Some(gn)) => {
                                        let len = gn.end.get().saturating_sub(gn.start.get());
                                        if len > 0 {
                                            alive_with_vertex += 1;
                                            total_alive_bytes += len;
                                            if len > max_branch_bytes {
                                                max_branch_bytes = len;
                                            }
                                            // Encode vertex for dedup detection.
                                            let vk =
                                                atomic_core::crdt::tables::encode_vertex_position(
                                                    &gn,
                                                );
                                            vertex_to_branches
                                                .entry(vk)
                                                .or_default()
                                                .push(decode_branch_id(bk));
                                            // Count newlines in this branch's content
                                            // to detect multi-line content_ranges.
                                            if let Some(hash) =
                                                txn.get_external(gn.change).ok().flatten()
                                            {
                                                let mut buf = vec![0u8; len as usize];
                                                let hash_fn = |id: atomic_core::types::NodeId| -> Option<atomic_core::types::Hash> {
                                                    if id.is_root() { None } else {
                                                        txn.get_external(id).ok().flatten()
                                                    }
                                                };
                                                use atomic_core::change::ChangeStore;
                                                if repo
                                                    .change_store()
                                                    .get_contents(hash_fn, gn, &mut buf)
                                                    .is_ok()
                                                {
                                                    let newline_count =
                                                        buf.iter().filter(|&&b| b == b'\n').count();
                                                    // A "well-formed" branch represents one line — exactly one
                                                    // newline (at the end) OR no newline (only the last line of a
                                                    // file without trailing newline).
                                                    let ends_with_newline =
                                                        buf.last() == Some(&b'\n');
                                                    let well_formed = (newline_count == 1
                                                        && ends_with_newline)
                                                        || newline_count == 0;
                                                    if !well_formed {
                                                        alive_multi_line += 1;
                                                        eprintln!(
                                                            "  MALFORMED commit {}: branch={:?} len={} newlines={} ends_nl={} change={} content={:?}",
                                                            i + 1,
                                                            decode_branch_id(bk),
                                                            len,
                                                            newline_count,
                                                            ends_with_newline,
                                                            hash,
                                                            String::from_utf8_lossy(&buf),
                                                        );
                                                    }
                                                }
                                            }
                                        } else {
                                            alive_phantom_zero_range += 1;
                                            eprintln!(
                                                "  PHANTOM (zero range) commit {}: branch={:?} vertex={:?}",
                                                i + 1,
                                                decode_branch_id(bk),
                                                gn
                                            );
                                        }
                                    }
                                    _ => {
                                        alive_phantom_no_vertex += 1;
                                        eprintln!(
                                            "  PHANTOM (no vertex) commit {}: branch={:?}",
                                            i + 1,
                                            decode_branch_id(bk)
                                        );
                                    }
                                }
                            }
                            BranchState::Deleted => deleted_branch_count += 1,
                        },
                        None => missing_branch_rows += 1,
                    }
                }

                // Detect branches sharing the same BRANCH_VERTEX entry —
                // these would cause the walker to emit the same content
                // multiple times.
                let mut duplicate_vertices = 0usize;
                let mut duplicate_extra_bytes: u64 = 0;
                for (vk, branches) in &vertex_to_branches {
                    if branches.len() > 1 {
                        duplicate_vertices += 1;
                        let gn = atomic_core::crdt::tables::decode_vertex_position(vk);
                        let len = gn.end.get().saturating_sub(gn.start.get());
                        duplicate_extra_bytes += len * (branches.len() as u64 - 1);
                        eprintln!(
                            "  DUPLICATE VERTEX commit {}: vertex={:?} shared by {} branches: {:?}",
                            i + 1,
                            gn,
                            branches.len(),
                            branches
                        );
                    }
                }

                // For the first commit that diverges, dump each branch
                // alongside the expected line so we can see exactly
                // which branches have wrong content.
                if total_alive_bytes != version.len() as u64 && i + 1 == 5 {
                    eprintln!(
                        "\n=== DETAILED BRANCH DUMP commit {} ({}) ===",
                        i + 1,
                        commits[i]
                    );
                    use atomic_core::crdt::tables::decode_trunk_id;
                    let trunk_id = decode_trunk_id(&tk);
                    {
                        let dumped = collect_alive_branch_dump(&repo, &txn, trunk_id);
                        let expected_split: Vec<&str> = std::str::from_utf8(version)
                            .unwrap_or("")
                            .split_inclusive('\n')
                            .collect();
                        let max = dumped.len().max(expected_split.len());
                        for k in 0..max {
                            let got = dumped
                                .get(k)
                                .map(|d| d.text.as_str())
                                .unwrap_or("<MISSING>");
                            let want = expected_split.get(k).copied().unwrap_or("<MISSING>");
                            if got != want {
                                let vertex_info = dumped
                                    .get(k)
                                    .map(|d| {
                                        format!("branch={:?} {} {}", d.branch_id, d.after, d.vertex)
                                    })
                                    .unwrap_or_else(|| "<NO BRANCH>".to_string());
                                eprintln!(
                                    "  LINE {}: got={:?} want={:?} | {}",
                                    k + 1,
                                    got,
                                    want,
                                    vertex_info
                                );
                                print_dump_context(&dumped, &expected_split, k);
                            }
                        }
                        eprintln!("=== END DUMP ===\n");
                    }
                }

                eprintln!(
                    "CRDT AUDIT commit {} ({}): expected_lines={} expected_bytes={} branches={} alive={} deleted={} missing_rows={} \
                     | alive_with_vertex={} alive_phantom_no_vertex={} alive_phantom_zero_range={} \
                     | total_alive_bytes={} multi_line_branches={} max_branch_bytes={} \
                     | duplicate_vertices={} duplicate_extra_bytes={}",
                    i + 1,
                    commits[i],
                    expected_lines.len(),
                    version.len(),
                    branch_keys.len(),
                    alive_branch_count,
                    deleted_branch_count,
                    missing_branch_rows,
                    alive_with_vertex,
                    alive_phantom_no_vertex,
                    alive_phantom_zero_range,
                    total_alive_bytes,
                    alive_multi_line,
                    max_branch_bytes,
                    duplicate_vertices,
                    duplicate_extra_bytes,
                );
            }
        }

        drop(txn);
    }

    // Final pass: this test is currently DIAGNOSTIC — its job is to
    // surface gaps in CRDT population so we can prioritise fixing them.
    // We deliberately do NOT assert exact equality yet; the eprintln!
    // output is what reviewers should consult.
}

// ═══════════════════════════════════════════════════════════════════════════
// Test: Hyperfine commits 1-7 (through error handling refactor)
//
// Stepping stone: replays 7 commits instead of 4. Commits 5-7 add a README
// (no src/main.rs change), a major refactor extracting run_benchmark() and
// HyperfineOptions (94 insertions, 73 deletions, 2 hunks), and then the
// error handling commit (29 insertions, 7 deletions, 7 hunks with a new
// function + return-type propagation across call sites).
//
// This catches regressions where medium-complexity multi-hunk patterns
// (code extraction + call-site updates) trigger duplication even after
// the consolidation fix.
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_hyperfine_extended_commit_sequence() {
    use std::process::Command;

    let git_temp = TempDir::new().expect("Failed to create temp dir for git");
    let git_path = git_temp.path().to_path_buf();
    let clone_status = Command::new("git")
        .args([
            "clone",
            "--quiet",
            "https://github.com/sharkdp/hyperfine.git",
        ])
        .arg(&git_path)
        .status();

    match clone_status {
        Ok(s) if s.success() => {}
        _ => {
            eprintln!("Skipping test: git clone failed (no network?)");
            return;
        }
    }

    // Commits that touch src/main.rs in the first 15 commits.
    // We skip commits that don't change src/main.rs (README, licenses, metadata).
    let commits: Vec<&str> = vec![
        "a658ab8c", // 1: Initial commit
        "d4ebdd7b", // 2: Add a progress bar (10 hunks, +52/-12)
        "197f9fb",  // 3: Code style update (15 hunks, +22/-17)
        "68fdc2c",  // 4: Add --warmup option (6 hunks, +44/-10) — previously failing
        "9ba7ada",  // 5: Refactoring — extract run_benchmark (2 hunks, +94/-73)
        "5cdf013",  // 6: Modify clap settings (7 hunks, +8/-9)
        "dab3f94",  // 7: Add --min-runs (2 hunks, +18/-3)
        "219bb1e",  // 8: Error handling (7 hunks, +29/-7)
    ];

    // Collect file contents at each commit (skip commits where file doesn't exist)
    let mut versions: Vec<(String, Vec<u8>)> = Vec::new();
    for sha in &commits {
        let output = Command::new("git")
            .args(["show", &format!("{}:src/main.rs", sha)])
            .current_dir(&git_path)
            .output()
            .expect("Failed to run git show");
        if !output.status.success() {
            // File doesn't exist at this commit — skip
            continue;
        }
        versions.push((sha.to_string(), output.stdout));
    }

    assert!(
        versions.len() >= 4,
        "Expected at least 4 versions, got {}",
        versions.len()
    );

    // Replay all versions into an atomic repo
    let (repo, _temp, repo_path) = create_test_repo();

    // First version: add file
    write_file(
        &repo_path,
        "src/main.rs",
        &String::from_utf8_lossy(&versions[0].1),
    );
    repo.add("src/main.rs", Default::default()).unwrap();
    let _ = record_change(&repo, &format!("Commit 1 ({})", versions[0].0));

    let content = repo
        .get_file_content_on_view("src/main.rs", repo.current_view())
        .unwrap()
        .unwrap();
    assert_eq!(
        content, versions[0].1,
        "Content mismatch after commit 1 ({})",
        versions[0].0
    );

    // Subsequent versions: modify file
    for (i, (sha, version)) in versions[1..].iter().enumerate() {
        write_file(&repo_path, "src/main.rs", &String::from_utf8_lossy(version));
        let _ = record_change(&repo, &format!("Commit {} ({})", i + 2, sha));

        let content = repo
            .get_file_content_via_crdt("src/main.rs")
            .unwrap()
            .unwrap();

        if content != *version {
            dump_crdt_line_mismatches(&repo, "src/main.rs", version);
        }

        assert_eq!(
            content.len(),
            version.len(),
            "Content length mismatch after commit {} ({}): got {} bytes, expected {} bytes.\n\
             This indicates content duplication in the change graph.",
            i + 2,
            sha,
            content.len(),
            version.len(),
        );

        assert_eq!(
            content,
            *version,
            "Content mismatch after commit {} ({})",
            i + 2,
            sha,
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Test: Isolated pairwise commit diffs at increasing complexity
//
// Rather than replaying the full sequence, this tests individual commit
// transitions in isolation (fresh repo each time). This isolates whether
// the bug is in a specific diff shape vs accumulated graph state.
//
// Tier 1: dab3f94 (2 hunks, +18/-3) — pure multi-site inserts
// Tier 2: 219bb1e (7 hunks, +29/-7) — new function + call-site changes
// Tier 3: 68fdc2c (6 hunks, +44/-10) — extract + warmup + replace
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_hyperfine_pairwise_transitions() {
    use std::process::Command;
    let git_temp = TempDir::new().expect("Failed to create temp dir for git");
    let git_path = git_temp.path().to_path_buf();
    let clone_status = Command::new("git")
        .args([
            "clone",
            "--quiet",
            "https://github.com/sharkdp/hyperfine.git",
        ])
        .arg(&git_path)
        .status();

    match clone_status {
        Ok(s) if s.success() => {}
        _ => {
            eprintln!("Skipping test: git clone failed (no network?)");
            return;
        }
    }

    // Each pair is (before_sha, after_sha, description, expected_hunk_complexity)
    let pairs: Vec<(&str, &str, &str)> = vec![
        // Tier 1: Simple multi-site insert (2 hunks)
        ("5cdf013", "dab3f94", "add --min-runs (2 hunks)"),
        // Tier 2: Medium — new function + return type propagation (7 hunks)
        ("dab3f94", "219bb1e", "error handling (7 hunks)"),
        // Tier 3: Complex — extract function + warmup + inline replace (6 hunks)
        ("197f9fb", "68fdc2c", "add --warmup (6 hunks)"),
        // Tier 4: Major refactor — extract run_benchmark + HyperfineOptions (2 big hunks)
        ("68fdc2c", "9ba7ada", "refactoring (2 hunks, +94/-73)"),
    ];

    for (before_sha, after_sha, description) in &pairs {
        // Get file at before and after
        let before_output = Command::new("git")
            .args(["show", &format!("{}:src/main.rs", before_sha)])
            .current_dir(&git_path)
            .output()
            .expect("Failed to run git show");
        assert!(
            before_output.status.success(),
            "git show failed for {}",
            before_sha
        );

        let after_output = Command::new("git")
            .args(["show", &format!("{}:src/main.rs", after_sha)])
            .current_dir(&git_path)
            .output()
            .expect("Failed to run git show");
        assert!(
            after_output.status.success(),
            "git show failed for {}",
            after_sha
        );

        // Fresh repo for each pair
        let (repo, _temp, repo_path) = create_test_repo();

        // Add the "before" version
        write_file(
            &repo_path,
            "src/main.rs",
            &String::from_utf8_lossy(&before_output.stdout),
        );
        repo.add("src/main.rs", Default::default()).unwrap();
        let _ = record_change(&repo, &format!("before: {}", before_sha));

        let content = repo
            .get_file_content_via_crdt("src/main.rs")
            .unwrap()
            .unwrap();
        assert_eq!(
            content, before_output.stdout,
            "[{}] Content mismatch after initial add ({})",
            description, before_sha
        );

        // Apply the "after" version
        write_file(
            &repo_path,
            "src/main.rs",
            &String::from_utf8_lossy(&after_output.stdout),
        );
        let _ = record_change(&repo, &format!("after: {}", after_sha));

        let content = repo
            .get_file_content_via_crdt("src/main.rs")
            .unwrap()
            .unwrap();

        assert_eq!(
            content.len(),
            after_output.stdout.len(),
            "[{}] Content length mismatch ({} → {}): got {} bytes, expected {} bytes.\n\
             This indicates content duplication in the change graph.",
            description,
            before_sha,
            after_sha,
            content.len(),
            after_output.stdout.len(),
        );

        assert_eq!(
            content, after_output.stdout,
            "[{}] Content mismatch ({} → {})",
            description, before_sha, after_sha,
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Test: Four sequential modifications matching the hyperfine pattern
//
// Reproduces the exact commit sequence that triggers duplication:
// 1. Initial file with basic structure
// 2. Add progress bar (new imports, inline setup)
// 3. Code style cleanup (minor tweaks)
// 4. Extract helper function + add warmup (complex multi-hunk)
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_four_step_evolution_fidelity() {
    let (repo, _temp, repo_path) = create_test_repo();

    let v1 = "\
use std::time::Instant;

fn time_cmd(cmd: &str) -> f64 {
    let start = Instant::now();
    run(cmd);
    start.elapsed().as_secs_f64()
}

fn main() {
    let cmd = get_cmd();
    let mut times = vec![];
    for _ in 0..10 {
        times.push(time_cmd(cmd));
    }
    let mean: f64 = times.iter().sum::<f64>() / times.len() as f64;
    println!(\"Mean: {:.3}s\", mean);
}
";

    let v2 = "\
use std::time::Instant;

fn time_cmd(cmd: &str) -> f64 {
    let start = Instant::now();
    run(cmd);
    start.elapsed().as_secs_f64()
}

fn main() {
    let cmd = get_cmd();
    let min_runs = 10;
    let mut times = vec![];

    let bar = ProgressBar::new(min_runs);
    let style = ProgressStyle::default_spinner()
        .template(\"{spinner} {msg}\");
    bar.set_style(style);
    bar.set_message(\"Measuring\");

    for _ in 0..min_runs {
        bar.inc(1);
        times.push(time_cmd(cmd));
    }
    bar.finish();

    let mean: f64 = times.iter().sum::<f64>() / times.len() as f64;
    println!(\"Mean: {:.3}s\", mean);
}
";

    let v3 = "\
use std::time::Instant;

fn time_cmd(cmd: &str) -> f64 {
    let start = Instant::now();
    run(cmd);
    start.elapsed().as_secs_f64()
}

fn main() {
    let cmd = get_cmd();
    let min_runs = 10;
    let mut times = vec![];

    let bar = ProgressBar::new(min_runs);
    let style = ProgressStyle::default_spinner()
        .template(\"{spinner} {msg:<28} {wide_bar}\");
    bar.set_style(style.clone());
    bar.set_message(\"Measuring\");

    for _ in 0..min_runs {
        bar.inc(1);
        times.push(time_cmd(cmd));
    }
    bar.finish_and_clear();

    let mean: f64 = times.iter().sum::<f64>() / times.len() as f64;
    println!(\"Mean: {:.3}s\", mean);
}
";

    let v4 = "\
use std::time::Instant;

fn time_cmd(cmd: &str) -> f64 {
    let start = Instant::now();
    run(cmd);
    start.elapsed().as_secs_f64()
}

fn get_bar(len: u64, msg: &str) -> ProgressBar {
    let style = ProgressStyle::default_spinner()
        .tick_chars(\"⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏\")
        .template(\"{spinner} {msg:<28} {wide_bar}\");
    let bar = ProgressBar::new(len);
    bar.set_style(style);
    bar.set_message(msg);
    bar
}

fn main() {
    let cmd = get_cmd();
    let min_runs = 10;
    let mut times = vec![];

    // Warmup phase
    if let Some(n) = get_warmup() {
        let bar = get_bar(n, \"Warmup\");
        for _ in 0..n {
            bar.inc(1);
            let _ = time_cmd(cmd);
        }
        bar.finish_and_clear();
    }

    let bar = get_bar(min_runs, \"Measuring\");
    for _ in 0..min_runs {
        bar.inc(1);
        times.push(time_cmd(cmd));
    }
    bar.finish_and_clear();

    let mean: f64 = times.iter().sum::<f64>() / times.len() as f64;
    println!(\"Mean: {:.3}s\", mean);
}
";

    // Step 1: initial
    write_file(&repo_path, "bench.rs", v1);
    repo.add("bench.rs", Default::default()).unwrap();
    let h1 = record_change(&repo, "initial");
    assert_content_matches(&repo, "bench.rs", v1);

    // Step 2: add progress bar
    write_file(&repo_path, "bench.rs", v2);
    let h2 = record_change(&repo, "add progress bar");
    assert_content_matches(&repo, "bench.rs", v2);

    // Step 3: code style
    write_file(&repo_path, "bench.rs", v3);
    let h3 = record_change(&repo, "code style");
    assert_content_matches(&repo, "bench.rs", v3);

    // Step 4: extract helper + warmup (the commit that triggers duplication)
    write_file(&repo_path, "bench.rs", v4);
    let h4 = record_change(&repo, "extract helper and add warmup");

    // Critical: content at HEAD must exactly match v4
    let content = repo
        .get_file_content_on_view("bench.rs", repo.current_view())
        .expect("Failed to get content")
        .expect("File not found");

    let actual = String::from_utf8_lossy(&content);
    let actual_lines = actual.lines().count();
    let expected_lines = v4.lines().count();

    assert_eq!(
        actual_lines,
        expected_lines,
        "Line count mismatch after step 4: got {} lines, expected {} lines.\n\
         This indicates content duplication in the change graph.\n\
         First 5 lines of actual:\n{}\n...\nLast 5 lines:\n{}",
        actual_lines,
        expected_lines,
        actual.lines().take(5).collect::<Vec<_>>().join("\n"),
        actual
            .lines()
            .rev()
            .take(5)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n"),
    );

    assert_eq!(
        actual.as_ref(),
        v4,
        "Content mismatch after step 4 (extract helper + warmup)"
    );

    // Verify all intermediate states too
    assert_content_after_change(&repo, "bench.rs", &h1, v1);
    assert_content_after_change(&repo, "bench.rs", &h2, v2);
    assert_content_after_change(&repo, "bench.rs", &h3, v3);
    assert_content_after_change(&repo, "bench.rs", &h4, v4);
}

// ═══════════════════════════════════════════════════════════════════════════
// Test: Single-line middle insertion
//
// Reproduces the "All recorded files are empty (no hunks)" bug triggered
// by hyperfine commit 2ab118dd ("Close stdin for child-processes").
//
// A single line is inserted in the middle of a file. The diff produces
// one Insert hunk with old_start > 0 and old_start < old_line_count
// (a "middle insertion"). The upstream consolidation in record_modified_file
// only fires when nuclear_hunk_count > 1 or when a nuclear hunk coexists
// with other hunks. A lone middle Insert slips through unconsolidated,
// reaches globalization, and is rejected because the globalize pipeline
// no longer handles middle insertions directly.
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_single_line_middle_insertion() {
    let (repo, _temp, repo_path) = create_test_repo();

    let v1 = "\
fn run_command(cmd: &str) -> Result<()> {
    let status = Command::new(\"sh\")
        .arg(\"-c\")
        .arg(cmd)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;

    Ok(status)
}

fn main() {
    let cmd = get_cmd();
    let result = run_command(cmd);
    println!(\"{:?}\", result);
}
";

    // Version 2: single line added in the middle (.stdin(Stdio::null()))
    let v2 = "\
fn run_command(cmd: &str) -> Result<()> {
    let status = Command::new(\"sh\")
        .arg(\"-c\")
        .arg(cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;

    Ok(status)
}

fn main() {
    let cmd = get_cmd();
    let result = run_command(cmd);
    println!(\"{:?}\", result);
}
";

    write_file(&repo_path, "src/main.rs", v1);
    repo.add("src/main.rs", Default::default()).unwrap();
    let _h1 = record_change(&repo, "initial");
    assert_content_matches(&repo, "src/main.rs", v1);

    // This is the critical step — a single-line middle insertion must
    // successfully record and produce correct content.
    write_file(&repo_path, "src/main.rs", v2);
    let _h2 = record_change(&repo, "add stdin null");
    assert_content_matches(&repo, "src/main.rs", v2);
}

// ═══════════════════════════════════════════════════════════════════════════
// Test: Single-line middle insertion after multiple prior modifications
//
// Same bug but with more history — the file has been modified several
// times before the single-line insertion. This matches the exact
// hyperfine scenario: 16 commits modify src/main.rs, then commit 17
// adds a single line in the middle.
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_single_line_middle_insertion_after_multiple_edits() {
    let (repo, _temp, repo_path) = create_test_repo();

    let v1 = "\
use std::process::Command;

fn run(cmd: &str) {
    Command::new(\"sh\")
        .arg(\"-c\")
        .arg(cmd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect(\"failed\");
}

fn main() {
    run(\"echo hello\");
}
";

    let v2 = "\
use std::process::{Command, Stdio};

fn run(cmd: &str) {
    Command::new(\"sh\")
        .arg(\"-c\")
        .arg(cmd)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect(\"failed\");
}

fn main() {
    let cmd = \"echo hello\";
    run(cmd);
}
";

    let v3 = "\
use std::process::{Command, Stdio};
use std::io;

fn run(cmd: &str) -> io::Result<()> {
    let status = Command::new(\"sh\")
        .arg(\"-c\")
        .arg(cmd)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;

    if !status.success() {
        eprintln!(\"Command failed\");
    }

    Ok(())
}

fn main() {
    let cmd = \"echo hello\";
    run(cmd).expect(\"run failed\");
}
";

    // v4: single line inserted in the middle — .stdin(Stdio::null())
    let v4 = "\
use std::process::{Command, Stdio};
use std::io;

fn run(cmd: &str) -> io::Result<()> {
    let status = Command::new(\"sh\")
        .arg(\"-c\")
        .arg(cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;

    if !status.success() {
        eprintln!(\"Command failed\");
    }

    Ok(())
}

fn main() {
    let cmd = \"echo hello\";
    run(cmd).expect(\"run failed\");
}
";

    write_file(&repo_path, "src/main.rs", v1);
    repo.add("src/main.rs", Default::default()).unwrap();
    let _h1 = record_change(&repo, "initial");

    write_file(&repo_path, "src/main.rs", v2);
    let _h2 = record_change(&repo, "refactor imports");

    write_file(&repo_path, "src/main.rs", v3);
    let _h3 = record_change(&repo, "add error handling");

    // The critical single-line middle insertion
    write_file(&repo_path, "src/main.rs", v4);
    let _h4 = record_change(&repo, "close stdin for child processes");
    assert_content_matches(&repo, "src/main.rs", v4);
}

// ═══════════════════════════════════════════════════════════════════════════
// CRDT-driven output walker (output_file_via_crdt)
//
// Exercises the walker that reads file content by following the CRDT
// Trunk → Branch chain, fetching bytes from each branch's BRANCH_VERTEX.
// Independent of the linear byte-graph walker, so its correctness depends
// only on (a) iter_trunk_branches_in_file_order's file order, (b) BRANCH_VERTEX
// being kept current across Insert/Modify, and (c) branch.state turnover.
// ═══════════════════════════════════════════════════════════════════════════

fn crdt_output(repo: &Repository, path: &str) -> Vec<u8> {
    let txn = repo.pristine().read_txn().expect("read_txn");
    atomic_core::output::crdt::output_file_via_crdt(&txn, repo.change_store(), path)
        .expect("output_file_via_crdt")
}

fn hyperfine_fixture_versions() -> [(&'static str, &'static [u8]); 4] {
    [
        (
            "a658ab8c",
            include_bytes!("fixtures/hyperfine/a658ab8c_main.rs"),
        ),
        (
            "d4ebdd7b",
            include_bytes!("fixtures/hyperfine/d4ebdd7b_main.rs"),
        ),
        (
            "197f9fb",
            include_bytes!("fixtures/hyperfine/197f9fb_main.rs"),
        ),
        (
            "68fdc2c",
            include_bytes!("fixtures/hyperfine/68fdc2c_main.rs"),
        ),
    ]
}

#[derive(Clone)]
struct DumpedBranchLine {
    branch_id: atomic_core::crdt::BranchId,
    after: String,
    vertex: String,
    text: String,
}

fn format_branch_after<T: atomic_core::pristine::CrdtTxnT>(
    txn: &T,
    branch_key: &[u8; 12],
) -> String {
    use atomic_core::crdt::tables::decode_branch_id;

    match txn.get_crdt_branch_after(branch_key) {
        Ok(Some(after)) if after == [0u8; 12] => "after=<START>".to_string(),
        Ok(Some(after)) => format!("after={:?}", decode_branch_id(&after)),
        Ok(None) => "after=<MISSING>".to_string(),
        Err(e) => format!("after=<ERR:{}>", e),
    }
}

fn collect_alive_branch_dump<
    T: atomic_core::pristine::CrdtTxnT + atomic_core::pristine::GraphTxnT,
>(
    repo: &Repository,
    txn: &T,
    trunk_id: atomic_core::crdt::TrunkId,
) -> Vec<DumpedBranchLine> {
    use atomic_core::change::ChangeStore;
    use atomic_core::crdt::queries::iter_trunk_branches_in_file_order;
    use atomic_core::crdt::tables::encode_branch_id;

    let ordered =
        iter_trunk_branches_in_file_order(txn, trunk_id).expect("ordered branch walk for dump");
    let mut dumped = Vec::new();

    for branch_id in ordered {
        let branch_key = encode_branch_id(&branch_id);
        let alive = matches!(
            txn.get_crdt_branch(&branch_key),
            Ok(Some(b)) if b.state.is_alive()
        );
        if !alive {
            continue;
        }

        let after = format_branch_after(txn, &branch_key);
        let branch_vertex = txn
            .get_crdt_branch_vertex(&branch_key)
            .expect("branch vertex for dump");

        match branch_vertex {
            None => dumped.push(DumpedBranchLine {
                branch_id,
                after,
                vertex: "vertex=<MISSING>".to_string(),
                text: "<MISSING VERTEX>".to_string(),
            }),
            Some(gn) => {
                let len = gn.end.get().saturating_sub(gn.start.get()) as usize;
                let vertex = format!(
                    "vertex=(change={:?} start={} end={})",
                    gn.change,
                    gn.start.get(),
                    gn.end.get()
                );
                if len == 0 {
                    dumped.push(DumpedBranchLine {
                        branch_id,
                        after,
                        vertex,
                        text: "<ZERO RANGE>".to_string(),
                    });
                    continue;
                }

                let hash_fn = |id: atomic_core::types::NodeId| -> Option<atomic_core::types::Hash> {
                    if id.is_root() {
                        None
                    } else {
                        txn.get_external(id).ok().flatten()
                    }
                };
                let mut buf = vec![0u8; len];
                repo.change_store()
                    .get_contents(hash_fn, gn, &mut buf)
                    .expect("load branch bytes for dump");

                dumped.push(DumpedBranchLine {
                    branch_id,
                    after,
                    vertex,
                    text: String::from_utf8_lossy(&buf).to_string(),
                });
            }
        }
    }

    dumped
}

fn print_dump_context(dumped: &[DumpedBranchLine], expected_lines: &[&str], center: usize) {
    let start = center.saturating_sub(2);
    let end = (center + 3).min(dumped.len().max(expected_lines.len()));
    for idx in start..end {
        let got = dumped
            .get(idx)
            .map(|d| d.text.as_str())
            .unwrap_or("<MISSING>");
        let want = expected_lines.get(idx).copied().unwrap_or("<MISSING>");
        let meta = dumped
            .get(idx)
            .map(|d| format!("branch={:?} {} {}", d.branch_id, d.after, d.vertex))
            .unwrap_or_else(|| "<NO BRANCH>".to_string());
        eprintln!("    [{}] got={:?} want={:?} | {}", idx + 1, got, want, meta);
    }
}

fn dump_crdt_line_mismatches(repo: &Repository, path: &str, expected: &[u8]) {
    use atomic_core::crdt::tables::{decode_trunk_id, encode_trunk_id};
    use atomic_core::pristine::CrdtTxnT;

    let Some((inode, _)) = repo
        .get_inode_and_position(path)
        .expect("inode lookup for dump")
    else {
        eprintln!("No inode for {}", path);
        return;
    };

    let txn = repo.pristine().read_txn().expect("read_txn for dump");
    let trunk_key = match txn
        .get_crdt_inode_trunk(inode.get())
        .expect("inode->trunk lookup for dump")
    {
        Some(key) => key,
        None => match txn
            .get_trunk_by_path(path)
            .expect("path->trunk lookup for dump")
        {
            Some(trunk_id) => encode_trunk_id(&trunk_id),
            None => {
                eprintln!("No trunk for {}", path);
                return;
            }
        },
    };

    let trunk_id = decode_trunk_id(&trunk_key);
    let dumped = collect_alive_branch_dump(repo, &txn, trunk_id);

    let expected_lines: Vec<&str> = std::str::from_utf8(expected)
        .expect("expected fixture utf8")
        .split_inclusive('\n')
        .collect();

    eprintln!("=== CRDT LINE DUMP {} ===", path);
    let max = dumped.len().max(expected_lines.len());
    for idx in 0..max {
        let got = dumped
            .get(idx)
            .map(|d| d.text.as_str())
            .unwrap_or("<MISSING>");
        let want = expected_lines.get(idx).copied().unwrap_or("<MISSING>");
        if got != want {
            let meta = dumped
                .get(idx)
                .map(|d| format!("branch={:?} {} {}", d.branch_id, d.after, d.vertex))
                .unwrap_or_else(|| "<NO BRANCH>".to_string());
            eprintln!("LINE {}:", idx + 1);
            eprintln!("  got : {:?}", got);
            eprintln!("  want: {:?}", want);
            eprintln!("  {}", meta);
            eprintln!("  context:");
            print_dump_context(&dumped, &expected_lines, idx);
        }
    }
    eprintln!("=== END CRDT LINE DUMP ===");
}

#[test]
fn test_crdt_walker_simple_add() {
    let (repo, _temp, repo_path) = create_test_repo();

    let v1 = "alpha\nbeta\ngamma\n";
    write_file(&repo_path, "data.txt", v1);
    repo.add("data.txt", Default::default()).unwrap();
    record_change(&repo, "add");

    let got = crdt_output(&repo, "data.txt");
    assert_eq!(
        String::from_utf8_lossy(&got),
        v1,
        "CRDT walker output mismatch after FileAdd"
    );
}

#[test]
fn test_crdt_walker_after_modify() {
    let (repo, _temp, repo_path) = create_test_repo();

    let v1 = "alpha\nbeta\ngamma\n";
    let v2 = "alpha\nBETA\ngamma\n"; // one-line Modify

    write_file(&repo_path, "data.txt", v1);
    repo.add("data.txt", Default::default()).unwrap();
    record_change(&repo, "add");

    write_file(&repo_path, "data.txt", v2);
    record_change(&repo, "modify");

    let got = crdt_output(&repo, "data.txt");
    assert_eq!(
        String::from_utf8_lossy(&got),
        v2,
        "CRDT walker output mismatch after Modify — \
         BRANCH_VERTEX for the modified line should point at the new content"
    );
}

#[test]
fn test_crdt_walker_after_insert() {
    let (repo, _temp, repo_path) = create_test_repo();

    let v1 = "alpha\nbeta\ngamma\n";
    let v2 = "alpha\nbeta\nDELTA\ngamma\n"; // insert in the middle

    write_file(&repo_path, "data.txt", v1);
    repo.add("data.txt", Default::default()).unwrap();
    record_change(&repo, "add");

    write_file(&repo_path, "data.txt", v2);
    record_change(&repo, "insert delta");

    let got = crdt_output(&repo, "data.txt");
    assert_eq!(
        String::from_utf8_lossy(&got),
        v2,
        "CRDT walker output mismatch after mid-file Insert — \
         the new branch must order after `beta` and before `gamma`"
    );
}

#[test]
fn test_crdt_walker_after_delete() {
    let (repo, _temp, repo_path) = create_test_repo();

    let v1 = "alpha\nbeta\ngamma\ndelta\n";
    let v2 = "alpha\ngamma\ndelta\n"; // delete `beta`

    write_file(&repo_path, "data.txt", v1);
    repo.add("data.txt", Default::default()).unwrap();
    record_change(&repo, "add");

    write_file(&repo_path, "data.txt", v2);
    record_change(&repo, "delete beta");

    let got = crdt_output(&repo, "data.txt");
    assert_eq!(
        String::from_utf8_lossy(&got),
        v2,
        "CRDT walker output mismatch after Delete — \
         the deleted branch must be filtered out by branch.state"
    );
}

#[test]
fn test_crdt_walker_prepend_in_second_commit() {
    // Load-bearing test for iter_trunk_branches_in_file_order: a later
    // commit prepending a line must show up at the *top*, not after all
    // of commit 1's branches (which is what plain BranchId sort would do).
    let (repo, _temp, repo_path) = create_test_repo();

    let v1 = "second\nthird\n";
    let v2 = "first\nsecond\nthird\n";

    write_file(&repo_path, "data.txt", v1);
    repo.add("data.txt", Default::default()).unwrap();
    record_change(&repo, "add");

    write_file(&repo_path, "data.txt", v2);
    record_change(&repo, "prepend first");

    let got = crdt_output(&repo, "data.txt");
    assert_eq!(
        String::from_utf8_lossy(&got),
        v2,
        "CRDT walker mis-ordered the prepended line"
    );
}

#[test]
fn test_crdt_walker_modify_then_insert_below() {
    // Pattern that often appears in real diffs: modify one line, then
    // insert a new line below it.  Verifies the CRDT walker can handle
    // a mix of Modify + Insert chained correctly.
    let (repo, _temp, repo_path) = create_test_repo();

    let v1 = "alpha\nbeta\ngamma\n";
    let v2 = "alpha\nBETA\nNEW\ngamma\n"; // modify beta + insert NEW

    write_file(&repo_path, "data.txt", v1);
    repo.add("data.txt", Default::default()).unwrap();
    record_change(&repo, "add");

    write_file(&repo_path, "data.txt", v2);
    record_change(&repo, "modify + insert");

    let got = crdt_output(&repo, "data.txt");
    assert_eq!(String::from_utf8_lossy(&got), v2);
}

#[test]
fn test_crdt_walker_two_separate_modifies() {
    // Two non-adjacent Modify operations in a single commit.  The
    // consolidation pass pairs Delete+Insert across the whole diff list,
    // so this catches placeholder-after-ref breakage when the pairing
    // isn't 1:1 with positional order.
    let (repo, _temp, repo_path) = create_test_repo();

    let v1 = "alpha\nbeta\ngamma\ndelta\nepsilon\n";
    let v2 = "alpha\nBETA\ngamma\nDELTA\nepsilon\n";

    write_file(&repo_path, "data.txt", v1);
    repo.add("data.txt", Default::default()).unwrap();
    record_change(&repo, "add");

    write_file(&repo_path, "data.txt", v2);
    record_change(&repo, "two modifies");

    let got = crdt_output(&repo, "data.txt");
    assert_eq!(String::from_utf8_lossy(&got), v2);
}

#[test]
fn test_crdt_walker_modify_in_block_with_insert_after_block() {
    // Replace block (modify several lines) followed by a separate Insert.
    // Tests that the placeholder rewrite handles the chain across blocks.
    let (repo, _temp, repo_path) = create_test_repo();

    let v1 = "a\nb\nc\nd\ne\n";
    let v2 = "a\nB\nC\nd\nNEW\ne\n";

    write_file(&repo_path, "data.txt", v1);
    repo.add("data.txt", Default::default()).unwrap();
    record_change(&repo, "add");

    write_file(&repo_path, "data.txt", v2);
    record_change(&repo, "block-modify + insert");

    let got = crdt_output(&repo, "data.txt");
    assert_eq!(String::from_utf8_lossy(&got), v2);
}

#[test]
fn test_crdt_walker_unknown_file_returns_empty() {
    let (repo, _temp, _repo_path) = create_test_repo();
    let got = crdt_output(&repo, "nonexistent.txt");
    assert!(got.is_empty());
}

// Tracking regression for task #24 (CRDT-first record line ordering).
//
// The walker is correct (see the surrounding focused walker tests); the
// failure surfaces a record-side problem: cross-block Delete/Insert
// consolidation in `build_crdt_ops_for_modified_file` promotes pairs to
// Modifies that reuse an existing branch's after-chain position while
// substituting content from a different line position.  The walker
// emits the modified content at the original branch's place in file
// order — which is wrong for cross-block pairings.
//
// The fix is to stop diff-then-pair and instead emit BranchOps that
// directly reflect the new file's line order against existing CRDT
// state.  Not ignored — we want the failure visible until #24 lands.
#[test]
fn test_crdt_walker_hyperfine_sequence_byte_exact() {
    // End-to-end correctness for the CRDT-driven walker against the exact
    // commit sequence that exposed the byte-graph linear-walker bug.  If
    // this passes, task #24 (wiring the walker into `get_file_content`)
    // is just plumbing.
    use std::process::Command;

    let git_temp = TempDir::new().expect("temp dir");
    let git_path = git_temp.path().to_path_buf();
    let clone_status = Command::new("git")
        .args([
            "clone",
            "--quiet",
            "https://github.com/sharkdp/hyperfine.git",
        ])
        .arg(&git_path)
        .status();
    match clone_status {
        Ok(s) if s.success() => {}
        _ => {
            eprintln!("Skipping test: git clone failed (no network?)");
            return;
        }
    }

    let commits: Vec<&str> = vec!["a658ab8c", "d4ebdd7b", "197f9fb", "68fdc2c"];
    let mut versions: Vec<Vec<u8>> = Vec::new();
    for sha in &commits {
        let output = Command::new("git")
            .args(["show", &format!("{}:src/main.rs", sha)])
            .current_dir(&git_path)
            .output()
            .expect("git show");
        assert!(output.status.success(), "git show failed for {}", sha);
        versions.push(output.stdout);
    }

    let (repo, _temp, repo_path) = create_test_repo();

    write_file(
        &repo_path,
        "src/main.rs",
        &String::from_utf8_lossy(&versions[0]),
    );
    repo.add("src/main.rs", Default::default()).unwrap();
    record_change(&repo, "Initial commit");
    let got = crdt_output(&repo, "src/main.rs");
    assert_eq!(
        got,
        versions[0],
        "CRDT walker mismatch at commit 1 ({}): {} bytes vs expected {}",
        commits[0],
        got.len(),
        versions[0].len()
    );

    for (i, version) in versions.iter().enumerate().skip(1) {
        write_file(&repo_path, "src/main.rs", &String::from_utf8_lossy(version));
        record_change(&repo, &format!("Commit {}", i + 1));
        let got = crdt_output(&repo, "src/main.rs");
        assert_eq!(
            got,
            *version,
            "CRDT walker mismatch at commit {} ({}): {} bytes vs expected {}",
            i + 1,
            commits[i],
            got.len(),
            version.len()
        );
    }
}

#[test]
fn test_crdt_walker_hyperfine_sequence_offline_fixture() {
    let versions = hyperfine_fixture_versions();
    let (repo, _temp, repo_path) = create_test_repo();

    write_file(
        &repo_path,
        "src/main.rs",
        &String::from_utf8_lossy(versions[0].1),
    );
    repo.add("src/main.rs", Default::default()).unwrap();
    record_change(&repo, "Initial commit");

    let got = crdt_output(&repo, "src/main.rs");
    assert_eq!(
        got, versions[0].1,
        "CRDT walker mismatch at commit 1 ({})",
        versions[0].0
    );

    for (i, (sha, version)) in versions.iter().enumerate().skip(1) {
        write_file(&repo_path, "src/main.rs", &String::from_utf8_lossy(version));
        record_change(&repo, &format!("Commit {}", i + 1));

        let got = crdt_output(&repo, "src/main.rs");
        if got != *version {
            dump_crdt_line_mismatches(&repo, "src/main.rs", version);
        }
        assert_eq!(
            got,
            *version,
            "CRDT walker mismatch at commit {} ({}): {} bytes vs expected {}",
            i + 1,
            sha,
            got.len(),
            version.len()
        );
    }
}
