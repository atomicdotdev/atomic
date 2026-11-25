//! Review policy over shared semantic facts; no AST traversal or source search.

use std::collections::BTreeSet;

use atomic_semantic::{
    relationships::{BoundCall, SourceGraph},
    source::{SourceSet, SourceSpan},
    ParserRegistry,
};

use super::{
    ChangedFunction, ContractAssessment, ContractIssue, DirectCall, ImpactReport, SourceLocation,
};

impl From<&SourceSpan> for SourceLocation {
    fn from(span: &SourceSpan) -> Self {
        Self {
            path: span.path.clone(),
            start_line: span.start_line,
            end_line: span.end_line,
        }
    }
}

fn direct(call: &BoundCall<'_>) -> DirectCall {
    DirectCall {
        caller: call.call.caller.clone(),
        callee: call.symbol.id.clone(),
        location: (&call.call.location).into(),
        declaration: (&call.symbol.location).into(),
        binding: call.reason.clone(),
        arguments: call.call.arguments,
        target_present: call.present,
        visibility_compatible: call.accessible,
        configuration_dependent: call.configuration_dependent,
    }
}

fn compatible(call: &BoundCall<'_>) -> Option<bool> {
    let arguments = call.call.arguments?;
    call.symbol.arity_known.then_some(
        call.present
            && call.accessible
            && arguments >= call.expected_min
            && call.expected_max.is_none_or(|max| arguments <= max),
    )
}

pub fn analyze(
    before: &[(String, String)],
    after: &[(String, String)],
    paths: &[String],
    registry: &mut ParserRegistry,
) -> ImpactReport {
    analyze_semantic(
        registry.analyze_sources(before),
        registry.analyze_sources(after),
        paths,
    )
}

pub fn analyze_with_coverage(
    before: &[(String, String)],
    after: &[(String, String)],
    paths: &[String],
    complete: bool,
    registry: &mut ParserRegistry,
) -> ImpactReport {
    let mut before = registry.analyze_sources(before);
    let mut after = registry.analyze_sources(after);
    before.complete &= complete;
    after.complete &= complete;
    before.paths_complete &= complete;
    after.paths_complete &= complete;
    analyze_semantic(before, after, paths)
}

/// Apply compatibility policy to already-parsed shared semantic source sets.
pub fn analyze_semantic(before: SourceSet, after: SourceSet, paths: &[String]) -> ImpactReport {
    let complete = before.complete && after.complete;
    let mut report = ImpactReport {
        source_selection_complete: Some(complete),
        inspected_files_before: before.files.len(),
        inspected_files_after: after.files.len(),
        searched_files_before: before.searched_files,
        searched_files_after: after.searched_files,
        ..Default::default()
    };
    report
        .gaps
        .extend(before.gaps.iter().map(|gap| format!("before: {gap}")));
    report
        .gaps
        .extend(after.gaps.iter().map(|gap| format!("after: {gap}")));
    let inventory_complete = before.paths_complete && after.paths_complete;
    let old = SourceGraph::new(before.files, before.paths, inventory_complete);
    let new = SourceGraph::new(after.files, after.paths, inventory_complete);
    let paths: BTreeSet<_> = paths
        .iter()
        .filter(|path| atomic_semantic::is_supported(path))
        .map(String::as_str)
        .collect();
    let mut roots = BTreeSet::new();
    for id in old
        .symbols
        .keys()
        .chain(new.symbols.keys())
        .collect::<BTreeSet<_>>()
    {
        let before = old.symbols.get(id);
        let after = new.symbols.get(id);
        let symbol = after.or(before).unwrap();
        if !matches!(
            symbol.entity.kind,
            atomic_semantic::EntityKind::Function | atomic_semantic::EntityKind::Method
        ) {
            continue;
        }
        if after.is_none()
            && new.paths.contains(&symbol.location.path)
            && !new.files.contains_key(&symbol.location.path)
        {
            if paths.contains(symbol.location.path.as_str()) {
                report.gaps.push(format!(
                    "{id}: after-source unavailable; deletion is not inferred"
                ));
            }
            continue;
        }
        if old.ambiguous.contains(id) || new.ambiguous.contains(id) {
            if paths.contains(symbol.location.path.as_str()) {
                report.gaps.push(format!("{id}: ambiguous conditional/overloaded declaration; no root delta or caller verdict inferred"));
            }
            continue;
        }
        if !paths.contains(symbol.location.path.as_str())
            || before.zip(after).is_some_and(|(a, b)| {
                a.tokens == b.tokens
                    && a.public == b.public
                    && a.parent_visible == b.parent_visible
                    && a.restricted == b.restricted
                    && a.conditional == b.conditional
            })
        {
            continue;
        }
        roots.insert(id.clone());
        report.changed_functions.push(ChangedFunction {
            id: id.clone(),
            before: before.map(|symbol| (&symbol.location).into()),
            after: after.map(|symbol| (&symbol.location).into()),
            change: match (before, after) {
                (None, _)
                    if old.paths.contains(&symbol.location.path)
                        && !old.files.contains_key(&symbol.location.path) =>
                {
                    "baseline_unavailable"
                }
                (None, _) => "added",
                (_, None) => "removed",
                _ => "modified",
            }
            .into(),
            signature_changed: before.zip(after).is_some_and(|(a, b)| {
                a.signature != b.signature
                    || a.public != b.public
                    || a.parent_visible != b.parent_visible
                    || a.restricted != b.restricted
                    || a.conditional != b.conditional
            }),
        });
        if before
            .zip(after)
            .is_some_and(|(a, b)| a.conditional != b.conditional)
        {
            report.gaps.push(format!("{}: conditional configuration changed; active configuration is not established by source syntax", symbol.id));
        }
    }
    for path in &paths {
        if old
            .files
            .get(*path)
            .zip(new.files.get(*path))
            .is_some_and(|(a, b)| a.tokens == b.tokens)
        {
            continue;
        }
        report.code_changed = true;
        let before = old
            .files
            .get(*path)
            .map(|file| file.outside_tokens.as_slice())
            .unwrap_or(&[]);
        let after = new
            .files
            .get(*path)
            .map(|file| file.outside_tokens.as_slice())
            .unwrap_or(&[]);
        if before != after {
            report.non_callable_changes.push((*path).into());
            report.gaps.push(format!("{path}: changed imports/types/globals require implementation review beyond call contracts"));
        }
    }
    let before_calls = old.bind_calls(None);
    let after_calls = new.bind_calls(Some(&old));
    report.callers_before = before_calls
        .iter()
        .filter(|call| roots.contains(&call.symbol.id))
        .map(direct)
        .collect();
    report.callers_after = after_calls
        .iter()
        .filter(|call| roots.contains(&call.symbol.id))
        .map(direct)
        .collect();
    report.callees_after = after_calls
        .iter()
        .filter(|call| roots.contains(&call.call.caller))
        .map(direct)
        .collect();
    let names: BTreeSet<_> = roots
        .iter()
        .filter_map(|id| new.symbols.get(id).or_else(|| old.symbols.get(id)))
        .map(|symbol| symbol.entity.name.as_str())
        .collect();
    let bound_sites: BTreeSet<_> = after_calls
        .iter()
        .map(|call| (&call.call.location, &call.call.caller))
        .collect();
    let unresolved = new
        .files
        .values()
        .flat_map(|file| &file.calls)
        .filter(|call| {
            (roots.contains(&call.caller)
                || call
                    .target
                    .last()
                    .is_some_and(|name| names.contains(name.as_str())))
                && !bound_sites.contains(&(&call.location, &call.caller))
        })
        .count();
    report.unresolved_references = Some(unresolved);
    if unresolved > 0 {
        report.gaps.push(format!("{unresolved} relevant source reference(s) need additional receiver/import context; excluded from bound-caller counts"));
    }
    let mut unchecked = 0;
    for call in &after_calls {
        if !roots.contains(&call.symbol.id) && !roots.contains(&call.call.caller) {
            continue;
        }
        let Some(valid) = compatible(call) else {
            unchecked += 1;
            continue;
        };
        report.checked_call_contracts += 1;
        if valid {
            report.compatible_call_contracts += 1;
            continue;
        }
        let prior: Vec<_> = before_calls
            .iter()
            .filter(|before| {
                before.call.caller == call.call.caller
                    && before.symbol.id == call.symbol.id
                    && before.call.tokens == call.call.tokens
            })
            .collect();
        if prior.iter().any(|call| compatible(call) == Some(false)) {
            report.preexisting_incompatible_contracts += 1;
            continue;
        }
        let before_symbol = old.symbols.get(&call.symbol.id);
        let after_symbol = new.symbols.get(&call.symbol.id);
        let introduced = prior.iter().any(|call| compatible(call) == Some(true))
            || (complete && roots.contains(&call.call.caller) && prior.is_empty());
        let (primary_location, primary_before) = if roots.contains(&call.call.caller) {
            ((&call.call.location).into(), false)
        } else if let Some(after) = after_symbol {
            ((&after.location).into(), false)
        } else {
            ((&call.symbol.location).into(), true)
        };
        report.issues.push(ContractIssue { function: call.symbol.id.clone(), call: direct(call), before_declaration: before_symbol.map(|symbol| (&symbol.location).into()), after_declaration: after_symbol.map(|symbol| (&symbol.location).into()), primary_location, primary_before, introduced,
            summary: if !call.present { "A previously bound call still references a removed declaration" } else if !call.accessible { "A direct reference violates the declaration's visibility/export contract" } else { "A bound call does not match the declaration's argument-count contract" }.into(),
            evidence: format!("AST target {} from {}; {} argument(s), expected {}..{:?}; target present={}, accessible={}; {}", call.symbol.id, call.call.caller, call.call.arguments.unwrap(), call.expected_min, call.expected_max, call.present, call.accessible, call.reason) });
    }
    if unchecked > 0 {
        report.gaps.push(format!("{unchecked} affected bound call(s) have unavailable argument-count conventions; no contract verdict inferred"));
    }
    if report.preexisting_incompatible_contracts > 0 {
        report.gaps.push(format!("{} incompatible call(s) already existed before the change; not attributed as new regressions", report.preexisting_incompatible_contracts));
    }
    report.contract_assessment =
        if !report.issues.is_empty() || report.preexisting_incompatible_contracts > 0 {
            ContractAssessment::IncompatibleResolvedCalls
        } else if report.checked_call_contracts > 0 {
            ContractAssessment::CompatibleResolvedCalls
        } else {
            ContractAssessment::NotEvaluated
        };
    report.gaps.sort();
    report.gaps.dedup();
    report
}
