mod model;
mod output;

use atomic_core::operation::OperationScope;
use atomic_repository::Repository;
use clap::{Args, Subcommand};

use self::model::{OperationDetailsDto, OperationLogDto};
use self::output::{render_details_human, render_log_human, to_pretty_json};
use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};

/// Inspect immutable repository operations and their effect receipts.
#[derive(Debug, Args)]
pub struct Op {
    #[command(subcommand)]
    pub command: OpCommands,
}

impl Command for Op {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            OpCommands::Log(command) => command.run(),
            OpCommands::Show(command) => command.run(),
            OpCommands::Undo(command) => command.run(),
            OpCommands::Restore(command) => command.run(),
        }
    }
}

/// Operation journal inspection and inverse-transition commands.
#[derive(Debug, Subcommand)]
pub enum OpCommands {
    /// Show operations reachable from the selected scope's current heads.
    Log(OpLog),
    /// Show one operation, its full payload, receipts, and head scopes.
    Show(OpShow),
    /// Append an inverse of the sole current operation head.
    Undo(OpUndo),
    /// Re-project the verified state selected by an earlier operation.
    Restore(OpRestore),
}

/// Show deterministic causal operation history.
#[derive(Debug, Args)]
pub struct OpLog {
    /// Limit the number of operations printed.
    #[arg(short = 'n', long, value_name = "LIMIT")]
    limit: Option<usize>,

    /// Show parents before children.
    #[arg(long)]
    reverse: bool,

    /// Inspect the repository-wide operation scope instead of this working copy.
    #[arg(long)]
    repository: bool,

    /// Emit stable machine-readable JSON.
    #[arg(long)]
    json: bool,
}

impl Command for OpLog {
    fn run(&self) -> CliResult<()> {
        let repository = open_operation_repository()?;
        let scope = if self.repository {
            OperationScope::Repository
        } else {
            OperationScope::WorkingCopy(repository.working_copy_id().ok_or_else(|| {
                CliError::InvalidRepository {
                    reason: "the current working copy has no valid working_copy_id".to_string(),
                }
            })?)
        };
        let log = repository
            .operation_log(scope, self.limit, self.reverse)
            .map_err(CliError::Repository)?;
        let output = OperationLogDto::from(&log);

        if self.json {
            println!("{}", json(&output)?);
        } else {
            print!("{}", render_log_human(&output));
        }
        Ok(())
    }
}

/// Show full details for one operation ID or unambiguous prefix.
#[derive(Debug, Args)]
pub struct OpShow {
    /// Full operation ID or a case-insensitive prefix of at least four characters.
    #[arg(value_name = "ID_OR_PREFIX")]
    id_or_prefix: String,

    /// Emit stable machine-readable JSON.
    #[arg(long)]
    json: bool,
}

impl Command for OpShow {
    fn run(&self) -> CliResult<()> {
        let repository = open_operation_repository()?;
        let operation_id = repository
            .resolve_operation_id(&self.id_or_prefix)
            .map_err(CliError::Repository)?;
        let details = repository
            .operation_details(operation_id)
            .map_err(CliError::Repository)?;
        let output = OperationDetailsDto::from(&details);

        if self.json {
            println!("{}", json(&output)?);
        } else {
            print!("{}", render_details_human(&output));
        }
        Ok(())
    }
}

/// Undo the current operation head, or an explicitly selected current head.
#[derive(Debug, Args)]
pub struct OpUndo {
    /// Current operation ID or unambiguous prefix; omitted means the sole head.
    #[arg(value_name = "ID_OR_PREFIX")]
    id_or_prefix: Option<String>,

    /// Emit the newly appended operation as stable JSON.
    #[arg(long)]
    json: bool,
}

impl Command for OpUndo {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let mut repository = Repository::open(&root).map_err(CliError::Repository)?;
        let working_copy =
            repository
                .working_copy_id()
                .ok_or_else(|| CliError::InvalidRepository {
                    reason: "the current working copy has no valid working_copy_id".to_string(),
                })?;
        let target = self
            .id_or_prefix
            .as_deref()
            .map(|selector| repository.resolve_operation_id(selector))
            .transpose()
            .map_err(CliError::Repository)?;
        let operation_id = repository
            .undo_operation(working_copy, target)
            .map_err(CliError::Repository)?;
        render_transition_result(&repository, "Undo", operation_id, self.json)
    }
}

/// Restore the state selected by a verified reachable operation.
#[derive(Debug, Args)]
pub struct OpRestore {
    /// Operation ID or unambiguous prefix whose after-state should be restored.
    #[arg(value_name = "ID_OR_PREFIX")]
    id_or_prefix: String,

    /// Emit the newly appended operation as stable JSON.
    #[arg(long)]
    json: bool,
}

impl Command for OpRestore {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let mut repository = Repository::open(&root).map_err(CliError::Repository)?;
        let working_copy =
            repository
                .working_copy_id()
                .ok_or_else(|| CliError::InvalidRepository {
                    reason: "the current working copy has no valid working_copy_id".to_string(),
                })?;
        let target = repository
            .resolve_operation_id(&self.id_or_prefix)
            .map_err(CliError::Repository)?;
        let operation_id = repository
            .restore_operation(working_copy, target)
            .map_err(CliError::Repository)?;
        render_transition_result(&repository, "Restore", operation_id, self.json)
    }
}

fn render_transition_result(
    repository: &Repository,
    action: &str,
    operation_id: atomic_core::OperationId,
    json_output: bool,
) -> CliResult<()> {
    let details = repository
        .operation_details(operation_id)
        .map_err(CliError::Repository)?;
    let output = OperationDetailsDto::from(&details);
    if json_output {
        println!("{}", json(&output)?);
    } else {
        println!("{action} completed as operation {operation_id}");
    }
    Ok(())
}

fn open_operation_repository() -> CliResult<Repository> {
    let root = find_repository_root()?;
    Repository::open_readonly_for_operation_inspection(root).map_err(CliError::Repository)
}

fn json<T: serde::Serialize>(value: &T) -> CliResult<String> {
    to_pretty_json(value).map_err(|error| CliError::Internal(error.into()))
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[derive(Debug, Parser)]
    #[command(name = "atomic")]
    struct TestCli {
        #[command(subcommand)]
        command: TestCommands,
    }

    #[derive(Debug, Subcommand)]
    enum TestCommands {
        Op(Op),
    }

    #[test]
    fn operation_log_parser_accepts_the_scoped_vertical_slice() {
        let parsed = TestCli::try_parse_from([
            "atomic",
            "op",
            "log",
            "-n",
            "7",
            "--reverse",
            "--repository",
            "--json",
        ])
        .expect("parse operation log flags");

        let TestCommands::Op(Op {
            command: OpCommands::Log(log),
        }) = parsed.command
        else {
            panic!("expected op log");
        };
        assert_eq!(log.limit, Some(7));
        assert!(log.reverse);
        assert!(log.repository);
        assert!(log.json);
    }

    #[test]
    fn operation_show_parser_requires_one_selector_and_accepts_json() {
        let parsed = TestCli::try_parse_from(["atomic", "op", "show", "abcd", "--json"])
            .expect("parse operation show flags");
        let TestCommands::Op(Op {
            command: OpCommands::Show(show),
        }) = parsed.command
        else {
            panic!("expected op show");
        };
        assert_eq!(show.id_or_prefix, "abcd");
        assert!(show.json);

        assert!(TestCli::try_parse_from(["atomic", "op", "show"]).is_err());
    }

    #[test]
    fn operation_undo_accepts_an_optional_selector() {
        let parsed = TestCli::try_parse_from(["atomic", "op", "undo", "abcd", "--json"])
            .expect("parse operation undo");
        let TestCommands::Op(Op {
            command: OpCommands::Undo(undo),
        }) = parsed.command
        else {
            panic!("expected op undo");
        };
        assert_eq!(undo.id_or_prefix.as_deref(), Some("abcd"));
        assert!(undo.json);
        assert!(TestCli::try_parse_from(["atomic", "op", "undo"]).is_ok());
    }

    #[test]
    fn operation_restore_requires_a_selector() {
        let parsed = TestCli::try_parse_from(["atomic", "op", "restore", "abcd", "--json"])
            .expect("parse operation restore");
        let TestCommands::Op(Op {
            command: OpCommands::Restore(restore),
        }) = parsed.command
        else {
            panic!("expected op restore");
        };
        assert_eq!(restore.id_or_prefix, "abcd");
        assert!(restore.json);
        assert!(TestCli::try_parse_from(["atomic", "op", "restore"]).is_err());
    }
}
