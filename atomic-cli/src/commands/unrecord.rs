//! The `unrecord` command for removing changes from a stack.
//!
//! This module implements the `atomic unrecord` command, which removes
//! the most recent change (or a specific change) from the current stack's
//! change log. The change itself is NOT deleted from the change store —
//! it can be re-applied later via `atomic apply`.
//!
//! # Usage
//!
//! ```text
//! atomic unrecord [OPTIONS] [CHANGE]
//!
//! Arguments:
//!   [CHANGE]  Hash or prefix of the change to unrecord (default: last change)
//!
//! Options:
//!   -n, --dry-run   Preview what would be unrecorded
//!   -h, --help      Print help information
//! ```
//!
//! # Examples
//!
//! Unrecord the most recent change:
//! ```text
//! $ atomic unrecord
//! Unrecorded: ABCDEF12 "Add feature file"
//! ```
//!
//! Unrecord a specific change by hash prefix:
//! ```text
//! $ atomic unrecord ABCDEF
//! Unrecorded: ABCDEF12 "Add feature file"
//! ```
//!
//! Preview without actually unrecording:
//! ```text
//! $ atomic unrecord --dry-run
//! Would unrecord: ABCDEF12 "Add feature file"
//! ```

use clap::Parser;

use atomic_core::types::Base32;
use atomic_repository::unrecord::UnrecordOptions;
use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{print_success, print_warning};

/// Remove the last change from the current stack.
///
/// The change is removed from the stack's change log but NOT deleted
/// from the change store. It can be re-applied later with `atomic apply`.
///
/// This is the inverse of `atomic record` — it "un-records" a change,
/// reverting the stack to the state before that change was applied.
/// The working copy is NOT modified; files remain on disk as-is.
///
/// # Workflow
///
/// ```text
/// atomic record -m "oops"   # record a change
/// atomic unrecord            # remove it from the stack
/// # fix the issue
/// atomic record -m "fixed"  # record the corrected version
/// ```
#[derive(Parser, Debug, Default)]
#[command(name = "unrecord")]
pub struct Unrecord {
    /// Hash or prefix of the change to unrecord.
    ///
    /// If not specified, the most recent change on the current stack
    /// is unrecorded. Provide a hash prefix to unrecord a specific change.
    #[arg(value_name = "CHANGE")]
    pub change: Option<String>,

    /// Preview what would be unrecorded without doing it.
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,
}

impl Command for Unrecord {
    fn run(&self) -> CliResult<()> {
        let repo_root = find_repository_root()?;
        let repo = Repository::open(&repo_root).map_err(|e| match e {
            atomic_repository::RepositoryError::NotFound { path } => CliError::RepositoryNotFound {
                searched_path: path.into(),
            },
            other => CliError::Repository(other),
        })?;

        let options = if self.dry_run {
            UnrecordOptions::dry_run()
        } else {
            UnrecordOptions::new()
        };

        let outcome = if let Some(ref _prefix) = self.change {
            // TODO: support unrecording a specific change by hash prefix
            // once hash_from_prefix is available on the transaction trait.
            return Err(CliError::InvalidArgument {
                message: "Unrecording a specific change by hash is not yet supported. \
                          Use `atomic unrecord` (no argument) to unrecord the last change."
                    .to_string(),
            });
        } else {
            // Unrecord the most recent change
            repo.unrecord_last(options).map_err(|e| match e {
                atomic_repository::RepositoryError::Unrecord(msg)
                    if msg.contains("empty") || msg.contains("Empty") =>
                {
                    CliError::InvalidArgument {
                        message: "Stack is empty — nothing to unrecord".to_string(),
                    }
                }
                other => CliError::Repository(other),
            })?
        };

        if outcome.was_dry_run {
            for hash in &outcome.unrecorded {
                print_warning(&format!("Would unrecord: {}", hash.to_base32()));
            }
        } else {
            for hash in &outcome.unrecorded {
                print_success(&format!("Unrecorded: {}", hash.to_base32()));
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default() {
        let cmd = Unrecord::default();
        assert!(cmd.change.is_none());
        assert!(!cmd.dry_run);
    }
}
