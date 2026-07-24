//! `atomic intent` — the canonical "record the why" engine surface.
//!
//! This is a NEW top-level command, a sibling of `atomic vault` (see
//! [`crate::commands::vault`]). It is distinct from the vault-scoped
//! `atomic vault intent ...` tree, which still ships and is marked
//! DEPRECATED: that tree manages the raw vault entries
//! (create/list/show/update/delete/link), while this one drives the
//! `atomic-canonical` engine — lifting a stored intent into a canonical
//! JSON-LD node, gating it, attesting it, and rendering it.
//!
//! The two are not yet output-compatible (`atomic intent list --json` omits
//! titles, `atomic intent new` has no `--priority`/`--json`), so the
//! deprecated tree cannot be removed until a migration lands that also
//! updates the vault skills embedded in `vault_defaults.rs`.
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
#[derive(Subcommand, Debug)]
pub enum IntentCommands {
    /// Validate an intent against the canonical shapes (the authoring gate).
    ///
    /// Lifts the intent (by ID from the vault, or from a markdown file path)
    /// into a canonical node and runs the SHACL-style gate. An un-attested
    /// vault intent will report violations for the missing proof and (because
    /// the vault stores `created_by`, not `attributedTo`) the missing author —
    /// this is correct: `validate` is the authoring check, `attest` is what
    /// makes it conform. Exits nonzero if the report does not conform.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic intent validate PIMO-1
    /// atomic intent validate ./plan.md
    /// atomic intent validate PIMO-1 --json
    /// ```
    Validate(IntentValidate),

    /// Show an intent as a rendered read-time projection.
    ///
    /// Lifts the intent and renders it via the canonical projection. This is a
    /// pure read: it does not gate and does not require a proof, so it works on
    /// a plain (un-attested) intent.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic intent show PIMO-1
    /// atomic intent show PIMO-1 --json
    /// ```
    Show(IntentShow),

    /// Scaffold a new directive-based intent into the vault.
    ///
    /// Emits a body built from the closed directive vocabulary
    /// (`:::why`, `:::acceptance-criterion`, `:::scope-in`, `:::scope-out`,
    /// `:::constraint`) so it round-trips cleanly through
    /// lift → validate → attest.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic intent new "Fix the login flow"
    /// atomic intent new "Add OAuth" --template feature
    /// ```
    New(IntentNew),

    /// Attest an intent: sign it into a tracked attestation entry.
    ///
    /// Gates the intent first (refusing to sign a non-conforming node),
    /// fills `attributedTo` from the signing identity's `did:atomic`, signs
    /// the canonical node, re-gates the attested result, and writes the node
    /// (with embedded contentHash + proof) as a tracked vault entry at
    /// `attestations/<id>/attested.md` (dual-written to a legacy `.atomic/`
    /// sidecar during transition). The attestation entry uses the vault's
    /// existing `blake3(body)` storage hash; the intent's own `content_hash`
    /// is never redefined.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic intent attest PIMO-1
    /// atomic intent attest PIMO-1 --identity alice-work
    /// atomic intent attest PIMO-1 --json
    /// ```
    Attest(IntentAttest),

    /// Verify a signed intent's attestation.
    ///
    /// Reads the attestation sidecar written by `attest`, confirms it is still
    /// fresh (the intent hasn't changed since it was signed), and verifies its
    /// content hash + Ed25519 Data Integrity proof. Exits nonzero if there is
    /// no attestation, it is stale, or the signature does not verify.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic intent verify PIMO-1
    /// atomic intent verify PIMO-1 --identity alice-work
    /// ```
    Verify(IntentVerify),

    /// List the vault's intents, attestation-aware.
    ///
    /// The canonical-family analogue of `atomic vault intent list`: alongside the
    /// human key and status it adds an `attested` column (fresh / stale / –) and
    /// a `verifies` column (✓ / ✗ / –). `verifies` is `✓` only when a fresh
    /// attestation signed by the resolving identity cryptographically checks out,
    /// `✗` when a same-signer node fails its hash/signature, and `–` otherwise
    /// (no attestation, stale, no resolvable identity, or a different signer).
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic intent list
    /// atomic intent list --identity alice-work
    /// atomic intent list --json
    /// ```
    List(IntentList),

    /// Update a stored intent's fields (status, priority, title, body).
    ///
    /// The canonical-family replacement for `atomic vault intent update`. It
    /// delegates to the same repository write path, so the frontmatter/body
    /// updates and the started-or-linked guard (behind `--force`) behave
    /// identically. The body can be set inline with `--body` or piped via
    /// `--body-stdin`.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic intent update PIMO-1 --status in-progress
    /// atomic intent update PIMO-1 --assignee bob --priority critical
    /// atomic intent update PIMO-1 --body "# Fix auth\n\n## Problem\n..."
    /// cat plan.md | atomic intent update PIMO-1 --body-stdin
    /// ```
    Update(IntentUpdate),

    /// Delete an unstarted backlog intent.
    ///
    /// The canonical-family replacement for `atomic vault intent delete`. It
    /// delegates to the same repository path and enforces the same guard: only
    /// backlog intents with no linked goals can be deleted. Use `--force` to
    /// skip the confirmation prompt.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic intent delete PIMO-1
    /// atomic intent delete PIMO-1 --force
    /// ```
    Delete(IntentDelete),

    /// Link a goal to a stored intent.
    ///
    /// The canonical-family replacement for `atomic vault intent link`. It
    /// delegates to the same repository path, associating a goal with an intent
    /// so the work done in the goal is tracked against the intent.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic intent link PIMO-1 --goal swift-meadow-a3f2
    /// ```
    Link(IntentLink),
}

/// Record the "why": lift, validate, attest, and render canonical intents.
///
/// A sibling of `atomic vault`. The verbs operate on real vault intents through
/// the `atomic-canonical` engine, persisting attestations additively as
/// sidecar files under `.atomic/` — the stored vault entries are never mutated.
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
