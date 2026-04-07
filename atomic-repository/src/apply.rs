//! Change insertion for Atomic VCS
//!
//! This module provides high-level functions for inserting changes into the
//! repository graph. Insertion is the process of taking a recorded change
//! and modifying the repository's internal graph to reflect its contents.
//!
//! # Overview
//!
//! Inserting a change involves several steps:
//!
//! 1. **Load**: Read the change from the change store
//! 2. **Validate**: Verify dependencies are present and change isn't already applied
//! 3. **Register**: Get an internal ID for the change
//! 4. **Apply Atoms**: Process each graph_op's atoms (vertices and edges)
//! 5. **Update View**: Add to change log and update Merkle state
//! 6. **Handle Conflicts**: Track zombies and missing contexts
//!
//! # Cross-View Operations
//!
//! Atomic supports inserting changes between views, enabling:
//!
//! - **Cherry-picking**: Insert specific changes from one view to another
//! - **View merging**: Insert all changes from one view to another
//! - **Tag-based insert**: Insert changes up to a tagged state
//!
//! ```rust,ignore
//! // Insert changes from feature view to main view
//! let options = CrossViewInsertOptions::new("feature", "main");
//! let result = repo.insert_from_view(options)?;
//!
//! // Insert changes up to a tag
//! let options = CrossViewInsertOptions::new("feature", "main")
//!     .up_to_tag("v1.0.0");
//! let result = repo.insert_from_view(options)?;
//! ```
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                      Change Insertion Flow                              │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  Change Store           Pristine              Graph                     │
//! │  ┌──────────┐         ┌───────────┐        ┌─────────────────┐         │
//! │  │ load_    │ verify  │ register  │ insert │  Add Vertices   │         │
//! │  │ change() │ ──────▶ │ _change() │──────▶ │  Update Edges   │         │
//! │  └──────────┘         └───────────┘        └─────────────────┘         │
//! │       │                    │                       │                   │
//! │       │                    ▼                       ▼                   │
//! │       │            ┌───────────────┐       ┌─────────────────┐         │
//! │       │            │   NodeId      │       │  Updated View   │         │
//! │       └───────────▶│   Assigned    │       │  New State      │         │
//! │                    └───────────────┘       └─────────────────┘         │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Dependency Resolution
//!
//! Changes in Atomic have explicit dependencies. Before a change can be
//! inserted, all its dependencies must already be present in the repository.
//! This module provides functions to:
//!
//! - Check if all dependencies are present
//! - Insert dependencies recursively if available
//! - Report missing dependencies for the user to resolve
//!
//! # Conflict Handling
//!
//! During insertion, conflicts can arise:
//!
//! - **Zombie vertices**: Content deleted by one change but modified by another
//! - **Missing context**: Context vertices that don't exist
//! - **Order conflicts**: Ambiguous insertion order
//!
//! These are tracked and can be reported to the user for resolution.
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_repository::{Repository, InsertOptions};
//!
//! let repo = Repository::open(".")?;
//!
//! // Insert a single change
//! let result = repo.apply_change(&hash, InsertOptions::default())?;
//! println!("Inserted change, new state: {}", result.new_state);
//!
//! // Insert with dependencies
//! let result = repo.apply_change_with_deps(&hash, InsertOptions::default())?;
//! if result.has_conflicts {
//!     println!("Warning: conflicts detected");
//! }
//! ```

use atomic_core::apply::{
    apply_file_ops, compute_new_state, validate_can_apply, verify_dependencies, write_edge_map,
    write_new_vertex, ConflictSummary, ConflictTracker, LocalApplyError, MissingContextConflict,
    Workspace, ZombieConflict,
};
use atomic_core::change::{Atom, AtomRef, Change, GraphOp};
use atomic_core::pristine::{GraphTxnT, MutTxnT, ViewState, ViewTxnT};
use atomic_core::types::{Base32, Hash, Merkle, NodeId};
use std::collections::{HashMap, HashSet, VecDeque};
use thiserror::Error;

// Error Types

/// Result type for insert operations.
pub type InsertResult<T> = Result<T, InsertError>;

/// Errors that can occur during change insertion.
#[derive(Debug, Error)]
pub enum InsertError {
    /// The change was not found in the change store.
    #[error("Change not found: {hash}")]
    ChangeNotFound {
        /// The hash of the missing change
        hash: String,
    },

    /// One or more dependencies are missing.
    #[error("Missing dependencies: {}", format_hashes(.missing))]
    MissingDependencies {
        /// The hashes of missing dependencies
        missing: Vec<Hash>,
    },

    /// The change is already applied to the view.
    #[error("Change already applied: {hash}")]
    AlreadyApplied {
        /// The hash of the already-applied change
        hash: String,
    },

    /// A conflict was detected during insertion.
    #[error("Conflict during insertion: {message}")]
    Conflict {
        /// Description of the conflict
        message: String,
    },

    /// The change file is corrupted or invalid.
    #[error("Invalid change: {message}")]
    InvalidChange {
        /// Description of the problem
        message: String,
    },

    /// A cyclic dependency was detected.
    #[error("Cyclic dependency detected: {message}")]
    CyclicDependency {
        /// Description of the cycle
        message: String,
    },

    /// An internal error occurred.
    #[error("Internal error: {0}")]
    Internal(String),

    /// Database error.
    #[error("Database error: {0}")]
    Database(String),

    /// IO error.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<LocalApplyError> for InsertError {
    fn from(e: LocalApplyError) -> Self {
        match e {
            LocalApplyError::DependencyMissing { hash } => InsertError::MissingDependencies {
                missing: vec![hash],
            },
            LocalApplyError::ChangeAlreadyApplied { hash } => InsertError::AlreadyApplied {
                hash: hash.to_base32(),
            },
            LocalApplyError::CyclicDependency { message } => {
                InsertError::CyclicDependency { message }
            }
            LocalApplyError::InvalidChange => InsertError::InvalidChange {
                message: "Change format is invalid".to_string(),
            },
            LocalApplyError::Corruption => InsertError::InvalidChange {
                message: "Change data is corrupted".to_string(),
            },
            other => InsertError::Internal(other.to_string()),
        }
    }
}

/// Format a list of hashes for display.
fn format_hashes(hashes: &[Hash]) -> String {
    hashes
        .iter()
        .map(|h| h.to_base32())
        .collect::<Vec<_>>()
        .join(", ")
}

// InsertOptions

/// Options for controlling change insertion.
#[derive(Debug, Clone)]
pub struct InsertOptions {
    /// View to insert to (None = current view).
    ///
    /// When `None`, the change is inserted into the repository's current view.
    /// Default: `None`
    pub view: Option<String>,

    /// Automatically insert missing dependencies if available.
    ///
    /// When `true`, if a change's dependencies are missing but available
    /// in the change store, they will be inserted first.
    /// Default: `false`
    pub apply_dependencies: bool,

    /// Allow inserting even if conflicts are detected.
    ///
    /// When `false`, insertion stops on the first conflict.
    /// When `true`, conflicts are tracked but insertion continues.
    /// Default: `true`
    pub allow_conflicts: bool,

    /// Maximum depth for recursive dependency resolution.
    ///
    /// This prevents infinite loops with cyclic dependencies.
    /// Default: `100`
    pub max_depth: usize,

    /// Record detailed conflict information.
    ///
    /// When `true`, detailed conflict tracking is performed.
    /// Default: `true`
    pub track_conflicts: bool,

    /// Skip the "already applied" validation check.
    ///
    /// When `true`, `validate_can_apply` is bypassed.  This is used by
    /// `rebuild_change_graph` which intentionally re-applies a change
    /// that is already in the view log (same NodeId, new hunks).
    /// Default: `false`
    pub skip_validation: bool,
}

impl Default for InsertOptions {
    fn default() -> Self {
        Self {
            view: None,
            apply_dependencies: false,
            allow_conflicts: true,
            max_depth: 100,
            track_conflicts: true,
            skip_validation: false,
        }
    }
}

impl InsertOptions {
    /// Set the view to insert to.
    pub fn view(mut self, name: impl Into<String>) -> Self {
        self.view = Some(name.into());
        self
    }

    /// Create options that will insert dependencies automatically.
    pub fn with_dependencies() -> Self {
        Self {
            apply_dependencies: true,
            ..Default::default()
        }
    }

    /// Create options that stop on conflicts.
    pub fn strict() -> Self {
        Self {
            allow_conflicts: false,
            ..Default::default()
        }
    }

    /// Set whether to insert dependencies automatically.
    pub fn apply_deps(mut self, apply: bool) -> Self {
        self.apply_dependencies = apply;
        self
    }

    /// Set whether to allow conflicts.
    pub fn allow_conflict(mut self, allow: bool) -> Self {
        self.allow_conflicts = allow;
        self
    }

    /// Skip the "already applied" validation check.
    ///
    /// Used by `rebuild_change_graph` which re-applies a change that is
    /// already in the view log (same NodeId, new hunks after revise).
    pub fn skip_validation(mut self, skip: bool) -> Self {
        self.skip_validation = skip;
        self
    }

    /// Set maximum recursion depth.
    pub fn max_recursion(mut self, depth: usize) -> Self {
        self.max_depth = depth;
        self
    }
}

// InsertStats

/// Statistics from a change insertion operation.
#[derive(Debug, Clone, Default)]
pub struct InsertStats {
    /// Number of changes inserted.
    pub changes_applied: usize,

    /// Number of atoms (vertices + edges) processed.
    pub atoms_processed: usize,

    /// Number of conflicts detected.
    pub conflicts_detected: usize,

    /// Number of dependencies that were automatically inserted.
    pub dependencies_applied: usize,

    /// Hashes of changes that were inserted.
    pub applied_hashes: Vec<Hash>,

    /// Conflict summary if any conflicts were detected.
    pub conflict_summary: Option<ConflictSummary>,
}

impl InsertStats {
    /// Create empty stats.
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if any changes were inserted.
    pub fn has_applied(&self) -> bool {
        self.changes_applied > 0
    }

    /// Check if any conflicts were detected.
    pub fn has_conflicts(&self) -> bool {
        self.conflicts_detected > 0
    }

    /// Merge stats from another operation.
    pub fn merge(&mut self, other: InsertStats) {
        self.changes_applied += other.changes_applied;
        self.atoms_processed += other.atoms_processed;
        self.conflicts_detected += other.conflicts_detected;
        self.dependencies_applied += other.dependencies_applied;
        self.applied_hashes.extend(other.applied_hashes);
        if other.conflict_summary.is_some() {
            self.conflict_summary = other.conflict_summary;
        }
    }
}

// InsertOutcome

/// The result of inserting a change.
#[derive(Debug, Clone)]
pub struct InsertOutcome {
    /// The new Merkle state of the view after insertion.
    pub new_state: Merkle,

    /// The sequence number of the inserted change on the view.
    pub sequence: u64,

    /// Whether any conflicts were detected during insertion.
    pub has_conflicts: bool,

    /// Statistics about the insertion.
    pub stats: InsertStats,
}

impl InsertOutcome {
    /// Create a new outcome.
    pub fn new(new_state: Merkle, sequence: u64, has_conflicts: bool, stats: InsertStats) -> Self {
        Self {
            new_state,
            sequence,
            has_conflicts,
            stats,
        }
    }
}

// Core Insertion Functions

/// Check what dependencies are missing for a change.
///
/// This is useful for determining what needs to be fetched from a remote
/// before a change can be inserted.
///
/// # Arguments
///
/// * `txn` - Read transaction
/// * `change` - The change to check
///
/// # Returns
///
/// A vector of missing dependency hashes (empty if all deps are present).
pub fn check_missing_dependencies<T: GraphTxnT>(
    txn: &T,
    change: &Change,
) -> InsertResult<Vec<Hash>> {
    verify_dependencies(txn, change).map_err(|e| InsertError::Database(e.to_string()))
}

/// Determine the order to insert a set of changes respecting dependencies.
///
/// This performs a topological sort of the changes based on their
/// dependency relationships.
///
/// # Arguments
///
/// * `changes` - Map of change hash to change
///
/// # Returns
///
/// Ordered list of hashes to insert (dependencies first).
pub fn compute_insert_order(
    changes: &std::collections::HashMap<Hash, Change>,
) -> InsertResult<Vec<Hash>> {
    let mut order = Vec::new();
    let mut visited = HashSet::new();
    let mut in_progress = HashSet::new();

    fn visit(
        hash: &Hash,
        changes: &std::collections::HashMap<Hash, Change>,
        visited: &mut HashSet<Hash>,
        in_progress: &mut HashSet<Hash>,
        order: &mut Vec<Hash>,
    ) -> InsertResult<()> {
        if visited.contains(hash) {
            return Ok(());
        }
        if in_progress.contains(hash) {
            return Err(InsertError::CyclicDependency {
                message: format!("Cycle detected involving {}", hash.to_base32()),
            });
        }

        in_progress.insert(*hash);

        if let Some(change) = changes.get(hash) {
            for dep in change.dependencies() {
                if changes.contains_key(dep) {
                    visit(dep, changes, visited, in_progress, order)?;
                }
            }
        }

        in_progress.remove(hash);
        visited.insert(*hash);
        order.push(*hash);

        Ok(())
    }

    for hash in changes.keys() {
        visit(hash, changes, &mut visited, &mut in_progress, &mut order)?;
    }

    Ok(order)
}

/// Write a single change to a view (low-level).
///
/// This is the core insertion function that modifies the graph.
/// It assumes all validation has been done.
///
/// # Arguments
///
/// * `txn` - Write transaction
/// * `view_name` - Name of the view to insert to
/// * `change_id` - Internal ID of the change
/// * `change_hash` - Hash of the change
/// * `change` - The change to insert
/// * `options` - Insertion options
///
/// # Returns
///
/// The result of the insertion including new state and conflict info.
/// Write a change to the graph and add it to a view's change log.
///
/// This function handles two cases:
/// 1. **New change**: The change hasn't been applied to the graph yet.
///    We apply all hunks and add it to the view's change log.
/// 2. **Existing change**: The change is already in the graph (applied via another view).
///    We skip hunk application and just add it to the view's change log.
///
/// This distinction is crucial because Atomic uses a shared graph model where
/// all views share the same underlying graph. When inserting a change from one
/// view to another, the graph already contains the change's vertices and edges.
pub fn write_change_to_graph<T: MutTxnT + ViewTxnT>(
    txn: &mut T,
    view_name: &str,
    change_id: NodeId,
    change_hash: &Hash,
    change: &Change,
    options: &InsertOptions,
    already_in_graph: bool,
) -> InsertResult<InsertOutcome> {
    let mut workspace = Workspace::new();
    let mut conflict_tracker = ConflictTracker::new();
    let mut stats = InsertStats::new();

    // Get the current view
    let mut view = txn
        .open_or_create_view(view_name)
        .map_err(|e| InsertError::Database(e.to_string()))?;

    // Validate we can apply (unless caller explicitly skipped validation,
    // e.g. rebuild_change_graph re-applying an existing change with new hunks).
    if !options.skip_validation {
        validate_can_apply(txn, &view, change_id, change_hash, change)?;
    }

    // Only apply hunks if the change isn't already in the graph.
    // All edges go to the global GRAPH + INODE_GRAPH tables.
    let should_apply_hunks = !already_in_graph;

    log::debug!(
        "write_change_to_graph: change_id={:?} hash={} should_apply_hunks={} view_kind={:?}",
        change_id,
        change_hash.to_base32(),
        should_apply_hunks,
        view.kind
    );

    if should_apply_hunks {
        // Process each graph_op (graph layer)
        for graph_op in change.hunks() {
            write_hunk(
                txn,
                &mut workspace,
                &mut conflict_tracker,
                change_id,
                graph_op,
                change,
                options,
                &mut stats,
            )?;
        }

        // Apply FileOps to CRDT tables (semantic layer)
        // This enables human-readable diffs and token-level blame
        if change.has_file_ops() {
            let _crdt_stats = apply_file_ops(txn, change_id, change.file_ops())
                .map_err(|e| InsertError::Database(e.to_string()))?;
        }
    }

    // Compute new state
    let new_state = compute_new_state(&view.state, change_hash);

    // Update the view
    let sequence = view.change_count + 1;
    txn.put_change(&mut view, change_id, change_hash)
        .map_err(|e| InsertError::Database(e.to_string()))?;

    // Update view state
    view.state = new_state;
    view.change_count = sequence;
    txn.update_view(&view)
        .map_err(|e| InsertError::Database(e.to_string()))?;

    // Build conflict summary
    let has_conflicts = conflict_tracker.has_conflicts();
    if has_conflicts && options.track_conflicts {
        stats.conflict_summary = Some(ConflictSummary::from_tracker(&conflict_tracker));
        stats.conflicts_detected = conflict_tracker.total_conflict_count();
    }

    stats.changes_applied = 1;
    stats.applied_hashes.push(*change_hash);

    Ok(InsertOutcome::new(
        new_state,
        sequence,
        has_conflicts,
        stats,
    ))
}

/// Write a single graph_op to the graph.
#[allow(clippy::too_many_arguments)]
fn write_hunk<T: MutTxnT>(
    txn: &mut T,
    workspace: &mut Workspace,
    conflict_tracker: &mut ConflictTracker,
    change_id: NodeId,
    graph_op: &GraphOp<Option<Hash>>,
    change: &Change,
    _options: &InsertOptions,
    stats: &mut InsertStats,
) -> InsertResult<()> {
    // Process atoms in the graph_op
    for atom_ref in graph_op.atoms() {
        match atom_ref {
            AtomRef::Insertion(insertion) => {
                write_new_vertex(txn, workspace, change_id, insertion, change)?;
                stats.atoms_processed += 1;
            }
            AtomRef::EdgeUpdate(edge_update) => {
                write_edge_map(txn, workspace, change_id, edge_update, change)?;
                stats.atoms_processed += 1;
            }
            AtomRef::Atom(atom) => {
                // Full atom - dispatch to appropriate handler
                match atom {
                    Atom::Insertion(nv) => {
                        write_new_vertex(txn, workspace, change_id, nv, change)?;
                        stats.atoms_processed += 1;
                    }
                    Atom::EdgeUpdate(em) => {
                        write_edge_map(txn, workspace, change_id, em, change)?;
                        stats.atoms_processed += 1;
                    }
                }
            }
        }

        // Track conflicts but never abort.  In the ambient graph model all
        // edges live in a single GRAPH and changes are validated at record
        // time, so "conflicts" here are structural context notes — not
        // errors that should block graph writes.
        if workspace.has_conflicts() {
            for missing_ctx in workspace.missing_contexts() {
                let conflict = if missing_ctx.is_predecessor {
                    MissingContextConflict::predecessors(missing_ctx.position, change_id)
                } else {
                    MissingContextConflict::successors(missing_ctx.position, change_id)
                };
                conflict_tracker.add_missing_context(conflict);
            }
            for zombie in workspace.zombies() {
                conflict_tracker.add_zombie(ZombieConflict::new(zombie.node));
            }
        }
    }

    Ok(())
}

/// Collect all dependencies needed to insert a change.
///
/// This recursively collects all transitive dependencies.
///
/// # Arguments
///
/// * `txn` - Read transaction
/// * `change` - The change to collect dependencies for
/// * `available` - Set of available change hashes
/// * `max_depth` - Maximum recursion depth
///
/// # Returns
///
/// Set of all dependency hashes needed (not including those already applied).
pub fn collect_all_dependencies<T: GraphTxnT>(
    txn: &T,
    change: &Change,
    available: &HashSet<Hash>,
    max_depth: usize,
) -> InsertResult<HashSet<Hash>> {
    let mut needed = HashSet::new();
    let mut queue: VecDeque<(Hash, usize)> = VecDeque::new();
    let mut visited = HashSet::new();

    // Start with direct dependencies
    for dep in change.dependencies() {
        if !visited.contains(dep) {
            queue.push_back((*dep, 0));
            visited.insert(*dep);
        }
    }

    while let Some((hash, depth)) = queue.pop_front() {
        if depth > max_depth {
            return Err(InsertError::CyclicDependency {
                message: format!("Maximum dependency depth {} exceeded", max_depth),
            });
        }

        // Check if already in repository
        if txn
            .get_internal(&hash)
            .map_err(|e| InsertError::Database(e.to_string()))?
            .is_some()
        {
            continue;
        }

        // Check if available
        if available.contains(&hash) {
            needed.insert(hash);
            // Note: We'd need the actual change content to recurse further
            // For now, we assume available changes have their deps satisfied
        } else {
            needed.insert(hash);
        }
    }

    Ok(needed)
}

// Cross-View Insert Operations

/// Options for inserting changes between views.
///
/// This struct configures how changes are copied from a source view
/// to a target view.
///
/// # Example
///
/// ```rust,ignore
/// // Insert all changes from feature to main
/// let options = CrossViewInsertOptions::new("feature", "main");
///
/// // Insert only up to a specific tag
/// let options = CrossViewInsertOptions::new("feature", "main")
///     .up_to_tag("v1.0.0");
///
/// // Insert specific changes only
/// let options = CrossViewInsertOptions::new("feature", "main")
///     .only_changes(vec![hash1, hash2]);
/// ```
#[derive(Debug, Clone)]
pub struct CrossViewInsertOptions {
    /// Source view to copy changes from.
    pub from_view: String,

    /// Target view to insert changes to.
    pub to_view: String,

    /// Optional tag to limit changes up to (inclusive).
    /// Only changes up to and including this tag's state will be inserted.
    pub up_to_tag: Option<String>,

    /// Optional specific changes to insert (if empty, insert all missing).
    pub only_changes: Vec<Hash>,

    /// Whether to insert dependencies automatically.
    pub apply_dependencies: bool,

    /// Whether to allow conflicts.
    pub allow_conflicts: bool,

    /// Whether to do a dry run (don't actually insert).
    pub dry_run: bool,
}

impl CrossViewInsertOptions {
    /// Create new cross-view insert options.
    ///
    /// # Arguments
    ///
    /// * `from_view` - Source view name
    /// * `to_view` - Target view name
    pub fn new(from_view: impl Into<String>, to_view: impl Into<String>) -> Self {
        Self {
            from_view: from_view.into(),
            to_view: to_view.into(),
            up_to_tag: None,
            only_changes: Vec::new(),
            apply_dependencies: true,
            allow_conflicts: false,
            dry_run: false,
        }
    }

    /// Limit changes to those up to and including a tag.
    pub fn up_to_tag(mut self, tag: impl Into<String>) -> Self {
        self.up_to_tag = Some(tag.into());
        self
    }

    /// Insert only specific changes.
    pub fn only_changes(mut self, changes: Vec<Hash>) -> Self {
        self.only_changes = changes;
        self
    }

    /// Set whether to insert dependencies automatically.
    pub fn with_dependencies(mut self, apply: bool) -> Self {
        self.apply_dependencies = apply;
        self
    }

    /// Set whether to allow conflicts.
    pub fn allow_conflicts(mut self, allow: bool) -> Self {
        self.allow_conflicts = allow;
        self
    }

    /// Set dry run mode.
    pub fn dry_run(mut self, dry: bool) -> Self {
        self.dry_run = dry;
        self
    }
}

/// Result of a cross-view insert operation.
#[derive(Debug, Clone)]
pub struct CrossViewInsertOutcome {
    /// Number of changes inserted.
    pub changes_applied: usize,

    /// Hashes of changes that were inserted.
    pub applied_hashes: Vec<Hash>,

    /// Hashes of changes that were skipped (already in target).
    pub skipped_hashes: Vec<Hash>,

    /// New state of the target view.
    pub new_state: Merkle,

    /// New sequence number of the target view.
    pub sequence: u64,

    /// Whether any conflicts were detected.
    pub has_conflicts: bool,

    /// Was this a dry run?
    pub was_dry_run: bool,
}

impl CrossViewInsertOutcome {
    /// Create a new outcome.
    pub fn new() -> Self {
        Self {
            changes_applied: 0,
            applied_hashes: Vec::new(),
            skipped_hashes: Vec::new(),
            new_state: Merkle::ZERO,
            sequence: 0,
            has_conflicts: false,
            was_dry_run: false,
        }
    }

    /// Check if any changes were inserted.
    pub fn has_applied(&self) -> bool {
        self.changes_applied > 0
    }

    /// Get the total number of changes processed.
    pub fn total_processed(&self) -> usize {
        self.applied_hashes.len() + self.skipped_hashes.len()
    }
}

impl Default for CrossViewInsertOutcome {
    fn default() -> Self {
        Self::new()
    }
}

/// Get all change hashes in a view.
///
/// Returns the hashes in order from oldest (sequence 0) to newest.
///
/// # Arguments
///
/// * `txn` - Read transaction
/// * `view` - The view to get changes from
///
/// # Returns
///
/// Ordered vector of (sequence, hash) pairs.
pub fn get_view_changes<T: ViewTxnT>(txn: &T, view: &ViewState) -> InsertResult<Vec<(u64, Hash)>> {
    let mut changes = Vec::new();

    let iter = txn
        .iter_changes(view, 0)
        .map_err(|e| InsertError::Database(e.to_string()))?;

    for result in iter {
        let (seq, node_id, _merkle) = result.map_err(|e| InsertError::Database(e.to_string()))?;

        // Get external hash
        let hash = txn
            .get_external(node_id)
            .map_err(|e| InsertError::Database(e.to_string()))?
            .ok_or_else(|| {
                InsertError::Internal(format!("Change {} has no external hash", node_id.0))
            })?;

        changes.push((seq, hash));
    }

    Ok(changes)
}

/// Get changes that are in the source view but not in the target view.
///
/// # Arguments
///
/// * `txn` - Read transaction
/// * `from_view` - Source view
/// * `to_view` - Target view
///
/// # Returns
///
/// Vector of hashes that need to be inserted, in dependency order.
pub fn get_missing_changes<T: ViewTxnT>(
    txn: &T,
    from_view: &ViewState,
    to_view: &ViewState,
) -> InsertResult<Vec<Hash>> {
    // Get all changes in source
    let source_changes = get_view_changes(txn, from_view)?;

    // Build set of changes in target
    let target_set: HashSet<Hash> = get_view_changes(txn, to_view)?
        .into_iter()
        .map(|(_, hash)| hash)
        .collect();

    // Filter to changes not in target, preserving order
    let missing: Vec<Hash> = source_changes
        .into_iter()
        .filter(|(_, hash)| !target_set.contains(hash))
        .map(|(_, hash)| hash)
        .collect();

    Ok(missing)
}

/// Get changes up to a specific sequence number.
///
/// # Arguments
///
/// * `txn` - Read transaction
/// * `view` - The view to query
/// * `max_sequence` - Maximum sequence (inclusive)
///
/// # Returns
///
/// Vector of hashes up to and including the specified sequence.
pub fn get_changes_up_to_seq<T: ViewTxnT>(
    txn: &T,
    view: &ViewState,
    max_sequence: u64,
) -> InsertResult<Vec<Hash>> {
    let mut changes = Vec::new();

    let iter = txn
        .iter_changes(view, 0)
        .map_err(|e| InsertError::Database(e.to_string()))?;

    for result in iter {
        let (seq, node_id, _merkle) = result.map_err(|e| InsertError::Database(e.to_string()))?;

        if seq > max_sequence {
            break;
        }

        let hash = txn
            .get_external(node_id)
            .map_err(|e| InsertError::Database(e.to_string()))?
            .ok_or_else(|| {
                InsertError::Internal(format!("Change {} has no external hash", node_id.0))
            })?;

        changes.push(hash);
    }

    Ok(changes)
}

/// Find which changes from a list are missing in a view.
///
/// # Arguments
///
/// * `txn` - Read transaction
/// * `view` - The view to check against
/// * `changes` - List of change hashes to check
///
/// # Returns
///
/// Vector of hashes that are not in the view.
pub fn filter_missing_in_view<T: ViewTxnT>(
    txn: &T,
    view: &ViewState,
    changes: &[Hash],
) -> InsertResult<Vec<Hash>> {
    let mut missing = Vec::new();

    for hash in changes {
        // Get internal ID if it exists
        let internal = txn
            .get_internal(hash)
            .map_err(|e| InsertError::Database(e.to_string()))?;

        if let Some(node_id) = internal {
            // Check if it's in the view
            let in_view = txn
                .get_change_seq(view, node_id)
                .map_err(|e| InsertError::Database(e.to_string()))?
                .is_some();

            if !in_view {
                missing.push(*hash);
            }
        } else {
            // Not even registered, definitely missing
            missing.push(*hash);
        }
    }

    Ok(missing)
}

/// Build a dependency-ordered list of changes to insert.
///
/// Given a set of changes to insert, this function determines the correct
/// order based on their dependencies.
///
/// # Arguments
///
/// * `changes` - Map of hash to Change
///
/// # Returns
///
/// Ordered vector of hashes (dependencies first).
pub fn order_changes_by_deps(changes: &HashMap<Hash, Change>) -> InsertResult<Vec<Hash>> {
    compute_insert_order(changes)
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    // InsertOptions Tests

    #[test]
    fn test_insert_options_default() {
        let opts = InsertOptions::default();
        assert!(!opts.apply_dependencies);
        assert!(opts.allow_conflicts);
        assert_eq!(opts.max_depth, 100);
        assert!(opts.track_conflicts);
    }

    #[test]
    fn test_insert_options_with_dependencies() {
        let opts = InsertOptions::with_dependencies();
        assert!(opts.apply_dependencies);
    }

    #[test]
    fn test_insert_options_strict() {
        let opts = InsertOptions::strict();
        assert!(!opts.allow_conflicts);
    }

    #[test]
    fn test_insert_options_builder() {
        let opts = InsertOptions::default()
            .apply_deps(true)
            .allow_conflict(false)
            .max_recursion(50);

        assert!(opts.apply_dependencies);
        assert!(!opts.allow_conflicts);
        assert_eq!(opts.max_depth, 50);
    }

    // InsertStats Tests

    #[test]
    fn test_insert_stats_new() {
        let stats = InsertStats::new();
        assert_eq!(stats.changes_applied, 0);
        assert_eq!(stats.atoms_processed, 0);
        assert!(!stats.has_applied());
        assert!(!stats.has_conflicts());
    }

    #[test]
    fn test_insert_stats_has_applied() {
        let mut stats = InsertStats::new();
        assert!(!stats.has_applied());

        stats.changes_applied = 1;
        assert!(stats.has_applied());
    }

    #[test]
    fn test_insert_stats_has_conflicts() {
        let mut stats = InsertStats::new();
        assert!(!stats.has_conflicts());

        stats.conflicts_detected = 1;
        assert!(stats.has_conflicts());
    }

    #[test]
    fn test_insert_stats_merge() {
        let mut stats1 = InsertStats::new();
        stats1.changes_applied = 2;
        stats1.atoms_processed = 10;

        let mut stats2 = InsertStats::new();
        stats2.changes_applied = 1;
        stats2.atoms_processed = 5;
        stats2.conflicts_detected = 1;

        stats1.merge(stats2);

        assert_eq!(stats1.changes_applied, 3);
        assert_eq!(stats1.atoms_processed, 15);
        assert_eq!(stats1.conflicts_detected, 1);
    }

    // InsertOutcome Tests

    #[test]
    fn test_insert_outcome_new() {
        let state = Merkle::of(b"test");
        let stats = InsertStats::new();
        let outcome = InsertOutcome::new(state, 1, false, stats);

        assert_eq!(outcome.new_state, state);
        assert_eq!(outcome.sequence, 1);
        assert!(!outcome.has_conflicts);
    }

    // Error Tests

    #[test]
    fn test_insert_error_display() {
        let err = InsertError::ChangeNotFound {
            hash: "ABC123".to_string(),
        };
        assert!(err.to_string().contains("ABC123"));

        let hash1 = Hash::of(b"dep1");
        let err = InsertError::MissingDependencies {
            missing: vec![hash1],
        };
        let msg = err.to_string();
        assert!(msg.contains("Missing dependencies"));

        let err = InsertError::AlreadyApplied {
            hash: "XYZ789".to_string(),
        };
        assert!(err.to_string().contains("already applied"));
    }

    #[test]
    fn test_insert_error_from_local() {
        let local_err = LocalApplyError::ChangeAlreadyApplied {
            hash: Hash::of(b"test"),
        };
        let insert_err: InsertError = local_err.into();
        assert!(matches!(insert_err, InsertError::AlreadyApplied { .. }));

        let local_err = LocalApplyError::DependencyMissing {
            hash: Hash::of(b"dep"),
        };
        let insert_err: InsertError = local_err.into();
        assert!(matches!(
            insert_err,
            InsertError::MissingDependencies { .. }
        ));
    }

    // Compute Insert Order Tests

    #[test]
    fn test_compute_insert_order_empty() {
        let changes = std::collections::HashMap::new();
        let order = compute_insert_order(&changes).unwrap();
        assert!(order.is_empty());
    }

    #[test]
    fn test_format_hashes() {
        let hashes = vec![Hash::of(b"a"), Hash::of(b"b")];
        let formatted = format_hashes(&hashes);
        assert!(formatted.contains(","));
    }

    #[test]
    fn test_format_hashes_empty() {
        let hashes: Vec<Hash> = vec![];
        let formatted = format_hashes(&hashes);
        assert!(formatted.is_empty());
    }

    #[test]
    fn test_format_hashes_single() {
        let hashes = vec![Hash::of(b"single")];
        let formatted = format_hashes(&hashes);
        assert!(!formatted.contains(","));
        assert!(!formatted.is_empty());
    }

    // CrossViewInsertOptions Tests

    #[test]
    fn test_cross_view_options_new() {
        let opts = CrossViewInsertOptions::new("feature", "main");
        assert_eq!(opts.from_view, "feature");
        assert_eq!(opts.to_view, "main");
        assert!(opts.up_to_tag.is_none());
        assert!(opts.only_changes.is_empty());
        assert!(opts.apply_dependencies);
        assert!(!opts.allow_conflicts);
        assert!(!opts.dry_run);
    }

    #[test]
    fn test_cross_view_options_up_to_tag() {
        let opts = CrossViewInsertOptions::new("feature", "main").up_to_tag("v1.0.0");
        assert_eq!(opts.up_to_tag, Some("v1.0.0".to_string()));
    }

    #[test]
    fn test_cross_view_options_only_changes() {
        let hash1 = Hash::of(b"change1");
        let hash2 = Hash::of(b"change2");
        let opts = CrossViewInsertOptions::new("feature", "main").only_changes(vec![hash1, hash2]);
        assert_eq!(opts.only_changes.len(), 2);
    }

    #[test]
    fn test_cross_view_options_builder() {
        let opts = CrossViewInsertOptions::new("feature", "main")
            .with_dependencies(false)
            .allow_conflicts(true)
            .dry_run(true);

        assert!(!opts.apply_dependencies);
        assert!(opts.allow_conflicts);
        assert!(opts.dry_run);
    }

    // CrossViewInsertOutcome Tests

    #[test]
    fn test_cross_view_outcome_new() {
        let outcome = CrossViewInsertOutcome::new();
        assert_eq!(outcome.changes_applied, 0);
        assert!(outcome.applied_hashes.is_empty());
        assert!(outcome.skipped_hashes.is_empty());
        assert_eq!(outcome.new_state, Merkle::ZERO);
        assert_eq!(outcome.sequence, 0);
        assert!(!outcome.has_conflicts);
        assert!(!outcome.was_dry_run);
    }

    #[test]
    fn test_cross_view_outcome_default() {
        let outcome = CrossViewInsertOutcome::default();
        assert!(!outcome.has_applied());
        assert_eq!(outcome.total_processed(), 0);
    }

    #[test]
    fn test_cross_view_outcome_has_applied() {
        let mut outcome = CrossViewInsertOutcome::new();
        assert!(!outcome.has_applied());

        outcome.changes_applied = 1;
        assert!(outcome.has_applied());
    }

    #[test]
    fn test_cross_view_outcome_total_processed() {
        let mut outcome = CrossViewInsertOutcome::new();
        outcome.applied_hashes.push(Hash::of(b"a"));
        outcome.applied_hashes.push(Hash::of(b"b"));
        outcome.skipped_hashes.push(Hash::of(b"c"));

        assert_eq!(outcome.total_processed(), 3);
    }
}
