//! `atomic vault memory` — manage vault memory files.
//!
//! Memory files are persistent knowledge documents stored in the vault.
//! They can be listed, viewed, and written (from stdin).
//!
//! # Usage
//!
//! ```text
//! atomic vault memory <COMMAND>
//!
//! Commands:
//!   list   List memory files
//!   show   Show a memory file's content
//!   write  Write a memory file (reads from stdin)
//! ```
//!
//! # Examples
//!
//! ```text
//! # List all memory files
//! atomic vault memory list
//!
//! # Show a specific memory file
//! atomic vault memory show architecture
//!
//! # Write a memory file from stdin
//! echo "# Architecture\nOur system uses..." | atomic vault memory write architecture
//!
//! # Write with a specific type
//! cat notes.md | atomic vault memory write design-decisions --type reference
//! ```

use clap::{Parser, Subcommand};

use atomic_core::pristine::vault::VaultEntryType;
use atomic_repository::Repository;

use crate::commands::{find_repository_root, format_size, Command};
use crate::error::{CliError, CliResult};

// Memory Subcommands

/// Subcommands for vault memory management.
#[derive(Subcommand, Debug)]
pub enum MemoryCommands {
    /// List memory files.
    ///
    /// Shows all memory files stored in the vault, with their sizes.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic vault memory list
    /// atomic vault memory list --json
    /// ```
    List(MemoryList),

    /// Show a memory file's content.
    ///
    /// Prints the content of a memory file to stdout. The name can be
    /// specified with or without the `memory/` prefix and `.md` extension.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic vault memory show architecture
    /// atomic vault memory show architecture.md
    /// atomic vault memory show --json architecture
    /// ```
    Show(MemoryShow),

    /// Write a memory file (reads content from stdin).
    ///
    /// Creates or updates a memory file in the vault. Content is read
    /// from standard input.
    ///
    /// # Examples
    ///
    /// ```text
    /// echo "# Design" | atomic vault memory write design
    /// cat architecture.md | atomic vault memory write architecture --type reference
    /// ```
    Write(MemoryWrite),
}

// Memory Command

/// Manage vault memory files.
///
/// Memory files are persistent knowledge documents stored in the vault.
/// They persist across sessions and can be used to share architectural
/// decisions, design notes, and project knowledge.
#[derive(Debug, clap::Args)]
#[command(name = "memory")]
pub struct Memory {
    #[command(subcommand)]
    pub command: MemoryCommands,
}

impl Command for Memory {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            MemoryCommands::List(cmd) => cmd.run(),
            MemoryCommands::Show(cmd) => cmd.run(),
            MemoryCommands::Write(cmd) => cmd.run(),
        }
    }
}

// MemoryList Command

/// List memory files.
#[derive(Parser, Debug)]
#[command(name = "list")]
pub struct MemoryList {
    /// Output as JSON.
    #[arg(long)]
    pub json: bool,
}

impl Command for MemoryList {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        let entries = repo
            .vault_list("memory/", None)
            .map_err(CliError::Repository)?;

        if self.json {
            let json: Vec<serde_json::Value> = entries
                .iter()
                .map(|e| {
                    serde_json::json!({
                        "path": e.path,
                        "size": e.content_size,
                        "updated_at": e.updated_at,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&json).unwrap());
        } else {
            if entries.is_empty() {
                println!("No memory files found.");
                return Ok(());
            }
            for e in &entries {
                let name = e.path.strip_prefix("memory/").unwrap_or(&e.path);
                println!("  {:6}  {}", format_size(e.content_size as u64), name);
            }
        }

        Ok(())
    }
}

// MemoryShow Command

/// Show a memory file's content.
#[derive(Parser, Debug)]
#[command(name = "show")]
pub struct MemoryShow {
    /// Memory file name (e.g., "architecture" or "architecture.md").
    pub name: String,

    /// Output as JSON.
    #[arg(long)]
    pub json: bool,
}

impl Command for MemoryShow {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        let path = normalize_memory_path(&self.name);
        let entry = repo
            .vault_retrieve(&path)
            .map_err(CliError::Repository)?
            .ok_or_else(|| CliError::VaultEntityNotFound {
                kind: "memory",
                id: self.name.clone(),
            })?;

        if self.json {
            let json = serde_json::json!({
                "path": path,
                "content": String::from_utf8_lossy(&entry.content_bytes),
                "frontmatter": serde_json::from_str::<serde_json::Value>(
                    &entry.frontmatter_json
                ).unwrap_or_default(),
            });
            println!("{}", serde_json::to_string_pretty(&json).unwrap());
        } else {
            print!("{}", String::from_utf8_lossy(&entry.content_bytes));
        }

        Ok(())
    }
}

// MemoryWrite Command

/// Write a memory file (reads content from stdin).
#[derive(Parser, Debug)]
#[command(name = "write")]
pub struct MemoryWrite {
    /// Memory file name (e.g., "architecture" or "architecture.md").
    pub name: String,

    /// Memory type (user, feedback, project, reference).
    #[arg(long, short = 't', default_value = "project")]
    pub r#type: String,
}

impl Command for MemoryWrite {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        // Read content from stdin
        use std::io::Read;
        let mut content = String::new();
        std::io::stdin()
            .read_to_string(&mut content)
            .map_err(CliError::Io)?;

        let path = normalize_memory_path(&self.name);
        let frontmatter = serde_json::json!({
            "name": self.name.replace(".md", ""),
            "type": self.r#type,
        })
        .to_string();

        let hash = repo
            .vault_store(
                &path,
                VaultEntryType::Memory,
                content.into_bytes(),
                frontmatter,
            )
            .map_err(CliError::Repository)?;

        // Materialize to disk
        repo.vault_materialize(&path)
            .map_err(CliError::Repository)?;

        println!("Wrote memory: {} ({})", path, &hash.to_string()[..12]);

        Ok(())
    }
}

// Helpers

/// Normalize a user-provided memory name to a full vault path.
///
/// Ensures the path starts with `memory/` and ends with `.md`.
fn normalize_memory_path(name: &str) -> String {
    let name = if name.starts_with("memory/") {
        name.to_string()
    } else {
        format!("memory/{}", name)
    };
    if name.ends_with(".md") {
        name
    } else {
        format!("{}.md", name)
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_memory_path_plain_name() {
        assert_eq!(
            normalize_memory_path("architecture"),
            "memory/architecture.md"
        );
    }

    #[test]
    fn test_normalize_memory_path_with_extension() {
        assert_eq!(
            normalize_memory_path("architecture.md"),
            "memory/architecture.md"
        );
    }

    #[test]
    fn test_normalize_memory_path_with_prefix() {
        assert_eq!(
            normalize_memory_path("memory/architecture"),
            "memory/architecture.md"
        );
    }

    #[test]
    fn test_normalize_memory_path_with_prefix_and_extension() {
        assert_eq!(
            normalize_memory_path("memory/architecture.md"),
            "memory/architecture.md"
        );
    }
}
