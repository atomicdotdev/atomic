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
pub struct AcceptanceCriterion {
    #[serde(rename = "@type")]
    pub type_: String,
    #[serde(rename = "@id")]
    pub id: String,
    pub text: String,
    #[serde(rename = "acStatus")]
    pub ac_status: String,
    #[serde(rename = "verifiedBy", skip_serializing_if = "Option::is_none")]
    pub verified_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
}

/// A decomposed task, lifted from a `:::task` directive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    #[serde(rename = "@type")]
    pub type_: String,
    #[serde(rename = "@id")]
    pub id: String,
    pub text: String,
    #[serde(rename = "taskStatus")]
    pub task_status: String,
    #[serde(rename = "touchesFile", skip_serializing_if = "Vec::is_empty")]
    pub touches_file: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub satisfies: Option<String>,
}

/// A Data Integrity proof (`eddsa-jcs-2022`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view: Option<String>,

    #[serde(rename = "motivatedBy", skip_serializing_if = "Option::is_none")]
    pub motivated_by: Option<String>,
    #[serde(rename = "informedBy", skip_serializing_if = "Vec::is_empty")]
    pub informed_by: Vec<String>,

    #[serde(rename = "hasAcceptanceCriterion", skip_serializing_if = "Vec::is_empty")]
    pub has_acceptance_criterion: Vec<AcceptanceCriterion>,
    #[serde(rename = "hasTask", skip_serializing_if = "Vec::is_empty")]
    pub has_task: Vec<Task>,

    /// The unconstrained reason. Presence enforced by the gate; content honest.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,

    #[serde(rename = "contentHash", skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    #[serde(rename = "attributedTo", skip_serializing_if = "Option::is_none")]
    pub attributed_to: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub proof: Option<Proof>,
}

impl CanonicalNode {
    /// The node as a JSON value (JSON-LD object).
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("canonical node serialization is infallible")
    }

    /// The value used for signing: everything except the `proof` (a Data
    /// Integrity signature covers the document minus the proof). `contentHash`
    /// is included so the signature also commits to the hash.
    pub fn signing_value(&self) -> serde_json::Value {
        let mut value = self.to_value();
        if let Some(obj) = value.as_object_mut() {
            obj.remove("proof");
        }
        value
    }

    /// The value used for the content hash: excludes both `proof` and
    /// `contentHash` (you cannot hash the hash).
    pub fn hashing_value(&self) -> serde_json::Value {
        let mut value = self.signing_value();
        if let Some(obj) = value.as_object_mut() {
            obj.remove("contentHash");
        }
        value
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
