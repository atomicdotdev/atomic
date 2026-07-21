//! `atomic agent attest` command implementation.
//!
//! Lists attestations in the repository — graph-level audit nodes that
//! capture AI cost, token usage, model breakdown, and session metadata.
//!
//! Attestations cover concrete changes. Views provide the query and
//! aggregation scope for listing receipts and provenance summaries.
//!
//! # Examples
//!
//! ```text
//! # List all attestations
//! atomic agent attest
//!
//! # Show details for a specific attestation
//! atomic agent attest --hash XMJZ3IPF
//!
//! # Show attestations covering changes in a specific view
//! atomic agent attest --view dev
//!
//! # Verbose output with model breakdown
//! atomic agent attest --verbose
//! ```

use clap::Args;

use atomic_core::change::attestation::Attestation;
use atomic_core::types::{Base32, Hash};
use atomic_repository::Repository;

use crate::commands::{find_repository_root, format_hash, Command};
use crate::error::{CliError, CliResult};
use crate::output::print_warning;

// Attest Command

/// List and inspect attestations in the repository.
///
/// Attestations are graph-level audit nodes that capture metadata about
/// AI agent sessions: cost, token usage per model, duration, and which
/// changes are covered. Views are the scope for asking which covered
/// changes, receipts, or provenance summaries are relevant.
#[derive(Debug, Args)]
pub struct Attest {
    /// Show details for a specific attestation by hash (or prefix).
    ///
    /// Cannot be combined with `--summary` / `--pending` (those are
    /// provenance-summary modes, not attestation lookups).
    #[arg(long, value_name = "HASH", conflicts_with_all = ["summary", "pending"])]
    hash: Option<String>,

    /// Filter to attestations covering changes in this view.
    #[arg(long, value_name = "VIEW")]
    view: Option<String>,

    /// Show verbose output with model breakdown and coverage.
    #[arg(short, long)]
    verbose: bool,

    /// Show a project-level AI provenance summary instead of the attestation list.
    ///
    /// Classifies each change in the view as AI / Human / Needs-attention / System
    /// based on embedded change provenance, and reports `AI / (AI + Human)`.
    /// Independent of attestations — works even if attestations are missing.
    ///
    /// Scans the full history of the selected view (good for canonical views
    /// like `dev`). For an agent / draft view that inherits from a parent,
    /// use `--pending <parent_view>` to summarize only the delta — otherwise
    /// inherited human/system changes will be counted in the denominator.
    #[arg(short, long)]
    summary: bool,

    /// Summarize the **pending delta** of `--view` relative to the given
    /// parent view (e.g., `--pending dev`). Only changes that are in
    /// `--view` but not in `<parent>` are classified. Implies `--summary`
    /// and requires `--view` to be set.
    #[arg(long, value_name = "PARENT_VIEW")]
    pending: Option<String>,
}

impl Attest {
    #[cfg(test)]
    pub(crate) fn default_for_test() -> Self {
        Self {
            hash: None,
            view: None,
            verbose: false,
            summary: false,
            pending: None,
        }
    }
}

impl Command for Attest {
    fn run(&self) -> CliResult<()> {
        let repo_root = find_repository_root()?;
        let repo = Repository::open(&repo_root).map_err(|e| match e {
            atomic_repository::RepositoryError::NotFound { path } => CliError::RepositoryNotFound {
                searched_path: path.into(),
            },
            other => CliError::Repository(other),
        })?;

        // Project-level AI provenance summary (independent of attestations).
        // `--pending <parent>` implies summary mode.
        if self.summary || self.pending.is_some() {
            // `--pending` compares a specific agent view against its parent;
            // defaulting the agent view to the current view would produce a
            // meaningless "X relative to X" empty delta, so require it.
            if self.pending.is_some() && self.view.is_none() {
                return Err(CliError::InvalidArgument {
                    message:
                        "--pending <parent_view> requires --view <agent_view> (the forked view to summarize)"
                            .to_string(),
                });
            }
            return self.show_summary(&repo);
        }

        // Show details for a specific attestation
        if let Some(ref hash_prefix) = self.hash {
            return self.show_detail(&repo, hash_prefix);
        }

        // Filter by view
        if let Some(ref view_name) = self.view {
            return self.show_for_view(&repo, view_name);
        }

        // List all attestations
        self.list_all(&repo)
    }
}

impl Attest {
    /// Print a project-level AI provenance summary.
    ///
    /// Reads each change's embedded provenance (independent of attestations)
    /// and classifies as AI / Human / Needs-attention / System. The headline
    /// metric is `ai_authored_pct = AI / (AI + Human)`.
    ///
    /// Default output is terse and product-facing: the headline %, the human
    /// count, and the AI source / tool breakdown. Implementation detail
    /// (system/bootstrap exclusions, model breakdown, zero-count data-quality
    /// lines) is shown only under `--verbose`. Non-zero data-quality
    /// conditions (needs-attention, unreadable) always surface as warnings.
    fn show_summary(&self, repo: &Repository) -> CliResult<()> {
        let view_name = self
            .view
            .clone()
            .unwrap_or_else(|| repo.current_view().to_string());

        let (summary, header_line) = if let Some(parent_view) = self.pending.as_deref() {
            let s = repo
                .provenance_summary_pending(&view_name, parent_view)
                .map_err(CliError::Repository)?;
            (s, format!("Pending work: {} → {}", view_name, parent_view))
        } else {
            let s = repo
                .provenance_summary(&view_name)
                .map_err(CliError::Repository)?;
            (s, format!("Project: {}", view_name))
        };

        println!("{}", header_line);
        println!();

        // Headline.
        match summary.ai_authored_pct() {
            None => {
                println!("No authored changes in this view yet.");
            }
            Some(pct) => {
                let denom = summary.authored_denominator();
                println!(
                    "AI-authored: {:.0}% ({} of {} authored change{})",
                    pct,
                    summary.ai_changes,
                    denom,
                    if denom == 1 { "" } else { "s" },
                );
                if summary.human_changes > 0 {
                    println!(
                        "Human-authored: {} change{}",
                        summary.human_changes,
                        if summary.human_changes == 1 { "" } else { "s" },
                    );
                }
                if !summary.by_vendor.is_empty() {
                    let parts: Vec<String> = summary
                        .by_vendor
                        .iter()
                        .map(|(v, n)| format!("{} {}", pretty_vendor(v), n))
                        .collect();
                    println!("AI sources: {}", parts.join(" · "));
                }
                if !summary.by_tool.is_empty() {
                    let parts: Vec<String> = summary
                        .by_tool
                        .iter()
                        .map(|(t, n)| format!("{} {}", pretty_tool(t), n))
                        .collect();
                    println!("Tools: {}", parts.join(" · "));
                }
            }
        }

        // Verbose: internal accounting.
        if self.verbose {
            println!();
            println!(
                "System/bootstrap changes excluded: {}",
                summary.system_changes
            );
            println!("Needs attention: {}", summary.needs_attention_changes);
            println!("Unreadable: {}", summary.unreadable_changes);
            if !summary.by_model.is_empty() {
                let parts: Vec<String> = summary
                    .by_model
                    .iter()
                    .map(|(m, n)| format!("{} {}", m, n))
                    .collect();
                println!("Models: {}", parts.join(" · "));
            }
        }

        // Non-zero data-quality conditions always surface (even without -v).
        if summary.needs_attention_changes > 0 {
            println!();
            print_warning(&format!(
                "{} change(s) have an agent-identity author but no embedded provenance — possible recording pipeline issue. Excluded from the percentage.",
                summary.needs_attention_changes,
            ));
        }
        if summary.unreadable_changes > 0 {
            println!();
            print_warning(&format!(
                "{} change(s) could not be loaded and are excluded from the percentage.",
                summary.unreadable_changes,
            ));
        }

        // On the canonical path, if the user actually selected a forked /
        // draft view, point them at --pending. Detection is exact (via the
        // view's recorded parent), so this never fires for canonical views.
        if self.pending.is_none() {
            if let Ok(info) = repo.get_view_info(&view_name) {
                if let Some(parent) = info.parent_name {
                    println!();
                    println!("This looks like a draft view. To summarize only pending work:");
                    println!(
                        "  atomic agent attest --pending {} --view {}",
                        parent, view_name,
                    );
                }
            }
        }

        Ok(())
    }

    /// List all attestations in the repository.
    fn list_all(&self, repo: &Repository) -> CliResult<()> {
        let mut attestations: Vec<(Hash, Attestation)> = Vec::new();

        for result in repo.change_store().iter_attestations() {
            let hash = match result {
                Ok(h) => h,
                Err(e) => {
                    print_warning(&format!("Skipping corrupt attestation: {}", e));
                    continue;
                }
            };

            match repo.load_attestation(&hash) {
                Ok(attest) => attestations.push((hash, attest)),
                Err(e) => {
                    print_warning(&format!(
                        "Failed to load attestation {}: {}",
                        format_hash(&hash, false),
                        e
                    ));
                }
            }
        }

        if attestations.is_empty() {
            println!("No attestations in this repository.");
            println!();
            println!("Attestations are created when AI agent sessions end.");
            println!("They capture cost, token usage, and model breakdown.");
            return Ok(());
        }

        // Sort by timestamp descending (newest first)
        attestations.sort_by_key(|a| std::cmp::Reverse(a.1.timestamp));

        println!(
            "{} attestation{}",
            attestations.len(),
            if attestations.len() == 1 { "" } else { "s" }
        );
        println!();

        for (hash, attest) in &attestations {
            self.print_summary(hash, attest);

            if self.verbose {
                self.print_models(attest);
                println!();
            }
        }

        let total_changes: usize = attestations.iter().map(|(_, a)| a.change_count()).sum();
        let total_cost: f64 = attestations.iter().map(|(_, a)| a.cost_usd).sum();
        let total_tokens: u64 = attestations.iter().map(|(_, a)| a.total_tokens()).sum();

        println!("──────────────────────────────────────────");
        let mut summary = format!(
            "{} {} covered",
            total_changes,
            if total_changes == 1 {
                "change"
            } else {
                "changes"
            },
        );
        if total_cost > 0.0 {
            summary = format!("{}  ·  {}", format_cost(total_cost), summary);
        }
        if total_tokens > 0 {
            summary = format!("{}  ·  {} tokens", summary, format_tokens(total_tokens));
        }
        println!("Total: {}", summary);

        Ok(())
    }

    /// Show attestations for a specific view.
    fn show_for_view(&self, repo: &Repository, view_name: &str) -> CliResult<()> {
        let results = repo
            .find_attestations_for_view(view_name)
            .map_err(CliError::Repository)?;

        if results.is_empty() {
            println!("No attestations cover changes in view '{}'.", view_name);
            return Ok(());
        }

        println!(
            "{} attestation{} covering changes in '{}'",
            results.len(),
            if results.len() == 1 { "" } else { "s" },
            view_name,
        );
        println!();

        for (hash, attest, covered_in_view) in &results {
            self.print_summary(hash, attest);
            println!(
                "    Coverage in {}: {}/{} changes",
                view_name,
                covered_in_view.len(),
                attest.change_count(),
            );

            if self.verbose {
                self.print_models(attest);
                self.print_changes(attest);
            }
            println!();
        }

        Ok(())
    }

    /// Show full details for a specific attestation.
    fn show_detail(&self, repo: &Repository, hash_prefix: &str) -> CliResult<()> {
        // Find the attestation by hash or prefix
        let (hash, attest) = self.find_by_prefix(repo, hash_prefix)?;

        println!("Attestation {}", format_hash(&hash, false));
        println!();

        // Agent info
        println!("Agent:     {}", attest.agent);
        println!("Session:   {}", attest.session_id);
        println!(
            "Changes:   {}",
            format_count(attest.change_count(), "change")
        );

        if attest.duration_wall_ms > 0 {
            println!("Wall time: {}", attest.wall_duration_display());
        }
        if attest.duration_api_ms > 0 {
            println!("API time:  {}", attest.api_duration_display());
        }
        if attest.cost_usd > 0.0 {
            println!("Cost:      {}", format_cost(attest.cost_usd));
        }
        if attest.total_tokens() > 0 {
            println!("Tokens:    {}", format_tokens(attest.total_tokens()));
        }
        if !attest.code_changes.is_empty() {
            println!(
                "Code:      +{} -{}",
                attest.code_changes.lines_added, attest.code_changes.lines_removed,
            );
        }

        if attest.cost_usd == 0.0 && attest.total_tokens() == 0 {
            println!();
            println!("Note: Cost and token data pending.");
            println!("  Claude Code does not expose this in the SessionEnd hook.");
            println!("  Use 'atomic agent attest --enrich' when available.");
        }

        if let Some(ref prev) = attest.previous_attestation {
            println!("Previous:  {}", format_hash(prev, false));
        }

        if let Some(ref notes) = attest.notes {
            println!("Notes:     {}", notes);
        }

        println!();

        // Model breakdown (only if there's data)
        if !attest.models.is_empty() {
            println!("Model Breakdown:");
            for model in &attest.models {
                println!("  {}", model);
            }
            println!();
        }

        // Changes covered
        self.print_changes(&attest);
        println!();

        // Coverage per view
        self.print_coverage(repo, &attest);

        Ok(())
    }

    /// Find an attestation by hash or hash prefix.
    fn find_by_prefix(&self, repo: &Repository, prefix: &str) -> CliResult<(Hash, Attestation)> {
        // Try exact match first
        if let Some(hash) = Hash::from_base32(prefix.as_bytes()) {
            if let Ok(attest) = repo.load_attestation(&hash) {
                return Ok((hash, attest));
            }
        }

        // Prefix search
        let prefix_lower = prefix.to_uppercase();
        let mut matches: Vec<(Hash, Attestation)> = Vec::new();

        for result in repo.change_store().iter_attestations() {
            let hash = match result {
                Ok(h) => h,
                Err(_) => continue,
            };

            let hash_str = hash.to_base32();
            if hash_str.starts_with(&prefix_lower) {
                if let Ok(attest) = repo.load_attestation(&hash) {
                    matches.push((hash, attest));
                }
            }
        }

        match matches.len() {
            0 => Err(CliError::InvalidArgument {
                message: format!("No attestation found matching '{}'", prefix),
            }),
            1 => Ok(matches.into_iter().next().unwrap()),
            n => Err(CliError::InvalidArgument {
                message: format!(
                    "Ambiguous hash prefix '{}' matches {} attestations. Be more specific.",
                    prefix, n,
                ),
            }),
        }
    }

    /// Print a one-line summary of an attestation.
    fn print_summary(&self, hash: &Hash, attest: &Attestation) {
        let mut parts = vec![format!(
            "{} {}",
            format_hash(hash, false),
            attest.agent.display_name
        )];

        if attest.cost_usd > 0.0 {
            parts.push(format_cost(attest.cost_usd));
        }
        if attest.total_tokens() > 0 {
            parts.push(format!("{} tokens", format_tokens(attest.total_tokens())));
        }
        if attest.duration_wall_ms > 0 {
            parts.push(attest.wall_duration_display());
        }
        parts.push(format_count(attest.change_count(), "change"));

        println!("  {}", parts.join(" · "));
    }

    /// Print model breakdown.
    fn print_models(&self, attest: &Attestation) {
        for model in &attest.models {
            println!("    {}", model);
        }
    }

    /// Print changes covered.
    fn print_changes(&self, attest: &Attestation) {
        if attest.changes_covered.is_empty() {
            return;
        }

        println!("Changes Covered ({}):", attest.change_count(),);
        for change_hash in &attest.changes_covered {
            println!("  {}", format_hash(change_hash, false));
        }
    }

    /// Print coverage per view.
    fn print_coverage(&self, repo: &Repository, attest: &Attestation) {
        let views = match repo.list_views() {
            Ok(s) => s,
            Err(_) => return,
        };

        let covered_set: std::collections::HashSet<&Hash> = attest.changes_covered.iter().collect();

        let mut has_coverage = false;

        for view_name in &views {
            let history = match repo
                .log(atomic_repository::history::HistoryOptions::default().view(view_name))
            {
                Ok(h) => h,
                Err(_) => continue,
            };

            let total = history.len();
            if total == 0 {
                continue;
            }

            let covered = history
                .iter()
                .filter(|e| covered_set.contains(&e.hash))
                .count();

            if covered == 0 {
                continue;
            }

            if !has_coverage {
                println!("Coverage:");
                has_coverage = true;
            }

            let pct = (covered as f64 / total as f64) * 100.0;
            let bar_width = 20;
            let filled = ((pct / 100.0) * bar_width as f64) as usize;
            let empty = bar_width - filled;
            let bar = format!("{}{}", "█".repeat(filled), "░".repeat(empty),);

            println!(
                "  {:<20} {} {}/{} ({:.0}%)",
                view_name, bar, covered, total, pct,
            );
        }
    }
}

// Formatting Helpers

/// Prettify a canonical vendor name (from `AIVendor::name()`, lowercase) into
/// a product-facing label. Falls back to the raw string for unknown vendors.
fn pretty_vendor(canonical: &str) -> String {
    match canonical {
        "anthropic" => "Anthropic",
        "openai" => "OpenAI",
        "google" => "Google",
        "meta" => "Meta",
        "mistral" => "Mistral",
        "cohere" => "Cohere",
        "amazon-bedrock" => "Amazon Bedrock",
        "azure-openai" => "Azure OpenAI",
        "local" => "Local",
        other => other,
    }
    .to_string()
}

/// Prettify a tool label from `AITool::description()` (e.g. "CLI: claude-code")
/// into a product-facing name. Drops the access-method prefix, then maps the
/// known agent registry keys to display names (e.g. "claude-code" →
/// "Claude Code"). Unknown names and prefix-less variants (e.g. "API") pass
/// through unchanged.
fn pretty_tool(description: &str) -> String {
    let name = match description.split_once(": ") {
        Some((_prefix, name)) => name,
        None => description,
    };
    match name {
        "claude-code" => "Claude Code",
        "codex" => "Codex",
        "gemini-cli" => "Gemini CLI",
        "agy" => "Antigravity CLI",
        "cursor" => "Cursor",
        "devin" => "Devin Desktop",
        "cline" => "Cline",
        "opencode" => "OpenCode",
        "copilot" => "Copilot",
        "sherpa" => "Sherpa",
        "pi" => "Pi",
        "kilo" => "Kilo Code",
        other => other,
    }
    .to_string()
}

fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        format!("{}", tokens)
    }
}

fn format_cost(usd: f64) -> String {
    if usd == 0.0 {
        "$0.00".to_string()
    } else if usd < 0.01 {
        format!("${:.4}", usd)
    } else {
        format!("${:.2}", usd)
    }
}

fn format_count(n: usize, word: &str) -> String {
    if n == 1 {
        format!("{} {}", n, word)
    } else {
        format!("{} {}s", n, word)
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_attest_default_for_test() {
        let cmd = Attest::default_for_test();
        assert!(cmd.hash.is_none());
        assert!(cmd.view.is_none());
        assert!(!cmd.verbose);
    }

    #[test]
    fn test_attest_with_hash() {
        let cmd = Attest {
            hash: Some("XMJZ3IPF".to_string()),
            view: None,
            verbose: false,
            summary: false,
            pending: None,
        };
        assert_eq!(cmd.hash.as_deref(), Some("XMJZ3IPF"));
    }

    #[test]
    fn test_attest_with_view() {
        let cmd = Attest {
            hash: None,
            view: Some("dev".to_string()),
            verbose: false,
            summary: false,
            pending: None,
        };
        assert_eq!(cmd.view.as_deref(), Some("dev"));
    }

    #[test]
    fn test_attest_verbose() {
        let cmd = Attest {
            hash: None,
            view: None,
            verbose: true,
            summary: false,
            pending: None,
        };
        assert!(cmd.verbose);
    }

    #[test]
    fn test_attest_summary_flag() {
        let cmd = Attest {
            hash: None,
            view: None,
            verbose: false,
            summary: true,
            pending: None,
        };
        assert!(cmd.summary);
    }

    #[test]
    fn test_attest_pending_flag() {
        let cmd = Attest {
            hash: None,
            view: Some("solitary-flower-0915".to_string()),
            verbose: false,
            summary: false,
            pending: Some("dev".to_string()),
        };
        assert_eq!(cmd.pending.as_deref(), Some("dev"));
        assert_eq!(cmd.view.as_deref(), Some("solitary-flower-0915"));
    }

    #[test]
    fn test_format_tokens_small() {
        assert_eq!(format_tokens(42), "42");
        assert_eq!(format_tokens(999), "999");
    }

    #[test]
    fn test_format_tokens_thousands() {
        assert_eq!(format_tokens(1_000), "1.0k");
        assert_eq!(format_tokens(8_400), "8.4k");
        assert_eq!(format_tokens(526_900), "526.9k");
    }

    #[test]
    fn test_format_tokens_millions() {
        assert_eq!(format_tokens(1_000_000), "1.0M");
        assert_eq!(format_tokens(2_500_000), "2.5M");
    }

    #[test]
    fn test_format_cost_zero() {
        assert_eq!(format_cost(0.0), "$0.00");
    }

    #[test]
    fn test_format_cost_small() {
        assert_eq!(format_cost(0.0057), "$0.0057");
    }

    #[test]
    fn test_format_cost_normal() {
        assert_eq!(format_cost(1.23), "$1.23");
    }

    #[test]
    fn test_format_cost_large() {
        assert_eq!(format_cost(42.50), "$42.50");
    }

    #[test]
    fn test_format_count_singular() {
        assert_eq!(format_count(1, "change"), "1 change");
    }

    #[test]
    fn test_format_count_plural() {
        assert_eq!(format_count(0, "change"), "0 changes");
        assert_eq!(format_count(5, "change"), "5 changes");
    }

    #[test]
    fn test_pretty_vendor_known() {
        assert_eq!(pretty_vendor("anthropic"), "Anthropic");
        assert_eq!(pretty_vendor("openai"), "OpenAI");
        assert_eq!(pretty_vendor("google"), "Google");
        assert_eq!(pretty_vendor("azure-openai"), "Azure OpenAI");
    }

    #[test]
    fn test_pretty_vendor_unknown_passes_through() {
        assert_eq!(pretty_vendor("some-future-vendor"), "some-future-vendor");
    }

    #[test]
    fn test_pretty_tool_strips_prefix_and_maps_display_name() {
        assert_eq!(pretty_tool("CLI: claude-code"), "Claude Code");
        assert_eq!(pretty_tool("Editor: cursor"), "Cursor");
        assert_eq!(pretty_tool("IDE Plugin: copilot"), "Copilot");
        assert_eq!(pretty_tool("CLI: gemini-cli"), "Gemini CLI");
        assert_eq!(pretty_tool("CLI: kilo"), "Kilo Code");
    }

    #[test]
    fn test_pretty_tool_unknown_strips_prefix_only() {
        // Unknown tool keys still drop the prefix but aren't remapped.
        assert_eq!(pretty_tool("CLI: future-agent"), "future-agent");
    }

    #[test]
    fn test_pretty_tool_no_prefix_passes_through() {
        assert_eq!(pretty_tool("API"), "API");
        assert_eq!(pretty_tool("Chat"), "Chat");
    }
}
