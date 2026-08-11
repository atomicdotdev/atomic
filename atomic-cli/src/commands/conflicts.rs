//! The `conflicts` command for listing files in a conflicted state.
//!
//! Merge conflicts are persisted per view (see the `CONFLICTS` table) and
//! surfaced by `atomic status` as `Conflicted` entries. This command is the
//! detail view: it lists each conflicted file on the current view together
//! with the conflict kind, the line where it begins, and the changes that
//! contend.
//!
//! A file is listed only while its on-disk content still carries conflict
//! markers, matching `atomic status` — once you resolve the markers and
//! record, it disappears.
//!
//! # Usage
//!
//! ```text
//! atomic conflicts [OPTIONS]
//!
//! Options:
//!   -s, --short   One machine-readable line per file: <path>:<line>:<kind>
//!   -h, --help    Print help information
//! ```

use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{path as style_path, print_blank, print_hint, print_success, warning};

/// Show files that are in a conflicted state on the current view.
#[derive(clap::Parser, Debug, Default)]
pub struct Conflicts {
    /// Machine-readable output: one line per file as `<path>:<line>:<kind>`.
    #[arg(short, long)]
    pub short: bool,
}

impl Conflicts {
    /// Create a new Conflicts command with default settings.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Command for Conflicts {
    fn run(&self) -> CliResult<()> {
        let repo_root = find_repository_root()?;
        let repo =
            Repository::open_readonly(&repo_root).map_err(|e| CliError::InvalidRepository {
                reason: e.to_string(),
            })?;

        let conflicts = repo
            .list_conflicts()
            .map_err(|e| CliError::Internal(e.into()))?;

        if self.short {
            for (path, records) in &conflicts {
                for c in records {
                    let line = c.line.map(|l| l.to_string()).unwrap_or_else(|| "-".into());
                    println!("{}:{}:{}", path, line, c.kind);
                }
            }
            return Ok(());
        }

        if conflicts.is_empty() {
            print_success("No conflicts.");
            return Ok(());
        }

        let file_word = if conflicts.len() == 1 {
            "file"
        } else {
            "files"
        };
        println!(
            "{}",
            warning(&format!("{} conflicted {}:", conflicts.len(), file_word))
        );
        print_blank();

        for (path, records) in &conflicts {
            println!("\t{}", style_path(path));
            for c in records {
                let where_ = match c.line {
                    Some(l) => format!("line {}", l),
                    None => "unknown line".to_string(),
                };
                if c.sides.is_empty() {
                    println!("\t    {} conflict at {}", c.kind, where_);
                } else {
                    let sides = c
                        .sides
                        .iter()
                        .map(|h| h.chars().take(12).collect::<String>())
                        .collect::<Vec<_>>()
                        .join(", ");
                    println!("\t    {} conflict at {} between {}", c.kind, where_, sides);
                }
            }
        }

        print_blank();
        print_hint("Resolve the markers (>>>>>>> / ======= / <<<<<<<), then run 'atomic record'.");

        Ok(())
    }
}
