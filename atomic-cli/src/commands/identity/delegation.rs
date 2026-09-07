//! `atomic identity delegation` — certificate plumbing.
//!
//! Install a certificate minted elsewhere, push one to a server, list what is
//! held locally, verify one, or revoke one by id. The porcelain
//! (`atomic identity agent`) composes these; they exist separately because the
//! two-machine flows — a CI runner installing a countersigned certificate, an
//! auditor verifying one out of a clone — only need one step each.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use atomic_canonical::delegation as cert;
use atomic_identity::delegation::DelegationId;
use atomic_identity::IdentityStore;
use atomic_remote::storage_types::{PushDelegationRequest, RevokeDelegationRequest};

use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_success, print_warning};

/// Delegation certificate management.
#[derive(Debug, clap::Args)]
pub struct DelegationCmd {
    #[command(subcommand)]
    pub command: DelegationCommands,
}

/// Available delegation subcommands.
#[derive(Debug, Subcommand)]
pub enum DelegationCommands {
    /// Install a certificate minted on another machine.
    Install(Install),
    /// Upload a locally held certificate to a server.
    Push(Push),
    /// List certificates held on this machine.
    List(List),
    /// Verify a certificate's proof, expiry and revocation.
    Verify(Verify),
    /// Revoke a certificate by id.
    Revoke(Revoke),
}

impl Command for DelegationCmd {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            DelegationCommands::Install(c) => c.run(),
            DelegationCommands::Push(c) => c.run(),
            DelegationCommands::List(c) => c.run(),
            DelegationCommands::Verify(c) => c.run(),
            DelegationCommands::Revoke(c) => c.run(),
        }
    }
}

// ---------------------------------------------------------------------------
// install
// ---------------------------------------------------------------------------

/// Install a certificate from a file.
#[derive(Debug, Parser)]
pub struct Install {
    /// Path to the certificate, or `-` for stdin.
    #[arg(required = true)]
    pub path: String,
}

impl Command for Install {
    fn run(&self) -> CliResult<()> {
        let raw = read_input(&self.path)?;
        let value: serde_json::Value =
            serde_json::from_str(&raw).map_err(|e| CliError::InvalidArgument {
                message: format!("{} is not valid JSON: {e}", self.path),
            })?;

        // Verify before storing. A certificate that does not verify is not
        // something to keep "in case" — it would be silently skipped at use
        // time and leave the operator wondering why the agent has no authority.
        let delegation =
            cert::verify_self_contained(&value).map_err(|e| CliError::DelegationError {
                message: format!("Refusing to install: the certificate does not verify: {e}"),
            })?;

        let store = IdentityStore::open_default().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to open identity store: {e}"))
        })?;
        store
            .save_delegation(&delegation.id.to_base32(), &raw)
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to store certificate: {e}")))?;

        print_success(&format!("Installed {}", delegation.id.to_urn()));
        println!("  Delegate      {}", delegation.delegate_name);
        println!("  From          {}", delegation.delegator_name);
        println!(
            "  Can           {}",
            delegation
                .scope
                .permissions
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        if let Some(expires) = delegation.expires {
            println!("  Expires       {}", expires.format("%Y-%m-%d"));
        }

        println!();
        print_hint(
            "The signature checks out, which proves the certificate was not altered. \
             That the delegator is who you think is settled by the server's registered key.",
        );
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// push
// ---------------------------------------------------------------------------

/// Upload a certificate to a server.
#[derive(Debug, Parser)]
pub struct Push {
    /// Delegation id (base32 or `urn:atomic:delegation:...`).
    ///
    /// Omit to push every locally held certificate that is still active.
    pub id: Option<String>,

    /// Server profile to push to.
    #[arg(long)]
    pub server: Option<String>,
}

impl Command for Push {
    fn run(&self) -> CliResult<()> {
        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to create runtime: {e}")))?;
        rt.block_on(self.execute())
    }
}

impl Push {
    async fn execute(&self) -> CliResult<()> {
        let store = IdentityStore::open_default().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to open identity store: {e}"))
        })?;

        let documents = match &self.id {
            Some(id) => vec![(normalize_id(id)?, load_document(&store, id)?)],
            None => store
                .list_delegations()
                .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to list: {e}")))?,
        };

        if documents.is_empty() {
            print_hint("No certificates to push.");
            return Ok(());
        }

        let mut pushed = 0;
        for (id, raw) in documents {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
                print_warning(&format!("Skipping {id}: not valid JSON"));
                continue;
            };
            let Ok(delegation) = cert::verify_self_contained(&value) else {
                print_warning(&format!("Skipping {id}: does not verify"));
                continue;
            };
            if delegation.is_expired() {
                continue;
            }

            // The delegator must be on this machine — the server authenticates
            // the *human*, since only they may enroll on their own behalf.
            let Ok(delegator) = store.load_by_name(&delegation.delegator_name) else {
                print_warning(&format!(
                    "Skipping {id}: the delegating identity '{}' is not on this machine",
                    delegation.delegator_name
                ));
                continue;
            };

            let (client, url) =
                crate::commands::client::build_apex_client_as(&delegator, self.server.as_deref())
                    .await?;
            let request = PushDelegationRequest { certificate: value };
            match client.push_delegation(&request).await {
                Ok(_) => {
                    println!("  {} → {url}", delegation.id.to_urn());
                    pushed += 1;
                }
                Err(e) => print_warning(&format!("Failed to push {id}: {e}")),
            }
        }

        if pushed > 0 {
            print_success(&format!("Pushed {pushed} certificate(s)"));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

/// List locally held certificates.
#[derive(Debug, Parser)]
pub struct List {
    /// Include expired and revoked certificates.
    #[arg(long)]
    pub include_expired: bool,
}

impl Command for List {
    fn run(&self) -> CliResult<()> {
        let store = IdentityStore::open_default().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to open identity store: {e}"))
        })?;

        let stored = store
            .list_delegations()
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to list: {e}")))?;

        let mut any = false;
        for (id, raw) in stored {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
                continue;
            };
            // Show what parses even if it does not verify — a certificate that
            // fails verification is exactly what the operator needs to see.
            let Ok(delegation) = cert::parse(&value) else {
                continue;
            };
            let verifies = cert::verify_self_contained(&value).is_ok();
            let status = if !verifies {
                "INVALID".to_string()
            } else if store.is_revoked_locally(&id) {
                "revoked".to_string()
            } else {
                delegation.status().to_string()
            };

            if !self.include_expired && status != "active" {
                continue;
            }

            if !any {
                println!(
                    "{:<28} {:<22} {:<22} STATUS",
                    "DELEGATION", "DELEGATE", "DELEGATOR"
                );
                any = true;
            }
            println!(
                "{:<28} {:<22} {:<22} {}",
                delegation.id.short(),
                delegation.delegate_name,
                delegation.delegator_name,
                status
            );
        }

        if !any {
            println!("No delegation certificates.");
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// verify
// ---------------------------------------------------------------------------

/// Verify a certificate.
#[derive(Debug, Parser)]
pub struct Verify {
    /// Delegation id, or a path to a certificate file.
    #[arg(required = true)]
    pub target: String,

    /// Skip the revocation check, which is the only step needing a network.
    #[arg(long)]
    pub offline: bool,

    /// Server profile to check revocation against.
    #[arg(long)]
    pub server: Option<String>,
}

impl Command for Verify {
    fn run(&self) -> CliResult<()> {
        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to create runtime: {e}")))?;
        rt.block_on(self.execute())
    }
}

impl Verify {
    async fn execute(&self) -> CliResult<()> {
        let path = PathBuf::from(&self.target);
        let raw = if path.exists() {
            std::fs::read_to_string(&path)?
        } else {
            let store = IdentityStore::open_default().map_err(|e| {
                CliError::Internal(anyhow::anyhow!("Failed to open identity store: {e}"))
            })?;
            load_document(&store, &self.target)?
        };

        let value: serde_json::Value =
            serde_json::from_str(&raw).map_err(|e| CliError::InvalidArgument {
                message: format!("not valid JSON: {e}"),
            })?;

        let delegation = match cert::verify_self_contained(&value) {
            Ok(d) => {
                println!("✓ Proof valid            signed by {}", d.delegator);
                println!("✓ Delegate key matches   {}", d.delegate);
                d
            }
            Err(e) => {
                return Err(CliError::DelegationError {
                    message: format!("✗ Verification failed: {e}"),
                })
            }
        };

        if delegation.is_expired() {
            return Err(CliError::DelegationError {
                message: format!(
                    "✗ Expired                on {}",
                    delegation
                        .expires
                        .map(|e| e.format("%Y-%m-%d").to_string())
                        .unwrap_or_default()
                ),
            });
        }
        match delegation.time_remaining() {
            Some(remaining) => println!(
                "✓ Not expired            {} days remaining",
                remaining.num_days()
            ),
            None => {
                println!("⚠ No expiry              an agent key with no expiry is a standing risk")
            }
        }

        if self.offline {
            println!("- Revocation             not checked (--offline)");
        } else {
            self.check_revocation(&delegation).await;
        }

        println!(
            "  Scope                  {} on {}",
            delegation
                .scope
                .permissions
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(", "),
            if delegation.scope.projects.is_empty() {
                "(all projects)".to_string()
            } else {
                delegation.scope.projects.join(", ")
            }
        );

        Ok(())
    }

    async fn check_revocation(&self, delegation: &atomic_identity::delegation::Delegation) {
        let store = match IdentityStore::open_default() {
            Ok(s) => s,
            Err(_) => return,
        };
        if store.is_revoked_locally(&delegation.id.to_base32()) {
            println!("✗ Revoked                recorded locally");
            return;
        }

        let Ok(delegator) = store.load_by_name(&delegation.delegator_name) else {
            println!("- Revocation             not checked (delegator not on this machine)");
            return;
        };
        let Ok((client, url)) =
            crate::commands::client::build_apex_client_as(&delegator, self.server.as_deref()).await
        else {
            println!("- Revocation             not checked (no server reachable)");
            return;
        };

        match client.delegation_status(&delegation.id.to_urn()).await {
            Ok(status) if status.status == "revoked" => {
                println!("✗ Revoked                {url} reports this delegation revoked")
            }
            Ok(_) => println!("✓ Not revoked            (checked {url})"),
            Err(e) => println!("- Revocation             not checked ({e})"),
        }
    }
}

// ---------------------------------------------------------------------------
// revoke
// ---------------------------------------------------------------------------

/// Revoke a certificate by id.
#[derive(Debug, Parser)]
pub struct Revoke {
    /// Delegation id (base32 or URN).
    #[arg(required = true)]
    pub id: String,

    /// Reason, recorded on the signed revocation.
    #[arg(long)]
    pub reason: Option<String>,

    /// Server profile to notify.
    #[arg(long)]
    pub server: Option<String>,

    /// Revoke locally without contacting a server.
    #[arg(long)]
    pub local: bool,
}

impl Command for Revoke {
    fn run(&self) -> CliResult<()> {
        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to create runtime: {e}")))?;
        rt.block_on(self.execute())
    }
}

impl Revoke {
    async fn execute(&self) -> CliResult<()> {
        let store = IdentityStore::open_default().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to open identity store: {e}"))
        })?;

        let id = normalize_id(&self.id)?;
        let raw = load_document(&store, &self.id)?;
        let value: serde_json::Value =
            serde_json::from_str(&raw).map_err(|e| CliError::InvalidArgument {
                message: format!("stored certificate is not valid JSON: {e}"),
            })?;
        let delegation = cert::parse(&value).map_err(|e| CliError::DelegationError {
            message: format!("stored certificate is malformed: {e}"),
        })?;

        let delegator = store
            .load_by_name(&delegation.delegator_name)
            .map_err(|_| CliError::DelegationError {
                message: format!(
                    "The delegating identity '{}' is not on this machine, so a signed \
                     revocation cannot be produced here.",
                    delegation.delegator_name
                ),
            })?;
        let keypair = store
            .load_keypair(&delegator.id, None)
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to load signing key: {e}")))?;

        let revocation =
            cert::mint_revocation(&delegator, &keypair, &delegation.id, self.reason.as_deref());
        let document = serde_json::to_string_pretty(&revocation)
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to encode: {e}")))?;
        store
            .save_revocation(&id, &document)
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to record revocation: {e}")))?;

        print_success(&format!("Revoked {}", delegation.id.to_urn()));

        if !self.local {
            let (client, url) =
                crate::commands::client::build_apex_client_as(&delegator, self.server.as_deref())
                    .await?;
            let request = RevokeDelegationRequest {
                revocation: revocation.clone(),
            };
            match client
                .revoke_delegation(&delegation.id.to_urn(), &request)
                .await
            {
                Ok(_) => println!("  Notified      {url}"),
                Err(e) => print_warning(&format!(
                    "Revoked locally, but {url} did not accept it: {e}\n  \
                     The server keeps honouring this delegation until it does."
                )),
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Accept either the base32 id or the full URN.
fn normalize_id(raw: &str) -> CliResult<String> {
    DelegationId::from_base32(raw)
        .map(|id| id.to_base32())
        .ok_or_else(|| CliError::InvalidArgument {
            message: format!("'{raw}' is not a delegation id"),
        })
}

fn load_document(store: &IdentityStore, raw: &str) -> CliResult<String> {
    let id = normalize_id(raw)?;
    store
        .load_delegation(&id)
        .map_err(|_| CliError::DelegationError {
            message: format!("No certificate stored under {raw}"),
        })
}

fn read_input(path: &str) -> CliResult<String> {
    if path == "-" {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        Ok(buf)
    } else {
        Ok(std::fs::read_to_string(path)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_accepted_in_both_renderings() {
        let id = DelegationId::from_bytes([3u8; 32]);
        assert_eq!(normalize_id(&id.to_base32()).unwrap(), id.to_base32());
        assert_eq!(normalize_id(&id.to_urn()).unwrap(), id.to_base32());
    }

    #[test]
    fn a_non_id_is_a_usage_error() {
        let err = normalize_id("nonsense!").unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }
}
