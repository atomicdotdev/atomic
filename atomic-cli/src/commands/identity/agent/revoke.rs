//! `atomic identity agent revoke` — withdraw an agent's authorization.
//!
//! Revocation signs a document rather than just calling an endpoint, so the
//! withdrawal is itself verifiable and survives being recorded offline. The
//! local copy is written *first*: from that moment this machine refuses to mint
//! a token for the agent, whether or not the server can be reached.
//!
//! The identity is kept. Deleting it would orphan the attribution on every
//! change the agent already recorded, turning a clean audit trail into a set of
//! unresolvable keys.

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
            self.notify_server(&revoked).await;
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

    async fn notify_server(
        &self,
        revoked: &[(String, atomic_identity::Identity, serde_json::Value)],
    ) {
        for (urn, delegator, revocation) in revoked {
            let client =
                crate::commands::client::build_apex_client_as(delegator, self.server.as_deref())
                    .await;
            let Ok((client, url)) = client else {
                print_warning(
                    "Revoked locally, but no server could be reached. The server will keep \
                     accepting this delegation until it is told.\n  \
                     Retry with:  atomic identity delegation revoke <urn>",
                );
                return;
            };

            let request = RevokeDelegationRequest {
                revocation: revocation.clone(),
            };
            match client.revoke_delegation(urn, &request).await {
                Ok(_) => println!("  Notified      {url}"),
                Err(e) => print_warning(&format!(
                    "Revoked locally, but {url} did not accept the revocation: {e}\n  \
                     The server will keep honouring this delegation until it does.\n  \
                     Retry with:  atomic identity delegation revoke {urn}"
                )),
            }
        }
    }
}
