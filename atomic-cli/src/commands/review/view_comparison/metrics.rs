use super::SideIndex;
use atomic_audit::{metrics::ExtractedMetric, Measurement, Metric};
use atomic_canonical::review_finding::AnalysisReport;
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn subject(subject: &str, actual: &str, logical: &str) -> String {
    match subject.strip_prefix(actual) {
        Some(tail) if tail.is_empty() || tail.starts_with('#') => format!("{logical}{tail}"),
        _ => subject.into(),
    }
}

fn inventory<'a>(
    view: &SideIndex<'a>,
    path: Option<&str>,
    logical: &str,
) -> BTreeMap<(Metric, String), &'a ExtractedMetric> {
    view.metrics(path)
        .iter()
        .copied()
        .map(|metric| {
            (
                (
                    metric.observation.metric,
                    subject(&metric.observation.subject, &metric.path, logical),
                ),
                metric,
            )
        })
        .collect()
}

fn missing(view: &SideIndex<'_>, path: Option<&str>, metric: Metric) -> Measurement {
    let Some(path) = path else {
        return Measurement::NotApplicable {
            reason: "File is absent in this revision".into(),
        };
    };
    let records: Vec<_> = view
        .metrics(Some(path))
        .iter()
        .filter(|record| record.observation.metric == metric)
        .collect();
    if records.is_empty()
        || view.view.analysis.metric_diagnostics.iter().any(|line| {
            !line.contains(super::super::analysis::SNAPSHOT_BASELINE)
                && (line.contains(path) || line.to_ascii_lowercase().contains("budget"))
        })
    {
        return Measurement::Unavailable { reason:"No reliable subject baseline was supplied for this metric; missing/unsupported/budget-limited data is not an absent subject".into() };
    }
    if let Some(reason) = records
        .iter()
        .find_map(|record| match &record.observation.after {
            Measurement::Unavailable { reason } => Some(reason.clone()),
            _ => None,
        })
    {
        return Measurement::Unavailable { reason };
    }
    Measurement::NotApplicable {
        reason: "Subject is not present in this revision".into(),
    }
}

pub(super) fn analysis(
    before: &SideIndex<'_>,
    after: &SideIndex<'_>,
    logical: &str,
    old: Option<&str>,
    new: Option<&str>,
    same_identity: bool,
) -> AnalysisReport {
    let before_metrics = inventory(before, old, logical);
    let after_metrics = inventory(after, new, logical);
    let keys: BTreeSet<_> = before_metrics
        .keys()
        .chain(after_metrics.keys())
        .cloned()
        .collect();
    let mut metrics = Vec::new();
    for key in keys {
        let first = before_metrics.get(&key).copied();
        let last = after_metrics.get(&key).copied();
        let mut metric = last.or(first).unwrap().clone();
        metric.path = logical.into();
        metric.observation.subject = key.1;
        metric.observation.before = first
            .map(|record| record.observation.after.clone())
            .unwrap_or_else(|| missing(before, old, key.0));
        metric.observation.after = last
            .map(|record| record.observation.after.clone())
            .unwrap_or_else(|| missing(after, new, key.0));
        metric.before_location = first.and_then(|record| record.after_location.clone());
        metric.after_location = last.and_then(|record| record.after_location.clone());
        if !same_identity
            && old.is_some()
            && new.is_some()
            && matches!(
                metric.scope,
                atomic_audit::catalog::MetricScope::Function
                    | atomic_audit::catalog::MetricScope::Class
            )
        {
            metric.observation.before = Measurement::Unavailable { reason: "Recorded inode was replaced; a same-named subject is not an established identity baseline".into() };
            metric.before_location = None;
        }
        metric.subject_identity_ambiguous = first
            .into_iter()
            .chain(last)
            .any(|record| record.subject_identity_ambiguous);
        if metric.subject_identity_ambiguous {
            metric.observation.before = Measurement::Unavailable { reason: "Anonymous/overloaded subject identity is ambiguous between independently extracted views; no ordinal-based delta inferred".into() };
            metric.before_location = None;
        }
        if first
            .zip(last)
            .is_some_and(|(a, b)| a.convention != b.convention)
        {
            metric.observation.before = Measurement::Unavailable {
                reason: "Measurement conventions differ between the supplied views".into(),
            };
        }
        metric.delta = metric.observation.delta();
        metric.detail=format!("{}; local measurements from two pinned recorded view corpora; renames use recorded file identity, not inferred similarity",metric.detail);
        metrics.push(metric);
    }
    let mut checks = Vec::new();
    for (side, view, path) in [("Before", before, old), ("After", after, new)] {
        for check in view.checks(path) {
            let mut check = (*check).clone();
            check.detail = format!("{side} view {}: {}", view.view.view, check.detail);
            checks.push(check);
        }
    }
    let excluded = before
        .view
        .analysis
        .excluded_metadata_paths
        .iter()
        .chain(&after.view.analysis.excluded_metadata_paths)
        .filter(|path| Some(path.as_str()) == old || Some(path.as_str()) == new)
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    AnalysisReport {
        schema: before.view.analysis.schema.clone(),
        analyzer: before.view.analysis.analyzer.clone(),
        analyzer_version: before.view.analysis.analyzer_version.clone(),
        checks,
        findings: vec![],
        metrics,
        metric_diagnostics: vec![],
        metric_comparison_scope: vec![logical.into()],
        excluded_metadata_paths: excluded,
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::incremental::ViewInspection;
    use super::*;

    fn snapshot(path: &str, source: &str) -> ViewInspection {
        let extracted = atomic_audit::extract::extract_changes(
            &[atomic_audit::extract::FileChange {
                path: path.into(),
                before: atomic_audit::extract::Source::Absent,
                after: atomic_audit::extract::Source::Text(source.into()),
            }],
            &mut atomic_semantic::ParserRegistry::new(),
            &Default::default(),
        )
        .unwrap();
        ViewInspection {
            conformance: None,
            source_ref: "urn:snapshot:test".into(),
            view: "test".into(),
            state: "test".into(),
            membership_hash: "test".into(),
            paths: vec![path.into()],
            content_hashes: BTreeMap::new(),
            analysis: AnalysisReport {
                schema: "test".into(),
                analyzer: "test".into(),
                analyzer_version: "test".into(),
                checks: vec![],
                findings: vec![],
                metrics: extracted.metrics,
                metric_diagnostics: extracted.diagnostics,
                metric_comparison_scope: extracted.comparison_paths,
                excluded_metadata_paths: vec![],
            },
        }
    }

    #[test]
    fn missing_data_and_anonymous_or_overloaded_subjects_never_make_numeric_baselines() {
        let mut before = snapshot("code.rs", "fn keep() {}\n");
        let after = snapshot("code.rs", "fn keep() {}\nfn added() { if true {} }\n");
        before
            .analysis
            .metric_diagnostics
            .push("code.rs (after): Subject inventory budget exceeded".into());
        let report = analysis(
            &SideIndex::new(&before),
            &SideIndex::new(&after),
            "code.rs",
            Some("code.rs"),
            Some("code.rs"),
            true,
        );
        for metric in report
            .metrics
            .iter()
            .filter(|m| m.observation.subject.contains("added"))
        {
            assert!(matches!(
                metric.observation.before,
                Measurement::Unavailable { .. }
            ));
            assert!(metric.delta.is_none());
        }
        let before = snapshot(
            "code.ts",
            "function f(a: string) { return a; }\nfunction f(a: number) { return a; }\n",
        );
        let report = analysis(
            &SideIndex::new(&before),
            &SideIndex::new(&before),
            "code.ts",
            Some("code.ts"),
            Some("code.ts"),
            true,
        );
        let ambiguous: Vec<_> = report
            .metrics
            .iter()
            .filter(|m| m.subject_identity_ambiguous)
            .collect();
        assert!(!ambiguous.is_empty());
        assert!(ambiguous.iter().all(|m| m.delta.is_none()
            && matches!(m.observation.before, Measurement::Unavailable { .. })));
    }

    #[test]
    fn replacement_does_not_infer_named_subject_continuity_from_the_path() {
        let before = snapshot("code.rs", "fn same_name() {}\n");
        let after = snapshot("code.rs", "fn same_name() { if true {} }\n");
        let report = analysis(
            &SideIndex::new(&before),
            &SideIndex::new(&after),
            "code.rs",
            Some("code.rs"),
            Some("code.rs"),
            false,
        );
        let metric = report
            .metrics
            .iter()
            .find(|m| m.observation.metric == Metric::CyclomaticComplexity)
            .unwrap();
        assert!(metric.delta.is_none());
        assert!(matches!(
            metric.observation.before,
            Measurement::Unavailable { .. }
        ));
        assert!(matches!(
            metric.observation.after,
            Measurement::Measured { value: 2 }
        ));
    }
}
