//! The `completions` command: emit a static shell completion script.
//!
//! This is the stable, ahead-of-time completion layer. It generates a script
//! (via `clap_complete::aot`) that completes subcommands and flags for the
//! given shell. Install it once, e.g. for zsh:
//!
//! ```text
//! atomic completions zsh > ~/.zfunc/_atomic
//! # ensure ~/.zfunc is on $fpath and `autoload -Uz compinit && compinit` runs
//! ```
//!
//! For *dynamic* value completion — completing live view names and change
//! hashes after `atomic insert view`/`atomic insert change` — enable the
//! dynamic engine instead by adding this to your shell rc:
//!
//! ```text
//! source <(COMPLETE=zsh atomic)
//! ```
//!
//! The dynamic path is wired in `main.rs` via `clap_complete::CompleteEnv` and
//! supersedes the static script (it also completes subcommands and flags).

use clap_complete::aot::Shell;

use crate::error::CliResult;

/// Generate a shell completion script.
///
/// The `<SHELL>` argument accepts any shell clap supports (`bash`, `zsh`,
/// `fish`, `elvish`, `powershell`). zsh is the primary target for now.
#[derive(Debug, clap::Args)]
#[command(name = "completions")]
pub struct Completions {
    /// Shell to generate a completion script for.
    #[arg(value_name = "SHELL")]
    pub shell: Shell,
}

impl Completions {
    /// Write the completion script for `cmd` to stdout.
    ///
    /// `cmd` is supplied by the caller (the top-level `Cli::command()`), since
    /// the completion generator needs the full command tree.
    pub fn generate(&self, mut cmd: clap::Command) -> CliResult<()> {
        let bin = cmd.get_name().to_string();
        clap_complete::aot::generate(self.shell, &mut cmd, bin, &mut std::io::stdout());
        Ok(())
    }
}
