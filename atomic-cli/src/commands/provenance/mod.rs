//! `atomic provenance` — project & sign W3C PROV over captured provenance.
//!
//! A read-only projection of captured provenance, unsigned by default. `--sign`
//! adds a native Data Integrity proof. With the optional `dsse-export` feature,
//! `--dsse` signs a portable export container and `verify-dsse` consumes it using
//! a caller-pinned exporter key. Neither signature establishes the truth of the
//! graph's claims or verifies referenced primaries.
//! No repository objects, sidecars or capture records are written.

use clap::{Parser, Subcommand};

use crate::commands::Command;
use crate::error::CliResult;

pub mod command;
pub mod mapping;
#[cfg(feature = "dsse-export")]
mod verify_dsse;

pub use command::{ProvenanceShow, ProvenanceTrace};

/// Subcommands for projecting/tracing W3C PROV over captured provenance.
#[derive(Subcommand, Debug)]
pub enum ProvenanceCommands {
    /// Walk the human-readable flywheel chain for a change.
    ///
    /// Loads the change's per-turn provenance graph, projects it, and prints the
    /// chain: the activity that generated the change, what it generated, the
    /// agent label and person, and the parent turn (walking `previous`). No
    /// identity is required for the plain chain; `--json` emits unsigned
    /// PROV JSON-LD (identical to `show`). Add `--sign` for a native proof.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic provenance trace ABCDEF
    /// atomic provenance trace urn:atomic:change:<base32>
    /// atomic provenance trace ABCDEF --json
    /// ```
    Trace(ProvenanceTrace),

    /// Emit W3C PROV JSON-LD for a change, unsigned unless `--sign` is supplied.
    ///
    /// `--identity` selects the Person shown in the derived projection and the
    /// exporter key when signing. It does not recover the original author.
    Show(ProvenanceShow),

    /// Verify an optional DSSE export with a pinned exporter key, emitting only
    /// the exact authenticated JSON payload. Does not verify native proofs.
    #[cfg(feature = "dsse-export")]
    VerifyDsse(verify_dsse::VerifyDsse),
}

/// Project & trace W3C PROV over the provenance atomic already captures.
///
/// A sibling of `atomic intent` / `atomic memory`. Read-only and
/// compute-on-demand: it projects the per-turn `ProvenanceGraph` into
/// PROV JSON-LD without ever touching the capture path or writing anything.
#[derive(Debug, clap::Args)]
#[command(name = "provenance")]
pub struct Provenance {
    #[command(subcommand)]
    pub command: ProvenanceCommands,
}

impl Command for Provenance {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            ProvenanceCommands::Trace(cmd) => cmd.run(),
            ProvenanceCommands::Show(cmd) => cmd.run(),
            #[cfg(feature = "dsse-export")]
            ProvenanceCommands::VerifyDsse(cmd) => cmd.run(),
        }
    }
}
