//! Test-only publication experiment, not a production importer. Real Git ODB
//! changes are compiled by existing synthesis once, then identical prepared
//! changes are replayed with the same staged verifier and journal protocol.
//! Timings exclude compilation and do not establish bounded-memory streaming.

use std::collections::{BTreeMap, BTreeSet};
use std::io::BufRead;
use std::time::Instant;

use atomic_core::change::{ChangeOrigin, GraphOp, InodeAttr, InodeKind};
use atomic_core::pristine::{
    GitShaIndexMutTxnT, GitShaIndexTxnT, InodeGraphOps, PathClaimTxnT, TreeTxnT,
};
use atomic_core::record::workflow::{
    record_added_file, DetectedFile, DetectionKind, RecordedFile, RecordingOptions,
};
use atomic_core::types::SetId;
use git2::{Oid, Signature, Time};

use super::*;
use crate::{
    ConversionPolicy, ManifestDisposition, ProjectTree, RepoPath, RepositoryEntry,
    RepositoryManifest,
};

struct Prepared {
    hash: Hash,
    change: Change,
    bytes: Vec<u8>,
    sha: String,
    expectation: StagedExpectation,
    deleted_paths: Vec<String>,
    source_node_id: atomic_core::types::NodeId,
}

struct Sample {
    // Retain storage while its already compiled payloads are used by replays.
    _source: tempfile::TempDir,
    _git: tempfile::TempDir,
    commits: Vec<Prepared>,
    workload: &'static str,
}

fn tagged(oid: Oid) -> GitObjectId {
    GitObjectId::new(GitHashAlgorithm::Sha1, oid.as_bytes().to_vec()).unwrap()
}

/// No checkout is created: both commit creation and reads use the Git ODB.
fn sample(count: usize) -> Sample {
    let git_dir = tempfile::tempdir().unwrap();
    let git = git2::Repository::init_bare(git_dir.path()).unwrap();
    let source = tempfile::tempdir().unwrap();
    let repo = Repository::init(source.path()).unwrap();
    let policy = ConversionPolicy::new(GitHashAlgorithm::Sha1);
    let mut parent = None;
    let mut subtree = None;
    let mut expected = BTreeMap::new();
    let mut closure = Vec::new();
    let mut commits = Vec::new();
    let started = Instant::now();
    for index in 0..count {
        let name = format!("file-{index:02}.txt");
        let path = format!("sample/{name}");
        let content = format!("sample commit {index}\n{}\n", "payload ".repeat(32));
        let blob = git.blob(content.as_bytes()).unwrap();
        let old = subtree.map(|oid| git.find_tree(oid).unwrap());
        let mut children = git.treebuilder(old.as_ref()).unwrap();
        children.insert(&name, blob, 0o100644).unwrap();
        let child_oid = children.write().unwrap();
        subtree = Some(child_oid);
        let mut root = git.treebuilder(None).unwrap();
        root.insert("sample", child_oid, 0o040000).unwrap();
        let tree = git.find_tree(root.write().unwrap()).unwrap();
        let signature = Signature::new(
            "Streaming Sample",
            "sample@example.invalid",
            &Time::new(1_700_000_000 + index as i64, 0),
        )
        .unwrap();
        let parent_commit = parent.map(|oid| git.find_commit(oid).unwrap());
        let parents: Vec<_> = parent_commit.iter().collect();
        let oid = git
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                &format!("add {path}"),
                &tree,
                &parents,
            )
            .unwrap();

        // Read the actual blob back from Git, never a materialized file.
        let read_blob = git
            .find_blob(tree.get_path(std::path::Path::new(&path)).unwrap().id())
            .unwrap();
        let bytes = read_blob.content();
        expected.insert(
            path.clone(),
            StagedPathExpectation {
                bytes: bytes.to_vec(),
                mode: 0o644,
                kind: InodeKind::Regular,
            },
        );
        let manifest = RepositoryManifest::new(
            SetId::ZERO,
            policy.root().content_key,
            expected
                .iter()
                .map(|(path, entry)| {
                    RepositoryEntry::new(
                        RepoPath::from_bytes(path.as_bytes()).unwrap(),
                        entry.bytes.clone(),
                        entry.mode,
                        entry.kind,
                        None,
                        ManifestDisposition::Included,
                    )
                    .unwrap()
                })
                .collect(),
        )
        .unwrap();
        let project = ProjectTree::from_manifest(manifest, &policy).unwrap();
        let verified =
            super::super::verify_prospective_equivalence(&project, &tagged(tree.id())).unwrap();
        let memory = atomic_core::output::memory::Memory::new();
        memory.add_file(&path, bytes);
        let recorded = record_added_file(
            &memory,
            &DetectedFile::added(&path),
            &RecordingOptions::new(),
        )
        .unwrap();
        let header = ChangeHeader::builder()
            .message(format!("add {path}"))
            .timestamp(chrono::DateTime::from_timestamp(1_700_000_000 + index as i64, 0).unwrap())
            .build();
        let origin = match parent {
            Some(parent) => {
                GitSynthesisOrigin::first_parent(tagged(oid), tagged(tree.id()), tagged(parent))
            }
            None => GitSynthesisOrigin::root(tagged(oid), tagged(tree.id())),
        }
        .unwrap();
        let expectation = StagedExpectation {
            paths: Vec::new(),
            tree: Some(StagedTreeExpectation {
                entries: expected.clone(),
                semantic_paths: vec![path.clone()],
                verify_paths: None,
                semantic_verify_paths: None,
            }),
        };
        let outcome = repo
            .synthesize_git_change(
                header,
                &[recorded],
                origin.clone(),
                git_synthesis_metadata(&origin, &[], None),
                Vec::new(),
                &[],
                false,
                &verified,
                Default::default(),
                &[],
                &expectation,
                Some(&oid.to_string()),
                &closure,
                &[],
            )
            .unwrap();
        closure.push(outcome.write.hash);
        let change = repo.load_change(&outcome.write.hash).unwrap();
        assert!(matches!(
            change.origin(),
            ChangeOrigin::GitSynthesized { .. }
        ));
        assert!(!source.path().join("sample").exists());
        commits.push(Prepared {
            hash: outcome.write.hash,
            bytes: std::fs::read(repo.change_store.change_path(&outcome.write.hash)).unwrap(),
            change,
            sha: oid.to_string(),
            expectation,
            deleted_paths: Vec::new(),
            source_node_id: repo
                .pristine
                .read_txn()
                .unwrap()
                .get_internal(&outcome.write.hash)
                .unwrap()
                .unwrap(),
        });
        parent = Some(oid);
    }
    println!(
        "SAMPLE preparation commits={count} elapsed_ms={:.3} (excluded from replay timings)",
        started.elapsed().as_secs_f64() * 1000.0
    );
    Sample {
        _source: source,
        _git: git_dir,
        commits,
        workload: "add-only",
    }
}

enum Edit {
    Put(&'static str, Vec<u8>, u16),
    Delete(&'static str),
    Move(&'static str, &'static str, Vec<u8>),
}

fn hot_content(cycle: usize, version: &str) -> Vec<u8> {
    (0..12)
        .map(|line| {
            format!(
                "cycle {cycle} line {line}: {}\n",
                if line == 5 { version } else { "stable" }
            )
        })
        .collect::<String>()
        .into_bytes()
}

/// A repeatable 16-commit lifecycle. The 32-commit history has an identical
/// first half, then repeats against the accumulated graph and semantic state.
fn mixed_edits(index: usize) -> (&'static str, Vec<Edit>) {
    let cycle = index / 16;
    let hot = "sample/hot.txt";
    let moved = "sample/moved.txt";
    let retired = "sample/retired/deep/file.txt";
    let binary = "sample/blob.bin";
    match index % 16 {
        0 => (
            "multi-add-or-edit",
            vec![
                Edit::Put(hot, hot_content(cycle, "base"), 0o644),
                Edit::Put(
                    "sample/stable.txt",
                    format!("stable cycle {cycle}\n").into_bytes(),
                    0o644,
                ),
            ],
        ),
        1 => (
            "localized-edit",
            vec![Edit::Put(hot, hot_content(cycle, "edit"), 0o644)],
        ),
        2 => (
            "nested-empty-add",
            vec![Edit::Put(retired, Vec::new(), 0o644)],
        ),
        3 => (
            "binary-add",
            vec![Edit::Put(binary, vec![0, 1, 2, 0xff, cycle as u8], 0o644)],
        ),
        4 => (
            "chmod",
            vec![Edit::Put(hot, hot_content(cycle, "edit"), 0o755)],
        ),
        5 => (
            "rename",
            vec![Edit::Move(hot, moved, hot_content(cycle, "edit"))],
        ),
        6 => (
            "edit-after-rename",
            vec![Edit::Put(moved, hot_content(cycle, "moved edit"), 0o755)],
        ),
        7 => ("retire-directories", vec![Edit::Delete(retired)]),
        8 => (
            "recreate-directories",
            vec![Edit::Put(retired, b"recreated\n".to_vec(), 0o644)],
        ),
        9 => (
            "binary-edit",
            vec![Edit::Put(
                binary,
                vec![0, 9, 8, 7, 0xfe, cycle as u8],
                0o644,
            )],
        ),
        10 => ("truncate", vec![Edit::Put(moved, Vec::new(), 0o755)]),
        11 => ("chmod-back", vec![Edit::Put(moved, Vec::new(), 0o644)]),
        12 => ("binary-delete", vec![Edit::Delete(binary)]),
        13 => (
            "rename-and-edit",
            vec![Edit::Move(moved, hot, hot_content(cycle, "restored"))],
        ),
        14 => ("retire-recreated-directories", vec![Edit::Delete(retired)]),
        15 => ("empty-commit", Vec::new()),
        _ => unreachable!(),
    }
}

/// Git tree construction only; this is not Atomic graph manipulation.
fn git_tree(
    git: &git2::Repository,
    entries: &BTreeMap<String, StagedPathExpectation>,
    prefix: &str,
) -> Oid {
    let mut builder = git.treebuilder(None).unwrap();
    let mut directories = BTreeSet::new();
    for (path, entry) in entries {
        let Some(rest) = path.strip_prefix(prefix) else {
            continue;
        };
        if let Some((directory, _)) = rest.split_once('/') {
            directories.insert(directory);
        } else {
            builder
                .insert(
                    rest,
                    git.blob(&entry.bytes).unwrap(),
                    0o100000 | i32::from(entry.mode),
                )
                .unwrap();
        }
    }
    for directory in directories {
        let oid = git_tree(git, entries, &format!("{prefix}{directory}/"));
        builder.insert(directory, oid, 0o040000).unwrap();
    }
    builder.write().unwrap()
}

fn mixed_sample(count: usize) -> Sample {
    assert!(matches!(count, 16 | 32));
    let git_dir = tempfile::tempdir().unwrap();
    let git = git2::Repository::init_bare(git_dir.path()).unwrap();
    let source = tempfile::tempdir().unwrap();
    let repo = Repository::init(source.path()).unwrap();
    let policy = ConversionPolicy::new(GitHashAlgorithm::Sha1);
    let mut parent = None;
    let mut expected: BTreeMap<String, StagedPathExpectation> = BTreeMap::new();
    let mut closure = Vec::new();
    let mut commits = Vec::new();
    let mut coverage = BTreeMap::new();
    let started = Instant::now();
    for index in 0..count {
        let (label, edits) = mixed_edits(index);
        *coverage.entry(label).or_insert(0) += 1;
        let previous = expected.clone();
        for edit in &edits {
            match edit {
                Edit::Put(path, bytes, mode) => {
                    expected.insert(
                        (*path).into(),
                        StagedPathExpectation {
                            bytes: bytes.clone(),
                            mode: *mode,
                            kind: InodeKind::Regular,
                        },
                    );
                }
                Edit::Delete(path) => {
                    expected.remove(*path).unwrap();
                }
                Edit::Move(old, new, bytes) => {
                    let mut entry = expected.remove(*old).unwrap();
                    entry.bytes = bytes.clone();
                    assert!(expected.insert((*new).into(), entry).is_none());
                }
            }
        }
        let tree = git.find_tree(git_tree(&git, &expected, "")).unwrap();
        let signature = Signature::new(
            "Streaming Sample",
            "sample@example.invalid",
            &Time::new(1_700_000_000 + index as i64, 0),
        )
        .unwrap();
        let parent_commit = parent.map(|oid| git.find_commit(oid).unwrap());
        let parents: Vec<_> = parent_commit.iter().collect();
        let oid = git
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                label,
                &tree,
                &parents,
            )
            .unwrap();
        // Replace fixture buffers with bytes read from the real Git tree/ODB.
        for (path, entry) in &mut expected {
            let git_entry = tree.get_path(std::path::Path::new(path)).unwrap();
            let blob = git.find_blob(git_entry.id()).unwrap();
            assert_eq!(blob.content(), entry.bytes);
            assert_eq!(git_entry.filemode() & 0o777, i32::from(entry.mode));
            entry.bytes = blob.content().to_vec();
        }
        let manifest = RepositoryManifest::new(
            SetId::ZERO,
            policy.root().content_key,
            expected
                .iter()
                .map(|(path, entry)| {
                    RepositoryEntry::new(
                        RepoPath::from_bytes(path.as_bytes()).unwrap(),
                        entry.bytes.clone(),
                        entry.mode,
                        entry.kind,
                        None,
                        ManifestDisposition::Included,
                    )
                    .unwrap()
                })
                .collect(),
        )
        .unwrap();
        let project = ProjectTree::from_manifest(manifest, &policy).unwrap();
        let verified =
            super::super::verify_prospective_equivalence(&project, &tagged(tree.id())).unwrap();
        let mut recorded = Vec::new();
        let mut deleted_paths = Vec::new();
        for edit in &edits {
            match edit {
                Edit::Put(path, _, _) => {
                    let entry = &expected[*path];
                    let mut record = match previous.get(*path) {
                        None => {
                            let memory = atomic_core::output::memory::Memory::new();
                            memory.add_file(path, &entry.bytes);
                            record_added_file(
                                &memory,
                                &DetectedFile::added(*path),
                                &RecordingOptions::new(),
                            )
                            .unwrap()
                        }
                        Some(old) if old.bytes != entry.bytes => repo
                            .record_foreign_modified_file(
                                path,
                                &old.bytes,
                                &entry.bytes,
                                old.bytes.contains(&0) || entry.bytes.contains(&0),
                            )
                            .unwrap_or_else(|error| {
                                panic!("commit {index} {label} {path}: {error}")
                            }),
                        Some(old) if old.mode != entry.mode => {
                            let (inode, position) =
                                repo.get_inode_and_position(path).unwrap().unwrap();
                            let mut record = RecordedFile::new(*path);
                            record.set_kind(DetectionKind::Modified);
                            record.set_inode(inode);
                            record.set_position(position);
                            record
                        }
                        Some(_) => continue,
                    };
                    record.set_attr(InodeAttr::Mode(entry.mode));
                    record.set_attr(InodeAttr::Kind(entry.kind));
                    recorded.push(record);
                }
                Edit::Delete(path) => {
                    recorded.push(repo.record_foreign_deleted_file(path).unwrap());
                    deleted_paths.push((*path).into());
                }
                Edit::Move(old, new, _) => {
                    recorded.push(
                        repo.record_foreign_moved_file(
                            repo.current_view(),
                            new,
                            old,
                            &previous[*old].bytes,
                            &expected[*new].bytes,
                            false,
                        )
                        .unwrap_or_else(|error| panic!("commit {index} {label}: {error}")),
                    );
                }
            }
        }
        let semantic_paths = recorded
            .iter()
            .filter(|record| record.crdt_ops().is_some())
            .map(|record| record.path().to_string())
            .collect();
        let expectation = StagedExpectation {
            paths: Vec::new(),
            tree: Some(StagedTreeExpectation {
                entries: expected.clone(),
                semantic_paths,
                verify_paths: None,
                semantic_verify_paths: None,
            }),
        };
        let header = ChangeHeader::builder()
            .message(format!("{index}: {label}"))
            .timestamp(chrono::DateTime::from_timestamp(1_700_000_000 + index as i64, 0).unwrap())
            .build();
        let origin = GitSynthesisOrigin::with_derivation(
            tagged(oid),
            tagged(tree.id()),
            parent.map(tagged).into_iter().collect(),
            if parent.is_none() {
                atomic_core::change::GitDerivation::Root
            } else if edits.is_empty() {
                atomic_core::change::GitDerivation::EmptyCommit
            } else {
                atomic_core::change::GitDerivation::FirstParent
            },
        )
        .unwrap();
        let outcome = repo
            .synthesize_git_change(
                header,
                &recorded,
                origin.clone(),
                git_synthesis_metadata(&origin, &[], None),
                Vec::new(),
                &deleted_paths,
                false,
                &verified,
                Default::default(),
                &[],
                &expectation,
                Some(&oid.to_string()),
                &closure,
                &[],
            )
            .unwrap_or_else(|error| panic!("source commit {index} {label}: {error}"));
        let change = repo.load_change(&outcome.write.hash).unwrap();
        match index % 16 {
            4 | 11 => assert!(change
                .hunks()
                .iter()
                .any(|op| matches!(op, GraphOp::SetAttr { .. }))),
            5 | 13 => assert!(change
                .hunks()
                .iter()
                .any(|op| matches!(op, GraphOp::FileMove { .. }))),
            7 | 14 => assert!(change
                .hunks()
                .iter()
                .any(|op| matches!(op, GraphOp::DirDel { .. }))),
            15 => assert!(change.hunks().is_empty()),
            _ => {}
        }
        closure.push(outcome.write.hash);
        assert_namespace(&repo, &repo.pristine.read_txn().unwrap(), &expectation);
        assert!(!source.path().join("sample").exists());
        commits.push(Prepared {
            hash: outcome.write.hash,
            bytes: std::fs::read(repo.change_store.change_path(&outcome.write.hash)).unwrap(),
            change,
            sha: oid.to_string(),
            expectation,
            deleted_paths,
            source_node_id: repo
                .pristine
                .read_txn()
                .unwrap()
                .get_internal(&outcome.write.hash)
                .unwrap()
                .unwrap(),
        });
        parent = Some(oid);
    }
    println!("SAMPLE preparation workload=mixed commits={count} elapsed_ms={:.3} coverage={coverage:?} (excluded from replay timings)", started.elapsed().as_secs_f64() * 1000.0);
    Sample {
        _source: source,
        _git: git_dir,
        commits,
        workload: "mixed",
    }
}

/// Full namespace check includes implicit directories, which Git tree/blob
/// equality alone cannot catch when the last descendant has been deleted.
fn assert_namespace<T>(repo: &Repository, txn: &T, expectation: &StagedExpectation)
where
    T: ViewTxnT
        + TreeTxnT
        + PathClaimTxnT
        + InodeGraphOps<InodeError = atomic_core::pristine::PristineError>,
{
    let view = txn.get_view(repo.current_view()).unwrap().unwrap();
    let visibility = super::super::filter::graph_visibility_closure(txn, &view).unwrap();
    let projection = repo.project_tree_for_visibility(txn, &visibility).unwrap();
    assert!(projection.name_conflicts.is_empty());
    let mut expected = BTreeSet::new();
    for path in expectation.tree.as_ref().unwrap().entries.keys() {
        for ancestor in std::path::Path::new(path).ancestors() {
            let text = ancestor.to_str().unwrap();
            if !text.is_empty() {
                expected.insert(text.to_string());
            }
        }
    }
    assert_eq!(
        projection.present.keys().cloned().collect::<BTreeSet<_>>(),
        expected
    );
}

/// Proof scope for the test-only coordinator, not a production import policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SampleVerification {
    EachCommit,
    FinalTip,
    PrivateStage,
}

impl SampleVerification {
    fn name(self) -> &'static str {
        match self {
            Self::EachCommit => "each-commit",
            Self::FinalTip => "final-tip",
            Self::PrivateStage => "private-stage-final-tip",
        }
    }
}

fn sample_expectation(entry: &Prepared, corrupt: bool) -> StagedExpectation {
    let mut expectation = entry.expectation.clone();
    if corrupt {
        expectation
            .tree
            .as_mut()
            .unwrap()
            .entries
            .values_mut()
            .next()
            .unwrap()
            .bytes
            .push(0xff);
    }
    expectation
}

/// Default sample publication keeps every historical-state proof enabled.
fn publish(
    repo: &Repository,
    entries: &[Prepared],
    parent_closure: &[Hash],
    corrupt_at: Option<usize>,
) -> Result<atomic_core::types::OperationId, RepositoryError> {
    publish_with_verification(
        repo,
        entries,
        parent_closure,
        corrupt_at,
        SampleVerification::EachCommit,
    )
}

fn publish_with_verification(
    repo: &Repository,
    entries: &[Prepared],
    parent_closure: &[Hash],
    corrupt_at: Option<usize>,
    verification: SampleVerification,
) -> Result<atomic_core::types::OperationId, RepositoryError> {
    publish_to_view(
        repo,
        repo.current_view(),
        entries,
        parent_closure,
        corrupt_at,
        verification,
    )
}

const PRIVATE_IMPORT_VIEW: &str = "git-import-private";

fn publish_to_view(
    repo: &Repository,
    view_name: &str,
    entries: &[Prepared],
    parent_closure: &[Hash],
    corrupt_at: Option<usize>,
    verification: SampleVerification,
) -> Result<atomic_core::types::OperationId, RepositoryError> {
    if entries.is_empty() {
        return Err(RepositoryError::InvalidOperation {
            message: "empty publication sample".into(),
        });
    }
    let publish_started = Instant::now();
    let mut save_ms = 0;
    let mut apply_ms = 0;
    let mut projection_verification_ms = 0;
    let mut namespace_ms = 0.0;
    let mut final_tip_verification_ms = 0.0;
    let mut commit_disk_ms = 0.0;
    let wc = repo.require_working_copy_id()?;
    let lock = repo.try_lock_operation(wc)?;
    let view = repo.pristine.read_txn()?.get_view(view_name)?.unwrap();
    if verification == SampleVerification::PrivateStage {
        let dev = repo
            .pristine
            .read_txn()?
            .get_view(repo.current_view())?
            .unwrap();
        if view_name != PRIVATE_IMPORT_VIEW
            || view.kind != atomic_core::pristine::ViewScope::Draft
            || view.parent != Some(dev.id)
            || dev.change_count != 0
            || dev.state != Merkle::ZERO
        {
            return Err(RepositoryError::InvalidOperation { message: "unchecked batches require the disposable private draft with an untouched empty dev parent".into() });
        }
    }
    if verification == SampleVerification::FinalTip
        && (!parent_closure.is_empty() || view.change_count != 0)
    {
        return Err(RepositoryError::InvalidOperation { message: "final-tip sample requires one complete transaction into an empty disposable view; do not publish unchecked intermediate batches".into() });
    }
    let before_record = repo.working_copy_record(wc)?;
    let mut after_record = before_record.clone();
    let mut after_state = view.state;
    for entry in entries {
        after_state = after_state.next(&entry.hash);
    }
    if after_record.desired_view == view.id {
        after_record.desired_state = after_state;
    }
    let state = |record, merkle| RepoStateRef {
        view: Some(ViewStateRef {
            name: view_name.into(),
            state: merkle,
            set_id: None,
        }),
        working_copy: Some(working_copy_state_ref(record)),
        git: None,
    };
    let operation = repo.prepare_metadata_operation(
        &lock,
        OperationKind::SynthesizeGit,
        None,
        state(before_record, view.state),
        state(after_record, after_state),
        entries
            .iter()
            .enumerate()
            .map(|(index, entry)| MetadataTransition {
                target: MetadataTarget::ViewChange {
                    view: view_name.into(),
                    change: entry.hash,
                },
                expected_old: MetadataValue::Absent,
                expected_new: MetadataValue::Sequence(view.change_count + index as u64),
            })
            .collect(),
        entries.iter().map(|entry| entry.hash).collect(),
        ActorRef::System {
            name: "git-streaming-sample-test".into(),
        },
        current_operation_timestamp_ms(),
    )?;
    let prepare_ms = publish_started.elapsed().as_secs_f64() * 1000.0;
    let result = (|| {
        let mut txn = lock.begin_write_immediate()?;
        let mut closure = parent_closure.to_vec();
        for (index, entry) in entries.iter().enumerate() {
            let expectation = match verification {
                SampleVerification::EachCommit => {
                    sample_expectation(entry, corrupt_at == Some(index))
                }
                SampleVerification::FinalTip | SampleVerification::PrivateStage => {
                    StagedExpectation::empty()
                }
            };
            let staged = repo.apply_synthesized_change_in_txn(
                &mut txn,
                view_name,
                &entry.hash,
                &entry.bytes,
                &entry.change,
                &entry.deleted_paths,
                view_name != repo.current_view(),
                &Default::default(),
                &[],
                &expectation,
                Some(&entry.sha),
                &closure,
                &[],
            );
            let write = match staged {
                Ok(write) => write,
                Err(error) => {
                    eprintln!(
                        "SAMPLE staged failure commit={} source_node={:?} target_node={:?}: {error}",
                        entry.change.hashed.header.message,
                        entry.source_node_id,
                        txn.get_internal(&entry.hash)?
                    );
                    return Err(error);
                }
            };
            save_ms += write.timings.save_ms;
            apply_ms += write.timings.apply_ms;
            projection_verification_ms += write.timings.commit_ms;
            let id = txn.get_internal(&entry.hash)?.unwrap();
            txn.put_git_sha(&entry.sha, id)?;
            closure.push(entry.hash);
            if verification == SampleVerification::EachCommit {
                let namespace = Instant::now();
                assert_namespace(repo, &*txn, &entry.expectation);
                namespace_ms += namespace.elapsed().as_secs_f64() * 1000.0;
            }
            // This is checked after every staged entry, before batch commit.
            assert_no_checkout(repo);
        }
        if verification == SampleVerification::FinalTip {
            let last = entries.last().unwrap();
            let expectation = sample_expectation(last, corrupt_at == Some(entries.len() - 1));
            let final_verify = Instant::now();
            repo.verify_staged_projection(
                &txn,
                view_name,
                &last.change,
                &[],
                &expectation,
                &last.hash,
                true,
            )?;
            final_tip_verification_ms = final_verify.elapsed().as_secs_f64() * 1000.0;
            let namespace = Instant::now();
            assert_namespace(repo, &*txn, &expectation);
            namespace_ms += namespace.elapsed().as_secs_f64() * 1000.0;
            assert_no_checkout(repo);
        }
        let commit = Instant::now();
        txn.commit()?;
        commit_disk_ms = commit.elapsed().as_secs_f64() * 1000.0;
        Ok(())
    })();
    match result {
        Ok(()) => {
            let finalize = Instant::now();
            repo.apply_operation_metadata_locked(&lock, operation.id())?;
            repo.finalize_operation_verified(&lock, operation.id())?;
            if let Some(path) = std::env::var_os("ATOMIC_REAL_SAMPLE_TIMINGS") {
                use std::io::Write;
                let row = serde_json::json!({
                    "commits": entries.len(), "first_sha": entries.first().unwrap().sha,
                    "verification_scope": verification.name(),
                    "last_sha": entries.last().unwrap().sha, "prepare_ms": prepare_ms,
                    "save_ms": save_ms, "apply_ms": apply_ms,
                    "projection_verification_ms": projection_verification_ms,
                    "final_tip_verification_ms": final_tip_verification_ms,
                    "namespace_guard_ms": namespace_ms, "graph_commit_disk_ms": commit_disk_ms,
                    "finalize_ms": finalize.elapsed().as_secs_f64() * 1000.0,
                    "total_ms": publish_started.elapsed().as_secs_f64() * 1000.0,
                });
                println!("HYPERFINE phases {row}");
                let mut file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)?;
                writeln!(&mut file, "{row}")?;
            }
            Ok(operation.id())
        }
        Err(error) => {
            repo.abort_prepared_metadata_operation(&lock, &operation)?;
            Err(error)
        }
    }
}

fn target() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    repo.pristine
        .require_repository_capability(super::super::CHANGE_FORMAT_VNEXT_CAPABILITY)
        .unwrap();
    (dir, repo)
}

fn create_private_stage(repo: &mut Repository) {
    let parent = repo.current_view().to_string();
    repo.create_draft_view(PRIVATE_IMPORT_VIEW, &parent)
        .unwrap();
}

fn assert_dev_unpublished(repo: &Repository) {
    let txn = repo.pristine.read_txn().unwrap();
    let dev = txn.get_view(repo.current_view()).unwrap().unwrap();
    assert_eq!(dev.change_count, 0);
    assert_eq!(dev.state, Merkle::ZERO);
    let visibility = super::super::filter::graph_visibility_closure(&txn, &dev).unwrap();
    assert!(repo
        .project_tree_for_visibility(&txn, &visibility)
        .unwrap()
        .present
        .is_empty());
    let record = repo
        .working_copy_record(repo.require_working_copy_id().unwrap())
        .unwrap();
    assert_eq!(record.desired_view, dev.id);
    assert_eq!(record.desired_state, Merkle::ZERO);
    assert_no_checkout(repo);
}

/// Only view references are written here: graph/semantic application happened
/// in the private batches through the existing engine. The complete actual dev
/// projection is proved before this metadata transaction is allowed to commit.
fn promote_private_stage(
    repo: &Repository,
    entries: &[Prepared],
    corrupt_final: bool,
) -> Result<(atomic_core::types::OperationId, f64), RepositoryError> {
    let started = Instant::now();
    let wc = repo.require_working_copy_id()?;
    let lock = repo.try_lock_operation(wc)?;
    let view_name = repo.current_view();
    let txn = repo.pristine.read_txn()?;
    let dev = txn.get_view(view_name)?.unwrap();
    let stage = txn.get_view(PRIVATE_IMPORT_VIEW)?.unwrap();
    if entries.is_empty()
        || dev.change_count != 0
        || dev.state != Merkle::ZERO
        || stage.parent != Some(dev.id)
        || stage.kind != atomic_core::pristine::ViewScope::Draft
    {
        return Err(RepositoryError::InvalidOperation {
            message: "private sample promotion requires a complete stage and untouched empty dev"
                .into(),
        });
    }
    let staged: Vec<_> = txn
        .iter_changes(&stage, 0)?
        .map(|row| {
            let (_, id, _) = row?;
            txn.get_external(id)?
                .ok_or_else(|| RepositoryError::InvalidOperation {
                    message: "stage member has no hash".into(),
                })
        })
        .collect::<Result<_, RepositoryError>>()?;
    let hashes: Vec<_> = entries.iter().map(|entry| entry.hash).collect();
    if staged != hashes
        || stage.state
            != hashes
                .iter()
                .fold(Merkle::ZERO, |state, hash| state.next(hash))
    {
        return Err(RepositoryError::InvalidOperation {
            message: "private stage does not match the exact selected source history".into(),
        });
    }
    let available: std::collections::HashSet<_> = hashes.iter().copied().collect();
    if entries.iter().any(|entry| {
        entry
            .change
            .dependencies()
            .iter()
            .any(|hash| !available.contains(hash))
    }) {
        return Err(RepositoryError::InvalidOperation {
            message: "private sample promotion would omit a dependency".into(),
        });
    }
    drop(txn);
    let before_record = repo.working_copy_record(wc)?;
    let mut after_record = before_record.clone();
    after_record.desired_state = stage.state;
    let state = |record, merkle| RepoStateRef {
        view: Some(ViewStateRef {
            name: view_name.into(),
            state: merkle,
            set_id: None,
        }),
        working_copy: Some(working_copy_state_ref(record)),
        git: None,
    };
    let operation = repo.prepare_metadata_operation(
        &lock,
        OperationKind::SynthesizeGit,
        None,
        state(before_record, dev.state),
        state(after_record, stage.state),
        hashes
            .iter()
            .enumerate()
            .map(|(index, hash)| MetadataTransition {
                target: MetadataTarget::ViewChange {
                    view: view_name.into(),
                    change: *hash,
                },
                expected_old: MetadataValue::Absent,
                expected_new: MetadataValue::Sequence(index as u64),
            })
            .collect(),
        hashes,
        ActorRef::System {
            name: "git-private-import-final-promotion-test".into(),
        },
        current_operation_timestamp_ms(),
    )?;
    let result = (|| {
        let mut txn = lock.begin_write_immediate()?;
        let mut target = txn.get_view(view_name)?.unwrap();
        for entry in entries {
            let id = txn.get_internal(&entry.hash)?.unwrap();
            txn.put_change(&mut target, id, &entry.hash)?;
        }
        txn.update_view(&target)?;
        repo.realign_tree_projection_in_txn(&mut txn, view_name)?;
        let last = entries.last().unwrap();
        let expectation = sample_expectation(last, corrupt_final);
        repo.verify_staged_projection(
            &txn,
            view_name,
            &last.change,
            &[],
            &expectation,
            &last.hash,
            true,
        )?;
        assert_namespace(repo, &*txn, &expectation);
        assert_no_checkout(repo);
        txn.commit()
    })();
    match result {
        Ok(()) => {
            repo.apply_operation_metadata_locked(&lock, operation.id())?;
            repo.finalize_operation_verified(&lock, operation.id())?;
            Ok((operation.id(), started.elapsed().as_secs_f64() * 1000.0))
        }
        Err(error) => {
            repo.abort_prepared_metadata_operation(&lock, &operation)?;
            Err(error)
        }
    }
}

fn assert_prefix(repo: &Repository, entries: &[Prepared]) {
    let txn = repo.pristine.read_txn().unwrap();
    let view = txn.get_view(repo.current_view()).unwrap().unwrap();
    let actual: Vec<_> = txn
        .iter_changes(&view, 0)
        .unwrap()
        .map(|row| {
            let (_, id, _) = row.unwrap();
            txn.get_external(id).unwrap().unwrap()
        })
        .collect();
    assert_eq!(
        actual,
        entries.iter().map(|entry| entry.hash).collect::<Vec<_>>()
    );
    let expected_state = entries
        .iter()
        .fold(Merkle::ZERO, |state, entry| state.next(&entry.hash));
    assert_eq!(view.state, expected_state);
    if let Some(last) = entries.last() {
        assert_namespace(repo, &txn, &last.expectation);
    }
    for (index, entry) in entries.iter().enumerate() {
        assert_eq!(
            txn.get_external(txn.get_by_git_sha(&entry.sha).unwrap().unwrap())
                .unwrap(),
            Some(entry.hash)
        );
        let mut expected_closure: Vec<_> =
            entries[..=index].iter().map(|entry| entry.hash).collect();
        expected_closure.sort();
        assert_eq!(
            repo.git_commit_closure(&entry.sha).unwrap().unwrap(),
            expected_closure
        );
    }
    drop(txn);
    if let Some(last) = entries.last() {
        for (path, expected) in &last.expectation.tree.as_ref().unwrap().entries {
            assert_eq!(
                repo.get_file_content(path).unwrap().as_deref(),
                Some(expected.bytes.as_slice())
            );
            let attrs = repo
                .projected_attributes_on_view_excluding(path, repo.current_view(), &[])
                .unwrap()
                .unwrap();
            assert!(!attrs.is_conflicted());
            assert_eq!(attrs.materialization.mode, expected.mode);
            assert_eq!(attrs.materialization.kind, expected.kind);
        }
    }
    assert_no_checkout(repo);
}

fn assert_no_checkout(repo: &Repository) {
    for entry in std::fs::read_dir(repo.root()).unwrap() {
        assert_eq!(
            entry.unwrap().file_name(),
            ".atomic",
            "sample materialized a working file before final checkout"
        );
    }
}

#[test]
fn failed_sample_batch_rolls_back_then_retries_without_materialization() {
    let sample = sample(4);
    let (dir, repo) = target();
    let wc = repo.require_working_copy_id().unwrap();
    let initial_materialized = repo.working_copy_record(wc).unwrap().materialized_state;
    publish(&repo, &sample.commits[..2], &[], None).unwrap();
    let parent: Vec<_> = sample.commits[..2].iter().map(|entry| entry.hash).collect();
    assert!(publish(&repo, &sample.commits[2..], &parent, Some(1)).is_err());
    assert_prefix(&repo, &sample.commits[..2]);
    assert!(!repo
        .pristine
        .read_txn()
        .unwrap()
        .has_git_sha(&sample.commits[2].sha)
        .unwrap());
    assert!(!repo
        .pristine
        .read_txn()
        .unwrap()
        .has_git_sha(&sample.commits[3].sha)
        .unwrap());
    publish(&repo, &sample.commits[2..], &parent, None).unwrap();
    assert_prefix(&repo, &sample.commits);
    assert_eq!(
        repo.working_copy_record(wc).unwrap().materialized_state,
        initial_materialized
    );
    drop(repo);
    let reopened = Repository::open(dir.path()).unwrap();
    assert_prefix(&reopened, &sample.commits);
    // The only materialization call is after all writes and verification.
    reopened.materialize(wc).unwrap();
    for (path, expected) in &sample
        .commits
        .last()
        .unwrap()
        .expectation
        .tree
        .as_ref()
        .unwrap()
        .entries
    {
        assert_eq!(
            std::fs::read(dir.path().join(path)).unwrap(),
            expected.bytes
        );
    }
}

#[test]
#[ignore = "manual small publication timing experiment; use --ignored --nocapture"]
fn compare_sixteen_git_commits_in_journaled_batches() {
    let sample = sample(16);
    benchmark_replay(&sample, &[1, 4, 16]);
}

#[test]
fn final_tip_verification_checks_final_state_not_intermediate_expectations() {
    let sample = sample(4);
    let (_strict_dir, strict) = target();
    assert!(publish(&strict, &sample.commits, &[], Some(1)).is_err());
    assert_prefix(&strict, &[]);

    let (_tip_dir, tip) = target();
    // An intermediate Git expectation is deliberately wrong. Final-tip proof
    // cannot certify it; only the complete final expectation is authoritative.
    let id = publish_with_verification(
        &tip,
        &sample.commits,
        &[],
        Some(1),
        SampleVerification::FinalTip,
    )
    .unwrap();
    assert_prefix(&tip, &sample.commits);
    assert_eq!(
        tip.operation_details(id).unwrap().verification,
        super::super::OperationVerificationState::Verified
    );

    let (_wrong_dir, wrong) = target();
    let error = publish_with_verification(
        &wrong,
        &sample.commits,
        &[],
        Some(3),
        SampleVerification::FinalTip,
    )
    .unwrap_err();
    assert!(error.to_string().contains("staged path"), "{error}");
    assert_prefix(&wrong, &[]);
    for entry in &sample.commits {
        assert!(!wrong
            .pristine
            .read_txn()
            .unwrap()
            .has_git_sha(&entry.sha)
            .unwrap());
    }
}

#[test]
fn final_tip_sample_refuses_appending_unchecked_batches() {
    let sample = sample(2);
    let (_dir, repo) = target();
    publish(&repo, &sample.commits[..1], &[], None).unwrap();
    let error = publish_with_verification(
        &repo,
        &sample.commits[1..],
        &[sample.commits[0].hash],
        None,
        SampleVerification::FinalTip,
    )
    .unwrap_err();
    assert!(error.to_string().contains("one complete transaction"));
    assert_prefix(&repo, &sample.commits[..1]);
}

#[test]
fn private_batches_stay_out_of_dev_until_final_verified_promotion() {
    let sample = mixed_sample(16);
    let (_dir, mut repo) = target();
    create_private_stage(&mut repo);
    let mut closure = Vec::new();
    assert!(publish_with_verification(
        &repo,
        &sample.commits[..4],
        &[],
        None,
        SampleVerification::PrivateStage
    )
    .is_err());
    for entries in sample.commits.chunks(4) {
        publish_to_view(
            &repo,
            PRIVATE_IMPORT_VIEW,
            entries,
            &closure,
            None,
            SampleVerification::PrivateStage,
        )
        .unwrap();
        closure.extend(entries.iter().map(|entry| entry.hash));
        assert_dev_unpublished(&repo);
    }
    assert!(promote_private_stage(&repo, &sample.commits, true).is_err());
    assert_dev_unpublished(&repo);
    assert_eq!(
        repo.pristine
            .read_txn()
            .unwrap()
            .get_view(PRIVATE_IMPORT_VIEW)
            .unwrap()
            .unwrap()
            .change_count,
        16
    );
    let (operation, _) = promote_private_stage(&repo, &sample.commits, false).unwrap();
    assert_eq!(
        repo.operation_details(operation).unwrap().verification,
        super::super::OperationVerificationState::Verified
    );
    assert_prefix(&repo, &sample.commits);
    materialize_and_assert(&repo, &sample.commits);
}

fn materialize_and_assert(repo: &Repository, entries: &[Prepared]) {
    // The only checkout phase is after complete publication and graph checks.
    let wc = repo.require_working_copy_id().unwrap();
    assert_prefix(repo, entries);
    repo.materialize(wc).unwrap();
    for (path, expected) in &entries
        .last()
        .unwrap()
        .expectation
        .tree
        .as_ref()
        .unwrap()
        .entries
    {
        assert_eq!(
            std::fs::read(repo.root().join(path)).unwrap(),
            expected.bytes
        );
    }
    let status = repo.status(wc, crate::StatusOptions::default()).unwrap();
    assert!(
        status.is_clean(),
        "materialized sample is not clean: {status:?}"
    );
}

fn benchmark_replay(sample: &Sample, sizes: &[usize]) {
    let count = sample.commits.len();
    let mut timings: BTreeMap<usize, Vec<f64>> = BTreeMap::new();
    // Every size runs three times, with rotating order; do not run this test
    // concurrently with other benchmarks. No timing/speedup assertion in CI.
    for round in 0..3 {
        for offset in 0..sizes.len() {
            let size = sizes[(round + offset) % sizes.len()];
            let (dir, repo) = target();
            let wc = repo.require_working_copy_id().unwrap();
            let materialized = repo.working_copy_record(wc).unwrap().materialized_state;
            let mut closure = Vec::new();
            let started = Instant::now();
            let mut operations = Vec::new();
            for entries in sample.commits.chunks(size) {
                operations.push(publish(&repo, entries, &closure, None).unwrap());
                closure.extend(entries.iter().map(|entry| entry.hash));
            }
            let elapsed = started.elapsed();
            for id in operations.iter().copied() {
                assert_eq!(
                    repo.operation_details(id).unwrap().verification,
                    super::super::OperationVerificationState::Verified
                );
            }
            assert_prefix(&repo, &sample.commits);
            assert_eq!(
                repo.working_copy_record(wc).unwrap().materialized_state,
                materialized
            );
            drop(repo);
            let reopened = Repository::open(dir.path()).unwrap();
            assert_prefix(&reopened, &sample.commits);
            materialize_and_assert(&reopened, &sample.commits);
            let ms = elapsed.as_secs_f64() * 1000.0;
            timings.entry(size).or_default().push(ms);
            println!("SAMPLE replay workload={} commits={count} round={round} batch_size={size} graph_transactions={} synthesis_operations={} elapsed_ms={ms:.3} full_per_commit_verification=true", sample.workload, operations.len(), operations.len());
        }
    }
    for (size, mut values) in timings {
        values.sort_by(f64::total_cmp);
        println!("SAMPLE summary workload={} commits={count} batch_size={size} median_ms={:.3} min_ms={:.3} max_ms={:.3} commits_per_second={:.2}", sample.workload, values[1], values[0], values[2], count as f64 * 1000.0 / values[1]);
    }
}

#[test]
fn mixed_sample_batch_failure_refuses_stale_payload_then_retries_after_reopen() {
    let sample = mixed_sample(32);
    let (dir, repo) = target();
    let wc = repo.require_working_copy_id().unwrap();
    let materialized = repo.working_copy_record(wc).unwrap().materialized_state;
    publish(&repo, &sample.commits[..4], &[], None).unwrap();
    let closure: Vec<_> = sample.commits[..4].iter().map(|entry| entry.hash).collect();
    // This failed transaction includes chmod, rename/edit, directory retirement
    // and recreation, binary replacement, truncation and chmod-back.
    let injected = publish(&repo, &sample.commits[4..12], &closure, Some(7)).unwrap_err();
    assert!(injected
        .to_string()
        .contains("staged synthesis verification failed"));
    assert_prefix(&repo, &sample.commits[..4]);
    for entry in &sample.commits[4..12] {
        assert!(!repo
            .pristine
            .read_txn()
            .unwrap()
            .has_git_sha(&entry.sha)
            .unwrap());
    }
    // Unlike fresh additions, these precompiled edits carry prior semantic
    // NodeIds from their source repository. Aborted registrations advance the
    // live allocator, so reusing that payload on this handle must fail closed.
    // A production streaming compiler must discard/reassemble its staged
    // payload against current identities, not blindly retry this test fixture.
    let stale = publish(&repo, &sample.commits[4..], &closure, None).unwrap_err();
    assert!(stale.to_string().contains("semantic closure"), "{stale}");
    assert_prefix(&repo, &sample.commits[..4]);
    for entry in &sample.commits[4..] {
        assert!(!repo
            .pristine
            .read_txn()
            .unwrap()
            .has_git_sha(&entry.sha)
            .unwrap());
    }
    assert_eq!(
        repo.working_copy_record(wc).unwrap().materialized_state,
        materialized
    );
    drop(repo);
    // In this disposable sample, reopening reconstructs allocation from the
    // durable four-commit prefix. Check the resulting source/target mapping;
    // this is not a general solution for arbitrary repository-local ID maps.
    let reopened = Repository::open(dir.path()).unwrap();
    publish(&reopened, &sample.commits[4..], &closure, None).unwrap();
    assert_prefix(&reopened, &sample.commits);
    for entry in &sample.commits {
        assert_eq!(
            reopened
                .pristine
                .read_txn()
                .unwrap()
                .get_internal(&entry.hash)
                .unwrap(),
            Some(entry.source_node_id)
        );
    }
    assert_eq!(
        reopened.working_copy_record(wc).unwrap().materialized_state,
        materialized
    );
    materialize_and_assert(&reopened, &sample.commits);
}

#[test]
#[ignore = "manual mixed-history doubling experiment; use --ignored --test-threads=1 --nocapture"]
fn compare_mixed_sixteen_and_thirty_two_git_commits() {
    let smaller = mixed_sample(16);
    let larger = mixed_sample(32);
    // Establish that this really doubles the same workload/history, rather
    // than comparing unrelated 16- and 32-commit fixtures.
    assert_eq!(
        smaller
            .commits
            .iter()
            .map(|entry| entry.hash)
            .collect::<Vec<_>>(),
        larger.commits[..16]
            .iter()
            .map(|entry| entry.hash)
            .collect::<Vec<_>>()
    );
    benchmark_replay(&smaller, &[1, 4, 16]);
    benchmark_replay(&larger, &[1, 4, 16, 32]);
}

#[derive(serde::Deserialize)]
struct RealEntry {
    path: String,
    bytes: Vec<u8>,
    mode: u16,
    kind: InodeKind,
}

#[derive(serde::Deserialize)]
struct RealCommit {
    sha: String,
    hash: String,
    entries: Vec<RealEntry>,
    semantic_paths: Vec<String>,
    deleted_paths: Vec<String>,
}

/// Load only a freshly completed clean-shim sample, never the old workspace's
/// imported Atomic state. The JSONL spool carries the existing shim's exact
/// prospective expectations; it is not an independent graph compiler.
fn load_real_sample(
    artifact: &std::path::Path,
    verification: SampleVerification,
) -> (Vec<Prepared>, serde_json::Value) {
    let summary: serde_json::Value =
        serde_json::from_slice(&std::fs::read(artifact.join("preparation.json")).unwrap()).unwrap();
    assert_eq!(
        summary["status"], "complete",
        "refuse an incomplete source sample: {summary}"
    );
    let requested = summary["requested"].as_u64().unwrap() as usize;
    assert_eq!(summary["completed"].as_u64(), Some(requested as u64));
    if summary["full_per_commit_verification"] == false {
        assert!(
            matches!(
                verification,
                SampleVerification::FinalTip | SampleVerification::PrivateStage
            ),
            "deferred private preparation cannot be described as historically verified"
        );
    }
    let source = Repository::open(artifact.join("source")).unwrap();
    let file = std::fs::File::open(artifact.join("commits.jsonl")).unwrap();
    let mut commits = Vec::new();
    let mut retained_bytes = 0usize;
    for line in std::io::BufReader::new(file).lines() {
        let record: RealCommit = serde_json::from_str(&line.unwrap()).unwrap();
        let hash = Hash::from_base32(record.hash.as_bytes()).unwrap();
        let change = source.load_change(&hash).unwrap();
        let bytes = std::fs::read(source.change_store.change_path(&hash)).unwrap();
        retained_bytes = retained_bytes.checked_add(bytes.len()).unwrap();
        let mut entries = BTreeMap::new();
        for entry in record.entries {
            assert_eq!(entry.kind, InodeKind::Regular, "real replay's final filesystem audit currently supports regular files only; do not skip this entry");
            retained_bytes = retained_bytes
                .checked_add(entry.bytes.len() + entry.path.len())
                .unwrap();
            assert!(entries
                .insert(
                    entry.path,
                    StagedPathExpectation {
                        bytes: entry.bytes,
                        mode: entry.mode,
                        kind: entry.kind
                    }
                )
                .is_none());
        }
        assert!(retained_bytes <= 256 * 1024 * 1024, "prepared sample payload exceeds 256 MiB; this is not a bounded-memory streaming importer");
        let source_node_id = source
            .pristine
            .read_txn()
            .unwrap()
            .get_internal(&hash)
            .unwrap()
            .unwrap();
        assert_eq!(source_node_id.get(), commits.len() as u64 + 1,
            "cross-repository semantic replay requires the sample's sequential source IDs; production must assemble in its actual target");
        commits.push(Prepared {
            hash,
            change,
            bytes,
            sha: record.sha,
            source_node_id,
            deleted_paths: record.deleted_paths,
            expectation: StagedExpectation {
                paths: Vec::new(),
                tree: Some(StagedTreeExpectation {
                    entries,
                    semantic_paths: record.semantic_paths,
                    verify_paths: None,
                    semantic_verify_paths: None,
                }),
            },
        });
    }
    assert_eq!(commits.len(), requested);
    assert_no_checkout(&source);
    println!(
        "HYPERFINE loaded commits={} retained_payload_bytes={retained_bytes}",
        commits.len()
    );
    (commits, summary)
}

#[test]
#[ignore = "manual real-history replay; requires ATOMIC_REAL_SAMPLE_ARTIFACT from the clean CLI sample"]
fn replay_hyperfine_in_hundred_commit_batches() {
    let artifact = std::path::PathBuf::from(
        std::env::var_os("ATOMIC_REAL_SAMPLE_ARTIFACT")
            .expect("set prepared clean-shim sample directory"),
    );
    let batch: usize = std::env::var("ATOMIC_REAL_SAMPLE_BATCH")
        .unwrap_or_else(|_| "100".into())
        .parse()
        .unwrap();
    let trials: usize = std::env::var("ATOMIC_REAL_SAMPLE_TRIALS")
        .unwrap_or_else(|_| "1".into())
        .parse()
        .unwrap();
    let budget = std::time::Duration::from_secs(
        std::env::var("ATOMIC_REAL_SAMPLE_REPLAY_BUDGET_SECONDS")
            .unwrap_or_else(|_| "120".into())
            .parse()
            .unwrap(),
    );
    assert!((1..=256).contains(&batch) && (1..=3).contains(&trials));
    let verification = match std::env::var("ATOMIC_REAL_SAMPLE_VERIFICATION").as_deref() {
        Ok("final-tip") => SampleVerification::FinalTip,
        Ok("private-stage") => SampleVerification::PrivateStage,
        Ok("each-commit") | Err(_) => SampleVerification::EachCommit,
        Ok(other) => panic!("unknown sample verification scope: {other}"),
    };
    let load_started = Instant::now();
    let (commits, preparation) = load_real_sample(&artifact, verification);
    if verification == SampleVerification::FinalTip {
        assert!(commits.len() <= batch, "final-tip experiment must fit in one transaction; multi-batch staging/promotion is not implemented");
    }
    let load_ms = load_started.elapsed().as_secs_f64() * 1000.0;
    let stem = match verification {
        SampleVerification::EachCommit => format!("replay-batch-{batch}"),
        SampleVerification::FinalTip => format!("replay-final-tip-batch-{batch}"),
        SampleVerification::PrivateStage => format!("replay-private-stage-batch-{batch}"),
    };
    let mut reports = Vec::new();
    for trial in 0..trials {
        let target_path = artifact.join(format!("{stem}-trial-{trial}"));
        assert!(
            !target_path.exists(),
            "preserve prior replay evidence; use a new batch/trial or artifact directory"
        );
        let setup = Instant::now();
        let mut repo = Repository::init(&target_path).unwrap();
        repo.pristine
            .require_repository_capability(super::super::CHANGE_FORMAT_VNEXT_CAPABILITY)
            .unwrap();
        if verification == SampleVerification::PrivateStage {
            create_private_stage(&mut repo);
            assert_dev_unpublished(&repo);
        }
        let wc = repo.require_working_copy_id().unwrap();
        let materialized = repo.working_copy_record(wc).unwrap().materialized_state;
        let setup_ms = setup.elapsed().as_secs_f64() * 1000.0;
        let mut closure = Vec::new();
        let mut operations = Vec::new();
        let started = Instant::now();
        let mut result: Result<(), RepositoryError> = (|| {
            for entries in commits.chunks(batch) {
                if started.elapsed() > budget {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "replay budget exhausted at {}/{} commits",
                            closure.len(),
                            commits.len()
                        ),
                    });
                }
                operations.push(publish_to_view(
                    &repo,
                    if verification == SampleVerification::PrivateStage {
                        PRIVATE_IMPORT_VIEW
                    } else {
                        repo.current_view()
                    },
                    entries,
                    &closure,
                    None,
                    verification,
                )?);
                closure.extend(entries.iter().map(|entry| entry.hash));
                if verification == SampleVerification::PrivateStage {
                    assert_dev_unpublished(&repo);
                }
                println!(
                    "HYPERFINE replay commits={}/{} batch_size={batch} elapsed_ms={:.3}",
                    closure.len(),
                    commits.len(),
                    started.elapsed().as_secs_f64() * 1000.0
                );
            }
            Ok(())
        })();
        let replay_ms = started.elapsed().as_secs_f64() * 1000.0;
        assert_no_checkout(&repo);
        assert_eq!(
            repo.working_copy_record(wc).unwrap().materialized_state,
            materialized
        );
        let mut report = serde_json::json!({
            "trial": trial, "batch_size": batch, "requested": commits.len(),
            "published": if verification == SampleVerification::PrivateStage { 0 } else { closure.len() },
            "staged_commits": if verification == SampleVerification::PrivateStage { closure.len() } else { 0 },
            "graph_transactions": operations.len(), "synthesis_operations": operations.len(),
            "setup_ms": setup_ms, "replay_ms": replay_ms, "load_ms": load_ms,
            "source_preparation_ms": preparation["source_preparation_ms"],
            "source_content_proof": preparation["source_content_proof"],
            "build_label": std::env::var("ATOMIC_REAL_SAMPLE_BUILD_LABEL").unwrap_or_else(|_| "unspecified test build".into()),
            "verification_scope": verification.name(),
            "full_per_commit_verification": verification == SampleVerification::EachCommit,
            "historical_states_verified": verification == SampleVerification::EachCommit,
            "materialized": false,
            "status": if result.is_ok() { "published" } else { "incomplete" },
            "error": result.as_ref().err().map(ToString::to_string),
        });
        if result.is_ok() && verification == SampleVerification::PrivateStage {
            match promote_private_stage(&repo, &commits, false) {
                Ok((operation, promotion_ms)) => {
                    operations.push(operation);
                    report["final_promotion_ms"] = serde_json::json!(promotion_ms);
                    report["publication_including_promotion_ms"] =
                        serde_json::json!(replay_ms + promotion_ms);
                    report["published"] = serde_json::json!(commits.len());
                    report["synthesis_operations"] = serde_json::json!(operations.len());
                    report["promotion_metadata_transactions"] = serde_json::json!(1);
                }
                Err(error) => {
                    assert_dev_unpublished(&repo);
                    report["status"] = serde_json::json!("final-proof-rejected");
                    report["error"] = serde_json::json!(error.to_string());
                    result = Err(error);
                }
            }
        }
        if result.is_ok() {
            let audit = Instant::now();
            assert_prefix(&repo, &commits);
            for id in operations {
                assert_eq!(
                    repo.operation_details(id).unwrap().verification,
                    super::super::OperationVerificationState::Verified
                );
            }
            drop(repo);
            let reopened = Repository::open(&target_path).unwrap();
            assert_prefix(&reopened, &commits);
            report["audit_reopen_ms"] = serde_json::json!(audit.elapsed().as_secs_f64() * 1000.0);
            let checkout = Instant::now();
            materialize_and_assert(&reopened, &commits);
            report["final_checkout_and_status_ms"] =
                serde_json::json!(checkout.elapsed().as_secs_f64() * 1000.0);
            report["materialized"] = serde_json::json!(true);
            report["status"] = serde_json::json!("complete");
        }
        println!("HYPERFINE replay result {report}");
        reports.push(report);
        let report_path = artifact.join(format!("{stem}.json"));
        std::fs::write(report_path, serde_json::to_vec_pretty(&reports).unwrap()).unwrap();
        result.unwrap();
    }
}
