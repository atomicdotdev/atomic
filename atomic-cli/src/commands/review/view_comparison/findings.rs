use super::SideIndex;
use atomic_canonical::{
    review_finding::{CheckStatus, ReviewFinding, SourceSide},
    review_graph::FindingComparison,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

fn owner(view: &SideIndex<'_>, finding: &ReviewFinding, path: &str) -> String {
    let line = &finding.locations[0];
    view.metrics(Some(path))
        .iter()
        .filter(|metric| {
            !metric.subject_identity_ambiguous
                && metric.path == path
                && matches!(
                    metric.scope,
                    atomic_audit::catalog::MetricScope::Function
                        | atomic_audit::catalog::MetricScope::Class
                )
        })
        .filter_map(|metric| {
            metric
                .after_location
                .as_ref()
                .filter(|span| span.start_line <= line.start_line && span.end_line >= line.end_line)
                .map(|span| {
                    (
                        span.end_line - span.start_line,
                        metric.observation.subject.as_str(),
                    )
                })
        })
        .min_by_key(|(size, _)| *size)
        .map(|(_, subject)| super::metrics::subject(subject, path, "<file>"))
        .unwrap_or_else(|| "<file-level-or-unavailable-owner>".into())
}
fn fingerprint(finding: &ReviewFinding) -> String {
    finding
        .evidence
        .iter()
        .find(|item| item.reference.starts_with("urn:atomic:audit-observation:"))
        .map(|item| item.reference.clone())
        .unwrap_or_default()
}
fn checked(view: &SideIndex<'_>, path: Option<&str>, rule: &str) -> bool {
    path.is_none()
        || view.checks(path).iter().any(|check| {
            Some(check.path.as_str()) == path
                && check.rule == rule
                && check.status == CheckStatus::Completed
        })
}
fn side(
    finding: &ReviewFinding,
    side: SourceSide,
) -> Vec<atomic_canonical::review_finding::Location> {
    finding
        .locations
        .iter()
        .map(|location| {
            let mut location = location.clone();
            location.side = side.clone();
            location
        })
        .collect()
}

pub(super) fn compare(
    before: &SideIndex<'_>,
    after: &SideIndex<'_>,
    reference: &str,
    old: Option<&str>,
    new: Option<&str>,
    same_identity: bool,
) -> (Vec<ReviewFinding>, BTreeMap<String, FindingComparison>) {
    let first = before.findings(old);
    let last = after.findings(new);
    let key = |view: &SideIndex<'_>, finding: &ReviewFinding, path: &str| {
        (
            finding.requirement_or_rule.id.clone(),
            owner(view, finding, path),
        )
    };
    let mut used = BTreeSet::new();
    let first_keys: Vec<_> = first
        .iter()
        .map(|finding| key(before, finding, old.unwrap()))
        .collect();
    let last_keys: Vec<_> = last
        .iter()
        .map(|finding| key(after, finding, new.unwrap()))
        .collect();
    let exact_key = |group: &(String, String), finding: &ReviewFinding| {
        let fingerprint = if group.0 == "anti-slop.manageability"
            && group.1 != "<file-level-or-unavailable-owner>"
        {
            "qualified-manageability-owner".into()
        } else {
            fingerprint(finding)
        };
        (!fingerprint.is_empty()).then(|| (group.clone(), fingerprint))
    };
    // Reserve exact continuations before pairing edited constructs. Otherwise
    // an edited first occurrence could consume a later unchanged observation.
    let mut exact_matches = BTreeMap::new();
    let mut edited_candidates = BTreeMap::<_, VecDeque<usize>>::new();
    if same_identity {
        let mut candidates = BTreeMap::<_, VecDeque<usize>>::new();
        for (index, previous) in first.iter().enumerate() {
            if let Some(key) = exact_key(&first_keys[index], previous) {
                candidates.entry(key).or_default().push_back(index);
            }
        }
        for (current_index, current) in last.iter().enumerate() {
            if let Some(index) = exact_key(&last_keys[current_index], current)
                .and_then(|key| candidates.get_mut(&key))
                .and_then(VecDeque::pop_front)
            {
                used.insert(index);
                exact_matches.insert(current_index, index);
            }
        }
        for (index, group) in first_keys.into_iter().enumerate() {
            if !used.contains(&index) {
                edited_candidates.entry(group).or_default().push_back(index);
            }
        }
    }
    let mut findings = Vec::new();
    let mut states = BTreeMap::new();
    let mut emit = |first: Option<&ReviewFinding>,
                    last: Option<&ReviewFinding>,
                    status: &str,
                    reason: &str| {
        let mut finding = last.or(first).unwrap().clone();
        finding.change = reference.into();
        if last.is_none() {
            for location in &mut finding.locations {
                location.side = SourceSide::Before;
            }
        }
        finding.introduced_by_change.status =
            atomic_canonical::review_finding::IntroductionStatus::Unknown;
        finding.introduced_by_change.rationale="Local comparison of two recorded view states; this is not attribution to an authored change".into();
        let state = FindingComparison {
            status: status.into(),
            before: first
                .map(|f| side(f, SourceSide::Before))
                .unwrap_or_default(),
            after: last.map(|f| side(f, SourceSide::After)).unwrap_or_default(),
            rationale: reason.into(),
        };
        states.insert(super::super::graph::finding_id(&finding), state);
        findings.push(finding);
    };
    for (current_index, current) in last.iter().copied().enumerate() {
        let group = &last_keys[current_index];
        if let Some(index) = exact_matches.get(&current_index) {
            let previous = first[*index];
            emit(Some(previous),Some(current),"existing","The same local rule/AST observation remains in the same recorded file identity and qualified metric owner; line shifts and file renames are not new findings.");
            continue;
        }
        let edited = edited_candidates
            .get_mut(group)
            .and_then(VecDeque::pop_front);
        if let Some(index) = edited {
            let previous = first[index];
            used.insert(index);
            emit(Some(previous),Some(current),"changed","The same local rule/owner still matches different AST bytes. Equivalence, introduction and resolution are not guessed.");
        } else if checked(before, old, &group.0) {
            emit(None,Some(current),"new","Additional local rule observation relative to a successfully checked before side or a proven absent file; not a whole-program defect verdict.");
        } else {
            emit(None,Some(current),"unknown","Before-side rule coverage is unavailable/skipped; a missing observation is not proof of a new issue.");
        }
    }
    for (index, previous) in first.iter().enumerate() {
        if used.contains(&index) {
            continue;
        }
        if checked(after, new, &previous.requirement_or_rule.id) {
            emit(Some(previous),None,"resolved","The before-side local observation is absent in a successfully checked after side or a proven removed file; not proof of behavioral correctness.");
        } else {
            emit(Some(previous),None,"unknown","After-side rule coverage is unavailable/skipped; a missing observation is not a resolution.");
        }
    }
    (findings, states)
}
