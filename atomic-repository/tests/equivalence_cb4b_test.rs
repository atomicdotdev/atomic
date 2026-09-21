#![cfg(unix)]

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::Path;
use std::process::{Command, Stdio};

use atomic_core::change::InodeKind;
use atomic_core::{GitHashAlgorithm, GitObjectId, SetId};
use atomic_repository::{
    compare_project_state, compare_project_to_index, compare_project_to_worktree,
    observe_git_index, observe_worktree, ContentFilter, ContentFilterError, ConversionPolicy,
    EquivalenceClaims, FilterDirection, FilteredContent, GitIndexEntry, GitIndexState,
    ManifestDisposition, MismatchKind, ObservationError, PhysicalKind, ProjectTree, RepoPath,
    RepositoryEntry, RepositoryManifest, WorktreeEntry, WorktreeObservation,
    GIT_INDEX_STATE_VERSION,
};
use tempfile::TempDir;

#[derive(Clone, Copy)]
struct IdentityFilter;

impl ContentFilter for IdentityFilter {
    fn clean(&self, _path: &Path, bytes: &[u8]) -> Result<FilteredContent, ContentFilterError> {
        Ok(FilteredContent {
            bytes: bytes.to_vec(),
            warnings: Vec::new(),
        })
    }

    fn smudge(&self, _path: &Path, bytes: &[u8]) -> Result<FilteredContent, ContentFilterError> {
        Ok(FilteredContent {
            bytes: bytes.to_vec(),
            warnings: Vec::new(),
        })
    }
}

struct FailingFilter;

impl ContentFilter for FailingFilter {
    fn clean(&self, path: &Path, _bytes: &[u8]) -> Result<FilteredContent, ContentFilterError> {
        Err(ContentFilterError::RequiredDriverFailed {
            driver: "required".into(),
            direction: FilterDirection::Clean,
            path: path.display().to_string(),
            reason: "fixture failure".into(),
        })
    }

    fn smudge(&self, path: &Path, _bytes: &[u8]) -> Result<FilteredContent, ContentFilterError> {
        Err(ContentFilterError::RequiredDriverFailed {
            driver: "required".into(),
            direction: FilterDirection::Smudge,
            path: path.display().to_string(),
            reason: "fixture failure".into(),
        })
    }
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn fixture() -> (TempDir, ConversionPolicy, ProjectTree) {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "core.autocrlf", "false"]);
    git(root, &["config", "core.filemode", "true"]);
    git(root, &["config", "core.symlinks", "true"]);
    git(root, &["config", "core.ignorecase", "false"]);
    git(root, &["config", "core.precomposeunicode", "false"]);

    fs::create_dir_all(root.join("bin")).unwrap();
    fs::create_dir_all(root.join("links")).unwrap();
    fs::create_dir_all(root.join(".vault")).unwrap();
    fs::write(root.join("file"), b"hello\n").unwrap();
    fs::write(root.join("empty"), []).unwrap();
    fs::write(root.join("bin/run"), b"#!/bin/sh\n").unwrap();
    fs::set_permissions(root.join("bin/run"), fs::Permissions::from_mode(0o755)).unwrap();
    symlink("../file", root.join("links/current")).unwrap();
    fs::write(root.join(".vault/private"), b"private").unwrap();
    git(root, &["add", "file", "empty", "bin/run", "links/current"]);

    let mut policy = ConversionPolicy::new(GitHashAlgorithm::Sha1);
    policy.platform.case_sensitive = true;
    let entries = vec![
        entry(
            b"file",
            b"hello\n",
            0o644,
            InodeKind::Regular,
            ManifestDisposition::Included,
        ),
        entry(
            b"empty",
            b"",
            0o644,
            InodeKind::Regular,
            ManifestDisposition::Included,
        ),
        entry(
            b"bin/run",
            b"#!/bin/sh\n",
            0o755,
            InodeKind::Regular,
            ManifestDisposition::Included,
        ),
        entry(
            b"links/current",
            b"../file",
            0o777,
            InodeKind::Symlink,
            ManifestDisposition::Included,
        ),
        entry(
            b".vault/private",
            b"private",
            0o644,
            InodeKind::Regular,
            ManifestDisposition::Excluded(atomic_repository::ExclusionReason::VaultPrivate),
        ),
    ];
    let manifest =
        RepositoryManifest::new(SetId::ZERO, policy.root().content_key.clone(), entries).unwrap();
    let project = ProjectTree::from_manifest(manifest, &policy).unwrap();
    (temp, policy, project)
}

fn entry(
    path: &[u8],
    bytes: &[u8],
    mode: u16,
    kind: InodeKind,
    disposition: ManifestDisposition,
) -> RepositoryEntry {
    RepositoryEntry::new(
        RepoPath::from_bytes(path).unwrap(),
        bytes.to_vec(),
        mode,
        kind,
        None,
        disposition,
    )
    .unwrap()
}

#[test]
fn real_index_and_worktree_observers_roundtrip_raw_paths_and_flags() {
    let (temp, policy, project) = fixture();
    let root = temp.path();
    let index = observe_git_index(root, &policy).unwrap();
    assert!(compare_project_to_index(&project, &index).is_equivalent());

    let worktree = observe_worktree(root, Some(&index), &IdentityFilter, &policy).unwrap();

    let report = compare_project_to_worktree(&project, &worktree, &policy);
    assert!(report.is_equivalent(), "{report:#?}");

    git(root, &["update-index", "--assume-unchanged", "file"]);
    git(root, &["update-index", "--skip-worktree", "empty"]);
    fs::write(root.join("intent"), b"later").unwrap();
    git(root, &["add", "-N", "intent"]);
    let flagged = observe_git_index(root, &policy).unwrap();
    assert!(flagged
        .entries
        .iter()
        .any(|entry| entry.path.as_bytes() == b"file" && entry.assume_unchanged));
    assert!(flagged
        .entries
        .iter()
        .any(|entry| entry.path.as_bytes() == b"empty" && entry.skip_worktree));
    assert!(flagged
        .entries
        .iter()
        .any(|entry| entry.path.as_bytes() == b"intent" && entry.intent_to_add));
}

#[test]
fn real_index_observer_retains_stages_one_through_three() {
    let (temp, policy, _project) = fixture();
    let root = temp.path();
    let oid = git(root, &["hash-object", "-w", "file"]);
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["update-index", "--index-info"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let stdin = child.stdin.as_mut().unwrap();
        for stage in 1..=3 {
            writeln!(stdin, "100644 {oid} {stage}\tconflicted").unwrap();
        }
    }
    assert!(child.wait().unwrap().success());
    let index = observe_git_index(root, &policy).unwrap();
    let stages: Vec<_> = index
        .entries
        .iter()
        .filter(|entry| entry.path.as_bytes() == b"conflicted")
        .map(|entry| entry.stage)
        .collect();
    assert_eq!(stages, vec![1, 2, 3]);
    assert!(index.tree.is_none());
}

#[test]
fn worktree_observer_retains_gitlink_identity_without_recursing() {
    let temp = TempDir::new().unwrap();
    fs::create_dir_all(temp.path().join("vendor/sub/ignored")).unwrap();
    fs::write(
        temp.path().join("vendor/sub/ignored/file"),
        b"not projected",
    )
    .unwrap();
    let policy = ConversionPolicy::new(GitHashAlgorithm::Sha1);
    let oid = GitObjectId::new(GitHashAlgorithm::Sha1, vec![7; 20]).unwrap();
    let bytes = b"0707070707070707070707070707070707070707".to_vec();
    let manifest = RepositoryManifest::new(
        SetId::ZERO,
        policy.root().content_key.clone(),
        vec![RepositoryEntry::new(
            RepoPath::from_bytes(b"vendor/sub").unwrap(),
            bytes,
            0o644,
            InodeKind::Gitlink,
            Some(oid.clone()),
            ManifestDisposition::Included,
        )
        .unwrap()],
    )
    .unwrap();
    let project = ProjectTree::from_manifest(manifest, &policy).unwrap();
    let index = GitIndexState::new(
        GitHashAlgorithm::Sha1,
        vec![GitIndexEntry {
            path: RepoPath::from_bytes(b"vendor/sub").unwrap(),
            stage: 0,
            mode: 0o160000,
            oid: Some(oid.clone()),
            intent_to_add: false,
            skip_worktree: false,
            assume_unchanged: false,
            sparse_directory: false,
        }],
    );
    let worktree = observe_worktree(temp.path(), Some(&index), &IdentityFilter, &policy).unwrap();
    assert_eq!(worktree.entries.len(), 1);
    assert_eq!(
        worktree.entries[0].repository_kind,
        Some(InodeKind::Gitlink)
    );
    assert_eq!(worktree.entries[0].gitlink.as_ref(), Some(&oid));
    assert!(compare_project_to_worktree(&project, &worktree, &policy).is_equivalent());
}

#[test]
fn required_filter_failure_fails_observation_closed() {
    let (temp, policy, _project) = fixture();
    let index = observe_git_index(temp.path(), &policy).unwrap();
    assert!(matches!(
        observe_worktree(temp.path(), Some(&index), &FailingFilter, &policy),
        Err(ObservationError::Filter { .. })
    ));
}

#[test]
fn adversarial_reports_are_structured_deterministic_and_joint() {
    let (_temp, policy, project) = fixture();
    let bad_oid = GitObjectId::new(GitHashAlgorithm::Sha1, vec![9; 20]).unwrap();
    let index = GitIndexState {
        version: GIT_INDEX_STATE_VERSION + 1,
        index_version: 4,
        object_format: GitHashAlgorithm::Sha1,
        sparse_index: false,
        entries: vec![
            GitIndexEntry {
                path: RepoPath::from_bytes(b"file").unwrap(),
                stage: 0,
                mode: 0o100755,
                oid: Some(bad_oid.clone()),
                intent_to_add: false,
                skip_worktree: false,
                assume_unchanged: false,
                sparse_directory: false,
            },
            GitIndexEntry {
                path: RepoPath::from_bytes(b"conflict").unwrap(),
                stage: 2,
                mode: 0o100644,
                oid: Some(bad_oid.clone()),
                intent_to_add: false,
                skip_worktree: false,
                assume_unchanged: false,
                sparse_directory: false,
            },
            GitIndexEntry {
                path: RepoPath::from_bytes(b"intent").unwrap(),
                stage: 0,
                mode: 0o100644,
                oid: Some(bad_oid.clone()),
                intent_to_add: true,
                skip_worktree: false,
                assume_unchanged: false,
                sparse_directory: false,
            },
            GitIndexEntry {
                path: RepoPath::from_bytes(b".vault/private").unwrap(),
                stage: 0,
                mode: 0o100644,
                oid: Some(bad_oid.clone()),
                intent_to_add: false,
                skip_worktree: false,
                assume_unchanged: false,
                sparse_directory: false,
            },
            GitIndexEntry {
                path: RepoPath::from_bytes(b"sparse").unwrap(),
                stage: 0,
                mode: 0o040000,
                oid: Some(bad_oid),
                intent_to_add: false,
                skip_worktree: true,
                assume_unchanged: false,
                sparse_directory: true,
            },
        ],
        tree: None,
    };
    let mut adversarial_platform = policy.platform.clone();
    adversarial_platform.case_sensitive = false;
    let worktree = WorktreeObservation::new(
        adversarial_platform,
        vec![
            observed(
                b"file",
                PhysicalKind::Regular,
                b"hello\r\n",
                Some(b"hello\r\n"),
                0o644,
            ),
            observed(
                b"EMPTY",
                PhysicalKind::Regular,
                b"not empty",
                Some(b"not empty"),
                0o644,
            ),
            observed(
                b"empty",
                PhysicalKind::Regular,
                b"not empty",
                Some(b"not empty"),
                0o644,
            ),
            observed(
                b"links/current",
                PhysicalKind::Symlink,
                b"wrong",
                Some(b"wrong"),
                0o777,
            ),
            observed(b"bin/run", PhysicalKind::Directory, b"", None, 0o755),
            observed(
                b"extra",
                PhysicalKind::Regular,
                b"extra",
                Some(b"extra"),
                0o644,
            ),
        ],
    );
    let claims = EquivalenceClaims {
        manifest_version: Some(99),
        object_algorithm: Some(GitHashAlgorithm::Sha256),
        manifest_root: Some("forged-manifest".into()),
        conversion_policy_root: Some("forged-policy".into()),
        git_tree_root: Some(GitObjectId::new(GitHashAlgorithm::Sha1, vec![8; 20]).unwrap()),
    };
    let first = compare_project_state(&project, &index, &worktree, &policy, &claims);
    let second = compare_project_state(&project, &index, &worktree, &policy, &claims);
    assert_eq!(first, second);
    let kinds: BTreeSet<_> = first
        .mismatches
        .iter()
        .map(|mismatch| mismatch.kind)
        .collect();
    for expected in [
        MismatchKind::ObjectHeader,
        MismatchKind::ClaimedRootForgery,
        MismatchKind::ConflictStage,
        MismatchKind::IntentToAdd,
        MismatchKind::SparseDirectory,
        MismatchKind::StaleContent,
        MismatchKind::Mode,
        MismatchKind::Kind,
        MismatchKind::LinkTarget,
        MismatchKind::NewlineOrFilterResult,
        MismatchKind::EmptyFile,
        MismatchKind::MissingPath,
        MismatchKind::ExtraPath,
        MismatchKind::ExclusionPolicy,
        MismatchKind::CaseFoldCollision,
    ] {
        assert!(
            kinds.contains(&expected),
            "missing {expected:?}: {first:#?}"
        );
    }
}

fn observed(
    path: &[u8],
    kind: PhysicalKind,
    worktree: &[u8],
    repository: Option<&[u8]>,
    mode: u16,
) -> WorktreeEntry {
    WorktreeEntry {
        path: RepoPath::from_bytes(path).unwrap(),
        physical_kind: kind,
        repository_kind: None,
        gitlink: None,
        mode: Some(mode),
        size: worktree.len() as u64,
        worktree_bytes: worktree.to_vec(),
        worktree_content: atomic_objects::content_key(worktree),
        repository_bytes_after_clean: repository.map(<[u8]>::to_vec),
        repository_content_after_clean: repository.map(atomic_objects::content_key),
        disposition: ManifestDisposition::Included,
        filter_warnings: Vec::new(),
        filter_error: None,
    }
}
