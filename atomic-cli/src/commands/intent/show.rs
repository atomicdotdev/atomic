//! `atomic intent show <ID>` — render an intent as a read-time projection.

use clap::Parser;

use atomic_canonical::{render, Target};
use atomic_repository::Repository;

use crate::commands::intent::bridge;
use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};

/// Show an intent as a rendered read-time projection.
///
/// Distinct from the former `atomic vault intent show`, which dumped the
/// raw stored body. This projects the lifted canonical node.
#[derive(Parser, Debug)]
#[command(name = "show")]
pub struct IntentShow {
    /// Intent ID (e.g. "PIMO-1" or "1").
    pub id: String,

    /// Output the canonical node as JSON-LD instead of the rendered view.
    #[arg(long)]
    pub json: bool,
}

impl Command for IntentShow {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        // Pure read-time projection: lift then render. No gate, no proof
        // requirement — this must work on a plain (un-attested) intent. The
        // status line is regenerated from the spine by the renderer. If a FRESH
        // attestation sidecar exists we project that (so `show` reflects the
        // attested author/proof); a stale one warns and falls back to the raw node.
        let inputs = bridge::read_intent(&repo, &self.id)?;
        let node = match bridge::load_attestation(&repo, &self.id, &inputs)? {
            bridge::Attestation::Fresh(node) => *node,
            bridge::Attestation::Stale(_) => {
                eprintln!(
                    "warning: the attestation for {} is stale; showing the current \
                     (un-attested) intent.",
                    self.id
                );
                bridge::lift(&inputs)?
            }
            bridge::Attestation::None => bridge::lift(&inputs)?,
        };

        if self.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&node.to_value()).unwrap()
            );
        } else {
            print!("{}", render(&node, Target::Cli));
        }

        Ok(())
    }
}
