//! `atomic kind validate [<k>] [--view V]` — check kind definitions and
//! every instance against its kind's spec.

use clap::Parser;

use atomic_repository::{
    KindProblemCode, KindReportStatus, KindSource, KindValidationReport, RepositoryError,
};

use super::{display_path, display_text, kind_not_found, open_repository, validation_failed};
use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{error, success};

/// Validate kind definitions and their instances.
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
        let repo = open_repository()?;
        let source = match &self.view {
            Some(view) => KindSource::View(view.clone()),
            None => KindSource::WorkingCopy,
        };
        let report = repo
            .kind_validate(&source, self.kind.as_deref())
            .map_err(|e| match (e, &self.kind) {
                (RepositoryError::InvalidOperation { .. }, Some(kind)) => kind_not_found(kind),
                (RepositoryError::ViewNotFound { name }, _) => CliError::ViewNotFound { name },
                (e, _) => CliError::Repository(e),
            })?;

        if self.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&report).expect("the report serializes")
            );
        } else {
            print_report(&report);
        }

        if report.ok {
            Ok(())
        } else {
            Err(validation_failed(format!(
                "kind validation failed in the {source} ({} problem(s))",
                problem_count(&report)
            )))
        }
    }
}

/// Every problem plus every violation of a field-checked instance, with an
/// invalid definition counted once per spec problem.
fn problem_count(report: &KindValidationReport) -> usize {
    let spec_problems: usize = report
        .kinds
        .iter()
        .filter(|k| k.status == KindReportStatus::Invalid)
        .map(|k| k.spec_problems.len().max(1))
        .sum();
    let definition_problems = report
        .problems
        .iter()
        .filter(|p| {
            p.code == KindProblemCode::KindInvalid
                && report
                    .kinds
                    .iter()
                    .any(|k| k.status == KindReportStatus::Invalid && k.def_path == p.path)
        })
        .count();
    let violations: usize = report
        .kinds
        .iter()
        .flat_map(|k| &k.instances)
        .filter(|i| i.problem.is_none())
        .map(|i| i.violations.len().max(usize::from(!i.conforms)))
        .sum();
    report.problems.len() - definition_problems + spec_problems + violations
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The report for people. Every path and message is passed through
/// [`display_text`]: they come from repository files and can echo the
/// values written there.
fn print_report(report: &KindValidationReport) {
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
                    (n, bad) => format!(
                        "{} of {} do not conform",
                        bad,
                        plural(n, "instance", "instances")
                    ),
                };
                let fields = plural(kind.field_count, "field", "fields");
                println!("{mark_ok} {}: {fields}, {edges}; {instances}", kind.name);
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
                println!("  ({} stays unregistered until this is fixed)", kind.name);
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
    for p in &report.problems {
        if p.code == KindProblemCode::KindInvalid && invalid_defs.contains(&p.path.as_str()) {
            continue;
        }
        println!(
            "{mark_bad} {}: {}",
            display_text(&display_path(&p.path)),
            display_text(&p.message)
        );
    }

    let source = &report.source;
    if report.kinds.is_empty() && report.problems.is_empty() {
        println!("No kinds in the {source}. Create one with `atomic kind init <name>`.");
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
    if report.ok {
        println!("{mark_ok} {summary}: all valid");
    } else {
        println!(
            "{mark_bad} {summary}: {}",
            plural(problem_count(report), "problem", "problems")
        );
    }
}
