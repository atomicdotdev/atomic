//! `atomic intent verify <ID>` — verify an intent's attestation sidecar.
//!
//! Reads the attested node written by `attest`, checks it is still fresh (the
//! intent hasn't changed since it was signed), and cryptographically verifies
//! its content hash + Ed25519 Data Integrity proof. This is the read-back that
//! makes the sidecar observable: `attest` writes it, `verify` proves it.

use clap::Parser;

use atomic_canonical::verify;
use atomic_identity::IdentityStore;
use atomic_repository::Repository;

use crate::commands::intent::bridge;
use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::agent_doc::{Doc, Fail, Ref};

/// Verify an intent's attestation (hash + signature) against a public key.
#[derive(Parser, Debug)]
#[command(name = "verify")]
pub struct IntentVerify {
    /// Intent ID (e.g. "PIMO-1" or "1").
    pub id: String,

    /// Identity whose public key to verify against. Defaults to the current
    /// default identity. Pass this if the intent was attested by someone else.
    #[arg(long)]
    pub identity: Option<String>,
}

/// Agent guidance for `atomic intent verify`.
pub const DOC: Doc = Doc {
    when: "need proof an attestation holds and who signed it",
    run: "intent verify <ID>",
    needs: &[
        Ref {
            cmd: "intent attest <ID>",
            note: "nothing to verify otherwise",
        },
    ],
    then: &[
        Ref {
            cmd: "intent show <ID> --json",
            note: "the attested projection",
        },
    ],
    instead: &[
        Ref {
            cmd: "intent list --json",
            note: "verify state for every intent at once",
        },
        Ref {
            cmd: "intent validate <ID> --json",
            note: "the shapes, not the signature",
        },
    ],
    fails: &[
        Fail {
            cond: "no attestation for this intent",
            exit: 2,
            fix: Ref {
                cmd: "intent attest <ID>",
                note: "",
            },
        },
        Fail {
            cond: "intent changed since signing (stale attestation)",
            exit: 2,
            fix: Ref {
                cmd: "intent attest <ID>",
                note: "",
            },
        },
        Fail {
            cond: "attested by a different identity",
            exit: 2,
            fix: Ref {
                cmd: "intent verify <ID> --identity <name>",
                note: "",
            },
        },
    ],
    ..Doc::EMPTY
};

impl Command for IntentVerify {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        let inputs = bridge::read_intent(&repo, &self.id)?;
        let node = match bridge::load_attestation(&repo, &self.id, &inputs)? {
            bridge::Attestation::None => {
                return Err(CliError::InvalidArgument {
                    message: format!(
                        "no attestation found for {}; run `atomic intent attest {}` first",
                        self.id, self.id
                    ),
                })
            }
            bridge::Attestation::Stale(_) => {
                return Err(CliError::InvalidArgument {
                    message: format!(
                        "the attestation for {} is stale (the intent changed since it was \
                         signed); re-run `atomic intent attest {}`",
                        self.id, self.id
                    ),
                })
            }
            bridge::Attestation::Fresh(node) => *node,
        };

        // Resolve the public key to verify against. `did:atomic` is a key
        // fingerprint (not recoverable to a key), so a store-backed resolver is
        // needed; for now we verify against the default (or named) identity and
        // let atomic_canonical::verify's DID check catch a mismatch.
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

        verify(&node, &identity.public_key).map_err(|e| CliError::InvalidArgument {
            message: format!(
                "verification failed: {e} (if this intent was attested by a different \
                 identity, pass --identity <name>)"
            ),
        })?;

        println!("Verified intent: {}", self.id);
        println!(
            "  author: {}",
            node.attributed_to.as_deref().unwrap_or("(unknown)")
        );
        if let Some(h) = &node.content_hash {
            println!("  hash:   {h}");
        }
        Ok(())
    }
}
