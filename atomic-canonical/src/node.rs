//! The canonical typed node — RtW's canonical form, serialized as JSON-LD.
//!
//! This is a *compile target*, not a hand-authored artifact (the lift produces
//! it from markdown+directives). JSON-LD because `@type` gives node types,
//! `@id` gives stable identity, reference properties are first-class edges, and
//! it is RDF the PROV/regulator toolchain already speaks.
//!
//! The `why` is an unconstrained prose slot: the gate checks that it is
//! present, never what it says ("presence enforced, content honest").

use serde::{Deserialize, Serialize};

use crate::hash::content_hash;
use crate::jcs;

pub const CONTEXT_URL: &str = "https://atomic.dev/ns/ctx.jsonld";

/// A typed, refutable, merkle-pinned verification record embedded on an
/// acceptance criterion. `evidence` is not a one-shot boolean: an AC accumulates
/// these records, each a fact about a materialized state (`observedAtMerkle`)
/// that either `pass`ed or `fail`ed a given `kind` of check at a given `scope`.
///
/// This is an embedded sub-object (like [`Proof`]), *not* a `@type` in the node
/// registry — it never stands alone as a graph node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationRecord {
    #[serde(rename = "@type")]
    pub type_: String,
    /// One of [`crate::vocab::VERIFICATION_KIND`].
    pub kind: String,
    /// One of [`crate::vocab::OUTCOME`].
    pub outcome: String,
    /// One of [`crate::vocab::VERIFICATION_SCOPE`].
    pub scope: String,
    /// The view Merkle the record is a fact about (the materialized state).
    #[serde(rename = "observedAtMerkle")]
    pub observed_at_merkle: String,
    /// The anchor: a change hash, test id, or similar. Serialized as `ref`.
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// A free-form note (e.g. the manual observation that caught an escape).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation: Option<String>,
}

/// A single acceptance criterion, lifted from a `:::acceptance-criterion` directive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceCriterion {
    #[serde(rename = "@type")]
    pub type_: String,
    #[serde(rename = "@id")]
    pub id: String,
    pub text: String,
    #[serde(rename = "acStatus")]
    pub ac_status: String,
    #[serde(
        rename = "verifiedBy",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub verified_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    /// Accumulated verification records (empty is omitted, so existing hashes
    /// are byte-identical when there are none).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verifications: Vec<VerificationRecord>,
    /// The verification kinds this criterion requires to be `met` (a later
    /// milestone derives `acStatus` from these). Empty is omitted.
    #[serde(
        rename = "requiredKinds",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub required_kinds: Vec<String>,
}

/// A scope boundary item, lifted from a `:::scope-in` / `:::scope-out`
/// directive. The prose is the unconstrained narrative; the gate enforces
/// presence, never content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeItem {
    #[serde(rename = "@type")]
    pub type_: String,
    #[serde(rename = "@id")]
    pub id: String,
    pub text: String,
    /// File paths this boundary names, lifted from nested `::file-ref{path=}`
    /// leaves. On a `:::scope-out` these are the files declared out of scope
    /// (projected as `SCOPE_OUT_FILE` edges for breach detection). Empty is
    /// omitted, so a scope item without file-refs serializes byte-identically
    /// to before — and, being part of the reviewable boundary definition, these
    /// paths correctly participate in `intentSubstanceHash`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
}

/// A constraint the implementation must respect, lifted from a `:::constraint`
/// directive. Prose body; typed so the graph knows how many there are.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Constraint {
    #[serde(rename = "@type")]
    pub type_: String,
    #[serde(rename = "@id")]
    pub id: String,
    pub text: String,
}

/// A typed dependency edge leaf, lifted from a `:::ref{to= edge=}` directive.
/// `@type`/`@id` are omitted when absent (the doc's `:::ref` carries only
/// `to`/`edge`), so no empty JSON-LD keys leak into the hash.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ref {
    #[serde(rename = "@type", default, skip_serializing_if = "Option::is_none")]
    pub type_: Option<String>,
    #[serde(rename = "@id", default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub to: String,
    pub edge: String,
}

/// A decomposed task, lifted from a `:::task` directive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    #[serde(rename = "@type")]
    pub type_: String,
    #[serde(rename = "@id")]
    pub id: String,
    pub text: String,
    #[serde(rename = "taskStatus")]
    pub task_status: String,
    #[serde(rename = "touchesFile", default, skip_serializing_if = "Vec::is_empty")]
    pub touches_file: Vec<String>,
    /// The acceptance criteria this task fulfills. A task may satisfy more than
    /// one criterion, so this is a list of `urn:atomic:ac:*` ids, not a scalar.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub satisfies: Vec<String>,
}

/// A Data Integrity proof (`eddsa-jcs-2022`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Proof {
    #[serde(rename = "@type")]
    pub type_: String,
    pub cryptosuite: String,
    #[serde(rename = "verificationMethod")]
    pub verification_method: String,
    #[serde(rename = "proofPurpose")]
    pub proof_purpose: String,
    #[serde(rename = "proofValue")]
    pub proof_value: String,
}

/// The default intent classification. An ordinary unit of work is a `"feature"`.
pub fn default_kind() -> String {
    "feature".to_string()
}

/// Whether a `kind` holds the default value. When it does, the field is omitted
/// from serialization entirely, so an ordinary intent's JSON (and its
/// content/substance hash) is byte-identical to the pre-`kind` shape.
pub fn kind_is_default(kind: &str) -> bool {
    kind == "feature"
}

/// The canonical Intent node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalNode {
    #[serde(rename = "@context")]
    pub context: String,
    #[serde(rename = "@type")]
    pub type_: String,
    #[serde(rename = "@id")]
    pub id: String,

    /// Human-facing key (e.g. "WORD-5"); a display alias over `@id`.
    #[serde(rename = "humanKey")]
    pub human_key: String,
    pub title: String,
    pub status: String,
    /// Classification discriminator (an [`crate::vocab::INTENT_KIND`] member),
    /// separate from the JSON-LD `@type` (which stays `"Intent"`). Authored via
    /// the frontmatter `kind` key; defaults to `"feature"`. Omitted from JSON
    /// when it holds the default, so ordinary intents keep their exact byte
    /// shape and hashes.
    #[serde(default = "default_kind", skip_serializing_if = "kind_is_default")]
    pub kind: String,
    // Every field that is omitted when empty carries `default` so the
    // serde round trip (`to_value` → `from_value`) the typed attest/verify
    // wrappers rely on is symmetric: an omitted key deserializes back to its
    // empty value rather than erroring on a "missing field".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<String>,

    #[serde(
        rename = "motivatedBy",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub motivated_by: Option<String>,
    #[serde(rename = "informedBy", default, skip_serializing_if = "Vec::is_empty")]
    pub informed_by: Vec<String>,

    #[serde(
        rename = "hasAcceptanceCriterion",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub has_acceptance_criterion: Vec<AcceptanceCriterion>,
    #[serde(rename = "hasTask", default, skip_serializing_if = "Vec::is_empty")]
    pub has_task: Vec<Task>,

    // Collection sub-nodes. Empty collections are omitted (skip_serializing_if),
    // so adding these fields does NOT change the hash of existing fixtures.
    #[serde(rename = "hasScopeIn", default, skip_serializing_if = "Vec::is_empty")]
    pub has_scope_in: Vec<ScopeItem>,
    #[serde(rename = "hasScopeOut", default, skip_serializing_if = "Vec::is_empty")]
    pub has_scope_out: Vec<ScopeItem>,
    #[serde(
        rename = "hasConstraint",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub has_constraint: Vec<Constraint>,
    #[serde(rename = "dependsOn", default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<Ref>,

    /// The unconstrained reason. Presence enforced by the gate; content honest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,

    #[serde(
        rename = "contentHash",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub content_hash: Option<String>,
    #[serde(
        rename = "attributedTo",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub attributed_to: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proof: Option<Proof>,
}

impl CanonicalNode {
    /// The node as a JSON value (JSON-LD object).
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("canonical node serialization is infallible")
    }

    /// The value used for signing: everything except the `proof`. Delegates to
    /// [`crate::proof::signing_view`] so the excluded key-set is defined once,
    /// at the Value level, shared by every node type.
    pub fn signing_value(&self) -> serde_json::Value {
        crate::proof::signing_view(&self.to_value())
    }

    /// The value used for the content hash: excludes both `proof` and
    /// `contentHash`. Delegates to [`crate::proof::hashing_view`].
    pub fn hashing_value(&self) -> serde_json::Value {
        crate::proof::hashing_view(&self.to_value())
    }

    /// Canonical bytes for signing (JCS of `signing_value`).
    pub fn signing_bytes(&self) -> Vec<u8> {
        jcs::canonicalize(&self.signing_value()).into_bytes()
    }

    /// Recompute the content hash from the current node state.
    pub fn compute_content_hash(&self) -> String {
        content_hash(&self.hashing_value())
    }

    /// The value used for the substance hash: the reviewable definition with all
    /// review state removed. Delegates to [`crate::proof::substance_view`].
    pub fn substance_value(&self) -> serde_json::Value {
        crate::proof::substance_view(&self.to_value())
    }
}

/// The `intentSubstanceHash` — a `blake3:<hex>` hash of the intent's reviewable
/// definition ([`CanonicalNode::substance_value`]), stable across review
/// activity. Mirrors the [`content_hash`] path exactly (JCS + BLAKE3), so the
/// two can never drift; the difference is only *what* is hashed.
pub fn intent_substance_hash(node: &CanonicalNode) -> String {
    content_hash(&node.substance_value())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ac(id: &str, text: &str) -> AcceptanceCriterion {
        AcceptanceCriterion {
            type_: "AcceptanceCriterion".to_string(),
            id: id.to_string(),
            text: text.to_string(),
            ac_status: "unmet".to_string(),
            verified_by: None,
            evidence: None,
            verifications: Vec::new(),
            required_kinds: Vec::new(),
        }
    }

    fn node() -> CanonicalNode {
        CanonicalNode {
            context: CONTEXT_URL.to_string(),
            type_: "Intent".to_string(),
            id: "urn:atomic:intent:sub-1".to_string(),
            human_key: "SUB-1".to_string(),
            title: "Substance hash fixture".to_string(),
            status: "todo".to_string(),
            kind: default_kind(),
            priority: None,
            view: None,
            motivated_by: None,
            informed_by: Vec::new(),
            has_acceptance_criterion: vec![ac("urn:atomic:ac:sub-1-ac-1", "a checkable outcome")],
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
        }
    }

    #[test]
    fn substance_hash_ignores_review_state() {
        let base = intent_substance_hash(&node());

        // Intent-level status is review state — hash holds.
        let mut status = node();
        status.status = "done".to_string();
        assert_eq!(
            intent_substance_hash(&status),
            base,
            "status must not move substance"
        );

        // Per-AC review state (acStatus / verifiedBy / evidence) — hash holds.
        let mut reviewed = node();
        reviewed.has_acceptance_criterion[0].ac_status = "met".to_string();
        reviewed.has_acceptance_criterion[0].verified_by = Some("did:atomic:lee".to_string());
        reviewed.has_acceptance_criterion[0].evidence = Some("urn:atomic:change:01J8".to_string());
        assert_eq!(
            intent_substance_hash(&reviewed),
            base,
            "AC review state must not move substance"
        );

        // Accumulating a verification record — hash holds.
        let mut verified = node();
        verified.has_acceptance_criterion[0]
            .verifications
            .push(VerificationRecord {
                type_: "VerificationRecord".to_string(),
                kind: "manual".to_string(),
                outcome: "fail".to_string(),
                scope: "view".to_string(),
                observed_at_merkle: "ABC".to_string(),
                reference: None,
                observation: None,
            });
        assert_eq!(
            intent_substance_hash(&verified),
            base,
            "verification records must not move substance"
        );
    }

    #[test]
    fn substance_hash_tracks_the_definition() {
        let base = intent_substance_hash(&node());

        // Editing a criterion's text changes the definition — hash moves.
        let mut edited = node();
        edited.has_acceptance_criterion[0].text = "a different outcome".to_string();
        assert_ne!(
            intent_substance_hash(&edited),
            base,
            "AC text must move substance"
        );

        // Adding a task changes the definition — hash moves.
        let mut tasked = node();
        tasked.has_task.push(Task {
            type_: "Task".to_string(),
            id: "urn:atomic:task:sub-1-1".to_string(),
            text: "do the thing".to_string(),
            task_status: "open".to_string(),
            touches_file: Vec::new(),
            satisfies: Vec::new(),
        });
        assert_ne!(
            intent_substance_hash(&tasked),
            base,
            "a new task must move substance"
        );

        // requiredKinds is part of the AC *definition* (the verification bar),
        // not review state — changing it moves the hash.
        let mut required = node();
        required.has_acceptance_criterion[0].required_kinds = vec!["unit".to_string()];
        let with_one = intent_substance_hash(&required);
        assert_ne!(
            with_one, base,
            "adding a requiredKinds entry must move substance"
        );

        required.has_acceptance_criterion[0]
            .required_kinds
            .push("e2e".to_string());
        assert_ne!(
            intent_substance_hash(&required),
            with_one,
            "changing requiredKinds must move substance"
        );

        // A scope-out `::file-ref` is part of the reviewable boundary
        // definition, so declaring a file out of scope moves the substance hash
        // (it is NOT excluded by substance_view). The empty case is unchanged.
        let mut scoped = node();
        scoped.has_scope_out.push(ScopeItem {
            type_: "ScopeItem".to_string(),
            id: "urn:atomic:scope:sub-1-scope-out-1".to_string(),
            text: "billing is off limits".to_string(),
            files: Vec::new(),
        });
        let with_prose_scope = intent_substance_hash(&scoped);
        scoped.has_scope_out[0]
            .files
            .push("src/billing.rs".to_string());
        assert_ne!(
            intent_substance_hash(&scoped),
            with_prose_scope,
            "a scope-out file-ref must move substance (it defines the boundary)"
        );
    }

    /// The `kind` field is additive and hash-neutral for ordinary intents: the
    /// default `"intent"` is omitted from JSON, so both the content hash and the
    /// substance hash are byte-identical to the pre-`kind` shape. A `"review"`
    /// kind serializes and therefore moves both hashes (it is a distinct
    /// definition), proving the field is real — not silently dropped.
    #[test]
    fn default_kind_is_omitted_and_hash_stable() {
        let n = node();
        assert_eq!(n.kind, "feature");

        // Proof of byte-identity: an ordinary intent carries NO `kind` key, so
        // its JSON (and every hash derived from it) matches the pre-`kind`
        // fixture exactly.
        let value = n.to_value();
        assert!(
            value.as_object().unwrap().get("kind").is_none(),
            "default kind must not appear in the canonical JSON"
        );

        let base_content = n.compute_content_hash();
        let base_substance = intent_substance_hash(&n);

        // Re-asserting the default kind changes nothing (still omitted).
        let mut still_default = n.clone();
        still_default.kind = default_kind();
        assert_eq!(still_default.compute_content_hash(), base_content);
        assert_eq!(intent_substance_hash(&still_default), base_substance);

        // A review kind IS serialized, and moves both hashes.
        let mut review = n.clone();
        review.kind = "review".to_string();
        assert!(review.to_value().as_object().unwrap().get("kind").is_some());
        assert_ne!(review.compute_content_hash(), base_content);
        assert_ne!(intent_substance_hash(&review), base_substance);
    }
}
