//! Present-day review of retained evidence, never an original capture claim.
use crate::node::Proof;
use serde::{Deserialize, Serialize};

pub const LEGACY_SCHEMA: &str = "atomic.publication-review.v1";
pub const SCHEMA: &str = "atomic.publication-review.v2";
pub const MAX_BYTES: usize = 1024 * 1024;

/// Exact signed intent revision and the source inputs it must still describe.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntentPin {
    pub id: String,
    pub source_hash: String,
    pub signed_hash: String,
    pub public_key: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangePin {
    pub hash: String,
    pub session: Option<String>,
    /// Reserved legacy field. The review resolver never derives authority from it.
    pub governing_intents: Vec<String>,
    pub provenance: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observations: Vec<IntentObservationPin>,
}

/// A source-pinned observation, not a claim of original governing authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntentObservationPin {
    #[serde(default, skip_serializing_if = "ObservationRole::is_work")]
    pub role: ObservationRole,
    pub provenance: String,
    pub node: String,
    pub session: String,
    pub turns: Vec<u32>,
    pub kind: String,
    /// Exact receipt identifier; not a filename or timestamp inference.
    pub reference: String,
    pub intent: Option<String>,
    /// resolved, unresolved, or ambiguous; none means no generating-turn binding.
    pub resolution: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
    /// Independently verified retained signed document. Historical does not mean
    /// fresh current source, and this is not an implementation authorization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retained_revision: Option<SignedRevisionPin>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedRevisionPin {
    pub signed_hash: String,
    pub public_key: String,
    pub source_relation: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ObservationRole {
    #[default]
    Work,
    /// Only the exact selected review intent, never arbitrary observed work.
    ReviewContext,
}

impl ObservationRole {
    fn is_work(&self) -> bool {
        matches!(self, Self::Work)
    }
}

/// Immutable approval scope. Every digest participates in the outer signature.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewSubject {
    pub project: String,
    pub roots: Vec<String>,
    pub changes: Vec<ChangePin>,
    pub review: IntentPin,
    pub work: Vec<IntentPin>,
    /// Independently reviewed terminal dispositions, not approved implementations.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub set_aside: Vec<IntentPin>,
    pub profile_hash: String,
    /// Selected perspective and all inherited state pins; unrelated reviewer
    /// artifact records should live on a separate review view.
    pub view_chain: Vec<ReviewViewPin>,
    /// Effective membership identity, including inherited changes.
    pub effective_set_id: String,
    /// Signed, view-independent source context. Names above are historical
    /// labels; this evidence, not continued view existence, proves the context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<ReviewSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewSnapshot {
    pub version: u32,
    /// Ordered own-log hashes corresponding one-for-one to view_chain.
    pub view_logs: Vec<Vec<String>>,
    /// Sorted unique full effective projection closure, including ancestors.
    pub membership: Vec<String>,
    pub intent_revisions: Vec<RetainedIntentRevision>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedIntentRevision {
    pub id: String,
    pub frontmatter: serde_json::Map<String, serde_json::Value>,
    pub body: String,
    pub document: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewViewPin {
    pub name: String,
    pub state: String,
    pub scope: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationReview {
    pub schema: String,
    /// This timestamp means review now, not capture in the original session.
    pub reviewed_at: String,
    pub subject: ReviewSubject,
    /// Authenticated renewal; old signed objects remain retained and inspectable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supersedes: Vec<String>,
    #[serde(rename = "attributedTo")]
    pub attributed_to: String,
    #[serde(
        rename = "contentHash",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub content_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proof: Option<Proof>,
}

impl PublicationReview {
    /// Admit untrusted bytes before parsing, including duplicate-key/depth checks.
    pub fn parse(bytes: &[u8]) -> crate::Result<Self> {
        if bytes.len() > MAX_BYTES {
            return Err(crate::CanonicalError::Proof(
                "publication review exceeds 1 MiB".into(),
            ));
        }
        let review: Self =
            serde_json::from_value(crate::jcs::admit_document(bytes)?).map_err(|error| {
                crate::CanonicalError::Proof(format!("invalid publication review: {error}"))
            })?;
        if review.schema != SCHEMA && review.schema != LEGACY_SCHEMA {
            return Err(crate::CanonicalError::Proof(
                "unsupported publication review schema".into(),
            ));
        }
        if review.schema == SCHEMA && review.subject.snapshot.is_none() {
            return Err(crate::CanonicalError::Proof(
                "v2 publication review requires retained source evidence".into(),
            ));
        }
        if review.schema == LEGACY_SCHEMA && review.subject.snapshot.is_some() {
            return Err(crate::CanonicalError::Proof(
                "legacy publication review cannot claim v2 snapshot semantics".into(),
            ));
        }
        Ok(review)
    }

    /// The exact document passed through Atomic's common signing path.
    pub fn value(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("review serialization")
    }

    /// Content address of the entire signed artifact, including its proof.
    pub fn artifact_hash(&self) -> String {
        let mut hash = blake3::Hasher::new();
        hash.update(self.schema.as_bytes());
        hash.update(b"\0");
        hash.update(crate::jcs::canonicalize(&self.value()).as_bytes());
        hash.finalize().to_hex().to_string()
    }
}
