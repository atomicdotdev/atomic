//! The `unrecord` command for removing changes from a view.
//!
//! This module implements the `atomic unrecord` command, which removes
//! the most recent change (or a specific change) from the current view's
//! change log. The change itself is NOT deleted from the change store —
//! it can be re-inserted later via `atomic insert`.
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

use atomic_core::types::{Base32, Hash};
use atomic_repository::unrecord::UnrecordOptions;
use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{print_success, print_warning};

/// Remove a change from the current view (default: the last change).
///
/// The change is removed from the view's change log but NOT deleted
/// from the change store. It can be re-inserted later with `atomic insert`.
///
/// This is the inverse of `atomic record` — it "un-records" the selected
/// change while retaining the other changes on the view.
/// The working copy is NOT modified; files remain on disk as-is.
/// Changes required by other changes in this view cannot be unrecorded.
///
/// # Workflow
///
/// ```text
/// atomic record -m "oops"   # record a change
/// atomic unrecord            # remove it from the view
/// # fix the issue
/// atomic record -m "fixed"  # record the corrected version
/// ```
#[derive(Parser, Debug, Default)]
#[command(name = "unrecord")]
pub struct Unrecord {
    /// Hash or prefix of the change to unrecord.
    ///
    /// If not specified, the most recent change on the current view
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

        let outcome = if let Some(ref prefix) = self.change {
            let hash = resolve_change(&repo, prefix)?;
            repo.unrecord(&hash, options)
                .map_err(CliError::Repository)?
        } else {
            // Unrecord the most recent change
            repo.unrecord_last(options).map_err(|e| match e {
                atomic_repository::RepositoryError::Unrecord(msg)
                    if msg.contains("empty") || msg.contains("Empty") =>
                {
                    CliError::InvalidArgument {
                        message: "View is empty — nothing to unrecord".to_string(),
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

fn resolve_change(repo: &Repository, prefix: &str) -> CliResult<Hash> {
    let prefix = prefix.to_ascii_uppercase();
    if prefix.is_empty()
        || prefix.len() > 52
        || !prefix
            .bytes()
            .all(|b| b.is_ascii_uppercase() || (b'2'..=b'7').contains(&b))
    {
        return Err(CliError::InvalidArgument {
            message: "Expected a Base32 change hash or a non-empty unique prefix".into(),
        });
    }

    // Resolve against the whole change store so a hash that exists only on
    // another view gets the repository's explicit membership error.
    let mut found = None;
    for entry in repo.iter_changes() {
        let hash = entry.map_err(|e| CliError::Internal(anyhow::anyhow!("{e}")))?;
        if hash.to_base32().starts_with(&prefix) {
            if found.is_some() {
                return Err(CliError::AmbiguousHash { hash: prefix });
            }
            found = Some(hash);
        }
    }
    found.ok_or(CliError::ChangeNotFound { hash: prefix })
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
