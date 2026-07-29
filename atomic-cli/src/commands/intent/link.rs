//! `atomic intent link <ID> --goal <GOAL>` — link a goal to a stored intent.
//!
//! The canonical-family analogue of `atomic vault intent link`. It delegates to
//! the exact same [`Repository::vault_intent_link`] path, so its behavior is
//! byte-for-byte identical to the vault verb it supersedes.

use clap::Parser;

use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::agent_doc::{Doc, Fail, Ref};

/// Link a goal to an intent.
///
/// Associates a goal with an intent so that the work done in the goal is tracked
/// against the intent.
#[derive(Parser, Debug)]
#[command(name = "link")]
pub struct IntentLink {
    /// Intent ID (e.g. "PIMO-1" or "1").
    pub id: String,

    /// Goal name to link.
    #[arg(long)]
    pub goal: String,
}

/// Agent guidance for `atomic intent link`.
///
/// There is no unlink verb anywhere in the CLI, which is why
/// `atomic intent delete`'s linked-goal failure points at icebox instead.
pub const DOC: Doc = Doc {
    when: "a goal's work should be tracked against an intent",
    run: "intent link <ID> --goal <GOAL>",
    needs: &[
        Ref {
            cmd: "vault goal start --name <GOAL>",
            note: "the goal must exist first",
        },
    ],
    then: &[
        Ref {
            cmd: "intent update <ID> --status in_progress",
            note: "mark the work started",
        },
    ],
    instead: &[
        Ref {
            cmd: "vault goal start --intent <ID>",
            note: "link at goal creation",
        },
    ],
    fails: &[
        Fail {
            cond: "goal name not found",
            exit: 3,
            fix: Ref {
                cmd: "vault goal list",
                note: "",
            },
        },
    ],
    ..Doc::EMPTY
};

impl Command for IntentLink {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        repo.vault_intent_link(&self.id, &self.goal)
            .map_err(CliError::Repository)?;
        println!("Linked goal '{}' to intent '{}'", self.goal, self.id);

        Ok(())
    }
}
