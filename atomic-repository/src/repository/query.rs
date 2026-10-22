//! Read-only queries over a canonical view projection, not ambient KG/CRDT
//! state or whichever bytes happen to be materialized in the working copy.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::OnceLock;

use atomic_core::pristine::vault::{KgNode, KgSubgraph};
use atomic_core::pristine::KgTxnT;

use super::*;
use crate::content_search::{search_file_contents, ContentSearchOptions, ContentSearchResult};

pub(super) struct QueryView {
    pub files: BTreeMap<String, Vec<u8>>,
    changes: HashSet<String>,
    views: HashSet<String>,
    nodes: OnceLock<HashMap<String, KgNode>>,
}

impl QueryView {
    pub fn nodes(&self) -> &HashMap<String, KgNode> {
        self.nodes.get_or_init(|| {
            let mut registry = atomic_semantic::ParserRegistry::new();
            let mut nodes = HashMap::new();
            for (path, bytes) in &self.files {
                let label = path.rsplit('/').next().unwrap_or(path);
                let id = format!("file:{path}");
                nodes.insert(id.clone(), KgNode::new(id, "file", label, "vcs"));
                if let Some((dir, _)) = path.rsplit_once('/') {
                    let id = format!("module:{dir}");
                    let label = dir.rsplit('/').next().unwrap_or(dir);
                    nodes.entry(id.clone()).or_insert_with(|| {
                        KgNode::new(id, "module", label, "vcs").with_summary(dir)
                    });
                }
                let Ok(source) = std::str::from_utf8(bytes) else {
                    continue;
                };
                for entity in registry.extract(path, source) {
                    let id = format!("entity:{}:{}:{}", entity.file, entity.name, entity.line);
                    let mut node = KgNode::new(&id, "entity", &entity.name, "ast").with_metadata(
                        serde_json::json!({
                            "kind": entity.kind.as_str(),
                            "file": entity.file,
                            "line": entity.line,
                            "end_line": entity.end_line,
                            "exported": entity.exported,
                        }),
                    );
                    node.summary = entity.signature;
                    nodes.insert(id, node);
                }
            }
            nodes
        })
    }

    /// Project ambient index candidates before scoring them. Entity metadata
    /// must be replaced, not merely path-filtered: identical path/name/line IDs
    /// can describe different signatures in sibling views.
    pub fn project_node<T: KgTxnT>(
        &self,
        txn: &T,
        node: KgNode,
    ) -> Result<Option<KgNode>, RepositoryError> {
        let visible = match node.kind.as_str() {
            "entity" | "file" | "module" => return Ok(self.nodes().get(&node.id).cloned()),
            "change" => node
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("full_hash"))
                .and_then(serde_json::Value::as_str)
                .is_some_and(|hash| self.changes.contains(hash)),
            "view" => node
                .id
                .strip_prefix("view:")
                .is_some_and(|name| self.views.contains(name)),
            "identity" if node.source == "vcs" => {
                let edges = txn
                    .get_kg_edges_to(&node.id)
                    .map_err(|error| RepositoryError::Database(error.to_string()))?;
                let mut visible = false;
                for edge in edges.iter().filter(|edge| edge.kind == "AUTHORED_BY") {
                    let Some(authoring_node) = txn
                        .get_kg_node(&edge.from_id)
                        .map_err(|error| RepositoryError::Database(error.to_string()))?
                    else {
                        continue;
                    };
                    // Identities shared with vault records stay shared; VCS-only
                    // identities require a visible authored change.
                    if authoring_node.kind != "change"
                        || self.project_node(txn, authoring_node)?.is_some()
                    {
                        visible = true;
                        break;
                    }
                }
                visible
            }
            // Vault/session records have no VCS view membership.
            _ => true,
        };
        Ok(visible.then_some(node))
    }

    pub fn search_content(
        &self,
        pattern: &str,
        opts: ContentSearchOptions,
    ) -> Result<ContentSearchResult, RepositoryError> {
        search_file_contents(&self.files, pattern, opts).map_err(|error| {
            RepositoryError::InvalidOperation {
                message: format!("code search failed: {error}"),
            }
        })
    }
}

impl Repository {
    pub(super) fn query_view(&self, view_name: &str) -> Result<QueryView, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        let view = txn
            .get_view(view_name)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
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
            let hash = txn
                .get_external(*id)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| {
                    RepositoryError::Database(format!("visible change {id} has no hash"))
                })?;
            changes.insert(hash.to_base32());
        }
        let views = txn
            .resolve_full_view_chain(&view)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .into_iter()
            .map(|view| view.name)
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
            let bytes = super::content::retrieve_content_with_filter_fast(
                &txn,
                &self.change_store,
                item.inode,
                item.position,
                atomic_core::output::alive::RetrieveOptions::new()
                    .with_graph_visibility(visibility.clone()),
            )
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
            files.insert(path, bytes);
        }
        Ok(QueryView {
            files,
            changes,
            views,
            nodes: OnceLock::new(),
        })
    }

    /// Search recorded source bytes through a view's validated ancestor and
    /// dependency closure. Unrecorded files/edits and the working-copy content
    /// index are deliberately excluded; no index rebuild or checkout occurs.
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

    /// Resolve calls from the requested view's recorded source. Ambient CALLS
    /// edges have no version identity, so filtering their endpoints alone
    /// cannot isolate sibling edits to the same caller function.
    pub fn vault_kg_callers_on_view(
        &self,
        view_name: &str,
        entity_id: &str,
    ) -> Result<KgSubgraph, RepositoryError> {
        let scope = self.query_view(view_name)?;
        if !scope
            .nodes()
            .get(entity_id)
            .is_some_and(|node| node.kind == "entity")
        {
            return Ok(KgSubgraph::default());
        }
        let edges: Vec<_> =
            Self::calls_from_sources(scope.files.iter().filter_map(|(path, bytes)| {
                std::str::from_utf8(bytes)
                    .ok()
                    .map(|source| (path.as_str(), source))
            }))
            .into_iter()
            .filter(|edge| edge.to_id == entity_id || edge.from_id == entity_id)
            .collect();
        let ids: HashSet<_> = edges
            .iter()
            .flat_map(|edge| [&edge.from_id, &edge.to_id])
            .collect();
        let mut nodes: Vec<_> = ids
            .into_iter()
            .filter_map(|id| scope.nodes().get(id).cloned())
            .collect();
        nodes.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(KgSubgraph { nodes, edges })
    }
}
