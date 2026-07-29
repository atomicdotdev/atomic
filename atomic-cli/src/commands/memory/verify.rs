//! `atomic memory verify <ID>` — verify a memory's attestation.
//!
//! Reads the attested node written by `attest`, checks it is still fresh (the
//! memory hasn't changed since it was signed), and cryptographically verifies
//! its content hash + Ed25519 Data Integrity proof.

use clap::Parser;

use atomic_canonical::verify_memory;
use atomic_identity::IdentityStore;
use atomic_repository::Repository;

use crate::commands::memory::bridge;
use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::agent_doc::{Doc, Fail, Ref};

/// Verify a memory's attestation (hash + signature) against a public key.
#[derive(Parser, Debug)]
#[command(name = "verify")]
pub struct MemoryVerify {
    /// Memory id (or `memory/<id>.md` path).
    pub id: String,

    /// Identity whose public key to verify against. Defaults to the current
    /// default identity. Pass this if the memory was attested by someone else.
    #[arg(long)]
    pub identity: Option<String>,
}

/// Agent guidance for `atomic memory verify`.
pub const DOC: Doc = Doc {
    when: "checking a signed memory still matches its proof",
    run: "memory verify <id>",
    needs: &[
        Ref {
            cmd: "memory attest <id>",
            note: "there must be an attestation to verify",
        },
    ],
    instead: &[
        Ref {
            cmd: "memory list",
            note: "the verifies column for every memory at once",
        },
        Ref {
            cmd: "memory validate <id>",
            note: "shape gate, not a signature check",
        },
    ],
    fails: &[
        Fail {
            cond: "no attestation exists for this memory",
            exit: 2,
            fix: Ref {
                cmd: "memory attest <id>",
                note: "",
            },
        },
        Fail {
            cond: "attestation is stale: the memory changed after signing",
            exit: 2,
            fix: Ref {
                cmd: "memory attest <id>",
                note: "",
            },
        },
        Fail {
            cond: "signed by a non-default identity",
            exit: 2,
            fix: Ref {
                cmd: "memory verify <id> --identity <name>",
                note: "",
            },
        },
        Fail {
            cond: "id not found: a data error reported with the usage code",
            exit: 2,
            fix: Ref {
                cmd: "memory list",
                note: "",
            },
        },
    ],
    ..Doc::EMPTY
};

impl Command for MemoryVerify {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        let inputs = bridge::read_memory(&repo, &self.id)?;
        let node = match bridge::load_attestation(&repo, &self.id, &inputs)? {
            bridge::Attestation::None => {
                return Err(CliError::InvalidArgument {
                    message: format!(
                        "no attestation found for {}; run `atomic memory attest {}` first",
                        self.id, self.id
                    ),
                })
            }
            bridge::Attestation::Stale(_) => {
                return Err(CliError::InvalidArgument {
                    message: format!(
                        "the attestation for {} is stale (the memory changed since it was \
                         signed); re-run `atomic memory attest {}`",
                        self.id, self.id
                    ),
                })
            }
            bridge::Attestation::Fresh(node) => *node,
        };

        // Resolve the public key. `did:atomic` is a key fingerprint (not
        // recoverable to a key), so we verify against the default (or named)
        // identity and let verify_memory's DID check catch a mismatch.
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

        verify_memory(&node, &identity.public_key).map_err(|e| CliError::InvalidArgument {
            message: format!(
                "verification failed: {e} (if this memory was attested by a different \
                 identity, pass --identity <name>)"
            ),
        })?;

        println!("Verified memory: {}", self.id);
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
