//! CB-6A binding storage and create-only publication tests (RFC §5.1, 7.2, 8.6).

use atomic_core::operation::{GitHashAlgorithm, GitObjectId, GitRefTarget};
use atomic_core::pristine::{BindingStoreOutcome, BindingTxnT};
use atomic_core::types::OperationId;
use atomic_core::Hash;

use crate::OperationHeadState;

use crate::git_binding::{
    BindingSigner, CausalOrigin, GitObjectFormat, GitOid, GitStateBinding, GitStateBindingPayload,
    LossNote, BINDING_VERSION, CLOSURE_ROOT_DOMAIN,
};
use atomic_identity::keypair::{KeyPair, SecretKey};

use super::*;

/// Deterministic test keypair — binding signing keys are created by test
/// code, never by the global identity store.
fn test_keypair(seed: u8) -> KeyPair {
    let mut secret = [0u8; 32];
    for (index, byte) in secret.iter_mut().enumerate() {
        *byte = seed.wrapping_add((index as u8) * 7 + 11);
    }
    KeyPair::from_secret_key(SecretKey::from_bytes(&secret))
}

fn oid_bytes(seed: u8) -> String {
    (0..40)
        .map(|index| format!("{:x}", (seed as usize + index) % 16))
        .collect()
}

/// A minimal well-formed binding for storage/publication tests.
fn sample_binding(keypair: &KeyPair, commit_seed: u8) -> GitStateBinding {
    let commit = GitOid::from_hex(&oid_bytes(commit_seed)).unwrap();
    let changes = vec![atomic_core::Hash::from_bytes([commit_seed; 32])];
    let mut payload = GitStateBindingPayload {
        version: BINDING_VERSION,
        git_object_format: GitObjectFormat::Sha1,
        git_commit: GitOid::from_hex(&oid_bytes(commit_seed)).unwrap(),
        git_tree: GitOid::from_hex(&oid_bytes(commit_seed.wrapping_add(1))).unwrap(),
        git_parents: Vec::new(),
        raw_commit_object: None,
        set_id: atomic_core::types::SetId::from_bytes([commit_seed; 32]),
        merkle_state: atomic_core::types::Merkle::from_bytes([commit_seed.wrapping_add(1); 32]),
        view_hint: Some("main".to_string()),
        ordered_changes: changes.clone(),
        closure_root: {
            let mut hasher = blake3::Hasher::new();
            hasher.update(CLOSURE_ROOT_DOMAIN);
            for change in &changes {
                hasher.update(change.as_bytes());
            }
            atomic_core::Hash::from_bytes(hasher_finalize(&hasher))
        },
        operation: OperationId::from_bytes([commit_seed.wrapping_add(3); 32]),
        origin: CausalOrigin::ForeignGitFirstParent,
        loss: vec![LossNote::Other {
            description: "test loss note".to_string(),
        }],
        provenance_roots: vec![atomic_core::Hash::from_bytes([commit_seed.wrapping_add(5); 32])],
        attestation_roots: Vec::new(),
        signer: BindingSigner::for_keypair(keypair),
    };
    payload.closure_root = payload.compute_closure_root();
    GitStateBinding::sign(payload, keypair).expect("sign test binding")
}

fn hasher_finalize(hasher: &blake3::Hasher) -> [u8; 32] {
    *hasher.finalize().as_bytes()
}

#[test]
fn stored_bindings_are_idempotent_and_survive_reopen() {
    let keypair = test_keypair(1);
    let binding = sample_binding(&keypair, 0x10);
    let (directory, repository, _git) = create_temp_repo_with_git();

    let outcome = repository.store_binding(&binding).expect("first store");
    assert_eq!(outcome, BindingStoreOutcome::Stored);
    let replay = repository.store_binding(&binding).expect("replay");
    assert_eq!(replay, BindingStoreOutcome::Idempotent);

    // Same payload, same key → byte-identical bytes → same id, idempotent.
    let republished = {
        let binding_again = sample_binding(&keypair, 0x10);
        assert_eq!(binding_again.encode(), binding.encode());
        repository.store_binding(&binding_again).expect("store again")
    };
    assert_eq!(republished, BindingStoreOutcome::Idempotent);

    drop(repository);
    let reopened = Repository::open(directory.path()).expect("reopen");
    let loaded = reopened
        .load_binding(&binding.id())
        .expect("load binding")
        .expect("binding present after reopen");
    assert_eq!(loaded, binding);
    assert_eq!(reopened.binding_ids().unwrap(), vec![binding.id()]);
}

#[test]
fn publication_creates_create_only_ref_and_is_idempotent() {
    let keypair = test_keypair(4);
    let binding = sample_binding(&keypair, 0x40);
    let (_directory, repository, git) = create_temp_repo_with_git();
    let working_copy = repository.working_copy();

    let id = binding.id();
    let ref_name = Repository::binding_ref_name(&id);
    assert!(ref_name.starts_with("refs/atomic/bindings/"));
    // shard = first two hex characters of the binding id
    assert_eq!(ref_name, format!("refs/atomic/bindings/{}/{}", &id.to_hex()[..2], id.to_hex()));

    let publication = repository
        .publish_binding(working_copy, &git, &binding, None)
        .expect("publish");
    assert!(matches!(publication, BindingPublication::Published { .. }));

    // The ref is direct and points at the journaled binding commit.
    let reference = git.find_reference(&ref_name).expect("binding ref exists");
    let target = reference.target().expect("direct target");
    let stored = repository.load_binding(&id).expect("stored binding");
    assert_eq!(stored.expect("stored"), binding);
    let republished = repository
        .load_published_binding(&git, &id)
        .expect("read back")
        .expect("published binding readable through its ref");
    assert_eq!(republished, binding);

    // Identical retry: idempotent replay, same target.
    let replay = repository
        .publish_binding(working_copy, &git, &binding, None)
        .expect("retry publish");
    assert!(matches!(replay, BindingPublication::Idempotent { .. }));
    assert_eq!(replay_target(&git, &ref_name), Some(target));
}

fn replay_target(git: &git2::Repository, ref_name: &str) -> Option<git2::Oid> {
    git.find_reference(ref_name).ok().and_then(|r| r.target())
}

#[test]
fn signing_failure_publishes_nothing() {
    let keypair = test_keypair(5);
    let binding = sample_binding(&keypair, 0x50);
    let (_directory, repository, git) = create_temp_repo_with_git();
    let working_copy = repository.working_copy();

    // Forge a binding object whose signature does not verify: canonical
    // bytes with the signature bytes replaced.
    let mut forged_bytes = binding.encode();
    let signature_start = forged_bytes.len() - 64;
    for byte in &mut forged_bytes[signature_start..] {
        *byte ^= 0xFF;
    }
    let forged = GitStateBinding::decode(&forged_bytes).expect("forged bytes still decode");
    assert!(forged.verify_signature().is_err());

    let error = repository
        .publish_binding(working_copy, &git, &forged, None)
        .expect_err("an unverifiable binding is never published");
    assert!(error.to_string().contains("signature fails"), "{error}");

    // Nothing was stored, journaled, or published.
    assert!(
        repository.binding_ids().unwrap().is_empty(),
        "a signing failure must not store the binding"
    );
    let heads = repository
        .operation_log(
            atomic_core::operation::OperationScope::WorkingCopy(working_copy),
            Some(10),
            false,
        )
        .unwrap();
    assert!(matches!(
        heads.head_state,
        OperationHeadState::Empty
    ));
    assert!(git.find_reference(&Repository::binding_ref_name(&forged.id())).is_err());
}

#[test]
fn journal_failure_cannot_expose_a_valid_looking_publication() {
    let keypair = test_keypair(6);
    let binding = sample_binding(&keypair, 0x60);
    let (_directory, repository, git) = create_temp_repo_with_git();
    let working_copy = repository.working_copy();
    let ref_name = Repository::binding_ref_name(&binding.id());

    // A competing writer holds the ordered operation lock. Publication must
    // fail with the typed contention error before any journal or ref write.
    let _held = repository
        .prepare_bridge_git_ref_write(
            working_copy,
            "refs/heads/main",
            None,
            GitRefTarget::Direct(
                GitObjectId::new(GitHashAlgorithm::Sha1, vec![0u8; 20]).unwrap(),
            ),
            Hash::of(b"competing evidence"),
        )
        .expect("hold the lock");

    let refused = repository.publish_binding(working_copy, &git, &binding, None);
    assert!(
        matches!(refused, Err(RepositoryError::LockContended { .. })),
        "publication under a held operation lock must fail closed, got {refused:?}"
    );
    assert!(git.find_reference(&ref_name).is_err(), "no ref may exist");

    // After the competing writer releases, the incomplete head (a journal
    // entry with no receipts) still blocks new operations and the ref stays
    // absent: a journal failure can never leave a half-published state.
    drop(_held);
    let blocked = repository.publish_binding(working_copy, &git, &binding, None);
    assert!(
        blocked
            .err()
            .is_some_and(|error| error.to_string().contains("incomplete")),
        "an incomplete journal head must block publication until recovered"
    );
    assert!(git.find_reference(&ref_name).is_err());
}

#[test]
fn crash_between_journal_and_ref_write_gates_the_repository_with_the_ref_absent() {
    let keypair = test_keypair(7);
    let binding = sample_binding(&keypair, 0x70);
    let (directory, repository, git) = create_temp_repo_with_git();
    let working_copy = repository.working_copy();
    let ref_name = Repository::binding_ref_name(&binding.id());

    // Simulate the publish flow up to (and including) the journal, then a
    // crash before the ref write: storage first, then the journal, then the
    // process dies.
    repository.store_binding(&binding).expect("store");
    let prepared = repository
        .prepare_bridge_git_ref_write(
            working_copy,
            &ref_name,
            None,
            GitRefTarget::Direct(GitObjectId::new(GitHashAlgorithm::Sha1, vec![1u8; 20]).unwrap()),
            Hash::of(binding.encode().as_slice()),
        )
        .expect("journal the publication");
    let operation_id = prepared.operation_id;
    drop(prepared); // crash: the prepared value and its locks are lost
    drop(repository);
    drop(git);

    // The ref never existed: storage has the fact, Git has no publication.
    let git = git2::Repository::open(directory.path()).expect("open git");
    assert!(git.find_reference(&ref_name).is_err());

    // Readonly opens gate on the incomplete head (fail closed).
    let readonly = Repository::open_readonly(directory.path()).unwrap_err();
    assert!(
        readonly.to_string().contains("still completing"),
        "readonly open must refuse an incomplete publication head: {readonly}"
    );

    // A later operation is blocked until the incomplete head is recovered;
    // the repository never invents an after-state for the interrupted write.
    let recovered = Repository::open(directory.path());
    assert!(
        recovered.is_err(),
        "writable open must not silently recover a GitRef effect; typed executor gate required"
    );
    let _ = operation_id;
}

#[test]
fn old_bindings_survive_unrecord_and_new_projection() {
    let keypair = test_keypair(8);
    let (directory, repository, git) = create_temp_repo_with_git();

    // Produce a real recorded change, then bind the pre-change commit.
    std::fs::write(directory.path().join("file.txt"), b"one\n").unwrap();
    let first_outcome = repository
        .record(
            ChangeHeader::new("first"),
            crate::record::RecordOptions::new()
                .with_all(true)
                .include_untracked(true)
                .save_to_store(true)
                .apply_after_record(true),
        )
        .unwrap();

    let first_keypair = test_keypair(8);
    let first_binding = sample_binding(&first_keypair, 0x70);
    repository
        .publish_binding(repository.working_copy(), &git, &first_binding, None)
        .expect("publish first binding");

    // Unrecord creates a new state; the old binding must remain valid.
    let change_hash = first_outcome.change().hash().expect("change hash");
    let unrecorded = repository
        .unrecord(&change_hash, UnrecordOptions::default())
        .expect("unrecord");
    let _ = unrecorded;

    // A new projection re-records different content.
    std::fs::write(directory.path().join("file.txt"), b"two\n").unwrap();
    let _second_outcome = repository
        .record(
            ChangeHeader::new("second"),
            crate::record::RecordOptions::new()
                .with_all(true)
                .include_untracked(true)
                .save_to_store(true)
                .apply_after_record(true),
        )
        .unwrap();

    // A second binding for the new projection.
    let second_binding = sample_binding(&test_keypair(9), 0x80);
    repository
        .publish_binding(repository.working_copy(), &git, &second_binding, None)
        .expect("publish second binding");

    // Both bindings survive, loadable and published.
    let ids = repository.binding_ids().unwrap();
    assert!(ids.contains(&first_binding.id()));
    assert!(ids.contains(&second_binding.id()));
    for binding in [&first_binding, &second_binding] {
        let loaded = repository
            .load_binding(&binding.id())
            .expect("load")
            .expect("binding survives view operations");
        assert_eq!(loaded, *binding);
        let published = repository
            .load_published_binding(&git, &binding.id())
            .expect("read ref")
            .expect("ref still present");
        assert_eq!(published, *binding);
    }
}

#[test]
fn conflicting_ref_target_is_refused_and_the_newer_work_survives() {
    let keypair = test_keypair(10);
    let binding = sample_binding(&keypair, 0x90);
    let (_directory, repository, git) = create_temp_repo_with_git();
    let working_copy = repository.working_copy();
    let ref_name = Repository::binding_ref_name(&binding.id());

    // A competing publisher got there first with a different (real) commit.
    let tree = git.treebuilder(None).unwrap().write().unwrap();
    let tree = git.find_tree(tree).unwrap();
    let signature = git2::Signature::now("Competing", "competing@example.com").unwrap();
    let older_target = git
        .commit(Some(&ref_name), &signature, &signature, "older binding", &tree, &[])
        .unwrap();

    let refused = repository.publish_binding(working_copy, &git, &binding, None);
    assert!(
        refused.is_err(),
        "publishing over a different binding target must be refused"
    );
    // The older (newer-arrived) work is untouched.
    assert_eq!(git.find_reference(&ref_name).unwrap().target(), Some(older_target));
}

#[test]
fn publication_with_a_signed_summary_carries_only_the_allowlisted_summary() {
    let keypair = test_keypair(11);
    let binding = sample_binding(&keypair, 0xA0);
    let (_directory, repository, git) = create_temp_repo_with_git();
    let working_copy = repository.working_copy();
    let ref_name = Repository::binding_ref_name(&binding.id());

    // A separately signed attestation summary with the allowlisted fields.
    let mut usage = atomic_core::change::attestation::ModelUsage::new("claude-sonnet-4-5");
    usage.input_tokens = 120;
    usage.output_tokens = 60;
    usage.cache_read_tokens = 10;
    usage.cache_write_tokens = 2;
    usage.cost_usd = 0.30;
    let mut attestation = atomic_core::change::attestation::Attestation::builder(
        "session-cb6a",
        atomic_core::change::attestation::AttestAgent::new(
            "claude-code",
            "Claude Code",
            "anthropic",
        ),
    )
    .cost_usd(0.30)
    .build();
    attestation.models = vec![usage];
    attestation.notes = Some("PRIVATE-NOTES-SENTINEL-CB6A".to_string());
    let summary = crate::git_binding::BindingAttestationSummary::from_attestations(
        "session-cb6a",
        std::slice::from_ref(&attestation),
    );
    let signed = crate::git_binding::SignedAttestationSummary::sign(&summary, &keypair)
        .expect("sign summary");

    repository
        .publish_binding(working_copy, &git, &binding, Some(&signed))
        .expect("publish with summary");

    // The binding commit's tree carries the binding and the signed summary —
    // and nothing else.
    let target = git.find_reference(&ref_name).unwrap().target().unwrap();
    let commit = git.find_commit(target).unwrap();
    let tree = commit.tree().unwrap();
    let mut names: Vec<String> = tree
        .iter()
        .filter_map(|entry| entry.name().map(|name| name.to_string()))
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "attestation-summary.cbor".to_string(),
            "binding.cbor".to_string()
        ],
        "binding trees carry exactly the public blobs"
    );

    // The summary decodes and verifies against the binding signer's key, and
    // carries only the allowlisted fields (no notes).
    let published = repository
        .load_published_binding(&git, &binding.id())
        .expect("read back")
        .expect("binding present");
    assert_eq!(published, binding);
    let summary_entry = tree
        .get_name("attestation-summary.cbor")
        .expect("summary blob present");
    let summary_blob = git.find_blob(summary_entry.id()).unwrap();
    let decoded = crate::git_binding::SignedAttestationSummary::decode(summary_blob.content())
        .expect("decode signed summary");
    decoded
        .verify_signature(&atomic_identity::keypair::PublicKey::from_bytes(
            &binding.payload().signer.verifying_key,
        )
        .unwrap())
        .expect("summary signature verifies");
    let decoded_summary = decoded.summary().expect("summary decodes");
    assert_eq!(decoded_summary.session_id, "session-cb6a");
    assert_eq!(decoded_summary.models.len(), 1);
    assert_eq!(decoded_summary.models[0].input_tokens, 120);
    assert_eq!(decoded_summary.models[0].output_tokens, 60);
    assert_eq!(
        decoded_summary.models[0].model,
        "claude-sonnet-4-5"
    );
    let summary_bytes = summary_blob.content();
    assert!(
        !contains_sentinel(summary_bytes),
        "the signed summary must never carry attestation notes"
    );

    // A summary signed by a different key is refused at publication.
    let other_keypair = test_keypair(12);
    let forged = crate::git_binding::SignedAttestationSummary::sign(&summary, &other_keypair)
        .expect("sign with wrong key");
    let refused = repository.publish_binding(working_copy, &git, &binding, Some(&forged));
    assert!(
        refused.is_err(),
        "a summary not signed by the binding signer must be refused"
    );
}

fn contains_sentinel(bytes: &[u8]) -> bool {
    let needle = b"PRIVATE-NOTES-SENTINEL";
    bytes.windows(needle.len()).any(|window| window == needle)
}

#[test]
fn conflicting_stored_bytes_are_refused_without_overwrite() {
    let keypair = test_keypair(2);
    let binding = sample_binding(&keypair, 0x20);
    let (_directory, repository, _git) = create_temp_repo_with_git();

    repository.store_binding(&binding).expect("store");
    // Corrupt storage directly: same id, different bytes. Storage must fail
    // closed and keep the original bytes.
    let id = *binding.id().as_bytes();
    let mut txn = repository.pristine().write_txn().unwrap();
    use atomic_core::pristine::{BindingMutTxnT, MutTxnT};
    let refused = txn.insert_binding_bytes(&id, b"forged bytes");
    assert!(refused.is_err(), "conflicting bytes must be refused");
    txn.commit().unwrap();

    let loaded = repository
        .load_binding(&binding.id())
        .expect("load after refused write")
        .expect("original binding survives");
    assert_eq!(loaded, binding);
}

#[test]
fn corrupt_stored_bytes_fail_closed_instead_of_reading_none() {
    let keypair = test_keypair(3);
    let binding = sample_binding(&keypair, 0x30);
    let (directory, repository, _git) = create_temp_repo_with_git();

    repository.store_binding(&binding).expect("store");

    // Simulate storage corruption by rewriting the row at the database layer
    // (bypassing the insert-only API), then prove load_binding fails closed
    // with an explicit error rather than silently returning None.
    let db_path = directory.path().join(".atomic/pristine.redb");
    drop(repository);
    let db = redb::Database::create(&db_path).expect("open db directly");
    let write_txn = db.begin_write().expect("write txn");
    {
        let mut table = write_txn
            .open_table(atomic_core::pristine::tables::BINDINGS)
            .expect("open bindings table");
        let id = *binding.id().as_bytes();
        let value: &[u8] = b"tampered";
        table.insert(&id, value).expect("corrupt row");
    }
    write_txn.commit().expect("commit corruption");
    drop(db);

    let reopened = Repository::open_readonly(directory.path()).expect("reopen readonly");
    let error = reopened.load_binding(&binding.id()).unwrap_err();
    assert!(
        error.to_string().contains("failed revalidation"),
        "corrupt stored binding must fail closed, got: {error}"
    );
}

/// A temp root with both an Atomic repository and a Git repository.
fn create_temp_repo_with_git() -> (tempfile::TempDir, TestRepository, git2::Repository) {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let git = git2::Repository::init(temp_dir.path()).expect("init git");
    let repo = Repository::init(temp_dir.path()).expect("init atomic");
    (temp_dir, TestRepository::new(repo), git)
}

// ── Review R1: exact-carrier validation before any network write ────────

#[test]
fn legit_carrier_verifies_to_the_exact_publication_commit() {
    let keypair = test_keypair(11);
    let binding = sample_binding(&keypair, 0x11);
    let (_directory, repository, git) = create_temp_repo_with_git();
    let working_copy = repository.working_copy();
    let id = binding.id();

    let publication = repository
        .publish_binding(working_copy, &git, &binding, None)
        .expect("publish");
    let target = match &publication {
        BindingPublication::Published { binding_commit, .. } => binding_commit.clone(),
        BindingPublication::Idempotent { binding_commit } => binding_commit.clone(),
    };

    let verified = repository
        .verify_published_binding_carrier(&git, &id)
        .expect("legit carrier validates")
        .expect("carrier present");
    assert_eq!(verified.binding, binding);
    assert_eq!(verified.carrier_oid, target, "the carrier is the exact publication commit");
}

#[test]
fn carrier_with_unexpected_private_parent_refuses_verification() {
    let keypair = test_keypair(12);
    let binding = sample_binding(&keypair, 0x12);
    let (_directory, repository, git) = create_temp_repo_with_git();
    let working_copy = repository.working_copy();
    let id = binding.id();
    let ref_name = Repository::binding_ref_name(&id);

    repository
        .publish_binding(working_copy, &git, &binding, None)
        .expect("publish");
    let legit = git
        .find_reference(&ref_name)
        .expect("ref")
        .target()
        .expect("direct target");

    // Hostile carrier: same binding tree, parent chain holds a private blob.
    let signature = git2::Signature::now("Test", "test@example.com").unwrap();
    let carrier_commit = git.find_commit(legit).unwrap();
    let binding_tree = carrier_commit.tree_id();
    let blob = git.blob(b"PRIVATE_REVIEW_SENTINEL\n").unwrap();
    let mut builder = git.treebuilder(None).unwrap();
    builder.insert("private.txt", blob, git2::FileMode::Blob.into()).unwrap();
    let private_tree = builder.write().unwrap();
    let private_commit = git
        .commit(
            None,
            &signature,
            &signature,
            "private WIP",
            &git.find_tree(private_tree).unwrap(),
            &[],
        )
        .unwrap();
    let hostile = git
        .commit(
            None,
            &signature,
            &signature,
            "same binding with private parent",
            &git.find_tree(binding_tree).unwrap(),
            &[&git.find_commit(private_commit).unwrap()],
        )
        .unwrap();
    git.reference(&ref_name, hostile, true, "hostile carrier swap").unwrap();

    let error = repository
        .verify_published_binding_carrier(&git, &id)
        .expect_err("unexpected parentage must refuse");
    assert!(
        error.to_string().contains("parent"),
        "the refusal must name the unexpected parentage: {error}"
    );
}

#[test]
fn carrier_with_extra_tree_entry_refuses_verification() {
    let keypair = test_keypair(13);
    let binding = sample_binding(&keypair, 0x13);
    let (_directory, repository, git) = create_temp_repo_with_git();
    let working_copy = repository.working_copy();
    let id = binding.id();
    let ref_name = Repository::binding_ref_name(&id);

    repository
        .publish_binding(working_copy, &git, &binding, None)
        .expect("publish");
    let legit = git
        .find_reference(&ref_name)
        .expect("ref")
        .target()
        .expect("direct target");

    // Hostile carrier: the legit tree entries PLUS an extra private blob.
    let signature = git2::Signature::now("Test", "test@example.com").unwrap();
    let legit_tree = git.find_commit(legit).unwrap().tree().unwrap();
    let mut builder = git.treebuilder(None).unwrap();
    for entry in legit_tree.iter() {
        builder.insert(entry.name().unwrap(), entry.id(), entry.filemode()).unwrap();
    }
    let extra = git.blob(b"PRIVATE_REVIEW_SENTINEL\n").unwrap();
    builder.insert("private.txt", extra, git2::FileMode::Blob.into()).unwrap();
    let hostile_tree = builder.write().unwrap();
    let hostile = git
        .commit(
            None,
            &signature,
            &signature,
            "carrier with an extra entry",
            &git.find_tree(hostile_tree).unwrap(),
            &[],
        )
        .unwrap();
    git.reference(&ref_name, hostile, true, "extra entry").unwrap();

    let error = repository
        .verify_published_binding_carrier(&git, &id)
        .expect_err("extra tree entries must refuse");
    assert!(
        error.to_string().contains("non-allowlisted entry"),
        "the refusal must name the non-allowlisted entry: {error}"
    );
}
