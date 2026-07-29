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
use crate::agent_doc::{Doc, Fail, Ref};

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

/// Agent guidance for `atomic memory write`.
///
/// The `run:` line carries a `< notes.md` redirect on purpose: content is read
/// from stdin to EOF, so a bare invocation with stdin on a tty blocks forever.
/// There is no exit code for that, so it cannot be a `fails:` row.
pub const DOC: Doc = Doc {
    when: "storing a document verbatim, not a gradeable fact",
    run: "memory write notes --type reference < notes.md",
    needs: &[
        Ref {
            cmd: "init",
            note: "a repository; content is read from stdin to EOF",
        },
    ],
    then: &[
        Ref {
            cmd: "vault show memory/<name>.md",
            note: "the canonical memory verbs cannot read it",
        },
        Ref {
            cmd: "add .vault",
            note: "the entry starts untracked",
        },
    ],
    instead: &[
        Ref {
            cmd: "memory new --kind <k> --text \"...\"",
            note: "liftable, gateable, attestable",
        },
    ],
    fails: &[
        Fail {
            cond: "no uid: show/validate/attest reject it",
            exit: 2,
            fix: Ref {
                cmd: "memory new --kind <k> --text \"...\"",
                note: "",
            },
        },
        Fail {
            cond: "a canonical <name> loses its spine",
            exit: 0,
            fix: Ref {
                cmd: "memory new --kind <k> --id <name>",
                note: "",
            },
        },
    ],
    ..Doc::EMPTY
};

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
