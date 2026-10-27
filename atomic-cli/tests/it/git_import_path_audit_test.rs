#![cfg(unix)]

use git2::{Oid, Repository as GitRepository, Signature};
use std::path::Path;
use std::process::Command;

fn tree(git: &GitRepository, files: &[(&str, &[u8], i32)]) -> Oid {
    let mut builder = git.treebuilder(None).unwrap();
    let mut directories = std::collections::BTreeMap::<&str, Vec<(&str, &[u8], i32)>>::new();
    for &(path, bytes, mode) in files {
        if let Some((directory, rest)) = path.split_once('/') {
            directories
                .entry(directory)
                .or_default()
                .push((rest, bytes, mode));
        } else {
            builder
                .insert(path, git.blob(bytes).unwrap(), mode)
                .unwrap();
        }
    }
    for (directory, children) in directories {
        builder
            .insert(directory, tree(git, &children), 0o040000)
            .unwrap();
    }
    builder.write().unwrap()
}

fn commit(git: &GitRepository, files: &[(&str, &[u8], i32)], parents: &[Oid]) -> Oid {
    let tree = git.find_tree(tree(git, files)).unwrap();
    let parents: Vec<_> = parents
        .iter()
        .map(|id| git.find_commit(*id).unwrap())
        .collect();
    let signature = Signature::now("Import regression", "import@example.com").unwrap();
    git.commit(
        None,
        &signature,
        &signature,
        "fixture",
        &tree,
        &parents.iter().collect::<Vec<_>>(),
    )
    .unwrap()
}

fn import(root: &Path, git: &GitRepository, tip: Oid, config: &Path) {
    git.reference("refs/heads/main", tip, true, "fixture")
        .unwrap();
    git.set_head("refs/heads/main").unwrap();
    git.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();
    let mut command = Command::new(
        std::env::var("ATOMIC_AUDIT_BINARY")
            .unwrap_or_else(|_| env!("CARGO_BIN_EXE_atomic").to_string()),
    );
    for (key, _) in std::env::vars().filter(|(key, _)| key.starts_with("ATOMIC_")) {
        command.env_remove(key);
    }
    let output = command
        .current_dir(root)
        .env("ATOMIC_CONFIG_DIR", config)
        .env("ATOMIC_SERVICE", "local")
        .env("ATOMIC_NONINTERACTIVE", "1")
        .args(["git", "import", "--no-vault", "--no-color"])
        .args(
            std::env::var("ATOMIC_AUDIT_FLAGS")
                // The full path/type corpus targets the ordinary importer;
                // an explicit audit override can still select --stream.
                .unwrap_or_else(|_| "--no-stream".into())
                .split_whitespace(),
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "import failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn check_case(states: Vec<Vec<(&str, &[u8], i32)>>, merge: bool) {
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let git = GitRepository::init(root.path()).unwrap();
    let mut ids = Vec::new();
    for (i, files) in states.iter().enumerate() {
        let parents = if i == 0 {
            vec![]
        } else if merge && i == states.len() - 1 {
            vec![ids[1], ids[2]]
        } else if merge && i == 2 {
            vec![ids[0]]
        } else {
            vec![ids[i - 1]]
        };
        ids.push(commit(&git, files, &parents));
    }
    import(root.path(), &git, *ids.last().unwrap(), config.path());
    let repo = atomic_repository::Repository::open(root.path()).unwrap();
    for (path, bytes, _) in states.last().unwrap() {
        // The repository tracks an imported path under its on-disk spelling
        // (as native tracking does); a name with a literal `%` keeps its
        // escaped identity, so look the path up under either spelling.
        let escaped = atomic_repository::escape_repo_path(path.as_bytes());
        let content = repo
            .get_file_content(path)
            .unwrap()
            .or_else(|| repo.get_file_content(&escaped).unwrap());
        assert_eq!(
            content.as_deref(),
            Some(*bytes),
            "path={path} (escaped {escaped})"
        );
    }
}

#[test]
fn audit_side_parent_space() {
    side_only("a b.txt");
}
#[test]
fn audit_side_parent_percent_escape() {
    side_only("a%41.txt");
}

#[test]
fn audit_percent_path_working_copy_is_clean() {
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let git = GitRepository::init(root.path()).unwrap();
    let tip = commit(
        &git,
        &[
            ("100%.txt", b"percent\n", 0o100644),
            ("a%41.txt", b"literal escape\n", 0o100644),
            ("aA.txt", b"decoded lookalike\n", 0o100644),
            ("dir%/café space.txt", b"nested percent\n", 0o100644),
            ("link%.txt", b"100%.txt", 0o120000),
        ],
        &[],
    );
    import(root.path(), &git, tip, config.path());
    let repo = atomic_repository::Repository::open(root.path()).unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    let status = repo.status(wc, Default::default()).unwrap();
    assert!(status.is_clean(), "unexpected status: {status:#?}");

    std::fs::write(root.path().join("a%41.txt"), b"edited literal escape\n").unwrap();
    let status = repo.status(wc, Default::default()).unwrap();
    assert_eq!(status.entries().len(), 1, "{status:#?}");
    assert_eq!(status.entries()[0].path(), Path::new("a%2541.txt"));
    assert_eq!(
        status.entries()[0].status(),
        atomic_repository::FileStatus::Modified
    );
    let filtered = repo
        .status(
            wc,
            atomic_repository::StatusOptions {
                path_filters: vec!["a%41.txt".into()],
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(filtered.entries().len(), 1, "{filtered:#?}");
    assert_eq!(filtered.entries()[0].path(), Path::new("a%2541.txt"));
    assert_eq!(
        filtered.entries()[0].status(),
        atomic_repository::FileStatus::Modified
    );

    std::fs::write(root.path().join("a%41.txt"), b"literal escape\n").unwrap();
    std::fs::remove_file(root.path().join("100%.txt")).unwrap();
    let status = repo.status(wc, Default::default()).unwrap();
    assert_eq!(status.entries().len(), 1, "{status:#?}");
    assert_eq!(status.entries()[0].path(), Path::new("100%25.txt"));
    assert_eq!(
        status.entries()[0].status(),
        atomic_repository::FileStatus::Deleted
    );
}

#[test]
fn audit_side_parent_unicode() {
    side_only("中文.txt");
}
#[test]
fn audit_side_parent_ascii_control() {
    side_only("plain.txt");
}
fn side_only(path: &str) {
    check_case(
        vec![
            vec![(path, b"base\n", 0o100644)],
            vec![(path, b"base\n", 0o100644), ("main", b"main\n", 0o100644)],
            vec![(path, b"side\n", 0o100644)],
            vec![(path, b"base\n", 0o100644), ("main", b"main\n", 0o100644)],
        ],
        true,
    );
}

#[test]
fn audit_two_historical_moves_same_destination() {
    check_case(
        vec![
            vec![
                ("a", b"alpha file first line\nalpha second line\n", 0o100644),
                ("b", b"beta file first line\nbeta second line\n", 0o100644),
            ],
            vec![
                (
                    "same",
                    b"alpha file first line\nalpha second line\n",
                    0o100644,
                ),
                ("b", b"beta file first line\nbeta second line\n", 0o100644),
            ],
            vec![("b", b"beta file first line\nbeta second line\n", 0o100644)],
            vec![(
                "same",
                b"beta file first line\nbeta second line\n",
                0o100644,
            )],
        ],
        false,
    );
}
#[test]
fn audit_file_to_directory() {
    check_case(
        vec![
            vec![("a", b"hello\n", 0o100644)],
            vec![("a/file", b"hello\n", 0o100644)],
        ],
        false,
    );
}
#[test]
fn audit_directory_to_file() {
    check_case(
        vec![
            vec![("a/file", b"hello\n", 0o100644)],
            vec![("a", b"hello\n", 0o100644)],
        ],
        false,
    );
}
#[test]
fn audit_recreate_and_move() {
    check_case(
        vec![
            vec![("a", b"old\n", 0o100644), ("keep", b"keep\n", 0o100644)],
            vec![("keep", b"keep\n", 0o100644)],
            vec![("a", b"new\n", 0o100644), ("keep", b"keep\n", 0o100644)],
            vec![("b", b"new\n", 0o100644), ("keep", b"keep\n", 0o100644)],
        ],
        false,
    );
}
#[test]
fn audit_two_identical_sources_one_destination() {
    check_case(
        vec![
            vec![
                ("a", b"identical content\n", 0o100644),
                ("b", b"identical content\n", 0o100644),
            ],
            vec![("c", b"identical content\n", 0o100644)],
        ],
        false,
    );
}

#[test]
fn audit_foreign_edit_uses_requested_view_identity() {
    use atomic_core::crdt::tables::decode_trunk_id;
    use atomic_core::pristine::{CrdtTxnT, TreeTxnT};
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let git = GitRepository::init(root.path()).unwrap();
    let tip = commit(&git, &[("same", b"original alpha\n", 0o100644)], &[]);
    import(root.path(), &git, tip, config.path());
    let mut repo = atomic_repository::Repository::open(root.path()).unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    let (canonical_inode, _) = repo
        .get_inode_and_position_on_view_excluding("same", "main", &[])
        .unwrap()
        .unwrap();
    let canonical_trunk = {
        let txn = repo.pristine().read_txn().unwrap();
        decode_trunk_id(
            &txn.get_crdt_inode_trunk(canonical_inode.get())
                .unwrap()
                .unwrap(),
        )
    };
    repo.create_shared_view("side").unwrap();
    repo.switch_view(wc, "side").unwrap();
    std::fs::write(root.path().join("same"), b"replacement beta\n").unwrap();
    repo.add(wc, "same", Default::default()).unwrap();
    repo.record_all(wc, "create new on side").unwrap();
    assert_eq!(
        repo.get_inode_and_position_on_view_excluding("same", "main", &[])
            .unwrap()
            .unwrap()
            .0,
        canonical_inode
    );
    {
        let txn = repo.pristine().read_txn().unwrap();
        assert_ne!(txn.get_inode("same").unwrap().unwrap(), canonical_inode);
    }
    let record = repo
        .record_foreign_modified_file_on_view_excluding(
            "same",
            b"original alpha\n",
            b"edited alpha\n",
            false,
            "main",
            &[],
        )
        .unwrap();
    assert_eq!(
        record.crdt_ops().unwrap().trunk_id(),
        canonical_trunk,
        "a foreign edit must follow the requested closure's inode, not current PATH_TRUNK"
    );
}

#[test]
fn audit_foreign_move_uses_requested_view_identity() {
    use atomic_core::crdt::tables::decode_trunk_id;
    use atomic_core::pristine::{CrdtTxnT, TreeTxnT};
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let git = GitRepository::init(root.path()).unwrap();
    let tip = commit(&git, &[("same", b"original alpha\n", 0o100644)], &[]);
    import(root.path(), &git, tip, config.path());
    let mut repo = atomic_repository::Repository::open(root.path()).unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    let (canonical_inode, _) = repo
        .get_inode_and_position_on_view_excluding("same", "main", &[])
        .unwrap()
        .unwrap();
    let canonical_trunk = {
        let txn = repo.pristine().read_txn().unwrap();
        decode_trunk_id(
            &txn.get_crdt_inode_trunk(canonical_inode.get())
                .unwrap()
                .unwrap(),
        )
    };
    repo.create_shared_view("side").unwrap();
    repo.switch_view(wc, "side").unwrap();
    std::fs::write(root.path().join("same"), b"replacement beta\n").unwrap();
    repo.add(wc, "same", Default::default()).unwrap();
    repo.record_all(wc, "create new on side").unwrap();
    assert_eq!(
        repo.get_inode_and_position_on_view_excluding("same", "main", &[])
            .unwrap()
            .unwrap()
            .0,
        canonical_inode
    );
    {
        let txn = repo.pristine().read_txn().unwrap();
        assert_ne!(txn.get_inode("same").unwrap().unwrap(), canonical_inode);
    }
    let record = repo
        .record_foreign_moved_file(
            "main",
            "moved",
            "same",
            b"original alpha\n",
            b"original alpha\n",
            false,
        )
        .unwrap();
    assert_eq!(
        record.crdt_ops().unwrap().trunk_id(),
        canonical_trunk,
        "a foreign edit must follow the requested closure's inode, not current PATH_TRUNK"
    );
}

#[test]
fn audit_backslash_and_directory_paths_remain_distinct() {
    check_case(
        vec![vec![
            ("dir\\file", b"literal backslash\n", 0o100644),
            ("dir/file", b"nested file\n", 0o100644),
        ]],
        false,
    );
}

#[test]
fn foreign_edit_ignores_other_views_branch_lifecycle() {
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let git = GitRepository::init(root.path()).unwrap();
    let tip = commit(&git, &[("same", b"original alpha\n", 0o100644)], &[]);
    import(root.path(), &git, tip, config.path());
    let mut repo = atomic_repository::Repository::open(root.path()).unwrap();
    let expected = repo
        .record_foreign_modified_file_on_view_excluding(
            "same",
            b"original alpha\n",
            b"edited alpha\n",
            true,
            "main",
            &[],
        )
        .unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    repo.create_view_from("side", "main").unwrap();
    repo.switch_view(wc, "side").unwrap();
    std::fs::write(
        root.path().join("same"),
        b"unrelated side replacement\nextra line\n",
    )
    .unwrap();
    repo.record_all(wc, "side edit").unwrap();
    let actual = repo
        .record_foreign_modified_file_on_view_excluding(
            "same",
            b"original alpha\n",
            b"edited alpha\n",
            true,
            "main",
            &[],
        )
        .unwrap();
    assert_eq!(
        format!("{:?}", actual.crdt_ops()),
        format!("{:?}", expected.crdt_ops()),
        "a foreign edit must tombstone only the requested closure's branches"
    );
    let deleted = repo
        .record_foreign_deleted_file_on_view_excluding("same", "main", &[])
        .unwrap();
    let old_branches: Vec<_> = expected
        .crdt_ops()
        .unwrap()
        .line_ops()
        .iter()
        .filter(|line| matches!(line.operation(), atomic_core::crdt::BranchOp::Delete { .. }))
        .map(|line| line.branch_id())
        .collect();
    assert_eq!(
        deleted
            .crdt_ops()
            .unwrap()
            .line_ops()
            .iter()
            .map(|line| line.branch_id())
            .collect::<Vec<_>>(),
        old_branches
    );
}

#[test]
fn checked_out_branch_survives_later_sorting_refs() {
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let git = GitRepository::init(root.path()).unwrap();
    let main = commit(&git, &[("main.txt", b"main checkout\n", 0o100644)], &[]);
    let side = commit(&git, &[("side.txt", b"other view\n", 0o100644)], &[main]);
    git.reference("refs/heads/z-side", side, true, "fixture")
        .unwrap();
    import(root.path(), &git, main, config.path());
    let repo = atomic_repository::Repository::open(root.path()).unwrap();
    assert_eq!(repo.current_view(), "main");
    assert_eq!(
        std::fs::read(root.path().join("main.txt")).unwrap(),
        b"main checkout\n"
    );
    assert!(!root.path().join("side.txt").exists());
    assert_eq!(
        repo.get_file_content("main.txt").unwrap().unwrap(),
        b"main checkout\n"
    );
}
