//! Recorded-source selection for semantic consumers. Search precedes AST work.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use atomic_core::{
    pristine::GraphVisibilityClosure,
    types::{Hash, Inode, NodeId, Position},
    ReadTxn,
};
use atomic_semantic::{
    relationships::context_paths,
    source::{SourceSet, Symbol},
    Language, ParserRegistry,
};

use super::{Repository, RepositoryError};
use crate::content_search::{ContentSearchOptions, RecordedContentSearch, RECORDED_CONTENT_LIMIT};

pub struct RecordedSemanticView<'repo> {
    repo: &'repo Repository,
    txn: ReadTxn,
    visibility: GraphVisibilityClosure,
    files: BTreeMap<String, (Inode, Position<NodeId>)>,
}

impl Repository {
    /// Pin source selection to exactly the before/after recorded view prefix.
    pub fn semantic_view_at_change(
        &self,
        hash: &Hash,
        include_change: bool,
    ) -> Result<RecordedSemanticView<'_>, RepositoryError> {
        use atomic_core::pristine::ViewTxnT;
        let txn = self.pristine.read_txn()?;
        let view =
            txn.get_view(&self.current_view)?
                .ok_or_else(|| RepositoryError::ViewNotFound {
                    name: self.current_view.clone(),
                })?;
        let visibility =
            super::content::graph_visibility_at_change(&txn, &view, hash, include_change)?
                .ok_or_else(|| RepositoryError::InvalidOperation {
                    message: format!("recorded semantic context unavailable for {hash}"),
                })?;
        self.semantic_view_with_visibility(txn, visibility)
    }

    /// Pin semantic source selection to the complete effective recorded state of
    /// a named view, without switching or consulting working-copy content.
    pub fn semantic_view_on_view(
        &self,
        name: &str,
    ) -> Result<RecordedSemanticView<'_>, RepositoryError> {
        use atomic_core::pristine::ViewTxnT;
        let txn = self.pristine.read_txn()?;
        let view = txn
            .get_view(name)?
            .ok_or_else(|| RepositoryError::ViewNotFound { name: name.into() })?;
        let visibility = super::graph_visibility_closure(&txn, &view)?;
        self.semantic_view_with_visibility(txn, visibility)
    }

    fn semantic_view_with_visibility(
        &self,
        txn: ReadTxn,
        visibility: GraphVisibilityClosure,
    ) -> Result<RecordedSemanticView<'_>, RepositoryError> {
        let projection = self.project_tree_for_visibility(&txn, &visibility)?;
        if !projection.name_conflicts.is_empty() {
            return Err(RepositoryError::InvalidOperation {
                message: "semantic review refuses unresolved name conflicts".into(),
            });
        }
        let files = projection
            .present
            .into_values()
            .filter(|entry| {
                !entry.is_directory
                    && !crate::is_vault_path(&entry.path)
                    && atomic_semantic::is_supported(&entry.path)
            })
            .map(|entry| (entry.path, (entry.inode, entry.position)))
            .collect();
        Ok(RecordedSemanticView {
            repo: self,
            txn,
            visibility,
            files,
        })
    }
}

impl RecordedSemanticView<'_> {
    fn content(&self, path: &str) -> Result<Option<Vec<u8>>, RepositoryError> {
        let Some(&(inode, position)) = self.files.get(path) else {
            return Ok(None);
        };
        let attributes = atomic_core::output::project_inode_attributes(
            &self.txn,
            position,
            self.visibility.attribute_visibility(),
        )?;
        if attributes.is_conflicted()
            || attributes.materialization.kind != atomic_core::change::InodeKind::Regular
        {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "{path}: semantic source is non-regular or has conflicting attributes"
                ),
            });
        }
        let content = match super::content::retrieve_span_bounded_if_present(
            &self.txn,
            &self.repo.change_store,
            inode,
            &self.visibility,
            RECORDED_CONTENT_LIMIT,
        )? {
            Some(content) => content,
            None => super::content::retrieve_content_bounded(
                &self.txn,
                &self.repo.change_store,
                inode,
                position,
                atomic_core::output::alive::RetrieveOptions::new()
                    .with_graph_visibility(self.visibility.clone()),
                RECORDED_CONTENT_LIMIT,
            )
            .map_err(|error| RepositoryError::Database(error.to_string()))?,
        };
        match content {
            super::content::BoundedContent::Complete(bytes) => Ok(Some(bytes)),
            super::content::BoundedContent::Oversized { .. } => {
                Err(RepositoryError::InvalidOperation {
                    message: format!("{path}: recorded content exceeds semantic search byte limit"),
                })
            }
        }
    }

    /// Parse changed files first. Unrelated paths cannot consume their budget.
    pub fn changed_sources(&self, paths: &[String], parser: &mut ParserRegistry) -> SourceSet {
        let mut set = SourceSet {
            paths: self.files.keys().cloned().collect(),
            complete: true,
            paths_complete: true,
            ..Default::default()
        };
        let (mut bytes, mut omitted) = (0, 0);
        for (index, path) in paths
            .iter()
            .filter(|path| self.files.contains_key(*path))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .enumerate()
        {
            if index >= 256 {
                omitted += 1;
                continue;
            }
            match self.content(path).and_then(|bytes| {
                bytes
                    .map(|bytes| {
                        String::from_utf8(bytes).map_err(|error| {
                            RepositoryError::InvalidOperation {
                                message: error.to_string(),
                            }
                        })
                    })
                    .transpose()
            }) {
                Ok(Some(source)) => {
                    if bytes + source.len() > 8 * 1024 * 1024 {
                        omitted += 1;
                        continue;
                    }
                    bytes += source.len();
                    match parser.analyze_source(path, &source) {
                        Ok(file) => set.files.push(file),
                        Err(error) => {
                            set.complete = false;
                            set.gaps.push(format!("{path}: {error}"));
                        }
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    set.complete = false;
                    set.gaps
                        .push(format!("{path}: changed source unavailable: {error}"));
                }
            }
        }
        if omitted > 0 {
            set.complete = false;
            set.gaps.push(format!("{omitted} changed source(s) exceed the root parsing budget (256 changed files / 8 MiB)"));
        }
        set
    }

    /// Reuse recorded syntext path/regex selection, then parse only matching
    /// files and explicit declaration/import context. No working-copy index use.
    pub fn select_context(
        &self,
        mut set: SourceSet,
        targets: &[Symbol],
        parser: &mut ParserRegistry,
    ) -> Result<SourceSet, RepositoryError> {
        if targets.is_empty() {
            return Ok(set);
        }
        let mut known: BTreeSet<_> = set.files.iter().map(|file| file.path.clone()).collect();
        let patterns = targets
            .iter()
            .map(|symbol| {
                let name = regex::escape(&symbol.entity.name);
                let pattern = if let Some(owner) = &symbol.owner {
                    let owner = regex::escape(owner);
                    format!(r"(?s:\b{owner}\b.*\b{name}\b|\b{name}\b.*\b{owner}\b)")
                } else {
                    format!(r"\b{name}\b")
                };
                regex::bytes::Regex::new(&pattern).map(|pattern| (symbol, pattern))
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| RepositoryError::InvalidOperation {
                message: error.to_string(),
            })?;
        let pattern = patterns
            .iter()
            .map(|(_, pattern)| pattern.as_str())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
            .join("|");
        let plan = RecordedContentSearch::new(
            self.files
                .keys()
                .filter(|path| {
                    !known.contains(*path)
                        && targets.iter().any(|target| candidate_path(path, target))
                })
                .map(String::as_str),
            &pattern,
            ContentSearchOptions::default(),
        )
        .map_err(|error| RepositoryError::InvalidOperation {
            message: error.to_string(),
        })?;
        let mut failed = 0;
        let matches = plan.matching_sources(
            256usize.saturating_sub(set.files.len()),
            8 * 1024 * 1024,
            |path| match self.content(path) {
                Ok(content) => Ok::<_, RepositoryError>(content),
                Err(_) => {
                    failed += 1;
                    Ok(None)
                }
            },
            |path, source| {
                patterns.iter().any(|(target, pattern)| {
                    candidate_path(path, target) && pattern.is_match(source)
                })
            },
        )?;
        set.searched_files = matches.searched_files;
        if matches.omitted_files > 0 || failed > 0 {
            set.complete = false;
            set.gaps.push(format!("Semantic candidate selection omitted {} matching/unavailable source(s); caller coverage is incomplete", matches.omitted_files + failed));
        }
        for (path, source) in matches.sources {
            known.insert(path.clone());
            match parser.analyze_source(&path, &source) {
                Ok(file) => set.files.push(file),
                Err(error) => {
                    set.complete = false;
                    set.gaps
                        .push(format!("{path}: candidate syntax unavailable: {error}"));
                }
            }
        }
        let inventory: BTreeSet<_> = self.files.keys().cloned().collect();
        let mut pending: VecDeque<_> = set
            .files
            .iter()
            .flat_map(|file| context_paths(file, &inventory))
            .collect();
        let (mut omitted, mut context_count, mut context_bytes) = (0, 0, 0);
        while let Some(path) = pending.pop_front() {
            if !known.insert(path.clone()) {
                continue;
            }
            if context_count >= 64 {
                omitted += 1;
                continue;
            }
            context_count += 1;
            match self.content(&path) {
                Ok(Some(bytes)) => {
                    if context_bytes + bytes.len() > 8 * 1024 * 1024 {
                        omitted += 1;
                        continue;
                    }
                    context_bytes += bytes.len();
                    match String::from_utf8(bytes)
                        .map_err(|error| error.to_string())
                        .and_then(|source| parser.analyze_source(&path, &source))
                    {
                        Ok(file) => {
                            pending.extend(context_paths(&file, &inventory));
                            set.files.push(file);
                        }
                        Err(error) => {
                            set.complete = false;
                            set.gaps.push(format!(
                                "{path}: declaration/import context unavailable: {error}"
                            ));
                        }
                    }
                }
                _ => {
                    omitted += 1;
                }
            }
        }
        if omitted > 0 {
            set.complete = false;
            set.gaps.push(format!("{omitted} required declaration/import context source(s) unavailable or over budget"));
        }
        Ok(set)
    }
}

fn candidate_path(path: &str, target: &Symbol) -> bool {
    if path == target.location.path || target.public {
        return true;
    }
    if Language::detect(&target.location.path) != Some(Language::Rust) {
        return false;
    }
    let file = std::path::Path::new(&target.location.path);
    let mut region = file
        .parent()
        .unwrap_or_else(|| std::path::Path::new(""))
        .to_path_buf();
    if !matches!(
        file.file_name().and_then(|name| name.to_str()),
        Some("lib.rs" | "main.rs" | "mod.rs")
    ) {
        region.push(file.file_stem().unwrap_or_default());
    }
    for module in &target.module_scope {
        region.push(module);
    }
    if target.parent_visible {
        region.pop();
    }
    std::path::Path::new(path).starts_with(region)
}
