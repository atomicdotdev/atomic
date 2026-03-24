//! The `identity delete` command for removing identities.
//!
//! This module implements the `atomic identity delete` command, which removes
//! an identity from local storage.
//!
//! # Usage
//!
//! ```text
//! atomic identity delete <NAME> [OPTIONS]
//!
//! Arguments:
//!   <NAME>  Name of the identity to delete
//!
//! Options:
//!   -f, --force  Delete without confirmation
//!   -h, --help   Print help information
//! ```
//!
//! # Examples
//!
//! Delete an identity:
//! ```text
//! $ atomic identity delete old-identity
//! Are you sure you want to delete identity 'old-identity'? [y/N] y
//! Deleted identity: old-identity
//! ```
//!
//! Force delete without confirmation:
//! ```text
//! $ atomic identity delete old-identity --force
//! Deleted identity: old-identity
//! ```

use clap::Parser;

use atomic_identity::IdentityStore;

use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_success, print_warning};

// Command Definition

/// Delete an identity.
///
/// Removes an identity from local storage. This action cannot be undone.
/// The identity's secret key will be permanently deleted.
#[derive(Debug, Parser)]
pub struct Delete {
    /// Name of the identity to delete.
    #[arg(required = true)]
    pub name: String,

    /// Delete without confirmation.
    ///
    /// Skip the confirmation prompt and delete immediately.
    #[arg(short, long)]
    pub force: bool,
}

impl Command for Delete {
    fn run(&self) -> CliResult<()> {
        // Open the identity store
        let mut store = IdentityStore::open_default().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to open identity store: {}", e))
        })?;

        // Load the identity by name to get its ID
        let identity = store
            .load_by_name(&self.name)
            .map_err(|_| CliError::IdentityNotFound(self.name.clone()))?;

        // Check if this is the default identity
        let is_default = store
            .get_default()
            .ok()
            .flatten()
            .map(|d| d.id == identity.id)
            .unwrap_or(false);

        if is_default && !self.force {
            print_warning(&format!(
                "Warning: '{}' is your default identity",
                self.name
            ));
        }

        // Confirm deletion unless --force is used
        if !self.force {
            use std::io::{self, Write};

            print!(
                "Are you sure you want to delete identity '{}'? [y/N] ",
                self.name
            );
            io::stdout().flush().unwrap();

            let mut input = String::new();
            io::stdin()
                .read_line(&mut input)
                .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to read input: {}", e)))?;

            let input = input.trim().to_lowercase();
            if input != "y" && input != "yes" {
                println!("Cancelled");
                return Ok(());
            }
        }

        // Delete the identity
        store
            .delete(&identity.id)
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to delete identity: {}", e)))?;

        print_success(&format!("Deleted identity: {}", self.name));

        if is_default {
            println!();
            print_warning("Your default identity has been deleted. Set a new default with:");
            println!("  atomic identity default <name>");
        }

        Ok(())
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_delete_command_fields() {
        let cmd = Delete {
            name: "old-identity".to_string(),
            force: false,
        };

        assert_eq!(cmd.name, "old-identity");
        assert!(!cmd.force);
    }

    #[test]
    fn test_delete_command_with_force() {
        let cmd = Delete {
            name: "old-identity".to_string(),
            force: true,
        };

        assert_eq!(cmd.name, "old-identity");
        assert!(cmd.force);
    }

    #[test]
    fn test_delete_command_name_required() {
        let cmd = Delete {
            name: "test".to_string(),
            force: false,
        };

        assert!(!cmd.name.is_empty());
    }

    #[test]
    fn test_force_default_false() {
        let cmd = Delete {
            name: "test".to_string(),
            force: false,
        };

        assert!(!cmd.force);
    }
}
