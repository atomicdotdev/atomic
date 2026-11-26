//! Entire recorded view inspection. No history replay or working-copy switch.

use std::collections::BTreeSet;

use atomic_core::{
    pristine::{GraphTxnT, ViewTxnT},
    types::{Base32, Hash},
};
use atomic_repository::{graph_visibility_closure, Repository};
use serde_json::json;

use super::incremental::{
    DependencyReference, IncrementalArgs, IncrementalReport, Selection, ViewInspection,
};
use crate::error::{CliError, CliResult};

fn invalid(message: impl Into<String>) -> CliError {
    CliError::InvalidArgument {
        message: message.into(),
    }
}

pub(super) fn resolve(repo: &Repository, args: &IncrementalArgs) -> CliResult<IncrementalReport> {
    let name = args
        .view
        .as_deref()
        .ok_or_else(|| invalid("Missing --view name"))?;
    if name.is_empty() || name.trim() != name {
        return Err(invalid("--view requires an exact nonempty view name"));
    }
    if args.change.is_some()
        || args.turn.is_some()
        || args.intent.is_some()
        || args.findings.is_some()
        || args.risk_profile.is_some()
        || args.from.is_some()
        || args.to.is_some()
    {
        return Err(invalid("--view is a snapshot selector; it cannot be combined with change/turn/intent selectors, supplied change findings or a change-risk profile"));
    }
    let mut report = collect(repo, name)?;
    let snapshot = report.view_snapshot.as_mut().expect("resolved snapshot");
    snapshot.conformance = Some(super::view_comparison::collect_view(repo, snapshot)?);
    report.graph = Some(super::graph::build(repo, &report, None, None)?);
    verify(repo, &report)?;
    Ok(report)
}

pub(super) fn collect(repo: &Repository, name: &str) -> CliResult<IncrementalReport> {
    let info = repo.get_view_info(name).map_err(CliError::Repository)?;
    let membership_hash = super::graph::membership_hash(repo, name)?;
    let mut paths: Vec<_> = repo
        .recorded_file_identities_on_view(name)
        .map_err(CliError::Repository)?
        .into_keys()
        .collect();
    paths.sort();
    let descriptor = json!({"project":repo.project_id().map_err(CliError::Repository)?.to_urn(),"view":name,"state":info.state.to_base32(),"membership_hash":membership_hash});
    let source_ref = format!(
        "urn:atomic:view-snapshot:{}",
        Hash::of(
            &serde_json::to_vec(&descriptor).map_err(|error| CliError::Internal(error.into()))?
        )
        .to_base32()
    );
    let current: BTreeSet<_> = repo
        .effective_history(None)
        .map_err(CliError::Repository)?
        .iter()
        .map(|entry| entry.hash.to_base32())
        .collect();
    let members = {
        let txn = repo
            .pristine()
            .read_txn()
            .map_err(|error| CliError::Internal(error.into()))?;
        let view = txn
            .get_view(name)
            .map_err(|error| CliError::Internal(error.into()))?
            .ok_or_else(|| invalid("View disappeared during snapshot resolution"))?;
        let closure = graph_visibility_closure(&txn, &view).map_err(CliError::Repository)?;
        closure
            .iter_dependency_first()
            .filter(|id| !id.is_root())
            .map(|id| {
                txn.get_external(*id)
                    .map_err(|error| CliError::Internal(error.into()))?
                    .ok_or_else(|| invalid("View membership has no external change hash"))
            })
            .collect::<CliResult<Vec<_>>>()?
    };
    let mut references = Vec::new();
    for hash in &members {
        let change = repo.load_change(hash).map_err(CliError::Repository)?;
        let mut dependencies: Vec<_> = change
            .hashed
            .dependencies
            .iter()
            .map(Hash::to_base32)
            .collect();
        dependencies.sort();
        dependencies.dedup();
        references.push(DependencyReference {
            hash: hash.to_base32(),
            direct: false,
            on_current_view: current.contains(&hash.to_base32()),
            summary: change
                .hashed
                .header
                .message
                .lines()
                .next()
                .unwrap_or_default()
                .chars()
                .take(240)
                .collect(),
            dependencies,
        });
    }
    let (analysis, content_hashes) =
        super::analysis::analyze_snapshot(repo, name, &source_ref, &paths)
            .map_err(CliError::Repository)?;
    let report = IncrementalReport {
        schema_version:3, purpose:"local_view_snapshot_review", independent_approval:false,
        selection:Selection { kind:"view".into(),value:Some(name.into()),reason:"Entire recorded view snapshot, once per live file; history is references only".into() },
        source:json!({"repository_root":repo.root(),"atomic_directory":repo.dot_dir(),"project_identity":repo.project_identity().map_err(CliError::Repository)?,"view":name,"view_state":info.state.to_base32(),"view_scope":info.kind_label(),"parent_view":info.parent_name,"view_change_count":info.change_count,"working_copy_view":repo.current_view(),"working_copy_status":"not_scanned"}),
        selected_roots:vec![],dependency_closure:members.iter().map(Hash::to_base32).collect(),dependency_only:members.iter().map(Hash::to_base32).collect(),dependency_references:references,
        changes:vec![],view_snapshot:Some(ViewInspection { source_ref, view:name.into(),state:info.state.to_base32(),membership_hash,paths,analysis,content_hashes,conformance:None }),view_comparison:None,
        turns:vec![],pending_work_outside_selection:vec![],intents:vec![],warnings:vec![
            "Local-only recorded view snapshot: no working-copy bytes, model calls or view switch.".into(),
            "Snapshot findings describe current source, not introduction by an authored change. There is no before revision or metric delta.".into(),
            "Snapshot conformance includes explicit view-assigned intents and declared criteria, not inferred change governance or acceptance approval. Change-regression risk still has no before baseline.".into(),
            "Metrics cover the full supplied live-file set subject to explicit limits: 1 MiB per source, 128 MiB total, 2M tokens, 65536 subjects and 2M clone comparisons. Omissions remain unavailable/skipped.".into(),
        ],graph:None,
    };
    verify(repo, &report)?;
    Ok(report)
}

pub(super) fn verify(repo: &Repository, report: &IncrementalReport) -> CliResult<()> {
    if let Some(comparison) = &report.view_comparison {
        super::view_comparison::verify(repo, comparison)?;
    }
    if let Some(snapshot) = &report.view_snapshot {
        if let Some(conformance) = &snapshot.conformance {
            conformance.verify(repo)?;
        }
        if repo
            .get_view_info(&snapshot.view)
            .map_err(CliError::Repository)?
            .state
            .to_base32()
            != snapshot.state
            || super::graph::membership_hash(repo, &snapshot.view)? != snapshot.membership_hash
        {
            return Err(invalid(
                "Recorded view snapshot changed during audit/export; retry the selected view",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::change::ChangeHeader;
    use atomic_repository::{record::RecordOptions, tracking::TrackingOptions};

    fn record(repo: &Repository) {
        repo.record(
            repo.require_working_copy_id().unwrap(),
            ChangeHeader::new("snapshot fixture"),
            RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true)
                .enrich_kg(false),
        )
        .unwrap();
    }

    #[test]
    fn inherited_parent_changes_invalidate_snapshot_even_when_child_state_is_unchanged() {
        let directory = tempfile::tempdir().unwrap();
        let mut repo = Repository::init(directory.path()).unwrap();
        std::fs::write(
            directory.path().join("source.rs"),
            "fn old() { todo!(); }\n",
        )
        .unwrap();
        repo.add(
            repo.require_working_copy_id().unwrap(),
            "source.rs",
            TrackingOptions::default(),
        )
        .unwrap();
        record(&repo);
        let parent = repo.current_view().to_owned();
        repo.create_draft_view("child", &parent).unwrap();
        let args = IncrementalArgs {
            view: Some("child".into()),
            ..Default::default()
        };
        let first = resolve(&repo, &args).unwrap();
        let child_state = first.view_snapshot.as_ref().unwrap().state.clone();
        std::fs::write(directory.path().join("source.rs"), "fn current() {}\n").unwrap();
        record(&repo);
        assert_eq!(
            repo.get_view_info("child").unwrap().state.to_base32(),
            child_state
        );
        assert!(verify(&repo, &first).is_err());
        let second = resolve(&repo, &args).unwrap();
        assert_ne!(
            first.view_snapshot.unwrap().source_ref,
            second.view_snapshot.unwrap().source_ref
        );
        assert_eq!(repo.current_view(), parent);
    }

    #[test]
    fn whole_view_metrics_do_not_stop_at_the_change_file_budget() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repository::init(directory.path()).unwrap();
        repo.init_vault().unwrap();
        for index in 0..134 {
            let path = format!("file_{index:03}.rs");
            std::fs::write(
                directory.path().join(&path),
                format!("fn function_{index}() {{}}\n"),
            )
            .unwrap();
            repo.add(
                repo.require_working_copy_id().unwrap(),
                &path,
                TrackingOptions::default(),
            )
            .unwrap();
        }
        record(&repo);
        let report = resolve(
            &repo,
            &IncrementalArgs {
                view: Some(repo.current_view().into()),
                ..Default::default()
            },
        )
        .unwrap();
        let snapshot = report.view_snapshot.unwrap();
        assert_eq!(snapshot.paths.len(), 134);
        assert_eq!(
            snapshot
                .analysis
                .checks
                .iter()
                .filter(|check| check.rule == "structural.syntax"
                    && check.status == atomic_canonical::review_finding::CheckStatus::Completed)
                .count(),
            134
        );
        let measured: BTreeSet<_> = snapshot
            .analysis
            .metrics
            .iter()
            .filter(|metric| {
                matches!(
                    metric.observation.after,
                    atomic_audit::Measurement::Measured { .. }
                )
            })
            .map(|metric| metric.path.as_str())
            .collect();
        assert_eq!(measured.len(), 134);
        assert!(measured.contains("file_133.rs"));
    }

    #[test]
    fn snapshot_clones_cover_the_whole_current_corpus_without_an_invented_baseline() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repository::init(directory.path()).unwrap();
        repo.init_vault().unwrap();
        for path in ["a.rs", "b.rs"] {
            std::fs::write(directory.path().join(path),"pub fn clone(value: i32) -> i32 { let a = value + 1; let b = a * 2; let c = b - value; if c > 0 { c + a } else { value - c } }\n").unwrap();
            repo.add(
                repo.require_working_copy_id().unwrap(),
                path,
                TrackingOptions::default(),
            )
            .unwrap();
        }
        record(&repo);
        let report = collect(&repo, repo.current_view()).unwrap();
        let snapshot = report.view_snapshot.unwrap();
        let clones: Vec<_> = snapshot
            .analysis
            .metrics
            .iter()
            .filter(|m| m.observation.metric == atomic_audit::Metric::DuplicatedTokens)
            .collect();
        assert_eq!(clones.len(), 2);
        for metric in clones {
            assert!(
                matches!(metric.observation.after,atomic_audit::Measurement::Measured { value } if value > 0)
            );
            assert!(matches!(
                metric.observation.before,
                atomic_audit::Measurement::Unavailable { .. }
            ));
            assert!(metric.delta.is_none());
        }
    }
}
