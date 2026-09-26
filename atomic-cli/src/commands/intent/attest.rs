//! `atomic intent attest <ID>` — sign an intent into a tracked attestation entry.

use clap::Parser;

use serde_json::Value;

use atomic_canonical::proof::prepare_attestation;
use atomic_canonical::{lift_and_attest, validate_intent, verify};
use atomic_core::pristine::VaultEntryType;
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

    /// Don't sign: print what a signer holding the identity's key elsewhere
    /// (a browser, a hardware token, a remote signing service) must sign —
    /// `{"document": …, "signingBytes": "<base64>"}`. Needs only the
    /// identity's public key. Complete with `--signed`.
    #[arg(long, conflicts_with = "signed")]
    pub prepare: bool,

    /// Record an attestation signed elsewhere: a JSON file holding the
    /// attested node (the `--prepare` document with its proof attached). It
    /// must be signed by `--identity`'s key and attest the intent as it is
    /// now.
    #[arg(long, value_name = "PATH")]
    pub signed: Option<std::path::PathBuf>,
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

        // Resolve the identity the way `atomic identity sign` does.
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

        // Signing elsewhere, step 1: say what to sign. The same preparation
        // `lift_and_attest` does (author, content hash, canonical bytes), with
        // no private key involved.
        if self.prepare {
            let prepared = prepare_attestation(unattested.to_value(), &identity.public_key);
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "document": prepared.value,
                    "signingBytes": data_encoding::BASE64.encode(&prepared.signing_bytes),
                }))
                .unwrap()
            );
            return Ok(());
        }

        let node = if let Some(path) = &self.signed {
            // Signing elsewhere, step 2: the signature must be over exactly
            // what `--prepare` produces for this intent now — so a signature
            // over a stale or altered intent is refused, not recorded.
            let text = std::fs::read_to_string(path).map_err(CliError::Io)?;
            let signed: Value =
                serde_json::from_str(&text).map_err(|e| CliError::InvalidArgument {
                    message: format!("{} is not JSON: {e}", path.display()),
                })?;
            let expected = prepare_attestation(unattested.to_value(), &identity.public_key).value;
            let mut unsigned = signed.clone();
            if let Some(obj) = unsigned.as_object_mut() {
                obj.remove("proof");
            }
            if unsigned != expected {
                return Err(CliError::InvalidArgument {
                    message: format!(
                        "the signed attestation is not of intent {} as it is now (re-run --prepare)",
                        self.id
                    ),
                });
            }
            serde_json::from_value::<atomic_canonical::CanonicalNode>(signed).map_err(|e| {
                CliError::InvalidArgument {
                    message: format!("not an attested intent: {e}"),
                }
            })?
        } else {
            let keypair = store.load_keypair(&identity.id, None).map_err(|e| {
                CliError::Internal(anyhow::anyhow!(
                    "Failed to load keypair for '{}': {}",
                    identity.name,
                    e
                ))
            })?;
            // Attest: lift + fill attributedTo (from the identity's
            // did:atomic when absent) + hash + sign.
            lift_and_attest(&inputs.frontmatter, &inputs.body, &identity, &keypair).map_err(
                |e| CliError::InvalidArgument {
                    message: format!("could not attest intent: {e}"),
                },
            )?
        };

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
        verify(&node, &identity.public_key).map_err(|e| CliError::InvalidArgument {
            message: format!("attested intent failed self-verification: {e}"),
        })?;

        // --- Legacy sidecar dual-write (transition) ---------------------------
        // Persist the attested node as a sidecar under `.atomic/` — NOT into
        // redb, NOT into the .vault/ tree, NOT via any VaultEntry write. This
        // never touches content_hash or the manifest merkle. Kept during the
        // transition so pre-upgrade readers keep working; `load_attestation`
        // prefers the tracked entry below and shadows this copy.
        let sidecar_path = bridge::attested_sidecar_path(&repo, &self.id)?;
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
            source.insert(
                "vaultPath".to_string(),
                serde_json::Value::String(vault_path),
            );
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

        // --- Tracked vault entry (new authoritative source) -------------------
        // Store JUST the attested node (not the {node,source} wrapper) as the
        // body, so it parses straight back to a CanonicalNode on read.
        let mut body = serde_json::to_string_pretty(&node.to_value())
            .expect("canonical node serialization is infallible");
        // serde pretty JSON has NO trailing newline; render_entry_to_markdown
        // appends one to any body not ending in '\n'. Pre-appending it here makes
        // materialize a byte-identity transform on the body, so the first
        // vault_scan sees Hash::of(body) == stored blake3(content) == Unchanged.
        body.push('\n');
        let content_bytes = body.into_bytes();

        // Frontmatter anchors — FLAT SCALAR strings only, so they survive
        // yaml_frontmatter_to_json (which only round-trips `key: scalar/array`
        // lines; a nested object would be mangled). sourceContentHash/vaultPath
        // contain ':' and '/' but no ": " (colon-space), so write_frontmatter_field
        // emits them bare and they round-trip cleanly.
        let mut fm = serde_json::Map::new();
        fm.insert(
            "intentId".into(),
            Value::String(bridge::normalized_id(&repo, &self.id)?),
        );
        fm.insert(
            "sourceContentHash".into(),
            Value::String(bridge::source_content_hash(&inputs)),
        );
        if let Some(vp) = bridge::vault_path_for(&repo, &self.id)? {
            fm.insert("vaultPath".into(), Value::String(vp));
        }
        let frontmatter_json = serde_json::to_string(&fm).unwrap();

        let vault_path = bridge::attestation_vault_path(&repo, &self.id)?;
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
            .map(|p| {
                let v = &p.proof_value;
                v.chars().take(12).collect::<String>()
            })
            .unwrap_or_default();

        if self.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&node.to_value()).unwrap()
            );
        } else {
            println!("Attested intent: {}", self.id);
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
/// `attributedTo` are filled by `lift_and_attest`.
fn is_fillable_by_attest(path: Option<&str>) -> bool {
    matches!(path, Some("proof") | Some("attributedTo"))
}
