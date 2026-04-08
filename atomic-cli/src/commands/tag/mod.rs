//! Tag management commands for Atomic VCS.
//!
//! Tags are named snapshots of a view's state at a particular point in time.
//! Unlike Git tags which point to commits, Atomic tags point to Merkle states -
//! cryptographic hashes representing the complete sequence of applied changes.
//!
//! # Tag Types
//!
//! - **Lightweight Tags**: Just a state reference (name + merkle state)
//! - **Annotated Tags**: Include metadata (message, author, timestamp)
//!
//! # Usage
//!
//! ```text
//! atomic tag <COMMAND>
//!
//! Commands:
//!   create  Create a new tag
//!   delete  Delete a tag
//!   list    List all tags
//!   show    Show details for a specific tag
//!
//! Options:
//!   -h, --help  Print help information
//! ```
//!
//! # Examples
//!
//! ## Creating Tags
//!
//! ```text
//! # Create a lightweight tag
//! $ atomic tag create v1.0.0
//! Created tag: v1.0.0
//!
//! # Create an annotated tag with a message
//! $ atomic tag create v1.0.0 -m "Release version 1.0.0"
//! Created annotated tag: v1.0.0
//!
//! # Create a tag with author information
//! $ atomic tag create v1.0.0 -m "Release" --author "Alice <alice@example.com>"
//! Created annotated tag: v1.0.0
//! ```
//!
//! ## Listing Tags
//!
//! ```text
//! # List all tags
//! $ atomic tag list
//! v1.0.0
//! v1.1.0
//! v2.0.0-beta
//!
//! # List tags with details
//! $ atomic tag list --verbose
//! v1.0.0      (seq: 42)   state: ABCD1234...  2024-01-15
//! v1.1.0      (seq: 58)   state: EFGH5678...  2024-02-20
//! v2.0.0-beta (seq: 73)   state: IJKL9012...  2024-03-10
//! ```
//!
//! ## Showing Tag Details
//!
//! ```text
//! $ atomic tag show v1.0.0
//! Tag: v1.0.0
//! View: dev
//! Sequence: 42
//! State: ABCDEF123456789...
//! Created: 2024-01-15 10:30:00 UTC
//! Message: Release version 1.0.0
//! Author: Alice <alice@example.com>
//! ```
//!
//! ## Deleting Tags
//!
//! ```text
//! $ atomic tag delete v1.0.0
//! Deleted tag: v1.0.0
//! ```

pub mod create;
pub mod delete;
pub mod list;
pub mod show;

use clap::Subcommand;

pub use create::Create;
pub use delete::Delete;
pub use list::List;
pub use show::Show;

use crate::commands::Command;
use crate::error::CliResult;

// Tag Subcommands

/// Subcommands for tag management.
///
/// Tags in Atomic are named snapshots of a view's Merkle state,
/// useful for marking releases, synchronization points, and rollback targets.
#[derive(Subcommand, Debug)]
pub enum TagCommands {
    /// Create a new tag.
    ///
    /// Creates a tag pointing to the current state of a view.
    /// Tags can be lightweight (just a reference) or annotated
    /// (with message and author information).
    ///
    /// # Examples
    ///
    /// ```text
    /// # Lightweight tag
    /// atomic tag create v1.0.0
    ///
    /// # Annotated tag
    /// atomic tag create v1.0.0 -m "Release version 1.0.0"
    /// ```
    Create(Create),

    /// Delete a tag.
    ///
    /// Removes a tag from the repository. This does not affect
    /// the underlying changes or view state.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic tag delete v1.0.0
    /// ```
    Delete(Delete),

    /// List all tags.
    ///
    /// Shows all tags in the repository, optionally with
    /// additional details like state and sequence number.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic tag list
    /// atomic tag list --verbose
    /// atomic tag list --view main
    /// ```
    List(List),

    /// Show details for a specific tag.
    ///
    /// Displays detailed information about a tag including
    /// its state, sequence number, and any annotation.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic tag show v1.0.0
    /// ```
    Show(Show),
}

// Tag Command

/// Tag management command.
///
/// This is the top-level command that dispatches to subcommands.
#[derive(Debug, clap::Args)]
pub struct Tag {
    #[command(subcommand)]
    pub command: TagCommands,
}

impl Command for Tag {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            TagCommands::Create(cmd) => cmd.run(),
            TagCommands::Delete(cmd) => cmd.run(),
            TagCommands::List(cmd) => cmd.run(),
            TagCommands::Show(cmd) => cmd.run(),
        }
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tag_commands_variants() {
        // Ensure all variants exist and can be matched
        fn check_variant(cmd: &TagCommands) -> &'static str {
            match cmd {
                TagCommands::Create(_) => "create",
                TagCommands::Delete(_) => "delete",
                TagCommands::List(_) => "list",
                TagCommands::Show(_) => "show",
            }
        }

        // Create instances of each variant
        let create = Create::default();
        let delete = Delete::default();
        let list = List::default();
        let show = Show::default();

        assert_eq!(check_variant(&TagCommands::Create(create)), "create");
        assert_eq!(check_variant(&TagCommands::Delete(delete)), "delete");
        assert_eq!(check_variant(&TagCommands::List(list)), "list");
        assert_eq!(check_variant(&TagCommands::Show(show)), "show");
    }
}
