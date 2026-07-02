//! `atomic memory` — scaffold, render, and validate canonical memories.

use std::path::PathBuf;

use clap::{Args, Subcommand};

use atomic_canonical::render::{render_memory, Target};
use atomic_canonical::{gate, lift, memory, proof, vocab};

use crate::commands::Command;
use crate::error::{CliError, CliResult};

use super::{finish_validate, now_rfc3339, resolve_signing_identity, write_new_file};

/// Author and validate canonical memories (durable, reusable context).
#[derive(Debug, Args)]
#[command(arg_required_else_help = true)]
pub struct Memory {
    #[command(subcommand)]
    command: MemoryCommands,
}

#[derive(Debug, Subcommand)]
enum MemoryCommands {
    /// Scaffold a new memory with the kind, about edges, and spine pre-filled.
    ///
    /// The body is open for the memory text — a constraint, a preference, a
    /// lesson, or context worth carrying forward.
    New(New),

    /// Render a memory as formatted text.
    Show(Show),

    /// Lift, attest, and validate a memory against the gate.
    ///
    /// Tier-1 always runs; `--shacl` also runs the tier-2 formal gate
    /// (pyshacl). The source file is never rewritten.
    Validate(Validate),
}

#[derive(Debug, Args)]
struct New {
    /// Name for the memory (used for the file and the id).
    name: String,

    /// Memory kind: constraint, preference, lesson, or context.
    #[arg(long)]
    kind: String,

    /// Modules/domains this memory is about (comma-separated).
    #[arg(long, value_delimiter = ',')]
    about: Vec<String>,

    /// Output path. Defaults to `<NAME>.md` in the current directory.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct Show {
    /// Path to the memory markdown file.
    file: PathBuf,
}

#[derive(Debug, Args)]
struct Validate {
    /// Path to the memory markdown file.
    file: PathBuf,

    /// Also run the tier-2 formal SHACL gate (pyshacl).
    #[arg(long)]
    shacl: bool,

    /// Write the attested canonical JSON-LD to this path.
    #[arg(long)]
    emit: Option<PathBuf>,

    /// Machine-readable output (the validation report as JSON).
    #[arg(long)]
    json: bool,

    /// Sign with this identity instead of the default.
    #[arg(long)]
    identity: Option<String>,

    /// Password for the identity's secret key, when protected.
    #[arg(long)]
    password: Option<String>,
}

impl Command for Memory {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            MemoryCommands::New(cmd) => cmd.run(),
            MemoryCommands::Show(cmd) => cmd.run(),
            MemoryCommands::Validate(cmd) => cmd.run(),
        }
    }
}

impl New {
    fn run(&self) -> CliResult<()> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err(CliError::InvalidArgument {
                message: "memory name cannot be empty".to_string(),
            });
        }
        // Closed vocabulary reaches the author at scaffold time: you can only
        // scaffold kinds the registry knows.
        if !vocab::is_known_memory_kind(&self.kind) {
            return Err(CliError::InvalidArgument {
                message: format!(
                    "unknown memory kind '{}' — known kinds: {}",
                    self.kind,
                    vocab::MEMORY_KIND.join(", ")
                ),
            });
        }
        let out = self
            .out
            .clone()
            .unwrap_or_else(|| PathBuf::from(format!("{name}.md")));
        let content = memory_template(name, &self.kind, &self.about);
        write_new_file(&out, &content)?;
        println!(
            "next: write the memory text inside :::memory, then `atomic memory validate {}`",
            out.display()
        );
        Ok(())
    }
}

impl Show {
    fn run(&self) -> CliResult<()> {
        let node = lift_from_file(&self.file)?;
        println!("{}", render_memory(&node, Target::Cli));
        Ok(())
    }
}

impl Validate {
    fn run(&self) -> CliResult<()> {
        let node = lift_from_file(&self.file)?;
        let (identity, keypair) =
            resolve_signing_identity(self.identity.as_deref(), self.password.as_deref())?;
        let attested = proof::attest_memory(node, &identity, &keypair);
        let report = gate::validate_memory(&attested);
        finish_validate(
            "memory",
            report,
            &attested.to_value(),
            self.shacl,
            self.emit.as_deref(),
            self.json,
        )
    }
}

fn lift_from_file(path: &PathBuf) -> CliResult<memory::MemoryNode> {
    let raw = std::fs::read_to_string(path).map_err(|e| CliError::InvalidArgument {
        message: format!("cannot read {}: {e}", path.display()),
    })?;
    let (frontmatter, body) =
        lift::parse_markdown(&raw).map_err(|e| CliError::InvalidArgument {
            message: e.to_string(),
        })?;
    memory::lift_memory(&frontmatter, &body).map_err(|e| CliError::InvalidArgument {
        message: e.to_string(),
    })
}

/// The memory template: kind/about/spine pre-filled, the body open.
fn memory_template(name: &str, kind: &str, about: &[String]) -> String {
    let uid = uuid::Uuid::new_v4();
    let created_at = now_rfc3339();
    let about_line = if about.is_empty() {
        String::new()
    } else {
        format!("about: [{}]\n", about.join(", "))
    };
    format!(
        "---\n\
         id: {name}\n\
         uid: {uid}\n\
         kind: {kind}\n\
         status: active\n\
         {about_line}\
         created_at: {created_at}\n\
         ---\n\
         \n\
         :::memory\n\
         <!-- The durable context worth carrying forward, in your own words. -->\n\
         :::\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_template_lifts_cleanly() {
        let md = memory_template(
            "upload-assumptions",
            "constraint",
            &["storage".to_string(), "replication".to_string()],
        );
        let (fm, body) = lift::parse_markdown(&md).unwrap();
        let node = memory::lift_memory(&fm, &body).unwrap();
        assert_eq!(node.memory_kind, "constraint");
        assert_eq!(node.status, "active");
        assert_eq!(node.about, vec!["storage", "replication"]);
        assert!(node.id.starts_with("urn:atomic:memory:"));
    }

    #[test]
    fn memory_template_without_about_omits_the_key() {
        let md = memory_template("m", "lesson", &[]);
        assert!(!md.contains("about:"));
        let (fm, body) = lift::parse_markdown(&md).unwrap();
        let node = memory::lift_memory(&fm, &body).unwrap();
        assert!(node.about.is_empty());
    }
}
