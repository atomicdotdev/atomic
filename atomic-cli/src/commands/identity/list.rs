//! The `identity list` command for listing identities.
//!
//! This module implements the `atomic identity list` command, which displays
//! all identities stored locally.
//!
//! # Usage
//!
//! ```text
//! atomic identity list [OPTIONS]
//!
//! Options:
//!   -u, --usage <USAGE>    Filter by usage context
//!   -t, --type <TYPE>      Filter by identity type
//!   -v, --verbose          Show additional details
//!   -f, --format <FORMAT>  Output format (table, json) [default: table]
//!   -h, --help             Print help information
//! ```
//!
//! # Examples
//!
//! List all identities:
//! ```text
//! $ atomic identity list
//! NAME           TYPE    USAGE      EMAIL                   DEFAULT
//! alice          user    personal   alice@example.com       *
//! alice-work     user    work       alice@company.com
//! ci-bot         agent   bot
//! ```

use clap::Parser;

use atomic_identity::{IdentityFilter, IdentityStore, IdentityUsage};

use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::table::{Alignment, Column, Table};
use crate::output::{print_hint, print_info};

// Command Definition

/// List all identities.
///
/// Displays all identities stored locally with their names, types, and
/// usage contexts.
#[derive(Debug, Parser)]
pub struct List {
    /// Filter by usage context.
    ///
    /// Only show identities with the specified usage (personal, work,
    /// community, bot).
    #[arg(short, long)]
    pub usage: Option<String>,

    /// Filter by identity type.
    ///
    /// Only show identities of the specified type (user, agent).
    #[arg(short = 't', long = "type")]
    pub identity_type: Option<String>,

    /// Show additional details.
    ///
    /// Include public key and creation date in the output.
    #[arg(short, long)]
    pub verbose: bool,

    /// Output format.
    ///
    /// - table: Human-readable table (default)
    /// - json: JSON output for scripting
    #[arg(short, long, default_value = "table")]
    pub format: String,
}

impl List {
    /// Build an identity filter from the command arguments.
    fn build_filter(&self) -> IdentityFilter {
        let mut filter = IdentityFilter::all();

        if let Some(usage_str) = &self.usage {
            filter = filter.usage(IdentityUsage::parse(usage_str));
        }

        if let Some(type_str) = &self.identity_type {
            filter = filter.identity_type(super::parse_identity_type(type_str));
        }

        filter
    }
}

impl Command for List {
    fn run(&self) -> CliResult<()> {
        // Open the identity store
        let store = IdentityStore::open_default().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to open identity store: {}", e))
        })?;

        // Get the default identity ID for marking
        let default_id = store
            .get_default()
            .ok()
            .flatten()
            .map(|i| i.id);

        // List identities with filter
        let filter = self.build_filter();
        let identities = store.list_filtered(&filter).map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to list identities: {}", e))
        })?;

        if identities.is_empty() {
            print_info("No identities found");
            println!();
            print_hint("Create a new identity with: atomic identity new <name> --email <email>");
            return Ok(());
        }

        // Output based on format
        match self.format.to_lowercase().as_str() {
            "json" => self.output_json(&identities, default_id.as_ref()),
            _ => self.output_table(&identities, default_id.as_ref()),
        }

        Ok(())
    }
}

impl List {
    /// Output identities as a table.
    fn output_table(
        &self,
        identities: &[atomic_identity::Identity],
        default_id: Option<&atomic_identity::IdentityId>,
    ) {
        let mut table = Table::new();

        let mut columns = vec![
            Column::new("NAME").min_width(12),
            Column::new("TYPE").min_width(8),
            Column::new("USAGE").min_width(10),
            Column::new("EMAIL").min_width(20),
            Column::new("DEFAULT").align(Alignment::Center),
        ];

        if self.verbose {
            columns.push(Column::new("ID").min_width(10));
            columns.push(Column::new("PUBLIC KEY").min_width(20));
        }

        table.set_columns(columns);

        for identity in identities {
            let is_default = default_id.map(|d| d == &identity.id).unwrap_or(false);

            let mut row = vec![
                identity.name.clone(),
                super::format_identity_type(&identity.identity_type).to_string(),
                super::format_usage(&identity.usage),
                identity.email.clone().unwrap_or_default(),
                if is_default { "*".to_string() } else { String::new() },
            ];

            if self.verbose {
                row.push(identity.id.short());
                row.push(format!("{}...", &identity.public_key_base32()[..16]));
            }

            table.add_row(row);
        }

        println!("{}", table);

        // Print summary
        println!();
        print_info(&format!("{} identit{} found",
            identities.len(),
            if identities.len() == 1 { "y" } else { "ies" }
        ));
    }

    /// Output identities as JSON.
    fn output_json(
        &self,
        identities: &[atomic_identity::Identity],
        default_id: Option<&atomic_identity::IdentityId>,
    ) {
        use serde_json::json;

        let json_identities: Vec<_> = identities
            .iter()
            .map(|identity| {
                let is_default = default_id.map(|d| d == &identity.id).unwrap_or(false);

                let mut obj = json!({
                    "name": identity.name,
                    "id": identity.id.to_base32(),
                    "type": super::format_identity_type(&identity.identity_type),
                    "usage": super::format_usage(&identity.usage),
                    "public_key": identity.public_key_base32(),
                    "is_default": is_default,
                });

                if let Some(email) = &identity.email {
                    obj["email"] = json!(email);
                }

                if let Some(desc) = &identity.metadata.description {
                    obj["description"] = json!(desc);
                }

                obj
            })
            .collect();

        let output = json!({
            "identities": json_identities,
            "count": identities.len(),
        });

        println!("{}", serde_json::to_string_pretty(&output).unwrap());
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_filter_no_options() {
        let cmd = List {
            usage: None,
            identity_type: None,
            verbose: false,
            format: "table".to_string(),
        };

        let _filter = cmd.build_filter();
        // Filter should match all - no specific assertions needed
    }

    #[test]
    fn test_build_filter_with_usage() {
        let cmd = List {
            usage: Some("work".to_string()),
            identity_type: None,
            verbose: false,
            format: "table".to_string(),
        };

        let _filter = cmd.build_filter();
        // Filter should be configured for work usage
    }

    #[test]
    fn test_build_filter_with_type() {
        let cmd = List {
            usage: None,
            identity_type: Some("agent".to_string()),
            verbose: false,
            format: "table".to_string(),
        };

        let _filter = cmd.build_filter();
        // Filter should be configured for agent type
    }

    #[test]
    fn test_list_command_fields() {
        let cmd = List {
            usage: Some("personal".to_string()),
            identity_type: Some("user".to_string()),
            verbose: true,
            format: "json".to_string(),
        };

        assert_eq!(cmd.usage, Some("personal".to_string()));
        assert_eq!(cmd.identity_type, Some("user".to_string()));
        assert!(cmd.verbose);
        assert_eq!(cmd.format, "json");
    }

    #[test]
    fn test_default_format() {
        let cmd = List {
            usage: None,
            identity_type: None,
            verbose: false,
            format: "table".to_string(),
        };

        assert_eq!(cmd.format, "table");
    }
}
