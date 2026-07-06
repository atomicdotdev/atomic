//! W3C PROV projection — a PROV JSON-LD named subgraph over one turn's provenance.
//!
//! This is a **projection** of provenance atomic already captures (a per-turn
//! `ProvenanceGraph`), NOT new capture. Given a plain input struct describing a
//! turn's activity, [`project`] builds the named-subgraph JSON-LD described in
//! the "Recording the Why" baseline §"A provenance graph".
//!
//! ## Unsigned by default; **signable** on demand
//! Per the baseline, the provenance graph is "one addressable, **signable** unit
//! you can hand to an auditor whole" — *signable*, not *signed*. It is a derived
//! view whose `used`/`generated` edges point at already-signed primaries
//! (intents, memories, changes), and its own chain integrity comes from
//! `turnParent` hash-linking — so the default projection carries **no** proof
//! envelope. [`attest_prov`] / [`verify_prov`] are the opt-in "signable" path:
//! when you need a standalone, tamper-evident bundle, they sign/verify the graph
//! through the *same* canonicalization/signing path every other node type uses
//! ([`crate::proof::attest_value`]), which injects a top-level
//! `attributedTo`/`contentHash`/`proof` bound to the signing Person.
//!
//! ## Layering
//! This module — like the rest of `atomic-canonical` — is **`atomic-core`
//! independent**. The projector takes a fully-owned plain [`ProvActivityInput`]
//! (no `atomic-core` types, no `Hash`); the `atomic-core::ProvenanceGraph ->
//! ProvActivityInput` mapping (and all base32/`Hash` handling) lives at the
//! `atomic-cli` boundary. Do not add `use atomic_core::` here.
//!
//! ## Agent id is a URN, not a DID (W3C DID-Core compliance)
//! The baseline example writes the `SoftwareAgent` as `did:atomic:agent:claude`.
//! We deliberately emit `urn:atomic:agent:<slug>` instead. A `did:` identifier
//! must resolve to a DID Document with verification material (DID-Core); a
//! keyless agent has none, so a `did:atomic:agent:<name>` would be a
//! non-resolvable *fake* DID — and it is not even a valid `did:atomic`, which is
//! a key fingerprint `base32(blake3(pubkey))` (see [`crate::did`]). PROV-O admits
//! any IRI for an agent, so a `urn:` is the honest, spec-compliant choice for a
//! non-verifiable descriptive label. When agents are issued their own keypairs,
//! the correct `@id` becomes a real resolvable `did:key` — never
//! `did:atomic:agent:`. Only the **Person's** key signs (in the signable path).
//!
//! ## The flywheel `used` edge
//! The activity's `used` edge (the intents/memories/vault state the turn pulled)
//! is a first-class, populated field ([`ProvActivityInput::used`]). Following the
//! baseline's "unknown links are omitted, never invented", it is emitted only
//! when non-empty. Today the `atomic-core` capture records no structured inputs,
//! so the mapping supplies an empty set until the capture records them (a
//! separate capture-path change); the projection itself is ready.

use atomic_identity::identity::Identity;
use atomic_identity::keypair::{KeyPair, PublicKey};
use serde_json::{json, Map, Value};

use crate::node::CONTEXT_URL;
use crate::proof;
use crate::Result;

/// Plain, fully-owned input to [`project`]. Carries no `atomic-core` types so the
/// projector stays `atomic-core` independent — the mapping from
/// `atomic_core::change::ProvenanceGraph` (and all `Hash::to_base32()` handling)
/// is done at the `atomic-cli` boundary.
#[derive(Debug, Clone, PartialEq)]
pub struct ProvActivityInput {
    /// Base32 of the change hash this graph explains. Names the subgraph
    /// (`urn:atomic:provgraph:<change_id_base32>`). REQUIRED.
    pub change_id_base32: String,
    /// Stable id for the turn activity (`urn:atomic:activity:<activity_id>`).
    /// Use `session_id` (or `session_id#<change_id_base32>` when a session has
    /// multiple turn-graphs) so distinct turn-activities never collide.
    pub activity_id: String,
    /// RFC3339 start time, or `None` to omit `prov:startedAtTime`.
    pub started_at: Option<String>,
    /// RFC3339 end time, or `None` to omit `prov:endedAtTime`.
    pub ended_at: Option<String>,
    /// Agent slug (`urn:atomic:agent:<slug>`) — a LABEL, non-DID.
    pub agent_slug: String,
    /// Human label for the `SoftwareAgent` (`prov:label`).
    pub agent_display_name: String,
    /// Optional vendor label on the `SoftwareAgent`.
    pub agent_vendor: Option<String>,
    /// The real signer's `did:atomic:<base32(blake3(pubkey))>` (the Person and
    /// the `actedOnBehalfOf` target).
    pub person_did: String,
    /// `urn:atomic:change:<base32>` for each explained change (PROV `generated`).
    pub generated: Vec<String>,
    /// What the turn `used` — `urn:atomic:intent:<..>` / `urn:atomic:memory:<..>`
    /// (and, eventually, vault-state refs) the activity pulled in. The flywheel
    /// input edge. Emitted only when non-empty (unknown links are omitted, never
    /// invented); empty until the capture records structured inputs.
    pub used: Vec<String>,
    /// `urn:atomic:activity:<parent-activity-id>` (PROV `wasInformedBy` /
    /// `turnParent`), or `None` to omit the key entirely.
    pub turn_parent: Option<String>,
}

/// `urn:atomic:change:<base32>` for a change hash's base32 form.
pub fn change_urn(change_id_base32: &str) -> String {
    format!("urn:atomic:change:{change_id_base32}")
}

/// `urn:atomic:provgraph:<base32>` — the named-subgraph `@id`.
pub fn provgraph_urn(change_id_base32: &str) -> String {
    format!("urn:atomic:provgraph:{change_id_base32}")
}

/// `urn:atomic:activity:<activity_id>` — the `prov:Activity` `@id`.
pub fn activity_urn(activity_id: &str) -> String {
    format!("urn:atomic:activity:{activity_id}")
}

/// `urn:atomic:agent:<slug>` — the `prov:SoftwareAgent` `@id` (a LABEL, non-DID).
pub fn agent_urn(slug: &str) -> String {
    format!("urn:atomic:agent:{slug}")
}

/// Normalize an agent registry name into a slug: lowercase, first hyphen
/// segment (e.g. `"Claude-Code" -> "claude"`). Mirrors the *shape* of
/// `atomic_agent::identity::normalize_agent_name`, implemented locally so this
/// crate does not depend on `atomic-agent`.
pub fn normalize_agent_slug(agent_name: &str) -> String {
    agent_name
        .split('-')
        .next()
        .unwrap_or(agent_name)
        .to_lowercase()
}

/// Project a turn's provenance into the named-subgraph JSON-LD.
///
/// Builds `urn:atomic:provgraph:<change_id_base32>` with
/// `@graph = [prov:Activity, prov:SoftwareAgent, prov:Person]`, exactly per the
/// doc §"A provenance graph" but with the hard-scope agent-id override
/// (`urn:atomic:agent:<slug>`, never a DID) and `used: []` (flywheel deferred).
///
/// Deterministic: fixed key insertion order + JCS canonicalization at sign time
/// make the signed bytes stable across calls.
pub fn project(input: &ProvActivityInput) -> Value {
    let agent_id = agent_urn(&input.agent_slug);

    // --- prov:Activity ---------------------------------------------------
    let mut activity = Map::new();
    activity.insert("@type".into(), json!("prov:Activity"));
    activity.insert("@id".into(), json!(activity_urn(&input.activity_id)));
    if let Some(started) = &input.started_at {
        activity.insert("prov:startedAtTime".into(), json!(started));
    }
    if let Some(ended) = &input.ended_at {
        activity.insert("prov:endedAtTime".into(), json!(ended));
    }
    // wasAssociatedWith (the agent) / actedOnBehalfOf (the person).
    activity.insert("associatedWith".into(), json!(agent_id));
    activity.insert("actedOnBehalfOf".into(), json!(input.person_did));
    activity.insert("generated".into(), json!(input.generated));
    // Flywheel `used` edge — emitted only when non-empty (unknown omitted, never
    // invented). Empty until the capture records structured inputs.
    if !input.used.is_empty() {
        activity.insert("used".into(), json!(input.used));
    }
    // turnParent — omit the key entirely when there is no parent turn.
    if let Some(parent) = &input.turn_parent {
        activity.insert("turnParent".into(), json!(parent));
    }

    // --- prov:SoftwareAgent (NON-VERIFIABLE descriptive label) -----------
    let mut agent = Map::new();
    agent.insert("@type".into(), json!("prov:SoftwareAgent"));
    agent.insert("@id".into(), json!(agent_id));
    agent.insert("actedOnBehalfOf".into(), json!(input.person_did));
    // Human label as rdfs:label (there is no prov:label in PROV-O); the ctx maps
    // the `label` term to rdfs:label.
    agent.insert("label".into(), json!(input.agent_display_name));
    if let Some(vendor) = &input.agent_vendor {
        agent.insert("vendor".into(), json!(vendor));
    }

    // --- prov:Person (the real did:atomic signer) ------------------------
    let mut person = Map::new();
    person.insert("@type".into(), json!("prov:Person"));
    person.insert("@id".into(), json!(input.person_did));

    let mut root = Map::new();
    root.insert("@context".into(), json!(CONTEXT_URL));
    root.insert("@id".into(), json!(provgraph_urn(&input.change_id_base32)));
    root.insert(
        "@graph".into(),
        json!([
            Value::Object(activity),
            Value::Object(agent),
            Value::Object(person),
        ]),
    );
    Value::Object(root)
}

/// Project + attest a turn's provenance: the PERSON's identity + keypair sign the
/// named subgraph through the shared [`crate::proof::attest_value`] path (fills a
/// top-level `attributedTo` from the signer's `did:atomic`, computes
/// `contentHash`, attaches an `eddsa-jcs-2022` Data Integrity proof).
pub fn attest_prov(input: &ProvActivityInput, identity: &Identity, keypair: &KeyPair) -> Value {
    proof::attest_value(project(input), identity, keypair)
}

/// Verify a signed PROV subgraph against a public key — thin wrapper over
/// [`crate::proof::verify_value`] (content-hash integrity + signature + the
/// proof's verificationMethod DID belonging to the key).
pub fn verify_prov(value: &Value, public_key: &PublicKey) -> Result<()> {
    proof::verify_value(value, public_key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::CanonicalError;
    use crate::proof::{PROP_ATTRIBUTED_TO, PROP_CONTENT_HASH, PROP_PROOF};
    use atomic_identity::keypair::KeyPair;

    fn dev_identity() -> (Identity, KeyPair) {
        let kp = KeyPair::generate();
        let id = Identity::new("lee", &kp);
        (id, kp)
    }

    fn sample_input(person_did: &str) -> ProvActivityInput {
        ProvActivityInput {
            change_id_base32: "CHANGEBASE32".into(),
            activity_id: "session-123".into(),
            started_at: None,
            ended_at: None,
            agent_slug: "claude".into(),
            agent_display_name: "Claude Code".into(),
            agent_vendor: Some("anthropic".into()),
            person_did: person_did.into(),
            generated: vec![change_urn("CHANGEBASE32")],
            used: vec![
                "urn:atomic:intent:019efe85".into(),
                "urn:atomic:memory:01J8ZC4R8T".into(),
            ],
            turn_parent: Some(activity_urn("session-000")),
        }
    }

    fn graph_nodes(value: &Value) -> &Vec<Value> {
        value
            .get("@graph")
            .and_then(Value::as_array)
            .expect("@graph is an array")
    }

    fn node_of_type<'a>(value: &'a Value, ty: &str) -> &'a Value {
        graph_nodes(value)
            .iter()
            .find(|n| n.get("@type").and_then(Value::as_str) == Some(ty))
            .unwrap_or_else(|| panic!("no @graph node of @type {ty}"))
    }

    #[test]
    fn project_emits_expected_shape() {
        let person = "did:atomic:PERSONFINGERPRINT";
        let value = project(&sample_input(person));

        // Top-level named subgraph.
        assert_eq!(
            value.get("@id").and_then(Value::as_str),
            Some("urn:atomic:provgraph:CHANGEBASE32")
        );
        assert_eq!(
            value.get("@context").and_then(Value::as_str),
            Some(CONTEXT_URL)
        );

        // Exactly Activity + SoftwareAgent + Person.
        assert_eq!(graph_nodes(&value).len(), 3);

        let activity = node_of_type(&value, "prov:Activity");
        assert_eq!(
            activity.get("@id").and_then(Value::as_str),
            Some("urn:atomic:activity:session-123")
        );
        assert_eq!(
            activity.get("associatedWith").and_then(Value::as_str),
            Some("urn:atomic:agent:claude")
        );
        assert_eq!(
            activity.get("actedOnBehalfOf").and_then(Value::as_str),
            Some(person)
        );
        assert_eq!(
            activity.get("generated"),
            Some(&json!(["urn:atomic:change:CHANGEBASE32"]))
        );
        // `used` is populated (the flywheel input edge).
        assert_eq!(
            activity.get("used"),
            Some(&json!([
                "urn:atomic:intent:019efe85",
                "urn:atomic:memory:01J8ZC4R8T"
            ]))
        );
        assert_eq!(
            activity.get("turnParent").and_then(Value::as_str),
            Some("urn:atomic:activity:session-000")
        );

        // SoftwareAgent @id is a URN, NEVER a did:.
        let agent = node_of_type(&value, "prov:SoftwareAgent");
        let agent_id = agent.get("@id").and_then(Value::as_str).unwrap();
        assert!(agent_id.starts_with("urn:atomic:agent:"));
        assert!(!agent_id.starts_with("did:"));
        assert_eq!(agent.get("label").and_then(Value::as_str), Some("Claude Code"));
        assert_eq!(agent.get("vendor").and_then(Value::as_str), Some("anthropic"));

        // Person @id == the real did:atomic.
        let person_node = node_of_type(&value, "prov:Person");
        assert_eq!(person_node.get("@id").and_then(Value::as_str), Some(person));
    }

    #[test]
    fn project_omits_absent_optional_keys() {
        let mut input = sample_input("did:atomic:PERSON");
        input.turn_parent = None;
        input.agent_vendor = None;
        input.started_at = None;
        input.ended_at = None;
        input.used = Vec::new();

        let value = project(&input);
        let activity = node_of_type(&value, "prov:Activity");
        // Omitted, not null.
        assert!(activity.get("turnParent").is_none());
        assert!(activity.get("prov:startedAtTime").is_none());
        assert!(activity.get("prov:endedAtTime").is_none());
        // `used` is omitted when empty (unknown links omitted, never invented).
        assert!(activity.get("used").is_none());

        let agent = node_of_type(&value, "prov:SoftwareAgent");
        assert!(agent.get("vendor").is_none());
    }

    #[test]
    fn project_is_deterministic_over_signing_bytes() {
        let input = sample_input("did:atomic:PERSON");
        let a = crate::jcs::canonicalize(&proof::signing_view(&project(&input)));
        let b = crate::jcs::canonicalize(&proof::signing_view(&project(&input)));
        assert_eq!(a, b);
    }

    #[test]
    fn attest_prov_then_verify_prov_roundtrips() {
        let (id, kp) = dev_identity();
        let person = crate::did::did_for_public_key(&id.public_key);
        let attested = attest_prov(&sample_input(&person), &id, &kp);

        let obj = attested.as_object().unwrap();
        assert!(obj
            .get(PROP_CONTENT_HASH)
            .and_then(Value::as_str)
            .unwrap()
            .starts_with("blake3:"));
        assert!(obj.get(PROP_PROOF).is_some());
        // Accepted injection: top-level attributedTo == the signer's Person did.
        assert_eq!(
            obj.get(PROP_ATTRIBUTED_TO).and_then(Value::as_str),
            Some(person.as_str())
        );

        // Verifies with the right key.
        verify_prov(&attested, &kp.public).expect("verify should succeed");

        // Tamper a signed field -> HashMismatch.
        let mut tampered = attested.clone();
        tampered
            .as_object_mut()
            .unwrap()
            .insert("@id".into(), Value::String("urn:atomic:provgraph:EVIL".into()));
        let err = verify_prov(&tampered, &kp.public).unwrap_err();
        assert!(
            matches!(err, CanonicalError::HashMismatch { .. }),
            "expected HashMismatch, got {err:?}"
        );

        // Wrong key -> verify error.
        let other = KeyPair::generate();
        assert!(verify_prov(&attested, &other.public).is_err());
    }

    #[test]
    fn slug_normalizes_like_agent_name() {
        assert_eq!(normalize_agent_slug("claude-code"), "claude");
        assert_eq!(normalize_agent_slug("Claude-Code"), "claude");
        assert_eq!(normalize_agent_slug("codex"), "codex");
        assert_eq!(normalize_agent_slug(""), "");
    }

    /// Every bare key the projection emits — including the signed envelope
    /// (`attributedTo`/`contentHash`/`proof`/…) — must be defined in the shipped
    /// `@context`, so the emitted JSON is real, resolvable RDF (the closed-registry
    /// discipline applied to the PROV projection).
    #[test]
    fn every_projected_key_is_defined_in_the_context() {
        let (id, kp) = dev_identity();
        let person = crate::did::did_for_public_key(&id.public_key);
        let signed = attest_prov(&sample_input(&person), &id, &kp);
        let undefined = crate::context::undefined_terms(&signed);
        assert!(
            undefined.is_empty(),
            "these projected keys have no @context term: {undefined:?}"
        );
    }
}
