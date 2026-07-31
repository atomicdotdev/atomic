//! The `workspace grant` command for managing workspace permission grants.
//!
//! Grants express fine-grained access control by binding a subject (user or
//! team) to a relation (read, write, admin) on a workspace. This module
//! provides three subcommands:
//!
//! - `atomic workspace grant list <workspace>` — list all grants
//! - `atomic workspace grant add <workspace> --team <slug> --permission write`
//! - `atomic workspace grant remove <workspace> --team <slug>`
//!
//! The `--permission` flag uses the user-facing name for what the API calls
//! `relation` — we deliberately avoid leaking Zanzibar terminology into the
//! CLI, and keep `--role` reserved for the existing role-bundle vocabularies
//! (owner/admin/member, maintainer/contributor).
//!
//! Team slugs are resolved to UUIDs via `GET /teams` (the same resolution flow
//! as `team member add`). Remove accepts `--team` and resolves the grant
//! internally via the DELETE-by-subject endpoint, so users never handle UUIDs.
//!
//! # Usage
//!
//! ```text
//! atomic workspace grant <SUBCOMMAND>
//!
//! Subcommands:
//!   list    List all grants on a workspace
//!   add     Grant a team or user access to a workspace
//!   remove  Revoke a team's or user's access to a workspace
//! ```

use clap::{Parser, Subcommand};

use crate::commands::client::build_client;
use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_success, Alignment, Column, Table};

/// Map a `TeamsError` to a `CliError` (same pattern as org member commands).
fn teams_err(e: atomic_teams::TeamsError) -> CliError {
    CliError::RemoteError {
        message: e.to_string(),
        url: None,
    }
}

/// Manage workspace permission grants.
#[derive(Debug, clap::Args)]
#[command(name = "grant")]
pub struct GrantCmd {
    #[command(subcommand)]
    pub command: GrantSubcommand,
}

/// Available grant subcommands.
#[derive(Subcommand, Debug)]
pub enum GrantSubcommand {
    /// List all permission grants on a workspace.
    List(GrantList),
    /// Grant a team or user access to a workspace.
    Add(GrantAdd),
    /// Revoke a team's or user's access to a workspace.
    Remove(GrantRemove),
}

impl Command for GrantCmd {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            GrantSubcommand::List(cmd) => cmd.run(),
            GrantSubcommand::Add(cmd) => cmd.run(),
            GrantSubcommand::Remove(cmd) => cmd.run(),
        }
    }
}

// ---------------------------------------------------------------------------
// grant list
// ---------------------------------------------------------------------------

/// List all permission grants on a workspace.
#[derive(Debug, Parser)]
#[command(name = "list")]
pub struct GrantList {
    /// Slug of the workspace.
    #[arg(required = true)]
    pub workspace: String,

    /// Organization override.
    #[arg(long)]
    pub org: Option<String>,

    /// Output format: `table` or `json`.
    #[arg(long, default_value = "table")]
    pub format: String,
}

impl Command for GrantList {
    fn run(&self) -> CliResult<()> {
        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| CliError::Internal(anyhow::anyhow!("{}", e)))?;

        rt.block_on(async {
            let client = build_client(self.org.as_deref(), None).await?;
            let grants = atomic_teams::grant::list_workspace_grants(&client, &self.workspace)
                .await
                .map_err(teams_err)?;

            if self.format == "json" {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&grants).unwrap_or_default()
                );
                return Ok(());
            }

            if grants.is_empty() {
                println!("No grants on workspace '{}'.", self.workspace);
                return Ok(());
            }

            let mut table = Table::new();
            table.set_columns(vec![
                Column::new("SUBJECT_TYPE").min_width(8),
                Column::new("SUBJECT_ID").min_width(10),
                Column::new("PERMISSION").min_width(8),
                Column::new("GRANTED_BY").min_width(10),
                Column::new("GRANTED_AT").min_width(10),
            ]);

            for g in &grants {
                table.add_row(vec![
                    g.subject_type.to_string(),
                    g.subject_id
                        .map(|id| id.to_string())
                        .unwrap_or_else(|| "everyone".to_string()),
                    g.relation.to_string(),
                    g.granted_by
                        .map(|id| id.to_string())
                        .unwrap_or_else(|| "—".to_string()),
                    crate::commands::format_timestamp(&g.granted_at),
                ]);
            }

            println!("{table}");
            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------
// grant add
// ---------------------------------------------------------------------------

/// Grant a team or user access to a workspace.
#[derive(Debug, Parser)]
#[command(name = "add")]
pub struct GrantAdd {
    /// Slug of the workspace to grant access to.
    #[arg(required = true)]
    pub workspace: String,

    /// Team slug to grant access to.
    ///
    /// The slug is resolved to a team UUID via `GET /teams`. Either `--team`
    /// or `--user` must be specified.
    #[arg(long)]
    pub team: Option<String>,

    /// User identity ID (UUID) to grant access to.
    ///
    /// Either `--team` or `--user` must be specified.
    #[arg(long)]
    pub user: Option<String>,

    /// Permission level: `read`, `write`, or `admin`.
    ///
    /// This is the user-facing name for what the API calls `relation`. We
    /// avoid the Zanzibar term to keep the CLI accessible.
    #[arg(long, required = true)]
    pub permission: String,

    /// Organization override.
    #[arg(long)]
    pub org: Option<String>,
}

impl Command for GrantAdd {
    fn run(&self) -> CliResult<()> {
        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| CliError::Internal(anyhow::anyhow!("{}", e)))?;

        rt.block_on(async {
            let client = build_client(self.org.as_deref(), None).await?;

            let (subject_type, subject_id) = resolve_subject(&client, &self.team, &self.user)
                .await?;
            let relation = parse_permission(&self.permission)?;

            let grant =
                atomic_teams::grant::add_workspace_grant(&client, &self.workspace, subject_type, subject_id, relation)
                    .await
                    .map_err(teams_err)?;

            print_success(&format!(
                "Granted {} access to {} on workspace '{}'",
                grant.relation,
                subject_label(subject_type, &self.team, &self.user),
                self.workspace,
            ));

            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------
// grant remove
// ---------------------------------------------------------------------------

/// Revoke a team's or user's access to a workspace.
#[derive(Debug, Parser)]
#[command(name = "remove")]
pub struct GrantRemove {
    /// Slug of the workspace to revoke access from.
    #[arg(required = true)]
    pub workspace: String,

    /// Team slug to revoke access from.
    ///
    /// The slug is resolved to a team UUID via `GET /teams`. Either `--team`
    /// or `--user` must be specified.
    #[arg(long)]
    pub team: Option<String>,

    /// User identity ID (UUID) to revoke access from.
    ///
    /// Either `--team` or `--user` must be specified.
    #[arg(long)]
    pub user: Option<String>,

    /// Organization override.
    #[arg(long)]
    pub org: Option<String>,
}

impl Command for GrantRemove {
    fn run(&self) -> CliResult<()> {
        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| CliError::Internal(anyhow::anyhow!("{}", e)))?;

        rt.block_on(async {
            let client = build_client(self.org.as_deref(), None).await?;

            let (subject_type, subject_id) = resolve_subject(&client, &self.team, &self.user)
                .await?;

            atomic_teams::grant::revoke_workspace_grant(
                &client,
                &self.workspace,
                subject_type,
                subject_id,
            )
            .await
            .map_err(teams_err)?;

            print_success(&format!(
                "Revoked all grants for {} on workspace '{}'",
                subject_label(subject_type, &self.team, &self.user),
                self.workspace,
            ));

            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve a team slug or user UUID to a `(GrantSubjectType, Uuid)` pair.
///
/// Exactly one of `team_slug` or `user_id` must be `Some`.
async fn resolve_subject(
    client: &atomic_remote::StorageClient,
    team_slug: &Option<String>,
    user_id: &Option<String>,
) -> CliResult<(atomic_teams::GrantSubjectType, uuid::Uuid)> {
    match (team_slug, user_id) {
        (Some(slug), None) => {
            let teams: Vec<atomic_teams::TeamInfo> =
                atomic_teams::team::list_teams(client, client.org_slug())
                    .await
                    .map_err(teams_err)?;

            let team = teams
                .iter()
                .find(|t| t.slug == *slug)
                .ok_or_else(|| CliError::InvalidArgument {
                    message: format!("Team '{}' not found in org '{}'.", slug, client.org_slug()),
                })?;

            Ok((atomic_teams::GrantSubjectType::Team, team.id))
        }
        (None, Some(id)) => {
            let uuid = uuid::Uuid::parse_str(id).map_err(|_| CliError::InvalidArgument {
                message: format!("'{}' is not a valid UUID.", id),
            })?;
            Ok((atomic_teams::GrantSubjectType::User, uuid))
        }
        (Some(_), Some(_)) => Err(CliError::InvalidArgument {
            message: "Specify either --team or --user, not both.".to_string(),
        }),
        (None, None) => Err(CliError::InvalidArgument {
            message: "Specify --team <slug> or --user <uuid>.".to_string(),
        }),
    }
}

/// Parse the user-facing `--permission` flag into a `GrantRelation`.
///
/// Workspace grants only accept `read`, `write`, and `admin` — `owner` is an
/// org-level relation and is rejected here.
fn parse_permission(s: &str) -> CliResult<atomic_teams::GrantRelation> {
    match s.to_lowercase().as_str() {
        "read" => Ok(atomic_teams::GrantRelation::Read),
        "write" => Ok(atomic_teams::GrantRelation::Write),
        "admin" => Ok(atomic_teams::GrantRelation::Admin),
        other => Err(CliError::InvalidArgument {
            message: format!(
                "Invalid permission '{other}'.\n  \
                 Valid values: read, write, admin."
            ),
        }),
    }
}

/// Build a human-readable label for the subject, used in success messages.
fn subject_label(
    subject_type: atomic_teams::GrantSubjectType,
    team_slug: &Option<String>,
    user_id: &Option<String>,
) -> String {
    match subject_type {
        atomic_teams::GrantSubjectType::Team => {
            format!("team '{}'", team_slug.as_deref().unwrap_or("?"))
        }
        atomic_teams::GrantSubjectType::User => {
            format!("user {}", user_id.as_deref().unwrap_or("?"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_permission_valid_values() {
        assert_eq!(
            parse_permission("read").unwrap(),
            atomic_teams::GrantRelation::Read
        );
        assert_eq!(
            parse_permission("write").unwrap(),
            atomic_teams::GrantRelation::Write
        );
        assert_eq!(
            parse_permission("admin").unwrap(),
            atomic_teams::GrantRelation::Admin
        );
    }

    #[test]
    fn parse_permission_case_insensitive() {
        assert_eq!(
            parse_permission("READ").unwrap(),
            atomic_teams::GrantRelation::Read
        );
        assert_eq!(
            parse_permission("Write").unwrap(),
            atomic_teams::GrantRelation::Write
        );
    }

    #[test]
    fn parse_permission_rejects_owner() {
        let err = parse_permission("owner").unwrap_err();
        match err {
            CliError::InvalidArgument { message } => {
                assert!(message.contains("Valid values: read, write, admin"));
            }
            other => panic!("expected InvalidArgument, got {:?}", other),
        }
    }

    #[test]
    fn parse_permission_rejects_garbage() {
        let err = parse_permission("superuser").unwrap_err();
        match err {
            CliError::InvalidArgument { message } => {
                assert!(message.contains("Invalid permission"));
            }
            other => panic!("expected InvalidArgument, got {:?}", other),
        }
    }
}
