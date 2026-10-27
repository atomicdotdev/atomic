//! Shim entry through the native workspace lifecycle, with default files retained.
#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

struct Fixture {
    root: TempDir,
    home: TempDir,
}

impl Fixture {
    fn command(&self, program: &str, args: &[&str]) -> Output {
        Command::new(program)
            .args(args)
            .current_dir(self.root.path())
            .env("HOME", self.home.path())
            .env("ATOMIC_HOME", self.home.path().join(".atomic"))
            .env("ATOMIC_SERVICE", "local")
            .env_remove("ATOMIC_RPC")
            .env_remove("ATOMIC_DAEMON_SOCKET")
            .env("ATOMIC_NONINTERACTIVE", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Shim workspace test")
            .env("GIT_AUTHOR_EMAIL", "bridge@example.invalid")
            .env("GIT_COMMITTER_NAME", "Shim workspace test")
            .env("GIT_COMMITTER_EMAIL", "bridge@example.invalid")
            .output()
            .unwrap()
    }

    fn ok(&self, program: &str, args: &[&str]) -> String {
        let output = self.command(program, args);
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.status.success(), "{args:?}: {text}");
        assert!(
            !text.contains("could not persist its interpretation closure"),
            "{text}"
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn atomic(&self, args: &[&str]) -> String {
        self.ok(env!("CARGO_BIN_EXE_atomic"), args)
    }

    fn git(&self, args: &[&str]) -> String {
        let mut isolated = vec!["-c", "core.hooksPath=/dev/null"];
        isolated.extend_from_slice(args);
        self.ok("git", &isolated)
    }

    fn new(preinit: bool) -> Self {
        let fixture = Self {
            root: TempDir::new().unwrap(),
            home: TempDir::new().unwrap(),
        };
        fixture.git(&["init", "-q", "-b", "main"]);
        fs::write(fixture.root.path().join("tracked.txt"), b"base\n").unwrap();
        fixture.git(&["add", "tracked.txt"]);
        fixture.git(&["commit", "-qm", "base"]);
        if preinit {
            fixture.atomic(&["init", "--no-vault"]);
            assert!(fixture.root.path().join(".atomicignore").exists());
        }
        fixture.atomic(&["git", "import", "--no-vault"]);
        let key = fixture.home.path().join("binding.key");
        fs::write(&key, "42".repeat(32)).unwrap();
        fixture.atomic(&[
            "git",
            "shim",
            "enable",
            "--binding-key-file",
            key.to_str().unwrap(),
        ]);
        fixture.atomic(&["git", "shim", "reconcile"]);
        fixture
    }

    fn assert_content(&self, view: &str, path: &str, expected: &[u8]) {
        // Reopen and render from canonical data, independently of the Git worktree.
        let repo = atomic_repository::Repository::open(self.root.path()).unwrap();
        assert_eq!(
            repo.get_file_content_on_view(path, view)
                .unwrap()
                .as_deref(),
            Some(expected)
        );
    }
}

fn preinit_record(native: bool) {
    let f = Fixture::new(true);
    let ignore = fs::read(f.root.path().join(".atomicignore")).unwrap();
    fs::write(f.root.path().join("tracked.txt"), b"edit\n").unwrap();
    let mut args = vec!["record", "-am", "edit"];
    if native {
        // This option uses the native command path rather than the local service.
        args.push("--allow-conflict-markers");
    }
    f.atomic(&args);
    f.assert_content("main", "tracked.txt", b"edit\n");
    f.assert_content("main", ".atomicignore", &ignore);
    assert_eq!(f.git(&["show", "HEAD:tracked.txt"]), "edit\n");
    assert!(f.atomic(&["status", "--short"]).trim().is_empty());
    assert!(f.git(&["status", "--porcelain"]).trim().is_empty());
    f.atomic(&["git", "shim", "reconcile"]);
    f.assert_content("main", "tracked.txt", b"edit\n");
}

#[test]
fn preinit_record_all_through_service() {
    preinit_record(false);
}

#[test]
fn preinit_record_all_through_native_command() {
    preinit_record(true);
}

#[test]
fn signed_parent_then_foreign_branch_edit_preserves_file_identity() {
    let f = Fixture::new(false);
    let identity = |root: &Path| {
        atomic_repository::Repository::open(root)
            .unwrap()
            .get_inode_and_position("tracked.txt")
            .unwrap()
            .unwrap()
    };
    let before = identity(f.root.path());
    f.git(&["switch", "-c", "feature"]);
    fs::write(f.root.path().join("tracked.txt"), b"feature edit\n").unwrap();
    f.git(&["add", "tracked.txt"]);
    f.git(&["commit", "-qm", "feature edit"]);
    let head = f.git(&["rev-parse", "HEAD"]);
    f.atomic(&["git", "shim", "reconcile"]);
    f.assert_content("feature", "tracked.txt", b"feature edit\n");
    assert_eq!(identity(f.root.path()), before);
    assert_eq!(f.git(&["rev-parse", "HEAD"]), head);
    assert!(f.atomic(&["status", "--short"]).trim().is_empty());
    f.atomic(&["git", "shim", "reconcile"]);
    f.assert_content("feature", "tracked.txt", b"feature edit\n");
    f.assert_content("main", "tracked.txt", b"base\n");

    f.git(&["mv", "tracked.txt", "renamed.txt"]);
    f.git(&["commit", "-qm", "rename"]);
    f.atomic(&["git", "shim", "reconcile"]);
    f.assert_content("feature", "renamed.txt", b"feature edit\n");
    {
        let repo = atomic_repository::Repository::open(f.root.path()).unwrap();
        assert_eq!(
            repo.get_inode_and_position("renamed.txt").unwrap().unwrap(),
            before
        );
        assert!(repo
            .get_file_content_on_view("tracked.txt", "feature")
            .unwrap()
            .is_none());
    }
    f.git(&["switch", "main"]);
    f.atomic(&["git", "shim", "reconcile"]);
    assert_eq!(
        fs::read(f.root.path().join("tracked.txt")).unwrap(),
        b"base\n"
    );
    f.git(&["switch", "feature"]);
    f.atomic(&["git", "shim", "reconcile"]);
    f.assert_content("feature", "renamed.txt", b"feature edit\n");
    f.git(&["rm", "renamed.txt"]);
    f.git(&["commit", "-qm", "delete"]);
    f.atomic(&["git", "shim", "reconcile"]);
    let repo = atomic_repository::Repository::open(f.root.path()).unwrap();
    assert!(repo
        .get_file_content_on_view("renamed.txt", "feature")
        .unwrap()
        .is_none());
    drop(repo);
    f.assert_content("main", "tracked.txt", b"base\n");
}

#[test]
fn signed_sibling_branches_import_in_either_order() {
    for order in [["left", "right"], ["right", "left"]] {
        let f = Fixture::new(false);
        f.git(&["branch", "left"]);
        f.git(&["branch", "right"]);
        for branch in order {
            f.git(&["switch", branch]);
            let bytes = format!("{branch} edit\n");
            fs::write(f.root.path().join("tracked.txt"), &bytes).unwrap();
            f.git(&["add", "tracked.txt"]);
            f.git(&["commit", "-qm", branch]);
            f.atomic(&["git", "shim", "reconcile"]);
            f.assert_content(branch, "tracked.txt", bytes.as_bytes());
        }
        // Both closures must still reconstruct independently after the other
        // branch was imported and became the current path-index projection.
        f.assert_content("left", "tracked.txt", b"left edit\n");
        f.assert_content("right", "tracked.txt", b"right edit\n");
        f.assert_content("main", "tracked.txt", b"base\n");
    }
}
