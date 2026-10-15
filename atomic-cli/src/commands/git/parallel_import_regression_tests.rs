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
