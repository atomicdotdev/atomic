//! Atomic - A mathematically sound distributed version control system.
//!
//! This is the main CLI entry point for Atomic. It parses command-line arguments
//! and dispatches to the appropriate command implementation.
//!
//! # Views vs Branches
//!
//! Atomic uses **Views** instead of branches. Views are perspectives on the graph -
//! they represent which changes have been inserted and in what order. Multiple
//! views can coexist, each showing a different perspective on the same
//! underlying data.
//!
//! # Command Structure
//!
//! Commands are organized into modules under `commands/`, with each command
//! implementing the [`Command`] trait. This provides:
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

// Many commands are scaffold/stub implementations with builder APIs not yet
// fully wired up. Suppress dead_code and unused_imports until they are.
#![allow(
    dead_code,
    unused_imports,
    unused_mut,
    unused_variables,
    unused_assignments
)]

mod agent_error;
mod commands;
mod error;
mod output;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};

use commands::{
    Add,
    Agent,
    ChangeCmd,
    Clone,
    Command,
    Completions,
    Conflicts,
    Diff,
    Doctor,
    Git,
    Identity,
    Init,
    Insert,
    Intent,
    Log,
    Memory,
    Move,
    ProjectCmd,
    Provenance,
    Pull,
    Push,
    Query,
    Record,
    Remote,
    Remove,
    Restore,
    Revise,
    Sandbox,
    ServerCmd,
    Session,
    Split,
    Stash,
    Status,
    Tag,
    Unrecord,
    Update,
    Vault,
    View,
    // Storage management (always available)
    WorkspaceCmd,
};

// Team features (conditional)
#[cfg(feature = "teams")]
use commands::{OrgCmd, TeamCmd};
use output::{hint, print_error};

// Agent-native help

/// Help layout applied to every command in the tree.
///
/// Atomic is agent-native, not human-native: `--help` is consumed far more
/// often by an orchestrating agent than read by a person (human guidance lives
/// on the docs website). This template keeps the output terse and deterministic
/// — the command's `about`, an explicit lowercase `usage:` line, then clap's
/// argument/option/subcommand sections — and drops the version/author banner
/// the default template prints, so an agent can parse it without wading through
/// a marketing header.
const AGENT_HELP_TEMPLATE: &str = "\
{about-with-newline}
usage: {usage}

{all-args}{after-help}";

/// Recursively apply the agent-native help layout to a command and every one of
/// its (transitive) subcommands.
///
/// clap does not propagate `help_template` to subcommands, so we walk the tree
/// once at startup and set it everywhere. This makes the whole CLI render help
/// the same agent-native way for free — any command added later is covered
/// without touching its definition. In the same pass we drop each command's
/// long description (`long_about`): the terse one-line summary is all an agent
/// needs, and the multi-paragraph prose / `# Examples` markdown that derives
/// from doc comments renders as mangled text and belongs on the docs website,
/// not in `--help`. We do the same for every argument's long help so clap does
/// not fall back to its expanded, multi-paragraph layout for one command while
/// staying compact for another — the whole tree renders the same terse way.
fn apply_agent_help(cmd: clap::Command) -> clap::Command {
    let subcommand_names: Vec<String> = cmd
        .get_subcommands()
        .map(|c| c.get_name().to_string())
        .collect();

    let mut cmd = cmd
        .help_template(AGENT_HELP_TEMPLATE)
        .long_about(None::<&str>)
        .mut_args(|arg| arg.long_help(None::<&str>));

    for name in subcommand_names {
        cmd = cmd.mut_subcommand(name, apply_agent_help);
    }
    cmd
}

// CLI Argument Definitions

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
    /// Emit extra diagnostic output.
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Disable ANSI color in output.
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
    /// Install hooks for AI coding agents (Claude Code, Antigravity CLI,
    /// Gemini CLI, Codex, OpenCode) so that each agent turn is automatically
    /// recorded as an Atomic change with full provenance and session metadata.
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
    /// Creates the `.atomic` directory structure and sets up an initial view.
    /// If the directory doesn't exist, it will be created.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Initialize in current directory
    /// atomic init
    ///
    /// # Initialize with custom view name
    /// atomic init --view main
    ///
    /// # Initialize a Rust project
    /// atomic init --kind rust
    /// ```
    Init(Init),

    /// Show the status of the working copy.
    ///
    /// Displays information about modified, added, deleted, and untracked files.
    Status(Status),

    /// List files that are in a conflicted state.
    ///
    /// Shows each conflicted file on the current view with the conflict kind,
    /// the line where it begins, and the changes that contend. A file is
    /// listed only while its content still carries conflict markers.
    Conflicts(Conflicts),

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

    /// Restore the working copy to the last recorded state.
    ///
    /// Restores the working copy to match the pristine state (last recorded
    /// state in the view). This discards any uncommitted changes. Equivalent
    /// to `git restore`. The legacy name `reset` is kept as an alias.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Restore specific files
    /// atomic restore src/main.rs
    ///
    /// # Discard all uncommitted changes
    /// atomic restore --force
    ///
    /// # Preview what would be restored
    /// atomic restore --dry-run
    /// ```
    #[command(alias = "reset")]
    Restore(Restore),

    /// Provision concurrent agent sandboxes (copy-on-write working trees).
    ///
    /// Each sandbox is a private, copy-on-write clone of the working tree so
    /// multiple agents can work at once with isolated build artifacts while
    /// sharing the single canonical graph.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic sandbox create agent-1
    /// atomic sandbox create agent-2 --dest /tmp/agent-2
    /// ```
    Sandbox(Sandbox),

    /// Inspect the Atomic-native agent session ledger.
    ///
    /// ```text
    /// atomic session show <session-id>
    /// atomic session show <session-id> --json
    /// ```
    #[command(name = "session")]
    Session(Session),

    /// Split a view (create a new view from an existing one).
    ///
    /// Creates a new view by forking from an existing view. All changes
    /// from the source view are copied to the new view.
    ///
    /// This is equivalent to `atomic view create <NAME> --from <SOURCE>`.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Split from current view
    /// atomic split experimental
    ///
    /// # Split from specific view
    /// atomic split hotfix --view release-1.0
    ///
    /// # Split and switch to new view
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
    /// in the view. This is useful for fixing typos, updating messages,
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
    /// Displays the log of changes inserted into the current view.
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

    /// Insert changes into a view.
    ///
    /// Inserts changes from change files, other views, or up to tagged states.
    /// Supports single change insertion, cross-view insert, and cherry-picking.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Insert a single change by hash
    /// atomic insert ABC12345
    ///
    /// # Insert changes from one view to another
    /// atomic insert from-view feature --to-view main
    ///
    /// # Insert changes up to a tag
    /// atomic insert tag v1.0.0 --from-view feature
    ///
    /// # Cherry-pick specific changes
    /// atomic insert pick ABC123 DEF456 --to-view main
    ///
    /// # Preview what would be inserted
    /// atomic insert preview feature --to-view main
    /// ```
    Insert(Insert),

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

    /// Diagnose and repair repository indexes.
    ///
    /// Use this for explicit maintenance tasks that may scan stored changes,
    /// such as backfilling the dependency index for legacy repositories.
    Doctor(Doctor),

    /// Git interoperability commands.
    ///
    /// Import Git repositories into Atomic, preserving history, authorship,
    /// and file operations.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Import current Git repository
    /// atomic git import
    ///
    /// # Import specific branch
    /// atomic git import --branch main
    ///
    /// # Import all local branches as views
    /// atomic git import --all
    ///
    /// # Preview without creating repository
    /// atomic git import --dry-run
    /// ```
    Git(Git),

    /// Manage views (perspectives on the graph).
    ///
    /// Views in Atomic are similar to branches in Git, but they represent
    /// perspectives on the same underlying graph rather than divergent histories.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Create a new view
    /// atomic view create feature-auth
    ///
    /// # Switch to a view
    /// atomic view switch feature-auth
    ///
    /// # List all views
    /// atomic view list
    ///
    /// # Delete a view
    /// atomic view delete old-feature
    /// ```
    View(View),

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

    /// Manage server profiles (staging, production, etc.).
    ///
    /// Server profiles let you store multiple atomic-storage server
    /// configurations and switch between them easily. Each profile
    /// can have its own URL, default org, and identity.
    ///
    /// # Examples
    ///
    /// ```text
    /// # List configured profiles
    /// atomic server
    ///
    /// # Add a staging profile
    /// atomic server add staging https://staging.atomic.storage --identity alice-staging
    ///
    /// # Switch to staging
    /// atomic server set staging
    ///
    /// # Show active profile
    /// atomic server show
    ///
    /// # Bind an identity to a profile
    /// atomic server set-identity prod alice-prod
    /// ```
    Server(ServerCmd),

    /// Manage remote workspaces.
    ///
    /// Workspaces are organizational containers for projects on a remote
    /// atomic-storage server.
    ///
    /// # Examples
    ///
    /// ```text
    /// # List workspaces
    /// atomic workspace list
    ///
    /// # Create a workspace
    /// atomic workspace create "My Team"
    ///
    /// # Show details
    /// atomic workspace show my-team
    /// ```
    Workspace(WorkspaceCmd),

    /// Manage remote projects.
    ///
    /// Projects live inside workspaces and represent a single Atomic VCS
    /// repository hosted on a remote server. When `--workspace` is omitted,
    /// commands fall back to the default workspace for the current org
    /// (set with `atomic workspace set`).
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic workspace set my-ws
    ///
    /// atomic project list
    /// atomic project create my-app
    /// atomic project init my-app
    ///
    /// atomic project create my-app --workspace other-ws
    /// ```
    Project(ProjectCmd),

    /// Manage organizations and members.
    ///
    /// Organizations group teams, workspaces, and projects. Each user has
    /// a personal organization created at registration. Team organizations
    /// enable multi-user collaboration.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Show current org
    /// atomic org show
    ///
    /// # Create a team org
    /// atomic org create "Acme Corp"
    ///
    /// # Switch default org
    /// atomic org set acme-corp
    ///
    /// # Manage members
    /// atomic org member list
    /// ```
    #[cfg(feature = "teams")]
    Org(OrgCmd),

    /// Manage teams within an organization.
    ///
    /// Teams are groups of identities used to manage access via grants.
    ///
    /// # Examples
    ///
    /// ```text
    /// # List teams
    /// atomic team list
    ///
    /// # Create a team
    /// atomic team create "Backend Eng"
    ///
    /// # Manage members
    /// atomic team member list backend-eng
    /// ```
    #[cfg(feature = "teams")]
    Team(TeamCmd),

    /// Temporarily save uncommitted changes.
    ///
    /// Stash saves your uncommitted working copy changes to a temporary
    /// orphan view, then restores the working copy to a clean state.
    /// This is useful when you need to switch views but have changes
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
    /// Tags are named references to a view's Merkle state at a specific
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

    /// Query the knowledge graph.
    ///
    /// Search code, explore relationships, visualize the codebase graph,
    /// and ask questions using the full knowledge graph — files, entities,
    /// modules, changes, views, goals, intents, and memories.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Search the knowledge graph
    /// atomic query search "replication"
    ///
    /// # Explore a node's connections
    /// atomic query neighbors "module:src/mongo/db/repl"
    ///
    /// # List entities in a file
    /// atomic query entities src/main.rs
    ///
    /// # Search source code content
    /// atomic query code "replication" -t cpp
    ///
    /// # Visualize the graph
    /// atomic query graph "replication" -k 20 --depth 2
    ///
    /// # Build/update the search index
    /// atomic query enrich
    /// ```
    Query(Query),

    /// Manage the vault (shared project knowledge store).
    ///
    /// The vault stores sessions, memory, skills, intents, and scratch
    /// notes as versioned markdown files in `.vault/`.
    ///
    /// # Examples
    ///
    /// ```text
    /// # List vault entries
    /// atomic vault list
    ///
    /// # Start a session
    /// atomic vault session start
    ///
    /// # Create an intent
    /// atomic vault intent create --title "Fix auth"
    /// ```
    Vault(Vault),

    /// Record the "why": lift, validate, attest, and render canonical intents.
    ///
    /// A sibling of `atomic vault`. Drives the `atomic-canonical` engine over
    /// real vault intents, persisting attestations additively as sidecar files
    /// under `.atomic/` — the stored vault entries are never mutated. Distinct
    /// from the vault-scoped `atomic vault intent ...` tree.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Scaffold a directive-based intent
    /// atomic intent new "Fix the login flow"
    ///
    /// # Validate it against the canonical shapes
    /// atomic intent validate PIMO-1
    ///
    /// # Render the canonical projection
    /// atomic intent show PIMO-1
    ///
    /// # Sign it into a canonical sidecar
    /// atomic intent attest PIMO-1
    /// ```
    #[command(name = "intent")]
    Intent(Intent),

    /// Record the "why" for durable context: lift, validate, attest, and render
    /// canonical memories.
    ///
    /// A sibling of the raw `atomic vault memory` tree, driving the
    /// `atomic-canonical` engine for MEMORIES. Attestations are persisted
    /// additively as tracked vault entries.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Scaffold a directive-based memory
    /// atomic memory new --kind constraint
    ///
    /// # Gate it against the canonical shapes
    /// atomic memory validate 01j8zc4r8t
    ///
    /// # Sign it into a tracked attestation
    /// atomic memory attest 01j8zc4r8t
    /// ```
    #[command(name = "memory")]
    Memory(Memory),

    /// Project & trace W3C PROV over the provenance atomic already captures.
    ///
    /// Read-only, compute-on-demand: projects the per-turn `ProvenanceGraph`
    /// into a signed W3C PROV JSON-LD named subgraph. The capture path is never
    /// touched and nothing is written. The person's real `did:atomic` signs the
    /// projection; the agent is a non-verifiable descriptive label.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Walk the flywheel chain for a change
    /// atomic provenance trace <change>
    ///
    /// # Emit the signed PROV JSON-LD artifact
    /// atomic provenance show <change>
    /// ```
    #[command(name = "provenance")]
    Provenance(Provenance),

    /// Remove the last change from the current view.
    ///
    /// The change is removed from the view's change log but NOT deleted
    /// from the change store. It can be re-inserted later with `atomic insert`.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Unrecord the most recent change
    /// atomic unrecord
    ///
    /// # Preview without actually unrecording
    /// atomic unrecord --dry-run
    /// ```
    Unrecord(Unrecord),

    /// Check for available updates and route to the correct upgrade path.
    ///
    /// Detects how this binary was installed (Homebrew, Cargo, official
    /// installer, or manual) and prints the upgrade command for that
    /// source. With `--check`, only reports status; exits 1 if outdated.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Check status and show how to upgrade
    /// atomic update
    ///
    /// # Status-only, scriptable: exit 1 if outdated
    /// atomic update --check
    /// ```
    Update(Update),

    /// Generate a shell completion script.
    ///
    /// Emits a static completion script for the given shell. For live
    /// completion of view names and change hashes, enable the dynamic engine
    /// instead with `source <(COMPLETE=zsh atomic)`.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic completions zsh > ~/.zfunc/_atomic
    /// ```
    Completions(Completions),
}

// Main Entry Point

/// The log filter `--verbose` turns on.
///
/// Scoped to the Atomic crates on purpose: a bare `debug` also unleashes
/// `reqwest`/`hyper` wire logging, which buries the one line the user wanted.
///
/// `atomic` is a prefix match, so it covers this binary (whose module paths
/// are `atomic::…`, after the `[[bin]]` name rather than the `atomic-cli`
/// package) along with every `atomic_*` library crate. `atomic_core` is pegged
/// back to `info`: its per-vertex graph logging is far below the level anyone
/// reaching for `--verbose` is asking about.
const VERBOSE_FILTER: &str = "atomic=debug,atomic_core=info";

/// Detect the global `--verbose` flag straight from `argv`.
///
/// Logging has to be live before clap runs, because argument parsing itself
/// can fail and we want the debug trail for that too. `--verbose` is a global
/// flag, so its position is unconstrained — scanning argv is both simpler and
/// more faithful than trying to parse twice.
fn verbose_requested() -> bool {
    std::env::args_os().any(|a| a == "-v" || a == "--verbose")
}

/// Install the logger, honouring `--verbose`.
///
/// Every command advertises `-v, --verbose  Emit extra diagnostic output`, but
/// the flag was parsed into a field nothing ever read: logging was initialised
/// before parsing and only `RUST_LOG` could raise the level. The debug lines
/// that explain *which identity a request authenticated as* already existed —
/// they were simply unreachable through the documented flag, which turned an
/// identity misconfiguration into an opaque server-side 401.
///
/// `RUST_LOG` still wins when set, so existing workflows are untouched.
fn init_logging() {
    let default = if verbose_requested() {
        VERBOSE_FILTER
    } else {
        "warn"
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default)).init();
}

fn main() {
    // Initialize logging
    init_logging();

    // Dynamic shell completion. When invoked in completion mode (the `COMPLETE`
    // env var is set by the installed shell hook), this emits candidates —
    // including live view names and change hashes registered on the insert
    // args — and exits before normal argument parsing. In normal invocations
    // it is a no-op. The factory mirrors the real command tree so completion
    // matches actual subcommands and aliases.
    clap_complete::CompleteEnv::with_factory(|| apply_agent_help(Cli::command())).complete();

    // Parse command-line arguments through the agent-native help layout.
    //
    // `try_get_matches_from_mut` rather than `get_matches`: clap's own failure
    // output is human prose ending in "For more information, try '--help'",
    // which costs an agent a second round trip and may discard the option/value
    // relationship or the parser's underlying cause. We intercept and re-render
    // in the same `key: value` dialect the rest of the machine surface speaks.
    // Help and version still print through clap untouched.
    let mut cmd = apply_agent_help(Cli::command());
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let matches = match cmd.try_get_matches_from_mut(args.clone()) {
        Ok(matches) => matches,
        Err(err) => agent_error::render_and_exit(err, &cmd, &args),
    };
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(err) => agent_error::render_and_exit(err, &cmd, &args),
    };

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

        Commands::Conflicts(conflicts) => conflicts.run(),

        Commands::Add(add) => add.run(),

        Commands::Remove(remove) => remove.run(),

        Commands::Move(mv) => mv.run(),

        Commands::Restore(restore) => restore.run(),

        Commands::Sandbox(sandbox) => sandbox.run(),

        Commands::Session(session) => session.run(),

        Commands::Split(split) => split.run(),

        Commands::Record(record) => record.run(),

        Commands::Revise(revise) => revise.run(),

        Commands::Log(log) => log.run(),

        Commands::Change(change) => change.run(),

        Commands::Diff(diff) => diff.run(),

        Commands::Doctor(doctor) => doctor.run(),

        Commands::Git(git) => git.run(),

        Commands::Insert(insert) => insert.run(),

        Commands::View(view) => view.run(),

        Commands::Stash(stash) => stash.run(),

        Commands::Push(push) => push.run(),

        Commands::Pull(pull) => pull.run(),

        Commands::Clone(clone) => clone.run(),

        Commands::Identity(identity) => identity.run(),

        Commands::Remote(remote) => remote.run(),

        Commands::Server(server) => server.run(),

        Commands::Workspace(workspace) => workspace.run(),

        Commands::Project(project) => project.run(),

        #[cfg(feature = "teams")]
        Commands::Org(org) => org.run(),

        #[cfg(feature = "teams")]
        Commands::Team(team) => team.run(),

        Commands::Tag(tag) => tag.run(),

        Commands::Unrecord(unrecord) => unrecord.run(),

        Commands::Update(update) => update.run(),

        Commands::Completions(completions) => completions.generate(Cli::command()),

        Commands::Query(query) => query.run(),

        Commands::Vault(vault) => vault.run(),

        Commands::Intent(intent) => intent.run(),

        Commands::Memory(memory) => memory.run(),

        Commands::Provenance(provenance) => provenance.run(),
    };

    // Handle errors with user-friendly output
    if let Err(err) = result {
        print_error(&err.to_string());

        // Print suggestion if available — to STDERR, alongside the error itself,
        // so a command's stdout (e.g. `--json` output on an erroring path) is
        // never polluted by a diagnostic hint.
        if let Some(suggestion) = err.suggestion() {
            eprintln!();
            eprintln!("{}", hint(&format!("Hint: {}", suggestion)));
        }

        // Exit with appropriate code
        std::process::exit(err.exit_code());
    }
}

#[cfg(test)]
mod agent_help_tests {
    use super::*;

    /// The raw clap command definition must be structurally valid before the
    /// agent-native help transformation is applied.
    #[test]
    fn clap_command_definition_is_valid() {
        Cli::command().debug_assert();
    }

    /// The agent-help walk must produce a structurally valid clap tree.
    /// `debug_assert` catches duplicate flags, bad templates, and the like,
    /// recursively across every command.
    #[test]
    fn agent_help_tree_is_valid() {
        apply_agent_help(Cli::command()).debug_assert();
    }

    /// Rendered help uses the agent-native template: a lowercase `usage:` line
    /// and none of the default banner (`Usage:` heading or the version/author
    /// line the stock template prints).
    #[test]
    fn agent_help_template_is_applied() {
        let mut cmd = apply_agent_help(Cli::command());
        let help = cmd.render_help().to_string();
        assert!(
            help.contains("usage: atomic"),
            "lowercase usage line: {help}"
        );
        assert!(
            !help.contains("Usage: atomic"),
            "default capitalized usage heading must be gone: {help}"
        );
    }

    /// Long descriptions are dropped tree-wide, so `--help` never leaks the
    /// `# Examples` markdown that derives from a command's doc comment.
    #[test]
    fn long_about_is_stripped() {
        let tree = apply_agent_help(Cli::command());
        let mut intent = tree
            .find_subcommand("intent")
            .expect("intent command present")
            .clone();
        let help = intent.render_help().to_string();
        assert!(
            !help.contains("# Examples"),
            "long_about markdown must not appear in help: {help}"
        );
    }
}
