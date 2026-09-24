//! Data Integrity proofs over canonical nodes (`eddsa-jcs-2022`).
//!
//! Signing covers the JCS-canonical node minus its `proof` (the standard Data
//! Integrity shape). We reuse atomic's existing Ed25519 `Signer`/`PublicKey`
//! and sign over the same canonical bytes the content hash uses, via the one
//! `jcs` entry point — so hash and signature can never disagree.
//!
//! `proofValue` is a multibase string in base58btc (`z`), as the
//! `eddsa-jcs-2022` cryptosuite specifies.

use atomic_identity::identity::Identity;
use atomic_identity::keypair::{KeyPair, PublicKey};
use atomic_identity::signing::{Signature, Signer};
use serde_json::Value;

use crate::did;
use crate::error::{CanonicalError, Result};
use crate::hash;
use crate::jcs;
use crate::memory::MemoryNode;
use crate::node::{CanonicalNode, Proof};

pub const CRYPTOSUITE: &str = "eddsa-jcs-2022";
pub const PROOF_TYPE: &str = "DataIntegrityProof";
pub const PROOF_PURPOSE: &str = "assertionMethod";
/// Multibase prefix for base58btc — the encoding `eddsa-jcs-2022` specifies.
const MULTIBASE_BASE58_BTC: char = 'z';

/// Canonical property names. Hardcoded because the closed vocabulary
/// standardizes them across *all* node types (Intent, Memory, and any future
/// PROV `@graph`), so the "which keys are excluded from signing/hashing" rule
/// has exactly one owner — these consts and the two views below.
pub const PROP_PROOF: &str = "proof";
pub const PROP_CONTENT_HASH: &str = "contentHash";
pub const PROP_ATTRIBUTED_TO: &str = "attributedTo";

/// Top-level intent property carrying review *state* (not substance).
pub const PROP_STATUS: &str = "status";
/// The acceptance-criterion array whose per-element review state the substance
/// view strips (keeping each criterion's reviewable *definition*).
pub const PROP_HAS_ACCEPTANCE_CRITERION: &str = "hasAcceptanceCriterion";

/// Per-acceptance-criterion keys that are review *state*, not definition. The
/// substance view strips these from every element of `hasAcceptanceCriterion`
/// so an intent's substance hash is stable across review activity (a criterion
/// flipping `unmet`→`met`, gaining a verifier/evidence, or accumulating
/// verification records). `verifications` extends the doc's original
/// `{acStatus, verifiedBy, evidence}` list — it postdates the doc but is the
/// same principle (review state, not substance).
pub const AC_REVIEW_STATE_KEYS: &[&str] = &["acStatus", "verifiedBy", "evidence", "verifications"];

/// The value covered by a signature: the document minus its `proof` (the
/// standard Data Integrity shape). `contentHash` is retained so the signature
/// also commits to the hash. This is the single site that owns the exclusion.
pub fn signing_view(value: &Value) -> Value {
    let mut value = value.clone();
    if let Some(obj) = value.as_object_mut() {
        obj.remove(PROP_PROOF);
    }
    value
}

/// The value covered by the content hash: excludes both `proof` and
/// `contentHash` (you cannot hash the hash).
pub fn hashing_view(value: &Value) -> Value {
    let mut value = signing_view(value);
    if let Some(obj) = value.as_object_mut() {
        obj.remove(PROP_CONTENT_HASH);
    }
    value
}

/// The value covered by the *substance* hash (`intentSubstanceHash`): the
/// reviewable definition of an intent, with all review **state** and
/// authorship/proof **metadata** removed. It extends [`hashing_view`] (which
/// already drops `proof` + `contentHash`) by additionally stripping the
/// top-level `status` and `attributedTo`, and, from every element of
/// `hasAcceptanceCriterion`, the review-state keys in [`AC_REVIEW_STATE_KEYS`].
///
/// `attributedTo` is excluded because [`attest_value`] injects it when an intent
/// is signed — so a pin stamped at `done` (before attestation) would otherwise
/// disagree with the recomputed hash of the *attested* node, producing a
/// spurious `STALE_TRIAGE`. Authorship is not reviewable substance.
///
/// What stays is the definition an editor could meaningfully change: each
/// criterion's `text`/`@id`/`@type` and its `requiredKinds` (the verification
/// bar is part of the criterion's *definition*, not review state), plus `why`,
/// scope, constraints, and tasks. So the substance hash moves when the intent's
/// *meaning* changes and holds steady while it is merely being reviewed or
/// attested.
pub fn substance_view(value: &Value) -> Value {
    let mut value = hashing_view(value);
    if let Some(obj) = value.as_object_mut() {
        obj.remove(PROP_STATUS);
        obj.remove(PROP_ATTRIBUTED_TO);
        if let Some(Value::Array(acs)) = obj.get_mut(PROP_HAS_ACCEPTANCE_CRITERION) {
            for ac in acs.iter_mut() {
                if let Some(ac_obj) = ac.as_object_mut() {
                    for key in AC_REVIEW_STATE_KEYS {
                        ac_obj.remove(*key);
                    }
                }
            }
        }
    }
    value
}

/// Generic attest over a JSON-LD value — the *one* canonicalization/signing
/// path all typed nodes share. Fills `attributedTo` (from the identity's
/// `did:atomic`) when absent, computes the content hash over `hashing_view`,
/// signs `jcs(signing_view)`, and attaches the proof. Returns the value.
///
/// It is [`prepare_attestation`] → sign → [`attach_proof`]; a signer that
/// holds its key outside this process uses those two halves directly.
pub fn attest_value(value: Value, identity: &Identity, keypair: &KeyPair) -> Value {
    let prepared = prepare_attestation(value, &identity.public_key);
    let signature = Signer::new(keypair).sign(&prepared.signing_bytes);
    attach_proof(prepared.value, &identity.public_key, &signature)
}

/// A value made ready to sign: `attributedTo` and `contentHash` filled in,
/// and the exact bytes the signature must cover.
#[derive(Debug, Clone)]
pub struct PreparedAttestation {
    /// The value to sign — pass it back to [`attach_proof`] unchanged.
    pub value: Value,
    /// `jcs(signing_view(value))`: what the Ed25519 signature covers.
    pub signing_bytes: Vec<u8>,
}

/// First half of [`attest_value`], for signers that hold the key somewhere
/// else — a browser's WebCrypto, a hardware token, a remote signing service.
/// Everything that must agree with [`verify_value`] (the author, the content
/// hash, the canonical bytes) is computed here, so the external signer only
/// ever signs bytes, and the result is an ordinary atomic attestation.
pub fn prepare_attestation(mut value: Value, public_key: &PublicKey) -> PreparedAttestation {
    let did = did::did_for_public_key(public_key);

    if let Some(obj) = value.as_object_mut() {
        // Fill attributedTo only if there is no non-empty value already.
        let has_author = obj
            .get(PROP_ATTRIBUTED_TO)
            .and_then(Value::as_str)
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        if !has_author {
            obj.insert(PROP_ATTRIBUTED_TO.to_string(), Value::String(did));
        }
    }

    // Hash first (over the value without proof/contentHash), then sign (over
    // the value with contentHash, without proof).
    let content_hash = hash::content_hash(&hashing_view(&value));
    if let Some(obj) = value.as_object_mut() {
        obj.insert(PROP_CONTENT_HASH.to_string(), Value::String(content_hash));
    }

    let signing_bytes = jcs::canonicalize(&signing_view(&value)).into_bytes();
    PreparedAttestation {
        value,
        signing_bytes,
    }
}

/// Second half of [`attest_value`]: attach the `eddsa-jcs-2022` proof for a
/// signature over [`PreparedAttestation::signing_bytes`] made by
/// `public_key`'s private key. Does not check the signature — run
/// [`verify_value`] on the result for that.
pub fn attach_proof(mut value: Value, public_key: &PublicKey, signature: &Signature) -> Value {
    let did = did::did_for_public_key(public_key);
    let proof = Proof {
        type_: PROOF_TYPE.to_string(),
        cryptosuite: CRYPTOSUITE.to_string(),
        verification_method: did::verification_method(&did),
        proof_purpose: PROOF_PURPOSE.to_string(),
        proof_value: encode_proof_value(signature),
    };
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            PROP_PROOF.to_string(),
            serde_json::to_value(proof).expect("proof serialization is infallible"),
        );
    }
    value
}

/// Generic verify over a JSON-LD value — the same three checks as the typed
/// path: (1) the content hash recomputes over `hashing_view`, (2) the signature
/// verifies over `jcs(signing_view)`, (3) the proof's verificationMethod DID
/// belongs to this key.
pub fn verify_value(value: &Value, public_key: &PublicKey) -> Result<()> {
    let obj = value
        .as_object()
        .ok_or_else(|| CanonicalError::Proof("value is not a JSON object".into()))?;

    // 1. Content hash integrity.
    let expected = obj
        .get(PROP_CONTENT_HASH)
        .and_then(Value::as_str)
        .ok_or_else(|| CanonicalError::Proof("node carries no contentHash".into()))?;
    let actual = hash::content_hash(&hashing_view(value));
    if expected != actual {
        return Err(CanonicalError::HashMismatch {
            expected: expected.to_string(),
            actual,
        });
    }

    // 2. Signature over the canonical signing bytes.
    let proof: Proof = obj
        .get(PROP_PROOF)
        .cloned()
        .and_then(|p| serde_json::from_value(p).ok())
        .ok_or_else(|| CanonicalError::Proof("node carries no proof".into()))?;

    // The proof's own metadata is stripped by `signing_view`, so it is not
    // covered by the signature. Enforce the suite/purpose/type we expect here,
    // otherwise a stored node's declared cryptosuite is silently mutable.
    if proof.type_ != PROOF_TYPE
        || proof.cryptosuite != CRYPTOSUITE
        || proof.proof_purpose != PROOF_PURPOSE
    {
        return Err(CanonicalError::Verification(format!(
            "unexpected proof metadata: type='{}' cryptosuite='{}' proofPurpose='{}'",
            proof.type_, proof.cryptosuite, proof.proof_purpose
        )));
    }

    let signature = decode_proof_value(&proof.proof_value)?;
    let signing_bytes = jcs::canonicalize(&signing_view(value)).into_bytes();
    signature
        .verify(&signing_bytes, public_key)
        .map_err(|e| CanonicalError::Verification(e.to_string()))?;

    // 3. The proof's verificationMethod DID must belong to this key.
    let did = did::did_from_verification_method(&proof.verification_method);
    if !did::did_matches_public_key(did, public_key) {
        return Err(CanonicalError::Verification(
            "verificationMethod DID does not match the public key".into(),
        ));
    }
    Ok(())
}

/// Produce a fully attested Intent node — a thin wrapper over
/// [`attest_value`] so hash and signature can never drift from any other node
/// type. The round trip through serde is lossless for an already-constructed
/// node (all fields are symmetric), so `from_value` cannot fail.
pub fn attest(node: CanonicalNode, identity: &Identity, keypair: &KeyPair) -> CanonicalNode {
    let value = attest_value(node.to_value(), identity, keypair);
    serde_json::from_value(value).expect("attested intent re-deserializes")
}

/// Verify an attested Intent node — thin wrapper over [`verify_value`].
pub fn verify(node: &CanonicalNode, public_key: &PublicKey) -> Result<()> {
    verify_value(&node.to_value(), public_key)
}

/// Produce a fully attested Memory node — the same thin wrapper over
/// [`attest_value`] the Intent path uses, so both node types sign through one
/// canonicalization path and their hashes/signatures cannot drift.
pub fn attest_memory(node: MemoryNode, identity: &Identity, keypair: &KeyPair) -> MemoryNode {
    let value = attest_value(node.to_value(), identity, keypair);
    serde_json::from_value(value).expect("attested memory re-deserializes")
}

/// Verify an attested Memory node — thin wrapper over [`verify_value`].
pub fn verify_memory(node: &MemoryNode, public_key: &PublicKey) -> Result<()> {
    verify_value(&node.to_value(), public_key)
}

fn encode_proof_value(signature: &Signature) -> String {
    let mut s = String::new();
    s.push(MULTIBASE_BASE58_BTC);
    s.push_str(&bs58::encode(signature.as_bytes()).into_string());
    s
}

fn decode_proof_value(value: &str) -> Result<Signature> {
    let mut chars = value.chars();
    match chars.next() {
        Some(MULTIBASE_BASE58_BTC) => {}
        _ => {
            return Err(CanonicalError::Proof(format!(
                "unsupported multibase prefix in proofValue: {value:.4}…"
            )))
        }
    }
    let body = &value[MULTIBASE_BASE58_BTC.len_utf8()..];
    let bytes = bs58::decode(body)
        .into_vec()
        .map_err(|e| CanonicalError::Proof(format!("bad base58btc proofValue: {e}")))?;
    Signature::from_slice(&bytes).map_err(|e| CanonicalError::Proof(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::CONTEXT_URL;
    use serde_json::json;

    fn dev_identity() -> (Identity, KeyPair) {
        let kp = KeyPair::generate();
        let id = Identity::new("lee", &kp);
        (id, kp)
    }

    fn minimal_value() -> Value {
        json!({
            "@context": CONTEXT_URL,
            "@type": "Intent",
            "@id": "urn:atomic:intent:generic-1",
            "title": "A generic value node",
            "status": "todo"
        })
    }

    /// An external signer — handed only the prepared bytes — produces
    /// exactly the attestation `attest_value` would, and it verifies.
    #[test]
    fn prepare_sign_attach_matches_attest_value() {
        let (id, kp) = dev_identity();
        let prepared = prepare_attestation(minimal_value(), &kp.public);
        let signature = Signer::new(&kp).sign(&prepared.signing_bytes);
        let external = attach_proof(prepared.value, &kp.public, &signature);

        assert_eq!(external, attest_value(minimal_value(), &id, &kp));
        verify_value(&external, &kp.public).expect("externally signed value verifies");

        // A signature over anything else does not.
        let wrong = Signer::new(&kp).sign(b"not the prepared bytes");
        let prepared = prepare_attestation(minimal_value(), &kp.public);
        let bad = attach_proof(prepared.value, &kp.public, &wrong);
        assert!(verify_value(&bad, &kp.public).is_err());
    }

    #[test]
    fn attest_value_then_verify_value_roundtrips() {
        let (id, kp) = dev_identity();
        let attested = attest_value(minimal_value(), &id, &kp);

        // The generic path filled the excluded-later fields.
        let obj = attested.as_object().unwrap();
        assert!(obj
            .get(PROP_CONTENT_HASH)
            .unwrap()
            .as_str()
            .unwrap()
            .starts_with("blake3:"));
        assert!(obj.get(PROP_PROOF).is_some());
        assert!(obj
            .get(PROP_ATTRIBUTED_TO)
            .unwrap()
            .as_str()
            .unwrap()
            .starts_with("did:atomic:"));

        // Verifies with the right key, independent of CanonicalNode.
        verify_value(&attested, &kp.public).expect("generic verify should succeed");

        // Tamper a signed field → the stored contentHash no longer matches.
        let mut tampered = attested.clone();
        tampered
            .as_object_mut()
            .unwrap()
            .insert("status".into(), Value::String("done".into()));
        let err = verify_value(&tampered, &kp.public).unwrap_err();
        assert!(
            matches!(err, CanonicalError::HashMismatch { .. }),
            "expected HashMismatch, got {err:?}"
        );

        // Wrong key → verification error.
        let other = KeyPair::generate();
        assert!(verify_value(&attested, &other.public).is_err());
    }

    #[test]
    fn substance_view_is_stable_across_attestation() {
        // Attesting injects attributedTo + proof + contentHash. None are
        // reviewable substance, so the substance view (and thus
        // intentSubstanceHash) must be identical before and after — otherwise a
        // pin stamped at `done` (pre-attest) spuriously reads as STALE_TRIAGE
        // against the attested node.
        let (id, kp) = dev_identity();
        let before = minimal_value();
        let attested = attest_value(before.clone(), &id, &kp);

        assert!(
            attested.get(PROP_ATTRIBUTED_TO).is_some(),
            "attestation should have injected attributedTo"
        );
        assert_eq!(
            substance_view(&before),
            substance_view(&attested),
            "substance view must ignore attributedTo/proof/contentHash injected by attestation"
        );
    }

    #[test]
    fn typed_wrapper_matches_value_core() {
        let (id, kp) = dev_identity();
        let node = CanonicalNode {
            context: CONTEXT_URL.to_string(),
            type_: "Intent".to_string(),
            id: "urn:atomic:intent:wrapper-1".to_string(),
            human_key: "W-1".to_string(),
            title: "Wrapper equivalence".to_string(),
            status: "todo".to_string(),
            kind: crate::node::default_kind(),
            priority: None,
            view: None,
            motivated_by: None,
            informed_by: Vec::new(),
            has_acceptance_criterion: Vec::new(),
            has_task: Vec::new(),
            has_scope_in: Vec::new(),
            has_scope_out: Vec::new(),
            has_constraint: Vec::new(),
            depends_on: Vec::new(),
            why: Some("a reason".to_string()),
            content_hash: None,
            attributed_to: None,
            created_at: "2026-06-25T00:00:00Z".to_string(),
            proof: None,
        };

        // The typed wrapper must add nothing over the value core: attesting the
        // node equals from_value(attest_value(node.to_value())).
        let via_wrapper = attest(node.clone(), &id, &kp);
        let via_core: CanonicalNode =
            serde_json::from_value(attest_value(node.to_value(), &id, &kp)).unwrap();

        assert_eq!(via_wrapper.content_hash, via_core.content_hash);
        assert_eq!(
            via_wrapper.proof.as_ref().map(|p| &p.proof_value),
            via_core.proof.as_ref().map(|p| &p.proof_value)
        );
    }

    /// A node shaped the way pre-widening attestations were: `satisfies` is a
    /// bare string, not a list. Built at the Value level so the fixture is
    /// genuinely scalar on the wire — which is exactly how the old typed struct
    /// (`satisfies: Option<String>`) serialized it.
    fn legacy_scalar_satisfies_value() -> Value {
        json!({
            "@context": CONTEXT_URL,
            "@type": "Intent",
            "@id": "urn:atomic:intent:legacy-1",
            "humanKey": "LEG-1",
            "title": "A pre-widening intent",
            "status": "backlog",
            "createdAt": "2026-07-20T14:50:41Z",
            "hasAcceptanceCriterion": [{
                "@type": "AcceptanceCriterion",
                "@id": "urn:atomic:ac:leg-1-ac-1",
                "text": "It works.",
                "acStatus": "open"
            }],
            "hasTask": [{
                "@type": "Task",
                "@id": "urn:atomic:task:leg-1-1",
                "text": "Do the thing.",
                "taskStatus": "open",
                "satisfies": "urn:atomic:ac:leg-1-ac-1"
            }]
        })
    }

    #[test]
    fn legacy_scalar_satisfies_deserializes_and_still_verifies() {
        // The regression this locks down: `satisfies` was widened from
        // `Option<String>` to a list, which made every attestation signed before
        // that change fail to deserialize — so valid signatures were reported as
        // absent and the intent showed as unattested.
        let (id, kp) = dev_identity();
        let attested = attest_value(legacy_scalar_satisfies_value(), &id, &kp);

        // Sanity: the fixture really is scalar on the wire, or this test proves
        // nothing about the shape we care about.
        assert!(attested["hasTask"][0]["satisfies"].is_string());

        // 1. It deserializes at all — this is what used to fail outright.
        let node: CanonicalNode =
            serde_json::from_value(attested.clone()).expect("legacy scalar must deserialize");
        assert_eq!(
            node.has_task[0].satisfies.as_slice(),
            vec!["urn:atomic:ac:leg-1-ac-1".to_string()],
            "a scalar must read as a one-element slice"
        );

        // 2. Re-serializing reproduces the signed bytes EXACTLY. This is the
        //    property that rules out the tempting `"x"` -> `["x"]` shim: the
        //    signature covers `to_value()` output, so any normalization would
        //    change the content hash and turn a valid signature into a reported
        //    forgery.
        assert_eq!(
            node.to_value(),
            attested,
            "round-trip must preserve the scalar shape byte-for-byte"
        );
        assert_eq!(
            jcs::canonicalize(&node.signing_value()),
            jcs::canonicalize(&signing_view(&attested)),
            "canonical signing bytes must be unchanged by the round-trip"
        );

        // 3. And therefore the original signature verifies through the typed path.
        verify(&node, &kp.public).expect("legacy attestation must still verify");
    }

    #[test]
    fn normalizing_a_legacy_scalar_would_break_verification() {
        // Guards the reasoning above rather than the code: it demonstrates why
        // `Satisfies` preserves representation. If someone later "simplifies" the
        // enum back to a plain `Vec`, this test documents the cost — verification
        // fails on data that is cryptographically sound.
        let (id, kp) = dev_identity();
        let attested = attest_value(legacy_scalar_satisfies_value(), &id, &kp);

        let mut normalized = attested.clone();
        normalized["hasTask"][0]["satisfies"] =
            json!([attested["hasTask"][0]["satisfies"].as_str().unwrap()]);

        let err = verify_value(&normalized, &kp.public)
            .expect_err("normalizing the shape must break the content hash");
        assert!(
            matches!(err, CanonicalError::HashMismatch { .. }),
            "expected a hash mismatch, got {err:?}"
        );
    }

    #[test]
    fn newly_lifted_tasks_never_mint_the_scalar_shape() {
        // The scalar variant exists only to round-trip history. Anything we
        // author must serialize as a list, so the old shape does not spread.
        let node = crate::lift::lift_intent(
            &serde_json::from_value(json!({
                "id": "NEW-1",
                "uid": "01KYWH0XDVKKYE4PX0ZENJYB5G",
                "title": "A new intent",
                "status": "backlog"
            }))
            .unwrap(),
            ":::task{#t-1 status=open satisfies=new-1-ac-1}\nWork.\n:::",
        )
        .expect("lift must succeed");

        assert!(
            node.to_value()["hasTask"][0]["satisfies"].is_array(),
            "a freshly lifted task must serialize `satisfies` as an array"
        );
    }
}
