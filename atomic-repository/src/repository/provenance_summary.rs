//! Project-level AI provenance summary.
//!
//! Computes "what fraction of a view's changes are AI-authored" by reading
//! each change's embedded [`Change::provenance()`](atomic_core::change::Change::provenance).
//! This is decoupled from attestations: the summary is correct even when
//! attestations are missing or have incorrect coverage.
//!
//! # Classification (four buckets — see development/atomic-attest-fixes.md §3.2)
//!
//! - **AI** — `change.has_provenance()` is `true`. Bucketed by vendor / tool / model.
//! - **Human** — no provenance AND author is not an agent identity.
//! - **NeedsAttention** — no provenance BUT the author looks like an agent
//!   identity (either `<normalized_agent>+<short>` from `build_agent_author` when
//!   a user identity is configured, or one of the known agent display-name
//!   fallbacks like `"Claude Code"`/`"Codex"` when no identity is configured).
//!   Indicates a data-quality issue — must NOT be counted as Human.
//! - **System** — no authors recorded (init / vault / bootstrap).
//!
//! # Headline metric
//!
//! `ai_authored_pct = AI / (AI + Human)`. System and NeedsAttention are excluded
//! from the headline denominator but reported separately. A separate
//! `ai_all_changes_pct` (AI / all) is also available for transparency.
//!
//! # Canonical vs pending (`provenance_summary_pending`)
//!
//! `provenance_summary(view)` scans the *full* history of `view`. That's the
//! right thing for canonical views (`dev`) — the result describes the project
//! state. For an **agent / draft view forked from a parent**, the full history
//! includes everything inherited from the parent, which would inflate the
//! denominator with the parent's human work. Use
//! [`Repository::provenance_summary_pending`] to classify only the **delta**
//! between the agent view and its parent.

use std::collections::BTreeMap;

use atomic_core::change::Author;

use super::Repository;
use crate::error::RepositoryError;
use crate::history::HistoryOptions;

/// Summary of AI-vs-human change attribution for a single view.
///
/// Counts changes (not lines). For a line-level summary, see follow-up
/// work on `file_ops` line counts (not yet implemented).
#[derive(Debug, Clone, Default)]
pub struct ProvenanceSummary {
    /// The view this summary covers.
    pub view_name: String,
    /// Changes with embedded AI provenance.
    pub ai_changes: usize,
    /// Changes authored by humans (no provenance, non-agent author).
    pub human_changes: usize,
    /// Changes whose author is an agent identity but lacks provenance —
    /// indicates a recording / attestation pipeline bug.
    pub needs_attention_changes: usize,
    /// Bootstrap / init / vault changes (no authors recorded).
    pub system_changes: usize,
    /// Changes the storage layer could not load. These DO NOT enter the
    /// denominator — they're reported separately so silent skips can't
    /// quietly distort the percentage.
    pub unreadable_changes: usize,
    /// AI change counts bucketed by vendor (one increment per unique vendor
    /// present in a change; multi-vendor changes are counted in each bucket).
    pub by_vendor: BTreeMap<String, usize>,
    /// AI change counts bucketed by tool description.
    pub by_tool: BTreeMap<String, usize>,
    /// AI change counts bucketed by model name.
    pub by_model: BTreeMap<String, usize>,
}

impl ProvenanceSummary {
    /// Total number of changes classified (excludes `unreadable_changes`,
    /// which never enter any bucket).
    pub fn total_changes(&self) -> usize {
        self.ai_changes
            + self.human_changes
            + self.needs_attention_changes
            + self.system_changes
    }

    /// Denominator for the headline metric: changes authored by a person or an AI.
    /// Excludes System and NeedsAttention.
    pub fn authored_denominator(&self) -> usize {
        self.ai_changes + self.human_changes
    }

    /// Headline metric: percent of human-or-AI-authored changes that are AI.
    /// Returns `None` when the denominator is zero (no authored changes).
    pub fn ai_authored_pct(&self) -> Option<f64> {
        let denom = self.authored_denominator();
        if denom == 0 {
            None
        } else {
            Some((self.ai_changes as f64) * 100.0 / denom as f64)
        }
    }

    /// Transparency metric: percent of ALL changes that are AI (denominator
    /// includes System and NeedsAttention). Named distinctly so it cannot be
    /// confused with the headline `ai_authored_pct`.
    pub fn ai_all_changes_pct(&self) -> Option<f64> {
        let total = self.total_changes();
        if total == 0 {
            None
        } else {
            Some((self.ai_changes as f64) * 100.0 / total as f64)
        }
    }
}

impl Repository {
    /// Compute the AI provenance summary for the **full history** of `view_name`.
    ///
    /// Right for canonical views (e.g., `dev`). For agent / draft views that
    /// inherit from a parent, this includes inherited changes — use
    /// [`Self::provenance_summary_pending`] instead.
    pub fn provenance_summary(
        &self,
        view_name: &str,
    ) -> Result<ProvenanceSummary, RepositoryError> {
        let history = self.log(HistoryOptions::default().view(view_name))?;
        let hashes: Vec<_> = history.iter().map(|e| e.hash).collect();
        self.classify_hashes(view_name, &hashes)
    }

    /// Compute the AI provenance summary for the **delta** between an agent
    /// view and its parent — only the changes present in `agent_view` and
    /// *not* in `parent_view`.
    ///
    /// Use this for any forked / draft view to avoid inflating the
    /// denominator with the parent's inherited (human / system / AI) changes.
    pub fn provenance_summary_pending(
        &self,
        agent_view: &str,
        parent_view: &str,
    ) -> Result<ProvenanceSummary, RepositoryError> {
        let delta = self.get_missing_changes_between(agent_view, Some(parent_view))?;
        self.classify_hashes(agent_view, &delta)
    }

    fn classify_hashes(
        &self,
        view_name: &str,
        hashes: &[atomic_core::types::Hash],
    ) -> Result<ProvenanceSummary, RepositoryError> {
        let mut summary = ProvenanceSummary {
            view_name: view_name.to_string(),
            ..Default::default()
        };

        for hash in hashes {
            let change = match self.load_change(hash) {
                Ok(c) => c,
                Err(_) => {
                    summary.unreadable_changes += 1;
                    continue;
                }
            };

            let provs = change.provenance();
            if !provs.is_empty() {
                summary.ai_changes += 1;

                // Bucket once per unique vendor / tool / model present in this change.
                let mut seen_vendors = std::collections::BTreeSet::new();
                let mut seen_tools = std::collections::BTreeSet::new();
                let mut seen_models = std::collections::BTreeSet::new();
                for p in provs {
                    let v = p.vendor.name().to_string();
                    if seen_vendors.insert(v.clone()) {
                        *summary.by_vendor.entry(v).or_insert(0) += 1;
                    }
                    let t = p.tool.description();
                    if seen_tools.insert(t.clone()) {
                        *summary.by_tool.entry(t).or_insert(0) += 1;
                    }
                    let m = p.model.clone();
                    if seen_models.insert(m.clone()) {
                        *summary.by_model.entry(m).or_insert(0) += 1;
                    }
                }
                continue;
            }

            // No provenance — disambiguate by author.
            if change.hashed.header.authors.is_empty() {
                summary.system_changes += 1;
            } else if change
                .hashed
                .header
                .authors
                .iter()
                .any(author_is_agent_identity)
            {
                summary.needs_attention_changes += 1;
            } else {
                summary.human_changes += 1;
            }
        }

        Ok(summary)
    }
}

/// Normalized agent names produced by `atomic_agent::identity::normalize_agent_name`
/// (first `-`-delimited segment, lowercased). These are the *prefixes* that
/// appear in the `<agent>+<short>` author shape. Keep in sync with the agent
/// registry (atomic-agent/src/hooks/).
const KNOWN_AGENT_PREFIXES: &[&str] = &[
    "claude", "codex", "gemini", "cursor", "cline", "opencode", "copilot", "sherpa", "pi",
];

/// Heuristically detect an agent-identity Author produced by
/// `atomic_agent::identity::build_agent_author`.
///
/// `build_agent_author` has two output shapes:
///
/// 1. **User identity configured** — `claude+60f5` style:
///    `<normalized_agent_name>+<3-8 char alphanumeric session short>`. We
///    require the prefix to be a *known* normalized agent name — otherwise a
///    real human author like `alice+2026` or `lee+laptop` would be
///    misclassified as an agent and wrongly removed from the Human denominator.
///
/// 2. **No user identity (fallback)** — just the agent's display name with
///    no email, e.g. `"Claude Code"` or `"Codex"`
///    (see `atomic_agent::identity::fallback_agent_author`). We match a
///    known set of agent display / registry names. Missing one here means
///    a fallback-author change without provenance gets misclassified as
///    Human — a false negative for the NeedsAttention data-quality signal.
fn author_is_agent_identity(author: &Author) -> bool {
    // Shape 1: <known-agent>+<short>
    if let Some((prefix, suffix)) = author.name.split_once('+') {
        if KNOWN_AGENT_PREFIXES.contains(&prefix) {
            let suffix_len = suffix.len();
            if (3..=8).contains(&suffix_len) && suffix.chars().all(|c| c.is_ascii_alphanumeric()) {
                return true;
            }
        }
    }

    // Shape 2: fallback display / registry names when no user identity is set.
    matches!(
        author.name.as_str(),
        "Claude Code"
            | "claude-code"
            | "Codex"
            | "codex"
            | "Gemini CLI"
            | "gemini-cli"
            | "Cursor"
            | "cursor"
            | "Cline"
            | "cline"
            | "OpenCode"
            | "opencode"
            | "Copilot"
            | "copilot"
            | "Sherpa"
            | "sherpa"
            | "Pi"
            | "pi"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn author(name: &str) -> Author {
        Author::new(name, None::<String>)
    }

    #[test]
    fn detects_agent_identity_canonical() {
        assert!(author_is_agent_identity(&author("claude+60f5")));
        assert!(author_is_agent_identity(&author("codex+abc1")));
        assert!(author_is_agent_identity(&author("gemini+a1b2c3d4")));
    }

    #[test]
    fn detects_agent_identity_fallback_display_names() {
        // When no user identity is configured, build_agent_author returns
        // the agent's display_name with no email. These changes must still
        // be classified as NeedsAttention if provenance is missing.
        assert!(author_is_agent_identity(&author("Claude Code")));
        assert!(author_is_agent_identity(&author("claude-code")));
        assert!(author_is_agent_identity(&author("Codex")));
        assert!(author_is_agent_identity(&author("Gemini CLI")));
    }

    #[test]
    fn rejects_human_authors() {
        assert!(!author_is_agent_identity(&author("Vincent Ruan")));
        assert!(!author_is_agent_identity(&author("alice")));
        assert!(!author_is_agent_identity(&author("Lee Faus")));
        // Sanity: a human whose name *contains* "claude" but isn't an agent
        // shape and isn't in the known display-name set.
        assert!(!author_is_agent_identity(&author("Claude Monet")));
    }

    #[test]
    fn rejects_human_authors_with_plus_suffix() {
        // Real humans use `+` in git author names/emails. The `+<short>`
        // shape must ONLY match known agent prefixes, otherwise these humans
        // get wrongly pulled out of the Human denominator.
        assert!(!author_is_agent_identity(&author("alice+2026")));
        assert!(!author_is_agent_identity(&author("lee+laptop")));
        assert!(!author_is_agent_identity(&author("bob+work1")));
    }

    #[test]
    fn rejects_no_plus_separator() {
        assert!(!author_is_agent_identity(&author("claude")));
        assert!(!author_is_agent_identity(&author("")));
    }

    #[test]
    fn rejects_implausible_suffix() {
        assert!(!author_is_agent_identity(&author("foo+")));
        assert!(!author_is_agent_identity(&author("foo+ab")));
        assert!(!author_is_agent_identity(&author("foo+toolongsuffixhere")));
        assert!(!author_is_agent_identity(&author("foo+has-dash")));
        assert!(!author_is_agent_identity(&author("foo+has space")));
    }

    #[test]
    fn rejects_empty_prefix() {
        assert!(!author_is_agent_identity(&author("+60f5")));
    }

    #[test]
    fn summary_default_is_empty() {
        let s = ProvenanceSummary::default();
        assert_eq!(s.total_changes(), 0);
        assert_eq!(s.authored_denominator(), 0);
        assert_eq!(s.ai_authored_pct(), None);
        assert_eq!(s.ai_all_changes_pct(), None);
    }

    #[test]
    fn ai_authored_pct_excludes_system() {
        let mut s = ProvenanceSummary::default();
        s.ai_changes = 4;
        s.human_changes = 1;
        s.system_changes = 2; // bootstrap excluded from headline denominator
        assert_eq!(s.authored_denominator(), 5);
        assert!((s.ai_authored_pct().unwrap() - 80.0).abs() < 1e-9);
        // ai_all_changes_pct uses total: 4 / 7
        assert!((s.ai_all_changes_pct().unwrap() - (4.0 * 100.0 / 7.0)).abs() < 1e-9);
    }

    #[test]
    fn ai_authored_pct_none_when_only_system() {
        let mut s = ProvenanceSummary::default();
        s.system_changes = 3;
        assert_eq!(s.authored_denominator(), 0);
        assert_eq!(s.ai_authored_pct(), None);
    }

    #[test]
    fn needs_attention_does_not_count_as_human() {
        let mut s = ProvenanceSummary::default();
        s.ai_changes = 1;
        s.human_changes = 0;
        s.needs_attention_changes = 2; // must NOT be in denominator
        assert_eq!(s.authored_denominator(), 1);
        assert_eq!(s.ai_authored_pct(), Some(100.0));
    }

    #[test]
    fn unreadable_changes_do_not_skew_percent() {
        // Unreadable changes are reported separately, NOT counted in any
        // bucket — they must not silently change the denominator.
        let mut s = ProvenanceSummary::default();
        s.ai_changes = 4;
        s.human_changes = 1;
        s.unreadable_changes = 3;
        assert_eq!(s.authored_denominator(), 5);
        assert_eq!(s.ai_authored_pct(), Some(80.0));
        // total_changes excludes unreadable by design
        assert_eq!(s.total_changes(), 5);
    }
}
