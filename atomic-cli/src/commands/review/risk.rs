//! Evidence-based risk presentation, separate from heuristic relationship counts.

use atomic_audit::impact::ImpactReport;
use atomic_audit::risk::{RiskLevel, RiskProfile};
use std::collections::BTreeSet;
use std::fmt::Write;

use atomic_canonical::review_finding::{Confidence, Evidence, EvidenceKind, Severity};
use atomic_canonical::review_graph::{
    PassStatus, ReviewGraph, ReviewPass, RiskAssessment, RiskCoverage, RiskRating, RiskSummary,
};

fn severity(severity: &Severity) -> u8 {
    match severity {
        Severity::Info => 0,
        Severity::Low => 1,
        Severity::Medium => 2,
        Severity::High | Severity::Critical => 3,
    }
}

fn rating(rating: RiskRating) -> u8 {
    match rating {
        RiskRating::High => 3,
        RiskRating::Medium => 2,
        RiskRating::Low => 1,
        RiskRating::Unknown | RiskRating::NotApplicable => 0,
    }
}

fn confidence(confidence: &Confidence) -> u8 {
    match confidence {
        Confidence::Low => 0,
        Confidence::Medium => 1,
        Confidence::High => 2,
    }
}

fn validate(assessment: &RiskAssessment, known_risk: u8) -> Result<(), &'static str> {
    if assessment.rationale.trim().is_empty()
        || assessment.evidence.is_empty()
        || assessment
            .evidence
            .iter()
            .any(|item| item.reference.trim().is_empty() || item.observation.trim().is_empty())
        || assessment
            .limitations
            .iter()
            .any(|item| item.trim().is_empty())
    {
        return Err(
            "risk assessment requires rationale, referenced evidence and nonempty limitation text",
        );
    }
    if assessment.rating == RiskRating::NotApplicable
        || assessment.coverage == RiskCoverage::NotApplicable
    {
        return Err("not-applicable risk is derived only from a metadata-only selection");
    }
    if assessment.rating == RiskRating::Low && assessment.coverage != RiskCoverage::Complete {
        return Err("low risk requires complete reviewer-declared coverage of the selected scope");
    }
    if assessment.coverage != RiskCoverage::Complete && assessment.limitations.is_empty() {
        return Err("incomplete risk assessment requires explicit coverage limitations");
    }
    if assessment.rating == RiskRating::Unknown && assessment.coverage == RiskCoverage::Complete {
        return Err("unknown risk cannot claim complete assessment coverage");
    }
    if assessment.rating != RiskRating::Unknown && assessment.coverage == RiskCoverage::Unavailable
    {
        return Err("unavailable risk assessment cannot supply a known rating");
    }
    if known_risk >= 2 && rating(assessment.rating) < known_risk {
        return Err("risk assessment cannot downgrade the severity of scoped risk findings");
    }
    Ok(())
}

fn signals(nodes: &[atomic_canonical::review_graph::GraphNode]) -> Vec<String> {
    let mut signals = Vec::new();
    for (metric, title) in [
        ("dependency_fan_out", "Declared dependency fan-out"),
        ("public_surface", "Declared public surface"),
    ] {
        let (mut increased, mut growth, mut added, mut added_values) = (0, 0u64, 0, 0u64);
        for node in nodes.iter().filter(|node| {
            node.kind == "metric" && node.properties["observation"]["metric"] == metric
        }) {
            let observation = &node.properties["observation"];
            let Some(after) = observation["after"]["value"]
                .as_u64()
                .filter(|_| observation["after"]["status"] == "measured")
            else {
                continue;
            };
            if let Some(before) = observation["before"]["value"]
                .as_u64()
                .filter(|_| observation["before"]["status"] == "measured")
            {
                if after > before {
                    increased += 1;
                    growth += after - before;
                }
            } else if observation["before"]["status"] == "not_applicable" && after > 0 {
                added += 1;
                added_values += after;
            }
        }
        if increased > 0 || added > 0 {
            signals.push(format!("{title}: {increased} increased file observation(s) (+{growth}); {added} added-file observation(s) ({added_values} declared). Not a severity score."));
        }
    }
    signals
}

pub(super) fn attach(
    graph: &mut ReviewGraph,
    submitted: Option<RiskAssessment>,
    impact: Vec<ImpactReport>,
    profile: &RiskProfile,
) -> Result<(), &'static str> {
    let pass = graph
        .passes
        .iter()
        .find(|pass| pass.pass == ReviewPass::BlastRadiusRisk)
        .expect("risk pass");
    let findings: Vec<_> = graph
        .findings
        .iter()
        .filter(|finding| finding.passes.contains(&ReviewPass::BlastRadiusRisk))
        .collect();
    let known_risk = findings
        .iter()
        .map(|finding| severity(&finding.finding.severity))
        .max()
        .unwrap_or(0);
    let model = if pass.status == PassStatus::NotApplicable {
        None
    } else {
        Some(atomic_audit::risk::assess(
            super::risk_facts::collect(&graph.nodes, &impact),
            profile,
        )?)
    };
    let (assessment, basis) = if pass.status == PassStatus::NotApplicable {
        if submitted.is_some() {
            return Err("metadata-only selection has no application-code risk assessment");
        }
        (
            RiskAssessment {
                rating: RiskRating::NotApplicable,
                confidence: Confidence::High,
                coverage: RiskCoverage::NotApplicable,
                rationale:
                    "No application code selected; metadata evidence checks remain separate.".into(),
                evidence: vec![],
                limitations: vec![],
            },
            "no_code",
        )
    } else if let Some(assessment) = submitted {
        validate(&assessment, known_risk)?;
        (assessment, "reviewer")
    } else {
        let strongest = findings
            .iter()
            .filter(|finding| severity(&finding.finding.severity) == known_risk)
            .min_by_key(|finding| confidence(&finding.finding.confidence));
        let estimate = model.as_ref().expect("code risk model");
        let estimated_rating = match estimate.rating {
            RiskLevel::High => RiskRating::High,
            RiskLevel::Medium => RiskRating::Medium,
            RiskLevel::Low => RiskRating::Low,
            RiskLevel::Unknown => RiskRating::Unknown,
        };
        let established = known_risk >= 2 && known_risk >= rating(estimated_rating);
        let confidence = if established {
            strongest.unwrap().finding.confidence.clone()
        } else if estimate.evidence_complete {
            Confidence::Medium
        } else {
            Confidence::Low
        };
        let evidence = if established {
            strongest.unwrap().finding.evidence.clone()
        } else {
            estimate
                .rules
                .iter()
                .take(3)
                .flat_map(|rule| {
                    rule.references.iter().map(|reference| Evidence {
                        kind: EvidenceKind::Reference,
                        reference: reference.clone(),
                        observation: format!("{}: {}", rule.id, rule.reason),
                    })
                })
                .take(8)
                .collect()
        };
        (RiskAssessment {
            rating: if established { if known_risk >= 3 { RiskRating::High } else { RiskRating::Medium } } else { estimated_rating },
            confidence,
            coverage: if pass.status == PassStatus::Blocked { RiskCoverage::Unavailable } else { RiskCoverage::Partial },
            rationale: if established { "Known issue severity sets the minimum risk; source analysis may leave additional exposure.".into() } else if estimate.rating == RiskLevel::Unknown { "Insufficient measured evidence to apply a risk rule or establish a localized change.".into() } else { format!("Rule-based change-review risk under {} / {}. This prioritizes inspection; it is not a defect probability or approval.", estimate.model, profile.name) },
            evidence,
            limitations: impact.iter().flat_map(|report| report.gaps.iter().cloned()).chain(estimate.facts.unknowns.iter().cloned()).chain(std::iter::once("Static review-risk estimate; runtime, security and executed-test coverage are not assessed.".into())).collect::<BTreeSet<_>>().into_iter().collect(),
        }, if established { "findings" } else { "rule_model" })
    };
    graph.risk = Some(RiskSummary {
        assessment, basis: basis.into(), findings: findings.iter().map(|finding| finding.number).collect(),
        unverified_call_candidates: graph.edges.iter().filter(|edge| edge.relation == "unverified_call_candidate").count(),
        unverified_import_candidates: graph.edges.iter().filter(|edge| edge.relation == "unverified_import_candidate").count(),
        relationship_coverage: "Recorded before/after statically bound free-function calls via supported lexical/import/module paths; unresolved receivers/dynamic bindings are excluded, not guessed. Not compiler/type or whole-program proof.".into(),
        signals: signals(&graph.nodes),
        impact,
        model,
    });
    graph.nodes.push(atomic_canonical::review_graph::GraphNode {
        id: "risk-summary".into(),
        kind: "risk_summary".into(),
        label: "Report-local risk assessment (not signed approval)".into(),
        properties: serde_json::to_value(graph.risk.as_ref().unwrap())
            .expect("risk summary serializes"),
    });
    graph.edges.push(atomic_canonical::review_graph::GraphEdge {
        from: ReviewPass::BlastRadiusRisk.id().into(),
        relation: "assesses_risk".into(),
        to: "risk-summary".into(),
    });
    if let Some(model) = &graph.risk.as_ref().unwrap().model {
        for rule in &model.rules {
            let id = format!("risk-rule:{}", rule.id);
            graph.nodes.push(atomic_canonical::review_graph::GraphNode {
                id: id.clone(),
                kind: "risk_rule".into(),
                label: rule.reason.clone(),
                properties: serde_json::to_value(rule).expect("risk rule serializes"),
            });
            graph.edges.push(atomic_canonical::review_graph::GraphEdge {
                from: "risk-summary".into(),
                relation: "triggered_by".into(),
                to: id,
            });
        }
    }
    for finding in findings {
        graph.edges.push(atomic_canonical::review_graph::GraphEdge {
            from: "risk-summary".into(),
            relation: "supported_by_finding".into(),
            to: finding.id.clone(),
        });
    }
    graph.nodes.sort_by(|left, right| left.id.cmp(&right.id));
    graph.edges.sort();
    Ok(())
}

pub(super) fn attach_snapshot(graph: &mut ReviewGraph) {
    graph.risk = Some(RiskSummary {
        assessment: RiskAssessment {
            rating: RiskRating::Unknown, confidence:Confidence::Low, coverage:RiskCoverage::Unavailable,
            rationale:"Whole-view snapshot audit has no change baseline; change-regression risk is not assessed. Current public-surface and dependency metrics remain available.".into(),
            evidence:vec![], limitations:vec!["No before/after change or call-contract comparison is implied by a view snapshot.".into()],
        },
        basis:"view_snapshot".into(),findings:vec![],unverified_call_candidates:0,unverified_import_candidates:0,
        relationship_coverage:"Snapshot audit/metric scope only; change-regression relationships were not evaluated.".into(),
        signals:vec![],impact:vec![],model:None,
    });
    graph.nodes.push(atomic_canonical::review_graph::GraphNode {
        id: "risk-summary".into(),
        kind: "risk_summary".into(),
        label: "Change risk not evaluated for view snapshot".into(),
        properties: serde_json::to_value(graph.risk.as_ref().unwrap())
            .expect("risk summary serializes"),
    });
    graph.edges.push(atomic_canonical::review_graph::GraphEdge {
        from: ReviewPass::BlastRadiusRisk.id().into(),
        relation: "assesses_risk".into(),
        to: "risk-summary".into(),
    });
}

pub(super) fn render(text: &mut String, stem: &str, summary: &RiskSummary) {
    let assessment = &summary.assessment;
    writeln!(
        text,
        "{stem}Risk: {:?} · Confidence: {:?} · Coverage: {:?} ({})",
        assessment.rating, assessment.confidence, assessment.coverage, summary.basis
    )
    .unwrap();
    writeln!(text, "{stem}{}", assessment.rationale).unwrap();
    if let Some(model) = &summary.model {
        for rule in model.rules.iter().take(3) {
            writeln!(
                text,
                "{stem}{:?} · {}: {}",
                rule.level, rule.id, rule.reason
            )
            .unwrap();
        }
        if model.rules.len() > 3 {
            writeln!(
                text,
                "{stem}{} further rule triggers retained in --json",
                model.rules.len() - 3
            )
            .unwrap();
        }
        let facts = &model.facts;
        writeln!(
            text,
            "{stem}Scope: {} production function(s), {} test/support function(s), {} component(s)",
            facts.production_functions,
            facts.test_support_functions,
            facts.components.len()
        )
        .unwrap();
        writeln!(text, "{stem}Direct impact: {} current bound caller(s) ({} production; {} test/support); {} observed production consumers of modified existing declarations", facts.observed_production_callers + facts.observed_test_support_callers, facts.observed_production_callers, facts.observed_test_support_callers, facts.existing_production_callers).unwrap();
        let checked = summary
            .impact
            .iter()
            .map(|report| report.checked_call_contracts)
            .sum::<usize>();
        let issues = summary
            .impact
            .iter()
            .map(|report| report.issues.len())
            .sum::<usize>();
        let preexisting = summary
            .impact
            .iter()
            .map(|report| report.preexisting_incompatible_contracts)
            .sum::<usize>();
        writeln!(text, "{stem}Source checks: {checked} call contracts · {issues} incompatible after-contract(s) · {preexisting} preexisting incompatibility(s)").unwrap();
        if summary.basis == "reviewer" {
            writeln!(
                text,
                "{stem}Automatic estimate retained: {:?} under {} / {}",
                model.rating, model.model, model.profile.name
            )
            .unwrap();
        }
        if assessment.coverage != RiskCoverage::Complete {
            let unresolved = facts
                .unresolved_references
                .map(|count| count.to_string())
                .unwrap_or_else(|| "unavailable".into());
            writeln!(text, "{stem}Coverage: {unresolved} unresolved reference expression(s), excluded from callers and severity; {} scoped limits retained in --json", assessment.limitations.len()).unwrap();
        }
    }
    if assessment.rating != RiskRating::NotApplicable {
        writeln!(
            text,
            "{stem}Risk findings: {}",
            if summary.findings.is_empty() {
                "none reported; estimated risk is not a confirmed defect".into()
            } else {
                summary
                    .findings
                    .iter()
                    .map(|number| format!("F{number}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        )
        .unwrap();
    }
    for evidence in assessment
        .evidence
        .iter()
        .filter(|_| summary.basis == "reviewer")
        .take(3)
    {
        writeln!(
            text,
            "{stem}Evidence: {} — {}",
            evidence.reference, evidence.observation
        )
        .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_canonical::review_finding::{Evidence, EvidenceKind};

    fn assessment() -> RiskAssessment {
        RiskAssessment {
            rating: RiskRating::Low,
            confidence: Confidence::Medium,
            coverage: RiskCoverage::Complete,
            rationale: "Checked the selected contracts and supported consumers.".into(),
            evidence: vec![Evidence {
                kind: EvidenceKind::Test,
                reference: "focused contract test".into(),
                observation:
                    "Selected before/after behavior and affected supported consumers checked."
                        .into(),
            }],
            limitations: vec![],
        }
    }

    #[test]
    fn low_requires_evidence_and_complete_scoped_coverage() {
        let original = assessment();
        assert!(validate(&original, 0).is_ok());
        for mutation in [
            "rationale",
            "evidence",
            "reference",
            "observation",
            "coverage",
            "limitation",
            "not_applicable",
        ] {
            let mut changed = original.clone();
            match mutation {
                "rationale" => changed.rationale.clear(),
                "evidence" => changed.evidence.clear(),
                "reference" => changed.evidence[0].reference.clear(),
                "observation" => changed.evidence[0].observation.clear(),
                "coverage" => {
                    changed.coverage = RiskCoverage::Partial;
                    changed.limitations.push("Consumers unresolved".into());
                }
                "limitation" => changed.limitations.push(" ".into()),
                _ => changed.rating = RiskRating::NotApplicable,
            }
            assert!(validate(&changed, 0).is_err(), "{mutation}");
        }
    }

    #[test]
    fn ratings_require_evidence_not_invented_defects_and_preserve_issue_floors() {
        let mut item = assessment();
        assert!(validate(&item, 3).is_err());
        item.rating = RiskRating::High;
        assert!(
            validate(&item, 0).is_ok(),
            "evidenced high review risk need not claim a defect"
        );
        assert!(validate(&item, 3).is_ok());
        item.rating = RiskRating::Medium;
        assert!(validate(&item, 3).is_err());
        assert!(validate(&item, 2).is_ok());
        item.coverage = RiskCoverage::Partial;
        assert!(validate(&item, 2).is_err());
        item.limitations
            .push("Dynamic consumers not resolved".into());
        assert!(validate(&item, 2).is_ok());
        item.rating = RiskRating::Unknown;
        assert!(validate(&item, 0).is_ok());
        assert!(validate(&item, 2).is_err());
        item.coverage = RiskCoverage::Complete;
        assert!(validate(&item, 0).is_err());
    }

    #[test]
    fn metric_signals_distinguish_growth_additions_and_unavailable_baselines() {
        use atomic_canonical::review_graph::GraphNode;
        use serde_json::json;
        let nodes = [
            json!({"metric": "public_surface", "before": {"status": "measured", "value": 42}, "after": {"status": "measured", "value": 56}}),
            json!({"metric": "public_surface", "before": {"status": "not_applicable"}, "after": {"status": "measured", "value": 24}}),
            json!({"metric": "public_surface", "before": {"status": "unavailable"}, "after": {"status": "measured", "value": 500}}),
            json!({"metric": "dependency_fan_out", "before": {"status": "measured", "value": 3}, "after": {"status": "measured", "value": 4}}),
            json!({"metric": "dependency_fan_out", "before": {"status": "measured", "value": 4}, "after": {"status": "measured", "value": 4}}),
        ].into_iter().map(|observation| GraphNode { id: "metric".into(), kind: "metric".into(), label: "".into(), properties: json!({"observation": observation}) }).collect::<Vec<_>>();
        let result = signals(&nodes);
        assert_eq!(result.len(), 2);
        assert!(result[0].contains("1 increased file observation(s) (+1)"));
        assert!(result[1].contains(
            "1 increased file observation(s) (+14); 1 added-file observation(s) (24 declared)"
        ));
        assert!(!result.iter().any(|signal| signal.contains("500")));
    }
}
