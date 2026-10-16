//! Local repository metadata, independent of remote storage project management.

use super::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use atomic_core::types::Base32;
use atomic_repository::Repository;
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
        let repo = Repository::open(find_repository_root()?).map_err(CliError::Repository)?;
        let record = repo
            .project_identity()
            .map_err(CliError::Repository)?
            .ok_or_else(|| CliError::Internal(anyhow::anyhow!("MissingProjectIdentity")))?;
        let project = record
            .project_id()
            .map_err(|e| CliError::Internal(e.into()))?;
        let object: serde_json::Value =
            serde_json::from_slice(&record.object).map_err(|e| CliError::Internal(e.into()))?;
        let signed_by = object
            .get("atomic:issuer")
            .and_then(|v| v.get("@id"))
            .and_then(|v| v.as_str());
        let minted = serde_json::to_value(record.mint_reason).expect("enum serialization");
        let json = matches!(
            self.command,
            RepoCommand::Identity {
                command: IdentityCommand::Show { json: true }
            }
        );
        if json {
            println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                "project": project.to_base32(), "urn": project.to_urn(), "created": record.created_at,
                "minted": minted, "signed_by": signed_by, "hash": record.hash.to_base32(),
            })).expect("JSON serialization"));
        } else {
            println!(
                "project: {project}\nurn: {}\ncreated: {}\nminted: {}\nsigned by: {}",
                project.to_urn(),
                record.created_at,
                minted.as_str().unwrap_or_default(),
                signed_by.unwrap_or("unsigned")
            );
        }
        Ok(())
    }
}
