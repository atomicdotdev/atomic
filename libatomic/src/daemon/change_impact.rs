//! Change-local structural impact. Never read working-copy sources, rebuild the
//! KG, traverse co-change history, or turn missing analysis into "private".
use crate::atomic::{ChangeBlastRadius, ChangeImpactEdge, ChangeImpactFile};
use atomic_core::change::Change;
use atomic_core::pristine::vault::KgSubgraph;
use atomic_core::types::Hash;
use atomic_repository::Repository;
use atomic_semantic::{Entity, ParserRegistry};
use similar::{ChangeTag, TextDiff};
use std::collections::{BTreeMap, BTreeSet};

const MAX_FILES: usize = 2000;
const MAX_SOURCE_BYTES: usize = 2 * 1024 * 1024;
const MAX_CALLERS: usize = 30;

pub(super) fn project(repo: &Repository, change: &Change, hash: &Hash) -> ChangeBlastRadius {
    let mut paths = atomic_repository::get_files_in_change(change);
    paths.extend(change.file_ops().iter().map(|op| op.path().to_string()));
    paths.sort();
    paths.dedup();
    let mut warnings = Vec::new();
    if paths.len() > MAX_FILES {
        warnings
            .push("Direct paths exceed the 2000-file analysis limit; coverage is partial.".into());
    }
    paths.truncate(MAX_FILES);
    let mut files: BTreeMap<String, u32> = paths.iter().cloned().map(|path| (path, 1)).collect();
    let mut links = BTreeSet::new();
    let mut registry = ParserRegistry::new();
    let mut analyzed = 0;
    let mut public_change = false;
    for (index, path) in paths.into_iter().enumerate() {
        if index >= 100 {
            warnings.push("Structural analysis limited to 100 direct files; remaining direct paths are retained.".into());
            break;
        }
        if !registry.is_supported(&path) {
            warnings.push(format!("{path}: no structural parser; direct impact only."));
            continue;
        }
        let before = repo.get_file_content_before_change(&path, hash);
        let after = repo.get_file_content_after_change(&path, hash);
        let (Ok(before), Ok(after)) = (before, after) else {
            warnings.push(format!("{path}: historical content reconstruction failed."));
            continue;
        };
        if before.is_none() && after.is_none() {
            warnings.push(format!("{path}: historical content unavailable."));
            continue;
        }
        if before.is_none() && !change.hunks().iter().any(|op| matches!(op, atomic_core::change::GraphOp::FileAdd { path: added, .. } if added == &path)) {
            warnings.push(format!("{path}: before-content missing without a file-add operation."));
        }
        if after.is_none() && !change.hunks().iter().any(|op| matches!(op, atomic_core::change::GraphOp::FileDel { path: deleted, .. } if deleted == &path)) {
            warnings.push(format!("{path}: after-content missing without a file-delete operation."));
        }
        let before = before.unwrap_or_default();
        let after = after.unwrap_or_default();
        if before.len() + after.len() > MAX_SOURCE_BYTES {
            warnings.push(format!("{path}: source exceeds the 2 MiB analysis limit."));
            continue;
        }
        let (Ok(before), Ok(after)) = (std::str::from_utf8(&before), std::str::from_utf8(&after))
        else {
            warnings.push(format!(
                "{path}: binary/non-UTF-8 source; direct impact only."
            ));
            continue;
        };
        let changed = changed_entities(&mut registry, &path, before, after);
        let Some(changed) = changed else {
            warnings.push(format!(
                "{path}: no analyzable entities; structural coverage unknown."
            ));
            continue;
        };
        analyzed += 1;
        let public: Vec<_> = changed.into_iter().filter(public_api).collect();
        if public.is_empty() {
            continue;
        }
        public_change = true;
        let file_id = format!("file:{path}");
        let Ok(graph) = repo.vault_kg_neighbors(&file_id, 1) else {
            warnings.push(format!("{path}: knowledge graph unavailable."));
            continue;
        };
        let defined: BTreeSet<_> = graph
            .edges
            .iter()
            .filter(|e| e.from_id == file_id && e.kind.eq_ignore_ascii_case("DEFINES"))
            .map(|e| e.to_id.as_str())
            .collect();
        if defined.is_empty() {
            warnings.push(format!(
                "{path}: no indexed definitions; structural coverage unknown."
            ));
            continue;
        }
        for entity in &public {
            if !matches!(entity.kind.as_str(), "function" | "method") {
                continue;
            }
            // Resolve stored IDs from indexed DEFINES edges. Do not fabricate an
            // entity ID at a historical line that may no longer exist in the KG.
            let matches: Vec<_> = graph
                .nodes
                .iter()
                .filter(|node| defined.contains(node.id.as_str()) && node.label == entity.name)
                .collect();
            if matches.len() != 1 {
                warnings.push(format!(
                    "{path}::{}: indexed identity absent/ambiguous.",
                    entity.name
                ));
                continue;
            }
            match repo.vault_kg_neighbors(&matches[0].id, 2) {
                Ok(callers) => collect_callers(
                    &callers,
                    &matches[0].id,
                    &path,
                    &mut files,
                    &mut links,
                    &mut warnings,
                ),
                Err(_) => warnings.push(format!("{path}::{}: callers unavailable.", entity.name)),
            }
        }
        // Importers are genuine incoming file dependencies. Include them for all
        // public API changes; do not hide type importers when callers also exist.
        for edge in &graph.edges {
            if edge.to_id == file_id && edge.kind.eq_ignore_ascii_case("INCLUDES") {
                if let Some(importer) = edge.from_id.strip_prefix("file:") {
                    add_link(importer, &path, "INCLUDES", &mut files, &mut links);
                }
            }
        }
    }
    let mode = if !warnings.is_empty() {
        "partial"
    } else if analyzed > 0 && !public_change {
        "private"
    } else {
        "structural"
    };
    if files.len() > MAX_FILES {
        warnings.push(
            "Structural impact exceeds the 2000-file display limit; coverage is partial.".into(),
        );
    }
    if links.len() > 4000 {
        warnings.push(
            "Structural links exceed the 4000-edge display limit; coverage is partial.".into(),
        );
    }
    // Preserve every direct file before bounded indirect results.
    let mut rows: Vec<_> = files.into_iter().collect();
    rows.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
    rows.truncate(MAX_FILES);
    let kept: BTreeSet<_> = rows.iter().map(|(path, _)| path.as_str()).collect();
    let edges = links
        .into_iter()
        .filter(|(from, to, _)| kept.contains(from.as_str()) && kept.contains(to.as_str()))
        .take(4000)
        .map(|(from_path, to_path, relation)| ChangeImpactEdge {
            from_path,
            to_path,
            relation,
        })
        .collect();
    ChangeBlastRadius {
        change_hash: Some(super::convert::hash_proto(hash)),
        mode: if warnings.is_empty() { mode } else { "partial" }.into(),
        files: rows.into_iter().map(|(path, hop)| ChangeImpactFile { path, hop }).collect(), edges,
        warnings: warnings.into_iter().take(100).collect(),
        context: "Recorded change sources + current repository KG; not a historical or view-isolated dependency snapshot.".into(),
    }
}

fn public_api(entity: &Entity) -> bool {
    entity.exported
        || matches!(
            entity.kind.as_str(),
            "class" | "interface" | "type_alias" | "enum"
        )
}

fn changed_entities(
    registry: &mut ParserRegistry,
    path: &str,
    before: &str,
    after: &str,
) -> Option<Vec<Entity>> {
    let old = registry.extract(path, before);
    let new = registry.extract(path, after);
    if old.is_empty() && new.is_empty() {
        return None;
    }
    let diff = TextDiff::configure()
        .timeout(std::time::Duration::from_millis(100))
        .diff_lines(before, after);
    let mut old_lines = BTreeSet::new();
    let mut new_lines = BTreeSet::new();
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Delete => {
                if let Some(line) = change.old_index() {
                    old_lines.insert(line as u32 + 1);
                }
            }
            ChangeTag::Insert => {
                if let Some(line) = change.new_index() {
                    new_lines.insert(line as u32 + 1);
                }
            }
            _ => {}
        }
    }
    let mut entities = Vec::new();
    for (candidates, lines) in [(old, old_lines), (new, new_lines)] {
        for entity in candidates {
            if lines.range(entity.line..=entity.end_line).next().is_some()
                && !entities
                    .iter()
                    .any(|e: &Entity| e.name == entity.name && e.kind == entity.kind)
            {
                entities.push(entity);
            }
        }
    }
    Some(entities)
}

fn add_link(
    from: &str,
    to: &str,
    kind: &str,
    files: &mut BTreeMap<String, u32>,
    links: &mut BTreeSet<(String, String, String)>,
) {
    if from.is_empty() || from == to {
        return;
    }
    files.entry(from.into()).or_insert(2);
    links.insert((from.into(), to.into(), kind.into()));
}

fn collect_callers(
    graph: &KgSubgraph,
    target: &str,
    path: &str,
    files: &mut BTreeMap<String, u32>,
    links: &mut BTreeSet<(String, String, String)>,
    warnings: &mut Vec<String>,
) {
    let callers: BTreeSet<_> = graph
        .edges
        .iter()
        .filter(|e| e.to_id == target && e.kind.eq_ignore_ascii_case("CALLS"))
        .map(|e| e.from_id.as_str())
        .collect();
    if callers.len() > MAX_CALLERS {
        warnings.push(format!(
            "{path}: common/high-fanout entity skipped (over {MAX_CALLERS} callers)."
        ));
        return;
    }
    for caller in callers {
        let definitions: Vec<_> = graph
            .edges
            .iter()
            .filter(|e| e.to_id == caller && e.kind.eq_ignore_ascii_case("DEFINES"))
            .filter_map(|e| e.from_id.strip_prefix("file:"))
            .collect();
        if definitions.is_empty() {
            warnings.push(format!("{path}: caller definition is not indexed."));
        }
        for source in definitions {
            add_link(source, path, "CALLS", files, links);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn analyzes_changed_ranges_in_both_versions_including_deletions() {
        let mut parser = ParserRegistry::new();
        let before = "export function kept() { return 1; }\nfunction hidden() { return 1; }\nexport function removed() { return 2; }\n";
        let after = "export function kept() { return 1; }\nfunction hidden() { return 2; }\n";
        let entities = changed_entities(&mut parser, "a.ts", before, after).unwrap();
        assert!(!entities.iter().any(|e| e.name == "kept"));
        assert!(entities
            .iter()
            .any(|e| e.name == "hidden" && !public_api(e)));
        assert!(entities
            .iter()
            .any(|e| e.name == "removed" && public_api(e)));
        assert!(changed_entities(&mut parser, "a.json", "{}", "{\"a\":1}").is_none());
    }
    #[test]
    fn callers_are_directed_and_direct_files_are_not_counted_twice() {
        use atomic_core::pristine::vault::KgEdge;
        let graph = KgSubgraph {
            nodes: vec![],
            edges: vec![
                KgEdge::new("entity:caller", "entity:target", "CALLS"),
                KgEdge::new("file:tests/caller.ts", "entity:caller", "DEFINES"),
                KgEdge::new("entity:target", "entity:outgoing", "CALLS"),
                KgEdge::new("file:wrong.ts", "entity:outgoing", "DEFINES"),
            ],
        };
        let mut files = BTreeMap::from([("src/a.ts".into(), 1)]);
        let mut links = BTreeSet::new();
        let mut warnings = vec![];
        collect_callers(
            &graph,
            "entity:target",
            "src/a.ts",
            &mut files,
            &mut links,
            &mut warnings,
        );
        assert_eq!(files.get("tests/caller.ts"), Some(&2));
        assert!(!files.contains_key("wrong.ts"));
        add_link(
            "src/a.ts",
            "tests/caller.ts",
            "INCLUDES",
            &mut files,
            &mut links,
        );
        assert_eq!(files["src/a.ts"], 1);
        assert!(warnings.is_empty());
    }

    #[test]
    fn common_callable_names_produce_a_partial_coverage_warning_not_arbitrary_callers() {
        use atomic_core::pristine::vault::KgEdge;
        let graph = KgSubgraph {
            nodes: vec![],
            edges: (0..31)
                .map(|i| KgEdge::new(format!("caller:{i}"), "target", "CALLS"))
                .collect(),
        };
        let mut files = BTreeMap::from([("src/a.ts".into(), 1)]);
        let mut links = BTreeSet::new();
        let mut warnings = vec![];
        collect_callers(
            &graph,
            "target",
            "src/a.ts",
            &mut files,
            &mut links,
            &mut warnings,
        );
        assert_eq!(files.len(), 1);
        assert!(links.is_empty());
        assert!(warnings[0].contains("high-fanout"));
    }
}
