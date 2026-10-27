//! Native rename recording must keep attributes on the existing graph inode.
//! Exercise the real CLI: `-a` includes the untracked rename destination.

use std::fs;
use std::path::Path;
use std::process::Command;

use atomic_core::change::{Change, GraphOp};
use atomic_core::crdt::{tables::decode_trunk_id, TrunkId, TrunkOp};
use atomic_core::pristine::{CrdtTxnT, TreeTxnT};
use atomic_core::types::{Inode, NodeId, Position};
use atomic_repository::{HistoryOptions, Repository, StatusOptions};
use tempfile::TempDir;

const BASE: &[u8] = b"one\ntwo\nthree\nfour\n";

struct Fixture {
    root: TempDir,
    home: TempDir,
    inode: Inode,
    position: Position<NodeId>,
    trunk: TrunkId,
}

fn atomic(root: &Path, home: &Path, args: &[&str]) {
    let output = Command::new(env!("CARGO_BIN_EXE_atomic"))
        .args(args)
        .current_dir(root)
        .env("ATOMIC_HOME", home.join(".atomic"))
        .env("ATOMIC_NONINTERACTIVE", "1")
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "atomic {args:?} failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

impl Fixture {
    fn new() -> Self {
        let root = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        atomic(root.path(), home.path(), &["init", "--no-vault"]);
        fs::write(root.path().join("old.txt"), BASE).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                root.path().join("old.txt"),
                fs::Permissions::from_mode(0o644),
            )
            .unwrap();
        }
        atomic(root.path(), home.path(), &["add", "old.txt"]);
        atomic(
            root.path(),
            home.path(),
            &["record", "-am", "base", "--author", "Rename Test"],
        );
        assert!(!root.path().join(".git").exists());
        let mut repo = Repository::open(root.path()).unwrap();
        let inode = repo.get_file_inode("old.txt").unwrap().unwrap();
        let txn = repo.pristine().read_txn().unwrap();
        let position = txn.inode_position(inode).unwrap().unwrap();
        let trunk = decode_trunk_id(&txn.get_crdt_inode_trunk(inode.get()).unwrap().unwrap());
        drop(txn);
        let current = repo.current_view().to_string();
        repo.create_view_from("before-rename", &current).unwrap();
        repo.create_view_from("rename-work", &current).unwrap();
        repo.switch_view(repo.require_working_copy_id().unwrap(), "rename-work")
            .unwrap();
        Self {
            root,
            home,
            inode,
            position,
            trunk,
        }
    }

    fn atomic(&self, args: &[&str]) {
        atomic(self.root.path(), self.home.path(), args);
    }

    fn raw_rename(&self) {
        fs::rename(
            self.root.path().join("old.txt"),
            self.root.path().join("new.txt"),
        )
        .unwrap();
    }

    fn record(&self, all: bool) -> Change {
        self.atomic(&[
            "record",
            if all { "-am" } else { "-m" },
            "rename",
            "--author",
            "Rename Test",
        ]);
        let repo = Repository::open(self.root.path()).unwrap();
        let history = repo.log(HistoryOptions::default()).unwrap();
        let latest = history.iter().max_by_key(|entry| entry.sequence).unwrap();
        let change = repo.load_change(&latest.hash).unwrap();
        assert_eq!(
            change
                .hunks()
                .iter()
                .filter(|op| matches!(op, GraphOp::FileMove { path, .. } if path == "new.txt"))
                .count(),
            1
        );
        assert!(!change
            .hunks()
            .iter()
            .any(|op| matches!(op, GraphOp::FileAdd { .. } | GraphOp::FileDel { .. })));
        assert!(change.file_ops().iter().any(|ops| matches!(ops.trunk_op(), Some(TrunkOp::Move { trunk, .. }) if *trunk == self.trunk)));
        self.assert_identity(&repo);
        change
    }

    fn assert_identity(&self, repo: &Repository) {
        assert_eq!(repo.get_file_inode("new.txt").unwrap(), Some(self.inode));
        assert!(repo.get_file_inode("old.txt").unwrap().is_none());
        let txn = repo.pristine().read_txn().unwrap();
        assert_eq!(txn.inode_position(self.inode).unwrap(), Some(self.position));
        assert_eq!(
            decode_trunk_id(&txn.get_crdt_inode_trunk(self.inode.get()).unwrap().unwrap()),
            self.trunk
        );
        assert!(repo
            .status(
                repo.require_working_copy_id().unwrap(),
                StatusOptions::default()
            )
            .unwrap()
            .is_clean());
    }

    fn roundtrip(&self, expected: &[u8]) {
        let repo = Repository::open(self.root.path()).unwrap();
        let current = repo.current_view().to_string();
        drop(repo);
        self.atomic(&["view", "switch", "before-rename"]);
        assert_eq!(fs::read(self.root.path().join("old.txt")).unwrap(), BASE);
        assert!(!self.root.path().join("new.txt").exists());
        self.atomic(&["view", "switch", &current]);
        // Reconstruct from recorded data, rather than accepting retained bytes.
        fs::remove_file(self.root.path().join("new.txt")).unwrap();
        let repo = Repository::open(self.root.path()).unwrap();
        repo.materialize(repo.require_working_copy_id().unwrap())
            .unwrap();
        assert_eq!(
            fs::read(self.root.path().join("new.txt")).unwrap(),
            expected
        );
        assert!(!self.root.path().join("old.txt").exists());
        self.assert_identity(&repo);
    }
}

#[test]
fn raw_rename_records_with_and_without_all() {
    for all in [true, false] {
        let fixture = Fixture::new();
        fixture.raw_rename();
        let change = fixture.record(all);
        assert!(
            !change
                .hunks()
                .iter()
                .any(|op| matches!(op, GraphOp::SetAttr { .. })),
            "unchanged attributes must not gain a new writer"
        );
        fixture.roundtrip(BASE);
    }
}

#[test]
fn explicit_move_and_content_edit_record_all_preserves_identity() {
    let fixture = Fixture::new();
    fixture.atomic(&["move", "old.txt", "new.txt"]);
    let edited = b"one\ntwo edited\nthree\nfour\n";
    fs::write(fixture.root.path().join("new.txt"), edited).unwrap();
    fixture.record(true);
    fixture.roundtrip(edited);
}

#[cfg(unix)]
#[test]
fn rename_and_chmod_record_existing_inode_with_and_without_all() {
    use atomic_core::change::InodeAttr;
    use std::os::unix::fs::PermissionsExt;

    for (all, edit) in [(true, false), (false, false), (true, true)] {
        let fixture = Fixture::new();
        let expected: &[u8] = if edit {
            fixture.atomic(&["move", "old.txt", "new.txt"]);
            b"one\ntwo edited\nthree\nfour\n"
        } else {
            fixture.raw_rename();
            BASE
        };
        let path = fixture.root.path().join("new.txt");
        fs::write(&path, expected).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let change = fixture.record(all);
        let attrs: Vec<_> = change
            .hunks()
            .iter()
            .filter_map(|op| match op {
                GraphOp::SetAttr { inode, path, value } => Some((inode, path, value)),
                _ => None,
            })
            .collect();
        assert_eq!(attrs.len(), 1);
        assert_eq!(attrs[0].1, "new.txt");
        assert_eq!(*attrs[0].2, InodeAttr::Mode(0o755));
        assert_eq!(attrs[0].0.pos, fixture.position.pos);
        let repo = Repository::open(fixture.root.path()).unwrap();
        use atomic_core::pristine::GraphTxnT;
        let origin = repo
            .pristine()
            .read_txn()
            .unwrap()
            .get_external(fixture.position.change)
            .unwrap()
            .unwrap();
        assert_eq!(attrs[0].0.change, Some(origin));
        assert!(change.dependencies().contains(&origin));
        drop(repo);
        assert!(change.file_ops().iter().any(|ops| matches!(ops.trunk_op(), Some(TrunkOp::SetMode { trunk, mode: 0o755 }) if *trunk == fixture.trunk)));
        fixture.roundtrip(expected);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
}

#[cfg(unix)]
#[test]
fn move_and_kind_change_record_on_existing_inode() {
    use atomic_core::change::{InodeAttr, InodeKind};

    let fixture = Fixture::new();
    fixture.atomic(&["move", "old.txt", "new.txt"]);
    let path = fixture.root.path().join("new.txt");
    fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink("target.txt", &path).unwrap();
    let change = fixture.record(true);
    assert!(change.hunks().iter().any(|op| matches!(op,
        GraphOp::SetAttr { inode, path, value: InodeAttr::Kind(InodeKind::Symlink) }
        if inode.change.is_some() && inode.pos == fixture.position.pos && path == "new.txt"
    )));
    assert!(change.file_ops().iter().any(|ops| matches!(ops.trunk_op(),
        Some(TrunkOp::SetKind { trunk, kind: InodeKind::Symlink }) if *trunk == fixture.trunk
    )));
    fs::remove_file(&path).unwrap();
    let repo = Repository::open(fixture.root.path()).unwrap();
    repo.materialize(repo.require_working_copy_id().unwrap())
        .unwrap();
    assert_eq!(fs::read_link(&path).unwrap(), Path::new("target.txt"));
    fixture.assert_identity(&repo);
}

#[test]
fn preadding_rename_destination_preserves_recorded_identity() {
    let fixture = Fixture::new();
    fixture.raw_rename();
    // libatomic's record -a composition calls AddFiles before Record.
    fixture.atomic(&["add", "new.txt"]);
    fixture.record(false);
    fixture.roundtrip(BASE);
}
