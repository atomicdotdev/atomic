//! CB-13B integration coverage: the reachable CLI cutover entry point.
//!
//! The cutover executor's repository-level gates have their own suite
//! (`cutover_tests`); this file proves the CLI command that owns it end to
//! end against a real colocated repository:
//!
//! - the explicit opt-in consent is a precondition (the cutover never
//!   enables implicitly);
//! - the readiness refusals surface verbatim through the CLI — an active
//!   hook dispatcher refuses (this build migrates no hooks);
//! - a fully ready repository cuts over with a journaled, verified
//!   `Cutover` operation, after which the legacy shadow writer refuses
//!   through the real CLI paths;
//! - re-execution is an idempotent already-fenced no-op;
//! - `--rollback` rolls the verified cutover back through the journal's
//!   typed inverse and the legacy writer works again.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

const ATOMIC_BIN: &str = env!("CARGO_BIN_EXE_atomic");

fn atomic(root: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(ATOMIC_BIN)
        .args(args)
        .current_dir(root)
        .env("HOME", home)
        .env("ATOMIC_HOME", home.join(".atomic"))
        .env("GIT_AUTHOR_DATE", "@1726732800 +0000")
        .env("GIT_COMMITTER_DATE", "@1726732800 +0000")
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

fn key_file(home: &Path) -> std::path::PathBuf {
    let mut secret = [0u8; 32];
    for (index, byte) in secret.iter_mut().enumerate() {
        *byte = 0x42u8.wrapping_add((index as u8).wrapping_mul(11).wrapping_add(5));
    }
    let hex: String = secret.iter().map(|byte| format!("{byte:02x}")).collect();
    let path = home.join("cb13b-key.hex");
    fs::write(&path, hex).expect("write key file");
    path
}

#[allow(dead_code)]
fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_AUTHOR_DATE", "@1726732800 +0000")
        .env("GIT_COMMITTER_DATE", "@1726732800 +0000")
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A colocated Git + Atomic repository with an active shadow sync and the
/// recorded explicit opt-in — the state `atomic git bridge cutover`
/// consumes.
struct Colocated {
    root: TempDir,
    home: TempDir,
}

impl Colocated {
    fn new() -> Self {
        let root = TempDir::new().expect("repo tempdir");
        let home = TempDir::new().expect("home tempdir");
        assert!(git_ok(&root, &["init", "-q", "-b", "main"]));
        fs::write(root.path().join("tracked.txt"), b"anchor me\n").expect("write file");
        assert!(git_ok(&root, &["add", "tracked.txt"]));
        assert!(git_ok(&root, &["commit", "-qm", "anchor base"]));
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
}

fn git_ok(root: &TempDir, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root.path())
        .args(args)
        .env("GIT_AUTHOR_NAME", "CB-13B Tests")
        .env("GIT_AUTHOR_EMAIL", "cb13b@example.invalid")
        .env("GIT_COMMITTER_NAME", "CB-13B Tests")
        .env("GIT_COMMITTER_EMAIL", "cb13b@example.invalid")
        .env("GIT_AUTHOR_DATE", "@1726732800 +0000")
        .env("GIT_COMMITTER_DATE", "@1726732800 +0000")
        .output()
        .expect("run git")
        .status
        .success()
}

#[test]
fn cutover_requires_the_explicit_opt_in_consent() {
    let root = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    assert!(git_ok(&root, &["init", "-q", "-b", "main"]));
    fs::write(root.path().join("tracked.txt"), b"anchor me\n").unwrap();
    assert!(git_ok(&root, &["add", "tracked.txt"]));
    assert!(git_ok(&root, &["commit", "-qm", "anchor base"]));
    atomic_ok(root.path(), home.path(), &["init", "--no-vault"]);

    let output = atomic_fail(root.path(), home.path(), &["git", "bridge", "cutover"]);
    assert!(
        output.contains("explicit opt-in consent"),
        "the consent precondition must refuse: {output}"
    );
}

#[test]
fn cutover_refuses_foreign_hook_surfaces_through_the_cli() {
    // A FOREIGN hook (not Atomic-owned) refuses: the readiness refusal
    // must reach the CLI instead of fencing partially. Atomic-owned
    // dispatchers, by contrast, are decommissioned by the cutover itself
    // (journaled migration-effect leases).
    let colocated = Colocated::new();
    let hook = colocated.root().join(".git/hooks/pre-commit");
    fs::create_dir_all(hook.parent().unwrap()).unwrap();
    fs::write(&hook, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let output = colocated.atomic_fails(&["git", "bridge", "cutover"]);
    assert!(
        output.contains("hook"),
        "the foreign hook readiness refusal must reach the CLI: {output}"
    );
}

#[test]
fn cutover_cli_entry_is_reachable_end_to_end() {
    let colocated = Colocated::new();
    let dispatcher = colocated.root().join(".git/hooks/post-checkout");
    assert!(
        dispatcher.exists(),
        "the fixture's bridge enable installs the advisory dispatcher"
    );

    // The cutover lands: journaled, verified, fenced — and it
    // decommissions the Atomic-owned advisory dispatchers through its
    // journaled migration-effect leases (no manual decommission step).
    let output = colocated.atomic(&["git", "bridge", "cutover"]);
    assert!(
        output.contains("journaled and verified"),
        "cutover must report the journaled outcome: {output}"
    );
    assert!(
        !dispatcher.exists(),
        "the owned advisory dispatcher is decommissioned by the cutover"
    );

    // The fence is real through the CLI: a recorded change's shadow
    // projection takes the legacy writer lock and refuses with the typed
    // fenced error. The record itself is durable; only the projection
    // refuses (the colocated bridge owns writes now).
    fs::write(colocated.root().join("tracked.txt"), b"after cutover\n").unwrap();
    colocated.atomic(&["add", "tracked.txt"]);
    let output = colocated.atomic_fails(&["record", "-m", "post cutover"]);
    assert!(
        output.contains("git-bridge-cutover"),
        "the legacy projection must refuse after the cutover: {output}"
    );

    // Re-execution is an idempotent already-fenced no-op.
    let output = colocated.atomic(&["git", "bridge", "cutover"]);
    assert!(
        output.contains("already cut over"),
        "re-execution must be a no-op: {output}"
    );

    // Rollback after later activity refuses: the shared repository head
    // moved past the cutover (the durable record advanced it), and the
    // typed inverse never undoes out of order.
    let output = colocated.atomic_fails(&["git", "bridge", "cutover", "--rollback"]);
    assert!(
        output.contains("not the cutover"),
        "rollback must refuse a moved shared head: {output}"
    );
}

#[test]
fn cutover_rollback_lifts_the_fence_and_restores_the_legacy_writer() {
    let colocated = Colocated::new();
    let dispatcher = colocated.root().join(".git/hooks/post-checkout");
    let original = fs::read(&dispatcher).expect("the fixture installed the dispatcher");
    colocated.atomic(&["git", "bridge", "cutover"]);
    assert!(
        !dispatcher.exists(),
        "the cutover decommissioned the dispatcher"
    );

    // Immediately after the cutover (no later operation), the rollback
    // runs: the shared head still denotes the cutover, and the leased
    // hook decommission restores the dispatcher byte-for-byte.
    let output = colocated.atomic(&["git", "bridge", "cutover", "--rollback"]);
    assert!(
        output.contains("rolled back"),
        "rollback must report the inverse: {output}"
    );
    let restored = fs::read(&dispatcher).expect("the dispatcher is restored by the rollback");
    assert_eq!(restored, original, "the dispatcher restores byte-for-byte");

    // The legacy writer works again: the pending record projects.
    fs::write(colocated.root().join("tracked.txt"), b"after rollback\n").unwrap();
    colocated.atomic(&["add", "tracked.txt"]);
    colocated.atomic(&["record", "-m", "after rollback"]);
}
