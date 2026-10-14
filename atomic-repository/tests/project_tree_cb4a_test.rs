#![cfg(unix)] // the fixture relies on unix permissions and symlinks

use std::fs;

use atomic_core::change::{Author, ChangeHeader, InodeKind};
use atomic_core::{GitHashAlgorithm, WorkingCopyId};
use atomic_repository::{
    ConversionPolicy, ManifestDisposition, RecordOptions, Repository, RepositoryEntry,
};
use tempfile::TempDir;

fn working_copy(repo: &Repository) -> WorkingCopyId {
    repo.require_working_copy_id().expect("working copy id")
}

fn comparable(entry: &RepositoryEntry) -> (&[u8], &[u8], u32, InodeKind) {
    (
        entry.path.as_bytes(),
        &entry.repository_bytes,
        entry.git_mode(),
        entry.kind,
    )
}

#[test]
fn explicit_closure_projection_survives_source_view_deletion_and_reopen() {
    let temp = TempDir::new().unwrap();
    let mut repo = Repository::init(temp.path()).unwrap();
    let wc = working_copy(&repo);
    repo.create_view("source").unwrap();
    repo.switch_view(wc, "source").unwrap();
    fs::write(temp.path().join("old.txt"), b"original\n").unwrap();
    repo.add(wc, "old.txt", Default::default()).unwrap();
    let original = *repo.record_all(wc, "original").unwrap().hash();
    fs::rename(temp.path().join("old.txt"), temp.path().join("new.txt")).unwrap();
    let renamed = *repo.record_all(wc, "rename").unwrap().hash();
    let policy = ConversionPolicy::new(GitHashAlgorithm::Sha1);
    let expected = repo.project_tree("source", &policy).unwrap();
    assert_eq!(
        repo.project_change_closure(&[renamed], &policy).unwrap(),
        expected
    );
    // Roots plus dependency closure determine paths, regardless of the selected
    // view or whether any surviving view still references those changes.
    repo.switch_view(wc, "dev").unwrap();
    repo.delete_view("source").unwrap();
    drop(repo);
    let repo = Repository::open(temp.path()).unwrap();
    assert_eq!(
        repo.project_change_closure(&[renamed], &policy).unwrap(),
        expected
    );
    let prior = repo.project_change_closure(&[original], &policy).unwrap();
    assert!(prior
        .manifest
        .entries
        .iter()
        .any(|e| e.path.as_bytes() == b"old.txt"));
    assert!(!prior
        .manifest
        .entries
        .iter()
        .any(|e| e.path.as_bytes() == b"new.txt"));
    assert!(!repo.view_exists("source").unwrap());
    assert!(!temp.path().join("old.txt").exists());
    assert!(!temp.path().join("new.txt").exists());
    let unknown = atomic_core::Hash::of(b"unknown closure root");
    assert!(repo.project_change_closure(&[unknown], &policy).is_err());
}

#[test]
#[cfg(unix)]
fn graph_projection_and_git_parser_agree_for_sha1_and_sha256() {
    use std::os::unix::fs::{symlink, PermissionsExt};

    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let repo = Repository::init(root).unwrap();

    fs::create_dir_all(root.join("bin")).unwrap();
    fs::create_dir_all(root.join("nested")).unwrap();
    fs::create_dir_all(root.join("links")).unwrap();
    fs::create_dir_all(root.join(".vault")).unwrap();
    fs::write(root.join("empty"), []).unwrap();
    fs::write(root.join("bin/run"), b"#!/bin/sh\necho graph\n").unwrap();
    fs::set_permissions(root.join("bin/run"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(root.join("nested/data"), [0, 255, 10]).unwrap();
    fs::write(root.join(".vault/private"), b"excluded").unwrap();
    symlink("../empty", root.join("links/current")).unwrap();

    for path in [
        "empty",
        "bin/run",
        "nested/data",
        "links/current",
        ".vault/private",
    ] {
        repo.add(working_copy(&repo), path, Default::default())
            .unwrap();
    }
    let header = ChangeHeader::builder()
        .message("CB-4A graph fixture")
        .author(Author::new("Test", Some("test@example.com")))
        .build();
    repo.record(working_copy(&repo), header, RecordOptions::default())
        .unwrap();

    for algorithm in [GitHashAlgorithm::Sha1, GitHashAlgorithm::Sha256] {
        let policy = ConversionPolicy::new(algorithm);
        let projected = repo.project_tree(repo.current_view(), &policy).unwrap();
        let parsed = projected
            .git
            .objects
            .parse_manifest(&projected.git.root, &policy)
            .unwrap();

        let expected: Vec<_> = projected
            .manifest
            .included_entries()
            .map(comparable)
            .collect();
        let actual: Vec<_> = parsed.included_entries().map(comparable).collect();
        assert_eq!(actual, expected);

        let executable = projected
            .manifest
            .entries
            .iter()
            .find(|entry| entry.path.as_bytes() == b"bin/run")
            .unwrap();
        assert_eq!(executable.git_mode(), 0o100755);
        let link = projected
            .manifest
            .entries
            .iter()
            .find(|entry| entry.path.as_bytes() == b"links/current")
            .unwrap();
        assert_eq!(link.kind, InodeKind::Symlink);
        assert_eq!(link.repository_bytes, b"../empty");
        let empty = projected
            .manifest
            .entries
            .iter()
            .find(|entry| entry.path.as_bytes() == b"empty")
            .unwrap();
        assert!(empty.repository_bytes.is_empty());
        let vault = projected
            .manifest
            .entries
            .iter()
            .find(|entry| entry.path.as_bytes() == b".vault/private")
            .unwrap();
        assert!(matches!(
            vault.disposition,
            ManifestDisposition::Excluded(_)
        ));
        assert!(!parsed
            .entries
            .iter()
            .any(|entry| entry.path.as_bytes().starts_with(b".vault/")));
    }
}
