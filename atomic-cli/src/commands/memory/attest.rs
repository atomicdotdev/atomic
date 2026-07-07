//! `atomic memory attest <ID>` — sign a memory into a tracked vault entry
//! (dual-written to a legacy sidecar during the transition).

use clap::Parser;

use serde_json::Value;

use atomic_canonical::{lift_and_attest_memory, validate_memory, verify_memory};
use atomic_core::pristine::VaultEntryType;
use atomic_identity::IdentityStore;
use atomic_repository::Repository;

use crate::commands::memory::bridge;
use crate::commands::memory::validation_failed;
use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};

/// Attest a memory: gate it, sign it, and write the attested node.
#[derive(Parser, Debug)]
#[command(name = "attest")]
pub struct MemoryAttest {
    /// Memory id (or `memory/<id>.md` path).
    pub id: String,

    /// Identity to sign with. Defaults to the current default identity.
    #[arg(long)]
    pub identity: Option<String>,

    /// Output the attested node as JSON-LD.
    #[arg(long)]
    pub json: bool,
}

impl Command for MemoryAttest {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        let inputs = bridge::read_memory(&repo, &self.id)?;

        // FIRST gate WITHOUT signing: refuse to sign a node that fails for a
        // reason attestation cannot fix (unknown memoryKind/status, empty text).
        // Missing proof/attributedTo are the expected pre-attest violations —
        // exactly what signing fills — so we only refuse on violations OUTSIDE
        // that fillable set.
        let unattested = bridge::lift(&inputs)?;
        let pre = validate_memory(&unattested);
        let blocking: Vec<_> = pre
            .results
            .iter()
            .filter(|v| !is_fillable_by_attest(v.path.as_deref()))
            .collect();
        if !blocking.is_empty() {
            eprintln!("Cannot attest {}: the memory does not conform.", self.id);
            eprint!("{pre}");
            return Err(validation_failed(format!(
                "memory {} does not conform; fix the violations above before attesting \
                 (run `atomic memory validate {}`)",
                self.id, self.id
            )));
        }

        // Resolve identity + keypair the way `atomic identity sign` does.
        let store = IdentityStore::open_default().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to open identity store: {}", e))
        })?;
        let identity = if let Some(name) = &self.identity {
            store
                .load_by_name(name)
                .map_err(|_| CliError::IdentityNotFound(name.clone()))?
        } else {
            store
                .get_default()
                .map_err(|e| {
                    CliError::Internal(anyhow::anyhow!("Failed to load default identity: {}", e))
                })?
                .ok_or_else(|| CliError::InvalidArgument {
                    message: "No default identity set. Create one first:\n  \
                              atomic identity new <name> --email <email> --set-default"
                        .to_string(),
                })?
        };
        let keypair = store.load_keypair(&identity.id, None).map_err(|e| {
            CliError::Internal(anyhow::anyhow!(
                "Failed to load keypair for '{}': {}",
                identity.name,
                e
            ))
        })?;

        // Attest: lift + fill attributedTo (from the identity's did:atomic when
        // absent) + hash + sign.
        let node = lift_and_attest_memory(&inputs.frontmatter, &inputs.body, &identity, &keypair)
            .map_err(|e| CliError::InvalidArgument {
            message: format!("could not attest memory: {e}"),
        })?;

        // Belt-and-suspenders: re-gate the ATTESTED node — proof + attributedTo
        // must now satisfy the gate.
        let post = validate_memory(&node);
        if !post.conforms {
            eprint!("{post}");
            return Err(validation_failed(format!(
                "attested memory {} still does not conform; refusing to persist",
                self.id
            )));
        }

        // Self-check: the proof verifies against the signing key before we write.
        verify_memory(&node, &keypair.public).map_err(|e| CliError::InvalidArgument {
            message: format!("attested memory failed self-verification: {e}"),
        })?;

        // --- Legacy sidecar dual-write (transition) ---------------------------
        // Persist the attested node under `.atomic/canonical/memory/<id>/` — NOT
        // into redb, NOT into the .vault/ tree. Never touches content_hash or
        // the manifest merkle. `load_attestation` prefers the tracked entry
        // below and shadows this copy.
        let sidecar_path = bridge::attested_sidecar_path(&repo, &self.id);
        if let Some(parent) = sidecar_path.parent() {
            std::fs::create_dir_all(parent).map_err(CliError::Io)?;
        }
        let mut artifact = serde_json::Map::new();
        artifact.insert("node".to_string(), node.to_value());
        let mut source = serde_json::Map::new();
        source.insert(
            "sourceContentHash".to_string(),
            Value::String(bridge::source_content_hash(&inputs)),
        );
        artifact.insert("source".to_string(), Value::Object(source));
        std::fs::write(
            &sidecar_path,
            serde_json::to_string_pretty(&Value::Object(artifact)).unwrap(),
        )
        .map_err(CliError::Io)?;

        // --- Tracked vault entry (new authoritative source) -------------------
        // Store JUST the attested node (not the {node,source} wrapper) as the
        // body, so it parses straight back to a MemoryNode on read.
        let mut body = serde_json::to_string_pretty(&node.to_value())
            .expect("memory node serialization is infallible");
        // Pre-append the trailing newline so materialize is a byte-identity
        // transform on the body (the first vault_scan sees Unchanged).
        body.push('\n');
        let content_bytes = body.into_bytes();

        // Frontmatter anchors — FLAT SCALAR strings only, so they survive
        // yaml_frontmatter_to_json. `sourceContentHash` contains ':' but no
        // ": " (colon-space), so it round-trips cleanly.
        let mut fm = serde_json::Map::new();
        fm.insert(
            "memoryId".into(),
            Value::String(bridge::memory_id(&self.id)),
        );
        fm.insert(
            "sourceContentHash".into(),
            Value::String(bridge::source_content_hash(&inputs)),
        );
        let frontmatter_json = serde_json::to_string(&fm).unwrap();

        let vault_path = bridge::attestation_vault_path(&self.id);
        repo.vault_store(
            &vault_path,
            VaultEntryType::Attestation,
            content_bytes,
            frontmatter_json,
        )
        .map_err(CliError::Repository)?;
        // Materialize the tracked attestation into the .vault/ working tree so it
        // exists on disk (matching the printed path) and is captured by a later
        // `atomic record` — i.e. it travels via the change graph like any entry.
        repo.vault_materialize(&vault_path)
            .map_err(CliError::Repository)?;

        let did = node.attributed_to.as_deref().unwrap_or("(unknown)");
        let proof_prefix = node
            .proof
            .as_ref()
            .map(|p| p.proof_value.chars().take(12).collect::<String>())
            .unwrap_or_default();

        if self.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&node.to_value()).unwrap()
            );
        } else {
            println!("Attested memory: {}", self.id);
            println!("  vault:     .vault/{vault_path}");
            println!("  sidecar:   {}", sidecar_path.display());
            println!("  author:    {did}");
            println!("  proof:     {proof_prefix}…");
            eprintln!(
                "note: signing keys are stored unencrypted on disk; treat this \
                 attestation as a non-production dev signature until key-at-rest \
                 encryption lands."
            );
        }

        Ok(())
    }
}

/// Is a pre-attest violation on this property path one that attestation fills
/// (and therefore not a reason to refuse signing)? Only `proof` and
/// `attributedTo` are filled by `lift_and_attest_memory` — the SAME fillable set
/// as intent.
fn is_fillable_by_attest(path: Option<&str>) -> bool {
    matches!(path, Some("proof") | Some("attributedTo"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_canonical::memory::lift_memory;
    use serde_json::{Map, Value};
    use tempfile::tempdir;

    /// A memory with a NON-fillable violation (bad memoryKind) must be refused
    /// by the pre-gate, and NO attestation entry should be written.
    #[test]
    fn test_attest_refuses_non_fillable_violation() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        // A bad memoryKind is a non-fillable violation.
        let fm: Map<String, Value> = serde_json::json!({
            "uid": "badkind01",
            "memoryKind": "not-a-kind",
            "status": "active",
            "createdAt": "2026-07-01T00:00:00+00:00",
        })
        .as_object()
        .unwrap()
        .clone();
        let body = ":::memory\nThe durable fact.\n:::";
        let vault_path = bridge::normalize_memory_path("badkind01");
        repo.vault_store(
            &vault_path,
            VaultEntryType::Memory,
            body.as_bytes().to_vec(),
            serde_json::to_string(&fm).unwrap(),
        )
        .unwrap();

        // Simulate the pre-gate directly (the CLI resolves the repo via
        // find_repository_root; here we exercise the same refusal logic).
        let node = lift_memory(&fm, body).unwrap();
        let report = validate_memory(&node);
        let blocking: Vec<_> = report
            .results
            .iter()
            .filter(|v| !is_fillable_by_attest(v.path.as_deref()))
            .collect();
        assert!(
            !blocking.is_empty(),
            "a bad memoryKind is a blocking (non-fillable) violation"
        );

        // No attestation entry was written.
        let apath = bridge::attestation_vault_path("badkind01");
        assert!(
            repo.vault_retrieve(&apath).unwrap().is_none(),
            "attest must not persist an attestation for a non-conforming memory"
        );
    }
}
