//! The `workspace set` command for setting a default workspace per org.
//!
//! Workspaces are org-scoped, so the default is stored per org in
//! `server.default_workspaces` (a map keyed by org slug). This command
//! updates that map for the current default org, or for an explicit org
//! passed via `--org`.
//!
//! # Usage
//!
//! ```text
//! atomic workspace set <SLUG> [--org <ORG>]
//!
//! Arguments:
//!   <SLUG>  Workspace slug to set as the default for the target org
//!
//! Options:
//!       --org <ORG>  Set the default workspace for this org instead
//!                    of the current default org
//!   -h, --help       Print help information
//! ```
//!
//! # Examples
//!
//! ```text
//! # Set default workspace for the current org
//! $ atomic workspace set backend
//! ✓ Default workspace for 'acme' set to: backend
//!
//! # Set default workspace for a different org (does not change current org)
//! $ atomic workspace set personal --org alice
//! ✓ Default workspace for 'alice' set to: personal
//!   (current default org is 'acme' — run 'atomic org set alice' to use it)
//! ```

use clap::Parser;

use atomic_config::GlobalConfig;

use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_success};

/// Set the default workspace for an org.
///
/// Updates `[server.default_workspaces]` in the global configuration file.
/// All subsequent commands that take an optional `--workspace` parameter
/// fall back to this slug for the target org.
///
/// This command does **not** validate that the slug exists on the server.
/// If the slug is invalid, subsequent commands will fail with a "not found"
/// error. This matches the behavior of `atomic org set`.
#[derive(Debug, Parser, Default)]
#[command(name = "set")]
pub struct WorkspaceSet {
    /// Workspace slug to set as the new default.
    ///
    /// This should be the URL-safe slug of a workspace that exists on
    /// the remote server under the target org.
    #[arg(required = true, value_name = "SLUG")]
    pub slug: String,

    /// Org to set the default workspace for.
    ///
    /// If omitted, uses the current default org from global config.
    /// Passing this lets you pre-configure a workspace for an org you
    /// are not currently using as the default.
    #[arg(long, value_name = "ORG")]
    pub org: Option<String>,
}

impl Command for WorkspaceSet {
    fn run(&self) -> CliResult<()> {
        if self.slug.is_empty() {
            return Err(CliError::InvalidArgument {
                message: "Workspace slug cannot be empty.".to_string(),
            });
        }

        let mut config = GlobalConfig::load().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to load global config: {e}"))
        })?;

        // Determine the target org. Explicit --org wins; otherwise fall back
        // to the configured default. We don't go through `resolve_org` here
        // because we want a distinct error message tied to *this* command.
        let target_org = match self.org.as_deref() {
            Some(o) if !o.is_empty() => o.to_string(),
            Some(_) => {
                return Err(CliError::InvalidArgument {
                    message: "Organization slug cannot be empty.".to_string(),
                });
            }
            None => config
                .server
                .default_org
                .clone()
                .ok_or_else(|| CliError::InvalidArgument {
                    message: "No default org set. Use --org or first run: \
                              atomic org set <slug>"
                        .to_string(),
                })?,
        };

        let previous = config
            .server
            .default_workspaces
            .insert(target_org.clone(), self.slug.clone());

        config.save().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to save global config: {e}"))
        })?;

        print_success(&format!(
            "Default workspace for '{}' set to: {}",
            target_org, self.slug
        ));

        if let Some(prev) = previous.as_ref() {
            if prev != &self.slug {
                print_hint(&format!("Previous default was: {prev}"));
            }
        }

        // If the user set a workspace for a non-current org, remind them
        // that it won't take effect until they switch orgs.
        if let Some(current_org) = config.server.default_org.as_deref() {
            if current_org != target_org {
                print_hint(&format!(
                    "Current default org is '{current_org}' — \
                     run 'atomic org set {target_org}' to use this workspace."
                ));
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_slug_rejected() {
        let cmd = WorkspaceSet {
            slug: "".to_string(),
            org: None,
        };
        let err = cmd.run().unwrap_err();
        match err {
            CliError::InvalidArgument { message } => {
                assert!(message.contains("cannot be empty"));
            }
            other => panic!("expected InvalidArgument, got {:?}", other),
        }
    }

    #[test]
    fn empty_org_override_rejected() {
        let cmd = WorkspaceSet {
            slug: "backend".to_string(),
            org: Some("".to_string()),
        };
        let err = cmd.run().unwrap_err();
        match err {
            CliError::InvalidArgument { message } => {
                assert!(message.contains("Organization"));
                assert!(message.contains("empty"));
            }
            other => panic!("expected InvalidArgument, got {:?}", other),
        }
    }

    #[test]
    fn fields_stored() {
        let cmd = WorkspaceSet {
            slug: "backend".to_string(),
            org: Some("acme".to_string()),
        };
        assert_eq!(cmd.slug, "backend");
        assert_eq!(cmd.org.as_deref(), Some("acme"));
    }
}
