//! The four conformance cases a value-shaped entry point cannot decide.
//!
//! `jcs::canonicalize` takes a `serde_json::Value`, and by the time one exists a
//! repeated member has collapsed to last-wins and a document nested past any
//! bound has already been built. `jcs::admit_document` sits on the bytes
//! instead, which is where those questions are still answerable.
//!
//! Every reject fixture in `tests/vectors/ingest/` is a byte-for-byte copy of a
//! conformance document; `tests/vectors/ingest/PROVENANCE.md` names the vector
//! each came from, the path the suite's manifest gives it, and the suite commit,
//! and it says which two are a vector's record sidecar rather than its statement
//! because the sidecar is the half carrying the fault. Each test below
//! asserts the refusal variant rather than a message, because the variant is what
//! a verifier branches on: a repeated member means two readings of one document
//! and a depth refusal is a bound on the work.
//!
//! The accept controls matter as much. A gate that refuses everything scores the
//! same as a gate that refuses nothing, so each boundary in this crate is
//! exercised with a certificate it must still take, as well as with one it must
//! now refuse.

use std::fs;
use std::path::{Path, PathBuf};

use atomic_canonical::delegation;
use atomic_canonical::jcs;
use atomic_canonical::CanonicalError;
use atomic_identity::delegation::{Delegation, DelegationScope};
use atomic_identity::identity::{Identity, IdentityType};
use atomic_identity::keypair::KeyPair;
use atomic_identity::IdentityStore;
use jcs_admit::Error as Admission;
use serde_json::Value;

fn vector(rel: &str) -> Vec<u8> {
    let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/vectors/ingest")
        .join(rel);
    fs::read(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"))
}

fn admission_error(bytes: &[u8]) -> Admission {
    match jcs::admit_document(bytes) {
        Err(CanonicalError::Admission(e)) => e,
        Err(other) => panic!("expected an admission refusal, got {other}"),
        Ok(_) => panic!("expected a refusal, the document was admitted"),
    }
}

// ---------------------------------------------------------------------------
// The four cases
// ---------------------------------------------------------------------------

/// `v0f4f2093061d303f` (`aia-c-5`). One wire document, two readings: a
/// first-wins reader shows `read_file` while the hash commits to
/// `delete_repository`. A signature over either reading verifies, so the fault
/// has to be refused before a reading is chosen.
#[test]
fn duplicate_member_is_refused() {
    let bytes = vector("records/v0f4f2093061d303f.jsonl");
    assert!(
        matches!(admission_error(&bytes), Admission::DuplicateMember { ref name, .. } if name == "toolName"),
        "v0f4f2093061d303f must be refused as a duplicate member"
    );
    // And the reason a value-shaped check cannot do it: the repeat is gone.
    let parsed: Value = serde_json::from_slice(&bytes).expect("the bytes do parse");
    assert_eq!(parsed["toolName"], "delete_repository");
}

/// `vd94ac70c9f0d84bf` (`aia-c-12`). One container past the cap, refused with
/// a catchable error naming the cap rather than by recursing. The cap is
/// `jcs::MAX_DEPTH`, declared once and the only depth bound on the path;
/// `tests/depth_boundary.rs` sits on it from both sides.
#[test]
fn nesting_one_past_the_cap_is_refused() {
    let bytes = vector("statements/vd94ac70c9f0d84bf.json");
    assert!(
        matches!(
            admission_error(&bytes),
            Admission::TooDeep {
                limit: jcs::MAX_DEPTH,
                ..
            }
        ),
        "vd94ac70c9f0d84bf must be refused at the depth cap"
    );
}

/// `v679f56481420e45a` (`aia-c-14`). 2^53 + 1 in `durationMs`. RFC 8785 defers
/// number formatting to ECMAScript, which has one numeric type, so a conforming
/// re-checker canonicalizes this to `9007199254740992` — a different integer,
/// with no error. A signed field a re-checker may rewrite is a field two parties
/// can disagree about, which is what the RFC 7493 profile removes.
#[test]
fn an_integer_past_2pow53_is_refused() {
    let bytes = vector("statements/v679f56481420e45a.json");
    assert!(
        matches!(admission_error(&bytes), Admission::UnsafeInteger { ref token } if token == "9007199254740993"),
        "v679f56481420e45a must be refused as an unsafe integer"
    );
}

/// `v97f5d8777e514257` (`aia-c-9`). A fractional `durationMs` in a signed
/// record. RFC 8785 admits it and writes the double; two implementations that
/// never format a float cannot disagree about one, so the profile refuses it and
/// leaves fractional values to the content-digest form.
#[test]
fn a_non_integer_in_a_signed_field_is_refused() {
    let bytes = vector("records/v97f5d8777e514257.jsonl");
    assert!(
        matches!(admission_error(&bytes), Admission::NonIntegerNumber { ref token } if token == "412.5"),
        "v97f5d8777e514257 must be refused as a non-integer"
    );
}

// ---------------------------------------------------------------------------
// Accept controls, one per call site the change touches
// ---------------------------------------------------------------------------

struct Pair {
    human: Identity,
    human_key: KeyPair,
    agent: Identity,
}

fn pair() -> Pair {
    let human_key = KeyPair::generate();
    let human = Identity::new("alice", &human_key);
    let agent_key = KeyPair::generate();
    let agent = Identity::builder("alice+claude")
        .identity_type(IdentityType::Agent)
        .public_key(agent_key.public.clone())
        .delegated_by(human.id)
        .build()
        .expect("agent identity");
    Pair {
        human,
        human_key,
        agent,
    }
}

fn certificate(p: &Pair) -> Value {
    let scope = DelegationScope::builder()
        .permission(atomic_identity::delegation::DelegationPermission::Record)
        .project("acme/*")
        .max_changes(64)
        .build();
    let terms = Delegation::new(&p.human, &p.agent, scope);
    delegation::mint(&p.human, &p.human_key, &terms)
}

/// A real certificate is admitted in both serializations that reach the
/// boundary: the compact bytes transport carries and the indented bytes the
/// store holds. `maxChanges` is the one numeric field in the vocabulary, and an
/// honest count passes the integers-only profile.
#[test]
fn a_real_certificate_is_admitted_in_both_serializations() {
    let doc = certificate(&pair());
    for rendered in [
        serde_json::to_vec(&doc).expect("compact"),
        serde_json::to_vec_pretty(&doc).expect("indented"),
    ] {
        let admitted = jcs::admit_document(&rendered).expect("a minted certificate is admissible");
        assert_eq!(
            admitted, doc,
            "admission must not alter an accepted document"
        );
    }
}

/// `delegation::decode_from_transport` — the `Atomic-Delegation` request header.
/// The accept half: a certificate round-trips and still verifies.
#[test]
fn decode_from_transport_still_takes_a_real_certificate() {
    let p = pair();
    let doc = certificate(&p);
    let decoded = delegation::decode_from_transport(&delegation::encode_for_transport(&doc))
        .expect("a minted certificate decodes");
    assert_eq!(decoded, doc);
    delegation::verify(&decoded, &p.human.public_key).expect("and verifies");
}

/// The same boundary, refusing. A repeated `delegateKey` names one key to a
/// reader and a different one to the signature; before this change the parse
/// discarded the repeat and the certificate went on to be verified.
#[test]
fn decode_from_transport_refuses_a_repeated_member() {
    let repeated =
        br#"{"@type":"AgentDelegation","delegateKey":"did:key:zFirst","delegateKey":"did:key:zSecond"}"#;
    let encoded = data_encoding::BASE64URL_NOPAD.encode(repeated);
    match delegation::decode_from_transport(&encoded) {
        Err(CanonicalError::Admission(Admission::DuplicateMember { name, .. })) => {
            assert_eq!(name, "delegateKey");
        }
        other => panic!("expected a duplicate-member refusal, got {other:?}"),
    }
}

/// `delegation::load_for_delegate` — certificates read back off the identity
/// store, which is a document this machine received from somewhere else. The
/// accept half: a stored certificate is still found.
#[test]
fn load_for_delegate_still_finds_a_stored_certificate() {
    let root = tempfile::tempdir().expect("temp store");
    let store = IdentityStore::open(root.path()).expect("open store");
    let p = pair();
    let doc = certificate(&p);
    let parsed = delegation::parse(&doc).expect("parse the minted certificate");
    store
        .save_delegation(
            &parsed.id.to_base32(),
            &serde_json::to_string_pretty(&doc).expect("indented"),
        )
        .expect("save");

    let found = delegation::load_for_delegate(&store, &p.agent).expect("load");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].delegation.delegate_name, "alice+claude");
}

/// The same boundary, refusing. A stored document carrying a repeated member is
/// skipped rather than verified, and the skip does not take the other
/// certificates with it.
#[test]
fn load_for_delegate_skips_a_stored_document_with_a_repeated_member() {
    let root = tempfile::tempdir().expect("temp store");
    let store = IdentityStore::open(root.path()).expect("open store");
    let p = pair();
    let doc = certificate(&p);
    let parsed = delegation::parse(&doc).expect("parse the minted certificate");
    store
        .save_delegation(
            &parsed.id.to_base32(),
            &serde_json::to_string(&doc).expect("compact"),
        )
        .expect("save the good one");

    let good = serde_json::to_string(&doc).expect("compact");
    let repeated = good.replacen('{', r#"{"delegateKey":"did:key:zSecond","#, 1);
    store
        .save_delegation("AAAABBBBCCCCDDDD", &repeated)
        .expect("save the one with the repeat");

    let found = delegation::load_for_delegate(&store, &p.agent).expect("load");
    assert_eq!(
        found.len(),
        1,
        "the admissible certificate survives and the repeated member does not"
    );
}
