//! Local repository metadata, independent of remote storage project management.

use super::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::service::Service;
use atomic_client::proto as pb;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
pub struct Repo {
    #[command(subcommand)]
    command: RepoCommand,
}
#[derive(Debug, Subcommand)]
enum RepoCommand {
    /// Inspect the stable local project identity.
    Identity {
        #[command(subcommand)]
        command: IdentityCommand,
    },
}
#[derive(Debug, Subcommand)]
enum IdentityCommand {
    Show {
        #[arg(long)]
        json: bool,
    },
}

impl Command for Repo {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let service = Service::open_root(&root)?.ok_or_else(|| CliError::InvalidArgument {
            message: "repository identity inspection requires a local repository service".into(),
        })?;
        let record = service.get_project_identity(pb::GetProjectIdentityRequest {
            repository: Some(service.reference.clone()),
        })?;
        let json = matches!(
            self.command,
            RepoCommand::Identity {
                command: IdentityCommand::Show { json: true }
            }
        );
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "project": record.project, "urn": record.urn, "created": record.created,
                    "minted": record.minted, "signed_by": record.signed_by, "hash": record.hash,
                }))
                .expect("JSON serialization")
            );
        } else {
            println!(
                "project: {}\nurn: {}\ncreated: {}\nminted: {}\nsigned by: {}",
                record.project,
                record.urn,
                record.created,
                record.minted,
                record.signed_by.as_deref().unwrap_or("unsigned")
            );
        }
        Ok(())
    }
}
