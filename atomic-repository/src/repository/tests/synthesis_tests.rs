//! CB-9A synthesis tests: normal-assembly foreign Git synthesis with hashed
//! origin, deterministic journaling, resurrection, and failure containment.
//!
//! The repository-level suites exercise `Repository::synthesize_git_change`
//! directly (the same engine the CLI importer drives inside the shared
//! workspace boundary). Git-backed fixtures build real commits with the `git`
//! CLI so the tagged OIDs under test are genuine.

use std::fs;
use std::path::Path;
use std::process::Command;

use atomic_core::change::{
    CausalFrontier, ChangeKind, ChangeOrigin, GitDerivation,
};
use atomic_core::operation::{GitHashAlgorithm, GitObjectId, OperationKind, OperationScope};
use atomic_core::record::workflow::record::{
    record_added_file, record_deleted_file, record_modified_file,
};
use atomic_core::record::workflow::{DetectedFile, RecordedFile, RecordingOptions};
use atomic_core::types::Base32;
use chrono::TimeZone;
use tempfile::TempDir;

use super::super::synthesis::StagedExpectation;
use super::*;
use crate::git_binding::{BindingSigner, CausalOrigin, GitObjectFormat, GitOid, GitStateBinding};
use crate::{InsertOptions, WorkspaceTxnStart, WorkspaceTxnMode};

// ── Fixtures ────────────────────────────────────────────────────────────

/// Deterministic tagged commit OID (test code only).
fn oid20(seed: u8) -> GitObjectId {
    GitObjectId::new(GitHashAlgorithm::Sha1, vec![seed; 20]).unwrap()
}

/// Lowercase hex of a tagged Git object ID (test helper).
fn oid_hex(oid: &GitObjectId) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::new();
    for byte in oid.as_bytes() {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// A prospective-equivalence verification over the empty project tree.
fn empty_verification() -> crate::VerifiedProspectiveEquivalence {
    let policy = crate::ConversionPolicy::new(GitHashAlgorithm::Sha1);
    let manifest = crate::RepositoryManifest::new(
        atomic_core::types::SetId::ZERO,
        policy.root().content_key,
        Vec::new(),
    )
    .unwrap();
    let project = crate::ProjectTree::from_manifest(manifest, &policy).unwrap();
    let expected =
        GitObjectId::new(GitHashAlgorithm::Sha1, project.git.root.as_bytes().to_vec()).unwrap();
    crate::verify_prospective_equivalence(&project, &expected).unwrap()
}

fn oid32(seed: u8) -> GitObjectId {
    GitObjectId::new(GitHashAlgorithm::Sha256, vec![seed; 32]).unwrap()
}

fn memory_add(path: &str, content: &[u8]) -> RecordedFile {
    let working_copy = atomic_core::output::memory::Memory::new();
    working_copy.add_file(path, content);
    let detected = DetectedFile::added(path);
    record_added_file(&working_copy, &detected, &RecordingOptions::new())
        .expect("record added file")
}

fn memory_modify(repo: &Repository, path: &str, old: &[u8], new: &[u8]) -> RecordedFile {
    use atomic_core::crdt::queries::iter_trunk_branches_in_file_order;
    use atomic_core::crdt::tables::encode_branch_id;
    use atomic_core::pristine::CrdtTxnT;

    let working_copy = atomic_core::output::memory::Memory::new();
    working_copy.add_file(path, new);
    let mut detected = DetectedFile::modified(path);
    if let Ok(Some((inode, position))) = repo.get_inode_and_position(path) {
        detected.inode = Some(inode);
        detected.position = Some(position);
    }
    // Bind the edit to the file's existing CRDT trunk and alive branches (the
    // real record workflow does the same through
    // `Repository::record_resolution_modified_file`): without the existing
    // identities the modify would allocate placeholder branches in a parallel
    // trunk and the semantic layer could not stay in sync across commits.
    let (existing_trunk, existing_branches) = {
        let txn = repo.pristine.read_txn().expect("read pristine");
        let trunk_id = txn.get_trunk_by_path(path).ok().flatten();
        let branches = trunk_id
            .as_ref()
            .map(|trunk| {
                iter_trunk_branches_in_file_order(&txn, *trunk)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|branch| {
                        txn.get_crdt_branch(&encode_branch_id(branch))
                            .ok()
                            .flatten()
                            .is_some_and(|data| data.state.is_alive())
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        (trunk_id, branches)
    };
    record_modified_file(
        &working_copy,
        &detected,
        old,
        None,
        &RecordingOptions::new(),
        existing_trunk,
        if existing_branches.is_empty() {
            None
        } else {
            Some(existing_branches.as_slice())
        },
    )
    .expect("record modified file")
}

fn memory_delete(repo: &Repository, path: &str) -> RecordedFile {
    let mut detected = DetectedFile::deleted(path);
    let (inode, position) = repo
        .get_inode_and_position(path)
        .ok()
        .flatten()
        .expect("tracked path has an inode binding");
    detected.inode = Some(inode);
    detected.position = Some(position);
    record_deleted_file(&detected, &RecordingOptions::new()).expect("record deleted file")
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "Atomic Tests")
        .env("GIT_AUTHOR_EMAIL", "tests@atomic.dev")
        .env("GIT_COMMITTER_NAME", "Atomic Tests")
        .env("GIT_COMMITTER_EMAIL", "tests@atomic.dev")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

/// A complete staged tree expectation (review F3): every entry of the
/// commit's verified Git tree — inherited paths included — with exact
/// repository bytes/mode/kind, plus the delta paths that must carry
/// regenerated FileOps. The touched-path contract stays empty: the
/// full-tree check subsumes it.
fn full_tree_expectation(
    entries: &[(&str, &[u8])],
    semantic_paths: &[&str],
) -> StagedExpectation {
    let mut tree_entries = std::collections::BTreeMap::new();
    for (path, bytes) in entries {
        tree_entries.insert(
            (*path).to_string(),
            super::super::synthesis::StagedPathExpectation {
                bytes: bytes.to_vec(),
                mode: 0o644,
                kind: atomic_core::change::InodeKind::Regular,
            },
        );
    }
    StagedExpectation {
        paths: Vec::new(),
        tree: Some(super::super::synthesis::StagedTreeExpectation {
            entries: tree_entries,
            semantic_paths: semantic_paths.iter().map(|p| p.to_string()).collect(),
        }),
    }
}

/// Convenience wrapper for one-file trees.
fn single_file_expectation(path: &str, bytes: &[u8]) -> StagedExpectation {
    full_tree_expectation(&[(path, bytes)], &[path])
}

/// A colocated Atomic + Git repository on `dev` with one tracked commit.
fn initialized_colocated_repository() -> (TempDir, Repository, String, String) {
    let directory = TempDir::new().unwrap();
    let repo = Repository::init(directory.path()).unwrap();
    git(directory.path(), &["init", "-b", "dev"]);
    fs::write(directory.path().join("tracked.txt"), b"tracked\n").unwrap();
    git(directory.path(), &["add", "tracked.txt"]);
    git(directory.path(), &["commit", "-m", "initial"]);
    let head = git(directory.path(), &["rev-parse", "HEAD"]);
    let tree = git(directory.path(), &["rev-parse", "HEAD^{tree}"]);
    (directory, repo, head, tree)
}

fn write_checkpoint(root: &Path, repo: &Repository, head: &str, tree: &str) {
    let path = root.join(".atomic/bridge/workspace.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let state = repo
        .working_copy_record(repo.require_working_copy_id().unwrap())
        .unwrap()
        .desired_state;
    let symref = git(root, &["symbolic-ref", "HEAD"]);
    fs::write(
        path,
        format!(
            "{{\"version\":2,\"view\":\"dev\",\"atomic_state\":\"{state}\",\"git_head_symref\":\"{symref}\",\"git_head\":\"{head}\",\"git_tree\":\"{tree}\",\"git_index_tree\":\"{tree}\"}}"
        ),
    )
    .unwrap();
}

/// A pinned header so repeated synthesis of the same commit hashes the same.
fn fixed_header(message: &str) -> ChangeHeader {
    ChangeHeader::builder()
        .message(message)
        .author(atomic_core::change::Author::new("Bridge Tests", Some("bridge@tests.dev")))
        .timestamp(chrono::Utc.timestamp_opt(1_700_000_000, 0).single().unwrap())
        .build()
}

// ── AC-1: normal assembly, hashed origin, graph-context dependencies ───

#[test]
fn synthesized_multi_file_root_change_reconstructs_every_path() {
    let directory = TempDir::new().unwrap();
    let repo = Repository::init(directory.path()).unwrap();

    // A commit adding several files in one change: each file's semantic ops
    // must carry a per-change unique placeholder namespace, so the staged
    // full-tree check reconstructs every path through the CRDT tables
    // (review F3/F4 — placeholder ids that collide would clobber the
    // TRUNKS/BRANCHES rows of earlier files in the same change).
    let multi = [
        ("alpha.rs", b"pub fn alpha() {}\n".to_vec()),
        ("beta.rs", b"pub fn beta() {}\n".to_vec()),
    ];
    let recorded: Vec<RecordedFile> = multi
        .iter()
        .map(|(path, bytes)| memory_add(path, bytes))
        .collect();
    let origin = GitSynthesisOrigin::root(oid20(0x31), oid20(0x33)).expect("root origin");
    let unhashed = crate::git_synthesis_metadata(&origin, &[], None);
    let expectation = full_tree_expectation(
        &[
            ("alpha.rs", &b"pub fn alpha() {}\n"[..]),
            ("beta.rs", &b"pub fn beta() {}\n"[..]),
        ],
        &["alpha.rs", "beta.rs"],
    );
    repo.synthesize_git_change(
        fixed_header("import multi"),
        &recorded,
        origin,
        unhashed,
        Vec::new(),
        &[],
        false,
        &empty_verification(),
        InsertOptions::default(),
        &[],
        &expectation,
        None,
        &[],
   false,
    )
    .expect("synthesize multi-file root change");

    drop(repo);
    let reopened = Repository::open(directory.path()).unwrap();
    assert_eq!(
        reopened.get_file_content("alpha.rs").unwrap(),
        Some(b"pub fn alpha() {}\n".to_vec())
    );
    assert_eq!(
        reopened.get_file_content("beta.rs").unwrap(),
        Some(b"pub fn beta() {}\n".to_vec())
    );
}

#[test]
fn synthesized_root_change_carries_hashed_origin_tagged_oids_and_bridge_sequencing() {
    let directory = TempDir::new().unwrap();
    let repo = Repository::init(directory.path()).unwrap();

    let recorded = memory_add("src/domain/model.rs", b"pub struct Model;\n");
    let origin =
        GitSynthesisOrigin::root(oid20(0x11), oid20(0x22)).expect("root origin");
    let unhashed = crate::git_synthesis_metadata(
        &origin,
        &["shallow"],
        Some(serde_json::json!({
            "git": { "repository": "fixture", "sha": "a".repeat(40), "short_sha": "aaaaaaaa" }
        })),
    );
    let outcome = repo
        .synthesize_git_change(
            fixed_header("import root"),
            &[recorded],
            origin.clone(),
            unhashed,
            Vec::new(),
            &[],
            false,
            &empty_verification(),
            InsertOptions::default(),
            &[],
            &single_file_expectation("src/domain/model.rs", b"pub struct Model;\n"),
            None,
            &[],
       false,
        )
        .expect("synthesize root change");

    // Reopen and inspect the serialized origin.
    drop(repo);
    let reopened = Repository::open(directory.path()).unwrap();
    let change = reopened.load_change(&outcome.write.hash).unwrap();

    match change.origin() {
        ChangeOrigin::GitSynthesized {
            commit,
            parents,
            derivation,
        } => {
            assert_eq!(commit, &oid20(0x11));
            assert!(parents.is_empty(), "root origin carries no parents");
            assert_eq!(*derivation, GitDerivation::Root);
        }
        other => panic!("expected GitSynthesized origin, found {other:?}"),
    }
    assert_eq!(*change.kind(), ChangeKind::Durable);
    assert!(change.causal_frontier().is_empty());
    // Dependencies come only from graph context; a root synthesis has none.
    assert!(change.dependencies().is_empty());
    // Git parent OIDs never leak into Atomic dependencies.
    assert!(!change
        .dependencies()
        .iter()
        .any(|dep| dep.as_bytes() == oid20(0x11).as_bytes()));

    // Tagged commit/tree OIDs, derivation, boundaries, and the bridge
    // sequencing label travel in unhashed provenance.
    let git_meta = change
        .unhashed
        .as_ref()
        .and_then(|value| value.get("git"))
        .expect("synthesized changes carry git provenance")
        .as_object()
        .expect("git provenance object")
        .clone();
    assert_eq!(
        git_meta.get("origin").and_then(|v| v.as_str()),
        Some("git_synthesized")
    );
    assert_eq!(
        git_meta.get("commit").and_then(|v| v.as_str()),
        Some(oid_hex(&oid20(0x11)).as_str())
    );
    assert_eq!(
        git_meta.get("commit_algorithm").and_then(|v| v.as_str()),
        Some("sha1")
    );
    assert_eq!(
        git_meta.get("tree").and_then(|v| v.as_str()),
        Some(oid_hex(&oid20(0x22)).as_str())
    );
    assert_eq!(
        git_meta.get("tree_algorithm").and_then(|v| v.as_str()),
        Some("sha1")
    );
    assert_eq!(
        git_meta.get("parents").and_then(|v| v.as_array().map(Vec::clone)),
        Some(vec![])
    );
    assert_eq!(
        git_meta.get("derivation").and_then(|v| v.as_str()),
        Some("root")
    );
    assert_eq!(
        git_meta.get("sequencing").and_then(|v| v.as_str()),
        Some("bridge")
    );
    assert_eq!(
        git_meta.get("boundaries").and_then(|v| v.as_array().map(Vec::clone)),
        Some(vec![serde_json::json!("shallow")])
    );
    // Legacy reader fields survive.
    assert_eq!(
        git_meta.get("repository").and_then(|v| v.as_str()),
        Some("fixture")
    );

    // The semantic layer materializes from the synthesized change.
    assert!(
        change.has_file_ops(),
        "synthesized changes carry semantic FileOps"
    );
    assert_eq!(
        reopened.get_file_content("src/domain/model.rs").unwrap(),
        Some(b"pub struct Model;\n".to_vec())
    );
}

#[test]
fn synthesized_first_parent_dependencies_come_from_graph_context_after_reopen() {
    let directory = TempDir::new().unwrap();
    let repo = Repository::init(directory.path()).unwrap();

    // Root commit adds a nested file.
    let add = memory_add("src/domain/model.rs", b"pub struct Model;\n");
    let root_origin = GitSynthesisOrigin::root(oid20(0x31), oid20(0x32)).unwrap();
    let root_unhashed = crate::git_synthesis_metadata(&root_origin, &[], None);
    let root_outcome = repo
        .synthesize_git_change(
            fixed_header("import root"),
            &[add],
            root_origin,
            root_unhashed,
            Vec::new(),
            &[],
            false,
            &empty_verification(),
            InsertOptions::default(),
            &[],
            &single_file_expectation("src/domain/model.rs", b"pub struct Model;\n"),
            None,
            &[],
       false,
        )
        .unwrap();

    // Child commit (git first parent = the root commit) modifies the file.
    // The diff baseline is the repository's own bytes.
    let old = repo
        .get_file_content("src/domain/model.rs")
        .unwrap()
        .expect("repository bytes for the tracked file");
    assert_eq!(old, b"pub struct Model;\n");
    let modify = memory_modify(&repo, "src/domain/model.rs", &old, b"pub struct Model;\npub enum Kind;\n");
    let child_origin = GitSynthesisOrigin::first_parent(
        oid20(0x41),
        oid20(0x42),
        oid20(0x31),
    )
    .unwrap();
    let child_unhashed = crate::git_synthesis_metadata(&child_origin, &[], None);
    let child_outcome = repo
        .synthesize_git_change(
            fixed_header("import child"),
            &[modify],
            child_origin.clone(),
            child_unhashed,
            Vec::new(),
            &[],
            false,
            &empty_verification(),
            InsertOptions::default(),
            &[],
            &single_file_expectation(
                "src/domain/model.rs",
                b"pub struct Model;\npub enum Kind;\n",
            ),
            None,
            &[],
            false,
        )
        .unwrap();

    drop(repo);
    let reopened = Repository::open(directory.path()).unwrap();
    let child = reopened.load_change(&child_outcome.write.hash).unwrap();

    // The complete ordered Git parent list is preserved in the origin...
    match child.origin() {
        ChangeOrigin::GitSynthesized {
            commit,
            parents,
            derivation,
        } => {
            assert_eq!(commit, &oid20(0x41));
            assert_eq!(parents, &[oid20(0x31)]);
            assert_eq!(*derivation, GitDerivation::FirstParent);
        }
        other => panic!("expected GitSynthesized origin, found {other:?}"),
    }
    // ...but Atomic dependencies come only from graph context: the
    // assembled change depends on the root synthesis it was built on.
    assert_eq!(child.dependencies(), &[root_outcome.write.hash]);
    // The Git parent OID is ancestry evidence, never a dependency.
    assert!(!child
        .dependencies()
        .iter()
        .any(|dep| dep.as_bytes() == oid20(0x31).as_bytes()));

    // Nested materialization and content after reopen.
    assert_eq!(
        reopened.get_file_content("src/domain/model.rs").unwrap(),
        Some(b"pub struct Model;\npub enum Kind;\n".to_vec())
    );
    let working_copy = reopened.require_working_copy_id().unwrap();
    reopened.materialize(working_copy).unwrap();
    assert_eq!(
        fs::read(directory.path().join("src/domain/model.rs")).unwrap(),
        b"pub struct Model;\npub enum Kind;\n"
    );
}

#[test]
fn deletion_synthesis_uses_repository_state_and_canonical_filedel() {
    let directory = TempDir::new().unwrap();
    let repo = Repository::init(directory.path()).unwrap();

    let add = memory_add("plain.txt", b"payload\n");
    let root_origin = GitSynthesisOrigin::root(oid20(0x51), oid20(0x52)).unwrap();
    let root_unhashed = crate::git_synthesis_metadata(&root_origin, &[], None);
    repo.synthesize_git_change(
        fixed_header("import add"),
        &[add],
        root_origin,
        root_unhashed,
        Vec::new(),
        &[],
        false,
        &empty_verification(),
        InsertOptions::default(),
        &[],
        &single_file_expectation("plain.txt", b"payload\n"),
        None,
        &[],
   false,
    )
    .unwrap();

    let delete = memory_delete(&repo, "plain.txt");
    let origin = GitSynthesisOrigin::first_parent(oid20(0x61), oid20(0x62), oid20(0x51)).unwrap();
    let unhashed = crate::git_synthesis_metadata(&origin, &[], None);
    let outcome = repo
        .synthesize_git_change(
            fixed_header("import delete"),
            &[delete],
            origin.clone(),
            unhashed,
            Vec::new(),
            &["plain.txt".to_string()],
            false,
            &empty_verification(),
            InsertOptions::default(),
            &[],
            // The deletion leaves the expected tree empty: every path is
            // gone, and the FileDel delta path must still carry semantic
            // FileOps (review F3/F4: empty trees check no leaked semantics).
            &StagedExpectation {
                paths: Vec::new(),
                tree: Some(super::super::synthesis::StagedTreeExpectation {
                    entries: std::collections::BTreeMap::new(),
                    semantic_paths: vec!["plain.txt".to_string()],
                }),
            },
            None,
            &[],
            false,
        )
        .unwrap();
    assert_eq!(
        repo.get_file_content("plain.txt").unwrap(),
        None,
        "the deletion removed the file from the view"
    );
    drop(repo);
    let reopened = Repository::open(directory.path()).unwrap();
    let change = reopened.load_change(&outcome.write.hash).unwrap();
    assert!(
        change
            .hunks()
            .iter()
            .any(|op| matches!(op, atomic_core::change::GraphOp::FileDel { path, .. } if path == "plain.txt")),
        "the deletion is encoded as a canonical FileDel"
    );
}

// ── AC-1: legacy bytes stay authoritative; re-import is idempotent ─────

#[test]
fn legacy_changes_are_never_rewritten_and_repeated_synthesis_is_idempotent() {
    let directory = TempDir::new().unwrap();
    let mut repo = Repository::init(directory.path()).unwrap();

    // A legacy imported change (no Git origin) written through the
    // pre-CB-9A writer.
    let legacy = memory_add("legacy.txt", b"legacy\n");
    let legacy_outcome = repo
        .write_import_recorded(
            fixed_header("legacy import"),
            &[legacy],
            serde_json::json!({ "git": { "sha": "b".repeat(40) } }),
            &[],
            false,
            &empty_verification(),
            Default::default(),
        )
        .unwrap();
    let hash_b32 = legacy_outcome.hash.to_base32();
    let legacy_path = directory
        .path()
        .join(".atomic/changes")
        .join(&hash_b32[..2])
        .join(format!("{hash_b32}.change"));
    let legacy_bytes = fs::read(&legacy_path).expect("legacy change file on disk");

    // A synthesized change built on top of the legacy one.
    let synthesized = memory_add("synthesized.txt", b"synthesized\n");
    let origin = GitSynthesisOrigin::root(oid20(0x71), oid20(0x72)).unwrap();
    let unhashed = crate::git_synthesis_metadata(&origin, &[], None);
    let first = repo
        .synthesize_git_change(
            fixed_header("synthesized import"),
            &[synthesized],
            origin,
            unhashed,
            Vec::new(),
            &[],
            false,
            &empty_verification(),
            InsertOptions::default(),
            &[],
            &full_tree_expectation(
                &[
                    ("legacy.txt", b"legacy\n" as &[u8]),
                    ("synthesized.txt", b"synthesized\n"),
                ],
                &["synthesized.txt"],
            ),
            None,
            &[],
            false,
        )
        .unwrap();
    assert_ne!(first.write.hash, legacy_outcome.hash);

    // Legacy bytes and hash are untouched: the stored origin stays Native.
    drop(repo);
    let reopened = Repository::open(directory.path()).unwrap();
    let legacy_change = reopened.load_change(&legacy_outcome.hash).unwrap();
    assert_eq!(*legacy_change.origin(), ChangeOrigin::Native);
    assert_eq!(
        fs::read(&legacy_path).unwrap(),
        legacy_bytes,
        "legacy imported bytes must remain authoritative without rewriting"
    );

    // Repeated synthesis of the same commit is a no-op, not a rewrite.
    let working_copy = reopened.require_working_copy_id().unwrap();
    let state_before = reopened.working_copy_record(working_copy).unwrap().desired_state;
    let repeat = memory_add("synthesized.txt", b"synthesized\n");
    let origin = GitSynthesisOrigin::root(oid20(0x71), oid20(0x72)).unwrap();
    let unhashed = crate::git_synthesis_metadata(&origin, &[], None);
    let again = reopened
        .synthesize_git_change(
            fixed_header("synthesized import"),
            &[repeat],
            origin,
            unhashed,
            Vec::new(),
            &[],
            false,
            &empty_verification(),
            InsertOptions::default(),
            &[],
            &full_tree_expectation(
                &[
                    ("legacy.txt", b"legacy\n" as &[u8]),
                    ("synthesized.txt", b"synthesized\n"),
                ],
                &["synthesized.txt"],
            ),
            None,
            &[],
            false,
        )
        .unwrap();
    assert!(again.already_in_view);
    assert!(again.operation.is_none());
    assert_eq!(again.write.hash, first.write.hash);
    assert_eq!(
        reopened.working_copy_record(working_copy).unwrap().desired_state,
        state_before,
        "an idempotent re-synthesis must not advance the view"
    );
}

// ── AC-3: journaling, failure containment, workspace boundary ──────────

#[test]
fn synthesis_failure_aborts_the_prepared_operation_without_view_advance() {
    let directory = TempDir::new().unwrap();
    let repo = Repository::init(directory.path()).unwrap();
    let working_copy = repo.require_working_copy_id().unwrap();
    let state_before = repo.working_copy_record(working_copy).unwrap().desired_state;

    // Injected fault: the mutation stage fails after the SynthesizeGit
    // intent was journaled, exercising the immutable recovery path.
    let synthesized = memory_add("doomed.txt", b"doomed\n");
    let origin = GitSynthesisOrigin::root(oid20(0x81), oid20(0x82)).unwrap();
    let unhashed = crate::git_synthesis_metadata(&origin, &[], None);
    super::super::synthesis::SYNTHESIS_APPLY_FAULT.with(|cell| cell.set(true));
    let error = repo
        .synthesize_git_change(
            fixed_header("doomed import"),
            &[synthesized],
            origin,
            unhashed,
            Vec::new(),
            &[],
            false,
            &empty_verification(),
            InsertOptions::default(),
            &[],
            &single_file_expectation("doomed.txt", b"doomed\n"),
            None,
            &[],
       false,
        )
        .expect_err("the injected mutation-stage fault must fail the synthesis");
    super::super::synthesis::SYNTHESIS_APPLY_FAULT.with(|cell| cell.set(false));

    // No unverified view/checkpoint advance survives.
    assert_eq!(
        repo.working_copy_record(working_copy).unwrap().desired_state,
        state_before,
        "the failed synthesis must not advance the view state"
    );
    assert!(repo.get_file_content("doomed.txt").unwrap().is_none());

    // The prepared SynthesizeGit operation was inverted by an immutable
    // Recover child, and the recovered head is complete (writable reopen
    // performs the same idempotent recovery without overwriting anything).
    drop(repo);
    let reopened = Repository::open(directory.path()).unwrap();
    let log = reopened
        .operation_log(OperationScope::WorkingCopy(working_copy), None, false)
        .unwrap();
    let kinds: Vec<OperationKind> = log
        .entries
        .iter()
        .map(|entry| entry.operation.payload().kind)
        .collect();
    assert!(
        kinds.contains(&OperationKind::SynthesizeGit),
        "the SynthesizeGit intent was journaled before the fault"
    );
    assert!(
        kinds.contains(&OperationKind::Recover),
        "the fault produced an immutable recovery operation"
    );
    for entry in &log.entries {
        let verified = entry.verification == crate::OperationVerificationState::Verified;
        let aborted_intent = entry.operation.payload().kind == OperationKind::SynthesizeGit
            && entry.verification == crate::OperationVerificationState::Prepared;
        assert!(
            verified || aborted_intent,
            "the chain must be Recover-verified with only the aborted intent left Prepared; \
             error was {error}"
        );
    }
    assert!(
        matches!(log.head_state, crate::OperationHeadState::Single(_)),
        "the recovered head is a single verified operation"
    );
    assert_eq!(
        reopened.working_copy_record(working_copy).unwrap().desired_state,
        state_before
    );
}

#[test]
fn synthesis_under_workspace_transaction_journals_synthesize_git_and_verifies() {
    let (directory, mut repo, head, tree) = initialized_colocated_repository();
    write_checkpoint(directory.path(), &repo, &head, &tree);
    let working_copy = repo.require_working_copy_id().unwrap();

    let WorkspaceTxnStart::Ready(workspace) =
        repo.begin_workspace_txn(WorkspaceTxnMode::Reconcile).unwrap()
    else {
        panic!("a clean colocated workspace must be ready");
    };
    let before_state = workspace.view().state;

    let synthesized = memory_add("bridge.txt", b"bridged\n");
    let origin = GitSynthesisOrigin::root(oid20(0x91), oid20(0x92)).unwrap();
    let unhashed = crate::git_synthesis_metadata(&origin, &[], None);
    let outcome = repo
        .synthesize_git_change_under_workspace(
            &workspace,
            fixed_header("bridged import"),
            &[synthesized],
            origin,
            unhashed,
            &[],
            false,
            &empty_verification(),
            InsertOptions::default(),
            None,
            &[],
            &single_file_expectation("bridge.txt", b"bridged\n"),
            &[],
            false,
        )
        .expect("workspace-backed synthesis");
    assert!(outcome.operation.is_some());
    assert!(!outcome.already_in_view);
    drop(workspace);

    // The journaled SynthesizeGit operation carries a verified receipt.
    let log = repo
        .operation_log(OperationScope::WorkingCopy(working_copy), None, false)
        .unwrap();
    let synthesis = log
        .entries
        .iter()
        .find(|entry| entry.operation.payload().kind == OperationKind::SynthesizeGit)
        .expect("SynthesizeGit operation in the working-copy log");
    assert_eq!(synthesis.verification, crate::OperationVerificationState::Verified);
    assert_ne!(outcome.write.insert.new_state, before_state);

    // The repository-scoped log also carries the operation (one causal order).
    let repo_log = repo
        .operation_log(OperationScope::Repository, None, false)
        .unwrap();
    assert!(repo_log
        .entries
        .iter()
        .any(|entry| entry.operation.payload().kind == OperationKind::SynthesizeGit));

    assert_eq!(
        repo.get_file_content("bridge.txt").unwrap(),
        Some(b"bridged\n".to_vec())
    );
}

#[test]
fn workspace_refusal_blocks_synthesis_entirely() {
    let (directory, mut repo, head, tree) = initialized_colocated_repository();
    write_checkpoint(directory.path(), &repo, &head, &tree);
    let working_copy = repo.require_working_copy_id().unwrap();
    let state_before = repo.working_copy_record(working_copy).unwrap().desired_state;

    // Git-owned in-progress state (MERGE_HEAD): every mode must refuse —
    // including Force — and no synthesis may journal or mutate.
    fs::write(directory.path().join(".git/MERGE_HEAD"), format!("{head}\n")).unwrap();
    let remediation = repo.begin_workspace_txn(WorkspaceTxnMode::Force).unwrap();
    assert!(matches!(
        remediation,
        WorkspaceTxnStart::Remediation(WorkspaceRemediation::GitOperationInProgress {
            disposition: GitOperationDisposition::ForceForbidden,
            ..
        })
    ));

    // The refused boundary leaves the workspace untouched.
    assert_eq!(
        repo.working_copy_record(working_copy).unwrap().desired_state,
        state_before
    );
    let log = repo
        .operation_log(OperationScope::WorkingCopy(working_copy), None, false)
        .unwrap();
    assert!(
        !log.entries
            .iter()
            .any(|entry| entry.operation.payload().kind == OperationKind::SynthesizeGit),
        "a refused boundary never journals synthesis"
    );
    assert!(repo.get_file_content("bridge.txt").unwrap().is_none());
}

// ── AC-2: verified bindings take resurrection over synthesis ───────────

/// Sign a binding over a real Git commit with a deterministic test keypair.
fn signed_binding(
    git_repo: &git2::Repository,
    commit_oid: git2::Oid,
    ordered: Vec<atomic_core::types::Hash>,
    seed: u8,
) -> GitStateBinding {
    use crate::git_binding::{BINDING_VERSION, GitStateBindingPayload};

    let commit = git_repo.find_commit(commit_oid).expect("commit");
    let mut secret = [0u8; 32];
    for (index, byte) in secret.iter_mut().enumerate() {
        *byte = seed.wrapping_add((index as u8) * 7 + 11);
    }
    let keypair = atomic_identity::keypair::KeyPair::from_secret_key(
        atomic_identity::keypair::SecretKey::from_bytes(&secret),
    );
    let mut payload = GitStateBindingPayload {
        version: BINDING_VERSION,
        git_object_format: GitObjectFormat::Sha1,
        git_commit: GitOid::from_hex(&commit_oid.to_string()).expect("commit oid"),
        git_tree: GitOid::from_hex(&commit.tree_id().to_string()).expect("tree oid"),
        git_parents: commit
            .parent_ids()
            .map(|oid| GitOid::from_hex(&oid.to_string()).expect("parent oid"))
            .collect(),
        raw_commit_object: None,
        set_id: atomic_core::types::SetId::from_bytes([11u8; 32]),
        merkle_state: atomic_core::types::Merkle::from_bytes([12u8; 32]),
        view_hint: None,
        ordered_changes: ordered,
        closure_root: atomic_core::types::Hash::from_bytes([0u8; 32]),
        operation: atomic_core::types::OperationId::from_bytes([13u8; 32]),
        origin: CausalOrigin::ExactAtomicResurrection,
        loss: Vec::new(),
        provenance_roots: Vec::new(),
        attestation_roots: Vec::new(),
        signer: BindingSigner::for_keypair(&keypair),
    };
    payload.closure_root = payload.compute_closure_root();
    GitStateBinding::sign(payload, &keypair).expect("sign binding")
}

#[test]
fn verified_binding_takes_resurrection_over_synthesis() {
    let directory = TempDir::new().unwrap();
    let repo = Repository::init(directory.path()).unwrap();

    // Record two Atomic changes that the binding will vouch for.
    let working_copy = repo.require_working_copy_id().unwrap();
    fs::write(directory.path().join("alpha.txt"), b"alpha\n").unwrap();
    let first = repo
        .record_with_message(
            working_copy,
            "first",
            RecordOptions::default().include_untracked(true),
        )
        .unwrap();
    fs::write(directory.path().join("beta.txt"), b"beta\n").unwrap();
    let second = repo
        .record_with_message(
            working_copy,
            "second",
            RecordOptions::default().include_untracked(true),
        )
        .unwrap();

    // A fresh repository where the binding's closure is absent.
    let target_dir = TempDir::new().unwrap();
    let mut target = Repository::init(target_dir.path()).unwrap();
    for hash in [first.hash(), second.hash()] {
        let change = repo.load_change(hash).unwrap();
        target.save_change(&change).unwrap();
    }

    // Build the real Git commit the binding is verified against.
    let git_repo = git2::Repository::init(target_dir.path()).unwrap();
    fs::write(target_dir.path().join("seed.txt"), b"seed\n").unwrap();
    let mut index = git_repo.index().unwrap();
    index.add_path(Path::new("seed.txt")).unwrap();
    index.write().unwrap();
    let tree_oid = index.write_tree().unwrap();
    let tree = git_repo.find_tree(tree_oid).unwrap();
    let signature =
        git2::Signature::now("Publisher", "pub@example.com").expect("signature");
    let commit_oid = git_repo
        .commit(Some("HEAD"), &signature, &signature, "bound", &tree, &[])
        .unwrap();

    let binding = signed_binding(
        &git_repo,
        commit_oid,
        vec![*first.hash(), *second.hash()],
        0x42,
    );
    target.store_binding(&binding).unwrap();

    // The verified binding is found by commit — by content, not by name.
    let found = target
        .verified_binding_for_commit(&git_repo, &oid20(0x77))
        .unwrap();
    assert!(found.is_none(), "an unrelated commit must not resurrect");

    let commit_oid_object =
        GitObjectId::new(GitHashAlgorithm::Sha1, commit_oid.as_bytes().to_vec()).unwrap();
    let found = target
        .verified_binding_for_commit(&git_repo, &commit_oid_object)
        .unwrap()
        .expect("the verified binding is found for its commit");

    let outcome = target
        .resurrect_binding_into_view(&found, &target.current_view())
        .expect("resurrection references the verified closure");
    assert_eq!(outcome.inserted.len(), 2);
    assert!(outcome.already_present.is_empty());
    let second_outcome = target
        .resurrect_binding_into_view(&found, &target.current_view())
        .unwrap();
    assert!(second_outcome.inserted.is_empty());
    assert_eq!(second_outcome.already_present.len(), 2);
}

#[test]
fn resurrection_fails_closed_when_the_closure_is_missing() {
    let directory = TempDir::new().unwrap();
    let repo = Repository::init(directory.path()).unwrap();
    fs::write(directory.path().join("only.txt"), b"only\n").unwrap();
    let recorded = repo
        .record_with_message(
            repo.require_working_copy_id().unwrap(),
            "only",
            RecordOptions::default().include_untracked(true),
        )
        .unwrap();

    let target_dir = TempDir::new().unwrap();
    let target = Repository::init(target_dir.path()).unwrap();
    let git_repo = git2::Repository::init(target_dir.path()).unwrap();
    fs::write(target_dir.path().join("seed.txt"), b"seed\n").unwrap();
    let mut index = git_repo.index().unwrap();
    index.add_path(Path::new("seed.txt")).unwrap();
    index.write().unwrap();
    let tree_oid = index.write_tree().unwrap();
    let tree = git_repo.find_tree(tree_oid).unwrap();
    let signature = git2::Signature::now("Publisher", "pub@example.com").unwrap();
    let commit_oid = git_repo
        .commit(Some("HEAD"), &signature, &signature, "bound", &tree, &[])
        .unwrap();
    let binding = signed_binding(&git_repo, commit_oid, vec![*recorded.hash()], 0x43);
    target.store_binding(&binding).unwrap();

    let commit_oid_object =
        GitObjectId::new(GitHashAlgorithm::Sha1, commit_oid.as_bytes().to_vec()).unwrap();
    let found = target
        .verified_binding_for_commit(&git_repo, &commit_oid_object)
        .unwrap()
        .expect("the binding verifies against the live Git object database");
    let error = target
        .resurrect_binding_into_view(&found, &target.current_view())
        .expect_err("a missing closure fails closed instead of synthesizing");
    assert!(
        error.to_string().contains("missing locally"),
        "explicit closure diagnostic required, found: {error}"
    );
}


// ── Review C4: the already-applied (reference) hash path carries the same
// alignment + pre-publication verification duties as the apply path ──────

#[test]
fn reference_registered_change_verifies_the_effective_projection_before_publication() {
    let directory = TempDir::new().unwrap();
    let mut repo = Repository::init(directory.path()).unwrap();

    let recorded = memory_add("doc.txt", b"referenced content\n");
    let origin = GitSynthesisOrigin::root(oid20(0x71), oid20(0x72)).expect("root origin");
    let unhashed = crate::git_synthesis_metadata(&origin, &[], None);
    let expectation = single_file_expectation("doc.txt", b"referenced content\n");

    // First synthesis publishes the change into the default view.
    repo.synthesize_git_change(
        fixed_header("import doc"),
        &[recorded.clone()],
        origin.clone(),
        unhashed.clone(),
        Vec::new(),
        &[],
        false,
        &empty_verification(),
        InsertOptions::default(),
        &[],
        &expectation,
        None,
        &[],
        false,
    )
    .expect("publish the change");

    // A second, empty view: re-synthesizing the SAME change content routes
    // through the reference path (registered + applied, not yet a member of
    // the target view). The reference path must verify the post-alignment
    // effective projection against the expectation before committing.
    repo.create_shared_view("verified-reference")
        .expect("create the target view");
    repo.synthesize_git_change(
        fixed_header("import doc"),
        &[recorded.clone()],
        origin.clone(),
        unhashed.clone(),
        Vec::new(),
        &[],
        false,
        &empty_verification(),
        InsertOptions {
            view: Some("verified-reference".to_string()),
            ..InsertOptions::default()
        },
        &[],
        &expectation,
        None,
        &[],
        true,
    )
    .expect("the reference path verifies and publishes into the new view");

    // The same reference path refuses when the expectation disagrees with
    // the actual post-alignment view: verification is not bypassed for
    // already-known hashes (review C4).
    repo.create_shared_view("verified-refusal")
        .expect("create the second target view");
    let wrong_bytes = single_file_expectation("doc.txt", b"DIVERGED BYTES\n");
    let error = repo
        .synthesize_git_change(
            fixed_header("import doc"),
            &[recorded],
            origin,
            unhashed,
            Vec::new(),
            &[],
            false,
            &empty_verification(),
            InsertOptions {
                view: Some("verified-refusal".to_string()),
                ..InsertOptions::default()
            },
            &[],
            &wrong_bytes,
            None,
            &[],
            true,
        )
        .expect_err(
            "the reference path must verify the effective projection and refuse a \
             diverged expectation",
        );
    assert!(
        error.to_string().contains("staged"),
        "the refusal must come from the staged verification: {error}"
    );
}


// ── Review C1: positive token-level equality after an edit flow — the
// applier-keyed leaf rows must reconstruct the edited line's tokens exactly,
// across a reload, with no stale predecessor tokens alive ────────────────

// ── Review C1: positive token-level equality after an edit flow — the
// applier-keyed leaf rows must reconstruct the edited line's tokens exactly,
// across a reload, with no stale predecessor tokens alive ────────────────

#[test]
fn edited_lines_render_exact_alive_tokens_after_reload() {
    let directory = TempDir::new().unwrap();
    let mut repo = Repository::init(directory.path()).unwrap();

    // A base change adding two files, then an edit of the first file.
    let base_files = [
        ("f.txt", b"unchanged line\noriginal words here\n".to_vec()),
        ("g.txt", b"untouched\n".to_vec()),
    ];
    let recorded_base: Vec<RecordedFile> = base_files
        .iter()
        .map(|(path, bytes)| memory_add(path, bytes))
        .collect();
    let origin = GitSynthesisOrigin::root(oid20(0x81), oid20(0x82)).expect("root origin");
    let unhashed = crate::git_synthesis_metadata(&origin, &[], None);
    let base_expectation = full_tree_expectation(
        &[
            ("f.txt", &b"unchanged line\noriginal words here\n"[..]),
            ("g.txt", &b"untouched\n"[..]),
        ],
        &["f.txt", "g.txt"],
    );
    repo.synthesize_git_change(
        fixed_header("import base"),
        &recorded_base,
        origin.clone(),
        unhashed.clone(),
        Vec::new(),
        &[],
        false,
        &empty_verification(),
        InsertOptions::default(),
        &[],
        &base_expectation,
        None,
        &[],
        false,
    )
    .expect("publish base");

    // The edit rewrites the second line of f.txt in place (Modify), bound to
    // the file's existing CRDT identities.
    let edited = memory_modify(
        &repo,
        "f.txt",
        b"unchanged line\noriginal words here\n",
        b"unchanged line\nedited tokens now\n",
    );
    let edit_expectation = full_tree_expectation(
        &[
            ("f.txt", &b"unchanged line\nedited tokens now\n"[..]),
            ("g.txt", &b"untouched\n"[..]),
        ],
        &["f.txt"],
    );
    repo.synthesize_git_change(
        fixed_header("edit f"),
        &[edited],
        GitSynthesisOrigin::first_parent(oid20(0x83), oid20(0x84), oid20(0x11))
            .expect("first-parent origin"),
        unhashed,
        Vec::new(),
        &[],
        false,
        &empty_verification(),
        InsertOptions::default(),
        &[],
        &edit_expectation,
        None,
        &[],
        true,
    )
    .expect("publish the edit");

    // Reopen and inspect the staged semantic rows through the CRDT layer.
    drop(repo);
    let reopened = Repository::open(directory.path()).unwrap();
    let edit_change = {
        let entries = reopened.effective_history(None).unwrap();
        reopened.load_change(&entries.last().unwrap().hash).unwrap()
    };
    let edit_id = reopened
        .pristine()
        .read_txn()
        .unwrap()
        .get_internal(&edit_change.hash().unwrap())
        .unwrap()
        .expect("the edit is registered");
    let mut txn = reopened.pristine().write_txn().unwrap();
    for (path, expected_alive_lines) in [("f.txt", 2usize), ("g.txt", 1usize)] {
        let lines = atomic_core::output::crdt::get_file_lines(&mut txn, path)
            .expect("read semantic lines");
        let alive: Vec<&atomic_core::output::crdt::Line> =
            lines.iter().filter(|line| line.state.is_alive()).collect();
        assert_eq!(
            alive.len(),
            expected_alive_lines,
            "{path} must render exactly its alive lines"
        );
        for line in &alive {
            assert!(
                !line.tokens.is_empty(),
                "{path} line {} renders alive tokens",
                line.number
            );
            for token in &line.tokens {
                assert!(
                    token.state.is_alive(),
                    "{path} line {} token {:?} must be alive (no stale predecessor \
                     tokens survive a Modify)",
                    path,
                    token.id
                );
            }
        }
    }
    // The rewritten line's tokens are keyed by the EDITING change's
    // namespace: the last-apply-wins re-keying defect is gone (review C1).
    let f_lines =
        atomic_core::output::crdt::get_file_lines(&mut txn, "f.txt").expect("f.txt lines");
    let edited_line = f_lines.iter().find(|line| line.number == 2).expect("edited line");
    for token in &edited_line.tokens {
        assert_eq!(
            token.id.change_id(),
            edit_id,
            "the rewritten line's tokens are keyed by the editing change"
        );
    }
    txn.commit().unwrap();
}

// ── Review D1: the already-applied reference path prepares the exact
// removal/add leases and compacted sequence/Merkle the fresh-apply path
// prepares, and an already-present hash still verifies its expectation ───

/// The view's member hashes in sequence order.
fn view_members(repo: &Repository, view: &str) -> Vec<atomic_core::types::Hash> {
    use atomic_core::pristine::{GraphTxnT, ViewTxnT};
    let txn = repo.pristine().read_txn().expect("read pristine");
    let view_state = txn.get_view(view).expect("read view").expect("view exists");
    txn.iter_changes(&view_state, 0)
        .expect("iterate members")
        .map(|row| {
            let (_, change_id, _) = row.expect("member row");
            txn.get_external(change_id)
                .expect("external hash")
                .expect("registered hash")
        })
        .collect()
}

#[test]
fn reference_registered_change_with_target_local_removal_prepares_exact_leases() {
    let directory = TempDir::new().unwrap();
    let mut repo = Repository::init(directory.path()).unwrap();
    let working_copy = repo.require_working_copy_id().unwrap();
    let verification = empty_verification();

    // A is applied to the default view first, so referencing it into the
    // target view routes through the reference path.
    let recorded_a = memory_add("doc.txt", b"referenced content\n");
    let origin_a = GitSynthesisOrigin::root(oid20(0x71), oid20(0x72)).expect("root origin");
    let unhashed_a = crate::git_synthesis_metadata(&origin_a, &[], None);
    let hash_a = repo
        .synthesize_git_change(
            fixed_header("import doc"),
            &[recorded_a.clone()],
            origin_a.clone(),
            unhashed_a.clone(),
            Vec::new(),
            &[],
            false,
            &empty_verification(),
            InsertOptions::default(),
            &[],
            &single_file_expectation("doc.txt", b"referenced content\n"),
            None,
            &[],
            false,
        )
        .expect("publish A into the default view")
        .write
        .hash;

    // The target view holds B (the member the rewrite supersedes) and the
    // unrelated C, which must survive.
    repo.create_shared_view("target").expect("create target");
    let target_options = |view: &str| InsertOptions {
        view: Some(view.to_string()),
        ..InsertOptions::default()
    };
    let recorded_b = memory_add("other.txt", b"superseded\n");
    let origin_b = GitSynthesisOrigin::root(oid20(0x73), oid20(0x74)).expect("origin b");
    let hash_b = repo
        .synthesize_git_change(
            fixed_header("import other"),
            &[recorded_b],
            origin_b.clone(),
            crate::git_synthesis_metadata(&origin_b, &[], None),
            Vec::new(),
            &[],
            false,
            &verification,
            target_options("target"),
            &[],
            &single_file_expectation("other.txt", b"superseded\n"),
            None,
            &[],
            false,
        )
        .expect("publish B")
        .write
        .hash;
    let recorded_c = memory_add("keep.txt", b"preserved\n");
    let origin_c = GitSynthesisOrigin::root(oid20(0x75), oid20(0x76)).expect("origin c");
    let hash_c = repo
        .synthesize_git_change(
            fixed_header("import keep"),
            &[recorded_c],
            origin_c.clone(),
            crate::git_synthesis_metadata(&origin_c, &[], None),
            Vec::new(),
            &[],
            false,
            &verification,
            target_options("target"),
            &[],
            &full_tree_expectation(
                &[("other.txt", b"superseded\n"), ("keep.txt", b"preserved\n")],
                &["keep.txt"],
            ),
            None,
            &[],
            false,
        )
        .expect("publish C")
        .write
        .hash;

    // Reference the already-applied A into `target` while superseding B:
    // the prepared operation must carry the removal lease for B and an add
    // lease whose expected sequence is the FINAL compacted position. The
    // historical defect prepared the pre-removal lease, committed
    // membership [C, A], then rejected its own lease after the commit —
    // leaving a Prepared head that blocked ordinary reopen (review D1).
    let outcome = repo
        .synthesize_git_change(
            fixed_header("import doc"),
            &[recorded_a],
            origin_a,
            unhashed_a,
            Vec::new(),
            &[],
            false,
            &verification,
            target_options("target"),
            &[hash_b],
            &full_tree_expectation(
                &[("doc.txt", b"referenced content\n"), ("keep.txt", b"preserved\n")],
                &["doc.txt"],
            ),
            None,
            &[],
            true,
        )
        .expect("the reference with removal must prepare exact leases and publish");

    assert!(!outcome.already_in_view, "the change is newly referenced");
    assert_eq!(
        view_members(&repo, "target"),
        vec![hash_c, hash_a],
        "the compacted target holds exactly the retained member C and the referenced A \
         (B was superseded); the committed membership must equal the prepared after-state"
    );
    // The operation must be verified with a receipt, and the repository
    // must reopen: a stale lease left a Prepared head that gated reopen.
    let operation_id = outcome.operation.expect("journaled operation");
    assert!(
        repo.operation_has_verified_receipt(operation_id)
            .expect("read verification"),
        "the reference operation must finalize verified"
    );
    drop(repo);
    let reopened = Repository::open(directory.path())
        .expect("ordinary reopen must succeed after the committed reference");
    assert_eq!(
        view_members(&reopened, "target"),
        vec![hash_c, hash_a],
        "reopen sees the exact published membership"
    );
}

#[test]
fn already_in_view_refuses_a_diverged_expectation() {
    let directory = TempDir::new().unwrap();
    let repo = Repository::init(directory.path()).unwrap();
    let recorded = memory_add("doc.txt", b"referenced content\n");
    let origin = GitSynthesisOrigin::root(oid20(0x71), oid20(0x72)).expect("root origin");
    let unhashed = crate::git_synthesis_metadata(&origin, &[], None);
    let correct = single_file_expectation("doc.txt", b"referenced content\n");
    repo.synthesize_git_change(
        fixed_header("import doc"),
        &[recorded.clone()],
        origin.clone(),
        unhashed.clone(),
        Vec::new(),
        &[],
        false,
        &empty_verification(),
        InsertOptions::default(),
        &[],
        &correct,
        None,
        &[],
        false,
    )
    .expect("publish the change");

    // Re-synthesizing the SAME change against a deliberately wrong
    // expectation must refuse: an already-present hash cannot bypass the
    // staged proof (review D1).
    let wrong = single_file_expectation("doc.txt", b"DEFINITELY WRONG\n");
    let error = repo
        .synthesize_git_change(
            fixed_header("import doc"),
            &[recorded.clone()],
            origin.clone(),
            unhashed.clone(),
            Vec::new(),
            &[],
            false,
            &empty_verification(),
            InsertOptions::default(),
            &[],
            &wrong,
            None,
            &[],
            true,
        )
        .expect_err("a wrong already-present expectation must be refused");
    assert!(
        error.to_string().contains("does not render the supplied expectation"),
        "the refusal must come from the already-present verification: {error}"
    );

    // A clean retry with the matching expectation is the documented no-op.
    let outcome = repo
        .synthesize_git_change(
            fixed_header("import doc"),
            &[recorded],
            origin,
            unhashed,
            Vec::new(),
            &[],
            false,
            &empty_verification(),
            InsertOptions::default(),
            &[],
            &correct,
            None,
            &[],
            true,
        )
        .expect("the matching expectation is a clean no-op");
    assert!(outcome.already_in_view, "the retry reads already-in-view");
    assert!(outcome.operation.is_none(), "no operation is journaled");
}

#[test]
fn reference_path_verification_failure_aborts_cleanly_and_retries() {
    let directory = TempDir::new().unwrap();
    let mut repo = Repository::init(directory.path()).unwrap();
    let verification = empty_verification();

    // A is applied to the default view; the target view is empty.
    let recorded_a = memory_add("doc.txt", b"referenced content\n");
    let origin_a = GitSynthesisOrigin::root(oid20(0x71), oid20(0x72)).expect("root origin");
    let unhashed_a = crate::git_synthesis_metadata(&origin_a, &[], None);
    repo.synthesize_git_change(
        fixed_header("import doc"),
        &[recorded_a.clone()],
        origin_a.clone(),
        unhashed_a.clone(),
        Vec::new(),
        &[],
        false,
        &empty_verification(),
        InsertOptions::default(),
        &[],
        &single_file_expectation("doc.txt", b"referenced content\n"),
        None,
        &[],
        false,
    )
    .expect("publish A");
    repo.create_shared_view("retry-target")
        .expect("create the target view");

    // The reference with a deliberately wrong expectation must fail BEFORE
    // publication (precommit abort): the immutable Recover child inverts
    // the prepared operation, nothing is published, and the repository
    // stays openable.
    let wrong = single_file_expectation("doc.txt", b"DIVERGED\n");
    let options = InsertOptions {
        view: Some("retry-target".to_string()),
        ..InsertOptions::default()
    };
    let error = repo
        .synthesize_git_change(
            fixed_header("import doc"),
            &[recorded_a.clone()],
            origin_a.clone(),
            unhashed_a.clone(),
            Vec::new(),
            &[],
            false,
            &empty_verification(),
            options.clone(),
            &[],
            &wrong,
            None,
            &[],
            true,
        )
        .expect_err("the diverged expectation must refuse before publication");
    assert!(
        error.to_string().contains("staged"),
        "the refusal must come from the staged verification: {error}"
    );
    drop(repo);
    let mut repo = Repository::open(directory.path())
        .expect("the aborted reference must not gate reopen");
    let txn = repo.pristine().read_txn().unwrap();
    let view = txn.get_view("retry-target").unwrap().expect("view exists");
    assert_eq!(
        txn.iter_changes(&view, 0).unwrap().count(),
        0,
        "no reference was published by the aborted operation"
    );
    drop(txn);

    // The clean retry with the exact expectation publishes.
    let correct = single_file_expectation("doc.txt", b"referenced content\n");
    repo.synthesize_git_change(
        fixed_header("import doc"),
        &[recorded_a],
        origin_a,
        unhashed_a,
        Vec::new(),
        &[],
        false,
        &empty_verification(),
        options,
        &[],
        &correct,
        None,
        &[],
        true,
    )
    .expect("the clean retry publishes");
    assert_eq!(view_members(&repo, "retry-target").len(), 1);
}

// ── Review D2: staged semantic verification validates the ACTUAL CRDT
// reconstruction and refuses every unattributed ambient corruption —
// unexpected chain branches, nonexistent-vertex "deletions", registered-
// but-never-applied leaf writers, and tombstoned opaque trunks ────────────

/// Publish the two-file base (f.txt + g.txt), then corrupt only f.txt's
/// ambient CRDT metadata via the public pristine APIs, then require the
/// g.txt successor's staged verification to refuse with a typed message.
struct CorruptedBase {
    #[allow(dead_code)]
    directory: TempDir,
    repo: Repository,
}

impl CorruptedBase {
    fn new(binary: bool) -> Self {
        let directory = TempDir::new().unwrap();
        let mut repo = Repository::init(directory.path()).unwrap();
        let f_bytes: &[u8] = if binary {
            &[0, 1, 2, 3]
        } else {
            b"inherited text\n"
        };
        let recorded: Vec<RecordedFile> = [
            ("f.txt", f_bytes),
            ("g.txt", &b"other\n"[..]),
        ]
        .iter()
        .map(|(path, bytes)| memory_add(path, bytes))
        .collect();
        let origin = GitSynthesisOrigin::root(oid20(0x31), oid20(0x32)).expect("root origin");
        let unhashed = crate::git_synthesis_metadata(&origin, &[], None);
        let expectation = full_tree_expectation(
            &[("f.txt", f_bytes), ("g.txt", b"other\n")],
            &["f.txt", "g.txt"],
        );
        repo.synthesize_git_change(
            fixed_header("import base"),
            &recorded,
            origin,
            unhashed,
            Vec::new(),
            &[],
            false,
            &empty_verification(),
            InsertOptions::default(),
            &[],
            &expectation,
            None,
            &[],
            false,
        )
        .expect("publish the base");
        Self { directory, repo }
    }

    /// Synthesize the g.txt successor and require the typed refusal.
    /// `f_bytes` is the untouched inherited content of f.txt.
    fn finish_refusal(&mut self, f_bytes: &[u8], expected_fragment: &str) -> String {
        let recorded = memory_modify(&self.repo, "g.txt", b"other\n", b"successor\n");
        let origin =
            GitSynthesisOrigin::first_parent(oid20(0x33), oid20(0x34), oid20(0x11))
                .expect("first-parent origin");
        let unhashed = crate::git_synthesis_metadata(&origin, &[], None);
        let expectation = full_tree_expectation(
            &[("f.txt", f_bytes), ("g.txt", b"successor\n")],
            &["g.txt"],
        );
        let error = self
            .repo
            .synthesize_git_change(
                fixed_header("edit g"),
                &[recorded],
                origin,
                unhashed,
                Vec::new(),
                &[],
                false,
                &empty_verification(),
                InsertOptions::default(),
                &[],
                &expectation,
                None,
                &[],
                true,
            )
            .expect_err("the corrupted ambient semantic state must refuse the successor");
        let message = error.to_string();
        assert!(
            message.contains(expected_fragment),
            "expected the refusal to mention {expected_fragment:?}: {message}"
        );
        message
    }
}

/// f.txt's first branch: (encoded key, branch id, trunk id).
fn first_branch(repo: &Repository, path: &str) -> (
    [u8; 12],
    atomic_core::crdt::BranchId,
    atomic_core::crdt::TrunkId,
) {
    use atomic_core::crdt::queries::iter_trunk_branches_in_file_order;
    use atomic_core::crdt::tables::encode_branch_id;
    use atomic_core::pristine::CrdtTxnT;
    let txn = repo.pristine().read_txn().unwrap();
    let trunk = txn.get_trunk_by_path(path).unwrap().expect("trunk exists");
    let branches = iter_trunk_branches_in_file_order(&txn, trunk).unwrap();
    let branch = branches[0];
    (encode_branch_id(&branch), branch, trunk)
}

/// Review D2 (extra-branch): an unexpected alive branch in the staged
/// after-chain with no BRANCH_VERTEX is refused — the historical silent
/// skip let the consumer walker fail with OrphanBranch while the graph
/// output stayed intact.
#[test]
fn staged_verification_refuses_an_unexpected_chain_branch() {
    use atomic_core::crdt::tables::{
        encode_branch_id, encode_branch_value, encode_trunk_id, SerializedBranch,
    };
    use atomic_core::pristine::{CrdtTxnT, MutTxnT};

    let mut base = CorruptedBase::new(false);
    let (first_key, _first_branch, trunk) = first_branch(&base.repo, "f.txt");
    {
        let txn = base.repo.pristine();
        let mut txn = txn.write_txn().unwrap();
        let extra = encode_branch_id(&atomic_core::crdt::BranchId::new(
            atomic_core::types::NodeId::ROOT,
            999,
        ));
        let row = SerializedBranch {
            trunk_id: trunk,
            state: atomic_core::crdt::BranchState::Alive,
            line_hash: 0,
        };
        txn.put_crdt_branch(&extra, &encode_branch_value(&row)).unwrap();
        txn.put_crdt_trunk_branch(&encode_trunk_id(&trunk), &extra)
            .unwrap();
        txn.put_crdt_branch_after(&extra, &first_key).unwrap();
        txn.commit().unwrap();
    }
    let error = base.finish_refusal(b"inherited text\n", "the staged file-order chain");
    assert!(
        error.contains("no BRANCH_VERTEX"),
        "the unexpected alive branch must be refused before the consumer walker fails          with OrphanBranch: {error}"
    );
}

/// Review D2 (tombstone-vertex): tombstoning a live branch AND pointing its
/// BRANCH_VERTEX at a nonexistent vertex must refuse — absence from the
/// global alive set is not a deletion proof.
#[test]
fn staged_verification_refuses_an_unattributed_branch_tombstone_with_fake_vertex() {
    use atomic_core::crdt::tables::{encode_branch_value, encode_vertex_position};
    use atomic_core::pristine::{CrdtTxnT, MutTxnT};

    let mut base = CorruptedBase::new(false);
    let (branch_key, first_branch_id, _trunk) = first_branch(&base.repo, "f.txt");
    {
        let txn = base.repo.pristine();
        let mut txn = txn.write_txn().unwrap();
        let mut row = txn.get_crdt_branch(&branch_key).unwrap().unwrap();
        row.state = atomic_core::crdt::BranchState::Deleted;
        txn.put_crdt_branch(&branch_key, &encode_branch_value(&row))
            .unwrap();
        let vertex = atomic_core::types::GraphNode::new(
            first_branch_id.change_id().to_owned(),
            10000u64.into(),
            20000u64.into(),
        );
        txn.put_crdt_branch_vertex(&branch_key, &encode_vertex_position(&vertex))
            .unwrap();
        txn.commit().unwrap();
    }
    let error =
            base.finish_refusal(b"inherited text\n", "unattributed tombstone");
    assert!(
        error.contains("no applied change deleted it"),
        "the fake vertex must not attribute the tombstone: {error}"
    );
}

/// Review D2 (extra-leaf): a live token linked under a NodeId whose hash is
/// registered but never saved or applied is refused — a registered NodeId
/// is not proof of a writer.
#[test]
fn staged_verification_refuses_a_leaf_written_by_a_never_applied_change() {
    use atomic_core::crdt::tables::{encode_leaf_id, encode_leaf_value, SerializedLeaf};
    use atomic_core::pristine::{CrdtTxnT, MutTxnT};

    let mut base = CorruptedBase::new(false);
    let (branch_key, first_branch_id, _trunk) = first_branch(&base.repo, "f.txt");
    {
        let txn = base.repo.pristine();
        let mut txn = txn.write_txn().unwrap();
        let owner = txn
            .register_change(&atomic_core::types::Hash::of(
                b"never applied or saved a change",
            ))
            .unwrap();
        let leaf = encode_leaf_id(&atomic_core::crdt::LeafId::new(owner, 999));
        let row = SerializedLeaf {
            branch_id: first_branch_id,
            kind: atomic_core::diff::TokenKind::Word,
            state: atomic_core::crdt::LeafState::Alive,
            content_start: 0,
            content_end: 9,
        };
        txn.put_crdt_leaf(&leaf, &encode_leaf_value(&row)).unwrap();
        txn.put_crdt_branch_leaf(&branch_key, &leaf).unwrap();
        txn.commit().unwrap();
    }
    let error =
            base.finish_refusal(b"inherited text\n", "token linkage corruption");
    assert!(
        error.contains("no applied out-of-closure change recorded it"),
        "a registered NodeId is not proof of a writer: {error}"
    );
}

/// Review D2 (binary-deleted): tombstoning a live opaque trunk with no
/// deleting operation is refused — opaque content has no invented text
/// tokens, but its trunk row must still be alive.
#[test]
fn staged_verification_refuses_a_tombstoned_opaque_trunk() {
    use atomic_core::crdt::tables::{encode_trunk_id, encode_trunk_value};
    use atomic_core::pristine::{CrdtTxnT, MutTxnT};

    let mut base = CorruptedBase::new(true);
    {
        let txn = base.repo.pristine();
        let mut txn = txn.write_txn().unwrap();
        let trunk = txn.get_trunk_by_path("f.txt").unwrap().expect("trunk exists");
        let key = encode_trunk_id(&trunk);
        let mut row = txn.get_crdt_trunk(&key).unwrap().unwrap();
        assert_eq!(row.encoding, 0, "the fixture trunk must be opaque");
        row.state = atomic_core::crdt::TrunkState::Deleted;
        txn.put_crdt_trunk(&key, &encode_trunk_value(&row)).unwrap();
        txn.commit().unwrap();
    }
    let error = base.finish_refusal(&[0, 1, 2, 3], "unattributed trunk tombstone");
    assert!(
        error.contains("the trunk row is marked Deleted"),
        "an opaque trunk must still be alive when the closure expects it: {error}"
    );
}

/// CB-9B F1 — the out-of-closure branch/trunk tombstone scanners may attribute
/// a tombstone only to an APPLIED change. A merely registered (fetched or
/// staged) change's FileOps are not execution proof. This unit reaches the two
/// scanners directly with a registered-but-never-applied deleter.
#[test]
fn out_of_closure_tombstone_attribution_requires_an_applied_writer() {
    use atomic_core::change::{Change, ChangeHeader, FileOps, LineOps};
    use atomic_core::crdt::{BranchId, TrunkId};
    use atomic_core::pristine::{GraphVisibilityClosure, GraphTxnT, MutTxnT};
    use atomic_core::types::{Hash, NodeId};

    let (_temp, repo) = create_temp_repo();
    let branch = BranchId::new(NodeId::new(7), 0);
    let trunk = TrunkId::new(NodeId::new(7), 0);

    // A change whose FileOps delete the branch and the file's trunk.
    let header = ChangeHeader::builder().message("unapplied deleter").build();
    let mut edit = FileOps::edit(trunk, "f.txt".to_string());
    edit.add_line_op(LineOps::delete_empty(branch));
    let change = Change::with_file_ops(
        header,
        Vec::new(),
        vec![edit, FileOps::delete(trunk, "f.txt".to_string())],
        Vec::new(),
        Vec::new(),
    );
    let mut bytes = Vec::new();
    let hash = change.serialize(&mut bytes).expect("serialize the deleter");

    let visibility = GraphVisibilityClosure::empty();
    let loader = |_hash: &Hash| -> Result<Change, String> { Ok(change.clone()) };

    let mut txn = repo.pristine().write_txn().unwrap();
    // Register (fetch/stage) but NEVER apply.
    let id = txn.register_change(&hash).unwrap();
    assert!(
        !txn.has_change_in_graph(id).unwrap(),
        "the deleter must stay unapplied for this regression"
    );

    let branch_attributed =
        super::super::synthesis::out_of_closure_writer_deleted_branch(
            &loader, &txn, "f.txt", branch, &visibility,
        )
        .unwrap();
    assert!(
        !branch_attributed,
        "a registered-but-unapplied change must not attribute a branch tombstone"
    );
    let trunk_attributed =
        super::super::synthesis::out_of_closure_writer_deleted_trunk(
            &loader, &txn, "f.txt", &visibility,
        )
        .unwrap();
    assert!(
        !trunk_attributed,
        "a registered-but-unapplied change must not attribute a trunk tombstone"
    );
}
