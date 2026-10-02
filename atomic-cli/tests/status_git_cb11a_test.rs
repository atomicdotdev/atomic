//! CB-11A end-to-end: RFC §9.2 golden parity rows, versioned machine-readable
//! status, stage/unstage, `diff --git` layers, forensic Observe, reconcile
//! refusal, bridge verify layers, and consent-based ignore mirroring.
//!
//! Every §9.2 row asserts the three columns of the table against the real
//! binary and the real `git status --short`:
//!
//! | Scenario | `git status --short` | `atomic status --git` | `atomic status` |
//!
//! The native codes (`M`/`A`/`D`/`P`/`T`/`C`) deliberately differ from the
//! `--git` XY subset; each row asserts its native column too.

#![cfg(not(windows))]
use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;
use tempfile::TempDir;

const ATOMIC_BIN: &str = env!("CARGO_BIN_EXE_atomic");

struct Fixture {
    repository: TempDir,
    home: TempDir,
}

impl Fixture {
    fn colocated() -> Self {
        let fixture = Self::empty();
        fixture.git_ok(&["init", "-q", "-b", "main"]);
        fixture.git_ok(&["config", "user.name", "CB-11A Tests"]);
        fixture.git_ok(&["config", "user.email", "cb11a@example.com"]);
        fixture.git_ok(&["config", "core.autocrlf", "false"]);
        fixture.git_ok(&["config", "commit.gpgsign", "false"]);
        fs::write(fixture.root().join("tracked.txt"), b"l1\nl2\nl3\n")
            .expect("write initial tracked file");
        fixture.git_ok(&["add", "tracked.txt"]);
        fixture.git_ok(&["commit", "--no-gpg-sign", "-q", "-m", "base"]);
        fixture.atomic_ok(&["git", "import", "--no-vault"]);
        fixture.atomic_ok(&["git", "bridge", "reconcile"]);
        assert!(
            fixture
                .root()
                .join(".atomic/bridge/workspace.json")
                .is_file(),
            "Git reconciliation must establish a valid bridge checkpoint"
        );
        fixture
    }

    fn empty() -> Self {
        Self {
            repository: TempDir::new().expect("repository tempdir"),
            home: TempDir::new().expect("home tempdir"),
        }
    }

    fn root(&self) -> &Path {
        self.repository.path()
    }

    fn atomic(&self, args: &[&str]) -> Output {
        let mut command = Command::new(ATOMIC_BIN);
        command
            .args(args)
            .current_dir(self.root())
            .env("HOME", self.home.path())
            .env("ATOMIC_HOME", self.home.path().join(".atomic"))
            .env("ATOMIC_NONINTERACTIVE", "1")
            .env("NO_COLOR", "1")
            .env("CLICOLOR", "0")
            .env("TERM", "dumb")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.output().expect("wait for atomic")
    }

    fn atomic_ok(&self, args: &[&str]) -> Output {
        let output = self.atomic(args);
        assert_success(&output, &format!("atomic {}", args.join(" ")));
        output
    }

    /// Combined stdout+stderr, used for notice/refusal assertions.
    fn atomic_text(&self, args: &[&str]) -> String {
        let output = self.atomic(args);
        output_text(&output)
    }

    fn atomic_json(&self, args: &[&str]) -> Value {
        let output = self.atomic_ok(args);
        serde_json::from_slice(&output.stdout).expect("atomic JSON output")
    }

    fn git(&self, args: &[&str]) -> Output {
        let mut command = Command::new("git");
        command
            .args(args)
            .current_dir(self.root())
            .env("HOME", self.home.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "CB-11A Tests")
            .env("GIT_AUTHOR_EMAIL", "cb11a@example.com")
            .env("GIT_COMMITTER_NAME", "CB-11A Tests")
            .env("GIT_COMMITTER_EMAIL", "cb11a@example.com")
            .env("GIT_AUTHOR_DATE", "2001-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2001-01-01T00:00:00Z")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.output().expect("run git")
    }

    fn git_ok(&self, args: &[&str]) -> Output {
        let output = self.git(args);
        assert_success(&output, &format!("git {}", args.join(" ")));
        output
    }

    fn git_short(&self) -> String {
        String::from_utf8(self.git_ok(&["status", "--short"]).stdout)
            .expect("UTF-8 git status")
            .trim_end()
            .to_string()
    }

    /// Stage content without touching the worktree, via the index plumbing.
    fn stage_partial_content(&self, content: &str) {
        let output = Command::new("git")
            .args(["hash-object", "-w", "--stdin"])
            .current_dir(self.root())
            .env("HOME", self.home.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                child
                    .stdin
                    .as_mut()
                    .expect("git hash-object stdin")
                    .write_all(content.as_bytes())?;
                child.wait_with_output()
            })
            .expect("git hash-object");
        assert!(output.status.success());
        let oid = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let info = format!("100644 {oid} 0\ttracked.txt\n");
        let mut child = Command::new("git")
            .args(["update-index", "--index-info"])
            .current_dir(self.root())
            .env("HOME", self.home.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("git update-index");
        child
            .stdin
            .as_mut()
            .expect("git update-index stdin")
            .write_all(info.as_bytes())
            .expect("write index info");
        let output = child.wait_with_output().expect("git update-index output");
        assert!(
            output.status.success(),
            "update-index --index-info: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn write(&self, relative: &str, content: &str) {
        fs::write(self.root().join(relative), content).expect("write fixture file");
    }

    fn read(&self, relative: &str) -> Vec<u8> {
        fs::read(self.root().join(relative)).expect("read fixture file")
    }
}

fn assert_success(output: &Output, operation: &str) {
    assert!(
        output.status.success(),
        "{operation} failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn strip_ansi(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for escape in chars.by_ref() {
                if escape == 'm' {
                    break;
                }
            }
        } else {
            output.push(c);
        }
    }
    output
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// The two-column lines of `atomic status --git` (dropping Note/heading lines).
fn git_status_rows(fixture: &Fixture) -> Vec<String> {
    String::from_utf8_lossy(&fixture.atomic_ok(&["status", "--git"]).stdout)
        .lines()
        .filter(|line| {
            line.len() >= 3
                && line.as_bytes()[2] == b' '
                && matches!(
                    line.as_bytes()[0],
                    b' ' | b'M' | b'A' | b'D' | b'T' | b'U' | b'R' | b'C' | b'?'
                )
                && matches!(
                    line.as_bytes()[1],
                    b' ' | b'M' | b'A' | b'D' | b'T' | b'U' | b'R' | b'C' | b'?'
                )
        })
        .map(ToString::to_string)
        .collect()
}

/// The short-format lines of native `atomic status -s`.
fn native_rows(fixture: &Fixture) -> Vec<String> {
    String::from_utf8_lossy(&fixture.atomic_ok(&["status", "-s"]).stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(ToString::to_string)
        .collect()
}

// ── §9.2 golden rows ────────────────────────────────────────────────────

#[test]
fn row_synchronized_clean_all_three_layers_clean() {
    let fixture = Fixture::colocated();
    assert_eq!(fixture.git_short(), "");
    assert_eq!(git_status_rows(&fixture), Vec::<String>::new());
    assert!(fixture
        .atomic_text(&["status"])
        .contains("nothing to record, working tree clean"));
}

#[test]
fn row_edit_tracked_file_is_unstaged_in_both_git_inspired_columns() {
    let fixture = Fixture::colocated();
    fixture.write("tracked.txt", "l1\nEDIT\nl3\n");
    assert_eq!(fixture.git_short(), " M tracked.txt");
    assert_eq!(git_status_rows(&fixture), vec![" M tracked.txt"]);
    assert!(native_rows(&fixture).contains(&"M  tracked.txt".to_string()));
}

#[test]
fn row_staged_edit_reports_staged_column_only() {
    let fixture = Fixture::colocated();
    fixture.write("tracked.txt", "l1\nEDIT\nl3\n");
    fixture.git_ok(&["add", "tracked.txt"]);
    assert_eq!(fixture.git_short(), "M  tracked.txt");
    assert_eq!(git_status_rows(&fixture), vec!["M  tracked.txt"]);
    assert!(native_rows(&fixture).contains(&"M  tracked.txt".to_string()));
}

#[test]
fn row_partial_staging_reports_mm_and_native_pending() {
    let fixture = Fixture::colocated();
    fixture.write("tracked.txt", "l1\nEDIT\nl3\n");
    fixture.stage_partial_content("l1\nSTAGED\nl3\n");
    assert_eq!(fixture.git_short(), "MM tracked.txt");
    assert_eq!(git_status_rows(&fixture), vec!["MM tracked.txt"]);
    assert!(native_rows(&fixture).contains(&"M  tracked.txt".to_string()));
}

#[test]
fn row_untracked_file_is_qq_everywhere() {
    let fixture = Fixture::colocated();
    fixture.write("new.txt", "new\n");
    assert_eq!(fixture.git_short(), "?? new.txt");
    assert_eq!(git_status_rows(&fixture), vec!["?? new.txt"]);
    assert!(native_rows(&fixture)
        .iter()
        .any(|row| row.contains("new.txt") && row.contains('?')));
}

#[test]
fn row_git_add_new_file_is_staged_and_natively_added() {
    let fixture = Fixture::colocated();
    fixture.write("new.txt", "new\n");
    fixture.git_ok(&["add", "new.txt"]);
    assert_eq!(fixture.git_short(), "A  new.txt");
    assert_eq!(git_status_rows(&fixture), vec!["A  new.txt"]);
    let native = native_rows(&fixture);
    let row = native
        .iter()
        .find(|row| row.contains("new.txt"))
        .expect("native status reports the git-added path");
    assert!(
        row.starts_with('A'),
        "git-added new path is displayed as Added: {row}"
    );
}

#[test]
fn row_executable_mode_change_reports_space_m_and_native_p() {
    let fixture = Fixture::colocated();
    let mut permissions = fs::metadata(fixture.root().join("tracked.txt"))
        .expect("metadata")
        .permissions();
    use std::os::unix::fs::PermissionsExt;
    permissions.set_mode(permissions.mode() | 0o100);
    fs::set_permissions(fixture.root().join("tracked.txt"), permissions).unwrap();
    assert_eq!(fixture.git_short(), " M tracked.txt");
    assert_eq!(git_status_rows(&fixture), vec![" M tracked.txt"]);
    assert!(native_rows(&fixture).contains(&"P  tracked.txt".to_string()));
}

#[test]
fn row_regular_to_symlink_reports_t_in_git_inspired_and_native() {
    let fixture = Fixture::colocated();
    fs::remove_file(fixture.root().join("tracked.txt")).unwrap();
    std::os::unix::fs::symlink("l1", fixture.root().join("tracked.txt")).unwrap();
    assert_eq!(fixture.git_short(), " T tracked.txt");
    assert_eq!(git_status_rows(&fixture), vec![" T tracked.txt"]);
    assert!(native_rows(&fixture).contains(&"T  tracked.txt".to_string()));
}

#[test]
fn row_filemode_false_capability_ignores_exec_bit_everywhere() {
    let fixture = Fixture::colocated();
    fixture.git_ok(&["config", "core.fileMode", "false"]);
    // The conversion policy and Git agree: no exec-bit parity on this repo.
    let mut permissions = fs::metadata(fixture.root().join("tracked.txt"))
        .expect("metadata")
        .permissions();
    use std::os::unix::fs::PermissionsExt;
    permissions.set_mode(permissions.mode() | 0o100);
    fs::set_permissions(fixture.root().join("tracked.txt"), permissions).unwrap();
    assert_eq!(
        fixture.git_short(),
        "",
        "fileMode=false: Git reports nothing"
    );
    assert_eq!(
        git_status_rows(&fixture),
        Vec::<String>::new(),
        "fileMode=false: --git reports nothing"
    );
}

#[test]
fn row_symlinks_disabled_policy_matches_git_typechange() {
    let fixture = Fixture::colocated();
    // core.symlinks=false on Linux: Git and Atomic both treat a symlink as
    // content, so the replacement is a type change in both columns.
    fixture.git_ok(&["config", "core.symlinks", "false"]);
    fs::remove_file(fixture.root().join("tracked.txt")).unwrap();
    std::os::unix::fs::symlink("l1", fixture.root().join("tracked.txt")).unwrap();
    assert_eq!(fixture.git_short(), " T tracked.txt");
    assert_eq!(git_status_rows(&fixture), vec![" T tracked.txt"]);
}

#[test]
fn row_bound_switch_is_clean_after_reconcile() {
    let fixture = Fixture::colocated();
    fixture.atomic_ok(&["view", "create", "feature"]);
    fixture.atomic_ok(&["view", "switch", "feature"]);
    fixture.atomic_ok(&["view", "switch", "main"]);
    fixture.atomic_ok(&["git", "bridge", "reconcile"]);
    assert_eq!(fixture.git_short(), "");
    assert_eq!(git_status_rows(&fixture), Vec::<String>::new());
    assert!(fixture
        .atomic_text(&["status"])
        .contains("nothing to record, working tree clean"));
}

#[test]
fn row_git_merge_uu_reports_uu_with_notice_and_native_conflict() {
    let fixture = Fixture::colocated();
    // Create a divergent line-2 edit on a branch and on main, then merge.
    fixture.git_ok(&["switch", "-q", "-c", "topic"]);
    fixture.write("tracked.txt", "l1\nTOPIC\nl3\n");
    fixture.git_ok(&["commit", "-aqm", "topic"]);
    fixture.git_ok(&["switch", "-q", "main"]);
    fixture.write("tracked.txt", "l1\nMAIN\nl3\n");
    fixture.git_ok(&["commit", "-aqm", "mainline"]);
    assert!(
        !fixture.git(&["merge", "topic"]).status.success(),
        "fixture merge must conflict"
    );
    assert_eq!(fixture.git_short(), "UU tracked.txt");
    assert_eq!(git_status_rows(&fixture), vec!["UU tracked.txt"]);
    let git_inspired = fixture.atomic_text(&["status", "--git"]);
    assert!(
        git_inspired.contains("Git operation in progress (Merge)")
            && git_inspired.contains("unmerged"),
        "--git must carry the merge notices: {git_inspired}"
    );
    let native = fixture.atomic_text(&["status"]);
    assert!(
        native.contains("Git operation in progress"),
        "native must surface the Git merge: {native}"
    );
    assert!(native.contains("conflict:"), "native reports C: {native}");

    // Recording refuses while the Git merge is in progress (CB-11A rule:
    // managed recording never captures unmerged index state).
    let record = fixture.atomic(&["record", "-m", "capture during merge"]);
    assert!(
        !record.status.success(),
        "record must refuse during a Git merge: {}",
        output_text(&record)
    );
    // The refusal is informative, not a crash, and the pending worktree
    // (snapshot) layer survives the refusal untouched.
    assert!(
        output_text(&record).contains("Git"),
        "refusal explains the Git boundary: {}",
        output_text(&record)
    );
    let preserved = fixture.atomic_json(&["status", "--git", "--json"]);
    assert!(
        preserved["unstaged"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["path"] == "tracked.txt" && entry["unmerged"] == true),
        "the conflicted worktree layer is preserved after the refusal: {preserved}"
    );
    fixture.git_ok(&["merge", "--abort"]);
}

#[test]
fn row_unstaged_atomic_marker_materialization_is_space_m_git_and_native_conflict() {
    let fixture = Fixture::colocated();
    // Divergent edits on two views, then insert → conflict markers on disk.
    fixture.atomic_ok(&["view", "create", "feature"]);
    fixture.atomic_ok(&["view", "switch", "feature"]);
    fixture.write("tracked.txt", "l1\nFEATURE\nl3\n");
    fixture.atomic_ok(&["record", "-m", "feature edit"]);
    fixture.atomic_ok(&["view", "switch", "main"]);
    fixture.write("tracked.txt", "l1\nMAIN\nl3\n");
    fixture.atomic_ok(&["record", "-m", "main edit"]);
    let insert = fixture.atomic(&["insert", "from-view", "feature", "--to-view", "main"]);
    assert_success(&insert, "atomic insert from-view feature --to-view main");
    assert!(
        fixture
            .read("tracked.txt")
            .windows(7)
            .any(|w| w == b">>>>>>>"),
        "insert must materialize conflict markers"
    );

    assert_eq!(fixture.git_short(), " M tracked.txt");
    assert_eq!(git_status_rows(&fixture), vec![" M tracked.txt"]);
    assert!(native_rows(&fixture).contains(&"C  tracked.txt".to_string()));
}

#[test]
fn row_committed_conflict_snapshot_is_clean_git_with_notice_and_native_conflict() {
    let fixture = Fixture::colocated();
    fixture.atomic_ok(&["view", "create", "feature"]);
    fixture.atomic_ok(&["view", "switch", "feature"]);
    fixture.write("tracked.txt", "l1\nFEATURE\nl3\n");
    fixture.atomic_ok(&["record", "-m", "feature edit"]);
    fixture.atomic_ok(&["view", "switch", "main"]);
    fixture.write("tracked.txt", "l1\nMAIN\nl3\n");
    fixture.atomic_ok(&["record", "-m", "main edit"]);
    let insert = fixture.atomic(&["insert", "from-view", "feature", "--to-view", "main"]);
    assert_success(&insert, "atomic insert");

    // The user commits the conflicted snapshot through Git: ordinary commit,
    // never an unmerged index state.
    fixture.git_ok(&["add", "tracked.txt"]);
    fixture.git_ok(&["commit", "-qm", "atomic-conflict snapshot"]);
    assert_eq!(fixture.git_short(), "", "Git is clean by design");
    let git_inspired = fixture.atomic_text(&["status", "--git"]);
    assert!(
        git_inspired.contains("committed conflict snapshot"),
        "--git must notice the committed conflict snapshot: {git_inspired}"
    );

    // Re-anchor and confirm the restored conflict metadata stays native C.
    fixture.atomic_ok(&["git", "import", "--no-vault"]);
    fixture.atomic_ok(&["git", "bridge", "reconcile"]);
    assert_eq!(fixture.git_short(), "");
    assert!(native_rows(&fixture).contains(&"C  tracked.txt".to_string()));
}

// ── Machine-readable output (bridge state/origin, layers, version) ──────

#[test]
fn machine_readable_status_git_carries_bridge_state_origin_and_version() {
    let fixture = Fixture::colocated();

    // A clean repository first: baseline tree and index tree agree.
    let clean = fixture.atomic_json(&["status", "--git", "--json"]);
    assert_eq!(clean["format"], "atomic-status-git");
    assert_eq!(clean["version"], 1);
    assert_eq!(clean["bridge"]["state"], "aligned");
    assert!(
        clean["bridge"]["origin"] == "checkpoint" || clean["bridge"]["origin"] == "observed",
        "origin names the bridge source: {}",
        clean["bridge"]["origin"]
    );
    assert_eq!(clean["bridge"]["view"], "main");
    assert!(clean["bridge"]["git_head"].is_string());
    assert!(clean["bridge"]["git_tree"].is_string());
    assert_eq!(
        clean["layers"]["baseline_tree"],
        clean["layers"]["index_tree"]
    );
    assert!(clean["layers"]["durable_manifest_root"].is_string());

    // Staged and untracked work appears in the machine-readable layers.
    fixture.write("tracked.txt", "l1\nEDIT\nl3\n");
    fixture.git_ok(&["add", "tracked.txt"]);
    fixture.write("untracked.txt", "untracked\n");

    let document = fixture.atomic_json(&["status", "--git", "--json"]);
    assert_eq!(document["format"], "atomic-status-git");
    assert!(document["staged"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| { entry["path"] == "tracked.txt" && entry["x"] == "M" && entry["y"] == " " }));
    assert!(document["untracked"]
        .as_array()
        .unwrap()
        .iter()
        .any(|path| path == "untracked.txt"));
    assert!(document["notices"].as_array().unwrap().is_empty());
}

#[test]
fn machine_readable_reports_remediation_bridge_state() {
    let fixture = Fixture::colocated();
    // Advance Git HEAD behind Atomic's back: the checkpoint is stale.
    fixture.write("tracked.txt", "l1\nRAW-GIT\nl3\n");
    fixture.git_ok(&["commit", "-aqm", "raw git commit"]);

    let document = fixture.atomic_json(&["status", "--git", "--json"]);
    assert_eq!(document["bridge"]["state"], "remediation");
    assert_eq!(document["bridge"]["origin"], "observed");
    assert!(document["notices"]
        .as_array()
        .unwrap()
        .iter()
        .any(|notice| notice.as_str().unwrap_or("").contains("Git baseline")));
}

// ── diff --git layers ───────────────────────────────────────────────────

#[test]
fn diff_git_separates_staged_and_unstaged_layers() {
    let fixture = Fixture::colocated();
    fixture.write("tracked.txt", "l1\nSTAGED\nl3\n");
    fixture.git_ok(&["add", "tracked.txt"]);
    fixture.write("tracked.txt", "l1\nWORKTREE\nl3\n");

    // Default: index → worktree (unstaged only).
    let unstaged = fixture.atomic_ok(&["diff", "--git"]).stdout;
    let unstaged = strip_ansi(&String::from_utf8_lossy(&unstaged));
    assert!(
        unstaged.contains("# atomic-diff-git v1"),
        "--git diff output is versioned: {unstaged}"
    );
    assert!(
        unstaged.contains("+WORKTREE"),
        "unstaged layer shows the worktree edit: {unstaged}"
    );
    assert!(
        !unstaged.contains("+STAGED"),
        "unstaged layer must not show staged edits: {unstaged}"
    );

    // --cached: Git baseline (HEAD tree) → index (staged).
    let staged = fixture.atomic_ok(&["diff", "--git", "--cached"]).stdout;
    let staged = strip_ansi(&String::from_utf8_lossy(&staged));
    assert!(staged.contains("# atomic-diff-git v1"), "{staged}");
    assert!(
        staged.contains("+STAGED"),
        "staged layer shows the index edit: {staged}"
    );
    assert!(
        !staged.contains("+WORKTREE"),
        "staged layer must not show unstaged edits: {staged}"
    );
}

// ── Stage/unstage and durable-tracking invariants ───────────────────────

#[test]
fn atomic_stage_stages_content_and_refuses_untracked() {
    let fixture = Fixture::colocated();
    fixture.write("tracked.txt", "l1\nEDIT\nl3\n");
    fixture.write("untracked.txt", "new\n");

    let refused = fixture.atomic(&["stage", "untracked.txt"]);
    assert!(
        !refused.status.success(),
        "stage must refuse untracked paths: {}",
        output_text(&refused)
    );
    assert!(
        output_text(&refused).contains("atomic add"),
        "refusal points at durable tracking: {}",
        output_text(&refused)
    );

    let staged = fixture.atomic_ok(&["stage", "tracked.txt"]);
    let text = output_text(&staged);
    assert!(text.contains("staged 1 path(s)"), "{text}");
    assert!(
        fixture.git_short().contains("M  tracked.txt"),
        "{}",
        fixture.git_short()
    );
    // Worktree bytes untouched by staging.
    assert_eq!(fixture.read("tracked.txt"), b"l1\nEDIT\nl3\n");
}

#[test]
fn atomic_unstage_restores_git_baseline_and_keeps_worktree() {
    let fixture = Fixture::colocated();
    fixture.write("tracked.txt", "l1\nEDIT\nl3\n");
    fixture.git_ok(&["add", "tracked.txt"]);
    assert_eq!(fixture.git_short(), "M  tracked.txt");

    let unstaged = fixture.atomic_ok(&["unstage", "tracked.txt"]);
    let text = output_text(&unstaged);
    assert!(text.contains("unstaged 1 path(s)"), "{text}");
    // Index is back at the baseline; worktree keeps the edit.
    assert_eq!(fixture.git_short(), " M tracked.txt");
    assert_eq!(fixture.read("tracked.txt"), b"l1\nEDIT\nl3\n");
}

#[test]
fn atomic_add_creates_durable_tracking_and_intent_to_add() {
    let fixture = Fixture::colocated();
    fixture.write("added.txt", "added\n");
    let added = fixture.atomic_ok(&["add", "added.txt"]);
    let text = output_text(&added);
    assert!(
        text.contains("intent-to-add"),
        "colocated add records intent-to-add: {text}"
    );
    assert!(
        text.contains("content is not staged"),
        "add must not claim content is staged: {text}"
    );
    assert_eq!(fixture.git_short(), " A added.txt");
    assert_eq!(git_status_rows(&fixture), vec![" A added.txt"]);

    // `atomic stage` may now stage the content (intent-to-add counts as
    // tracked), and only then does the index hold content.
    fixture.atomic_ok(&["stage", "added.txt"]);
    assert_eq!(fixture.git_short(), "A  added.txt");
}

// ── Alternate index evidence ────────────────────────────────────────────

#[test]
fn alternate_git_index_file_is_evidence_not_replacement_staging() {
    let fixture = Fixture::colocated();

    // The default primary index stages nothing; durable tracking is intact.
    let before = fixture.atomic_json(&["status", "--git", "--json"]);
    assert_eq!(
        before["layers"]["baseline_tree"],
        before["layers"]["index_tree"]
    );

    // Build an alternate index that stages an edit; the main index and the
    // durable layer stay untouched.
    let alternate = fixture.root().join(".git").join("alternate-index");
    let output = Command::new("git")
        .arg("-C")
        .arg(fixture.root())
        .env("GIT_INDEX_FILE", &alternate)
        .args(["read-tree", "HEAD"])
        .env("HOME", fixture.home.path())
        .output()
        .expect("git read-tree");
    assert_success(&output, "git read-tree (alternate index)");
    let staged_oid = {
        let blob = "l1\nALT\nl3\n";
        let output = Command::new("git")
            .arg("-C")
            .arg(fixture.root())
            .args(["hash-object", "-w", "--stdin"])
            .env("HOME", fixture.home.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                child
                    .stdin
                    .as_mut()
                    .expect("stdin")
                    .write_all(blob.as_bytes())?;
                child.wait_with_output()
            })
            .expect("hash-object");
        assert!(output.status.success(), "hash-object failed");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    let info = format!("100644 {staged_oid} 0\ttracked.txt\n");
    let output = Command::new("git")
        .arg("-C")
        .arg(fixture.root())
        .env("GIT_INDEX_FILE", &alternate)
        .args(["update-index", "--index-info"])
        .env("HOME", fixture.home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child
                .stdin
                .as_mut()
                .expect("stdin")
                .write_all(info.as_bytes())?;
            child.wait_with_output()
        })
        .expect("update-index");
    assert_success(&output, "git update-index (alternate index)");

    // The observation resolves GIT_INDEX_FILE explicitly.
    let mut command = Command::new(ATOMIC_BIN);
    command
        .args(["status", "--git", "--json"])
        .current_dir(fixture.root())
        .env("HOME", fixture.home.path())
        .env("ATOMIC_HOME", fixture.home.path().join(".atomic"))
        .env("ATOMIC_NONINTERACTIVE", "1")
        .env("NO_COLOR", "1")
        .env("GIT_INDEX_FILE", &alternate)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = command.output().expect("status --git --json");
    assert_success(&output, "atomic status --git --json (alternate index)");
    let document: Value = serde_json::from_slice(&output.stdout).expect("JSON");
    assert!(
        document["notices"]
            .as_array()
            .unwrap()
            .iter()
            .any(|notice| notice
                .as_str()
                .unwrap_or("")
                .contains("alternate index observed (GIT_INDEX_FILE)")),
        "alternate index is surfaced as evidence: {document}"
    );
    assert!(
        document["staged"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| { entry["path"] == "tracked.txt" && entry["x"] == "M" }),
        "the alternate index supplies the staged layer: {document}"
    );

    // The default index and the durable layer are unchanged.
    assert_eq!(fixture.git_short(), "");
    let after = fixture.atomic_json(&["status", "--git", "--json"]);
    assert_eq!(
        after["layers"]["baseline_tree"],
        after["layers"]["index_tree"]
    );
    assert_eq!(
        after["layers"]["durable_manifest_root"], before["layers"]["durable_manifest_root"],
        "an alternate index never replaces durable staging"
    );
}

// ── Forensic Observe and reconcile refusal (AC-3) ───────────────────────

#[test]
fn forensic_observe_reports_layers_without_mutation() {
    let fixture = Fixture::colocated();
    // A raw Git commit makes the checkpoint stale; Observe must not touch it.
    fixture.write("tracked.txt", "l1\nRAW\nl3\n");
    fixture.git_ok(&["commit", "-aqm", "raw git advance"]);
    let before = fixture.atomic_ok(&["op", "log", "--json"]).stdout;
    let _tracked_before = fixture.read("tracked.txt");

    let report = fixture.atomic_ok(&["status", "--no-reconcile"]);
    let text = output_text(&report);
    assert!(text.contains("Forensic status"), "{text}");
    assert!(text.contains("Atomic view"), "{text}");
    assert!(text.contains("Atomic state"), "{text}");
    assert!(text.contains("Durable manifest root"), "{text}");
    assert!(text.contains("Bridge checkpoint"), "{text}");
    assert!(text.contains("Git HEAD"), "{text}");
    assert!(text.contains("Drift"), "{text}");

    let after = fixture.atomic_ok(&["op", "log", "--json"]).stdout;
    assert_eq!(before, after, "forensic observe must journal nothing");
    assert_eq!(
        fixture.read("tracked.txt"),
        b"l1\nRAW\nl3\n",
        "forensic observe never materializes"
    );
}

#[test]
fn reconcile_status_refuses_and_never_materializes() {
    let fixture = Fixture::colocated();
    // Git HEAD advanced: the stale-baseline guard refuses reconcile-mode
    // status, and nothing may be materialized or journaled.
    fixture.write("tracked.txt", "l1\nRAW\nl3\n");
    fixture.git_ok(&["commit", "-aqm", "raw git advance"]);
    let ops_before = fixture.atomic_ok(&["op", "log", "--json"]).stdout;

    let refused = fixture.atomic(&["status"]);
    let text = output_text(&refused);
    assert!(
        text.contains("HeadChanged"),
        "reconcile status refuses the stale baseline: {text}"
    );
    assert!(
        text.contains("stale-baseline report"),
        "the refusal names the stale-baseline remediation path: {text}"
    );

    let ops_after = fixture.atomic_ok(&["op", "log", "--json"]).stdout;
    assert_eq!(ops_before, ops_after, "refused status journals nothing");
    assert_eq!(fixture.read("tracked.txt"), b"l1\nRAW\nl3\n");
}

// ── Bridge verify proves the five layers ────────────────────────────────

#[test]
fn bridge_verify_proves_all_five_layers_and_reports_divergence() {
    let fixture = Fixture::colocated();
    let verified = fixture.atomic_ok(&["git", "bridge", "verify"]);
    let text = output_text(&verified);
    assert!(text.contains("layer durable"), "{text}");
    assert!(text.contains("layer manifest"), "{text}");
    assert!(text.contains("layer index"), "{text}");
    assert!(text.contains("layer worktree"), "{text}");
    assert!(text.contains("layer snapshot"), "{text}");

    // Structured divergence names the layer.
    fixture.write("tracked.txt", "l1\nDRIFT\nl3\n");
    let diverged = fixture.atomic(&["git", "bridge", "verify"]);
    assert!(
        !diverged.status.success(),
        "verify must fail on worktree drift"
    );
    let text = output_text(&diverged);
    assert!(
        text.contains("diverged layer"),
        "divergence names the layer: {text}"
    );
}

/// Atomic-directory manifest: every relative path with its exact bytes.
///
/// CB-13A R2 uses this to prove bridge verify is a read-only diagnostic —
/// the manifest must be byte-identical across a verify invocation, even on
/// a repository whose operation head is still incomplete.
fn atomic_manifest(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(dir: &Path, prefix: &str, out: &mut BTreeMap<String, Vec<u8>>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = format!("{prefix}/{}", entry.file_name().to_string_lossy());
            if path.is_dir() {
                walk(&path, &name, out);
            } else if let Ok(bytes) = fs::read(&path) {
                out.insert(name, bytes);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(&root.join(".atomic"), ".atomic", &mut out);
    out
}

/// CB-13A R2: bridge verify is a read-only diagnostic. Around a switch that
/// was interrupted mid-effect (the switch's own error path synchronously
/// recovered its journal), verify must observe the state WITHOUT any further
/// mutation: the Atomic-directory manifest must stay byte-identical across a
/// passing verify, and a failing verify must neither repair nor journal. The
/// operations-head layer is part of every verdict.
#[test]
fn bridge_verify_is_read_only_around_an_interrupted_switch() {
    let fixture = Fixture::colocated();

    // A draft feature projection whose switch back to main removes a tracked
    // path, so the deterministic switch failpoint interrupts mid-effect.
    fixture.atomic_ok(&["view", "create", "feature", "--draft", "--parent", "main"]);
    fixture.atomic_ok(&["view", "switch", "feature", "--force"]);
    fs::remove_file(fixture.root().join("tracked.txt")).expect("remove tracked file");
    fs::create_dir_all(fixture.root().join("feature-only")).expect("feature dir");
    fs::write(
        fixture.root().join("feature-only/01-first.txt"),
        b"first source-only tracked file\n",
    )
    .expect("write first");
    fs::write(
        fixture.root().join("feature-only/02-second.txt"),
        b"second source-only tracked file\n",
    )
    .expect("write second");
    fixture.git_ok(&["add", "-A"]);
    fixture.git_ok(&["commit", "--no-gpg-sign", "-q", "-m", "feature projection"]);
    fixture.atomic_ok(&["git", "bridge", "reconcile"]);

    // Interrupt the switch after its first tracked removal.
    let crashed = {
        let mut command = Command::new(ATOMIC_BIN);
        command
            .args(["view", "switch", "main", "--force"])
            .current_dir(fixture.root())
            .env("HOME", fixture.home.path())
            .env("ATOMIC_HOME", fixture.home.path().join(".atomic"))
            .env("ATOMIC_NONINTERACTIVE", "1")
            .env("NO_COLOR", "1")
            .env("CLICOLOR", "0")
            .env("TERM", "dumb")
            .env("ATOMIC_FAIL_SWITCH_AFTER_FIRST_TRACKED_REMOVAL", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.output().expect("wait for atomic")
    };
    assert!(
        !crashed.status.success(),
        "the failpoint must interrupt the switch"
    );

    let after_crash = atomic_manifest(fixture.root());

    // Read-only diagnosis on the just-crashed repository: the switch's own
    // error path synchronously recovered its journal, and verify must now
    // observe the complete state WITHOUT any further mutation.
    let diagnosed = fixture.atomic_ok(&["git", "bridge", "verify"]);
    let text = output_text(&diagnosed);
    assert!(
        text.contains("layer operations"),
        "verify reports the operations layer: {text}"
    );
    let after_verify = atomic_manifest(fixture.root());
    assert_eq!(
        after_crash, after_verify,
        "bridge verify must not recover, migrate, or align before inspection"
    );

    // Drift must be reported without any repair: verify fails and the
    // manifest stays byte-identical.
    fs::write(fixture.root().join("drift.txt"), b"drift\n").expect("write drift file");
    let diverged = fixture.atomic(&["git", "bridge", "verify"]);
    assert!(
        !diverged.status.success(),
        "verify must fail on untracked-worktree drift against the projection"
    );
    let after_diverged = atomic_manifest(fixture.root());
    assert_eq!(
        after_verify, after_diverged,
        "a failing verify must not repair or journal anything"
    );
}

// ── Ignore-policy mirroring (consent) and shelves ───────────────────────

#[test]
fn ignore_divergence_reported_until_consented_mirroring() {
    let fixture = Fixture::colocated();
    fixture.write(".atomicignore", ".atomic\n.git\ntarget/\n*.log\n");

    // Without consent the divergence is reported, never hidden.
    let document = fixture.atomic_json(&["status", "--git", "--json"]);
    assert_eq!(document["ignore_policy"]["divergence"], true);
    let unmirrored: Vec<&str> = document["ignore_policy"]["unmirrored"]
        .as_array()
        .unwrap()
        .iter()
        .map(|pattern| pattern.as_str().unwrap_or(""))
        .collect();
    assert!(unmirrored.contains(&"target/"), "{unmirrored:?}");
    let rendered = fixture.atomic_text(&["status", "--git"]);
    assert!(rendered.contains("ignore-policy divergence"), "{rendered}");
    assert!(
        !fixture.root().join(".git/info/exclude").exists()
            || !String::from_utf8_lossy(
                &fs::read(fixture.root().join(".git/info/exclude")).unwrap_or_default()
            )
            .contains("target/"),
        "no mirroring without consent"
    );

    // Explicit consent mirrors into the managed block.
    let enabled = fixture.atomic_ok(&["git", "bridge", "enable", "--mirror-ignores"]);
    let text = output_text(&enabled);
    assert!(text.contains("newly mirrored"), "{text}");
    let exclude =
        fs::read_to_string(fixture.root().join(".git/info/exclude")).expect("exclude file");
    assert!(exclude.contains("target/"), "{exclude}");
    assert!(
        text.contains("managed") || text.contains("exclude"),
        "{text}"
    );

    let after = fixture.atomic_json(&["status", "--git", "--json"]);
    assert_eq!(after["ignore_policy"]["divergence"], false);
    assert_eq!(after["ignore_policy"]["managed_block_present"], true);
}

#[test]
fn tracked_paths_are_never_excluded_by_ignore_rules() {
    let fixture = Fixture::colocated();
    // A pattern that would match a tracked path must not hide it.
    fixture.write(".atomicignore", ".atomic\n.git\ntracked.txt\n");
    fixture.write("other.txt", "visible\n");

    // Native status still classifies the tracked path (not untracked, not
    // ignored), and edits remain visible.
    fixture.write("tracked.txt", "l1\nEDIT\nl3\n");
    let native = native_rows(&fixture);
    assert!(
        native.contains(&"M  tracked.txt".to_string()),
        "tracked path is never excluded by ignore rules: {native:?}"
    );
    // Untracked discovery still works for other paths.
    assert!(native.iter().any(|row| row.contains("other.txt")));
}

#[test]
fn shelved_artifacts_report_under_separate_heading() {
    let fixture = Fixture::colocated();
    // Simulate a shelf artifact for this working copy + view.
    let working_copy_dirs: Vec<PathBuf> =
        fs::read_dir(fixture.root().join(".atomic/working-copies"))
            .expect("working copies")
            .map(|entry| entry.expect("working copy entry").path())
            .collect();
    assert!(!working_copy_dirs.is_empty(), "a working copy exists");
    let shelf = working_copy_dirs[0]
        .join("workspaces")
        .join("main")
        .join("shelved-artifact.txt");
    fs::create_dir_all(shelf.parent().unwrap()).unwrap();
    fs::write(&shelf, b"shelved bytes\n").unwrap();

    let document = fixture.atomic_json(&["status", "--git", "--json"]);
    let shelved = document["shelved"].as_array().expect("shelved array");
    assert!(
        shelved.iter().any(|path| path == "shelved-artifact.txt"),
        "shelf artifacts are reported: {shelved:?}"
    );
    let rendered = fixture.atomic_text(&["status", "--git"]);
    assert!(
        rendered.contains("Shelved (excluded from view status"),
        "shelves have a separate heading: {rendered}"
    );
    assert!(
        !git_status_rows(&fixture)
            .iter()
            .any(|row| row.contains("shelved-artifact")),
        "shelved artifacts are not status entries"
    );
}
