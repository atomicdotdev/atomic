//! Agent identity management — `atomic identity agent`.
//!
//! An agent identity is a keypair of its own, bound to a human by a signed
//! delegation certificate. This module is the porcelain over that: one command
//! to issue one, and the lifecycle commands to inspect, renew and withdraw it.
//!
//! The plumbing each of these composes is separately available —
//! `atomic identity new --delegated-by`, `atomic identity delegate`,
//! `atomic identity delegation push` — for the cases the porcelain does not
//! cover, notably enrolling a key generated on a machine the human never
//! touches.
//!
//! # Usage
//!
//! ```text
//! atomic identity agent <COMMAND>
//!
//! Commands:
//!   create   Create an agent identity and delegate to it
//!   list     List agent identities and their delegations
//!   show     Show one agent's identity and delegation in full
//!   renew    Issue a fresh certificate for an existing agent key
//!   revoke   Withdraw an agent's delegation
//! ```
//!
//! # Example
//!
//! ```text
//! $ atomic identity agent create claude \
//!     --agent-type claude-code \
//!     --projects acme/api,acme/web \
//!     --can read,record,push \
//!     --expires 30d
//! ```

pub mod create;
pub mod list;
pub mod renew;
pub mod revoke;
pub mod show;

pub use create::Create;
pub use list::List;
pub use renew::Renew;
pub use revoke::Revoke;
pub use show::Show;

use chrono::Duration;
use clap::Subcommand;

use atomic_identity::delegation::DelegationPermission;

use crate::commands::Command;
use crate::error::{CliError, CliResult};

/// Default lifetime of a delegation when the caller does not say.
///
/// Thirty days is the compromise the whole design leans on: agent secret keys
/// sit unencrypted at `0600` (the store's password path is an unimplemented
/// `TODO`), so the defence is that a leaked key stops working soon and costs
/// one command to replace. Shorter is safer and more annoying; renewal is
/// `atomic identity agent renew`.
pub const DEFAULT_EXPIRY_DAYS: i64 = 30;

/// Agent identity commands.
#[derive(Debug, clap::Args)]
pub struct Agent {
    /// The agent subcommand to run.
    #[command(subcommand)]
    pub command: AgentCommands,
}

/// Available agent subcommands.
#[derive(Debug, Subcommand)]
pub enum AgentCommands {
    /// Create an agent identity and delegate to it.
    ///
    /// Generates a keypair, creates a delegated identity, signs a certificate
    /// with your key, enrolls it with the server, and binds it in config so
    /// hooks pick it up. One command, because every one of those steps is
    /// useless without the others.
    ///
    /// # Examples
    ///
    /// ```text
    /// # An agent that can record and push to two projects for 30 days
    /// atomic identity agent create claude \
    ///     --agent-type claude-code \
    ///     --projects acme/api,acme/web \
    ///     --can read,record,push
    ///
    /// # A read-only agent, no server enrollment
    /// atomic identity agent create reviewer --can read --local
    /// ```
    Create(Create),

    /// List agent identities and the state of their delegations.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic identity agent list
    /// atomic identity agent list --include-expired
    /// atomic identity agent list --json
    /// ```
    List(List),

    /// Show one agent in full: identity, certificate, scope, status.
    Show(Show),

    /// Reissue an agent's current scope with a new expiry.
    ///
    /// A convenience over `atomic identity grant new`: it carries the existing
    /// `--can` and `--projects` forward so you need only say how long. The key
    /// does not change, so nothing is re-enrolled, and like every issuance it
    /// reaches no server.
    Renew(Renew),

    /// Withdraw an agent's authority.
    ///
    /// Bumps the agent's epoch — killing every grant issued to it so far,
    /// including ones this machine has never seen — and deny-lists the grants
    /// it does know, each with a signed revocation.
    ///
    /// Unlike issuing, this must reach the server: a credential the holder
    /// possesses cannot prove its own withdrawal. The identity and its past
    /// work stay, so attribution for changes already recorded does not
    /// evaporate.
    Revoke(Revoke),
}

impl Command for Agent {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            AgentCommands::Create(cmd) => cmd.run(),
            AgentCommands::List(cmd) => cmd.run(),
            AgentCommands::Show(cmd) => cmd.run(),
            AgentCommands::Renew(cmd) => cmd.run(),
            AgentCommands::Revoke(cmd) => cmd.run(),
        }
    }
}

// ---------------------------------------------------------------------------
// Shared argument parsing
// ---------------------------------------------------------------------------

/// Parse a comma-separated permission list (`read,record,push`).
pub fn parse_permissions(raw: &str) -> CliResult<Vec<DelegationPermission>> {
    let mut out = Vec::new();
    for part in raw.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let permission: DelegationPermission =
            part.parse()
                .map_err(|e: String| CliError::InvalidArgument {
                    message: format!("--can: {e}"),
                })?;
        if !out.contains(&permission) {
            out.push(permission);
        }
    }
    if out.is_empty() {
        return Err(CliError::InvalidArgument {
            message: "--can needs at least one permission (e.g. --can read,record,push)"
                .to_string(),
        });
    }
    Ok(out)
}

/// Parse a comma-separated pattern list, dropping blanks.
pub fn parse_patterns(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Parse a duration like `30d`, `12h`, `90m`, or a bare number of days.
pub fn parse_duration(raw: &str) -> CliResult<Duration> {
    let raw = raw.trim();
    let invalid = || CliError::InvalidArgument {
        message: format!(
            "--expires: '{raw}' is not a duration (try 30d, 12h, 90m, or a number of days)"
        ),
    };

    let (value, unit) = match raw.chars().last() {
        Some(c) if c.is_ascii_alphabetic() => (&raw[..raw.len() - 1], c),
        Some(_) => (raw, 'd'),
        None => return Err(invalid()),
    };

    let value: i64 = value.trim().parse().map_err(|_| invalid())?;
    if value <= 0 {
        return Err(CliError::InvalidArgument {
            message: "--expires must be a positive duration; a delegation that is already \
                      expired would authorize nothing"
                .to_string(),
        });
    }

    match unit.to_ascii_lowercase() {
        'd' => Ok(Duration::days(value)),
        'h' => Ok(Duration::hours(value)),
        'm' => Ok(Duration::minutes(value)),
        'w' => Ok(Duration::weeks(value)),
        _ => Err(invalid()),
    }
}

/// The `urn:atomic:agent:<slug>` label for a software agent.
///
/// A URN, never a `did:` — the label names a *kind* of software, not a key.
/// The agent's key has its own DID; conflating the two would put a
/// non-resolvable identifier where a DID is expected.
pub fn software_agent_urn(slug: &str) -> String {
    format!(
        "{}{}",
        atomic_canonical::delegation::AGENT_URN_PREFIX,
        slug.trim().to_lowercase()
    )
}

/// Render a duration as a short human phrase (`in 30d`, `3d ago`).
pub fn humanize_remaining(remaining: Option<Duration>) -> String {
    let Some(remaining) = remaining else {
        return "never".to_string();
    };
    let days = remaining.num_days();
    let hours = remaining.num_hours();

    if remaining.num_seconds() < 0 {
        let ago = -remaining;
        return if ago.num_days() > 0 {
            format!("{}d ago", ago.num_days())
        } else {
            format!("{}h ago", ago.num_hours().max(1))
        };
    }
    if days > 0 {
        format!("in {days}d")
    } else if hours > 0 {
        format!("in {hours}h")
    } else {
        "in <1h".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permissions_parse_and_dedupe() {
        let parsed = parse_permissions("read, record ,push,read").unwrap();
        assert_eq!(
            parsed,
            vec![
                DelegationPermission::Read,
                DelegationPermission::Record,
                DelegationPermission::Push
            ]
        );
    }

    #[test]
    fn an_unknown_permission_is_a_usage_error_not_a_silent_drop() {
        let err = parse_permissions("read,teleport").unwrap_err();
        assert!(err.to_string().contains("teleport"), "{err}");
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn an_empty_permission_list_is_rejected() {
        assert!(parse_permissions("").is_err());
        assert!(parse_permissions(" , ").is_err());
    }

    #[test]
    fn durations_accept_the_common_units() {
        assert_eq!(parse_duration("30d").unwrap(), Duration::days(30));
        assert_eq!(parse_duration("12h").unwrap(), Duration::hours(12));
        assert_eq!(parse_duration("90m").unwrap(), Duration::minutes(90));
        assert_eq!(parse_duration("2w").unwrap(), Duration::weeks(2));
        // A bare number means days, the unit anyone would assume.
        assert_eq!(parse_duration("7").unwrap(), Duration::days(7));
    }

    #[test]
    fn a_non_positive_duration_is_rejected_with_the_reason() {
        let err = parse_duration("0d").unwrap_err();
        assert!(err.to_string().contains("positive"), "{err}");
        assert!(parse_duration("-5d").is_err());
        assert!(parse_duration("soon").is_err());
    }

    #[test]
    fn patterns_split_and_trim() {
        assert_eq!(
            parse_patterns(" acme/api , acme/web ,"),
            vec!["acme/api".to_string(), "acme/web".to_string()]
        );
        assert!(parse_patterns("").is_empty());
    }

    #[test]
    fn software_agent_label_is_a_urn_not_a_did() {
        let urn = software_agent_urn("Claude-Code");
        assert_eq!(urn, "urn:atomic:agent:claude-code");
        assert!(!urn.starts_with("did:"));
    }

    #[test]
    fn remaining_time_reads_naturally_in_both_directions() {
        assert_eq!(humanize_remaining(None), "never");
        assert_eq!(humanize_remaining(Some(Duration::days(30))), "in 30d");
        assert_eq!(humanize_remaining(Some(Duration::hours(5))), "in 5h");
        assert_eq!(humanize_remaining(Some(Duration::days(-3))), "3d ago");
    }
}
