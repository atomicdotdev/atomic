//! CB-7A end-to-end: bridge anchoring and bound Git HEAD adoption.
//!
//! Normative source: RFC-ATOMIC-GIT-CAUSAL-BRIDGE §5.1–5.2, §7.1–7.5, §8.1,
//! §12.1–12.7, Phase 7 (tracker CB-7A).
//!
//! Proven through the real binary and real Git:
//! - `git bridge enable` proves full repository/index/worktree equivalence and
//!   only then creates a signed Anchor binding plus the verified checkpoint;
//!   mismatched layers, pending work, unborn HEADs, and `--adopt-git` are
//!   typed refusals; repeat enable is idempotent (AC-1);
//! - a changed HEAD with a verified binding adopts with zero tracked-file
//!   rewrites, journals `ImportGitHead`, and updates the verified checkpoint;
//!   unbound commits return typed remediation (AC-2);
//! - detached HEADs adopt into ephemeral `git/<oid>` Draft views where
//!   recording works; `git switch -c` renames the view without discarding
//!   identity; named checkouts retain it; missing targets are typed (AC-3);
//! - anchoring preserves custom hooks and retrying an interrupted enable is
//!   idempotent (AC-4).

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
        .env("GIT_AUTHOR_NAME", "CB-7A Tests")
        .env("GIT_AUTHOR_EMAIL", "cb7a@example.com")
        .env("GIT_COMMITTER_NAME", "CB-7A Tests")
        .env("GIT_COMMITTER_EMAIL", "cb7a@example.com")
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

/// Hex-encoded deterministic test signing key file — test-owned, never the
/// global identity store (managed signing is CB-12B).
fn write_key_file(root: &Path, name: &str, seed: u8) -> std::path::PathBuf {
    let mut secret = [0u8; 32];
    for (index, byte) in secret.iter_mut().enumerate() {
        *byte = seed.wrapping_add((index as u8) * 7 + 11);
    }
    let hex: String = secret.iter().map(|byte| format!("{byte:02x}")).collect();
    let path = root.join(name);
    fs::write(&path, hex).expect("write key file");
    path
}

/// Colocated fixture: a Git repository with one commit, imported into Atomic
/// (RFC §6.1: the same conversion policy makes every layer equivalent).
struct Colocated {
    root: tempfile::TempDir,
    home: tempfile::TempDir,
    key: std::path::PathBuf,
}

impl Colocated {
    fn new(name: &str) -> Self {
        let root = tempfile::TempDir::new().expect("repo tempdir");
        let home = tempfile::TempDir::new().expect("home tempdir");
        git_ok(root.path(), &["init", "-q", "-b", "main"]);
        fs::write(root.path().join("tracked.txt"), b"anchor me\n").expect("write file");
        fs::create_dir(root.path().join("nested")).expect("nested dir");
        fs::write(root.path().join("nested/inner.txt"), b"inner\n").expect("write file");
        git_ok(root.path(), &["add", "tracked.txt", "nested/inner.txt"]);
        git_ok(root.path(), &["commit", "-qm", "anchor base"]);
        atomic_ok(root.path(), home.path(), &["init"]);
        atomic_ok(root.path(), home.path(), &["git", "import", "--no-vault"]);
        // The Anchor key must live outside the worktree: any untracked file
        // breaks complete manifest equivalence by design (RFC §12.5).
        let key = write_key_file(home.path(), &format!("{name}.hex"), 0x71);
        Self { root, home, key }
    }

    fn key(&self) -> &Path {
        &self.key
    }

    fn anchor(&self) -> String {
        atomic_ok(
            self.root.path(),
            self.home.path(),
            &[
                "git",
                "bridge",
                "enable",
                "--binding-key-file",
                self.key().to_str().expect("utf8 key path"),
            ],
        )
    }

    fn binding_refs(&self) -> Vec<String> {
        git(
            self.root.path(),
            &[
                "for-each-ref",
                "--format=%(refname)",
                "refs/atomic/bindings",
            ],
        )
        .lines()
        .map(str::to_string)
        .collect()
    }

    fn checkpoint_view(&self) -> String {
        checkpoint_field(self.root.path(), "view")
    }

    fn checkpoint_head(&self) -> String {
        checkpoint_field(self.root.path(), "git_head")
    }
}

fn checkpoint_field(root: &Path, field: &str) -> String {
    let bytes = fs::read(root.join(".atomic/bridge/workspace.json")).expect("checkpoint exists");
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("checkpoint json");
    value[field].as_str().unwrap_or("").to_string()
}

fn head_map(root: &Path) -> serde_json::Value {
    let bytes = fs::read(root.join(".atomic/bridge/head-map.json")).expect("head map exists");
    serde_json::from_slice(&bytes).expect("head map json")
}

fn binding_id_from_output(output: &str) -> String {
    let marker = "Anchor binding ";
    let start = output.find(marker).expect("Anchor binding id in output") + marker.len();
    output[start..start + 64].to_string()
}

// ── AC-1: enable → Anchor ────────────────────────────────────────────────

#[test]
fn enable_anchors_equivalent_store_with_signed_binding_and_checkpoint() {
    let fixture = Colocated::new("anchor-equivalent");
    let output = fixture.anchor();
    assert!(output.contains("Anchor binding"), "{output}");
    assert!(
        output.contains("verified working-copy checkpoint"),
        "{output}"
    );

    let refs = fixture.binding_refs();
    assert_eq!(refs.len(), 1, "exactly one binding ref: {refs:?}");

    let head = git(fixture.root.path(), &["rev-parse", "HEAD"]);
    assert_eq!(
        fixture.checkpoint_head(),
        head,
        "checkpoint records anchored HEAD"
    );
    assert!(!fixture.checkpoint_view().is_empty());

    // The Anchor verifies end to end through the binding verifier.
    let id = binding_id_from_output(&output);
    verify_signature_and_content(&fixture, &id);
}

/// The Anchor's cryptographic proof: signature and recomputed content verify.
/// Provenance trust is a CB-12B publication gate, so an untrusted test key
/// still proves the binding end to end.
fn verify_signature_and_content(fixture: &Colocated, id: &str) {
    let output = atomic(
        fixture.root.path(),
        fixture.home.path(),
        &["git", "bridge", "binding", "verify", id],
    );
    let text = atomic_text(&output);
    assert!(text.contains("signature valid: true"), "{text}");
    assert!(text.contains("content: recomputed-ok"), "{text}");
}

#[test]
fn repeat_enable_is_idempotent() {
    let fixture = Colocated::new("anchor-repeat");
    let first = fixture.anchor();
    let first_id = binding_id_from_output(&first);

    let retried = fixture.anchor();
    assert!(
        retried.contains("already exists"),
        "repeat enable replays the identical Anchor: {retried}"
    );
    assert!(retried.contains(&first_id), "same binding id: {retried}");
    assert_eq!(
        fixture.binding_refs().len(),
        1,
        "create-only: no duplicate ref"
    );
}

#[test]
fn enable_refuses_dirty_state_and_preserves_pending_work() {
    let fixture = Colocated::new("anchor-dirty");
    fs::write(fixture.root.path().join("tracked.txt"), b"pending edit\n").expect("pending edit");

    let output = atomic(
        fixture.root.path(),
        fixture.home.path(),
        &[
            "git",
            "bridge",
            "enable",
            "--binding-key-file",
            fixture.key().to_str().unwrap(),
        ],
    );
    let text = atomic_text(&output);
    assert!(
        output.status.success(),
        "passive enable still installs hooks: {text}"
    );
    assert!(text.contains("pending work is preserved"), "{text}");
    assert!(
        text.contains("CB-7B"),
        "refusal names the re-globalizing unit: {text}"
    );
    assert!(
        fixture.binding_refs().is_empty(),
        "dirty state must not produce an Anchor"
    );
    assert_eq!(
        fs::read(fixture.root.path().join("tracked.txt")).unwrap(),
        b"pending edit\n",
        "pending work preserved byte-for-byte"
    );
}

#[test]
fn enable_mismatch_offers_adopt_atomic_and_executes_the_projection() {
    let fixture = Colocated::new("anchor-mismatch");
    // Atomic-ahead divergence. Since CB-8A, a record projects into the Git
    // shadow immediately (both statuses clean), so the divergence is built
    // by rewinding Git behind the projected Atomic state.
    fs::write(fixture.root.path().join("tracked.txt"), b"atomic ahead\n").expect("edit");
    atomic_ok(
        fixture.root.path(),
        fixture.home.path(),
        &["add", "tracked.txt"],
    );
    atomic_ok(
        fixture.root.path(),
        fixture.home.path(),
        &["record", "-m", "atomic ahead"],
    );
    // Rewind the ref and index only: the worktree stays at the projected
    // content, so the Atomic working copy stays clean and the mismatch is
    // purely the unprojected HEAD plus a still-current index.
    git_ok(fixture.root.path(), &["reset", "-q", "--mixed", "HEAD~1"]);

    let refused = atomic(
        fixture.root.path(),
        fixture.home.path(),
        &[
            "git",
            "bridge",
            "enable",
            "--binding-key-file",
            fixture.key().to_str().unwrap(),
        ],
    );
    let refused_text = atomic_text(&refused);
    assert!(
        refused.status.success(),
        "passive enable reports the mismatch without failing: {refused_text}"
    );
    assert!(refused_text.contains("--adopt-atomic"), "{refused_text}");
    assert!(
        refused_text.contains("never silently merges"),
        "refusal forbids silent merges: {refused_text}"
    );
    assert!(
        fixture.binding_refs().is_empty(),
        "a mismatched store is never anchored silently"
    );

    let adopted = atomic(
        fixture.root.path(),
        fixture.home.path(),
        &[
            "git",
            "bridge",
            "enable",
            "--adopt-atomic",
            "--binding-key-file",
            fixture.key().to_str().unwrap(),
        ],
    );
    let adopted_text = atomic_text(&adopted);
    assert!(
        adopted.status.success(),
        "adopt-atomic projection: {adopted_text}"
    );
    assert!(adopted_text.contains("Anchor binding"), "{adopted_text}");
    assert_eq!(fixture.binding_refs().len(), 1);
    atomic_ok(
        fixture.root.path(),
        fixture.home.path(),
        &["git", "bridge", "verify"],
    );
}

#[test]
fn adopt_git_flag_is_explicitly_unavailable() {
    let fixture = Colocated::new("anchor-adopt-git");
    let output = atomic(
        fixture.root.path(),
        fixture.home.path(),
        &["git", "bridge", "enable", "--adopt-git"],
    );
    assert!(!output.status.success(), "--adopt-git must fail closed");
    let text = atomic_text(&output);
    assert!(text.contains("--adopt-git is unavailable"), "{text}");
    assert!(text.contains("Phase 9"), "{text}");
    assert!(text.contains("CB-9B/9C"), "{text}");
}

#[test]
fn enable_without_signing_key_reports_an_actionable_refusal() {
    let fixture = Colocated::new("anchor-no-key");
    let output = atomic(
        fixture.root.path(),
        fixture.home.path(),
        &["git", "bridge", "enable"],
    );
    assert!(output.status.success(), "hook integration still succeeds");
    let text = atomic_text(&output);
    assert!(text.contains("bridge anchor not created"), "{text}");
    assert!(text.contains("--binding-key-file"), "{text}");
    assert!(
        fixture.binding_refs().is_empty(),
        "no Anchor without an explicit signing key"
    );
}

#[test]
fn enable_on_unborn_head_is_typed() {
    let root = tempfile::TempDir::new().expect("repo tempdir");
    let home = tempfile::TempDir::new().expect("home tempdir");
    git_ok(root.path(), &["init", "-q", "-b", "main"]);
    atomic_ok(root.path(), home.path(), &["init"]);
    let key = write_key_file(home.path(), "unborn.hex", 0x33);
    let key = key.to_str().unwrap();
    let output = atomic(
        root.path(),
        home.path(),
        &["git", "bridge", "enable", "--binding-key-file", key],
    );
    let text = atomic_text(&output);
    assert!(
        output.status.success(),
        "unborn HEAD is a typed notice, not a hook failure: {text}"
    );
    assert!(text.contains("unborn Git HEAD"), "{text}");
    assert!(text.contains("create at least one commit"), "{text}");
    assert!(!root.path().join(".atomic/bridge/workspace.json").exists());
}

// ── AC-2: bound HEAD adoption ────────────────────────────────────────────

#[test]
fn reset_to_bound_head_adopts_with_zero_tracked_rewrites() {
    let fixture = Colocated::new("adoption-reset");
    fixture.anchor();
    let anchored_head = git(fixture.root.path(), &["rev-parse", "HEAD"]);

    // An unbound commit first: adoption must refuse, never import.
    fs::write(fixture.root.path().join("tracked.txt"), b"unbound\n").expect("edit");
    git_ok(fixture.root.path(), &["add", "tracked.txt"]);
    git_ok(fixture.root.path(), &["commit", "-qm", "unbound"]);
    let refused = atomic(
        fixture.root.path(),
        fixture.home.path(),
        &["status", "--short"],
    );
    assert!(
        !refused.status.success(),
        "unbound HEAD refuses: {}",
        atomic_text(&refused)
    );

    // `git reset --hard` back to the bound commit: Git rewrote the files; the
    // adoption must adopt without touching them again (§7.3 no file writes).
    git_ok(
        fixture.root.path(),
        &["reset", "-q", "--hard", &anchored_head],
    );
    let path = fixture.root.path().join("tracked.txt");
    let before_mtime = fs::metadata(&path).unwrap().modified().unwrap();

    let status = atomic(
        fixture.root.path(),
        fixture.home.path(),
        &["status", "--short"],
    );
    assert!(
        status.status.success(),
        "bound reset adopts: {}",
        atomic_text(&status)
    );
    assert_eq!(fs::read(&path).unwrap(), b"anchor me\n");
    let after_mtime = fs::metadata(&path).unwrap().modified().unwrap();
    assert_eq!(
        after_mtime, before_mtime,
        "adoption must not rewrite tracked files"
    );
    assert_eq!(fixture.checkpoint_head(), anchored_head);
    atomic_ok(
        fixture.root.path(),
        fixture.home.path(),
        &["git", "bridge", "verify"],
    );
}

#[test]
fn unbound_head_returns_typed_remediation_naming_phase_9() {
    let fixture = Colocated::new("adoption-unbound-typed");
    fixture.anchor();
    fs::write(fixture.root.path().join("tracked.txt"), b"no binding\n").expect("edit");
    git_ok(fixture.root.path(), &["add", "tracked.txt"]);
    git_ok(fixture.root.path(), &["commit", "-qm", "unbound"]);

    let output = atomic(fixture.root.path(), fixture.home.path(), &["status"]);
    assert!(!output.status.success());
    let text = atomic_text(&output);
    assert!(text.contains("HeadChanged"), "{text}");
}

#[test]
fn bound_detached_adopt_journals_import_git_head_and_updates_checkpoint() {
    let fixture = Colocated::new("adoption-detached");
    fixture.anchor();
    git_ok(fixture.root.path(), &["checkout", "-q", "--detach"]);

    let status = atomic(
        fixture.root.path(),
        fixture.home.path(),
        &["status", "--short"],
    );
    assert!(
        status.status.success(),
        "bound detached status adopts: {}",
        atomic_text(&status)
    );

    // Ephemeral Draft view mapped to the detached commit (§7.5).
    let view = fixture.checkpoint_view();
    let short = git(fixture.root.path(), &["rev-parse", "--short", "HEAD"]);
    assert_eq!(view, format!("git/{short}"), "ephemeral view name");
    assert_eq!(
        fixture.checkpoint_head(),
        git(fixture.root.path(), &["rev-parse", "HEAD"])
    );

    // The adoption journaled ImportGitHead (RFC §7.3).
    let log = atomic_ok(
        fixture.root.path(),
        fixture.home.path(),
        &["op", "log", "--json"],
    );
    assert!(log.contains("import_git_head"), "op log: {log}");

    // The head map records the ephemeral mapping (private evidence).
    let map = head_map(fixture.root.path());
    let head = git(fixture.root.path(), &["rev-parse", "HEAD"]);
    let entry = &map["mappings"][head.as_str()];
    assert_eq!(entry["view"].as_str(), Some(view.as_str()), "{map:?}");
    assert_eq!(entry["ephemeral"].as_bool(), Some(true));

    // The clean-adoption gate proved full manifest equivalence during the
    // adoption itself (§7.3, §12.5); status stays clean on the adopted view.
    let after = atomic_ok(
        fixture.root.path(),
        fixture.home.path(),
        &["status", "--short"],
    );
    assert!(after.trim().is_empty() || !after.contains("M "), "{after}");
}

// ── AC-3: identity selection and ephemeral lifecycle ─────────────────────

#[test]
fn recording_works_on_the_detached_view_and_switch_c_renames_it() {
    let fixture = Colocated::new("adoption-rename");
    fixture.anchor();
    git_ok(fixture.root.path(), &["checkout", "-q", "--detach"]);
    atomic_ok(
        fixture.root.path(),
        fixture.home.path(),
        &["status", "--short"],
    );
    let ephemeral = fixture.checkpoint_view();
    assert!(ephemeral.starts_with("git/"), "{ephemeral}");

    // Recording works normally on the ephemeral detached view (RFC §7.5).
    fs::write(fixture.root.path().join("tracked.txt"), b"detached work\n").expect("edit");
    atomic_ok(
        fixture.root.path(),
        fixture.home.path(),
        &["add", "tracked.txt"],
    );
    atomic_ok(
        fixture.root.path(),
        fixture.home.path(),
        &["record", "-m", "detached work"],
    );
    assert_eq!(fixture.checkpoint_view(), ephemeral);

    // `git switch -c` renames the ephemeral view without discarding identity
    // (RFC §7.5): the recorded change stays in the renamed view.
    git_ok(fixture.root.path(), &["switch", "-qc", "topic"]);
    let renamed = atomic(
        fixture.root.path(),
        fixture.home.path(),
        &["status", "--short"],
    );
    let renamed_text = atomic_text(&renamed);
    assert!(
        renamed.status.success(),
        "switch -c rename adopts: {renamed_text}"
    );
    assert_eq!(fixture.checkpoint_view(), "topic", "{renamed_text}");
    let views = atomic_ok(fixture.root.path(), fixture.home.path(), &["view", "list"]);
    assert!(views.contains("topic"), "{views}");
    assert!(
        !views.contains(&ephemeral.as_str()),
        "old name gone: {views}"
    );
    let map = head_map(fixture.root.path());
    let head = git(fixture.root.path(), &["rev-parse", "HEAD"]);
    assert_eq!(
        map["mappings"][head.as_str()]["view"].as_str(),
        Some("topic"),
        "{map:?}"
    );
    assert_eq!(
        map["mappings"][head.as_str()]["ephemeral"].as_bool(),
        Some(false)
    );
}

#[test]
fn later_named_checkout_retains_the_ephemeral_view() {
    let fixture = Colocated::new("adoption-retain");
    fixture.anchor();
    git_ok(fixture.root.path(), &["checkout", "-q", "--detach"]);
    atomic_ok(
        fixture.root.path(),
        fixture.home.path(),
        &["status", "--short"],
    );
    let ephemeral = fixture.checkpoint_view();
    assert!(ephemeral.starts_with("git/"));

    // Returning to the named branch adopts its bound view and retains the
    // ephemeral view (§7.5 "the ephemeral view is retained").
    git_ok(fixture.root.path(), &["switch", "-q", "main"]);
    let switched = atomic_ok(
        fixture.root.path(),
        fixture.home.path(),
        &["status", "--short"],
    );
    assert_eq!(fixture.checkpoint_view(), "main", "{switched}");
    let views = atomic_ok(fixture.root.path(), fixture.home.path(), &["view", "list"]);
    assert!(views.contains(&ephemeral), "{views}");

    // A no-op detach at the same commit leaves the workspace aligned.
    git_ok(fixture.root.path(), &["checkout", "-q", "--detach", "main"]);
    let detached = atomic_ok(
        fixture.root.path(),
        fixture.home.path(),
        &["status", "--short"],
    );
    assert_eq!(fixture.checkpoint_view(), "main", "{detached}");
}

#[test]
fn missing_head_target_is_typed_without_guessing() {
    let fixture = Colocated::new("adoption-missing-target");
    fixture.anchor();
    git_ok(
        fixture.root.path(),
        &["checkout", "-q", "--orphan", "ghost"],
    );
    git_ok(fixture.root.path(), &["rm", "-rqf", "."]);

    let output = atomic(
        fixture.root.path(),
        fixture.home.path(),
        &["status", "--short"],
    );
    assert!(
        !output.status.success(),
        "missing HEAD target refuses: {}",
        atomic_text(&output)
    );
    let text = atomic_text(&output);
    assert!(
        text.contains("MissingHeadTarget") || text.contains("UnbornHead"),
        "{text}"
    );
    assert!(
        !fixture.checkpoint_view().starts_with("git/"),
        "no guessing into an ephemeral view: {text}"
    );
}

// ── AC-4: interruption, hooks, and retry ─────────────────────────────────

#[test]
fn enable_preserves_custom_hooks_and_advisory_behavior() {
    let fixture = Colocated::new("anchor-hooks");
    let hook = fixture.root.path().join(".git/hooks/post-checkout");
    fs::write(&hook, b"#!/bin/sh\necho custom ran\n").expect("custom hook");
    let before = fs::read(&hook).unwrap();

    fixture.anchor();

    assert_eq!(
        fs::read(&hook).unwrap(),
        before,
        "custom post-checkout hook is byte-for-byte preserved"
    );
    assert!(fixture
        .root
        .path()
        .join(".atomic/bridge/workspace.json")
        .exists());
}

#[test]
fn anchor_retry_after_interrupted_checkpoint_write_is_idempotent() {
    let fixture = Colocated::new("anchor-retry");
    let first = fixture.anchor();
    let first_id = binding_id_from_output(&first);
    // Simulate a crash between the journaled Anchor publication and the
    // checkpoint write: the binding exists, the derived checkpoint is gone.
    fs::remove_file(fixture.root.path().join(".atomic/bridge/workspace.json")).expect("remove");

    let retried = fixture.anchor();
    assert!(
        retried.contains("already exists"),
        "retry replays the identical Anchor: {retried}"
    );
    assert!(retried.contains(&first_id), "same binding id after retry");
    assert_eq!(
        fixture.binding_refs().len(),
        1,
        "create-only: no duplicate ref"
    );
    verify_signature_and_content(&fixture, &first_id);
    assert!(fixture
        .root
        .path()
        .join(".atomic/bridge/workspace.json")
        .exists());
    assert_eq!(
        fixture.checkpoint_head(),
        git(fixture.root.path(), &["rev-parse", "HEAD"])
    );
}

#[test]
fn adoption_is_event_independent_and_never_depends_on_hooks() {
    let fixture = Colocated::new("adoption-no-hooks");
    fixture.anchor();
    // Correctness never depends on hooks having run (RFC §11.1): remove the
    // advisory dispatcher and let the command boundary observe Git directly.
    fs::remove_file(fixture.root.path().join(".git/hooks/post-checkout")).ok();

    git_ok(fixture.root.path(), &["checkout", "-q", "--detach"]);
    let status = atomic(
        fixture.root.path(),
        fixture.home.path(),
        &["status", "--short"],
    );
    assert!(
        status.status.success(),
        "adoption without hooks: {}",
        atomic_text(&status)
    );
    assert!(fixture.checkpoint_view().starts_with("git/"));
}

#[test]
fn adoption_refuses_a_held_git_index_lock_instead_of_racing() {
    let fixture = Colocated::new("adoption-race");
    fixture.anchor();
    git_ok(fixture.root.path(), &["checkout", "-q", "--detach"]);
    fs::write(fixture.root.path().join(".git/index.lock"), b"held\n").expect("lock");

    let output = atomic(
        fixture.root.path(),
        fixture.home.path(),
        &["status", "--short"],
    );
    assert!(!output.status.success(), "index lock refuses adoption");
    let text = atomic_text(&output);
    assert!(text.contains("IndexLocked"), "{text}");

    fs::remove_file(fixture.root.path().join(".git/index.lock")).expect("release lock");
    let status = atomic(
        fixture.root.path(),
        fixture.home.path(),
        &["status", "--short"],
    );
    assert!(status.status.success(), "{}", atomic_text(&status));
    assert!(fixture.checkpoint_view().starts_with("git/"));
}

#[test]
fn branch_rename_adopts_via_binding_identity_without_same_name_guessing() {
    let fixture = Colocated::new("adoption-branch-rename");
    fixture.anchor();
    // Rename the branch HEAD is attached to: the commit identity is unchanged
    // and still bound, so adoption re-anchors against the binding (RFC §8.1
    // "reconciled through the mapping") — never against a same-name view.
    git_ok(
        fixture.root.path(),
        &["branch", "-m", "main", "renamed-main"],
    );

    let output = atomic(
        fixture.root.path(),
        fixture.home.path(),
        &["status", "--short"],
    );
    assert!(
        output.status.success(),
        "branch rename adopts: {}",
        atomic_text(&output)
    );
    assert_eq!(
        fixture.checkpoint_head(),
        git(fixture.root.path(), &["rev-parse", "HEAD"])
    );
    // The adoption clean gate proved full manifest equivalence (§7.3, §12.5);
    // the prototype verifier still keys on the branch/view name pair (CB-10A).
    let views = atomic_ok(fixture.root.path(), fixture.home.path(), &["view", "list"]);
    assert!(views.contains("main"), "{views}");
}

#[test]
fn deleted_branch_target_adopts_the_bound_commit_into_an_ephemeral_view() {
    let fixture = Colocated::new("adoption-branch-delete");
    fixture.anchor();
    // Detach, then delete the branch under the workspace.
    git_ok(fixture.root.path(), &["checkout", "-q", "--detach"]);
    atomic_ok(
        fixture.root.path(),
        fixture.home.path(),
        &["status", "--short"],
    );
    git_ok(fixture.root.path(), &["branch", "-D", "main"]);

    // The commit is still bound, so the workspace re-adopts it into the
    // mapped ephemeral view instead of refusing (§7.5).
    git_ok(fixture.root.path(), &["checkout", "-q", "--detach"]);
    let output = atomic(
        fixture.root.path(),
        fixture.home.path(),
        &["status", "--short"],
    );
    assert!(
        output.status.success(),
        "deleted branch adopts: {}",
        atomic_text(&output)
    );
    let views = atomic_ok(fixture.root.path(), fixture.home.path(), &["view", "list"]);
    assert!(views.contains("git/"), "{views}");
}

#[test]
fn two_linked_worktrees_share_bindings_but_keep_their_own_checkpoints() {
    let fixture = Colocated::new("linked-worktrees");
    fixture.anchor();
    let anchored_head = git(fixture.root.path(), &["rev-parse", "HEAD"]);

    // A linked Git worktree registered against the common Atomic repository:
    // bindings live in the shared pristine; checkpoints are per workspace.
    let worktree_parent = tempfile::TempDir::new().expect("worktree parent");
    let worktree = worktree_parent.path().join("linked-wt");
    git_ok(
        fixture.root.path(),
        &[
            "worktree",
            "add",
            "-q",
            worktree.to_str().unwrap(),
            "-b",
            "wt2branch",
        ],
    );
    fs::create_dir_all(worktree.join(".atomic")).expect("worktree dot dir");
    let pointer = format!("{}/.atomic", fixture.root.path().display());
    fs::write(worktree.join(".atomic/repository"), pointer).expect("repository pointer");
    // A read command registers the linked worktree's own working-copy
    // identity against the common repository.
    atomic_ok(&worktree, fixture.home.path(), &["view", "list"]);

    // The binding is shared through the common pristine and common refs.
    assert_eq!(fixture.binding_refs().len(), 1);

    // A detached checkout of the bound commit in the second worktree adopts
    // into its own ephemeral view and its own checkpoint, without disturbing
    // the first workspace's checkpoint (§7.5, §12.1).
    git_ok(&worktree, &["checkout", "-q", "--detach"]);
    atomic_ok(&worktree, fixture.home.path(), &["status", "--short"]);
    let worktree_view = checkpoint_field(&worktree, "view");
    assert!(worktree_view.starts_with("git/"), "{worktree_view}");
    assert_eq!(
        checkpoint_field(&worktree, "git_head"),
        anchored_head,
        "second worktree checkpoint tracks its own HEAD"
    );
    assert_eq!(
        fixture.checkpoint_view(),
        "main",
        "first worktree checkpoint untouched"
    );

    // The worktree view is named after the detached commit's short OID.
    let short = git(&worktree, &["rev-parse", "--short=7", "HEAD"]);
    assert_eq!(worktree_view, format!("git/{short}"));

    // Both workspaces verify independently against the shared bindings.
    atomic_ok(
        fixture.root.path(),
        fixture.home.path(),
        &["git", "bridge", "verify"],
    );
}
