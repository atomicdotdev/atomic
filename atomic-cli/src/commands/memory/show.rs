//! `atomic memory show <ID>` — render a memory as a read-time projection.

use clap::Parser;

use atomic_canonical::{render_memory, Target};
use atomic_repository::Repository;

use crate::commands::memory::bridge;
use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::agent_doc::{Doc, Fail, Ref};

/// Show a memory as a rendered read-time projection.
///
/// Unlike the retired raw `vault memory show` (which dumped the stored body),
/// this projects the lifted canonical node.
#[derive(Parser, Debug)]
#[command(name = "show")]
pub struct MemoryShow {
    /// Memory id (or `memory/<id>.md` path).
    pub id: String,

    /// Output the canonical node as JSON-LD instead of the rendered view.
    #[arg(long)]
    pub json: bool,
}

/// Agent guidance for `atomic memory show`.
pub const DOC: Doc = Doc {
    when: "you have a memory id and need its text and sign state",
    // NOT `--json`: the two `then:` refs branch on the `signed: yes|no` line,
    // and the JSON projection emits `proof`/`attributedTo` with no `signed` key
    // at all — an agent following the `run:` would never see what they name.
    run: "memory show <id>",
    needs: &[
        Ref {
            cmd: "vault sync",
            note: "after hand-editing .vault/memory/<id>.md",
        },
    ],
    then: &[
        Ref {
            cmd: "memory verify <id>",
            note: "when it printed signed: yes",
        },
        Ref {
            cmd: "memory attest <id>",
            note: "when it printed signed: no",
        },
    ],
    instead: &[
        Ref {
            cmd: "vault show memory/<id>.md",
            note: "raw stored bytes, no lift and no gate",
        },
        Ref {
            cmd: "vault context \"<terms>\"",
            note: "when you do not have an id yet",
        },
    ],
    fails: &[
        Fail {
            cond: "entry came from memory write: no uid, cannot lift",
            exit: 2,
            fix: Ref {
                cmd: "vault show memory/<id>.md",
                note: "",
            },
        },
        Fail {
            cond: ".vault edited but not synced: prints the pre-edit text",
            exit: 0,
            fix: Ref {
                cmd: "vault sync",
                note: "",
            },
        },
        Fail {
            cond: "stale attestation: warns, prints signed: no",
            exit: 0,
            fix: Ref {
                cmd: "memory attest <id>",
                note: "",
            },
        },
    ],
    ..Doc::EMPTY
};

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
