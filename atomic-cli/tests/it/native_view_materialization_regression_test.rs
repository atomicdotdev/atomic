//! Working-copy output must agree with the selected canonical graph closure.

use std::{fs, path::Path, process::Command};

use atomic_repository::Repository;
use tempfile::TempDir;

struct Fixture {
    root: TempDir,
    home: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let fixture = Self {
            root: TempDir::new().unwrap(),
            home: TempDir::new().unwrap(),
        };
        fixture.run(&["init", "--no-vault"]);
        fixture
    }

    fn run(&self, args: &[&str]) -> String {
        let output = self.output(args);
        assert!(
            output.status.success(),
            "atomic {args:?}:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn output(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_atomic"))
            .args(args)
            .current_dir(self.root.path())
            .env("ATOMIC_HOME", self.home.path())
            .env("ATOMIC_NONINTERACTIVE", "1")
            .env("NO_COLOR", "1")
            .output()
            .unwrap()
    }

    fn write(&self, path: &str, bytes: &[u8]) {
        fs::write(self.root.path().join(path), bytes).unwrap();
    }

    fn read(&self, path: &str) -> Vec<u8> {
        fs::read(self.root.path().join(path)).unwrap()
    }

    fn record(&self, message: &str) {
        self.run(&["record", "-m", message, "--author", "View Regression"]);
    }

    fn switch(&self, view: &str) {
        self.run(&["view", "switch", view, "--force"]);
    }

    fn draft(&self, view: &str) {
        self.run(&["view", "create", view, "--draft", "--parent", "dev"]);
    }
}

#[test]
fn live_parent_switch_uses_the_same_merged_bytes_as_its_filesystem_lease() {
    let f = Fixture::new();
    let base = b"[server]\nhost = localhost\nport = 8080\nworkers = 4\n\n[database]\nurl = postgres://localhost/mydb\npool_size = 10\ntimeout = 30\n";
    f.write("server.conf", base);
    f.run(&["add", "server.conf"]);
    f.record("base");
    f.draft("feature");
    f.switch("feature");
    let child = String::from_utf8(base.to_vec())
        .unwrap()
        .replace("port = 8080", "port = 9090")
        .replace("pool_size = 10", "pool_size = 20");
    f.write("server.conf", child.as_bytes());
    f.record("child edits");
    f.switch("dev");
    let parent = String::from_utf8(base.to_vec())
        .unwrap()
        .replace("workers = 4", "workers = 8")
        .replace("timeout = 30", "timeout = 60");
    f.write("server.conf", parent.as_bytes());
    f.record("parent edits");
    let merged = child
        .replace("workers = 4", "workers = 8")
        .replace("timeout = 30", "timeout = 60");
    for _ in 0..2 {
        f.switch("feature");
        assert_eq!(f.read("server.conf"), merged.as_bytes());
        let status = f.run(&["status", "--short"]);
        assert!(status.trim().is_empty(), "status: {status}");
        f.switch("dev");
        assert_eq!(f.read("server.conf"), parent.as_bytes());
    }
    assert!(!f.root.path().join(".git").exists());
}

#[test]
fn inserting_rename_refreshes_both_names_in_opposite_import_orders() {
    let f = Fixture::new();
    f.write("local.txt", b"recorded local file\n");
    f.run(&["add", "local.txt"]);
    f.record("unrelated base");
    for (view, bytes) in [
        ("bill", b"bill body\n".as_slice()),
        ("sally", b"sally body\n".as_slice()),
    ] {
        f.draft(view);
        f.switch(view);
        f.write("f.txt", bytes);
        f.run(&["add", "f.txt"]);
        f.record(view);
        f.switch("dev");
    }
    for (target, order) in [("ab", ["bill", "sally"]), ("ba", ["sally", "bill"])] {
        f.draft(target);
        f.switch(target);
        for source in order {
            f.run(&["insert", "from-view", source, "--to-view", target]);
        }
        let bytes = f.read("f.txt");
        assert!(bytes
            .windows(b"(name conflict)".len())
            .any(|w| w == b"(name conflict)"));
    }
    f.switch("bill");
    let bill_inode = Repository::open(f.root.path())
        .unwrap()
        .get_file_inode("f.txt")
        .unwrap()
        .unwrap();
    fs::rename(f.root.path().join("f.txt"), f.root.path().join("bill.txt")).unwrap();
    f.record("bill renames");
    for target in ["ab", "ba"] {
        f.switch(target);
        let conflict_bytes = f.read("f.txt");
        let conflict_path = f.root.path().join("f.txt");
        let refuse_without_publication = || {
            let before = Repository::open_readonly(f.root.path())
                .unwrap()
                .get_view_info(target)
                .unwrap()
                .state;
            let file_bytes = fs::read(&conflict_path).ok();
            let file_kind = fs::symlink_metadata(&conflict_path).unwrap().file_type();
            let local = f.read("local.txt");
            let output = f.output(&["insert", "from-view", "bill", "--to-view", target]);
            assert!(
                !output.status.success(),
                "insert must preserve unrecorded work"
            );
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(stderr.contains("unrecorded changes"), "{stderr}");
            let after = Repository::open_readonly(f.root.path())
                .unwrap()
                .get_view_info(target)
                .unwrap()
                .state;
            assert_eq!(before, after, "refusal must not insert the rename");
            assert_eq!(fs::read(&conflict_path).ok(), file_bytes);
            assert_eq!(
                fs::symlink_metadata(&conflict_path).unwrap().file_type(),
                file_kind
            );
            assert_eq!(f.read("local.txt"), local);
            assert!(!f.root.path().join("bill.txt").exists());
        };

        // #248 refuses all unrecorded work before publishing membership,
        // including edits unrelated to the insertion's surgical refresh.
        f.write("local.txt", b"unrecorded local edit\n");
        refuse_without_publication();
        f.write("local.txt", b"recorded local file\n");
        let mut edited_markers = conflict_bytes.clone();
        edited_markers.extend_from_slice(b"local conflict resolution in progress\n");
        f.write("f.txt", &edited_markers);
        refuse_without_publication();
        f.write("f.txt", &conflict_bytes);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{symlink, PermissionsExt};
            let permissions = fs::metadata(&conflict_path).unwrap().permissions();
            fs::set_permissions(&conflict_path, fs::Permissions::from_mode(0o755)).unwrap();
            refuse_without_publication();
            assert_eq!(
                fs::metadata(&conflict_path).unwrap().permissions().mode() & 0o777,
                0o755
            );
            fs::set_permissions(&conflict_path, permissions).unwrap();

            // Equal referent bytes do not make a symlink the recorded file.
            let outside = TempDir::new().unwrap();
            let referent = outside.path().join("conflict.txt");
            fs::write(&referent, &conflict_bytes).unwrap();
            fs::remove_file(&conflict_path).unwrap();
            symlink(&referent, &conflict_path).unwrap();
            refuse_without_publication();
            assert_eq!(fs::read(&referent).unwrap(), conflict_bytes);
            fs::remove_file(&conflict_path).unwrap();
            f.write("f.txt", &conflict_bytes);
        }
        // The exact recorded conflict render has no unrecorded changes.
        f.run(&["insert", "from-view", "bill", "--to-view", target]);
        assert_eq!(f.read("f.txt"), b"sally body\n");
        assert_eq!(f.read("bill.txt"), b"bill body\n");
        let status = f.run(&["status", "--short"]);
        assert!(status.trim().is_empty(), "status: {status}");
        let repo = Repository::open(f.root.path()).unwrap();
        assert_eq!(repo.get_file_inode("bill.txt").unwrap(), Some(bill_inode));
        // Reconstruction must not depend on the prior materialized markers.
        fs::remove_file(f.root.path().join("f.txt")).unwrap();
        repo.materialize(repo.require_working_copy_id().unwrap())
            .unwrap();
        assert_eq!(f.read("f.txt"), b"sally body\n");
        drop(repo);
        f.switch("dev");
        f.switch(target);
        assert_eq!(f.read("f.txt"), b"sally body\n");
        assert_eq!(f.read("bill.txt"), b"bill body\n");
    }
    assert!(!Path::new(f.root.path()).join(".git").exists());
}
