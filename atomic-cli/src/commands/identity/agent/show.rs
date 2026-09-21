//! `atomic identity agent show` — one agent in full.

use clap::Parser;

use atomic_identity::IdentityStore;

use crate::commands::delegation::load_for_delegate;
use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_warning};

use super::humanize_remaining;

/// Show an agent identity and its delegations.
#[derive(Debug, Parser)]
pub struct Show {
    /// Agent identity name (e.g. `alice+claude`).
    #[arg(required = true)]
    pub name: String,

    /// Show every certificate, not only the one in force.
    #[arg(long)]
    pub history: bool,

    /// Print the signed certificate as JSON.
    #[arg(long)]
    pub certificate: bool,
}

impl Command for Show {
    fn run(&self) -> CliResult<()> {
        let store = IdentityStore::open_default().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to open identity store: {e}"))
        })?;

        let identity = store
            .load_by_name(&self.name)
            .map_err(|_| CliError::IdentityNotFound(self.name.clone()))?;

        if !identity.identity_type.is_delegated() && !identity.identity_type.is_agent() {
            return Err(CliError::InvalidArgument {
                message: format!(
                    "'{}' is not an agent identity.\n  Use 'atomic identity show {}' instead.",
                    self.name, self.name
                ),
            });
        }

        let delegations = load_for_delegate(&store, &identity)?;

        if self.certificate {
            let Some(newest) = delegations.first() else {
                return Err(CliError::DelegationError {
                    message: format!("No certificate stored for '{}'", self.name),
                });
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&newest.document).unwrap()
            );
            return Ok(());
        }

        println!("Agent: {}", identity.name);
        println!();
        println!("  DID           {}", identity.id.to_did());
        println!("  Key           {}", identity.public_key.to_did_key());
        if let Some(email) = &identity.email {
            println!("  Email         {email}");
        }
        println!(
            "  Type          {}",
            super::super::format_identity_type(&identity.identity_type)
        );

        if delegations.is_empty() {
            println!();
            print_warning("No delegation certificate — this key can prove who it is, but is authorized for nothing.");
            print_hint(&format!(
                "Issue one:  atomic identity delegate {}",
                identity.name
            ));
            return Ok(());
        }

        let shown = if self.history {
            &delegations[..]
        } else {
            &delegations[..1]
        };

        for resolved in shown {
            let d = &resolved.delegation;
            println!();
            println!("  Delegation    {}", d.id.to_urn());
            println!("    Status      {}", resolved.status);
            println!("    From        {}  ({})", d.delegator_name, d.delegator);
            if let Some(agent) = &d.software_agent {
                println!("    Software    {agent}");
            }
            println!(
                "    Can         {}",
                d.scope
                    .permissions
                    .iter()
                    .map(|p| p.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            println!("    Servers     {}", or_all(&d.scope.servers));
            println!("    Workspaces  {}", or_all(&d.scope.workspaces));
            println!("    Projects    {}", or_all(&d.scope.projects));
            println!("    Views       {}", or_all(&d.scope.views));
            if let Some(max) = d.scope.max_changes {
                println!("    Change cap  {max}");
            }
            println!("    Issued      {}", d.issued.format("%Y-%m-%d %H:%M UTC"));
            match d.expires {
                Some(expires) => println!(
                    "    Expires     {}  ({})",
                    expires.format("%Y-%m-%d %H:%M UTC"),
                    humanize_remaining(d.time_remaining())
                ),
                None => println!("    Expires     never"),
            }
        }

        if !self.history && delegations.len() > 1 {
            println!();
            print_hint(&format!(
                "{} older certificate(s) not shown — use --history",
                delegations.len() - 1
            ));
        }

        println!();
        print_hint(
            "Scope narrows; it never widens. This agent's effective access is whatever \
             the delegator has, intersected with the scope above.",
        );

        Ok(())
    }
}

/// An empty pattern list means unrestricted, which is worth saying explicitly
/// rather than rendering as a blank the reader has to interpret.
fn or_all(items: &[String]) -> String {
    if items.is_empty() {
        "(all)".to_string()
    } else {
        items.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_scope_dimensions_say_all() {
        assert_eq!(or_all(&[]), "(all)");
        assert_eq!(or_all(&["acme/api".to_string()]), "acme/api");
    }
}
