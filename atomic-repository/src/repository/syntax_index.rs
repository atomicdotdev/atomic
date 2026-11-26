//! Request-scoped source relationships. Nothing here is persisted in the KG.

use super::*;
use atomic_semantic::relationships::{script_candidates, SourceGraph};
use atomic_semantic::{Entity, EntityKind, ParserRegistry};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, serde::Serialize)]
pub struct Caller {
    pub path: String,
    pub name: String,
    pub line: u32,
    pub summary: Option<String>,
}

#[derive(Default)]
pub struct SyntaxIndex {
    pub entities: Vec<Entity>,
    pub calls: BTreeSet<(String, String)>,
    pub file_deps: BTreeSet<(String, String)>,
    pub file_modules: BTreeMap<String, String>,
    bound_callers: BTreeMap<String, Vec<Caller>>,
    aliases: BTreeMap<String, BTreeSet<String>>,
}

fn selector(entity: &Entity) -> String {
    format!("{}#{}", entity.file, entity.name)
}
fn callable(entity: &Entity) -> bool {
    matches!(entity.kind, EntityKind::Function | EntityKind::Method)
}

impl SyntaxIndex {
    pub fn from_sources(sources: &[(String, String)]) -> Self {
        let mut parser = ParserRegistry::new();
        let mut index = Self::default();
        let mut parsed = Vec::new();
        let mut paths = BTreeSet::new();
        for (path, source) in sources {
            if crate::is_vault_path(path) {
                continue;
            }
            paths.insert(path.clone());
            index.file_modules.insert(
                path.clone(),
                Path::new(path)
                    .parent()
                    .and_then(|p| p.to_str())
                    .unwrap_or_default()
                    .to_owned(),
            );
            match parser.analyze_source(path, source) {
                Ok(file) => {
                    index.entities.extend(file.entities.clone());
                    parsed.push(file);
                }
                Err(_) => index.entities.extend(parser.extract(path, source)),
            }
        }
        let graph = SourceGraph::new(parsed, paths, true);
        for symbol in graph
            .symbols
            .values()
            .filter(|symbol| callable(&symbol.entity))
        {
            for alias in [
                symbol.id.clone(),
                selector(&symbol.entity),
                symbol.entity.name.clone(),
            ] {
                index
                    .aliases
                    .entry(alias)
                    .or_default()
                    .insert(symbol.id.clone());
            }
        }
        for call in graph
            .bind_calls(None)
            .into_iter()
            .filter(|call| call.present && call.accessible)
        {
            let caller = &graph.symbols[&call.call.caller];
            index
                .calls
                .insert((call.symbol.id.clone(), caller.id.clone()));
            index
                .bound_callers
                .entry(call.symbol.id.clone())
                .or_default()
                .push(Caller {
                    path: caller.location.path.clone(),
                    name: caller.entity.name.clone(),
                    line: caller.location.start_line,
                    summary: caller.entity.signature.clone(),
                });
            if caller.location.path != call.symbol.location.path {
                index.file_deps.insert((
                    caller.location.path.clone(),
                    call.symbol.location.path.clone(),
                ));
            }
        }
        for file in graph.files.values() {
            for import in file
                .entities
                .iter()
                .filter(|entity| entity.kind == EntityKind::Import)
            {
                for target in script_candidates(&file.path, &import.name, &graph.paths) {
                    if target != file.path {
                        index.file_deps.insert((file.path.clone(), target));
                    }
                }
            }
        }
        index
    }

    pub fn callers(&self, target: &str) -> Vec<Caller> {
        let mut result: Vec<_> = self
            .aliases
            .get(target)
            .into_iter()
            .flatten()
            .flat_map(|id| self.bound_callers.get(id).into_iter().flatten())
            .cloned()
            .collect();
        result.sort_by(|a, b| (&a.path, &a.name, a.line).cmp(&(&b.path, &b.name, b.line)));
        result.dedup_by(|a, b| a.path == b.path && a.name == b.name && a.line == b.line);
        result
    }
}

impl Repository {
    /// Parse each file once. View reads render graph bytes through the selected
    /// filter; working-copy queries read that physical directory's tracked files.
    pub fn syntax_index(&self, view: Option<&str>) -> Result<SyntaxIndex, RepositoryError> {
        let paths = if let Some(view) = view {
            let txn = self.pristine.read_txn()?;
            let view = txn
                .get_view(view)?
                .ok_or_else(|| RepositoryError::ViewNotFound {
                    name: view.to_owned(),
                })?;
            let visibility = graph_visibility_closure(&txn, &view)?;
            self.project_tree_for_visibility(&txn, &visibility)?
                .present
                .into_values()
                .filter(|entry| !entry.is_directory)
                .map(|entry| entry.path)
                .collect::<Vec<_>>()
        } else {
            self.list_tracked_files()?
                .into_iter()
                .filter(|file| !file.is_directory)
                .map(|file| file.path.to_string_lossy().into_owned())
                .collect()
        };
        let mut sources = Vec::new();
        for path in paths {
            if crate::is_vault_path(&path) || !atomic_semantic::is_supported(&path) {
                continue;
            }
            let bytes = if let Some(view) = view {
                self.get_file_content_on_view(&path, view)?
            } else {
                match std::fs::read(self.root().join(&path)) {
                    Ok(bytes) => Some(bytes),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Err(error) => return Err(error.into()),
                }
            };
            if let Some(source) = bytes.and_then(|bytes| String::from_utf8(bytes).ok()) {
                sources.push((path, source));
            }
        }
        Ok(SyntaxIndex::from_sources(&sources))
    }

    pub fn query_callers(&self, target: &str) -> Result<Vec<Caller>, RepositoryError> {
        Ok(self.syntax_index(None)?.callers(target))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caller_queries_use_shared_qualified_bindings_not_global_name_joins() {
        let index = SyntaxIndex::from_sources(&[
            ("api.ts".into(), "export function value() {}".into()),
            (
                "noise.ts".into(),
                "function unrelated() { table.value(); }".into(),
            ),
            (
                "user.ts".into(),
                "import {value as read} from './api'; function consumer() { read(); }".into(),
            ),
            ("api.rs".into(), "pub fn value() {}".into()),
        ]);
        let callers = index.callers("api.ts#value");
        assert_eq!(callers.len(), 1);
        assert_eq!(callers[0].name, "consumer");
        assert!(index.callers("api.rs#value").is_empty());
    }

    #[test]
    fn static_relationships_never_treat_vault_source_as_application_code() {
        let index = SyntaxIndex::from_sources(&[
            ("src/api.ts".into(), "export function work() {}".into()),
            (
                ".vault/fake.ts".into(),
                "import { work } from '../src/api'; export function fake() { work(); }".into(),
            ),
            (
                "nested/.vault/more.ts".into(),
                "export function another() {}".into(),
            ),
            (
                ".vaultish/real.ts".into(),
                "export function real() {}".into(),
            ),
        ]);
        assert!(!index
            .entities
            .iter()
            .any(|entity| crate::is_vault_path(&entity.file)));
        assert!(index.callers("src/api.ts#work").is_empty());
        assert!(!index
            .file_modules
            .keys()
            .any(|path| crate::is_vault_path(path)));
        assert!(index
            .entities
            .iter()
            .any(|entity| entity.file == ".vaultish/real.ts"));
    }

    #[test]
    fn script_imports_follow_relative_paths_instead_of_duplicate_file_stems() {
        let index = SyntaxIndex::from_sources(&[
            ("src/ui.ts".into(), "import { changed } from './api';\nimport '../shared';\nexport { api } from './nested';\n".into()),
            ("src/api.ts".into(), "export function changed() {}".into()),
            ("tests/api.ts".into(), "function other() {}".into()),
            ("shared.ts".into(), "export const value = 1;".into()),
            ("src/nested/index.ts".into(), "export const api = 1;".into()),
        ]);
        assert_eq!(
            index.file_deps,
            BTreeSet::from([
                ("src/ui.ts".into(), "src/api.ts".into()),
                ("src/ui.ts".into(), "shared.ts".into()),
                ("src/ui.ts".into(), "src/nested/index.ts".into()),
            ])
        );
    }
}
