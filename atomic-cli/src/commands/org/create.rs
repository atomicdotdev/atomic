//! The `org create` command for creating a new team organization.
//!
//! This module implements the `atomic org create` command, which creates
//! a new organization on the remote atomic-storage server.
//!
//! # Usage
//!
//! ```text
//! atomic org create [OPTIONS] <NAME>
//!
//! Arguments:
//!   <NAME>  Display name for the new organization
//!
//! Options:
//!   --email <EMAIL>  Contact email for the organization
//!   -h, --help       Print help information
//! ```
//!
//! # Examples
//!
//! ```text
//! # Create a basic organization
//! $ atomic org create "Acme Corp"
//! ✓ Created organization: acme-corp
//!
//!   Name:  Acme Corp
//!   Slug:  acme-corp
//!   Plan:  free
//!
//! Next steps:
//!   atomic org set acme-corp    Set as your default org
//!   atomic org member add <id>     Add members to the org
//!   atomic team create <name>      Create a team
//!
//! # Create with a contact email
//! $ atomic org create "Acme Corp" --email admin@acme.com
//! ```

use clap::Parser;

use crate::commands::client::build_client;
use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_next_steps, print_success, KeyValueTable};

/// Create a new team organization.
///
/// Creates a new organization on the remote server. The URL-safe slug is
/// derived server-side from the display name. You must be authenticated
/// with a registered identity.
#[derive(Debug, Parser)]
#[command(name = "create")]
pub struct OrgCreate {
    /// Display name for the organization.
    ///
    /// The server derives a URL-safe slug from this name (e.g.
    /// "Acme Corp" → "acme-corp"). The name can contain spaces
    /// and special characters.
    #[arg(required = true, value_name = "NAME")]
    pub name: String,

    /// Contact email for the organization.
    ///
    /// Optional email address displayed on the organization profile
    /// and used for administrative notifications.
    #[arg(long, value_name = "EMAIL")]
    pub email: Option<String>,
}

impl Command for OrgCreate {
    fn run(&self) -> CliResult<()> {
        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {e}"))
        })?;

        rt.block_on(self.execute())
    }
}

impl OrgCreate {
    async fn execute(&self) -> CliResult<()> {
        // Build client using current default org (doesn't matter which org
        // we're scoped to — the server creates a new org regardless).
        let client = build_client(None)?;

        let info = atomic_teams::org::create_org(&client, &self.name, self.email.as_deref())
            .await
            .map_err(|e| CliError::RemoteError {
                message: e.to_string(),
                url: None,
            })?;

        print_success(&format!("Created organization: {}", info.slug));
        println!();

        let table = KeyValueTable::new()
            .add("Name", &info.name)
            .add("Slug", &info.slug)
            .add("Kind", &info.kind)
            .add("Plan", &info.plan);
        let table = if let Some(email) = &info.email {
            table.add("Email", email)
        } else {
            table
        };
        let table = table.add("ID", info.id.to_string());

        println!("{table}");

        print_next_steps(&[
            (
                &format!("atomic org set {}", info.slug),
                "Set as your default org",
            ),
            ("atomic org member add <id>", "Add members to the org"),
            ("atomic team create <name>", "Create a team"),
        ]);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_name() {
        let cmd = OrgCreate {
            name: "Acme Corp".to_string(),
            email: None,
        };
        assert_eq!(cmd.name, "Acme Corp");
        assert!(cmd.email.is_none());
    }

    #[test]
    fn with_email() {
        let cmd = OrgCreate {
            name: "Acme Corp".to_string(),
            email: Some("admin@acme.com".to_string()),
        };
        assert_eq!(cmd.name, "Acme Corp");
        assert_eq!(cmd.email.as_deref(), Some("admin@acme.com"));
    }

    #[test]
    fn name_with_special_characters() {
        let cmd = OrgCreate {
            name: "Acme & Sons (LLC)".to_string(),
            email: None,
        };
        assert_eq!(cmd.name, "Acme & Sons (LLC)");
    }
}
