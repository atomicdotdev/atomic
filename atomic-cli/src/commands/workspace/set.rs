//! The `workspace set` command for setting a default workspace per org.
//!
//! Workspaces are org-scoped, so the default is stored per org in
//! `server.default_workspaces` (a map keyed by org slug). This command
//! updates that map for the current default org, or for an explicit org
//! passed via `--org`.
//!
//! The slug is validated against the server before being written to
//! config — typos fail immediately instead of surfacing as confusing
//! 404s on the next `project create / list / init`.
//!
//! # Usage
//!
//! ```text
//! atomic workspace set <SLUG> [--org <ORG>] [--no-verify]
//!
//! Arguments:
//!   <SLUG>  Workspace slug to set as the default for the target org
//!
//! Options:
//!       --org <ORG>  Set the default workspace for this org instead
//!                    of the current default org
//!       --no-verify  Skip server-side slug validation (advanced)
//!   -h, --help       Print help information
//! ```
//!
//! # Examples
//!
//! ```text
//! # Set default workspace for the current org (validated against the server)
//! $ atomic workspace set backend
//! ✓ Default workspace for 'acme' set to: backend
//!
//! # Typo: fails before any config change
//! $ atomic workspace set backendz
//! ✗ Workspace 'backendz' not found in org 'acme'.
//!
//! # Set default workspace for a different org
//! $ atomic workspace set personal --org alice
//!
//! # Skip validation (e.g. pre-configuring before the workspace exists)
//! $ atomic workspace set new-ws --no-verify
//! ```

use clap::Parser;

use atomic_config::GlobalConfig;

use crate::commands::client::{build_client_with_org, remote_err};
use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_success};

/// Set the default workspace for an org.
///
/// Updates `[server.default_workspaces]` in the global configuration file.
/// All subsequent commands that take an optional `--workspace` parameter
/// fall back to this slug for the target org.
///
/// Validates the slug against the server before saving. Pass `--no-verify`
/// to skip the network round-trip.
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

    /// Skip server-side slug validation.
    ///
    /// Useful for pre-configuring a workspace slug before the workspace
    /// exists on the server, or for offline workflows.
    #[arg(long)]
    pub no_verify: bool,
}

impl Command for WorkspaceSet {
    fn run(&self) -> CliResult<()> {
        self.run_with_verifier(verify_workspace_exists)
    }
}

impl WorkspaceSet {
    /// Inner runner that takes the slug-verification step as a parameter.
    ///
    /// Splitting this out lets tests inject a deterministic verifier
    /// (succeed / fail) without making a real network call, so we can
    /// assert the load-bearing invariant: **when verification fails, the
    /// config file is not modified**.
    fn run_with_verifier<F>(&self, verify: F) -> CliResult<()>
    where
        F: FnOnce(&str, &str) -> CliResult<()>,
    {
        if self.slug.is_empty() {
            return Err(CliError::InvalidArgument {
                message: "Workspace slug cannot be empty.".to_string(),
            });
        }

        let mut config = GlobalConfig::load().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to load global config: {e}"))
        })?;

        // Determine the target org. Explicit --org wins; otherwise fall
        // back to the configured default. We don't go through `resolve_org`
        // here because we want a distinct error message tied to *this*
        // command's flow.
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

        // Validate against the server first so a bad slug fails fast
        // rather than producing confusing 404s on subsequent commands.
        // The `?` here is load-bearing: if it returns Err, the mutation
        // and save below never execute.
        if !self.no_verify {
            verify(&target_org, &self.slug)?;
        }

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

/// Confirm the workspace slug resolves on the server under the target org.
///
/// 404s surface as a clean slug-specific message; other failures (network,
/// auth, wrong org) bubble through so the user can distinguish problems.
fn verify_workspace_exists(org_slug: &str, workspace_slug: &str) -> CliResult<()> {
    let rt = tokio::runtime::Runtime::new()
        .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {e}")))?;

    rt.block_on(async {
        let (client, _resolved) = build_client_with_org(Some(org_slug)).await?;
        match client.get_workspace(workspace_slug).await {
            Ok(_) => Ok(()),
            Err(e) if e.is_not_found() => Err(CliError::InvalidArgument {
                message: format!(
                    "Workspace '{workspace_slug}' not found in org '{org_slug}'.\n  \
                     Check the slug with: atomic workspace list --org {org_slug}\n  \
                     Or pass --no-verify to set the value without checking."
                ),
            }),
            Err(e) => Err(remote_err(e)),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_slug_rejected() {
        let cmd = WorkspaceSet {
            slug: "".to_string(),
            org: None,
            no_verify: true, // skip network so the empty-slug check is the only failure
        };
        let err = cmd.run().unwrap_err();
        match err {
            CliError::InvalidArgument { message } => {
                assert!(message.contains("cannot be empty"));
            }
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    #[test]
    fn empty_org_override_rejected() {
        let cmd = WorkspaceSet {
            slug: "backend".to_string(),
            org: Some("".to_string()),
            no_verify: true,
        };
        let err = cmd.run().unwrap_err();
        match err {
            CliError::InvalidArgument { message } => {
                assert!(message.contains("Organization"));
                assert!(message.contains("empty"));
            }
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    #[test]
    fn fields_stored() {
        let cmd = WorkspaceSet {
            slug: "backend".to_string(),
            org: Some("acme".to_string()),
            no_verify: false,
        };
        assert_eq!(cmd.slug, "backend");
        assert_eq!(cmd.org.as_deref(), Some("acme"));
        assert!(!cmd.no_verify);
    }

    // -----------------------------------------------------------------
    // Regression invariant: when the verifier fails, config must NOT be
    // mutated. Protects against accidentally reordering verify / save in
    // future refactors.
    //
    // Tests touch HOME to redirect `GlobalConfig::{load,save}` at an
    // isolated config dir, so `#[serial]` is required.
    // -----------------------------------------------------------------

    use serial_test::serial;

    struct HomeGuard {
        _tmp: tempfile::TempDir,
        original: Option<std::ffi::OsString>,
    }

    impl HomeGuard {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let original = std::env::var_os("HOME");
            // SAFETY: env mutation is technically UB if accessed
            // concurrently from another thread. `#[serial]` serializes
            // these tests against each other but does NOT prevent
            // unmarked tests elsewhere in the workspace from reading
            // HOME concurrently. No other test in this crate currently
            // mutates or reads HOME outside its own `#[serial]` block,
            // so the race window is empty today. A path-aware
            // GlobalConfig API would eliminate this — tracked as a
            // follow-up.
            unsafe {
                std::env::set_var("HOME", tmp.path());
            }
            Self {
                _tmp: tmp,
                original,
            }
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            // SAFETY: see HomeGuard::new.
            unsafe {
                match &self.original {
                    Some(v) => std::env::set_var("HOME", v),
                    None => std::env::remove_var("HOME"),
                }
            }
        }
    }

    fn seed_config(default_org: &str, existing_workspace: Option<(&str, &str)>) {
        let mut cfg = GlobalConfig::load().unwrap();
        cfg.server.default_org = Some(default_org.to_string());
        if let Some((org, ws)) = existing_workspace {
            cfg.server
                .default_workspaces
                .insert(org.to_string(), ws.to_string());
        }
        cfg.save().unwrap();
    }

    #[test]
    #[serial]
    fn config_unchanged_when_verifier_fails() {
        let _guard = HomeGuard::new();
        seed_config("acme", Some(("acme", "old-ws")));

        let cmd = WorkspaceSet {
            slug: "new-ws".to_string(),
            org: None, // use default org "acme"
            no_verify: false,
        };

        let result = cmd.run_with_verifier(|org, ws| {
            Err(CliError::InvalidArgument {
                message: format!("Workspace '{ws}' not found in org '{org}'."),
            })
        });

        assert!(result.is_err(), "verifier failure should propagate");

        // Critical invariant: the old workspace mapping is still there,
        // unchanged.
        let after = GlobalConfig::load().unwrap();
        assert_eq!(
            after.server.default_workspaces.get("acme"),
            Some(&"old-ws".to_string()),
            "default_workspaces[acme] must not change when verification fails"
        );
    }

    #[test]
    #[serial]
    fn config_written_when_verifier_succeeds() {
        let _guard = HomeGuard::new();
        seed_config("acme", Some(("acme", "old-ws")));

        let cmd = WorkspaceSet {
            slug: "new-ws".to_string(),
            org: None,
            no_verify: false,
        };

        let result = cmd.run_with_verifier(|_, _| Ok(()));
        assert!(result.is_ok(), "verifier success path should succeed");

        let after = GlobalConfig::load().unwrap();
        assert_eq!(
            after.server.default_workspaces.get("acme"),
            Some(&"new-ws".to_string()),
        );
    }

    #[test]
    #[serial]
    fn no_verify_skips_verifier_entirely() {
        let _guard = HomeGuard::new();
        seed_config("acme", None);

        // Verifier panics if called — `--no-verify` must short-circuit.
        let cmd = WorkspaceSet {
            slug: "new-ws".to_string(),
            org: None,
            no_verify: true,
        };

        let result = cmd
            .run_with_verifier(|_, _| panic!("verifier must not be called when no_verify is true"));
        assert!(result.is_ok());

        let after = GlobalConfig::load().unwrap();
        assert_eq!(
            after.server.default_workspaces.get("acme"),
            Some(&"new-ws".to_string()),
        );
    }

    #[test]
    #[serial]
    fn explicit_org_overrides_default_for_target() {
        // Side-benefit test: writing to a non-default org leaves the
        // default org's workspace alone.
        let _guard = HomeGuard::new();
        seed_config("acme", Some(("acme", "acme-ws")));

        let cmd = WorkspaceSet {
            slug: "alice-ws".to_string(),
            org: Some("alice".to_string()),
            no_verify: false,
        };

        let result = cmd.run_with_verifier(|_, _| Ok(()));
        assert!(result.is_ok());

        let after = GlobalConfig::load().unwrap();
        assert_eq!(
            after.server.default_workspaces.get("acme"),
            Some(&"acme-ws".to_string()),
            "acme's workspace must not change when writing to alice"
        );
        assert_eq!(
            after.server.default_workspaces.get("alice"),
            Some(&"alice-ws".to_string()),
        );
    }
}
