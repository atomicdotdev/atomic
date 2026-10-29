//! `atomic diagnostics` — a read-only support report.
//!
//! Collects the context a support conversation otherwise asks for one
//! follow-up at a time: the Atomic version and install source, installed
//! agent integration packages and their versions, installed skills, which
//! agents have Atomic hooks enabled, and whether the Git shim is enabled.
//!
//! Values that cannot be read are reported as unknown with a safe reason.
//! No configuration is written, repaired, or fetched; normal command logging
//! still applies. Parser errors do not include source text, and paths under a
//! known home directory are shown as `~`. Review metadata and paths before
//! sharing the report, since user-controlled values may contain secrets.
//!
//! # Examples
//!
//! ```text
//! # Paste-ready text report
//! atomic diagnostics
//!
//! # The same facts as one JSON document
//! atomic diagnostics --json
//! ```

mod collect;
mod render;

use clap::Args;

use crate::commands::Command;
use crate::error::CliResult;

/// Print a read-only support report.
#[derive(Debug, Args)]
pub struct Diagnostics {
    /// Print the report as one JSON document.
    #[arg(long)]
    json: bool,
}

impl Command for Diagnostics {
    fn run(&self) -> CliResult<()> {
        let report = collect::collect();
        if self.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&report)
                    .expect("the report is plain data and always serializes")
            );
        } else {
            print!("{}", render::render_text(&report));
        }
        Ok(())
    }
}
