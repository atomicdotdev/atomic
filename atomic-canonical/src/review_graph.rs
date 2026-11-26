//! Read-only review projection. Local graph IDs do not assert persisted KG facts.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::review_finding::{Confidence, CriterionOutcome, Evidence, ReviewFinding};

pub const SCHEMA: &str = "atomic.review-graph.v1";
pub const INPUT_SCHEMA: &str = "atomic.review-findings.v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewPass {
    AntiSlop,
    IntentConformance,
    BlastRadiusRisk,
}

impl ReviewPass {
    pub const ALL: [Self; 3] = [
        Self::AntiSlop,
        Self::IntentConformance,
        Self::BlastRadiusRisk,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::AntiSlop => "Anti-slop",
            Self::IntentConformance => "Intent conformance",
            Self::BlastRadiusRisk => "Blast radius / risk",
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::AntiSlop => "pass:anti_slop",
            Self::IntentConformance => "pass:intent_conformance",
            Self::BlastRadiusRisk => "pass:blast_radius_risk",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewScope {
    pub project: String,
    pub view: String,
    pub view_state: String,
    pub selected_roots: Vec<String>,
    pub dependency_closure: Vec<String>,
    /// Digest of ordered recorded source membership, including inherited caller
    /// context. These are not additional reviewed roots or a duplicated log.
    pub source_membership_hash: String,
    /// Current intent source hashes, not a claim of historical authentication.
    pub intent_sources: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmittedFinding {
    pub pass: ReviewPass,
    pub finding: ReviewFinding,
}

/// Optional substantive observations supplied by the actual inspector. This is
/// not signed evidence, and importing it confers no independent approval.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FindingsInput {
    pub schema: String,
    pub scope: ReviewScope,
    pub findings: Vec<SubmittedFinding>,
    /// Report-local judgment under the same exact source scope as findings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk_assessment: Option<RiskAssessment>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskRating {
    Unknown,
    Low,
    Medium,
    High,
    NotApplicable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskCoverage {
    Complete,
    Partial,
    Unavailable,
    NotApplicable,
}

/// Report-local rule estimate or reviewer assessment, not signed approval.
/// Complete coverage means the declared selected scope, not the whole project.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RiskAssessment {
    pub rating: RiskRating,
    pub confidence: Confidence,
    pub coverage: RiskCoverage,
    pub rationale: String,
    pub evidence: Vec<Evidence>,
    pub limitations: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RiskSummary {
    pub assessment: RiskAssessment,
    /// reviewer, findings, rule_model, or no_code; never independent approval.
    pub basis: String,
    pub findings: Vec<u32>,
    /// Diagnostic edges retained in this bounded graph, not repository totals.
    pub unverified_call_candidates: usize,
    pub unverified_import_candidates: usize,
    pub relationship_coverage: String,
    pub signals: Vec<String>,
    pub impact: Vec<atomic_audit::impact::ImpactReport>,
    pub model: Option<atomic_audit::risk::RiskEvaluation>,
}

#[derive(Clone, Debug, Serialize)]
pub struct NumberedFinding {
    pub number: u32,
    pub id: String,
    pub passes: Vec<ReviewPass>,
    pub finding: ReviewFinding,
    /// Task/file links are navigation hints only, not failing criterion claims.
    pub related_criteria: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comparison: Option<FindingComparison>,
}

#[derive(Clone, Debug, Serialize)]
pub struct FindingComparison {
    /// new/existing/resolved/changed/unknown local observations, not a bug verdict.
    pub status: String,
    pub before: Vec<crate::review_finding::Location>,
    pub after: Vec<crate::review_finding::Location>,
    pub rationale: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct CriterionReview {
    pub id: String,
    pub intent: String,
    pub text: String,
    pub declared_status: String,
    pub review_assessment: CriterionOutcome,
    pub findings: Vec<u32>,
    pub source_hash: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PassStatus {
    NotApplicable,
    Partial,
    Pending,
    Blocked,
}

#[derive(Clone, Debug, Serialize)]
pub struct PassResult {
    pub pass: ReviewPass,
    pub status: PassStatus,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct GraphNode {
    pub id: String,
    pub kind: String,
    pub label: String,
    pub properties: Value,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct GraphEdge {
    pub from: String,
    pub relation: String,
    pub to: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ReviewGraph {
    pub schema: String,
    pub scope: ReviewScope,
    pub passes: Vec<PassResult>,
    pub findings: Vec<NumberedFinding>,
    pub criteria: Vec<CriterionReview>,
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    pub diagnostics: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub risk: Option<RiskSummary>,
}
