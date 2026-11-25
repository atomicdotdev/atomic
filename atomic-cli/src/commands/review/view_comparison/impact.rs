use atomic_audit::impact::ImpactReport;
use atomic_canonical::review_graph::SubmittedFinding;
use atomic_repository::Repository;
use atomic_semantic::ParserRegistry;
use std::collections::BTreeSet;

use super::ViewComparison;

pub(in super::super) fn changed_paths(comparison: &ViewComparison) -> Vec<String> {
    if comparison.before.state == comparison.after.state
        && comparison.before.membership_hash == comparison.after.membership_hash
    {
        return vec![];
    }
    let recorded_changes: BTreeSet<_> = comparison
        .conformance
        .changes
        .iter()
        .flat_map(|change| change.paths.iter())
        .collect();
    comparison
        .files
        .iter()
        .filter(|file| {
            file.status != "unchanged"
                && (file.status != "unavailable"
                    || file
                        .before_path
                        .iter()
                        .chain(&file.after_path)
                        .any(|path| recorded_changes.contains(path)))
        })
        .flat_map(|file| file.before_path.iter().chain(&file.after_path))
        .filter(|path| !atomic_repository::is_vault_path(path))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

pub(in super::super) fn inspect(
    repo: &Repository,
    comparison: &ViewComparison,
) -> (Vec<ImpactReport>, Vec<SubmittedFinding>) {
    let paths = changed_paths(comparison);
    let mut result = ImpactReport {
        change: comparison.source_ref.clone(),
        code_changed: !paths.is_empty(),
        ..Default::default()
    };
    if paths.is_empty() {
        result.source_selection_complete = Some(true);
        result.unresolved_references = Some(0);
    } else if !paths.iter().any(|path| atomic_semantic::is_supported(path)) {
        result.gaps.push("Changed paths have no supported semantic source; configuration/non-code runtime effects are not inferred".into());
    } else {
        match (
            repo.semantic_view_on_view(&comparison.before.view),
            repo.semantic_view_on_view(&comparison.after.view),
        ) {
            (Ok(before), Ok(after)) => {
                result = super::super::impact::inspect_views(
                    &before,
                    &after,
                    &paths,
                    &mut ParserRegistry::new(),
                );
                result.change = comparison.source_ref.clone();
            }
            (before, after) => {
                if let Err(error) = before {
                    result.gaps.push(format!(
                        "Before named-view semantic source unavailable: {error}"
                    ));
                }
                if let Err(error) = after {
                    result.gaps.push(format!(
                        "After named-view semantic source unavailable: {error}"
                    ));
                }
            }
        }
    }
    let findings = super::super::impact::contract_findings(&result);
    result.gaps.push("Compared the complete effective named-view states, not intermediate authored patches. Semantic callable IDs are path/module-addressed; a recorded file rename can change bindings. Caller context is bounded by the existing semantic selector; compiler/type/runtime/test execution is not performed.".into());
    (vec![result], findings)
}
