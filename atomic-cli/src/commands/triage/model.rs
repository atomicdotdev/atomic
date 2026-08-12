//! The canonical triage report model (milestone T3).
//!
//! One model, projected to skins. Every finding and criterion carries both a
//! machine face (closed-vocab `code` / `severity` / `suggested_query`) and a
//! human face (`message` / `remedy`), mirroring the structure documented in
//! `atomic/docs/triage.md` ("Output: one model, four skins"). This module only
//! defines the serializable shapes; [`super::project`] populates them and
//! [`super::output`] renders the bounded CLI skin.

use std::collections::BTreeMap;

use serde::Serialize;

use atomic_canonical::vocab::FINDING_CODE;

// ── Finding codes (the closed set) ──────────────────────────────────────────
//
// These are aliases into the single source of truth, `atomic_canonical::vocab::
// FINDING_CODE`, so a finding can never carry a code outside the closed
// vocabulary. The index-based binding is verified by `finding_codes_match` in
// the tests below (it fails loudly if the vocab is ever reordered).

/// A materialized view failed its baseline: a view-scoped verification record
/// with a failing outcome on a reached intent's acceptance criterion.
pub(crate) const F_VIEW_VERIFY_FAIL: &str = FINDING_CODE[0];
pub(crate) const F_GATE_VIOLATION: &str = FINDING_CODE[1];
pub(crate) const F_SCOPE_OUT_BREACH: &str = FINDING_CODE[2];
pub(crate) const F_ORPHAN_CHANGE: &str = FINDING_CODE[3];
pub(crate) const F_MET_AC_NO_EVIDENCE: &str = FINDING_CODE[4];
pub(crate) const F_UNMET_AC_WITH_CANDIDATE: &str = FINDING_CODE[5];
pub(crate) const F_BAGGAGE_DEP: &str = FINDING_CODE[6];
/// A caller of modified code that lives outside the candidate change-set — a
/// blast-radius entity that may be affected but is not itself under review.
pub(crate) const F_BLAST_UNREVIEWED: &str = FINDING_CODE[7];
/// A granted `done` whose reviewable substance drifted from its triage pin
/// (T5b freshness reconciliation).
pub(crate) const F_STALE_TRIAGE: &str = FINDING_CODE[8];
/// A promoted intent has an in-flight `REMEDIATES`-linked intent (T6b).
/// Informational — surfaced, never blocking.
pub(crate) const F_OPEN_REMEDIATION: &str = FINDING_CODE[9];
/// A reached work intent whose changes are not covered by an independent,
/// completed review intent (the review-coverage promotion gate).
pub(crate) const F_UNREVIEWED_CHANGE: &str = FINDING_CODE[10];

// All eleven closed-vocabulary finding codes are now populated by `build_report`.

// ── Severities ──────────────────────────────────────────────────────────────

pub(crate) const SEV_BLOCK: &str = "block";
pub(crate) const SEV_WARN: &str = "warn";
pub(crate) const SEV_INFO: &str = "info";

// ── The report ──────────────────────────────────────────────────────────────

/// A triage verdict. Serialized lowercase (`ready` / `blocked` / `stale`).
///
/// `Stale` is emitted by T5b freshness reconciliation: a granted `done` whose
/// substance drifted from its pin yields a `STALE_TRIAGE` finding and, absent
/// any blocking finding, a `stale` verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Ready,
    Blocked,
    Stale,
}

/// The full canonical triage report: a verdict, the pins that reproduce it, a
/// bounded summary, and the per-intent / per-change / per-finding detail.
#[derive(Debug, Clone, Serialize)]
pub struct TriageReport {
    /// The `urn:atomic:triage:<blake3>` content address of the pinned inputs.
    pub reference: String,
    pub verdict: Verdict,
    pub inputs: Inputs,
    pub summary: Summary,
    pub intents: Vec<IntentReport>,
    pub changes: Vec<ChangeReport>,
    pub findings: Vec<Finding>,
    /// The guided walkthrough: candidate modifications grouped into semantic
    /// layers (module clusters), in reading order — foundations first. A pure,
    /// deterministic projection of the pinned inputs; empty when the candidate
    /// set modifies no files.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub walkthrough: Vec<WalkthroughLayer>,
}

/// The pinned inputs the report is a fact about (the evidence-bundle role).
#[derive(Debug, Clone, Serialize)]
pub struct Inputs {
    pub feature: String,
    pub target: String,
    pub view_merkle: String,
    pub candidate_changes: Vec<String>,
    pub closure_additions: Vec<String>,
    /// Intent id → `intentSubstanceHash` at review time. `BTreeMap` for a
    /// canonical, reproducible key order.
    pub intent_substance_hashes: BTreeMap<String, String>,
}

/// Bounded counts — the summary skin the CLI leads with.
#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub changes: usize,
    pub files: usize,
    pub criteria_met: usize,
    pub criteria_unmet: usize,
    pub findings_block: usize,
    pub findings_warn: usize,
    pub findings_info: usize,
}

/// An intent reached by the candidate set (a "testsuite").
#[derive(Debug, Clone, Serialize)]
pub struct IntentReport {
    pub id: String,
    pub why: Option<String>,
    pub conforms: bool,
    pub gate_violations: Vec<String>,
    pub criteria: Vec<CriterionReport>,
    /// The `attributedTo` of the independent, completed review intent that
    /// covers this work intent's changes, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewed_by: Option<String>,
}

/// A single acceptance criterion under an intent (a "testcase").
#[derive(Debug, Clone, Serialize)]
pub struct CriterionReport {
    pub id: String,
    pub text: String,
    pub status: String,
    pub verified_by: Option<String>,
    pub judgment_required: bool,
    /// Candidate change hashes (base32) whose tasks satisfy this criterion.
    pub satisfied_by: Vec<String>,
}

/// One changed file in a candidate change: a change-type symbol (`+`/`-`/`~`/
/// `±`), the path, and a per-file hunk summary — the same view `atomic change`
/// prints. No diff reconstruction; full content is one `review_command` away.
#[derive(Debug, Clone, Serialize)]
pub struct FileChange {
    pub symbol: String,
    pub path: String,
    pub summary: String,
}

/// A single unified-diff line: a tag (`+` add / `-` delete / ` ` context) and
/// the line's text content (no trailing newline).
#[derive(Debug, Clone, Serialize)]
pub struct DiffLineView {
    pub tag: String,
    pub content: String,
}

/// A single unified-diff hunk: its `@@ -a,b +c,d @@` header and its lines.
#[derive(Debug, Clone, Serialize)]
pub struct DiffHunkView {
    pub header: String,
    pub lines: Vec<DiffLineView>,
}

/// The real unified diff for one file in a candidate change.
#[derive(Debug, Clone, Serialize)]
pub struct DiffFileView {
    pub path: String,
    /// `added` / `modified` / `deleted`.
    pub status: String,
    pub hunks: Vec<DiffHunkView>,
}

/// A candidate change and the join facts attached to it.
#[derive(Debug, Clone, Serialize)]
pub struct ChangeReport {
    pub id: String,
    /// The change's commit message (trimmed). Empty if the change could not be
    /// loaded.
    pub message: String,
    pub modifies: Vec<String>,
    pub coverage: String,
    /// Per-file change summaries (symbol + path + hunk summary), from the
    /// change's own hunks. Empty when the change could not be loaded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<FileChange>,
    /// The real per-file unified diff (actual code hunks). Best-effort; empty
    /// when the change could not be diffed or was too large to embed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diff: Vec<DiffFileView>,
    /// The exact command to inspect the full change content.
    pub review_command: String,
    /// Caller entity ids outside the candidate change-set that reach this
    /// change's modified code (its blast radius). Best-effort, KG-dependent;
    /// empty when the call graph is not enriched.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blast_radius: Vec<String>,
    /// Compact provenance (session / agent / turn), best-effort; `None` if no
    /// provenance graph explains the change.
    pub provenance: Option<serde_json::Value>,
}

/// One semantic layer in the guided walkthrough: a cluster of modified files
/// (a module), the tasks/criteria/changes that land there, and a deterministic
/// prose rationale. The `Vec<WalkthroughLayer>` order on [`TriageReport`] IS
/// the reading order (foundations first, entry points last).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WalkthroughLayer {
    /// Stable layer id: `layer:<module>` (e.g. `layer:atomic-core/src/pristine`).
    pub id: String,
    /// Human title — the module path (or `(root)` for top-level files).
    pub title: String,
    /// Deterministic template prose explaining what this layer does and why it
    /// reads at this position — assembled from task text, criteria, and change
    /// messages. Never LLM-authored (the report must stay reproducible).
    pub rationale: String,
    /// Modified `file:<path>` node ids in this layer (keys into
    /// `changes[].modifies` / `changes[].diff`).
    pub files: Vec<String>,
    /// Canonical ids of intent tasks whose `::file-ref` touches land here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<String>,
    /// Acceptance-criterion ids those tasks satisfy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub criteria: Vec<String>,
    /// Candidate change hashes (base32) that modify files in this layer.
    pub changes: Vec<String>,
    /// Earlier layer ids this layer builds on (via `IMPORTS`/`INCLUDES` edges
    /// between modified files) — why it reads after them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
}

/// A finding — the machine + human face of one issue.
#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub code: String,
    pub severity: String,
    pub focus: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_query: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remedy: Option<String>,
}

impl Finding {
    pub(crate) fn new(
        code: &str,
        severity: &str,
        focus: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Finding {
            code: code.to_string(),
            severity: severity.to_string(),
            focus: focus.into(),
            message: message.into(),
            suggested_query: None,
            remedy: None,
        }
    }

    pub(crate) fn with_query(mut self, q: impl Into<String>) -> Self {
        self.suggested_query = Some(q.into());
        self
    }

    pub(crate) fn with_remedy(mut self, r: impl Into<String>) -> Self {
        self.remedy = Some(r.into());
        self
    }

    /// Sort rank: block (0) → warn (1) → info (2), then anything else last.
    pub(crate) fn severity_rank(&self) -> u8 {
        match self.severity.as_str() {
            SEV_BLOCK => 0,
            SEV_WARN => 1,
            SEV_INFO => 2,
            _ => 3,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_canonical::vocab::is_known_finding_code;

    #[test]
    fn finding_codes_match() {
        assert_eq!(F_GATE_VIOLATION, "GATE_VIOLATION");
        assert_eq!(F_SCOPE_OUT_BREACH, "SCOPE_OUT_BREACH");
        assert_eq!(F_ORPHAN_CHANGE, "ORPHAN_CHANGE");
        assert_eq!(F_MET_AC_NO_EVIDENCE, "MET_AC_NO_EVIDENCE");
        assert_eq!(F_UNMET_AC_WITH_CANDIDATE, "UNMET_AC_WITH_CANDIDATE");
        assert_eq!(F_BAGGAGE_DEP, "BAGGAGE_DEP");
        assert_eq!(F_STALE_TRIAGE, "STALE_TRIAGE");
        assert_eq!(F_OPEN_REMEDIATION, "OPEN_REMEDIATION");
        assert_eq!(F_VIEW_VERIFY_FAIL, "VIEW_VERIFY_FAIL");
        assert_eq!(F_BLAST_UNREVIEWED, "BLAST_UNREVIEWED");
        assert_eq!(F_UNREVIEWED_CHANGE, "UNREVIEWED_CHANGE");

        for c in [
            F_VIEW_VERIFY_FAIL,
            F_GATE_VIOLATION,
            F_SCOPE_OUT_BREACH,
            F_ORPHAN_CHANGE,
            F_MET_AC_NO_EVIDENCE,
            F_UNMET_AC_WITH_CANDIDATE,
            F_BAGGAGE_DEP,
            F_BLAST_UNREVIEWED,
            F_STALE_TRIAGE,
            F_OPEN_REMEDIATION,
            F_UNREVIEWED_CHANGE,
        ] {
            assert!(is_known_finding_code(c), "{c} must be in the closed set");
        }
    }

    #[test]
    fn verdict_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&Verdict::Blocked).unwrap(),
            "\"blocked\""
        );
        assert_eq!(serde_json::to_string(&Verdict::Ready).unwrap(), "\"ready\"");
        assert_eq!(serde_json::to_string(&Verdict::Stale).unwrap(), "\"stale\"");
    }
}
