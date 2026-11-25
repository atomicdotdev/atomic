//! Local recorded-source inspection and accountable native review publication.
//!
//! The command owns execution; skills only select the scope and explain results.

mod analysis;
mod graph;
mod html;
mod impact;
mod incremental;
mod metric_output;
mod presentation;
mod progress;
mod publish;
mod risk;
mod risk_facts;
mod terminal;
mod view_comparison;
mod view_snapshot;

use crate::commands::Command;
use crate::error::CliResult;
use clap::{Args, Subcommand};

/// Review the current recorded intent/change, or publish a frozen review scope.
#[derive(Debug, Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct Review {
    #[command(flatten)]
    incremental: incremental::IncrementalArgs,
    #[command(subcommand)]
    action: Option<Action>,
}

#[derive(Debug, Subcommand)]
enum Action {
    /// Attest a native source-to-target review and explicit risk evaluation; no LLM.
    Publish(publish::PublishArgs),
}

impl Command for Review {
    fn run(&self) -> CliResult<()> {
        match &self.action {
            Some(Action::Publish(args)) => publish::run(args),
            None => incremental::run(&self.incremental),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        review: Review,
    }

    #[test]
    fn default_is_incremental_and_needs_no_agent() {
        assert!(Cli::try_parse_from(["review"])
            .unwrap()
            .review
            .action
            .is_none());
    }

    #[test]
    fn publication_uses_native_source_to_target_scope_without_a_runner() {
        let cli = Cli::try_parse_from([
            "review",
            "publish",
            "--from",
            "draft",
            "--to",
            "dev",
            "--dry-run",
        ])
        .unwrap();
        assert!(matches!(cli.review.action, Some(Action::Publish(_))));
        assert!(Cli::try_parse_from(["review", "publish", "--views", "a,b"]).is_err());
        assert!(Cli::try_parse_from(["review", "publish", "--runner", "opencode"]).is_err());
        assert!(Cli::try_parse_from(["review", "publish", "--session", "session"]).is_err());
        assert!(Cli::try_parse_from(["review", "publish", "--resume", "job"]).is_err());
    }

    #[test]
    fn inspection_selectors_cannot_leak_into_publication() {
        assert!(Cli::try_parse_from(["review", "--intent", "work", "publish"]).is_err());
    }

    #[test]
    fn bundled_skill_is_a_thin_wrapper_for_the_implemented_routes() {
        let skill = include_str!("../../../skills/review/SKILL.md");
        assert!(skill.contains("atomic review --json"));
        assert!(skill.contains("atomic review publish --from draft --to dev"));
        assert!(!skill.contains("identity agent create"));
        assert!(!skill.contains("smolvm machine"));
        assert!(skill.contains("changes[].analysis"));
        let contract = include_str!("../../../skills/review/references/review-contract.md");
        assert!(contract.contains("introduced_by_change"));
        assert!(contract.contains("Local semantic audit pass"));
        for flag in [
            "--review-model",
            "--review-runner",
            "--review-timeout-seconds",
        ] {
            assert_eq!(
                Cli::try_parse_from(["review", flag, "unused"])
                    .err()
                    .unwrap()
                    .kind(),
                clap::error::ErrorKind::UnknownArgument
            );
        }
        assert_eq!(
            Cli::try_parse_from(["review", "--preflight-only"])
                .err()
                .unwrap()
                .kind(),
            clap::error::ErrorKind::UnknownArgument
        );
        for args in [
            vec!["review", "--json"],
            vec!["review", "publish", "--from", "draft", "--to", "dev"],
            vec![
                "review",
                "publish",
                "--from",
                "draft",
                "--to",
                "dev",
                "--dry-run",
                "--json",
            ],
        ] {
            assert!(Cli::try_parse_from(args).is_ok());
        }
    }
}
