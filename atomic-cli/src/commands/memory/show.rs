//! `atomic memory show <ID>` — show canonical and freeform memories.

use clap::Parser;

use atomic_canonical::{render_memory, Target};
use atomic_repository::Repository;

use crate::commands::memory::bridge;
use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};

/// Show a canonical memory as a rendered projection, or a freeform memory as
/// its stored body.
#[derive(Parser, Debug)]
#[command(name = "show")]
pub struct MemoryShow {
    /// Memory id (or `memory/<id>.md` path).
    pub id: String,

    /// Output JSON instead of the rendered view or raw body.
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
        if bridge::is_freeform_memory(&inputs) {
            if self.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "path": bridge::normalize_memory_path(&self.id),
                        "content": inputs.body,
                        "frontmatter": inputs.frontmatter,
                    }))
                    .unwrap()
                );
            } else {
                print!("{}", inputs.body);
            }
            return Ok(());
        }

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
            println!(
                "{}",
                serde_json::to_string_pretty(&node.to_value()).unwrap()
            );
        } else {
            print!("{}", render_memory(&node, Target::Cli));
        }

        Ok(())
    }
}
