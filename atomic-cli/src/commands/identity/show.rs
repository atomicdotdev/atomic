//! The `identity show` command for displaying identity details.
//!
//! This module implements the `atomic identity show` command, which displays
//! detailed information about a specific identity.
//!
//! # Usage
//!
//! ```text
//! atomic identity show <NAME> [OPTIONS]
//!
//! Arguments:
//!   <NAME>  Name of the identity to show
//!
//! Options:
//!   -f, --format <FORMAT>  Output format (default, json) [default: default]
//!       --show-public-key  Show the full public key
//!   -h, --help             Print help information
//! ```
//!
//! # Examples
//!
//! Show identity details:
//! ```text
//! $ atomic identity show alice
//! Identity: alice
//!   ID:          ABCD1234EFGH5678
//!   Email:       alice@example.com
//!   Type:        user
//!   Usage:       personal
//!   Created:     2024-01-15 10:30:00 UTC
//!   Public Key:  IJKL9012... (use --show-public-key for full key)
//! ```

use clap::Parser;

use atomic_identity::IdentityStore;

use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_info, print_section};

// Command Definition

/// Show identity details.
///
/// Displays detailed information about a specific identity, including
/// the public key, metadata, and configuration.
#[derive(Debug, Parser)]
pub struct Show {
    /// Name of the identity to show.
    #[arg(required = true)]
    pub name: String,

    /// Output format.
    ///
    /// - default: Human-readable output
    /// - json: JSON output for scripting
    #[arg(short, long, default_value = "default")]
    pub format: String,

    /// Show the full public key.
    ///
    /// By default, the public key is truncated for readability.
    #[arg(long)]
    pub show_public_key: bool,
}

impl Command for Show {
    fn run(&self) -> CliResult<()> {
        // Open the identity store
        let store = IdentityStore::open_default().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to open identity store: {}", e))
        })?;

        // Load the identity by name
        let identity = store.load_by_name(&self.name).map_err(|_| {
            CliError::IdentityNotFound(self.name.clone())
        })?;

        // Check if this is the default identity
        let is_default = store
            .get_default()
            .ok()
            .flatten()
            .map(|d| d.id == identity.id)
            .unwrap_or(false);

        // Output based on format
        match self.format.to_lowercase().as_str() {
            "json" => self.output_json(&identity, is_default),
            _ => self.output_default(&identity, is_default),
        }

        Ok(())
    }
}

impl Show {
    /// Output identity details in human-readable format.
    fn output_default(&self, identity: &atomic_identity::Identity, is_default: bool) {
        print_section(&format!("Identity: {}", identity.name));
        println!();

        // Basic info
        println!("  ID:          {}", identity.id.to_base32());

        if let Some(email) = &identity.email {
            println!("  Email:       {}", email);
        }

        println!("  Type:        {}", super::format_identity_type(&identity.identity_type));
        println!("  Usage:       {}", super::format_usage(&identity.usage));

        // Metadata
        println!("  Created:     {}", identity.metadata.created_at.format("%Y-%m-%d %H:%M:%S UTC"));

        if let Some(modified) = identity.metadata.modified_at {
            println!("  Modified:    {}", modified.format("%Y-%m-%d %H:%M:%S UTC"));
        }

        if let Some(expires) = identity.metadata.expires_at {
            let expired = identity.is_expired();
            println!("  Expires:     {} {}",
                expires.format("%Y-%m-%d %H:%M:%S UTC"),
                if expired { "(EXPIRED)" } else { "" }
            );
        }

        if let Some(description) = &identity.metadata.description {
            println!("  Description: {}", description);
        }

        // Delegation info
        if let Some(delegated_by) = &identity.delegated_by {
            println!("  Delegated by: {}", delegated_by.short());
        }

        // Public key
        println!();
        if self.show_public_key {
            println!("  Public Key:  {}", identity.public_key_base32());
        } else {
            println!("  Public Key:  {}... (use --show-public-key for full key)",
                &identity.public_key_base32()[..24]);
        }

        // Default status
        if is_default {
            println!();
            print_info("This is your default identity");
        }
    }

    /// Output identity details as JSON.
    fn output_json(&self, identity: &atomic_identity::Identity, is_default: bool) {
        use serde_json::json;

        let mut obj = json!({
            "name": identity.name,
            "id": identity.id.to_base32(),
            "type": super::format_identity_type(&identity.identity_type),
            "usage": super::format_usage(&identity.usage),
            "public_key": identity.public_key_base32(),
            "is_default": is_default,
            "created_at": identity.metadata.created_at.to_rfc3339(),
            "is_expired": identity.is_expired(),
        });

        if let Some(email) = &identity.email {
            obj["email"] = json!(email);
        }

        if let Some(modified) = identity.metadata.modified_at {
            obj["modified_at"] = json!(modified.to_rfc3339());
        }

        if let Some(expires) = identity.metadata.expires_at {
            obj["expires_at"] = json!(expires.to_rfc3339());
        }

        if let Some(description) = &identity.metadata.description {
            obj["description"] = json!(description);
        }

        if let Some(delegated_by) = &identity.delegated_by {
            obj["delegated_by"] = json!(delegated_by.to_base32());
        }

        println!("{}", serde_json::to_string_pretty(&obj).unwrap());
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_show_command_fields() {
        let cmd = Show {
            name: "alice".to_string(),
            format: "default".to_string(),
            show_public_key: false,
        };

        assert_eq!(cmd.name, "alice");
        assert_eq!(cmd.format, "default");
        assert!(!cmd.show_public_key);
    }

    #[test]
    fn test_show_command_with_json_format() {
        let cmd = Show {
            name: "alice".to_string(),
            format: "json".to_string(),
            show_public_key: true,
        };

        assert_eq!(cmd.name, "alice");
        assert_eq!(cmd.format, "json");
        assert!(cmd.show_public_key);
    }

    #[test]
    fn test_default_format() {
        let cmd = Show {
            name: "test".to_string(),
            format: "default".to_string(),
            show_public_key: false,
        };

        assert_eq!(cmd.format, "default");
    }

    #[test]
    fn test_show_public_key_default() {
        let cmd = Show {
            name: "test".to_string(),
            format: "default".to_string(),
            show_public_key: false,
        };

        assert!(!cmd.show_public_key);
    }
}
