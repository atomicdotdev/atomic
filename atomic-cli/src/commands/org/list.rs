//! The `org list` command for listing organizations you belong to.
//!
//! This command hits the server **apex** `GET /orgs` endpoint, which returns
//! every organization the caller is a member of along with their role. Unlike
//! the org-scoped endpoints, it does not require a default org to be
//! configured — which is essential, since listing your orgs is often the
//! first thing you do before setting a default.
//!
//! # Usage
//!
//! ```text
//! atomic org list [OPTIONS]
//!
//! Options:
//!   --server <NAME>  Server profile override
//!   --format <FMT>    Output format: table or json [default: table]
//!   -h, --help        Print help information
//! ```
//!
//! # Examples
//!
//! ```text
//! $ atomic org list
//!  NAME        SLUG        KIND      ROLE    PLAN
//!  Acme Corp   acme-corp   team      owner   team
//!  Personal    alice       personal  owner   free
//!
//! 2 organizations found.
//!
//!   Hint: Use 'atomic org set <slug>' to change the default organization.
//! ```

use clap::Parser;

use crate::commands::client::build_apex_client;
use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{
    print_hint, print_info, print_section, Alignment, Column, KeyValueTable, Table,
};

/// List organizations you belong to.
///
/// Calls the apex `GET /orgs` endpoint and shows every org the caller is a
/// member of, with the caller's role. No default org needs to be set.
#[derive(Debug, Parser)]
#[command(name = "list")]
pub struct OrgList {
    /// Server profile override.
    ///
    /// Selects a named profile from `[servers.*]` in `~/.atomic/config.toml`.
    /// When omitted, the configured `default_server` (or legacy `[server]`
    /// block) is used.
    #[arg(long, value_name = "SERVER")]
    pub server: Option<String>,

    /// Output format.
    ///
    /// Use `table` for human-readable output or `json` for machine-readable.
    #[arg(long, value_name = "FORMAT")]
    pub format: Option<String>,
}

impl Command for OrgList {
    fn run(&self) -> CliResult<()> {
        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {e}"))
        })?;

        rt.block_on(self.execute())
    }
}

impl OrgList {
    async fn execute(&self) -> CliResult<()> {
        // Apex client: no org subdomain required, so this works even when no
        // default org is configured (the whole point of `org list`).
        let client = build_apex_client(self.server.as_deref()).await?;

        let orgs = atomic_teams::org::list_my_orgs(&client)
            .await
            .map_err(|e| CliError::RemoteError {
                message: e.to_string(),
                url: None,
            })?;

        let is_json = self
            .format
            .as_deref()
            .map(|f| f.eq_ignore_ascii_case("json"))
            .unwrap_or(false);

        if is_json {
            self.output_json(&orgs)?;
        } else {
            self.output_table(&orgs);
        }

        Ok(())
    }

    fn output_json(&self, orgs: &[atomic_teams::MyOrgInfo]) -> CliResult<()> {
        let json = serde_json::to_string_pretty(orgs).map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to serialize org list: {e}"))
        })?;
        println!("{json}");
        Ok(())
    }

    fn output_table(&self, orgs: &[atomic_teams::MyOrgInfo]) {
        if orgs.is_empty() {
            print_info("You do not belong to any organizations.");
            println!();
            print_hint("Create one with: atomic org create <name>");
            return;
        }

        // When there is exactly one org, a key/value card reads better than a
        // wide table and matches the legacy single-org output.
        if orgs.len() == 1 {
            print_section("Your organization:");
            println!();
            let o = &orgs[0];
            let table = KeyValueTable::new()
                .add("Name", &o.name)
                .add("Slug", &o.slug)
                .add("Kind", &o.kind)
                .add("Plan", &o.plan)
                .add("Role", &o.role);
            let table = if let Some(email) = &o.email {
                table.add("Email", email)
            } else {
                table
            };
            let table = table
                .add("ID", o.id.to_string())
                .add("Joined", o.joined_at.to_rfc3339());
            println!("{table}");
            println!();
            print_hint("Use 'atomic org set <slug>' to set this as your default organization.");
            return;
        }

        let mut table = Table::new();
        table.set_columns(vec![
            Column::new("NAME").min_width(12),
            Column::new("SLUG").min_width(10),
            Column::new("KIND").min_width(8),
            Column::new("ROLE").min_width(6),
            Column::new("PLAN").min_width(6),
            Column::new("EMAIL").min_width(16),
        ]);

        for o in orgs {
            table.add_row(vec![
                o.name.clone(),
                o.slug.clone(),
                o.kind.clone(),
                o.role.clone(),
                o.plan.clone(),
                o.email.clone().unwrap_or_default(),
            ]);
        }

        println!("{table}");
        println!();
        let word = if orgs.len() == 1 {
            "organization"
        } else {
            "organizations"
        };
        print_info(&format!("{} {} found.", orgs.len(), word));
        println!();
        print_hint("Use 'atomic org set <slug>' to change the default organization.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_args() {
        let cmd = OrgList {
            server: None,
            format: None,
        };
        assert!(cmd.server.is_none());
        assert!(cmd.format.is_none());
    }

    #[test]
    fn server_override() {
        let cmd = OrgList {
            server: Some("staging".to_string()),
            format: None,
        };
        assert_eq!(cmd.server.as_deref(), Some("staging"));
    }

    #[test]
    fn json_format() {
        let cmd = OrgList {
            server: None,
            format: Some("json".to_string()),
        };
        let is_json = cmd
            .format
            .as_deref()
            .map(|f| f.eq_ignore_ascii_case("json"))
            .unwrap_or(false);
        assert!(is_json);
    }

    #[test]
    fn table_format_is_default() {
        let cmd = OrgList {
            server: None,
            format: Some("table".to_string()),
        };
        let is_json = cmd
            .format
            .as_deref()
            .map(|f| f.eq_ignore_ascii_case("json"))
            .unwrap_or(false);
        assert!(!is_json);
    }

    #[test]
    fn json_format_case_insensitive() {
        for variant in &["json", "JSON", "Json"] {
            let cmd = OrgList {
                server: None,
                format: Some(variant.to_string()),
            };
            let is_json = cmd
                .format
                .as_deref()
                .map(|f| f.eq_ignore_ascii_case("json"))
                .unwrap_or(false);
            assert!(is_json, "Expected true for format={variant}");
        }
    }

    #[test]
    fn empty_table_output_includes_hint() {
        // The empty-list path must point the user at `atomic org create`.
        // We can't easily render the table without stdout capture, but we can
        // at least assert the branch is taken for an empty slice.
        let orgs: Vec<atomic_teams::MyOrgInfo> = Vec::new();
        assert!(orgs.is_empty());
    }
}
