//! Project management commands for remote atomic-storage servers.
//!
//! Projects live inside workspaces and represent a single Atomic VCS repository
//! hosted on a remote server. Each project has a name, slug, default view, and
//! visibility setting.
//!
//! # Usage
//!
//! ```text
//! atomic project <COMMAND>
//!
//! Commands:
//!   list    List projects in a workspace
//!   create  Create a new remote project
//!   show    Show project details
//!   update  Update a project
//!   delete  Delete a project
//!   init    Initialize a local repo as a remote project
//!
//! Options:
//!   -h, --help  Print help information
//! ```
//!
//! # Slug Format
//!
//! Several subcommands accept a combined `workspace/project` slug to identify
//! a project. For example:
//!
//! ```text
//! atomic project show my-workspace/my-project
//! atomic project update my-workspace/my-project --name new-name
//! atomic project delete my-workspace/my-project --force
//! ```

pub mod create;
pub mod delete;
pub mod init;
pub mod list;
pub mod show;
pub mod update;

use clap::Subcommand;

use crate::commands::Command;
use crate::error::{CliError, CliResult};

/// Parse a combined `workspace/project` slug into its two components.
///
/// # Errors
///
/// Returns [`CliError::InvalidArgument`] if the input does not contain
/// exactly one `/` separator.
pub fn parse_project_path(path: &str) -> CliResult<(&str, &str)> {
    path.split_once('/')
        .ok_or_else(|| CliError::InvalidArgument {
            message: format!("Expected format 'workspace/project', got '{}'", path),
        })
}

// ---------------------------------------------------------------------------
// Subcommand dispatch
// ---------------------------------------------------------------------------

/// Available project subcommands.
#[derive(Subcommand, Debug)]
pub enum ProjectCommands {
    /// List projects in a workspace.
    ///
    /// Shows all projects within a workspace. If `--workspace` is omitted,
    /// uses the default workspace for the current org (set via
    /// `atomic workspace switch`).
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic project list
    /// atomic project list --workspace my-ws
    /// atomic project list --workspace my-ws --format json
    /// ```
    List(list::ProjectList),

    /// Create a new remote project.
    ///
    /// Creates a project inside a workspace on the server. If `--workspace`
    /// is omitted, uses the default workspace for the current org (set via
    /// `atomic workspace switch`).
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic project create my-project
    /// atomic project create my-project --workspace my-ws
    /// atomic project create my-project --workspace my-ws --kind rust
    /// ```
    Create(create::ProjectCreate),

    /// Show project details.
    ///
    /// Displays detailed information about a project using the
    /// `workspace/project` slug format.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic project show my-ws/my-project
    /// atomic project show my-ws/my-project --format json
    /// ```
    Show(show::ProjectShow),

    /// Update a project.
    ///
    /// Modifies one or more fields of an existing project.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic project update my-ws/my-project --name new-name
    /// atomic project update my-ws/my-project --visibility public
    /// ```
    Update(update::ProjectUpdate),

    /// Delete a project.
    ///
    /// Removes a project from the remote server.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic project delete my-ws/my-project
    /// atomic project delete my-ws/my-project --force
    /// ```
    Delete(delete::ProjectDelete),

    /// Initialize a local repo as a remote project.
    ///
    /// Creates the project on the server and configures the local
    /// repository with the remote URL. If `--workspace` is omitted, uses
    /// the default workspace for the current org (set via
    /// `atomic workspace switch`).
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic project init my-project
    /// atomic project init my-project --workspace my-ws
    /// atomic project init my-project --workspace my-ws --kind rust
    /// ```
    Init(init::ProjectInit),
}

/// Project management command.
///
/// Manage projects on a remote atomic-storage server. Projects live inside
/// workspaces and each represents a single Atomic VCS repository.
#[derive(Debug, clap::Args)]
#[command(name = "project", visible_alias = "proj")]
pub struct ProjectCmd {
    #[command(subcommand)]
    pub command: ProjectCommands,
}

impl Command for ProjectCmd {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            ProjectCommands::List(cmd) => cmd.run(),
            ProjectCommands::Create(cmd) => cmd.run(),
            ProjectCommands::Show(cmd) => cmd.run(),
            ProjectCommands::Update(cmd) => cmd.run(),
            ProjectCommands::Delete(cmd) => cmd.run(),
            ProjectCommands::Init(cmd) => cmd.run(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_project_path_valid() {
        let (ws, proj) = parse_project_path("my-ws/my-proj").unwrap();
        assert_eq!(ws, "my-ws");
        assert_eq!(proj, "my-proj");
    }

    #[test]
    fn parse_project_path_extra_slashes_takes_first() {
        let (ws, proj) = parse_project_path("ws/proj/extra").unwrap();
        assert_eq!(ws, "ws");
        assert_eq!(proj, "proj/extra");
    }

    #[test]
    fn parse_project_path_no_slash() {
        let result = parse_project_path("noslash");
        assert!(result.is_err());
        match result.unwrap_err() {
            CliError::InvalidArgument { message } => {
                assert!(message.contains("workspace/project"));
                assert!(message.contains("noslash"));
            }
            other => panic!("expected InvalidArgument, got {:?}", other),
        }
    }

    #[test]
    fn parse_project_path_empty_string() {
        assert!(parse_project_path("").is_err());
    }

    #[test]
    fn test_project_commands_variants() {
        fn check_variant(cmd: &ProjectCommands) -> &'static str {
            match cmd {
                ProjectCommands::List(_) => "list",
                ProjectCommands::Create(_) => "create",
                ProjectCommands::Show(_) => "show",
                ProjectCommands::Update(_) => "update",
                ProjectCommands::Delete(_) => "delete",
                ProjectCommands::Init(_) => "init",
            }
        }

        let list = list::ProjectList {
            workspace: Some("ws".to_string()),
            org: None,
            format: "table".to_string(),
        };
        assert_eq!(check_variant(&ProjectCommands::List(list)), "list");

        let create = create::ProjectCreate {
            name: "proj".to_string(),
            workspace: Some("ws".to_string()),
            description: None,
            kind: None,
            default_view: "dev".to_string(),
            visibility: "private".to_string(),
            org: None,
        };
        assert_eq!(check_variant(&ProjectCommands::Create(create)), "create");
    }
}
