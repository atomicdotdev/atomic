//! `atomic provenance` — project & sign W3C PROV over captured provenance.
//!
//! A top-level command (a sibling of `atomic intent` / `atomic memory`) that
//! PROJECTS the per-turn `ProvenanceGraph` atomic already captures into a signed
//! W3C PROV JSON-LD named subgraph. This is a PROJECTION, not new capture: the
//! agent's capture path is never touched.
//!
//! # Verbs
//!
//! ```text
//! atomic provenance trace <CHANGE>   Walk the flywheel chain for a change
//! atomic provenance show  <CHANGE>   Emit the signed PROV JSON-LD @graph
//! ```
//!
//! # Compute-on-demand — writes nothing
//!
//! Both verbs only READ (`find_provenance_for_change` + disk-scan fallback +
//! `load_provenance_graph`) and project+sign in memory. No new `VaultEntryType`,
//! no stored entry, no sidecar, no `save_provenance_graph`, no `content_hash` /
//! manifest-merkle change. The person's real `did:atomic` signs the projection;
//! the `SoftwareAgent` is a NON-VERIFIABLE descriptive label
//! (`urn:atomic:agent:<slug>`, never a DID).

use clap::{Parser, Subcommand};

use crate::commands::Command;
use crate::error::CliResult;

pub mod command;
pub mod mapping;

pub use command::{ProvenanceShow, ProvenanceTrace};

/// Subcommands for projecting/tracing W3C PROV over captured provenance.
#[derive(Subcommand, Debug)]
pub enum ProvenanceCommands {
    /// Walk the human-readable flywheel chain for a change.
    ///
    /// Loads the change's per-turn provenance graph, projects it, and prints the
    /// chain: the activity that generated the change, what it generated, the
    /// agent label and person, and the parent turn (walking `previous`). No
    /// identity is required for the plain chain; `--json` emits the signed
    /// PROV JSON-LD `@graph` (identical to `show --json`).
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic provenance trace ABCDEF
    /// atomic provenance trace urn:atomic:change:<base32>
    /// atomic provenance trace ABCDEF --json
    /// ```
    Trace(ProvenanceTrace),

    /// Emit the signed W3C PROV JSON-LD named subgraph for a change.
    ///
    /// Projects the change's provenance graph and signs it on the fly with the
    /// person's identity — the verifiable artifact you hand an auditor. It
    /// carries a top-level `attributedTo`/`contentHash`/`proof` envelope (the
    /// person's `did:atomic` signs the whole subgraph); this one-line divergence
    /// from the doc example is intentional and documented in `atomic-canonical`'s
    /// `prov` module.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic provenance show ABCDEF
    /// atomic provenance show ABCDEF --identity alice-work
    /// ```
    Show(ProvenanceShow),
}

/// Project & trace W3C PROV over the provenance atomic already captures.
///
/// A sibling of `atomic intent` / `atomic memory`. Read-only and
/// compute-on-demand: it projects the per-turn `ProvenanceGraph` into signed
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
        }
    }
}
