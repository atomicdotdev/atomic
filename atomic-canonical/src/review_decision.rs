//! A user's attested evaluation of native review evidence, not a clean-code claim.
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{node::Proof, review_graph::ReviewPass};

pub const SCHEMA: &str = "atomic.review-decision.v1";
pub const EVALUATION_SCHEMA: &str = "atomic.review-evaluation.v1";
pub const MAX_BYTES: usize = 256 * 1024 * 1024;

fn digest(value: &impl Serialize) -> String {
    let value = serde_json::to_value(value).expect("review evidence serializes");
    data_encoding::BASE32_NOPAD
        .encode(blake3::hash(crate::jcs::canonicalize(&value).as_bytes()).as_bytes())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewPin {
    pub view: String,
    pub scope: String,
    pub state: String,
    pub membership_hash: String,
    pub members: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewScope {
    pub project: String,
    pub source: ViewPin,
    pub target: ViewPin,
    /// Sorted dependency-complete changes this insertion would add.
    pub changes: Vec<String>,
    pub intent_scope_hash: String,
    pub context_intent_hashes: BTreeMap<String, String>,
}

impl ReviewScope {
    pub fn hash(&self) -> String {
        digest(self)
    }

    pub fn validate_report(&self, report: &Value) -> Result<(), String> {
        if self.source.view.is_empty()
            || self.target.view.is_empty()
            || self.source.view == self.target.view
            || self.source.scope != "draft"
            || self.target.scope != "shared"
        {
            return Err(
                "Native publication requires a draft/source and a distinct shared/target".into(),
            );
        }
        for pin in [&self.source, &self.target] {
            if pin.membership_hash != digest(&pin.members)
                || pin.members.iter().collect::<BTreeSet<_>>().len() != pin.members.len()
            {
                return Err("Recorded membership identity is inconsistent".into());
            }
        }
        for (key, pin) in [
            ("from", &self.source),
            ("after", &self.source),
            ("to", &self.target),
            ("before", &self.target),
        ] {
            let actual = &report["view_comparison"][key];
            if actual["view"] != pin.view
                || actual["state"] != pin.state
                || actual["membership_hash"] != pin.membership_hash
            {
                return Err(
                    "Report source/target/before/after pins differ from the attested scope".into(),
                );
            }
        }
        let conformance = &report["view_comparison"]["conformance"];
        if report["graph"]["scope"]["project"] != self.project
            || report["graph"]["scope"]["view"] != self.source.view
            || report["graph"]["scope"]["view_state"] != self.source.state
            || conformance["scope"]["view"] != self.source.view
            || conformance["scope"]["current_inventory_hash"] != self.intent_scope_hash
        {
            return Err("Review project or intent-scope pins differ".into());
        }
        let contexts: BTreeMap<_, _> = conformance["criteria"]
            .as_array()
            .ok_or("Conformance criteria unavailable")?
            .iter()
            .filter_map(|criterion| {
                Some((
                    criterion["intent"].as_str()?.to_string(),
                    criterion["current_context"]["source_hash"]
                        .as_str()?
                        .to_string(),
                ))
            })
            .collect();
        if contexts != self.context_intent_hashes {
            return Err("Current intent context changed since native review".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewIssue {
    pub id: String,
    pub pass: ReviewPass,
    pub kind: String,
    pub reference: String,
    pub summary: String,
    pub waivable: bool,
    /// Complete evidence references/details, including grouped coverage gaps.
    pub evidence: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Ready,
    AcceptedDegradation,
    NotReady,
}

impl Disposition {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::AcceptedDegradation => "accepted_degradation",
            Self::NotReady => "not_ready",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptedDegradation {
    pub issue: String,
    pub rationale: String,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub follow_up: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PassEvaluation {
    pub pass: ReviewPass,
    pub evaluated: bool,
    pub disposition: Disposition,
    pub rationale: String,
    pub accepted_degradations: Vec<AcceptedDegradation>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewEvaluation {
    pub schema: String,
    pub scope_hash: String,
    pub report_hash: String,
    pub passes: Vec<PassEvaluation>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewDecision {
    pub schema: String,
    pub reviewed_at: String,
    pub scope: ReviewScope,
    pub report_hash: String,
    pub report: Value,
    pub issues: Vec<ReviewIssue>,
    /// Derived from evidence, never changed to ready by accepting degradation.
    pub readiness: Disposition,
    pub evaluation: ReviewEvaluation,
    pub independent_approval: bool,
    #[serde(rename = "attributedTo")]
    pub attributed_to: String,
    pub public_key: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supersedes: Vec<String>,
    #[serde(
        rename = "contentHash",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub content_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proof: Option<Proof>,
}

pub const PASSES: [ReviewPass; 3] = [
    ReviewPass::AntiSlop,
    ReviewPass::IntentConformance,
    ReviewPass::BlastRadiusRisk,
];

/// Remove physical-workspace display fields, not source, findings or evidence.
pub fn normalize_report(mut report: Value) -> Value {
    if let Some(source) = report["source"].as_object_mut() {
        for key in [
            "repository_root",
            "atomic_directory",
            "working_copy_view",
            "working_copy",
        ] {
            source.remove(key);
        }
    }
    for reference in report["dependency_references"]
        .as_array_mut()
        .into_iter()
        .flatten()
    {
        if let Some(reference) = reference.as_object_mut() {
            reference.remove("on_current_view");
        }
    }
    report
}

pub fn report_hash(report: &Value) -> String {
    digest(report)
}

fn pass(value: &Value) -> Result<ReviewPass, String> {
    serde_json::from_value(value.clone()).map_err(|_| "Unknown native review pass".into())
}

/// Extract all current findings and criterion gaps; group coverage without
/// dropping any of its evidence. The original report is retained alongside it.
pub fn issues(report: &Value) -> Result<Vec<ReviewIssue>, String> {
    if report["schema_version"] != 5 || report["selection"]["kind"] != "view_comparison" {
        return Err("Native publication requires a source-to-target v5 paired-view review".into());
    }
    let graph = &report["graph"];
    let findings = graph["findings"]
        .as_array()
        .ok_or("Review findings unavailable")?;
    let criteria = graph["criteria"]
        .as_array()
        .ok_or("Review criteria unavailable")?;
    let statuses = graph["passes"]
        .as_array()
        .ok_or("Review pass coverage unavailable")?;
    let mut coverage = BTreeMap::<ReviewPass, Vec<String>>::new();
    let mut seen = BTreeSet::new();
    for status in statuses {
        let own = pass(&status["pass"])?;
        if !seen.insert(own) {
            return Err("Duplicate native review pass".into());
        }
        if status["status"] != "not_applicable" && status["status"] != "complete" {
            coverage.entry(own).or_default().push(format!(
                "{}: {}",
                status["status"],
                status["detail"].as_str().unwrap_or("Coverage not supplied")
            ));
        }
    }
    if seen != PASSES.into_iter().collect() {
        return Err("All three native review passes are required".into());
    }
    let hash = report_hash(report);
    let mut result = Vec::new();
    let mut push =
        |own: ReviewPass, kind: &str, reference: String, summary: String, evidence: Vec<String>| {
            let id = format!(
                "urn:atomic:review-issue:{}",
                digest(&json!([hash, own, kind, reference, summary, evidence]))
            );
            result.push(ReviewIssue {
                id,
                pass: own,
                kind: kind.into(),
                reference,
                summary,
                waivable: kind != "publication_integrity",
                evidence,
            });
        };
    for finding in findings {
        if finding["comparison"]["status"] == "resolved" {
            continue;
        }
        let id = finding["id"].as_str().ok_or("Finding identity missing")?;
        for own in finding["passes"].as_array().ok_or("Finding pass missing")? {
            push(
                pass(own)?,
                "finding",
                id.into(),
                finding["finding"]["summary"]
                    .as_str()
                    .unwrap_or("Current review finding")
                    .into(),
                vec![
                    format!("F{}", finding["number"]),
                    serde_json::to_string(&finding["comparison"]).unwrap(),
                ],
            );
        }
    }
    for criterion in criteria {
        if criterion["review_assessment"] != "met" {
            push(
                ReviewPass::IntentConformance,
                "criterion",
                criterion["id"]
                    .as_str()
                    .ok_or("Criterion identity missing")?
                    .into(),
                format!(
                    "Acceptance assessment {}: {}",
                    criterion["review_assessment"],
                    criterion["text"].as_str().unwrap_or("criterion")
                ),
                vec![format!(
                    "Intent {} · source {} · author declared {}",
                    criterion["intent"], criterion["source_hash"], criterion["declared_status"]
                )],
            );
        }
    }
    let gate = &report["publication_gate"];
    if gate["allowed"] != true
        || gate["blocks"]
            .as_array()
            .is_none_or(|blocks| !blocks.is_empty())
    {
        push(
            ReviewPass::IntentConformance,
            "publication_integrity",
            "publication-gate".into(),
            "Original provenance/publication integrity checks do not permit insertion".into(),
            vec![serde_json::to_string(gate).unwrap()],
        );
    }
    for node in graph["nodes"]
        .as_array()
        .ok_or("Native review observations unavailable")?
    {
        let p = &node["properties"];
        if node["kind"] == "check" && p["status"] != "completed" {
            coverage
                .entry(ReviewPass::AntiSlop)
                .or_default()
                .push(format!(
                    "{} · {} · {} · {}",
                    p["path"], p["rule"], p["status"], p["detail"]
                ));
        }
        if node["kind"] == "metric" {
            let own = if matches!(
                p["observation"]["metric"].as_str(),
                Some("public_surface" | "dependency_fan_out")
            ) {
                ReviewPass::BlastRadiusRisk
            } else {
                ReviewPass::AntiSlop
            };
            for side in ["before", "after"] {
                if p["observation"][side]["status"] == "unavailable" {
                    coverage.entry(own).or_default().push(format!(
                        "{} · {} · {side}: {}",
                        node["id"], p["path"], p["observation"][side]["reason"]
                    ));
                }
            }
        }
    }
    let risk = &graph["risk"]["assessment"];
    if risk["rating"] != "low" && risk["rating"] != "not_applicable" {
        push(
            ReviewPass::BlastRadiusRisk,
            "risk",
            "risk-summary".into(),
            format!(
                "Review risk {} · coverage {}",
                risk["rating"], risk["coverage"]
            ),
            vec![risk["rationale"]
                .as_str()
                .unwrap_or("Risk assessment unavailable")
                .into()],
        );
    }
    for limitation in risk["limitations"].as_array().into_iter().flatten() {
        coverage
            .entry(ReviewPass::BlastRadiusRisk)
            .or_default()
            .push(
                limitation
                    .as_str()
                    .unwrap_or("Unspecified risk limitation")
                    .into(),
            );
    }
    for diagnostic in report["view_comparison"]["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
    {
        coverage.entry(ReviewPass::AntiSlop).or_default().push(
            diagnostic
                .as_str()
                .unwrap_or("Recorded corpus analysis unavailable")
                .into(),
        );
    }
    for diagnostic in report["view_comparison"]["conformance"]["diagnostics"]
        .as_array()
        .into_iter()
        .flatten()
    {
        coverage
            .entry(ReviewPass::IntentConformance)
            .or_default()
            .push(
                diagnostic
                    .as_str()
                    .unwrap_or("Conformance context unavailable")
                    .into(),
            );
    }
    for (own, details) in coverage {
        let details: Vec<_> = details
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if !details.is_empty() {
            push(
                own,
                "coverage",
                format!("{}:coverage", own.id()),
                format!(
                    "{} coverage has {} explicit limitation(s)",
                    own.label(),
                    details.len()
                ),
                details,
            );
        }
    }
    result.sort_by(|a, b| a.pass.cmp(&b.pass).then(a.id.cmp(&b.id)));
    Ok(result)
}

impl ReviewEvaluation {
    pub fn template(scope: &ReviewScope, report: &Value, issues: &[ReviewIssue]) -> Self {
        Self {
            schema: EVALUATION_SCHEMA.into(),
            scope_hash: scope.hash(),
            report_hash: report_hash(report),
            passes: PASSES
                .into_iter()
                .map(|own| {
                    let hard = issues
                        .iter()
                        .any(|issue| issue.pass == own && !issue.waivable);
                    let accepted_degradations: Vec<_> = issues
                        .iter()
                        .filter(|issue| issue.pass == own && !hard)
                        .map(|issue| AcceptedDegradation {
                            issue: issue.id.clone(),
                            rationale: String::new(),
                            evidence: vec![],
                            follow_up: None,
                        })
                        .collect();
                    PassEvaluation {
                        pass: own,
                        evaluated: false,
                        disposition: if hard {
                            Disposition::NotReady
                        } else if accepted_degradations.is_empty() {
                            Disposition::Ready
                        } else {
                            Disposition::AcceptedDegradation
                        },
                        rationale: String::new(),
                        accepted_degradations,
                    }
                })
                .collect(),
        }
    }

    pub fn validate(
        &self,
        scope: &ReviewScope,
        report: &Value,
        issues: &[ReviewIssue],
    ) -> Result<(), String> {
        if self.schema != EVALUATION_SCHEMA
            || self.scope_hash != scope.hash()
            || self.report_hash != report_hash(report)
        {
            return Err("Evaluation is stale or belongs to a different review scope/report".into());
        }
        let mut seen = BTreeSet::new();
        for evaluation in &self.passes {
            if !seen.insert(evaluation.pass)
                || !evaluation.evaluated
                || evaluation.rationale.trim().is_empty()
            {
                return Err("Explicit evaluation and rationale are required exactly once for each of the three review passes".into());
            }
            let expected: BTreeSet<_> = issues
                .iter()
                .filter(|issue| issue.pass == evaluation.pass)
                .map(|issue| issue.id.as_str())
                .collect();
            let mut accepted = BTreeSet::new();
            for entry in &evaluation.accepted_degradations {
                if entry.rationale.trim().is_empty()
                    || !expected.contains(entry.issue.as_str())
                    || issues
                        .iter()
                        .any(|issue| issue.id == entry.issue && !issue.waivable)
                    || !accepted.insert(entry.issue.as_str())
                {
                    return Err("Accepted degradation must name an exact waivable current issue once with a nonempty rationale; integrity failures are not waivable".into());
                }
            }
            match evaluation.disposition {
                Disposition::Ready if !expected.is_empty() || !accepted.is_empty()=>return Err("Outstanding native evidence cannot be labelled ready; explicitly accept degradation or retain not_ready".into()),
                Disposition::AcceptedDegradation if expected.is_empty() || accepted!=expected=>return Err("Every outstanding issue in an accepted-degradation pass must be explicitly accounted for".into()),
                Disposition::NotReady if !accepted.is_empty()=>return Err("A not-ready pass cannot also claim accepted degradation".into()),
                _=>{},
            }
        }
        if seen != PASSES.into_iter().collect() {
            return Err("All three passes must be explicitly evaluated".into());
        }
        Ok(())
    }

    pub fn insertion_disposition(&self) -> Disposition {
        if self
            .passes
            .iter()
            .any(|pass| pass.disposition == Disposition::NotReady)
        {
            Disposition::NotReady
        } else if self
            .passes
            .iter()
            .any(|pass| pass.disposition == Disposition::AcceptedDegradation)
        {
            Disposition::AcceptedDegradation
        } else {
            Disposition::Ready
        }
    }
}

impl ReviewDecision {
    pub fn artifact_hash(&self) -> String {
        digest(self)
    }
    pub fn value(&self) -> Value {
        serde_json::to_value(self).expect("review decision serializes")
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema != SCHEMA || self.independent_approval || self.reviewed_at.trim().is_empty()
        {
            return Err(
                "Invalid native review decision header or unsupported independent-approval claim"
                    .into(),
            );
        }
        if self.report_hash != report_hash(&self.report) {
            return Err("Attested review report digest mismatch".into());
        }
        let issues = issues(&self.report)?;
        if self.issues != issues {
            return Err("Attested issue set differs from the complete native report".into());
        }
        if self.readiness
            != if issues.is_empty() {
                Disposition::Ready
            } else {
                Disposition::NotReady
            }
        {
            return Err("Accepted degradation cannot rewrite evidence readiness".into());
        }
        self.scope.validate_report(&self.report)?;
        self.evaluation
            .validate(&self.scope, &self.report, &self.issues)
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_BYTES {
            return Err("Review decision exceeds the explicit 256 MiB artifact byte budget; no findings were truncated".into());
        }
        let value = crate::jcs::admit_document(bytes).map_err(|error| error.to_string())?;
        let decision: Self = serde_json::from_value(value).map_err(|error| error.to_string())?;
        decision.validate()?;
        Ok(decision)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence(count: usize) -> (ReviewScope, Value) {
        let base = digest(&"base");
        let change = digest(&"change");
        let target = ViewPin {
            view: "dev".into(),
            scope: "shared".into(),
            state: digest(&"before"),
            membership_hash: digest(&vec![base.clone()]),
            members: vec![base.clone()],
        };
        let source = ViewPin {
            view: "draft".into(),
            scope: "draft".into(),
            state: digest(&"after"),
            membership_hash: digest(&vec![base.clone(), change.clone()]),
            members: vec![base, change.clone()],
        };
        let scope = ReviewScope {
            project: "urn:atomic:project:fixture".into(),
            source,
            target,
            changes: vec![change],
            intent_scope_hash: digest(&"intents"),
            context_intent_hashes: BTreeMap::new(),
        };
        let pin = |pin: &ViewPin| json!({"view":pin.view,"state":pin.state,"membership_hash":pin.membership_hash});
        let findings:Vec<_>=(0..count).map(|index|json!({"number":index+1,"id":format!("finding:{index}"),"passes":["blast_radius_risk"],"finding":{"summary":format!("Consumer {index} remains incompatible")},"comparison":{"status":"new"}})).collect();
        let report = json!({"schema_version":5,"selection":{"kind":"view_comparison"},"source":{"repository_root":"/one","working_copy_view":"draft"},"dependency_references":[{"hash":"context","on_current_view":true}],"view_comparison":{"from":pin(&scope.source),"to":pin(&scope.target),"before":pin(&scope.target),"after":pin(&scope.source),"conformance":{"scope":{"view":"draft","current_inventory_hash":scope.intent_scope_hash},"criteria":[],"diagnostics":[]}},"graph":{"scope":{"project":scope.project,"view":"draft","view_state":scope.source.state},"findings":findings,"criteria":[{"id":"criterion:one","intent":"intent:one","text":"Tests demonstrate compatibility","review_assessment":"unknown","source_hash":"pinned","declared_status":"met"}],"nodes":[],"passes":PASSES.map(|pass|json!({"pass":pass,"status":"partial","detail":"Static source inspection, not runtime proof"})),"risk":{"assessment":{"rating":"high","coverage":"partial","rationale":"Known incompatible consumers","limitations":["Tests were not executed"]}}},"publication_gate":{"allowed":true,"blocks":[]}});
        (scope, normalize_report(report))
    }
    fn evaluated(scope: &ReviewScope, report: &Value, issues: &[ReviewIssue]) -> ReviewEvaluation {
        let mut value = ReviewEvaluation::template(scope, report, issues);
        for pass in &mut value.passes {
            pass.evaluated = true;
            pass.rationale =
                "Reviewed the complete scoped evidence and explicit limitations".into();
            for accepted in &mut pass.accepted_degradations {
                accepted.rationale="Known incompatibility accepted by the responsible signer for a bounded rollout; follow-up required".into();
            }
        }
        value
    }

    #[test]
    fn all_137_findings_are_linked_and_acceptance_does_not_rewrite_readiness() {
        let (scope, report) = evidence(137);
        let issues = issues(&report).unwrap();
        assert_eq!(
            issues
                .iter()
                .filter(|issue| issue.kind == "finding")
                .count(),
            137
        );
        let evaluation = evaluated(&scope, &report, &issues);
        evaluation.validate(&scope, &report, &issues).unwrap();
        assert_eq!(
            evaluation.insertion_disposition(),
            Disposition::AcceptedDegradation
        );
        let decision = ReviewDecision {
            schema: SCHEMA.into(),
            reviewed_at: "2026-11-25T00:00:00Z".into(),
            scope,
            report_hash: report_hash(&report),
            report,
            issues,
            readiness: Disposition::NotReady,
            evaluation,
            independent_approval: false,
            attributed_to: "accountable-signer".into(),
            public_key: "key".into(),
            supersedes: vec![],
            content_hash: None,
            proof: None,
        };
        decision.validate().unwrap();
        let mut false_ready = decision.clone();
        false_ready.readiness = Disposition::Ready;
        assert!(false_ready.validate().is_err());
        let mut false_approval = decision;
        false_approval.independent_approval = true;
        assert!(false_approval.validate().is_err());
    }

    #[test]
    fn opaque_partial_stale_duplicate_or_foreign_acceptance_is_refused() {
        let (scope, report) = evidence(2);
        let issues = issues(&report).unwrap();
        let good = evaluated(&scope, &report, &issues);
        let validate = |value: &ReviewEvaluation| value.validate(&scope, &report, &issues);
        assert!(validate(&ReviewEvaluation::template(&scope, &report, &issues)).is_err());
        let mut value = good.clone();
        value.passes.pop();
        assert!(validate(&value).is_err());
        let mut value = good.clone();
        value.passes[0].evaluated = false;
        assert!(validate(&value).is_err());
        let mut value = good.clone();
        value.passes[0].rationale.clear();
        assert!(validate(&value).is_err());
        let mut value = good.clone();
        value.passes[2].accepted_degradations.pop();
        assert!(validate(&value).is_err());
        let mut value = good.clone();
        value.passes[2].accepted_degradations[0].rationale.clear();
        assert!(validate(&value).is_err());
        let mut value = good.clone();
        value.passes[2].accepted_degradations[0].issue = "foreign finding".into();
        assert!(validate(&value).is_err());
        let mut value = good.clone();
        let duplicate = value.passes[2].accepted_degradations[0].clone();
        value.passes[2].accepted_degradations.push(duplicate);
        assert!(validate(&value).is_err());
        let mut value = good.clone();
        value.report_hash = "stale".into();
        assert!(validate(&value).is_err());
        let mut value = good;
        value.scope_hash = "other scope".into();
        assert!(validate(&value).is_err());
    }

    #[test]
    fn unresolved_evidence_cannot_be_called_ready_and_not_ready_does_not_authorize() {
        let (scope, report) = evidence(1);
        let issues = issues(&report).unwrap();
        let mut evaluation = evaluated(&scope, &report, &issues);
        evaluation.passes[2].disposition = Disposition::Ready;
        evaluation.passes[2].accepted_degradations.clear();
        assert!(evaluation.validate(&scope, &report, &issues).is_err());
        evaluation.passes[2].disposition = Disposition::NotReady;
        evaluation.validate(&scope, &report, &issues).unwrap();
        assert_eq!(evaluation.insertion_disposition(), Disposition::NotReady);
    }

    #[test]
    fn provenance_integrity_failure_is_never_waivable_code_degradation() {
        let (scope, mut report) = evidence(1);
        report["publication_gate"] =
            json!({"allowed":false,"blocks":["tampered original signature"]});
        let issues = issues(&report).unwrap();
        let mut evaluation = evaluated(&scope, &report, &issues);
        evaluation.validate(&scope, &report, &issues).unwrap();
        assert_eq!(evaluation.insertion_disposition(), Disposition::NotReady);
        let own = evaluation
            .passes
            .iter_mut()
            .find(|pass| pass.pass == ReviewPass::IntentConformance)
            .unwrap();
        own.disposition = Disposition::AcceptedDegradation;
        own.accepted_degradations = issues
            .iter()
            .filter(|issue| issue.pass == ReviewPass::IntentConformance)
            .map(|issue| AcceptedDegradation {
                issue: issue.id.clone(),
                rationale: "Attempted bypass".into(),
                evidence: vec![],
                follow_up: None,
            })
            .collect();
        assert!(evaluation.validate(&scope, &report, &issues).is_err());
    }

    #[test]
    fn corpus_budget_diagnostics_are_explicitly_evaluated_even_without_a_metric_node() {
        let (scope, mut report) = evidence(0);
        report["view_comparison"]["diagnostics"] =
            json!(["Before target: file omitted by semantic byte budget"]);
        let issues = issues(&report).unwrap();
        let issue = issues
            .iter()
            .find(|issue| issue.pass == ReviewPass::AntiSlop && issue.kind == "coverage")
            .unwrap();
        assert!(issue
            .evidence
            .iter()
            .any(|entry| entry.contains("file omitted by semantic byte budget")));
        let mut evaluation = evaluated(&scope, &report, &issues);
        evaluation.passes[0].accepted_degradations.clear();
        assert!(evaluation.validate(&scope, &report, &issues).is_err());
    }

    #[test]
    fn scope_roles_are_explicit_and_environment_does_not_change_report_identity() {
        let (scope, report) = evidence(1);
        scope.validate_report(&report).unwrap();
        let mut wrong = report.clone();
        wrong["view_comparison"]["before"] = wrong["view_comparison"]["from"].clone();
        assert!(scope.validate_report(&wrong).is_err());
        let mut physical = report.clone();
        physical["source"] = json!({"repository_root":"/other/machine","atomic_directory":"/other/.atomic","working_copy_view":"dev","working_copy":{"id":"foreign physical directory"}});
        physical["dependency_references"][0]["on_current_view"] = json!(false);
        assert_eq!(normalize_report(physical), report);
    }
}
