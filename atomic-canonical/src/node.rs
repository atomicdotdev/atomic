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

/// The acceptance criteria a task fulfills, in whichever JSON shape it was read.
///
/// # Why this is not just `Vec<String>`
///
/// `satisfies` was a scalar `Option<String>` until it was widened to a list (a
/// task may satisfy more than one criterion). Attestations signed before that
/// change therefore carry a bare string:
///
/// ```json
/// { "@id": "urn:atomic:task:t-1", "satisfies": "urn:atomic:ac:t-1-ac-1" }
/// ```
///
/// A plain `Vec<String>` cannot deserialize that, so every pre-widening
/// attestation was rejected outright and its intent silently fell back to
/// "unattested" — discarding signatures that are in fact perfectly valid.
///
/// The tempting fix — normalize `"x"` into `vec!["x"]` on read — is **wrong, and
/// worse than the bug**. Signatures here cover *re-serialized* bytes, not the
/// bytes on disk: [`crate::proof::verify`] calls `to_value()`, recomputes the
/// content hash over `hashing_view`, and checks the signature over
/// `jcs(signing_view)`. Rewriting a scalar into a one-element array changes those
/// bytes, so the hash stops matching. And because a verification error surfaces
/// as "signature invalid" rather than "unreadable", normalizing would convert an
/// honest *unknown* into a **false accusation of tampering** against
/// cryptographically sound data.
///
/// So this type is deliberately *representation-preserving*: it remembers which
/// shape it was read in and serializes back into that same shape, leaving the
/// signed bytes byte-for-byte intact. Readers use [`Satisfies::as_slice`] and
/// never learn which variant they hold.
///
/// Newly lifted tasks are always [`Satisfies::Many`] — see the
/// `From<Vec<String>>` impl, the only construction path in the codebase. Do not
/// construct [`Satisfies::One`] elsewhere: it exists solely to round-trip
/// history, and minting a fresh scalar would propagate the old shape forward.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Satisfies {
    /// A single criterion, as pre-widening attestations stored it. Effectively
    /// read-only: preserved across a round-trip, never newly minted.
    One(String),
    /// Zero or more criteria — the current shape, used by every new task.
    Many(Vec<String>),
}

impl Satisfies {
    /// The criteria as a slice, whatever the shape on disk. Callers use this
    /// instead of matching, so the legacy variant stays invisible to them.
    pub fn as_slice(&self) -> &[String] {
        match self {
            // `from_ref` lets a scalar masquerade as a one-element slice without
            // allocating — and, crucially, without rewriting the stored form.
            Self::One(one) => std::slice::from_ref(one),
            Self::Many(many) => many,
        }
    }

    /// True when no criteria are referenced. Drives `skip_serializing_if`, so an
    /// empty list is omitted exactly as it was under the plain `Vec`.
    pub fn is_empty(&self) -> bool {
        self.as_slice().is_empty()
    }
}

impl Default for Satisfies {
    fn default() -> Self {
        Self::Many(Vec::new())
    }
}

impl From<Vec<String>> for Satisfies {
    fn from(many: Vec<String>) -> Self {
        Self::Many(many)
    }
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
    /// The acceptance criteria this task fulfills — a list on new tasks, but
    /// possibly a bare string on attestations signed before the field was
    /// widened. See [`Satisfies`] for why the shape is preserved, not normalized.
    #[serde(default, skip_serializing_if = "Satisfies::is_empty")]
    pub satisfies: Satisfies,
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
}
