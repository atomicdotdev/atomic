use super::*;

#[test]
fn typechange_pair_is_independent_of_delta_order() {
    for added_first in [true, false] {
        let added = ParsedFile {
            path: "pkg/LICENSE".into(),
            operation: FileOperation::Added,
            new_content: Some(b"license text\n".to_vec()),
            old_content: None,
            old_path: None,
            diff_lines: None,
            new_mode: Some(0o100644),
        };
        let deleted = ParsedFile {
            path: "pkg/LICENSE".into(),
            operation: FileOperation::Deleted,
            new_content: None,
            old_content: Some(b"../LICENSE".to_vec()),
            old_path: None,
            diff_lines: None,
            new_mode: None,
        };
        let files = normalize_typechanges(if added_first {
            vec![added, deleted]
        } else {
            vec![deleted, added]
        });
        assert_eq!(files.len(), 1, "added_first={added_first}");
        assert_eq!(files[0].operation, FileOperation::Modified);
        assert_eq!(files[0].new_mode, Some(0o100644));
        assert_eq!(
            files[0].new_content.as_deref(),
            Some(b"license text\n".as_slice())
        );
        assert_eq!(
            files[0].old_content.as_deref(),
            Some(b"../LICENSE".as_slice())
        );
    }
}

fn audit_fallback_path(path: &[u8]) {
    let root = tempfile::tempdir().unwrap();
    let git = GitRepository::init(root.path()).unwrap();
    let signature = git2::Signature::now("fixture", "fixture@example.com").unwrap();
    let mut builder = git.treebuilder(None).unwrap();
    builder
        .insert(path, git.blob(b"old\n").unwrap(), 0o100644)
        .unwrap();
    let a_tree = git.find_tree(builder.write().unwrap()).unwrap();
    let a = git
        .commit(None, &signature, &signature, "first", &a_tree, &[])
        .unwrap();
    builder
        .insert(path, git.blob(b"new\n").unwrap(), 0o100644)
        .unwrap();
    let b_tree = git.find_tree(builder.write().unwrap()).unwrap();
    let b = git
        .commit(
            None,
            &signature,
            &signature,
            "second",
            &b_tree,
            &[&git.find_commit(a).unwrap()],
        )
        .unwrap();
    let parsed = parse_diff_files_via_git_cli(&git, b, a, &b_tree, &a_tree).unwrap();
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].path, atomic_repository::escape_repo_path(path));
    assert_eq!(parsed[0].new_content.as_deref(), Some(b"new\n".as_slice()));
}
#[test]
fn audit_fallback_ascii() {
    audit_fallback_path(b"plain.txt");
}
#[test]
fn audit_fallback_percent() {
    audit_fallback_path(b"a%41.txt");
}
#[test]
fn audit_fallback_tab() {
    audit_fallback_path(b"a\tb.txt");
}
#[test]
fn audit_fallback_unicode() {
    audit_fallback_path("中文.txt".as_bytes());
}

#[cfg(unix)]
#[test]
fn fallback_diff_survives_removed_process_cwd() {
    const CHILD: &str = "ATOMIC_FALLBACK_REMOVED_CWD_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let original = std::env::current_dir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        std::env::set_current_dir(cwd.path()).unwrap();
        // Only this child process uses the disposable directory as cwd.
        std::fs::remove_dir(cwd.path()).unwrap();
        audit_fallback_path(b"plain.txt");
        std::env::set_current_dir(original).unwrap();
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("commands::git::parallel::import_regression_tests::fallback_diff_survives_removed_process_cwd")
        .arg("--nocapture")
        .env(CHILD, "1")
        .output().unwrap();
    assert!(
        output.status.success(),
        "isolated child failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn fallback_newline() {
    audit_fallback_path(b"line\nbreak");
}
#[test]
fn fallback_non_utf8() {
    audit_fallback_path(b"raw-\xff");
}

#[test]
fn fallback_rename_preserves_both_raw_paths() {
    let root = tempfile::tempdir().unwrap();
    let git = GitRepository::init(root.path()).unwrap();
    let signature = git2::Signature::now("fixture", "fixture@example.com").unwrap();
    let mut builder = git.treebuilder(None).unwrap();
    let blob = git.blob(b"identical content\n").unwrap();
    builder.insert("old\t%name", blob, 0o100644).unwrap();
    let old_tree = git.find_tree(builder.write().unwrap()).unwrap();
    let old = git
        .commit(None, &signature, &signature, "old", &old_tree, &[])
        .unwrap();
    builder.remove("old\t%name").unwrap();
    builder.insert("new\n中文", blob, 0o100644).unwrap();
    let new_tree = git.find_tree(builder.write().unwrap()).unwrap();
    let new = git
        .commit(
            None,
            &signature,
            &signature,
            "rename",
            &new_tree,
            &[&git.find_commit(old).unwrap()],
        )
        .unwrap();
    let parsed = parse_diff_files_via_git_cli(&git, new, old, &new_tree, &old_tree).unwrap();
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].operation, FileOperation::Renamed);
    assert_eq!(parsed[0].old_path.as_deref(), Some("old%09%25name"));
    assert_eq!(
        parsed[0].path,
        atomic_repository::escape_repo_path("new\n中文".as_bytes())
    );
    assert_eq!(parsed[0].old_content, parsed[0].new_content);
}

#[test]
fn tree_lookup_preserves_nested_raw_names_and_modes_on_every_host() {
    let root = tempfile::tempdir().unwrap();
    let git = GitRepository::init(root.path()).unwrap();
    let content = b"raw nested content\n";
    let mut nested = git.treebuilder(None).unwrap();
    nested
        .insert(
            b"file-\xff%\t".as_slice(),
            git.blob(content).unwrap(),
            0o100755,
        )
        .unwrap();
    let mut builder = git.treebuilder(None).unwrap();
    builder
        .insert(b"dir-\xfe".as_slice(), nested.write().unwrap(), 0o040000)
        .unwrap();
    let tree = git.find_tree(builder.write().unwrap()).unwrap();
    let path = atomic_repository::escape_repo_path(b"dir-\xfe/file-\xff%\t");

    assert!(tree_entry_exists(&git, &tree, &path));
    assert_eq!(git_mode_at(&git, &tree, &path), Some(0o100755));
    assert_eq!(get_file_content(&git, &tree, &path).unwrap(), content);
    assert!(!tree_entry_exists(&git, &tree, "missing"));
    assert!(git_tree_entry(&git, &tree, "dir%00name").is_err());
}

#[test]
fn added_path_under_same_commit_deletion_is_not_pre_registered() {
    // Issue #279: `x` (file or symlink) becomes a directory `x/...` in one
    // commit; only paths strictly below a deleted path are affected.
    let deleted: HashSet<&str> = ["x", ".agents/skills/review"].into_iter().collect();
    assert!(sits_under_deleted_path("x/y", &deleted));
    assert!(sits_under_deleted_path("x/deep/z", &deleted));
    assert!(sits_under_deleted_path(
        ".agents/skills/review/SKILL.md",
        &deleted
    ));
    assert!(!sits_under_deleted_path("x", &deleted));
    assert!(!sits_under_deleted_path("xy/z", &deleted));
    assert!(!sits_under_deleted_path(
        ".agents/skills/other/SKILL.md",
        &deleted
    ));
    assert!(!sits_under_deleted_path("top", &HashSet::new()));
}

#[test]
fn sits_under_deleted_path_matches_exact_ancestors_only() {
    let deleted: std::collections::HashSet<&str> = ["a/b", "tool"].into_iter().collect();
    assert!(sits_under_deleted_path("a/b/c.txt", &deleted));
    assert!(sits_under_deleted_path("tool/main.sh", &deleted));
    assert!(!sits_under_deleted_path("a/bc/d.txt", &deleted));
    assert!(!sits_under_deleted_path("a/b", &deleted));
    assert!(!sits_under_deleted_path("tools/x", &deleted));
    assert!(!sits_under_deleted_path(
        "a/b/c.txt",
        &std::collections::HashSet::new()
    ));
}

#[test]
fn a_rename_whose_source_is_reoccupied_is_a_typechange_plus_an_add() {
    // emanote dc1fb69: README.md moved into emanote/ while a symlink to the
    // moved file takes the old path. libgit2 pairs the vacated blob with the
    // added path as a rename and leaves the symlink as a same-path add.
    for renamed_first in [true, false] {
        let renamed = ParsedFile {
            path: "pkg/README.md".into(),
            operation: FileOperation::Renamed,
            new_content: Some(b"readme\n".to_vec()),
            old_content: Some(b"readme\n".to_vec()),
            old_path: Some("README.md".into()),
            diff_lines: None,
            new_mode: Some(0o100644),
        };
        let link = ParsedFile {
            path: "README.md".into(),
            operation: FileOperation::Added,
            new_content: Some(b"pkg/README.md".to_vec()),
            old_content: None,
            old_path: None,
            diff_lines: None,
            new_mode: Some(0o120000),
        };
        let files = normalize_typechanges(normalize_rename_collisions(if renamed_first {
            vec![renamed, link]
        } else {
            vec![link, renamed]
        }));
        assert_eq!(files.len(), 2, "renamed_first={renamed_first}");
        let old = files.iter().find(|file| file.path == "README.md").unwrap();
        assert_eq!(old.operation, FileOperation::Modified);
        assert_eq!(old.old_content.as_deref(), Some(b"readme\n".as_slice()));
        assert_eq!(
            old.new_content.as_deref(),
            Some(b"pkg/README.md".as_slice())
        );
        assert_eq!(old.new_mode, Some(0o120000));
        let moved = files
            .iter()
            .find(|file| file.path == "pkg/README.md")
            .unwrap();
        assert_eq!(moved.operation, FileOperation::Added);
        assert_eq!(moved.old_path, None);
        assert_eq!(moved.old_content, None);
        assert_eq!(moved.new_content.as_deref(), Some(b"readme\n".as_slice()));
    }
}

#[test]
fn a_rename_onto_a_deleted_path_is_a_delete_plus_a_typechange() {
    // The mirror shape: a symlink replaced by the regular file it pointed
    // to. libgit2 pairs the moved blob with the symlink's path as a rename
    // and leaves the symlink's removal as a same-path delete.
    for deleted_first in [true, false] {
        let deleted = ParsedFile {
            path: "README.md".into(),
            operation: FileOperation::Deleted,
            new_content: None,
            old_content: Some(b"pkg/README.md".to_vec()),
            old_path: None,
            diff_lines: None,
            new_mode: None,
        };
        let renamed = ParsedFile {
            path: "README.md".into(),
            operation: FileOperation::Renamed,
            new_content: Some(b"readme\n".to_vec()),
            old_content: Some(b"readme\n".to_vec()),
            old_path: Some("pkg/README.md".into()),
            diff_lines: None,
            new_mode: Some(0o100644),
        };
        let files = normalize_typechanges(normalize_rename_collisions(if deleted_first {
            vec![deleted, renamed]
        } else {
            vec![renamed, deleted]
        }));
        assert_eq!(files.len(), 2, "deleted_first={deleted_first}");
        let vacated = files
            .iter()
            .find(|file| file.path == "pkg/README.md")
            .unwrap();
        assert_eq!(vacated.operation, FileOperation::Deleted);
        assert_eq!(vacated.old_content.as_deref(), Some(b"readme\n".as_slice()));
        assert_eq!(vacated.new_content, None);
        let target = files.iter().find(|file| file.path == "README.md").unwrap();
        assert_eq!(target.operation, FileOperation::Modified);
        assert_eq!(
            target.old_content.as_deref(),
            Some(b"pkg/README.md".as_slice())
        );
        assert_eq!(target.new_content.as_deref(), Some(b"readme\n".as_slice()));
        assert_eq!(target.new_mode, Some(0o100644));
    }
}

#[test]
fn an_ordinary_rename_is_left_alone() {
    let renamed = ParsedFile {
        path: "pkg/README.md".into(),
        operation: FileOperation::Renamed,
        new_content: Some(b"readme\n".to_vec()),
        old_content: Some(b"readme\n".to_vec()),
        old_path: Some("README.md".into()),
        diff_lines: None,
        new_mode: Some(0o100644),
    };
    let files = normalize_rename_collisions(vec![renamed.clone()]);
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].operation, FileOperation::Renamed);
    assert_eq!(files[0].old_path.as_deref(), Some("README.md"));
}

#[test]
fn a_rename_swap_stays_a_rename_chain() {
    // `git mv a tmp; git mv c a; git mv tmp b`: Git reports two renames,
    // and so must the normalized deltas, in either order.
    let a_to_b = ParsedFile {
        path: "b".into(),
        operation: FileOperation::Renamed,
        new_content: Some(b"a\n".to_vec()),
        old_content: Some(b"a\n".to_vec()),
        old_path: Some("a".into()),
        diff_lines: None,
        new_mode: Some(0o100644),
    };
    let c_to_a = ParsedFile {
        path: "a".into(),
        operation: FileOperation::Renamed,
        new_content: Some(b"c\n".to_vec()),
        old_content: Some(b"c\n".to_vec()),
        old_path: Some("c".into()),
        diff_lines: None,
        new_mode: Some(0o100644),
    };
    for files in [
        vec![a_to_b.clone(), c_to_a.clone()],
        vec![c_to_a.clone(), a_to_b.clone()],
    ] {
        let normalized = normalize_rename_collisions(files);
        assert_eq!(normalized.len(), 2);
        assert!(normalized
            .iter()
            .all(|file| file.operation == FileOperation::Renamed));
        assert_eq!(
            normalized
                .iter()
                .map(|file| (file.old_path.clone().unwrap(), file.path.clone()))
                .collect::<HashSet<_>>(),
            HashSet::from([
                ("a".to_string(), "b".to_string()),
                ("c".to_string(), "a".to_string())
            ])
        );
    }
}

#[test]
fn a_gitlink_rename_without_a_source_path_is_left_alone() {
    let deleted = ParsedFile {
        path: "vendor/lib".into(),
        operation: FileOperation::Deleted,
        new_content: None,
        old_content: Some(b"link\n".to_vec()),
        old_path: None,
        diff_lines: None,
        new_mode: None,
    };
    let gitlink = ParsedFile {
        path: "vendor/lib".into(),
        operation: FileOperation::Renamed,
        new_content: Some(b"0123456789abcdef0123456789abcdef01234567".to_vec()),
        old_content: None,
        old_path: None,
        diff_lines: None,
        new_mode: Some(0o160000),
    };
    let files = normalize_rename_collisions(vec![deleted, gitlink]);
    assert_eq!(files.len(), 2);
    assert!(files.iter().all(|file| file.path == "vendor/lib"));
    assert!(files
        .iter()
        .any(|file| file.operation == FileOperation::Renamed));
}
