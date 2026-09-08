//! `atomic identity agent renew` — a fresh grant carrying the previous scope.
//!
//! A convenience over `atomic identity grant new`: it reads the agent's current
//! scope and reissues it with a new expiry, so you do not have to retype
//! `--can` and `--projects` to extend something that was already right.
//!
//! Renewal deliberately does *not* touch the keypair. Rotating the key would
//! mean re-enrolling with every server and would break attribution for work
//! already recorded; what expires is the authorization, so that is what gets
//! replaced.
//!
//! Like every other issuance, this reaches no server. The new grant is usable
//! the moment it is signed. `--publish` sends it up for visibility only.
//!
//! # Renewal does not withdraw the old grant
//!
//! The previous certificate stays valid until its own expiry. That is fine when
//! extending — the old one is strictly narrower in time — but it means renewing
//! with a *tighter* scope does not take the wider one away. To make a narrowing
//! bite immediately, bump the epoch:
//!
//! ```text
//! atomic identity agent revoke <agent>     # bumps the epoch, then reissue
//! ```

use chrono::Duration;
use clap::Parser;

use atomic_canonical::delegation as cert;
use atomic_identity::delegation::{Delegation, DelegationScope};
use atomic_identity::{Identity, IdentityStore};
use atomic_remote::storage_types::PushDelegationRequest;

use crate::commands::delegation::load_for_delegate;
use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_success, print_warning};

use super::{
    humanize_remaining, parse_duration, parse_patterns, parse_permissions, DEFAULT_EXPIRY_DAYS,
};

/// Issue a fresh certificate for an existing agent identity.
#[derive(Debug, Parser)]
pub struct Renew {
    /// Agent identity name.
    #[arg(required = true)]
    pub name: String,

    /// New lifetime (`30d`, `12h`, `2w`).
    #[arg(long)]
    pub expires: Option<String>,

    /// Replace the permission list. Omit to carry the current one forward.
    #[arg(long)]
    pub can: Option<String>,

    /// Replace the project patterns. Omit to carry them forward.
    #[arg(long)]
    pub projects: Option<String>,

    /// Server profile to publish to, with `--publish`.
    #[arg(long)]
    pub server: Option<String>,

    /// Also publish the new grant so it appears in server listings.
    ///
    /// Optional and off by default: a grant works the moment it is signed, and
    /// publishing only makes the server able to *show* it.
    #[arg(long)]
    pub publish: bool,
}

impl Command for Renew {
    fn run(&self) -> CliResult<()> {
        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to create runtime: {e}")))?;
        rt.block_on(self.execute())
    }
}

impl Renew {
    async fn execute(&self) -> CliResult<()> {
        let store = IdentityStore::open_default().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to open identity store: {e}"))
        })?;

        let agent = store
            .load_by_name(&self.name)
            .map_err(|_| CliError::IdentityNotFound(self.name.clone()))?;

        // Renew from the newest certificate even when it is expired or revoked
        // — its scope is the record of what was intended, and expiry is the
        // very thing being fixed.
        let previous = load_for_delegate(&store, &agent)?
            .into_iter()
            .next()
            .ok_or_else(|| CliError::DelegationError {
                message: format!(
                    "'{}' has no certificate to renew.\n  Issue one with:  atomic identity delegate {}",
                    self.name, self.name
                ),
            })?;

        let delegator = store
            .load_by_name(&previous.delegation.delegator_name)
            .map_err(|_| CliError::DelegationError {
                message: format!(
                    "The delegating identity '{}' is not on this machine, so a renewal cannot \
                     be signed here.\n  Renew from the machine holding that key.",
                    previous.delegation.delegator_name
                ),
            })?;

        // Guard against a name collision resolving to a different key than the
        // one that signed the original.
        if delegator.id.to_did() != previous.delegation.delegator {
            return Err(CliError::DelegationError {
                message: format!(
                    "Identity '{}' on this machine is not the key that issued the current \
                     certificate.\n  Renew from the machine holding {}.",
                    previous.delegation.delegator_name, previous.delegation.delegator
                ),
            });
        }

        let scope = self.next_scope(&previous.delegation.scope)?;
        let expires = self
            .expires
            .as_deref()
            .map(parse_duration)
            .transpose()?
            .unwrap_or_else(|| Duration::days(DEFAULT_EXPIRY_DAYS));

        let mut terms = Delegation::new(&delegator, &agent, scope).expires_in(expires);
        if let Some(agent_urn) = &previous.delegation.software_agent {
            terms = terms.with_software_agent(agent_urn.clone());
        }

        let keypair = store.load_keypair(&delegator.id, None).map_err(|e| {
            CliError::Internal(anyhow::anyhow!(
                "Failed to load the signing key for '{}': {e}",
                delegator.name
            ))
        })?;
        let certificate = cert::mint(&delegator, &keypair, &terms);

        let document = serde_json::to_string_pretty(&certificate).map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to encode certificate: {e}"))
        })?;
        store
            .save_delegation(&terms.id.to_base32(), &document)
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to store certificate: {e}")))?;

        print_success(&format!("Renewed delegation for {}", agent.name));
        println!();
        println!("  Delegation    {}", terms.id.to_urn());
        println!(
            "  Can           {}",
            terms
                .scope
                .permissions
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        if let Some(expiry) = terms.expires {
            println!(
                "  Expires       {}  ({})",
                expiry.format("%Y-%m-%d"),
                humanize_remaining(terms.time_remaining())
            );
        }
        println!("  Key           unchanged — nothing to re-enroll");
        println!("  Ready to use  no server call needed");

        if self.publish {
            self.publish_grant(&delegator, &certificate).await?;
        }

        Ok(())
    }

    /// Carry the previous scope forward, replacing only what was named.
    fn next_scope(&self, previous: &DelegationScope) -> CliResult<DelegationScope> {
        let mut scope = previous.clone();
        if let Some(can) = &self.can {
            scope.permissions = parse_permissions(can)?;
        }
        if let Some(projects) = &self.projects {
            scope.projects = parse_patterns(projects);
        }
        Ok(scope)
    }

    /// Publish for visibility. Never required for the grant to work.
    async fn publish_grant(
        &self,
        delegator: &Identity,
        certificate: &serde_json::Value,
    ) -> CliResult<()> {
        let (client, url) =
            crate::commands::client::build_apex_client_as(delegator, self.server.as_deref())
                .await?;

        let request = PushDelegationRequest {
            certificate: certificate.clone(),
        };
        match client.push_delegation(&request).await {
            Ok(_) => {
                println!("  Published to  {url}");
                Ok(())
            }
            Err(e) => {
                // Publishing is cosmetic — the grant is already signed and
                // usable — so a server that will not take it is a warning, not
                // something that should discard the renewal.
                print_warning(&format!(
                    "Renewed, but {url} would not list it: {e}\n  \
                     The grant still works; only the listing is missing.\n  \
                     Retry with:  atomic identity grant publish"
                ));
                Ok(())
            }
        }
    }
}
