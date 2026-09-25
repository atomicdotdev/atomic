//! `atomic agent identity` — select the delegated identity hooks sign as.
//!
//! Agent hooks record turns as the plus-tag of the default identity by
//! default — legible, but keyed to the human. This command tree selects a
//! delegated agent identity so every hooked agent on this machine records
//! under the agent's own key instead.
//!
//! The selection is deliberately **global** (one identity per machine, not
//! per repository): `set` writes `agent_identity` to `~/.atomic/config.toml`,
//! and every hook invocation — in any repo, for any agent — resolves it
//! fresh. `ATOMIC_AGENT_IDENTITY` remains the per-process escape hatch and
//! takes precedence, and the active server profile's `agent_identity`
//! binding (written by `atomic identity agent create`) is the implicit
//! fallback when nothing explicit is set.
//!
//! Resolution order (shared with the hook handler, see
//! [`resolve_effective_agent_identity`]):
//!
//! ```text
//! ATOMIC_AGENT_IDENTITY env var
//!   > global agent_identity setting
//!   > active server profile's agent_identity
//!   > plus-tag fallback (behavior from before agent identities existed)
//! ```
//!
//! # Examples
//!
//! ```text
//! # Select an agent identity (machine-wide)
//! atomic agent identity set fred+opencode
//!
//! # Stop recording as the agent
//! atomic agent identity unset
//!
//! # See what hooks will sign as, and why
//! atomic agent identity show
//! ```

use clap::{Args, Subcommand};

use atomic_canonical::delegation as cert;
use atomic_identity::IdentityStore;

use atomic_config::GlobalConfig;

use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_success, print_warning};

// Identity Command

/// Select or inspect the agent identity that recording hooks sign as.
#[derive(Debug, Args)]
#[command(arg_required_else_help = true)]
pub struct Identity {
    #[command(subcommand)]
    command: IdentityCommands,
}

/// Available `atomic agent identity` subcommands.
#[derive(Debug, Subcommand)]
pub enum IdentityCommands {
    /// Select the agent identity for hook recording (machine-wide).
    ///
    /// Validates the identity eagerly — it must exist in the identity store
    /// and be an agent/delegated identity, since a human identity here
    /// would silently sign agent work with the human's key — then writes it
    /// to `~/.atomic/config.toml` as the global `agent_identity`. Every
    /// hooked agent on this machine records under it from the next hook
    /// invocation on.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic agent identity set fred+opencode
    /// ```
    Set(Set),

    /// Clear the agent identity selection.
    ///
    /// Hooks go back to recording as the plus-tag of the default identity —
    /// the behavior from before agent identities existed.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic agent identity unset
    /// ```
    Unset(Unset),

    /// Show the effective agent identity and where it comes from.
    ///
    /// Runs the same resolution the hooks use (env var, global setting,
    /// active server profile, or none), so what you see here is exactly
    /// what the next recorded turn will sign as.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic agent identity show
    /// ```
    Show(Show),
}

impl Command for Identity {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            IdentityCommands::Set(cmd) => cmd.run(),
            IdentityCommands::Unset(cmd) => cmd.run(),
            IdentityCommands::Show(cmd) => cmd.run(),
        }
    }
}

// Effective Identity Resolution

/// Where a resolved agent identity came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentIdentitySource {
    /// The `ATOMIC_AGENT_IDENTITY` environment variable.
    EnvVar,
    /// The global `agent_identity` setting in `~/.atomic/config.toml`.
    GlobalSetting,
    /// The active server profile's `agent_identity` binding.
    ServerProfile,
}

impl std::fmt::Display for AgentIdentitySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            AgentIdentitySource::EnvVar => "ATOMIC_AGENT_IDENTITY env var",
            AgentIdentitySource::GlobalSetting => "global setting (~/.atomic/config.toml)",
            AgentIdentitySource::ServerProfile => "active server profile",
        })
    }
}

impl AgentIdentitySource {
    /// Stable machine key for JSON output (`env`, `global`, `server-profile`).
    /// The human labels live in [`Display`](Self::fmt).
    pub fn key(&self) -> &'static str {
        match self {
            AgentIdentitySource::EnvVar => "env",
            AgentIdentitySource::GlobalSetting => "global",
            AgentIdentitySource::ServerProfile => "server-profile",
        }
    }
}

/// Resolve the agent identity that recording hooks should sign as.
///
/// Order: `ATOMIC_AGENT_IDENTITY` env var, else the global `agent_identity`
/// setting, else the active server profile's `agent_identity` binding.
/// Returns `None` when nothing is configured — hooks then record as the
/// plus-tag of the default identity, exactly as before agent identities
/// existed. Config read failures degrade to `None`: a hook must never fail
/// over identity selection.
pub fn resolve_effective_agent_identity() -> Option<(String, AgentIdentitySource)> {
    // 1. Env var — the per-process escape hatch, and the only handle a
    //    remote runner has.
    let env = std::env::var(atomic_agent::identity::AGENT_IDENTITY_ENV)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());

    // An unreadable config degrades to no selection rather than failing
    // the hook.
    let config = GlobalConfig::load().ok()?;
    resolve_from_parts(env.as_deref(), &config)
}

/// Pure resolution over the chain, split from the I/O so the order is
/// unit-testable.
///
/// Env var beats the global setting beats the active server profile; empty
/// or whitespace-only values are skipped as if unset at every level.
fn resolve_from_parts(
    env: Option<&str>,
    config: &GlobalConfig,
) -> Option<(String, AgentIdentitySource)> {
    if let Some(name) = env.map(str::trim).filter(|n| !n.is_empty()) {
        return Some((name.to_string(), AgentIdentitySource::EnvVar));
    }
    if let Some(name) = config.agent_identity.as_deref().map(str::trim) {
        if !name.is_empty() {
            return Some((name.to_string(), AgentIdentitySource::GlobalSetting));
        }
    }
    if let Some(name) = config.active_server_agent_identity().map(str::trim) {
        if !name.is_empty() {
            return Some((name.to_string(), AgentIdentitySource::ServerProfile));
        }
    }
    None
}

// Set Command

/// Select the agent identity for hook recording.
#[derive(Debug, Args)]
pub struct Set {
    /// Name of the agent identity to sign as (e.g., "fred+opencode").
    name: String,
}

impl Command for Set {
    fn run(&self) -> CliResult<()> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err(CliError::InvalidArgument {
                message: "Identity name cannot be empty".to_string(),
            });
        }

        // Eager validation: the record path falls back softly on a bad
        // name, so this is the one place with room to say what's wrong.
        let store = IdentityStore::open_default().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to open identity store: {e}"))
        })?;
        let identity = store
            .load_by_name(name)
            .map_err(|_| CliError::IdentityNotFound(name.to_string()))?;

        // A human identity here would silently sign agent work as the
        // human with no delegation behind it — refuse it outright.
        if !identity.identity_type.is_delegated() && !identity.identity_type.is_agent() {
            return Err(CliError::InvalidArgument {
                message: format!(
                    "'{name}' is a human identity. Agent hooks can only sign as an agent \
                     identity — create one with `atomic identity agent create`."
                ),
            });
        }

        // Keyed attribution without a certificate still records (the key is
        // the agent's), but the envelope will carry no delegation URN and a
        // server cannot confirm the delegation. That is worth saying now,
        // not discovering from a change header later.
        if cert::active_for_delegate(&store, &identity).is_none() {
            print_warning(&format!(
                "No active delegation certificate for '{name}'. Turns will record keyed to \
                 the agent, but the envelope will carry no delegation URN until a \
                 certificate is granted (`atomic identity grant new {name}`)."
            ));
        }

        let mut config = GlobalConfig::load().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to load global config: {e}"))
        })?;
        config.agent_identity = Some(name.to_string());
        config.save().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to save global config: {e}"))
        })?;

        print_success(&format!(
            "Agent identity set to '{name}' — recording hooks now sign as it"
        ));
        print_hint("Effective immediately for every hooked agent on this machine");
        Ok(())
    }
}

// Unset Command

/// Clear the agent identity selection.
#[derive(Debug, Args)]
pub struct Unset {}

impl Command for Unset {
    fn run(&self) -> CliResult<()> {
        let mut config = GlobalConfig::load().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to load global config: {e}"))
        })?;
        let had = config.agent_identity.is_some();
        config.agent_identity = None;
        config.save().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to save global config: {e}"))
        })?;

        if had {
            print_success("Agent identity unset — hooks record as the plus-tag of the default identity");
        } else {
            print_hint("No agent identity was set");
        }
        Ok(())
    }
}

// Show Command

/// Show the effective agent identity and where it comes from.
#[derive(Debug, Args)]
pub struct Show {}

impl Command for Show {
    fn run(&self) -> CliResult<()> {
        match resolve_effective_agent_identity() {
            Some((name, source)) => {
                print_success(&format!("Agent identity: {name}"));
                println!("  Source:    {source}");
                // The record path falls back softly when the name does not
                // resolve — show should say so, because that is a silent
                // difference between what is selected and what records.
                match IdentityStore::open_default() {
                    Ok(store) => match store.load_by_name(&name) {
                        Ok(identity) => {
                            println!("  Email:     {}", identity.email.as_deref().unwrap_or("-"));
                            println!("  Key:       {}", identity.public_key_base32());
                            match cert::active_for_delegate(&store, &identity) {
                                Some(d) => {
                                    println!("  Delegation: {}", d.delegation.id.to_urn());
                                }
                                None => {
                                    print_warning(
                                        "No active delegation certificate — turns record \
                                     keyed to the agent but carry no delegation URN",
                                    );
                                }
                            }
                        }
                        Err(_) => {
                            print_warning(&format!(
                                "'{name}' does not resolve in the identity store — \
                                 hooks will fall back to the plus-tag path"
                            ));
                        }
                    },
                    Err(e) => {
                        print_warning(&format!(
                            "Could not open identity store ({e}) — hooks will fall \
                             back to the plus-tag path if the identity stays unresolvable"
                        ));
                    }
                }
            }
            None => {
                print_hint(
                    "No agent identity selected — recording hooks sign as the \
                     plus-tag of the default identity",
                );
                print_hint("Select one with: atomic agent identity set <name>");
            }
        }
        Ok(())
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    // The env var and the real ~/.atomic/config.toml both sit outside this
    // test's control, so the chain is pinned against synthetic parts via
    // resolve_from_parts — the same function the hook handler reaches
    // through resolve_effective_agent_identity.
    #[test]
    fn env_var_beats_everything() {
        let config = GlobalConfig {
            agent_identity: Some("global+agent".to_string()),
            server: atomic_config::ServerConfig {
                agent_identity: Some("server+agent".to_string()),
                ..atomic_config::ServerConfig::default()
            },
            ..GlobalConfig::default()
        };
        let (name, source) =
            resolve_from_parts(Some("env+agent"), &config).expect("env var must win");
        assert_eq!(name, "env+agent");
        assert_eq!(source, AgentIdentitySource::EnvVar);
    }

    #[test]
    fn global_setting_beats_server_profile() {
        let config = GlobalConfig {
            agent_identity: Some("global+agent".to_string()),
            server: atomic_config::ServerConfig {
                agent_identity: Some("server+agent".to_string()),
                ..atomic_config::ServerConfig::default()
            },
            ..GlobalConfig::default()
        };
        let (name, source) =
            resolve_from_parts(None, &config).expect("global setting must beat profile");
        assert_eq!(name, "global+agent");
        assert_eq!(source, AgentIdentitySource::GlobalSetting);
    }

    #[test]
    fn server_profile_is_the_implicit_fallback() {
        let config = GlobalConfig {
            server: atomic_config::ServerConfig {
                agent_identity: Some("server+agent".to_string()),
                ..atomic_config::ServerConfig::default()
            },
            ..GlobalConfig::default()
        };
        let (name, source) =
            resolve_from_parts(None, &config).expect("profile binding must be used");
        assert_eq!(name, "server+agent");
        assert_eq!(source, AgentIdentitySource::ServerProfile);
    }

    #[test]
    fn nothing_set_means_no_selection() {
        // The backward-compatibility guarantee: with nothing configured,
        // there is no selection and hooks keep the plus-tag path.
        assert!(resolve_from_parts(None, &GlobalConfig::default()).is_none());
    }

    #[test]
    fn blank_values_are_skipped_at_every_level() {
        let config = GlobalConfig {
            agent_identity: Some("   ".to_string()),
            server: atomic_config::ServerConfig {
                agent_identity: Some("".to_string()),
                ..atomic_config::ServerConfig::default()
            },
            ..GlobalConfig::default()
        };
        assert!(resolve_from_parts(Some("  "), &config).is_none());

        // A blank global falls through to the profile binding.
        let config = GlobalConfig {
            server: atomic_config::ServerConfig {
                agent_identity: Some("server+agent".to_string()),
                ..atomic_config::ServerConfig::default()
            },
            ..GlobalConfig::default()
        };
        let (name, source) = resolve_from_parts(None, &config).unwrap();
        assert_eq!(name, "server+agent");
        assert_eq!(source, AgentIdentitySource::ServerProfile);
    }

    #[test]
    fn source_labels_are_stable() {
        assert_eq!(
            AgentIdentitySource::EnvVar.to_string(),
            "ATOMIC_AGENT_IDENTITY env var"
        );
        assert_eq!(
            AgentIdentitySource::GlobalSetting.to_string(),
            "global setting (~/.atomic/config.toml)"
        );
        assert_eq!(
            AgentIdentitySource::ServerProfile.to_string(),
            "active server profile"
        );
    }
}