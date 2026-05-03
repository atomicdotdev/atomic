//! The `org switch` command for changing the default organization.
//!
//! This module implements the `atomic org switch` command, which updates
//! the `server.default_org` field in the global configuration file. All
//! subsequent commands that interact with the remote server will use the
//! new default organization.
//!
//! # Usage
//!
//! ```text
//! atomic org switch <SLUG>
//!
//! Arguments:
//!   <SLUG>  Organization slug to set as the new default
//!
//! Options:
//!   -h, --help  Print help information
//! ```
//!
//! # Examples
//!
//! ```text
//! # Switch to a different org
//! $ atomic org switch acme-corp
//! ✓ Default organization set to: acme-corp
//!
//! # Verify the switch
//! $ atomic org show
//!   Name:  Acme Corp
//!   Slug:  acme-corp
//!   ...
//! ```

use clap::Parser;

use atomic_config::GlobalConfig;

use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_success};

/// Switch the default organization.
///
/// Updates the `[server] default_org` field in the global configuration
/// file (`~/.config/atomic/config.toml`). All subsequent commands that
/// interact with the remote server will scope their requests to this
/// organization.
///
/// This command does **not** validate that the slug exists on the server.
/// If the slug is invalid, subsequent commands will fail with a "not found"
/// error.
#[derive(Debug, Parser)]
#[command(name = "switch")]
pub struct OrgSwitch {
    /// Organization slug to set as the new default.
    ///
    /// This should be the URL-safe slug of an organization you belong
    /// to on the remote server (e.g. `"acme-corp"`).
    #[arg(required = true, value_name = "SLUG")]
    pub slug: String,
}

impl Command for OrgSwitch {
    fn run(&self) -> CliResult<()> {
        // Load the current global config (or defaults if none exists).
        let mut config = GlobalConfig::load().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to load global config: {e}"))
        })?;

        let previous = config.server.default_org.clone();

        // Update the default org.
        config.server.default_org = Some(self.slug.clone());

        // Persist to disk.
        config.save().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to save global config: {e}"))
        })?;

        print_success(&format!("Default organization set to: {}", self.slug));

        if let Some(prev) = &previous {
            if prev != &self.slug {
                print_hint(&format!("Previous default was: {prev}"));
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_slug() {
        let cmd = OrgSwitch {
            slug: "acme-corp".to_string(),
        };
        assert_eq!(cmd.slug, "acme-corp");
    }

    #[test]
    fn slug_with_hyphens() {
        let cmd = OrgSwitch {
            slug: "my-cool-org-123".to_string(),
        };
        assert_eq!(cmd.slug, "my-cool-org-123");
    }

    #[test]
    fn slug_simple() {
        let cmd = OrgSwitch {
            slug: "alice".to_string(),
        };
        assert_eq!(cmd.slug, "alice");
    }
}
