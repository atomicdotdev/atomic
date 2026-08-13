//! `atomic triage` — project the code-review candidate set over two views.
//!
//! Milestone T0 exposes the candidate-set primitive: given a `feature` view
//! and a `--into` target view, report the change hashes visible to `feature`
//! but not `target`, plus their transitive dependency-closure additions, plus
//! which of those additions are "baggage" (not covered by any intent).

use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};

use clap::{Parser, Subcommand};

use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};

pub mod model;
pub mod output;
pub mod project;

/// Triage a feature view against a target before insert.
#[derive(Debug, Parser)]
pub struct Triage {
    #[command(subcommand)]
    pub command: TriageCommands,
}

impl Command for Triage {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            TriageCommands::Candidates(cmd) => cmd.run(),
            TriageCommands::Review(cmd) => cmd.run(),
        }
    }
}

/// Subcommands for `atomic triage`.
#[derive(Subcommand, Debug)]
pub enum TriageCommands {
    /// Report the candidate change set of a feature view relative to a target.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic triage candidates feature --into dev
    /// atomic triage candidates feature --into dev --json
    /// ```
    Candidates(TriageCandidates),

    /// Build the canonical triage report and render it (verdict + findings).
    ///
    /// Walks the change → file → task → intent → acceptance-criterion join,
    /// gates each reached intent, and emits a bounded CLI dashboard (default)
    /// or the full JSON worklist (`--json`).
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic triage review feature --into dev
    /// atomic triage review feature --into dev --json
    /// atomic triage review feature --into dev --walkthrough     # guided reading order
    /// atomic triage review feature --into dev --html            # write + open in browser
    /// atomic triage review feature --into dev --html --output review.html
    /// atomic triage review feature --into dev --html --no-open
    /// atomic triage review feature --into dev --attest > review.signed.json
    /// ```
    Review(TriageReview),
}

/// Compute the triage candidate set for `<feature>` relative to `--into`.
#[derive(Debug, Parser)]
pub struct TriageCandidates {
    /// The feature (source) view to review.
    pub feature: String,

    /// The target view the feature would be inserted into.
    #[arg(long)]
    pub into: String,

    /// Emit the candidate set as JSON.
    #[arg(long)]
    pub json: bool,
}

impl Command for TriageCandidates {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        let set = repo
            .triage_candidate_set(&self.feature, &self.into)
            .map_err(CliError::Repository)?;

        if self.json {
            println!("{}", serde_json::to_string_pretty(&set).unwrap());
            return Ok(());
        }

        println!("Triage candidates: {} \u{2192} {}", set.feature, set.target);
        println!("  only in {}: {}", set.feature, set.only_in_feature.len());
        for hash in &set.only_in_feature {
            println!("    {}", hash);
        }
        println!("  closure additions: {}", set.closure_additions.len());
        for hash in &set.closure_additions {
            println!("    {}", hash);
        }
        println!("  baggage: {}", set.baggage.len());
        for entry in &set.baggage {
            let coverage = match entry.coverage {
                atomic_repository::Coverage::Covered => "covered",
                atomic_repository::Coverage::Uncovered => "uncovered",
                atomic_repository::Coverage::Unknown => "unknown",
            };
            let files = if entry.modifies.is_empty() {
                String::new()
            } else {
                format!("  [{}]", entry.modifies.join(", "))
            };
            println!("    {} ({}){}", entry.change, coverage, files);
        }

        Ok(())
    }
}

/// Write the rendered HTML to a file and (unless suppressed) open it in the
/// default browser via a `file://` URL.
fn write_and_open_html(
    html: &str,
    report: &model::TriageReport,
    output: Option<&Path>,
    open: bool,
) -> CliResult<()> {
    let path: PathBuf = match output {
        Some(p) => p.to_path_buf(),
        None => {
            let slug = |s: &str| -> String {
                s.chars()
                    .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                    .collect()
            };
            std::env::temp_dir().join(format!(
                "atomic-triage-{}-into-{}.html",
                slug(&report.inputs.feature),
                slug(&report.inputs.target),
            ))
        }
    };

    std::fs::write(&path, html)?;
    let abs = std::fs::canonicalize(&path).unwrap_or(path);
    let url = path_to_file_url(&abs);

    println!("Wrote triage report to {}", abs.display());
    println!("{}", url);

    if open {
        if let Err(e) = open_in_browser(&url) {
            eprintln!("(could not open a browser: {e} — open the file:// URL above manually)");
        }
    }

    Ok(())
}

/// Convert an absolute filesystem path into a `file://` URL (spaces encoded).
fn path_to_file_url(path: &Path) -> String {
    let s = path
        .to_string_lossy()
        .replace('\\', "/")
        .replace(' ', "%20");
    if s.starts_with('/') {
        format!("file://{}", s) // unix: file:///Users/…
    } else {
        format!("file:///{}", s) // windows: file:///C:/…
    }
}

/// Spawn the platform's default opener for a URL, detached.
fn open_in_browser(target: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = ProcessCommand::new("open");
        c.arg(target);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = ProcessCommand::new("cmd");
        c.args(["/C", "start", "", target]);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = ProcessCommand::new("xdg-open");
        c.arg(target);
        c
    };

    cmd.stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    Ok(())
}

/// Build the canonical triage report for `<feature>` relative to `--into`.
#[derive(Debug, Parser)]
pub struct TriageReview {
    /// The feature (source) view to review.
    pub feature: String,

    /// The target view the feature would be inserted into.
    #[arg(long)]
    pub into: String,

    /// Emit the full report as JSON instead of the bounded CLI dashboard.
    #[arg(long)]
    pub json: bool,

    /// Print the guided walkthrough: the candidate changes grouped into
    /// ordered semantic layers (foundations first), with each layer's
    /// rationale, files, and inspect commands. Bounded — never a diff dump.
    #[arg(long)]
    pub walkthrough: bool,

    /// Write a self-contained HTML report (inline CSS/JS, no external assets)
    /// to a file and open it in the default browser.
    #[arg(long)]
    pub html: bool,

    /// With `--html`, write the report to this path instead of a temp file.
    #[arg(long)]
    pub output: Option<PathBuf>,

    /// With `--html`, write the file but do not open a browser (headless/CI).
    #[arg(long)]
    pub no_open: bool,

    /// Emit a signed (attested) JSON export: the report plus an Ed25519
    /// Data Integrity proof, frozen for portability/compliance.
    #[arg(long)]
    pub attest: bool,

    /// Identity whose key signs the `--attest` export. Defaults to the current
    /// default identity.
    #[arg(long)]
    pub identity: Option<String>,
}

impl Command for TriageReview {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;

        let report = project::build_report(&repo, &self.feature, &self.into)?;

        // Output selection precedence:
        // attest > html > json > walkthrough > CLI dashboard.
        if self.attest {
            let signed = output::attest_report(&report, self.identity.as_deref())?;
            println!("{}", serde_json::to_string_pretty(&signed).unwrap());
        } else if self.html {
            let html = output::render_html(&report);
            write_and_open_html(&html, &report, self.output.as_deref(), !self.no_open)?;
        } else if self.json {
            println!("{}", serde_json::to_string_pretty(&report).unwrap());
        } else if self.walkthrough {
            output::print_walkthrough(&report);
        } else {
            output::print_report(&report);
        }

        Ok(())
    }
}
