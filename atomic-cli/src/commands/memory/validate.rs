//! `atomic memory validate <ID|path>` — gate a memory against the canonical
//! shapes.

use std::path::Path;

use clap::Parser;

use atomic_canonical::lift::parse_markdown;
use atomic_canonical::memory::lift_memory;
use atomic_canonical::{validate_memory, ValidationReport};
use atomic_repository::Repository;

use crate::commands::memory::bridge;
use crate::commands::memory::validation_failed;
use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};

/// Validate a memory against the canonical shapes.
#[derive(Parser, Debug)]
#[command(name = "validate")]
pub struct MemoryValidate {
    /// Memory id (or a path to a memory markdown file).
    pub id_or_path: String,

    /// Output the validation report as JSON.
    #[arg(long)]
    pub json: bool,
}

impl Command for MemoryValidate {
    fn run(&self) -> CliResult<()> {
        // A path-shaped argument (ends in `.md` or contains a separator) is
        // loaded as a file; a missing file is reported as FileNotFound rather
        // than misreported as a bad id. Otherwise it is a memory id read
        // through the vault bridge; a FRESH attestation gates the attested node
        // (so a previously-attested memory reports conforms), a STALE one warns
        // and falls back to the raw node.
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
            lift_memory(&fm, &body).map_err(|e| CliError::InvalidArgument {
                message: format!("could not lift memory: {e}"),
            })?
        } else {
            let root = find_repository_root()?;
            let repo = Repository::open(&root).map_err(CliError::Repository)?;
            let inputs = bridge::read_memory(&repo, &self.id_or_path)?;
            match bridge::load_attestation(&repo, &self.id_or_path, &inputs)? {
                bridge::Attestation::Fresh(node) => *node,
                bridge::Attestation::Stale(_) => {
                    eprintln!(
                        "warning: the attestation for {} is stale (the memory changed since it \
                         was signed); re-run `atomic memory attest {}`.",
                        self.id_or_path, self.id_or_path
                    );
                    bridge::lift(&inputs)?
                }
                bridge::Attestation::None => bridge::lift(&inputs)?,
            }
        };

        let report = validate_memory(&node);

        if self.json {
            println!("{}", serde_json::to_string_pretty(&report_json(&report)).unwrap());
        } else {
            print!("{report}");
        }

        if report.conforms {
            Ok(())
        } else {
            Err(validation_failed(format!(
                "memory does not conform ({} violation(s))",
                report.results.len()
            )))
        }
    }
}

/// Is the argument *shaped* like a filesystem path (rather than a memory id)?
/// True when it ends in `.md` or contains a path separator. A `memory/<id>`
/// (no `.md`) form is NOT path-shaped — it is a valid id form the bridge
/// normalizes. A bare `memory/<id>.md` IS path-shaped, so `validate` would try
/// to read it as a file; callers wanting the vault entry use the bare `<id>`.
fn is_path_shaped(arg: &str) -> bool {
    arg.ends_with(".md")
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

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_canonical::memory::lift_memory;
    use serde_json::{Map, Value};

    fn unattested_node() -> atomic_canonical::MemoryNode {
        let fm: Map<String, Value> = serde_json::json!({
            "uid": "01j8zc4r8t",
            "memoryKind": "constraint",
            "status": "active",
            "createdAt": "2026-07-01T00:00:00+00:00",
        })
        .as_object()
        .unwrap()
        .clone();
        lift_memory(&fm, ":::memory\nThe durable fact.\n:::").unwrap()
    }

    #[test]
    fn test_unattested_reports_exactly_two_fillable_violations() {
        let report = validate_memory(&unattested_node());
        assert!(!report.conforms);
        let mut paths: Vec<_> = report
            .results
            .iter()
            .map(|v| v.path.clone().unwrap_or_default())
            .collect();
        paths.sort();
        assert_eq!(
            paths,
            vec!["attributedTo".to_string(), "proof".to_string()],
            "an un-attested memory reports EXACTLY the two fillable violations"
        );
    }

    #[test]
    fn test_is_path_shaped() {
        assert!(is_path_shaped("plan.md"));
        assert!(is_path_shaped("memory/x.md"));
        assert!(!is_path_shaped("x"));
        assert!(!is_path_shaped("memory/x"));
    }
}
