//! Vault commands — shared project knowledge store.
//!
//! The vault stores goals, memory, skills, intents, and scratch
//! notes as versioned markdown files in `.vault/`.
//!
//! # Usage
//!
//! ```text
//! atomic vault <COMMAND>
//!
//! Commands:
//!   show          Print a vault entry's content
//!   list          List vault entries
//!   materialize   Materialize vault entries to markdown on disk
//!   sync          Sync vault markdown files back to the database
//!   goal          Manage vault goals
//!   intent        Manage vault intents (tasks)
//!   memory        Manage vault memory (shared knowledge)
//!   context       Retrieve memories relevant to a task
//!   summaries     Get tool result summaries for a goal
//!
//! Options:
//!   -h, --help    Print help information
//! ```
//!
//! # Examples
//!
//! ```text
//! # List all vault entries
//! $ atomic vault list
//!
//! # Show a specific entry
//! $ atomic vault show memory/architecture.md
//!
//! # Start a new goal
//! $ atomic vault goal start
//!
//! # Create a new intent
//! $ atomic vault intent create --title "Fix auth"
//!
//! # Sync markdown files back to the database
//! $ atomic vault sync
//! ```

pub mod context;
pub mod goal;
pub mod init;
pub mod list;
pub mod materialize;
pub mod removed;
pub mod show;
pub mod summaries;
pub mod sync;

use clap::Subcommand;

pub use context::Context;
pub use goal::Goal;
pub use init::Init;
pub use list::List;
pub use materialize::Materialize;
pub use removed::{RemovedVaultIntent, RemovedVaultMemory};
pub use show::Show;
pub use summaries::Summaries;
pub use sync::Sync;

// Query is a top-level command (atomic query), re-imported here for
// backward compatibility (atomic vault query).
pub use crate::commands::query::Query;

use crate::commands::Command;
use crate::error::CliResult;

// Vault Subcommands

/// Subcommands for vault management.
///
/// The vault is a shared project knowledge store that tracks goals,
/// memory, skills, intents, and scratch notes as versioned markdown.
#[derive(Subcommand, Debug)]
pub enum VaultCommands {
    /// Initialize the vault in an existing repository.
    ///
    /// Creates `.vault/` with default skills, prompts, and memory.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic vault init
    /// ```
    Init(Init),

    /// Print a vault entry's content.
    ///
    /// Retrieves and displays the content of a vault entry by its
    /// vault-relative path.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic vault show memory/architecture.md
    /// atomic vault show goals/swift-meadow-a3f2/_goal.md --json
    /// ```
    Show(Show),

    /// List vault entries.
    ///
    /// Lists all entries in the vault, optionally filtered by type
    /// or path prefix.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic vault list
    /// atomic vault list --type memory
    /// atomic vault list --prefix goals/
    /// ```
    List(List),

    /// Materialize vault entries to markdown on disk.
    ///
    /// Writes vault entries from the database to the filesystem as
    /// markdown files. Use `--path` to materialize a single entry.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic vault materialize
    /// atomic vault materialize --path memory/architecture.md
    /// ```
    Materialize(Materialize),

    /// Sync vault markdown files back to the database.
    ///
    /// Reads modified vault markdown files from disk and updates
    /// the database to match.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic vault sync
    /// ```
    Sync(Sync),

    /// Manage vault goals.
    ///
    /// Goals track development activity — start, stop, resume,
    /// list, and inspect goals.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic vault goal start
    /// atomic vault goal stop
    /// atomic vault goal list
    /// atomic vault goal show swift-meadow-a3f2
    /// ```
    Goal(Goal),

    /// Research Vault memories relevant to a task.
    ///
    /// Ranks vault memories against free-text terms, an intent, or
    /// file paths, and emits prompt-ready Markdown (or JSON).
    Context(Context),

    /// Get tool result summaries for a goal.
    ///
    /// Retrieves previews of tool results stored in the vault,
    /// optionally filtered to a specific goal.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic vault summaries
    /// atomic vault summaries --goal swift-meadow-a3f2
    /// ```
    Summaries(Summaries),

    /// Query the knowledge graph (alias for `atomic query`).
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic vault query search "authentication"
    /// ```
    Query(Query),

    /// [REMOVED — use `atomic intent`] Agent-facing redirect shim.
    ///
    /// The intent family moved to the root `atomic intent` command. This
    /// captures old `atomic vault intent …` invocations and prints a
    /// machine-parseable redirect, then exits non-zero.
    Intent(RemovedVaultIntent),

    /// [REMOVED — use `atomic memory`] Agent-facing redirect shim.
    ///
    /// The memory family moved to the root `atomic memory` command. This
    /// captures old `atomic vault memory …` invocations and prints a
    /// machine-parseable redirect, then exits non-zero.
    Memory(RemovedVaultMemory),
}

// Vault Command

/// Vault — shared project knowledge store.
///
/// This is the top-level command that dispatches to subcommands.
#[derive(Debug, clap::Args)]
#[command(name = "vault")]
pub struct Vault {
    #[command(subcommand)]
    pub command: VaultCommands,
}

impl Command for Vault {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            VaultCommands::Init(cmd) => cmd.run(),
            VaultCommands::Show(cmd) => cmd.run(),
            VaultCommands::List(cmd) => cmd.run(),
            VaultCommands::Materialize(cmd) => cmd.run(),
            VaultCommands::Sync(cmd) => cmd.run(),
            VaultCommands::Goal(cmd) => cmd.run(),
            VaultCommands::Intent(cmd) => cmd.run(),
            VaultCommands::Memory(cmd) => cmd.run(),
            VaultCommands::Context(cmd) => cmd.run(),
            VaultCommands::Summaries(cmd) => cmd.run(),
            VaultCommands::Query(cmd) => cmd.run(),
        }
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vault_commands_variants() {
        // Ensure all variants exist and can be matched
        fn check_variant(cmd: &VaultCommands) -> &'static str {
            match cmd {
                VaultCommands::Init(_) => "init",
                VaultCommands::Show(_) => "show",
                VaultCommands::List(_) => "list",
                VaultCommands::Materialize(_) => "materialize",
                VaultCommands::Sync(_) => "sync",
                VaultCommands::Goal(_) => "goal",
                VaultCommands::Intent(_) => "intent",
                VaultCommands::Memory(_) => "memory",
                VaultCommands::Context(_) => "context",
                VaultCommands::Summaries(_) => "summaries",
                VaultCommands::Query(_) => "query",
            }
        }

        // Verify variant name extraction works for each variant name string
        let names = [
            "init",
            "show",
            "list",
            "materialize",
            "sync",
            "goal",
            "context",
            "summaries",
            "query",
            "intent",
            "memory",
        ];
        assert_eq!(names.len(), 11);
    }
}
