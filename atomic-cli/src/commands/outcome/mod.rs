//! `atomic outcome` — collapse a set of views into one rollup.
//!
//! Where `atomic triage review` answers "is *this* view ready to promote?" for
//! one source and one target, an outcome answers the question above that:
//! given **N** views, what would promoting all of them at once cost, touch,
//! and collide on?
//!
//! The report is a read-only projection. It creates no records, moves no
//! changes, and writes nothing to disk.

use clap::{Parser, Subcommand};
use clap_complete::engine::ArgValueCompleter;

use atomic_repository::Repository;

use crate::commands::complete::complete_view_names;
use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};

pub mod output;

/// Roll up a set of views into a single outcome.
#[derive(Debug, Parser)]
#[command(after_help = "\
<VIEW> defaults to the current view; --into defaults to that view's parent. Name
several views to ask what promoting all of them at once would cost.

Examples:
  # What would promoting the current view cost?
  atomic outcome generate
  atomic outcome generate --json

  # A stacked promotion: several views at once
  atomic outcome generate feature-login feature-payments --into dev
  atomic outcome generate feature-login feature-payments --into dev --json")]
pub struct Outcome {
    #[command(subcommand)]
    pub command: OutcomeCommands,
}

impl Command for Outcome {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            OutcomeCommands::Generate(cmd) => cmd.run(),
        }
    }
}

/// Subcommands for `atomic outcome`.
#[derive(Subcommand, Debug)]
pub enum OutcomeCommands {
    /// Collapse N views into one rollup: cost, footprint, and mergeability.
    ///
    /// See this subcommand's examples.
    Generate(OutcomeGenerate),
}

/// Generate the outcome for a set of views against a target.
#[derive(Debug, Parser)]
#[command(after_help = "\
<VIEW> defaults to the current view; --into defaults to that view's parent. Name
several views to ask what promoting all of them at once would cost.

Examples:
  # What would promoting the current view cost?
  atomic outcome generate
  atomic outcome generate --json

  # A stacked promotion: several views at once
  atomic outcome generate feature-login feature-payments --into dev
  atomic outcome generate feature-login feature-payments --into dev --json")]
pub struct OutcomeGenerate {
    /// The source view(s) to roll up. Defaults to the current view.
    #[arg(
        value_name = "VIEW",
        add = ArgValueCompleter::new(complete_view_names)
    )]
    pub views: Vec<String>,

    /// The target view the sources would be promoted into. Defaults to the
    /// first source view's parent.
    #[arg(
        long,
        value_name = "VIEW",
        add = ArgValueCompleter::new(complete_view_names)
    )]
    pub into: Option<String>,

    /// Emit the full outcome as JSON.
    #[arg(long)]
    pub json: bool,
}

impl Command for OutcomeGenerate {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;
        let (sources, target) = resolve_views(&repo, &self.views, self.into.as_deref())?;

        let outcome = repo
            .outcome(&sources, &target)
            .map_err(CliError::Repository)?;

        if self.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&outcome).map_err(|e| {
                    CliError::Internal(anyhow::anyhow!("failed to serialize outcome: {e}"))
                })?
            );
            return Ok(());
        }

        output::print_outcome(&outcome);
        Ok(())
    }
}

/// Resolve the (sources, target) pair.
///
/// With no views named, the source is the current view and the target its
/// parent — the same default `atomic triage review` and a bare `atomic insert`
/// use, so the common gesture needs no arguments at all.
fn resolve_views(
    repo: &Repository,
    views: &[String],
    into: Option<&str>,
) -> CliResult<(Vec<String>, String)> {
    let sources: Vec<String> = if views.is_empty() {
        vec![repo.current_view().to_string()]
    } else {
        for name in views {
            if !repo.view_exists(name).map_err(CliError::Repository)? {
                return Err(unknown_view(repo, name, "<VIEW>"));
            }
        }
        views.to_vec()
    };

    let target = match into {
        Some(name) => {
            if !repo.view_exists(name).map_err(CliError::Repository)? {
                return Err(unknown_view(repo, name, "--into"));
            }
            name.to_string()
        }
        None => match repo
            .parent_change_count(&sources[0])
            .map_err(CliError::Repository)?
        {
            Some((parent, _)) => parent,
            None => {
                return Err(CliError::InvalidArgument {
                    message: format!(
                        "'{}' is a root view — it has no parent to promote into.\n  \
                         Pass --into <view> to choose a target (see 'atomic view list').",
                        sources[0]
                    ),
                })
            }
        },
    };

    Ok((sources, target))
}

/// The error for a view name that does not resolve.
///
/// The usual cause is typing the placeholder from the help text instead of a
/// real view name, so the message says the argument is optional and names the
/// current view as the value to simply drop in — or the value to leave off
/// entirely.
fn unknown_view(repo: &Repository, name: &str, arg: &str) -> CliError {
    CliError::InvalidArgument {
        message: format!(
            "No view named '{name}' — {arg} takes a real view name, not a placeholder.\n  \
             Current view is '{}'; omit {arg} to roll that up instead, or run \
             'atomic view list' to see all views.",
            repo.current_view()
        ),
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    fn parse(args: &[&str]) -> OutcomeGenerate {
        OutcomeGenerate::try_parse_from(std::iter::once("generate").chain(args.iter().copied()))
            .unwrap()
    }

    #[test]
    fn source_views_are_a_optional_list() {
        assert!(parse(&[]).views.is_empty());
        assert_eq!(parse(&["a"]).views, vec!["a".to_string()]);
        assert_eq!(
            parse(&["a", "b", "c"]).views,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        assert_eq!(parse(&["--into", "dev"]).into.as_deref(), Some("dev"));
    }

    #[test]
    fn usage_advertises_both_arguments_as_optional() {
        for cmd in [OutcomeGenerate::command()] {
            for id in ["views", "into"] {
                let arg = cmd
                    .get_arguments()
                    .find(|a| a.get_id() == id)
                    .unwrap_or_else(|| panic!("missing argument `{id}`"));
                assert!(!arg.is_required_set(), "`{id}` is still required");
            }
        }
        let usage = OutcomeGenerate::command().render_usage().to_string();
        assert!(usage.contains("[VIEW]"), "usage: {usage}");
        assert!(!usage.contains("[FEATURE]"), "usage: {usage}");
    }

    #[test]
    fn help_carries_the_stacked_examples() {
        // `apply_agent_help` drops `long_about` but keeps `after_help`, so the
        // examples have to live there to be discoverable.
        let help = OutcomeGenerate::command().render_long_help().to_string();
        assert!(help.contains("Examples:"), "help missing examples:\n{help}");
        assert!(
            help.contains("feature-login feature-payments --into dev"),
            "help missing the stacked example:\n{help}"
        );
    }
}
