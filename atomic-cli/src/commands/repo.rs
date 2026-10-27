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
    /// Adopt the remote's project identity, replacing the local one.
    ///
    /// Divergence remediation: when a pull or push reports that the remote
    /// project identity differs from yours, this fetches the remote binding
    /// and takes it as your own (the derived knowledge graph is rebuilt under
    /// the adopted id). Local-only work is preserved — the identity is a
    /// namespace, not a content address.
    Adopt {
        /// Named remote (or URL) to adopt from. Defaults to `origin`.
        #[arg(long)]
        remote: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Mint a fresh project identity, replacing the local one.
    ///
    /// The explicit fork action: this repository stops claiming the old
    /// lineage. Remotes that bound the previous identity will refuse pushes
    /// from here — push to a different project instead.
    Fork {
        #[arg(long)]
        json: bool,
    },
}

impl Command for Repo {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        match &self.command {
            RepoCommand::Identity {
                command: IdentityCommand::Show { json },
            } => Self::show(&root, *json),
            RepoCommand::Identity {
                command: IdentityCommand::Adopt { remote, json },
            } => Self::adopt(&root, remote.as_deref(), *json),
            RepoCommand::Identity {
                command: IdentityCommand::Fork { json },
            } => Self::fork(&root, *json),
        }
    }
}

impl Repo {
    fn show(root: &std::path::Path, json: bool) -> CliResult<()> {
        let service = Service::open_root(root)?.ok_or_else(|| CliError::InvalidArgument {
            message: "repository identity inspection requires a local repository service".into(),
        })?;
        let record = service.get_project_identity(pb::GetProjectIdentityRequest {
            repository: Some(service.reference.clone()),
        })?;
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

    /// `repo identity adopt` — fetch the remote binding and force-adopt it.
    fn adopt(root: &std::path::Path, remote: Option<&str>, json: bool) -> CliResult<()> {
        use atomic_remote::{HttpRemote, HttpRemoteConfig};
        let repository = atomic_repository::Repository::open(root).map_err(CliError::Repository)?;
        let remote_name = remote.unwrap_or("origin").to_string();
        let entry = match repository.get_remote(&remote_name) {
            Ok(entry) => entry,
            Err(atomic_repository::RepositoryError::RemoteNotFound { .. }) => {
                return Err(CliError::InvalidArgument {
                    message: format!(
                        "no remote named '{remote_name}'; add one with 'atomic remote add {remote_name} <url>' or pass --remote"
                    ),
                });
            }
            Err(e) => return Err(CliError::Repository(e)),
        };
        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {e}"))
        })?;
        let (project_id, previous) = rt.block_on(async {
            let config = crate::commands::auth::attach_identity(
                HttpRemoteConfig::default(),
                &entry.url,
                None,
            )
            .await;
            let remote = HttpRemote::with_config(&entry.url, config).map_err(|e| {
                CliError::InvalidArgument {
                    message: format!("cannot reach remote {}: {e}", entry.url),
                }
            })?;
            let previous = repository
                .project_identity()
                .map_err(CliError::Repository)?
                .map(|record| {
                    record
                        .project_id()
                        .map(|id| id.to_base32())
                        .unwrap_or_else(|_| "<unreadable>".to_string())
                });
            let project_id = libatomic::identity_transport::adopt_from_remote(&repository, &remote)
                .await
                .map_err(CliError::Repository)?;
            Ok::<_, CliError>((project_id, previous))
        })?;
        if let Some(previous) = previous.filter(|p| *p != project_id) {
            println!("Replaced local identity {previous}");
        }
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "adopted": project_id,
                    "urn": format!("urn:atomic:project:{project_id}"),
                }))
                .expect("JSON serialization")
            );
        } else {
            println!("Adopted project identity {project_id}");
            println!("The derived knowledge graph was rebuilt under the adopted id.");
        }
        Ok(())
    }

    /// `repo identity fork` — mint a fresh local identity.
    fn fork(root: &std::path::Path, json: bool) -> CliResult<()> {
        let repository = atomic_repository::Repository::open(root).map_err(CliError::Repository)?;
        let previous = repository
            .project_identity()
            .map_err(CliError::Repository)?
            .map(|record| {
                record
                    .project_id()
                    .map(|id| id.to_base32())
                    .unwrap_or_else(|_| "<unreadable>".to_string())
            });
        let record = repository.fork_identity().map_err(CliError::Repository)?;
        let project_id = record.project_id().map(|id| id.to_base32()).map_err(|e| {
            CliError::Repository(atomic_repository::RepositoryError::InvalidOperation {
                message: e.to_string(),
            })
        })?;
        let signed = serde_json::from_slice::<serde_json::Value>(&record.object)
            .ok()
            .and_then(|object| object.get("proof").is_some().then_some(true))
            .unwrap_or(false);
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "project": project_id,
                    "urn": format!("urn:atomic:project:{project_id}"),
                    "previous": previous,
                    "minted": format!("{:?}", record.mint_reason).to_lowercase(),
                    "signed": signed,
                }))
                .expect("JSON serialization")
            );
        } else {
            if let Some(previous) = &previous {
                println!("Abandoned local identity {previous}");
            }
            println!("Forked project identity {project_id}");
            println!(
                "Remotes bound to the previous identity will refuse pushes from this repository."
            );
            println!("Create a new project for this lineage and push there.");
        }
        Ok(())
    }
}
