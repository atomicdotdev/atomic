//! Stash preserves the working entry itself, including its mode and link text.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};

use atomic_repository::{RecordOptions, Repository, StashPushOptions, StatusOptions};

fn record_path(repo: &Repository, path: &str) {
    let wc = repo.require_working_copy_id().unwrap();
    repo.add(wc, path, Default::default()).unwrap();
    repo.record_with_message(wc, "base", RecordOptions::default())
        .unwrap();
}

#[test]
fn untracked_only_stash_preserves_executable_and_dangling_link() {
    for keep in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(root.path()).unwrap();
        let script = root.path().join("new.sh");
        fs::write(&script, b"untracked script\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        symlink("missing-target", root.path().join("link")).unwrap();
        repo.stash_push(StashPushOptions {
            include_untracked: true,
            keep,
            ..Default::default()
        })
        .unwrap()
        .expect("untracked-only work must be stashed with -u");
        assert_eq!(script.exists(), keep);
        assert_eq!(fs::symlink_metadata(root.path().join("link")).is_ok(), keep);
        drop(repo);
        let repo = Repository::open(root.path()).unwrap();
        repo.stash_apply(None).unwrap();
        assert_eq!(fs::read(&script).unwrap(), b"untracked script\n");
        assert_eq!(
            fs::metadata(&script).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            fs::read_link(root.path().join("link")).unwrap(),
            std::path::Path::new("missing-target")
        );
    }
}

#[test]
fn stash_keeps_chmod_with_or_without_content_and_deletions_after_reopen() {
    for bytes in [b"recorded\n".as_slice(), b"edited\n".as_slice()] {
        let root = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(root.path()).unwrap();
        let script = root.path().join("script.sh");
        fs::write(&script, b"recorded\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o644)).unwrap();
        record_path(&repo, "script.sh");
        fs::write(root.path().join("obsolete"), b"obsolete\n").unwrap();
        record_path(&repo, "obsolete");

        fs::write(&script, bytes).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_file(root.path().join("obsolete")).unwrap();
        repo.stash_push(StashPushOptions::default())
            .unwrap()
            .unwrap();
        assert_eq!(fs::read(&script).unwrap(), b"recorded\n");
        assert_eq!(
            fs::metadata(&script).unwrap().permissions().mode() & 0o777,
            0o644
        );
        let wc = repo.require_working_copy_id().unwrap();
        assert!(repo
            .status(wc, StatusOptions::default())
            .unwrap()
            .is_clean());
        drop(repo);

        let repo = Repository::open(root.path()).unwrap();
        repo.stash_apply(None).unwrap();
        assert_eq!(fs::read(&script).unwrap(), bytes);
        assert_eq!(
            fs::metadata(&script).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert!(!root.path().join("obsolete").exists());
    }
}

#[test]
fn stash_keeps_regular_to_dangling_symlink_after_reopen() {
    let root = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(root.path()).unwrap();
    let path = root.path().join("entry");
    fs::write(&path, b"recorded\n").unwrap();
    record_path(&repo, "entry");
    fs::remove_file(&path).unwrap();
    symlink("missing-target", &path).unwrap();

    repo.stash_push(StashPushOptions::default())
        .unwrap()
        .unwrap();
    assert!(fs::symlink_metadata(&path).unwrap().file_type().is_file());
    assert_eq!(fs::read(&path).unwrap(), b"recorded\n");
    drop(repo);

    let repo = Repository::open(root.path()).unwrap();
    repo.stash_apply(None).unwrap();
    assert!(fs::symlink_metadata(&path)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        fs::read_link(&path).unwrap(),
        std::path::Path::new("missing-target")
    );
}

#[test]
fn stash_keeps_symlink_to_regular_without_writing_external_target() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("keep");
    fs::write(&target, b"external content\n").unwrap();
    let mut repo = Repository::init(root.path()).unwrap();
    let path = root.path().join("entry");
    symlink(&target, &path).unwrap();
    record_path(&repo, "entry");
    fs::remove_file(&path).unwrap();
    fs::write(&path, b"local replacement\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();

    repo.stash_push(StashPushOptions::default())
        .unwrap()
        .unwrap();
    assert_eq!(fs::read_link(&path).unwrap(), target);
    drop(repo);

    let repo = Repository::open(root.path()).unwrap();
    repo.stash_apply(None).unwrap();
    assert!(fs::symlink_metadata(&path).unwrap().file_type().is_file());
    assert_eq!(fs::read(&path).unwrap(), b"local replacement\n");
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(fs::read(&target).unwrap(), b"external content\n");
}

#[test]
fn stash_restore_refuses_a_replaced_parent_symlink_and_keeps_payload() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(root.path()).unwrap();
    fs::create_dir(root.path().join("nested")).unwrap();
    let path = root.path().join("nested/file");
    fs::write(&path, b"recorded\n").unwrap();
    record_path(&repo, "nested/file");
    fs::write(&path, b"local work\n").unwrap();
    repo.stash_push(StashPushOptions::default())
        .unwrap()
        .unwrap();
    fs::remove_file(&path).unwrap();
    fs::remove_dir(root.path().join("nested")).unwrap();
    fs::write(outside.path().join("file"), b"external content\n").unwrap();
    symlink(outside.path(), root.path().join("nested")).unwrap();

    let error = repo.stash_apply(None).unwrap_err();
    assert!(error.contains("symlink"), "{error}");
    assert_eq!(
        fs::read(outside.path().join("file")).unwrap(),
        b"external content\n"
    );
    assert_eq!(repo.stash_list().unwrap().len(), 1);
}
