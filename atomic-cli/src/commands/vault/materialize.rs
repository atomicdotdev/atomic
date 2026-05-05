//! `atomic vault materialize` — inflate vault entries to markdown on disk.
//!
//! Writes vault entries out as markdown files in the working copy so they
//! can be read, edited, or diffed with regular file-system tools.
//!
//! # Usage
//!
//! ```text
//! atomic vault materialize [OPTIONS]
//!
//! Options:
//!   -p, --path <PATH>  Materialize only this specific vault path
//!   -h, --help         Print help information
//! ```
//!
//! # Examples
//!
//! ```text
//! # Materialize all vault entries to disk
//! $ atomic vault materialize
//! Materialized 12 vault entries.
//!
//! # Materialize a single entry
//! $ atomic vault materialize --path memory/architecture.md
//! Materialized: memory/architecture.md
//! ```

use clap::Parser;

use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};

/// Materialize vault entries to markdown files on disk.
///
/// Without `--path`, materializes every entry in the vault. With `--path`,
/// only the specified entry is written.
#[derive(Parser, Debug)]
#[command(name = "materialize")]
pub struct Materialize {
    /// Materialize only this specific vault path.
    ///
    /// The path is relative to the vault root (e.g. "memory/architecture.md").
    #[arg(long, short = 'p')]
    pub path: Option<String>,
}

impl Command for Materialize {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        if let Some(ref path) = self.path {
            repo.vault_materialize(path).map_err(CliError::Repository)?;
            println!("Materialized: {}", path);
        } else {
            let count = repo.vault_materialize_all().map_err(CliError::Repository)?;
            println!("Materialized {} vault entries.", count);
        }

        Ok(())
    }
}
