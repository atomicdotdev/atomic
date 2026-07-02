//! `atomic memory show <ID>` — render a memory as a read-time projection.

use clap::Parser;

use atomic_canonical::{render_memory, Target};
use atomic_repository::Repository;

use crate::commands::memory::bridge;
use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};

/// Show a memory as a rendered read-time projection.
///
/// Distinct from [`crate::commands::vault::memory::MemoryShow`], which dumps the
/// raw stored body. This projects the lifted canonical node.
#[derive(Parser, Debug)]
#[command(name = "show")]
pub struct MemoryShow {
    /// Memory id (or `memory/<id>.md` path).
    pub id: String,

    /// Output the canonical node as JSON-LD instead of the rendered view.
    #[arg(long)]
    pub json: bool,
}

impl Command for MemoryShow {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        // Pure read-time projection: lift then render. No gate, no proof
        // requirement — this must work on a plain (un-attested) memory. If a
        // FRESH attestation exists we project that (so `show` reflects the
        // attested author/proof); a stale one warns and falls back to the raw
        // node.
        let inputs = bridge::read_memory(&repo, &self.id)?;
        let node = match bridge::load_attestation(&repo, &self.id, &inputs)? {
            bridge::Attestation::Fresh(node) => *node,
            bridge::Attestation::Stale(_) => {
                eprintln!(
                    "warning: the attestation for {} is stale; showing the current \
                     (un-attested) memory.",
                    self.id
                );
                bridge::lift(&inputs)?
            }
            bridge::Attestation::None => bridge::lift(&inputs)?,
        };

        if self.json {
            println!("{}", serde_json::to_string_pretty(&node.to_value()).unwrap());
        } else {
            print!("{}", render_memory(&node, Target::Cli));
        }

        Ok(())
    }
}
