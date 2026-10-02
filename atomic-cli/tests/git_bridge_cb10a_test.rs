//! CB-10A integration coverage: persistent ref mappings and three-way
//! reconciliation (RFC §8.1/§8.5).
//!
//! Every fixture drives a fresh `atomic` binary against a colocated Git
//! repository, mirroring the bridge contract end to end:
//!
//! - the three-way matrix: neither moved (no-op), Git-only (import),
//!   Atomic-only (export under the expected-old lease), both moved
//!   (containment decides; incompatible persists `Diverged` and moves
//!   neither side);
//! - the mapping persists across fresh processes (every command reopens the
//!   shared pristine) and is keyed by durable view identity;
//! - explicit Draft publication and view-delete lifecycle reconciliation;
//! - observe/status paths never materialize and never move a ref.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use git2::{Oid, Repository as GitRepository, Signature};
use tempfile::TempDir;

const ATOMIC_BIN: &str = env!("CARGO_BIN_EXE_atomic");

fn atomic(root: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(ATOMIC_BIN)
        .args(args)
        .current_dir(root)
        .env("HOME", home)
        .env("ATOMIC_HOME", home.join(".atomic"))
        .env("GIT_AUTHOR_NAME", "CB-10A Tests")
        .env("GIT_AUTHOR_EMAIL", "cb10a@example.com")
        .env("GIT_COMMITTER_NAME", "CB-10A Tests")
        .env("GIT_COMMITTER_EMAIL", "cb10a@example.com")
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

fn atomic_ok(root: &Path, home: &Path, args: &[&str]) -> String {
    let output = atomic(root, home, args);
    assert!(
        output.status.success(),
        "atomic {args:?} failed:\n{}",
        atomic_text(&output)
    );
    atomic_text(&output)
}

fn atomic_fail(root: &Path, home: &Path, args: &[&str]) -> String {
    let output = atomic(root, home, args);
    assert!(
        !output.status.success(),
        "atomic {args:?} unexpectedly succeeded:\n{}",
        atomic_text(&output)
    );
    atomic_text(&output)
}

/// A colocated Git + Atomic repository with an active shadow sync.
struct Colocated {
    root: TempDir,
    home: TempDir,
}

impl Colocated {
    fn new(name: &str) -> Self {
        let root = TempDir::new().expect("repo tempdir");
        let home = TempDir::new().expect("home tempdir");
        assert!(git(root.path(), &["init", "-q", "-b", "main"])
            .status
            .success());
        fs::write(root.path().join("tracked.txt"), b"anchor me\n").expect("write file");
        assert!(git(root.path(), &["add", "tracked.txt"]).status.success());
        assert!(git(root.path(), &["commit", "-qm", "anchor base"])
            .status
            .success());
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
        // CB-10A: establish the Synchronized baseline mapping for the main
        // view. The reconcile also proves the whole five-layer state.
        atomic_ok(root.path(), home.path(), &["git", "bridge", "reconcile"]);
        let _ = name;
        Self { root, home }
    }

    fn root(&self) -> &Path {
        self.root.path()
    }

    fn home(&self) -> &Path {
        self.home.path()
    }

    fn atomic(&self, args: &[&str]) -> String {
        atomic_ok(self.root(), self.home(), args)
    }

    fn atomic_fails(&self, args: &[&str]) -> String {
        atomic_fail(self.root(), self.home(), args)
    }

    fn atomic_output(&self, args: &[&str]) -> Output {
        atomic(self.root(), self.home(), args)
    }

    fn git(&self, args: &[&str]) -> String {
        let output = git(self.root(), args);
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("utf8 git output")
            .trim()
            .to_string()
    }

    fn tip(&self, reference: &str) -> Option<Oid> {
        let repository = GitRepository::open(self.root()).expect("git repo");
        repository
            .find_reference(reference)
            .ok()
            .and_then(|reference| reference.target())
    }

    #[allow(dead_code)]
    fn status_text(&self) -> String {
        self.atomic(&["git", "bridge", "status"])
    }
}

fn key_file(home: &Path) -> std::path::PathBuf {
    let mut secret = [0u8; 32];
    for (index, byte) in secret.iter_mut().enumerate() {
        *byte = 0x42u8.wrapping_add((index as u8).wrapping_mul(11).wrapping_add(5));
    }
    let hex: String = secret.iter().map(|byte| format!("{byte:02x}")).collect();
    let path = home.join("cb10a-key.hex");
    fs::write(&path, hex).expect("write key file");
    path
}

fn git(root: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "CB-10A Tests")
        .env("GIT_AUTHOR_EMAIL", "cb10a@example.com")
        .env("GIT_COMMITTER_NAME", "CB-10A Tests")
        .env("GIT_COMMITTER_EMAIL", "cb10a@example.com")
        .output()
        .expect("run git")
}

fn commit_file(repository: &GitRepository, path: &str, content: &[u8], message: &str) -> Oid {
    let worktree = repository.workdir().expect("Git worktree");
    fs::write(worktree.join(path), content).expect("write committed file");
    let mut index = repository.index().expect("Git index");
    index.add_path(Path::new(path)).expect("stage file");
    index.write().expect("write index");
    let tree_oid = index.write_tree().expect("write tree");
    let tree = repository.find_tree(tree_oid).expect("find tree");
    let signature = Signature::now("CB-10A Tests", "cb10a@example.com").expect("signature");
    let parent = repository
        .head()
        .ok()
        .and_then(|head| head.target())
        .and_then(|oid| repository.find_commit(oid).ok());
    match parent.as_ref() {
        Some(parent) => repository
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                message,
                &tree,
                &[parent],
            )
            .expect("commit with parent"),
        None => repository
            .commit(Some("HEAD"), &signature, &signature, message, &tree, &[])
            .expect("initial commit"),
    }
}

fn view_merkle(fixture: &Colocated) -> String {
    let output = fixture.atomic(&["view", "show", "main"]);
    output
        .lines()
        .find_map(|line| line.strip_prefix("Merkle: "))
        .map(str::trim)
        .expect("view merkle")
        .to_string()
}

fn disable_shadow_sync(root: &Path) {
    // Shadow sync is keyed on the managed /.atomic/ exclude line; removing it
    // makes record a durable-only transition so reconcile's Atomic→Git
    // export path is exercised under its expected-old lease.
    let exclude = root.join(".git/info/exclude");
    let content = fs::read_to_string(&exclude).expect("read exclude");
    let filtered: String = content
        .lines()
        .filter(|line| line.trim() != "/.atomic/")
        .map(|line| format!("{line}\n"))
        .collect();
    fs::write(exclude, filtered).expect("write exclude");
}

// ---------------------------------------------------------------------------
// Three-way matrix (RFC §8.5)
// ---------------------------------------------------------------------------

#[test]
fn neither_moved_is_a_persisted_noop_mapping() {
    let fixture = Colocated::new("neither");
    let main_tip = fixture.tip("refs/heads/main").expect("main tip");

    // The baseline reconcile in the fixture created the mapping; a second
    // reconcile must be a verified no-op that moves nothing.
    fixture.atomic(&["git", "bridge", "reconcile"]);
    assert_eq!(fixture.tip("refs/heads/main"), Some(main_tip));

    // The mapping is durable bookkeeping in the shared pristine: a fresh
    // process (`git bridge status`) reports it as Synchronized.
    let status = fixture.atomic(&["git", "bridge", "status"]);
    assert!(status.contains("main"), "{status}");
    assert!(status.contains("synchronized"), "{status}");
    assert!(status.contains("refs/heads/main"), "{status}");
    assert!(
        status.contains(&main_tip.to_string()),
        "status must show the observed tip {main_tip}: {status}"
    );
}

#[test]
fn git_only_movement_imports_and_refreshes_the_mapping() {
    let fixture = Colocated::new("git-ahead");
    let baseline = fixture.tip("refs/heads/main").expect("main tip");

    // Git-only movement: a plain user commit on the mapped branch.
    let repository = GitRepository::open(fixture.root()).expect("git repo");
    let moved = commit_file(
        &repository,
        "tracked.txt",
        b"git moved\n",
        "git-side commit",
    );
    drop(repository);

    // Reconcile imports the Git movement.
    let output = fixture.atomic_output(&["git", "bridge", "reconcile"]);
    assert!(
        output.status.success(),
        "reconcile failed:\n{}",
        atomic_text(&output)
    );

    // The mapped ref did not move (import is Git→Atomic), and the mapping now
    // observes the imported tip as Synchronized.
    assert_eq!(fixture.tip("refs/heads/main"), Some(moved));
    let status = fixture.atomic(&["git", "bridge", "status"]);
    assert!(status.contains("synchronized"), "{status}");
    assert!(
        status.contains(&moved.to_string()),
        "mapping must observe the imported tip {moved}:\n{status}"
    );
    assert!(
        !status.contains("git-ahead"),
        "no Git-ahead residue after import:\n{status}"
    );
    let _ = baseline;
}

#[test]
fn atomic_only_movement_exports_under_the_expected_old_lease() {
    let fixture = Colocated::new("atomic-ahead");
    disable_shadow_sync(fixture.root());

    // Atomic-only movement: record without the shadow projection.
    fs::write(fixture.root().join("tracked.txt"), b"atomic moved\n").expect("write");
    fixture.atomic(&["record", "-m", "atomic-side change"]);

    let baseline = fixture.tip("refs/heads/main").expect("main tip");
    let output = fixture.atomic_output(&["git", "bridge", "reconcile"]);
    assert!(
        output.status.success(),
        "reconcile failed:\n{}",
        atomic_text(&output)
    );
    assert!(
        atomic_text(&output).contains("Projected the current Atomic view to Git HEAD"),
        "{}",
        atomic_text(&output)
    );

    // The export moved the mapped branch under its journaled lease.
    let exported = fixture.tip("refs/heads/main").expect("exported tip");
    assert_ne!(exported, baseline);
    {
        let repository = GitRepository::open(fixture.root()).expect("git repo");
        let commit = repository.find_commit(exported).expect("export commit");
        assert!(
            commit
                .message()
                .expect("message")
                .contains("Atomic bridge projection"),
            "export commit must be an Atomic projection: {:?}",
            commit.message()
        );
    }

    // The mapping records the exported tip and reports Synchronized.
    let status = fixture.atomic(&["git", "bridge", "status"]);
    assert!(status.contains("synchronized"), "{status}");
    assert!(
        status.contains(&exported.to_string()),
        "mapping must record the exported tip:\n{status}"
    );
}

#[test]
fn incompatible_both_moved_persists_diverged_and_moves_neither_side() {
    let fixture = Colocated::new("diverged");
    disable_shadow_sync(fixture.root());

    // Atomic moves first (durable record, shadow projection disabled), then
    // Git moves with a plain commit that is never imported: no bound closure
    // evidence exists on either side.
    fs::write(fixture.root().join("atomic-only.txt"), b"atomic diverged\n").expect("write");
    fixture.atomic(&["add", "atomic-only.txt"]);
    fixture.atomic(&["record", "-m", "divergent atomic change"]);
    let repository = GitRepository::open(fixture.root()).expect("git repo");
    let git_moved = commit_file(
        &repository,
        "tracked.txt",
        b"git diverged\n",
        "divergent git commit",
    );
    drop(repository);

    let atomic_state_before = view_merkle(&fixture);

    // Reconcile must refuse: both sides moved beyond the last mutually
    // observed mapping and no closure proof selects a side.
    let failure = fixture.atomic_fails(&["git", "bridge", "reconcile"]);
    assert!(
        failure.contains("both advanced beyond the last mutually observed mapping"),
        "{failure}"
    );
    assert!(failure.contains("atomic view insert"), "{failure}");

    // Neither side moved.
    assert_eq!(fixture.tip("refs/heads/main"), Some(git_moved));
    assert_eq!(view_merkle(&fixture), atomic_state_before);

    // The mapping persisted the Diverged status with explicit remediation.
    let status = fixture.atomic(&["git", "bridge", "status"]);
    assert!(status.contains("diverged"), "{status}");
    assert!(
        status.contains("atomic view insert"),
        "remediation must name insert/merge:\n{status}"
    );

    // `atomic status --no-reconcile` surfaces the divergence with
    // remediation (the reconciling path refuses on the stale Git baseline —
    // correct behavior: reconciliation itself is the remediation).
    let status_output = fixture.atomic(&["status", "--no-reconcile"]);
    assert!(
        status_output.contains("Diverged"),
        "atomic status must surface the diverged mapping:\n{status_output}"
    );
}

#[test]
fn equal_trees_with_different_closures_still_reconcile_as_diverged() {
    // Identical tree content but independent histories: containment cannot
    // be proven from bytes, so neither side may move.
    let fixture = Colocated::new("equal-trees");
    disable_shadow_sync(fixture.root());

    fs::write(fixture.root().join("tracked.txt"), b"atomic moved\n").expect("write");
    fixture.atomic(&["record", "-m", "atomic reaches same tree"]);
    let repository = GitRepository::open(fixture.root()).expect("git repo");
    let git_moved = commit_file(
        &repository,
        "tracked.txt",
        b"atomic moved\n",
        "git reaches same tree",
    );
    drop(repository);

    let failure = fixture.atomic_fails(&["git", "bridge", "reconcile"]);
    assert!(
        failure.contains("both advanced beyond the last mutually observed mapping"),
        "equal trees must not prove identity: {failure}"
    );
    assert_eq!(fixture.tip("refs/heads/main"), Some(git_moved));
}

// ---------------------------------------------------------------------------
// Persistence and lifecycle
// ---------------------------------------------------------------------------

#[test]
fn mapping_persists_across_reopen_and_processes() {
    let fixture = Colocated::new("reopen");
    // Every atomic invocation is a fresh process with a fresh repository
    // handle: the mapping written by the fixture's reconcile is still there.
    for _ in 0..3 {
        let status = fixture.atomic(&["git", "bridge", "status"]);
        assert!(status.contains("synchronized"), "{status}");
        assert!(status.contains("refs/heads/main"), "{status}");
    }
}

#[test]
fn draft_publication_is_explicit_and_routes_through_the_mapping() {
    let fixture = Colocated::new("publish");

    // Create a draft and record on it (projection lands on the private ref).
    fixture.atomic(&["view", "create", "feature-auth", "--draft"]);
    fixture.atomic(&["view", "switch", "feature-auth"]);
    fs::write(fixture.root().join("feature.txt"), b"draft work\n").expect("write");
    fixture.atomic(&["add", "feature.txt"]);
    fixture.atomic(&["record", "-m", "draft change"]);

    let draft_ref = fixture
        .tip("refs/atomic/views/feature-auth")
        .expect("draft ref");
    assert!(
        fixture.tip("refs/heads/feature-auth").is_none(),
        "drafts never publish implicitly"
    );

    // Explicit publication creates the branch at the projection commit.
    fixture.atomic(&[
        "git",
        "bridge",
        "publish",
        "feature-auth",
        "--branch",
        "published",
    ]);
    assert_eq!(fixture.tip("refs/heads/published"), Some(draft_ref));
    assert!(
        fixture.tip("refs/atomic/views/feature-auth").is_some(),
        "the draft ref is retained"
    );

    // The mapping records the branch-backed ref as Synchronized.
    let status = fixture.atomic(&["git", "bridge", "status"]);
    assert!(status.contains("refs/heads/published"), "{status}");
    assert!(
        status.contains(&draft_ref.to_string()),
        "mapping observes the published tip:\n{status}"
    );

    // Further draft work projects through the published mapping: the branch
    // tracks it (the mapping is the authoritative projection target).
    fs::write(fixture.root().join("feature.txt"), b"more draft work\n").expect("write");
    fixture.atomic(&["add", "feature.txt"]);
    fixture.atomic(&["record", "-m", "second draft change"]);
    let advanced = fixture.tip("refs/heads/published").expect("published tip");
    assert_ne!(
        advanced, draft_ref,
        "published branch must track draft work"
    );
    let status = fixture.atomic(&["git", "bridge", "status"]);
    assert!(status.contains("synchronized"), "{status}");
    assert!(
        status.contains(&advanced.to_string()),
        "mapping must observe the advanced branch:\n{status}"
    );
}

#[test]
fn draft_delete_removes_its_mapping_row() {
    let fixture = Colocated::new("delete-draft");
    fixture.atomic(&["view", "create", "to-delete", "--draft"]);
    fixture.atomic(&["view", "switch", "to-delete"]);
    // The current-view reconcile materializes the draft's baseline mapping
    // without recording (no snapshot child views).
    fixture.atomic(&["git", "bridge", "reconcile"]);
    let status = fixture.atomic(&["git", "bridge", "status"]);
    assert!(status.contains("to-delete"), "{status}");

    fixture.atomic(&["view", "switch", "main"]);
    fixture.atomic(&["view", "delete", "to-delete", "--force"]);

    let status = fixture.atomic(&["git", "bridge", "status"]);
    assert!(
        !status.contains("to-delete"),
        "deleted draft mapping must be removed:\n{status}"
    );
}

#[test]
fn status_and_observe_never_materialize_or_move_refs() {
    let fixture = Colocated::new("observe-only");
    disable_shadow_sync(fixture.root());

    // Atomic-only movement leaves the mapping GitAhead; observation-only
    // commands must not export it.
    fs::write(fixture.root().join("tracked.txt"), b"unpublished\n").expect("write");
    fixture.atomic(&["record", "-m", "not exported yet"]);
    let before = fixture.tip("refs/heads/main");

    let _ = fixture.atomic(&["git", "bridge", "status"]);
    let _ = fixture.atomic(&["status", "--no-reconcile"]);
    // `git bridge verify` reports the divergence (fails) but moves nothing.
    let _ = fixture.atomic_output(&["git", "bridge", "verify"]);

    assert_eq!(
        fixture.tip("refs/heads/main"),
        before,
        "observe/status-only reconciliation must never move a ref"
    );

    // The status reports the pending export explicitly.
    let status = fixture.atomic(&["git", "bridge", "status"]);
    assert!(status.contains("atomic-ahead"), "{status}");
    assert!(
        status.contains("atomic git bridge reconcile"),
        "status must give actionable remediation:\n{status}"
    );
}

#[test]
fn journaled_mapping_writes_are_leased_and_idempotent() {
    // Mapping writes go through the operation journal: a real transition
    // returns an operation id, a no-transition write is an idempotent None,
    // and an out-of-band concurrent move is adopted by the next journaled
    // write under a fresh lease (never silently reverted).
    let fixture = Colocated::new("lease");
    let root = fixture.root();

    let repo = atomic_repository::Repository::open(root).expect("open repository");
    let working_copy = repo.require_working_copy_id().expect("working copy");
    let mut first = repo
        .get_ref_mapping("main")
        .expect("read mapping")
        .expect("baseline mapping");
    first.last_observed_local = Some("1234567890abcdef1234567890abcdef12345678".to_string());
    let operation = repo
        .set_ref_mapping(working_copy, "main", Some(first.clone()))
        .expect("first mapping write");
    assert!(
        operation.is_some(),
        "a real mapping transition must journal an operation"
    );

    // An identical write is an idempotent no-op: no transition to journal.
    let idempotent = repo
        .set_ref_mapping(working_copy, "main", Some(first.clone()))
        .expect("idempotent write");
    assert!(idempotent.is_none());

    // An out-of-band writer moves the row (bypassing the journal).
    {
        use atomic_core::pristine::{MutTxnT, RefMappingMutTxnT, ViewTxnT};
        let mut txn = repo.pristine().write_txn().expect("write txn");
        let view = txn.get_view("main").expect("view").expect("main");
        let mut moved = first.clone();
        moved.last_observed_local = Some("abcdef0123456789abcdef0123456789abcdef01".to_string());
        txn.put_ref_mapping_bytes(view.id, &moved.encode().expect("encode"))
            .expect("put moved row");
        txn.commit().expect("commit");
    }

    // The next journaled write observes the concurrent value as its lease
    // baseline and transitions from it — it never reverts the concurrent
    // move behind that writer's back.
    let mut third = first.clone();
    third.last_observed_local = Some("111122223333444455556666777788889999aaaa".to_string());
    repo.set_ref_mapping(working_copy, "main", Some(third))
        .expect("third write adopts the concurrent row");
    let stored = repo
        .get_ref_mapping("main")
        .expect("read mapping")
        .expect("mapping row");
    assert_eq!(
        stored.last_observed_local.as_deref(),
        Some("111122223333444455556666777788889999aaaa")
    );
}

#[test]
fn detached_draft_head_reconciles_through_the_mapping() {
    let fixture = Colocated::new("detached");
    fixture.atomic(&["view", "create", "feature-detached", "--draft"]);
    fixture.atomic(&["view", "switch", "feature-detached"]);
    fs::write(fixture.root().join("detached.txt"), b"detached work\n").expect("write");
    fixture.atomic(&["add", "detached.txt"]);
    fixture.atomic(&["record", "-m", "detached change"]);

    // CB-8A Draft policy: HEAD is detached at the projection commit.
    {
        let repository = GitRepository::open(fixture.root()).expect("git repo");
        let head = repository.head().expect("head");
        assert!(
            !head.is_branch(),
            "draft HEAD must be detached, found {:?}",
            head.kind()
        );
    }

    // The reconcile is a no-op for the mapped pair and stays synchronized.
    let output = fixture.atomic_output(&["git", "bridge", "reconcile"]);
    assert!(
        output.status.success(),
        "detached reconcile failed:\n{}",
        atomic_text(&output)
    );
    let status = fixture.atomic(&["git", "bridge", "status"]);
    assert!(status.contains("synchronized"), "{status}");
    assert!(
        status.contains("refs/atomic/views/feature-detached"),
        "drafts map to the private views ref:\n{status}"
    );
}

#[test]
fn the_mapping_lives_in_the_shared_pristine_not_working_copy_storage() {
    // Linked worktrees resolve their own working copies against one common
    // pristine (`.atomic/pristine.redb`). The mapping is keyed there by
    // durable view id — never in the per-working-copy working-copies storage
    // — so every worktree of the repository shares one mapping state.
    let fixture = Colocated::new("shared-store");
    let repo = atomic_repository::Repository::open(fixture.root()).expect("open repository");
    let working_copy = repo.require_working_copy_id().expect("working copy");
    let mut advanced = repo
        .get_ref_mapping("main")
        .expect("read")
        .expect("baseline mapping")
        .clone();
    advanced.last_observed_local = Some("fedcba9876543210fedcba9876543210fedcba98".to_string());
    repo.set_ref_mapping(working_copy, "main", Some(advanced))
        .expect("write through this working copy");
    drop(repo);

    // The same row is readable from the common pristine with a completely
    // fresh handle — the row never lived in working-copy-local state.
    let reopened = atomic_repository::Repository::open(fixture.root()).expect("reopen");
    let stored = reopened
        .get_ref_mapping("main")
        .expect("read")
        .expect("mapping row");
    assert_eq!(
        stored.last_observed_local.as_deref(),
        Some("fedcba9876543210fedcba9876543210fedcba98")
    );
    drop(reopened);
    let _ = fixture.atomic(&["git", "bridge", "status"]);
}

#[test]
fn both_moved_incompatible_after_import_and_record_persists_diverged() {
    // Real both-moved sequence: Git commits and reconcile imports (binding
    // the closure), then a shadow-disabled record moves Atomic without
    // exporting, then a plain Git commit moves Git again. Both sides are
    // beyond the last mutually observed mapping and the new Git commit has
    // no bound closure: the reconcile must persist Diverged and move
    // neither side.
    let fixture = Colocated::new("both-moved");
    let repository = GitRepository::open(fixture.root()).expect("git repo");
    let _imported = commit_file(
        &repository,
        "tracked.txt",
        b"imported work\n",
        "to be imported",
    );
    drop(repository);
    fixture.atomic(&["git", "bridge", "reconcile"]);

    // Atomic moves without projecting (shadow disabled AFTER the import so
    // the managed exclude line is not restored by the reconcile).
    disable_shadow_sync(fixture.root());
    fs::write(fixture.root().join("atomic-only.txt"), b"atomic diverged\n").expect("write");
    fixture.atomic(&["add", "atomic-only.txt"]);
    fixture.atomic(&["record", "-m", "divergent atomic change"]);

    // Git moves again with a commit that is never imported (no closure).
    let repository = GitRepository::open(fixture.root()).expect("git repo");
    let git_moved = commit_file(
        &repository,
        "tracked.txt",
        b"git diverged\n",
        "divergent git commit",
    );
    drop(repository);

    let failure = fixture.atomic_fails(&["git", "bridge", "reconcile"]);
    assert!(
        failure.contains("both advanced beyond the last mutually observed mapping"),
        "{failure}"
    );
    // Neither side moved.
    assert_eq!(fixture.tip("refs/heads/main"), Some(git_moved));
    let status = fixture.atomic(&["git", "bridge", "status"]);
    assert!(status.contains("diverged"), "{status}");
}

/// CB-13C F3: the reconcile divergence branch PERSISTS the divergence
/// status before its terminal event, so the honest outcome is `failed`
/// — `Refused` would promise before-any-mutation falsely. Pinned against
/// the real both-moved fixture and the real journal.
#[test]
fn diverged_reconcile_emits_failed_not_refused_after_persisting_status() {
    let fixture = Colocated::new("both-moved-f3");
    let repository = GitRepository::open(fixture.root()).expect("git repo");
    let _ = commit_file(
        &repository,
        "tracked.txt",
        b"imported work\n",
        "to be imported",
    );
    drop(repository);
    fixture.atomic(&["git", "bridge", "reconcile"]);

    disable_shadow_sync(fixture.root());
    fs::write(fixture.root().join("atomic-only.txt"), b"atomic diverged\n").expect("write");
    fixture.atomic(&["add", "atomic-only.txt"]);
    fixture.atomic(&["record", "-m", "divergent atomic change"]);

    let repository = GitRepository::open(fixture.root()).expect("git repo");
    let _ = commit_file(
        &repository,
        "tracked.txt",
        b"git diverged\n",
        "divergent git commit",
    );
    drop(repository);

    let failure = fixture.atomic_fails(&["git", "bridge", "reconcile"]);
    assert!(
        failure.contains("both advanced beyond the last mutually observed mapping"),
        "{failure}"
    );

    let journal = fs::read_to_string(fixture.root().join(".atomic/bridge/events.jsonl"))
        .expect("the journal exists");
    let last_reconcile = journal
        .lines()
        .rfind(|line| line.contains("\"event\":\"reconcile\""))
        .expect("a reconcile event was journaled");
    let value: serde_json::Value = serde_json::from_str(last_reconcile).unwrap();
    assert_eq!(
        value.get("outcome").and_then(|outcome| outcome.as_str()),
        Some("failed"),
        "the post-persistence terminal outcome must be failed, not refused: {value}"
    );
    assert_eq!(
        value.get("direction").and_then(|d| d.as_str()),
        Some("diverged"),
        "{value}"
    );
}

#[allow(dead_code)]
fn stale_bookkeeping_heals_but_an_unreachable_baseline_fails_closed() {
    // Two honest stale-row semantics (CB-10A review R2/R4):
    //
    // 1. A lost mapping refresh (the checkpoint is fresher than the mapping)
    //    must not invent a movement: reconcile is a verified no-op that
    //    refreshes the bookkeeping, never a spurious import/export.
    // 2. A mapping row whose observed baseline is unreachable garbage makes
    //    the added-commit walk unprovable: the reconcile must fail closed to
    //    Diverged instead of reading an empty walk as positive containment.
    let fixture = Colocated::new("stale-heal");

    // (1) Wind the mapping's Atomic observation behind both sides while the
    // observed ref tip stays real: the verified pair is aligned, so the
    // reconcile is a healing no-op.
    let repo = atomic_repository::Repository::open(fixture.root()).expect("open repository");
    let _working_copy = repo.require_working_copy_id().expect("working copy");
    let main_tip = fixture.tip("refs/heads/main").expect("main tip");
    {
        use atomic_core::pristine::{MutTxnT, RefMappingMutTxnT, ViewTxnT};
        let mut txn = repo.pristine().write_txn().expect("write txn");
        let view = txn.get_view("main").expect("view").expect("main");
        let mut stale = repo
            .get_ref_mapping("main")
            .expect("read")
            .expect("mapping")
            .clone();
        stale.last_observed_atomic =
            Some("BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB".to_string());
        stale.last_exported = None;
        stale.last_exported_state = None;
        txn.put_ref_mapping_bytes(view.id, &stale.encode().expect("encode"))
            .expect("stale put");
        txn.commit().expect("commit");
    }
    drop(repo);

    let output = fixture.atomic_output(&["git", "bridge", "reconcile"]);
    assert!(
        output.status.success(),
        "stale-mapping reconcile failed:\n{}",
        atomic_text(&output)
    );
    assert!(
        atomic_text(&output).contains("already matches"),
        "an aligned pair must reconcile as a no-op: {}",
        atomic_text(&output)
    );
    let status = fixture.atomic(&["git", "bridge", "status"]);
    assert!(status.contains("synchronized"), "{status}");

    // (2) An unreachable baseline (a row claiming an impossible observed tip)
    // with real movement on both sides makes the walk unprovable: reconcile
    // refuses and moves nothing instead of reading the empty walk as a
    // positive containment.
    disable_shadow_sync(fixture.root());
    fs::write(fixture.root().join("atomic-only.txt"), b"atomic moved\n").expect("write");
    fixture.atomic(&["add", "atomic-only.txt"]);
    fixture.atomic(&["record", "-m", "divergent atomic change"]);
    let atomic_state = view_merkle(&fixture);
    let repo = atomic_repository::Repository::open(fixture.root()).expect("open repository");
    {
        use atomic_core::pristine::{MutTxnT, RefMappingMutTxnT, ViewTxnT};
        let mut txn = repo.pristine().write_txn().expect("write txn");
        let view = txn.get_view("main").expect("view").expect("main");
        let mut garbage = repo
            .get_ref_mapping("main")
            .expect("read")
            .expect("mapping")
            .clone();
        garbage.last_observed_local = Some(git2::Oid::zero().to_string());
        txn.put_ref_mapping_bytes(view.id, &garbage.encode().expect("encode"))
            .expect("garbage put");
        txn.commit().expect("commit");
    }
    drop(repo);

    let failure = fixture.atomic_fails(&["git", "bridge", "reconcile"]);
    assert!(
        failure.contains("both advanced beyond the last mutually observed mapping"),
        "an unprovable baseline must fail closed: {failure}"
    );
    assert_eq!(
        fixture.tip("refs/heads/main"),
        Some(main_tip),
        "the refuse path must move neither side"
    );
    assert_eq!(view_merkle(&fixture), atomic_state, "atomic must not move");
}

// ---------------------------------------------------------------------------
// Review R1-R5 fixtures (ATOM::aaron::5): proofs, publication, HEAD
// independence, and refused lifecycle operations.
// ---------------------------------------------------------------------------

#[test]
fn forged_atomic_state_header_cannot_authorize_import() {
    // Review R1: a foreign Git commit whose message names the current Atomic
    // state must not select an import in the both-moved matrix. Message text
    // is not containment evidence; the reconcile refuses and moves nothing.
    let fixture = Colocated::new("forged-header");
    disable_shadow_sync(fixture.root());

    // Atomic moves (record without projection).
    fs::write(fixture.root().join("atomic.txt"), b"atomic side\n").expect("write");
    fixture.atomic(&["add", "atomic.txt"]);
    fixture.atomic(&["record", "-m", "atomic-side change"]);
    let atomic_state = view_merkle(&fixture);
    let baseline = fixture.tip("refs/heads/main").expect("main tip");

    // A foreign Git commit forges the projection header and stages both
    // files (so the import path itself would have succeeded had the forged
    // header been accepted).
    fs::write(fixture.root().join("tracked.txt"), b"git moved\n").expect("write");
    let repository = GitRepository::open(fixture.root()).expect("git repo");
    let mut index = repository.index().expect("git index");
    index
        .add_path(Path::new("tracked.txt"))
        .expect("stage tracked");
    index
        .add_path(Path::new("atomic.txt"))
        .expect("stage atomic");
    index.write().expect("write index");
    drop(index);
    drop(repository);
    fixture.git(&[
        "commit",
        "-qm",
        &format!("forged projection\n\nAtomic-State: {atomic_state}\n"),
    ]);
    let foreign = fixture.tip("refs/heads/main").expect("foreign tip");
    assert_ne!(foreign, baseline, "the foreign commit moved the ref");

    // The forged header must not authorize the import.
    let failure = fixture.atomic_fails(&["git", "bridge", "reconcile"]);
    assert!(
        failure.contains("both advanced beyond the last mutually observed mapping"),
        "a forged header must not select a side: {failure}"
    );
    assert_eq!(fixture.tip("refs/heads/main"), Some(foreign));
    assert_eq!(view_merkle(&fixture), atomic_state, "atomic must not move");

    // The mapping persists the refusal as Diverged with remediation.
    let status = fixture.atomic(&["git", "bridge", "status"]);
    assert!(status.contains("diverged"), "{status}");
}

#[test]
fn stale_export_binding_cannot_select_a_side() {
    // Review R1: a cached export taken at an older Atomic state is obsolete.
    // It must not prove "Git contains Atomic" for the newer state.
    let fixture = Colocated::new("stale-export");
    disable_shadow_sync(fixture.root());

    // Atomic moves and exports: the mapping binds the export to that state.
    fs::write(fixture.root().join("tracked.txt"), b"first\n").expect("write");
    fixture.atomic(&["record", "-m", "exported change"]);
    fixture.atomic(&["git", "bridge", "reconcile"]);
    let exported = fixture.tip("refs/heads/main").expect("exported tip");

    // Atomic moves again (never exported) and a foreign Git commit lands:
    // both moved, and the only cached export is now obsolete.
    fs::write(fixture.root().join("tracked.txt"), b"second\n").expect("write");
    fixture.atomic(&["record", "-m", "unexported change"]);
    let atomic_state = view_merkle(&fixture);
    let repository = GitRepository::open(fixture.root()).expect("git repo");
    let foreign = commit_file(&repository, "extra.txt", b"foreign\n", "foreign commit");
    drop(repository);

    let failure = fixture.atomic_fails(&["git", "bridge", "reconcile"]);
    assert!(
        failure.contains("both advanced beyond the last mutually observed mapping"),
        "an obsolete export must not select a side: {failure}"
    );
    assert_eq!(fixture.tip("refs/heads/main"), Some(foreign));
    assert_eq!(view_merkle(&fixture), atomic_state);
    let _ = exported;
}

#[test]
fn publish_refuses_an_occupied_target_branch() {
    // Review R3: publication is create-only. An occupied target branch is a
    // typed refusal — never a permissible update that could rewind history.
    let fixture = Colocated::new("publish-occupied");
    fixture.atomic(&["view", "create", "feature-occupied", "--draft"]);
    fixture.atomic(&["view", "switch", "feature-occupied"]);
    fs::write(fixture.root().join("feature.txt"), b"draft work\n").expect("write");
    fixture.atomic(&["add", "feature.txt"]);
    fixture.atomic(&["record", "-m", "draft change"]);
    let draft_ref = fixture
        .tip("refs/atomic/views/feature-occupied")
        .expect("draft ref");

    // The target branch already exists.
    fixture.git(&["branch", "occupied"]);
    let occupied_tip = fixture.tip("refs/heads/occupied").expect("occupied tip");

    let failure = fixture.atomic_fails(&[
        "git",
        "bridge",
        "publish",
        "feature-occupied",
        "--branch",
        "occupied",
    ]);
    assert!(
        failure.contains("already exists"),
        "the create-only refusal must name the occupied target: {failure}"
    );
    assert_eq!(
        fixture.tip("refs/heads/occupied"),
        Some(occupied_tip),
        "the occupied branch must be unmoved"
    );
    // The draft's mapping is untouched: it still maps the private ref.
    let status = fixture.atomic(&["git", "bridge", "status"]);
    assert!(
        status.contains("refs/atomic/views/feature-occupied"),
        "the draft mapping must stay on its private ref:\n{status}"
    );
    let _ = draft_ref;
}

#[test]
fn publish_refuses_during_a_git_sequence_operation_then_succeeds_clean() {
    // Review R3: the workspace preflight runs before any mutation. A Git-owned
    // sequence marker (MERGE_HEAD) refuses publication with the target
    // untouched; after the marker is cleared, publication proceeds through
    // the journaled reference-transaction CAS.
    let fixture = Colocated::new("publish-merge-head");
    fixture.atomic(&["view", "create", "feature-seq", "--draft"]);
    fixture.atomic(&["view", "switch", "feature-seq"]);
    fs::write(fixture.root().join("feature.txt"), b"draft work\n").expect("write");
    fixture.atomic(&["add", "feature.txt"]);
    fixture.atomic(&["record", "-m", "draft change"]);
    let draft_ref = fixture
        .tip("refs/atomic/views/feature-seq")
        .expect("draft ref");

    // A merge in progress (MERGE_HEAD present) refuses publication.
    fs::write(
        fixture.root().join(".git/MERGE_HEAD"),
        "0123456789abcdef0123456789abcdef01234567\n",
    )
    .expect("write MERGE_HEAD");
    let failure = fixture.atomic_fails(&[
        "git",
        "bridge",
        "publish",
        "feature-seq",
        "--branch",
        "published",
    ]);
    assert!(
        !failure.contains("Published draft view"),
        "publication must be refused while a sequence marker is present: {failure}"
    );
    assert!(
        fixture.tip("refs/heads/published").is_none(),
        "the refused publication must not create the branch"
    );
    fs::remove_file(fixture.root().join(".git/MERGE_HEAD")).expect("clear MERGE_HEAD");

    // With the marker cleared, publication proceeds create-only.
    fixture.atomic(&[
        "git",
        "bridge",
        "publish",
        "feature-seq",
        "--branch",
        "published",
    ]);
    assert_eq!(fixture.tip("refs/heads/published"), Some(draft_ref));
}

#[test]
fn detached_mapped_ref_movement_is_not_falsely_synchronized() {
    // Review R4: a published Draft whose mapped branch moves independently of
    // HEAD (a distinct empty commit: equal tree, different closure) must not
    // be reconciled through the HEAD-based path. The reconcile refuses with a
    // typed refusal; the mapping's observation and export binding are never
    // re-pointed at the foreign commit.
    let fixture = Colocated::new("detached-mapped-move");
    fixture.atomic(&["view", "create", "topic", "--draft"]);
    fixture.atomic(&["view", "switch", "topic"]);
    fs::write(fixture.root().join("topic.txt"), b"topic work\n").expect("write");
    fixture.atomic(&["add", "topic.txt"]);
    fixture.atomic(&["record", "-m", "topic change"]);
    let projection = fixture
        .tip("refs/atomic/views/topic")
        .expect("draft projection");
    fixture.atomic(&["git", "bridge", "publish", "topic", "--branch", "published"]);
    assert_eq!(fixture.tip("refs/heads/published"), Some(projection));

    // An unbound empty commit advances the mapped branch: equal tree,
    // distinct causal closure.
    let tree = fixture.git(&["rev-parse", "HEAD^{tree}"]);
    let foreign = fixture.git(&[
        "commit-tree",
        &tree,
        "-p",
        &projection.to_string(),
        "-m",
        "foreign empty commit",
    ]);
    fixture.git(&["update-ref", "refs/heads/published", &foreign]);
    assert_ne!(fixture.tip("refs/heads/published"), Some(projection));

    // Status reports the Git-side movement; it never synchronizes by itself.
    let status = fixture.atomic(&["git", "bridge", "status"]);
    assert!(status.contains("git-ahead"), "{status}");

    // The reconcile refuses: the movement is independent of Git HEAD.
    let head_before = fixture.git(&["rev-parse", "HEAD"]);
    let atomic_state = view_merkle(&fixture);
    let failure = fixture.atomic_fails(&["git", "bridge", "reconcile"]);
    assert!(
        failure.contains("independently of Git HEAD"),
        "the mapped-ref movement must be refused, not imported: {failure}"
    );
    assert_eq!(
        fixture.git(&["rev-parse", "HEAD"]),
        head_before,
        "HEAD must stay untouched"
    );
    assert_eq!(view_merkle(&fixture), atomic_state, "atomic must not move");

    // The stored mapping was never re-pointed at the foreign commit: the
    // observation and the export binding still name the published tip.
    let status = fixture.atomic(&["git", "bridge", "status"]);
    assert!(status.contains("git-ahead"), "{status}");
    assert!(
        status.contains(&projection.to_string()),
        "the observation must still be the published tip:\n{status}"
    );
    assert!(
        !status.contains(&foreign),
        "the foreign commit must not be adopted as synchronized/exported:\n{status}"
    );
}

#[test]
fn refused_shared_view_delete_keeps_its_mapping() {
    // Review R5: deleting a Shared view is refused (Shared views are
    // permanent); the refusal must leave main's mapping exactly as it was —
    // branch attached, Synchronized — instead of tombstoning it.
    let fixture = Colocated::new("shared-delete");
    fixture.atomic(&["view", "create", "topic", "--draft"]);
    fixture.atomic(&["view", "switch", "topic"]);

    let failure = fixture.atomic_fails(&["view", "delete", "main", "--force"]);
    assert!(
        failure.contains("cannot delete shared view"),
        "the shared-permanence refusal must survive: {failure}"
    );

    let status = fixture.atomic(&["git", "bridge", "status"]);
    assert!(status.contains("main"), "{status}");
    assert!(
        status.contains("synchronized"),
        "the refused delete must not tombstone the mapping:\n{status}"
    );
    assert!(
        status.contains("refs/heads/main"),
        "the refused delete must keep the mapped branch:\n{status}"
    );
}

// ============================================================================
// CB-10A follow-up (::17) regressions — failing-before/passing-after for the
// review findings R1, R3, R4, R6, plus the R7 codec hardening units in core
// and the R2 containment units in atomic-repository.
// ============================================================================

/// R1: the stale-source publication probe. An external writer moves the
/// draft's PRIVATE ref (`refs/atomic/views/<view>`) — which the verified
/// workspace invariant does not cover — and the publish must refuse instead
/// of exporting the foreign commit and claiming `Synchronized` for it.
#[test]
fn stale_draft_ref_source_is_refused_by_publish() {
    let fixture = Colocated::new("stale-source");
    fixture.atomic(&["view", "create", "topic", "--draft"]);
    fixture.atomic(&["view", "switch", "topic"]);
    fs::write(fixture.root().join("topic.txt"), b"draft work\n").expect("write");
    fixture.atomic(&["add", "topic.txt"]);
    fixture.atomic(&["record", "-m", "draft change"]);
    // Establish the verified checkpoint for the draft workspace.
    fixture.atomic(&["git", "bridge", "reconcile"]);

    let draft_tip = fixture.tip("refs/atomic/views/topic").expect("draft ref");
    let main_tip = fixture.tip("refs/heads/main").expect("main ref");
    assert_ne!(draft_tip, main_tip);

    // The foreign move: the private ref is re-pointed at main's commit.
    {
        let git = GitRepository::open(fixture.root()).expect("git");
        git.reference(
            "refs/atomic/views/topic",
            main_tip,
            true,
            "foreign move of the private ref",
        )
        .expect("update-ref");
    }

    // Publication refuses: the candidate commit is not the verified
    // projection commit, and NOTHING is written.
    let output = fixture.atomic_output(&[
        "git",
        "bridge",
        "publish",
        "topic",
        "--branch",
        "stale-published",
    ]);
    assert!(
        !output.status.success(),
        "the stale-source publication must refuse"
    );
    let text = atomic_text(&output);
    assert!(
        text.contains("not the verified projection commit"),
        "the refusal must name the verification, got:\n{text}"
    );
    assert!(
        fixture.tip("refs/heads/stale-published").is_none(),
        "no branch may be created from the stale source"
    );

    // Healing the source restores publication.
    {
        let git = GitRepository::open(fixture.root()).expect("git");
        git.reference(
            "refs/atomic/views/topic",
            draft_tip,
            true,
            "restore the verified projection",
        )
        .expect("update-ref back");
    }
    fixture.atomic(&["git", "bridge", "publish", "topic", "--branch", "published"]);
    assert_eq!(fixture.tip("refs/heads/published"), Some(draft_tip));
}

/// R6: the A-observes-absent / B-writes / A-stores-stale probe. The baseline
/// creation is an atomic expected-absence lease: a caller that observed
/// absence refuses instead of storing a stale baseline over a row another
/// working copy wrote first.
#[test]
fn expected_absent_baseline_creation_refuses_after_a_concurrent_write() {
    let fixture = Colocated::new("expected-absent");
    fixture.atomic(&["view", "create", "secondary"]);
    let repo = atomic_repository::Repository::open(fixture.root()).expect("open");
    let working_copy = repo.require_working_copy_id().expect("working copy");

    // A observes absence on a view that has no mapping row yet.
    let observed_by_a = repo.get_ref_mapping("secondary").expect("read");
    assert!(
        observed_by_a.is_none(),
        "the fresh view has no mapping row yet"
    );

    // B writes the row (the concurrent winner).
    let baseline = {
        let info = repo.get_view_info("secondary").expect("view info");
        let view_id = {
            use atomic_core::pristine::ViewTxnT;
            let txn = repo.pristine().read_txn().unwrap();
            txn.get_view("secondary").unwrap().unwrap().id
        };
        atomic_core::pristine::RefMapping {
            version: atomic_core::pristine::REF_MAPPING_VERSION,
            view_id,
            view_name: "secondary".to_string(),
            scope: info.scope as u8,
            local_ref: Some("refs/heads/secondary".to_string()),
            remote: None,
            last_observed_local: Some("1111111111111111111111111111111111111111".to_string()),
            last_observed_remote: None,
            last_exported: Some("1111111111111111111111111111111111111111".to_string()),
            last_exported_state: None,
            last_observed_atomic: Some({
                use atomic_core::types::Base32;
                info.state.to_base32()
            }),
            status: atomic_core::pristine::RefSyncStatus::Synchronized,
        }
    };
    repo.set_ref_mapping(working_copy, "secondary", Some(baseline))
        .expect("B's baseline write");

    // A's stale create refuses: the row it observed as absent now exists.
    let stale = {
        let info = repo.get_view_info("secondary").expect("view info");
        let view_id = {
            use atomic_core::pristine::ViewTxnT;
            let txn = repo.pristine().read_txn().unwrap();
            txn.get_view("secondary").unwrap().unwrap().id
        };
        atomic_core::pristine::RefMapping {
            version: atomic_core::pristine::REF_MAPPING_VERSION,
            view_id,
            view_name: "secondary".to_string(),
            scope: info.scope as u8,
            local_ref: Some("refs/heads/secondary".to_string()),
            remote: None,
            last_observed_local: None,
            last_observed_remote: None,
            last_exported: None,
            last_exported_state: None,
            last_observed_atomic: Some({
                use atomic_core::types::Base32;
                info.state.to_base32()
            }),
            status: atomic_core::pristine::RefSyncStatus::Synchronized,
        }
    };
    let result = repo.create_ref_mapping_expected_absent(working_copy, "secondary", stale);
    assert!(
        result.is_err(),
        "the expected-absence creation must refuse after a concurrent write"
    );
    // B's row survives untouched.
    let stored = repo
        .get_ref_mapping("secondary")
        .expect("read")
        .expect("B's row");
    assert_eq!(
        stored.last_observed_local.as_deref(),
        Some("1111111111111111111111111111111111111111"),
        "the concurrent row must not be clobbered by the stale creation"
    );
}

/// R3 crash window A: the mapping intent is DURABLE before the ref write.
/// A process that dies between journaling the intent and writing the branch
/// leaves NO branch, the pre-intent mapping row, and the pending operation
/// recoverable — the next writable open completes it without the branch
/// ever having existed.
#[test]
fn crash_before_the_ref_write_leaves_no_branch_and_a_recoverable_intent() {
    use atomic_core::operation::{
        GitHashAlgorithm, GitObjectId, GitRefTarget, MetadataTarget, MetadataTransition,
        MetadataValue,
    };

    let fixture = Colocated::new("crash-a");
    let tip: Oid = fixture.tip("refs/heads/main").expect("main tip");

    {
        let repo = atomic_repository::Repository::open(fixture.root()).expect("open");
        let working_copy = repo.require_working_copy_id().expect("working copy");
        let baseline = repo
            .get_ref_mapping("main")
            .expect("read")
            .expect("baseline row");
        // The intent moves the row FORWARD (recording the remote tracking
        // pair): identical old/new would be refused as a no-op transition by
        // the journal. The expected-old is the row EXACTLY as observed.
        let mut intended = baseline.clone();
        intended.remote = Some(("origin".to_string(), "refs/heads/main".to_string()));
        let intent = MetadataTransition {
            target: MetadataTarget::RefMapping {
                view: "main".to_string(),
            },
            expected_old: MetadataValue::Bytes(baseline.encode().expect("encode observed")),
            expected_new: MetadataValue::Bytes(intended.encode().expect("encode intent")),
        };
        let _prepared = repo
            .prepare_bridge_git_ref_write_with_metadata(
                working_copy,
                "refs/heads/never-created",
                None,
                GitRefTarget::Direct(
                    GitObjectId::new(GitHashAlgorithm::Sha1, tip.as_bytes().to_vec()).expect("oid"),
                ),
                atomic_core::Hash::of(b"crash-window-a"),
                vec![intent],
            )
            .expect("prepare the durable intent");
        // The "process" dies here: the prepared guard drops, the operation
        // stays an incomplete head, and the ref was never written.
    }

    // Reopen: the writable open recovers the incomplete head.
    let repo = atomic_repository::Repository::open(fixture.root()).expect("reopen recovers");
    let git = GitRepository::open(fixture.root()).expect("git");
    assert!(
        git.find_reference("refs/heads/never-created").is_err(),
        "no branch may exist when the crash happened before the ref write"
    );
    let mapping = repo
        .get_ref_mapping("main")
        .expect("read")
        .expect("mapping survives");
    assert_eq!(
        mapping.last_exported.as_deref(),
        Some(tip.to_string().as_str()),
        "the pre-intent binding is unchanged: the metadata transition is applied \
         only after the ref receipt, which never happened"
    );
}

/// R4: a PROVEN both-moved mapping reconciles through the mapped pair — the
/// export runs under its expected-old lease instead of the both-moved
/// refusal, because the closure containment proofs resolved the conflict.
///
/// Shape: the imported commit C2's closure is bound (its interpretation was
/// imported), the mapping row is regressed to the pre-import baseline on
/// BOTH sides (crashed bookkeeping), so the mapping sees git-moved (C2) and
/// atomic-moved (the current state is beyond the stale observation) with a
/// positive `atomic_contains_git` proof — classified both-moved Export.
#[test]
fn both_moved_proven_export_reconciles_through_the_mapped_pair() {
    let fixture = Colocated::new("both-moved-export");

    // 1. A Git-side commit C2, imported so its closure is bound and the view
    //    state advances to include it. The import also refreshes the mapping
    //    to Synchronized at C2.
    fs::write(fixture.root().join("imported.txt"), b"imported work\n").expect("write");
    fixture.git(&["add", "-A"]);
    fixture.git(&["commit", "-qm", "imported commit C2"]);
    fixture.atomic(&["git", "import", "--no-vault"]);
    let c2_tip: Oid = fixture.tip("refs/heads/main").expect("main tip");

    // 2. Simulate the crashed bookkeeping: the mapping row observes the
    //    pre-import baseline on BOTH sides (C2's parent / a pre-import
    //    state), while the verified view now contains C2's closure.
    let base_tip: Oid = {
        let git = GitRepository::open(fixture.root()).expect("git");
        let head = git
            .find_reference("refs/heads/main")
            .expect("main")
            .peel_to_commit()
            .expect("commit");
        let parent = head.parent(0).expect("parent").id();
        parent
    };
    assert_ne!(c2_tip, base_tip);
    {
        let repo = atomic_repository::Repository::open(fixture.root()).expect("open");
        let working_copy = repo.require_working_copy_id().expect("working copy");
        let mut mapping = repo.get_ref_mapping("main").expect("read").expect("row");
        mapping.last_observed_local = Some(base_tip.to_string());
        mapping.last_observed_atomic =
            Some("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_string());
        mapping.last_exported = None;
        mapping.last_exported_state = None;
        repo.set_ref_mapping(working_copy, "main", Some(mapping))
            .expect("stale row write");
    }

    // 3. The reconcile must NOT refuse with Diverged: the proofs resolve the
    //    both-moved case to Export, and the export runs (a no-op projection:
    //    the branch already sits at the view's verified projection).
    let output = fixture.atomic_output(&["git", "bridge", "reconcile"]);
    assert!(
        output.status.success(),
        "a proven both-moved Export must reconcile through the mapped pair:\n{}",
        atomic_text(&output)
    );

    // 4. The workspace verifies and the mapping is Synchronized again.
    let status = fixture.atomic(&["git", "bridge", "verify"]);
    assert!(status.contains("✓"), "the workspace must verify:\n{status}");
}

/// R4: a renamed mapped ref is never silently re-bound — the reconcile
/// refuses with the explicit independent-move remediation and moves nothing.
#[test]
fn renamed_mapped_ref_refuses_with_explicit_remediation() {
    let fixture = Colocated::new("renamed-ref");
    let main_tip = fixture.tip("refs/heads/main").expect("main tip");

    // The foreign rename: refs/heads/main vanishes.
    fixture.git(&["branch", "-m", "main", "elsewhere"]);
    assert!(fixture.tip("refs/heads/main").is_none());

    let output = fixture.atomic_output(&["git", "bridge", "reconcile"]);
    assert!(
        !output.status.success(),
        "the renamed mapped ref must refuse (no silent re-binding)"
    );
    let text = atomic_text(&output);
    assert!(
        text.contains("independently of Git HEAD"),
        "the refusal must name the independent mapped-ref move, got:\n{text}"
    );
    assert_eq!(
        fixture.tip("refs/heads/elsewhere"),
        Some(main_tip),
        "the renamed ref is untouched"
    );
}

/// R7 evidence matrix: a real linked Git worktree shares the repository-global
/// mapping through the common pristine, writes its own observation under the
/// same leases, and a stale cross-worktree write refuses (the observation
/// moved) instead of clobbering the newer row.
#[test]
fn linked_worktree_shares_the_mapping_and_stale_writes_refuse() {
    let fixture = Colocated::new("worktree-mapping");

    // Create a real linked worktree through Git itself.
    {
        let git = GitRepository::open(fixture.root()).expect("git");
        let commit = git
            .find_reference("refs/heads/main")
            .expect("main")
            .peel_to_commit()
            .expect("commit");
        let linked_path = fixture.root().parent().unwrap().join("linked-wt");
        let _ = std::fs::remove_dir_all(&linked_path);
        git.worktree("linked", &linked_path, None)
            .expect("add worktree");
        drop(commit);
    }
    let linked_root = fixture.root().parent().unwrap().join("linked-wt");
    struct CleanupWorktree(std::path::PathBuf);
    impl Drop for CleanupWorktree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = CleanupWorktree(linked_root.clone());
    assert!(
        linked_root.join(".git").exists(),
        "the linked worktree exists"
    );
    fs::remove_file(linked_root.join(".atomicignore")).ok();

    // The linked worktree's atomic directory points at the common pristine:
    // its mapping writes land in the SHARED row.
    {
        let repo = atomic_repository::Repository::open(&linked_root).expect("open the worktree");
        let working_copy = repo
            .require_working_copy_id()
            .expect("worktree working copy");
        let mapping = repo
            .get_ref_mapping("main")
            .expect("read the shared row from the worktree")
            .expect("the shared baseline row is visible from the linked worktree");
        let mut advanced = mapping.clone();
        advanced.last_observed_local = Some("1234123412341234123412341234123412341234".to_string());
        repo.set_ref_mapping(working_copy, "main", Some(advanced))
            .expect("write through the linked worktree");
    }

    // A second worktree handle observes the shared row...
    let repo = atomic_repository::Repository::open(fixture.root()).expect("reopen main");
    let stored = repo.get_ref_mapping("main").expect("read").expect("row");
    assert_eq!(
        stored.last_observed_local.as_deref(),
        Some("1234123412341234123412341234123412341234"),
        "the mapping is repository-global: the worktree's write is visible"
    );

    // ...and a stale write pinned to the PRE-worktree observation refuses.
    let mut stale = stored.clone();
    stale.last_observed_local = Some("abcdefabcdefabcdefabcdefabcdefabcdefabcd".to_string());
    let stale_original = {
        let mut row = stored.clone();
        row.last_observed_local = Some("fedcba9876543210fedcba9876543210fedcba98".to_string());
        row
    };
    let working_copy = repo.require_working_copy_id().expect("main working copy");
    let result = repo.set_ref_mapping_from_observation(
        working_copy,
        "main",
        Some(&stale_original),
        Some(stale),
    );
    assert!(
        result.is_err(),
        "the cross-worktree stale write must refuse (observation moved)"
    );
    let final_row = repo.get_ref_mapping("main").expect("read").expect("row");
    assert_eq!(
        final_row.last_observed_local.as_deref(),
        Some("1234123412341234123412341234123412341234"),
        "the worktree's newer row survives the refused stale write"
    );
}
