//! `atomic vault sync` — sync vault markdown files back to the database.
//!
//! Reads vault markdown files from the working copy (`.vault/`)
//! and updates the vault database with any changes found on disk.
//!
//! # Usage
//!
//! ```text
//! atomic vault sync
//! ```
//!
//! # Examples
//!
//! ```text
//! # Sync all vault files back to the database
//! $ atomic vault sync
//!   synced: memory/architecture.md
//!   synced: memory/conventions.md
//!
//! Synced 2 vault files.
//! ```

use clap::Parser;

use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::agent_doc::{Doc, Fail, Ref};

/// Sync vault markdown files back to the database.
///
/// Reads vault files from the `.vault/` directory on disk and
/// updates the vault database to match. This is the inverse of
/// `atomic vault materialize`.
#[derive(Parser, Debug)]
#[command(name = "sync")]
pub struct Sync;

/// Agent guidance for `atomic vault sync`.
///
/// The `fails:` row is the destructive twin of the round trip: sync propagates
/// DELETIONS as readily as edits and reports success either way, so an entry
/// missing from disk is removed from the database and `vault materialize` will
/// not bring it back. Never sync a partially-materialized tree.
pub const DOC: Doc = Doc {
    when: "a .vault file was hand-edited; reads show stale text",
    run: "vault sync",
    needs: &[
        Ref {
            cmd: "vault init",
            note: "if the repo has no vault yet",
        },
        Ref {
            cmd: "vault materialize",
            note: "disk must hold every entry first",
        },
    ],
    then: &[
        Ref {
            cmd: "memory show <id>",
            note: "reads the synced body",
        },
        Ref {
            cmd: "intent validate <id>",
            note: "",
        },
        Ref {
            cmd: "intent attest <id>",
            note: "",
        },
    ],
    instead: &[
        Ref {
            cmd: "vault materialize",
            note: "the inverse: db -> disk",
        },
    ],
    fails: &[
        // `vault materialize` is prevention, NOT recovery: it reports success
        // and restores nothing, so it must not appear as the `fix`. Verified —
        // rm an entry, sync ("Synced 1 vault files.", exit 0), materialize
        // ("Materialized 4 vault entries.", exit 0), show -> not found.
        Fail {
            cond: "an entry missing from disk is deleted from the db, unrecoverably",
            exit: 0,
            fix: Ref::UNFIXABLE,
        },
    ],
    ..Doc::EMPTY
};

impl Command for Sync {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        let updated = repo
            .vault_record_working_copy()
            .map_err(CliError::Repository)?;

        if updated.is_empty() {
            println!("Vault is up to date.");
        } else {
            for path in &updated {
                println!("  synced: {}", path);
            }
            println!("\nSynced {} vault files.", updated.len());
        }

        Ok(())
    }
}
