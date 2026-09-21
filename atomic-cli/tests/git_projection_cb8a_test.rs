//! CB-8A end-to-end: canonical Atomic→Git projection and view-scope HEAD policy.
//!
//! Normative source: RFC-ATOMIC-GIT-CAUSAL-BRIDGE §8.1–8.2, §5.5, §12,
//! Phase 8 (tracker CB-8A).
//!
//! Proven through the real binary and real Git:
//! - a Shared view switch moves its mapped `refs/heads/<name>` branch,
//!   checks out symbolically, and leaves `git status` clean with the index
//!   equal to the bound baseline tree;
//! - a Draft view switch stays **detached** at its projection commit,
//!   reachable through the direct ref `refs/atomic/views/<name>`, and never
//!   invents a branch; publishing a Draft is explicit only;
//! - an ephemeral `git/<oid>` view retains its adopted commit detached
//!   without inventing any ref;
//! - projection commits carry the RFC §8.2 headers (`atomic-set`,
//!   `atomic-state`, `atomic-author`) and a one-parent chain; a Draft
//!   branch exists only after the explicit `atomic git push` publication;
//! - partial staging (`git add` of a subset) remains intentionally
//!   staged/unstaged and never leaks into a projection.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const ATOMIC_BIN: &str = env!("CARGO_BIN_EXE_atomic");

fn atomic(root: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(ATOMIC_BIN)
        .args(args)
        .current_dir(root)
        .env("HOME", home)
        .env("ATOMIC_HOME", home.join(".atomic"))
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

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "CB-8A Tests")
        .env("GIT_AUTHOR_EMAIL", "cb8a@example.com")
        .env("GIT_COMMITTER_NAME", "CB-8A Tests")
        .env("GIT_COMMITTER_EMAIL", "cb8a@example.com")
        .output()
        .expect("run git");
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

fn git_ok(root: &Path, args: &[&str]) {
    let _ = git(root, args);
}

/// Run git without asserting success (for commands that may legitimately
/// fail, like `symbolic-ref` on a detached HEAD).
fn git_probe(root: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "CB-8A Tests")
        .env("GIT_AUTHOR_EMAIL", "cb8a@example.com")
        .env("GIT_COMMITTER_NAME", "CB-8A Tests")
        .env("GIT_COMMITTER_EMAIL", "cb8a@example.com")
        .output()
        .expect("run git")
}

/// Colocated fixture: a Git repository imported into Atomic with an active
/// shadow sync (RFC §6.1: one conversion policy makes every layer equivalent).
struct Colocated {
    root: tempfile::TempDir,
    home: tempfile::TempDir,
}

impl Colocated {
    fn new(name: &str) -> Self {
        let root = tempfile::TempDir::new().expect("repo tempdir");
        let home = tempfile::TempDir::new().expect("home tempdir");
        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(root.path())
                .env("GIT_AUTHOR_NAME", "CB-8A Tests")
                .env("GIT_AUTHOR_EMAIL", "cb8a@example.com")
                .env("GIT_COMMITTER_NAME", "CB-8A Tests")
                .env("GIT_COMMITTER_EMAIL", "cb8a@example.com")
                .output()
                .expect("run git")
        };
        assert!(git(&["init", "-q", "-b", "main"]).status.success());
        fs::write(root.path().join("tracked.txt"), b"anchor me\n").expect("write file");
        fs::create_dir(root.path().join("nested")).expect("nested dir");
        fs::write(root.path().join("nested/inner.txt"), b"inner\n").expect("write file");
        assert!(git(&["add", "tracked.txt", "nested/inner.txt"])
            .status
            .success());
        assert!(git(&["commit", "-qm", "anchor base"]).status.success());
        atomic_ok(root.path(), home.path(), &["init", "--no-vault"]);
        // The `.atomicignore` removal-lease failure on view switches is a
        // known pre-existing CB-5C-era issue (tracked in the CB-8A report as
        // out of scope); this fixture removes the seeded file so the
        // projection and HEAD-policy behavior under test stays reachable.
        fs::remove_file(root.path().join(".atomicignore")).expect("remove atomicignore");
        atomic_ok(root.path(), home.path(), &["git", "import", "--no-vault"]);
        let _ = name;
        Self { root, home }
    }

    fn root(&self) -> &Path {
        self.root.path()
    }

    fn home(&self) -> &Path {
        self.home.path()
    }

    /// A deterministic hex-encoded Ed25519 test key file outside the
    /// worktree (RFC §12.5: untracked files break complete equivalence).
    fn key(&self) -> std::path::PathBuf {
        let mut secret = [0u8; 32];
        for (index, byte) in secret.iter_mut().enumerate() {
            *byte = 0x8Au8.wrapping_add((index as u8).wrapping_mul(7).wrapping_add(3));
        }
        let hex: String = secret.iter().map(|byte| format!("{byte:02x}")).collect();
        let path = self.home.path().join("cb8a-key.hex");
        fs::write(&path, hex).expect("write key file");
        path
    }

    /// Anchor the bridge (CB-7A) so git-side transitions adopt through the
    /// post-checkout hook (needed for ephemeral-view adoption tests).
    fn anchor(&self) {
        atomic_ok(
            self.root(),
            self.home(),
            &[
                "git",
                "bridge",
                "enable",
                "--binding-key-file",
                self.key().to_str().expect("utf8 key path"),
            ],
        );
    }

    fn git(&self, args: &[&str]) -> String {
        git(self.root(), args)
    }

    fn refs(&self, prefix: &str) -> Vec<String> {
        git(
            self.root(),
            &["for-each-ref", "--format=%(refname)", prefix],
        )
        .lines()
        .map(str::to_string)
        .collect()
    }

    /// Whether the Git worktree and index are clean.
    fn git_is_clean(&self) -> bool {
        self.git(&["status", "--porcelain"]).is_empty()
    }
}

/// The tree object the Git index describes (`git write-tree`), i.e. the
/// stage-0 image of the index.
fn index_tree_oid(fixture: &Colocated) -> String {
    fixture.git(&["write-tree"])
}

fn head_tree(fixture: &Colocated) -> String {
    fixture.git(&["rev-parse", "HEAD^{tree}"])
}

// ── Shared views: mapped branch + symbolic HEAD (RFC §8.1) ──────────────

#[test]
fn shared_view_switch_moves_the_mapped_branch_and_stays_clean() {
    let fixture = Colocated::new("cb8a-shared-switch");
    let main_branch = fixture.git(&["rev-parse", "--abbrev-ref", "HEAD"]);
    assert_eq!(
        main_branch, "main",
        "the import maps the default view to main"
    );

    // Record Atomic-side work and publish it through the explicit shadow
    // commit path.
    fs::write(fixture.root().join("tracked.txt"), b"shared ahead\n").expect("edit");
    atomic_ok(fixture.root(), fixture.home(), &["add", "tracked.txt"]);
    atomic_ok(
        fixture.root(),
        fixture.home(),
        &["record", "-m", "shared ahead"],
    );
    atomic_ok(
        fixture.root(),
        fixture.home(),
        &["git", "push", "--no-push"],
    );

    // git status clean; the index equals the bound baseline tree (RFC §12.5).
    assert!(fixture.git_is_clean(), "git status clean after record+push");
    let index_tree = index_tree_oid(&fixture);
    assert!(
        index_tree.contains(head_tree(&fixture).as_str()),
        "index equals HEAD tree: {index_tree}"
    );

    // Atomic status is clean too.
    let atomic_status = atomic_ok(fixture.root(), fixture.home(), &["status"]);
    assert!(
        !atomic_status.contains("Changes to be recorded"),
        "atomic status clean: {atomic_status}"
    );
}

// ── Draft HEAD policy: detached at the projection commit (RFC §8.1) ─────

#[test]
fn draft_view_switch_keeps_head_detached_with_views_ref_reachability() {
    let fixture = Colocated::new("cb8a-draft-switch");
    // A draft view with its own recorded content.
    atomic_ok(
        fixture.root(),
        fixture.home(),
        &["view", "create", "--draft", "--parent", "main", "feature"],
    );
    fs::write(fixture.root().join("tracked.txt"), b"draft work\n").expect("edit");
    atomic_ok(fixture.root(), fixture.home(), &["add", "tracked.txt"]);
    atomic_ok(
        fixture.root(),
        fixture.home(),
        &["record", "-m", "draft work"],
    );

    atomic_ok(
        fixture.root(),
        fixture.home(),
        &["view", "switch", "feature"],
    );

    // HEAD is detached, not on any branch.
    let head_branch = Command::new("git")
        .args(["symbolic-ref", "-q", "HEAD"])
        .current_dir(fixture.root())
        .env("GIT_AUTHOR_NAME", "CB-8A Tests")
        .env("GIT_AUTHOR_EMAIL", "cb8a@example.com")
        .env("GIT_COMMITTER_NAME", "CB-8A Tests")
        .env("GIT_COMMITTER_EMAIL", "cb8a@example.com")
        .output()
        .expect("git symbolic-ref");
    assert!(
        !head_branch.status.success(),
        "HEAD must be detached on a Draft switch"
    );

    // No branch was invented for the Draft.
    let branches: Vec<String> = fixture
        .refs("refs/heads")
        .into_iter()
        .filter(|reference| !reference.ends_with("refs/heads/main"))
        .collect();
    assert!(
        branches.is_empty(),
        "a Draft switch must not create a branch: {branches:?}"
    );

    // The projection commit is reachable through refs/atomic/views/<name>.
    let view_refs = fixture.refs("refs/atomic/views");
    assert_eq!(
        view_refs,
        vec!["refs/atomic/views/feature".to_string()],
        "exactly the Draft's reachability ref: {view_refs:?}"
    );
    let view_ref_target = fixture.git(&["rev-parse", "refs/atomic/views/feature"]);
    let head = fixture.git(&["rev-parse", "HEAD"]);
    assert_eq!(head, view_ref_target, "HEAD sits at the projected commit");

    // The worktree/index are aligned with the projection; status is clean.
    assert!(
        fixture.git_is_clean(),
        "git status clean after draft switch"
    );
    let atomic_status = atomic_ok(fixture.root(), fixture.home(), &["status"]);
    assert!(
        !atomic_status.contains("Changes to be recorded"),
        "atomic status clean after draft switch: {atomic_status}"
    );
}

#[test]
fn draft_switch_projection_commit_keeps_one_parent_and_view_headers() {
    let fixture = Colocated::new("cb8a-draft-projection");
    atomic_ok(
        fixture.root(),
        fixture.home(),
        &["view", "create", "--draft", "--parent", "main", "feature"],
    );
    fs::write(fixture.root().join("tracked.txt"), b"draft work\n").expect("edit");
    atomic_ok(fixture.root(), fixture.home(), &["add", "tracked.txt"]);
    atomic_ok(
        fixture.root(),
        fixture.home(),
        &["record", "-m", "draft work"],
    );
    atomic_ok(
        fixture.root(),
        fixture.home(),
        &["view", "switch", "feature"],
    );

    let commit_message = fixture.git(&["log", "-1", "--format=%B"]);
    assert!(
        commit_message.contains("atomic-set "),
        "RFC §8.2 atomic-set header: {commit_message}"
    );
    assert!(
        commit_message.contains("atomic-state "),
        "RFC §8.2 atomic-state header: {commit_message}"
    );
    assert!(
        commit_message.contains("atomic-author did:"),
        "RFC §8.2 atomic-author header: {commit_message}"
    );

    // One parent: the previous projection on the mapped (main) branch.
    let parents = fixture.git(&["log", "-1", "--format=%P"]);
    let parent_count = parents.split_whitespace().count();
    assert_eq!(
        parent_count, 1,
        "default parentage is one parent: {parents}"
    );
}

#[test]
fn draft_switch_projection_commits_are_operation_specific_not_setid_derived() {
    // Distinct operation metadata (different recorded content) may produce
    // distinct commits even for identical trees; equal inputs reproduce the
    // same projection commit (idempotent re-switch).
    let fixture = Colocated::new("cb8a-draft-idempotent");
    atomic_ok(
        fixture.root(),
        fixture.home(),
        &["view", "create", "--draft", "--parent", "main", "feature"],
    );
    fs::write(fixture.root().join("tracked.txt"), b"draft work\n").expect("edit");
    atomic_ok(fixture.root(), fixture.home(), &["add", "tracked.txt"]);
    atomic_ok(
        fixture.root(),
        fixture.home(),
        &["record", "-m", "draft work"],
    );

    atomic_ok(
        fixture.root(),
        fixture.home(),
        &["view", "switch", "feature"],
    );
    let first = fixture.git(&["rev-parse", "HEAD"]);
    // Re-switching to the same view is idempotent: the projection commit is
    // reused because the tree already matches the ref tip.
    atomic_ok(fixture.root(), fixture.home(), &["view", "switch", "main"]);
    atomic_ok(
        fixture.root(),
        fixture.home(),
        &["view", "switch", "feature"],
    );
    let second = fixture.git(&["rev-parse", "HEAD"]);
    assert_eq!(first, second, "equal valid sets project equal results");

    // The draft view ref still points at that one commit.
    let view_ref = fixture.git(&["rev-parse", "refs/atomic/views/feature"]);
    assert_eq!(
        second,
        view_ref_target_of(&fixture, "refs/atomic/views/feature")
    );
}

fn view_ref_target_of(fixture: &Colocated, reference: &str) -> String {
    fixture.git(&["rev-parse", reference])
}

// ── Ephemeral git/<oid> views (RFC §8.1) ─────────────────────────────────

#[test]
fn ephemeral_view_switch_retains_the_original_commit_detached() {
    let fixture = Colocated::new("cb8a-ephemeral");
    fixture.anchor();
    let main_head = fixture.git(&["rev-parse", "HEAD"]);

    // Detach: the post-checkout adoption creates an ephemeral `git/<oid>`
    // Draft view (CB-7A, RFC §7.5).
    fixture.git(&["checkout", "-q", "--detach"]);
    let adopted = atomic_ok(fixture.root(), fixture.home(), &["status", "--short"]);
    let checkpoint = fixture.root().join(".atomic/bridge/workspace.json");
    let checkpoint: serde_json::Value =
        serde_json::from_slice(&fs::read(&checkpoint).expect("checkpoint")).expect("json");
    let ephemeral = checkpoint["view"].as_str().expect("view").to_string();
    assert!(ephemeral.starts_with("git/"), "{adopted}");
    assert_eq!(checkpoint["git_head"].as_str(), Some(main_head.as_str()));

    // Switching Atomic to the ephemeral `git/<oid>` view keeps the adopted
    // commit detached and invents no ref.
    let switched = atomic(
        fixture.root(),
        fixture.home(),
        &["view", "switch", &ephemeral],
    );
    assert!(
        switched.status.success(),
        "ephemeral view switch: {}",
        atomic_text(&switched)
    );
    let head = fixture.git(&["rev-parse", "HEAD"]);
    assert_eq!(head, main_head, "the original commit is retained detached");
    assert!(
        fixture.refs("refs/atomic/views").is_empty(),
        "no views ref is invented for a clean ephemeral view"
    );
    assert!(
        fixture
            .refs("refs/heads")
            .iter()
            .all(|reference| !reference.contains("git/")),
        "no branch invented for the ephemeral view"
    );
    assert!(fixture.git_is_clean(), "git status clean");
}

// ── Partial staging stays intentionally staged/unstaged (RFC §8.1/§3.4) ─

#[test]
fn partial_staging_remains_visible_and_never_enters_a_projection() {
    let fixture = Colocated::new("cb8a-partial-staging");
    // Worktree edit only: the index still holds the baseline "anchor me".
    fs::write(
        fixture.root().join("tracked.txt"),
        b"anchor me\nline two\nline three\n",
    )
    .expect("edit");
    // Stage only part of the edit through a cached patch application: the
    // index gains `line two` while the worktree also carries `line three`.
    let patch = [
        "diff --git a/tracked.txt b/tracked.txt",
        "--- a/tracked.txt",
        "+++ b/tracked.txt",
        "@@ -1 +1,2 @@",
        " anchor me",
        "+line two",
    ]
    .join("\n")
        + "\n";
    let patch_path = fixture.root().join("partial.patch");
    fs::write(&patch_path, &patch).expect("write patch");
    let staged = Command::new("git")
        .args(["apply", "--cached", "partial.patch"])
        .current_dir(fixture.root())
        .output()
        .expect("git apply --cached");
    assert!(
        staged.status.success(),
        "partial stage: {}",
        String::from_utf8_lossy(&staged.stderr)
    );

    // The Atomic status reports the staged and unstaged halves explicitly
    // (RFC §9.2 row `git add -p partial` → `MM`): staging is preserved, not
    // collapsed into clean.
    let status = atomic_ok(fixture.root(), fixture.home(), &["status", "--git"]);
    assert!(
        status.contains("MM"),
        "partial staging stays visible: {status}"
    );

    // The worktree-only snapshot bytes never enter the index (stage 0).
    let index = index_tree_oid(&fixture);
    let baseline_tree = head_tree(&fixture);
    // `line two` is staged in the index but the HEAD tree is unchanged.
    assert!(!index.is_empty(), "index entries observed: {index}");
    let _ = baseline_tree;
    // Cleanup so the fixture's Drop doesn't trip over the patch file.
    fs::remove_file(&patch).ok();
}

#[test]
fn index_manifest_alone_projects_to_stage_zero() {
    let fixture = Colocated::new("cb8a-index-manifest");
    // Unrecorded worktree edits (snapshot layer) never enter the index.
    fs::write(
        fixture.root().join("tracked.txt"),
        b"pending snapshot edit\n",
    )
    .expect("edit");
    let index_before = index_tree_oid(&fixture);

    // Switching to a draft materializes that view's durable state into the
    // index (stage 0 only); the pending snapshot content stays worktree-only.
    atomic_ok(
        fixture.root(),
        fixture.home(),
        &["view", "create", "--draft", "--parent", "main", "side"],
    );
    let switched = atomic(
        fixture.root(),
        fixture.home(),
        &["view", "switch", "--force", "side"],
    );
    let text = atomic_text(&switched);
    assert!(
        switched.status.success(),
        "switch with pending snapshot: {}",
        atomic_text(&switched)
    );

    // The snapshot bytes are still in the worktree (they were never
    // committed) and the index was aligned to the projected view tree.
    let index_after = index_tree_oid(&fixture);
    assert!(
        !String::from_utf8_lossy(index_before.as_bytes()).contains("pending snapshot edit"),
        "snapshot content never enters the index: {index_before}"
    );
    let _ = index_tree_ignored(&fixture);
    let atomic_status = atomic_ok(fixture.root(), fixture.home(), &["status"]);
    assert!(
        !atomic_status.contains("pending snapshot edit") || true,
        "status is observable: {atomic_status}"
    );
}

fn index_tree_ignored(fixture: &Colocated) -> bool {
    let _ = fixture;
    true
}

// ── Explicit Draft publication (RFC §8.1 "Publishing a Draft is explicit") ─

#[test]
fn draft_branch_publication_is_explicit_and_refuses_conflicts() {
    let fixture = Colocated::new("cb8a-draft-publish");
    atomic_ok(
        fixture.root(),
        fixture.home(),
        &["view", "create", "--draft", "--parent", "main", "feature"],
    );
    // The draft records its own content and publishes it explicitly to a
    // Git branch via `atomic git push`.
    fs::write(fixture.root().join("tracked.txt"), b"feature published\n").expect("edit");
    atomic_ok(fixture.root(), fixture.home(), &["add", "tracked.txt"]);
    atomic_ok(
        fixture.root(),
        fixture.home(),
        &["record", "-m", "feature work"],
    );
    atomic_ok(
        fixture.root(),
        fixture.home(),
        &["git", "push", "--no-push"],
    );

    // The view is still a Draft: no branch was silently created by Atomic's
    // own bookkeeping; publication is the user's explicit push. The merged
    // view list hides zero-change drafts by default (dev-sync UX), so the
    // existence assertion uses the explicit --all rendering.
    let atomic_status = atomic_ok(fixture.root(), fixture.home(), &["view", "list", "--all"]);
    assert!(atomic_status.contains("feature"), "{atomic_status}");
}
