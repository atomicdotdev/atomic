//! The `view split` command for extracting changes into a new draft view.
//!
//! A split creates a new **Draft** view that forks from a source view and then
//! removes a chosen set of changes from that source. Because every change lives
//! in the single canonical graph and a view is only a change-set *filter*, this
//! is a pure metadata operation — no edges are copied and no new changes are
//! created.
//!
//! After a split of `[3,4]` out of `dev = [1,2,3,4,5,6,7]`:
//!
//! - `dev` becomes `[1,2,5,6,7]`.
//! - the new draft holds `{3,4}` in its own log and inherits the rest from
//!   `dev`, so it sees the full pre-split state `[1..7]` — the snapshot you
//!   keep iterating on.
//!
//! # Safety
//!
//! Splitting from the middle is refused when a change staying behind depends on
//! one being removed (which would leave `dev` incoherent). Use `--cascade` to
//! move those dependents along too, or `--dry-run` to preview the analysis.
//!
//! # Usage
//!
//! ```text
//! atomic view split [OPTIONS] <NAME> [CHANGES]...
//!
//! Arguments:
//!   <NAME>         Name of the new draft view
//!   [CHANGES]...   Change hashes/prefixes to split out (or use --last)
//!
//! Options:
//!       --from <VIEW>   Source view to split out of (default: current view)
//!       --last <N>      Split the last N changes of the source view
//!       --cascade       Also move changes that depend on the split-out set
//!   -n, --dry-run       Preview the split without performing it
//!   -s, --switch        Switch to the new draft after creating it
//! ```

use clap::Parser;
use clap_complete::engine::ArgValueCompleter;

use atomic_core::types::Base32;
use atomic_repository::{Repository, SplitOptions};

use crate::commands::complete::complete_view_names;
use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::view as style_view;
use crate::output::{hash as style_hash, print_hint, print_info, print_success, print_warning};

/// Split changes out of a view into a new draft view.
#[derive(Parser, Debug, Default)]
#[command(name = "split")]
pub struct Split {
    /// Name of the new draft view to create.
    #[arg(value_name = "NAME")]
    pub name: String,

    /// Change hashes (or unambiguous prefixes) to split out.
    ///
    /// Mutually exclusive with `--last`.
    #[arg(value_name = "CHANGES")]
    pub changes: Vec<String>,

    /// Source view to split out of. Defaults to the current view.
    #[arg(long = "from", value_name = "VIEW", add = ArgValueCompleter::new(complete_view_names))]
    pub from: Option<String>,

    /// Split the last N changes of the source view instead of naming them.
    #[arg(long = "last", value_name = "N", conflicts_with = "changes")]
    pub last: Option<usize>,

    /// Also move any changes that depend on the split-out set, rather than
    /// refusing the split.
    #[arg(long = "cascade")]
    pub cascade: bool,

    /// Preview what would be split without performing it.
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,

    /// Switch to the new draft view after creating it.
    #[arg(short = 's', long = "switch")]
    pub switch: bool,
}

impl Command for Split {
    fn run(&self) -> CliResult<()> {
        let repo_root = find_repository_root()?;
        let mut repo = Repository::open(&repo_root).map_err(|e| match e {
            atomic_repository::RepositoryError::NotFound { path } => CliError::RepositoryNotFound {
                searched_path: path.into(),
            },
            other => CliError::Repository(other),
        })?;

        let from_view = self
            .from
            .clone()
            .unwrap_or_else(|| repo.current_view().to_string());

        // Resolve which changes to split: either an explicit list or --last N.
        let change_hashes = self.resolve_changes(&repo, &from_view)?;

        let options = SplitOptions {
            target_view: self.name.clone(),
            from_view: Some(from_view.clone()),
            changes: change_hashes,
            cascade: self.cascade,
            dry_run: self.dry_run,
            // When switching, the draft is materialized by the switch below, so
            // there's no need to reconcile the source we're leaving. Otherwise,
            // refresh the source's working copy in place.
            materialize: !self.switch,
        };

        let outcome = repo.split_view(options).map_err(CliError::Repository)?;

        // ── Dry run: report the analysis. ──
        if outcome.was_dry_run {
            print_info(&format!(
                "Dry run: splitting {} change(s) out of {}",
                outcome.requested.len(),
                style_view(&outcome.from_view),
            ));
            for c in &outcome.requested {
                println!("  requested  {}", style_hash(c.hash.to_base32()));
            }
            if outcome.blocked {
                print_warning(&format!(
                    "Blocked: {} change(s) remaining in '{}' depend on the split-out set:",
                    outcome.dependents.len(),
                    outcome.from_view,
                ));
                for c in &outcome.dependents {
                    println!("  dependent  {}", style_hash(c.hash.to_base32()));
                }
                print_hint("Re-run with --cascade to move the dependents too.");
            } else {
                if !outcome.dependents.is_empty() {
                    print_info(&format!(
                        "{} dependent change(s) would be moved along (--cascade):",
                        outcome.dependents.len(),
                    ));
                    for c in &outcome.dependents {
                        println!("  dependent  {}", style_hash(c.hash.to_base32()));
                    }
                }
                print_info(&format!(
                    "Would create draft '{}' with {} change(s); '{}' would keep {} change(s).",
                    self.name,
                    outcome.moved.len(),
                    outcome.from_view,
                    outcome.source_change_count,
                ));
            }
            return Ok(());
        }

        // ── Real split completed. ──
        print_success(&format!(
            "Split {} change(s) out of {} into draft {}",
            outcome.moved.len(),
            style_view(&outcome.from_view),
            style_view(&self.name),
        ));
        if outcome.dependents.is_empty() {
            for c in &outcome.moved {
                println!("  moved  {}", style_hash(c.hash.to_base32()));
            }
        } else {
            for c in &outcome.requested {
                println!("  moved      {}", style_hash(c.hash.to_base32()));
            }
            for c in &outcome.dependents {
                println!("  cascaded   {}", style_hash(c.hash.to_base32()));
            }
        }
        print_info(&format!(
            "'{}' now has {} change(s); draft '{}' has {} own change(s).",
            outcome.from_view, outcome.source_change_count, self.name, outcome.target_change_count,
        ));

        if outcome.working_copy_updated && (outcome.files_written > 0 || outcome.files_removed > 0)
        {
            print_info(&format!(
                "Working copy updated: {} file(s) refreshed, {} removed.",
                outcome.files_written, outcome.files_removed,
            ));
        }

        self.maybe_switch(&mut repo)
    }
}

impl Split {
    /// Resolve the set of change hashes to split, from either the explicit
    /// positional list or `--last N`.
    fn resolve_changes(
        &self,
        repo: &Repository,
        from_view: &str,
    ) -> CliResult<Vec<atomic_core::types::Hash>> {
        use atomic_core::types::Hash;

        if let Some(n) = self.last {
            if n == 0 {
                return Err(CliError::InvalidArgument {
                    message: "--last must be greater than zero".to_string(),
                });
            }
            let all = repo
                .view_own_change_hashes(from_view)
                .map_err(CliError::Repository)?;
            if all.len() < n {
                return Err(CliError::InvalidArgument {
                    message: format!(
                        "view '{}' has only {} change(s); cannot split the last {}",
                        from_view,
                        all.len(),
                        n
                    ),
                });
            }
            return Ok(all[all.len() - n..].to_vec());
        }

        if self.changes.is_empty() {
            return Err(CliError::InvalidArgument {
                message: "specify one or more changes to split, or use --last <N>".to_string(),
            });
        }

        let mut hashes = Vec::with_capacity(self.changes.len());
        for spec in &self.changes {
            let hash = if let Some(h) = Hash::from_base32(spec.as_bytes()) {
                h
            } else {
                repo.find_change_by_prefix(spec)
                    .map_err(CliError::Repository)?
                    .ok_or_else(|| CliError::ChangeNotFound { hash: spec.clone() })?
            };
            hashes.push(hash);
        }
        Ok(hashes)
    }

    /// Optionally switch to the new draft view.
    fn maybe_switch(&self, repo: &mut Repository) -> CliResult<()> {
        if self.switch {
            let result = repo.switch_view(&self.name).map_err(CliError::Repository)?;
            print_success(&format!(
                "Switched to view: {} ({} files updated)",
                style_view(&self.name),
                result.files_written,
            ));
        } else {
            print_hint(&format!(
                "Use 'atomic view switch {}' to switch to the new draft",
                self.name
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_name_and_changes() {
        let cmd = Split::try_parse_from(["split", "wip", "ABCD", "EF12"]).unwrap();
        assert_eq!(cmd.name, "wip");
        assert_eq!(cmd.changes, vec!["ABCD".to_string(), "EF12".to_string()]);
        assert!(cmd.last.is_none());
        assert!(!cmd.cascade);
        assert!(!cmd.dry_run);
        assert!(!cmd.switch);
    }

    #[test]
    fn parses_flags() {
        let cmd = Split::try_parse_from([
            "split",
            "wip",
            "--from",
            "dev",
            "--last",
            "3",
            "--cascade",
            "--dry-run",
            "--switch",
        ])
        .unwrap();
        assert_eq!(cmd.name, "wip");
        assert_eq!(cmd.from.as_deref(), Some("dev"));
        assert_eq!(cmd.last, Some(3));
        assert!(cmd.cascade);
        assert!(cmd.dry_run);
        assert!(cmd.switch);
    }

    #[test]
    fn last_conflicts_with_explicit_changes() {
        let err = Split::try_parse_from(["split", "wip", "ABCD", "--last", "2"]);
        assert!(err.is_err(), "--last and explicit changes must conflict");
    }

    #[test]
    fn name_is_required() {
        assert!(Split::try_parse_from(["split"]).is_err());
    }
}
