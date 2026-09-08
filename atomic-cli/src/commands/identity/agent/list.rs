//! `atomic identity agent list` — agents and the state of their delegations.

use clap::Parser;
use serde_json::json;

use atomic_identity::delegation::DelegationStatus;
use atomic_identity::{IdentityStore, IdentityType};

use crate::commands::delegation::{load_for_delegate, ResolvedDelegation};
use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::print_hint;

use super::humanize_remaining;

/// List agent identities and their delegations.
#[derive(Debug, Parser)]
pub struct List {
    /// Include agents whose delegation has expired or been revoked.
    #[arg(long)]
    pub include_expired: bool,

    /// Output as JSON.
    #[arg(long)]
    pub json: bool,
}

impl Command for List {
    fn run(&self) -> CliResult<()> {
        let store = IdentityStore::open_default().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to open identity store: {e}"))
        })?;

        let identities = store
            .list()
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to list identities: {e}")))?;

        let mut rows = Vec::new();
        for identity in identities {
            if !matches!(
                identity.identity_type,
                IdentityType::Agent | IdentityType::Delegated
            ) {
                continue;
            }
            // The freshest certificate is the one that governs; older ones are
            // history and would only add noise to a list.
            let newest = load_for_delegate(&store, &identity)?.into_iter().next();
            let status = newest
                .as_ref()
                .map(|d| d.status)
                .unwrap_or(DelegationStatus::Revoked);

            if !self.include_expired && status != DelegationStatus::Active {
                continue;
            }
            rows.push((identity, newest, status));
        }

        if self.json {
            let payload: Vec<_> = rows
                .iter()
                .map(|(identity, delegation, status)| {
                    json!({
                        "name": identity.name,
                        "did": identity.id.to_did(),
                        "publicKey": identity.public_key.to_did_key(),
                        "type": super::super::format_identity_type(&identity.identity_type),
                        "status": status.to_string(),
                        "delegation": delegation.as_ref().map(describe_delegation),
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&payload).unwrap());
            return Ok(());
        }

        if rows.is_empty() {
            if self.include_expired {
                println!("No agent identities.");
            } else {
                println!("No active agent identities.");
                print_hint("Use --include-expired to see withdrawn or lapsed ones");
            }
            println!();
            println!(
                "  {}  Create one",
                crate::output::command("atomic identity agent create <name>")
            );
            return Ok(());
        }

        println!(
            "{:<24} {:<14} {:<22} {:<18} {:<9} STATUS",
            "NAME", "AGENT", "CAN", "ON", "EXPIRES"
        );
        for (identity, delegation, status) in &rows {
            let (can, on, expires) = match delegation {
                Some(d) => (
                    summarize(
                        &d.delegation
                            .scope
                            .permissions
                            .iter()
                            .map(|p| p.to_string())
                            .collect::<Vec<_>>(),
                        22,
                    ),
                    summarize(&d.delegation.scope.projects, 18),
                    humanize_remaining(d.delegation.time_remaining()),
                ),
                None => ("-".to_string(), "-".to_string(), "-".to_string()),
            };
            let agent_kind = delegation
                .as_ref()
                .and_then(|d| d.delegation.software_agent.clone())
                .map(|urn| {
                    urn.trim_start_matches(atomic_canonical::delegation::AGENT_URN_PREFIX)
                        .to_string()
                })
                .unwrap_or_else(|| "agent".to_string());

            println!(
                "{:<24} {:<14} {:<22} {:<18} {:<9} {}",
                identity.name, agent_kind, can, on, expires, status
            );
        }

        Ok(())
    }
}

/// The JSON view of a delegation for `--json`.
fn describe_delegation(resolved: &ResolvedDelegation) -> serde_json::Value {
    let d = &resolved.delegation;
    json!({
        "id": d.id.to_urn(),
        "delegator": d.delegator,
        "delegatorName": d.delegator_name,
        "softwareAgent": d.software_agent,
        "permissions": d.scope.permissions.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
        "servers": d.scope.servers,
        "workspaces": d.scope.workspaces,
        "projects": d.scope.projects,
        "views": d.scope.views,
        "maxChanges": d.scope.max_changes,
        "issued": d.issued.to_rfc3339(),
        "expires": d.expires.map(|e| e.to_rfc3339()),
        "status": resolved.status.to_string(),
    })
}

/// Render a list into a fixed width, eliding the tail as `first,+n`.
///
/// Truncating mid-word would leave a project name that looks real but is not;
/// `+2` at least says how much is hidden and sends the reader to `show`.
fn summarize(items: &[String], width: usize) -> String {
    if items.is_empty() {
        return "(all)".to_string();
    }
    let joined = items.join(",");
    if joined.len() <= width {
        return joined;
    }
    let mut out = String::new();
    let mut shown = 0;
    for item in items {
        let candidate = if out.is_empty() {
            item.clone()
        } else {
            format!("{out},{item}")
        };
        // Leave room for the ",+n" suffix.
        if candidate.len() + 4 > width {
            break;
        }
        out = candidate;
        shown += 1;
    }
    if shown == 0 {
        // Even one entry does not fit; show what we can rather than nothing.
        return items[0].chars().take(width).collect();
    }
    format!("{out},+{}", items.len() - shown)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_scope_reads_as_unrestricted() {
        assert_eq!(summarize(&[], 20), "(all)");
    }

    #[test]
    fn a_short_list_is_shown_whole() {
        assert_eq!(summarize(&["read".into(), "push".into()], 20), "read,push");
    }

    #[test]
    fn a_long_list_says_how_much_is_hidden() {
        let items: Vec<String> = vec!["acme/api".into(), "acme/web".into(), "acme/docs".into()];
        let out = summarize(&items, 18);
        assert!(out.ends_with("+2") || out.ends_with("+1"), "{out}");
        assert!(out.len() <= 18, "{out} is {} chars", out.len());
    }

    #[test]
    fn a_single_oversized_entry_is_clipped_rather_than_dropped() {
        let items = vec!["a-really-long-project-name/that-does-not-fit".to_string()];
        let out = summarize(&items, 10);
        assert_eq!(out.len(), 10);
    }
}
