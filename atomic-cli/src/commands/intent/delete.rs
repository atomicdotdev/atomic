//! `atomic intent delete <ID>` — discard an unstarted backlog intent.
//!
//! The canonical-family analogue of `atomic vault intent delete`. It delegates
//! to the exact same [`Repository::vault_intent_delete`] path (and the same
//! "unstarted backlog, no linked goals" guard it enforces), so its behavior is
//! byte-for-byte identical to the vault verb it supersedes.

use clap::Parser;

use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::agent_doc::{Doc, Fail, Ref};

/// Delete an unstarted backlog intent.
///
/// Removes an intent that has not left backlog and has no linked goals. Use this
/// for accidental drafts only; started work should be closed or superseded
/// instead of deleted. Without `--force`, prompts for confirmation.
#[derive(Parser, Debug)]
#[command(name = "delete")]
pub struct IntentDelete {
    /// Intent ID (e.g. "PIMO-1" or "1").
    pub id: String,

    /// Skip the confirmation prompt.
    #[arg(long, short = 'f')]
    pub force: bool,

    /// Output as JSON.
    #[arg(long)]
    pub json: bool,
}

/// Agent guidance for `atomic intent delete`.
///
/// The first `fails:` row is the one that matters: without `--force` on a
/// non-tty — every agent invocation — the confirmation prompt fails and is
/// reported as an INTERNAL error, exit 128, complete with a "please report this
/// bug" hint. The row exists so an agent passes --force instead of filing an
/// issue.
pub const DOC: Doc = Doc {
    when: "a draft must be discarded before work starts",
    run: "intent delete <ID> --force",
    then: &[
        Ref {
            cmd: "intent list --json",
            note: "confirm it is gone",
        },
    ],
    instead: &[
        Ref {
            cmd: "intent update <ID> --status icebox",
            note: "park started work instead",
        },
    ],
    fails: &[
        Fail {
            cond: "no --force and no tty (every agent call)",
            exit: 128,
            fix: Ref {
                cmd: "intent delete <ID> --force",
                note: "",
            },
        },
        Fail {
            cond: "status is not backlog",
            exit: 3,
            fix: Ref {
                cmd: "intent update <ID> --status backlog",
                note: "",
            },
        },
        Fail {
            cond: "the intent is linked to a goal",
            exit: 3,
            fix: Ref {
                cmd: "intent update <ID> --status icebox",
                note: "",
            },
        },
    ],
    ..Doc::EMPTY
};

impl Command for IntentDelete {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        if !self.force {
            let prompt = format!(
                "Discard intent '{}'? This removes the unstarted draft from the vault.",
                self.id
            );
            let confirmed = dialoguer::Confirm::new()
                .with_prompt(&prompt)
                .default(false)
                .interact()
                .map_err(|e| CliError::Internal(anyhow::anyhow!("Prompt failed: {}", e)))?;

            if !confirmed {
                return Err(CliError::Cancelled);
            }
        }

        let result = repo
            .vault_intent_delete(&self.id)
            .map_err(CliError::Repository)?;

        if self.json {
            let json = serde_json::json!({
                "id": result.id,
                "intent_file": result.intent_file,
                "deleted": true,
            });
            println!("{}", serde_json::to_string_pretty(&json).unwrap());
        } else {
            println!("Deleted intent: {}", result.id);
            println!("  file: .vault/{}", result.intent_file);
        }

        Ok(())
    }
}
