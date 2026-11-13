use super::*;

#[test]
fn evidence_writes_reuse_an_existing_nonreentrant_boundary_without_widening_it() {
    let f = Fixture::new();
    f.repo
        .record_publication_review(&f.signed(), &f.config(), &|did| f.keys(did))
        .unwrap();
    let _outer = f.repo.try_lock_common_operation().unwrap();
    f.repo
        .vault_store(
            "scratch/outer-operation.md",
            VaultEntryType::Scratch,
            b"metadata write".to_vec(),
            "{}".into(),
        )
        .unwrap();
    assert!(
        f.repo.try_lock_common_operation().is_err(),
        "metadata alias must not enable independent nested operations"
    );
}

#[test]
fn review_materialization_preserves_unregistered_working_files() {
    let f = Fixture::new();
    let signed = f.signed();
    let parent = f
        .repo
        .vault_dir()
        .join("publication-reviews")
        .join(signed.artifact_hash());
    std::fs::create_dir_all(&parent).unwrap();
    let file = parent.join("review.md");
    std::fs::write(&file, b"user work").unwrap();
    assert!(f
        .repo
        .record_publication_review(&signed, &f.config(), &|did| f.keys(did))
        .is_err());
    assert_eq!(std::fs::read(&file).unwrap(), b"user work");
    assert!(f.repo.publication_reviews().unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn review_materialization_refuses_symlinked_parents() {
    let f = Fixture::new();
    let signed = f.signed();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(
        outside.path(),
        f.repo.vault_dir().join("publication-reviews"),
    )
    .unwrap();
    assert!(f
        .repo
        .record_publication_review(&signed, &f.config(), &|did| f.keys(did))
        .is_err());
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    assert!(f.repo.publication_reviews().unwrap().is_empty());
}

#[test]
fn preparing_after_parent_advance_does_not_refresh_old_test_evidence() {
    let f = Fixture::new();
    let original_own_state = f
        .repo
        .get_view_info("review-source")
        .unwrap()
        .state_base32();
    let other = f
        .repo
        .save_change(&Change::empty(ChangeHeader::new("new inherited context")))
        .unwrap();
    f.repo
        .insert_change_rec(&other, crate::InsertOptions::default().view("dev"))
        .unwrap();
    assert_eq!(
        f.repo
            .get_view_info("review-source")
            .unwrap()
            .state_base32(),
        original_own_state
    );
    assert!(
        !f.conforms(&f.signed(), &f.config()),
        "a fresh subject cannot refresh a check against old inherited context"
    );
}

#[test]
fn source_and_inherited_state_advances_do_not_change_reviewed_content() {
    for view in ["review-source", "dev"] {
        let f = Fixture::new();
        f.repo
            .record_publication_review(&f.signed(), &f.config(), &|did| f.keys(did))
            .unwrap();
        let other = f
            .repo
            .save_change(&Change::empty(ChangeHeader::new("new view state")))
            .unwrap();
        f.repo
            .insert_change_rec(&other, crate::InsertOptions::default().view(view))
            .unwrap();
        assert!(
            f.verdict().allowed(),
            "an immutable review must survive live view/ancestor advancement"
        );
        let stored = f.repo.publication_reviews().unwrap();
        assert!(!stored[0]
            .subject
            .changes
            .iter()
            .any(|pin| pin.hash == other.to_base32()));
        assert!(!stored[0]
            .subject
            .snapshot
            .as_ref()
            .unwrap()
            .membership
            .contains(&other.to_base32()));
    }
}

#[test]
fn review_checks_at_an_unrelated_state_cannot_authorize_current_changes() {
    let f = Fixture::new();
    let inputs = bridge::read_intent(&f.repo, &f.review).unwrap();
    let state = f
        .repo
        .get_view_info("review-source")
        .unwrap()
        .state_base32();
    let body = inputs.body.replace(
        &format!("observedAtMerkle={state}"),
        "observedAtMerkle=unrelated-state",
    );
    f.repo
        .vault_intent_update(
            &f.review,
            IntentUpdateOptions {
                content: Some(body),
                force: true,
                ..Default::default()
            },
        )
        .unwrap();
    sign_intent(&f.repo, &f.review, &f.reviewer, &f.reviewer_key);
    assert!(!f.conforms(&f.signed(), &f.config()));
}

#[test]
fn approval_is_exact_and_does_not_clear_incomplete_sessions() {
    let f = Fixture::new();
    f.repo
        .record_publication_review(&f.signed(), &f.config(), &|did| f.keys(did))
        .unwrap();
    assert!(f
        .repo
        .retrospective_review_for(
            &Hash::of(b"another change").to_base32(),
            SESSION,
            &f.config(),
            Some(&|did| f.keys(did))
        )
        .unwrap()
        .is_none());
    assert!(f
        .repo
        .retrospective_review_for(
            &f.change.to_base32(),
            "another-session",
            &f.config(),
            Some(&|did| f.keys(did))
        )
        .unwrap()
        .is_none());
    let incomplete = atomic_core::change::session::IncompleteSession::new(
        "retained incomplete original",
        Vec::<String>::new(),
        "refs/atomic/wip/fixture",
        atomic_core::change::session::SessionIncompleteOrigin::UnattributedGitOperation,
    );
    f.repo
        .mark_session_incomplete(SESSION, None, None, &incomplete)
        .unwrap();
    let verdict = f.verdict();
    assert!(!verdict.allowed());
    assert!(verdict.blocks.iter().any(|block| matches!(
        block,
        super::super::super::provenance_gate::GateBlocker::SessionIncomplete { .. }
    )));
}

#[test]
fn actual_shared_insertion_revalidates_the_exact_retained_subject() {
    let f = Fixture::new();
    let path = f.repo.dot_dir().join("config.toml");
    let mut config = atomic_config::RepoConfig::load(&path).unwrap();
    config.git.trust = f.config().trust;
    config.save(&path).unwrap();
    let before = f.repo.get_view_changes(Some("dev")).unwrap();
    assert!(!before.iter().any(|(_, hash)| *hash == f.change));
    f.repo
        .record_publication_review(&f.signed(), &f.config(), &|did| f.keys(did))
        .unwrap();
    f.repo
        .insert_from_view(crate::CrossViewInsertOptions::new("review-source", "dev"))
        .unwrap();
    assert!(f
        .repo
        .get_view_changes(Some("dev"))
        .unwrap()
        .iter()
        .any(|(_, hash)| *hash == f.change));
    assert_eq!(f.repo.current_view(), "review-source");
}

#[test]
fn renewal_retains_history_and_cannot_suppress_a_different_scope() {
    let f = Fixture::new();
    let old = f.signed();
    let old_path = f
        .repo
        .record_publication_review(&old, &f.config(), &|did| f.keys(did))
        .unwrap();
    assert!(f.repo.vault_delete(&old_path).is_err());
    assert!(f
        .repo
        .vault_store(
            &old_path,
            VaultEntryType::Attestation,
            b"{}".to_vec(),
            "{}".into()
        )
        .is_err());
    f.repo
        .vault_intent_update(
            &f.review,
            IntentUpdateOptions {
                title: Some("renewed exact review".into()),
                status: Some("done".into()),
                force: true,
                ..Default::default()
            },
        )
        .unwrap();
    sign_intent(&f.repo, &f.review, &f.reviewer, &f.reviewer_key);
    let mut renewed = f.signed();
    renewed.supersedes = vec![old.artifact_hash()];
    renewed = f.sign(renewed);
    f.repo
        .record_publication_review(&renewed, &f.config(), &|did| f.keys(did))
        .unwrap();
    assert!(f.verdict().allowed(), "{}", f.verdict().refusal_report());
    assert_eq!(f.repo.publication_reviews().unwrap().len(), 2);
    assert!(f.repo.vault_retrieve(&old_path).unwrap().is_some());
    let mut wrong = renewed.clone();
    wrong.supersedes = vec!["missing-review".into()];
    wrong = f.sign(wrong);
    assert!(f
        .repo
        .record_publication_review(&wrong, &f.config(), &|did| f.keys(did))
        .is_err());
}

#[test]
fn unindexed_bad_original_and_missing_registered_provenance_fail_closed() {
    let f = Fixture::new();
    let signed = f.signed();
    f.repo
        .record_publication_review(&signed, &f.config(), &|did| f.keys(did))
        .unwrap();
    let original = Attestation::builder(SESSION, AttestAgent::new("fixture", "fixture", "fixture"))
        .changes_covered(vec![f.change])
        .build();
    f.repo.change_store().save_attestation(&original).unwrap(); // deliberately unindexed
    assert!(!f.verdict().allowed());
    let f = Fixture::new();
    let signed = f.signed();
    f.repo
        .record_publication_review(&signed, &f.config(), &|did| f.keys(did))
        .unwrap();
    let hash = Hash::from_base32(
        signed
            .subject
            .changes
            .iter()
            .find(|c| c.hash == f.change.to_base32())
            .unwrap()
            .provenance[0]
            .as_bytes(),
    )
    .unwrap();
    std::fs::remove_file(f.repo.change_store().provenance_path(&hash)).unwrap();
    assert!(!f.verdict().allowed());
}

#[test]
fn unrelated_observed_intent_cannot_be_excluded_by_the_review_author() {
    let f = Fixture::new();
    let owner = signed_intent(&f.repo, &f.author, &f.author_key, "feature", None);
    let graph = ProvenanceGraphBuilder::new(SESSION, "fixture")
        .add_node(receipt_node(&owner))
        .changes_explained(vec![f.change])
        .build();
    f.repo.save_provenance_graph(&graph).unwrap();
    assert!(!f.conforms(&f.signed(), &f.config()));
}

#[test]
fn evidence_writer_contends_with_the_promotion_boundary() {
    let f = Fixture::new();
    f.repo
        .record_publication_review(&f.signed(), &f.config(), &|did| f.keys(did))
        .unwrap();
    std::thread::scope(|scope| {
        let (ready_send, ready_receive) = std::sync::mpsc::channel();
        let (release_send, release_receive) = std::sync::mpsc::channel();
        let path = f.repo.root().to_path_buf();
        let pristine = std::sync::Arc::clone(&f.repo.pristine);
        let worker = scope.spawn(move || {
            let repo = Repository::open_with_pristine(&path, pristine).unwrap();
            let _guard = repo
                .try_lock_common_operation_with_reentrancy(true)
                .unwrap();
            ready_send.send(()).unwrap();
            release_receive.recv().unwrap();
        });
        ready_receive.recv().unwrap();
        let attempt = f.repo.vault_intent_update(
            &f.review,
            IntentUpdateOptions {
                title: Some("racing update".into()),
                force: true,
                ..Default::default()
            },
        );
        let view_attempt = f.repo.set_view_identity(
            "review-source",
            atomic_core::pristine::ViewScope::Draft,
            None,
        );
        release_send.send(()).unwrap();
        worker.join().unwrap();
        assert!(
            attempt.is_err(),
            "evidence must not mutate under another operation's publication lease"
        );
        assert!(
            view_attempt.is_err(),
            "view identity must not change during publication either"
        );
    });
}
