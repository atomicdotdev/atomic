//! Request-scoped source relationships. Nothing here is persisted in the KG.

use super::*;
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
        let mut names = BTreeMap::<String, BTreeSet<String>>::new();
        for (path, source) in sources {
            let entities = parser.extract(path, source);
            for entity in entities.iter().filter(|entity| callable(entity)) {
                names
                    .entry(entity.name.clone())
                    .or_default()
                    .insert(selector(entity));
            }
            index.file_modules.insert(
                path.clone(),
                Path::new(path)
                    .parent()
                    .and_then(|p| p.to_str())
                    .unwrap_or_default()
                    .to_owned(),
            );
            index.entities.extend(entities.clone());
            parsed.push((path, source, entities));
        }
        for (path, source, entities) in &parsed {
            for reference in parser.extract_references(path, source) {
                let Some(caller) = entities
                    .iter()
                    .filter(|entity| {
                        callable(entity)
                            && reference.line >= entity.line
                            && reference.line <= entity.end_line
                    })
                    .min_by_key(|entity| entity.end_line - entity.line)
                else {
                    continue;
                };
                let caller = selector(caller);
                if let Some(targets) = names.get(&reference.symbol) {
                    for target in targets {
                        if target != &caller {
                            index.calls.insert((target.clone(), caller.clone()));
                        }
                    }
                }
            }
            for import in entities
                .iter()
                .filter(|entity| entity.kind == EntityKind::Import)
            {
                let tokens: BTreeSet<_> = import
                    .name
                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                    .filter(|s| !s.is_empty())
                    .collect();
                for (target, _) in sources {
                    let stem = Path::new(target)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or_default();
                    if target != *path && tokens.contains(stem) {
                        index.file_deps.insert(((*path).clone(), target.clone()));
                    }
                }
            }
        }
        index
    }

    pub fn callers(&self, target: &str) -> Vec<Caller> {
        let (path, name) = target
            .rsplit_once('#')
            .map(|(path, name)| (Some(path), name))
            .unwrap_or((None, target));
        let targets: BTreeSet<_> = self
            .entities
            .iter()
            .filter(|entity| {
                callable(entity)
                    && entity.name == name
                    && path.is_none_or(|path| entity.file == path)
            })
            .map(selector)
            .collect();
        let callers: BTreeSet<_> = self
            .calls
            .iter()
            .filter(|(callee, _)| targets.contains(callee))
            .map(|(_, caller)| caller.as_str())
            .collect();
        let mut result: Vec<_> = self
            .entities
            .iter()
            .filter(|entity| callable(entity) && callers.contains(selector(entity).as_str()))
            .map(|entity| Caller {
                path: entity.file.clone(),
                name: entity.name.clone(),
                line: entity.line,
                summary: entity.signature.clone(),
            })
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
            if !atomic_semantic::is_supported(&path) {
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
