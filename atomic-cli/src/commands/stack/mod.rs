//! Stack management commands for Atomic VCS.
//!
//! Stacks are one of Atomic's most powerful features, enabling parallel development
//! lines that can be managed independently. Unlike Git branches, stacks are
//! **first-class citizens** in Atomic's data model - they represent views of the
//! same underlying graph, not divergent histories.
//!
//! # Key Characteristics
//!
//! - **Independent Evolution**: Each stack maintains its own sequence of changes
//! - **Selective Merging**: Pull specific changes between stacks without complex rebasing
//! - **Conflict-Free by Design**: Atomic's patch theory handles most conflicts automatically
//! - **Named and Discoverable**: Stacks have human-readable names and can be listed/queried
//!
//! # Critical Difference: Stacks are NOT Git Branches
//!
//! **Stacks share the same working copy** - they are NOT isolated filesystems like Git branches.
//!
//! When you switch between stacks:
//! - ✅ **Recorded changes** are applied/unapplied (patch history is isolated)
//! - ✅ **Unrecorded changes** remain in your working copy (files persist)
//! - ❌ **Untracked files** do NOT disappear (unlike `git checkout`)
//!
//! Think of stacks as **different views of patch history** operating on the **same workspace**.
//!
//! # Usage
//!
//! ```text
//! atomic stack <COMMAND>
//!
//! Commands:
//!   new     Create a new stack
//!   switch  Switch to a different stack
//!   delete  Delete a stack
//!   list    List all stacks
//!
//! Options:
//!   -h, --help  Print help information
//! ```
//!
//! # Examples
//!
//! ## Creating and Switching Stacks
//!
//! ```text
//! # Create a new feature stack
//! $ atomic stack new feature-auth
//! Created stack: feature-auth
//!
//! # Switch to the feature stack
//! $ atomic stack switch feature-auth
//! Switched to stack: feature-auth
//!
//! # List all stacks
//! $ atomic stack list
//!   dev
//! * feature-auth
//! ```
//!
//! ## Feature Development Workflow
//!
//! ```text
//! # Create and switch to a feature stack
//! $ atomic stack new feature-auth --switch
//! Created stack: feature-auth
//! Switched to stack: feature-auth
//!
//! # Work on your feature
//! $ atomic record -m "Add authentication module"
//! $ atomic record -m "Add login endpoint"
//!
//! # Switch back to main development
//! $ atomic stack switch dev
//! Switched to stack: dev
//! ```
//!
//! ## Cleanup Old Stacks
//!
//! ```text
//! # Delete a merged feature stack
//! $ atomic stack delete feature-old
//! Deleted stack: feature-old
//!
//! # Force delete if needed
//! $ atomic stack delete experiment --force
//! Deleted stack: experiment
//! ```

pub mod delete;
pub mod list;
pub mod new;
pub mod switch;

use clap::Subcommand;

pub use delete::Delete;
pub use list::List;
pub use new::New;
pub use switch::Switch;

use crate::commands::Command;
use crate::error::CliResult;

// Stack Subcommands

/// Subcommands for stack management.
///
/// Stacks in Atomic are similar to branches in Git, but they represent
/// views of the same underlying graph rather than divergent histories.
#[derive(Subcommand, Debug)]
pub enum StackCommands {
    /// Create a new stack.
    ///
    /// Creates a new stack, optionally based on the current stack's state.
    /// By default, the new stack starts with the same changes as the current stack.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Create a new stack
    /// atomic stack new feature-auth
    ///
    /// # Create and switch to the new stack
    /// atomic stack new feature-auth --switch
    /// ```
    New(New),

    /// Switch to a different stack.
    ///
    /// Changes the current stack to the specified one. This updates which
    /// view of the graph you're working with, but doesn't change the
    /// files in your working copy.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic stack switch dev
    /// atomic stack switch feature-auth
    /// ```
    Switch(Switch),

    /// Delete a stack.
    ///
    /// Removes the specified stack. You cannot delete the current stack -
    /// switch to a different stack first.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic stack delete old-feature
    /// atomic stack delete experiment --force
    /// ```
    Delete(Delete),

    /// List all stacks.
    ///
    /// Shows all stacks in the repository. The current stack is marked
    /// with an asterisk (*).
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic stack list
    /// atomic stack list --verbose
    /// ```
    List(List),
}

// Stack Command

/// Stack management command.
///
/// This is the top-level command that dispatches to subcommands.
#[derive(Debug, clap::Args)]
pub struct Stack {
    #[command(subcommand)]
    pub command: StackCommands,
}

impl Command for Stack {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            StackCommands::New(cmd) => cmd.run(),
            StackCommands::Switch(cmd) => cmd.run(),
            StackCommands::Delete(cmd) => cmd.run(),
            StackCommands::List(cmd) => cmd.run(),
        }
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stack_commands_variants() {
        // Ensure all variants exist and can be matched
        fn check_variant(cmd: &StackCommands) -> &'static str {
            match cmd {
                StackCommands::New(_) => "new",
                StackCommands::Switch(_) => "switch",
                StackCommands::Delete(_) => "delete",
                StackCommands::List(_) => "list",
            }
        }

        // Create instances of each variant
        let new = New::default();
        let switch = Switch::default();
        let delete = Delete::default();
        let list = List::default();

        assert_eq!(check_variant(&StackCommands::New(new)), "new");
        assert_eq!(check_variant(&StackCommands::Switch(switch)), "switch");
        assert_eq!(check_variant(&StackCommands::Delete(delete)), "delete");
        assert_eq!(check_variant(&StackCommands::List(list)), "list");
    }
}
