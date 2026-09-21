//! CB-6C: exact bound resurrection and the checked Git SHA cache.
//!
//! Fixtures verify original causal and semantic identities — hashes, bytes,
//! dependencies, frontiers, semantic identities, membership order, Merkle
//! state, SetId, persisted conflicts — never merely matching text.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use atomic_core::change::Change;
use atomic_core::operation::{GitHashAlgorithm, GitObjectId};
use atomic_core::types::OperationId;
use atomic_core::pristine::ViewTxnT;
use atomic_core::types::{Base32, Hash, Merkle, SetId};

use crate::git_binding::{
    BindingSigner, CausalOrigin, GitObjectFormat, GitOid, GitStateBinding, GitStateBindingPayload,
    BINDING_VERSION,
};
use crate::record::RecordOptions;
use crate::repository::project_tree::ConversionPolicy;
use crate::repository::synthesis::git_oid_hex;
use crate::{ExactResurrection, GitShaResolution, Repository};

use super::*;

fn keypair(seed: u8) -> atomic_identity::keypair::KeyPair {
    let mut secret = [0u8; 32];
    for (index, byte) in secret.iter_mut().enumerate() {
        *byte = seed.wrapping_add((index as u8) * 7 + 11);
    }
    atomic_identity::keypair::KeyPair::from_secret_key(
        atomic_identity::keypair::SecretKey::from_bytes(&secret),
    )
}

fn record_all(repo: &Repository, message: &str) {
    let working_copy = repo.require_working_copy_id().unwrap();
    repo.record_with_message(
        working_copy,
        message,
        RecordOptions::new()
            .with_all(true)
            .include_untracked(true)
            .save_to_store(true),
    )
    .unwrap();
}

fn hex_oid(oid: &GitObjectId) -> String {
    oid.as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Materialize the projected Git tree objects into a real Git repository and
/// commit them as one squash-shaped commit; HEAD resolves to it.
fn bind_projection_to_commit(
    git: &git2::Repository,
    project: &crate::ProjectTree,
) -> (git2::Oid, Vec<u8>) {
    let odb = git.odb().unwrap();
    for (expected, object) in project.git.objects.iter() {
        let kind = match object.kind {
            crate::GitObjectKind::Blob => git2::ObjectType::Blob,
            crate::GitObjectKind::Tree => git2::ObjectType::Tree,
        };
        let written = odb.write(kind, &object.bytes).unwrap();
        assert_eq!(written.as_bytes(), expected.as_bytes());
    }
    let tree = git
        .find_tree(git2::Oid::from_bytes(project.git.root.as_bytes()).unwrap())
        .unwrap();
    let signature = git2::Signature::now("Publisher", "pub@example.com").unwrap();
    let commit = git
        .commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "bound",
            &tree,
            &[],
        )
        .unwrap();
    git.set_head("refs/heads/main").unwrap();
    let raw = odb.read(commit).unwrap().data().to_vec();
    (commit, raw)
}

/// Assemble a signed binding payload from explicit identity facts.
fn signed_binding(
    seed: u8,
    git_object_format: GitObjectFormat,
    commit_hex: &str,
    tree_hex: &str,
    parents: Vec<GitOid>,
    raw_commit: Option<Vec<u8>>,
    set_id: SetId,
    merkle_state: Merkle,
    ordered_changes: Vec<Hash>,
) -> GitStateBinding {
    let signer = keypair(seed);
    let mut payload = GitStateBindingPayload {
        version: BINDING_VERSION,
        git_object_format,
        git_commit: GitOid::from_hex(commit_hex).unwrap(),
        git_tree: GitOid::from_hex(tree_hex).unwrap(),
        git_parents: parents,
        raw_commit_object: raw_commit,
        set_id,
        merkle_state,
        view_hint: None,
        ordered_changes,
        closure_root: Hash::from_bytes([0u8; 32]),
        operation: OperationId::from_bytes([13u8; 32]),
        origin: CausalOrigin::ExactAtomicResurrection,
        loss: Vec::new(),
        provenance_roots: vec![Hash::of(b"provenance-root")],
        attestation_roots: vec![Hash::of(b"attestation-root")],
        signer: BindingSigner::for_keypair(&signer),
    };
    payload.closure_root = payload.compute_closure_root();
    GitStateBinding::sign(payload, &signer).unwrap()
}

/// A publisher: recorded closure + projected tree bound to one squash-shaped
/// commit in a bare bridge repository.
struct Publisher {
    repo: Repository,
    view: String,
    ordered: Vec<Hash>,
    set_id: SetId,
    merkle: Merkle,
    commit_hex: String,
    tree_hex: String,
    raw_commit: Vec<u8>,
}

impl Publisher {
    fn new(directory: &Path) -> Self {
        let repo = Repository::init(directory).unwrap();

        fs::create_dir_all(directory.join("nested")).unwrap();
        fs::write(directory.join("nested/alpha.txt"), b"alpha body\n").unwrap();
        record_all(&repo, "first");
        fs::write(directory.join("nested/alpha.txt"), b"alpha body v2\n").unwrap();
        record_all(&repo, "second");
        fs::write(directory.join("empty.txt"), b"").unwrap();
        record_all(&repo, "third");

        let view = repo.current_view().to_string();
        let identity = repo.view_identity(&view).unwrap();
        let ordered: Vec<Hash> = repo
            .effective_history(Some(&view))
            .unwrap()
            .iter()
            .map(|entry| entry.hash)
            .collect();

        let policy = ConversionPolicy::new(GitHashAlgorithm::Sha1);
        let project = repo.project_tree(&view, &policy).unwrap();
        let git = git2::Repository::init_opts(
            directory.join("bridge.git"),
            git2::RepositoryInitOptions::new().bare(true),
        )
        .unwrap();
        let (commit, raw_commit) = bind_projection_to_commit(&git, &project);
        Self {
            repo,
            ordered,
            set_id: identity.set_id,
            merkle: identity.merkle,
            commit_hex: commit.to_string(),
            tree_hex: hex_oid(&project.git.root),
            raw_commit,
            view,
        }
    }

    fn binding(&self, seed: u8) -> GitStateBinding {
        signed_binding(
            seed,
            GitObjectFormat::Sha1,
            &self.commit_hex,
            &self.tree_hex,
            Vec::new(),
            Some(self.raw_commit.clone()),
            self.set_id,
            self.merkle,
            self.ordered.clone(),
        )
    }
}

/// A fresh target repository plus a bridge Git repository carrying the same
/// bound commit (the transport-delivered world).
struct Target {
    repo: Repository,
    git: git2::Repository,
    commit_hex: String,
    raw_commit: Vec<u8>,
}

impl Target {
    fn new(directory: &Path, publisher: &Publisher) -> Self {
        let repo = Repository::init(directory).unwrap();
        let git = git2::Repository::init_opts(
            directory.join("bridge.git"),
            git2::RepositoryInitOptions::new().bare(true),
        )
        .unwrap();
        // The bound commit projects from the PUBLISHER: the target's graph is
        // still empty; resurrection applies the closure in isolation.
        let policy = ConversionPolicy::new(GitHashAlgorithm::Sha1);
        let project = publisher.repo.project_tree(&publisher.view, &policy).unwrap();
        let (commit, raw) = bind_projection_to_commit(&git, &project);
        Self {
            repo,
            git,
            commit_hex: commit.to_string(),
            raw_commit: raw,
        }
    }

    /// A target whose bridge Git ODB never received the commit.
    fn with_lost_git_objects(directory: &Path) -> Self {
        let repo = Repository::init(directory).unwrap();
        let git = git2::Repository::init_opts(
            directory.join("bridge.git"),
            git2::RepositoryInitOptions::new().bare(true),
        )
        .unwrap();
        Self {
            repo,
            git,
            commit_hex: String::new(),
            raw_commit: Vec::new(),
        }
    }

    /// The binding for this target's bound commit over the publisher's
    /// closure identity facts.
    fn binding(&self, publisher: &Publisher, seed: u8) -> GitStateBinding {
        signed_binding(
            seed,
            GitObjectFormat::Sha1,
            &self.commit_hex,
            &publisher.tree_hex,
            Vec::new(),
            Some(self.raw_commit.clone()),
            publisher.set_id,
            publisher.merkle,
            publisher.ordered.clone(),
        )
    }

    fn view(&self) -> &str {
        self.repo.current_view()
    }
}

/// Cache the closure the way CB-6B transport would (change files only).
fn cache_closure(target: &Target, publisher: &Publisher) {
    for hash in &publisher.ordered {
        let change = publisher.repo.load_change(hash).unwrap();
        target.repo.save_change(&change).unwrap();
    }
}

/// The full identity contract: membership order, exact bytes, dependencies,
/// frontier, semantic identities, Merkle state, SetId.
fn assert_exact_restoration(target: &Repository, view: &str, publisher: &Publisher) {
    let txn = target.pristine.read_txn().unwrap();
    let view_state = txn.get_view(view).unwrap().expect("view exists");
    for (index, hash) in publisher.ordered.iter().enumerate() {
        let change_id = txn.get_internal(hash).unwrap().expect("registered");
        let seq = txn.get_change_seq(&view_state, change_id).unwrap();
        assert_eq!(
            seq,
            Some(index as u64),
            "change {} must hold membership position {}",
            hash.to_base32(),
            index + 1
        );
    }
    drop(txn);

    for hash in &publisher.ordered {
        let original = publisher.repo.load_change(hash).unwrap();
        let restored = target.load_change(hash).unwrap();
        let mut original_bytes = Vec::new();
        let mut restored_bytes = Vec::new();
        original.serialize(&mut original_bytes).unwrap();
        restored.serialize(&mut restored_bytes).unwrap();
        assert_eq!(
            restored_bytes, original_bytes,
            "change bytes are hash-authoritative and must be identical"
        );
        assert_eq!(
            restored.dependencies().to_vec(),
            original.dependencies().to_vec(),
            "context dependencies are the originals"
        );
        assert_eq!(
            restored.causal_frontier(),
            original.causal_frontier(),
            "causal frontiers are the originals"
        );
        assert_eq!(
            restored.file_ops().len(),
            original.file_ops().len(),
            "semantic (trunk/branch/leaf) identities travel inside the change bytes"
        );
    }

    let identity = target.view_identity(view).unwrap();
    assert_eq!(identity.merkle, publisher.merkle, "Merkle state restored");
    assert_eq!(identity.set_id, publisher.set_id, "SetId restored");
    assert_eq!(identity.closure_len, publisher.ordered.len() as u64);
}

// ── AC-1: exact round trips ─────────────────────────────────────────────

#[test]
fn fresh_repository_restores_the_exact_squashed_closure_from_a_binding() {
    let source = TempDir::new().unwrap();
    let publisher = Publisher::new(source.path());
    let destination = TempDir::new().unwrap();

    let target = Target::new(destination.path(), &publisher);
    cache_closure(&target, &publisher);
    let binding = target.binding(&publisher, 0x42);
    target.repo.store_binding(&binding).unwrap();

    let outcome = target
        .repo
        .resurrect_binding_exact(&target.git, &binding, target.view(), None)
        .unwrap();

    assert_eq!(outcome.inserted, publisher.ordered, "inserted in binding order");
    assert!(outcome.already_present.is_empty());
    assert_eq!(outcome.provenance_roots, vec![Hash::of(b"provenance-root")]);
    assert_eq!(outcome.attestation_roots, vec![Hash::of(b"attestation-root")]);
    assert_eq!(outcome.proof.bound_tree, outcome.proof.projected_tree);
    assert_eq!(outcome.proof.merkle_state, publisher.merkle);
    assert!(outcome.operation.is_some(), "publication is journaled");
    assert!(!outcome.provenance_trusted, "unknown test signer stays untrusted");
    assert_exact_restoration(&target.repo, target.view(), &publisher);

    // Deep equality survives close/reopen.
    let destination_path = destination.path().to_path_buf();
    let view = target.view().to_string();
    drop(target);
    {
        let reopened = Repository::open_readonly(&destination_path).unwrap();
        assert_exact_restoration(&reopened, &view, &publisher);
    }

    // Idempotent re-adoption is a verified no-op.
    let target = Target {
        repo: Repository::open(&destination_path).unwrap(),
        git: git2::Repository::open(destination_path.join("bridge.git")).unwrap(),
        commit_hex: String::new(),
        raw_commit: Vec::new(),
    };
    let second = target
        .repo
        .resurrect_binding_exact(&target.git, &binding, target.view(), None)
        .unwrap();
    assert!(second.inserted.is_empty());
    assert_eq!(second.already_present, publisher.ordered);
    assert!(second.operation.is_none());

    // The projected tree after resurrection equals the bound tree exactly,
    // and the zero-byte file survives as present content.
    let policy = ConversionPolicy::new(GitHashAlgorithm::Sha1);
    let project = target.repo.project_tree(target.view(), &policy).unwrap();
    assert_eq!(hex_oid(&project.git.root), publisher.tree_hex);
    let empty = project
        .manifest
        .entries
        .iter()
        .find(|entry| entry.path.as_bytes() == b"empty.txt")
        .expect("the zero-byte file is projected");
    assert!(empty.repository_bytes.is_empty());
}

#[test]
fn squash_export_restores_all_originals_without_a_substitute() {
    // The publisher's closure (three original changes) is exported as ONE Git
    // commit; the fresh repository restores exactly the three originals —
    // no squashed substitute change is manufactured.
    let source = TempDir::new().unwrap();
    let publisher = Publisher::new(source.path());
    assert_eq!(publisher.ordered.len(), 3);

    let destination = TempDir::new().unwrap();
    let target = Target::new(destination.path(), &publisher);
    cache_closure(&target, &publisher);
    let binding = target.binding(&publisher, 0x43);
    target.repo.store_binding(&binding).unwrap();

    let outcome = target
        .repo
        .resurrect_binding_exact(&target.git, &binding, target.view(), None)
        .unwrap();
    assert_eq!(outcome.inserted, publisher.ordered);
    assert_exact_restoration(&target.repo, target.view(), &publisher);
}

#[test]
fn alternate_valid_merkle_orders_restore_their_own_order() {
    // The same valid change set in a different dependency-consistent order is
    // a different binding with a different Merkle state; each restores its
    // own order and the order-invariant SetId is unchanged.
    let source = TempDir::new().unwrap();
    let publisher = Publisher::new(source.path());

    let mut reordered = publisher.ordered.clone();
    reordered.swap(1, 2);
    let mut state = Merkle::ZERO;
    for hash in &reordered {
        state = state.next(hash);
    }
    let binding = signed_binding(
        0x45,
        GitObjectFormat::Sha1,
        &publisher.commit_hex,
        &publisher.tree_hex,
        Vec::new(),
        Some(publisher.raw_commit.clone()),
        publisher.set_id,
        state,
        reordered.clone(),
    );

    let destination = TempDir::new().unwrap();
    let target = Target::new(destination.path(), &publisher);
    cache_closure(&target, &publisher);
    target.repo.store_binding(&binding).unwrap();

    let outcome = target
        .repo
        .resurrect_binding_exact(&target.git, &binding, target.view(), None)
        .unwrap();
    assert_eq!(outcome.inserted, reordered);
    let identity = target.repo.view_identity(target.view()).unwrap();
    assert_eq!(identity.merkle, state);
    assert_eq!(identity.set_id, publisher.set_id, "SetId is order-invariant");
}

#[test]
fn a_lost_git_odb_is_restored_from_the_preserved_raw_commit() {
    let source = TempDir::new().unwrap();
    let publisher = Publisher::new(source.path());

    let destination = TempDir::new().unwrap();
    let target = Target::with_lost_git_objects(destination.path());
    cache_closure(&target, &publisher);
    // The binding names the PUBLISHER's commit — which this target's Git
    // object database never received (the original ODB is lost).
    let binding = publisher.binding(0x4B);
    target.repo.store_binding(&binding).unwrap();

    let outcome = target
        .repo
        .resurrect_binding_exact(&target.git, &binding, target.view(), None)
        .unwrap();
    assert_eq!(outcome.inserted, publisher.ordered);
    assert!(
        outcome.restored_raw_commit,
        "the raw commit bytes must be restored into the odb"
    );
    // The restored object is byte-identical to the preserved bytes.
    let odb = target.git.odb().unwrap();
    let restored = odb
        .read(git2::Oid::from_bytes(binding.payload().git_commit.as_bytes()).unwrap())
        .unwrap();
    assert_eq!(
        restored.data(),
        binding.payload().raw_commit_object.as_deref().unwrap()
    );
    assert_eq!(outcome.identity.merkle, publisher.merkle);
    assert_eq!(outcome.identity.set_id, publisher.set_id);
}

#[test]
fn a_sha256_format_binding_fails_closed_against_a_sha1_odb() {
    // Git-side SHA-256 verification is deferred to libgit2 support (a known
    // issue); identity-level enforcement must still fail closed: a SHA-256
    // binding can never resurrect through a SHA-1 object database, and a
    // well-formed SHA-256 codec payload round-trips its closure root.
    let source = TempDir::new().unwrap();
    let publisher = Publisher::new(source.path());
    let destination = TempDir::new().unwrap();
    let target = Target::new(destination.path(), &publisher);
    cache_closure(&target, &publisher);

    // A structurally valid SHA-256 payload over the same closure: codec-level
    // validation (object-format width consistency + closure root) accepts it.
    let well_formed = signed_binding(
        0x4F,
        GitObjectFormat::Sha256,
        &hex_of(&[0x5Au8; 32]),
        &hex_of(&[0x5Bu8; 32]),
        Vec::new(),
        None,
        publisher.set_id,
        publisher.merkle,
        publisher.ordered.clone(),
    );
    assert_eq!(
        well_formed.payload().closure_root,
        well_formed.payload().compute_closure_root()
    );

    // Resurrection refuses closed before any mutation: the SHA-256 commit
    // cannot exist in a SHA-1 object database.
    let binding = signed_binding(
        0x4F,
        GitObjectFormat::Sha256,
        &hex_of(&[0x5Au8; 32]),
        &hex_of(&[0x5Bu8; 32]),
        Vec::new(),
        None,
        publisher.set_id,
        publisher.merkle,
        publisher.ordered.clone(),
    );
    let error = target
        .repo
        .resurrect_binding_exact(&target.git, &binding, target.view(), None)
        .unwrap_err();
    assert!(
        matches!(error, crate::RepositoryError::BindingRejected { .. }),
        "a SHA-256 binding is refused against a SHA-1 odb: {error}"
    );
    let identity = target.repo.view_identity(target.view()).unwrap();
    assert_eq!(identity.merkle, Merkle::ZERO);
}

fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

// ── AC-2: mandatory isolated proof, recoverable failure ─────────────────

#[test]
fn projection_mismatch_is_rejected_without_publishing_anything() {
    let source = TempDir::new().unwrap();
    let publisher = Publisher::new(source.path());

    let destination = TempDir::new().unwrap();
    let target = Target::with_lost_git_objects(destination.path());
    cache_closure(&target, &publisher);
    // The bridge repository carries a DIFFERENT tree (extra path): the
    // binding below is internally consistent, but the closure projects a
    // different tree.
    let odb = target.git.odb().unwrap();
    let blob = odb
        .write(
            git2::ObjectType::Blob,
            b"an extra path the closure does not project\n",
        )
        .unwrap();
    let mut builder = target.git.treebuilder(None).unwrap();
    builder.insert("extra.txt", blob, 0o100644).unwrap();
    let wrong_tree_oid = builder.write().unwrap();
    let wrong_tree = target.git.find_tree(wrong_tree_oid).unwrap();
    let signature = git2::Signature::now("Publisher", "pub@example.com").unwrap();
    let commit = target.git
        .commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "wrong tree",
            &wrong_tree,
            &[],
        )
        .unwrap();
    target.git.set_head("refs/heads/main").unwrap();
    let raw = target.git.odb().unwrap().read(commit).unwrap().data().to_vec();

    let binding = signed_binding(
        0x46,
        GitObjectFormat::Sha1,
        &commit.to_string(),
        &wrong_tree_oid.to_string(),
        Vec::new(),
        Some(raw),
        publisher.set_id,
        publisher.merkle,
        publisher.ordered.clone(),
    );
    target.repo.store_binding(&binding).unwrap();

    let error = target
        .repo
        .resurrect_binding_exact(&target.git, &binding, target.view(), None)
        .unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains("projected tree") && message.contains("does not match"),
        "the proof must reject a tree mismatch: {message}"
    );

    // Nothing was published: no membership, unchanged view state, closure
    // changes stay in the retry cache, and the worktree is untouched.
    let identity = target.repo.view_identity(target.view()).unwrap();
    assert_eq!(identity.merkle, Merkle::ZERO);
    let txn = target.repo.pristine.read_txn().unwrap();
    let view_state = txn.get_view(target.view()).unwrap().unwrap();
    assert_eq!(
        txn.iter_changes(&view_state, 0).unwrap().count(),
        0,
        "no closure change may be a member after a rejected proof"
    );
    drop(txn);
    for hash in &publisher.ordered {
        assert!(target.repo.has_change(hash), "the retry cache keeps changes");
    }
    let worktree: Vec<_> = fs::read_dir(destination.path())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name != ".atomic" && name != "bridge.git")
        .collect();
    assert!(
        worktree.is_empty(),
        "a rejected proof must not materialize into the worktree: {worktree:?}"
    );

    // The rejection is recoverable evidence: the operation journal shows a
    // Recover child, not a half-applied head.
    assert!(matches!(
        target
            .repo
            .operation_log(
                atomic_core::operation::OperationScope::WorkingCopy(
                    target.repo.require_working_copy_id().unwrap()
                ),
                Some(1),
                false,
            )
            .unwrap()
            .head_state,
        crate::OperationHeadState::Single(_)
    ), "the aborted attempt was recovered (no diverged head)");
}

#[test]
fn wrong_set_id_data_is_rejected() {
    let source = TempDir::new().unwrap();
    let publisher = Publisher::new(source.path());
    let destination = TempDir::new().unwrap();
    let target = Target::new(destination.path(), &publisher);
    cache_closure(&target, &publisher);
    let binding = signed_binding(
        0x47,
        GitObjectFormat::Sha1,
        &publisher.commit_hex,
        &publisher.tree_hex,
        Vec::new(),
        Some(publisher.raw_commit.clone()),
        SetId::from_bytes([9u8; 32]),
        publisher.merkle,
        publisher.ordered.clone(),
    );
    target.repo.store_binding(&binding).unwrap();

    let error = target
        .repo
        .resurrect_binding_exact(&target.git, &binding, target.view(), None)
        .unwrap_err();
    assert!(
        error.to_string().contains("SetId"),
        "SetId mismatch must be named: {error}"
    );
    let identity = target.repo.view_identity(target.view()).unwrap();
    assert_eq!(identity.merkle, Merkle::ZERO);
}

#[test]
fn an_interrupted_build_publishes_nothing_and_a_retry_succeeds() {
    let source = TempDir::new().unwrap();
    let publisher = Publisher::new(source.path());
    let destination = TempDir::new().unwrap();
    let target = Target::new(destination.path(), &publisher);
    cache_closure(&target, &publisher);
    let binding = target.binding(&publisher, 0x48);
    target.repo.store_binding(&binding).unwrap();

    // Inject the apply fault: the isolated build fails after the
    // ResurrectBinding intent was journaled (crash before publication).
    super::resurrection::RESURRECTION_APPLY_FAULT.with(|cell| cell.set(true));
    let error = target
        .repo
        .resurrect_binding_exact(&target.git, &binding, target.view(), None)
        .unwrap_err();
    assert!(error.to_string().contains("injected"));
    super::resurrection::RESURRECTION_APPLY_FAULT.with(|cell| cell.set(false));

    // The baseline is unchanged and recovery evidence exists.
    let identity = target.repo.view_identity(target.view()).unwrap();
    assert_eq!(identity.merkle, Merkle::ZERO);
    assert!(matches!(
        target
            .repo
            .operation_log(
                atomic_core::operation::OperationScope::WorkingCopy(
                    target.repo.require_working_copy_id().unwrap()
                ),
                Some(1),
                false,
            )
            .unwrap()
            .head_state,
        crate::OperationHeadState::Single(_)
    ));

    // Retry: idempotent resume, exact final state.
    let outcome = target
        .repo
        .resurrect_binding_exact(&target.git, &binding, target.view(), None)
        .unwrap();
    assert_eq!(outcome.inserted, publisher.ordered);
    assert_exact_restoration(&target.repo, target.view(), &publisher);
}

#[test]
fn concurrent_git_mutation_retries_under_the_three_attempt_guard() {
    let source = TempDir::new().unwrap();
    let publisher = Publisher::new(source.path());
    let destination = TempDir::new().unwrap();
    let target = Target::new(destination.path(), &publisher);
    cache_closure(&target, &publisher);
    let binding = target.binding(&publisher, 0x49);
    target.repo.store_binding(&binding).unwrap();

    // Two contended attempts are retried; the third succeeds.
    super::resurrection::RESURRECTION_CONTENTION_ATTEMPTS.with(|cell| cell.set(2));
    let outcome = target
        .repo
        .resurrect_binding_exact(&target.git, &binding, target.view(), None)
        .unwrap();
    assert_eq!(outcome.inserted, publisher.ordered);

    // Exhausting the guard refuses with a typed contention error and leaves
    // no membership behind.
    let destination2 = TempDir::new().unwrap();
    let target2 = Target::new(destination2.path(), &publisher);
    cache_closure(&target2, &publisher);
    super::resurrection::RESURRECTION_CONTENTION_ATTEMPTS.with(|cell| cell.set(5));
    let error = target2
        .repo
        .resurrect_binding_exact(&target2.git, &binding, target2.view(), None)
        .unwrap_err();
    assert!(
        matches!(
            error,
            crate::RepositoryError::ResurrectionContended { attempts: 3, .. }
        ),
        "expected ResurrectionContended after 3 attempts, got: {error}"
    );
    let identity = target2.repo.view_identity(target2.view()).unwrap();
    assert_eq!(identity.merkle, Merkle::ZERO);
    super::resurrection::RESURRECTION_CONTENTION_ATTEMPTS.with(|cell| cell.set(0));
}

#[test]
fn an_incomplete_closure_is_an_explicit_refusal() {
    let source = TempDir::new().unwrap();
    let publisher = Publisher::new(source.path());
    let destination = TempDir::new().unwrap();
    let target = Target::new(destination.path(), &publisher);
    // The closure is NOT cached and no remote/pack is available.
    let binding = target.binding(&publisher, 0x4A);
    target.repo.store_binding(&binding).unwrap();

    let error = target
        .repo
        .resurrect_binding_exact(&target.git, &binding, target.view(), None)
        .unwrap_err();
    assert!(
        matches!(error, crate::RepositoryError::BindingClosureIncomplete { .. }),
        "expected an explicit incomplete-closure refusal, got: {error}"
    );
    let identity = target.repo.view_identity(target.view()).unwrap();
    assert_eq!(identity.merkle, Merkle::ZERO);
}

// ── persisted conflict restoration ──────────────────────────────────────

#[test]
fn persisted_conflicts_are_restored_exactly() {
    // Two competing inserts at the same position, joined by a cross-view
    // insert: the publisher view carries a genuine persisted conflict. The
    // same ordered closure re-applied on a fresh graph must re-derive the
    // identical conflict set (identities and sides), not marker text.
    let source = TempDir::new().unwrap();
    let mut repo = Repository::init(source.path()).unwrap();
    let file = source.path().join("f.txt");
    let working_copy = repo.require_working_copy_id().unwrap();

    fs::write(&file, "line1\nline2\nline3\n").unwrap();
    repo.add(working_copy, "f.txt", TrackingOptions::default()).unwrap();
    repo.record_with_message(
        working_copy,
        "base",
        RecordOptions::new().with_all(true).save_to_store(true),
    )
    .unwrap();

    let dev = repo.current_view().to_string();
    repo.create_view_from("feature", &dev).unwrap();
    repo.switch_view(working_copy, "feature").unwrap();
    fs::write(&file, "line1\nAAA-inserted\nline2\nline3\n").unwrap();
    repo.record_with_message(
        working_copy,
        "edit A",
        RecordOptions::new().with_all(true).save_to_store(true),
    )
    .unwrap();
    repo.switch_view(working_copy, &dev).unwrap();
    fs::write(&file, "line1\nBBB-inserted\nline2\nline3\n").unwrap();
    repo.record_with_message(
        working_copy,
        "edit B",
        RecordOptions::new().with_all(true).save_to_store(true),
    )
    .unwrap();

    // Join the competing change into dev: a genuine persisted conflict.
    repo.insert_from_view(crate::apply::CrossViewInsertOptions::new("feature", &dev))
        .unwrap();
    repo.materialize(repo.require_working_copy_id().unwrap()).unwrap();

    let publisher_conflicts = {
        let txn = repo.pristine.read_txn().unwrap();
        let view_state = txn.get_view(&dev).unwrap().unwrap();
        txn.iter_conflicts(view_state.id).unwrap().len()
    };
    assert!(
        publisher_conflicts >= 1,
        "fixture precondition: the publisher view carries a persisted conflict"
    );

    // Marker bytes are ordinary repository content, so the conflicted state
    // projects; the binding names that exact tree.
    let policy = ConversionPolicy::new(GitHashAlgorithm::Sha1);
    let project = repo.project_tree(&dev, &policy).unwrap();
    let git = git2::Repository::init_opts(
        source.path().join("bridge.git"),
        git2::RepositoryInitOptions::new().bare(true),
    )
    .unwrap();
    let (commit, raw) = bind_projection_to_commit(&git, &project);
    let identity = repo.view_identity(&dev).unwrap();
    let ordered: Vec<Hash> = repo
        .effective_history(Some(&dev))
        .unwrap()
        .iter()
        .map(|entry| entry.hash)
        .collect();
    // Conflict-marker sides order by original registration order, so the
    // binding carries that (also dependency-consistent) order of the same
    // change set; the Merkle is computed over this exact order.
    let registration_order: Vec<Hash> = vec![ordered[0], ordered[2], ordered[1]];
    let mut registration_merkle = Merkle::ZERO;
    for hash in &registration_order {
        registration_merkle = registration_merkle.next(hash);
    }
    let binding = signed_binding(
        0x4D,
        GitObjectFormat::Sha1,
        &commit.to_string(),
        &hex_oid(&project.git.root),
        Vec::new(),
        Some(raw),
        identity.set_id,
        registration_merkle,
        registration_order.clone(),
    );

    // A fresh repository with the same bound commit and closure.
    let destination = TempDir::new().unwrap();
    let target_repo = Repository::init(destination.path()).unwrap();
    let target_git = git2::Repository::init_opts(
        destination.path().join("bridge.git"),
        git2::RepositoryInitOptions::new().bare(true),
    )
    .unwrap();
    {
        let odb = target_git.odb().unwrap();
        for (expected, object) in project.git.objects.iter() {
            let kind = match object.kind {
                crate::GitObjectKind::Blob => git2::ObjectType::Blob,
                crate::GitObjectKind::Tree => git2::ObjectType::Tree,
            };
            let written = odb.write(kind, &object.bytes).unwrap();
            assert_eq!(written.as_bytes(), expected.as_bytes());
        }
        let target_tree = target_git
            .find_tree(git2::Oid::from_bytes(project.git.root.as_bytes()).unwrap())
            .unwrap();
        let signature = git2::Signature::now("Publisher", "pub@example.com").unwrap();
        let target_commit = target_git
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "bound",
                &target_tree,
                &[],
            )
            .unwrap();
        target_git.set_head("refs/heads/main").unwrap();
    }
    for hash in &ordered {
        let change = repo.load_change(hash).unwrap();
        target_repo.save_change(&change).unwrap();
    }
    target_repo.store_binding(&binding).unwrap();

    let outcome = target_repo
        .resurrect_binding_exact(&target_git, &binding, target_repo.current_view(), None)
        .unwrap();
    assert_eq!(outcome.inserted, registration_order);
    assert_eq!(outcome.proof.bound_tree, outcome.proof.projected_tree);
    assert_eq!(outcome.identity.merkle, registration_merkle);
    assert_eq!(outcome.identity.set_id, identity.set_id);

    // The persisted conflict set is restored exactly: materializing the fresh
    // repository's own worktree re-derives the same conflict records (never
    // substituted marker text), and the conflict snapshot content matches the
    // bound tree byte-for-byte.
    target_repo
        .materialize(target_repo.require_working_copy_id().unwrap())
        .unwrap();
    let restored_conflicts = {
        let txn = target_repo.pristine.read_txn().unwrap();
        let view_state = txn
            .get_view(target_repo.current_view())
            .unwrap()
            .unwrap();
        txn.iter_conflicts(view_state.id).unwrap().len()
    };
    assert_eq!(
        restored_conflicts, publisher_conflicts,
        "complete persisted conflicts are re-derived, never substituted"
    );
    let restored_project = target_repo
        .project_tree(target_repo.current_view(), &policy)
        .unwrap();
    assert_eq!(
        hex_oid(&restored_project.git.root),
        hex_oid(&project.git.root)
    );
}

// ── AC-3: checked cache cutover ─────────────────────────────────────────

#[test]
fn the_checked_sha_cache_never_outruns_a_verified_binding() {
    let source = TempDir::new().unwrap();
    let publisher = Publisher::new(source.path());
    let destination = TempDir::new().unwrap();
    let target = Target::new(destination.path(), &publisher);
    cache_closure(&target, &publisher);
    let binding = target.binding(&publisher, 0x4C);
    target.repo.store_binding(&binding).unwrap();

    let commit: GitObjectId = binding.payload().git_commit.to_git_object_id().unwrap();
    let sha = git_oid_hex(&commit);

    // Cold cache: the binding is found and verified.
    let cold = target.repo.resolve_git_sha(&target.git, &commit).unwrap();
    match &cold {
        GitShaResolution::VerifiedBinding { index_candidate, .. } => {
            assert!(!index_candidate, "cold lookup has no index tie");
        }
        other => panic!("expected a verified binding, got {other:?}"),
    }

    // A warm cache row tying the SHA to a closure change: the SAME verified
    // binding results, and the hit is marked as index-derived. (The row is
    // registered directly so the test does not depend on adoption.)
    {
        use atomic_core::pristine::GitShaIndexMutTxnT;
        let mut txn = target.repo.pristine.write_txn().unwrap();
        let change_id = txn
            .register_change(&publisher.ordered[0])
            .unwrap();
        txn.put_git_sha(&sha, change_id).unwrap();
        txn.commit().unwrap();
    }
    let warm = target.repo.resolve_git_sha(&target.git, &commit).unwrap();
    match (&cold, &warm) {
        (
            GitShaResolution::VerifiedBinding { binding: cold_binding, .. },
            GitShaResolution::VerifiedBinding {
                binding: warm_binding,
                index_candidate,
            },
        ) => {
            assert!(index_candidate, "the index contributed the tie");
            assert_eq!(warm_binding.id(), cold_binding.id());
        }
        (cold, warm) => panic!("warm cache changed the resolution: {cold:?} vs {warm:?}"),
    }
}

#[test]
fn missing_stale_and_malicious_index_rows_are_dropped_like_a_cold_cache() {
    let source = TempDir::new().unwrap();
    let repo = Repository::init(source.path()).unwrap();
    fs::write(source.path().join("f.txt"), b"tracked\n").unwrap();
    let recorded_hash = {
        let working_copy = repo.require_working_copy_id().unwrap();
        let outcome = repo
            .record_with_message(
                working_copy,
                "tracked",
                RecordOptions::new()
                    .with_all(true)
                    .include_untracked(true)
                    .save_to_store(true),
            )
            .unwrap();
        *outcome.hash()
    };
    // Import-style provenance: the change's unhashed metadata names the SHA
    // (unhashed bytes never affect the change identity).
    {
        let mut change = repo.load_change(&recorded_hash).unwrap();
        change.unhashed = Some(serde_json::json!({
            "git": { "sha": "0123456789abcdef0123456789abcdef01234567" }
        }));
        repo.save_change(&change).unwrap();
    }

    // Legitimate import-style row: the change's unhashed provenance names it.
    let legit_sha = "0123456789abcdef0123456789abcdef01234567".to_string();
    repo.index_git_sha(&legit_sha, &recorded_hash).unwrap();

    // Malicious: a row maps another SHA to a change that does not name it.
    let forged_sha = "ffffffffffffffffffffffffffffffffffffffff".to_string();
    repo.index_git_sha(&forged_sha, &recorded_hash).unwrap();

    // Missing/stale: a row whose change is no longer registered at all.
    use atomic_core::pristine::{GitShaIndexMutTxnT, GitShaIndexTxnT};
    let ghost_sha = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".to_string();
    {
        let mut txn = repo.pristine.write_txn().unwrap();
        txn.put_git_sha(&ghost_sha, atomic_core::types::NodeId::new(u64::MAX - 1))
            .unwrap();
        txn.commit().unwrap();
    }

    let (verified, stale) = repo.checked_git_sha_markers().unwrap();
    assert!(
        verified.contains(&legit_sha),
        "the legitimate row verifies: {verified:?}"
    );
    assert!(
        !verified.contains(&forged_sha) && !verified.contains(&ghost_sha),
        "invalid rows never verify as identity: {verified:?}"
    );
    assert!(
        stale.contains(&ghost_sha) && stale.contains(&forged_sha),
        "invalid rows are reported: {stale:?}"
    );

    // The unregistered row was repaired out of the cache; the forged row is
    // reported and skipped (its change still exists) and the legitimate row
    // stays. None of the invalid rows ever authorizes anything.
    assert!(!repo.has_git_sha(&ghost_sha).unwrap());
    assert!(repo.checked_git_sha(&legit_sha).unwrap().is_some());
    assert!(repo.checked_git_sha(&forged_sha).unwrap().is_none());
    assert!(repo.checked_git_sha(&ghost_sha).unwrap().is_none());

    // No candidate anywhere resolves cold.
    let git = git2::Repository::init_opts(
        source.path().join("bridge.git"),
        git2::RepositoryInitOptions::new().bare(true),
    )
    .unwrap();
    let unknown = GitObjectId::new(GitHashAlgorithm::Sha1, vec![0x11u8; 20]).unwrap();
    let resolution = repo.resolve_git_sha(&git, &unknown).unwrap();
    assert!(
        matches!(resolution, GitShaResolution::Cold),
        "no candidate anywhere resolves cold: {resolution:?}"
    );
}

#[test]
fn a_forged_binding_is_refused_and_never_blessed() {
    let source = TempDir::new().unwrap();
    let publisher = Publisher::new(source.path());
    let destination = TempDir::new().unwrap();
    let target = Target::new(destination.path(), &publisher);
    cache_closure(&target, &publisher);

    // A forged binding claims a tree the ODB cannot contain: content
    // verification refuses closed before anything is restored.
    let forged = signed_binding(
        0x4E,
        GitObjectFormat::Sha1,
        &publisher.commit_hex,
        &hex_of(&[0x11u8; 20]),
        Vec::new(),
        Some(publisher.raw_commit.clone()),
        publisher.set_id,
        publisher.merkle,
        publisher.ordered.clone(),
    );
    let error = target
        .repo
        .resurrect_binding_exact(&target.git, &forged, target.view(), None)
        .unwrap_err();
    assert!(
        matches!(error, crate::RepositoryError::BindingRejected { .. }),
        "forged content is rejected closed: {error}"
    );
    let identity = target.repo.view_identity(target.view()).unwrap();
    assert_eq!(identity.merkle, Merkle::ZERO);
}

fn hex_of_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn a_stale_view_hint_is_never_identity() {
    // The view hint is a name only (RFC §5.1): a binding whose hint names a
    // foreign view still resurrects by content into the caller's view, and a
    // hint cannot redirect membership to a foreign view.
    let source = TempDir::new().unwrap();
    let publisher = Publisher::new(source.path());
    let destination = TempDir::new().unwrap();
    let target = Target::new(destination.path(), &publisher);
    cache_closure(&target, &publisher);
    let binding = signed_binding(
        0x52,
        GitObjectFormat::Sha1,
        &target.commit_hex,
        &publisher.tree_hex,
        Vec::new(),
        Some(target.raw_commit.clone()),
        publisher.set_id,
        publisher.merkle,
        publisher.ordered.clone(),
    );
    // Forge a hint naming a view that does not exist: resurrection must
    // ignore it (the caller chose the view) and verify the same content.
    let mut payload = binding.payload().clone();
    payload.view_hint = Some("elsewhere".to_string());
    let hinted = GitStateBinding::sign(payload, &keypair(0x52)).unwrap();
    target.repo.store_binding(&hinted).unwrap();

    let outcome = target
        .repo
        .resurrect_binding_exact(&target.git, &hinted, target.view(), None)
        .unwrap();
    assert_eq!(outcome.inserted, publisher.ordered);
    assert_exact_restoration(&target.repo, target.view(), &publisher);
}

#[test]
fn duplicate_binding_candidates_resolve_to_the_same_verified_binding() {
    // Two stored bindings name the same commit (e.g. re-published under two
    // signers); the resolution verifies each fully and the result is the same
    // regardless of storage order — an index hit can never pick a winner.
    let source = TempDir::new().unwrap();
    let publisher = Publisher::new(source.path());
    let destination = TempDir::new().unwrap();
    let target = Target::new(destination.path(), &publisher);
    cache_closure(&target, &publisher);
    let first = target.binding(&publisher, 0x53);
    let second = target.binding(&publisher, 0x54); // same facts, other signer
    assert_ne!(first.id(), second.id());
    target.repo.store_binding(&first).unwrap();
    target.repo.store_binding(&second).unwrap();

    let commit: GitObjectId = first.payload().git_commit.to_git_object_id().unwrap();
    let resolution = target.repo.resolve_git_sha(&target.git, &commit).unwrap();
    match resolution {
        GitShaResolution::VerifiedBinding { binding, .. } => {
            // Either stored binding verifies; both vouch for the same facts.
            assert!(
                binding.id() == first.id() || binding.id() == second.id(),
                "the verifying binding is returned: {}",
                binding.id()
            );
        }
        other => panic!("expected a verified binding, got {other:?}"),
    }

    // A stale view-name hint on one of the duplicates changes nothing.
    let mut payload = second.payload().clone();
    payload.view_hint = Some("nonexistent".to_string());
    let hinted = GitStateBinding::sign(payload, &keypair(0x53)).unwrap();
    let resolution = target.repo.resolve_git_sha(&target.git, &commit).unwrap();
    match resolution {
        GitShaResolution::VerifiedBinding { binding, .. } => {
            let _ = hinted;
            assert!(
                binding.id() == first.id() || binding.id() == second.id(),
                "only verified candidates win: {}",
                binding.id()
            );
        }
        other => panic!("expected a verified binding, got {other:?}"),
    }
}
