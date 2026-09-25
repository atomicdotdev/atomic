//! `atomic identity grant` — managing grants after they are issued.
//!
//! Load one that was issued elsewhere, list what this machine holds, verify
//! one, revoke one, or publish one for visibility.
//!
//! # Loading is the common one
//!
//! A grant is issued on the human's machine and used on the agent's. Three ways
//! across, in rough order of how often you will want them:
//!
//! | | |
//! |---|---|
//! | `ATOMIC_DELEGATION=<base64>` | nothing to install; the automation path |
//! | `atomic identity grant load <file>` | a file dropped on the agent's machine |
//! | `atomic identity grant load -` | piped in over stdin |
//!
//! # Publishing is optional
//!
//! `publish` sends a grant to the server so it shows up in listings. It is
//! **not** required to use one: the server verifies a presented grant against
//! your registered key, so a grant works the moment you sign it. Publish when
//! you want a dashboard to know, not to make something function.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use atomic_canonical::delegation as cert;
use atomic_canonical::jcs;
use atomic_identity::delegation::DelegationId;
use atomic_identity::IdentityStore;
use atomic_remote::storage_types::{PushDelegationRequest, RevokeDelegationRequest};

use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_success, print_warning};

/// Grant management.
#[derive(Debug, clap::Args)]
pub struct DelegationCmd {
    #[command(subcommand)]
    pub command: DelegationCommands,
}

/// Available delegation subcommands.
#[derive(Debug, Subcommand)]
pub enum DelegationCommands {
    /// Issue a grant to an agent. The operation you run often.
    ///
    /// Boxed because it carries every scope flag and dwarfs the other variants
    /// — unboxed, each of them would pay for its size.
    New(Box<super::delegate::Delegate>),
    /// Load a grant issued on another machine.
    #[command(alias = "install")]
    Load(Install),
    /// Publish a grant so it appears in server listings.
    ///
    /// Optional: a grant works without this. Grants are presented with each
    /// request and verified against your registered key, so the server needs no
    /// advance notice.
    #[command(alias = "push")]
    Publish(Push),
    /// List grants held on this machine.
    List(List),
    /// Verify a grant's proof, expiry and revocation.
    Verify(Verify),
    /// Revoke a grant by id, or every grant you have issued.
    Revoke(Revoke),
}

impl Command for DelegationCmd {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            DelegationCommands::New(c) => c.run(),
            DelegationCommands::Load(c) => c.run(),
            DelegationCommands::Publish(c) => c.run(),
            DelegationCommands::List(c) => c.run(),
            DelegationCommands::Verify(c) => c.run(),
            DelegationCommands::Revoke(c) => c.run(),
        }
    }
}

// ---------------------------------------------------------------------------
// install
// ---------------------------------------------------------------------------

/// Load a grant from a file or stdin.
#[derive(Debug, Parser)]
pub struct Install {
    /// Path to the grant, or `-` for stdin.
    #[arg(required = true)]
    pub path: String,
}

impl Command for Install {
    fn run(&self) -> CliResult<()> {
        let raw = read_input(&self.path)?;
        // Admission before the value exists: the raw bytes are what gets stored
        // and later re-verified, so a fault the parse would have swallowed is a
        // fault this machine would keep.
        let value = jcs::admit_document(raw.as_bytes()).map_err(|e| CliError::InvalidArgument {
            message: format!("{} was refused: {e}", self.path),
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
            "Ready to use. The signature checks out, which proves the grant was not \
             altered; that the delegator is who you think is settled by the server's \
             registered key when you use it.",
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
            let Ok(value) = jcs::admit_document(raw.as_bytes()) else {
                print_warning(&format!("Skipping {id}: refused at the ingest boundary"));
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
            let Ok(value) = jcs::admit_document(raw.as_bytes()) else {
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

        let value = jcs::admit_document(raw.as_bytes()).map_err(|e| CliError::InvalidArgument {
            message: format!("refused: {e}"),
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
            Ok(status) if status.revoked => {
                println!("✗ Revoked                {url} reports this grant revoked")
            }
            Ok(_) => println!("✓ Not revoked            (checked {url})"),
            Err(e) => println!("- Revocation             not checked ({e})"),
        }
    }
}

// ---------------------------------------------------------------------------
// revoke
// ---------------------------------------------------------------------------

/// Revoke a grant by id, or every grant you have issued.
#[derive(Debug, Parser)]
pub struct Revoke {
    /// Grant id (base32 or URN). Omit when using `--all-mine`.
    #[arg(required_unless_present = "all_mine")]
    pub id: Option<String>,

    /// Invalidate **every** grant you have ever issued, to every agent.
    ///
    /// The key-compromise button. If your signing key leaks, an attacker can
    /// mint grants the server has never seen and there is no list to revoke —
    /// this is the one action that reaches them, because it works on time
    /// rather than on identifiers.
    ///
    /// Your agents stop working until you issue fresh grants. That is the
    /// intended effect.
    #[arg(long, conflicts_with = "id")]
    pub all_mine: bool,

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

        if self.all_mine {
            return self.revoke_everything(&store).await;
        }

        let raw_id = self
            .id
            .as_deref()
            .expect("clap requires one of id/--all-mine");
        let id = normalize_id(raw_id)?;
        let raw = load_document(&store, raw_id)?;
        let value = jcs::admit_document(raw.as_bytes()).map_err(|e| CliError::InvalidArgument {
            message: format!("stored grant was refused: {e}"),
        })?;
        let delegation = cert::parse(&value).map_err(|e| CliError::DelegationError {
            message: format!("stored grant is malformed: {e}"),
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

        // Local first: from here this machine will not present the grant,
        // whether or not the network call below succeeds.
        store
            .save_revocation(&id, &document)
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to record revocation: {e}")))?;

        print_success(&format!("Revoked {}", delegation.id.to_urn()));

        if self.local {
            print_warning("Local only. The server keeps honouring this grant until it is told.");
            return Ok(());
        }

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
            Ok(_) => println!("  Deny-listed   {url}"),
            Err(e) => print_warning(&format!(
                "Revoked locally, but {url} did not accept it: {e}\n  \
                 The grant stays usable against that server until it does.\n  \
                 Retry with:  atomic identity grant revoke {}",
                delegation.id.to_urn()
            )),
        }

        Ok(())
    }

    /// Bump the delegator's epoch: everything they have issued, to anyone, dies.
    ///
    /// There is nothing to sign per-grant here and nothing to record locally,
    /// because the whole point is to reach grants this machine has never seen.
    /// It is purely a server-side statement about time, so unlike a targeted
    /// revocation it is useless offline — and says so rather than pretending.
    async fn revoke_everything(&self, store: &IdentityStore) -> CliResult<()> {
        if self.local {
            return Err(CliError::InvalidArgument {
                message: "--all-mine cannot be done locally: it is a statement the server \
                          makes about every grant you have issued, including ones this \
                          machine has never seen."
                    .to_string(),
            });
        }

        let delegator = super::load_identity_or_default(store, None)?;
        let (client, url) =
            crate::commands::client::build_apex_client_as(&delegator, self.server.as_deref())
                .await?;

        let result = client
            .set_delegator_epoch()
            .await
            .map_err(|e| CliError::RemoteError {
                message: format!("Could not set the epoch: {e}"),
                url: Some(url.clone()),
            })?;

        print_success(&format!(
            "Every grant issued by '{}' is now invalid at {url}",
            delegator.name
        ));
        if let Some(at) = result.delegations_valid_from {
            println!("  Epoch         {}", at.format("%Y-%m-%d %H:%M:%S UTC"));
        }
        println!();
        print_hint(
            "Your agents will stop working until you issue fresh grants:  \
             atomic identity grant new <agent>",
        );

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
