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
    let mut command = Command::new(env!("CARGO_BIN_EXE_atomic"));
    for (key, _) in std::env::vars().filter(|(key, _)| key.starts_with("ATOMIC_")) {
        command.env_remove(key);
    }
    let output = command
        .current_dir(root)
        .env("ATOMIC_CONFIG_DIR", config)
        .env("ATOMIC_SERVICE", "local")
        .env("ATOMIC_NONINTERACTIVE", "1")
        .args(["git", "import", "--no-vault", "--no-color"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "import failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn generated_file_deletion_preserves_lifecycle_after_modification() {
    for initial in [b"one\ntwo\n".as_slice(), b"".as_slice()] {
        let root = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        let git = GitRepository::init(root.path()).unwrap();
        let first = commit(
            &git,
            &[
                ("yarn.lock", initial, 0o100644),
                ("keep.txt", b"keep\n", 0o100644),
            ],
            &[],
        );
        let second = commit(
            &git,
            &[
                ("yarn.lock", b"one\nchanged\nthree\n", 0o100644),
                ("keep.txt", b"keep\n", 0o100644),
            ],
            &[first],
        );
        // Also cover deleting an empty generated file without a content edge.
        let parent = if initial.is_empty() { first } else { second };
        let tip = commit(&git, &[("keep.txt", b"keep\n", 0o100644)], &[parent]);
        import(root.path(), &git, tip, config.path());
        let mut repo = atomic_repository::Repository::open(root.path()).unwrap();
        assert_eq!(repo.get_file_content("yarn.lock").unwrap(), None);
        assert_eq!(
            repo.get_file_content("keep.txt").unwrap().unwrap(),
            b"keep\n"
        );
        let mut history = repo
            .log(atomic_repository::HistoryOptions::default())
            .unwrap();
        history.sort_by_key(|entry| entry.sequence);
        let changes: Vec<_> = history
            .iter()
            .map(|entry| repo.load_change(&entry.hash).unwrap())
            .collect();
        assert!(changes.iter().any(|change| {
            change.hunks().iter().any(|hunk| {
            matches!(hunk, atomic_core::change::GraphOp::FileDel {path, ..} if path == "yarn.lock")
        })
        }));
        repo.create_view_from("roundtrip", "main").unwrap();
        let wc = repo.require_working_copy_id().unwrap();
        repo.switch_view(wc, "roundtrip").unwrap();
        repo.switch_view(wc, "main").unwrap();
        assert_eq!(repo.get_file_content("yarn.lock").unwrap(), None);

        // Reconstruct only from canonical changes, without the importer's
        // indexes/journal, using opposite valid dependency orders for any
        // independent changes.
        for reverse in [false, true] {
            let rebuilt_dir = tempfile::tempdir().unwrap();
            let rebuilt = atomic_repository::Repository::init(rebuilt_dir.path()).unwrap();
            let mut hashes: Vec<_> = changes
                .iter()
                .map(|change| rebuilt.save_change(change).unwrap())
                .collect();
            if reverse {
                hashes.reverse();
            }
            let mut inserted = std::collections::HashSet::new();
            while !hashes.is_empty() {
                let ready = hashes
                    .iter()
                    .position(|hash| {
                        rebuilt
                            .load_change(hash)
                            .unwrap()
                            .dependencies()
                            .iter()
                            .all(|dependency| inserted.contains(dependency))
                    })
                    .expect("history must contain the complete dependency closure");
                let hash = hashes.remove(ready);
                rebuilt.insert_change(&hash, Default::default()).unwrap();
                inserted.insert(hash);
            }
            drop(rebuilt);
            let rebuilt = atomic_repository::Repository::open(rebuilt_dir.path()).unwrap();
            rebuilt
                .materialize(rebuilt.require_working_copy_id().unwrap())
                .unwrap();
            assert_eq!(rebuilt.get_file_content("yarn.lock").unwrap(), None);
            assert!(!rebuilt_dir.path().join("yarn.lock").exists());
            assert_eq!(
                std::fs::read(rebuilt_dir.path().join("keep.txt")).unwrap(),
                b"keep\n"
            );
        }
    }
}

#[test]
fn truncating_generated_file_keeps_the_empty_file() {
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let git = GitRepository::init(root.path()).unwrap();
    let first = commit(&git, &[("yarn.lock", b"one\ntwo\n", 0o100644)], &[]);
    let tip = commit(&git, &[("yarn.lock", b"", 0o100644)], &[first]);
    import(root.path(), &git, tip, config.path());
    let repo = atomic_repository::Repository::open(root.path()).unwrap();
    assert_eq!(
        repo.get_file_content("yarn.lock").unwrap(),
        Some(Vec::new())
    );
}

#[test]
fn merge_rename_keeps_old_path_removal_when_side_parent_modifies_destination() {
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let git = GitRepository::init(root.path()).unwrap();
    let original = (0..30)
        .map(|i| format!("unchanged line {i}\n"))
        .collect::<String>();
    let renamed = format!("{original}side addition\n");
    let resolved = format!("{renamed}merge addition\n");
    let first = commit(&git, &[("README.md", original.as_bytes(), 0o100644)], &[]);
    let side = commit(
        &git,
        &[("README.rst", renamed.as_bytes(), 0o100644)],
        &[first],
    );
    let main = commit(
        &git,
        &[
            ("README.md", original.as_bytes(), 0o100644),
            ("main.txt", b"main\n", 0o100644),
        ],
        &[first],
    );
    let tip = commit(
        &git,
        &[
            ("README.rst", resolved.as_bytes(), 0o100644),
            ("main.txt", b"main\n", 0o100644),
        ],
        &[main, side],
    );
    import(root.path(), &git, tip, config.path());
    let repo = atomic_repository::Repository::open(root.path()).unwrap();
    assert_eq!(repo.get_file_content("README.md").unwrap(), None);
    assert_eq!(
        repo.get_file_content("README.rst").unwrap().unwrap(),
        resolved.as_bytes()
    );
}

#[test]
fn merge_rename_over_symlink_deletes_the_displaced_identity() {
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let git = GitRepository::init(root.path()).unwrap();
    let original = (0..30)
        .map(|i| format!("readme line {i}\n"))
        .collect::<String>();
    let result = format!("{original}updated readme\n");
    let first_files = [
        ("README.md", b"docs/README.md".as_slice(), 0o120000),
        ("docs/README.md", original.as_bytes(), 0o100644),
    ];
    // The displaced inode itself arrived by a previous move. Two historical
    // moves to README.md must not be mistaken for two live name claimants.
    let before_move = commit(
        &git,
        &[
            ("link.md", b"docs/README.md".as_slice(), 0o120000),
            ("docs/README.md", original.as_bytes(), 0o100644),
        ],
        &[],
    );
    let first = commit(&git, &first_files, &[before_move]);
    let side = commit(
        &git,
        &[("README.md", result.as_bytes(), 0o100644)],
        &[first],
    );
    let mut main_files = first_files.to_vec();
    main_files.push(("main.txt", b"main\n", 0o100644));
    let main = commit(&git, &main_files, &[first]);
    let tip = commit(
        &git,
        &[
            ("README.md", result.as_bytes(), 0o100644),
            ("main.txt", b"main\n", 0o100644),
        ],
        &[main, side],
    );
    import(root.path(), &git, tip, config.path());
    let repo = atomic_repository::Repository::open(root.path()).unwrap();
    assert_eq!(
        repo.get_file_content("README.md").unwrap().unwrap(),
        result.as_bytes()
    );
    assert_eq!(repo.get_file_content("docs/README.md").unwrap(), None);
}

#[test]
fn symlink_to_regular_file_survives_simultaneous_sibling_deletion() {
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let git = GitRepository::init(root.path()).unwrap();
    let first = commit(
        &git,
        &[
            ("LICENSE", b"license text\n", 0o100644),
            ("a/LICENSE", b"../LICENSE", 0o120000),
            ("b/LICENSE", b"../LICENSE", 0o120000),
        ],
        &[],
    );
    let result = [
        ("LICENSE", b"license text\n".as_slice(), 0o100644),
        ("b/LICENSE", b"license text\n".as_slice(), 0o100644),
        ("c/LICENSE", b"license text\n".as_slice(), 0o100644),
    ];
    let side = commit(&git, &result, &[first]);
    let tip = commit(&git, &result, &[first, side]);
    import(root.path(), &git, tip, config.path());
    let repo = atomic_repository::Repository::open(root.path()).unwrap();
    assert_eq!(
        repo.get_file_content("b/LICENSE").unwrap().unwrap(),
        b"license text\n"
    );
    assert_eq!(repo.get_file_content("a/LICENSE").unwrap(), None);
}
