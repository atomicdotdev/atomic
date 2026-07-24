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
//! # Attestation persistence (tracked vault entry, dual-read)
//!
//! Attestations are now written as a FIRST-CLASS TRACKED vault entry at
//! `attestations/<sanitized-id>/attested.md` (entry_type `attestation`). The
//! body is the signed JSON-LD node (pretty-printed + a trailing `\n`); the
//! vault's storage hash stays `blake3(body)` exactly as for every other entry,
//! and the node's own `contentHash` + `eddsa-jcs-2022` proof live INSIDE that
//! opaque body. The trailing newline makes materialize a byte-identity
//! transform, so the first `vault_scan_working_copy` sees the entry as
//! Unchanged (no false Modified, no double-merkle advance).
//!
//! During the transition, `attest` also DUAL-WRITES the legacy sidecar under
//! `<repo.dot_dir()>/canonical/intents/<sanitized-id>/attested.jsonld` (the
//! engine-internal `.atomic/` metadata dir, outside the `.vault/` tree, outside
//! redb, invisible to scan/drift/view-switch-wipe). [`load_attestation`] PREFERS
//! the tracked entry and falls back to the legacy sidecar, so pre-upgrade
//! attestations keep working. Both paths key off the SAME normalized/sanitized
//! id so the fallback lines up.

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
    let frontmatter: Map<String, Value> =
        serde_json::from_str(&entry.frontmatter_json).map_err(|e| CliError::InvalidArgument {
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

/// Sanitize an intent id/key for use as a directory name in the sidecar/vault
/// path. Keeps ASCII alphanumerics, `-` and `_`; everything else becomes `_`.
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

/// The normalized intent id used to key BOTH the tracked vault attestation path
/// and the legacy `.atomic` sidecar dir, so the dual-read fallback lines up
/// (critic #4). Resolution mirrors the repository's private `normalize_intent_id`
/// closely enough that `attest 1`, `attest pimo-1`, and `attest PIMO-1` all
/// collapse to the SAME `<sanitized>` on both the tracked and the legacy paths:
///
/// 1. A case-insensitive match against a manifest key wins (handles `pimo-1` /
///    `PIMO-1`, identical to `vault_path_for`'s resolution).
/// 2. A bare number is expanded with the manifest's `intent_prefix`
///    (`1` -> `PIMO-1`), matching `normalize_intent_id`'s bare-number branch, and
///    that expansion is confirmed against a manifest key when present.
/// 3. Otherwise fall back to the raw arg uppercased (numeric ids missing from the
///    manifest stay as the bare number — the same on both paths, so reconciliation
///    still holds).
pub fn normalized_id(repo: &Repository, id: &str) -> CliResult<String> {
    // Resolution lives in exactly one place — the repository resolver — which
    // fills in the current project + author for a bare number, matches full
    // human keys, and resolves ULIDs/prefixes. When nothing resolves (e.g. a
    // reference to an intent with no manifest entry yet), fall back to the
    // uppercased raw arg so `attest` still produces a deterministic sidecar id.
    match repo.resolve_intent_key(id) {
        Ok(key) => Ok(key),
        Err(_) => Ok(id.to_uppercase()),
    }
}

/// The tracked-vault path for an intent's attestation:
/// `attestations/<sanitized-normalized-id>/attested.md`.
pub fn attestation_vault_path(repo: &Repository, id: &str) -> CliResult<String> {
    Ok(format!(
        "attestations/{}/attested.md",
        sanitize_id(&normalized_id(repo, id)?)
    ))
}

/// The directory that holds an intent's canonical sidecar artifacts:
/// `<dot_dir>/canonical/intents/<sanitized-normalized-id>/`. Keyed off the SAME
/// normalized id as the tracked vault path (critic #4).
pub fn sidecar_dir(repo: &Repository, id: &str) -> CliResult<PathBuf> {
    Ok(repo
        .dot_dir()
        .join("canonical")
        .join("intents")
        .join(sanitize_id(&normalized_id(repo, id)?)))
}

/// The attested-node sidecar file for an intent.
pub fn attested_sidecar_path(repo: &Repository, id: &str) -> CliResult<PathBuf> {
    Ok(sidecar_dir(repo, id)?.join("attested.jsonld"))
}

/// Candidate legacy-sidecar paths, most-preferred first: the normalized-id path
/// (what the current `attest` writes) and the raw-arg path an M1a-era build
/// wrote (it sanitized the RAW CLI arg, un-normalized). Probing both lets the
/// dual-read fallback find pre-upgrade sidecars regardless of the id form the
/// original `attest` was invoked with. De-dups when the two coincide.
fn attested_sidecar_candidates(repo: &Repository, id: &str) -> CliResult<Vec<PathBuf>> {
    let normalized = attested_sidecar_path(repo, id)?;
    let raw = repo
        .dot_dir()
        .join("canonical")
        .join("intents")
        .join(sanitize_id(id))
        .join("attested.jsonld");
    let mut candidates = vec![normalized];
    if !candidates.contains(&raw) {
        candidates.push(raw);
    }
    Ok(candidates)
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
    // 1) Tracked vault entry (new authoritative source). The body is the pretty
    //    JSON-LD node (+ a trailing '\n'); parse it straight to a CanonicalNode.
    let vpath = attestation_vault_path(repo, id)?;
    if let Some(entry) = repo.vault_retrieve(&vpath).map_err(CliError::Repository)? {
        let raw = String::from_utf8_lossy(&entry.content_bytes);
        match serde_json::from_str::<CanonicalNode>(raw.trim_end()) {
            Ok(node) => {
                // Staleness: prefer the frontmatter anchor `sourceContentHash`;
                // frontmatter_json round-trips as a flat-scalar JSON object.
                let recorded = serde_json::from_str::<Value>(&entry.frontmatter_json)
                    .ok()
                    .and_then(|v| {
                        v.get("sourceContentHash")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    });
                let current = source_content_hash(inputs);
                return Ok(match recorded {
                    Some(h) if h == current => Attestation::Fresh(Box::new(node)),
                    _ => Attestation::Stale(Box::new(node)),
                });
            }
            Err(e) => {
                eprintln!("warning: ignoring malformed tracked attestation {vpath}: {e}");
                // fall through to the legacy sidecar
            }
        }
    }

    // 2) Legacy sidecar fallback (pre-upgrade attestations). Probe the
    //    normalized-id path first (what the current `attest` writes), then the
    //    raw-arg path an M1a-era build used (it sanitized the RAW CLI arg without
    //    normalizing), so a sidecar written via `attest 1` / `attest pimo-1` is
    //    still found after upgrade regardless of the id form (critic #4).
    let path = match attested_sidecar_candidates(repo, id)?
        .into_iter()
        .find(|p| p.exists())
    {
        Some(p) => p,
        None => return Ok(Attestation::None),
    };
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

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_canonical::lift_and_attest;
    use atomic_core::pristine::VaultEntryType;
    use atomic_identity::identity::Identity;
    use atomic_identity::keypair::KeyPair;
    use atomic_repository::IntentCreateOptions;
    use tempfile::tempdir;

    /// Build inputs for a minimal, liftable intent and attest them into a node.
    fn attested_node(inputs: &LiftInputs) -> (CanonicalNode, KeyPair) {
        let kp = KeyPair::generate();
        let id = Identity::new("tester", &kp);
        let node = lift_and_attest(&inputs.frontmatter, &inputs.body, &id, &kp)
            .expect("lift_and_attest on a minimal intent should succeed");
        (node, kp)
    }

    /// Create a repo+vault and a single intent; return (repo, normalized id,
    /// lift inputs for the stored intent).
    fn repo_with_intent() -> (Repository, String, LiftInputs, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();
        let result = repo
            .vault_intent_create(IntentCreateOptions {
                title: "Test intent".to_string(),
                priority: Some("medium".to_string()),
                assignee: None,
                labels: Vec::new(),
                session_id: None,
                turn_id: None,
            })
            .unwrap();
        let id = result.id;
        let inputs = read_intent(&repo, &id).unwrap();
        (repo, id, inputs, dir)
    }

    /// Mirror `attest`'s tracked-vault write exactly (body = pretty JSON + '\n',
    /// flat-string frontmatter anchors).
    fn write_tracked(repo: &Repository, id: &str, node: &CanonicalNode, inputs: &LiftInputs) {
        let mut body = serde_json::to_string_pretty(&node.to_value()).unwrap();
        body.push('\n');
        let mut fm = serde_json::Map::new();
        fm.insert(
            "intentId".into(),
            Value::String(normalized_id(repo, id).unwrap()),
        );
        fm.insert(
            "sourceContentHash".into(),
            Value::String(source_content_hash(inputs)),
        );
        let frontmatter_json = serde_json::to_string(&fm).unwrap();
        let vpath = attestation_vault_path(repo, id).unwrap();
        repo.vault_store(
            &vpath,
            VaultEntryType::Attestation,
            body.into_bytes(),
            frontmatter_json,
        )
        .unwrap();
    }

    /// Mirror `attest`'s legacy sidecar write exactly ({node, source} wrapper).
    fn write_legacy_sidecar(
        repo: &Repository,
        id: &str,
        node: &CanonicalNode,
        inputs: &LiftInputs,
    ) {
        let path = attested_sidecar_path(repo, id).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut artifact = serde_json::Map::new();
        artifact.insert("node".to_string(), node.to_value());
        let mut source = serde_json::Map::new();
        source.insert(
            "sourceContentHash".to_string(),
            Value::String(source_content_hash(inputs)),
        );
        artifact.insert("source".to_string(), Value::Object(source));
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&Value::Object(artifact)).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn test_load_attestation_prefers_tracked_then_legacy() {
        let (repo, id, inputs, _dir) = repo_with_intent();
        let (node, _kp) = attested_node(&inputs);

        // (a) Only a legacy sidecar present -> dual-read fallback returns Fresh.
        write_legacy_sidecar(&repo, &id, &node, &inputs);
        assert!(matches!(
            load_attestation(&repo, &id, &inputs).unwrap(),
            Attestation::Fresh(_)
        ));

        // (b) A tracked entry present -> it is used (still Fresh).
        write_tracked(&repo, &id, &node, &inputs);
        assert!(matches!(
            load_attestation(&repo, &id, &inputs).unwrap(),
            Attestation::Fresh(_)
        ));

        // (c) ID reconciliation (critic #4): `1`, `pimo-1`, `PIMO-1` all resolve
        //     the same attestation. The normalized manifest key is e.g. "NONA-1"
        //     (derived from the temp dir name); the bare number and lowercase
        //     forms must all resolve to it.
        let lower = id.to_lowercase();
        let num = id.rsplit('-').next().unwrap().to_string();
        for alias in [id.as_str(), lower.as_str(), num.as_str()] {
            assert_eq!(
                attestation_vault_path(&repo, alias).unwrap(),
                attestation_vault_path(&repo, &id).unwrap(),
                "alias {alias} must resolve to the same tracked path"
            );
            assert_eq!(
                attested_sidecar_path(&repo, alias).unwrap(),
                attested_sidecar_path(&repo, &id).unwrap(),
                "alias {alias} must resolve to the same legacy sidecar path"
            );
            assert!(
                matches!(
                    load_attestation(&repo, alias, &inputs).unwrap(),
                    Attestation::Fresh(_)
                ),
                "alias {alias} must load the same Fresh attestation"
            );
        }
    }

    #[test]
    fn test_tracked_entry_parses_and_verifies() {
        // End-to-end at the persistence layer: writing the tracked entry + legacy
        // sidecar exactly the way `attest` does, then (a) the tracked entry's body
        // parses back to a CanonicalNode carrying a proof, (b) the legacy sidecar
        // is also present (dual-write), (c) load_attestation reads the TRACKED
        // entry, and (d) the parsed node cryptographically verifies against the
        // signing key.
        let (repo, id, inputs, _dir) = repo_with_intent();
        let (node, kp) = attested_node(&inputs);
        write_tracked(&repo, &id, &node, &inputs);
        write_legacy_sidecar(&repo, &id, &node, &inputs);

        // (a) tracked entry exists; body parses to a CanonicalNode with a proof.
        let vpath = attestation_vault_path(&repo, &id).unwrap();
        let entry = repo.vault_retrieve(&vpath).unwrap().expect("tracked entry");
        assert_eq!(entry.entry_type, VaultEntryType::Attestation);
        let raw = String::from_utf8_lossy(&entry.content_bytes);
        let parsed: CanonicalNode = serde_json::from_str(raw.trim_end()).unwrap();
        assert!(parsed.proof.is_some(), "tracked node must carry a proof");
        assert!(parsed.content_hash.is_some());
        // Body ends in exactly one trailing newline (critic #1 invariant).
        assert!(raw.ends_with('\n') && !raw.ends_with("\n\n"));

        // (b) legacy sidecar also written.
        assert!(attested_sidecar_path(&repo, &id).unwrap().exists());

        // (c) load_attestation returns the tracked node as Fresh.
        let loaded = match load_attestation(&repo, &id, &inputs).unwrap() {
            Attestation::Fresh(n) => *n,
            Attestation::Stale(_) => panic!("expected Fresh, got Stale"),
            Attestation::None => panic!("expected Fresh, got None"),
        };

        // (d) the loaded node verifies against the signing key.
        atomic_canonical::verify(&loaded, &kp.public).expect("tracked node must verify");
    }

    #[test]
    fn test_attestation_staleness() {
        let (repo, id, inputs, _dir) = repo_with_intent();
        let (node, _kp) = attested_node(&inputs);
        write_tracked(&repo, &id, &node, &inputs);

        // Fresh against the original inputs.
        assert!(matches!(
            load_attestation(&repo, &id, &inputs).unwrap(),
            Attestation::Fresh(_)
        ));

        // Simulate an edit to the intent body: the current source hash no longer
        // matches the recorded anchor -> Stale.
        let edited = LiftInputs {
            frontmatter: inputs.frontmatter.clone(),
            body: format!("{}\n<!-- edited -->", inputs.body),
        };
        assert!(matches!(
            load_attestation(&repo, &id, &edited).unwrap(),
            Attestation::Stale(_)
        ));
    }

    #[test]
    fn test_legacy_sidecar_raw_arg_fallback_is_found() {
        // An M1a-era `attest <raw-arg>` sanitized the RAW arg without normalizing,
        // so a bare-number/lowercase invocation wrote the sidecar under that
        // literal form (e.g. .atomic/canonical/intents/1/attested.jsonld). After
        // upgrade the reader normalizes ids; the raw-arg candidate probe must
        // still find such a pre-upgrade sidecar.
        let (repo, id, inputs, _dir) = repo_with_intent();
        let (node, _kp) = attested_node(&inputs);

        // The bare-number alias (e.g. "1" for "NONA::user::1").
        let num = id.rsplit("::").next().unwrap().to_string();
        let normalized = attested_sidecar_path(&repo, &num).unwrap();

        // Write the sidecar at the RAW-arg location an M1a build used.
        let raw_dir = repo
            .dot_dir()
            .join("canonical")
            .join("intents")
            .join(sanitize_id(&num));
        std::fs::create_dir_all(&raw_dir).unwrap();
        let raw_path = raw_dir.join("attested.jsonld");
        assert_ne!(
            raw_path, normalized,
            "raw-arg path must differ from the normalized path for this test to be meaningful"
        );
        let mut artifact = serde_json::Map::new();
        artifact.insert("node".to_string(), node.to_value());
        let mut source = serde_json::Map::new();
        source.insert(
            "sourceContentHash".to_string(),
            Value::String(source_content_hash(&inputs)),
        );
        artifact.insert("source".to_string(), Value::Object(source));
        std::fs::write(
            &raw_path,
            serde_json::to_string_pretty(&Value::Object(artifact)).unwrap(),
        )
        .unwrap();

        // No tracked entry and no normalized-path sidecar exist; the raw-arg
        // probe must still find it -> Fresh.
        assert!(matches!(
            load_attestation(&repo, &num, &inputs).unwrap(),
            Attestation::Fresh(_)
        ));
    }
}
