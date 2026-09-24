//! A remote sandbox talks to its repository's database owner over iroh —
//! the same protocol, with every request checked against its token.
//!
//! Runs offline: the owner and the sandbox dial each other directly on
//! loopback (`ATOMIC_OWNER_IROH_OFFLINE`), no relays.

use std::path::Path;
use std::process::{Command, Output};

use atomic_core::change::ChangeHeader;
use atomic_repository::{InsertOptions, RecordOptions, Repository, TrackingOptions};
use serde_json::Value;
use tempfile::TempDir;

fn atomic(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_atomic"))
        .args(args)
        .arg("--no-color")
        .current_dir(cwd)
        .env("ATOMIC_OWNER_IROH_OFFLINE", "1")
        .output()
        .expect("run atomic")
}

fn ok(output: Output, what: &str) -> String {
    assert!(
        output.status.success(),
        "{what} failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn failure(output: Output, what: &str) -> String {
    assert!(!output.status.success(), "{what} should have failed");
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// A repository with two recorded files on `dev`.
fn repository(root: &Path) {
    let repo = Repository::init(root).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("README.md"), "hello\n").unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    repo.add("README.md", TrackingOptions::default()).unwrap();
    repo.add("src/lib.rs", TrackingOptions::default()).unwrap();
    let options = RecordOptions::new()
        .with_all(true)
        .save_to_store(true)
        .apply_after_record(false);
    let outcome = repo.record(ChangeHeader::new("first"), options).unwrap();
    repo.write_recorded(&outcome, InsertOptions::default())
        .unwrap();
}

struct Owner<'a>(&'a Path);

impl Drop for Owner<'_> {
    fn drop(&mut self) {
        let _ = atomic(
            self.0,
            &["agent", "database-owner", "shutdown", "--repository", "."],
        );
    }
}

fn reserve(cwd: &Path, session: &str, turn: &str) -> Output {
    atomic(
        cwd,
        &[
            "agent",
            "database-owner",
            "reserve",
            "--repository",
            ".",
            "--session-id",
            session,
            "--turn",
            turn,
            "--now",
            "1700000000",
            "--json",
        ],
    )
}

#[test]
fn a_remote_sandbox_reaches_its_view_through_the_owner_and_nothing_else() {
    let host = TempDir::new().unwrap();
    repository(host.path());
    let _owner = Owner(host.path());
    let vm = TempDir::new().unwrap();
    let vm_dir = vm.path().join("work");

    let out = ok(
        atomic(
            host.path(),
            &[
                "sandbox",
                "create",
                "exp-1",
                "--remote",
                "--view",
                "dev",
                "--acting-as",
                "did:key:zAgent",
                "--dest",
                vm_dir.to_str().unwrap(),
            ],
        ),
        "sandbox create --remote",
    );
    assert!(out.contains("Remote sandbox 'exp-1' created"), "{out}");
    let pointer_path = vm_dir.join(".atomic-sandbox");
    let pointer: Value = serde_json::from_slice(&std::fs::read(&pointer_path).unwrap()).unwrap();
    assert_eq!(pointer["view"], "dev");
    assert!(pointer["token"].as_str().unwrap().starts_with("ast_"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&pointer_path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "the pointer holds a token: owner-only");
    }

    // The view's tree, and nothing of the repository.
    let out = ok(atomic(&vm_dir, &["sandbox", "materialize"]), "materialize");
    assert!(out.contains("Materialized 3 entries"), "{out}");
    assert_eq!(
        std::fs::read_to_string(vm_dir.join("README.md")).unwrap(),
        "hello\n"
    );
    assert_eq!(
        std::fs::read_to_string(vm_dir.join("src/lib.rs")).unwrap(),
        "pub fn f() {}\n"
    );
    assert!(!vm_dir.join(".atomic").exists());

    // Provenance goes over the same protocol, bound to the sandbox's view:
    // its own sessions, never the host's.
    let reserved: Value =
        serde_json::from_str(&ok(reserve(&vm_dir, "vm-session", "1"), "remote reserve")).unwrap();
    assert_eq!(reserved["committed"], true);
    ok(reserve(host.path(), "host-session", "1"), "local reserve");
    let refused = failure(
        reserve(&vm_dir, "host-session", "2"),
        "reserving the host's session",
    );
    assert!(refused.contains("forbidden"), "{refused}");
    ok(
        reserve(&vm_dir, "vm-session", "2"),
        "its own session's next turn",
    );

    // Only a local caller administers sandboxes or the owner.
    let refused = failure(
        atomic(
            &vm_dir,
            &["agent", "database-owner", "shutdown", "--repository", "."],
        ),
        "remote shutdown",
    );
    assert!(refused.contains("forbidden"), "{refused}");

    // A tampered token reaches nothing.
    let mut forged = pointer.clone();
    forged["token"] = Value::String("ast_forged".into());
    std::fs::write(&pointer_path, serde_json::to_vec(&forged).unwrap()).unwrap();
    let refused = failure(atomic(&vm_dir, &["sandbox", "materialize"]), "forged token");
    assert!(refused.contains("unauthorized"), "{refused}");
    std::fs::write(&pointer_path, serde_json::to_vec(&pointer).unwrap()).unwrap();

    // Renewal keeps the same token; closing ends it.
    let out = ok(
        atomic(host.path(), &["sandbox", "renew", "dev", "--ttl", "600"]),
        "renew",
    );
    assert!(out.contains("now expires"), "{out}");
    ok(
        atomic(&vm_dir, &["sandbox", "materialize"]),
        "materialize after renew",
    );
    let out = ok(atomic(host.path(), &["sandbox", "close", "dev"]), "close");
    assert!(out.contains("revoked"), "{out}");
    let refused = failure(
        atomic(&vm_dir, &["sandbox", "materialize"]),
        "closed sandbox",
    );
    assert!(refused.contains("unauthorized"), "{refused}");
}
