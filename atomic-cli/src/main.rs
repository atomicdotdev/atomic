//! Atomic - A mathematically sound distributed version control system.
//!
//! This is the main CLI entry point for Atomic. It parses command-line arguments
//! and dispatches to the appropriate command implementation.
//!
//! # Stacks vs Branches
//!
//! Atomic uses **Stacks** instead of branches. Stacks are views of the graph -
//! they represent which changes have been applied and in what order. Multiple
//! stacks can coexist, each showing a different perspective on the same
//! underlying data.
//!
//! # Command Structure
//!
//! Commands are organized into modules under `commands/`, with each command
//! implementing the [`Command`](commands::Command) trait. This provides:
//!
//! - Consistent error handling across all commands
//! - Testable command implementations
//! - Clear separation of concerns
//!
//! # Example Usage
//!
//! ```text
//! # Initialize a new repository
//! atomic init
//!
//! # Add files to track
//! atomic add src/main.rs
//!
//! # Record changes
//! atomic record -m "Initial commit"
//!
//! # View history
//! atomic log
//!
//! # Check status
//! atomic status
//! ```

mod commands;
mod error;
mod output;

use clap::{Parser, Subcommand};

use commands::{
    Add, Agent, Apply, ChangeCmd, Clone, Command, Diff, Hive, Identity, Init, Log, Move, Pull,
    Push, Record, Remote, Remove, Reset, Revise, Split, Stack, Stash, Status, Tag,
};
use output::{print_error, print_hint};

// =============================================================================
// CLI Argument Definitions
// =============================================================================

/// Atomic - A mathematically sound distributed version control system.
///
/// Atomic uses patch theory to represent changes as composable, commutative
/// operations on a directed graph, enabling conflict-free merges when changes
/// are truly independent.
#[derive(Parser, Debug)]
#[command(name = "atomic")]
#[command(author, version, about, long_about = None)]
#[command(propagate_version = true)]
#[command(arg_required_else_help = true)]
struct Cli {
    /// Enable verbose output for debugging.
    ///
    /// When enabled, shows additional information about what Atomic is doing.
    /// Useful for troubleshooting or understanding the internal operations.
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Disable colored output.
    ///
    /// By default, Atomic uses colors when outputting to a terminal.
    /// Use this flag to disable colors (useful for piping output).
    #[arg(long, global = true)]
    no_color: bool,

    /// The command to run.
    #[command(subcommand)]
    command: Commands,
}

/// Available commands for the Atomic CLI.
///
/// Each command is implemented in its own module under `commands/`.
#[derive(Subcommand, Debug)]
enum Commands {
    /// Manage AI agent integration.
    ///
    /// Install hooks for AI coding agents (Claude Code, Gemini CLI, Codex,
    /// OpenCode) so that each agent turn is automatically recorded as an
    /// Atomic change with full provenance and session metadata.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Enable for Claude Code
    /// atomic agent enable
    ///
    /// # Check status
    /// atomic agent status
    ///
    /// # Disable
    /// atomic agent disable
    /// ```
    Agent(Agent),

    /// Initialize a new Atomic repository.
    ///
    /// Creates the `.atomic` directory structure and sets up an initial stack.
    /// If the directory doesn't exist, it will be created.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Initialize in current directory
    /// atomic init
    ///
    /// # Initialize with custom stack name
    /// atomic init --stack main
    ///
    /// # Initialize a Rust project
    /// atomic init --kind rust
    /// ```
    Init(Init),

    /// Show the status of the working copy.
    ///
    /// Displays information about modified, added, deleted, and untracked files.
    Status(Status),

    /// Add files to be tracked.
    ///
    /// Adds files to Atomic's internal tree so their changes can be recorded.
    Add(Add),

    /// Remove files from tracking.
    ///
    /// Stops tracking files in the repository. Files can either be deleted
    /// from disk or kept as untracked files using the `--keep` flag.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Remove and delete file
    /// atomic remove old_file.txt
    ///
    /// # Stop tracking but keep file
    /// atomic remove --keep secrets.txt
    ///
    /// # Remove directory
    /// atomic remove old_code/
    /// ```
    #[command(visible_alias = "rm")]
    Remove(Remove),

    /// Move or rename tracked files.
    ///
    /// Moves or renames files while preserving their version history.
    /// This is the recommended way to rename files in an Atomic repository.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Rename a file
    /// atomic move old_name.rs new_name.rs
    ///
    /// # Move to directory
    /// atomic move file.txt src/file.txt
    /// ```
    #[command(visible_alias = "mv")]
    Move(Move),

    /// Reset the working copy to the last recorded state.
    ///
    /// Restores the working copy to match the pristine state (last recorded
    /// state in the stack). This discards any uncommitted changes.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Discard all uncommitted changes
    /// atomic reset --force
    ///
    /// # Reset specific files
    /// atomic reset src/main.rs
    ///
    /// # Switch to a different stack
    /// atomic reset --stack main
    ///
    /// # Preview what would be reset
    /// atomic reset --dry-run
    /// ```
    Reset(Reset),

    /// Split a stack (create a new stack from an existing one).
    ///
    /// Creates a new stack by forking from an existing stack. All changes
    /// from the source stack are copied to the new stack.
    ///
    /// This is equivalent to `atomic stack new <NAME> --from <SOURCE>`.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Split from current stack
    /// atomic split experimental
    ///
    /// # Split from specific stack
    /// atomic split hotfix --stack release-1.0
    ///
    /// # Split and switch to new stack
    /// atomic split feature-auth --switch
    /// ```
    Split(Split),

    /// Record changes to the repository.
    ///
    /// Creates a new change from the current state of tracked files.
    Record(Record),

    /// Revise a change in-place.
    ///
    /// Modifies a previously recorded change without losing its position
    /// in the stack. This is useful for fixing typos, updating messages,
    /// or making corrections to recent changes.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Revise the last change
    /// atomic revise
    ///
    /// # Revise with a new message
    /// atomic revise -m "Better message"
    ///
    /// # Only change the message
    /// atomic revise --reword
    ///
    /// # Revise a previous change
    /// atomic revise @~1
    /// ```
    Revise(Revise),

    /// Show change history.
    ///
    /// Displays the log of changes applied to the current stack.
    Log(Log),

    /// Show details for a specific change.
    ///
    /// Displays detailed information about a change by hash, hash prefix,
    /// or sequence number. If no identifier is provided, shows the most
    /// recent change.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Show change by hash prefix
    /// atomic change ABC12345
    ///
    /// # Show most recent change
    /// atomic change
    ///
    /// # Show change by sequence number
    /// atomic change #42
    /// ```
    Change(ChangeCmd),

    /// Apply changes to a stack.
    ///
    /// Applies changes from change files, other stacks, or up to tagged states.
    /// Supports single change application, cross-stack apply, and cherry-picking.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Apply a single change by hash
    /// atomic apply ABC12345
    ///
    /// # Apply changes from one stack to another
    /// atomic apply from-stack feature --to-stack main
    ///
    /// # Apply changes up to a tag
    /// atomic apply tag v1.0.0 --from-stack feature
    ///
    /// # Cherry-pick specific changes
    /// atomic apply pick ABC123 DEF456 --to-stack main
    ///
    /// # Preview what would be applied
    /// atomic apply preview feature --to-stack main
    /// ```
    Apply(Apply),

    /// Show differences in the working copy.
    ///
    /// Displays the diff between the working copy and the last recorded state.
    /// Supports multiple output formats and diff algorithms.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Show all changes
    /// atomic diff
    ///
    /// # Show changes for specific file
    /// atomic diff src/main.rs
    ///
    /// # Show only statistics
    /// atomic diff --stat
    ///
    /// # Use patience algorithm
    /// atomic diff --algorithm patience
    /// ```
    Diff(Diff),

    /// Manage stacks (views of the graph).
    ///
    /// Stacks in Atomic are similar to branches in Git, but they represent
    /// views of the same underlying graph rather than divergent histories.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Create a new stack
    /// atomic stack new feature-auth
    ///
    /// # Switch to a stack
    /// atomic stack switch feature-auth
    ///
    /// # List all stacks
    /// atomic stack list
    ///
    /// # Delete a stack
    /// atomic stack delete old-feature
    /// ```
    Stack(Stack),

    /// Push changes to a remote.
    ///
    /// Uploads local changes to the specified remote repository.
    /// See `atomic push --help` for detailed options.
    Push(Push),

    /// Pull changes from a remote.
    ///
    /// Downloads and applies changes from the specified remote repository.
    /// See `atomic pull --help` for detailed options.
    Pull(Pull),

    /// Clone a remote repository.
    ///
    /// Creates a new local repository from a remote source.
    /// See `atomic clone --help` for detailed options.
    Clone(Clone),

    /// Manage identities for signing changes.
    ///
    /// Create, list, and manage user identities that are used to sign
    /// changes. Supports multiple identities for different contexts
    /// (personal, work, community) and agent identities.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Create a new identity
    /// atomic identity new alice --email alice@example.com
    ///
    /// # List all identities
    /// atomic identity list
    ///
    /// # Show current identity
    /// atomic identity whoami
    /// ```
    Identity(Identity),

    /// Manage Hive Agent Social Platform integration.
    ///
    /// Register your AI agent on Hive, check claim status, and manage
    /// your agent identity. Every agent is identified by an Ed25519
    /// keypair compatible with atomic-identity.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Initialize and register agent
    /// atomic hive init --name my-agent --vendor anthropic --model claude-sonnet-4
    ///
    /// # Check registration status
    /// atomic hive status
    ///
    /// # Check if claimed by human owner
    /// atomic hive claim
    ///
    /// # View agent profile
    /// atomic hive profile
    /// ```
    Hive(Hive),

    /// Manage remote repositories.
    ///
    /// Add, remove, list, and modify named remotes that can be used
    /// with push, pull, and clone commands.
    ///
    /// # Examples
    ///
    /// ```text
    /// # List all remotes
    /// atomic remote
    ///
    /// # Add a new remote
    /// atomic remote add origin https://api.example.com/repo
    ///
    /// # Remove a remote
    /// atomic remote remove upstream
    ///
    /// # Change remote URL
    /// atomic remote set-url origin https://new-url.com/repo
    /// ```
    Remote(Remote),

    /// Temporarily save uncommitted changes.
    ///
    /// Stash saves your uncommitted working copy changes to a temporary
    /// orphan stack, then restores the working copy to a clean state.
    /// This is useful when you need to switch stacks but have changes
    /// that belong elsewhere.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Save uncommitted changes
    /// atomic stash
    ///
    /// # Apply and remove most recent stash
    /// atomic stash pop
    ///
    /// # List all stashes
    /// atomic stash list
    ///
    /// # Apply stash without removing
    /// atomic stash apply
    ///
    /// # Delete a stash
    /// atomic stash drop
    /// ```
    Stash(Stash),

    /// Manage tags (named state snapshots).
    ///
    /// Tags are named references to a stack's Merkle state at a specific
    /// point in time. They're useful for marking releases, sync points,
    /// and rollback targets.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Create a lightweight tag
    /// atomic tag create v1.0.0
    ///
    /// # Create an annotated tag
    /// atomic tag create v1.0.0 -m "Release version 1.0.0"
    ///
    /// # List all tags
    /// atomic tag list
    ///
    /// # Show tag details
    /// atomic tag show v1.0.0
    ///
    /// # Delete a tag
    /// atomic tag delete v1.0.0
    /// ```
    Tag(Tag),
}

// =============================================================================
// Main Entry Point
// =============================================================================

fn main() {
    // Initialize logging
    env_logger::init();

    // Parse command-line arguments
    let cli = Cli::parse();

    // Configure color output
    if cli.no_color {
        // Disable colors globally
        console::set_colors_enabled(false);
        console::set_colors_enabled_stderr(false);
    }

    // Execute the command and handle errors
    let result = match cli.command {
        Commands::Agent(agent) => agent.run(),

        Commands::Init(init) => init.run(),

        Commands::Status(status) => status.run(),

        Commands::Add(add) => add.run(),

        Commands::Remove(remove) => remove.run(),

        Commands::Move(mv) => mv.run(),

        Commands::Reset(reset) => reset.run(),

        Commands::Split(split) => split.run(),

        Commands::Record(record) => record.run(),

        Commands::Revise(revise) => revise.run(),

        Commands::Log(log) => log.run(),

        Commands::Change(change) => change.run(),

        Commands::Diff(diff) => diff.run(),

        Commands::Apply(apply) => apply.run(),

        Commands::Stack(stack) => stack.run(),

        Commands::Stash(stash) => stash.run(),

        Commands::Push(push) => push.run(),

        Commands::Pull(pull) => pull.run(),

        Commands::Clone(clone) => clone.run(),

        Commands::Identity(identity) => identity.run(),

        Commands::Hive(hive) => hive.run(),

        Commands::Remote(remote) => remote.run(),

        Commands::Tag(tag) => tag.run(),
    };

    // Handle errors with user-friendly output
    if let Err(err) = result {
        print_error(&err.to_string());

        // Print suggestion if available
        if let Some(suggestion) = err.suggestion() {
            println!();
            print_hint(&format!("Hint: {}", suggestion));
        }

        // Exit with appropriate code
        std::process::exit(err.exit_code());
    }
}
