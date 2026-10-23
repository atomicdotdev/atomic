//! Change-local structural impact. Never read working-copy sources, rebuild the
//! KG, traverse co-change history, or turn missing analysis into "private".
//! Dependency context comes from recorded bytes visible in the selected Atomic
//! view, never from a persisted code-structure graph or working-copy buffers.
use crate::atomic::{ChangeBlastRadius, ChangeImpactEdge, ChangeImpactFile};
use atomic_core::change::Change;
use atomic_core::types::Hash;
use atomic_repository::{Repository, SyntaxIndex};
use atomic_semantic::{Entity, ParserRegistry};
use similar::{ChangeTag, TextDiff};
use std::collections::{BTreeMap, BTreeSet};

const MAX_FILES: usize = 2000;
const MAX_SOURCE_BYTES: usize = 2 * 1024 * 1024;
const MAX_CALLERS: usize = 30;

pub(super) fn project(
    repo: &Repository,
    change: &Change,
    hash: &Hash,
    view: &str,
) -> ChangeBlastRadius {
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
    let mut syntax = None;
    for (index, path) in paths.into_iter().enumerate() {
        if index >= 100 {
            warnings.push("Structural analysis limited to 100 direct files; remaining direct paths are retained.".into());
            break;
        }
        if !registry.is_supported(&path) {
            warnings.push(format!("{path}: no structural parser; direct impact only."));
            continue;
        }
        let before = repo.get_file_content_before_change_on_view(&path, hash, view);
        let after = repo.get_file_content_after_change_on_view(&path, hash, view);
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
        let index = syntax.get_or_insert_with(|| repo.syntax_index(Some(view)));
        let index = match index {
            Ok(index) => index,
            Err(error) => {
                warnings.push(format!("{path}: view syntax unavailable: {error}"));
                continue;
            }
        };
        for entity in &public {
            if !matches!(entity.kind.as_str(), "function" | "method") {
                continue;
            }
            let matches: Vec<_> = index
                .entities
                .iter()
                .filter(|candidate| candidate.file == path && candidate.name == entity.name)
                .collect();
            if matches.len() != 1 {
                warnings.push(format!(
                    "{path}::{}: definition absent/ambiguous in the selected view.",
                    entity.name
                ));
                continue;
            }
            collect_callers(
                index,
                &format!("{path}#{}", entity.name),
                &path,
                &mut files,
                &mut links,
                &mut warnings,
            );
        }
        // Importers are genuine incoming file dependencies. Include them for all
        // public API changes; do not hide type importers when callers also exist.
        for (importer, target) in &index.file_deps {
            if target == &path {
                add_link(importer, &path, "includes", &mut files, &mut links);
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
        context: format!("Recorded change sources + syntax visible in Atomic view '{view}'; not a historical dependency snapshot."),
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
    let definitions = |entities: Vec<Entity>| {
        entities
            .into_iter()
            .filter(|entity| entity.kind != atomic_semantic::EntityKind::Import)
            .collect::<Vec<_>>()
    };
    let old = definitions(registry.extract(path, before));
    let new = definitions(registry.extract(path, after));
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
    let mut entities: Vec<Entity> = Vec::new();
    for (candidates, lines) in [(old, old_lines), (new, new_lines)] {
        for entity in candidates {
            if lines.range(entity.line..=entity.end_line).next().is_none() {
                continue;
            }
            if let Some(existing) = entities
                .iter_mut()
                .find(|e| e.name == entity.name && e.kind == entity.kind)
            {
                // Becoming public is an API change just as becoming private is.
                // Retain visibility from either recorded version, not whichever
                // version happened to be visited first.
                existing.exported |= entity.exported;
            } else {
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
    index: &SyntaxIndex,
    target: &str,
    path: &str,
    files: &mut BTreeMap<String, u32>,
    links: &mut BTreeSet<(String, String, String)>,
    warnings: &mut Vec<String>,
) {
    let callers = index.callers(target);
    if callers.len() > MAX_CALLERS {
        warnings.push(format!(
            "{path}: common/high-fanout entity skipped (over {MAX_CALLERS} callers)."
        ));
        return;
    }
    for caller in callers {
        add_link(&caller.path, path, "calls", files, links);
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
        assert!(
            changed_entities(&mut parser, "a.ts", "import './old';", "import './new';").is_none()
        );
    }
    #[test]
    fn visibility_changes_use_public_api_from_either_recorded_version() {
        let mut parser = ParserRegistry::new();
        let private = "function changed() { return 1; }\n";
        let public = "export function changed() { return 1; }\n";
        for (before, after) in [(private, public), (public, private)] {
            let entities = changed_entities(&mut parser, "a.ts", before, after).unwrap();
            assert_eq!(entities.len(), 1);
            assert!(public_api(&entities[0]));
        }
    }

    #[test]
    fn callers_are_directed_and_direct_files_are_not_counted_twice() {
        let index = SyntaxIndex::from_sources(&[
            (
                "src/a.ts".into(),
                "export function target() { outgoing(); }".into(),
            ),
            (
                "tests/caller.ts".into(),
                "function caller() { target(); }".into(),
            ),
            ("wrong.ts".into(), "function outgoing() {}".into()),
        ]);
        let mut files = BTreeMap::from([("src/a.ts".into(), 1)]);
        let mut links = BTreeSet::new();
        let mut warnings = vec![];
        collect_callers(
            &index,
            "src/a.ts#target",
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
            "includes",
            &mut files,
            &mut links,
        );
        assert_eq!(files["src/a.ts"], 1);
        assert!(warnings.is_empty());
    }

    #[test]
    fn common_callable_names_produce_a_partial_coverage_warning_not_arbitrary_callers() {
        let mut sources = vec![("src/a.ts".into(), "export function target() {}".into())];
        sources.extend((0..31).map(|i| {
            (
                format!("caller{i}.ts"),
                format!("function caller{i}() {{ target(); }}"),
            )
        }));
        let index = SyntaxIndex::from_sources(&sources);
        let mut files = BTreeMap::from([("src/a.ts".into(), 1)]);
        let mut links = BTreeSet::new();
        let mut warnings = vec![];
        collect_callers(
            &index,
            "src/a.ts#target",
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
