use super::*;
use atomic_canonical::publication_review::MAX_BYTES;
use atomic_core::pristine::{MutTxnT, VaultEntry, VaultMutTxnT};

fn stored(f: &Fixture, review: &PublicationReview) -> Vec<u8> {
    f.repo
        .vault_retrieve(&format!("{PREFIX}{}/review.md", review.artifact_hash()))
        .unwrap()
        .expect("review is stored")
        .content_bytes
}

/// Make a signed review larger by naming more source members. The result is
/// only a document to store: it no longer describes the fixture repository.
fn padded_to(review: &PublicationReview, compact_len: usize) -> PublicationReview {
    let mut padded = review.clone();
    let members = &mut padded.subject.snapshot.as_mut().unwrap().membership;
    let member = Hash::of(b"padding").to_base32();
    let per_member = member.len() + 3;
    let missing = compact_len - serde_json::to_vec(review).unwrap().len();
    members.extend(std::iter::repeat_n(member, missing / per_member));
    let short = compact_len - serde_json::to_vec(&padded).unwrap().len();
    padded.reviewed_at.push_str(&"0".repeat(short));
    assert_eq!(serde_json::to_vec(&padded).unwrap().len(), compact_len);
    padded
}

/// The 1 MiB bound is checked on the compact signed document when a review is
/// prepared and when it is read. Storing a larger encoding of it lets a review
/// pass the first check, be recorded, and then fail the second for good.
#[test]
fn a_recorded_review_is_stored_no_larger_than_its_signed_document() {
    let f = Fixture::new();
    let review = f.signed();
    f.repo
        .record_publication_review(&review, &f.config(), &|did| f.keys(did))
        .unwrap();
    let bytes = stored(&f, &review);
    assert!(
        bytes.len() <= serde_json::to_vec(&review).unwrap().len() + 1,
        "stored {} bytes for a {} byte document",
        bytes.len(),
        serde_json::to_vec(&review).unwrap().len()
    );
    assert_eq!(PublicationReview::parse(&bytes).unwrap(), review);
    assert_eq!(f.repo.publication_reviews().unwrap(), vec![review]);
}

#[test]
fn every_review_within_the_bound_is_storable_and_reads_back() {
    let f = Fixture::new();
    // One byte is the trailing newline of the stored file.
    let largest = padded_to(&f.signed(), MAX_BYTES - 1);
    let bytes = stored_bytes(&largest).unwrap();
    assert_eq!(bytes.len(), MAX_BYTES);
    assert_eq!(PublicationReview::parse(&bytes).unwrap(), largest);
}

#[test]
fn a_review_the_inventory_could_not_read_back_is_refused_before_it_is_written() {
    let f = Fixture::new();
    let too_large = padded_to(&f.signed(), MAX_BYTES);
    let error = stored_bytes(&too_large).unwrap_err().to_string();
    assert!(error.contains("exceeds 1 MiB"), "{error}");
}

/// Reviews recorded before the compact encoding stay where they are, and
/// recording one of them again is still the no-op it was.
#[test]
fn a_review_stored_in_the_earlier_pretty_encoding_can_be_recorded_again() {
    let f = Fixture::new();
    let review = f.signed();
    let path = format!("{PREFIX}{}/review.md", review.artifact_hash());
    let pretty = format!("{}\n", serde_json::to_string_pretty(&review).unwrap()).into_bytes();
    f.repo
        .vault_store(
            &path,
            VaultEntryType::Attestation,
            pretty.clone(),
            "{}".into(),
        )
        .unwrap();

    assert_eq!(
        f.repo
            .record_publication_review(&review, &f.config(), &|did| f.keys(did))
            .unwrap(),
        path
    );
    assert_eq!(
        stored(&f, &review),
        pretty,
        "the stored artifact is immutable"
    );
    assert_eq!(f.repo.publication_reviews().unwrap(), vec![review]);
    assert!(f.verdict().allowed(), "{}", f.verdict().refusal_report());
}

/// Fill the inventory to its limit with distinct unsigned legacy documents.
/// The inventory only parses and addresses what it lists, so these are enough.
fn fill_inventory(f: &Fixture) {
    use atomic_canonical::publication_review::{IntentPin, ReviewSubject, LEGACY_SCHEMA};
    let pin = IntentPin {
        id: "ATOM::1".into(),
        source_hash: String::new(),
        signed_hash: String::new(),
        public_key: String::new(),
    };
    let mut txn = f.repo.pristine().write_txn().unwrap();
    for index in 0..MAX_REVIEWS {
        let review = PublicationReview {
            schema: LEGACY_SCHEMA.into(),
            reviewed_at: format!("filler {index}"),
            subject: ReviewSubject {
                project: String::new(),
                roots: vec![],
                changes: vec![],
                review: pin.clone(),
                work: vec![],
                set_aside: vec![],
                profile_hash: String::new(),
                view_chain: vec![],
                effective_set_id: String::new(),
                snapshot: None,
            },
            supersedes: vec![],
            attributed_to: "filler".into(),
            content_hash: None,
            proof: None,
        };
        txn.put_vault_entry(
            &format!("{PREFIX}{}/review.md", review.artifact_hash()),
            &VaultEntry::new(
                VaultEntryType::Attestation,
                serde_json::to_vec(&review).unwrap(),
                "{}".into(),
                String::new(),
            ),
        )
        .unwrap();
    }
    txn.commit().unwrap();
}

/// The inventory refuses to list more than its limit. A review recorded past
/// the limit would therefore take every stored review down with it.
#[test]
fn a_full_inventory_refuses_another_review_and_stays_listable() {
    let f = Fixture::new();
    let review = f.signed();
    fill_inventory(&f);
    assert_eq!(f.repo.publication_reviews().unwrap().len(), MAX_REVIEWS);

    let error = f
        .repo
        .record_publication_review(&review, &f.config(), &|did| f.keys(did))
        .unwrap_err()
        .to_string();
    assert!(error.contains("already holds 4096 artifacts"), "{error}");
    assert_eq!(f.repo.publication_reviews().unwrap().len(), MAX_REVIEWS);
    assert!(!f
        .repo
        .vault_dir()
        .join("publication-reviews")
        .join(review.artifact_hash())
        .exists());
}
