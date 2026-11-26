//! Analyzer-neutral inspection evidence, not a signed publication approval.
//!
//! Structural analyzers and substantive review skills use the same contract.
//! Empty findings are meaningful only alongside the explicit check coverage.

use serde::{Deserialize, Serialize};

pub const SCHEMA: &str = "atomic.review-analysis.v1";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Low,
    Medium,
    High,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntroductionStatus {
    Yes,
    No,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Introduction {
    pub status: IntroductionStatus,
    pub rationale: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceSide {
    Before,
    After,
}

/// Positions are 1-based, inclusive, on the specified recorded side.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Location {
    pub path: String,
    pub side: SourceSide,
    pub start_line: u32,
    pub end_line: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleReference {
    pub id: String,
    pub explanation: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    Source,
    Diff,
    Test,
    Command,
    Reference,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub kind: EvidenceKind,
    pub reference: String,
    pub observation: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewFinding {
    pub analyzer: String,
    /// Exact recorded change hash, or a domain-separated local snapshot/file
    /// comparison reference. Never an inferred working-tree revision.
    pub change: String,
    pub summary: String,
    pub locations: Vec<Location>,
    pub requirement_or_rule: RuleReference,
    pub evidence: Vec<Evidence>,
    pub introduced_by_change: Introduction,
    pub severity: Severity,
    pub confidence: Confidence,
    pub proposed_correction: String,
    /// Explicit review judgments, never inferred merely from shared file paths.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acceptance_criteria: Vec<CriterionAssessment>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CriterionOutcome {
    Unmet,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CriterionAssessment {
    pub criterion: String,
    pub assessment: CriterionOutcome,
    /// Why this finding's evidence contradicts the criterion, or why unknown.
    pub rationale: String,
}

impl ReviewFinding {
    /// Reject structurally incomplete findings before presenting them as evidence.
    /// This checks presence, not the truth of the reviewer's judgment.
    pub fn validate(&self) -> Result<(), &'static str> {
        if [
            self.analyzer.as_str(),
            &self.change,
            &self.summary,
            &self.requirement_or_rule.id,
            &self.requirement_or_rule.explanation,
            &self.introduced_by_change.rationale,
            &self.proposed_correction,
        ]
        .iter()
        .any(|value| value.trim().is_empty())
        {
            return Err(
                "finding requires analyzer, change, summary, rule, rationale and correction",
            );
        }
        if self.locations.is_empty()
            || self.locations.iter().any(|location| {
                location.path.trim().is_empty()
                    || location.start_line == 0
                    || location.end_line < location.start_line
            })
        {
            return Err("finding requires valid 1-based recorded source locations");
        }
        if self.evidence.is_empty()
            || self
                .evidence
                .iter()
                .any(|item| item.reference.trim().is_empty() || item.observation.trim().is_empty())
        {
            return Err("finding requires referenced supporting evidence");
        }
        if self
            .acceptance_criteria
            .iter()
            .any(|link| link.criterion.trim().is_empty() || link.rationale.trim().is_empty())
        {
            return Err("criterion assessments require an exact criterion and evidenced rationale");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Completed,
    Skipped,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub change: String,
    pub path: String,
    pub rule: String,
    pub status: CheckStatus,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnalysisReport {
    pub schema: String,
    pub analyzer: String,
    pub analyzer_version: String,
    pub checks: Vec<Check>,
    pub findings: Vec<ReviewFinding>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub metrics: Vec<atomic_audit::metrics::ExtractedMetric>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub metric_diagnostics: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub metric_comparison_scope: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded_metadata_paths: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_analysis_without_metric_fields_is_not_given_fabricated_measurements() {
        let report: AnalysisReport = serde_json::from_value(serde_json::json!({
            "schema": SCHEMA, "analyzer": "legacy", "analyzer_version": "1", "checks": [], "findings": [],
        })).unwrap();
        assert!(report.excluded_metadata_paths.is_empty());
        assert!(
            report.metrics.is_empty()
                && report.metric_diagnostics.is_empty()
                && report.metric_comparison_scope.is_empty()
        );
    }

    #[test]
    fn contract_requires_rule_locations_evidence_and_introduction_rationale() {
        let mut finding = ReviewFinding {
            analyzer: "review-skill".into(),
            change: "exact-recorded-hash".into(),
            summary: "An error is silently discarded".into(),
            locations: vec![Location {
                path: "app.ts".into(),
                side: SourceSide::After,
                start_line: 3,
                end_line: 3,
            }],
            requirement_or_rule: RuleReference {
                id: "error-handling".into(),
                explanation: "Handle or propagate failures".into(),
            },
            evidence: vec![Evidence {
                kind: EvidenceKind::Source,
                reference: "app.ts:3".into(),
                observation: "catch (error) {}".into(),
            }],
            introduced_by_change: Introduction {
                status: IntroductionStatus::Unknown,
                rationale: "Before-content unavailable".into(),
            },
            severity: Severity::Medium,
            confidence: Confidence::Medium,
            proposed_correction: "Propagate the failure or document intentional suppression".into(),
            acceptance_criteria: Vec::new(),
        };
        finding.validate().unwrap();
        let value = serde_json::to_value(&finding).unwrap();
        assert_eq!(value["introduced_by_change"]["status"], "unknown");
        assert_eq!(
            serde_json::from_value::<ReviewFinding>(value).unwrap(),
            finding
        );
        let complete = finding.clone();
        finding.requirement_or_rule.id.clear();
        assert!(finding.validate().is_err());
        finding = complete.clone();
        finding.introduced_by_change.rationale.clear();
        assert!(finding.validate().is_err());
        finding = complete.clone();
        finding.evidence.clear();
        assert!(finding.validate().is_err());
        finding = complete;
        finding.locations[0].start_line = 0;
        assert!(finding.validate().is_err());
        finding.locations[0].start_line = 3;
        finding.acceptance_criteria.push(CriterionAssessment {
            criterion: "actual-criterion".into(),
            assessment: CriterionOutcome::Unmet,
            rationale: "".into(),
        });
        assert!(finding.validate().is_err());
    }
}
