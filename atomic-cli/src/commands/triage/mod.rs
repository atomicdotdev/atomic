//! `atomic triage` — project the code-review candidate set over two views.
//!
//! Milestone T0 exposes the candidate-set primitive: given a `feature` view
//! and a `--into` target view, report the change hashes visible to `feature`
//! but not `target`, plus their transitive dependency-closure additions, plus
//! which of those additions are "baggage" (not covered by any intent).

use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};

use clap::{Parser, Subcommand};
use clap_complete::engine::ArgValueCompleter;

use atomic_repository::{Repository, RepositoryError};

use crate::commands::complete::complete_view_names;
use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};

pub mod model;
pub mod output;
pub mod project;

/// Triage a feature view against a target before insert.
///
/// Both subcommands take an optional source view and an optional `--into`
/// target: the source defaults to the current view and the target to that
/// view's parent, so a bare `atomic triage review` answers "is what I am
/// working on ready to promote?".
#[derive(Debug, Parser)]
#[command(after_help = "\
The <VIEW> argument and --into are both optional. Run 'atomic triage review
--help' for the full example set.")]
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
    /// Report the candidate change set of a view relative to a target.
    ///
    /// Which changes would land if this view were promoted: only-in-source,
    /// the transitive dependency-closure additions, and which additions are
    /// "baggage" (covered by no intent). See this subcommand's examples.
    Candidates(TriageCandidates),

    /// Build the canonical triage report and render it (verdict + findings).
    ///
    /// Walks the change → file → task → intent → acceptance-criterion join,
    /// gates each reached intent, and emits a bounded CLI dashboard (default)
    /// or the full JSON worklist (`--json`). See this subcommand's examples.
    Review(TriageReview),
}

/// Compute the triage candidate set for a view relative to a target.
#[derive(Debug, Parser)]
#[command(after_help = "\
<VIEW> defaults to the current view; --into defaults to that view's parent.

Examples:
  # What would land if the current view were promoted? (no arguments needed)
  atomic triage candidates
  atomic triage candidates --json

  # A specific source and target
  atomic triage candidates my-feature --into dev
  atomic triage candidates my-feature --into dev --json")]
pub struct TriageCandidates {
    /// The feature (source) view to review. Defaults to the current view.
    #[arg(
        value_name = "VIEW",
        add = ArgValueCompleter::new(complete_view_names)
    )]
    pub feature: Option<String>,

    /// The target view the feature would be inserted into. Defaults to the
    /// feature view's parent.
    #[arg(
        long,
        value_name = "VIEW",
        add = ArgValueCompleter::new(complete_view_names)
    )]
    pub into: Option<String>,

    /// Emit the candidate set as JSON.
    #[arg(long)]
    pub json: bool,
}

impl Command for TriageCandidates {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;
        let (feature, into) = resolve_views(&repo, self.feature.as_deref(), self.into.as_deref())?;

        let set = repo
            .triage_candidate_set(&feature, &into)
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

/// Build the canonical triage report for a view relative to a target.
#[derive(Debug, Parser)]
#[command(after_help = "\
<VIEW> defaults to the current view; --into defaults to that view's parent. So a
bare 'atomic triage review' asks: is the view I am working on ready to promote?

Examples:
  # Promote-readiness of the current view, in guided reading order
  atomic triage review --walkthrough

  # Bounded dashboard: verdict + findings, for the current view
  atomic triage review

  # The full JSON worklist — start here when driving this by hand
  atomic triage review --json

  # A specific source and target
  atomic triage review my-feature --into dev
  atomic triage review my-feature --into dev --json
  atomic triage review my-feature --into dev --walkthrough

  # Chapter tour in a browser
  atomic triage review my-feature --into dev --html
  atomic triage review my-feature --into dev --html --output review.html
  atomic triage review my-feature --into dev --html --no-open

  # Signed export for portability/compliance
  atomic triage review my-feature --into dev --attest > review.signed.json")]
pub struct TriageReview {
    /// The feature (source) view to review. Defaults to the current view.
    #[arg(
        value_name = "VIEW",
        add = ArgValueCompleter::new(complete_view_names)
    )]
    pub feature: Option<String>,

    /// The target view the feature would be inserted into. Defaults to the
    /// feature view's parent.
    #[arg(
        long,
        value_name = "VIEW",
        add = ArgValueCompleter::new(complete_view_names)
    )]
    pub into: Option<String>,

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
        let (feature, into) = resolve_views(&repo, self.feature.as_deref(), self.into.as_deref())?;

        let report = project::build_report(&repo, &feature, &into)?;

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

/// Resolve the `(source, target)` view pair a triage run operates on.
///
/// Both arguments are optional so the common gesture needs no arguments at all:
/// the source is the current view and the target is that view's direct parent
/// — the same default `atomic insert` uses for a bare promote. `arg` names the
/// argument a name came from so an unknown view can be reported against it.
///
/// # Errors
///
/// Returns [`CliError::InvalidArgument`] if an explicitly named view does not
/// exist, or if the target was omitted and the source is a root view (nothing
/// to promote into).
fn resolve_views(
    repo: &Repository,
    feature: Option<&str>,
    into: Option<&str>,
) -> CliResult<(String, String)> {
    let source = match feature {
        Some(name) => {
            require_view(repo, name, "<VIEW>")?;
            name.to_string()
        }
        None => repo.current_view().to_string(),
    };

    let target = match into {
        Some(name) => {
            require_view(repo, name, "--into")?;
            name.to_string()
        }
        None => match repo.parent_change_count(&source) {
            Ok(Some((parent, _))) => parent,
            Ok(None) => {
                return Err(CliError::InvalidArgument {
                    message: format!(
                        "'{source}' is a root view — it has no parent to promote into.\n  \
                         Pass --into <view> to choose a target (see 'atomic view list')."
                    ),
                })
            }
            Err(RepositoryError::ViewNotFound { name }) => {
                return Err(unknown_view(repo, &name, "<VIEW>"))
            }
            Err(e) => return Err(CliError::Repository(e)),
        },
    };

    Ok((source, target))
}

/// Fail with an actionable message when a triage view argument names a view
/// that does not exist.
///
/// The usual cause is typing the placeholder from the help text
/// (`atomic triage review feature`) instead of a real view name, so the message
/// says the argument is optional and names the current view as the value to
/// simply drop in — or the value to leave off entirely.
fn require_view(repo: &Repository, name: &str, arg: &str) -> CliResult<()> {
    if repo.view_exists(name).map_err(CliError::Repository)? {
        Ok(())
    } else {
        Err(unknown_view(repo, name, arg))
    }
}

/// The error for a view name that does not resolve.
fn unknown_view(repo: &Repository, name: &str, arg: &str) -> CliError {
    CliError::InvalidArgument {
        message: format!(
            "No view named '{name}' — {arg} takes a real view name, not a placeholder.\n  \
             Current view is '{}'; omit {arg} to triage that instead, or run \
             'atomic view list' to see all views.",
            repo.current_view()
        ),
    }
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};
    use tempfile::{tempdir, TempDir};

    use super::*;

    /// A repo with a root view `dev` and a draft `feature-x` parented on it,
    /// checked out on the draft — the state the defaults are meant to serve.
    fn draft_repo() -> (Repository, TempDir) {
        let dir = tempdir().unwrap();
        let mut repo = Repository::init(dir.path()).unwrap();
        repo.create_draft_view("feature-x", "dev").unwrap();
        repo.set_current_view_in_memory("feature-x");
        (repo, dir)
    }

    /// The `<VIEW>` / `--into` pair parsed off a `triage review` argument list.
    fn parse_views(args: &[&str]) -> (Option<String>, Option<String>) {
        let cmd =
            TriageReview::try_parse_from(std::iter::once("review").chain(args.iter().copied()))
                .unwrap();
        (cmd.feature, cmd.into)
    }

    #[test]
    fn both_view_arguments_are_optional() {
        assert_eq!(parse_views(&[]), (None, None));
        // A lone source, and a lone target, are each valid on their own.
        assert_eq!(
            parse_views(&["feature-x"]),
            (Some("feature-x".into()), None)
        );
        assert_eq!(parse_views(&["--into", "dev"]), (None, Some("dev".into())));
    }

    #[test]
    fn usage_advertises_both_arguments_as_optional() {
        for cmd in [TriageReview::command(), TriageCandidates::command()] {
            // Neither argument may be required, or clap puts it in the usage
            // summary instead of `[OPTIONS]` / `[VIEW]`.
            for id in ["feature", "into"] {
                let arg = cmd
                    .get_arguments()
                    .find(|a| a.get_id() == id)
                    .unwrap_or_else(|| panic!("missing argument `{id}`"));
                assert!(!arg.is_required_set(), "`{id}` is still required");
            }
        }

        let usage = TriageReview::command().render_usage().to_string();
        assert!(usage.contains("[VIEW]"), "usage: {usage}");
        // The positional is named VIEW, not FEATURE — `feature` read as a
        // literal view name, which is exactly the mistake that prompted this.
        assert!(!usage.contains("[FEATURE]"), "usage: {usage}");
    }

    #[test]
    fn omitted_arguments_resolve_to_current_view_and_its_parent() {
        let (repo, _dir) = draft_repo();
        let (feature, into) = resolve_views(&repo, None, None).unwrap();
        assert_eq!(feature, "feature-x");
        assert_eq!(into, "dev");
    }

    #[test]
    fn explicit_arguments_pass_through_unchanged() {
        let (repo, _dir) = draft_repo();
        let (feature, into) = resolve_views(&repo, Some("dev"), Some("feature-x")).unwrap();
        assert_eq!(feature, "dev");
        assert_eq!(into, "feature-x");
    }

    #[test]
    fn omitted_target_follows_the_named_source_not_the_current_view() {
        let (repo, _dir) = draft_repo();
        // Current view is feature-x, but naming `dev` as the source must make
        // the target *its* parent — there is none, so this is a root-view error.
        let err = resolve_views(&repo, Some("dev"), None).unwrap_err();
        assert!(err.to_string().contains("root view"), "unexpected: {err}");
    }

    #[test]
    fn a_root_source_with_no_target_explains_the_missing_parent() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let err = resolve_views(&repo, None, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("root view"), "unexpected: {msg}");
        assert!(msg.contains("--into"), "unexpected: {msg}");
    }

    #[test]
    fn a_placeholder_view_name_is_rejected_with_the_optional_argument_as_the_fix() {
        let (repo, _dir) = draft_repo();
        let err = resolve_views(&repo, Some("feature"), None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("No view named 'feature'"), "unexpected: {msg}");
        assert!(msg.contains("omit <VIEW>"), "unexpected: {msg}");
        assert!(msg.contains("feature-x"), "unexpected: {msg}");
    }

    #[test]
    fn a_bad_target_is_reported_against_into_not_the_positional() {
        let (repo, _dir) = draft_repo();
        let err = resolve_views(&repo, None, Some("nope")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("No view named 'nope'"), "unexpected: {msg}");
        assert!(msg.contains("omit --into"), "unexpected: {msg}");
    }

    #[test]
    fn candidates_parses_the_same_optional_pair_as_review() {
        let cmd = TriageCandidates::try_parse_from(["candidates"]).unwrap();
        assert!(cmd.feature.is_none());
        assert!(cmd.into.is_none());

        let cmd =
            TriageCandidates::try_parse_from(["candidates", "feature-x", "--into", "dev"]).unwrap();
        assert_eq!(cmd.feature.as_deref(), Some("feature-x"));
        assert_eq!(cmd.into.as_deref(), Some("dev"));
    }

    #[test]
    fn help_carries_examples_for_both_subcommands() {
        // The agent help template drops `long_about` but keeps `after_help`, so
        // the examples have to live there to be discoverable.
        for (name, about) in [
            ("candidates", "atomic triage candidates"),
            ("review", "--walkthrough"),
        ] {
            let help = Triage::command()
                .find_subcommand_mut(name)
                .unwrap()
                .render_long_help()
                .to_string();
            assert!(
                help.contains(about),
                "{name} help missing `{about}`:\n{help}"
            );
            assert!(
                help.contains("Examples:"),
                "{name} help missing examples:\n{help}"
            );
        }
    }
}
