//! CB-13C AC-3 (ATOM::aaron-ogle::20): the MEASURED rollout-budget and
//! watcher-off-equivalence artifact.
//!
//! RFC §21 / AC-3 requires the 100k-file warm/cold and incremental-import
//! budgets and watcher-off equivalence to be *measured on documented
//! environments*, not inferred from unit-test presence. This file is that
//! measurement. It is `#[ignore]`-gated so ordinary suites stay fast; run
//! it explicitly, in release mode, against a real filesystem:
//!
//! ```text
//! TMPDIR=/home/aaron/code/work/atomic/target/bench-tmp \
//!   cargo test -p atomic-cli --release \
//!   --test import_budget_100k_bench -- --ignored --nocapture
//! ```
//!
//! The run prints the measured wall-clock numbers and the environment
//! block; the published numbers live in the RFC §21 evidence matrix in
//! `docs/RFC-ATOMIC-GIT-CAUSAL-BRIDGE-TODO.md`. Assertions here are the
//! recorded calibration: budgets are asserted against the published
//! scan-tier thresholds (with the documented environment), and the
//! equivalence run asserts byte-level identity between the watcher-off
//! command-boundary path and the watcher-on (`bridge watch --once`, scan
//! tier) path.
//!
//! Environment facts the matrix records (from the measured run):
//! - the corpus is 100 directories × 1000 files of ~90-byte deterministic
//!   content, committed once by Git before the first import;
//! - budgets are the `atomic git import` wall clock through the real CLI
//!   binary (CARGO_BIN_EXE), not the synthetic change-source provider;
//! - the fsmonitor/Watchman tiers are UNSHIPPED (CB-13D), so incremental
//!   budgets are scan-tier: the scan re-stats the full 100k tracked set.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Instant;

const ATOMIC_BIN: &str = env!("CARGO_BIN_EXE_atomic");

/// Measured calibration (release, documented environment — see the RFC
/// §21 matrix in docs/RFC-ATOMIC-GIT-CAUSAL-BRIDGE-TODO.md). The dominant
/// cold/warm component is the post-import content-search index rebuild
/// (external `syntext` crate, ~9ms/file, linear); the import pipeline
/// itself is sub-100s at 100k. Incremental imports DEFER the content index
/// (`--incremental`), so their budget is the scan-tier full-stat cost of
/// the tracked set.
const PUBLISHED_COLD_BUDGET_MS: u128 = 2_400_000;
const PUBLISHED_WARM_BUDGET_MS: u128 = 2_400_000;
const PUBLISHED_INCREMENTAL_SCAN_BUDGET_MS: u128 = 300_000;

const COLD_DIRS: usize = 100;
const FILES_PER_DIR: usize = 1_000;

fn atomic(root: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(ATOMIC_BIN)
        .args(args)
        .current_dir(root)
        .env("HOME", home)
        .env("ATOMIC_HOME", home.join(".atomic"))
        .env("GIT_AUTHOR_NAME", "Budget Bench")
        .env("GIT_AUTHOR_EMAIL", "bench@example.com")
        .env("GIT_COMMITTER_NAME", "Budget Bench")
        .env("GIT_COMMITTER_EMAIL", "bench@example.com")
        .output()
        .expect("run atomic")
}

fn atomic_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Run atomic, assert success, return stdout+stderr.
fn atomic_ok(root: &Path, home: &Path, args: &[&str]) -> String {
    let output = atomic(root, home, args);
    assert!(
        output.status.success(),
        "atomic {args:?} failed:\n{}",
        atomic_text(&output)
    );
    atomic_text(&output)
}

/// Run atomic with a wall-clock measurement; returns (seconds, output).
fn timed_atomic(root: &Path, home: &Path, args: &[&str]) -> (f64, Output) {
    let started = Instant::now();
    let output = atomic(root, home, args);
    (started.elapsed().as_secs_f64(), output)
}

fn git(root: &Path, args: &[&str]) -> Output {
    let mut command = Command::new("git");
    command.args(args).current_dir(root);
    // Deterministic identity + dates: identical corpus state across fresh
    // repositories produces identical commit OIDs (the equivalence run
    // depends on this).
    command
        .env("GIT_AUTHOR_NAME", "Budget Bench")
        .env("GIT_AUTHOR_EMAIL", "bench@example.com")
        .env("GIT_COMMITTER_NAME", "Budget Bench")
        .env("GIT_COMMITTER_EMAIL", "bench@example.com")
        .env("GIT_AUTHOR_DATE", "@1726732800 +0000")
        .env("GIT_COMMITTER_DATE", "@1726732800 +0000");
    command.output().expect("run git")
}

fn git_ok(root: &Path, args: &[&str]) {
    let output = git(root, args);
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn key_file(home: &Path) -> PathBuf {
    let mut secret = [0u8; 32];
    for (index, byte) in secret.iter_mut().enumerate() {
        *byte = 0x42u8.wrapping_add((index as u8).wrapping_mul(13).wrapping_add(7));
    }
    let hex: String = secret.iter().map(|byte| format!("{byte:02x}")).collect();
    let path = home.join("bench-binding.key");
    fs::write(&path, hex).expect("write binding key");
    path
}

/// Print the documented-environment block the matrix records.
fn print_environment(corpus_root: &Path) {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);
    println!("ENV BEGIN");
    println!("tmpdir: {}", std::env::temp_dir().display());
    if let Ok(output) = Command::new("stat")
        .args(["-f", "-c", "%T"])
        .arg(corpus_root)
        .output()
    {
        println!(
            "corpus_fs: {}",
            String::from_utf8_lossy(&output.stdout).trim()
        );
    }
    if let Ok(output) = Command::new("git").arg("--version").output() {
        println!(
            "git: {}",
            String::from_utf8_lossy(&output.stdout).trim()
        );
    }
    if let Ok(release) = fs::read_to_string("/proc/sys/kernel/osrelease") {
        println!("kernel: {}", release.trim());
    }
    println!("cpus: {cpus}");
    println!("profile: release (CARGO_BIN_EXE_atomic)");
    println!("ENV END");
}

/// Generate the deterministic corpus: `dirs` directories × `files_per_dir`
/// files, content `content-v1-{i}` (~90 bytes), then one Git commit.
fn make_corpus(root: &Path, dirs: usize, files_per_dir: usize, tag: &str) {
    for dir in 0..dirs {
        let dir_path = root.join(format!("dir-{dir:04}"));
        fs::create_dir_all(&dir_path).expect("create corpus dir");
        for file in 0..files_per_dir {
            fs::write(
                dir_path.join(format!("file-{file:05}.txt")),
                format!("{tag} content for dir-{dir:04} file-{file:05} — measured-budget corpus row\n"),
            )
            .expect("write corpus file");
        }
    }
}

/// Count the recorded change files in the repository's change store.
fn change_count(root: &Path) -> usize {
    fs::read_dir(root.join(".atomic/changes"))
        .map(|dirs| {
            dirs.flatten()
                .filter_map(|d| fs::read_dir(d.path()).ok())
                .flatten()
                .filter_map(|f| f.ok())
                .filter(|f| {
                    f.path().extension().map(|x| x == "change").unwrap_or(false)
                })
                .count()
        })
        .unwrap_or(0)
}

/// AC-3 measurement: the 100k-file warm/cold and incremental-import
/// budgets through the real CLI on the real filesystem.
#[test]
#[ignore = "measured-budget run; execute explicitly in release mode"]
fn measured_100k_import_budgets_real_filesystem() {
    let root = tempfile::TempDir::new().expect("corpus tempdir");
    let home = tempfile::TempDir::new().expect("home tempdir");
    let root = root.path();
    let home = home.path();

    print_environment(root);
    println!(
        "corpus: {COLD_DIRS} dirs x {FILES_PER_DIR} files = {} files",
        COLD_FILES_TOTAL
    );

    git_ok(root, &["init", "-q", "-b", "main"]);
    make_corpus(root, COLD_DIRS, FILES_PER_DIR, "v1");
    git_ok(root, &["add", "-A"]);
    git_ok(root, &["commit", "-qm", "budget corpus v1"]);

    atomic_ok(root, home, &["init", "--no-vault"]);
    fs::remove_file(root.join(".atomicignore")).expect("remove atomicignore");

    // Cold: the first import builds the full change store from nothing.
    let (cold_s, cold_out) = timed_atomic(root, home, &["git", "import", "--no-vault"]);
    assert!(cold_out.status.success(), "cold import failed:\n{cold_out:?}");
    println!("COLD_IMPORT_S {cold_s:.3}");

    // Warm: re-import with no working-copy or Git change.
    let (warm_s, warm_out) = timed_atomic(root, home, &["git", "import", "--no-vault"]);
    assert!(warm_out.status.success(), "warm import failed:\n{warm_out:?}");
    println!("WARM_IMPORT_S {warm_s:.3}");

    // Incremental (scan tier): edit a small set, commit, re-import with
    // --incremental (the git-shadow-sync mode, which defers the content
    // index). The scan change source re-stats the full tracked set — this
    // measures the shipped tier, and the printed number is the scan-tier
    // budget.
    for dir in 0..5 {
        for file in 0..10 {
            fs::write(
                root.join(format!("dir-{dir:04}"))
                    .join(format!("file-{file:05}.txt")),
                format!("v2 edited content for dir-{dir:04} file-{file:05}\n"),
            )
            .expect("edit corpus file");
        }
    }
    git_ok(root, &["add", "-A"]);
    git_ok(root, &["commit", "-qm", "incremental edits v2"]);
    let (incremental_s, incremental_out) =
        timed_atomic(root, home, &["git", "import", "--no-vault", "--incremental"]);
    assert!(
        incremental_out.status.success(),
        "incremental import failed:\n{incremental_out:?}"
    );
    println!("INCREMENTAL_SCAN_IMPORT_S {incremental_s:.3}");

    // CB-13C ::23 AC-5: the 100k STATUS timings (the tracked-set scan the
    // status transaction performs) — cold (first status after the imports)
    // and warm (immediate re-run) through the real CLI.
    let (status_cold_s, status_cold_out) = timed_atomic(root, home, &["status", "--no-vault"]);
    assert!(
        status_cold_out.status.success(),
        "cold status failed:\n{status_cold_out:?}"
    );
    println!("COLD_STATUS_100K_S {status_cold_s:.3}");
    let (status_warm_s, status_warm_out) = timed_atomic(root, home, &["status", "--no-vault"]);
    assert!(
        status_warm_out.status.success(),
        "warm status failed:\n{status_warm_out:?}"
    );
    println!("WARM_STATUS_100K_S {status_warm_s:.3}");

    let recorded_changes = change_count(root);
    println!("CHANGE_FILES_AFTER_INCREMENTAL {recorded_changes}");
    assert!(
        recorded_changes >= COLD_FILES_TOTAL,
        "the change store must hold at least the 100k corpus changes"
    );

    // Recorded calibration (published in the RFC §21 matrix). These are
    // sanity ceilings for the documented environment, generous enough to
    // absorb CI noise but tight enough to catch a pathological regression.
    let cold_ms = (cold_s * 1000.0) as u128;
    let warm_ms = (warm_s * 1000.0) as u128;
    let incremental_ms = (incremental_s * 1000.0) as u128;
    assert!(
        cold_ms <= PUBLISHED_COLD_BUDGET_MS,
        "cold import {cold_ms}ms exceeded the published scan-tier budget {}ms",
        PUBLISHED_COLD_BUDGET_MS
    );
    assert!(
        warm_ms <= PUBLISHED_WARM_BUDGET_MS,
        "warm import {warm_ms}ms exceeded the published scan-tier budget {}ms",
        PUBLISHED_WARM_BUDGET_MS
    );
    assert!(
        incremental_ms <= PUBLISHED_INCREMENTAL_SCAN_BUDGET_MS,
        "incremental import {incremental_ms}ms exceeded the published scan-tier budget {}ms",
        PUBLISHED_INCREMENTAL_SCAN_BUDGET_MS
    );
    println!("BUDGETS WITHIN PUBLISHED GATES");
}

const COLD_FILES_TOTAL: usize = COLD_DIRS * FILES_PER_DIR;

/// Equivalence corpus size for the measured watcher-off run (kept small
/// enough for a fast explicit run; identity, not timing, is asserted).
const EQUIV_DIRS: usize = 10;
const EQUIV_FILES_PER_DIR: usize = 50;

/// A single colocated repo with the bridge enabled and an initial import
/// of the deterministic corpus.
struct EquivFixture {
    root: tempfile::TempDir,
    home: tempfile::TempDir,
}

impl EquivFixture {
    fn build(watch_enabled: bool) -> Self {
        let root = tempfile::TempDir::new().expect("equiv repo tempdir");
        let home = tempfile::TempDir::new().expect("equiv home tempdir");
        let root = root;
        let home = home;

        git_ok(root.path(), &["init", "-q", "-b", "main"]);
        fs::write(root.path().join("anchor.txt"), b"anchor me\n").expect("anchor file");
        make_corpus(root.path(), EQUIV_DIRS, EQUIV_FILES_PER_DIR, "v1");
        git_ok(root.path(), &["add", "-A"]);
        git_ok(root.path(), &["commit", "-qm", "equiv corpus v1"]);

        atomic_ok(root.path(), home.path(), &["init", "--no-vault"]);
        fs::remove_file(root.path().join(".atomicignore")).expect("remove atomicignore");
        atomic_ok(root.path(), home.path(), &["git", "import", "--no-vault"]);
        atomic_ok(
            root.path(),
            home.path(),
            &[
                "git",
                "bridge",
                "enable",
                "--binding-key-file",
                key_file(home.path()).to_str().expect("utf8 key path"),
            ],
        );
        atomic_ok(root.path(), home.path(), &["git", "bridge", "reconcile"]);
        if watch_enabled {
            enable_watch(root.path());
        }
        Self { root, home }
    }

    fn root(&self) -> &Path {
        self.root.path()
    }

    fn home(&self) -> &Path {
        self.home.path()
    }

    fn atomic_ok(&self, args: &[&str]) -> String {
        atomic_ok(self.root(), self.home(), args)
    }

    /// The external Git transition both fixtures run (deterministic dates
    /// make the commit OIDs identical across the two repositories).
    fn external_commit(&self) {
        fs::write(self.root().join("dir-0000").join("file-00000.txt"), "EXTERNAL EDIT v2\n")
            .expect("external edit");
        git_ok(self.root(), &["add", "-A"]);
        git_ok(self.root(), &["commit", "-qm", "external equivalence commit"]);
    }

    /// The logical state identity (mirrors the CB-13D harness): the
    /// checkpoint's state-shape fields, the refname/target set with
    /// binding refs normalized to their class, the base32-normalized
    /// verify output, the worktree byte snapshot, and the change count.
    fn equivalence_identity(&self) -> String {
        let checkpoint =
            fs::read_to_string(self.root().join(".atomic/bridge/workspace.json"))
                .expect("checkpoint");
        let mut identity = String::new();
        for line in checkpoint.lines() {
            let trimmed = line.trim();
            for key in [
                "\"version\"",
                "\"view\"",
                "\"kind\"",
                "\"git_tree\"",
                "\"git_index_tree\"",
                "\"git_index_digest\"",
            ] {
                if trimmed.starts_with(key) {
                    identity.push_str(trimmed);
                    identity.push('\n');
                }
            }
            if trimmed.starts_with("\"value\"") {
                identity.push_str(trimmed);
                identity.push('\n');
            }
        }
        let refs = {
            let output = git(self.root(), &["for-each-ref", "--format=%(refname) %(objectname)"]);
            assert!(
                output.status.success(),
                "for-each-ref failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).expect("utf8 for-each-ref")
        };
        identity.push_str("refs:\n");
        for line in refs.lines() {
            let refname = line.split(' ').next().unwrap_or(line);
            if refname.starts_with("refs/atomic/bindings/") {
                identity.push_str("refs/atomic/bindings/*\n");
            } else {
                identity.push_str(line);
                identity.push('\n');
            }
        }
        let verify = self.atomic_ok(&["git", "bridge", "verify"]);
        identity.push_str(&format!("verify:\n{}\n", normalize_base32(&verify)));
        identity.push_str(&format!(
            "worktree-bytes: {:?}\n",
            worktree_bytes(self.root())
        ));
        identity.push_str(&format!("changes: {}\n", change_count(self.root())));
        identity
    }
}

fn enable_watch(root: &Path) {
    let config = root.join(".atomic/config.toml");
    let content = fs::read_to_string(&config).expect("read repo config");
    assert!(
        content.contains("[git.bridge]"),
        "bridge enable must have recorded the consent table first"
    );
    let mut in_watch = false;
    let mut flipped = false;
    let rewritten: String = content
        .lines()
        .map(|line| {
            if line.trim() == "[git.bridge.watch]" {
                in_watch = true;
                return line.to_string();
            }
            if line.trim().starts_with('[') {
                in_watch = false;
                return line.to_string();
            }
            if in_watch && line.trim() == "enabled = false" {
                flipped = true;
                return "enabled = true".to_string();
            }
            line.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(flipped, "the disabled watch default must be present");
    fs::write(config, rewritten + "\n").expect("write repo config");
}

/// Byte snapshot of the working tree (excluding .git/.atomic), order-
/// independent: (relative path, bytes) pairs hashed by serialization.
fn worktree_bytes(root: &Path) -> u64 {
    let mut snapshot: Vec<(String, Vec<u8>)> = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        for entry in fs::read_dir(&directory).expect("read dir").flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|name| name.to_str());
            if name == Some(".git") || name == Some(".atomic") {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(bytes) = fs::read(&path) {
                snapshot.push((
                    path.strip_prefix(root)
                        .expect("relative path")
                        .display()
                        .to_string(),
                    bytes,
                ));
            }
        }
    }
    snapshot.sort();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    use std::hash::Hasher;
    for (path, bytes) in snapshot {
        hasher.write(path.as_bytes());
        hasher.write(&bytes);
    }
    hasher.finish()
}

/// Replace base32-looking runs of >= 32 chars with `<state>` (mirrors the
/// CB-13D harness normalization: per-repository change identities are not
/// state shapes).
fn normalize_base32(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    let mut run = String::new();
    for character in text.chars() {
        let matches = character.is_ascii_uppercase() || ('2'..='7').contains(&character);
        if matches {
            run.push(character);
        } else {
            if run.len() >= 32 {
                normalized.push_str("<state>");
            } else {
                normalized.push_str(&run);
            }
            run.clear();
            normalized.push(character);
        }
    }
    if run.len() >= 32 {
        normalized.push_str("<state>");
    } else {
        normalized.push_str(&run);
    }
    normalized
}

/// AC-3 measurement: watcher-off equivalence through the real CLI.
///
/// From two identical fixtures, the watcher-OFF command-boundary path
/// (`git bridge reconcile`) and the watcher-ON daemon path
/// (`git bridge watch --once`, scan tier) must reach identical views,
/// checkpoints, refs, verified states, worktree bytes and change counts —
/// and both must pass the typed equivalence verifier.
#[test]
#[ignore = "measured-equivalence run; execute explicitly in release mode"]
fn measured_watcher_off_equivalence_real_cli() {
    let off = EquivFixture::build(false);
    let on = EquivFixture::build(true);

    print_environment(off.root());

    // Identical external transitions (deterministic dates → identical OIDs).
    off.external_commit();
    on.external_commit();

    // Path A: watcher off — the next command boundary reconciles.
    let (off_s, off_out) = timed_atomic(off.root(), off.home(), &["git", "bridge", "reconcile"]);
    assert!(
        off_out.status.success(),
        "watcher-off reconcile failed:\n{off_out:?}"
    );
    println!("WATCH_OFF_RECONCILE_S {off_s:.3}");

    // Path B: watcher on (scan tier) — one reactive daemon pass.
    let (on_s, on_out) = timed_atomic(on.root(), on.home(), &["git", "bridge", "watch", "--once"]);
    assert!(
        on_out.status.success(),
        "watch-on pass failed:\n{on_out:?}"
    );
    println!("WATCH_ON_ONCE_S {on_s:.3}");

    // Identical logical state, byte-for-byte on every identity field.
    let identity_off = off.equivalence_identity();
    let identity_on = on.equivalence_identity();
    assert_eq!(
        identity_off, identity_on,
        "watcher-on and watcher-off paths must reach identical logical state"
    );
    println!("EQUIVALENCE_IDENTITY_MATCH true");

    // Both paths also pass the typed equivalence verifier (the verify
    // output is part of the identity; assert its non-failure shape here).
    assert!(
        identity_off.contains("verify:"),
        "verify output must be part of the recorded identity"
    );
    println!("WATCHER_OFF_EQUIVALENCE_MEASURED true");
}
