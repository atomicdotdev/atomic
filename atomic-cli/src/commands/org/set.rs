//! The `org set` command for changing the default organization.
//!
//! This module implements the `atomic org set` command, which updates
//! the `server.default_org` field in the global configuration file. All
//! subsequent commands that interact with the remote server will use the
//! new default organization.
//!
//! The slug is validated against the server before being written to
//! config — typos fail immediately instead of self-locking subsequent
//! commands. Accepts `switch` as a hidden alias for backward compatibility
//! with the previous command name.
//!
//! # Usage
//!
//! ```text
//! atomic org set <SLUG>
//!
//! Arguments:
//!   <SLUG>  Organization slug to set as the new default
//!
//! Options:
//!       --no-verify  Skip server-side slug validation (advanced)
//!   -h, --help       Print help information
//! ```
//!
//! # Examples
//!
//! ```text
//! # Set the default org (validated against the server)
//! $ atomic org set acme-corp
//! ✓ Default organization set to: acme-corp
//!
//! # Typo: fails before any config change
//! $ atomic org set acme-corpz
//! ✗ Organization 'acme-corpz' not found on the server.
//!
//! # Skip validation (e.g. pre-configuring before the org exists)
//! $ atomic org set acme-corp --no-verify
//! ```

use clap::Parser;

use atomic_config::GlobalConfig;

use crate::commands::client::build_client_with_org;
use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_success};

/// Set the default organization.
///
/// Updates the `[server] default_org` field in the global configuration
/// file (`~/.atomic/config.toml`). All subsequent commands that interact
/// with the remote server will scope their requests to this organization.
///
/// Validates the slug against the server before saving (the server
/// responds with 404 if no tenant matches the subdomain). Pass
/// `--no-verify` to skip the network round-trip.
#[derive(Debug, Parser, Default)]
#[command(name = "set", alias = "switch")]
pub struct OrgSet {
    /// Organization slug to set as the new default.
    ///
    /// This should be the URL-safe slug of an organization you belong
    /// to on the remote server (e.g. `"acme-corp"`).
    #[arg(required = true, value_name = "SLUG")]
    pub slug: String,

    /// Skip server-side slug validation.
    ///
    /// Useful for pre-configuring an org slug before the org exists on
    /// the server, or for offline workflows. Without this flag, the
    /// command fails fast if the slug does not resolve to a tenant.
    #[arg(long)]
    pub no_verify: bool,
}

impl Command for OrgSet {
    fn run(&self) -> CliResult<()> {
        if self.slug.is_empty() {
            return Err(CliError::InvalidArgument {
                message: "Organization slug cannot be empty.".to_string(),
            });
        }

        // Validate against the server first so a bad slug fails fast
        // rather than self-locking subsequent commands.
        if !self.no_verify {
            verify_org_exists(&self.slug)?;
        }

        let mut config = GlobalConfig::load().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to load global config: {e}"))
        })?;

        let previous = config.server.default_org.clone();
        config.server.default_org = Some(self.slug.clone());
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

/// Confirm the slug resolves to an org on the server.
///
/// Returns a [`CliError::InvalidArgument`] with a clean, slug-specific
/// message on 404 so the user sees the actual problem (typo) rather than
/// a raw HTTP response. Other failures (network, auth) bubble up so the
/// user can distinguish "wrong slug" from "server unreachable".
fn verify_org_exists(slug: &str) -> CliResult<()> {
    let rt = tokio::runtime::Runtime::new()
        .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {e}")))?;

    rt.block_on(async {
        let (client, _resolved) = build_client_with_org(Some(slug))?;
        match atomic_teams::org::get_org(&client, slug).await {
            Ok(_) => Ok(()),
            Err(e) if is_org_not_found(&e) => Err(CliError::InvalidArgument {
                message: format!(
                    "Organization '{slug}' not found on the server.\n  \
                     Check the slug with: atomic org list\n  \
                     Or pass --no-verify to set the value without checking."
                ),
            }),
            Err(e) => Err(CliError::RemoteError {
                message: e.to_string(),
                url: None,
            }),
        }
    })
}

fn is_org_not_found(err: &atomic_teams::TeamsError) -> bool {
    matches!(err, atomic_teams::TeamsError::OrgNotFound(_))
        || matches!(
            err,
            atomic_teams::TeamsError::Remote(re) if re.is_not_found()
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_slug() {
        let cmd = OrgSet {
            slug: "acme-corp".to_string(),
            no_verify: false,
        };
        assert_eq!(cmd.slug, "acme-corp");
    }

    #[test]
    fn slug_with_hyphens() {
        let cmd = OrgSet {
            slug: "my-cool-org-123".to_string(),
            no_verify: false,
        };
        assert_eq!(cmd.slug, "my-cool-org-123");
    }

    #[test]
    fn slug_simple() {
        let cmd = OrgSet {
            slug: "alice".to_string(),
            no_verify: false,
        };
        assert_eq!(cmd.slug, "alice");
    }

    #[test]
    fn empty_slug_rejected() {
        let cmd = OrgSet {
            slug: "".to_string(),
            no_verify: true, // skip network so the check is the only failure
        };
        let err = cmd.run().unwrap_err();
        match err {
            CliError::InvalidArgument { message } => {
                assert!(message.contains("cannot be empty"));
            }
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }
}
