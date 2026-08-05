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
        self.run_with_verifier(verify_org_exists)
    }
}

impl OrgSet {
    /// Inner runner that takes the slug-verification step as a parameter.
    ///
    /// Splitting this out lets tests inject a deterministic verifier
    /// (succeed / fail) without making a real network call, so we can
    /// assert the load-bearing invariant: **when verification fails, the
    /// config file is not modified**.
    fn run_with_verifier<F>(&self, verify: F) -> CliResult<()>
    where
        F: FnOnce(&str) -> CliResult<()>,
    {
        if self.slug.is_empty() {
            return Err(CliError::InvalidArgument {
                message: "Organization slug cannot be empty.".to_string(),
            });
        }

        // Validate against the server first so a bad slug fails fast
        // rather than self-locking subsequent commands. The `?` here is
        // load-bearing: if it returns Err, config is not touched.
        if !self.no_verify {
            verify(&self.slug)?;
        }

        let mut config = GlobalConfig::load().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to load global config: {e}"))
        })?;

        // Write to the *active* server profile (named profile when one is
        // active, else the legacy `[server]` block) so the new default is
        // read back by org/workspace/project commands, which resolve the
        // same profile.
        let (server, profile_name) = config
            .resolve_server_mut(None)
            .map_err(|e| CliError::Internal(anyhow::anyhow!("{e}")))?;

        let previous = server.default_org.clone();
        server.default_org = Some(self.slug.clone());

        config.save().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to save global config: {e}"))
        })?;

        match &profile_name {
            Some(name) => print_success(&format!(
                "Default organization for server '{name}' set to: {}",
                self.slug
            )),
            None => print_success(&format!("Default organization set to: {}", self.slug)),
        }

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
        let (client, _resolved) = build_client_with_org(Some(slug), None).await?;
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

    // -----------------------------------------------------------------
    // Regression invariant: when the verifier fails, config must NOT be
    // mutated. This protects against accidentally reordering the verify /
    // save steps in future refactors.
    //
    // Tests touch HOME to redirect `GlobalConfig::{load,save}` at an
    // isolated config dir, so `#[serial]` is required.
    // -----------------------------------------------------------------

    use serial_test::serial;

    /// Save the current `HOME` value and point it at a tempdir for the
    /// lifetime of the guard. Restores on drop. Wraps the tempdir so the
    /// directory lives as long as `HOME` points at it.
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

    fn seed_config(default_org: &str) {
        let mut cfg = GlobalConfig::load().unwrap();
        cfg.server.default_org = Some(default_org.to_string());
        cfg.save().unwrap();
    }

    #[test]
    #[serial]
    fn config_unchanged_when_verifier_fails() {
        let _guard = HomeGuard::new();
        seed_config("old-org");

        let cmd = OrgSet {
            slug: "new-org".to_string(),
            no_verify: false,
        };

        let result = cmd.run_with_verifier(|slug| {
            // Simulate "slug not found on server".
            Err(CliError::InvalidArgument {
                message: format!("Organization '{slug}' not found on the server."),
            })
        });

        assert!(result.is_err(), "verifier failure should propagate");

        // Critical invariant: config on disk still has the old default.
        let after = GlobalConfig::load().unwrap();
        assert_eq!(
            after.server.default_org.as_deref(),
            Some("old-org"),
            "default_org must not change when verification fails"
        );
    }

    #[test]
    #[serial]
    fn writes_to_active_named_profile_not_legacy_block() {
        // Regression for the "org set doesn't pass through" bug: when a named
        // profile is active (default_server set), `org set` must mutate that
        // profile's default_org, not the legacy [server] block — otherwise
        // read commands (which resolve the named profile) never see it.
        let _guard = HomeGuard::new();

        let mut cfg = GlobalConfig::load().unwrap();
        cfg.default_server = Some("prod".to_string());
        cfg.servers.insert(
            "prod".to_string(),
            atomic_config::ServerConfig {
                url: Some("https://atomic.storage".to_string()),
                default_org: None,
                default_workspaces: std::collections::BTreeMap::new(),
                identity: Some("continuouslee".to_string()),
                single_tenant: false,
            },
        );
        cfg.save().unwrap();

        let cmd = OrgSet {
            slug: "atomic".to_string(),
            no_verify: false,
        };
        let result = cmd.run_with_verifier(|_| Ok(()));
        assert!(result.is_ok());

        let after = GlobalConfig::load().unwrap();
        assert_eq!(
            after.servers["prod"].default_org.as_deref(),
            Some("atomic"),
            "org set must write to the active named profile"
        );
        assert!(
            after.server.default_org.is_none(),
            "org set must not touch the legacy [server] block when a named profile is active"
        );
    }

    #[test]
    #[serial]
    fn config_written_when_verifier_succeeds() {
        let _guard = HomeGuard::new();
        seed_config("old-org");

        let cmd = OrgSet {
            slug: "new-org".to_string(),
            no_verify: false,
        };

        let result = cmd.run_with_verifier(|_| Ok(()));
        assert!(result.is_ok(), "verifier success path should succeed");

        let after = GlobalConfig::load().unwrap();
        assert_eq!(after.server.default_org.as_deref(), Some("new-org"));
    }

    #[test]
    #[serial]
    fn no_verify_skips_verifier_entirely() {
        let _guard = HomeGuard::new();
        seed_config("old-org");

        // The verifier panics if called — `--no-verify` must short-circuit
        // before reaching it.
        let cmd = OrgSet {
            slug: "new-org".to_string(),
            no_verify: true,
        };

        let result =
            cmd.run_with_verifier(|_| panic!("verifier must not be called when no_verify is true"));
        assert!(result.is_ok());

        let after = GlobalConfig::load().unwrap();
        assert_eq!(after.server.default_org.as_deref(), Some("new-org"));
    }
}
