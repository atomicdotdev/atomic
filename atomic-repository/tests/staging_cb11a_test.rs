//! CB-11A five-layer staging observation tests (RFC §9.2/§9.3).
//!
//! The five layers are: durable tracking (Atomic `TREE` projection), the
//! durable manifest root, the primary Git index (baseline → index), the
//! physical worktree (index → worktree), and the pending snapshot (the
//! complete baseline → worktree patch). These tests prove:
//!
//! - index movement (`git add`, `git reset`, `git rm --cached`, alternate
//!   `GIT_INDEX_FILE`) is mirrored into `StagingState` only and never mutates
//!   durable tracking;
//! - stage/unstage-style classification (staged X column vs unstaged Y
//!   column) matches `git status --short` for the parity subset;
//! - only stage-0 index entries project to the two-column codes and a
//!   snapshot is the complete baseline→worktree patch, never staged content;
//! - the listed edge cases hold: intra-file partial staging, intent-to-add,
//!   skip-worktree, assume-unchanged, sparse index expansion, staged
//!   delete/recreate, mode changes and type changes.

#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::Command;

use atomic_core::change::ChangeHeader;
use atomic_core::{GitHashAlgorithm, WorkingCopyId};
use atomic_repository::record::RecordOptions;
use atomic_repository::{
    observe_staging_state, ConversionPolicy, Repository, StageCode, StagingNotice, StagingState,
};
use tempfile::TempDir;

fn git_hash_object(root: &Path, content: &str) -> String {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["hash-object", "-w", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("hash-object");
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(content.as_bytes())
            .expect("write content");
    }
    let output = child.wait_with_output().expect("hash-object output");
    assert!(
        output.status.success(),
        "hash-object: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_AUTHOR_NAME", "CB-11A Tests")
        .env("GIT_AUTHOR_EMAIL", "cb11a@example.com")
        .env("GIT_COMMITTER_NAME", "CB-11A Tests")
        .env("GIT_COMMITTER_EMAIL", "cb11a@example.com")
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    text.strip_suffix('\n').unwrap_or(&text).to_string()
}

struct Colocated {
    #[allow(dead_code)]
    temp: TempDir,
    root: std::path::PathBuf,
    repo: Repository,
    working_copy: WorkingCopyId,
    policy: ConversionPolicy,
}

impl Colocated {
    /// A Git repository with one committed file, durably tracked by Atomic.
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let root = temp.path().to_path_buf();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "core.autocrlf", "false"]);
        git(&root, &["config", "commit.gpgsign", "false"]);
        fs::write(root.join("f.txt"), b"l1\nl2\nl3\nl4\nl5\n").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "base"]);
        // Mirror the shadow excludes `atomic git import` seeds so the
        // fixture's `git status --short` is not polluted by `.atomic/`.
        let info = root.join(".git").join("info");
        fs::create_dir_all(&info).unwrap();
        fs::write(info.join("exclude"), b"/.atomic/\n").unwrap();

        let repo = Repository::init(&root).unwrap();
        let working_copy = repo.require_working_copy_id().unwrap();
        repo.add(working_copy, "f.txt", Default::default()).unwrap();
        repo.record(
            working_copy,
            ChangeHeader::new("track baseline"),
            RecordOptions::new().add_path("f.txt"),
        )
        .unwrap();
        Self {
            temp,
            root,
            repo,
            working_copy,
            policy: ConversionPolicy::new(GitHashAlgorithm::Sha1),
        }
    }

    fn observe(&self) -> StagingState {
        observe_staging_state(&self.repo, self.working_copy, &self.policy).unwrap()
    }

    /// The durable manifest root — layer 1/2 evidence.
    fn durable_root(&self) -> String {
        let view = self.repo.desired_view_name(self.working_copy).unwrap();
        self.repo
            .project_tree(&view, &self.policy)
            .unwrap()
            .manifest
            .root()
            .content_key
    }

    fn durable_tracked_paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = self
            .repo
            .list_tracked_files()
            .unwrap()
            .into_iter()
            .filter(|file| !file.is_directory)
            .map(|file| file.path.display().to_string())
            .collect();
        paths.sort();
        paths
    }

    fn git_short(&self) -> String {
        git(&self.root, &["status", "--short"])
    }
}

fn entry<'a>(state: &'a StagingState, path: &str) -> &'a atomic_repository::StagingEntry {
    state
        .entries
        .iter()
        .find(|entry| entry.path.as_bytes() == path.as_bytes())
        .unwrap_or_else(|| panic!("no staging entry for '{path}'"))
}

fn paths(state: &StagingState) -> Vec<String> {
    state
        .entries
        .iter()
        .map(|entry| String::from_utf8_lossy(entry.path.as_bytes()).into_owned())
        .collect()
}

// ── Layer alignment ─────────────────────────────────────────────────────

#[test]
fn synchronized_layers_are_clean_and_manifest_backed() {
    let fixture = Colocated::new();
    let state = fixture.observe();

    assert!(
        state.is_clean(),
        "expected clean, got {:?}",
        fixture.git_short()
    );
    assert!(
        state.notices.is_empty(),
        "unexpected notices: {:?}",
        state.notices
    );
    let baseline = state.baseline_tree.as_ref().expect("HEAD exists").clone();
    let index = state
        .index_tree
        .as_ref()
        .expect("index tree computable")
        .clone();
    assert_eq!(baseline, index, "clean: baseline tree == index tree");
    let root = state
        .durable_manifest_root
        .as_ref()
        .expect("durable manifest evaluated");
    assert_eq!(root.content_key, fixture.durable_root());
    assert_eq!(
        state.entries.len(),
        1,
        "exactly the tracked path is reported: {:?}",
        paths(&state)
    );
    assert!(state.snapshot.is_none() && state.remainder.is_none());
}

#[test]
fn unstaged_edit_reports_modified_in_y_only() {
    let fixture = Colocated::new();
    fs::write(fixture.root.join("f.txt"), b"l1\nEDIT2\nl3\nl4\nl5\n").unwrap();

    let state = fixture.observe();
    let entry = entry(&state, "f.txt");
    assert_eq!(entry.x, StageCode::Unmodified, "nothing staged");
    assert_eq!(entry.y, StageCode::Modified);
    assert!(state.staged().next().is_none());
    assert_eq!(state.unstaged().count(), 1);
    // Git agrees on the unstaged layer.
    assert_eq!(fixture.git_short(), " M f.txt");
}

#[test]
fn staged_edit_reports_modified_in_x_only() {
    let fixture = Colocated::new();
    fs::write(fixture.root.join("f.txt"), b"l1\nEDIT2\nl3\nl4\nl5\n").unwrap();
    git(&fixture.root, &["add", "f.txt"]);

    let state = fixture.observe();
    let entry = entry(&state, "f.txt");
    assert_eq!(entry.x, StageCode::Modified);
    assert_eq!(entry.y, StageCode::Unmodified);
    assert_eq!(state.staged().count(), 1);
    assert!(state.unstaged().next().is_none());
    assert_eq!(fixture.git_short(), "M  f.txt");
}

// ── Partial (intra-file) staging ────────────────────────────────────────

#[test]
fn partial_staging_reports_both_columns_and_leaves_snapshot_complete() {
    let fixture = Colocated::new();
    // Two single-line edits leave the worktree fully modified. A snapshot is
    // taken first: it captures the complete baseline→worktree patch. The
    // index is then given an intra-file *partial* selection (an intermediate
    // blob that matches neither baseline nor worktree), exactly what
    // `git add -p` produces at the index level.
    fs::write(fixture.root.join("f.txt"), b"l1\nA2\nl3\nl4\nA5\n").unwrap();
    assert_eq!(fixture.git_short(), " M f.txt");
    fixture
        .repo
        .snapshot(
            fixture.working_copy,
            ChangeHeader::new("pending work"),
            RecordOptions::new().add_path("f.txt"),
        )
        .unwrap();
    let before_stage_snapshot = fixture
        .repo
        .snapshot_status(fixture.working_copy)
        .unwrap()
        .snapshot
        .expect("pending snapshot exists after snapshot()");

    let partial = git_hash_object(&fixture.root, "l1\nA2\nl3\nl4\nl5\n");
    let mut child = Command::new("git")
        .arg("-C")
        .arg(&fixture.root)
        .args(["update-index", "--index-info"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("update-index");
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(format!("100644 {partial} 0\tf.txt\n").as_bytes())
            .expect("write index info");
    }
    let output = child.wait_with_output().expect("update-index output");
    assert!(
        output.status.success(),
        "update-index --index-info: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.git_short(), "MM f.txt", "partial stage is MM");

    let state = fixture.observe();
    let entry = entry(&state, "f.txt");
    // Baseline → index and index → worktree both differ: both columns report.
    assert_eq!(entry.x, StageCode::Modified, "staged hunk is the X column");
    assert_eq!(
        entry.y,
        StageCode::Modified,
        "remaining hunk is the Y column"
    );
    assert_eq!(fixture.git_short(), "MM f.txt");

    // The snapshot is the complete baseline→worktree patch, not staged
    // content: staging part of the worktree edit cannot change it, and the
    // staged selection shows up in the index layer, never in the snapshot.
    let status = fixture.repo.snapshot_status(fixture.working_copy).unwrap();
    assert_eq!(
        status.snapshot,
        Some(before_stage_snapshot),
        "index movement must not change the pending snapshot"
    );
}

// ── Intent-to-add ───────────────────────────────────────────────────────

#[test]
fn intent_to_add_records_intent_without_staged_content() {
    let fixture = Colocated::new();
    fs::write(fixture.root.join("g.txt"), b"new\n").unwrap();
    git(&fixture.root, &["add", "--intent-to-add", "g.txt"]);
    assert_eq!(fixture.git_short(), " A g.txt");

    let state = fixture.observe();
    assert_eq!(fixture.git_short(), " A g.txt", "git unchanged");
    let entry = entry(&state, "g.txt");
    assert_eq!(
        entry.x,
        StageCode::Unmodified,
        "intent-to-add stages nothing"
    );
    assert_eq!(entry.y, StageCode::Added, "worktree holds unstaged content");
    assert!(entry.intent_to_add);
    assert!(!entry.durable_tracked, "not durably tracked yet");
    assert!(state.notices.iter().any(|notice| matches!(
        notice,
        StagingNotice::IntentToAdd { paths } if paths == &vec!["g.txt".to_string()]
    )));
}

// ── Flagged entries ─────────────────────────────────────────────────────

#[test]
fn skip_worktree_exempts_worktree_comparison() {
    let fixture = Colocated::new();
    git(&fixture.root, &["update-index", "--skip-worktree", "f.txt"]);
    fs::write(fixture.root.join("f.txt"), b"l1\nLOCAL\nl3\nl4\nl5\n").unwrap();

    let state = fixture.observe();
    let entry = entry(&state, "f.txt");
    assert!(entry.skip_worktree);
    assert_eq!(entry.x, StageCode::Unmodified);
    assert_eq!(entry.y, StageCode::Unmodified, "flagged entries are exempt");
    assert!(state.is_clean());
    assert!(state.notices.iter().any(|notice| matches!(
        notice,
        StagingNotice::FlaggedEntries { skip_worktree, .. } if skip_worktree == &vec!["f.txt".to_string()]
    )));
}

#[test]
fn assume_unchanged_exempt_from_change_detection() {
    let fixture = Colocated::new();
    git(
        &fixture.root,
        &["update-index", "--assume-unchanged", "f.txt"],
    );
    fs::write(fixture.root.join("f.txt"), b"l1\nLOCAL\nl3\nl4\nl5\n").unwrap();

    let state = fixture.observe();
    let entry = entry(&state, "f.txt");
    assert!(entry.assume_unchanged);
    assert_eq!(entry.y, StageCode::Unmodified);
    assert!(state.is_clean());
    assert!(state.notices.iter().any(|notice| matches!(
        notice,
        StagingNotice::FlaggedEntries { assume_unchanged, .. }
            if assume_unchanged == &vec!["f.txt".to_string()]
    )));
}

// ── Sparse index ────────────────────────────────────────────────────────

#[test]
fn sparse_index_entries_are_not_deletions() {
    let fixture = Colocated::new();
    fs::create_dir_all(fixture.root.join("sub")).unwrap();
    fs::write(fixture.root.join("sub/deep.txt"), b"deep\n").unwrap();
    git(&fixture.root, &["add", "sub/deep.txt"]);
    git(&fixture.root, &["commit", "-q", "-m", "subtree"]);

    // Re-track the new baseline so the durable layer matches.
    let repo = &fixture.repo;
    repo.add(fixture.working_copy, "sub/deep.txt", Default::default())
        .unwrap();
    repo.record(
        fixture.working_copy,
        ChangeHeader::new("track subtree"),
        RecordOptions::new().add_path("sub/deep.txt"),
    )
    .unwrap();

    // Cone sparse-checkout excluding sub/ produces a sparse directory entry
    // when the sparse index is enabled (git >= 2.36).
    git(&fixture.root, &["config", "index.sparse", "true"]);
    git(&fixture.root, &["sparse-checkout", "init", "--cone"]);
    git(
        &fixture.root,
        &[
            "sparse-checkout",
            "set",
            "--cone",
            "--skip-checks",
            "nonexistent",
        ],
    );
    let listing = git(&fixture.root, &["ls-files", "-t"]);
    assert!(
        listing.contains("S sub/"),
        "expected a sparse directory entry, got: {listing}"
    );

    let state = fixture.observe();
    assert!(
        state
            .notices
            .iter()
            .any(|notice| matches!(notice, StagingNotice::SparseIndexEntries { paths } if paths.iter().any(|p| p == "sub/deep.txt"))),
        "sparse-covered paths must be reported: {:?}",
        state.notices
    );
    // Sparse absence is NOT a deletion: no column may report `D` for any
    // expanded path, and the observation stays clean.
    for entry in &state.entries {
        assert_ne!(
            entry.x,
            StageCode::Deleted,
            "sparse absence is not a deletion: {:?}",
            entry.path
        );
        assert_ne!(
            entry.y,
            StageCode::Deleted,
            "sparse absence is not a deletion: {:?}",
            entry.path
        );
    }
    assert!(
        state.is_clean(),
        "sparse-checkout exclusion is not pending work: {:?}",
        fixture.git_short()
    );
}

// ── Staged deletion and recreation ──────────────────────────────────────

#[test]
fn staged_delete_with_recreated_worktree_reports_untracked_too() {
    let fixture = Colocated::new();
    git(&fixture.root, &["rm", "--cached", "-q", "f.txt"]);

    let state = fixture.observe();
    let entry = entry(&state, "f.txt");
    assert_eq!(entry.x, StageCode::Deleted);
    assert_eq!(entry.y, StageCode::Unmodified);
    assert!(
        entry.durable_tracked,
        "index movement never untracks durably"
    );
    assert!(state.staged().next().is_some());
    let root_before = fixture.durable_root();
    let tracked_before = fixture.durable_tracked_paths();

    // Recreate the file in the worktree: staged deletion + untracked file.
    fs::write(fixture.root.join("f.txt"), b"recreated\n").unwrap();
    let state = fixture.observe();
    let staged_delete = state
        .entries
        .iter()
        .find(|entry| entry.x == StageCode::Deleted)
        .expect("staged deletion still reported");
    assert_eq!(staged_delete.path.as_bytes(), b"f.txt");
    assert!(
        state
            .entries
            .iter()
            .any(|entry| entry.x == StageCode::Untracked && entry.y == StageCode::Untracked),
        "recreated file is untracked: {:?}",
        paths(&state)
    );
    assert_eq!(fixture.durable_root(), root_before);
    assert_eq!(fixture.durable_tracked_paths(), tracked_before);
}

// ── Mode and type changes ───────────────────────────────────────────────

#[test]
fn executable_mode_change_reports_modified_unstaged() {
    let fixture = Colocated::new();
    let mut permissions = fs::metadata(fixture.root.join("f.txt"))
        .unwrap()
        .permissions();
    use std::os::unix::fs::PermissionsExt;
    permissions.set_mode(0o755);
    fs::set_permissions(fixture.root.join("f.txt"), permissions).unwrap();
    assert_eq!(fixture.git_short(), " M f.txt");

    let state = fixture.observe();
    let entry = entry(&state, "f.txt");
    assert_eq!(entry.x, StageCode::Unmodified);
    assert_eq!(
        entry.y,
        StageCode::Modified,
        "mode change is M in the subset"
    );
}

#[test]
fn regular_to_symlink_reports_type_change() {
    let fixture = Colocated::new();
    fs::remove_file(fixture.root.join("f.txt")).unwrap();
    std::os::unix::fs::symlink("l1", fixture.root.join("f.txt")).unwrap();
    assert_eq!(fixture.git_short(), " T f.txt");

    let state = fixture.observe();
    let entry = entry(&state, "f.txt");
    assert_eq!(entry.x, StageCode::Unmodified);
    assert_eq!(entry.y, StageCode::TypeChanged);
}

// ── Durable tracking is never mutated by index movement ─────────────────

#[test]
fn index_movement_never_mutates_durable_tracking() {
    let fixture = Colocated::new();
    let root_before = fixture.durable_root();
    let tracked_before = fixture.durable_tracked_paths();

    // git add (stage content)
    fs::write(fixture.root.join("f.txt"), b"l1\nEDIT\nl3\nl4\nl5\n").unwrap();
    git(&fixture.root, &["add", "f.txt"]);
    let state = fixture.observe();
    assert_eq!(entry(&state, "f.txt").x, StageCode::Modified);
    assert_eq!(
        fixture.durable_root(),
        root_before,
        "git add must not touch TREE"
    );
    assert_eq!(fixture.durable_tracked_paths(), tracked_before);

    // git reset (unstage)
    git(&fixture.root, &["reset", "-q", "f.txt"]);
    let state = fixture.observe();
    assert_eq!(entry(&state, "f.txt").x, StageCode::Unmodified);
    assert_eq!(
        fixture.durable_root(),
        root_before,
        "git reset must not touch TREE"
    );

    // git rm --cached (untrack in index only)
    git(&fixture.root, &["rm", "--cached", "-q", "f.txt"]);
    let state = fixture.observe();
    assert_eq!(entry(&state, "f.txt").x, StageCode::Deleted);
    assert!(entry(&state, "f.txt").durable_tracked);
    assert_eq!(
        fixture.durable_root(),
        root_before,
        "git rm --cached must not touch TREE"
    );
    assert_eq!(fixture.durable_tracked_paths(), tracked_before);
}

#[test]
fn only_stage_zero_projects_and_unmerged_stages_are_reported() {
    let fixture = Colocated::new();
    // Stages 1-3 for f.txt (unmerged), worktree content present.
    let base = git(&fixture.root, &["rev-parse", "HEAD:f.txt"]);
    let ours = git_hash_object(&fixture.root, "l1\nOURS\nl3\nl4\nl5\n");
    let theirs = git_hash_object(&fixture.root, "l1\nTHEIRS\nl3\nl4\nl5\n");
    // Feed stage 1/2/3 lines through stdin.
    git(&fixture.root, &["update-index", "--force-remove", "f.txt"]);
    let info =
        format!("100644 {base} 1\tf.txt\n100644 {ours} 2\tf.txt\n100644 {theirs} 3\tf.txt\n");
    let mut child = Command::new("git")
        .arg("-C")
        .arg(&fixture.root)
        .args(["update-index", "--index-info"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("update-index");
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(info.as_bytes())
            .expect("write index info");
    }
    let output = child.wait_with_output().expect("update-index output");
    assert!(
        output.status.success(),
        "update-index --index-info: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fixture.git_short().contains("UU"),
        "git reports UU: {}",
        fixture.git_short()
    );

    let state = fixture.observe();
    let entry = entry(&state, "f.txt");
    assert!(entry.unmerged, "stages 1-3 must be reported as unmerged");
    assert_eq!(entry.x, StageCode::Unmerged);
    assert_eq!(entry.y, StageCode::Unmerged);
    assert!(
        state
            .notices
            .iter()
            .any(|notice| matches!(notice, StagingNotice::UnmergedIndexEntries { paths } if paths.contains(&"f.txt".to_string()))),
        "unmerged stages produce a notice: {:?}",
        state.notices
    );
    // Unmerged entries are never claimed as stage-0 staged content.
    assert!(entry.index_mode.is_none());
}
