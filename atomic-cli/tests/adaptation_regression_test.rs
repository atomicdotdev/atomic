//! Regressions at the repository / unchanged libatomic contract boundary.
use std::{fs, path::Path, process::Command};

fn cli(root: &Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_atomic"))
        .current_dir(root)
        .args(args)
        .env("ATOMIC_HOME", root.join(".atomic/test-home"))
        .env("ATOMIC_SERVICE", "local")
        .env_remove("ATOMIC_RPC")
        .env("ATOMIC_NONINTERACTIVE", "1")
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn init() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    cli(root.path(), &["init", "--no-vault"]);
    root
}

fn commit_git_paths(root: &Path, message: &str, paths: &[&str]) {
    let git = git2::Repository::open(root).unwrap();
    let mut index = git.index().unwrap();
    index.clear().unwrap();
    for path in paths {
        index.add_path(Path::new(path)).unwrap();
    }
    index.write().unwrap();
    let tree = git.find_tree(index.write_tree().unwrap()).unwrap();
    let author = git2::Signature::now("Regression", "regression@example.test").unwrap();
    let parent = git.head().ok().and_then(|head| head.peel_to_commit().ok());
    let parents: Vec<_> = parent.iter().collect();
    git.commit(Some("HEAD"), &author, &author, message, &tree, &parents)
        .unwrap();
}

#[test]
fn git_import_retires_empty_parent_directories_and_recreates_them() {
    let root = tempfile::tempdir().unwrap();
    git2::Repository::init(root.path()).unwrap();
    fs::write(root.path().join("base.txt"), "base\n").unwrap();
    fs::create_dir_all(root.path().join("sub/inner")).unwrap();
    fs::write(root.path().join("sub/inner/child.txt"), "child\n").unwrap();
    fs::write(root.path().join("sub/sibling.txt"), "sibling\n").unwrap();
    commit_git_paths(
        root.path(),
        "nested files",
        &["base.txt", "sub/inner/child.txt", "sub/sibling.txt"],
    );
    let git = git2::Repository::open(root.path()).unwrap();
    let branch = git.head().unwrap().shorthand().unwrap().to_owned();
    drop(git);
    cli(root.path(), &["git", "import", "--no-vault"]);
    cli(
        root.path(),
        &["view", "create", "before-delete", "--from", &branch],
    );
    // A child view inherits future parent changes. Detach this saved closure
    // before importing the deletion into its original parent.
    cli(root.path(), &["view", "promote", "before-delete"]);

    fs::remove_file(root.path().join("sub/inner/child.txt")).unwrap();
    fs::remove_dir(root.path().join("sub/inner")).unwrap();
    commit_git_paths(
        root.path(),
        "remove nested last child",
        &["base.txt", "sub/sibling.txt"],
    );
    cli(
        root.path(),
        &["git", "import", "--incremental", "--no-vault"],
    );
    assert!(cli(root.path(), &["status", "--short"]).trim().is_empty());
    assert!(root.path().join("sub/sibling.txt").is_file());

    fs::remove_file(root.path().join("sub/sibling.txt")).unwrap();
    fs::remove_dir(root.path().join("sub")).unwrap();
    commit_git_paths(root.path(), "remove outer last child", &["base.txt"]);
    cli(
        root.path(),
        &["git", "import", "--incremental", "--no-vault"],
    );
    assert!(cli(root.path(), &["status", "--short"]).trim().is_empty());
    cli(root.path(), &["view", "create", "after-delete"]);
    cli(root.path(), &["view", "switch", "before-delete"]);
    assert_eq!(
        fs::read(root.path().join("sub/inner/child.txt")).unwrap(),
        b"child\n"
    );
    cli(root.path(), &["view", "switch", "after-delete"]);
    assert!(!root.path().join("sub").exists());
    assert!(cli(root.path(), &["status", "--short"]).trim().is_empty());

    cli(root.path(), &["view", "switch", &branch]);
    fs::create_dir_all(root.path().join("sub/inner")).unwrap();
    fs::write(root.path().join("sub/inner/reborn.txt"), "reborn\n").unwrap();
    commit_git_paths(
        root.path(),
        "recreate directories",
        &["base.txt", "sub/inner/reborn.txt"],
    );
    cli(
        root.path(),
        &["git", "import", "--incremental", "--no-vault"],
    );
    assert!(cli(root.path(), &["status", "--short"]).trim().is_empty());
    assert_eq!(
        fs::read(root.path().join("sub/inner/reborn.txt")).unwrap(),
        b"reborn\n"
    );
    assert!(!root.path().join("sub/inner/child.txt").exists());
}

#[test]
fn remove_preserves_files_unless_delete_is_explicit() {
    let root = init();
    for name in ["keep.txt", "delete.txt", "legacy-keep.txt"] {
        fs::write(root.path().join(name), "private bytes\n").unwrap();
        cli(root.path(), &["add", name]);
    }
    cli(root.path(), &["remove", "keep.txt"]);
    assert_eq!(
        fs::read(root.path().join("keep.txt")).unwrap(),
        b"private bytes\n"
    );
    cli(root.path(), &["remove", "--delete", "delete.txt"]);
    assert!(!root.path().join("delete.txt").exists());
    cli(root.path(), &["remove", "--keep", "legacy-keep.txt"]);
    assert_eq!(
        fs::read(root.path().join("legacy-keep.txt")).unwrap(),
        b"private bytes\n"
    );
}

#[test]
fn directory_replaced_by_file_is_reported_as_working_changes() {
    let root = init();
    fs::create_dir(root.path().join("thing")).unwrap();
    fs::write(root.path().join("thing/child"), "original\n").unwrap();
    cli(
        root.path(),
        &["record", "-am", "directory", "--author", "Regression"],
    );
    fs::remove_file(root.path().join("thing/child")).unwrap();
    fs::remove_dir(root.path().join("thing")).unwrap();
    fs::write(root.path().join("thing"), "replacement\n").unwrap();
    let status = cli(root.path(), &["status", "--short"]);
    assert!(status.contains("thing/child"), "{status}");
    assert!(status.contains("thing"), "{status}");
    cli(root.path(), &["revise", "-m", "replacement", "--", "thing"]);
    assert_eq!(
        fs::read(root.path().join("thing")).unwrap(),
        b"replacement\n"
    );
    assert!(cli(root.path(), &["status", "--short"]).trim().is_empty());
}

#[test]
fn view_list_migrates_retained_history_before_the_readonly_service_query() {
    use atomic_core::pristine::{MutTxnT, PathClaimMutTxnT};
    use atomic_core::types::Base32;
    use atomic_repository::{RecordOptions, Repository, UnrecordOptions};
    let root = tempfile::tempdir().unwrap();
    let repo = Repository::init(root.path()).unwrap();
    let wc = repo.require_working_copy_id().unwrap();
    fs::write(root.path().join("orphan.txt"), b"retained\n").unwrap();
    repo.add(wc, "orphan.txt", Default::default()).unwrap();
    let hash = *repo
        .record_with_message(wc, "orphan", RecordOptions::default())
        .unwrap()
        .hash();
    repo.unrecord(&hash, UnrecordOptions::default()).unwrap();
    let mut txn = repo.pristine().write_txn().unwrap();
    txn.reset_path_claim_migration().unwrap();
    txn.commit().unwrap();
    drop(repo);
    fs::remove_file(root.path().join("orphan.txt")).unwrap();
    assert!(Repository::open_readonly(root.path()).is_err());
    cli(root.path(), &["view", "list"]);
    cli(root.path(), &["insert", &hash.to_base32()]);
    assert_eq!(
        fs::read(root.path().join("orphan.txt")).unwrap(),
        b"retained\n"
    );
    assert!(cli(root.path(), &["status", "--short"]).trim().is_empty());
}

#[cfg(unix)]
#[test]
fn diff_and_restore_include_content_permissions_and_symlink_types() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let root = init();
    let path = root.path().join("script.sh");
    fs::write(&path, "original\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    cli(
        root.path(),
        &["record", "-am", "regular", "--author", "Regression"],
    );
    fs::write(&path, "edited\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    let diff = cli(root.path(), &["diff", "--no-color"]);
    assert!(
        diff.contains("-original") && diff.contains("+edited"),
        "{diff}"
    );
    let native_diff = cli(root.path(), &["diff", "--cached", "--no-color"]);
    assert!(
        native_diff.contains("-original") && native_diff.contains("+edited"),
        "{native_diff}"
    );
    let preview = cli(root.path(), &["restore", "--dry-run"]);
    assert!(preview.contains("script.sh"), "{preview}");
    cli(root.path(), &["restore", "script.sh"]);
    assert_eq!(fs::read(&path).unwrap(), b"original\n");
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o644
    );
    assert!(cli(root.path(), &["status", "--short"]).trim().is_empty());
    // Permission-only changes must not vanish from CLI rendering.
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(cli(root.path(), &["diff", "--name-only"]).contains("script.sh"));
    cli(root.path(), &["restore", "script.sh"]);

    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("private.txt");
    fs::write(&target, "private target bytes\n").unwrap();
    fs::remove_file(&path).unwrap();
    symlink(&target, &path).unwrap();
    let diff = cli(root.path(), &["diff", "--no-color"]);
    assert!(
        !diff.contains("private target bytes"),
        "must diff link text, not dereference it"
    );
    cli(root.path(), &["restore", "script.sh"]);
    assert!(!fs::symlink_metadata(&path)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read(&path).unwrap(), b"original\n");
    assert_eq!(fs::read(&target).unwrap(), b"private target bytes\n");
    // Restore in the reverse direction must recreate the recorded link.
    fs::remove_file(&path).unwrap();
    symlink("missing-target", &path).unwrap();
    cli(
        root.path(),
        &["record", "-am", "symlink", "--author", "Regression"],
    );
    fs::remove_file(&path).unwrap();
    fs::write(&path, "replacement\n").unwrap();
    cli(root.path(), &["restore", "script.sh"]);
    assert_eq!(fs::read_link(&path).unwrap(), Path::new("missing-target"));
    assert!(cli(root.path(), &["status", "--short"]).trim().is_empty());
}

#[cfg(unix)]
#[test]
fn remove_preview_does_not_capture_pending_bridge_edits() {
    use std::collections::BTreeMap;
    fn snapshot(root: &Path) -> BTreeMap<std::path::PathBuf, Vec<u8>> {
        fn visit(path: &Path, files: &mut BTreeMap<std::path::PathBuf, Vec<u8>>) {
            if path.is_dir() {
                for entry in fs::read_dir(path).unwrap() {
                    visit(&entry.unwrap().path(), files);
                }
            } else if path.is_file() {
                files.insert(path.to_path_buf(), fs::read(path).unwrap());
            }
        }
        let mut files = BTreeMap::new();
        for path in ["atomic.redb", "changes", "bridge", "deferred-tree-ops.json"] {
            visit(&root.join(".atomic").join(path), &mut files);
        }
        files
    }
    let root = tempfile::tempdir().unwrap();
    {
        let git = git2::Repository::init(root.path()).unwrap();
        fs::write(root.path().join("file.txt"), "recorded\n").unwrap();
        let mut index = git.index().unwrap();
        index.add_path(Path::new("file.txt")).unwrap();
        index.write().unwrap();
        let tree = git.find_tree(index.write_tree().unwrap()).unwrap();
        let author = git2::Signature::now("Regression", "regression@example.test").unwrap();
        git.commit(Some("HEAD"), &author, &author, "fixture", &tree, &[])
            .unwrap();
    }
    cli(root.path(), &["git", "import", "--no-vault"]);
    fs::write(root.path().join("file.txt"), "pending edit\n").unwrap();
    let before = snapshot(root.path());
    let output = cli(root.path(), &["remove", "--dry-run", "file.txt"]);
    assert!(output.contains("Would remove 1"), "{output}");
    assert_eq!(
        snapshot(root.path()),
        before,
        "preview must not mutate DB, changes, journal or bridge evidence"
    );
    assert_eq!(
        fs::read(root.path().join("file.txt")).unwrap(),
        b"pending edit\n"
    );
}
