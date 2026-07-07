//! The canonical Memory node — durable, reusable context (a constraint, a
//! preference, a lesson, or a piece of context) serialized as JSON-LD.
//!
//! Like [`crate::node::CanonicalNode`], a Memory is a *compile target*, not a
//! hand-authored artifact: [`lift_memory`] produces it from frontmatter + body.
//! It hashes and signs through the **same** single canonicalization path every
//! node type shares ([`crate::proof::signing_view`] / [`crate::proof::hashing_view`]
//! → [`crate::jcs`] → [`crate::hash`]), so a Memory's hash and signature can
//! never drift from an Intent's.
//!
//! ## Directionality (load-bearing)
//! A memory references its *inputs*, never its future uses. The only outward
//! edges are `about` (the modules/domains it is relevant to) and the revision
//! chain (`supersedes` / `previousRevision`). There is deliberately **no**
//! field for the intents that consume it — an intent names the memories that
//! informed it via `informedBy`; a memory never lists its consumers. That
//! absence is what lets memories stay stable while intents accrete around them.
//!
//! ## Single authoring site
//! The memory text has exactly one source. If the body contains a `:::memory`
//! container directive its body is authoritative and the surrounding prose is
//! ignored; only when no container is present does the whole trimmed body
//! become the text. The two are never concatenated, so the same fact can never
//! acquire two sites.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::{CanonicalError, Result};
use crate::hash::content_hash;
use crate::jcs;
use crate::node::{Proof, CONTEXT_URL};
use crate::vocab::NodeType;

/// The exact set of directive names `lift_memory` consumes. The lift and the
/// closed-vocabulary closure test both read this const (as [`crate::lift`] does
/// for the intent lift) so the test cannot rot when the memory lift gains or
/// loses a branch. Every name here must be a member of
/// [`crate::vocab::DIRECTIVE_NAMES`].
pub(crate) const LIFTED_MEMORY_DIRECTIVES: &[&str] = &["memory"];

/// The canonical Memory node (mirrors the doc's Memory example).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryNode {
    #[serde(rename = "@context")]
    pub context: String,
    #[serde(rename = "@type")]
    pub type_: String,
    #[serde(rename = "@id")]
    pub id: String,

    #[serde(rename = "memoryKind")]
    pub memory_kind: String,
    /// The open, unconstrained memory body. Presence enforced by the gate;
    /// content left honest (never graded).
    pub text: String,
    /// INPUT edges only — the modules/services/domains this memory is about.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub about: Vec<String>,
    pub status: String,

    // Revision chain (the only edges besides `about`). Omitted when absent so
    // no empty JSON-LD keys leak into the hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    #[serde(
        rename = "previousRevision",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub previous_revision: Option<String>,

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

impl MemoryNode {
    /// The node as a JSON value (JSON-LD object).
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).expect("memory node serialization is infallible")
    }

    /// The value used for signing: everything except the `proof`. Delegates to
    /// [`crate::proof::signing_view`] so the excluded key-set lives in one
    /// place, shared by every node type.
    pub fn signing_value(&self) -> Value {
        crate::proof::signing_view(&self.to_value())
    }

    /// The value used for the content hash: excludes both `proof` and
    /// `contentHash`. Delegates to [`crate::proof::hashing_view`].
    pub fn hashing_value(&self) -> Value {
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

/// Lift a memory from its frontmatter spine and markdown body.
///
/// `memoryKind`, `status`, `about`, `supersedes`, and `previousRevision` are
/// read from the frontmatter. The text is lifted from the FIRST `:::memory`
/// container directive if present (single authoring site), else falls back to
/// the whole trimmed body.
pub fn lift_memory(frontmatter: &Map<String, Value>, body: &str) -> Result<MemoryNode> {
    crate::directive::check_embedded_directives(body)?;
    let uid = opt_str(frontmatter, "uid")
        .or_else(|| opt_str(frontmatter, "id"))
        .ok_or_else(|| CanonicalError::Lift("memory frontmatter missing 'uid' (or 'id')".into()))?;
    let node_id = as_memory_urn(&uid);

    let memory_kind =
        require_str(frontmatter, "memoryKind").or_else(|_| require_str(frontmatter, "kind"))?;
    let status = opt_str(frontmatter, "status").unwrap_or_else(|| "active".to_string());

    // Single authoring site: the first `:::memory` container's body is
    // authoritative; the surrounding prose is ignored. Only when no container
    // is present does the whole trimmed body become the text — never both.
    //
    // The memory text is OPEN prose (presence enforced, content honest), so we
    // must NOT run it through the closed-vocabulary directive parser: a body
    // that legitimately begins a line with `::` (a Rust turbofish `::<T>`, or
    // prose discussing `:::why`) is valid memory content, not a directive
    // error. We do a lenient fence scan instead, which never rejects.
    let text = extract_memory_container(body).unwrap_or_else(|| body.trim().to_string());

    Ok(MemoryNode {
        context: CONTEXT_URL.to_string(),
        type_: NodeType::Memory.as_str().to_string(),
        id: node_id,
        memory_kind,
        text,
        about: str_array(frontmatter, "about"),
        status,
        supersedes: opt_str(frontmatter, "supersedes"),
        previous_revision: opt_str(frontmatter, "previousRevision"),
        content_hash: None,
        attributed_to: opt_str(frontmatter, "attributedTo"),
        created_at: opt_str(frontmatter, "createdAt")
            .or_else(|| opt_str(frontmatter, "created_at"))
            .unwrap_or_default(),
        proof: None,
    })
}

/// Lenient scan for a single `:::memory` container fence, returning its inner
/// body if present. Unlike the closed-vocabulary [`crate::directive`] parser,
/// this never errors on unrelated `::`/`:::` tokens in the open memory prose —
/// only a recognized fence (a name in [`LIFTED_MEMORY_DIRECTIVES`]) is treated
/// as structure; everything else is content. An unterminated fence falls back
/// to the whole body (fail-open, preserving content).
fn extract_memory_container(body: &str) -> Option<String> {
    let lines: Vec<&str> = body.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        if let Some(name) = container_open_name(lines[i].trim_start()) {
            if LIFTED_MEMORY_DIRECTIVES.contains(&name) {
                let mut collected: Vec<&str> = Vec::new();
                i += 1;
                while i < lines.len() {
                    if lines[i].trim() == ":::" {
                        return Some(collected.join("\n").trim().to_string());
                    }
                    collected.push(lines[i]);
                    i += 1;
                }
                return None; // unterminated → fall back to whole body
            }
        }
        i += 1;
    }
    None
}

/// If `line` opens a container directive (`:::name` optionally with `{...}`),
/// return the directive name. A bare `:::` (a close) yields `None`.
fn container_open_name(line: &str) -> Option<&str> {
    let rest = line.strip_prefix(":::")?;
    let end = rest
        .find(|c: char| c == '{' || c.is_whitespace())
        .unwrap_or(rest.len());
    let name = &rest[..end];
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// Wrap a local id into `urn:atomic:memory:<local>` unless already a urn.
fn as_memory_urn(local: &str) -> String {
    if local.starts_with("urn:atomic:") {
        local.to_string()
    } else {
        format!("urn:atomic:memory:{local}")
    }
}

fn require_str(fm: &Map<String, Value>, key: &str) -> Result<String> {
    opt_str(fm, key)
        .ok_or_else(|| CanonicalError::Lift(format!("memory frontmatter missing '{key}'")))
}

fn opt_str(fm: &Map<String, Value>, key: &str) -> Option<String> {
    match fm.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

fn str_array(fm: &Map<String, Value>, key: &str) -> Vec<String> {
    match fm.get(key) {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        Some(Value::String(s)) if !s.is_empty() => s
            .split(',')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fm() -> Map<String, Value> {
        json!({
            "uid": "01J8ZC4R8T",
            "memoryKind": "constraint",
            "status": "active",
            "about": ["urn:atomic:module:storage", "urn:atomic:module:replication"]
        })
        .as_object()
        .unwrap()
        .clone()
    }

    #[test]
    fn lift_memory_from_frontmatter_and_body() {
        let body = "Multi-region writes must converge with no single ordering authority.";
        let node = lift_memory(&fm(), body).unwrap();
        assert_eq!(node.type_, "Memory");
        assert_eq!(node.id, "urn:atomic:memory:01J8ZC4R8T");
        assert_eq!(node.memory_kind, "constraint");
        assert_eq!(node.status, "active");
        assert_eq!(
            node.about,
            vec![
                "urn:atomic:module:storage".to_string(),
                "urn:atomic:module:replication".to_string()
            ]
        );
        assert!(node.text.contains("no single ordering authority"));
    }

    #[test]
    fn lift_memory_prefers_memory_container() {
        // With a :::memory container, its body is authoritative and the
        // surrounding prose is ignored (single authoring site).
        let body = "\
ignored surrounding prose

:::memory{}
The constraint text that is authoritative.
:::

more ignored prose";
        let node = lift_memory(&fm(), body).unwrap();
        assert_eq!(node.text, "The constraint text that is authoritative.");
        assert!(!node.text.contains("ignored"));

        // Without a container, the whole trimmed body is the text.
        let plain = lift_memory(&fm(), "  just a plain body  ").unwrap();
        assert_eq!(plain.text, "just a plain body");
    }

    #[test]
    fn lift_memory_accepts_kind_alias_and_defaults_status() {
        let mut m = json!({ "uid": "abc", "kind": "lesson" })
            .as_object()
            .unwrap()
            .clone();
        let node = lift_memory(&m, "a lesson").unwrap();
        assert_eq!(node.memory_kind, "lesson");
        assert_eq!(node.status, "active"); // defaulted when absent

        // Missing kind entirely is a lift error.
        m.remove("kind");
        assert!(lift_memory(&m, "x").is_err());
    }
}
