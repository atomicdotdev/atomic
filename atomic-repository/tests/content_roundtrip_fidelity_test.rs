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
        .get_file_content_via_overlay(path, repo.current_stack())
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

    let content = repo
        .get_file_content_via_overlay("src/main.rs", repo.current_stack())
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
            .get_file_content_via_overlay("src/main.rs", repo.current_stack())
            .unwrap()
            .unwrap();

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
        .get_file_content_via_overlay("bench.rs", repo.current_stack())
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
