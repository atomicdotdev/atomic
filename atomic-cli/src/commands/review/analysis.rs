//! Adapt recorded source inspection to Atomic's analyzer-neutral contract.

use std::collections::BTreeMap;

use atomic_audit::extract::{self, ExtractionConfig, FileChange, Source};
use atomic_audit::structural::{self as review, Rule, StructuralInspection};
use atomic_canonical::review_finding::{
    AnalysisReport, Check, CheckStatus, Confidence, Evidence, EvidenceKind, Introduction,
    IntroductionStatus, Location, ReviewFinding, RuleReference, Severity, SourceSide, SCHEMA,
};
use atomic_core::{
    change::Change,
    crdt::TrunkOp,
    types::{Base32, Hash},
};
use atomic_repository::Repository;
use atomic_semantic::{Language, ParserRegistry};

const ANALYZER: &str = "atomic-audit";

fn empty_report() -> AnalysisReport {
    AnalysisReport {
        schema: SCHEMA.into(),
        analyzer: ANALYZER.into(),
        analyzer_version: format!(
            "{};rules={}",
            atomic_audit::VERSION,
            review::RULESET_VERSION
        ),
        checks: vec![],
        findings: vec![],
        metrics: vec![],
        metric_diagnostics: vec![],
        metric_comparison_scope: vec![],
        excluded_metadata_paths: vec![],
    }
}

pub(super) fn analyze_change(
    repo: &Repository,
    change: &Change,
    hash: &Hash,
    paths: &[String],
    selected: bool,
    on_current_view: bool,
    registry: &mut ParserRegistry,
) -> AnalysisReport {
    let mut report = empty_report();
    let change_id = hash.to_base32();
    let mut metric_changes = Vec::new();
    let mut metric_bytes = ExtractionConfig::default().max_total_bytes;
    for path in paths {
        if atomic_repository::is_vault_path(path) {
            if selected {
                report.excluded_metadata_paths.push(path.clone());
            }
            continue;
        }
        let skip = if !selected {
            Some("Dependency context only; not a selected review root")
        } else if !on_current_view {
            Some("Off current view; exact before/after content not inspected")
        } else if Language::detect(path).is_none() {
            Some("Unsupported language; no structural or anti-slop rules executed")
        } else {
            None
        };
        if let Some(reason) = skip {
            checks(&mut report, &change_id, path, CheckStatus::Skipped, reason);
            if selected {
                metric_changes.push(FileChange {
                    path: path.clone(),
                    before: Source::Unavailable(reason.into()),
                    after: Source::Unavailable(reason.into()),
                });
            }
            continue;
        }
        match repo.get_file_content_after_change(path, hash) {
            Ok(Some(after)) => {
                let before = repo.get_file_content_before_change(path, hash);
                let added = change.file_ops().iter().any(|ops| {
                    ops.path() == path && matches!(ops.trunk_op(), Some(TrunkOp::Create { .. }))
                });
                inspect_content(
                    &mut report,
                    registry,
                    &change_id,
                    path,
                    before
                        .as_ref()
                        .map(|value| value.as_deref())
                        .map_err(|error| error.to_string()),
                    &after,
                    added,
                );
                metric_changes.push(FileChange {
                    path: path.clone(),
                    before: metric_source(
                        before.map_err(|error| error.to_string()),
                        &mut metric_bytes,
                    ),
                    after: metric_source(Ok(Some(after)), &mut metric_bytes),
                });
            }
            Ok(None) => {
                checks(
                    &mut report,
                    &change_id,
                    path,
                    CheckStatus::Skipped,
                    "No recorded after-content; structural after-side checks skipped",
                );
                metric_changes.push(FileChange {
                    path: path.clone(),
                    before: metric_source(
                        repo.get_file_content_before_change(path, hash)
                            .map_err(|error| error.to_string()),
                        &mut metric_bytes,
                    ),
                    after: Source::Absent,
                });
            }
            Err(error) => {
                let reason = format!("Recorded after-content unavailable: {error}");
                checks(&mut report, &change_id, path, CheckStatus::Error, &reason);
                metric_changes.push(FileChange {
                    path: path.clone(),
                    before: Source::Unavailable(reason.clone()),
                    after: Source::Unavailable(reason),
                });
            }
        }
    }
    if selected {
        match extract::extract_changes(&metric_changes, registry, &ExtractionConfig::default()) {
            Ok(metrics) => {
                report.metrics = metrics.metrics;
                report.metric_diagnostics = metrics.diagnostics;
                report.metric_comparison_scope = metrics.comparison_paths;
            }
            Err(reason) => report.metric_diagnostics.push(reason.into()),
        }
    }
    report
}

pub(super) const SNAPSHOT_BASELINE: &str =
    "View snapshot has no before revision; no change introduction or delta is inferred";

/// Inspect each supplied live path once, using the exact named-view renderer.
pub(super) fn analyze_snapshot(
    repo: &Repository,
    view: &str,
    reference: &str,
    paths: &[String],
) -> Result<(AnalysisReport, BTreeMap<String, String>), atomic_repository::RepositoryError> {
    let mut report = empty_report();
    let mut registry = ParserRegistry::new();
    let mut files = Vec::new();
    let mut eligible = Vec::new();
    // Whole-view mode must not inherit the per-change 128-file/8-MiB limits.
    // Cross-file clone work and all other limits remain explicit and bounded.
    let config = ExtractionConfig {
        max_files: paths.len(),
        max_total_bytes: 128 * 1024 * 1024,
        max_total_tokens: 2_000_000,
        max_total_subjects: 65_536,
        max_clone_pairs: 2_000_000,
        ..Default::default()
    };
    let mut remaining = config.max_total_bytes;
    let mut content_hashes = BTreeMap::new();
    for path in paths {
        if atomic_repository::is_vault_path(path) {
            report.excluded_metadata_paths.push(path.clone());
        } else if Language::detect(path).is_none() {
            let reason = "Unsupported language; no structural or anti-slop rules executed";
            checks(&mut report, reference, path, CheckStatus::Skipped, reason);
            files.push(FileChange {
                path: path.clone(),
                before: Source::Unavailable(SNAPSHOT_BASELINE.into()),
                after: Source::Unavailable(reason.into()),
            });
        } else {
            eligible.push(path.clone());
        }
    }
    repo.for_each_file_content_and_kind_on_view(&eligible, view, |path, bytes, kind| {
        let bytes = bytes.ok_or_else(|| atomic_repository::RepositoryError::Output(format!("Recorded view source disappeared while inspecting '{path}'")))?;
        let reason = if kind != Some(atomic_core::change::InodeKind::Regular) {
            Some("Recorded symlink/non-regular source is not followed or audited as code")
        } else if bytes.len() > atomic_semantic::MAX_SOURCE_BYTES || bytes.len() > remaining {
            Some("View snapshot source budget exceeded (1 MiB per file, 128 MiB total); source not silently truncated")
        } else { None };
        let after = if let Some(reason) = reason {
            checks(&mut report, reference, path, CheckStatus::Skipped, reason);
            Source::Unavailable(reason.into())
        } else {
            remaining -= bytes.len();
            content_hashes.insert(path.to_string(), Hash::of(&bytes).to_base32());
            match parse(&mut registry, path, &bytes) {
                Ok(parsed) => inspect_parsed(&mut report, reference, path, parsed, Err(SNAPSHOT_BASELINE.into()), true),
                Err(reason) => checks(&mut report, reference, path, CheckStatus::Skipped, &reason),
            }
            Source::from_bytes(bytes)
        };
        // Treat every current token as in scope for the whole-view clone scan.
        // Restore the unavailable historical baseline below; no change delta is
        // inferred from this extraction-only absent side.
        files.push(FileChange { path:path.into(), before:Source::Absent, after });
        Ok(())
    })?;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    match extract::extract_changes(&files, &mut registry, &config) {
        Ok(metrics) => {
            report.metrics = metrics.metrics;
            for metric in &mut report.metrics {
                metric.observation.before = atomic_audit::Measurement::Unavailable {
                    reason: SNAPSHOT_BASELINE.into(),
                };
                metric.before_location = None;
                metric.delta = None;
            }
            report.metric_diagnostics = metrics.diagnostics;
            report.metric_comparison_scope = metrics.comparison_paths;
        }
        Err(reason) => report.metric_diagnostics.push(reason.into()),
    }
    Ok((report, content_hashes))
}

fn metric_source(bytes: Result<Option<Vec<u8>>, String>, remaining: &mut usize) -> Source {
    match bytes {
        // The caller has already verified selected change membership in the
        // source view. A successful None is recorded absence, not a failed read.
        Ok(None) => Source::Absent,
        Ok(Some(bytes))
            if bytes.len() <= atomic_semantic::MAX_SOURCE_BYTES && bytes.len() <= *remaining =>
        {
            *remaining -= bytes.len();
            Source::from_bytes(bytes)
        }
        Ok(Some(_)) => Source::Unavailable("Recorded metric source byte budget exceeded".into()),
        Err(reason) => Source::Unavailable(reason),
    }
}

fn checks(
    report: &mut AnalysisReport,
    change: &str,
    path: &str,
    status: CheckStatus,
    detail: &str,
) {
    for rule in Rule::ALL {
        report.checks.push(Check {
            change: change.into(),
            path: path.into(),
            rule: rule.id().into(),
            status: status.clone(),
            detail: detail.into(),
        });
    }
}

fn parse(
    registry: &mut ParserRegistry,
    path: &str,
    bytes: &[u8],
) -> Result<StructuralInspection, String> {
    if bytes.contains(&0) {
        return Err("Binary content is not inspected".into());
    }
    let source = std::str::from_utf8(bytes).map_err(|_| "Non-UTF-8 content is not inspected")?;
    review::inspect(registry, path, source).map_err(str::to_owned)
}

fn inspect_content(
    report: &mut AnalysisReport,
    registry: &mut ParserRegistry,
    change: &str,
    path: &str,
    before: Result<Option<&[u8]>, String>,
    after: &[u8],
    added: bool,
) {
    let after = match parse(registry, path, after) {
        Ok(after) => after,
        Err(reason) => {
            checks(report, change, path, CheckStatus::Skipped, &reason);
            return;
        }
    };
    let baseline = match before {
        Ok(Some(bytes)) => parse(registry, path, bytes),
        Ok(None) if added => Ok(StructuralInspection {
            issues: Vec::new(),
            has_syntax_errors: false,
            ambiguous_manageability_subjects: Vec::new(),
        }),
        Ok(None) => Err("No before-content and no explicit file-creation operation".into()),
        Err(reason) => Err(format!("Recorded before-content unavailable: {reason}")),
    };
    inspect_parsed(report, change, path, after, baseline, false);
}

fn inspect_parsed(
    report: &mut AnalysisReport,
    change: &str,
    path: &str,
    after: StructuralInspection,
    baseline: Result<StructuralInspection, String>,
    snapshot: bool,
) {
    if !snapshot {
        report.checks.push(Check {
            change: change.into(),
            path: path.into(),
            rule: "recorded-baseline".into(),
            status: if baseline.is_ok() {
                CheckStatus::Completed
            } else {
                CheckStatus::Error
            },
            detail: baseline
                .as_ref()
                .map(|_| "Recorded baseline inspected".into())
                .unwrap_or_else(Clone::clone),
        });
    }
    let language = Language::detect(path).expect("successful parse has a detected language");
    for rule in Rule::ALL {
        let (status, detail) = if !rule.supports(language) {
            (
                CheckStatus::Skipped,
                "Rule not implemented for this language",
            )
        } else if rule != Rule::Syntax && after.has_syntax_errors {
            (
                CheckStatus::Skipped,
                "Syntax recovery prevents reliable anti-slop inspection",
            )
        } else if rule == Rule::Manageability && !after.ambiguous_manageability_subjects.is_empty()
        {
            (CheckStatus::Skipped, "Ambiguous named-function identities excluded from joint manageability; unique subjects were checked")
        } else {
            (CheckStatus::Completed, rule.coverage())
        };
        report.checks.push(Check {
            change: change.into(),
            path: path.into(),
            rule: rule.id().into(),
            status,
            detail: detail.into(),
        });
    }
    // Compare exact node text as a multiset, not line numbers. A line-only move
    // is pre-existing; an extra identical occurrence is a new candidate. This
    // is syntactic attribution, not a proof of behavioral regression.
    let mut counts = BTreeMap::new();
    if let Ok(before) = &baseline {
        for issue in &before.issues {
            *counts
                .entry((issue.rule, issue.source.as_str()))
                .or_insert(0usize) += 1;
        }
    }
    for issue in &after.issues {
        // Manageability is joint source/metric evidence, not a threshold on an
        // isolated measurement. Existing equal/improving burden is not a new
        // manageability finding just because unrelated tokens changed.
        let prior_burden = issue.manageability.as_ref().and_then(|current| {
            baseline.as_ref().ok().and_then(|before| {
                before
                    .issues
                    .iter()
                    .filter_map(|issue| issue.manageability.as_ref())
                    .find(|before| before.subject == current.subject)
            })
        });
        if let (Some(current), Some(before)) = (&issue.manageability, prior_burden) {
            if current.complexity.saturating_sub(before.complexity) < 3
                && current.nesting <= before.nesting
            {
                continue;
            }
        }
        let (status, rationale) = match &baseline {
            Err(reason) => (IntroductionStatus::Unknown, reason.clone()),
            Ok(before) if before.has_syntax_errors && issue.rule != Rule::Syntax => (
                IntroductionStatus::Unknown,
                "Before-content has syntax errors; anti-slop baseline not evaluated".into(),
            ),
            Ok(before) if before.has_syntax_errors && issue.rule == Rule::Syntax => {
                if !issue.source.is_empty()
                    && before
                        .issues
                        .iter()
                        .any(|old| old.rule == issue.rule && old.source == issue.source)
                {
                    (
                        IntroductionStatus::No,
                        "The same observed syntax error existed in recorded before-content".into(),
                    )
                } else {
                    (IntroductionStatus::Unknown, "Both revisions have syntax errors but the observed error changed; introduction cannot be established".into())
                }
            }
            Ok(before) if issue.manageability.as_ref().is_some_and(|current| before.ambiguous_manageability_subjects.contains(&current.subject)) => (IntroductionStatus::Unknown, "Recorded before-side function identity is ambiguous; joint burden introduction cannot be established".into()),
            Ok(_) if issue.manageability.is_some() => (
                IntroductionStatus::Yes,
                if let Some(before) = prior_burden {
                format!("Recorded before-side joint burden for {}: {} code lines, complexity {}, nesting {}; the after-side adds at least 3 decision points or a nesting level while still meeting the joint review policy", before.subject,before.code_lines,before.complexity,before.nesting)
                } else {
                    "Joint long-body/control-flow review trigger was absent in the successfully inspected recorded before-side".into()
                },
            ),
            Ok(_) => {
                let count = counts
                    .entry((issue.rule, issue.source.as_str()))
                    .or_default();
                if *count > 0 {
                    *count -= 1;
                    (IntroductionStatus::No, "Matching rule and exact AST node text existed before this change (line shifts ignored)".into())
                } else if baseline.as_ref().is_ok_and(|before| {
                    before.issues.iter().any(|old| old.rule == issue.rule)
                        && !before
                            .issues
                            .iter()
                            .any(|old| old.rule == issue.rule && old.source == issue.source)
                }) {
                    (IntroductionStatus::Unknown, "The same rule already matched different node text before this change; cannot distinguish a new issue from an edited existing match".into())
                } else {
                    (IntroductionStatus::Yes, "Additional AST rule match relative to recorded before-content; behavioral impact still requires review".into())
                }
            }
        };
        let severity = match issue.rule {
            Rule::UnnecessaryMachinery | Rule::Manageability | Rule::DuplicatedBehavior => {
                Severity::Low
            }
            _ => Severity::Medium,
        };
        let summary = issue.rule.summary();
        let explanation = atomic_audit::AUDIT_CARDS
            .iter()
            .find(|card| card.id == issue.rule.id())
            .expect("every implemented rule has a card")
            .question;
        let correction = issue.rule.correction();
        let confidence = Confidence::Medium;
        let reference = format!("{change}:{path}:{}-{}", issue.start_line, issue.end_line);
        let mut finding = ReviewFinding {
            analyzer: ANALYZER.into(),
            change: change.into(),
            summary: summary.into(),
            locations: vec![Location {
                path: path.into(),
                side: SourceSide::After,
                start_line: issue.start_line,
                end_line: issue.end_line,
            }],
            requirement_or_rule: RuleReference {
                id: issue.rule.id().into(),
                explanation: explanation.into(),
            },
            evidence: vec![
                Evidence {
                    kind: EvidenceKind::Source,
                    reference: reference.clone(),
                    observation: format!(
                        "AST node excerpt for {}: {}",
                        issue.rule.id(),
                        issue.source.chars().take(1024).collect::<String>()
                    ),
                },
                Evidence {
                    kind: EvidenceKind::Source,
                    reference: reference.clone(),
                    observation: issue.detail.clone(),
                },
                Evidence {
                    kind: EvidenceKind::Diff,
                    reference,
                    observation: rationale.clone(),
                },
            ],
            introduced_by_change: Introduction { status, rationale },
            severity,
            confidence,
            proposed_correction: correction.into(),
            acceptance_criteria: Vec::new(),
        };
        if snapshot {
            finding
                .evidence
                .retain(|evidence| evidence.kind != EvidenceKind::Diff);
            finding.evidence.push(Evidence {
                kind: EvidenceKind::Reference,
                reference: format!("urn:atomic:audit-observation:{}",Hash::of(issue.source.as_bytes()).to_base32()),
                observation: "Fingerprint of the exact locally inspected AST node bytes, before display clipping; not an authored change identity.".into(),
            });
        }
        // This is a presence gate, not an automated acceptance verdict.
        if let Err(error) = finding.validate() {
            report.checks.push(Check {
                change: change.into(),
                path: path.into(),
                rule: issue.rule.id().into(),
                status: CheckStatus::Error,
                detail: error.into(),
            });
        } else {
            report.findings.push(finding);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn analyze(before: Result<Option<&[u8]>, String>, after: &[u8], added: bool) -> AnalysisReport {
        let mut report = AnalysisReport {
            schema: SCHEMA.into(),
            analyzer: ANALYZER.into(),
            analyzer_version: "test".into(),
            checks: vec![],
            findings: vec![],
            metrics: vec![],
            metric_diagnostics: vec![],
            metric_comparison_scope: vec![],
            excluded_metadata_paths: vec![],
        };
        inspect_content(
            &mut report,
            &mut ParserRegistry::new(),
            "recorded-hash",
            "lib.rs",
            before,
            after,
            added,
        );
        report
    }

    #[test]
    fn manageability_reports_new_or_worsened_joint_burden_not_unchanged_counts() {
        let function = |lines: usize, branches: usize| {
            let mut source = String::from("fn large(x:i32) {\n");
            for i in 0..branches {
                source.push_str(&format!("if x == {i} {{ work(); }}\n"));
            }
            for _ in 0..lines {
                source.push_str("work();\n");
            }
            source.push_str("}\n");
            source
        };
        let before = function(100, 15);
        let same_burden = before.replace("work()", "other()");
        let unchanged = analyze(Ok(Some(before.as_bytes())), same_burden.as_bytes(), false);
        assert!(!unchanged
            .findings
            .iter()
            .any(|finding| finding.requirement_or_rule.id == "anti-slop.manageability"));
        for after in [function(101, 15), function(120, 15), function(100, 16)] {
            let report = analyze(Ok(Some(before.as_bytes())), after.as_bytes(), false);
            assert!(
                !report
                    .findings
                    .iter()
                    .any(|finding| finding.requirement_or_rule.id == "anti-slop.manageability"),
                "trivial or straight-line growth must not become a finding"
            );
        }
        let after = function(120, 18);
        let increased = analyze(Ok(Some(before.as_bytes())), after.as_bytes(), false);
        let finding = increased
            .findings
            .iter()
            .find(|finding| finding.requirement_or_rule.id == "anti-slop.manageability")
            .unwrap();
        assert_eq!(finding.introduced_by_change.status, IntroductionStatus::Yes);
        assert!(finding
            .evidence
            .iter()
            .any(|evidence| evidence.observation.contains("atomic.manageability.v1")));
        assert!(finding
            .introduced_by_change
            .rationale
            .contains("before-side joint burden"));
        let unavailable = analyze(Err("baseline unavailable".into()), after.as_bytes(), false);
        assert_eq!(
            unavailable
                .findings
                .iter()
                .find(|finding| finding.requirement_or_rule.id == "anti-slop.manageability")
                .unwrap()
                .introduced_by_change
                .status,
            IntroductionStatus::Unknown
        );
        let ambiguous_before = format!("#[cfg(unix)]\n{before}\n#[cfg(windows)]\n{before}");
        let ambiguous = analyze(
            Ok(Some(ambiguous_before.as_bytes())),
            after.as_bytes(),
            false,
        );
        assert_eq!(
            ambiguous
                .findings
                .iter()
                .find(|finding| finding.requirement_or_rule.id == "anti-slop.manageability")
                .unwrap()
                .introduced_by_change
                .status,
            IntroductionStatus::Unknown
        );
    }

    #[test]
    fn introduced_and_preexisting_matches_use_recorded_baseline_not_line_numbers() {
        let report = analyze(
            Ok(Some(b"fn f() { todo!(); }")),
            b"\nfn f() { todo!(); todo!(); }",
            false,
        );
        assert_eq!(report.findings.len(), 2);
        assert_eq!(
            report.findings[0].introduced_by_change.status,
            IntroductionStatus::No
        );
        assert_eq!(
            report.findings[1].introduced_by_change.status,
            IntroductionStatus::Yes
        );
        assert!(report
            .findings
            .iter()
            .all(|finding| finding.validate().is_ok()));
    }

    #[test]
    fn unknown_baseline_is_not_an_introduced_issue_or_a_clean_result() {
        for before in [
            Ok(None),
            Err("unavailable".into()),
            Ok(Some(b"\xff".as_slice())),
        ] {
            let report = analyze(before, b"fn f() { todo!(); }", false);
            assert_eq!(
                report.findings[0].introduced_by_change.status,
                IntroductionStatus::Unknown
            );
            assert!(report
                .checks
                .iter()
                .any(|check| check.status == CheckStatus::Error));
        }
        let added = analyze(Ok(None), b"fn f() { todo!(); }", true);
        assert_eq!(
            added.findings[0].introduced_by_change.status,
            IntroductionStatus::Yes
        );
    }

    #[test]
    fn editing_an_existing_placeholder_does_not_prove_it_was_introduced() {
        let report = analyze(
            Ok(Some(b"fn f() { todo!(); }")),
            b"fn f() { todo!(\"reason\"); }",
            false,
        );
        assert_eq!(
            report.findings[0].introduced_by_change.status,
            IntroductionStatus::Unknown
        );
    }

    #[test]
    fn clean_and_unsupported_checks_are_distinct_and_binary_is_not_clean() {
        let clean = analyze(Ok(None), b"fn f() {}", true);
        assert!(clean.findings.is_empty());
        assert!(clean.checks.iter().any(
            |check| check.rule == "structural.syntax" && check.status == CheckStatus::Completed
        ));
        assert!(clean
            .checks
            .iter()
            .any(|check| check.rule == "anti-slop.empty-catch"
                && check.status == CheckStatus::Skipped));
        let binary = analyze(Ok(None), b"\x00\xff", true);
        assert!(binary.findings.is_empty());
        assert!(binary
            .checks
            .iter()
            .all(|check| check.status == CheckStatus::Skipped));
    }

    #[test]
    fn syntax_recovery_does_not_claim_anti_slop_checks_passed() {
        let broken = analyze(Ok(None), b"fn f( { todo!();", true);
        assert!(broken
            .findings
            .iter()
            .all(|finding| finding.requirement_or_rule.id == "structural.syntax"));
        assert!(broken
            .checks
            .iter()
            .any(|check| check.rule == "anti-slop.rust-placeholder"
                && check.status == CheckStatus::Skipped));
        let repaired = analyze(Ok(Some(b"fn f( { todo!();")), b"fn f() { todo!(); }", false);
        assert_eq!(
            repaired.findings[0].introduced_by_change.status,
            IntroductionStatus::Unknown
        );
        let changed_error = analyze(Ok(Some(b"fn old( {")), b"fn different[ {", false);
        assert_eq!(
            changed_error.findings[0].introduced_by_change.status,
            IntroductionStatus::Unknown
        );
    }
}
