//! `atomic memory write <name>` — freeform memory writer (the raw escape hatch).
//!
//! The freeform counterpart to `atomic memory new`: `new` authors a canonical,
//! liftable memory (with a `memoryKind` spine that validates and attests);
//! `write` stores arbitrary content read from **stdin** at `memory/<name>.md`
//! with a simple `{name, type}` frontmatter. Ported from the retired
//! `atomic vault memory write` so nothing is lost when the `atomic vault memory`
//! subtree goes away.
//!
//! # Examples
//!
//! ```text
//! echo "# Design" | atomic memory write design
//! cat notes.md | atomic memory write architecture --type reference
//! ```

use clap::Parser;

use atomic_core::pristine::VaultEntryType;
use atomic_repository::Repository;

use crate::commands::memory::bridge;
use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};

/// Write a freeform memory file (reads content from stdin).
#[derive(Parser, Debug)]
#[command(name = "write")]
pub struct MemoryWrite {
    /// Memory name/id (e.g. "architecture" or a ULID). Stored at `memory/<name>.md`.
    pub name: String,

    /// Freeform type label (e.g. user, feedback, project, reference).
    #[arg(long, short = 't', default_value = "project")]
    pub r#type: String,
}

impl Command for MemoryWrite {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        // Freeform body from stdin — no lift/gate; this is the raw path.
        use std::io::Read;
        let mut content = String::new();
        std::io::stdin()
            .read_to_string(&mut content)
            .map_err(CliError::Io)?;

        let path = bridge::normalize_memory_path(&self.name);
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
        repo.vault_materialize(&path)
            .map_err(CliError::Repository)?;

        println!("Wrote memory: {} ({})", path, &hash.to_string()[..12]);
        Ok(())
    }
}
