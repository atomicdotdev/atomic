//! `atomic vault kind validate [<k>] [--view V]` — check kind definitions and
//! every instance against its kind's spec.
//!
//! Two validators run. The field-level report from
//! [`Repository::kind_validate`] keeps the existing per-field messages. The
//! SHACL profile (`atomic_shacl::Profile::Kind`) runs the generated NodeShape
//! for each registered kind against the kind's RDF projection. SHACL is the
//! normative authority; the field report is the human-readable oracle. See
//! `docs/adr/0001-kind-kg-conformance.md`.

use clap::Parser;

use atomic_repository::{
    KindProblemCode, KindReportStatus, KindSource, KindValidationReport, Repository,
    RepositoryError,
};

use super::{display_path, display_text, kind_not_found, open_for_read, validation_failed};
use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{error, success};

/// Validate kind definitions and instances.
///
/// Reports definitions that do not register, instances that violate their
/// kind's spec (including refs that do not resolve), instances of undeclared
/// kinds, and stray files under `.vault/kinds/`. Exits 2 on any problem.
#[derive(Parser, Debug)]
#[command(name = "validate")]
pub struct KindValidate {
    /// Only validate this kind.
    pub kind: Option<String>,

    /// Validate what is recorded in this view instead of the working copy.
    #[arg(long, value_name = "VIEW")]
    pub view: Option<String>,

    /// Print the validation report as JSON.
    #[arg(long)]
    pub json: bool,
}

impl Command for KindValidate {
    fn run(&self) -> CliResult<()> {
        let repo = open_for_read()?;
        let source = match &self.view {
            Some(view) => KindSource::View(view.clone()),
            None => KindSource::WorkingCopy,
        };

        let report = repo
            .kind_validate(&source, self.kind.as_deref())
            .map_err(|e| match (e, &self.kind) {
                (RepositoryError::InvalidOperation { .. }, Some(kind)) => kind_not_found(kind),
                (e, _) => CliError::Repository(e),
            })?;

        let shacl = run_shacl(&repo, &source, self.kind.as_deref())?;

        if self.json {
            let mut value =
                serde_json::to_value(&report).map_err(|e| CliError::Internal(e.into()))?;
            if let Some(shacl) = &shacl {
                value["shacl"] =
                    serde_json::to_value(shacl).map_err(|e| CliError::Internal(e.into()))?;
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&value).map_err(|e| CliError::Internal(e.into()))?
            );
        } else {
            print_report(&report, shacl.as_ref());
            if let Some(shacl) = &shacl {
                print_shacl(shacl);
            }
        }

        let shacl_failed = shacl.as_ref().is_some_and(|s| !s.conforms);
        if report.ok && !shacl_failed {
            Ok(())
        } else {
            Err(validation_failed(format!(
                "kind validation failed in the {source} ({} problem(s){})",
                problem_count(&report),
                if shacl_failed {
                    ", SHACL violations"
                } else {
                    ""
                }
            )))
        }
    }
}

/// The SHACL result for the selected kinds, when the repository has any.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ShaclResult {
    pub conforms: bool,
    pub profile: String,
    pub snapshot_identity: String,
    pub diagnostics: Vec<String>,
    pub results: Vec<serde_json::Value>,
}

/// Run the generated kind shapes over the selected kind rows.
///
/// The dataset is the ambient knowledge-graph projection filtered to the
/// selected kind instances and their one-hop neighbourhood. That keeps the
/// SHACL focus set aligned with the rows the field-level gate validated. An
/// instance the registry tracks but the projection does not hold fails the
/// gate rather than passing vacuously.
fn run_shacl(
    repo: &Repository,
    source: &KindSource,
    kind: Option<&str>,
) -> CliResult<Option<ShaclResult>> {
    // Without a project identity there are no IRIs to project, so the RDF
    // gate has nothing to say. The field-level oracle remains the authority
    // for that repository. See ADR 0001 decision 2.
    if repo
        .project_identity()
        .map_err(CliError::Repository)?
        .is_none()
    {
        return Ok(None);
    }
    let shapes = super::shapes::kind_shapes(repo, source, kind)?;
    if shapes.bundles.is_empty() {
        return Ok(None);
    }
    let dataset = super::shapes::kind_dataset(repo, &shapes)?;
    let request = atomic_shacl::ValidationRequest {
        profile: atomic_shacl::Profile::Kind,
        shape: None,
        node: None,
        custom: shapes.bundles,
    };
    let output =
        atomic_shacl::validate_ntriples(&dataset.ntriples, &dataset.snapshot_identity, &request)
            .map_err(|e| CliError::ShaclError(e.to_string()))?;

    let mut diagnostics = output.diagnostics;
    if !dataset.missing.is_empty() {
        diagnostics.push(format!(
            "{} kind instance(s) have no projected knowledge-graph node: {}",
            dataset.missing.len(),
            dataset.missing.join(", ")
        ));
    }
    Ok(Some(ShaclResult {
        conforms: output.conforms && dataset.missing.is_empty(),
        profile: "kind".into(),
        snapshot_identity: output.snapshot_identity,
        diagnostics,
        results: output.results,
    }))
}

/// Every problem plus every violation of a field-level instance, with an
/// invalid definition counted once per spec problem.
fn problem_count(report: &KindValidationReport) -> usize {
    let spec_problems: usize = report
        .kinds
        .iter()
        .filter(|kind| kind.status == KindReportStatus::Invalid)
        .map(|kind| kind.spec_problems.len().max(1))
        .sum();
    let definition_problems = report
        .problems
        .iter()
        .filter(|problem| {
            problem.code == KindProblemCode::KindInvalid
                && report.kinds.iter().any(|kind| {
                    kind.status == KindReportStatus::Invalid && kind.def_path == problem.path
                })
        })
        .count();
    let violations: usize = report
        .kinds
        .iter()
        .flat_map(|kind| &kind.instances)
        .filter(|instance| instance.problem.is_none())
        .map(|instance| {
            instance
                .violations
                .len()
                .max(usize::from(!instance.conforms))
        })
        .sum();
    report.problems.len() - definition_problems + spec_problems + violations
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn print_shacl(shacl: &ShaclResult) {
    if shacl.conforms {
        println!(
            "{} SHACL kind profile: conforms ({} result(s))",
            success("✓"),
            shacl.results.len()
        );
        return;
    }
    println!("{} SHACL kind profile: violations", error("✗"));
    for result in &shacl.results {
        let focus = result
            .get("focus_node")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let message = result
            .get("result_message")
            .and_then(|v| v.as_str())
            .unwrap_or("does not conform");
        let path = result
            .get("result_path")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        println!(
            "  {} {}: {}{}",
            error("✗"),
            display_text(focus),
            display_text(message),
            if path.is_empty() {
                String::new()
            } else {
                format!(" ({})", display_text(path))
            }
        );
    }
}

/// The report for people. Every path and message is passed through
/// [`display_text`]: they come from repository files and can echo written values.
fn print_report(report: &KindValidationReport, shacl: Option<&ShaclResult>) {
    let mark_ok = success("✓");
    let mark_bad = error("✗");
    for kind in &report.kinds {
        match kind.status {
            KindReportStatus::Registered => {
                let edges = if kind.edges.is_empty() {
                    "no edges".to_string()
                } else {
                    format!("edges {}", kind.edges.join(", "))
                };
                let bad = kind.instances.iter().filter(|i| !i.conforms).count();
                let instances = match (kind.instance_count, bad) {
                    (0, _) => "no instances".to_string(),
                    (n, 0) => format!("{} conform", plural(n, "instance", "instances")),
                    (n, bad) => format!("{} of {}", bad, plural(n, "instance", "instances")),
                };
                let fields = plural(kind.field_count, "field", "fields");
                println!(
                    "{mark_ok} {}: {fields}, {edges}; {instances}",
                    display_text(&kind.name)
                );
                for instance in &kind.instances {
                    for v in &instance.violations {
                        let at = v
                            .field
                            .as_deref()
                            .map(|f| format!("{}: ", display_text(f)))
                            .unwrap_or_default();
                        println!(
                            "{mark_bad} {}: {at}{}",
                            display_text(&display_path(&instance.path)),
                            display_text(&v.message)
                        );
                    }
                }
            }
            KindReportStatus::Invalid => {
                for p in &kind.spec_problems {
                    println!(
                        "{mark_bad} {}: {}",
                        display_text(&display_path(&kind.def_path)),
                        display_text(p)
                    );
                }
                println!("  ({} stays unregistered)", display_text(&kind.name));
            }
        }
    }

    // The definition-level problem was printed problem by problem above.
    let invalid_defs: Vec<&str> = report
        .kinds
        .iter()
        .filter(|k| k.status == KindReportStatus::Invalid)
        .map(|k| k.def_path.as_str())
        .collect();
    for problem in &report.problems {
        if problem.code == KindProblemCode::KindInvalid
            && invalid_defs.contains(&problem.path.as_str())
        {
            continue;
        }
        println!(
            "{mark_bad} {}: {}",
            display_text(&display_path(&problem.path)),
            display_text(&problem.message)
        );
    }

    let source = &report.source;
    if report.kinds.is_empty() && report.problems.is_empty() {
        println!("No kinds in the {source}. Create one with `atomic vault kind init <name>`.");
        return;
    }
    let registered = report
        .kinds
        .iter()
        .filter(|k| k.status == KindReportStatus::Registered)
        .count();
    let instances: usize = report.kinds.iter().map(|k| k.instance_count).sum();
    let summary = format!(
        "{} ({registered} registered), {} in the {source}",
        plural(report.kinds.len(), "kind", "kinds"),
        plural(instances, "instance", "instances"),
    );
    println!();
    let shacl_failed = shacl.is_some_and(|shacl| !shacl.conforms);
    if report.ok && !shacl_failed {
        println!("{mark_ok} {summary}: all valid");
    } else {
        println!(
            "{mark_bad} {summary}: {}{}",
            plural(problem_count(report), "problem", "problems"),
            if shacl_failed { ", SHACL violations" } else { "" }
        );
    }
}
