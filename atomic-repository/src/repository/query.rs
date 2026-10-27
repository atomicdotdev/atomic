//! Read-only queries over recorded view projections, not ambient KG or disk bytes.

use std::collections::{BTreeMap, HashSet};

use atomic_core::kg::{
    iri,
    ontology::{Class, Predicate},
};
use atomic_core::pristine::KgTxnT;
use atomic_core::KgNode;

use super::*;
use crate::content_search::{
    ContentSearchOptions, ContentSearchResult, RecordedContentSearch, RECORDED_CONTENT_LIMIT,
};
use atomic_core::pristine::GraphVisibilityClosure;

pub(super) struct QueryView<'repo> {
    repo: &'repo Repository,
    pub(super) txn: atomic_core::ReadTxn,
    visibility: GraphVisibilityClosure,
    files: BTreeMap<String, (Inode, Position<NodeId>)>,
    changes: HashSet<String>,
    views: HashSet<String>,
    nodes: BTreeMap<String, KgNode>,
}

impl QueryView<'_> {
    pub fn nodes(&self) -> &BTreeMap<String, KgNode> {
        &self.nodes
    }

    pub fn contains_file(&self, path: &str) -> bool {
        self.files.contains_key(path)
    }

    /// Project ambient candidates before ranking. File identities are canonical,
    /// but their presence must come from the selected view's current projection.
    pub fn project_node<T: KgTxnT>(
        &self,
        txn: &T,
        node: KgNode,
    ) -> Result<Option<KgNode>, RepositoryError> {
        let visible = if node.has_type(Class::File) {
            return Ok(self.nodes.get(&node.id).cloned());
        } else if node.has_type(Class::Change) {
            self.changes.contains(&node.id)
        } else if node.has_type(Class::View) {
            self.views.contains(&node.id)
        } else if node.has_type(Class::Identity) && node.source == "vcs" {
            let mut visible = false;
            for edge in txn.get_kg_edges_to(&node.id)? {
                if edge.kind != Predicate::AuthoredBy {
                    continue;
                }
                let Some(authoring_node) = txn.get_kg_node(&edge.from_id)? else {
                    continue;
                };
                // An identity shared with a vault record stays shared. A VCS-only
                // identity needs an authored change visible in the selected view.
                if !authoring_node.has_type(Class::Change)
                    || self.project_node(txn, authoring_node)?.is_some()
                {
                    visible = true;
                    break;
                }
            }
            visible
        } else {
            // Vault and session resources have no VCS view membership.
            true
        };
        Ok(visible.then_some(node))
    }

    pub fn search_content(
        &self,
        pattern: &str,
        opts: ContentSearchOptions,
    ) -> Result<ContentSearchResult, RepositoryError> {
        let plan = self.content_plan(pattern, opts).map_err(|error| {
            RepositoryError::InvalidOperation {
                message: format!("code search failed: {error}"),
            }
        })?;
        self.search_content_plan(plan)
    }

    pub fn content_plan(
        &self,
        pattern: &str,
        opts: ContentSearchOptions,
    ) -> Result<RecordedContentSearch, crate::content_search::ContentSearchError> {
        RecordedContentSearch::new(self.files.keys().map(String::as_str), pattern, opts)
    }

    pub fn search_content_plan(
        &self,
        plan: RecordedContentSearch,
    ) -> Result<ContentSearchResult, RepositoryError> {
        plan.run(|path| {
            let (inode, position) = self.files[path];
            let rendered = super::content::retrieve_content_bounded(
                &self.txn,
                &self.repo.change_store,
                inode,
                position,
                atomic_core::output::alive::RetrieveOptions::new()
                    .with_graph_visibility(self.visibility.clone()),
                RECORDED_CONTENT_LIMIT,
            )
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
            let (bytes, retained_bytes) = match rendered {
                super::content::BoundedContent::Complete(bytes) => {
                    let retained = bytes.len();
                    (Some(bytes), retained)
                }
                super::content::BoundedContent::Oversized { retained_bytes } => {
                    (None, retained_bytes)
                }
            };
            #[cfg(any(test, feature = "query-test-instrumentation"))]
            crate::query_diagnostics::retrieved(retained_bytes);
            #[cfg(not(any(test, feature = "query-test-instrumentation")))]
            let _ = retained_bytes;
            Ok(bytes)
        })
    }
}

impl Repository {
    pub(super) fn query_view(&self, view_name: &str) -> Result<QueryView<'_>, RepositoryError> {
        let txn = self.pristine.read_txn()?;
        let view = txn
            .get_view(view_name)?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.to_string(),
            })?;
        let visibility = graph_visibility_closure(&txn, &view)?;
        let projection = self.project_tree_for_visibility(&txn, &visibility)?;
        if !projection.name_conflicts.is_empty() {
            return Err(RepositoryError::InvalidOperation {
                message: "cannot query a view with unresolved path claims".to_string(),
            });
        }
        let mut changes = HashSet::new();
        for id in visibility
            .iter_dependency_first()
            .filter(|id| !id.is_root())
        {
            let hash = txn.get_external(*id)?.ok_or_else(|| {
                RepositoryError::Database(format!("visible change {id} has no hash"))
            })?;
            changes.insert(iri::mint_change(&hash));
        }
        let project = self.project_id()?;
        let views = txn
            .resolve_full_view_chain(&view)?
            .into_iter()
            .map(|view| iri::mint_view(&project, &view.name))
            .collect();
        let rules = IgnoreRules::load_for_enrichment(self.root());
        let mut files = BTreeMap::new();
        for (path, item) in projection.present {
            if item.is_directory
                || crate::ignore::is_enrichment_internal(Path::new(&path))
                || rules.is_ignored(Path::new(&path), false)
            {
                continue;
            }
            let attrs = atomic_core::output::project_inode_attributes(
                &txn,
                item.position,
                visibility.attribute_visibility(),
            )
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
            if attrs.is_conflicted() {
                return Err(RepositoryError::InvalidOperation {
                    message: format!("cannot query '{path}' with conflicting inode attributes"),
                });
            }
            // Never follow a symlink into working-copy or external content.
            if attrs.materialization.kind != atomic_core::change::InodeKind::Regular {
                continue;
            }
            files.insert(path, (item.inode, item.position));
        }
        let nodes = files
            .keys()
            .map(|path| {
                let id = iri::mint_file(&project, path);
                let label = path.rsplit('/').next().unwrap_or(path);
                let node = KgNode::new(&id, Class::File, label, "vcs")
                    .with_metadata(serde_json::json!({"path": path}));
                (id, node)
            })
            .collect();
        Ok(QueryView {
            repo: self,
            txn,
            visibility,
            files,
            changes,
            views,
            nodes,
        })
    }

    /// Search recorded source bytes through a view's validated ancestor and
    /// dependency closure. Unrecorded files/edits and the working-copy content
    /// index are excluded; no index rebuild or checkout occurs.
    /// Existing ambient ignore rules still apply. Totals/facets are exact even
    /// with zero displayed matches; rendered files over 10 MiB are skipped.
    pub fn search_content_on_view(
        &self,
        view_name: &str,
        pattern: &str,
        opts: ContentSearchOptions,
    ) -> Result<ContentSearchResult, RepositoryError> {
        self.search_content_paths_on_view(view_name, pattern, opts, &[])
    }

    /// Like [`Self::search_content_on_view`], restricted to a union of exact
    /// repository-relative paths and directory prefixes before matching.
    /// Path/type filters precede content I/O; closure/path/attribute validation
    /// remains global. Content errors outside the selected files are not read.
    pub fn search_content_paths_on_view(
        &self,
        view_name: &str,
        pattern: &str,
        opts: ContentSearchOptions,
        paths: &[String],
    ) -> Result<ContentSearchResult, RepositoryError> {
        let mut scope = self.query_view(view_name)?;
        if !paths.is_empty() {
            scope.files.retain(|path, _| {
                paths.iter().any(|prefix| {
                    let prefix = prefix.trim_end_matches('/');
                    path == prefix
                        || path
                            .strip_prefix(prefix)
                            .is_some_and(|rest| rest.starts_with('/'))
                })
            });
        }
        scope.search_content(pattern, opts)
    }

    /// Resolve callers from the selected view's recorded source using the same
    /// request-scoped parser as working-copy caller queries. No KG code edges
    /// are created or required. Accepts `<path>#<symbol>` or a bare symbol.
    pub fn query_callers_on_view(
        &self,
        view_name: &str,
        target: &str,
    ) -> Result<Vec<Caller>, RepositoryError> {
        let scope = self.query_view(view_name)?;
        let mut sources = Vec::new();
        for (path, (inode, position)) in &scope.files {
            let bytes = super::content::retrieve_content_with_filter_fast(
                &scope.txn,
                &self.change_store,
                *inode,
                *position,
                atomic_core::output::alive::RetrieveOptions::new()
                    .with_graph_visibility(scope.visibility.clone()),
            )
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
            if let Ok(source) = String::from_utf8(bytes) {
                sources.push((path.clone(), source));
            }
        }
        Ok(SyntaxIndex::from_sources(&sources).callers(target))
    }
}
