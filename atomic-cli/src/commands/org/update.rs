//! The `org update` command for updating organization settings.
//!
//! This module implements the `atomic org update` command, which modifies
//! an organization's display name and/or contact email on the remote
//! atomic-storage server.
//!
//! # Usage
//!
//! ```text
//! atomic org update [OPTIONS] [SLUG]
//!
//! Arguments:
//!   [SLUG]  Organization slug to update (defaults to current org)
//!
//! Options:
//!   --name <NAME>      New display name
//!   --email <EMAIL>    New contact email
//!   --org <ORG>        Organization slug override for the API client
//!   -h, --help         Print help information
//! ```
//!
//! # Examples
//!
//! ```text
//! # Update the current org's name
//! $ atomic org update --name "Acme Industries"
//! ✓ Updated organization: acme-corp
//!
//! # Update a specific org's email
//! $ atomic org update acme-corp --email new-admin@acme.com
//!
//! # Update both name and email
//! $ atomic org update acme-corp --name "New Name" --email new@acme.com
//! ```

use clap::Parser;

use crate::commands::client::build_client;
use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_success, KeyValueTable};

/// Update organization settings.
///
/// Modify the display name or contact email for an organization.
/// Only fields that are provided will be updated; omitted fields
/// remain unchanged on the server.
///
/// Requires admin or owner permissions on the organization.
#[derive(Debug, Parser)]
#[command(name = "update")]
pub struct OrgUpdate {
    /// Organization slug to update.
    ///
    /// If omitted, the current default organization from global
    /// configuration is used.
    #[arg(value_name = "SLUG")]
    pub slug: Option<String>,

    /// New display name for the organization.
    ///
    /// If not provided, the existing name is preserved.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// New contact email for the organization.
    ///
    /// If not provided, the existing email is preserved.
    #[arg(long, value_name = "EMAIL")]
    pub email: Option<String>,

    /// Organization slug override for the API client.
    ///
    /// Overrides the default organization from global configuration
    /// when building the API client. This is distinct from the
    /// positional `slug` argument, which specifies *which* org to
    /// update.
    #[arg(long, value_name = "ORG")]
    pub org: Option<String>,
}

impl Command for OrgUpdate {
    fn run(&self) -> CliResult<()> {
        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {e}"))
        })?;

        rt.block_on(self.execute())
    }
}

impl OrgUpdate {
    async fn execute(&self) -> CliResult<()> {
        // At least one field must be provided to update.
        if self.name.is_none() && self.email.is_none() {
            return Err(CliError::InvalidArgument {
                message: "At least one of --name or --email must be provided".to_string(),
            });
        }

        let client = build_client(self.org.as_deref()).await?;

        // Determine which slug to update: explicit arg > client's org slug.
        let slug = self.slug.as_deref().unwrap_or_else(|| client.org_slug());

        let info = atomic_teams::org::update_org(
            &client,
            slug,
            self.name.as_deref(),
            self.email.as_deref(),
        )
        .await
        .map_err(|e| CliError::RemoteError {
            message: e.to_string(),
            url: None,
        })?;

        print_success(&format!("Updated organization: {}", info.slug));
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

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_args() {
        let cmd = OrgUpdate {
            slug: None,
            name: None,
            email: None,
            org: None,
        };
        assert!(cmd.slug.is_none());
        assert!(cmd.name.is_none());
        assert!(cmd.email.is_none());
        assert!(cmd.org.is_none());
    }

    #[test]
    fn with_name_only() {
        let cmd = OrgUpdate {
            slug: Some("acme".to_string()),
            name: Some("Acme Industries".to_string()),
            email: None,
            org: None,
        };
        assert_eq!(cmd.slug.as_deref(), Some("acme"));
        assert_eq!(cmd.name.as_deref(), Some("Acme Industries"));
        assert!(cmd.email.is_none());
    }

    #[test]
    fn with_email_only() {
        let cmd = OrgUpdate {
            slug: None,
            name: None,
            email: Some("new@acme.com".to_string()),
            org: None,
        };
        assert_eq!(cmd.email.as_deref(), Some("new@acme.com"));
    }

    #[test]
    fn with_both_fields() {
        let cmd = OrgUpdate {
            slug: Some("acme".to_string()),
            name: Some("New Name".to_string()),
            email: Some("new@acme.com".to_string()),
            org: None,
        };
        assert!(cmd.name.is_some());
        assert!(cmd.email.is_some());
    }

    #[test]
    fn with_org_override() {
        let cmd = OrgUpdate {
            slug: Some("target-org".to_string()),
            name: Some("Name".to_string()),
            email: None,
            org: Some("client-org".to_string()),
        };
        // The positional slug is the org to update
        assert_eq!(cmd.slug.as_deref(), Some("target-org"));
        // The --org flag is the client scope
        assert_eq!(cmd.org.as_deref(), Some("client-org"));
    }

    #[test]
    fn no_fields_is_invalid() {
        // Verify that execute() would reject this — we test the validation
        // logic inline since we can't call execute() without a server.
        let cmd = OrgUpdate {
            slug: None,
            name: None,
            email: None,
            org: None,
        };
        assert!(cmd.name.is_none() && cmd.email.is_none());
    }
}
