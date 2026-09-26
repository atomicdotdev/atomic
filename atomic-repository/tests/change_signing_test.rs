//! Integration tests for change signing (CB: intent ATOM::aaron::27).
//!
//! Covers:
//! - AC-1: record with a signing identity produces a `.change` file whose
//!   signature verifies against the identity's public key; the SIGNATURE
//!   section does not affect the change hash.
//! - AC-3: out-of-band verification — the forge scenario (attacker signs
//!   with their own key and embeds their own DID) must fail verification
//!   against the victim's known key.
//! - AC-5: backward compatibility — unsigned changes still load and apply;
//!   the change hash is identical with and without a signature.

use std::fs;
use std::path::Path;

use atomic_core::change::format_v3::{ChangeReader, ChangeSignature, SectionType};
use atomic_core::change::signing::{sign_change, verify_change_signature, ChangeSignatureError};
use atomic_core::change::{Author, Change, ChangeHeader};
use atomic_core::types::{Base32, Hash};
use atomic_repository::record::{RecordOptions, SigningIdentity};
use atomic_repository::Repository;
use tempfile::TempDir;

fn add_file(repo: &Repository, repo_path: &Path, name: &str, content: &str) {
    fs::write(repo_path.join(name), content).expect("write file");
    repo.add(name, Default::default()).expect("add file");
}

fn record_unsigned(repo: &Repository, message: &str) -> Hash {
    let header = ChangeHeader::builder()
        .message(message)
        .author(Author::new("Test", Some("test@example.com")))
        .build();
    *repo
        .record(header, RecordOptions::default())
        .expect("record")
        .hash()
}

fn signing_identity(name: &str) -> (SigningIdentity, [u8; 32]) {
    // Deterministic test keypair — the seed IS the key material, so the
    // test doesn't need the identity store.
    let mut seed = [0u8; 32];
    for (i, b) in name.bytes().cycle().take(32).enumerate() {
        seed[i] = b;
    }
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&seed);
    let public = signing_key.verifying_key().to_bytes();
    let pk_base32 = data_encoding::BASE32_NOPAD.encode(&public);
    let did = format!("did:atomic:{}", &pk_base32[..16]);
    (
        SigningIdentity {
            signer_did: did,
            secret_key: seed,
        },
        public,
    )
}

fn record_signed(repo: &Repository, message: &str, signing: &SigningIdentity) -> (Hash, Change) {
    let header = ChangeHeader::builder()
        .message(message)
        .author(Author::new("Test", Some("test@example.com")))
        .build();
    let outcome = repo
        .record(
            header,
            RecordOptions::default().with_signing_identity(signing.clone()),
        )
        .expect("record");
    let hash = *outcome.hash();
    let change = repo.load_change(&hash).expect("load change");
    (hash, change)
}

// ═══════════════════════════════════════════════════════════════════════
// AC-1: signed records verify; signature does not change the hash
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn signed_record_verifies_against_public_key() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path().to_path_buf();
    let repo = Repository::init(&repo_path).expect("init");

    add_file(&repo, &repo_path, "signed.txt", "signed content\n");
    let (signing, public) = signing_identity("alice");
    let (hash, change) = record_signed(&repo, "signed change", &signing);

    // The change carries a signature.
    let signature = change.signature.as_ref().expect("signature present");
    assert!(signature.is_ed25519());

    // The signature verifies against the OUT-OF-BAND public key.
    verify_change_signature(signature, &public, &hash).expect("verify");

    // The stored .change file on disk parses and its SIGNATURE section
    // round-trips.
    let base32 = hash.to_base32();
    let file_path = repo_path
        .join(".atomic/changes")
        .join(&base32[..2])
        .join(format!("{}.change", base32));
    let bytes = fs::read(&file_path).expect("read .change file");

    let mut cursor = std::io::Cursor::new(&bytes);
    let mut reader = ChangeReader::open(&mut cursor).expect("open reader");
    let mut found_signature: Option<ChangeSignature> = None;
    while let Some(section) = reader.next_section().expect("section") {
        if section.section_type == SectionType::Signature {
            found_signature = Some(
                section
                    .deserialize()
                    .expect("deserialize signature section"),
            );
        }
    }
    let file_hash_bytes = reader.verify().expect("verify file hash");
    let file_hash = Hash::from_bytes(file_hash_bytes);

    let file_signature = found_signature.expect("SIGNATURE section present in file");
    assert_eq!(file_signature, *signature);
    assert_eq!(file_hash, hash, "file hash equals change identity");
    verify_change_signature(&file_signature, &public, &file_hash).expect("verify from file");
}

#[test]
fn signature_does_not_change_change_hash() {
    // A signed change's hash is a pure function of content+header+deps:
    // stripping the signature and re-serializing must yield the SAME hash,
    // and re-signing with a DIFFERENT key must also yield the same hash.
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path().to_path_buf();
    let repo = Repository::init(&repo_path).expect("init");

    add_file(&repo, &repo_path, "h.txt", "hash stability\n");
    let (signing_alice, _alice_public) = signing_identity("alice-signing");
    let (signing_mallory, _mallory_public) = signing_identity("mallory-signing");

    let (_hash, change) = record_signed(&repo, "signed by alice", &signing_alice);

    // Strip the signature and re-serialize — the hash must not change.
    let signed_hash = Change::hash(&change).expect("hash signed");
    let mut unsigned = change.clone();
    unsigned.signature = None;
    let unsigned_hash_of_same = unsigned.hash().expect("hash unsigned variant");
    assert_eq!(
        signed_hash, unsigned_hash_of_same,
        "removing the signature must not change the change hash"
    );

    // Re-sign with a different key — hash still must not change.
    let did = signing_mallory.signer_did.clone();
    unsigned
        .sign_with(&did, &signing_mallory.secret_key, 999)
        .expect("re-sign with mallory's key");
    let mallory_hash = unsigned.hash().expect("hash mallory-signed");
    assert_eq!(
        signed_hash, mallory_hash,
        "re-signing with a different key must not change the change hash"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// AC-3: the forge scenario — embedded key is not a trust root
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn forge_with_attacker_key_fails_against_victim_key() {
    // An attacker signs content with THEIR key and claims to be the victim
    // (their DID is embedded in the signature). Verification against the
    // victim's known public key must FAIL.
    let attacker = ed25519_dalek::SigningKey::from_bytes(
        b"attacker-attacker-attacker-32byte!!"[..32]
            .try_into()
            .unwrap(),
    );
    let victim = ed25519_dalek::SigningKey::from_bytes(
        b"victim--victim--victim--victim--32b!!"[..32]
            .try_into()
            .unwrap(),
    );

    let hash = Hash::of(b"malicious content claiming to be Aaron");
    let attacker_did = "did:atomic:ATTACKERCLAIMINGTOBEVICTIM00000000";

    // Attacker forges the signature section.
    let forged = sign_change(
        attacker_did,
        attacker.to_bytes().as_slice().try_into().unwrap(),
        &hash,
        1739290034,
    );

    // Verifier resolves the VICTIM's public key out-of-band (identity store,
    // lookup-key, whatever) and checks.
    let victim_public = victim.verifying_key().to_bytes();
    let err = verify_change_signature(&forged, &victim_public, &hash).unwrap_err();
    assert!(
        matches!(
            err,
            ChangeSignatureError::SignatureVerificationFailed { .. }
        ),
        "forged signature must fail against victim's key, got: {err:?}"
    );

    // Sanity: it WOULD verify against the attacker's own key — proving that
    // self-referential verification (checking against the embedded key) is
    // broken and must never be used.
    let attacker_public = attacker.verifying_key().to_bytes();
    verify_change_signature(&forged, &attacker_public, &hash).expect(
        "forged sig verifies against attacker key — demonstrating why embedded-key trust is broken",
    );
}

#[test]
fn unsigned_change_has_no_signature_and_still_works() {
    // AC-5 (backward compat): an unsigned change loads fine; its signature
    // is None; its content applies.
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path().to_path_buf();
    let repo = Repository::init(&repo_path).expect("init");

    add_file(&repo, &repo_path, "legacy.txt", "legacy content\n");
    let hash = record_unsigned(&repo, "legacy unsigned change");

    let change = repo.load_change(&hash).expect("load unsigned change");
    assert!(
        change.signature.is_none(),
        "unsigned change has no signature"
    );

    // Round-trip through serialize/deserialize still works.
    let mut bytes = Vec::new();
    let re_hash = change.serialize(&mut bytes).expect("serialize");
    assert_eq!(re_hash, hash);
    let (re_change, _) =
        Change::deserialize(&mut std::io::Cursor::new(&bytes)).expect("deserialize");
    assert!(re_change.signature.is_none());
}

// ═══════════════════════════════════════════════════════════════════════
// AC-5: signed and unsigned changes coexist in one repository
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn signed_and_unsigned_changes_coexist() {
    let temp = TempDir::new().unwrap();
    let repo_path = temp.path().to_path_buf();
    let repo = Repository::init(&repo_path).expect("init");

    // Unsigned first (simulates a pre-signing change).
    add_file(&repo, &repo_path, "old.txt", "old\n");
    let old = record_unsigned(&repo, "old unsigned");

    // Then signed (new binary).
    let (signing, public) = signing_identity("carol");
    add_file(&repo, &repo_path, "new.txt", "new\n");
    let new = {
        let header = ChangeHeader::builder()
            .message("new signed")
            .author(Author::new("Test", Some("test@example.com")))
            .build();
        let outcome = repo
            .record(
                header,
                RecordOptions::default().with_signing_identity(signing.clone()),
            )
            .expect("record");
        *outcome.hash()
    };

    // Both load; only the new one carries a signature.
    let old_change = repo.load_change(&old).expect("load old");
    let new_change = repo.load_change(&new).expect("load new");
    assert!(old_change.signature.is_none());
    assert!(new_change.signature.is_some());

    // The signature verifies out-of-band.
    verify_change_signature(new_change.signature.as_ref().unwrap(), &public, &new)
        .expect("verify signed change");

    // History shows both.
    let history = repo
        .log(atomic_repository::history::HistoryOptions::default())
        .expect("log");
    let hashes: Vec<Hash> = history.into_iter().map(|e| e.hash).collect();
    assert!(hashes.contains(&old));
    assert!(hashes.contains(&new));
}
