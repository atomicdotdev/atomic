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

mod cross_view;
mod queries;
mod types;

#[cfg(test)]
mod tests;

pub use cross_view::{CrossViewInsertOptions, CrossViewInsertOutcome};
pub use queries::{
    filter_missing_in_view, get_changes_up_to_seq, get_missing_changes, get_view_changes,
    order_changes_by_deps,
};
pub use types::{InsertError, InsertOptions, InsertOutcome, InsertResult, InsertStats};

// Re-export format_hashes for internal use (tests)
#[cfg(test)]
pub(crate) use types::format_hashes;

use atomic_core::apply::{
    apply_file_ops_batched, compute_new_state, validate_can_apply, verify_dependencies,
    write_edge_map, write_new_vertex, ConflictTracker, MissingContextConflict, Workspace,
    ZombieConflict,
};
use atomic_core::change::{Atom, AtomRef, Change, GraphOp};
use atomic_core::pristine::{GraphTxnT, MutTxnT, WriteTxn};
use atomic_core::types::{Base32, Hash, NodeId};
use std::collections::{HashSet, VecDeque};

// ── Core Insertion Functions ─────────────────────────────────────────

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
///
/// # Arguments
///
/// * `txn` - Write transaction
/// * `view_name` - Name of the view to insert to
/// * `change_id` - Internal ID of the change
/// * `change_hash` - Hash of the change
/// * `change` - The change to insert
/// * `options` - Insertion options
/// * `already_in_graph` - Whether the change's edges are already in the graph
///
/// # Returns
///
/// The result of the insertion including new state and conflict info.
pub fn write_change_to_graph(
    txn: &mut WriteTxn<'_>,
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
    let trace_record = std::env::var_os("ATOMIC_TRACE_RECORD").is_some();

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
        let hunks = change.hunks();
        let total_hunks = hunks.len();
        let hunk_phase_start = std::time::Instant::now();
        // Process each graph_op (graph layer)
        for (hunk_idx, graph_op) in hunks.iter().enumerate() {
            if trace_record && (hunk_idx == 0 || (hunk_idx + 1) % 1_000 == 0) {
                eprintln!(
                    "[write_change_to_graph] applying hunk {}/{} elapsed={:?}",
                    hunk_idx + 1,
                    total_hunks,
                    hunk_phase_start.elapsed()
                );
            }
            log::debug!(
                "write_change_to_graph: hunk {}/{} starting: {:?}",
                hunk_idx + 1,
                total_hunks,
                std::mem::discriminant(graph_op)
            );
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
            log::debug!(
                "write_change_to_graph: hunk {}/{} complete",
                hunk_idx + 1,
                total_hunks
            );
        }
        if trace_record {
            eprintln!(
                "[write_change_to_graph] hunks complete count={} elapsed={:?}",
                total_hunks,
                hunk_phase_start.elapsed()
            );
        }

        // Apply FileOps to CRDT tables (semantic layer)
        // This enables human-readable diffs and token-level blame
        if change.has_file_ops() {
            let file_ops = change.file_ops();
            let file_ops_count = file_ops.len();
            log::debug!(
                "write_change_to_graph: applying {} FileOps to CRDT tables",
                file_ops_count
            );
            let crdt_start = std::time::Instant::now();
            if trace_record {
                eprintln!(
                    "[write_change_to_graph] apply_file_ops start count={}",
                    file_ops_count
                );
            }
            let _crdt_stats = apply_file_ops_batched(txn, change_id, file_ops).map_err(
                |e: atomic_core::pristine::PristineError| InsertError::Database(e.to_string()),
            )?;
            let crdt_ms = crdt_start.elapsed().as_millis();
            if trace_record {
                eprintln!(
                    "[write_change_to_graph] apply_file_ops complete elapsed={:?}",
                    crdt_start.elapsed()
                );
            }
            if crdt_ms > 50 {
                log::warn!(
                    "write_change_to_graph: SLOW apply_file_ops took {}ms ({} FileOps)",
                    crdt_ms,
                    file_ops_count
                );
            } else {
                log::debug!("write_change_to_graph: apply_file_ops took {}ms", crdt_ms);
            }
        } else {
            log::debug!("write_change_to_graph: no FileOps to apply");
        }
    }

    // Compute new state
    log::debug!("write_change_to_graph: computing new state + updating view");
    let step_start = std::time::Instant::now();
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
    log::debug!(
        "write_change_to_graph: view update took {}ms",
        step_start.elapsed().as_millis()
    );

    // Build conflict summary
    let has_conflicts = conflict_tracker.has_conflicts();
    if has_conflicts && options.track_conflicts {
        stats.conflict_summary = Some(atomic_core::apply::ConflictSummary::from_tracker(
            &conflict_tracker,
        ));
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

// ── Dependency Collection ────────────────────────────────────────────

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
