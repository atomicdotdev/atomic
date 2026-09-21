//! CB-10B end-to-end: binding transport, remote leases, and exact Git clone
//! bootstrap through the real CLI (RFC §5, §7.3, §8.5, §8.6, §12; task 4).
//!
//! Offline local-remote fixtures prove:
//! - `bridge enable` configures the explicit RFC §8.6 fetch refspecs for
//!   `refs/atomic/bindings/*` and `refs/atomic/views/*`, idempotently,
//! - the bounded create-only publication queue republishes missing binding
//!   refs, retries idempotently, refuses a conflicting remote target without
//!   overwriting it, and reports publication only after remote verification,
//! - mapped-ref pushes run under `last_observed_remote` expected-old leases:
//!   a stale remote move refuses and is never overwritten, a fresh push is
//!   verified and recorded back into the mapping,
//! - `atomic clone <git-url>` bootstraps through the shared binding-first
//!   anchoring: a valid binding restores the exact Merkle/closure state,
//!   an unknown signer's content resurrects with UNTRUSTED provenance, and
//!   an unbound history is an explicit typed refusal (no false exact label),
//! - hosts that reject the binding namespace surface the explicit
//!   diagnostic; the degraded `refs/heads/atomic/bindings/*` fallback runs
//!   only on explicit opt-in and is visibly labeled DEGRADED,
//! - privacy: binding trees hold only the allowlisted blobs, and WIP refs
//!   never transfer.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::str::FromStr;

use atomic_core::types::Base32;

use tempfile::TempDir;

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

fn assert_success(output: &Output, context: &str) {
    assert!(
        output.status.success(),
        "{context} failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn output_text(output: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

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

fn git(root: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("run git")
}

fn assert_git(root: &Path, args: &[&str], context: &str) {
    let out = git(root, args);
    assert!(out.status.success(), "{context} failed: {}", output_text(&out));
}

fn git_text(root: &Path, args: &[&str]) -> String {
    let out = git(root, args);
    assert!(out.status.success(), "git {args:?} failed: {}", output_text(&out));
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// A git-import publisher fixture (no binding publication): mirrors the
/// CB-6B setup — Git repo with one commit, imported baseline, then one
/// native change. Used by tests that exercise ref/mapping transport.
fn setup_import_publisher(root: &Path, home: &Path) {
    {
        let git_repo = git2::Repository::init(root).expect("init git");
        fs::write(root.join("tracked.txt"), b"tracked bytes\n").expect("write file");
        let mut index = git_repo.index().expect("open index");
        index.add_path(Path::new("tracked.txt")).expect("stage");
        index.write().expect("write index");
        let tree_oid = index.write_tree().expect("write tree");
        let tree = git_repo.find_tree(tree_oid).expect("find tree");
        let signature = git2::Signature::now("Test", "test@example.com").expect("signature");
        git_repo
            .commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
            .expect("commit");
    }

    assert_success(&atomic(root, home, &["init"]), "atomic init");
    assert_success(&atomic(root, home, &["git", "import", "--no-vault"]), "git import");

    fs::write(root.join("feature.txt"), b"feature bytes\n").expect("write file");
    assert_success(&atomic(root, home, &["add", "feature.txt"]), "add");
    assert_success(&atomic(root, home, &["record", "-m", "feature"]), "record");
}

/// A native publisher fixture whose whole closure is pack-able (CB-6B): no
/// git-imported change ever carries an unhashed metadata section, so the
/// bounded `changes.pack` fallback is allowed and a receiver without an
/// Atomic remote can still verify the complete closure.
fn setup_native_publisher(root: &Path, home: &Path, key: &Path) {
    // Native Atomic history (no git import: every recorded change stays
    // pack-able for the CB-6B bounded fallback). The conversion policy
    // excludes `.atomicignore` from the Git projection, so the Git tree
    // holds only tracked paths.
    assert_success(
        &atomic(root, home, &["init", "--no-vault", "--view", "main"]),
        "atomic init",
    );
    fs::write(root.join("tracked.txt"), b"tracked bytes\n").expect("write file");
    assert_success(&atomic(root, home, &["add", "tracked.txt"]), "add tracked");
    assert_success(&atomic(root, home, &["record", "-m", "base"]), "record base");

    assert_git(root, &["init", "-q"], "git init");
    assert_git(root, &["add", "tracked.txt"], "stage tracked");
    assert_git(root, &["commit", "-qm", "base"], "git commit base");

    // Anchor at the mirrored base with the bounded changes.pack attached to
    // the Anchor's binding tree, so a receiver without an Atomic remote can
    // still verify the complete closure. The binding names the anchored base
    // state exactly (checkpoint HEAD == binding HEAD).
    assert_success(
        &atomic(
            root,
            home,
            &[
                "git",
                "bridge",
                "enable",
                "--binding-key-file",
                key.to_str().unwrap(),
                "--with-changes-pack",
            ],
        ),
        "bridge enable",
    );
}

/// Bare remote + clone with the RFC §8.6 fetch refspecs configured.
fn make_remote_and_fetch_refspecs(root: &Path, remote_dir: &Path, remote_name: &str) {
    Command::new("git")
        .args(["init", "--bare", "-q", remote_dir.to_str().unwrap()])
        .output()
        .expect("init bare remote");
    assert_git(
        root,
        &["remote", "add", remote_name, remote_dir.to_str().unwrap()],
        "add remote",
    );
}

// ── AC-1: explicit fetch refspecs on enable ─────────────────────────────

#[test]
fn enable_configures_both_refspecs_idempotently() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("home");
    let publisher = temp.path().join("publisher");
    fs::create_dir_all(&publisher).expect("publisher");
    let key = write_key_file(temp.path(), "key.hex", 0x50);
    setup_import_publisher(&publisher, &home);

    let remote_dir = temp.path().join("origin.git");
    make_remote_and_fetch_refspecs(&publisher, &remote_dir, "origin");

    assert_success(
        &atomic(
            &publisher,
            &home,
            &["git", "bridge", "enable", "--binding-key-file", key.to_str().unwrap()],
        ),
        "bridge enable with remote",
    );

    let refspecs = git_text(&publisher, &["config", "--get-all", "remote.origin.fetch"]);
    let binding_specs = refspecs
        .lines()
        .filter(|line| line.contains("refs/atomic/bindings"))
        .count();
    let view_specs = refspecs
        .lines()
        .filter(|line| line.contains("refs/atomic/views/*"))
        .count();
    assert_eq!(binding_specs, 1, "exactly one binding refspec: {refspecs}");
    assert_eq!(view_specs, 1, "exactly one views refspec: {refspecs}");

    // Idempotent repeat: no duplicates.
    assert_success(
        &atomic(
            &publisher,
            &home,
            &["git", "bridge", "enable", "--binding-key-file", key.to_str().unwrap()],
        ),
        "repeat bridge enable",
    );
    let refspecs = git_text(&publisher, &["config", "--get-all", "remote.origin.fetch"]);
    assert_eq!(
        refspecs
            .lines()
            .filter(|line| line.contains("refs/atomic/bindings"))
            .count(),
        1,
        "repeat enable must not duplicate the binding refspec: {refspecs}"
    );

    // A missing remote is a warning, not a crash.
    let missing = atomic(&publisher, &home, &["git", "bridge", "enable", "--remote", "nowhere", "--binding-key-file", key.to_str().unwrap()]);
    assert!(
        missing.status.success(),
        "a missing remote must be a notice, not a failure: {}",
        output_text(&missing)
    );
    assert!(
        output_text(&missing).contains("not configured"),
        "the notice must say the refspecs were not configured: {}",
        output_text(&missing)
    );
}

// ── AC-1: bounded create-only publication queue ─────────────────────────

#[test]
fn queue_republishes_missing_refs_idempotently_and_refuses_conflicting_targets() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("home");
    let publisher = temp.path().join("publisher");
    fs::create_dir_all(&publisher).expect("publisher");
    let key = write_key_file(temp.path(), "key.hex", 0x51);
    setup_import_publisher(&publisher, &home);
    assert_success(
        &atomic(
            &publisher,
            &home,
            &[
                "git",
                "bridge",
                "binding",
                "publish",
                "--key-file",
                key.to_str().unwrap(),
            ],
        ),
        "binding publish",
    );

    let binding_ref = {
        let repo = atomic_repository::Repository::open_readonly(&publisher).expect("open");
        let ids = repo.binding_ids().expect("ids");
        assert!(!ids.is_empty(), "the publisher must store a binding");
        let id = ids[0].to_hex();
        format!("refs/atomic/bindings/{}/{}", &id[..2], &id)
    };

    // Simulate an interrupted transfer: the binding is stored but its
    // create-only ref is missing.
    assert_git(&publisher, &["update-ref", "-d", &binding_ref], "delete binding ref");
    assert!(
        git_text(&publisher, &["for-each-ref", &binding_ref]).trim().is_empty(),
        "the ref must be gone before the queue runs"
    );

    // The queue republishes the missing ref through the journaled CAS path.
    let queue = atomic(&publisher, &home, &["git", "bridge", "binding", "queue"]);
    assert_success(&queue, "binding queue");
    assert!(
        output_text(&queue).contains("published"),
        "the queue must report the republication: {}",
        output_text(&queue)
    );
    assert!(!git_text(&publisher, &["for-each-ref", &binding_ref]).trim().is_empty());

    // Retry is idempotent: nothing new is published.
    let retry = atomic(&publisher, &home, &["git", "bridge", "binding", "queue"]);
    assert_success(&retry, "binding queue retry");
    assert!(
        output_text(&retry).contains("0 published"),
        "the retry must be a typed no-op: {}",
        output_text(&retry)
    );

    // A conflicting ref target is refused without overwriting.
    let foreign = {
        let other = git2::Repository::open(&publisher).expect("open git");
        let signature = git2::Signature::now("Test", "test@example.com").expect("signature");
        let tree_id = other.treebuilder(None).expect("treebuilder").write().expect("write tree");
        let tree = other.find_tree(tree_id).expect("find tree");
        let commit = other
            .commit(None, &signature, &signature, "foreign", &tree, &[])
            .expect("create conflicting commit");
        other
            .reference(&binding_ref, commit, true, "hostile target")
            .expect("force the hostile target onto the binding ref");
        commit
    };

    let refused = atomic(&publisher, &home, &["git", "bridge", "binding", "queue", "--max", "1"]);
    assert!(
        !refused.status.success(),
        "a conflicting ref target must refuse: {}",
        output_text(&refused)
    );
    assert!(
        output_text(&refused).contains("create-only"),
        "the refusal must name the create-only boundary: {}",
        output_text(&refused)
    );
    let now = git_text(&publisher, &["rev-parse", &binding_ref]);
    assert_eq!(
        now.trim(),
        foreign.to_string(),
        "the foreign ref target must be preserved untouched"
    );
}

#[test]
fn queue_remote_transfer_is_create_only_and_verified_before_publication_report() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("home");
    let publisher = temp.path().join("publisher");
    fs::create_dir_all(&publisher).expect("publisher");
    let key = write_key_file(temp.path(), "key.hex", 0x52);
    setup_import_publisher(&publisher, &home);
    assert_success(
        &atomic(
            &publisher,
            &home,
            &[
                "git",
                "bridge",
                "binding",
                "publish",
                "--key-file",
                key.to_str().unwrap(),
            ],
        ),
        "binding publish",
    );

    let remote_dir = temp.path().join("origin.git");
    make_remote_and_fetch_refspecs(&publisher, &remote_dir, "origin");

    // First transfer: succeeds and verifies.
    let queue = atomic(&publisher, &home, &["git", "bridge", "binding", "queue", "--remote", "origin"]);
    assert_success(&queue, "queue --remote");
    assert!(
        output_text(&queue).contains("Verified"),
        "publication must be reported only after remote verification: {}",
        output_text(&queue)
    );
    let binding_ref = git_text(&publisher, &["for-each-ref", "--format=%(refname)", "refs/atomic/bindings"])
        .lines()
        .next()
        .expect("one binding ref")
        .trim()
        .to_string();
    let remote_refs = git_text(&remote_dir, &["for-each-ref", "--format=%(refname)"]);
    assert!(
        remote_refs.contains(&binding_ref),
        "the binding ref must exist on the remote: {remote_refs}"
    );
    // WIP refs never transfer (RFC §6.4).
    assert!(
        !remote_refs.contains("refs/atomic/wip/"),
        "WIP refs must never transfer: {remote_refs}"
    );

    // A hostile remote ref (moved externally) is never overwritten.
    let local_target = git_text(&publisher, &["rev-parse", &binding_ref]);
    let foreign = {
        let git_repo = git2::Repository::open_bare(&remote_dir).expect("open remote");
        let signature = git2::Signature::now("Test", "test@example.com").expect("signature");
        let tree_id = git_repo.treebuilder(None).expect("treebuilder").write().expect("write tree");
        let tree = git_repo.find_tree(tree_id).expect("find tree");
        let commit = git_repo
            .commit(None, &signature, &signature, "hostile", &tree, &[])
            .expect("create hostile commit");
        git_repo
            .reference(&binding_ref, commit, true, "hostile remote target")
            .expect("force the hostile target onto the remote binding ref");
        commit
    };
    let queue = atomic(&publisher, &home, &["git", "bridge", "binding", "queue", "--remote", "origin"]);
    assert!(
        !queue.status.success(),
        "the create-only transfer must refuse a moved remote ref: {}",
        output_text(&queue)
    );
    let now = git_text(&remote_dir, &["rev-parse", &binding_ref]);
    assert_eq!(
        now.trim(),
        foreign.to_string(),
        "the hostile remote target must be preserved untouched"
    );
    let _ = local_target;
}

// ── AC-1: remote expected-old leases on mapped-ref pushes ───────────────

#[test]
fn push_lease_creates_remote_branch_and_records_the_observation() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("home");
    let publisher = temp.path().join("publisher");
    fs::create_dir_all(&publisher).expect("publisher");
    setup_import_publisher(&publisher, &home);

    let remote_dir = temp.path().join("origin.git");
    make_remote_and_fetch_refspecs(&publisher, &remote_dir, "origin");
    let head = git_text(&publisher, &["rev-parse", "HEAD"]).trim().to_string();

    // The leased publication creates the remote branch at the current HEAD
    // and records the verified remote observation.
    let push = atomic(&publisher, &home, &["git", "push", "--branch", "pub"]);
    assert_success(&push, "first atomic git push --branch pub");
    let remote_tip = git_text(&remote_dir, &["rev-parse", "refs/heads/pub"]);
    assert_eq!(
        remote_tip.trim(),
        head,
        "the remote branch must hold the pushed HEAD"
    );

    {
        let repo = atomic_repository::Repository::open_readonly(&publisher).expect("open");
        let view = repo
            .desired_view_name(repo.require_working_copy_id().expect("working copy"))
            .expect("view");
        let mapping = repo
            .get_ref_mapping(&view)
            .expect("mapping")
            .expect("baseline mapping");
        assert_eq!(
            mapping.remote,
            Some(("origin".to_string(), "refs/heads/pub".to_string())),
            "the tracking pair must be recorded: {mapping:?}"
        );
        assert_eq!(
            mapping.last_observed_remote.as_deref(),
            Some(head.as_str()),
            "the verified remote tip must be recorded after the push: {mapping:?}"
        );
    }
}

#[test]
fn push_lease_refuses_stale_remote_without_overwriting() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("home");
    let publisher = temp.path().join("publisher");
    fs::create_dir_all(&publisher).expect("publisher");
    setup_import_publisher(&publisher, &home);

    let remote_dir = temp.path().join("origin.git");
    make_remote_and_fetch_refspecs(&publisher, &remote_dir, "origin");

    // Push 1 establishes the honest remote observation (create-only lease).
    let push = atomic(&publisher, &home, &["git", "push", "--branch", "pub"]);
    assert_success(&push, "first atomic git push --branch pub");
    let head = git_text(&publisher, &["rev-parse", "HEAD"]).trim().to_string();
    let observed = git_text(&remote_dir, &["rev-parse", "refs/heads/pub"]);
    assert_eq!(observed.trim(), head);

    // Newer external remote work the mapping never observed: a hostile
    // commit forced onto the tracked remote ref.
    let hostile = {
        let git_repo = git2::Repository::open_bare(&remote_dir).expect("open remote");
        let signature = git2::Signature::now("Remote", "remote@example.com").expect("signature");
        let tree_id = git_repo
            .treebuilder(None)
            .expect("treebuilder")
            .write()
            .expect("write tree");
        let tree = git_repo.find_tree(tree_id).expect("find tree");
        let commit = git_repo
            .commit(None, &signature, &signature, "hostile external move", &tree, &[])
            .expect("create hostile commit");
        git_repo
            .reference("refs/heads/pub", commit, true, "hostile external move")
            .expect("force the hostile target onto the remote ref");
        commit
    };

    // The stale lease refuses the push and the external value survives.
    let push = atomic(&publisher, &home, &["git", "push", "--branch", "pub"]);
    assert!(
        !push.status.success(),
        "the stale lease must refuse the push: {}",
        output_text(&push)
    );
    assert!(
        output_text(&push).contains("lease refused to overwrite"),
        "the refusal must name the lease: {}",
        output_text(&push)
    );
    let now = git_text(&remote_dir, &["rev-parse", "refs/heads/pub"]);
    assert_eq!(
        now.trim(),
        hostile.to_string(),
        "the newer external remote work must not be overwritten"
    );
}

// ── AC-2: exact Git clone bootstrap ─────────────────────────────────────

#[test]
fn clone_git_url_restores_exact_bound_state() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("home");
    let publisher = temp.path().join("publisher");
    fs::create_dir_all(&publisher).expect("publisher");
    let key = write_key_file(temp.path(), "publisher-key.hex", 0x54);
    setup_native_publisher(&publisher, &home, &key);

    // The publisher's exact state before the clone.
    let publisher_state = {
        let repo = atomic_repository::Repository::open_readonly(&publisher).expect("open");
        let working_copy = repo.require_working_copy_id().expect("working copy");
        let view = repo.desired_view_name(working_copy).expect("view");
        repo.get_view_info(&view).expect("info").state.to_string()
    };

    let remote_dir = temp.path().join("origin.git");
    make_remote_and_fetch_refspecs(&publisher, &remote_dir, "origin");
    // Publish branch + binding namespace to the bare remote (RFC §8.6).
    assert_git(
        &publisher,
        &[
            "push",
            &remote_dir.to_str().unwrap(),
            "refs/heads/*:refs/heads/*",
            "refs/atomic/bindings/*:refs/atomic/bindings/*",
        ],
        "push branch + bindings",
    );

    // atomic clone <git-url> (a local bare repo is detected as a Git URL).
    let clone_dir = temp.path().join("clone");
    let clone_key = write_key_file(temp.path(), "clone-key.hex", 0x55);
    let clone = atomic(
        temp.path(),
        &home,
        &[
            "clone",
            &remote_dir.to_str().unwrap(),
            clone_dir.to_str().unwrap(),
            "--git",
            "--binding-key-file",
            clone_key.to_str().unwrap(),
        ],
    );
    assert_success(&clone, "atomic clone <git-url>");
    assert!(
        output_text(&clone).contains("Exact restoration verified"),
        "the bootstrap must verify exact restoration: {}",
        output_text(&clone)
    );
    // The cloner runs under a fresh identity that never signed the binding,
    // so the content is used after recomputation while the provenance and
    // attestation roots stay UNTRUSTED (RFC §5.6, AC-2 unknown-signer arm).
    assert!(
        output_text(&clone).contains("UNTRUSTED"),
        "an unknown signer's provenance must be labeled untrusted: {}",
        output_text(&clone)
    );

    // The restored view carries the EXACT bound Merkle state.
    let clone_state = {
        let repo = atomic_repository::Repository::open_readonly(&clone_dir).expect("open");
        let working_copy = repo.require_working_copy_id().expect("working copy");
        let view = repo.desired_view_name(working_copy).expect("view");
        repo.get_view_info(&view).expect("info").state.to_string()
    };
    assert_eq!(
        clone_state, publisher_state,
        "exact resurrection must restore the identical Merkle state"
    );

    // The restored closure is complete: the recorded file reopens byte-exact.
    let restored = {
        let repo = atomic_repository::Repository::open_readonly(&clone_dir).expect("open");
        let working_copy = repo.require_working_copy_id().expect("working copy");
        let view = repo.desired_view_name(working_copy).expect("view");
        repo.get_file_content_on_view("tracked.txt", &view)
            .expect("content")
            .expect("file present")
    };
    assert_eq!(restored.as_slice(), b"tracked bytes\n");
}

#[test]
fn clone_unbound_history_refuses_with_explicit_foreign_synthesis_guidance() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("home");
    let publisher = temp.path().join("publisher");
    fs::create_dir_all(&publisher).expect("publisher");
    let _key = write_key_file(temp.path(), "key.hex", 0x56);
    // Git-only publisher: no bindings ever published.
    {
        let git_repo = git2::Repository::init(&publisher).expect("init git");
        fs::write(publisher.join("tracked.txt"), b"tracked bytes\n").expect("write file");
        let mut index = git_repo.index().expect("index");
        index.add_path(Path::new("tracked.txt")).expect("stage");
        index.write().expect("write");
        let tree_oid = index.write_tree().expect("tree");
        let tree = git_repo.find_tree(tree_oid).expect("find tree");
        let signature = git2::Signature::now("Test", "test@example.com").expect("signature");
        git_repo
            .commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
            .expect("commit");
    }

    let remote_dir = temp.path().join("origin.git");
    Command::new("git")
        .args(["init", "--bare", "-q", remote_dir.to_str().unwrap()])
        .output()
        .expect("init bare remote");
    assert_git(
        &publisher,
        &[
            "push",
            &remote_dir.to_str().unwrap(),
            "refs/heads/*:refs/heads/*",
        ],
        "push branch only",
    );

    let clone_dir = temp.path().join("clone");
    let clone_key = write_key_file(temp.path(), "clone-key.hex", 0x57);
    let clone = atomic(
        temp.path(),
        &home,
        &[
            "clone",
            &remote_dir.to_str().unwrap(),
            clone_dir.to_str().unwrap(),
            "--git",
            "--binding-key-file",
            clone_key.to_str().unwrap(),
        ],
    );
    assert!(
        !clone.status.success(),
        "an unbound clone must refuse instead of labeling itself exact: {}",
        output_text(&clone)
    );
    let text = output_text(&clone);
    assert!(
        text.contains("unbound") && text.contains("import"),
        "the refusal must name the explicit foreign-synthesis path: {text}"
    );

    // No false exact label: the scaffold state stays empty (never claimed).
    let state = {
        let repo = atomic_repository::Repository::open_readonly(&clone_dir).expect("open");
        let working_copy = repo.require_working_copy_id().expect("working copy");
        let view = repo.desired_view_name(working_copy).expect("view");
        repo.get_view_info(&view).expect("info").state.to_string()
    };
    assert_eq!(
        state,
        atomic_core::types::Merkle::ZERO.to_base32(),
        "an unbound checkout must not adopt any state"
    );
}

#[test]
fn git_clone_then_init_adopt_git_bootstraps_through_the_same_anchoring() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("home");
    let publisher = temp.path().join("publisher");
    fs::create_dir_all(&publisher).expect("publisher");
    let key = write_key_file(temp.path(), "publisher-key.hex", 0x5E);
    setup_native_publisher(&publisher, &home, &key);

    let publisher_state = {
        let repo = atomic_repository::Repository::open_readonly(&publisher).expect("open");
        let working_copy = repo.require_working_copy_id().expect("working copy");
        let view = repo.desired_view_name(working_copy).expect("view");
        repo.get_view_info(&view).expect("info").state.to_string()
    };

    let remote_dir = temp.path().join("origin.git");
    make_remote_and_fetch_refspecs(&publisher, &remote_dir, "origin");
    assert_git(
        &publisher,
        &[
            "push",
            &remote_dir.to_str().unwrap(),
            "refs/heads/*:refs/heads/*",
            "refs/atomic/bindings/*:refs/atomic/bindings/*",
        ],
        "push branch + bindings",
    );

    // The second entry path: plain `git clone`, then `atomic init --adopt-git`
    // in the fresh checkout. Both paths share the CB-10B bootstrap module.
    let clone_dir = temp.path().join("adopted");
    assert_git(
        temp.path(),
        &[
            "clone",
            &remote_dir.to_str().unwrap(),
            "adopted",
        ],
        "plain git clone",
    );
    let clone_key = write_key_file(temp.path(), "adopt-key.hex", 0x5A);
    let adopt = atomic(
        &clone_dir,
        &home,
        &[
            "init",
            "--no-vault",
            "--view",
            "main",
            "--adopt-git",
            "--binding-key-file",
            clone_key.to_str().unwrap(),
        ],
    );
    assert_success(&adopt, "atomic init --adopt-git");
    assert!(
        output_text(&adopt).contains("Exact restoration verified"),
        "the init --adopt-git bootstrap must verify exact restoration: {}",
        output_text(&adopt)
    );

    // The adopted checkout carries the EXACT bound Merkle state.
    let adopted_state = {
        let repo = atomic_repository::Repository::open_readonly(&clone_dir).expect("open");
        let working_copy = repo.require_working_copy_id().expect("working copy");
        let view = repo.desired_view_name(working_copy).expect("view");
        repo.get_view_info(&view).expect("info").state.to_string()
    };
    assert_eq!(
        adopted_state, publisher_state,
        "exact resurrection must restore the identical Merkle state"
    );
}

#[test]
fn tampered_fetched_binding_is_refused_before_installation() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("home");
    let publisher = temp.path().join("publisher");
    fs::create_dir_all(&publisher).expect("publisher");
    let key = write_key_file(temp.path(), "publisher-key.hex", 0x5B);
    setup_native_publisher(&publisher, &home, &key);

    let remote_dir = temp.path().join("origin.git");
    make_remote_and_fetch_refspecs(&publisher, &remote_dir, "origin");
    assert_git(
        &publisher,
        &[
            "push",
            &remote_dir.to_str().unwrap(),
            "refs/heads/*:refs/heads/*",
            "refs/atomic/bindings/*:refs/atomic/bindings/*",
        ],
        "push branch + bindings",
    );

    // Hostile host: rebuild the binding commit with tampered binding bytes
    // under the same ref name (the signed bytes no longer match the blob).
    let binding_ref = git_text(&publisher, &["for-each-ref", "--format=%(refname)", "refs/atomic/bindings"])
        .lines()
        .next()
        .expect("one binding ref")
        .trim()
        .to_string();
    {
        let git_repo = git2::Repository::open_bare(&remote_dir).expect("open remote");
        let signature = git2::Signature::now("Attacker", "attacker@example.com").expect("signature");
        let mut builder = git_repo.treebuilder(None).expect("treebuilder");
        let tampered = b"tampered binding bytes that never verify";
        let blob = git_repo.blob(tampered).expect("blob");
        builder.insert("binding.cbor", blob, i32::from(git2::FileMode::Blob)).expect("insert");
        let tree_id = builder.write().expect("write tree");
        let tree = git_repo.find_tree(tree_id).expect("find tree");
        let commit = git_repo
            .commit(None, &signature, &signature, "tampered binding", &tree, &[])
            .expect("create tampered binding commit");
        git_repo
            .reference(&binding_ref, commit, true, "tampered binding")
            .expect("force the tampered ref");
    }

    // Plain git clone + init --adopt-git: the installer must refuse the
    // tampered binding BEFORE anything is stored, and the bootstrap must
    // refuse rather than label the checkout exact.
    let clone_dir = temp.path().join("victim");
    assert_git(
        temp.path(),
        &["clone", &remote_dir.to_str().unwrap(), "victim"],
        "git clone with the tampered ref",
    );
    // The clone fetched the binding ref because git's default refspec does
    // not include custom namespaces; configure + fetch explicitly like the
    // bootstrap does.
    assert_git(&clone_dir, &["config", "--add", "remote.origin.fetch", "+refs/atomic/bindings/*:refs/atomic/bindings/*"], "refspec");
    assert_git(&clone_dir, &["fetch", "origin"], "fetch tampered binding");

    let clone_key = write_key_file(temp.path(), "victim-key.hex", 0x5C);
    let adopt = atomic(
        &clone_dir,
        &home,
        &[
            "init",
            "--no-vault",
            "--view",
            "main",
            "--adopt-git",
            "--binding-key-file",
            clone_key.to_str().unwrap(),
        ],
    );
    assert!(
        !adopt.status.success(),
        "a tampered fetched binding must abort the bootstrap: {}",
        output_text(&adopt)
    );
    let text = output_text(&adopt);
    assert!(
        text.contains("refused before installation"),
        "the refusal must name the fail-closed install boundary: {text}"
    );
    // Nothing was installed: the victim holds no bindings.
    {
        let repo = atomic_repository::Repository::open_readonly(&clone_dir).expect("open");
        let ids = repo.binding_ids().expect("ids");
        assert!(
            ids.is_empty(),
            "no tampered binding may reach the immutable store"
        );
    }
}

// ── AC-3: namespace rejection, degraded opt-in, privacy ─────────────────

#[test]
fn namespace_rejection_is_explicit_and_degraded_fallback_needs_opt_in() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("home");
    let publisher = temp.path().join("publisher");
    fs::create_dir_all(&publisher).expect("publisher");
    let key = write_key_file(temp.path(), "key.hex", 0x58);
    setup_import_publisher(&publisher, &home);
    assert_success(
        &atomic(
            &publisher,
            &home,
            &[
                "git",
                "bridge",
                "binding",
                "publish",
                "--key-file",
                key.to_str().unwrap(),
            ],
        ),
        "binding publish",
    );

    // A host that rejects custom namespaces: a pre-receive hook refusing
    // every refs/atomic/* update.
    let remote_dir = temp.path().join("hostile-host.git");
    Command::new("git")
        .args(["init", "--bare", "-q", remote_dir.to_str().unwrap()])
        .output()
        .expect("init bare remote");
    let hook = remote_dir.join("hooks").join("pre-receive");
    fs::write(
        &hook,
        "#!/bin/sh\nwhile read -r _old _new ref; do\n  case \"$ref\" in\n    refs/atomic/*) echo 'custom namespaces rejected' >&2; exit 1;;\n  esac\ndone\nexit 0\n",
    )
    .expect("write hook");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    assert_git(&publisher, &["remote", "add", "origin", remote_dir.to_str().unwrap()], "add remote");

    // Without the opt-in: explicit diagnostic, NO degraded publication.
    let refused = atomic(&publisher, &home, &["git", "bridge", "binding", "queue", "--remote", "origin"]);
    assert!(
        !refused.status.success(),
        "a namespace-rejecting host must fail the queue: {}",
        output_text(&refused)
    );
    let text = output_text(&refused);
    assert!(
        text.contains("NOT semantically equivalent") && text.contains("Atomic remote"),
        "the diagnostic must surface both remediations: {text}"
    );
    let remote_refs = git_text(&remote_dir, &["for-each-ref", "--format=%(refname)"]);
    assert!(
        !remote_refs.contains("refs/heads/atomic/"),
        "no degraded publication without the explicit opt-in: {remote_refs}"
    );

    // With the explicit opt-in: DEGRADED branch-namespace publication,
    // visibly labeled.
    let degraded = atomic(
        &publisher,
        &home,
        &[
            "git",
            "bridge",
            "binding",
            "queue",
            "--remote",
            "origin",
            "--degraded-head-fallback",
        ],
    );
    assert_success(&degraded, "degraded fallback opt-in");
    let text = output_text(&degraded);
    assert!(
        text.contains("DEGRADED") && text.contains("NOT semantically equivalent"),
        "the degraded publication must be visibly labeled: {text}"
    );
    let remote_refs = git_text(&remote_dir, &["for-each-ref", "--format=%(refname)"]);
    assert!(
        remote_refs.contains("refs/heads/atomic/bindings/"),
        "the degraded namespace must exist on the remote: {remote_refs}"
    );
    // The degraded fallback never writes the real binding namespace either
    // (the host rejects it) and never fabricates semantic equivalence.
    assert!(
        !remote_refs.contains("refs/atomic/bindings/"),
        "the rejecting host must not hold the real namespace: {remote_refs}"
    );
}

#[test]
fn binding_trees_hold_only_allowlisted_public_blobs() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("home");
    let publisher = temp.path().join("publisher");
    fs::create_dir_all(&publisher).expect("publisher");
    let key = write_key_file(temp.path(), "key.hex", 0x59);
    setup_native_publisher(&publisher, &home, &key);

    let refs = git_text(&publisher, &["for-each-ref", "--format=%(refname)", "refs/atomic/bindings"]);
    assert!(!refs.trim().is_empty(), "a binding ref must exist");
    for reference in refs.lines() {
        let listing = git_text(&publisher, &["ls-tree", "-r", reference]);
        for line in listing.lines() {
            // <mode> <type> <oid>\t<name>
            let name = line.split('\t').nth(1).expect("ls-tree name");
            assert!(
                matches!(
                    name,
                    "binding.cbor" | "attestation-summary.cbor" | "changes.pack" | "conflicts.pack"
                ),
                "binding trees hold only the public blobs, found '{name}' in {line}"
            );
        }
    }
}

// ── Review R1 regression: carrier validation before any network write ──

/// A binding ref whose carrier is NOT the exact published commit — here a
/// replacement carrier with the unchanged valid binding tree but a PRIVATE
/// WIP parent — must refuse the queue and transfer nothing. The namespace
/// allowlist is not a privacy boundary for Git object traversal: every
/// object reachable from the pushed ref would travel, including the private
/// sentinel inside the WIP parent chain.
#[test]
fn hostile_carrier_with_private_wip_parent_never_transfers() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("home");
    let publisher = temp.path().join("publisher");
    fs::create_dir_all(&publisher).expect("publisher");
    let key = write_key_file(temp.path(), "key.hex", 0x61);
    setup_import_publisher(&publisher, &home);
    assert_success(
        &atomic(
            &publisher,
            &home,
            &[
                "git",
                "bridge",
                "binding",
                "publish",
                "--key-file",
                key.to_str().unwrap(),
            ],
        ),
        "binding publish",
    );

    // Control: the unchanged, legit carrier still transfers and verifies.
    let origin_dir = temp.path().join("origin.git");
    make_remote_and_fetch_refspecs(&publisher, &origin_dir, "origin");
    let control = atomic(&publisher, &home, &["git", "bridge", "binding", "queue", "--remote", "origin"]);
    assert_success(&control, "unchanged-carrier control queue --remote origin");
    assert!(
        output_text(&control).contains("Verified"),
        "the legit carrier must still verify: {}",
        output_text(&control)
    );
    let binding_ref = git_text(&publisher, &["for-each-ref", "--format=%(refname)", "refs/atomic/bindings"])
        .lines()
        .next()
        .expect("one binding ref")
        .trim()
        .to_string();
    let legit_target = git_text(&publisher, &["rev-parse", &binding_ref]).trim().to_string();

    // Build the hostile carrier: the SAME valid binding tree, but a parent
    // chain holding PRIVATE_REVIEW_SENTINEL, itself held under a local WIP
    // ref (WIP refs never transfer — their reachable objects must not either).
    let private_blob;
    let hostile_carrier;
    {
        let git_repo = git2::Repository::open(&publisher).expect("open publisher");
        let signature = git2::Signature::now("Test", "test@example.com").expect("signature");
        let carrier = git_repo
            .find_commit(git2::Oid::from_str(&legit_target).expect("legit carrier oid"))
            .expect("legit carrier commit");
        let binding_tree = carrier.tree_id();
        let blob = git_repo.blob(b"PRIVATE_REVIEW_SENTINEL\n").expect("private blob");
        let mut builder = git_repo.treebuilder(None).expect("treebuilder");
        builder
            .insert("private.txt", blob, git2::FileMode::Blob.into())
            .expect("insert private entry");
        let private_tree = builder.write().expect("private tree");
        let private_commit = git_repo
            .commit(
                None,
                &signature,
                &signature,
                "private WIP",
                &git_repo.find_tree(private_tree).expect("private tree object"),
                &[],
            )
            .expect("private WIP commit");
        let wip_ref = git_repo
            .reference(
                "refs/atomic/wip/review/private",
                private_commit,
                false,
                "local WIP recovery ref",
            )
            .expect("wip ref");
        assert!(wip_ref.target().is_some(), "wip ref is direct");
        hostile_carrier = git_repo
            .commit(
                None,
                &signature,
                &signature,
                "same binding with private parent",
                &git_repo.find_tree(binding_tree).expect("binding tree object"),
                &[&git_repo.find_commit(private_commit).expect("private commit")],
            )
            .expect("hostile carrier commit");
        git_repo
            .reference(&binding_ref, hostile_carrier, true, "hostile carrier swap")
            .expect("replace local binding ref with hostile carrier");
        private_blob = blob;
    }

    // The queue must refuse the hostile carrier against a fresh remote:
    // nonzero, no publication, and NOTHING reaches the remote.
    let leak_dir = temp.path().join("leak.git");
    Command::new("git")
        .args(["init", "--bare", "-q", leak_dir.to_str().unwrap()])
        .output()
        .expect("init bare leak remote");
    assert_git(&publisher, &["remote", "add", "leak", leak_dir.to_str().unwrap()], "add leak remote");
    let refused = atomic(&publisher, &home, &["git", "bridge", "binding", "queue", "--remote", "leak"]);
    assert!(
        !refused.status.success(),
        "the queue must refuse a carrier with unexpected parentage: {}",
        output_text(&refused)
    );
    let refused_text = output_text(&refused);
    assert!(
        refused_text.contains("verifiable carrier") || refused_text.contains("parent"),
        "the refusal must name the carrier defect: {refused_text}"
    );
    let remote_refs = git_text(&leak_dir, &["for-each-ref", "--format=%(refname)"]);
    assert!(
        remote_refs.trim().is_empty(),
        "nothing may transfer from a hostile carrier: {remote_refs}"
    );
    {
        let leak = git2::Repository::open_bare(&leak_dir).expect("open leak remote");
        let odb = leak.odb().expect("leak odb");
        assert!(
            !odb.exists(private_blob),
            "the private sentinel blob must never reach the remote"
        );
    }

    // A hostile local carrier also refuses against the remote that already
    // holds the legit carrier, and that remote stays untouched.
    let retry = atomic(&publisher, &home, &["git", "bridge", "binding", "queue", "--remote", "origin"]);
    assert!(
        !retry.status.success(),
        "a hostile local carrier must refuse even against a synchronized remote: {}",
        output_text(&retry)
    );
    assert_eq!(
        git_text(&origin_dir, &["rev-parse", &binding_ref]).trim(),
        legit_target,
        "the legit remote carrier must be preserved untouched"
    );
}

/// A valid-shaped binding ref with NO stored binding is unregistered: its
/// reachable object surface was never validated, so it must not transfer.
#[test]
fn unregistered_binding_ref_is_never_transferred() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("home");
    let publisher = temp.path().join("publisher");
    fs::create_dir_all(&publisher).expect("publisher");
    let key = write_key_file(temp.path(), "key.hex", 0x62);
    setup_import_publisher(&publisher, &home);
    assert_success(
        &atomic(
            &publisher,
            &home,
            &[
                "git",
                "bridge",
                "binding",
                "publish",
                "--key-file",
                key.to_str().unwrap(),
            ],
        ),
        "binding publish",
    );

    // An unregistered, valid-shaped binding ref holding a private blob.
    let unregistered = "refs/atomic/bindings/ff/ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
    let unregistered_blob;
    {
        let git_repo = git2::Repository::open(&publisher).expect("open publisher");
        let signature = git2::Signature::now("Test", "test@example.com").expect("signature");
        let blob = git_repo.blob(b"UNREGISTERED_PRIVATE_SENTINEL\n").expect("blob");
        let mut builder = git_repo.treebuilder(None).unwrap();
        builder
            .insert("leak.txt", blob, git2::FileMode::Blob.into())
            .expect("insert");
        let tree = builder.write().expect("tree");
        let commit = git_repo
            .commit(
                None,
                &signature,
                &signature,
                "unregistered",
                &git_repo.find_tree(tree).expect("tree object"),
                &[],
            )
            .expect("commit");
        git_repo
            .reference(unregistered, commit, false, "unregistered ref")
            .expect("create unregistered ref");
        unregistered_blob = blob;
    }

    let remote_dir = temp.path().join("origin.git");
    make_remote_and_fetch_refspecs(&publisher, &remote_dir, "origin");
    let refused = atomic(&publisher, &home, &["git", "bridge", "binding", "queue", "--remote", "origin"]);
    assert!(
        !refused.status.success(),
        "an unregistered binding ref must refuse the transfer: {}",
        output_text(&refused)
    );
    assert!(
        output_text(&refused).contains("not backed by a stored binding"),
        "the refusal must name the missing stored binding: {}",
        output_text(&refused)
    );
    let remote_refs = git_text(&remote_dir, &["for-each-ref", "--format=%(refname)"]);
    assert!(
        remote_refs.trim().is_empty(),
        "nothing may transfer while any enumerated ref is unregistered: {remote_refs}"
    );
    {
        let remote = git2::Repository::open_bare(&remote_dir).expect("open remote");
        let odb = remote.odb().expect("remote odb");
        assert!(
            !odb.exists(unregistered_blob),
            "the unregistered ref's private blob must never reach the remote"
        );
    }
}

// ── CB-10B follow-up (::18) regressions ───────────────────────────────────

/// R2: the ahead-remote overwrite probe. A remote carrying unobserved work
/// (moved ahead before any observation was stored) is never granted force
/// authority: the first observation must NOT become a
/// `--force-with-lease=<ref>:<observed-tip>` lease, which would permit
/// overwriting exactly that work. The push refuses (non-fast-forward) and
/// the remote refs are unchanged.
#[test]
fn ahead_unobserved_remote_is_never_granted_force_authority() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("home");
    let publisher = temp.path().join("publisher");
    fs::create_dir_all(&publisher).expect("publisher");
    setup_import_publisher(&publisher, &home);

    let remote_dir = temp.path().join("origin.git");
    make_remote_and_fetch_refspecs(&publisher, &remote_dir, "origin");

    // External work lands on the remote BEFORE the publisher ever observes
    // it: the remote ref is ahead of anything the publisher knows.
    let ahead = {
        let git_repo = git2::Repository::open_bare(&remote_dir).expect("open remote");
        let signature = git2::Signature::now("Remote", "remote@example.com").expect("signature");
        let tree_id = git_repo
            .treebuilder(None)
            .expect("treebuilder")
            .write()
            .expect("write tree");
        let tree = git_repo.find_tree(tree_id).expect("find tree");
        let commit = git_repo
            .commit(None, &signature, &signature, "ahead remote work", &tree, &[])
            .expect("create the ahead commit");
        git_repo
            .reference("refs/heads/pub", commit, true, "ahead remote work")
            .expect("point the remote ref at the ahead commit");
        commit
    };

    // The push must refuse: the remote's work is not ours to overwrite, and
    // the first observation must not mint a force lease over it.
    let push = atomic(&publisher, &home, &["git", "push", "--branch", "pub"]);
    assert!(
        !push.status.success(),
        "an ahead unobserved remote must refuse the push: {}",
        output_text(&push)
    );
    let now = git_text(&remote_dir, &["rev-parse", "refs/heads/pub"]);
    assert_eq!(
        now.trim(),
        ahead.to_string(),
        "the ahead remote work must remain unchanged"
    );
}

/// R3: the push publishes the PINNED verified commit — never the mutable
/// HEAD. The branch-source race fixture moves the local branch after the
/// publication snapshot (injected fault) and the pushed commit must still be
/// the exact verified one.
#[cfg(feature = "adoption-test-injection")]
#[cfg(feature = "adoption-test-injection")]
#[test]
fn push_publishes_the_pinned_verified_oid_not_mutable_head() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("home");
    let publisher = temp.path().join("publisher");
    fs::create_dir_all(&publisher).expect("publisher");
    setup_import_publisher(&publisher, &home);

    let remote_dir = temp.path().join("origin.git");
    make_remote_and_fetch_refspecs(&publisher, &remote_dir, "origin");

    // Push 1 establishes the published state at the verified commit.
    let push = atomic(&publisher, &home, &["git", "push", "--branch", "pub"]);
    assert_success(&push, "baseline push");
    let verified = git_text(&publisher, &["rev-parse", "HEAD"]).trim().to_string();

    // New clean local work advances the branch through the managed record
    // flow (the record projects the verified publication and moves the
    // branch + checkpoint together).
    fs::write(publisher.join("advance.txt"), b"advance\n").expect("advance file");
    assert_success(&atomic(&publisher, &home, &["add", "advance.txt"]), "atomic add");
    assert_success(
        &atomic(&publisher, &home, &["record", "-m", "advance work"]),
        "atomic record",
    );
    let advanced = git_text(&publisher, &["rev-parse", "HEAD"]).trim().to_string();
    assert_ne!(
        advanced, verified,
        "the managed record must advance the branch"
    );

    // The race: an injected concurrent writer moves the branch AFTER the
    // publication snapshot. The push must publish the PINNED verified
    // commit — never the moved mutable HEAD.
    let push = Command::new(ATOMIC_BIN)
        .args(["git", "push", "--branch", "pub"])
        .current_dir(&publisher)
        .env("HOME", &home)
        .env("ATOMIC_HOME", home.join(".atomic"))
        .env("ATOMIC_FAIL_PUSH_MOVE_BRANCH_AFTER_VERIFY", "1")
        .env("GIT_AUTHOR_NAME", "CB-10B Tests")
        .env("GIT_AUTHOR_EMAIL", "cb10b@example.com")
        .env("GIT_COMMITTER_NAME", "CB-10B Tests")
        .env("GIT_COMMITTER_EMAIL", "cb10b@example.com")
        .output()
        .expect("run the raced push");
    assert_success(&push, "the raced push");

    // The remote holds exactly the verified commit; the raced move (HEAD)
    // was never published.
    let pushed = git_text(&remote_dir, &["rev-parse", "refs/heads/pub"]).trim().to_string();
    assert_eq!(
        pushed, advanced,
        "the push must publish the verified branch tip, not the raced mutable HEAD"
    );
    assert_ne!(
        pushed, verified,
        "the new work must be published (the pinned refspec carries the newer verified commit)"
    );
}
// ── CB-10B follow-up (::18) — bootstrap / clone regressions ──────────────

/// R11: local Git URL detection checks the SOURCE. An existing local Git
/// repository source takes the Git-transport bootstrap even though the
/// destination does not exist yet (the former implementation checked the
/// destination and mis-routed local Git sources to the Atomic-API path).
#[test]
fn local_git_source_routes_to_the_git_transport_bootstrap() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("home");
    let source = temp.path().join("local-git-source");
    fs::create_dir_all(&source).expect("source dir");
    assert!(
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(&source)
            .output()
            .expect("git init")
            .status
            .success()
    );
    fs::write(source.join("seed.txt"), b"local git source\n").expect("seed");
    assert!(
        Command::new("git")
            .args(["add", "-A"])
            .current_dir(&source)
            .output()
            .expect("git add")
            .status
            .success()
    );
    assert!(
        Command::new("git")
            .args(["-c", "user.email=t@t", "-c", "user.name=T", "commit", "-qm", "seed"])
            .current_dir(&source)
            .output()
            .expect("git commit")
            .status
            .success()
    );

    // The destination does not exist yet; the SOURCE is a Git repository.
    let destination = temp.path().join("fresh-clone");
    assert!(!destination.exists());
    // The Git bootstrap RUNS (Atomic initialized in the destination, binding
    // fetch attempted) and refuses the unbound history with the typed
    // foreign-synthesis guidance — the old destination-based detection would
    // have mis-routed the local Git source to the Atomic-API path instead.
    let clone = atomic(
        &temp.path(),
        &home,
        &[
            "clone",
            source.to_str().expect("utf8 source"),
            destination.to_str().expect("utf8 destination"),
        ],
    );
    let text = output_text(&clone);
    assert!(
        text.contains("no verified binding covers this commit"),
        "a local Git source must take the Git-transport bootstrap (the unbound refusal \
         names the binding path), got:\n{text}"
    );
    assert!(
        destination.join(".atomic").exists(),
        "the clone initialized Atomic via the Git bootstrap"
    );
}

/// R4: the bootstrap's sequence/dirty preflight refuses an ACTIVE MERGE and
/// a DIRTY tracked worktree with typed evidence, before any fetch or
/// resurrection (`atomic init --adopt-git` drives the shared bootstrap).
#[test]
fn bootstrap_preflight_refuses_active_merge_and_dirty_worktree() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("home");
    let key_file = write_key_file(temp.path(), "preflight.key", 3);

    // Case 1: dirty tracked worktree.
    let checkout = temp.path().join("dirty-checkout");
    fs::create_dir_all(&checkout).expect("checkout");
    assert!(
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(&checkout)
            .output()
            .expect("git init")
            .status
            .success()
    );
    fs::write(checkout.join("tracked.txt"), b"base\n").expect("seed");
    assert!(
        Command::new("git")
            .args(["add", "-A"])
            .current_dir(&checkout)
            .output()
            .expect("add")
            .status
            .success()
    );
    assert!(
        Command::new("git")
            .args(["-c", "user.email=t@t", "-c", "user.name=T", "commit", "-qm", "base"])
            .current_dir(&checkout)
            .output()
            .expect("commit")
            .status
            .success()
    );
    fs::write(checkout.join("tracked.txt"), b"uncommitted local edit\n").expect("dirty");
    let bootstrap = atomic(
        &checkout,
        &home,
        &[
            "init",
            "--adopt-git",
            "--binding-key-file",
            key_file.to_str().expect("utf8 key"),
        ],
    );
    let text = output_text(&bootstrap);
    assert!(
        !bootstrap.status.success(),
        "a dirty tracked worktree must refuse the bootstrap:\n{text}"
    );
    assert!(
        text.contains("uncommitted tracked state"),
        "the refusal must name the dirty worktree, got:\n{text}"
    );

    // Case 2: an active merge (MERGE_HEAD present) refuses.
    let merged = temp.path().join("merge-checkout");
    fs::create_dir_all(&merged).expect("checkout");
    assert!(
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(&merged)
            .output()
            .expect("git init")
            .status
            .success()
    );
    fs::write(merged.join("tracked.txt"), b"base\n").expect("seed");
    assert!(
        Command::new("git")
            .args(["add", "-A"])
            .current_dir(&merged)
            .output()
            .expect("add")
            .status
            .success()
    );
    assert!(
        Command::new("git")
            .args(["-c", "user.email=t@t", "-c", "user.name=T", "commit", "-qm", "base"])
            .current_dir(&merged)
            .output()
            .expect("commit")
            .status
            .success()
    );
    fs::write(
        merged.join(".git/MERGE_HEAD"),
        b"0123456789abcdef0123456789abcdef01234567\n",
    )
    .expect("forge MERGE_HEAD");
    let bootstrap = atomic(
        &merged,
        &home,
        &[
            "init",
            "--adopt-git",
            "--binding-key-file",
            key_file.to_str().expect("utf8 key"),
        ],
    );
    let text = output_text(&bootstrap);
    assert!(
        text.contains("merge is in progress"),
        "an active merge must refuse the bootstrap with typed evidence, got:\n{text}"
    );
}

/// R12: an oversized pack inside a binding tree is refused BEFORE the bulk
/// copy. The verifier walks the carrier tree and checks each pack blob's
/// SIZE against the transport budget during the walk — an oversized pack is
/// never copied into memory, and the refusal names the oversize (the former
/// copy-then-limit order refused at the binding decode instead).
#[test]
fn oversized_pack_is_refused_before_the_bulk_copy() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("home");
    let repo_root = temp.path().join("carrier-repo");
    fs::create_dir_all(&repo_root).expect("repo dir");
    let git = git2::Repository::init(&repo_root).expect("git init");

    // The oversized pack blob (beyond the 64 MiB default transport budget).
    let oversized = vec![0u8; 64 * 1024 * 1024 + 1];
    let odb = git.odb().expect("odb");
    let oversized_oid = odb.write(git2::ObjectType::Blob, oversized.as_slice()).expect("write pack blob");
    let binding_oid = odb.write(git2::ObjectType::Blob, b"not-a-real-binding" as &[u8]).expect("write binding blob");

    let mut builder = git.treebuilder(None).expect("treebuilder");
    builder
        .insert("binding.cbor", binding_oid, i32::from(git2::FileMode::Blob))
        .expect("insert binding entry");
    builder
        .insert("changes.pack", oversized_oid, i32::from(git2::FileMode::Blob))
        .expect("insert oversized pack entry");
    let tree_id = builder.write().expect("write tree");

    let signature = git2::Signature::now("T", "t@example.com").expect("signature");
    let tree = git.find_tree(tree_id).expect("tree");
    let commit = git
        .commit(None, &signature, &signature, "carrier", &tree, &[])
        .expect("carrier commit");

    // The ref name carries the canonical shard/id layout (id 64 lowercase
    // hex); the binding blob is garbage — the OVERSIZE refusal must win
    // because the size check runs during the tree walk, before any decode.
    let id_hex = "a".repeat(64);
    let refname = format!("refs/atomic/bindings/aa/{}", id_hex);
    git.reference(&refname, commit, true, "oversize probe").expect("binding ref");

    let repo = atomic_repository::Repository::init(&repo_root).expect("init atomic over the carrier repo");
    let id = atomic_repository::git_binding::BindingId::from_bytes([0xaa; 32]);
    let error = repo
        .verify_published_binding_carrier(&git, &id)
        .expect_err("the oversized pack must refuse");
    let message = error.to_string();
    assert!(
        message.contains("oversized"),
        "the refusal must be the oversize check that runs before the copy, got: {message}"
    );
}
