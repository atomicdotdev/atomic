//! `atomic identity delegate` — mint a certificate (plumbing).
//!
//! The step `atomic identity agent create` performs on your behalf, exposed on
//! its own for the two cases the porcelain cannot cover:
//!
//! - **Re-scoping** an agent that already exists, without touching its key.
//! - **Countersigning a request** (`--request`), where the agent generated its
//!   own key somewhere you will never see it — a CI runner, a hosted agent. The
//!   request is self-signed by that key, which proves the far end really holds
//!   it, so you are not delegating to a key nobody has.

use std::path::PathBuf;

use clap::Parser;

use atomic_canonical::delegation as cert;
use atomic_identity::delegation::{Delegation, DelegationScope};
use atomic_identity::{Identity, IdentityStore, IdentityType};

use crate::commands::identity::agent::{
    parse_duration, parse_patterns, parse_permissions, software_agent_urn, DEFAULT_EXPIRY_DAYS,
};
use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_success};

/// Mint a delegation certificate for an agent.
#[derive(Debug, Parser)]
pub struct Delegate {
    /// Agent identity to delegate to. Omit when using `--request`.
    pub agent: Option<String>,

    /// Countersign a self-signed `AgentDelegationRequest` from a file.
    ///
    /// Use `-` to read the request from stdin.
    #[arg(long, conflicts_with = "agent")]
    pub request: Option<String>,

    /// Permissions, comma-separated.
    #[arg(long, default_value = "read,record,push")]
    pub can: String,

    /// Project patterns, comma-separated globs.
    #[arg(long)]
    pub projects: Option<String>,

    /// Workspace patterns, comma-separated globs.
    #[arg(long)]
    pub workspaces: Option<String>,

    /// View patterns, comma-separated globs.
    #[arg(long)]
    pub views: Option<String>,

    /// Bind the certificate to a server URL. Repeat for several.
    #[arg(long = "server-url")]
    pub server_urls: Vec<String>,

    /// Lifetime (`30d`, `12h`, `2w`).
    #[arg(long)]
    pub expires: Option<String>,

    /// Cap the number of changes the agent may create.
    #[arg(long)]
    pub max_changes: Option<u64>,

    /// Delegating identity. Defaults to your default identity.
    #[arg(short, long)]
    pub identity: Option<String>,

    /// Write the certificate here instead of storing it locally.
    ///
    /// The right choice when countersigning a request: the certificate belongs
    /// on the requesting machine, not this one.
    #[arg(short, long)]
    pub output: Option<PathBuf>,
}

impl Command for Delegate {
    fn run(&self) -> CliResult<()> {
        let store = IdentityStore::open_default().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to open identity store: {e}"))
        })?;

        let delegator = super::load_identity_or_default(&store, self.identity.as_deref())?;
        if delegator.identity_type.is_delegated() || delegator.identity_type.is_agent() {
            return Err(CliError::InvalidArgument {
                message: format!("'{}' is an agent and cannot delegate", delegator.name),
            });
        }

        // Resolve the delegate: either an identity in the local store, or the
        // subject of a countersigned request.
        let delegate = match (&self.request, &self.agent) {
            (Some(path), _) => self.delegate_from_request(path)?,
            (None, Some(name)) => store
                .load_by_name(name)
                .map_err(|_| CliError::IdentityNotFound(name.clone()))?,
            (None, None) => {
                return Err(CliError::InvalidArgument {
                    message: "name an agent identity, or pass --request <file> to countersign \
                              a request"
                        .to_string(),
                })
            }
        };

        let scope = self.build_scope()?;
        let expires = self
            .expires
            .as_deref()
            .map(parse_duration)
            .transpose()?
            .unwrap_or_else(|| chrono::Duration::days(DEFAULT_EXPIRY_DAYS));

        let mut terms = Delegation::new(&delegator, &delegate, scope).expires_in(expires);
        if let Some(agent_urn) = delegate
            .metadata
            .description
            .as_deref()
            .and_then(software_agent_from_description)
        {
            terms = terms.with_software_agent(agent_urn);
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

        match &self.output {
            Some(path) if path.as_os_str() == "-" => {
                println!("{document}");
                return Ok(());
            }
            Some(path) => {
                std::fs::write(path, &document)?;
                print_success(&format!("Wrote certificate to {}", path.display()));
                println!("  Delegation    {}", terms.id.to_urn());
                println!();
                print_hint(
                    "Install it on the agent's machine:  atomic identity delegation install <file>",
                );
            }
            None => {
                store
                    .save_delegation(&terms.id.to_base32(), &document)
                    .map_err(|e| {
                        CliError::Internal(anyhow::anyhow!("Failed to store certificate: {e}"))
                    })?;
                print_success(&format!("Delegated to {}", delegate.name));
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
                    println!("  Expires       {}", expiry.format("%Y-%m-%d"));
                }
                println!();
                print_hint("Enroll it with the server:  atomic identity delegation push");
            }
        }

        Ok(())
    }
}

impl Delegate {
    fn build_scope(&self) -> CliResult<DelegationScope> {
        let mut builder = DelegationScope::builder().permissions(parse_permissions(&self.can)?);
        for url in &self.server_urls {
            builder = builder.server(url.clone());
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

    /// Verify a self-signed request and turn it into a delegate identity.
    ///
    /// The verification is the point: it proves whoever produced the request
    /// holds the private half of the key it names. Without it, `--request`
    /// would be a way to talk someone into signing a certificate for a key
    /// chosen by an attacker.
    fn delegate_from_request(&self, path: &str) -> CliResult<Identity> {
        let raw = if path == "-" {
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            buf
        } else {
            std::fs::read_to_string(path)?
        };

        let value: serde_json::Value =
            serde_json::from_str(&raw).map_err(|e| CliError::InvalidArgument {
                message: format!("{path} is not valid JSON: {e}"),
            })?;

        let request = cert::verify_request(&value).map_err(|e| CliError::DelegationError {
            message: format!(
                "The request in {path} does not verify: {e}\n  \
                 Only countersign a request whose self-signature checks out — it is the \
                 only evidence the far end actually holds that key."
            ),
        })?;

        let public_key =
            atomic_identity::PublicKey::from_did_key(&request.delegate_key).map_err(|e| {
                CliError::DelegationError {
                    message: format!("Request carries a malformed key: {e}"),
                }
            })?;

        print_hint(&format!(
            "Countersigning a verified request from '{}' ({})",
            request.delegate_name, request.delegate
        ));

        let mut builder = Identity::builder(&request.delegate_name)
            .identity_type(IdentityType::Agent)
            .public_key(public_key);
        if let Some(agent) = &request.software_agent {
            builder = builder.description(format!("software-agent:{agent}"));
        }
        builder
            .build()
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to build delegate: {e}")))
    }
}

/// Recover the software-agent URN we stash in an identity description.
fn software_agent_from_description(description: &str) -> Option<String> {
    description
        .strip_prefix("software-agent:")
        .map(str::to_string)
        .or_else(|| {
            description
                .strip_prefix("agent-type:")
                .map(software_agent_urn)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn software_agent_is_recovered_from_the_description_marker() {
        assert_eq!(
            software_agent_from_description("software-agent:urn:atomic:agent:ci"),
            Some("urn:atomic:agent:ci".to_string())
        );
        assert_eq!(
            software_agent_from_description("agent-type:Claude-Code"),
            Some("urn:atomic:agent:claude-code".to_string())
        );
        assert_eq!(software_agent_from_description("just a description"), None);
    }
}
