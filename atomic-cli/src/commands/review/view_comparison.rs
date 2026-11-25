//! Two pinned recorded view states, compared locally without switching either.

mod conformance;
pub(super) use conformance::{collect_view, Conformance};
mod findings;
mod impact;
mod metrics;

pub(super) use impact::{changed_paths, inspect as inspect_impact};

use super::incremental::{IncrementalArgs, IncrementalReport, Selection, ViewInspection};
use crate::error::{CliError, CliResult};
use atomic_canonical::{review_finding::AnalysisReport, review_graph::FindingComparison};
use atomic_core::types::{Base32, Hash};
use atomic_repository::Repository;
use serde::Serialize;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

struct SideIndex<'a> {
    view: &'a ViewInspection,
    metrics: BTreeMap<&'a str, Vec<&'a atomic_audit::metrics::ExtractedMetric>>,
    checks: BTreeMap<&'a str, Vec<&'a atomic_canonical::review_finding::Check>>,
    findings: BTreeMap<&'a str, Vec<&'a atomic_canonical::review_finding::ReviewFinding>>,
}

impl<'a> SideIndex<'a> {
    fn new(view: &'a ViewInspection) -> Self {
        let mut index = Self {
            view,
            metrics: BTreeMap::new(),
            checks: BTreeMap::new(),
            findings: BTreeMap::new(),
        };
        for metric in &view.analysis.metrics {
            index.metrics.entry(&metric.path).or_default().push(metric);
        }
        for check in &view.analysis.checks {
            index.checks.entry(&check.path).or_default().push(check);
        }
        for finding in &view.analysis.findings {
            for path in finding
                .locations
                .iter()
                .map(|location| location.path.as_str())
                .collect::<BTreeSet<_>>()
            {
                index.findings.entry(path).or_default().push(finding);
            }
        }
        index
    }
    fn metrics(&self, path: Option<&str>) -> &[&'a atomic_audit::metrics::ExtractedMetric] {
        path.and_then(|path| self.metrics.get(path))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
    fn checks(&self, path: Option<&str>) -> &[&'a atomic_canonical::review_finding::Check] {
        path.and_then(|path| self.checks.get(path))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
    fn findings(
        &self,
        path: Option<&str>,
    ) -> &[&'a atomic_canonical::review_finding::ReviewFinding] {
        path.and_then(|path| self.findings.get(path))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

fn invalid(message: impl Into<String>) -> CliError {
    CliError::InvalidArgument {
        message: message.into(),
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ViewPin {
    pub view: String,
    pub state: String,
    pub membership_hash: String,
    pub source_ref: String,
}
impl From<&ViewInspection> for ViewPin {
    fn from(view: &ViewInspection) -> Self {
        Self {
            view: view.view.clone(),
            state: view.state.clone(),
            membership_hash: view.membership_hash.clone(),
            source_ref: view.source_ref.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ComparedFile {
    pub source_ref: String,
    pub path: String,
    pub before_path: Option<String>,
    pub after_path: Option<String>,
    pub before_inode: Option<String>,
    pub after_inode: Option<String>,
    pub before_hash: Option<String>,
    pub after_hash: Option<String>,
    pub status: String,
    pub content_status: String,
    pub analysis: AnalysisReport,
    /// Keys are report-local finding IDs, not persistent KG assertions.
    pub finding_states: BTreeMap<String, FindingComparison>,
}

#[derive(Debug, Serialize)]
pub struct ViewComparison {
    pub source_ref: String,
    /// Proposed source and insertion destination (CLI direction).
    pub from: ViewPin,
    pub to: ViewPin,
    /// Destination baseline and proposed source state (measurement direction).
    pub before: ViewPin,
    pub after: ViewPin,
    pub files: Vec<ComparedFile>,
    pub diagnostics: Vec<String>,
    pub conformance: conformance::Conformance,
}
impl ViewComparison {
    pub(super) fn file(&self, reference: &str) -> Option<&ComparedFile> {
        self.files.iter().find(|file| file.source_ref == reference)
    }
}

pub(super) fn resolve(repo: &Repository, args: &IncrementalArgs) -> CliResult<IncrementalReport> {
    let from = args
        .from
        .as_deref()
        .ok_or_else(|| invalid("--from requires --to and vice versa"))?;
    let to = args
        .to
        .as_deref()
        .ok_or_else(|| invalid("--from requires --to and vice versa"))?;
    if [from, to]
        .iter()
        .any(|name| name.is_empty() || name.trim() != *name)
    {
        return Err(invalid("Comparison view names must be exact and nonempty"));
    }
    if args.view.is_some()
        || args.change.is_some()
        || args.turn.is_some()
        || args.intent.is_some()
        || args.findings.is_some()
        || args.risk_profile.is_some()
    {
        return Err(invalid("--from/--to cannot be combined with snapshot/change/turn/intent selectors, supplied change findings or a change-risk profile"));
    }
    let mut before = super::view_snapshot::collect(repo, to)?;
    let mut after = super::view_snapshot::collect(repo, from)?;
    let before_view = before.view_snapshot.take().unwrap();
    let after_view = after.view_snapshot.take().unwrap();
    let from_pin = ViewPin::from(&after_view);
    let to_pin = ViewPin::from(&before_view);
    let source_ref = format!(
        "urn:atomic:view-comparison:{}",
        Hash::of(
            &serde_json::to_vec(&json!({"schema":"source-target-v1","from":from_pin,"to":to_pin}))
                .map_err(|error| CliError::Internal(error.into()))?
        )
        .to_base32()
    );
    let before_ids = identities(repo, &before_view)?;
    let after_ids = identities(repo, &after_view)?;
    let pairs = pair_paths(&before_ids, &after_ids);
    let before_index = SideIndex::new(&before_view);
    let after_index = SideIndex::new(&after_view);
    let mut files = Vec::new();
    for (old, new) in pairs {
        let old_id = old.as_ref().and_then(|path| before_ids.get(path)).cloned();
        let new_id = new.as_ref().and_then(|path| after_ids.get(path)).cloned();
        let old_hash = old
            .as_ref()
            .and_then(|path| before_view.content_hashes.get(path))
            .cloned();
        let new_hash = new
            .as_ref()
            .and_then(|path| after_view.content_hashes.get(path))
            .cloned();
        let content_status = match (&old_hash, &new_hash) {
            (Some(a), Some(b)) => {
                if a == b {
                    "unchanged"
                } else {
                    "modified"
                }
            }
            _ => "unavailable",
        }
        .to_string();
        let status = match (&old, &new) {
            (None, Some(_)) => "added",
            (Some(_), None) => "removed",
            (Some(a), Some(b)) if a != b => "renamed",
            _ if old_id != new_id => "replaced",
            _ => content_status.as_str(),
        }
        .to_string();
        let path = new.as_ref().or(old.as_ref()).unwrap().clone();
        let file_ref = format!(
            "{source_ref}:file:{}",
            Hash::of(
                &serde_json::to_vec(&json!([old, new, old_id, new_id]))
                    .map_err(|error| CliError::Internal(error.into()))?
            )
            .to_base32()
        );
        let mut analysis = metrics::analysis(
            &before_index,
            &after_index,
            &path,
            old.as_deref(),
            new.as_deref(),
            old_id == new_id,
        );
        let (local_findings, states) = findings::compare(
            &before_index,
            &after_index,
            &file_ref,
            old.as_deref(),
            new.as_deref(),
            old_id == new_id,
        );
        analysis.findings = local_findings;
        files.push(ComparedFile {
            source_ref: file_ref,
            path,
            before_path: old,
            after_path: new,
            before_inode: old_id,
            after_inode: new_id,
            before_hash: old_hash,
            after_hash: new_hash,
            status,
            content_status,
            analysis,
            finding_states: states,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path).then(a.source_ref.cmp(&b.source_ref)));
    let diagnostics = before_view
        .analysis
        .metric_diagnostics
        .iter()
        .filter(|line| !line.contains(super::analysis::SNAPSHOT_BASELINE))
        .map(|line| format!("Before (target) {to}: {line}"))
        .chain(
            after_view
                .analysis
                .metric_diagnostics
                .iter()
                .filter(|line| !line.contains(super::analysis::SNAPSHOT_BASELINE))
                .map(|line| format!("After (source) {from}: {line}")),
        )
        .collect();
    let mut comparison = ViewComparison {
        source_ref,
        before: to_pin.clone(),
        after: from_pin.clone(),
        from: from_pin,
        to: to_pin,
        files,
        diagnostics,
        conformance: Default::default(),
    };
    comparison.conformance = conformance::collect(
        repo,
        &comparison,
        &before.dependency_closure,
        &after.dependency_closure,
    )?;
    verify(repo, &comparison)?;
    after.schema_version = 5;
    after.purpose = "local_view_comparison_review";
    after.selection=Selection { kind:"view_comparison".into(),value:Some(format!("{from} -> {to}")),reason:"From is the proposed source view; To is the insertion destination. Measurements compare target-before with source-after, using both complete recorded corpora.".into() };
    after.source["from_view"] = json!(from);
    after.source["from_state"] = json!(comparison.from.state);
    after.source["to_view"] = json!(to);
    after.source["to_state"] = json!(comparison.to.state);
    after.source["before_view"] = json!(to);
    after.source["after_view"] = json!(from);
    let mut refs = BTreeMap::new();
    for reference in before
        .dependency_references
        .into_iter()
        .chain(after.dependency_references)
    {
        refs.insert(reference.hash.clone(), reference);
    }
    after.dependency_references = refs.into_values().collect();
    let members: BTreeSet<_> = before
        .dependency_closure
        .into_iter()
        .chain(after.dependency_closure)
        .collect();
    after.dependency_closure = members.into_iter().collect();
    after.dependency_only = after.dependency_closure.clone();
    after.warnings=vec![
        format!("Local review source {from} -> target {to}; measurements BEFORE {to} -> AFTER {from}. No view switch, working-copy bytes, model or network calls."),
        "Audit lifecycles compare local rule/AST observations within recorded file identities and qualified metric owners. Changed/unknown observations are not fabricated resolutions or proof of behavioral equivalence.".into(),
        "Each side uses the same explicit semantic/metric bounds as --view. Unsupported, unavailable and over-budget data stay unknown, not absent or zero.".into(),
        "Metric comparisons use the complete supplied corpus on each side. Renames are based on canonical recorded inode identity, never filename/content similarity.".into(),
        "Conformance includes actual membership-delta governance and recorded criteria where available; current vault fallback is labelled context, never historical approval. Blast radius uses the shared named-view semantic selector, direct-call contracts and local risk model; compiler/type/runtime behavior is not proven.".into(),
    ];
    after.view_comparison = Some(comparison);
    after.graph = Some(super::graph::build(repo, &after, None, None)?);
    super::view_snapshot::verify(repo, &after)?;
    Ok(after)
}

fn identities(repo: &Repository, view: &ViewInspection) -> CliResult<BTreeMap<String, String>> {
    let ids = repo
        .recorded_file_inodes_on_view(&view.paths, &view.view)
        .map_err(CliError::Repository)?;
    view.paths
        .iter()
        .zip(ids)
        .map(|(path, inode)| {
            Ok((
                path.clone(),
                inode
                    .ok_or_else(|| {
                        invalid("Visible recorded file lost its inode identity during comparison")
                    })?
                    .get()
                    .to_string(),
            ))
        })
        .collect()
}

fn pair_paths(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
) -> Vec<(Option<String>, Option<String>)> {
    let mut old: BTreeSet<_> = before.keys().cloned().collect();
    let mut new: BTreeSet<_> = after.keys().cloned().collect();
    let mut pairs = Vec::new();
    for path in before
        .keys()
        .filter(|path| after.get(*path) == before.get(*path))
    {
        old.remove(path);
        new.remove(path);
        pairs.push((Some(path.clone()), Some(path.clone())));
    }
    let mut old_ids = BTreeMap::<&str, Vec<String>>::new();
    let mut new_ids = BTreeMap::<&str, Vec<String>>::new();
    for path in &old {
        old_ids.entry(&before[path]).or_default().push(path.clone());
    }
    for path in &new {
        new_ids.entry(&after[path]).or_default().push(path.clone());
    }
    for (inode, paths) in old_ids {
        if paths.len() == 1 {
            if let Some(targets) = new_ids.get(inode).filter(|paths| paths.len() == 1) {
                let a = paths[0].clone();
                let b = targets[0].clone();
                old.remove(&a);
                new.remove(&b);
                pairs.push((Some(a), Some(b)));
            }
        }
    }
    let replacements = old
        .iter()
        .filter(|path| new.contains(*path))
        .cloned()
        .collect::<Vec<_>>();
    for path in replacements {
        old.remove(&path);
        new.remove(&path);
        pairs.push((Some(path.clone()), Some(path)));
    }
    pairs.extend(old.into_iter().map(|path| (Some(path), None)));
    pairs.extend(new.into_iter().map(|path| (None, Some(path))));
    pairs
}

pub(super) fn verify(repo: &Repository, comparison: &ViewComparison) -> CliResult<()> {
    comparison.conformance.verify(repo)?;
    for pin in [&comparison.from, &comparison.to] {
        if repo
            .get_view_info(&pin.view)
            .map_err(CliError::Repository)?
            .state
            .to_base32()
            != pin.state
            || super::graph::membership_hash(repo, &pin.view)? != pin.membership_hash
        {
            return Err(invalid(format!(
                "Recorded comparison view '{}' changed during audit/export; retry both views",
                pin.view
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::change::ChangeHeader;
    use atomic_repository::{record::RecordOptions, tracking::TrackingOptions};

    fn map(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(path, inode)| (path.to_string(), inode.to_string()))
            .collect()
    }
    #[test]
    fn pair_canonical_identities_before_path_replacements_or_similarity() {
        for (old, new, expected) in [
            (
                map(&[("a", "1"), ("b", "2")]),
                map(&[("a", "2"), ("b", "1")]),
                vec![(Some("a"), Some("b")), (Some("b"), Some("a"))],
            ),
            (
                map(&[("a", "1"), ("b", "2")]),
                map(&[("b", "1")]),
                vec![(Some("a"), Some("b")), (Some("b"), None)],
            ),
            (
                map(&[("a", "1")]),
                map(&[("a", "3")]),
                vec![(Some("a"), Some("a"))],
            ),
            (
                map(&[("a", "1")]),
                map(&[("similar-a", "3")]),
                vec![(Some("a"), None), (None, Some("similar-a"))],
            ),
            // An ambiguous alias is not a guessed rename.
            (
                map(&[("a", "1"), ("alias", "1")]),
                map(&[("b", "1")]),
                vec![(Some("a"), None), (Some("alias"), None), (None, Some("b"))],
            ),
        ] {
            let actual: BTreeSet<_> = pair_paths(&old, &new).into_iter().collect();
            let expected: BTreeSet<_> = expected
                .into_iter()
                .map(|(a, b)| (a.map(str::to_owned), b.map(str::to_owned)))
                .collect();
            assert_eq!(actual, expected);
        }
    }
    #[test]
    fn comparison_checks_both_effective_parent_memberships_without_switching() {
        let directory = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(directory.path()).unwrap();
        let wc = repo.require_working_copy_id().unwrap();
        let parent = repo.current_view().to_owned();
        std::fs::write(
            directory.path().join("file.rs"),
            "fn before() { todo!(); }\n",
        )
        .unwrap();
        repo.add(wc, "file.rs", TrackingOptions::default()).unwrap();
        let record = |repo: &Repository| {
            repo.record(
                wc,
                ChangeHeader::new("pin fixture"),
                RecordOptions::new()
                    .with_all(true)
                    .save_to_store(true)
                    .apply_after_record(true)
                    .enrich_kg(false),
            )
            .unwrap()
        };
        record(&repo);
        repo.create_draft_view("left", &parent).unwrap();
        repo.create_draft_view("right", &parent).unwrap();
        let report = resolve(
            &repo,
            &IncrementalArgs {
                from: Some("left".into()),
                to: Some("right".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let comparison = report.view_comparison.as_ref().unwrap();
        verify(&repo, comparison).unwrap();
        std::fs::write(directory.path().join("file.rs"), "fn after() {}\n").unwrap();
        record(&repo);
        assert_eq!(
            repo.get_view_info("left").unwrap().state.to_base32(),
            comparison.from.state
        );
        assert_eq!(
            repo.get_view_info("right").unwrap().state.to_base32(),
            comparison.to.state
        );
        assert!(verify(&repo, comparison).is_err());
        assert!(super::super::html::render(&repo, &report).is_err());
        assert_eq!(repo.current_view(), parent);
    }
}
