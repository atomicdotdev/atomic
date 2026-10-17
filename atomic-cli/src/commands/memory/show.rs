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

/// The pure read-time projection: lift then render. No gate, no proof
/// requirement — this must work on a plain (un-attested) memory; a
/// freeform memory renders its stored body. If a FRESH attestation
/// exists we project that (so `show` reflects the attested
/// author/proof); a stale one warns and falls back to the raw node.
/// Shared by the local body (repo-read inputs) and the routed hook
/// (wire-carried inputs) — one render, two data sources.
pub(crate) fn project(
    id: &str,
    inputs: &bridge::MemLiftInputs,
    attestation: &bridge::Attestation,
    json: bool,
) -> CliResult<()> {
    if bridge::is_freeform_memory(inputs) {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "path": bridge::normalize_memory_path(id),
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

    let node = match attestation {
        bridge::Attestation::Fresh(node) => (**node).clone(),
        bridge::Attestation::Stale(_) => {
            eprintln!(
                "warning: the attestation for {id} is stale; showing the current \
                 (un-attested) memory."
            );
            bridge::lift(inputs)?
        }
        bridge::Attestation::None => bridge::lift(inputs)?,
    };

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&node.to_value()).unwrap()
        );
    } else {
        print!("{}", render_memory(&node, Target::Cli));
    }
    Ok(())
}

impl Command for MemoryShow {
    fn run(&self) -> CliResult<()> {
        // Every form routes: the wire carries the lift inputs plus the raw
        // attestation sources, so the projection runs client-side with
        // the SAME bridge code over wire data.
        if crate::commands::rpc::memory_show(self)? {
            return Ok(());
        }

        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        let inputs = bridge::read_memory(&repo, &self.id)?;
        let attestation = bridge::load_attestation(&repo, &self.id, &inputs)?;
        project(&self.id, &inputs, &attestation, self.json)
    }
}
