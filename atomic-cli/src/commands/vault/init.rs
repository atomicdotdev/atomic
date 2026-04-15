//! `atomic vault init` — initialize the vault in an existing repository.

use clap::Parser;

use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{print_info, print_success};

/// Initialize the vault in an existing Atomic repository.
///
/// This is useful after `atomic git import` or when adding a vault
/// to a repository that was created with `--no-vault`.
///
/// Creates `.vault/` with default skills, prompts, and memory.
///
/// # Examples
///
/// ```text
/// atomic vault init
/// ```
#[derive(Parser, Debug)]
pub struct Init;

impl Command for Init {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let mut repo = Repository::open(&root).map_err(|e| CliError::InvalidRepository {
            reason: e.to_string(),
        })?;

        if repo.has_vault().unwrap_or(false) {
            print_info("Vault is already initialized");
            return Ok(());
        }

        repo.init_vault().map_err(CliError::Repository)?;
        print_info(&format!(
            "Initialized vault at {}",
            repo.vault_dir().display()
        ));

        // Add all vault files to tracking
        let vault_dir = repo.vault_dir();
        if vault_dir.exists() {
            add_vault_files_recursive(&repo, &vault_dir)?;
        }

        // Record vault files as their own change
        let header = atomic_core::change::ChangeHeader::new("Initialize vault");
        match repo.record(header, atomic_repository::RecordOptions::default()) {
            Ok(_) => print_success("Recorded vault defaults"),
            Err(atomic_repository::RecordError::NothingToRecord) => {}
            Err(e) => log::warn!("Failed to record vault files: {}", e),
        }

        Ok(())
    }
}

fn add_vault_files_recursive(repo: &Repository, dir: &std::path::Path) -> CliResult<()> {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                add_vault_files_recursive(repo, &path)?;
            } else if path.is_file() {
                if let Ok(rel) = path.strip_prefix(repo.root()) {
                    let _ = repo.add(rel, atomic_repository::TrackingOptions::default());
                }
            }
        }
    }
    Ok(())
}
