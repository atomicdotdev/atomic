//! The read→lift bridge and attestation plumbing shared by the `atomic memory`
//! verbs — the memory sibling of [`crate::commands::intent::bridge`].
//!
//! # Read bridge
//!
//! Memories have NO dedicated repository create/show/update method (unlike
//! intents, which have `vault_intent_create`/`vault_intent_show`/
//! `vault_intent_update`). A stored memory is read via the GENERIC vault path
//! [`Repository::vault_retrieve`] by its exact path `memory/<id>.md` — the same
//! path the raw `atomic vault memory write/show` uses. The returned `VaultEntry`
//! carries the two lift inputs:
//!
//! - `frontmatter_json: String` — a JSON *object* string (empty entries are
//!   `"{}"`). This is the pre-parsed frontmatter spine; it is fed straight to
//!   the lift. The body is NOT routed through `parse_markdown` for the vault
//!   path, because the stored `content_bytes` is the body WITHOUT any
//!   frontmatter block and `lift_memory` does its own lenient `:::memory` scan.
//! - `content_bytes: Vec<u8>` — the markdown body.
//!
//! # Id / path scheme (memory-local, NOT the intent normalizer)
//!
//! Memories are NOT `PREFIX-N` and have no prefix allocator. The id is the
//! filename stem and the vault path is `memory/<id>.md`. There is NO manifest
//! lookup / case-folding / prefix expansion — the manifest key IS the path.
//! [`normalize_memory_path`] is the single resolver; it deliberately does NOT
//! reuse the intent `normalize_intent_id`/`intent_dir_for` machinery (which
//! would reintroduce the 0.8.0 intent-collapse bug).
//!
//! # Attestation persistence (tracked vault entry, dual-read)
//!
//! Identical mechanism to the intent bridge (d63713c): the attestation is a
//! FIRST-CLASS TRACKED vault entry at `attestations/memory/<id>/attested.md`
//! (entry_type `attestation`), namespaced under `attestations/memory/` so a
//! memory id can never collide with an intent human key at
//! `attestations/<ID>/attested.md`. The body is the signed JSON-LD `MemoryNode`
//! (pretty-printed + a trailing `\n` so materialize is a byte-identity
//! transform); the vault's storage hash stays `blake3(body)`. `attest` also
//! DUAL-WRITES the legacy sidecar under
//! `<repo.dot_dir()>/canonical/memory/<id>/attested.jsonld`. [`load_attestation`]
//! PREFERS the tracked entry and falls back to the legacy sidecar.

use std::path::PathBuf;

use serde_json::{Map, Value};

use atomic_canonical::memory::lift_memory;
use atomic_canonical::MemoryNode;
use atomic_core::pristine::VaultEntry;
use atomic_repository::Repository;

use crate::error::{CliError, CliResult};

/// The two lift inputs pulled off a stored memory `VaultEntry`.
pub struct MemLiftInputs {
    /// The parsed frontmatter spine (a JSON object).
    pub frontmatter: Map<String, Value>,
    /// The markdown body (without any frontmatter block).
    pub body: String,
}

/// Normalize a bare id, a `memory/<id>` path, an `<id>.md` name, or a full
/// `memory/<id>.md` path all to the canonical vault path `memory/<id>.md`.
///
/// This is the SINGLE memory id/path resolver: no manifest lookup, no prefix,
/// no case-folding — the manifest key IS the path (contrast the intent
/// normalizer, which is deliberately NOT reused). A memory is a single flat
/// file per id, never a directory.
pub fn normalize_memory_path(id_or_path: &str) -> String {
    let without_prefix = id_or_path.strip_prefix("memory/").unwrap_or(id_or_path);
    let stem = without_prefix.strip_suffix(".md").unwrap_or(without_prefix);
    format!("memory/{stem}.md")
}

/// The `<id>` stem for a bare id or any accepted path form. Used both to build
/// the human-facing messages and (sanitized) to KEY the attestation path.
pub fn memory_id(id_or_path: &str) -> String {
    let path = normalize_memory_path(id_or_path);
    path.strip_prefix("memory/")
        .and_then(|s| s.strip_suffix(".md"))
        .unwrap_or(&path)
        .to_string()
}

/// Parse a memory `VaultEntry` into the frontmatter map + body the lift consumes.
///
/// `frontmatter_json` is parsed exactly as the repository parses it
/// (`serde_json::from_str` into a `Map`). A malformed frontmatter string is a
/// clean argument error rather than an internal panic.
pub fn inputs_from_entry(entry: &VaultEntry) -> CliResult<MemLiftInputs> {
    let frontmatter: Map<String, Value> = serde_json::from_str(&entry.frontmatter_json)
        .map_err(|e| CliError::InvalidArgument {
            message: format!("memory frontmatter is not valid JSON: {e}"),
        })?;
    let body = String::from_utf8_lossy(&entry.content_bytes).into_owned();
    Ok(MemLiftInputs { frontmatter, body })
}

/// Read a memory by id (or path) from the vault and return its lift inputs.
pub fn read_memory(repo: &Repository, id_or_path: &str) -> CliResult<MemLiftInputs> {
    let path = normalize_memory_path(id_or_path);
    let entry = repo
        .vault_retrieve(&path)
        .map_err(CliError::Repository)?
        .ok_or_else(|| CliError::InvalidArgument {
            message: format!(
                "memory not found: {id_or_path} (create with `atomic memory new`)"
            ),
        })?;
    inputs_from_entry(&entry)
}

/// Lift the given inputs into a canonical `MemoryNode`, mapping a lift failure
/// (missing `uid`, missing `memoryKind`, …) to a clean argument error.
pub fn lift(inputs: &MemLiftInputs) -> CliResult<MemoryNode> {
    lift_memory(&inputs.frontmatter, &inputs.body).map_err(|e| CliError::InvalidArgument {
        message: format!("could not lift memory: {e}"),
    })
}

/// Sanitize a memory id for use as a directory name in the sidecar/vault path.
/// Keeps ASCII alphanumerics, `-` and `_`; everything else becomes `_`.
/// Copied verbatim from `intent/bridge.rs::sanitize_id`.
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

/// The tracked-vault path for a memory's attestation:
/// `attestations/memory/<sanitized-id>/attested.md`.
///
/// NOTE the `attestations/memory/` sub-namespace: a memory id can therefore
/// never collide with an intent human key at `attestations/<ID>/attested.md`.
/// `infer_entry_type` maps any `attestations/` prefix to `Attestation`, so this
/// needs no atomic-repository change.
pub fn attestation_vault_path(id_or_path: &str) -> String {
    format!(
        "attestations/memory/{}/attested.md",
        sanitize_id(&memory_id(id_or_path))
    )
}

/// The directory that holds a memory's canonical sidecar artifacts:
/// `<dot_dir>/canonical/memory/<sanitized-id>/`.
pub fn sidecar_dir(repo: &Repository, id_or_path: &str) -> PathBuf {
    repo.dot_dir()
        .join("canonical")
        .join("memory")
        .join(sanitize_id(&memory_id(id_or_path)))
}

/// The attested-node sidecar file for a memory.
pub fn attested_sidecar_path(repo: &Repository, id_or_path: &str) -> PathBuf {
    sidecar_dir(repo, id_or_path).join("attested.jsonld")
}

/// A hash of the source lift inputs, recorded so a later edit to the memory can
/// be detected as making the attestation stale. Standalone digest of
/// (frontmatter + body); NOT the vault's `content_hash`. Copied verbatim from
/// `intent/bridge.rs::source_content_hash`.
pub fn source_content_hash(inputs: &MemLiftInputs) -> String {
    let fm = serde_json::to_string(&inputs.frontmatter).unwrap_or_default();
    let mut hasher = blake3::Hasher::new();
    hasher.update(fm.as_bytes());
    hasher.update(b"\0");
    hasher.update(inputs.body.as_bytes());
    format!("blake3:{}", hasher.finalize().to_hex())
}

/// The state of a memory's attestation relative to the current source.
pub enum Attestation {
    /// No attestation on disk (tracked or legacy).
    None,
    /// Attestation present and its recorded source hash matches the current memory.
    Fresh(Box<MemoryNode>),
    /// Attestation present but the memory changed since it was signed.
    Stale(Box<MemoryNode>),
}

/// Load a memory's attestation and classify it against the current source
/// (`inputs`). Prefers the tracked vault entry, falling back to the legacy
/// sidecar. A missing attestation is `None`; a corrupt/malformed one is treated
/// as `None`/skipped with a warning (fail-open, so a read still works on the
/// raw node). Same mechanism as the intent bridge.
pub fn load_attestation(
    repo: &Repository,
    id_or_path: &str,
    inputs: &MemLiftInputs,
) -> CliResult<Attestation> {
    // 1) Tracked vault entry (authoritative). The body is the pretty JSON-LD
    //    node (+ trailing '\n'); parse it straight to a MemoryNode.
    let vpath = attestation_vault_path(id_or_path);
    if let Some(entry) = repo.vault_retrieve(&vpath).map_err(CliError::Repository)? {
        let raw = String::from_utf8_lossy(&entry.content_bytes);
        match serde_json::from_str::<MemoryNode>(raw.trim_end()) {
            Ok(node) => {
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

    // 2) Legacy sidecar fallback (pre-upgrade attestations). Memory has no
    //    PREFIX-N ambiguity, so there is exactly ONE candidate path.
    let path = attested_sidecar_path(repo, id_or_path);
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
    let node: MemoryNode = match serde_json::from_value(node_val) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_canonical::lift_and_attest_memory;
    use atomic_core::pristine::VaultEntryType;
    use atomic_identity::identity::Identity;
    use atomic_identity::keypair::KeyPair;
    use tempfile::tempdir;

    /// A minimal, liftable memory spine (uid + kind + stable createdAt).
    fn memory_inputs() -> MemLiftInputs {
        let frontmatter = serde_json::json!({
            "uid": "01j8zc4r8t",
            "memoryKind": "constraint",
            "status": "active",
            "createdAt": "2026-07-01T00:00:00+00:00",
        })
        .as_object()
        .unwrap()
        .clone();
        MemLiftInputs {
            frontmatter,
            body: ":::memory\nThe durable fact.\n:::".to_string(),
        }
    }

    fn attested_node(inputs: &MemLiftInputs) -> (MemoryNode, KeyPair) {
        let kp = KeyPair::generate();
        let id = Identity::new("tester", &kp);
        let node = lift_and_attest_memory(&inputs.frontmatter, &inputs.body, &id, &kp)
            .expect("lift_and_attest_memory on a minimal memory should succeed");
        (node, kp)
    }

    /// Create a repo+vault and a single memory via the GENERIC vault path.
    fn repo_with_memory() -> (Repository, String, MemLiftInputs, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();
        let inputs = memory_inputs();
        let id = "01j8zc4r8t".to_string();
        let path = normalize_memory_path(&id);
        repo.vault_store(
            &path,
            VaultEntryType::Memory,
            inputs.body.clone().into_bytes(),
            serde_json::to_string(&inputs.frontmatter).unwrap(),
        )
        .unwrap();
        (repo, id, inputs, dir)
    }

    /// Mirror `attest`'s tracked-vault write exactly.
    fn write_tracked(repo: &Repository, id: &str, node: &MemoryNode, inputs: &MemLiftInputs) {
        let mut body = serde_json::to_string_pretty(&node.to_value()).unwrap();
        body.push('\n');
        let mut fm = serde_json::Map::new();
        fm.insert("memoryId".into(), Value::String(memory_id(id)));
        fm.insert(
            "sourceContentHash".into(),
            Value::String(source_content_hash(inputs)),
        );
        let frontmatter_json = serde_json::to_string(&fm).unwrap();
        let vpath = attestation_vault_path(id);
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
        node: &MemoryNode,
        inputs: &MemLiftInputs,
    ) {
        let path = attested_sidecar_path(repo, id);
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
    fn test_normalize_memory_path_all_forms() {
        for form in ["x", "memory/x", "x.md", "memory/x.md"] {
            assert_eq!(normalize_memory_path(form), "memory/x.md");
        }
        assert_eq!(memory_id("memory/x.md"), "x");
        assert_eq!(memory_id("x"), "x");
    }

    #[test]
    fn test_attestation_vault_path_stable_and_namespaced() {
        // Stable across all accepted id/path forms.
        for form in ["x", "memory/x", "x.md", "memory/x.md"] {
            assert_eq!(
                attestation_vault_path(form),
                "attestations/memory/x/attested.md"
            );
        }
        // Collision guard: the memory namespace differs from an intent
        // attestation path for the same literal id.
        let intent_path = format!("attestations/{}/attested.md", "x");
        assert_ne!(attestation_vault_path("x"), intent_path);
    }

    #[test]
    fn test_load_attestation_prefers_tracked_then_legacy() {
        let (repo, id, inputs, _dir) = repo_with_memory();
        let (node, _kp) = attested_node(&inputs);

        // (a) Only a legacy sidecar present -> dual-read fallback returns Fresh.
        write_legacy_sidecar(&repo, &id, &node, &inputs);
        assert!(matches!(
            load_attestation(&repo, &id, &inputs).unwrap(),
            Attestation::Fresh(_)
        ));

        // (b) A tracked entry present -> it is used (still Fresh, tracked wins).
        write_tracked(&repo, &id, &node, &inputs);
        assert!(matches!(
            load_attestation(&repo, &id, &inputs).unwrap(),
            Attestation::Fresh(_)
        ));
    }

    #[test]
    fn test_tracked_entry_parses_and_verifies() {
        let (repo, id, inputs, _dir) = repo_with_memory();
        let (node, kp) = attested_node(&inputs);
        write_tracked(&repo, &id, &node, &inputs);
        write_legacy_sidecar(&repo, &id, &node, &inputs);

        // (a) tracked entry exists; body parses to a MemoryNode with a proof.
        let vpath = attestation_vault_path(&id);
        let entry = repo.vault_retrieve(&vpath).unwrap().expect("tracked entry");
        assert_eq!(entry.entry_type, VaultEntryType::Attestation);
        let raw = String::from_utf8_lossy(&entry.content_bytes);
        let parsed: MemoryNode = serde_json::from_str(raw.trim_end()).unwrap();
        assert!(parsed.proof.is_some(), "tracked node must carry a proof");
        assert!(parsed.content_hash.is_some());
        // Body ends in exactly one trailing newline.
        assert!(raw.ends_with('\n') && !raw.ends_with("\n\n"));

        // (b) legacy sidecar also written.
        assert!(attested_sidecar_path(&repo, &id).exists());

        // (c) load_attestation returns the tracked node as Fresh.
        let loaded = match load_attestation(&repo, &id, &inputs).unwrap() {
            Attestation::Fresh(n) => *n,
            Attestation::Stale(_) => panic!("expected Fresh, got Stale"),
            Attestation::None => panic!("expected Fresh, got None"),
        };

        // (d) the loaded node verifies against the signing key.
        atomic_canonical::verify_memory(&loaded, &kp.public)
            .expect("tracked memory node must verify");
    }

    #[test]
    fn test_attestation_staleness() {
        let (repo, id, inputs, _dir) = repo_with_memory();
        let (node, _kp) = attested_node(&inputs);
        write_tracked(&repo, &id, &node, &inputs);

        // Fresh against the original inputs.
        assert!(matches!(
            load_attestation(&repo, &id, &inputs).unwrap(),
            Attestation::Fresh(_)
        ));

        // Edit the body -> current source hash no longer matches -> Stale.
        let edited = MemLiftInputs {
            frontmatter: inputs.frontmatter.clone(),
            body: format!("{}\n<!-- edited -->", inputs.body),
        };
        assert!(matches!(
            load_attestation(&repo, &id, &edited).unwrap(),
            Attestation::Stale(_)
        ));
    }
}
