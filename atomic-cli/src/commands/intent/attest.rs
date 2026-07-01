//! `atomic intent attest <ID>` — sign an intent into a canonical sidecar.

use clap::Parser;

use atomic_canonical::{lift_and_attest, validate_intent, verify};
use atomic_identity::IdentityStore;
use atomic_repository::Repository;

use crate::commands::intent::bridge;
use crate::commands::intent::validation_failed;
use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};

/// Attest an intent: gate it, sign it, and write the attested node to a sidecar.
#[derive(Parser, Debug)]
#[command(name = "attest")]
pub struct IntentAttest {
    /// Intent ID (e.g. "PIMO-1" or "1").
    pub id: String,

    /// Identity to sign with. Defaults to the current default identity.
    #[arg(long)]
    pub identity: Option<String>,

    /// Output the attested node as JSON-LD.
    #[arg(long)]
    pub json: bool,
}

impl Command for IntentAttest {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        let inputs = bridge::read_intent(&repo, &self.id)?;

        // FIRST gate WITHOUT signing: refuse to sign a node that fails for a
        // reason attestation cannot fix (unknown status, missing why, a scope
        // declaration with no scope-out). Missing proof/attributedTo are the
        // expected pre-attest violations — those are exactly what signing
        // fills, so we only refuse on violations OUTSIDE that fillable set.
        let unattested = bridge::lift(&inputs)?;
        let pre = validate_intent(&unattested);
        let blocking: Vec<_> = pre
            .results
            .iter()
            .filter(|v| !is_fillable_by_attest(v.path.as_deref()))
            .collect();
        if !blocking.is_empty() {
            eprintln!("Cannot attest {}: the intent does not conform.", self.id);
            eprint!("{pre}");
            return Err(validation_failed(format!(
                "intent {} does not conform; fix the violations above before attesting \
                 (run `atomic intent validate {}`)",
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
                .ok_or_else(|| {
                    CliError::Internal(anyhow::anyhow!(
                        "No default identity set. Create one first:\n  \
                         atomic identity new <name> --email <email> --set-default"
                    ))
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
        let node = lift_and_attest(&inputs.frontmatter, &inputs.body, &identity, &keypair)
            .map_err(|e| CliError::InvalidArgument {
                message: format!("could not attest intent: {e}"),
            })?;

        // Belt-and-suspenders: re-gate the ATTESTED node — proof + attributedTo
        // must now satisfy the gate.
        let post = validate_intent(&node);
        if !post.conforms {
            eprint!("{post}");
            return Err(validation_failed(format!(
                "attested intent {} still does not conform; refusing to persist",
                self.id
            )));
        }

        // Self-check: the proof verifies against the signing key before we
        // write anything to disk.
        verify(&node, &keypair.public).map_err(|e| CliError::InvalidArgument {
            message: format!("attested intent failed self-verification: {e}"),
        })?;

        // Persist the attested node as a sidecar under `.atomic/` — NOT into
        // redb, NOT into the .vault/ tree, NOT via any VaultEntry write. This
        // never touches content_hash or the manifest merkle.
        let sidecar_path = bridge::attested_sidecar_path(&repo, &self.id);
        if let Some(parent) = sidecar_path.parent() {
            std::fs::create_dir_all(parent).map_err(CliError::Io)?;
        }
        // Record source anchors alongside the node for staleness detection: the
        // vault path + the source content hash the sidecar attests. The node
        // itself is stored under "node"; the anchors under "source".
        let mut artifact = serde_json::Map::new();
        artifact.insert("node".to_string(), node.to_value());
        let mut source = serde_json::Map::new();
        if let Some(vault_path) = bridge::vault_path_for(&repo, &self.id)? {
            source.insert("vaultPath".to_string(), serde_json::Value::String(vault_path));
        }
        source.insert(
            "sourceContentHash".to_string(),
            serde_json::Value::String(bridge::source_content_hash(&inputs)),
        );
        artifact.insert("source".to_string(), serde_json::Value::Object(source));
        let artifact = serde_json::Value::Object(artifact);
        std::fs::write(
            &sidecar_path,
            serde_json::to_string_pretty(&artifact).unwrap(),
        )
        .map_err(CliError::Io)?;

        let did = node
            .attributed_to
            .as_deref()
            .unwrap_or("(unknown)");
        let proof_prefix = node
            .proof
            .as_ref()
            .map(|p| {
                let v = &p.proof_value;
                v.chars().take(12).collect::<String>()
            })
            .unwrap_or_default();

        if self.json {
            println!("{}", serde_json::to_string_pretty(&node.to_value()).unwrap());
        } else {
            println!("Attested intent: {}", self.id);
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
/// `attributedTo` are filled by `lift_and_attest`.
fn is_fillable_by_attest(path: Option<&str>) -> bool {
    matches!(path, Some("proof") | Some("attributedTo"))
}
