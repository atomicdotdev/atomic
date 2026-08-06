//! Attestation dual-read for intents and memories.
//!
//! An attestation is stored as a first-class tracked vault entry
//! (`attestations/<id>/attested.md` for intents,
//! `attestations/memory/<id>/attested.md` for memories) whose body is the
//! signed JSON-LD node. Pre-upgrade attestations live in a legacy sidecar
//! under `<dot_dir>/canonical/…/attested.jsonld`. Reads prefer the tracked
//! entry and fall back to the sidecar, classifying the result against the
//! current source content (Fresh vs Stale). Malformed artifacts fail open
//! (logged, treated as absent) so a read still works on the raw entry.

use std::path::PathBuf;

use atomic_canonical::{CanonicalNode, MemoryNode};
use atomic_core::pristine::VaultEntry;
use atomic_repository::Repository;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::{FacadeError, FacadeResult};

/// The two lift inputs pulled off a stored `VaultEntry`.
#[derive(Debug, Clone)]
pub struct LiftInputs {
    /// The parsed frontmatter spine (a JSON object).
    pub frontmatter: Map<String, Value>,
    /// The markdown body (without any frontmatter block).
    pub body: String,
}

/// Parse a `VaultEntry` into the frontmatter map + body the lift consumes.
pub fn inputs_from_entry(kind: &'static str, entry: &VaultEntry) -> FacadeResult<LiftInputs> {
    let frontmatter: Map<String, Value> =
        serde_json::from_str(&entry.frontmatter_json).map_err(|e| FacadeError::Malformed {
            message: format!("{kind} frontmatter is not valid JSON: {e}"),
        })?;
    let body = String::from_utf8_lossy(&entry.content_bytes).into_owned();
    Ok(LiftInputs { frontmatter, body })
}

/// Digest of (frontmatter + body) recorded at attest time; recomputed on read
/// to classify an attestation as Fresh or Stale. NOT the vault content hash.
pub fn source_content_hash(inputs: &LiftInputs) -> String {
    let fm = serde_json::to_string(&inputs.frontmatter).unwrap_or_default();
    let mut hasher = blake3::Hasher::new();
    hasher.update(fm.as_bytes());
    hasher.update(b"\0");
    hasher.update(inputs.body.as_bytes());
    format!("blake3:{}", hasher.finalize().to_hex())
}

/// Freshness of an attestation relative to the current source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AttestationStatus {
    /// No attestation on disk (tracked or legacy).
    None,
    /// Attestation present; recorded source hash matches the current source.
    Fresh,
    /// Attestation present but the source changed since it was signed.
    Stale,
}

/// A loaded attestation: the signed node plus its freshness classification.
#[derive(Debug)]
pub enum Attested<N> {
    /// No attestation found.
    None,
    /// Present and matching the current source.
    Fresh(Box<N>),
    /// Present but the source changed since signing.
    Stale(Box<N>),
}

impl<N> Attested<N> {
    /// The serializable status.
    pub fn status(&self) -> AttestationStatus {
        match self {
            Self::None => AttestationStatus::None,
            Self::Fresh(_) => AttestationStatus::Fresh,
            Self::Stale(_) => AttestationStatus::Stale,
        }
    }

    /// The signed node, if present.
    pub fn node(&self) -> Option<&N> {
        match self {
            Self::None => None,
            Self::Fresh(n) | Self::Stale(n) => Some(n),
        }
    }
}

/// Sanitize an id for use as a directory name in attestation paths. Keeps
/// ASCII alphanumerics, `-` and `_`; everything else becomes `_`.
pub fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

// ── Intent attestations ─────────────────────────────────────────────────

/// Normalize an intent id via the repository resolver, falling back to the
/// uppercased raw arg when nothing resolves (matches the CLI's `attest`).
pub fn normalized_intent_id(repo: &Repository, id: &str) -> String {
    repo.resolve_intent_key(id)
        .unwrap_or_else(|_| id.to_uppercase())
}

/// Tracked-vault path of an intent's attestation.
pub fn intent_attestation_vault_path(repo: &Repository, id: &str) -> String {
    format!(
        "attestations/{}/attested.md",
        sanitize_id(&normalized_intent_id(repo, id))
    )
}

/// Load an intent's attestation, preferring the tracked vault entry and
/// falling back to the legacy sidecar(s).
pub fn load_intent_attestation(
    repo: &Repository,
    id: &str,
    inputs: &LiftInputs,
) -> FacadeResult<Attested<CanonicalNode>> {
    let vpath = intent_attestation_vault_path(repo, id);
    if let Some(found) = load_tracked(repo, &vpath, inputs)? {
        return Ok(found);
    }

    let normalized = intent_sidecar_path(repo, &normalized_intent_id(repo, id));
    let raw = intent_sidecar_path(repo, id);
    let candidates = if raw == normalized {
        vec![normalized]
    } else {
        vec![normalized, raw]
    };
    load_legacy_sidecar(&candidates, inputs)
}

fn intent_sidecar_path(repo: &Repository, id: &str) -> PathBuf {
    repo.dot_dir()
        .join("canonical")
        .join("intents")
        .join(sanitize_id(id))
        .join("attested.jsonld")
}

// ── Memory attestations ─────────────────────────────────────────────────

/// Normalize a bare id, `memory/<id>`, `<id>.md`, or `memory/<id>.md` to the
/// canonical vault path `memory/<id>.md`.
pub fn normalize_memory_path(id_or_path: &str) -> String {
    let without_prefix = id_or_path.strip_prefix("memory/").unwrap_or(id_or_path);
    let stem = without_prefix.strip_suffix(".md").unwrap_or(without_prefix);
    format!("memory/{stem}.md")
}

/// The `<id>` stem for any accepted memory path form.
pub fn memory_id(id_or_path: &str) -> String {
    let path = normalize_memory_path(id_or_path);
    path.strip_prefix("memory/")
        .and_then(|s| s.strip_suffix(".md"))
        .unwrap_or(&path)
        .to_string()
}

/// Tracked-vault path of a memory's attestation (namespaced under
/// `attestations/memory/` so memory ids never collide with intent keys).
pub fn memory_attestation_vault_path(id_or_path: &str) -> String {
    format!(
        "attestations/memory/{}/attested.md",
        sanitize_id(&memory_id(id_or_path))
    )
}

/// Load a memory's attestation, preferring the tracked vault entry and
/// falling back to the legacy sidecar.
pub fn load_memory_attestation(
    repo: &Repository,
    id_or_path: &str,
    inputs: &LiftInputs,
) -> FacadeResult<Attested<MemoryNode>> {
    let vpath = memory_attestation_vault_path(id_or_path);
    if let Some(found) = load_tracked(repo, &vpath, inputs)? {
        return Ok(found);
    }

    let sidecar = repo
        .dot_dir()
        .join("canonical")
        .join("memory")
        .join(sanitize_id(&memory_id(id_or_path)))
        .join("attested.jsonld");
    load_legacy_sidecar(&[sidecar], inputs)
}

// ── Shared read machinery ───────────────────────────────────────────────

/// Read + classify the tracked vault entry at `vpath`. `Ok(None)` means "no
/// usable tracked entry — try the legacy sidecar".
fn load_tracked<N: DeserializeOwned>(
    repo: &Repository,
    vpath: &str,
    inputs: &LiftInputs,
) -> FacadeResult<Option<Attested<N>>> {
    let Some(entry) = repo.vault_retrieve(vpath)? else {
        return Ok(None);
    };

    let raw = String::from_utf8_lossy(&entry.content_bytes);
    let node: N = match serde_json::from_str(raw.trim_end()) {
        Ok(node) => node,
        Err(e) => {
            log::warn!("ignoring malformed tracked attestation {vpath}: {e}");
            return Ok(None);
        }
    };

    let recorded = serde_json::from_str::<Value>(&entry.frontmatter_json)
        .ok()
        .and_then(|v| {
            v.get("sourceContentHash")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    Ok(Some(classify(node, recorded.as_deref(), inputs)))
}

/// Read + classify the first existing legacy sidecar. Missing/corrupt
/// sidecars fail open to `Attested::None`.
fn load_legacy_sidecar<N: DeserializeOwned>(
    candidates: &[PathBuf],
    inputs: &LiftInputs,
) -> FacadeResult<Attested<N>> {
    let Some(path) = candidates.iter().find(|p| p.exists()) else {
        return Ok(Attested::None);
    };

    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) => {
            log::warn!("ignoring unreadable attestation sidecar {}: {e}", path.display());
            return Ok(Attested::None);
        }
    };
    let artifact: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("ignoring unparsable attestation sidecar {}: {e}", path.display());
            return Ok(Attested::None);
        }
    };
    let Some(node_val) = artifact.get("node").cloned() else {
        log::warn!("attestation sidecar {} has no 'node' — ignoring", path.display());
        return Ok(Attested::None);
    };
    let node: N = match serde_json::from_value(node_val) {
        Ok(n) => n,
        Err(e) => {
            log::warn!("ignoring malformed attestation node in {}: {e}", path.display());
            return Ok(Attested::None);
        }
    };

    let recorded = artifact
        .get("source")
        .and_then(|s| s.get("sourceContentHash"))
        .and_then(Value::as_str);
    Ok(classify(node, recorded, inputs))
}

fn classify<N>(node: N, recorded: Option<&str>, inputs: &LiftInputs) -> Attested<N> {
    let current = source_content_hash(inputs);
    match recorded {
        Some(h) if h == current => Attested::Fresh(Box::new(node)),
        _ => Attested::Stale(Box::new(node)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_path_normalization() {
        for form in ["notes", "notes.md", "memory/notes", "memory/notes.md"] {
            assert_eq!(normalize_memory_path(form), "memory/notes.md");
            assert_eq!(memory_id(form), "notes");
        }
    }

    #[test]
    fn sanitize_replaces_non_identifier_chars() {
        assert_eq!(sanitize_id("PIMO::user::1"), "PIMO__user__1");
        assert_eq!(sanitize_id("ok-id_2"), "ok-id_2");
    }

    #[test]
    fn attestation_status_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&AttestationStatus::Fresh).unwrap(),
            "\"fresh\""
        );
        assert_eq!(
            serde_json::to_string(&AttestationStatus::None).unwrap(),
            "\"none\""
        );
    }
}
