//! Project-identity transport: adoption after clone/pull.
//!
//! The matrix under test: a remote identity is adopted when the local
//! repository has none, replaces a transport-replaceable `Upgrade` mint (and
//! invalidates the derived KG so it re-projects under the adopted id), is a
//! no-op when identical, and is refused (reported, never overwritten) when it
//! differs from an established `Created`/`Adopted` identity.

use atomic_core::kg::project_id::{MintReason, ProjectIdentityRecord};
use atomic_repository::{IdentityAdoption, Repository};

/// A well-formed remote identity: signed by no one (hash-verifiable), which is
/// exactly what an unsigned origin mint or a pre-verification transport looks
/// like.
fn remote_identity() -> ProjectIdentityRecord {
    atomic_repository::build_project_identity(MintReason::Created, None)
}

#[test]
fn adoption_into_a_repository_without_identity() {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init(directory.path()).unwrap();
    assert!(repo.project_identity().unwrap().is_none());

    let remote = remote_identity();
    assert_eq!(
        repo.adopt_remote_project_identity(&remote).unwrap(),
        IdentityAdoption::Adopted
    );
    assert_eq!(repo.project_identity().unwrap().unwrap().hash, remote.hash);
    assert_eq!(
        repo.project_identity().unwrap().unwrap().mint_reason,
        MintReason::Adopted
    );
}

#[test]
fn adoption_replaces_the_upgrade_mint_and_invalidates_the_derived_kg() {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init(directory.path()).unwrap();
    // Simulate what a full writable open does to an identity-less clone.
    let placeholder = repo.ensure_project_identity(MintReason::Upgrade).unwrap();
    // The derived KG is current for the placeholder id.
    assert!(!repo.rebuild_kg(false).unwrap(), "no rebuild expected");

    let remote = remote_identity();
    assert_eq!(
        repo.adopt_remote_project_identity(&remote).unwrap(),
        IdentityAdoption::ReplacedUpgrade
    );
    assert_eq!(repo.project_identity().unwrap().unwrap().hash, remote.hash);

    // The KG generation was reset: the next rebuild pass must actually run
    // (project-qualified IRIs changed under the adopted id), and afterwards
    // the graph is current again.
    assert!(
        repo.rebuild_kg(false).unwrap(),
        "adoption must force a rebuild"
    );
    assert!(
        !repo.rebuild_kg(false).unwrap(),
        "second pass must be a no-op"
    );
    let _ = placeholder;
}

#[test]
fn adoption_of_the_same_identity_is_a_noop() {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init(directory.path()).unwrap();
    let remote = remote_identity();
    repo.adopt_remote_project_identity(&remote).unwrap();
    assert_eq!(
        repo.adopt_remote_project_identity(&remote).unwrap(),
        IdentityAdoption::AlreadyCurrent
    );
}

#[test]
fn adoption_never_overwrites_an_established_identity() {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init(directory.path()).unwrap();
    let established = repo.ensure_project_identity(MintReason::Created).unwrap();

    let remote = remote_identity();
    assert_ne!(remote.hash, established.hash);
    assert_eq!(
        repo.adopt_remote_project_identity(&remote).unwrap(),
        IdentityAdoption::Diverged
    );
    // The local identity is untouched.
    assert_eq!(
        repo.project_identity().unwrap().unwrap().hash,
        established.hash
    );

    // And re-presenting the established identity is AlreadyCurrent, not
    // Diverged (a second clone of the same origin converges).
    assert_eq!(
        repo.adopt_remote_project_identity(&established).unwrap(),
        IdentityAdoption::AlreadyCurrent
    );
}

#[test]
fn adoption_refuses_a_document_whose_hash_does_not_match() {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init(directory.path()).unwrap();
    let mut tampered = remote_identity();
    let mut object: serde_json::Value = serde_json::from_slice(&tampered.object.clone()).unwrap();
    object["atomic:projectId"] =
        serde_json::json!("BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB");
    tampered.object = serde_json::to_vec(&object).unwrap();
    assert!(repo.adopt_remote_project_identity(&tampered).is_err());
    assert!(repo.project_identity().unwrap().is_none());
}

// ── Explicit remediation (`repo identity adopt` / `repo identity fork`) ─────

#[test]
fn force_adopt_replaces_an_established_identity_and_invalidates_the_derived_kg() {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init(directory.path()).unwrap();
    // An ESTABLISHED local identity (Created) — the case plain adoption
    // refuses.
    let established = repo.ensure_project_identity(MintReason::Created).unwrap();
    assert!(!repo.rebuild_kg(false).unwrap());

    let remote = remote_identity();
    assert_eq!(
        repo.adopt_remote_project_identity(&remote).unwrap(),
        IdentityAdoption::Diverged,
        "plain adoption must still refuse"
    );

    let adopted = repo.force_adopt_remote_identity(&remote).unwrap();
    assert_eq!(adopted.mint_reason, MintReason::Adopted);
    assert_eq!(repo.project_identity().unwrap().unwrap().hash, remote.hash);
    // The derived graph re-projects under the adopted id.
    assert!(
        repo.rebuild_kg(false).unwrap(),
        "force adopt must force a rebuild"
    );
    assert!(!repo.rebuild_kg(false).unwrap());
    let _ = established;
}

#[test]
fn force_adopt_still_verifies_the_document() {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init(directory.path()).unwrap();
    repo.ensure_project_identity(MintReason::Created).unwrap();
    let mut tampered = remote_identity();
    let mut object: serde_json::Value = serde_json::from_slice(&tampered.object).unwrap();
    object["dcterms:created"] = serde_json::json!("2031-01-01T00:00:00Z");
    tampered.object = serde_json::to_vec(&object).unwrap();
    assert!(repo.force_adopt_remote_identity(&tampered).is_err());
}

#[test]
fn fork_mints_a_fresh_distinct_identity_and_abandons_the_old_lineage() {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init(directory.path()).unwrap();
    let before = repo.ensure_project_identity(MintReason::Created).unwrap();

    let forked = repo.fork_identity().unwrap();
    assert_ne!(forked.hash, before.hash);
    assert_eq!(repo.project_identity().unwrap().unwrap().hash, forked.hash);
    // A second fork mints yet another lineage (idempotence is NOT a property
    // of fork — every call is a new project).
    let again = repo.fork_identity().unwrap();
    assert_ne!(again.hash, forked.hash);
    // A forked lineage is ESTABLISHED: plain adoption of the abandoned
    // lineage refuses (Diverged) — taking it back is an explicit
    // force-adopt, same as any other divergence remediation.
    assert_eq!(
        repo.adopt_remote_project_identity(&before).unwrap(),
        IdentityAdoption::Diverged
    );
    assert_eq!(
        repo.force_adopt_remote_identity(&before).unwrap().hash,
        before.hash
    );
}
