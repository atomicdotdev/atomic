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
    commit_named(git, files, parents, "fixture")
}

fn commit_named(
    git: &GitRepository,
    files: &[(&str, &[u8], i32)],
    parents: &[Oid],
    message: &str,
) -> Oid {
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
        message,
        &tree,
        &parents.iter().collect::<Vec<_>>(),
    )
    .unwrap()
}

fn import(root: &Path, git: &GitRepository, tip: Oid, config: &Path) {
    import_with_args(root, git, tip, config, &[]);
}

fn import_with_args(root: &Path, git: &GitRepository, tip: Oid, config: &Path, args: &[&str]) {
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
        .args(args)
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

// Two independent spelling fixes converge to identical bytes. The second
// merge has no Git tree delta, but still joins distinct semantic writers.
#[test]
fn identical_side_edits_need_a_causal_merge_resolution() {
    for reverse in [false, true] {
        for args in [
            vec!["--all"],
            vec!["--all", "--with-crdt"],
            vec![],
            vec!["--with-crdt"],
        ] {
            let root = tempfile::tempdir().unwrap();
            let config = tempfile::tempdir().unwrap();
            let git = GitRepository::init(root.path()).unwrap();
            let base = commit_named(
                &git,
                &[("README.md", b"heading\nFamliar\ntail\n", 0o100644)],
                &[],
                "base",
            );
            let files: &[(&str, &[u8], i32)] =
                &[("README.md", b"heading\nFamiliar\ntail\n", 0o100644)];
            let a = commit_named(&git, files, &[base], "fix A");
            let b = commit_named(&git, files, &[base], "fix B");
            let (first, second) = if reverse { (b, a) } else { (a, b) };
            let merge1 = commit_named(&git, files, &[base, first], "merge first fix");
            let tip = commit_named(&git, files, &[merge1, second], "merge identical second fix");
            import_with_args(root.path(), &git, tip, config.path(), &args);
            let mut repo = atomic_repository::Repository::open(root.path()).unwrap();
            assert_eq!(
                repo.get_file_content("README.md").unwrap().unwrap(),
                files[0].1
            );
            repo.create_view_from("roundtrip", "main").unwrap();
            let wc = repo.require_working_copy_id().unwrap();
            repo.switch_view(wc, "roundtrip").unwrap();
            repo.switch_view(wc, "main").unwrap();
            drop(repo);
            let repo = atomic_repository::Repository::open(root.path()).unwrap();
            assert_eq!(
                repo.get_file_content("README.md").unwrap().unwrap(),
                files[0].1
            );
            if args.contains(&"--all") {
                assert_canonical_rebuild(&repo, files);
            }
        }
    }
}

#[test]
fn full_history_reconciles_unchanged_paths_after_criss_cross_merges() {
    for reverse in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        let git = GitRepository::init(root.path()).unwrap();
        let old: &[(&str, &[u8], i32)] = &[("file.txt", b"before\n", 0o100644)];
        let files: &[(&str, &[u8], i32)] = &[("file.txt", b"after\n", 0o100644)];
        let base = commit_named(&git, old, &[], "base");
        let left = commit_named(&git, files, &[base], "left edit");
        let right = commit_named(
            &git,
            &[("file.txt", b"other\n", 0o100644)],
            &[base],
            "right edit",
        );
        let a = commit_named(&git, files, &[left, right], "merge left first");
        let b = commit_named(&git, files, &[right, left], "merge right first");
        assert_eq!(git.merge_bases(a, b).unwrap().len(), 2);
        let parents = if reverse { [b, a] } else { [a, b] };
        let tip = commit_named(&git, files, &parents, "merge criss cross");
        for parent in parents {
            assert_eq!(
                git.find_commit(parent).unwrap().tree_id(),
                git.find_commit(tip).unwrap().tree_id()
            );
        }
        import_with_args(
            root.path(),
            &git,
            tip,
            config.path(),
            &["--all", "--with-crdt"],
        );
        let repo = atomic_repository::Repository::open(root.path()).unwrap();
        assert_eq!(
            repo.get_file_content("file.txt").unwrap().unwrap(),
            files[0].1
        );
        assert_canonical_rebuild(&repo, files);
    }
}

#[test]
fn full_history_reconciles_independent_reverts_without_git_tree_changes() {
    for reverse in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        let git = GitRepository::init(root.path()).unwrap();
        let files: &[(&str, &[u8], i32)] = &[("file.txt", b"original\n", 0o100644)];
        let base = commit_named(&git, files, &[], "base");
        let left = commit_named(
            &git,
            &[("file.txt", b"left\n", 0o100644)],
            &[base],
            "left edit",
        );
        let right = commit_named(
            &git,
            &[("file.txt", b"right\n", 0o100644)],
            &[base],
            "right edit",
        );
        let a = commit_named(&git, files, &[left], "revert left");
        let b = commit_named(&git, files, &[right], "revert right");
        let parents = if reverse { [b, a] } else { [a, b] };
        let tip = commit_named(&git, files, &parents, "merge independent reverts");
        assert_eq!(git.merge_base(a, b).unwrap(), base);
        import_with_args(
            root.path(),
            &git,
            tip,
            config.path(),
            &["--all", "--with-crdt"],
        );
        let repo = atomic_repository::Repository::open(root.path()).unwrap();
        assert_eq!(
            repo.get_file_content("file.txt").unwrap().unwrap(),
            files[0].1
        );
        assert_canonical_rebuild(&repo, files);
    }
}

#[test]
fn full_history_references_parent_closure_after_visiting_a_sibling() {
    for with_crdt in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        let git = GitRepository::init(root.path()).unwrap();
        let make = |files: &[(&str, &[u8], i32)], parents: &[Oid], time: i64| {
            let signature = Signature::new(
                "Import regression",
                "import@example.com",
                &git2::Time::new(time, 0),
            )
            .unwrap();
            let tree = git.find_tree(tree(&git, files)).unwrap();
            let parents: Vec<_> = parents
                .iter()
                .map(|id| git.find_commit(*id).unwrap())
                .collect();
            git.commit(
                None,
                &signature,
                &signature,
                "fixture",
                &tree,
                &parents.iter().collect::<Vec<_>>(),
            )
            .unwrap()
        };
        let base = make(&[("base.txt", b"base\n", 0o100644)], &[], 1);
        let a = make(
            &[
                ("base.txt", b"base\n", 0o100644),
                ("a.txt", b"before\n", 0o100644),
            ],
            &[base],
            2,
        );
        let b = make(
            &[
                ("base.txt", b"base\n", 0o100644),
                ("b.txt", b"sibling\n", 0o100644),
            ],
            &[base],
            3,
        );
        let edited = make(
            &[
                ("base.txt", b"base\n", 0o100644),
                ("a.txt", b"after\n", 0o100644),
            ],
            &[a],
            4,
        );
        let files: &[(&str, &[u8], i32)] = &[
            ("base.txt", b"base\n", 0o100644),
            ("a.txt", b"after\n", 0o100644),
            ("b.txt", b"sibling\n", 0o100644),
        ];
        let tip = make(files, &[edited, b], 5);
        let args = if with_crdt {
            vec!["--all", "--with-crdt"]
        } else {
            vec!["--all"]
        };
        import_with_args(root.path(), &git, tip, config.path(), &args);
        let repo = atomic_repository::Repository::open(root.path()).unwrap();
        for (path, expected, _) in files {
            assert_eq!(repo.get_file_content(path).unwrap().unwrap(), *expected);
        }
        // Editing a leg must retain its introducing inode/trunk instead of
        // synthesizing a new FileAdd after another leg hid the parent.
        let hash = repo.checked_git_sha(&edited.to_string()).unwrap().unwrap();
        let change = repo.load_change(&hash).unwrap();
        assert!(!change.hunks().iter().any(
            |op| matches!(op, atomic_core::change::GraphOp::FileAdd { path, .. } if path == "a.txt")
        ));
    }
}

fn assert_canonical_rebuild(repo: &atomic_repository::Repository, files: &[(&str, &[u8], i32)]) {
    assert_canonical_rebuild_without(repo, files, &[]);
}

#[test]
fn full_history_merge_retains_a_rename_replacing_a_symlink() {
    for reverse_parents in [false, true] {
        for semantic in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let config = tempfile::tempdir().unwrap();
            let git = GitRepository::init(root.path()).unwrap();
            let text = b"# Readme\n\nThe canonical project documentation.\n".as_slice();
            let base_files = [
                ("README.md", b"docs/README.md".as_slice(), 0o120000),
                ("docs/README.md", text, 0o100644),
                ("left.txt", b"base\n".as_slice(), 0o100644),
            ];
            let base = commit_named(&git, &base_files, &[], "base symlink and document");
            let mut left_files = base_files;
            left_files[2].1 = b"left\n";
            let left = commit_named(&git, &left_files, &[base], "independent left edit");
            let right_files = [
                ("README.md", text, 0o100644),
                ("left.txt", b"base\n".as_slice(), 0o100644),
            ];
            let right = commit_named(&git, &right_files, &[base], "move document over symlink");
            let files = [
                ("README.md", text, 0o100644),
                ("left.txt", b"left\n".as_slice(), 0o100644),
            ];
            let parents = if reverse_parents {
                [right, left]
            } else {
                [left, right]
            };
            let merge = commit_named(&git, &files, &parents, "merge replacement rename");
            let args = if semantic {
                vec!["--all", "--with-crdt"]
            } else {
                vec!["--all"]
            };
            import_with_args(root.path(), &git, merge, config.path(), &args);
            let repo = atomic_repository::Repository::open(root.path()).unwrap();
            assert_eq!(
                repo.get_file_content("README.md").unwrap(),
                Some(text.to_vec())
            );
            assert_canonical_rebuild_without(&repo, &files, &["docs/README.md"]);
        }
    }
}

fn assert_canonical_rebuild_without(
    repo: &atomic_repository::Repository,
    files: &[(&str, &[u8], i32)],
    absent: &[&str],
) {
    let history = repo
        .log(atomic_repository::HistoryOptions::default())
        .unwrap();
    let changes: Vec<_> = history
        .iter()
        .map(|entry| repo.load_change(&entry.hash).unwrap())
        .collect();
    for reverse in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let rebuilt = atomic_repository::Repository::init(root.path()).unwrap();
        let mut pending: Vec<_> = changes
            .iter()
            .map(|change| rebuilt.save_change(change).unwrap())
            .collect();
        if reverse {
            pending.reverse();
        }
        let mut inserted = std::collections::HashSet::new();
        while !pending.is_empty() {
            let index = pending
                .iter()
                .position(|hash| {
                    let change = rebuilt.load_change(hash).unwrap();
                    change
                        .dependencies()
                        .iter()
                        .chain(change.causal_frontier().roots().iter())
                        .all(|dep| inserted.contains(dep))
                })
                .expect("complete dependency closure");
            let hash = pending.remove(index);
            rebuilt.insert_change(&hash, Default::default()).unwrap();
            inserted.insert(hash);
        }
        drop(rebuilt);
        let rebuilt = atomic_repository::Repository::open(root.path()).unwrap();
        rebuilt
            .materialize(rebuilt.require_working_copy_id().unwrap())
            .unwrap();
        for path in absent {
            assert_eq!(rebuilt.get_file_content(path).unwrap(), None);
            assert!(!root.path().join(path).exists());
        }
        for (path, bytes, mode) in files {
            assert_eq!(rebuilt.get_file_content(path).unwrap().unwrap(), *bytes);
            if let Some((parent, _)) = path.rsplit_once('/') {
                let (_, _, directory_position) = rebuilt
                    .resolve_single_path_claim(rebuilt.current_view(), parent)
                    .unwrap()
                    .unwrap();
                let (claim, _, _) = rebuilt
                    .resolve_single_path_claim(rebuilt.current_view(), path)
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    claim.parent.end_pos(),
                    directory_position,
                    "child '{path}' must connect to its surviving directory in GRAPH"
                );
            }
            let target = root.path().join(path);
            let metadata = std::fs::symlink_metadata(&target).unwrap();
            if *mode == 0o120000 {
                use std::os::unix::ffi::OsStrExt;
                assert!(metadata.file_type().is_symlink());
                assert_eq!(
                    std::fs::read_link(target).unwrap().as_os_str().as_bytes(),
                    *bytes
                );
            } else {
                use std::os::unix::fs::PermissionsExt;
                assert!(!metadata.file_type().is_symlink());
                assert_eq!(metadata.permissions().mode() & 0o777, *mode as u32 & 0o777);
                assert_eq!(std::fs::read(target).unwrap(), *bytes);
            }
        }
    }
}

#[test]
fn identical_truncations_keep_an_empty_file_at_merge() {
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let git = GitRepository::init(root.path()).unwrap();
    let base = commit_named(&git, &[("file.txt", b"old\n", 0o100644)], &[], "base");
    let files: &[(&str, &[u8], i32)] = &[("file.txt", b"", 0o100644)];
    let a = commit_named(&git, files, &[base], "truncate A");
    let b = commit_named(&git, files, &[base], "truncate B");
    let tip = commit_named(&git, files, &[a, b], "merge truncations");
    import_with_args(
        root.path(),
        &git,
        tip,
        config.path(),
        &["--all", "--with-crdt"],
    );
    let repo = atomic_repository::Repository::open(root.path()).unwrap();
    assert_eq!(repo.get_file_content("file.txt").unwrap(), Some(Vec::new()));
}

#[test]
fn full_history_records_explicit_resolution_for_independent_same_name_creates() {
    for reverse in [false, true] {
        for selected in [
            b"root=true\nindent=tab\n".as_slice(),
            b"root=true\nend=lf\n".as_slice(),
            b"root=true\nindent=tab\nend=lf\n".as_slice(),
        ] {
            let root = tempfile::tempdir().unwrap();
            let config = tempfile::tempdir().unwrap();
            let git = GitRepository::init(root.path()).unwrap();
            let base = commit_named(&git, &[("keep", b"keep\n", 0o100644)], &[], "base");
            let a = commit_named(
                &git,
                &[
                    ("keep", b"keep\n", 0o100644),
                    (".editorconfig", b"root=true\nindent=tab\n", 0o100644),
                ],
                &[base],
                "config A",
            );
            let b = commit_named(
                &git,
                &[
                    ("keep", b"keep\n", 0o100644),
                    (".editorconfig", b"root=true\nend=lf\n", 0o100644),
                ],
                &[base],
                "config B",
            );
            let files: &[(&str, &[u8], i32)] = &[
                ("keep", b"keep\n", 0o100644),
                (".editorconfig", selected, 0o100644),
            ];
            let parents = if reverse { vec![b, a] } else { vec![a, b] };
            let tip = commit_named(&git, files, &parents, "resolve config");
            import_with_args(
                root.path(),
                &git,
                tip,
                config.path(),
                &["--all", "--with-crdt"],
            );
            let repo = atomic_repository::Repository::open(root.path()).unwrap();
            let hash = repo.checked_git_sha(&tip.to_string()).unwrap().unwrap();
            let change = repo.load_change(&hash).unwrap();
            assert!(change
                .hunks()
                .iter()
                .any(|op| matches!(op, atomic_core::change::GraphOp::SolveNameConflict { .. })));
            assert_canonical_rebuild(&repo, files);
        }
    }
}

#[test]
fn full_history_resolves_identical_independent_creates_without_a_git_merge_diff() {
    for reverse in [false, true] {
        for eager in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let config = tempfile::tempdir().unwrap();
            let git = GitRepository::init(root.path()).unwrap();
            let base = commit_named(&git, &[("keep", b"keep\n", 0o100644)], &[], "base");
            let files: &[(&str, &[u8], i32)] = &[
                ("keep", b"keep\n", 0o100644),
                ("LICENSE", b"MIT license\n", 0o100644),
            ];
            let a = commit_named(&git, files, &[base], "license A");
            let b = commit_named(&git, files, &[base], "license B");
            let parents = if reverse { vec![b, a] } else { vec![a, b] };
            let tip = commit_named(&git, files, &parents, "merge identical licenses");
            let args = if eager {
                vec!["--all", "--with-crdt"]
            } else {
                vec!["--all"]
            };
            import_with_args(root.path(), &git, tip, config.path(), &args);
            let repo = atomic_repository::Repository::open(root.path()).unwrap();
            let hash = repo.checked_git_sha(&tip.to_string()).unwrap().unwrap();
            let change = repo.load_change(&hash).unwrap();
            assert!(change
                .hunks()
                .iter()
                .any(|op| matches!(op, atomic_core::change::GraphOp::SolveNameConflict { .. })));
            assert_canonical_rebuild(&repo, files);
        }
    }
}

#[test]
fn full_history_merge_preserves_new_file_kinds_and_permission_only_changes() {
    for reverse in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        let git = GitRepository::init(root.path()).unwrap();
        let base = commit_named(&git, &[("keep", b"keep\n", 0o100644)], &[], "base");
        let a = commit_named(&git, &[("keep", b"keep\n", 0o100644)], &[base], "left");
        let b = commit_named(&git, &[("keep", b"keep\n", 0o100644)], &[base], "right");
        let files: &[(&str, &[u8], i32)] = &[
            ("keep", b"keep\n", 0o100755),
            ("script.sh", b"#!/bin/sh\necho hello\n", 0o100755),
            ("link", b"keep", 0o120000),
        ];
        let parents = if reverse { vec![b, a] } else { vec![a, b] };
        let tip = commit_named(&git, files, &parents, "merge adds executable and symlink");
        import_with_args(
            root.path(),
            &git,
            tip,
            config.path(),
            &["--all", "--with-crdt"],
        );
        let repo = atomic_repository::Repository::open(root.path()).unwrap();
        assert_canonical_rebuild(&repo, files);
    }
}

#[test]
fn merge_name_resolution_does_not_resurrect_the_losing_rename_source() {
    for reverse in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        let git = GitRepository::init(root.path()).unwrap();
        let base = commit_named(
            &git,
            &[(
                "old.sh",
                b"one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n",
                0o100755,
            )],
            &[],
            "base",
        );
        let a = commit_named(
            &git,
            &[(
                "new.sh",
                b"one\ntwo\nthree\nfour\nfive\nsix\nseven\nchanged\n",
                0o100755,
            )],
            &[base],
            "rename and edit",
        );
        let files: &[(&str, &[u8], i32)] = &[("new.sh", b"completely replaced script\n", 0o100755)];
        let b = commit_named(&git, files, &[base], "delete and recreate");
        let parents = if reverse { vec![b, a] } else { vec![a, b] };
        let tip = commit_named(&git, files, &parents, "retain replacement identity");
        import_with_args(
            root.path(),
            &git,
            tip,
            config.path(),
            &["--all", "--with-crdt"],
        );
        let repo = atomic_repository::Repository::open(root.path()).unwrap();
        assert_eq!(repo.get_file_content("old.sh").unwrap(), None);
        assert_canonical_rebuild_without(&repo, files, &["old.sh"]);
    }
}

#[test]
fn full_history_attributes_deletion_only_changes_after_switching_legs() {
    use atomic_core::pristine::GraphTxnT;
    for reverse in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        let git = GitRepository::init(root.path()).unwrap();
        let make = |bytes: &[u8], parents: &[Oid], time: i64| {
            let signature = Signature::new(
                "Import regression",
                "import@example.com",
                &git2::Time::new(time, 0),
            )
            .unwrap();
            let tree = git
                .find_tree(tree(&git, &[("file.txt", bytes, 0o100644)]))
                .unwrap();
            let parents: Vec<_> = parents
                .iter()
                .map(|id| git.find_commit(*id).unwrap())
                .collect();
            git.commit(
                None,
                &signature,
                &signature,
                "fixture",
                &tree,
                &parents.iter().collect::<Vec<_>>(),
            )
            .unwrap()
        };
        let base = make(b"seed\n", &[], 1);
        let add = make(b"seed\ntemporary\n", &[base], if reverse { 3 } else { 2 });
        let delete = make(b"seed\n", &[add], if reverse { 4 } else { 3 });
        let sibling = make(b"other\n", &[base], if reverse { 2 } else { 4 });
        let parents = if reverse {
            vec![sibling, delete]
        } else {
            vec![delete, sibling]
        };
        let tip = make(b"other\n", &parents, 5);
        import_with_args(
            root.path(),
            &git,
            tip,
            config.path(),
            &["--all", "--with-crdt"],
        );
        let repo = atomic_repository::Repository::open(root.path()).unwrap();
        let hash = repo.checked_git_sha(&delete.to_string()).unwrap().unwrap();
        let txn = repo.pristine().read_txn().unwrap();
        let id = txn.get_internal(&hash).unwrap().unwrap();
        assert!(
            !txn.has_change_in_graph(id).unwrap(),
            "fixture must exercise a deletion with no new vertices"
        );
        assert!(txn.was_change_applied(id).unwrap());
        drop(txn);
        assert_eq!(
            repo.get_file_content("file.txt").unwrap(),
            Some(b"other\n".to_vec())
        );
        assert_canonical_rebuild(&repo, &[("file.txt", b"other\n", 0o100644)]);
    }
}

#[test]
fn full_history_attributes_sibling_edits_across_a_rename() {
    for reverse in [false, true] {
        for eager in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let config = tempfile::tempdir().unwrap();
            let git = GitRepository::init(root.path()).unwrap();
            let make = |path: &str, bytes: &[u8], parents: &[Oid], time: i64| {
                let signature = Signature::new(
                    "Import regression",
                    "import@example.com",
                    &git2::Time::new(time, 0),
                )
                .unwrap();
                let tree = git
                    .find_tree(tree(&git, &[(path, bytes, 0o100644)]))
                    .unwrap();
                let parents: Vec<_> = parents
                    .iter()
                    .map(|id| git.find_commit(*id).unwrap())
                    .collect();
                git.commit(
                    None,
                    &signature,
                    &signature,
                    "fixture",
                    &tree,
                    &parents.iter().collect::<Vec<_>>(),
                )
                .unwrap()
            };
            let base = make("old.txt", b"one\ntwo\nthree\nfour\nfive\n", &[], 1);
            // Visit the old-name sibling before interpreting the rename.
            let sibling = make(
                "old.txt",
                b"one\ntwo\nthree\nfour\nfive\nsibling\n",
                &[base],
                2,
            );
            let renamed = make("new.txt", b"ONE\ntwo\nthree\nfour\nfive\n", &[base], 3);
            let parents = if reverse {
                vec![sibling, renamed]
            } else {
                vec![renamed, sibling]
            };
            let expected = b"ONE\ntwo\nthree\nfour\nfive\nsibling\n";
            let tip = make("new.txt", expected, &parents, 4);
            let args: &[&str] = if eager {
                &["--all", "--with-crdt"]
            } else {
                &["--all"]
            };
            import_with_args(root.path(), &git, tip, config.path(), args);
            let mut repo = atomic_repository::Repository::open(root.path()).unwrap();
            assert_eq!(repo.get_file_content("old.txt").unwrap(), None);
            assert_eq!(
                repo.get_file_content("new.txt").unwrap(),
                Some(expected.to_vec())
            );
            repo.create_view_from("roundtrip", "main").unwrap();
            let wc = repo.require_working_copy_id().unwrap();
            repo.switch_view(wc, "roundtrip").unwrap();
            repo.switch_view(wc, "main").unwrap();
            assert_canonical_rebuild(&repo, &[("new.txt", expected, 0o100644)]);
        }
    }
}

#[test]
fn whole_file_merge_resolves_concurrent_line_delete_and_modify() {
    for reverse in [false, true] {
        for eager in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let config = tempfile::tempdir().unwrap();
            let git = GitRepository::init(root.path()).unwrap();
            let base = commit_named(
                &git,
                &[("file.txt", b"one\ntwo\nthree\nfour\n", 0o100644)],
                &[],
                "base",
            );
            let deleted = commit_named(
                &git,
                &[("file.txt", b"one\nfour\n", 0o100644)],
                &[base],
                "delete middle lines",
            );
            let modified = commit_named(
                &git,
                &[("file.txt", b"one\nTWO\nthree\nfour\n", 0o100644)],
                &[base],
                "modify deleted line",
            );
            let parents = if reverse {
                vec![modified, deleted]
            } else {
                vec![deleted, modified]
            };
            let expected = b"resolved\nwhole file\n";
            let tip = commit_named(
                &git,
                &[("file.txt", expected, 0o100644)],
                &parents,
                "resolve all lines",
            );
            let args: &[&str] = if eager {
                &["--all", "--with-crdt"]
            } else {
                &["--all"]
            };
            import_with_args(root.path(), &git, tip, config.path(), args);
            let repo = atomic_repository::Repository::open(root.path()).unwrap();
            assert_eq!(
                repo.get_file_content("file.txt").unwrap(),
                Some(expected.to_vec())
            );
            // The replacement must explicitly supersede the base identities,
            // including the lines already tombstoned by one parent.
            let history = repo
                .log(atomic_repository::HistoryOptions::default())
                .unwrap();
            let changes: Vec<_> = history
                .iter()
                .map(|entry| repo.load_change(&entry.hash).unwrap())
                .collect();
            let base_change = changes
                .iter()
                .find(|change| {
                    change.file_ops().iter().any(|ops| {
                        matches!(
                            ops.trunk_op(),
                            Some(atomic_core::crdt::TrunkOp::Create { .. })
                        )
                    })
                })
                .unwrap();
            let base_hash = base_change.hash().unwrap();
            use atomic_core::pristine::GraphTxnT;
            let txn = repo.pristine().read_txn().unwrap();
            let base_id = txn.get_internal(&base_hash).unwrap().unwrap();
            let base_branches: Vec<_> = base_change
                .file_ops()
                .iter()
                .flat_map(|ops| ops.line_ops())
                .map(|line| {
                    atomic_core::crdt::BranchId::new(base_id, line.branch_id().branch_idx())
                })
                .collect();
            let merged = changes
                .iter()
                .find(|change| change.hashed.header.message == "resolve all lines")
                .unwrap();
            let deleted: std::collections::HashSet<_> = merged
                .file_ops()
                .iter()
                .flat_map(|ops| ops.line_ops())
                .filter(|line| {
                    matches!(line.operation(), atomic_core::crdt::BranchOp::Delete { .. })
                })
                .map(|line| line.branch_id())
                .collect();
            assert!(
                base_branches.iter().all(|branch| deleted.contains(branch)),
                "merge must delete every observed base line"
            );
            drop(txn);
            assert_canonical_rebuild(&repo, &[("file.txt", expected, 0o100644)]);
        }
    }
}

#[test]
fn foreign_merge_resolves_directory_claims_and_retains_disjoint_children() {
    for reverse in [false, true] {
        for eager in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let config = tempfile::tempdir().unwrap();
            let git = GitRepository::init(root.path()).unwrap();
            let base = commit_named(&git, &[("keep", b"keep\n", 0o100644)], &[], "base");
            let left = commit_named(
                &git,
                &[
                    ("keep", b"keep\n", 0o100644),
                    (".vscode/settings.json", b"{}\n", 0o100644),
                    (".vscode/nested/left", b"left\n", 0o100644),
                ],
                &[base],
                "left directory",
            );
            let right = commit_named(
                &git,
                &[
                    ("keep", b"keep\n", 0o100644),
                    (".vscode/settings.json", b"{}\n", 0o100644),
                    (".vscode/nested/right", b"right\n", 0o100755),
                ],
                &[base],
                "right directory",
            );
            let files: &[(&str, &[u8], i32)] = &[
                ("keep", b"keep\n", 0o100644),
                (".vscode/settings.json", b"{}\n", 0o100644),
                (".vscode/nested/left", b"left\n", 0o100644),
                (".vscode/nested/right", b"right\n", 0o100755),
                (".vscode/new-at-merge", b"new\n", 0o100644),
            ];
            let parents = if reverse {
                vec![right, left]
            } else {
                vec![left, right]
            };
            let tip = commit_named(&git, files, &parents, "merge directory identities");
            let args: &[&str] = if eager {
                &["--all", "--with-crdt"]
            } else {
                &["--all"]
            };
            import_with_args(root.path(), &git, tip, config.path(), args);
            let mut repo = atomic_repository::Repository::open(root.path()).unwrap();
            repo.create_view_from("roundtrip", "main").unwrap();
            let wc = repo.require_working_copy_id().unwrap();
            repo.switch_view(wc, "roundtrip").unwrap();
            repo.switch_view(wc, "main").unwrap();
            assert_canonical_rebuild(&repo, files);
        }
    }
}

#[test]
fn merge_deletion_binds_the_existing_semantic_identity() {
    for reverse in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        let git = GitRepository::init(root.path()).unwrap();
        let base = commit_named(
            &git,
            &[
                ("keep", b"keep\n", 0o100644),
                ("removed", b"old\n", 0o100644),
            ],
            &[],
            "base",
        );
        let deleted = commit_named(
            &git,
            &[
                ("keep", b"left\n", 0o100644),
                ("removed", b"old\n", 0o100644),
            ],
            &[base],
            "left edit",
        );
        let edited = commit_named(
            &git,
            &[
                ("keep", b"keep\n", 0o100644),
                ("removed", b"changed\n", 0o100644),
            ],
            &[base],
            "edit",
        );
        let parents = if reverse {
            vec![edited, deleted]
        } else {
            vec![deleted, edited]
        };
        let tip = commit_named(
            &git,
            &[("keep", b"keep\n", 0o100644)],
            &parents,
            "resolve deletion",
        );
        import_with_args(
            root.path(),
            &git,
            tip,
            config.path(),
            &["--all", "--with-crdt"],
        );
        let repo = atomic_repository::Repository::open(root.path()).unwrap();
        let history = repo
            .log(atomic_repository::HistoryOptions::default())
            .unwrap();
        let change = history
            .iter()
            .map(|entry| repo.load_change(&entry.hash).unwrap())
            .find(|change| change.hashed.header.message == "resolve deletion")
            .unwrap();
        let ops = change
            .file_ops()
            .iter()
            .find(|ops| ops.path() == "removed")
            .unwrap();
        assert!(matches!(
            ops.trunk_op(),
            Some(atomic_core::crdt::TrunkOp::Delete { .. })
        ));
        assert!(
            !ops.trunk_id().change_id().is_root(),
            "merge deletion must name the prior trunk"
        );
        assert!(
            !ops.line_ops().is_empty(),
            "merge deletion must tombstone prior branches"
        );
        assert_canonical_rebuild_without(&repo, &[("keep", b"keep\n", 0o100644)], &["removed"]);
    }
}
