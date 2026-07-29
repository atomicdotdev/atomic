//! `atomic vault sync` — sync vault markdown files back to the database.
//!
//! Reads vault markdown files from the working copy (`.vault/`)
//! and updates the vault database with any changes found on disk.
//!
//! # Usage
//!
//! ```text
//! atomic vault sync
//! ```
//!
//! # Examples
//!
//! ```text
//! # Sync all vault files back to the database
//! $ atomic vault sync
//!   synced: memory/architecture.md
//!   synced: memory/conventions.md
//!
//! Synced 2 vault files.
//! ```

use clap::Parser;

use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};

/// Sync vault markdown files back to the database.
///
/// Reads vault files from the `.vault/` directory on disk and
/// updates the vault database to match. This is the inverse of
/// `atomic vault materialize`.
#[derive(Parser, Debug)]
#[command(name = "sync")]
pub struct Sync;

impl Command for Sync {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        let updated = repo
            .vault_record_working_copy()
            .map_err(CliError::Repository)?;

        if updated.is_empty() {
            println!("Vault is up to date.");
        } else {
            for path in &updated {
                println!("  synced: {}", path);
            }
            println!("\nSynced {} vault files.", updated.len());
        }

        Ok(())
    }
}
