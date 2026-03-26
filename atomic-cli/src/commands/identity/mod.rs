//! Identity management commands for the Atomic CLI.
//!
//! This module provides commands for managing user identities, including:
//!
//! - Creating new identities (personal, work, community, agent)
//! - Listing existing identities
//! - Setting default identities
//! - Showing identity details
//! - Deleting identities
//!
//! # Usage
//!
//! ```text
//! atomic identity <COMMAND>
//!
//! Commands:
//!   new      Create a new identity
//!   list     List all identities
//!   show     Show identity details
//!   default  Set the default identity
//!   delete   Delete an identity
//!   whoami   Show the current default identity
//! ```
//!
//! # Example
//!
//! ```text
//! # Create a new personal identity
//! atomic identity new alice --email alice@example.com --usage personal
//!
//! # List all identities
//! atomic identity list
//!
//! # Set default identity for work
//! atomic identity default alice-work --usage work
//!
//! # Show current identity
//! atomic identity whoami
//! ```

pub mod delete;
pub mod list;
pub mod new;
pub mod register;
pub mod show;
pub mod whoami;

// Re-export command structs
pub use delete::Delete;
pub use list::List;
pub use new::New;
pub use register::Register;
pub use show::Show;
pub use whoami::WhoAmI;

use clap::Subcommand;

use crate::commands::Command;
use crate::error::CliResult;

// Identity Command Router

/// Identity management commands.
///
/// Manage user identities for signing changes. Atomic supports multiple
/// identities for different contexts (personal, work, community) and
/// agent identities for automated systems.
#[derive(Debug, clap::Args)]
pub struct Identity {
    /// The identity subcommand to run.
    #[command(subcommand)]
    pub command: IdentityCommands,
}

/// Available identity subcommands.
#[derive(Debug, Subcommand)]
pub enum IdentityCommands {
    /// Create a new identity.
    ///
    /// Generates a new Ed25519 keypair and stores the identity locally.
    /// The identity can be used to sign changes and prove authorship.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Create a personal identity
    /// atomic identity new alice --email alice@example.com
    ///
    /// # Create a work identity
    /// atomic identity new alice-work --email alice@company.com --usage work
    ///
    /// # Create an agent identity
    /// atomic identity new ci-bot --type agent --usage bot
    /// ```
    New(New),

    /// List all identities.
    ///
    /// Shows all identities stored locally with their names, types, and
    /// usage contexts.
    ///
    /// # Examples
    ///
    /// ```text
    /// # List all identities
    /// atomic identity list
    ///
    /// # List only work identities
    /// atomic identity list --usage work
    ///
    /// # Show verbose output
    /// atomic identity list -v
    /// ```
    List(List),

    /// Show identity details.
    ///
    /// Displays detailed information about a specific identity, including
    /// the public key and metadata.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Show identity by name
    /// atomic identity show alice
    ///
    /// # Show in JSON format
    /// atomic identity show alice --format json
    /// ```
    Show(Show),

    /// Set the default identity.
    ///
    /// Sets which identity to use by default when recording changes.
    /// You can set a global default or a default for specific usage contexts.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Set global default
    /// atomic identity default alice
    ///
    /// # Set default for work context
    /// atomic identity default alice-work --usage work
    ///
    /// # Clear the default
    /// atomic identity default --clear
    /// ```
    Default(Default),

    /// Delete an identity.
    ///
    /// Removes an identity from local storage. This cannot be undone.
    /// The identity's secret key will be permanently deleted.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Delete an identity
    /// atomic identity delete old-identity
    ///
    /// # Force delete without confirmation
    /// atomic identity delete old-identity --force
    /// ```
    Delete(Delete),

    /// Show the current default identity.
    ///
    /// Displays information about the identity that will be used for
    /// signing changes.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Show current identity
    /// atomic identity whoami
    ///
    /// # Show identity for work context
    /// atomic identity whoami --usage work
    /// ```
    #[command(name = "whoami")]
    WhoAmI(WhoAmI),

    /// Register an identity with a remote atomic-storage server.
    ///
    /// Pushes the local identity to a server, creating a tenant whose
    /// slug is derived from the username. Authentication is Ed25519
    /// signature-based — no passwords required.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Register with the default identity
    /// atomic identity register https://atomic.storage
    ///
    /// # Register a specific identity
    /// atomic identity register https://atomic.storage --identity alice-work
    /// ```
    Register(Register),
}

impl Command for Identity {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            IdentityCommands::New(cmd) => cmd.run(),
            IdentityCommands::List(cmd) => cmd.run(),
            IdentityCommands::Show(cmd) => cmd.run(),
            IdentityCommands::Default(cmd) => cmd.run(),
            IdentityCommands::Delete(cmd) => cmd.run(),
            IdentityCommands::WhoAmI(cmd) => cmd.run(),
            IdentityCommands::Register(cmd) => cmd.run(),
        }
    }
}

// Default Subcommand

/// Set the default identity.
#[derive(Debug, clap::Args)]
pub struct Default {
    /// Name of the identity to set as default.
    ///
    /// If not provided with --clear, this is required.
    #[arg(required_unless_present = "clear")]
    pub name: Option<String>,

    /// Usage context to set the default for.
    ///
    /// If not specified, sets the global default.
    #[arg(long, short = 'u')]
    pub usage: Option<String>,

    /// Clear the default identity instead of setting one.
    #[arg(long, conflicts_with = "name")]
    pub clear: bool,
}

impl Command for Default {
    fn run(&self) -> CliResult<()> {
        use crate::output::{print_success, print_warning};
        use atomic_identity::{IdentityStore, IdentityUsage};

        let mut store = IdentityStore::open_default().map_err(|e| {
            crate::error::CliError::Internal(anyhow::anyhow!(
                "Failed to open identity store: {}",
                e
            ))
        })?;

        if self.clear {
            // Clear the default
            if let Some(usage_str) = &self.usage {
                let usage = IdentityUsage::parse(usage_str);
                // Clear usage-specific default by setting global default
                // Note: The store doesn't have a clear_default_for_usage method yet
                // For now, we just inform the user
                print_warning(&format!(
                    "Clearing default for usage '{}' is not yet supported",
                    usage
                ));
            } else {
                store.clear_default().map_err(|e| {
                    crate::error::CliError::Internal(anyhow::anyhow!(
                        "Failed to clear default: {}",
                        e
                    ))
                })?;
                print_success("Cleared default identity");
            }
        } else if let Some(name) = &self.name {
            // Load the identity by name
            let identity = store
                .load_by_name(name)
                .map_err(|_| crate::error::CliError::IdentityNotFound(name.clone()))?;

            if let Some(usage_str) = &self.usage {
                let usage = IdentityUsage::parse(usage_str);
                store
                    .set_default_for_usage(&usage, &identity.id)
                    .map_err(|e| {
                        crate::error::CliError::Internal(anyhow::anyhow!(
                            "Failed to set default: {}",
                            e
                        ))
                    })?;
                print_success(&format!(
                    "Set '{}' as default identity for {} usage",
                    name, usage
                ));
            } else {
                store.set_default(&identity.id).map_err(|e| {
                    crate::error::CliError::Internal(anyhow::anyhow!(
                        "Failed to set default: {}",
                        e
                    ))
                })?;
                print_success(&format!("Set '{}' as default identity", name));
            }
        }

        Ok(())
    }
}

// Helper Functions

/// Format an identity type for display.
pub fn format_identity_type(identity_type: &atomic_identity::IdentityType) -> &'static str {
    match identity_type {
        atomic_identity::IdentityType::User => "user",
        atomic_identity::IdentityType::Agent => "agent",
        atomic_identity::IdentityType::Delegated => "delegated",
    }
}

/// Format a usage context for display.
pub fn format_usage(usage: &atomic_identity::IdentityUsage) -> String {
    usage.to_string()
}

/// Parse an identity type from a string.
pub fn parse_identity_type(s: &str) -> atomic_identity::IdentityType {
    match s.to_lowercase().as_str() {
        "agent" | "bot" => atomic_identity::IdentityType::Agent,
        "delegated" => atomic_identity::IdentityType::Delegated,
        _ => atomic_identity::IdentityType::User,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_identity::{IdentityType, IdentityUsage};

    #[test]
    fn test_format_identity_type() {
        assert_eq!(format_identity_type(&IdentityType::User), "user");
        assert_eq!(format_identity_type(&IdentityType::Agent), "agent");
        assert_eq!(format_identity_type(&IdentityType::Delegated), "delegated");
    }

    #[test]
    fn test_format_usage() {
        assert_eq!(format_usage(&IdentityUsage::Personal), "personal");
        assert_eq!(format_usage(&IdentityUsage::Work), "work");
        assert_eq!(format_usage(&IdentityUsage::Community), "community");
        assert_eq!(format_usage(&IdentityUsage::Bot), "bot");
    }

    #[test]
    fn test_parse_identity_type() {
        assert_eq!(parse_identity_type("user"), IdentityType::User);
        assert_eq!(parse_identity_type("agent"), IdentityType::Agent);
        assert_eq!(parse_identity_type("bot"), IdentityType::Agent);
        assert_eq!(parse_identity_type("delegated"), IdentityType::Delegated);
        assert_eq!(parse_identity_type("unknown"), IdentityType::User);
    }
}
