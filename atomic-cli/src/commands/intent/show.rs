//! `atomic intent show <ID>` — render an intent as a read-time projection.

use clap::Parser;

use atomic_canonical::{render, Target};
use atomic_repository::Repository;

use crate::commands::intent::bridge;
use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::agent_doc::{Doc, Fail, Ref};

/// Show an intent as a rendered read-time projection.
///
/// Projects the lifted canonical node (as opposed to dumping the raw stored
/// body).
#[derive(Parser, Debug)]
#[command(name = "show")]
pub struct IntentShow {
    /// Intent ID (e.g. "PIMO-1" or "1").
    pub id: String,

    /// Output the canonical node as JSON-LD instead of the rendered view.
    #[arg(long)]
    pub json: bool,
}

/// Agent guidance for `atomic intent show`.
pub const DOC: Doc = Doc {
    when: "need an intent's why, criteria and scope before acting",
    run: "intent show <ID> --json",
    then: &[
        Ref {
            cmd: "intent validate <ID> --json",
            note: "is it still conformant",
        },
    ],
    instead: &[
        Ref {
            cmd: "vault show intents/<uid>/intent.md",
            note: "raw stored markdown, no lift",
        },
        Ref {
            cmd: "query neighbors intent:<ULID>",
            note: "KG edges; the urn form returns nothing",
        },
    ],
    fails: &[
        Fail {
            cond: ".vault file edited but not synced: stale projection",
            exit: 0,
            fix: Ref {
                cmd: "vault sync",
                note: "",
            },
        },
        Fail {
            cond: "stale attestation: warns, shows the un-attested node",
            exit: 0,
            fix: Ref {
                cmd: "intent attest <ID>",
                note: "",
            },
        },
    ],
    ..Doc::EMPTY
};

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
