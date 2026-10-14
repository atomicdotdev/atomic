//! CB-6A binding verification against a live Git object database (RFC §5.2, 5.6).

use atomic_core::types::OperationId;
use atomic_core::Hash;

use crate::git_binding::{
    evaluate_binding_trust, verify_binding_content, verify_binding_cryptography,
    verify_binding_hint, BindingSigner, BindingVerificationError, BindingVerificationInput,
    CausalOrigin, GitObjectFormat, GitOid, GitStateBinding, GitStateBindingPayload,
};
use atomic_config::{GitTrustConfig, SignerTrust};
use atomic_identity::keypair::{KeyPair, SecretKey};

fn test_keypair(seed: u8) -> KeyPair {
    let mut secret = [0u8; 32];
    for (index, byte) in secret.iter_mut().enumerate() {
        *byte = seed.wrapping_add((index as u8) * 7 + 11);
    }
    KeyPair::from_secret_key(SecretKey::from_bytes(&secret))
}

/// One Git commit plus everything a binding names about it.
struct BoundCommit {
    commit_oid: git2::Oid,
    tree_oid: git2::Oid,
    raw_commit: Vec<u8>,
    git: git2::Repository,
}

/// Create an unreferenced commit with a single file, and read back its raw
/// object bytes exactly as a binding would preserve them.
fn commit_with_tree(
    root: &std::path::Path,
    content: &[u8],
    parents: &[git2::Oid],
    message: &str,
) -> BoundCommit {
    let git = git2::Repository::init(root).expect("init git");
    let (commit_oid, tree_oid, raw_commit) = {
        let blob = git.blob(content).expect("write blob");
        let mut builder = git.treebuilder(None).expect("treebuilder");
        builder
            .insert("file.txt", blob, git2::FileMode::Blob.into())
            .expect("insert");
        let tree_oid = builder.write().expect("write tree");
        let tree = git.find_tree(tree_oid).expect("find tree");
        let signature = git2::Signature::now("Bridge", "bridge@example.com").expect("signature");
        let parents: Vec<git2::Commit> = parents
            .iter()
            .map(|oid| git.find_commit(*oid).expect("parent commit"))
            .collect();
        let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
        let commit_oid = git
            .commit(None, &signature, &signature, message, &tree, &parent_refs)
            .expect("commit");
        let raw_commit = git
            .odb()
            .expect("odb")
            .read(commit_oid)
            .expect("read commit")
            .data()
            .to_vec();
        (commit_oid, tree_oid, raw_commit)
    };
    BoundCommit {
        commit_oid,
        tree_oid,
        raw_commit,
        git,
    }
}

fn binding_for_commit(
    keypair: &KeyPair,
    commit: &BoundCommit,
    mutate: impl FnOnce(&mut GitStateBindingPayload),
) -> GitStateBinding {
    let mut payload = GitStateBindingPayload {
        version: crate::git_binding::BINDING_VERSION,
        git_object_format: GitObjectFormat::Sha1,
        git_commit: GitOid::from_hex(&commit.commit_oid.to_string()).unwrap(),
        git_tree: GitOid::from_hex(&commit.tree_oid.to_string()).unwrap(),
        git_parents: Vec::new(),
        raw_commit_object: Some(commit.raw_commit.clone()),
        set_id: atomic_core::types::SetId::from_bytes([7u8; 32]),
        merkle_state: atomic_core::types::Merkle::from_bytes([8u8; 32]),
        view_hint: Some("main".to_string()),
        ordered_changes: vec![Hash::from_bytes([9u8; 32])],
        closure_root: Hash::from_bytes([0u8; 32]),
        operation: OperationId::from_bytes([10u8; 32]),
        origin: CausalOrigin::ForeignGitRoot,
        loss: Vec::new(),
        provenance_roots: vec![Hash::from_bytes([11u8; 32])],
        attestation_roots: vec![Hash::from_bytes([12u8; 32])],
        signer: BindingSigner::for_keypair(keypair),
    };
    payload.closure_root = payload.compute_closure_root();
    mutate(&mut payload);
    payload.closure_root = payload.compute_closure_root();
    GitStateBinding::sign(payload, keypair).expect("sign")
}

#[test]
fn a_correct_binding_verifies_against_the_real_commit() {
    let temp = tempfile::TempDir::new().unwrap();
    let commit = commit_with_tree(temp.path(), b"contents\n", &[], "bound commit");
    let keypair = test_keypair(1);
    let binding = binding_for_commit(&keypair, &commit, |_| {});

    verify_binding_cryptography(&binding).expect("cryptography verifies");
    verify_binding_content(&commit.git, &binding).expect("content verifies");

    // Hints are optional: a message without an atomic-binding hint is not an
    // error, and a matching hint verifies.
    let message = commit_message_text(&commit.git, &commit.commit_oid);
    verify_binding_hint(&message, &binding).expect("no hint is not an error");
    let hinted_message = format!("subject\n\natomic-binding {}\n", binding.id().to_hex());
    verify_binding_hint(&hinted_message, &binding).expect("matching hint verifies");
}

fn commit_message_text(git: &git2::Repository, oid: &git2::Oid) -> String {
    git.find_commit(*oid)
        .unwrap()
        .message()
        .unwrap()
        .to_string()
}

#[test]
fn a_forged_atomic_binding_hint_fails_closed() {
    let temp = tempfile::TempDir::new().unwrap();
    let commit = commit_with_tree(temp.path(), b"contents\n", &[], "forged claim");
    let keypair = test_keypair(2);
    let binding = binding_for_commit(&keypair, &commit, |_| {});

    // The commit claims a DIFFERENT binding id than the one being verified:
    // a forged hint. Hints route lookup only, but a mismatched hint must
    // never be accepted as evidence for this binding.
    let forged_id = "deadbeef".repeat(8);
    let forged_message = format!("subject\n\natomic-binding {forged_id}\n");
    let error = verify_binding_hint(&forged_message, &binding).unwrap_err();
    assert!(
        matches!(error, BindingVerificationError::HintMismatch { .. }),
        "{error}"
    );
    // And the hint parser only accepts hex hints.
    assert!(atomic_binding_hints_in(&forged_message).len() == 1);
}

fn atomic_binding_hints_in(message: &str) -> Vec<String> {
    let mut hints = Vec::new();
    for line in message.lines() {
        if let Some(rest) = line.trim_start().strip_prefix("atomic-binding ") {
            let value = rest.trim();
            if !value.is_empty() && value.chars().all(|c| c.is_ascii_hexdigit()) {
                hints.push(value.to_ascii_lowercase());
            }
        }
    }
    hints
}

#[test]
fn a_wrong_tree_fails_content_verification() {
    let temp = tempfile::TempDir::new().unwrap();
    let commit = commit_with_tree(temp.path(), b"contents\n", &[], "bound commit");
    let keypair = test_keypair(2);
    let binding = binding_for_commit(&keypair, &commit, |payload| {
        // Claim a different tree than the commit actually has.
        payload.git_tree = GitOid::from_hex(&"a".repeat(40)).unwrap();
    });

    let error = verify_binding_content(&commit.git, &binding).unwrap_err();
    assert!(
        matches!(error, BindingVerificationError::TreeMismatch { .. }),
        "{error}"
    );
}

#[test]
fn complete_ordered_parents_verify_and_wrong_order_fails() {
    let temp = tempfile::TempDir::new().unwrap();
    let git = git2::Repository::init(temp.path()).expect("init git");
    let (parent_one, parent_two, tree_oid) = {
        let blob = git.blob(b"merge child\n").unwrap();
        let mut builder = git.treebuilder(None).unwrap();
        builder
            .insert("file.txt", blob, git2::FileMode::Blob.into())
            .unwrap();
        let tree_oid = builder.write().unwrap();
        let tree = git.find_tree(tree_oid).unwrap();
        let signature = git2::Signature::now("B", "b@example.com").unwrap();
        let parent_one = git
            .commit(None, &signature, &signature, "parent one", &tree, &[])
            .unwrap();
        let parent_two = git
            .commit(None, &signature, &signature, "parent two", &tree, &[])
            .unwrap();
        (parent_one, parent_two, tree_oid)
    };
    let commit = commit_with_tree(
        temp.path(),
        b"merge child\n",
        &[parent_one, parent_two],
        "merge",
    );
    drop(git);
    assert_eq!(commit.tree_oid, tree_oid, "merge reuses the same tree");

    let keypair = test_keypair(3);
    let correct = binding_for_commit(&keypair, &commit, |payload| {
        payload.git_parents = vec![
            GitOid::from_hex(&parent_one.to_string()).unwrap(),
            GitOid::from_hex(&parent_two.to_string()).unwrap(),
        ];
        payload.raw_commit_object = None;
    });
    verify_binding_content(&commit.git, &correct).expect("complete ordered parents verify");

    // Swapped parent order fails: order is part of the binding.
    let swapped = binding_for_commit(&keypair, &commit, |payload| {
        payload.git_parents = vec![
            GitOid::from_hex(&parent_two.to_string()).unwrap(),
            GitOid::from_hex(&parent_one.to_string()).unwrap(),
        ];
        payload.raw_commit_object = None;
    });
    let error = verify_binding_content(&commit.git, &swapped).unwrap_err();
    assert!(
        matches!(error, BindingVerificationError::ParentOrder { .. })
            || matches!(error, BindingVerificationError::ParentMismatch { .. }),
        "{error}"
    );
}

#[test]
fn unknown_and_revoked_signers_supply_content_but_stay_untrusted() {
    let temp = tempfile::TempDir::new().unwrap();
    let commit = commit_with_tree(temp.path(), b"contents\n", &[], "bound commit");

    // The unknown signer is cryptographically valid and the content is
    // byte-correct: recomputation succeeds, but provenance stays untrusted.
    let unknown_keypair = test_keypair(4);
    let binding = binding_for_commit(&unknown_keypair, &commit, |_| {});
    let content_verified = verify_binding_content(&commit.git, &binding).is_ok();
    assert!(
        content_verified,
        "content correctness is signer-independent"
    );

    let repository_identity = "did:atomic:REPOSITORY";
    let policy = GitTrustConfig::default();
    let evaluation = evaluate_binding_trust(
        &binding,
        &policy,
        BindingVerificationInput {
            repository_identity: Some(repository_identity),
        },
        content_verified,
    );
    assert!(evaluation.signature_valid);
    assert_eq!(evaluation.signer_trust, SignerTrust::Unknown);
    assert!(
        evaluation.content_usable(),
        "unknown signers still supply recomputed content"
    );
    assert!(!evaluation.provenance_trusted());
    assert!(
        !evaluation.can_satisfy_publication_gate(),
        "unknown signers can never satisfy a publication gate"
    );

    // A revoked signer stays revoked even if someone lists it as trusted.
    let revoked_policy = GitTrustConfig {
        signers: vec![binding.payload().signer.did.clone()],
        revoked: vec![binding.payload().signer.did.clone()],
    };
    let evaluation = evaluate_binding_trust(
        &binding,
        &revoked_policy,
        BindingVerificationInput {
            repository_identity: Some(repository_identity),
        },
        content_verified,
    );
    assert_eq!(evaluation.signer_trust, SignerTrust::Revoked);
    assert!(!evaluation.can_satisfy_publication_gate());

    // The repository identity with verified content satisfies the gate.
    let repo_keypair = test_keypair(5);
    let trusted_binding = binding_for_commit(&repo_keypair, &commit, |_| {});
    let trusted_did = trusted_binding.payload().signer.did.clone();
    let repo_policy = GitTrustConfig {
        signers: vec![],
        revoked: vec![],
    };
    // The repository identity is the signer here, so evaluation trusts it.
    let evaluation = evaluate_binding_trust(
        &trusted_binding,
        &repo_policy,
        BindingVerificationInput {
            repository_identity: Some(&trusted_did),
        },
        true,
    );
    assert_eq!(evaluation.signer_trust, SignerTrust::Trusted);
    assert!(evaluation.provenance_trusted());
    assert!(evaluation.can_satisfy_publication_gate());
}
