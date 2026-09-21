//! `atomic identity agent create` — issue an agent identity in one command.
//!
//! Six things have to happen for an agent to be able to work on your behalf,
//! and none of them is useful alone: generate a keypair, create a delegated
//! identity, sign a certificate binding the two, store it, enroll it with the
//! server, and bind it in config so hooks find it. This command does all six
//! and reports what it did.
//!
//! # Usage
//!
//! ```text
//! atomic identity agent create <NAME> [OPTIONS]
//!
//! Options:
//!       --agent-type <SLUG>   Software agent (claude-code, gemini-cli, ...)
//!       --can <PERMS>         Comma-separated: read,record,push,pull,...
//!       --projects <GLOBS>    Comma-separated project patterns
//!       --workspaces <GLOBS>  Comma-separated workspace patterns
//!       --views <GLOBS>       Comma-separated view patterns
//!       --expires <DURATION>  30d, 12h, 2w (default: 30d)
//!       --max-changes <N>     Cap on changes this agent may create
//!   -i, --identity <NAME>     Delegating identity (default: your default)
//!       --server <NAME>       Server profile to enroll with
//!       --local               Skip server enrollment
//! ```

use chrono::Utc;
use clap::Parser;

use atomic_canonical::delegation as cert;
use atomic_identity::delegation::{Delegation, DelegationScope};
use atomic_identity::{Identity, IdentityStore, IdentityType, IdentityUsage, KeyPair};
use atomic_remote::storage_types::EnrollAgentRequest;

use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_success, print_warning};

use super::{
    humanize_remaining, parse_duration, parse_patterns, parse_permissions, software_agent_urn,
    DEFAULT_EXPIRY_DAYS,
};

/// Create an agent identity and delegate to it.
#[derive(Debug, Parser)]
pub struct Create {
    /// Short name for the agent.
    ///
    /// The stored identity is named `<you>+<name>` — the plus-tag convention
    /// the agent recording path already uses, so `log` and `blame` read as an
    /// agent at a glance and the email still routes to you.
    #[arg(required = true)]
    pub name: String,

    /// Software agent slug (`claude-code`, `gemini-cli`, `codex`, ...).
    ///
    /// Recorded as `urn:atomic:agent:<slug>` on the certificate. Descriptive
    /// only: it says what kind of software holds the key, not what it may do.
    #[arg(long = "agent-type")]
    pub agent_type: Option<String>,

    /// Permissions to grant, comma-separated.
    #[arg(long, default_value = "read,record,push")]
    pub can: String,

    /// Project patterns the agent may touch, comma-separated globs.
    ///
    /// Omit to leave projects unrestricted — which still does not widen
    /// anything, since the agent is capped by your own access regardless.
    #[arg(long)]
    pub projects: Option<String>,

    /// Workspace patterns, comma-separated globs.
    #[arg(long)]
    pub workspaces: Option<String>,

    /// View patterns, comma-separated globs.
    #[arg(long)]
    pub views: Option<String>,

    /// How long the delegation lasts (`30d`, `12h`, `2w`).
    #[arg(long)]
    pub expires: Option<String>,

    /// Cap the number of changes this agent may create.
    #[arg(long)]
    pub max_changes: Option<u64>,

    /// Which of your identities delegates. Defaults to your default identity.
    #[arg(short, long)]
    pub identity: Option<String>,

    /// Server profile to enroll with.
    #[arg(long)]
    pub server: Option<String>,

    /// Create and sign locally without contacting a server.
    ///
    /// The certificate is still valid and still verifiable; it just is not
    /// known to any server yet. `atomic identity delegation push` enrolls it
    /// later.
    #[arg(long)]
    pub local: bool,

    /// Print the certificate as JSON instead of a summary.
    #[arg(long)]
    pub json: bool,
}

impl Command for Create {
    fn run(&self) -> CliResult<()> {
        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to create runtime: {e}")))?;
        rt.block_on(self.execute())
    }
}

impl Create {
    async fn execute(&self) -> CliResult<()> {
        let store = IdentityStore::open_default().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to open identity store: {e}"))
        })?;

        // 1. The delegator. An agent cannot delegate — allowing it would make
        //    the chain of custody a graph and the revocation story unbounded.
        let delegator = load_delegator(&store, self.identity.as_deref())?;
        if delegator.identity_type.is_delegated() || delegator.identity_type.is_agent() {
            return Err(CliError::InvalidArgument {
                message: format!(
                    "'{}' is itself an agent identity and cannot delegate.\n  \
                     Pass a human identity with --identity <name>.",
                    delegator.name
                ),
            });
        }

        let agent_name = format!("{}+{}", delegator.name, self.name);
        if store.exists_by_name(&agent_name) {
            return Err(CliError::IdentityAlreadyExists(agent_name));
        }

        // 2. The server the certificate will be bound to. Resolved before
        //    signing because it is part of what gets signed.
        let server_url = if self.local {
            None
        } else {
            Some(crate::commands::client::apex_server_url(
                self.server.as_deref(),
            )?)
        };

        // 3. The agent's own keypair.
        let keypair = KeyPair::generate();
        let mut agent_builder = Identity::builder(&agent_name)
            .identity_type(IdentityType::Agent)
            .usage(IdentityUsage::Bot)
            .public_key(keypair.public.clone())
            .delegated_by(delegator.id)
            .description(format!(
                "Agent identity acting on behalf of {}",
                delegator.name
            ));
        if let Some(email) = delegator.email.as_deref() {
            agent_builder = agent_builder.email(plus_tag_email(email, &self.name));
        }
        let agent = agent_builder
            .build()
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to build identity: {e}")))?;

        // 4. The scope, then the certificate.
        let scope = self.build_scope(server_url.as_deref())?;
        let expires = self
            .expires
            .as_deref()
            .map(parse_duration)
            .transpose()?
            .unwrap_or_else(|| chrono::Duration::days(DEFAULT_EXPIRY_DAYS));

        let mut terms = Delegation::new(&delegator, &agent, scope).expires_in(expires);
        if let Some(slug) = &self.agent_type {
            terms = terms.with_software_agent(software_agent_urn(slug));
        }

        let delegator_keypair = store.load_keypair(&delegator.id, None).map_err(|e| {
            CliError::Internal(anyhow::anyhow!(
                "Failed to load the signing key for '{}': {e}",
                delegator.name
            ))
        })?;
        let certificate = cert::mint(&delegator, &delegator_keypair, &terms);

        // 5. Persist. The identity and its key first — a certificate naming a
        //    key that was never stored is worse than no certificate.
        store
            .save_with_keypair(&agent, &keypair, None)
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to save identity: {e}")))?;

        let document = serde_json::to_string_pretty(&certificate).map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to encode certificate: {e}"))
        })?;
        let delegation_id = terms.id.to_base32();
        store
            .save_delegation(&delegation_id, &document)
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to store certificate: {e}")))?;

        if self.json {
            println!("{document}");
            return Ok(());
        }

        // 6. Enroll, and bind in config so hooks find the key without flags.
        let enrolled = match &server_url {
            Some(url) => self.enroll(&delegator, &agent, &certificate, url).await,
            None => Ok(false),
        };

        self.report(&delegator, &agent, &terms, server_url.as_deref(), &enrolled);

        if server_url.is_some() && matches!(enrolled, Ok(true)) {
            bind_agent_in_config(self.server.as_deref(), &agent.name);
        }

        Ok(())
    }

    /// Assemble the scope from the flags.
    fn build_scope(&self, server_url: Option<&str>) -> CliResult<DelegationScope> {
        let mut builder = DelegationScope::builder().permissions(parse_permissions(&self.can)?);

        // Bind to the server unless this is a local-only certificate. A
        // certificate with no server is valid everywhere, which is exactly what
        // we do not want once one is in play.
        if let Some(url) = server_url {
            builder = builder.server(url);
        }
        for pattern in self
            .projects
            .as_deref()
            .map(parse_patterns)
            .unwrap_or_default()
        {
            builder = builder.project(pattern);
        }
        for pattern in self
            .workspaces
            .as_deref()
            .map(parse_patterns)
            .unwrap_or_default()
        {
            builder = builder.workspace(pattern);
        }
        for pattern in self
            .views
            .as_deref()
            .map(parse_patterns)
            .unwrap_or_default()
        {
            builder = builder.view(pattern);
        }
        if let Some(max) = self.max_changes {
            builder = builder.max_changes(max);
        }
        Ok(builder.build())
    }

    /// Enroll the agent key and certificate with the server.
    ///
    /// Returns `Ok(false)` when the server does not implement agent identities
    /// yet — an older deployment is a reason to say so and carry on with a
    /// locally valid certificate, not to fail after the key already exists.
    async fn enroll(
        &self,
        delegator: &Identity,
        agent: &Identity,
        certificate: &serde_json::Value,
        server_url: &str,
    ) -> CliResult<bool> {
        let (client, _) =
            crate::commands::client::build_apex_client_as(delegator, self.server.as_deref())
                .await?;

        let request = EnrollAgentRequest {
            name: agent.name.clone(),
            email: agent.email.clone(),
            public_key: agent.public_key_base32(),
            certificate: certificate.clone(),
        };

        match client.enroll_agent(&request).await {
            Ok(_) => Ok(true),
            Err(e) if is_unsupported(&e) => {
                print_warning(&format!(
                    "{server_url} does not support agent identities yet — the certificate is \
                     valid locally but the server will not accept this key.\n  \
                     Enroll it once the server is upgraded:  atomic identity delegation push"
                ));
                Ok(false)
            }
            Err(e) => Err(CliError::RemoteError {
                message: format!("Failed to enroll agent: {e}"),
                url: Some(server_url.to_string()),
            }),
        }
    }

    fn report(
        &self,
        delegator: &Identity,
        agent: &Identity,
        terms: &Delegation,
        server_url: Option<&str>,
        enrolled: &CliResult<bool>,
    ) {
        print_success(&format!("Created agent identity  {}", agent.name));
        println!();
        println!("  DID           {}", agent.id.to_did());
        println!("  Key           {}", agent.public_key.to_did_key());
        println!(
            "  Delegated by  {}  ({})",
            delegator.name,
            delegator.id.to_did()
        );
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
        if !terms.scope.projects.is_empty() {
            println!("  On            {}", terms.scope.projects.join(", "));
        }
        if let Some(max) = terms.scope.max_changes {
            println!("  Change cap    {max}");
        }
        match terms.expires {
            Some(expires) => println!(
                "  Expires       {}  ({})",
                expires.format("%Y-%m-%d"),
                humanize_remaining(Some(expires - Utc::now()))
            ),
            None => println!("  Expires       never"),
        }
        println!("  Delegation    {}", terms.id.to_urn());

        println!();
        match (server_url, enrolled) {
            (Some(url), Ok(true)) => {
                println!("Registered with {url}");
                println!("  Bound in      ~/.atomic/config.toml (agent_identity)");
            }
            (Some(_), Ok(false)) => {}
            (Some(url), Err(e)) => print_warning(&format!("Not registered with {url}: {e}")),
            (None, _) => print_hint("Local only — run 'atomic identity delegation push' to enroll"),
        }

        println!();
        println!("{}", crate::output::hint("Next steps:"));
        println!(
            "  {}  Inspect this agent",
            crate::output::command(&format!("atomic identity agent show {}", agent.name))
        );
        println!(
            "  {}  Withdraw it",
            crate::output::command(&format!("atomic identity agent revoke {}", agent.name))
        );
    }
}

/// Load the delegating identity: `--identity`, else the store default.
fn load_delegator(store: &IdentityStore, name: Option<&str>) -> CliResult<Identity> {
    match name {
        Some(name) => store
            .load_by_name(name)
            .map_err(|_| CliError::IdentityNotFound(name.to_string())),
        None => store
            .get_default()
            .map_err(|e| {
                CliError::Internal(anyhow::anyhow!("Failed to load default identity: {e}"))
            })?
            .ok_or_else(|| {
                CliError::Internal(anyhow::anyhow!(
                    "No default identity set. Create one first:\n  \
                     atomic identity new <name> --email <email> --set-default"
                ))
            }),
    }
}

/// `alice@example.com` + `claude` → `alice+claude@example.com`.
///
/// Plus-addressing is stripped by mail servers, so replies still reach the
/// human while the address itself says an agent produced the change.
fn plus_tag_email(email: &str, tag: &str) -> String {
    match email.split_once('@') {
        Some((local, domain)) => {
            // Do not stack tags if the base address already carries one.
            let local = local.split('+').next().unwrap_or(local);
            format!("{local}+{tag}@{domain}")
        }
        None => email.to_string(),
    }
}

/// Is this the far end saying it has never heard of agent identities?
fn is_unsupported(error: &atomic_remote::RemoteError) -> bool {
    let message = error.to_string();
    message.contains("404") || message.to_lowercase().contains("not found")
}

/// Record the agent identity on the server profile so hooks use it by default.
///
/// Best-effort: failing to write config must not undo a successful enrollment,
/// so this warns rather than erroring.
fn bind_agent_in_config(server_override: Option<&str>, agent_name: &str) {
    if let Err(e) = crate::commands::identity::bind_agent_identity(server_override, agent_name) {
        print_warning(&format!(
            "Agent enrolled, but the config binding could not be written: {e}\n  \
             Pass --identity {agent_name} explicitly, or set agent_identity by hand."
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plus_tag_email_routes_back_to_the_human() {
        assert_eq!(
            plus_tag_email("alice@example.com", "claude"),
            "alice+claude@example.com"
        );
    }

    #[test]
    fn plus_tags_do_not_stack() {
        // Delegating from an identity that already has a tag must not produce
        // alice+work+claude@ — mail servers strip from the first '+'.
        assert_eq!(
            plus_tag_email("alice+work@example.com", "claude"),
            "alice+claude@example.com"
        );
    }

    #[test]
    fn a_malformed_address_is_left_alone() {
        assert_eq!(plus_tag_email("not-an-email", "claude"), "not-an-email");
    }
}
