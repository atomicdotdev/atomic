use super::*;

fn store_trusted(f: &Fixture, signed: &PublicationReview) {
    let path = f.repo.dot_dir().join("config.toml");
    let mut config = atomic_config::RepoConfig::load(&path).unwrap();
    config.git.trust = f.config().trust;
    config.save(&path).unwrap();
    f.repo
        .record_publication_review(signed, &f.config(), &|did| f.keys(did))
        .unwrap();
}

#[test]
fn stored_approval_survives_source_view_deletion_and_name_reuse() {
    let mut f = Fixture::new();
    let signed = f.signed();
    assert_eq!(signed.schema, SCHEMA);
    store_trusted(&f, &signed);
    f.repo
        .insert_change_rec(&f.change, crate::InsertOptions::default().view("dev"))
        .unwrap();
    let wc = f.repo.require_working_copy_id().unwrap();
    f.repo.switch_view(wc, "dev").unwrap();
    f.repo.delete_view("review-source").unwrap();
    assert!(f.conforms(&signed, &f.config()));
    assert!(f.verdict().allowed());
    f.repo.create_draft_view("review-source", "dev").unwrap();
    let other = f
        .repo
        .save_change(&Change::empty(ChangeHeader::new(
            "unrelated replacement view",
        )))
        .unwrap();
    f.repo
        .insert_change_rec(
            &other,
            crate::InsertOptions::default().view("review-source"),
        )
        .unwrap();
    assert!(
        f.conforms(&signed, &f.config()),
        "historical labels must not bind to a newly reused view name"
    );
}

#[test]
fn retained_membership_logs_and_intent_documents_cannot_be_forged() {
    let f = Fixture::new();
    let original = f.signed();
    let mut variants = Vec::new();
    let mut bad = original.clone();
    bad.subject.snapshot.as_mut().unwrap().membership.pop();
    variants.push(bad);
    let mut bad = original.clone();
    bad.subject.snapshot.as_mut().unwrap().view_logs[0].pop();
    variants.push(bad);
    let mut bad = original.clone();
    bad.subject.snapshot.as_mut().unwrap().intent_revisions[0].document["title"] =
        json!("forged retained document");
    variants.push(bad);
    let mut bad = original.clone();
    bad.subject.snapshot.as_mut().unwrap().intent_revisions[0]
        .frontmatter
        .insert("title".into(), json!("forged source inputs"));
    variants.push(bad);
    let mut bad = original.clone();
    bad.subject
        .snapshot
        .as_mut()
        .unwrap()
        .intent_revisions
        .pop();
    variants.push(bad);
    let mut bad = original.clone();
    bad.subject.snapshot.as_mut().unwrap().version = 99;
    variants.push(bad);
    for bad in variants {
        assert!(
            !f.conforms(&f.sign(bad), &f.config()),
            "even a new outer signature cannot repair broken source evidence"
        );
    }
    let mut missing = original;
    missing.subject.snapshot = None;
    assert!(PublicationReview::parse(&serde_json::to_vec(&missing).unwrap()).is_err());
}

#[test]
fn deletion_does_not_disable_trust_revocation_or_current_intent_freshness() {
    let mut f = Fixture::new();
    let signed = f.signed();
    store_trusted(&f, &signed);
    f.repo
        .insert_change_rec(&f.change, crate::InsertOptions::default().view("dev"))
        .unwrap();
    f.repo
        .switch_view(f.repo.require_working_copy_id().unwrap(), "dev")
        .unwrap();
    f.repo.delete_view("review-source").unwrap();
    assert!(f.conforms(&signed, &f.config()));
    let mut revoked = f.config();
    revoked.trust.revoked.push(signed.attributed_to.clone());
    assert!(!f.conforms(&signed, &revoked));
    f.repo
        .vault_intent_update(
            &f.work,
            IntentUpdateOptions {
                title: Some("changed intent still requires review".into()),
                force: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(!f.conforms(&signed, &f.config()));
}

#[test]
fn legacy_documents_keep_addresses_and_do_not_inherit_snapshot_semantics() {
    let f = Fixture::new();
    let mut legacy = f.signed();
    legacy.schema = atomic_canonical::publication_review::LEGACY_SCHEMA.into();
    legacy.subject.snapshot = None;
    let legacy = f.sign(legacy);
    let bytes = serde_json::to_vec(&legacy).unwrap();
    let parsed = PublicationReview::parse(&bytes).unwrap();
    assert_eq!(parsed.artifact_hash(), legacy.artifact_hash());
    assert!(f.conforms(&parsed, &f.config()));
    let mut invalid = parsed;
    invalid.subject.snapshot = f.signed().subject.snapshot;
    assert!(PublicationReview::parse(&serde_json::to_vec(&invalid).unwrap()).is_err());
}
