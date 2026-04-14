//! VCS → KG enrichment pipeline.
//!
//! Populates the knowledge graph from repository data:
//! - Changes → `change:{hash}` nodes + `AUTHORED_BY`, `MODIFIES` edges
//! - Files → `file:{path}` nodes
//! - Views → `view:{name}` nodes + `CHILD_OF`, `ON_VIEW` edges
//! - Dependencies → `DEPENDS_ON` edges between change nodes

use super::*;
use atomic_core::pristine::ontology::edge_kind;
use atomic_core::pristine::vault::{KgEdge, KgNode};
use atomic_core::pristine::KgMutTxnT;
use atomic_core::types::Base32;

impl Repository {
    /// Enrich the KG from all VCS data (views, changes, files, deps).
    ///
    /// This is the main entry point for populating the KG from the
    /// repository's version control data. Call this after `init_kg()`.
    ///
    /// Returns statistics about the number of nodes and edges created.
    pub fn kg_enrich_from_vcs(&self) -> Result<KgEnrichStats, RepositoryError> {
        let mut stats = KgEnrichStats::default();

        // Phase 1: Views → nodes + parent edges
        stats.views = self.kg_enrich_views()?;

        // Phase 2: Tracked files → nodes
        stats.files = self.kg_enrich_files()?;

        // Phase 3: Changes → nodes + edges (modifies, authored_by, on_view, depends_on)
        stats.changes = self.kg_enrich_changes()?;

        // Phase 4: AST entities → nodes + DEFINES edges
        #[cfg(feature = "ast")]
        {
            stats.entities = self.kg_enrich_entities()?;
        }

        Ok(stats)
    }

    /// Enrich the KG with view hierarchy data.
    ///
    /// For each view in the repository, creates a `view:{name}` node and
    /// a `CHILD_OF` edge to its parent view (if any).
    fn kg_enrich_views(&self) -> Result<usize, RepositoryError> {
        let view_names = self
            .list_views()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let mut count = 0;
        for name in &view_names {
            // Resolve full ViewInfo for this view. If resolution fails
            // (shouldn't happen — the name just came from list_views),
            // skip rather than abort the entire enrichment.
            let info = match self.get_view_info(name) {
                Ok(info) => info,
                Err(e) => {
                    log::warn!("kg_enrich_views: skipping view '{}': {}", name, e);
                    continue;
                }
            };

            let node = KgNode::new(format!("view:{}", info.name), "view", &info.name, "vcs")
                .with_summary(format!(
                    "{} view, {} changes",
                    info.kind_label(),
                    info.change_count
                ))
                .with_metadata(serde_json::json!({
                    "scope": info.kind_label(),
                    "change_count": info.change_count,
                    "state": info.state_short(),
                }));

            txn.upsert_kg_node(&node)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            count += 1;

            // Parent edge
            if let Some(ref parent_name) = info.parent_name {
                let edge = KgEdge::new(
                    format!("view:{}", info.name),
                    format!("view:{}", parent_name),
                    edge_kind::CHILD_OF,
                );
                txn.upsert_kg_edge(&edge)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
            }
        }

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(count)
    }

    /// Enrich the KG with tracked file nodes.
    ///
    /// Creates a `file:{path}` node for each file tracked in the repository.
    fn kg_enrich_files(&self) -> Result<usize, RepositoryError> {
        let files = self
            .list_tracked_files()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let mut count = 0;
        for file in &files {
            let path = file.path.to_string_lossy();
            let label = path.rsplit('/').next().unwrap_or(&path);

            let node = KgNode::new(format!("file:{}", path), "file", label, "vcs");

            txn.upsert_kg_node(&node)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            count += 1;
        }

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(count)
    }

    /// Enrich the KG with change data from the current view's history.
    ///
    /// For each change:
    /// - Creates a `change:{short_hash}` node with message and timestamp
    /// - Creates `AUTHORED_BY` edges to author identity nodes
    /// - Creates `ON_VIEW` edge to the current view
    /// - Creates `DEPENDS_ON` edges to dependency changes
    /// - Creates `MODIFIES` edges to files (via `FileOps` in the change)
    fn kg_enrich_changes(&self) -> Result<usize, RepositoryError> {
        use crate::history::HistoryOptions;

        let history = self
            .log(HistoryOptions::with_headers())
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        if history.is_empty() {
            return Ok(0);
        }

        let view_name = self.current_view().to_string();

        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let mut count = 0;
        for entry in &history {
            let hash_str = entry.hash.to_base32();
            let short_hash = truncate_hash(&hash_str);

            let mut node = KgNode::new(
                format!("change:{}", short_hash),
                "change",
                short_hash,
                "vcs",
            );

            // Add header info if available
            if let Some(ref header) = entry.header {
                node = node.with_summary(&header.message);
                node = node.with_metadata(serde_json::json!({
                    "sequence": entry.sequence,
                    "timestamp": header.timestamp.to_rfc3339(),
                    "full_hash": hash_str,
                }));

                // Author edges
                for author in &header.authors {
                    let author_id = format!("identity:{}", author.name);
                    // Ensure identity node exists
                    let identity_node = KgNode::new(&author_id, "identity", &author.name, "vcs");
                    txn.upsert_kg_node(&identity_node)
                        .map_err(|e| RepositoryError::Database(e.to_string()))?;

                    let edge = KgEdge::new(
                        format!("change:{}", short_hash),
                        &author_id,
                        edge_kind::AUTHORED_BY,
                    );
                    txn.upsert_kg_edge(&edge)
                        .map_err(|e| RepositoryError::Database(e.to_string()))?;
                }
            } else {
                node = node.with_metadata(serde_json::json!({
                    "sequence": entry.sequence,
                    "full_hash": hash_str,
                }));
            }

            txn.upsert_kg_node(&node)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;

            // ON_VIEW edge
            let view_edge = KgEdge::new(
                format!("change:{}", short_hash),
                format!("view:{}", view_name),
                edge_kind::ON_VIEW,
            );
            txn.upsert_kg_edge(&view_edge)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;

            count += 1;
        }

        // Dependencies — load each change and extract from hashed.dependencies
        for entry in &history {
            let hash_str = entry.hash.to_base32();
            let short_hash = truncate_hash(&hash_str);

            // Try to load the change to get dependency hashes directly
            match self.load_change(&entry.hash) {
                Ok(change) => {
                    for dep_hash in change.dependencies() {
                        let dep_hash_str = dep_hash.to_base32();
                        let dep_short = truncate_hash(&dep_hash_str);

                        let dep_edge = KgEdge::new(
                            format!("change:{}", short_hash),
                            format!("change:{}", dep_short),
                            edge_kind::DEPENDS_ON,
                        );
                        txn.upsert_kg_edge(&dep_edge)
                            .map_err(|e| RepositoryError::Database(e.to_string()))?;
                    }

                    // File modification edges from FileOps
                    for file_op in change.file_ops() {
                        let path = file_op.path();
                        if path.is_empty() {
                            continue;
                        }
                        let file_label = path.rsplit('/').next().unwrap_or(path);
                        let file_node =
                            KgNode::new(format!("file:{}", path), "file", file_label, "vcs");
                        txn.upsert_kg_node(&file_node)
                            .map_err(|e| RepositoryError::Database(e.to_string()))?;

                        let mod_edge = KgEdge::new(
                            format!("change:{}", short_hash),
                            format!("file:{}", path),
                            edge_kind::MODIFIES,
                        );
                        txn.upsert_kg_edge(&mod_edge)
                            .map_err(|e| RepositoryError::Database(e.to_string()))?;
                    }
                }
                Err(e) => {
                    log::warn!(
                        "kg_enrich_changes: could not load change {}: {}",
                        short_hash,
                        e
                    );
                }
            }
        }

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(count)
    }

    /// Enrich the KG for a single newly-recorded change.
    ///
    /// Call this after `record()` to immediately add the new change
    /// to the knowledge graph. Lighter than `kg_enrich_from_vcs()`.
    pub fn kg_enrich_change(&self, hash: &Hash) -> Result<(), RepositoryError> {
        let hash_str = hash.to_base32();
        let short_hash = truncate_hash(&hash_str);

        let change = self.load_change(hash)?;
        let header = &change.hashed.header;

        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Create change node
        let node = KgNode::new(
            format!("change:{}", short_hash),
            "change",
            short_hash,
            "vcs",
        )
        .with_summary(&header.message)
        .with_metadata(serde_json::json!({
            "timestamp": header.timestamp.to_rfc3339(),
            "full_hash": hash_str,
        }));

        txn.upsert_kg_node(&node)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Author edges
        for author in &header.authors {
            let author_id = format!("identity:{}", author.name);
            let identity_node = KgNode::new(&author_id, "identity", &author.name, "vcs");
            txn.upsert_kg_node(&identity_node)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;

            txn.upsert_kg_edge(&KgEdge::new(
                format!("change:{}", short_hash),
                &author_id,
                edge_kind::AUTHORED_BY,
            ))
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }

        // ON_VIEW edge to current view
        let view_name = self.current_view().to_string();
        txn.upsert_kg_edge(&KgEdge::new(
            format!("change:{}", short_hash),
            format!("view:{}", view_name),
            edge_kind::ON_VIEW,
        ))
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Dependency edges
        for dep_hash in change.dependencies() {
            let dep_hash_str = dep_hash.to_base32();
            let dep_short = truncate_hash(&dep_hash_str);

            txn.upsert_kg_edge(&KgEdge::new(
                format!("change:{}", short_hash),
                format!("change:{}", dep_short),
                edge_kind::DEPENDS_ON,
            ))
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }

        // File modification edges from FileOps
        let mut file_paths = Vec::new();
        for file_op in change.file_ops() {
            let path = file_op.path();
            if path.is_empty() {
                continue;
            }
            file_paths.push(path.to_string());
            let file_label = path.rsplit('/').next().unwrap_or(path);
            let file_node = KgNode::new(format!("file:{}", path), "file", file_label, "vcs");
            txn.upsert_kg_node(&file_node)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;

            txn.upsert_kg_edge(&KgEdge::new(
                format!("change:{}", short_hash),
                format!("file:{}", path),
                edge_kind::MODIFIES,
            ))
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }

        // AST entity extraction for changed files
        #[cfg(feature = "ast")]
        {
            let mut parser_registry = atomic_semantic::ParserRegistry::new();

            for path in &file_paths {
                // Only parse supported language files
                if !atomic_semantic::is_supported(path) {
                    continue;
                }

                // Read the file content from the working copy
                let abs_path = self.root().join(path);
                let source = match std::fs::read_to_string(&abs_path) {
                    Ok(s) => s,
                    Err(e) => {
                        log::debug!("Can't read {} for AST extraction: {}", path, e);
                        continue;
                    }
                };

                // Extract entities
                let entities = parser_registry.extract(path, &source);

                for entity in &entities {
                    let entity_id =
                        format!("entity:{}:{}:{}", entity.file, entity.name, entity.line);

                    let mut node = KgNode::new(&entity_id, "entity", &entity.name, "ast")
                        .with_metadata(serde_json::json!({
                            "kind": entity.kind.as_str(),
                            "file": entity.file,
                            "line": entity.line,
                            "end_line": entity.end_line,
                            "exported": entity.exported,
                        }));

                    if let Some(ref sig) = entity.signature {
                        node = node.with_summary(sig);
                    }

                    txn.upsert_kg_node(&node)
                        .map_err(|e| RepositoryError::Database(e.to_string()))?;

                    // DEFINES edge: file → entity
                    txn.upsert_kg_edge(&KgEdge::new(
                        format!("file:{}", entity.file),
                        &entity_id,
                        edge_kind::DEFINES,
                    ))
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;

                    // Change MODIFIES entity (more precise than file-level)
                    txn.upsert_kg_edge(&KgEdge::new(
                        format!("change:{}", short_hash),
                        &entity_id,
                        edge_kind::MODIFIES,
                    ))
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
                }
            }
        }

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(())
    }

    /// Enrich the KG with AST entities from tracked files.
    ///
    /// Parses all supported source files in the working copy and creates
    /// `entity:{file}:{name}:{line}` nodes with `DEFINES` edges from
    /// the corresponding file nodes. Runs during bulk enrichment
    /// (`atomic vault query enrich`, `atomic git import`).
    #[cfg(feature = "ast")]
    fn kg_enrich_entities(&self) -> Result<usize, RepositoryError> {
        let files = self
            .list_tracked_files()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let mut parser_registry = atomic_semantic::ParserRegistry::new();
        let mut count = 0;

        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        for file in &files {
            if file.is_directory {
                continue;
            }

            let path_str = file.path.to_string_lossy().to_string();

            if !atomic_semantic::is_supported(&path_str) {
                continue;
            }

            let abs_path = self.root().join(&file.path);
            let source = match std::fs::read_to_string(&abs_path) {
                Ok(s) => s,
                Err(_) => continue,
            };

            let entities = parser_registry.extract(&path_str, &source);

            for entity in &entities {
                let entity_id = format!("entity:{}:{}:{}", entity.file, entity.name, entity.line);

                let mut node = KgNode::new(&entity_id, "entity", &entity.name, "ast")
                    .with_metadata(serde_json::json!({
                        "kind": entity.kind.as_str(),
                        "file": entity.file,
                        "line": entity.line,
                        "end_line": entity.end_line,
                        "exported": entity.exported,
                    }));

                if let Some(ref sig) = entity.signature {
                    node = node.with_summary(sig);
                }

                txn.upsert_kg_node(&node)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;

                txn.upsert_kg_edge(&KgEdge::new(
                    format!("file:{}", entity.file),
                    &entity_id,
                    edge_kind::DEFINES,
                ))
                .map_err(|e| RepositoryError::Database(e.to_string()))?;

                count += 1;
            }
        }

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(count)
    }
}

/// Statistics from a VCS enrichment run.
#[derive(Debug, Clone, Default)]
pub struct KgEnrichStats {
    /// Number of view nodes created.
    pub views: usize,
    /// Number of file nodes created.
    pub files: usize,
    /// Number of change nodes created.
    pub changes: usize,
    /// Number of AST entity nodes created.
    pub entities: usize,
}

impl KgEnrichStats {
    /// Total number of nodes created across all phases.
    pub fn total(&self) -> usize {
        self.views + self.files + self.changes + self.entities
    }
}

impl std::fmt::Display for KgEnrichStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} views, {} files, {} changes, {} entities",
            self.views, self.files, self.changes, self.entities
        )
    }
}

/// Truncate a base32 hash string to a short prefix for use as an identifier.
fn truncate_hash(hash_str: &str) -> &str {
    if hash_str.len() > 12 {
        &hash_str[..12]
    } else {
        hash_str
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kg_enrich_stats_display() {
        let stats = KgEnrichStats {
            views: 3,
            files: 10,
            changes: 5,
            entities: 8,
        };
        assert_eq!(
            stats.to_string(),
            "3 views, 10 files, 5 changes, 8 entities"
        );
    }

    #[test]
    fn test_kg_enrich_stats_total() {
        let stats = KgEnrichStats {
            views: 2,
            files: 7,
            changes: 3,
            entities: 4,
        };
        assert_eq!(stats.total(), 16);
    }

    #[test]
    fn test_kg_enrich_stats_default() {
        let stats = KgEnrichStats::default();
        assert_eq!(stats.views, 0);
        assert_eq!(stats.files, 0);
        assert_eq!(stats.changes, 0);
        assert_eq!(stats.entities, 0);
        assert_eq!(stats.total(), 0);
        assert_eq!(stats.to_string(), "0 views, 0 files, 0 changes, 0 entities");
    }

    #[test]
    fn test_truncate_hash() {
        assert_eq!(truncate_hash("abcdefghijklmnop"), "abcdefghijkl");
        assert_eq!(truncate_hash("short"), "short");
        assert_eq!(truncate_hash("exactly12chr"), "exactly12chr");
        assert_eq!(truncate_hash(""), "");
    }

    #[test]
    fn test_kg_enrich_from_vcs_empty_repo() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_kg().unwrap();

        let stats = repo.kg_enrich_from_vcs().unwrap();
        // Should have at least the default view (e.g. "dev")
        assert!(
            stats.views >= 1,
            "expected at least 1 view, got {}",
            stats.views
        );
        assert_eq!(stats.files, 0);
        assert_eq!(stats.changes, 0);
    }

    #[test]
    fn test_kg_enrich_views_creates_nodes() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_kg().unwrap();

        let count = repo.kg_enrich_views().unwrap();
        assert!(count >= 1);

        // Verify the default view node exists
        let view_name = repo.current_view().to_string();
        let node = repo.vault_kg_node(&format!("view:{}", view_name)).unwrap();
        assert!(node.is_some(), "expected view node to exist");

        let node = node.unwrap();
        assert_eq!(node.kind, "view");
        assert_eq!(node.source, "vcs");
    }
}
