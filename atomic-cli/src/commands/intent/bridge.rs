//! The read→lift bridge and sidecar plumbing shared by the `atomic intent`
//! verbs.
//!
//! # Read bridge
//!
//! A stored intent is read via [`Repository::vault_intent_show`], which returns
//! a `VaultEntry` carrying the two lift inputs:
//!
//! - `frontmatter_json: String` — a JSON *object* string (empty entries are
//!   `"{}"`). This is the pre-parsed frontmatter spine; we feed it straight to
//!   the lift. We do NOT route the body through `parse_markdown` for the vault
//!   path, because the stored `content_bytes` is the body WITHOUT any
//!   frontmatter block and `parse_markdown` would return an empty map.
//! - `content_bytes: Vec<u8>` — the markdown body.
//!
//! The `--path` branch of `validate` reads a full markdown file instead and
//! splits it with [`atomic_canonical::lift::parse_markdown`].
//!
//! # Sidecar persistence (additive)
//!
//! Attestations are written as plain files under
//! `<repo.dot_dir()>/canonical/intents/<sanitized-id>/attested.jsonld`. That is
//! the engine-internal `.atomic/` metadata dir — outside the `.vault/`
//! materialized tree, outside redb, and never part of the manifest merkle. It
//! is therefore invisible to `vault_scan_working_copy`, to drift detection, and
//! to the `atomic view switch` untracked-file wipe (which only clears the
//! working tree). No `VaultEntry` is mutated to store an attestation.

use std::path::PathBuf;

use serde_json::{Map, Value};

use atomic_canonical::lift::lift_intent;
use atomic_canonical::CanonicalNode;
use atomic_core::pristine::VaultEntry;
use atomic_repository::Repository;

use crate::error::{CliError, CliResult};

/// The two lift inputs pulled off a stored `VaultEntry`.
pub struct LiftInputs {
    /// The parsed frontmatter spine (a JSON object).
    pub frontmatter: Map<String, Value>,
    /// The markdown body (without any frontmatter block).
    pub body: String,
}

/// Parse a `VaultEntry` into the frontmatter map + body the lift consumes.
///
/// `frontmatter_json` is parsed exactly as the repository itself parses it
/// (`serde_json::from_str` into a `Map`). A malformed frontmatter string is a
/// clean argument error rather than an internal panic.
pub fn inputs_from_entry(entry: &VaultEntry) -> CliResult<LiftInputs> {
    let frontmatter: Map<String, Value> = serde_json::from_str(&entry.frontmatter_json)
        .map_err(|e| CliError::InvalidArgument {
            message: format!("intent frontmatter is not valid JSON: {e}"),
        })?;
    let body = String::from_utf8_lossy(&entry.content_bytes).into_owned();
    Ok(LiftInputs { frontmatter, body })
}

/// Read an intent by ID from the vault and return its lift inputs.
///
/// `vault_intent_show` normalizes the ID (`"PIMO-1"` / `"pimo-1"` / `"1"`) and
/// resolves the stored entry.
pub fn read_intent(repo: &Repository, id: &str) -> CliResult<LiftInputs> {
    let entry = repo.vault_intent_show(id).map_err(CliError::Repository)?;
    inputs_from_entry(&entry)
}

/// Lift the given inputs into a canonical node, mapping a lift failure (unknown
/// directive, missing `id`, malformed `:::ref`, …) to a clean argument error so
/// the CLI reports it gracefully instead of surfacing an internal error.
pub fn lift(inputs: &LiftInputs) -> CliResult<CanonicalNode> {
    lift_intent(&inputs.frontmatter, &inputs.body).map_err(|e| CliError::InvalidArgument {
        message: format!("could not lift intent: {e}"),
    })
}

/// Sanitize an intent id/key for use as a directory name in the sidecar path.
/// Keeps ASCII alphanumerics, `-` and `_`; everything else becomes `_`.
fn sanitize_id(id: &str) -> String {
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

/// The directory that holds an intent's canonical sidecar artifacts:
/// `<dot_dir>/canonical/intents/<sanitized-id>/`.
pub fn sidecar_dir(repo: &Repository, id: &str) -> PathBuf {
    repo.dot_dir()
        .join("canonical")
        .join("intents")
        .join(sanitize_id(id))
}

/// The attested-node sidecar file for an intent.
pub fn attested_sidecar_path(repo: &Repository, id: &str) -> PathBuf {
    sidecar_dir(repo, id).join("attested.jsonld")
}

/// A hash of the source lift inputs, recorded in the sidecar so a later edit to
/// the intent can be detected as making the attestation stale. This is a
/// standalone digest of (frontmatter + body); it is NOT the vault's
/// `content_hash` and never feeds it. `attest` writes it; `validate`/`show`/
/// `verify` recompute it to decide whether a sidecar is still fresh.
pub fn source_content_hash(inputs: &LiftInputs) -> String {
    let fm = serde_json::to_string(&inputs.frontmatter).unwrap_or_default();
    let mut hasher = blake3::Hasher::new();
    hasher.update(fm.as_bytes());
    hasher.update(b"\0");
    hasher.update(inputs.body.as_bytes());
    format!("blake3:{}", hasher.finalize().to_hex())
}

/// The state of an intent's attestation sidecar relative to the current source.
pub enum Attestation {
    /// No sidecar on disk.
    None,
    /// Sidecar present and its recorded source hash matches the current intent.
    Fresh(Box<CanonicalNode>),
    /// Sidecar present but the intent changed since it was signed.
    Stale(Box<CanonicalNode>),
}

/// Load an intent's attestation sidecar and classify it against the current
/// source (`inputs`). A missing sidecar is `None`; a corrupt/malformed one is
/// treated as `None` with a warning (so a read still works on the raw node).
/// This is what makes an attestation observable: `validate`/`show`/`verify`
/// consult it instead of always re-lifting the un-attested vault entry.
pub fn load_attestation(
    repo: &Repository,
    id: &str,
    inputs: &LiftInputs,
) -> CliResult<Attestation> {
    let path = attested_sidecar_path(repo, id);
    if !path.exists() {
        return Ok(Attestation::None);
    }
    let raw = std::fs::read_to_string(&path).map_err(CliError::Io)?;
    let artifact: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "warning: ignoring unreadable attestation sidecar {}: {e}",
                path.display()
            );
            return Ok(Attestation::None);
        }
    };
    let node_val = match artifact.get("node") {
        Some(n) => n.clone(),
        None => {
            eprintln!(
                "warning: attestation sidecar {} has no 'node' — ignoring",
                path.display()
            );
            return Ok(Attestation::None);
        }
    };
    let node: CanonicalNode = match serde_json::from_value(node_val) {
        Ok(n) => n,
        Err(e) => {
            eprintln!(
                "warning: ignoring malformed attestation node in {}: {e}",
                path.display()
            );
            return Ok(Attestation::None);
        }
    };
    let recorded = artifact
        .get("source")
        .and_then(|s| s.get("sourceContentHash"))
        .and_then(Value::as_str);
    let current = source_content_hash(inputs);
    match recorded {
        Some(h) if h == current => Ok(Attestation::Fresh(Box::new(node))),
        _ => Ok(Attestation::Stale(Box::new(node))),
    }
}

/// Resolve the vault-relative path of a stored intent via the manifest.
///
/// `vault_intent_show` normalizes and confirms existence but does not return a
/// path; `normalize_intent_id` is private, so we match the manifest keys
/// case-insensitively (the manifest keys are the normalized IDs, e.g.
/// `"PIMO-1"`). Returns `None` if the intent has no manifest entry / path.
pub fn vault_path_for(repo: &Repository, id: &str) -> CliResult<Option<String>> {
    let manifest = repo.vault_manifest().map_err(CliError::Repository)?;
    Ok(manifest
        .intents
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(id))
        .map(|(_, summary)| summary.vault_path.clone())
        .filter(|p| !p.is_empty()))
}
