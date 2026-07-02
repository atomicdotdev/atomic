//! `atomic intent` — scaffold, render, and validate canonical intents.

use std::path::PathBuf;

use clap::{Args, Subcommand};

use atomic_canonical::render::{render, Target};
use atomic_canonical::{gate, lift, proof};

use crate::commands::Command;
use crate::error::{CliError, CliResult};

use super::{finish_validate, now_rfc3339, resolve_signing_identity, write_new_file};

/// Author and validate canonical intents (markdown + typed directives).
#[derive(Debug, Args)]
#[command(arg_required_else_help = true)]
pub struct Intent {
    #[command(subcommand)]
    command: IntentCommands,
}

#[derive(Debug, Subcommand)]
enum IntentCommands {
    /// Scaffold a new intent from the feature template.
    ///
    /// The frontmatter spine is pre-filled and the directive blocks are
    /// stubbed — you can only scaffold types the closed registry knows.
    /// The prose inside each block stays yours.
    New(New),

    /// Render an intent as formatted text (a pure projection of the node).
    Show(Show),

    /// Lift, attest, and validate an intent against the gate.
    ///
    /// Tier-1 (in-process shapes) always runs; `--shacl` also runs the
    /// tier-2 formal gate (pyshacl) over the JSON-LD projection. The source
    /// file is never rewritten — attestation happens in-memory so the gate
    /// can check authorship and proof.
    Validate(Validate),
}

#[derive(Debug, Args)]
struct New {
    /// Human key for the intent (e.g. WORD-5).
    key: String,

    /// Title for the intent.
    #[arg(long, default_value = "")]
    title: String,

    /// Output path. Defaults to `<KEY>.md` in the current directory.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct Show {
    /// Path to the intent markdown file.
    file: PathBuf,
}

#[derive(Debug, Args)]
struct Validate {
    /// Path to the intent markdown file.
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

impl Command for Intent {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            IntentCommands::New(cmd) => cmd.run(),
            IntentCommands::Show(cmd) => cmd.run(),
            IntentCommands::Validate(cmd) => cmd.run(),
        }
    }
}

impl New {
    fn run(&self) -> CliResult<()> {
        let key = self.key.trim();
        if key.is_empty() {
            return Err(CliError::InvalidArgument {
                message: "intent key cannot be empty".to_string(),
            });
        }
        let out = self
            .out
            .clone()
            .unwrap_or_else(|| PathBuf::from(format!("{key}.md")));
        let content = feature_template(key, &self.title);
        write_new_file(&out, &content)?;
        println!(
            "next: edit the directive blocks, then `atomic intent validate {}`",
            out.display()
        );
        Ok(())
    }
}

impl Show {
    fn run(&self) -> CliResult<()> {
        let node = lift_from_file(&self.file)?;
        println!("{}", render(&node, Target::Cli));
        Ok(())
    }
}

impl Validate {
    fn run(&self) -> CliResult<()> {
        let node = lift_from_file(&self.file)?;
        let (identity, keypair) =
            resolve_signing_identity(self.identity.as_deref(), self.password.as_deref())?;
        let attested = proof::attest(node, &identity, &keypair);
        let report = gate::validate_intent(&attested);
        finish_validate(
            "intent",
            report,
            &attested.to_value(),
            self.shacl,
            self.emit.as_deref(),
            self.json,
        )
    }
}

fn lift_from_file(path: &PathBuf) -> CliResult<atomic_canonical::node::CanonicalNode> {
    let raw = std::fs::read_to_string(path).map_err(|e| CliError::InvalidArgument {
        message: format!("cannot read {}: {e}", path.display()),
    })?;
    let (frontmatter, body) =
        lift::parse_markdown(&raw).map_err(|e| CliError::InvalidArgument {
            message: e.to_string(),
        })?;
    lift::lift_intent(&frontmatter, &body).map_err(|e| CliError::InvalidArgument {
        message: e.to_string(),
    })
}

/// The feature template: pre-filled spine, stubbed directive blocks,
/// unconstrained prose slots. Guidance lives in HTML comments the lift
/// ignores.
fn feature_template(key: &str, title: &str) -> String {
    let uid = uuid::Uuid::new_v4();
    let created_at = now_rfc3339();
    let slug = key.to_lowercase();
    format!(
        "---\n\
         id: {key}\n\
         uid: {uid}\n\
         title: {title}\n\
         status: backlog\n\
         priority: medium\n\
         created_at: {created_at}\n\
         ---\n\
         \n\
         :::why\n\
         <!-- The reason, in your own words. Presence is enforced; content is yours. -->\n\
         :::\n\
         \n\
         :::acceptance-criterion{{#{slug}-ac-1 status=open}}\n\
         <!-- What \"done\" looks like. When met: status=met verifiedBy=<did> evidence=<urn:atomic:change:...> -->\n\
         :::\n\
         \n\
         :::task{{#{slug}-1 status=open satisfies={slug}-ac-1}}\n\
         <!-- A decomposed work item. Name touched files with ::file-ref{{path=...}} -->\n\
         :::\n\
         \n\
         :::scope-in\n\
         <!-- What this intent covers. -->\n\
         :::\n\
         \n\
         :::scope-out\n\
         <!-- What it explicitly does not — boundaries the agent must respect. -->\n\
         :::\n\
         \n\
         :::constraint\n\
         <!-- A rule the implementation must respect. -->\n\
         :::\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_template_lifts_cleanly() {
        let md = feature_template("WORD-5", "Add name prompt modal");
        let (fm, body) = lift::parse_markdown(&md).unwrap();
        let node = lift::lift_intent(&fm, &body).unwrap();
        assert_eq!(node.human_key, "WORD-5");
        assert_eq!(node.status, "backlog");
        assert_eq!(node.has_acceptance_criterion.len(), 1);
        assert_eq!(node.has_acceptance_criterion[0].ac_status, "open");
        assert_eq!(node.has_task.len(), 1);
        assert_eq!(node.has_scope_in.len(), 1);
        assert_eq!(node.has_scope_out.len(), 1);
        assert_eq!(node.has_constraint.len(), 1);
        // The why block exists (prose slot present, even while empty of prose).
        assert!(node.why.is_some());
    }

    #[test]
    fn template_only_scaffolds_registry_types() {
        // Every directive the template stubs must be in the closed registry.
        let md = feature_template("X-1", "t");
        for line in md.lines() {
            if let Some(rest) = line.strip_prefix(":::") {
                let name = rest.split(['{', ' ']).next().unwrap_or("");
                if !name.is_empty() {
                    assert!(
                        atomic_canonical::vocab::is_known_directive(name),
                        "template stubs unknown directive '{name}'"
                    );
                }
            }
        }
    }
}
