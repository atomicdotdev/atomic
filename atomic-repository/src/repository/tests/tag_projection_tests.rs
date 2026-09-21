//! CB-8A tag projection tests (RFC §8.4, tracker CB-8A ac-4).
//!
//! Real git2 fixtures, real bindings, real Git tag objects:
//! - an annotated round trip carries the `atomic-binding` trailer and a fresh
//!   store restores the exact bound Merkle state;
//! - a lightweight Git tag imports as an Atomic state tag;
//! - ReviewGate/aggregate tags stay Atomic-only and create no Git tag;
//! - wrong (forged trailer) and missing (unbound commit/state) bindings are
//!   rejected on both directions;
//! - tag export/import never moves Git HEAD, the Git index, or the Atomic
//!   view state, and the Git tag object carries no private metadata.

use atomic_core::pristine::TagKind;
use atomic_core::types::{Merkle, OperationId, SetId};
use atomic_core::Hash;

use crate::git_binding::{
    BindingSigner, CausalOrigin, GitObjectFormat, GitOid, GitStateBinding, GitStateBindingPayload,
    BINDING_VERSION,
};
use atomic_identity::keypair::{KeyPair, SecretKey};

use super::*;

fn test_keypair(seed: u8) -> KeyPair {
    let mut secret = [0u8; 32];
    for (index, byte) in secret.iter_mut().enumerate() {
        *byte = seed.wrapping_add((index as u8) * 7 + 11);
    }
    KeyPair::from_secret_key(SecretKey::from_bytes(&secret))
}

/// A colocated Git + Atomic fixture: one real Git commit, one Atomic state
/// tag `v1` on view `dev` pinned to the view state, and (optionally) a fully
/// verified Git-state binding whose Merkle state is the tag's state.
struct TagFixture {
    _dir: tempfile::TempDir,
    repo: TestRepository,
    git: git2::Repository,
    commit_oid: git2::Oid,
    binding: GitStateBinding,
    tag_state: Merkle,
}

fn tag_fixture(seed: u8, store_binding: bool) -> TagFixture {
    let dir = tempfile::TempDir::new().unwrap();
    let git = git2::Repository::init(dir.path()).unwrap();

    // A real commit in the live Git object database.
    let blob = git.blob(b"tagged content\n").unwrap();
    let (tree_oid, commit_oid) = {
        let mut builder = git.treebuilder(None).unwrap();
        builder.insert("file.txt", blob, 0o100644).unwrap();
        let tree_oid = builder.write().unwrap();
        let tree = git.find_tree(tree_oid).unwrap();
        let signature = git2::Signature::now("CB-8A", "cb8a-tag@example.com").unwrap();
        let commit_oid = git
            .commit(None, &signature, &signature, "bound commit", &tree, &[])
            .unwrap();
        (tree_oid, commit_oid)
    };
    let raw_commit = git
        .odb()
        .unwrap()
        .read(commit_oid)
        .unwrap()
        .data()
        .to_vec();

    let repo = TestRepository::new(Repository::init(dir.path()).unwrap());
    let tag = repo
        .create_tag_on_view("dev", "v1", Some("Release version 1.0.0"), TagKind::Release)
        .unwrap();
    let tag_state = tag.state;

    let keypair = test_keypair(seed);
    let mut payload = GitStateBindingPayload {
        version: BINDING_VERSION,
        git_object_format: GitObjectFormat::Sha1,
        git_commit: GitOid::from_hex(&commit_oid.to_string()).unwrap(),
        git_tree: GitOid::from_hex(&tree_oid.to_string()).unwrap(),
        git_parents: Vec::new(),
        raw_commit_object: Some(raw_commit),
        set_id: SetId::from_bytes([seed; 32]),
        merkle_state: tag_state,
        view_hint: Some("dev".to_string()),
        ordered_changes: vec![Hash::from_bytes([seed.wrapping_add(9); 32])],
        closure_root: Hash::from_bytes([0u8; 32]),
        operation: OperationId::from_bytes([seed.wrapping_add(1); 32]),
        origin: CausalOrigin::ForeignGitRoot,
        loss: Vec::new(),
        provenance_roots: Vec::new(),
        attestation_roots: Vec::new(),
        signer: BindingSigner::for_keypair(&keypair),
    };
    payload.closure_root = payload.compute_closure_root();
    let binding = GitStateBinding::sign(payload, &keypair).unwrap();
    if store_binding {
        repo.store_binding(&binding).unwrap();
    }

    TagFixture {
        _dir: dir,
        repo,
        git,
        commit_oid,
        binding,
        tag_state,
    }
}

/// A fresh Atomic store that has fetched the same binding (e.g. through the
/// binding transport) but has never seen the tag itself.
fn fresh_store_with_binding(binding: &GitStateBinding) -> (tempfile::TempDir, TestRepository) {
    let dir = tempfile::TempDir::new().unwrap();
    let repo = TestRepository::new(Repository::init(dir.path()).unwrap());
    repo.store_binding(binding).unwrap();
    (dir, repo)
}

// ── Annotated round trip restores the bound state (ac-4) ─────────────────

#[test]
fn annotated_tag_round_trip_carries_the_binding_and_restores_the_bound_state() {
    let fixture = tag_fixture(0x20, true);

    // Export: the Atomic state tag becomes a Git annotated tag whose message
    // carries the immutable binding.
    let outcome = fixture
        .repo
        .export_tag_to_git("dev", "v1", &fixture.git)
        .unwrap();
    let binding_hex = fixture.binding.id().to_hex();
    assert_eq!(
        outcome,
        crate::repository::TagProjectionOutcome::Exported {
            tag_ref: "refs/tags/v1".to_string(),
            target: fixture.commit_oid.to_string(),
            binding_id: binding_hex.clone(),
        },
        "export reports the Git tag, bound commit, and binding"
    );

    let reference = fixture.git.find_reference("refs/tags/v1").unwrap();
    let tag_object = reference.peel(git2::ObjectType::Tag).unwrap();
    let tag_object = tag_object.as_tag().unwrap();
    assert_eq!(
        tag_object.target().unwrap().id(),
        fixture.commit_oid,
        "the annotated tag points at the bound commit"
    );
    let message = tag_object.message().unwrap();
    assert!(
        message.contains("Release version 1.0.0"),
        "the Atomic tag message is preserved: {message}"
    );
    assert!(
        message.contains(&format!("atomic-binding {binding_hex}")),
        "the message carries the atomic-binding trailer: {message}"
    );

    // Fresh store: importing the Git tag restores the exact bound state.
    let (_fresh_dir, fresh) = fresh_store_with_binding(&fixture.binding);
    let restored = fresh
        .import_git_tag_from_git("dev", &fixture.git, "v1")
        .unwrap();
    assert_eq!(restored.name, "v1");
    assert_eq!(restored.view, "dev");
    assert_eq!(
        restored.state, fixture.tag_state,
        "the imported tag pins the same bound Merkle state"
    );
    assert_eq!(
        restored.state,
        fixture.binding.payload().merkle_state,
        "the bound state equals the binding's Merkle state"
    );
    assert_eq!(
        restored.message.as_deref(),
        Some(message),
        "the annotated message round-trips"
    );
    assert!(
        restored
            .metadata
            .as_ref()
            .and_then(|metadata| metadata["git"]["binding"].as_str())
            .is_some_and(|hex| hex == binding_hex),
        "the imported tag records the binding id: {:?}",
        restored.metadata
    );

    // The stored tag in the fresh store is the restored one.
    let stored = fresh.get_tag_from_view("v1", "dev").unwrap().unwrap();
    assert_eq!(stored.state, fixture.tag_state);
}

// ── Lightweight tags import as Atomic tags (ac-4) ────────────────────────

#[test]
fn lightweight_git_tag_imports_as_an_atomic_state_tag() {
    let fixture = tag_fixture(0x30, true);
    // Export the annotated binding carrier first so the fixture mirrors a
    // real bridge; then add a *lightweight* Git tag on the bound commit.
    fixture
        .repo
        .export_tag_to_git("dev", "v1", &fixture.git)
        .unwrap();
    let commit = fixture.git.find_commit(fixture.commit_oid).unwrap();
    fixture
        .git
        .tag_lightweight("plain", commit.as_object(), false)
        .unwrap();

    let imported = fixture
        .repo
        .import_git_tag_from_git("dev", &fixture.git, "plain")
        .unwrap();
    assert_eq!(imported.name, "plain");
    assert!(
        imported.message.is_none(),
        "a lightweight tag carries no annotation: {:?}",
        imported.message
    );
    assert_eq!(
        imported.state, fixture.tag_state,
        "the lightweight tag pins the bound state of the commit it peels to"
    );
    assert_eq!(
        imported
            .metadata
            .as_ref()
            .and_then(|metadata| metadata["git"]["sha"].as_str()),
        Some(fixture.commit_oid.to_string().as_str()),
        "the imported tag records the Git sha"
    );
}

// ── ReviewGate/aggregate tags stay Atomic-only (ac-4) ────────────────────

#[test]
fn review_gate_tags_produce_no_git_tag() {
    let fixture = tag_fixture(0x40, true);
    // An aggregate ReviewGate tag — even with the view state fully bound —
    // is Atomic-only by policy and projects nothing.
    fixture
        .repo
        .create_tag_with_metadata(
            "pr-42-squash",
            Some("Squash merge aggregate"),
            TagKind::ReviewGate,
            None,
        )
        .unwrap();

    let outcome = fixture
        .repo
        .export_tag_to_git("dev", "pr-42-squash", &fixture.git)
        .unwrap();
    assert_eq!(
        outcome,
        crate::repository::TagProjectionOutcome::AtomicOnly { kind: "review-gate" },
        "ReviewGate tags report Atomic-only"
    );
    assert!(
        fixture.git.find_reference("refs/tags/pr-42-squash").is_err(),
        "no Git tag may be created for a ReviewGate tag"
    );
    // The release tag's binding carrier is untouched by the refusal.
    assert!(fixture.git.find_reference("refs/tags/v1").is_err());
}

// ── Wrong/missing bindings are rejected (ac-4) ───────────────────────────

#[test]
fn export_refuses_a_tag_whose_state_has_no_binding() {
    let fixture = tag_fixture(0x50, false);
    let error = fixture
        .repo
        .export_tag_to_git("dev", "v1", &fixture.git)
        .unwrap_err();
    match error {
        RepositoryError::InvalidOperation { message } => {
            assert!(
                message.contains("no verified Git state binding"),
                "an unbound state is refused, never promoted: {message}"
            );
        }
        other => panic!("expected InvalidOperation, got: {other:?}"),
    }
    assert!(
        fixture.git.find_reference("refs/tags/v1").is_err(),
        "a refused export must not publish a Git tag"
    );
}

#[test]
fn import_refuses_a_tag_on_an_unbound_commit() {
    let fixture = tag_fixture(0x60, true);
    // A second commit that no binding vouches for.
    let tree = fixture.git.find_tree({
        let blob = fixture.git.blob(b"unbound\n").unwrap();
        let mut builder = fixture.git.treebuilder(None).unwrap();
        builder.insert("other.txt", blob, 0o100644).unwrap();
        builder.write().unwrap()
    }).unwrap();
    let signature = git2::Signature::now("CB-8A", "cb8a-tag@example.com").unwrap();
    let unbound_oid = fixture
        .git
        .commit(None, &signature, &signature, "unbound commit", &tree, &[])
        .unwrap();
    let commit = fixture.git.find_commit(unbound_oid).unwrap();
    fixture
        .git
        .tag_lightweight("loose", commit.as_object(), false)
        .unwrap();

    let error = fixture
        .repo
        .import_git_tag_from_git("dev", &fixture.git, "loose")
        .unwrap_err();
    match error {
        RepositoryError::InvalidOperation { message } => {
            assert!(
                message.contains("no verified Git state binding"),
                "a missing binding is refused: {message}"
            );
        }
        other => panic!("expected InvalidOperation, got: {other:?}"),
    }
    assert!(
        fixture.repo.get_tag_from_view("loose", "dev").unwrap().is_none(),
        "a refused import must not create an Atomic tag"
    );
}

#[test]
fn import_rejects_an_annotated_tag_claiming_a_wrong_binding() {
    let fixture = tag_fixture(0x70, true);
    // Forge an annotated tag whose message names a different binding.
    let forged_hex = "f".repeat(64);
    let commit = fixture.git.find_commit(fixture.commit_oid).unwrap();
    let signature = git2::Signature::now("CB-8A", "cb8a-tag@example.com").unwrap();
    fixture
        .git
        .tag(
            "forged",
            commit.as_object(),
            &signature,
            &format!("release\n\natomic-binding {forged_hex}\n"),
            false,
        )
        .unwrap();

    let error = fixture
        .repo
        .import_git_tag_from_git("dev", &fixture.git, "forged")
        .unwrap_err();
    match error {
        RepositoryError::InvalidOperation { message } => {
            assert!(
                message.contains("does not match the bound commit"),
                "a forged binding trailer is a rejection, not a hint: {message}"
            );
        }
        other => panic!("expected InvalidOperation, got: {other:?}"),
    }
    assert!(fixture.repo.get_tag_from_view("forged", "dev").unwrap().is_none());
}

// ── Tag operations never touch HEAD, index, or private metadata (ac-4) ───

#[test]
fn tag_projection_never_changes_git_head_index_or_private_metadata() {
    let dir = tempfile::TempDir::new().unwrap();
    let git = git2::Repository::init(dir.path()).unwrap();

    // A real commit reachable from HEAD so HEAD observations are meaningful.
    let blob = git.blob(b"tagged content\n").unwrap();
    let mut builder = git.treebuilder(None).unwrap();
    builder.insert("file.txt", blob, 0o100644).unwrap();
    let tree_oid = builder.write().unwrap();
    let tree = git.find_tree(tree_oid).unwrap();
    let signature = git2::Signature::now("CB-8A", "cb8a-tag@example.com").unwrap();
    let commit_oid = git
        .commit(Some("refs/heads/main"), &signature, &signature, "bound commit", &tree, &[])
        .unwrap();
    git.set_head("refs/heads/main").unwrap();
    let raw_commit = git.odb().unwrap().read(commit_oid).unwrap().data().to_vec();
    // Staged/unstaged noise: an untracked file must survive untouched.
    std::fs::write(dir.path().join("untracked.txt"), b"untracked\n").unwrap();

    let repo = TestRepository::new(Repository::init(dir.path()).unwrap());
    let tag = repo
        .create_tag_on_view("dev", "v1", Some("Release version 1.0.0"), TagKind::Release)
        .unwrap();

    let keypair = test_keypair(0x80);
    let mut payload = GitStateBindingPayload {
        version: BINDING_VERSION,
        git_object_format: GitObjectFormat::Sha1,
        git_commit: GitOid::from_hex(&commit_oid.to_string()).unwrap(),
        git_tree: GitOid::from_hex(&tree_oid.to_string()).unwrap(),
        git_parents: Vec::new(),
        raw_commit_object: Some(raw_commit),
        set_id: SetId::from_bytes([0x80; 32]),
        merkle_state: tag.state,
        view_hint: Some("dev".to_string()),
        ordered_changes: vec![Hash::from_bytes([0x81; 32])],
        closure_root: Hash::from_bytes([0u8; 32]),
        operation: OperationId::from_bytes([0x82; 32]),
        origin: CausalOrigin::ForeignGitRoot,
        loss: Vec::new(),
        provenance_roots: Vec::new(),
        attestation_roots: Vec::new(),
        signer: BindingSigner::for_keypair(&keypair),
    };
    payload.closure_root = payload.compute_closure_root();
    let binding = GitStateBinding::sign(payload, &keypair).unwrap();
    repo.store_binding(&binding).unwrap();

    let head_before = git.head().unwrap().target().unwrap();
    let statuses_before = git
        .statuses(None)
        .unwrap()
        .iter()
        .map(|entry| (entry.path().unwrap_or_default().to_string(), entry.status()))
        .collect::<Vec<_>>();

    // Export + import the tag.
    let outcome = repo.export_tag_to_git("dev", "v1", &git).unwrap();
    assert!(matches!(outcome, crate::repository::TagProjectionOutcome::Exported { .. }));
    let (_fresh_dir, fresh) = fresh_store_with_binding(&binding);
    fresh.import_git_tag_from_git("dev", &git, "v1").unwrap();

    let head_after = git.head().unwrap().target().unwrap();
    assert_eq!(head_before, head_after, "tag projection never moves HEAD");
    assert_eq!(
        head_after,
        commit_oid,
        "HEAD still points at the bound commit"
    );
    let statuses_after = git
        .statuses(None)
        .unwrap()
        .iter()
        .map(|entry| (entry.path().unwrap_or_default().to_string(), entry.status()))
        .collect::<Vec<_>>();
    assert_eq!(
        statuses_before, statuses_after,
        "staged/unstaged state is untouched by tag projection"
    );

    // Privacy: the Git tag object carries only the annotation and the
    // binding id — no provenance roots, transcripts, or raw binding bytes.
    let raw_tag = git
        .odb()
        .unwrap()
        .read(git.find_reference("refs/tags/v1").unwrap().target().unwrap())
        .unwrap()
        .data()
        .to_vec();
    let raw_text = String::from_utf8_lossy(&raw_tag);
    assert!(
        raw_text.contains(&binding.id().to_hex()),
        "the tag message names the binding"
    );
    assert!(
        !raw_text.contains(&hex_encode(binding.signature())),
        "the binding signature never enters the tag object"
    );
    assert!(
        !raw_text.contains(&atomic_core::types::Base32::to_base32(&tag.state)),
        "the Merkle state itself never enters the tag object"
    );
    assert!(
        !raw_text.contains("provenance") && !raw_text.contains("attestation"),
        "private metadata never enters the tag object: {raw_text}"
    );

    // The Atomic view state is untouched by export and import.
    let view_state_after = repo.get_view_info("dev").unwrap().state;
    assert_eq!(
        view_state_after, tag.state,
        "the tagged view state did not move"
    );
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
