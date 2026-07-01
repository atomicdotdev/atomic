//! `atomic intent validate <ID|path>` — gate an intent against the canonical
//! shapes.

use std::path::Path;

use clap::Parser;

use atomic_canonical::lift::{lift_intent, parse_markdown};
use atomic_canonical::{validate_intent, ValidationReport};
use atomic_repository::Repository;

use crate::commands::intent::bridge;
use crate::commands::intent::validation_failed;
use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};

/// Validate an intent against the canonical shapes.
#[derive(Parser, Debug)]
#[command(name = "validate")]
pub struct IntentValidate {
    /// Intent ID (e.g. "PIMO-1" or "1"), or a path to an intent markdown file.
    pub id_or_path: String,

    /// Output the validation report as JSON.
    #[arg(long)]
    pub json: bool,
}

impl Command for IntentValidate {
    fn run(&self) -> CliResult<()> {
        // Resolve the argument. A path-shaped argument (ends in `.md` or
        // contains a separator) is loaded as a file — and if it doesn't exist
        // we report FileNotFound rather than misreporting it as a bad ID.
        // Otherwise the argument is an intent ID read through the vault bridge;
        // if a FRESH attestation sidecar exists we gate the attested node (so a
        // previously-attested intent reports conforms), a STALE one warns and
        // falls back to the raw node.
        let node = if is_path_shaped(&self.id_or_path) {
            let path = Path::new(&self.id_or_path);
            if !path.is_file() {
                return Err(CliError::FileNotFound {
                    path: path.to_path_buf(),
                });
            }
            let doc = std::fs::read_to_string(path).map_err(CliError::Io)?;
            let (fm, body) = parse_markdown(&doc).map_err(|e| CliError::InvalidArgument {
                message: format!("could not parse markdown frontmatter: {e}"),
            })?;
            lift_intent(&fm, &body).map_err(|e| CliError::InvalidArgument {
                message: format!("could not lift intent: {e}"),
            })?
        } else {
            let root = find_repository_root()?;
            let repo = Repository::open(&root).map_err(CliError::Repository)?;
            let inputs = bridge::read_intent(&repo, &self.id_or_path)?;
            match bridge::load_attestation(&repo, &self.id_or_path, &inputs)? {
                bridge::Attestation::Fresh(node) => *node,
                bridge::Attestation::Stale(_) => {
                    eprintln!(
                        "warning: the attestation for {} is stale (the intent changed since it \
                         was signed); re-run `atomic intent attest {}`.",
                        self.id_or_path, self.id_or_path
                    );
                    bridge::lift(&inputs)?
                }
                bridge::Attestation::None => bridge::lift(&inputs)?,
            }
        };

        let report = validate_intent(&node);

        if self.json {
            println!("{}", serde_json::to_string_pretty(&report_json(&report)).unwrap());
        } else {
            print!("{report}");
        }

        if report.conforms {
            Ok(())
        } else {
            Err(validation_failed(format!(
                "intent does not conform ({} violation(s))",
                report.results.len()
            )))
        }
    }
}

/// Is the argument *shaped* like a filesystem path (rather than an intent ID)?
/// True when it ends in `.md` or contains a path separator. We decide path-vs-ID
/// on shape, not existence, so a mistyped `./plan.md` is reported as a missing
/// file (FileNotFound) instead of misleadingly falling through to "invalid ID".
/// Intent IDs are `PREFIX-N` / `N` — no `.md` suffix, no separator.
fn is_path_shaped(arg: &str) -> bool {
    arg.ends_with(".md") || arg.contains('/') || arg.contains(std::path::MAIN_SEPARATOR)
}

/// Serialize a [`ValidationReport`] to the documented JSON shape:
/// `{conforms, results: [{focus_node, shape, path, message}]}`.
fn report_json(report: &ValidationReport) -> serde_json::Value {
    serde_json::json!({
        "conforms": report.conforms,
        "results": report
            .results
            .iter()
            .map(|v| serde_json::json!({
                "focus_node": v.focus_node,
                "shape": v.shape,
                "path": v.path,
                "message": v.message,
            }))
            .collect::<Vec<_>>(),
    })
}
