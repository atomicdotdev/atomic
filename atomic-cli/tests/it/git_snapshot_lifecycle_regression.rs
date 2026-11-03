//! Entry-boundary snapshots must not consume durable changes or invent identities.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::process::{Command, Output};

use atomic_core::change::InodeKind;
use atomic_core::pristine::TreeTxnT;
use atomic_repository::Repository;
use tempfile::TempDir;

struct Fixture {
    root: TempDir,
    home: TempDir,
}

impl Fixture {
    fn command(&self, program: &str, args: &[&str]) -> Output {
        let mut command = Command::new(program);
        for (name, _) in std::env::vars_os() {
            let text = name.to_string_lossy();
            if text.starts_with("ATOMIC_")
                || text.starts_with("OPENCODE_")
                || text.starts_with("GIT_")
            {
                command.env_remove(name);
            }
        }
        command
            .args(args)
            .current_dir(self.root.path())
            .env("HOME", self.home.path())
            .env("ATOMIC_HOME", self.home.path().join(".atomic"))
            .env("ATOMIC_CONFIG_DIR", self.home.path().join(".atomic"))
            .env("ATOMIC_SERVICE", "local")
            .env("ATOMIC_NONINTERACTIVE", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Snapshot regression")
            .env("GIT_AUTHOR_EMAIL", "snapshot@example.invalid")
            .env("GIT_COMMITTER_NAME", "Snapshot regression")
            .env("GIT_COMMITTER_EMAIL", "snapshot@example.invalid")
            .env("NO_COLOR", "1")
            .output()
            .unwrap()
    }

    fn ok(&self, program: &str, args: &[&str]) -> String {
        let output = self.command(program, args);
        assert!(output.status.success(), "{program} {args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap()
    }

    fn atomic(&self, args: &[&str]) -> String {
        self.ok(env!("CARGO_BIN_EXE_atomic"), args)
    }

    fn git(&self, args: &[&str]) -> String {
        self.ok("git", args)
    }

    fn new(rolling: bool, full: bool) -> Self {
        let fixture = Self {
            root: TempDir::new().unwrap(),
            home: TempDir::new().unwrap(),
        };
        fixture.git(&["init", "-q", "-b", "main"]);
        fs::write(fixture.root.path().join("tracked.txt"), b"first\nsecond\n").unwrap();
        if full {
            fs::create_dir(fixture.root.path().join("dir")).unwrap();
            fs::write(fixture.root.path().join("dir/café.txt"), b"cafe\n").unwrap();
            fs::write(fixture.root.path().join("literal%20path.txt"), b"percent\n").unwrap();
            fs::write(
                fixture.root.path().join(".gitattributes"),
                b"*.txt text eol=lf\n",
            )
            .unwrap();
            fs::write(
                fixture.root.path().join("run.sh"),
                b"#!/bin/sh\necho fixture\n",
            )
            .unwrap();
            fs::set_permissions(
                fixture.root.path().join("run.sh"),
                fs::Permissions::from_mode(0o755),
            )
            .unwrap();
            symlink("tracked.txt", fixture.root.path().join("tracked-link")).unwrap();
        }
        fixture.git(&["add", "-A"]);
        fixture.git(&["commit", "-qm", "seed"]);
        if full {
            fs::write(
                fixture.root.path().join("tracked.txt"),
                b"first\nsecond commit\n",
            )
            .unwrap();
            fixture.git(&["add", "-A"]);
            fixture.git(&["commit", "-qm", "text edit"]);
            fixture.git(&["commit", "--allow-empty", "-qm", "empty"]);
        }
        let mut import = vec!["git", "import", "--no-vault"];
        if !rolling {
            import.push("--no-stream");
        }
        fixture.atomic(&import);
        let key = fixture.home.path().join("binding.key");
        fs::write(&key, "ab".repeat(32)).unwrap();
        fixture.atomic(&[
            "git",
            "shim",
            "enable",
            "--no-refspecs",
            "--binding-key-file",
            key.to_str().unwrap(),
        ]);
        fixture.atomic(&["git", "shim", "reconcile"]);
        fs::write(
            fixture.root.path().join("tracked.txt"),
            b"first\nnative edit\n",
        )
        .unwrap();
        if full {
            fixture.atomic(&["move", "dir/café.txt", "dir/moved café.txt"]);
            fs::write(
                fixture.root.path().join("dir/moved café.txt"),
                b"cafe native edit\n",
            )
            .unwrap();
        }
        fixture.atomic(&["record", "-am", "native edit and move"]);
        fixture
    }

    fn neighbors(&self) -> Vec<String> {
        self.git(&["ls-tree", "-rz", "HEAD"])
            .split('\0')
            .filter(|entry| !entry.is_empty() && !entry.ends_with("\ttracked.txt"))
            .map(str::to_owned)
            .collect()
    }

    fn assert_deleted(&self) {
        self.atomic(&["git", "shim", "verify"]);
        assert!(self
            .git(&["ls-tree", "HEAD", "--", "tracked.txt"])
            .is_empty());
        let repo = Repository::open(self.root.path()).unwrap();
        assert!(repo
            .get_file_content_on_view("tracked.txt", "main")
            .unwrap()
            .is_none());
    }
}

#[test]
fn first_deletion_record_succeeds_without_status_retry_or_reopen() {
    for rolling in [false, true] {
        let fixture = Fixture::new(rolling, false);
        let before = fixture
            .git(&["rev-list", "--count", "HEAD"])
            .trim()
            .parse::<usize>()
            .unwrap();
        // Do not insert any observation between unlink and this first record.
        fs::remove_file(fixture.root.path().join("tracked.txt")).unwrap();
        fixture.atomic(&["record", "-am", "native deletion"]);
        fixture.assert_deleted();
        let after = fixture
            .git(&["rev-list", "--count", "HEAD"])
            .trim()
            .parse::<usize>()
            .unwrap();
        assert_eq!(
            after,
            before + 1,
            "rolling={rolling}: record deletion exactly once"
        );
        assert!(fixture.git(&["status", "--porcelain"]).is_empty());
        let no_op = fixture.command(env!("CARGO_BIN_EXE_atomic"), &["record", "-am", "no-op"]);
        assert!(!no_op.status.success());
        assert!(String::from_utf8_lossy(&no_op.stderr).contains("Nothing to record"));
        assert_eq!(
            fixture
                .git(&["rev-list", "--count", "HEAD"])
                .trim()
                .parse::<usize>()
                .unwrap(),
            after
        );
    }
}

#[test]
fn recreate_add_and_record_preserve_identity_bytes_attributes_and_neighbors() {
    // Keep both the one-file minimization and the richer retained scenario.
    for rolling in [false, true] {
        for full in [false, true] {
            for kind in ["regular", "executable", "symlink"] {
                let fixture = Fixture::new(rolling, full);
                let (inode, position) = {
                    let repo = Repository::open(fixture.root.path()).unwrap();
                    let inode = repo.get_file_inode("tracked.txt").unwrap().unwrap();
                    let txn = repo.pristine().read_txn().unwrap();
                    (inode, txn.inode_position(inode).unwrap().unwrap())
                };
                let neighbors = fixture.neighbors();
                let path = fixture.root.path().join("tracked.txt");
                fs::remove_file(&path).unwrap();
                // Deliberately isolate recreate/add from the first-record bug.
                fixture.atomic(&["status"]);
                fixture.atomic(&["record", "-am", "delete"]);
                fixture.assert_deleted();
                let (bytes, git_mode, expected_kind) = if kind == "symlink" {
                    symlink("literal%20path.txt", &path).unwrap();
                    (
                        b"literal%20path.txt".as_slice(),
                        "120000",
                        InodeKind::Symlink,
                    )
                } else {
                    fs::write(&path, b"restored\n").unwrap();
                    let executable = kind == "executable";
                    fs::set_permissions(
                        &path,
                        fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 }),
                    )
                    .unwrap();
                    (
                        b"restored\n".as_slice(),
                        if executable { "100755" } else { "100644" },
                        InodeKind::Regular,
                    )
                };
                fixture.atomic(&["add", "tracked.txt"]);
                fixture.atomic(&["record", "-am", "recreated"]);
                fixture.atomic(&["git", "shim", "verify"]);
                assert_eq!(fixture.git(&["show", "HEAD:tracked.txt"]).as_bytes(), bytes);
                assert!(fixture
                    .git(&["ls-tree", "HEAD", "--", "tracked.txt"])
                    .starts_with(git_mode));
                assert_eq!(fixture.neighbors(), neighbors);
                assert!(fixture.git(&["status", "--porcelain"]).is_empty());
                let repo = Repository::open(fixture.root.path()).unwrap();
                assert_eq!(repo.get_file_inode("tracked.txt").unwrap(), Some(inode));
                assert_eq!(
                    repo.pristine()
                        .read_txn()
                        .unwrap()
                        .inode_position(inode)
                        .unwrap(),
                    Some(position)
                );
                let policy = atomic_repository::ConversionPolicy::new(
                    atomic_core::operation::GitHashAlgorithm::Sha1,
                );
                let projected = repo.project_tree("main", &policy).unwrap();
                let entry = projected
                    .manifest
                    .entries
                    .iter()
                    .find(|entry| entry.path.as_bytes() == b"tracked.txt")
                    .unwrap();
                assert_eq!(entry.repository_bytes.as_ref(), bytes);
                assert_eq!(entry.kind, expected_kind);
                assert_eq!(entry.git_mode(), u32::from_str_radix(git_mode, 8).unwrap());
            }
        }
    }
}
