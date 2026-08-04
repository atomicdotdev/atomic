//! Agent-facing deprecation shims for the retired `atomic vault intent` and
//! `atomic vault memory` subtrees.
//!
//! The real command families now live at the root (`atomic intent`,
//! `atomic memory`). These shims capture whatever was typed under the old path
//! and emit a terse, **machine-parseable** redirect — keyword-prefixed
//! `key: value` lines, no color, no emoji, no prose — then exit non-zero (2) so
//! an agent knows the command did NOT run and can reissue the root equivalent.

use clap::Parser;

use crate::commands::Command;
use crate::error::CliResult;

/// Removed: `atomic vault intent …` → `atomic intent …`.
#[derive(Parser, Debug)]
#[command(name = "intent", disable_help_flag = true)]
pub struct RemovedVaultIntent {
    /// Captured and ignored — the command was removed; see the emitted redirect.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub args: Vec<String>,
}

impl Command for RemovedVaultIntent {
    fn run(&self) -> CliResult<()> {
        let run = match first_verb(&self.args) {
            "create" | "new" => "atomic intent new \"<title>\"",
            "show" => "atomic intent show <id>",
            "list" => "atomic intent list",
            "update" => "atomic intent update <id> --status <status>",
            "delete" => "atomic intent delete <id>",
            "link" => "atomic intent link <id> --goal <goal>",
            "validate" => "atomic intent validate <id>",
            "attest" => "atomic intent attest <id>",
            "verify" => "atomic intent verify <id>",
            _ => "atomic intent --help",
        };
        emit_removed(
            "atomic vault intent",
            "atomic intent",
            "new show list validate attest verify update delete link",
            run,
        )
    }
}

/// Removed: `atomic vault memory …` → `atomic memory …`.
#[derive(Parser, Debug)]
#[command(name = "memory", disable_help_flag = true)]
pub struct RemovedVaultMemory {
    /// Captured and ignored — the command was removed; see the emitted redirect.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub args: Vec<String>,
}

impl Command for RemovedVaultMemory {
    fn run(&self) -> CliResult<()> {
        let run = match first_verb(&self.args) {
            "write" => "atomic memory write <name>",
            "show" => "atomic memory show <id>",
            "list" => "atomic memory list",
            _ => "atomic memory --help",
        };
        emit_removed(
            "atomic vault memory",
            "atomic memory",
            "new write show list kinds validate attest verify",
            run,
        )
    }
}

/// First non-flag token an agent typed after the removed command (its old
/// verb, e.g. `create`), used to tailor the `run:` redirect. Empty if none.
fn first_verb(args: &[String]) -> &str {
    args.iter()
        .map(String::as_str)
        .find(|a| !a.starts_with('-'))
        .unwrap_or("")
}

/// Emit the machine-parseable redirect to stderr and exit non-zero (2).
///
/// Agent-oriented by construction: one `key: value` datum per line, stable
/// keys (`removed`/`use`/`verbs`/`run`/`help`), no styling. Exiting 2 (a usage
/// error, distinct from 1) tells the caller the command did not run.
fn emit_removed(removed: &str, family: &str, verbs: &str, run: &str) -> CliResult<()> {
    eprintln!("removed: {removed}");
    eprintln!("use: {family}");
    eprintln!("verbs: {verbs}");
    eprintln!("run: {run}");
    eprintln!("help: {family} --help");
    std::process::exit(2);
}
