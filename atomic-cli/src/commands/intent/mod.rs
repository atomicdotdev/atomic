//! `atomic intent` — the canonical "record the why" engine surface.
//!
//! This is the canonical top-level command, a sibling of `atomic vault` (see
//! [`crate::commands::vault`]). It replaces the retired vault-scoped
//! `atomic vault intent ...` subtree (now a removed-command shim): this family
//! drives the `atomic-canonical` engine — lifting a stored intent into a
//! canonical JSON-LD node, gating it, attesting it, and rendering it.
//!
//! # Verbs
//!
//! ```text
//! atomic intent validate <ID|path>   Gate an intent against the canonical shapes
//! atomic intent show <ID>            Render an intent as a read-time projection
//! atomic intent new <TITLE>          Scaffold a directive-based intent
//! atomic intent attest <ID>          Sign an intent into a tracked attestation entry
//! atomic intent verify <ID>          Verify a signed intent's attestation
//! atomic intent list                 List the vault's intents (attestation-aware)
//! atomic intent update <ID>          Update a stored intent's fields
//! atomic intent delete <ID>          Delete an unstarted backlog intent
//! atomic intent link <ID> --goal <G> Link a goal to a stored intent
//! ```
//!
//! # Persistence — additive by construction
//!
//! Attestation NEVER mutates the stored `VaultEntry`, its `content_hash`, or the
//! manifest merkle. The attested node + proof are written as a plain file under
//! `.atomic/canonical/intents/<id>/` (the engine-internal metadata dir), outside
//! the `.vault/` materialized tree and outside redb. See [`bridge`].

use clap::{Parser, Subcommand};

use crate::commands::Command;
use crate::error::CliResult;

pub mod attest;
pub mod bridge;
pub mod delete;
pub mod link;
pub mod list;
pub mod new;
pub mod show;
pub mod update;
pub mod validate;
pub mod verify;

pub use attest::IntentAttest;
pub use delete::IntentDelete;
pub use link::IntentLink;
pub use list::IntentList;
pub use new::IntentNew;
pub use show::IntentShow;
pub use update::IntentUpdate;
pub use validate::IntentValidate;
pub use verify::IntentVerify;

/// Subcommands for the canonical intent engine.
///
/// Doc comments here are the terse `--help` `about` lines (agent-native).
/// Worked examples and the full lifecycle live in the docs site:
/// <https://docs.atomic.dev/commands/intent>.
#[derive(Subcommand, Debug)]
pub enum IntentCommands {
    /// Validate an intent against the canonical shapes (the authoring gate).
    Validate(IntentValidate),

    /// Show an intent as a rendered read-time projection.
    Show(IntentShow),

    /// Scaffold a new directive-based intent into the vault.
    New(IntentNew),

    /// Attest an intent: sign it into a tracked attestation entry.
    Attest(IntentAttest),

    /// Verify a signed intent's attestation.
    Verify(IntentVerify),

    /// List the vault's intents, attestation-aware.
    List(IntentList),

    /// Update a stored intent's fields (status, priority, title, body).
    Update(IntentUpdate),

    /// Delete an unstarted backlog intent.
    Delete(IntentDelete),

    /// Link a goal to a stored intent.
    Link(IntentLink),
}

/// Record the "why": lift, validate, attest, and render canonical intents.
///
/// A sibling of `atomic vault` that drives the `atomic-canonical` engine over
/// real vault intents, persisting attestations additively — the stored vault
/// entries are never mutated. Full lifecycle and examples:
/// <https://docs.atomic.dev/commands/intent>.
#[derive(Debug, clap::Args)]
#[command(name = "intent")]
pub struct Intent {
    #[command(subcommand)]
    pub command: IntentCommands,
}

impl Command for Intent {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            IntentCommands::Validate(cmd) => cmd.run(),
            IntentCommands::Show(cmd) => cmd.run(),
            IntentCommands::New(cmd) => cmd.run(),
            IntentCommands::Attest(cmd) => cmd.run(),
            IntentCommands::Verify(cmd) => cmd.run(),
            IntentCommands::List(cmd) => cmd.run(),
            IntentCommands::Update(cmd) => cmd.run(),
            IntentCommands::Delete(cmd) => cmd.run(),
            IntentCommands::Link(cmd) => cmd.run(),
        }
    }
}

/// A validation-failure error: the canonical gate reported a non-conforming
/// node. Kept as a `Parser`-free helper so `main.rs`'s `err.exit_code()` fires
/// with a distinct, user-fixable exit code (2, a usage/authoring error) rather
/// than the internal-bug code.
pub(crate) fn validation_failed(message: impl Into<String>) -> crate::error::CliError {
    crate::error::CliError::InvalidArgument {
        message: message.into(),
    }
}
