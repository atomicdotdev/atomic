//! The `identity new` command for creating new identities.
//!
//! This module implements the `atomic identity new` command, which generates
//! a new Ed25519 keypair and stores the identity locally.
//!
//! # Usage
//!
//! ```text
//! atomic identity new <NAME> [OPTIONS]
//!
//! Arguments:
//!   <NAME>  Name for the new identity
//!
//! Options:
//!   -e, --email <EMAIL>    Email address for the identity
//!   -t, --type <TYPE>      Identity type (user, agent) [default: user]
//!   -u, --usage <USAGE>    Usage context (personal, work, community, bot) [default: personal]
//!   -d, --description <DESC>  Description of the identity
//!       --set-default      Set this identity as the default
//!   -h, --help             Print help information
//! ```
//!
//! # Examples
//!
//! Create a personal identity:
//! ```text
//! $ atomic identity new alice --email alice@example.com
//! Created identity: alice
//!   ID:         ABCD1234...
//!   Email:      alice@example.com
//!   Type:       user
//!   Usage:      personal
//!   Public Key: EFGH5678...
//!
//! Next steps:
//!   atomic identity default alice    Set as default identity
//!   atomic identity show alice       View identity details
//! ```

use clap::Parser;

use atomic_identity::{Identity, IdentityStore, IdentityType, IdentityUsage, KeyPair};

use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_success};

// Command Definition

/// Create a new identity.
///
/// Generates a new Ed25519 keypair and stores the identity locally.
/// The identity can be used to sign changes and prove authorship.
#[derive(Debug, Parser)]
pub struct New {
    /// Name for the new identity.
    ///
    /// This is the human-readable name used to identify this identity.
    /// It should be unique within your identity store.
    #[arg(required = true)]
    pub name: String,

    /// Email address for the identity.
    ///
    /// Optional but recommended. This will be included in change headers.
    #[arg(short, long)]
    pub email: Option<String>,

    /// Identity type.
    ///
    /// - user: A human user (default)
    /// - agent: An AI or automated system
    #[arg(short = 't', long = "type", default_value = "user")]
    pub identity_type: String,

    /// Usage context for this identity.
    ///
    /// - personal: Side projects, personal work (default)
    /// - work: Professional/employer-related work
    /// - community: Open source, organization work
    /// - bot: Automated systems
    #[arg(short, long, default_value = "personal")]
    pub usage: String,

    /// Description of the identity.
    ///
    /// A human-readable description of what this identity is for.
    #[arg(short, long)]
    pub description: Option<String>,

    /// Set this identity as the default.
    ///
    /// If set, this identity will be used by default when recording changes.
    #[arg(long)]
    pub set_default: bool,

    /// Set this identity as the default for its usage context.
    ///
    /// If set, this identity will be the default for its usage type
    /// (e.g., default for "work" usage).
    #[arg(long)]
    pub set_default_for_usage: bool,
}

impl New {
    /// Parse the identity type from the string argument.
    fn parse_identity_type(&self) -> IdentityType {
        super::parse_identity_type(&self.identity_type)
    }

    /// Parse the usage context from the string argument.
    fn parse_usage(&self) -> IdentityUsage {
        IdentityUsage::parse(&self.usage)
    }
}

impl Command for New {
    fn run(&self) -> CliResult<()> {
        // Open or create the identity store
        let mut store = IdentityStore::open_default().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to open identity store: {}", e))
        })?;

        // Check if an identity with this name already exists
        if store.exists_by_name(&self.name) {
            return Err(CliError::IdentityAlreadyExists(self.name.clone()));
        }

        // Generate a new keypair
        let keypair = KeyPair::generate();

        // Build the identity
        let mut builder = Identity::builder(&self.name)
            .identity_type(self.parse_identity_type())
            .usage(self.parse_usage());

        if let Some(email) = &self.email {
            builder = builder.email(email);
        }

        if let Some(description) = &self.description {
            builder = builder.description(description);
        }

        // Use the keypair's public key for the identity
        let identity = builder
            .public_key(keypair.public.clone())
            .build()
            .map_err(|e| {
                CliError::Internal(anyhow::anyhow!("Failed to create identity: {}", e))
            })?;

        // Save the identity with its keypair
        store.save_with_keypair(&identity, &keypair, None).map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to save identity: {}", e))
        })?;

        // Set as default if requested
        if self.set_default {
            store.set_default(&identity.id).map_err(|e| {
                CliError::Internal(anyhow::anyhow!("Failed to set as default: {}", e))
            })?;
        }

        if self.set_default_for_usage {
            store.set_default_for_usage(&identity.usage, &identity.id).map_err(|e| {
                CliError::Internal(anyhow::anyhow!("Failed to set as default for usage: {}", e))
            })?;
        }

        // Print success message
        print_success(&format!("Created identity: {}", self.name));
        println!();

        // Print identity details
        println!("  ID:         {}", identity.id.short());
        if let Some(email) = &identity.email {
            println!("  Email:      {}", email);
        }
        println!("  Type:       {}", super::format_identity_type(&identity.identity_type));
        println!("  Usage:      {}", super::format_usage(&identity.usage));
        println!("  Public Key: {}...", &identity.public_key_base32()[..16]);

        if self.set_default {
            println!();
            print_hint("This identity is now your default");
        } else if self.set_default_for_usage {
            println!();
            print_hint(&format!(
                "This identity is now the default for {} usage",
                identity.usage
            ));
        }

        // Print next steps
        if !self.set_default {
            println!();
            println!("{}", crate::output::hint("Next steps:"));
            println!("  {}  Set as default identity",
                crate::output::command(&format!("atomic identity default {}", self.name)));
            println!("  {}  View identity details",
                crate::output::command(&format!("atomic identity show {}", self.name)));
        }

        Ok(())
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_identity_type_user() {
        let cmd = New {
            name: "test".to_string(),
            email: None,
            identity_type: "user".to_string(),
            usage: "personal".to_string(),
            description: None,
            set_default: false,
            set_default_for_usage: false,
        };
        assert_eq!(cmd.parse_identity_type(), IdentityType::User);
    }

    #[test]
    fn test_parse_identity_type_agent() {
        let cmd = New {
            name: "test".to_string(),
            email: None,
            identity_type: "agent".to_string(),
            usage: "bot".to_string(),
            description: None,
            set_default: false,
            set_default_for_usage: false,
        };
        assert_eq!(cmd.parse_identity_type(), IdentityType::Agent);
    }

    #[test]
    fn test_parse_usage_personal() {
        let cmd = New {
            name: "test".to_string(),
            email: None,
            identity_type: "user".to_string(),
            usage: "personal".to_string(),
            description: None,
            set_default: false,
            set_default_for_usage: false,
        };
        assert!(cmd.parse_usage().is_personal());
    }

    #[test]
    fn test_parse_usage_work() {
        let cmd = New {
            name: "test".to_string(),
            email: None,
            identity_type: "user".to_string(),
            usage: "work".to_string(),
            description: None,
            set_default: false,
            set_default_for_usage: false,
        };
        assert!(cmd.parse_usage().is_work());
    }

    #[test]
    fn test_parse_usage_community() {
        let cmd = New {
            name: "test".to_string(),
            email: None,
            identity_type: "user".to_string(),
            usage: "community".to_string(),
            description: None,
            set_default: false,
            set_default_for_usage: false,
        };
        assert!(cmd.parse_usage().is_community());
    }

    #[test]
    fn test_parse_usage_bot() {
        let cmd = New {
            name: "test".to_string(),
            email: None,
            identity_type: "agent".to_string(),
            usage: "bot".to_string(),
            description: None,
            set_default: false,
            set_default_for_usage: false,
        };
        assert!(cmd.parse_usage().is_bot());
    }

    #[test]
    fn test_new_command_fields() {
        let cmd = New {
            name: "alice".to_string(),
            email: Some("alice@example.com".to_string()),
            identity_type: "user".to_string(),
            usage: "personal".to_string(),
            description: Some("Test identity".to_string()),
            set_default: true,
            set_default_for_usage: false,
        };

        assert_eq!(cmd.name, "alice");
        assert_eq!(cmd.email, Some("alice@example.com".to_string()));
        assert_eq!(cmd.identity_type, "user");
        assert_eq!(cmd.usage, "personal");
        assert_eq!(cmd.description, Some("Test identity".to_string()));
        assert!(cmd.set_default);
        assert!(!cmd.set_default_for_usage);
    }
}
