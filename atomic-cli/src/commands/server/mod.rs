//! Server profile management commands.
//!
//! This module provides commands for managing named server profiles in
//! `~/.atomic/config.toml`. Server profiles let you switch between
//! environments (staging, production, local) with a single flag or by
//! setting a default.
//!
//! # Available Subcommands
//!
//! - `server` (no args) — List all configured server profiles
//! - `server add <name> <url>` — Add a new server profile
//! - `server set <name>` — Switch the active default server profile
//! - `server show [name]` — Show details for a profile
//! - `server remove <name>` — Remove a server profile
//! - `server set-identity <name> <identity>` — Bind an identity to a profile
//!
//! # Examples
//!
//! ```bash
//! # List server profiles
//! atomic server
//!
//! # Add a staging profile
//! atomic server add staging https://staging.atomic.storage --identity alice-staging
//!
//! # Switch to staging
//! atomic server set staging
//!
//! # Switch back to production (legacy [server] block)
//! atomic server set --default
//!
//! # Show the active profile
//! atomic server show
//!
//! # Bind an identity to an existing profile
//! atomic server set-identity prod alice-prod
//! ```

use clap::{Parser, Subcommand};

use atomic_config::{GlobalConfig, ServerConfig};

use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_success};

// Server Command

/// Manage named server profiles.
///
/// Server profiles let you store multiple atomic-storage server configurations
/// (e.g. staging and production) and switch between them easily. Each profile
/// can have its own URL, default org, and identity.
///
/// Profiles are stored under `[servers.*]` in `~/.atomic/config.toml`.
/// Management commands use the active profile (set with `atomic server set`)
/// unless you override it per-command with `--server <name>`.
#[derive(Debug, Clone, Parser)]
#[command(name = "server")]
pub struct ServerCmd {
    /// Subcommand to execute.
    #[command(subcommand)]
    pub command: Option<ServerSubcommand>,
}

/// Server subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum ServerSubcommand {
    /// Add a new server profile.
    #[command(name = "add")]
    Add(ServerAdd),

    /// Switch the active default server profile.
    ///
    /// Use `--default` to clear the named profile and revert to the legacy
    /// `[server]` block.
    #[command(name = "set")]
    Set(ServerSet),

    /// Show details for a server profile.
    ///
    /// Without a name, shows the currently active profile.
    #[command(name = "show")]
    Show(ServerShow),

    /// Remove a server profile.
    #[command(name = "remove", visible_alias = "rm")]
    Remove(ServerRemove),

    /// Bind an identity to a server profile.
    ///
    /// Management commands targeting this server will use the specified
    /// identity instead of the global default.
    #[command(name = "set-identity")]
    SetIdentity(ServerSetIdentity),
}

// Subcommand Structs

/// Add a new server profile.
#[derive(Debug, Clone, Parser)]
pub struct ServerAdd {
    /// Name for the new server profile (e.g. "staging", "prod").
    #[arg(value_name = "NAME")]
    pub name: String,

    /// Base URL of the atomic-storage server.
    #[arg(value_name = "URL")]
    pub url: String,

    /// Default organization slug for this server.
    #[arg(long)]
    pub org: Option<String>,

    /// Identity name to use when connecting to this server.
    #[arg(long)]
    pub identity: Option<String>,

    /// Make this the active default server immediately.
    #[arg(long)]
    pub set_default: bool,
}

/// Switch the active default server profile.
#[derive(Debug, Clone, Parser)]
pub struct ServerSet {
    /// Name of the server profile to make active.
    ///
    /// Omit (with `--default`) to clear the named profile and revert to
    /// the legacy `[server]` block.
    #[arg(value_name = "NAME", required_unless_present = "revert")]
    pub name: Option<String>,

    /// Revert to the legacy `[server]` block (clear `default_server`).
    #[arg(long = "default", conflicts_with = "name")]
    pub revert: bool,
}

/// Show details for a server profile.
#[derive(Debug, Clone, Parser)]
pub struct ServerShow {
    /// Name of the profile to show. Defaults to the active profile.
    #[arg(value_name = "NAME")]
    pub name: Option<String>,
}

/// Remove a server profile.
#[derive(Debug, Clone, Parser)]
pub struct ServerRemove {
    /// Name of the server profile to remove.
    #[arg(value_name = "NAME")]
    pub name: String,
}

/// Bind an identity to a server profile.
#[derive(Debug, Clone, Parser)]
pub struct ServerSetIdentity {
    /// Name of the server profile to update.
    #[arg(value_name = "NAME")]
    pub name: String,

    /// Identity name to bind. Use an empty string or `--clear` to remove.
    #[arg(value_name = "IDENTITY", required_unless_present = "clear")]
    pub identity: Option<String>,

    /// Remove the identity binding (revert to global default).
    #[arg(long, conflicts_with = "identity")]
    pub clear: bool,
}

// Implementation

impl ServerCmd {
    /// List all configured server profiles.
    fn list_servers(&self) -> CliResult<()> {
        let config = GlobalConfig::load()
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to load config: {}", e)))?;

        if config.servers.is_empty() {
            print_hint(
                "No named server profiles configured.\n  \
                 Use 'atomic server add <name> <url>' to add one.\n  \
                 The legacy [server] block is always available as the default.",
            );
            if config.server.is_configured() {
                println!();
                println!(
                    "  Active server (legacy): {}",
                    config.server.url.as_deref().unwrap_or("(none)")
                );
                if let Some(ref org) = config.server.default_org {
                    println!("  Default org:            {}", org);
                }
            }
            return Ok(());
        }

        let active = config.default_server.as_deref();

        for (name, profile) in &config.servers {
            let is_active = Some(name.as_str()) == active;
            let active_marker = if is_active { " *" } else { "" };
            let url = profile.url.as_deref().unwrap_or("(no url)");
            let org = profile
                .default_org
                .as_deref()
                .map(|o| format!(", org: {}", o))
                .unwrap_or_default();
            let identity = profile
                .identity
                .as_deref()
                .map(|i| format!(", identity: {}", i))
                .unwrap_or_default();

            println!("{}{}\t{}{}{}", name, active_marker, url, org, identity);
        }

        if active.is_none() {
            println!();
            print_hint("No named profile is active — using the legacy [server] block.");
            print_hint("Run 'atomic server set <name>' to activate a profile.");
        }

        Ok(())
    }

    /// Add a new server profile.
    fn add_server(&self, add: &ServerAdd) -> CliResult<()> {
        if !add.url.contains("://") {
            return Err(CliError::InvalidArgument {
                message: format!(
                    "Invalid URL '{}': must include a scheme (e.g. https://)",
                    add.url
                ),
            });
        }

        let mut config = GlobalConfig::load()
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to load config: {}", e)))?;

        if config.servers.contains_key(&add.name) {
            return Err(CliError::InvalidArgument {
                message: format!(
                    "Server profile '{}' already exists. \
                     Use 'atomic server set-identity' or 'atomic server remove' to modify it.",
                    add.name
                ),
            });
        }

        let profile = ServerConfig {
            url: Some(add.url.trim_end_matches('/').to_string()),
            default_org: add.org.clone(),
            default_workspaces: std::collections::BTreeMap::new(),
            identity: add.identity.clone(),
            // Auto-detected at registration; manual profiles default to
            // multi-tenant URL semantics.
            single_tenant: false,
        };

        config.servers.insert(add.name.clone(), profile);

        if add.set_default {
            config.default_server = Some(add.name.clone());
        }

        config
            .save()
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to save config: {}", e)))?;

        print_success(&format!("Server profile '{}' added", add.name));
        println!("  URL:      {}", add.url);
        if let Some(ref org) = add.org {
            println!("  Org:      {}", org);
        }
        if let Some(ref identity) = add.identity {
            println!("  Identity: {}", identity);
        }
        if add.set_default {
            println!();
            print_success(&format!("'{}' is now the active server", add.name));
        } else {
            println!();
            print_hint(&format!(
                "To make this your default: atomic server set {}",
                add.name
            ));
        }

        Ok(())
    }

    /// Switch the active default server profile.
    fn set_server(&self, set: &ServerSet) -> CliResult<()> {
        let mut config = GlobalConfig::load()
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to load config: {}", e)))?;

        if set.revert {
            config.default_server = None;
            config
                .save()
                .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to save config: {}", e)))?;
            print_success("Reverted to legacy [server] block");
            return Ok(());
        }

        let name = set.name.as_deref().unwrap();

        if !config.servers.contains_key(name) {
            return Err(CliError::InvalidArgument {
                message: format!(
                    "Server profile '{}' not found.\n  \
                     Use 'atomic server add {} <url>' to create it.",
                    name, name
                ),
            });
        }

        config.default_server = Some(name.to_string());
        config
            .save()
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to save config: {}", e)))?;

        print_success(&format!("Active server is now '{}'", name));

        let profile = &config.servers[name];
        if let Some(ref url) = profile.url {
            println!("  URL:      {}", url);
        }
        if let Some(ref org) = profile.default_org {
            println!("  Org:      {}", org);
        }
        if let Some(ref identity) = profile.identity {
            println!("  Identity: {}", identity);
        }

        Ok(())
    }

    /// Show details for a server profile.
    fn show_server(&self, show: &ServerShow) -> CliResult<()> {
        let config = GlobalConfig::load()
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to load config: {}", e)))?;

        let (name, profile) = if let Some(ref n) = show.name {
            // Explicit name
            let p = config
                .servers
                .get(n)
                .ok_or_else(|| CliError::InvalidArgument {
                    message: format!("Server profile '{}' not found.", n),
                })?;
            (n.as_str(), p)
        } else if let Some(ref active) = config.default_server {
            // Active named profile
            let p = config
                .servers
                .get(active)
                .ok_or_else(|| CliError::InvalidArgument {
                    message: format!("Default server profile '{}' not found in config.", active),
                })?;
            (active.as_str(), p)
        } else {
            // Legacy block — show it directly
            println!("Active server: (legacy [server] block)");
            println!(
                "  URL:      {}",
                config.server.url.as_deref().unwrap_or("(not configured)")
            );
            println!(
                "  Org:      {}",
                config
                    .server
                    .default_org
                    .as_deref()
                    .unwrap_or("(not configured)")
            );
            if let Some(ref identity) = config.server.identity {
                println!("  Identity: {}", identity);
            } else {
                println!("  Identity: (global default)");
            }
            if !config.server.default_workspaces.is_empty() {
                println!("  Workspaces:");
                for (org, ws) in &config.server.default_workspaces {
                    println!("    {} → {}", org, ws);
                }
            }
            return Ok(());
        };

        let is_active = config.default_server.as_deref() == Some(name);
        println!(
            "Server profile: {}{}",
            name,
            if is_active { " (active)" } else { "" }
        );
        println!(
            "  URL:      {}",
            profile.url.as_deref().unwrap_or("(not configured)")
        );
        println!(
            "  Org:      {}",
            profile.default_org.as_deref().unwrap_or("(not configured)")
        );
        if let Some(ref identity) = profile.identity {
            println!("  Identity: {}", identity);
        } else {
            println!("  Identity: (global default)");
        }
        if !profile.default_workspaces.is_empty() {
            println!("  Workspaces:");
            for (org, ws) in &profile.default_workspaces {
                println!("    {} → {}", org, ws);
            }
        }

        Ok(())
    }

    /// Remove a server profile.
    fn remove_server(&self, remove: &ServerRemove) -> CliResult<()> {
        let mut config = GlobalConfig::load()
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to load config: {}", e)))?;

        if config.servers.remove(&remove.name).is_none() {
            return Err(CliError::InvalidArgument {
                message: format!("Server profile '{}' not found.", remove.name),
            });
        }

        // Clear default_server if it pointed to the removed profile
        if config.default_server.as_deref() == Some(&remove.name) {
            config.default_server = None;
            println!(
                "Note: '{}' was the active server. Reverted to legacy [server] block.",
                remove.name
            );
        }

        config
            .save()
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to save config: {}", e)))?;

        print_success(&format!("Server profile '{}' removed", remove.name));
        Ok(())
    }

    /// Bind (or clear) an identity for a server profile.
    fn set_identity_for_server(&self, cmd: &ServerSetIdentity) -> CliResult<()> {
        let mut config = GlobalConfig::load()
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to load config: {}", e)))?;

        let profile =
            config
                .servers
                .get_mut(&cmd.name)
                .ok_or_else(|| CliError::InvalidArgument {
                    message: format!(
                        "Server profile '{}' not found. \
                     Use 'atomic server add {} <url>' to create it.",
                        cmd.name, cmd.name
                    ),
                })?;

        if cmd.clear {
            profile.identity = None;
            config
                .save()
                .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to save config: {}", e)))?;
            print_success(&format!(
                "Identity cleared for server profile '{}'",
                cmd.name
            ));
            print_hint("Management commands will now use the global default identity.");
        } else {
            let identity_name = cmd.identity.as_deref().unwrap();
            profile.identity = Some(identity_name.to_string());
            config
                .save()
                .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to save config: {}", e)))?;
            print_success(&format!(
                "Identity '{}' bound to server profile '{}'",
                identity_name, cmd.name
            ));
        }

        Ok(())
    }
}

impl Command for ServerCmd {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            None => self.list_servers(),
            Some(ServerSubcommand::Add(add)) => self.add_server(add),
            Some(ServerSubcommand::Set(set)) => self.set_server(set),
            Some(ServerSubcommand::Show(show)) => self.show_server(show),
            Some(ServerSubcommand::Remove(remove)) => self.remove_server(remove),
            Some(ServerSubcommand::SetIdentity(cmd)) => self.set_identity_for_server(cmd),
        }
    }
}
