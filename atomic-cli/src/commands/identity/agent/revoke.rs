//! `atomic identity agent revoke` — withdraw an agent's authority.
//!
//! Because grants are presented rather than registered, the server holds no
//! list of what this agent has been issued — so revoking one certificate at a
//! time cannot be the whole story. Two things happen:
//!
//! 1. **The epoch is bumped.** Every grant issued to this agent before now is
//!    dead, including ones this machine has never seen. This is the part that
//!    actually stops the agent, and the only mechanism that covers grants
//!    issued from a laptop you no longer have.
//! 2. **Known grants are deny-listed**, with a signed revocation each, so the
//!    withdrawal is individually auditable and provable rather than a bare
//!    timestamp.
//!
//! The identity is kept. Deleting it would orphan the attribution on every
//! change the agent already recorded, turning a clean audit trail into a set of
//! unresolvable keys.
//!
//! Unlike issuing, revoking **must** reach the server. A credential the holder
//! possesses cannot prove its own withdrawal, so this is the one operation
//! where a failed network call leaves real work outstanding — and it says so.

use clap::Parser;

use atomic_canonical::delegation as cert;
use atomic_identity::IdentityStore;
use atomic_remote::storage_types::RevokeDelegationRequest;

use crate::commands::delegation::load_for_delegate;
use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_success, print_warning};

/// Revoke an agent's delegation.
#[derive(Debug, Parser)]
pub struct Revoke {
    /// Agent identity name.
    #[arg(required = true)]
    pub name: String,

    /// Why, recorded on the signed revocation.
    #[arg(long)]
    pub reason: Option<String>,

    /// Also delete the agent's secret key from this machine.
    ///
    /// The identity record and its history are kept either way.
    #[arg(long)]
    pub retire: bool,

    /// Server profile to notify.
    #[arg(long)]
    pub server: Option<String>,

    /// Revoke locally without contacting a server.
    ///
    /// Stops *this machine* from using the agent. The server keeps honouring
    /// every outstanding grant until told, so this is a stopgap, not a
    /// revocation.
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
        let mut store = IdentityStore::open_default().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to open identity store: {e}"))
        })?;

        let agent = store
            .load_by_name(&self.name)
            .map_err(|_| CliError::IdentityNotFound(self.name.clone()))?;

        let delegations = load_for_delegate(&store, &agent)?;
        if delegations.is_empty() {
            return Err(CliError::DelegationError {
                message: format!("'{}' has no delegation to revoke", self.name),
            });
        }

        // Revoke every live certificate, not just the newest. Leaving an older
        // one standing would make revocation look done while the agent kept
        // working under a certificate nobody was looking at.
        let mut revoked = Vec::new();
        for resolved in delegations.iter().filter(|d| d.is_usable()) {
            let delegator = store
                .load_by_name(&resolved.delegation.delegator_name)
                .map_err(|_| CliError::DelegationError {
                    message: format!(
                        "The delegating identity '{}' is not on this machine, so a signed \
                         revocation cannot be produced here.",
                        resolved.delegation.delegator_name
                    ),
                })?;
            let keypair = store.load_keypair(&delegator.id, None).map_err(|e| {
                CliError::Internal(anyhow::anyhow!("Failed to load signing key: {e}"))
            })?;

            let revocation = cert::mint_revocation(
                &delegator,
                &keypair,
                &resolved.delegation.id,
                self.reason.as_deref(),
            );
            let document = serde_json::to_string_pretty(&revocation).map_err(|e| {
                CliError::Internal(anyhow::anyhow!("Failed to encode revocation: {e}"))
            })?;

            // Local first: this machine stops using the delegation immediately,
            // even if the network call below fails.
            store
                .save_revocation(&resolved.id(), &document)
                .map_err(|e| {
                    CliError::Internal(anyhow::anyhow!("Failed to record revocation: {e}"))
                })?;

            revoked.push((resolved.delegation.id.to_urn(), delegator, revocation));
        }

        if revoked.is_empty() {
            print_hint(&format!(
                "'{}' has no active delegation — nothing to revoke.",
                self.name
            ));
            return Ok(());
        }

        print_success(&format!(
            "Revoked {} delegation(s) for {}",
            revoked.len(),
            agent.name
        ));
        for (urn, _, _) in &revoked {
            println!("  {urn}");
        }
        if let Some(reason) = &self.reason {
            println!("  Reason        {reason}");
        }

        if !self.local {
            self.notify_server(&agent, &revoked).await;
        }

        if self.retire {
            match store.delete(&agent.id) {
                Ok(()) => println!("  Key           deleted from this machine"),
                Err(e) => print_warning(&format!("Could not delete the agent key: {e}")),
            }
        }

        println!();
        print_hint("Past changes stay attributable — the identity and its history are kept.");

        Ok(())
    }

    /// Tell the server: bump the epoch, then deny-list each known grant.
    ///
    /// The epoch first, because it is the part that actually stops the agent
    /// and covers grants nobody has a copy of. Deny-listing the ones we do know
    /// is the audit trail on top.
    async fn notify_server(
        &self,
        agent: &atomic_identity::Identity,
        revoked: &[(String, atomic_identity::Identity, serde_json::Value)],
    ) {
        let Some((_, delegator, _)) = revoked.first() else {
            return;
        };

        let Ok((client, url)) =
            crate::commands::client::build_apex_client_as(delegator, self.server.as_deref()).await
        else {
            print_warning(
                "Revoked locally, but no server could be reached. Outstanding grants stay \
                 valid until the server is told.\n  \
                 Retry with:  atomic identity agent revoke <name>",
            );
            return;
        };

        match self.bump_epoch(&client, agent).await {
            Ok(()) => println!("  Epoch bumped  {url} — every outstanding grant is now dead"),
            Err(e) => print_warning(&format!(
                "Could not bump the epoch at {url}: {e}\n  \
                 Grants this machine has never seen REMAIN VALID until it succeeds.\n  \
                 Retry with:  atomic identity agent revoke {}",
                agent.name
            )),
        }

        for (urn, _, revocation) in revoked {
            let request = RevokeDelegationRequest {
                revocation: revocation.clone(),
            };
            match client.revoke_delegation(urn, &request).await {
                Ok(_) => println!("  Deny-listed   {urn}"),
                Err(e) => print_warning(&format!("Could not deny-list {urn}: {e}")),
            }
        }
    }

    /// Find the agent's server-side id and bump its epoch.
    ///
    /// The lookup is by DID rather than name: names are not unique across
    /// machines, and bumping the epoch on the wrong agent would silently do
    /// nothing while reporting success.
    async fn bump_epoch(
        &self,
        client: &atomic_remote::StorageClient,
        agent: &atomic_identity::Identity,
    ) -> CliResult<()> {
        let did = agent.id.to_did();
        let agents = client
            .list_agents()
            .await
            .map_err(|e| CliError::RemoteError {
                message: format!("Could not list agents: {e}"),
                url: None,
            })?;

        let found = agents
            .iter()
            .find(|a| a.did == did)
            .ok_or_else(|| CliError::RemoteError {
                message: format!(
                    "'{}' is not enrolled with this server, so there is no epoch to bump",
                    agent.name
                ),
                url: None,
            })?;

        client
            .set_agent_epoch(&found.id.to_string())
            .await
            .map(|_| ())
            .map_err(|e| CliError::RemoteError {
                message: e.to_string(),
                url: None,
            })
    }
}
