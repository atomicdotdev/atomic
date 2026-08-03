//! # atomic-canonical — Recording the Why
//!
//! The canonical-record layer for Atomic's "record the why" design. It turns an
//! authored surface (markdown frontmatter + container directives) into a
//! **canonical typed node** (JSON-LD), hashes it deterministically
//! (JCS + BLAKE3), signs it (Ed25519 Data Integrity proof), gates it
//! (SHACL-style, closed-world), and renders it back (a pure projection).
//!
//! Three layers, kept apart:
//! - **surface**  — markdown the author writes (frontmatter spine + directives)
//! - **canonical** — the typed node that gets hashed and signed ([`node`])
//! - **render**   — the read-time projection ([`render`](mod@render))
//!
//! Load-bearing principles enforced here: *presence is enforced, content is
//! left honest* (the `why` is never schematized); *the vocabulary is closed*
//! ([`vocab`]); *the fact is singular* (status lives only in the spine and is
//! regenerated on render).
//!
//! ## M0 scope
//! This crate proves the Intent round trip **in isolation**. Wiring it into the
//! vault store — redefining `content_hash`, re-plumbing drift detection and the
//! manifest merkle, the CLI verbs — is M1 and deliberately not done here, so the
//! existing vault stays stable while the engine is validated.
//!
//! ## Security caveat (M0)
//! Signing uses atomic's existing Ed25519 identities, whose secret keys are
//! currently stored **unencrypted** on disk. M0 treats signing identities as
//! **non-production dev identities**. Key-at-rest encryption must land before
//! any attested node is trusted in a real/shared setting.

pub mod context;
pub mod did;
pub mod directive;
pub mod error;
pub mod gate;
pub mod hash;
pub mod jcs;
pub mod lift;
pub mod memory;
pub mod node;
pub mod proof;
pub mod prov;
pub mod render;
pub mod triage_ref;
pub mod vocab;

pub use error::{CanonicalError, Result};
pub use gate::{validate_intent, validate_memory, ValidationReport, Violation};
pub use memory::MemoryNode;
pub use node::{
    intent_substance_hash, AcceptanceCriterion, CanonicalNode, Constraint, Proof, Ref, ScopeItem,
    Task,
};
pub use prov::{attest_prov, project, verify_prov, ProvActivityInput};
pub use render::{render, render_memory, Target};
pub use triage_ref::{triage_reference, verify_triage_reference, TriagePins};

use atomic_identity::identity::Identity;
use atomic_identity::keypair::KeyPair;
use serde_json::{Map, Value};

/// Lift an authored intent (frontmatter + body) into a canonical node,
/// attest it (hash + sign) with the given identity, and return the node.
/// The node is *not* gated here — call [`validate_intent`] to gate it.
pub fn lift_and_attest(
    frontmatter: &Map<String, Value>,
    body: &str,
    identity: &Identity,
    keypair: &KeyPair,
) -> Result<CanonicalNode> {
    let node = lift::lift_intent(frontmatter, body)?;
    Ok(proof::attest(node, identity, keypair))
}

/// Verify an attested node's content hash and signature against a public key.
pub fn verify(
    node: &CanonicalNode,
    public_key: &atomic_identity::keypair::PublicKey,
) -> Result<()> {
    proof::verify(node, public_key)
}

/// Lift an authored memory (frontmatter + body) into a canonical node, attest
/// it (hash + sign) with the given identity, and return the node. Mirrors
/// [`lift_and_attest`] and signs through the same single path. The node is
/// *not* gated here — call [`validate_memory`] to gate it.
pub fn lift_and_attest_memory(
    frontmatter: &Map<String, Value>,
    body: &str,
    identity: &Identity,
    keypair: &KeyPair,
) -> Result<MemoryNode> {
    let node = memory::lift_memory(frontmatter, body)?;
    Ok(proof::attest_memory(node, identity, keypair))
}

/// Verify an attested memory's content hash and signature against a public key.
pub fn verify_memory(
    node: &MemoryNode,
    public_key: &atomic_identity::keypair::PublicKey,
) -> Result<()> {
    proof::verify_memory(node, public_key)
}
