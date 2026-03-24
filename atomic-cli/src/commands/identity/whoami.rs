//! The `identity whoami` command for showing the current identity.
//!
//! This module implements the `atomic identity whoami` command, which displays
//! information about the identity that will be used for signing changes.
//!
//! # Usage
//!
//! ```text
//! atomic identity whoami [OPTIONS]
//!
//! Options:
//!   -u, --usage <USAGE>    Show identity for specific usage context
//!   -f, --format <FORMAT>  Output format (default, json) [default: default]
//!   -h, --help             Print help information
//! ```
//!
//! # Examples
//!
//! Show current identity:
//! ```text
//! $ atomic identity whoami
//! Current identity: alice
//!   Email:       alice@example.com
//!   Type:        user
//!   Usage:       personal
//!   Public Key:  ABCD1234...
//! ```
//!
//! Show identity for work context:
//! ```text
//! $ atomic identity whoami --usage work
//! Current identity for work: alice-work
//!   Email:       alice@company.com
//!   Type:        user
//!   Usage:       work
//!   Public Key:  EFGH5678...
//! ```

use clap::Parser;

use atomic_identity::{IdentityStore, IdentityUsage};

use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_info, print_section, print_warning};

// Command Definition

/// Show the current default identity.
///
/// Displays information about the identity that will be used when
/// recording changes. You can also check the default identity for
/// a specific usage context.
#[derive(Debug, Parser)]
pub struct WhoAmI {
    /// Show identity for a specific usage context.
    ///
    /// If specified, shows the default identity for that usage
    /// (personal, work, community, bot) instead of the global default.
    #[arg(short, long)]
    pub usage: Option<String>,

    /// Output format.
    ///
    /// - default: Human-readable output
    /// - json: JSON output for scripting
    #[arg(short, long, default_value = "default")]
    pub format: String,
}

impl Command for WhoAmI {
    fn run(&self) -> CliResult<()> {
        // Open the identity store
        let store = IdentityStore::open_default().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to open identity store: {}", e))
        })?;

        // Get the appropriate default identity
        let (identity, context) = if let Some(usage_str) = &self.usage {
            let usage = IdentityUsage::parse(usage_str);
            let identity = store.get_default_for_usage(&usage).map_err(|e| {
                CliError::Internal(anyhow::anyhow!("Failed to get default identity: {}", e))
            })?;
            (identity, Some(usage))
        } else {
            let identity = store.get_default().map_err(|e| {
                CliError::Internal(anyhow::anyhow!("Failed to get default identity: {}", e))
            })?;
            (identity, None)
        };

        // Handle no identity case
        let Some(identity) = identity else {
            if let Some(usage) = &context {
                print_warning(&format!("No default identity set for {} usage", usage));
            } else {
                print_warning("No default identity set");
            }
            println!();
            print_hint("Create a new identity with: atomic identity new <name> --email <email>");
            print_hint("Set a default with: atomic identity default <name>");
            return Ok(());
        };

        // Output based on format
        match self.format.to_lowercase().as_str() {
            "json" => self.output_json(&identity, context.as_ref()),
            _ => self.output_default(&identity, context.as_ref()),
        }

        Ok(())
    }
}

impl WhoAmI {
    /// Output identity in human-readable format.
    fn output_default(
        &self,
        identity: &atomic_identity::Identity,
        context: Option<&IdentityUsage>,
    ) {
        if let Some(usage) = context {
            print_section(&format!(
                "Current identity for {}: {}",
                usage, identity.name
            ));
        } else {
            print_section(&format!("Current identity: {}", identity.name));
        }
        println!();

        if let Some(email) = &identity.email {
            println!("  Email:       {}", email);
        }

        println!(
            "  Type:        {}",
            super::format_identity_type(&identity.identity_type)
        );
        println!("  Usage:       {}", super::format_usage(&identity.usage));
        println!("  Public Key:  {}...", &identity.public_key_base32()[..24]);

        if let Some(description) = &identity.metadata.description {
            println!("  Description: {}", description);
        }

        // Show delegation info if relevant
        if identity.identity_type.is_delegated() {
            if let Some(delegator) = &identity.delegated_by {
                println!();
                print_info(&format!("Acting on behalf of: {}", delegator.short()));
            }
        }
    }

    /// Output identity as JSON.
    fn output_json(&self, identity: &atomic_identity::Identity, context: Option<&IdentityUsage>) {
        use serde_json::json;

        let mut obj = json!({
            "name": identity.name,
            "id": identity.id.to_base32(),
            "type": super::format_identity_type(&identity.identity_type),
            "usage": super::format_usage(&identity.usage),
            "public_key": identity.public_key_base32(),
        });

        if let Some(email) = &identity.email {
            obj["email"] = json!(email);
        }

        if let Some(description) = &identity.metadata.description {
            obj["description"] = json!(description);
        }

        if let Some(delegator) = &identity.delegated_by {
            obj["delegated_by"] = json!(delegator.to_base32());
        }

        if let Some(usage) = context {
            obj["context"] = json!(super::format_usage(usage));
        }

        println!("{}", serde_json::to_string_pretty(&obj).unwrap());
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_whoami_command_fields() {
        let cmd = WhoAmI {
            usage: None,
            format: "default".to_string(),
        };

        assert!(cmd.usage.is_none());
        assert_eq!(cmd.format, "default");
    }

    #[test]
    fn test_whoami_command_with_usage() {
        let cmd = WhoAmI {
            usage: Some("work".to_string()),
            format: "default".to_string(),
        };

        assert_eq!(cmd.usage, Some("work".to_string()));
    }

    #[test]
    fn test_whoami_command_json_format() {
        let cmd = WhoAmI {
            usage: None,
            format: "json".to_string(),
        };

        assert_eq!(cmd.format, "json");
    }

    #[test]
    fn test_whoami_command_with_usage_and_format() {
        let cmd = WhoAmI {
            usage: Some("personal".to_string()),
            format: "json".to_string(),
        };

        assert_eq!(cmd.usage, Some("personal".to_string()));
        assert_eq!(cmd.format, "json");
    }

    #[test]
    fn test_default_format() {
        let cmd = WhoAmI {
            usage: None,
            format: "default".to_string(),
        };

        assert_eq!(cmd.format, "default");
    }
}
