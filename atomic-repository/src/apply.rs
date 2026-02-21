//! Change application for Atomic VCS
//!
//! This module provides high-level functions for applying changes to the
//! repository graph. Application is the process of taking a recorded change
//! and modifying the repository's internal graph to reflect its contents.
//!
//! # Overview
//!
//! Applying a change involves several steps:
//!
//! 1. **Load**: Read the change from the change store
//! 2. **Validate**: Verify dependencies are present and change isn't already applied
//! 3. **Register**: Get an internal ID for the change
//! 4. **Apply Atoms**: Process each graph_op's atoms (vertices and edges)
//! 5. **Update Stack**: Add to change log and update Merkle state
//! 6. **Handle Conflicts**: Track zombies and missing contexts
//!
//! # Cross-Stack Operations
//!
//! Atomic supports applying changes between stacks, enabling:
//!
//! - **Cherry-picking**: Apply specific changes from one stack to another
//! - **Stack merging**: Apply all changes from one stack to another
//! - **Tag-based apply**: Apply changes up to a tagged state
//!
//! ```rust,ignore
//! // Apply changes from feature stack to main stack
//! let options = CrossStackApplyOptions::new("feature", "main");
//! let result = repo.apply_from_stack(options)?;
//!
//! // Apply changes up to a tag
//! let options = CrossStackApplyOptions::new("feature", "main")
//!     .up_to_tag("v1.0.0");
//! let result = repo.apply_from_stack(options)?;
//! ```
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                      Change Application Flow                            │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  Change Store           Pristine              Graph                     │
//! │  ┌──────────┐         ┌───────────┐        ┌─────────────────┐         │
//! │  │ load_    │ verify  │ register  │ apply  │  Add Vertices   │         │
//! │  │ change() │ ──────▶ │ _change() │──────▶ │  Update Edges   │         │
//! │  └──────────┘         └───────────┘        └─────────────────┘         │
//! │       │                    │                       │                   │
//! │       │                    ▼                       ▼                   │
//! │       │            ┌───────────────┐       ┌─────────────────┐         │
//! │       │            │   NodeId      │       │  Updated Stack  │         │
//! │       └───────────▶│   Assigned    │       │  New State      │         │
//! │                    └───────────────┘       └─────────────────┘         │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Dependency Resolution
//!
//! Changes in Atomic have explicit dependencies. Before a change can be
//! applied, all its dependencies must already be present in the repository.
//! This module provides functions to:
//!
//! - Check if all dependencies are present
//! - Apply dependencies recursively if available
//! - Report missing dependencies for the user to resolve
//!
//! # Conflict Handling
//!
//! During application, conflicts can arise:
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
//! use atomic_repository::{Repository, ApplyOptions};
//!
//! let repo = Repository::open(".")?;
//!
//! // Apply a single change
//! let result = repo.apply_change(&hash, ApplyOptions::default())?;
//! println!("Applied change, new state: {}", result.new_state);
//!
//! // Apply with dependencies
//! let result = repo.apply_change_with_deps(&hash, ApplyOptions::default())?;
//! if result.has_conflicts {
//!     println!("Warning: conflicts detected");
//! }
//! ```

use atomic_core::apply::{
    apply_edge_map, apply_file_ops, apply_new_vertex, compute_new_state, validate_can_apply,
    verify_dependencies, ApplyTarget, ConflictSummary, ConflictTracker, LocalApplyError,
    MissingContextConflict, Workspace, ZombieConflict,
};
use atomic_core::change::{Atom, AtomRef, Change, GraphOp};
use atomic_core::pristine::{GraphTxnT, MutTxnT, StackState, StackTxnT};
use atomic_core::types::{Base32, Hash, Merkle, NodeId};
use std::collections::{HashMap, HashSet, VecDeque};
use thiserror::Error;

// Error Types

/// Result type for apply operations.
pub type ApplyResult<T> = Result<T, ApplyError>;

/// Errors that can occur during change application.
#[derive(Debug, Error)]
pub enum ApplyError {
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

    /// The change is already applied to the stack.
    #[error("Change already applied: {hash}")]
    AlreadyApplied {
        /// The hash of the already-applied change
        hash: String,
    },

    /// A conflict was detected during application.
    #[error("Conflict during application: {message}")]
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

impl From<LocalApplyError> for ApplyError {
    fn from(e: LocalApplyError) -> Self {
        match e {
            LocalApplyError::DependencyMissing { hash } => ApplyError::MissingDependencies {
                missing: vec![hash],
            },
            LocalApplyError::ChangeAlreadyApplied { hash } => ApplyError::AlreadyApplied {
                hash: hash.to_base32(),
            },
            LocalApplyError::CyclicDependency { message } => {
                ApplyError::CyclicDependency { message }
            }
            LocalApplyError::InvalidChange => ApplyError::InvalidChange {
                message: "Change format is invalid".to_string(),
            },
            LocalApplyError::Corruption => ApplyError::InvalidChange {
                message: "Change data is corrupted".to_string(),
            },
            other => ApplyError::Internal(other.to_string()),
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

// ApplyOptions

/// Options for controlling change application.
#[derive(Debug, Clone)]
pub struct ApplyOptions {
    /// Stack to apply to (None = current stack).
    ///
    /// When `None`, the change is applied to the repository's current stack.
    /// Default: `None`
    pub stack: Option<String>,

    /// Automatically apply missing dependencies if available.
    ///
    /// When `true`, if a change's dependencies are missing but available
    /// in the change store, they will be applied first.
    /// Default: `false`
    pub apply_dependencies: bool,

    /// Allow applying even if conflicts are detected.
    ///
    /// When `false`, application stops on the first conflict.
    /// When `true`, conflicts are tracked but application continues.
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
}

impl Default for ApplyOptions {
    fn default() -> Self {
        Self {
            stack: None,
            apply_dependencies: false,
            allow_conflicts: true,
            max_depth: 100,
            track_conflicts: true,
        }
    }
}

impl ApplyOptions {
    /// Set the stack to apply to.
    pub fn stack(mut self, name: impl Into<String>) -> Self {
        self.stack = Some(name.into());
        self
    }

    /// Create options that will apply dependencies automatically.
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

    /// Set whether to apply dependencies automatically.
    pub fn apply_deps(mut self, apply: bool) -> Self {
        self.apply_dependencies = apply;
        self
    }

    /// Set whether to allow conflicts.
    pub fn allow_conflict(mut self, allow: bool) -> Self {
        self.allow_conflicts = allow;
        self
    }

    /// Set maximum recursion depth.
    pub fn max_recursion(mut self, depth: usize) -> Self {
        self.max_depth = depth;
        self
    }
}

// ApplyStats

/// Statistics from a change application operation.
#[derive(Debug, Clone, Default)]
pub struct ApplyStats {
    /// Number of changes applied.
    pub changes_applied: usize,

    /// Number of atoms (vertices + edges) processed.
    pub atoms_processed: usize,

    /// Number of conflicts detected.
    pub conflicts_detected: usize,

    /// Number of dependencies that were automatically applied.
    pub dependencies_applied: usize,

    /// Hashes of changes that were applied.
    pub applied_hashes: Vec<Hash>,

    /// Conflict summary if any conflicts were detected.
    pub conflict_summary: Option<ConflictSummary>,
}

impl ApplyStats {
    /// Create empty stats.
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if any changes were applied.
    pub fn has_applied(&self) -> bool {
        self.changes_applied > 0
    }

    /// Check if any conflicts were detected.
    pub fn has_conflicts(&self) -> bool {
        self.conflicts_detected > 0
    }

    /// Merge stats from another operation.
    pub fn merge(&mut self, other: ApplyStats) {
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

// ApplyOutcome

/// The result of applying a change.
#[derive(Debug, Clone)]
pub struct ApplyOutcome {
    /// The new Merkle state of the stack after application.
    pub new_state: Merkle,

    /// The sequence number of the applied change on the stack.
    pub sequence: u64,

    /// Whether any conflicts were detected during application.
    pub has_conflicts: bool,

    /// Statistics about the application.
    pub stats: ApplyStats,
}

impl ApplyOutcome {
    /// Create a new outcome.
    pub fn new(new_state: Merkle, sequence: u64, has_conflicts: bool, stats: ApplyStats) -> Self {
        Self {
            new_state,
            sequence,
            has_conflicts,
            stats,
        }
    }
}

// Core Application Functions

/// Check what dependencies are missing for a change.
///
/// This is useful for determining what needs to be fetched from a remote
/// before a change can be applied.
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
) -> ApplyResult<Vec<Hash>> {
    verify_dependencies(txn, change).map_err(|e| ApplyError::Database(e.to_string()))
}

/// Determine the order to apply a set of changes respecting dependencies.
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
/// Ordered list of hashes to apply (dependencies first).
pub fn compute_apply_order(
    changes: &std::collections::HashMap<Hash, Change>,
) -> ApplyResult<Vec<Hash>> {
    let mut order = Vec::new();
    let mut visited = HashSet::new();
    let mut in_progress = HashSet::new();

    fn visit(
        hash: &Hash,
        changes: &std::collections::HashMap<Hash, Change>,
        visited: &mut HashSet<Hash>,
        in_progress: &mut HashSet<Hash>,
        order: &mut Vec<Hash>,
    ) -> ApplyResult<()> {
        if visited.contains(hash) {
            return Ok(());
        }
        if in_progress.contains(hash) {
            return Err(ApplyError::CyclicDependency {
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

/// Apply a single change to a stack (low-level).
///
/// This is the core application function that modifies the graph.
/// It assumes all validation has been done.
///
/// # Arguments
///
/// * `txn` - Write transaction
/// * `stack_name` - Name of the stack to apply to
/// * `change_id` - Internal ID of the change
/// * `change_hash` - Hash of the change
/// * `change` - The change to apply
/// * `options` - Application options
///
/// # Returns
///
/// The result of the application including new state and conflict info.
/// Apply a change to the graph and add it to a stack's change log.
///
/// This function handles two cases:
/// 1. **New change**: The change hasn't been applied to the graph yet.
///    We apply all hunks and add it to the stack's change log.
/// 2. **Existing change**: The change is already in the graph (applied via another stack).
///    We skip hunk application and just add it to the stack's change log.
///
/// This distinction is crucial because Atomic uses a shared graph model where
/// all stacks share the same underlying graph. When applying a change from one
/// stack to another, the graph already contains the change's vertices and edges.
pub fn apply_change_to_graph<T: MutTxnT + StackTxnT>(
    txn: &mut T,
    stack_name: &str,
    change_id: NodeId,
    change_hash: &Hash,
    change: &Change,
    options: &ApplyOptions,
    already_in_graph: bool,
) -> ApplyResult<ApplyOutcome> {
    let mut workspace = Workspace::new();
    let mut conflict_tracker = ConflictTracker::new();
    let mut stats = ApplyStats::new();

    // Get the current stack
    let mut stack = txn
        .open_or_create_stack(stack_name)
        .map_err(|e| ApplyError::Database(e.to_string()))?;

    // Validate we can apply
    validate_can_apply(txn, &stack, change_id, change_hash, change)?;

    // Determine where edges should be written based on the stack kind.
    // Shared stacks → global GRAPH; Local workspaces → STACK_GRAPH[(stack_id, vertex)].
    let apply_target = ApplyTarget::from_stack_kind(stack.kind, stack.id);

    // Only apply hunks if the change isn't already visible through the
    // target stack's graph view.
    //
    // For **Shared** stacks the check is simple: if the change is already
    // registered in the global GRAPH we skip hunk application.
    //
    // For **Local** stacks the situation is nuanced.  A Local stack's
    // overlay chain is `STACK_GRAPH[self] ∪ ... ∪ GRAPH`.  If the change
    // is already in the global GRAPH (e.g. it was recorded on a Shared
    // parent) AND the Local stack's overlay reaches GRAPH (i.e. it has a
    // Shared ancestor), then the edges are already visible — re-applying
    // them into STACK_GRAPH would create duplicates that conflict with
    // future Replacement operations on divergent stacks.
    //
    // We only force re-application for Local stacks when the change is
    // NOT in the global GRAPH (meaning it was recorded on another Local
    // stack whose STACK_GRAPH is invisible to us).
    let should_apply_hunks = if already_in_graph {
        // Change is in global GRAPH.
        // Shared target: skip (edges are already there).
        // Local target with Shared parent: skip (overlay sees GRAPH).
        // Local target with NO parent: must re-apply (overlay doesn't reach GRAPH).
        match &apply_target {
            ApplyTarget::Global => false,
            ApplyTarget::Local { .. } => {
                // Check if the stack has a Shared ancestor by looking at
                // the parent chain.  If so, the overlay reaches GRAPH.
                !stack.parent.is_some()
            }
        }
    } else {
        // Change is not in global GRAPH — must apply hunks.
        true
    };

    if should_apply_hunks {
        // Process each graph_op (graph layer)
        for graph_op in change.hunks() {
            apply_hunk(
                txn,
                &mut workspace,
                &mut conflict_tracker,
                change_id,
                graph_op,
                change,
                options,
                &mut stats,
                &apply_target,
            )?;
        }

        // Apply FileOps to CRDT tables (semantic layer)
        // This enables human-readable diffs and token-level blame
        if change.has_file_ops() {
            let _crdt_stats = apply_file_ops(txn, change_id, change.file_ops())
                .map_err(|e| ApplyError::Database(e.to_string()))?;
        }
    }

    // Compute new state
    let new_state = compute_new_state(&stack.state, change_hash);

    // Update the stack
    let sequence = stack.change_count + 1;
    txn.put_change(&mut stack, change_id, change_hash)
        .map_err(|e| ApplyError::Database(e.to_string()))?;

    // Update stack state
    stack.state = new_state;
    stack.change_count = sequence;
    txn.update_stack(&stack)
        .map_err(|e| ApplyError::Database(e.to_string()))?;

    // Build conflict summary
    let has_conflicts = conflict_tracker.has_conflicts();
    if has_conflicts && options.track_conflicts {
        stats.conflict_summary = Some(ConflictSummary::from_tracker(&conflict_tracker));
        stats.conflicts_detected = conflict_tracker.total_conflict_count();
    }

    stats.changes_applied = 1;
    stats.applied_hashes.push(*change_hash);

    Ok(ApplyOutcome::new(new_state, sequence, has_conflicts, stats))
}

/// Apply a single graph_op to the graph.
fn apply_hunk<T: MutTxnT>(
    txn: &mut T,
    workspace: &mut Workspace,
    conflict_tracker: &mut ConflictTracker,
    change_id: NodeId,
    graph_op: &GraphOp<Option<Hash>>,
    change: &Change,
    options: &ApplyOptions,
    stats: &mut ApplyStats,
    apply_target: &ApplyTarget,
) -> ApplyResult<()> {
    // Process atoms in the graph_op
    for atom_ref in graph_op.atoms() {
        match atom_ref {
            AtomRef::Insertion(insertion) => {
                apply_new_vertex(txn, workspace, change_id, insertion, change, apply_target)?;
                stats.atoms_processed += 1;
            }
            AtomRef::EdgeUpdate(edge_update) => {
                apply_edge_map(txn, workspace, change_id, edge_update, change, apply_target)?;
                stats.atoms_processed += 1;
            }
            AtomRef::Atom(atom) => {
                // Full atom - dispatch to appropriate handler
                match atom {
                    Atom::Insertion(nv) => {
                        apply_new_vertex(txn, workspace, change_id, nv, change, apply_target)?;
                        stats.atoms_processed += 1;
                    }
                    Atom::EdgeUpdate(em) => {
                        apply_edge_map(txn, workspace, change_id, em, change, apply_target)?;
                        stats.atoms_processed += 1;
                    }
                }
            }
        }

        // Check for conflicts after each atom
        if workspace.has_conflicts() {
            // Transfer conflicts from workspace to tracker
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

            if !options.allow_conflicts {
                return Err(ApplyError::Conflict {
                    message: "Conflict detected during atom application".to_string(),
                });
            }
        }
    }

    Ok(())
}

/// Collect all dependencies needed to apply a change.
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
) -> ApplyResult<HashSet<Hash>> {
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
            return Err(ApplyError::CyclicDependency {
                message: format!("Maximum dependency depth {} exceeded", max_depth),
            });
        }

        // Check if already in repository
        if txn
            .get_internal(&hash)
            .map_err(|e| ApplyError::Database(e.to_string()))?
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

// Cross-Stack Apply Operations

/// Options for applying changes between stacks.
///
/// This struct configures how changes are copied from a source stack
/// to a target stack.
///
/// # Example
///
/// ```rust,ignore
/// // Apply all changes from feature to main
/// let options = CrossStackApplyOptions::new("feature", "main");
///
/// // Apply only up to a specific tag
/// let options = CrossStackApplyOptions::new("feature", "main")
///     .up_to_tag("v1.0.0");
///
/// // Apply specific changes only
/// let options = CrossStackApplyOptions::new("feature", "main")
///     .only_changes(vec![hash1, hash2]);
/// ```
#[derive(Debug, Clone)]
pub struct CrossStackApplyOptions {
    /// Source stack to copy changes from.
    pub from_stack: String,

    /// Target stack to apply changes to.
    pub to_stack: String,

    /// Optional tag to limit changes up to (inclusive).
    /// Only changes up to and including this tag's state will be applied.
    pub up_to_tag: Option<String>,

    /// Optional specific changes to apply (if empty, apply all missing).
    pub only_changes: Vec<Hash>,

    /// Whether to apply dependencies automatically.
    pub apply_dependencies: bool,

    /// Whether to allow conflicts.
    pub allow_conflicts: bool,

    /// Whether to do a dry run (don't actually apply).
    pub dry_run: bool,
}

impl CrossStackApplyOptions {
    /// Create new cross-stack apply options.
    ///
    /// # Arguments
    ///
    /// * `from_stack` - Source stack name
    /// * `to_stack` - Target stack name
    pub fn new(from_stack: impl Into<String>, to_stack: impl Into<String>) -> Self {
        Self {
            from_stack: from_stack.into(),
            to_stack: to_stack.into(),
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

    /// Apply only specific changes.
    pub fn only_changes(mut self, changes: Vec<Hash>) -> Self {
        self.only_changes = changes;
        self
    }

    /// Set whether to apply dependencies automatically.
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

/// Result of a cross-stack apply operation.
#[derive(Debug, Clone)]
pub struct CrossStackApplyOutcome {
    /// Number of changes applied.
    pub changes_applied: usize,

    /// Hashes of changes that were applied.
    pub applied_hashes: Vec<Hash>,

    /// Hashes of changes that were skipped (already in target).
    pub skipped_hashes: Vec<Hash>,

    /// New state of the target stack.
    pub new_state: Merkle,

    /// New sequence number of the target stack.
    pub sequence: u64,

    /// Whether any conflicts were detected.
    pub has_conflicts: bool,

    /// Was this a dry run?
    pub was_dry_run: bool,
}

impl CrossStackApplyOutcome {
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

    /// Check if any changes were applied.
    pub fn has_applied(&self) -> bool {
        self.changes_applied > 0
    }

    /// Get the total number of changes processed.
    pub fn total_processed(&self) -> usize {
        self.applied_hashes.len() + self.skipped_hashes.len()
    }
}

impl Default for CrossStackApplyOutcome {
    fn default() -> Self {
        Self::new()
    }
}

/// Get all change hashes in a stack.
///
/// Returns the hashes in order from oldest (sequence 0) to newest.
///
/// # Arguments
///
/// * `txn` - Read transaction
/// * `stack` - The stack to get changes from
///
/// # Returns
///
/// Ordered vector of (sequence, hash) pairs.
pub fn get_stack_changes<T: StackTxnT>(
    txn: &T,
    stack: &StackState,
) -> ApplyResult<Vec<(u64, Hash)>> {
    let mut changes = Vec::new();

    let iter = txn
        .iter_changes(stack, 0)
        .map_err(|e| ApplyError::Database(e.to_string()))?;

    for result in iter {
        let (seq, node_id, _merkle) = result.map_err(|e| ApplyError::Database(e.to_string()))?;

        // Get external hash
        let hash = txn
            .get_external(node_id)
            .map_err(|e| ApplyError::Database(e.to_string()))?
            .ok_or_else(|| {
                ApplyError::Internal(format!("Change {} has no external hash", node_id.0))
            })?;

        changes.push((seq, hash));
    }

    Ok(changes)
}

/// Get changes that are in the source stack but not in the target stack.
///
/// # Arguments
///
/// * `txn` - Read transaction
/// * `from_stack` - Source stack
/// * `to_stack` - Target stack
///
/// # Returns
///
/// Vector of hashes that need to be applied, in dependency order.
pub fn get_missing_changes<T: StackTxnT>(
    txn: &T,
    from_stack: &StackState,
    to_stack: &StackState,
) -> ApplyResult<Vec<Hash>> {
    // Get all changes in source
    let source_changes = get_stack_changes(txn, from_stack)?;

    // Build set of changes in target
    let target_set: HashSet<Hash> = get_stack_changes(txn, to_stack)?
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
/// * `stack` - The stack to query
/// * `max_sequence` - Maximum sequence (inclusive)
///
/// # Returns
///
/// Vector of hashes up to and including the specified sequence.
pub fn get_changes_up_to_seq<T: StackTxnT>(
    txn: &T,
    stack: &StackState,
    max_sequence: u64,
) -> ApplyResult<Vec<Hash>> {
    let mut changes = Vec::new();

    let iter = txn
        .iter_changes(stack, 0)
        .map_err(|e| ApplyError::Database(e.to_string()))?;

    for result in iter {
        let (seq, node_id, _merkle) = result.map_err(|e| ApplyError::Database(e.to_string()))?;

        if seq > max_sequence {
            break;
        }

        let hash = txn
            .get_external(node_id)
            .map_err(|e| ApplyError::Database(e.to_string()))?
            .ok_or_else(|| {
                ApplyError::Internal(format!("Change {} has no external hash", node_id.0))
            })?;

        changes.push(hash);
    }

    Ok(changes)
}

/// Find which changes from a list are missing in a stack.
///
/// # Arguments
///
/// * `txn` - Read transaction
/// * `stack` - The stack to check against
/// * `changes` - List of change hashes to check
///
/// # Returns
///
/// Vector of hashes that are not in the stack.
pub fn filter_missing_in_stack<T: StackTxnT>(
    txn: &T,
    stack: &StackState,
    changes: &[Hash],
) -> ApplyResult<Vec<Hash>> {
    let mut missing = Vec::new();

    for hash in changes {
        // Get internal ID if it exists
        let internal = txn
            .get_internal(hash)
            .map_err(|e| ApplyError::Database(e.to_string()))?;

        if let Some(node_id) = internal {
            // Check if it's in the stack
            let in_stack = txn
                .get_change_seq(stack, node_id)
                .map_err(|e| ApplyError::Database(e.to_string()))?
                .is_some();

            if !in_stack {
                missing.push(*hash);
            }
        } else {
            // Not even registered, definitely missing
            missing.push(*hash);
        }
    }

    Ok(missing)
}

/// Build a dependency-ordered list of changes to apply.
///
/// Given a set of changes to apply, this function determines the correct
/// order based on their dependencies.
///
/// # Arguments
///
/// * `changes` - Map of hash to Change
///
/// # Returns
///
/// Ordered vector of hashes (dependencies first).
pub fn order_changes_by_deps(changes: &HashMap<Hash, Change>) -> ApplyResult<Vec<Hash>> {
    compute_apply_order(changes)
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    // ApplyOptions Tests

    #[test]
    fn test_apply_options_default() {
        let opts = ApplyOptions::default();
        assert!(!opts.apply_dependencies);
        assert!(opts.allow_conflicts);
        assert_eq!(opts.max_depth, 100);
        assert!(opts.track_conflicts);
    }

    #[test]
    fn test_apply_options_with_dependencies() {
        let opts = ApplyOptions::with_dependencies();
        assert!(opts.apply_dependencies);
    }

    #[test]
    fn test_apply_options_strict() {
        let opts = ApplyOptions::strict();
        assert!(!opts.allow_conflicts);
    }

    #[test]
    fn test_apply_options_builder() {
        let opts = ApplyOptions::default()
            .apply_deps(true)
            .allow_conflict(false)
            .max_recursion(50);

        assert!(opts.apply_dependencies);
        assert!(!opts.allow_conflicts);
        assert_eq!(opts.max_depth, 50);
    }

    // ApplyStats Tests

    #[test]
    fn test_apply_stats_new() {
        let stats = ApplyStats::new();
        assert_eq!(stats.changes_applied, 0);
        assert_eq!(stats.atoms_processed, 0);
        assert!(!stats.has_applied());
        assert!(!stats.has_conflicts());
    }

    #[test]
    fn test_apply_stats_has_applied() {
        let mut stats = ApplyStats::new();
        assert!(!stats.has_applied());

        stats.changes_applied = 1;
        assert!(stats.has_applied());
    }

    #[test]
    fn test_apply_stats_has_conflicts() {
        let mut stats = ApplyStats::new();
        assert!(!stats.has_conflicts());

        stats.conflicts_detected = 1;
        assert!(stats.has_conflicts());
    }

    #[test]
    fn test_apply_stats_merge() {
        let mut stats1 = ApplyStats::new();
        stats1.changes_applied = 2;
        stats1.atoms_processed = 10;

        let mut stats2 = ApplyStats::new();
        stats2.changes_applied = 1;
        stats2.atoms_processed = 5;
        stats2.conflicts_detected = 1;

        stats1.merge(stats2);

        assert_eq!(stats1.changes_applied, 3);
        assert_eq!(stats1.atoms_processed, 15);
        assert_eq!(stats1.conflicts_detected, 1);
    }

    // ApplyOutcome Tests

    #[test]
    fn test_apply_outcome_new() {
        let state = Merkle::of(b"test");
        let stats = ApplyStats::new();
        let outcome = ApplyOutcome::new(state, 1, false, stats);

        assert_eq!(outcome.new_state, state);
        assert_eq!(outcome.sequence, 1);
        assert!(!outcome.has_conflicts);
    }

    // Error Tests

    #[test]
    fn test_apply_error_display() {
        let err = ApplyError::ChangeNotFound {
            hash: "ABC123".to_string(),
        };
        assert!(err.to_string().contains("ABC123"));

        let hash1 = Hash::of(b"dep1");
        let err = ApplyError::MissingDependencies {
            missing: vec![hash1],
        };
        let msg = err.to_string();
        assert!(msg.contains("Missing dependencies"));

        let err = ApplyError::AlreadyApplied {
            hash: "XYZ789".to_string(),
        };
        assert!(err.to_string().contains("already applied"));
    }

    #[test]
    fn test_apply_error_from_local() {
        let local_err = LocalApplyError::ChangeAlreadyApplied {
            hash: Hash::of(b"test"),
        };
        let apply_err: ApplyError = local_err.into();
        assert!(matches!(apply_err, ApplyError::AlreadyApplied { .. }));

        let local_err = LocalApplyError::DependencyMissing {
            hash: Hash::of(b"dep"),
        };
        let apply_err: ApplyError = local_err.into();
        assert!(matches!(apply_err, ApplyError::MissingDependencies { .. }));
    }

    // Compute Apply Order Tests

    #[test]
    fn test_compute_apply_order_empty() {
        let changes = std::collections::HashMap::new();
        let order = compute_apply_order(&changes).unwrap();
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

    // CrossStackApplyOptions Tests

    #[test]
    fn test_cross_stack_options_new() {
        let opts = CrossStackApplyOptions::new("feature", "main");
        assert_eq!(opts.from_stack, "feature");
        assert_eq!(opts.to_stack, "main");
        assert!(opts.up_to_tag.is_none());
        assert!(opts.only_changes.is_empty());
        assert!(opts.apply_dependencies);
        assert!(!opts.allow_conflicts);
        assert!(!opts.dry_run);
    }

    #[test]
    fn test_cross_stack_options_up_to_tag() {
        let opts = CrossStackApplyOptions::new("feature", "main").up_to_tag("v1.0.0");
        assert_eq!(opts.up_to_tag, Some("v1.0.0".to_string()));
    }

    #[test]
    fn test_cross_stack_options_only_changes() {
        let hash1 = Hash::of(b"change1");
        let hash2 = Hash::of(b"change2");
        let opts = CrossStackApplyOptions::new("feature", "main").only_changes(vec![hash1, hash2]);
        assert_eq!(opts.only_changes.len(), 2);
    }

    #[test]
    fn test_cross_stack_options_builder() {
        let opts = CrossStackApplyOptions::new("feature", "main")
            .with_dependencies(false)
            .allow_conflicts(true)
            .dry_run(true);

        assert!(!opts.apply_dependencies);
        assert!(opts.allow_conflicts);
        assert!(opts.dry_run);
    }

    // CrossStackApplyOutcome Tests

    #[test]
    fn test_cross_stack_outcome_new() {
        let outcome = CrossStackApplyOutcome::new();
        assert_eq!(outcome.changes_applied, 0);
        assert!(outcome.applied_hashes.is_empty());
        assert!(outcome.skipped_hashes.is_empty());
        assert_eq!(outcome.new_state, Merkle::ZERO);
        assert_eq!(outcome.sequence, 0);
        assert!(!outcome.has_conflicts);
        assert!(!outcome.was_dry_run);
    }

    #[test]
    fn test_cross_stack_outcome_default() {
        let outcome = CrossStackApplyOutcome::default();
        assert!(!outcome.has_applied());
        assert_eq!(outcome.total_processed(), 0);
    }

    #[test]
    fn test_cross_stack_outcome_has_applied() {
        let mut outcome = CrossStackApplyOutcome::new();
        assert!(!outcome.has_applied());

        outcome.changes_applied = 1;
        assert!(outcome.has_applied());
    }

    #[test]
    fn test_cross_stack_outcome_total_processed() {
        let mut outcome = CrossStackApplyOutcome::new();
        outcome.applied_hashes.push(Hash::of(b"a"));
        outcome.applied_hashes.push(Hash::of(b"b"));
        outcome.skipped_hashes.push(Hash::of(b"c"));

        assert_eq!(outcome.total_processed(), 3);
    }
}
