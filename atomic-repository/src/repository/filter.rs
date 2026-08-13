use super::*;

use atomic_core::types::SetId;

/// Collect all change `NodeId`s inserted into a view into a `HashSet`.
///
/// This is the canonical helper for building a **change filter** — the set
/// of changes that define a view's content.  It is used by:
///
/// - `materialize` (to filter which files are materialised)
/// - `visible_file_paths` (to compute the file set for `switch_view`)
/// - `status` (to decide which tracked files are "ours")
/// - `get_file_content*` variants (to scope graph retrieval)
///
/// Centralising this pattern eliminates duplication and ensures every
/// call site uses the same iteration + error handling.
///
/// # Complexity
///
/// O(C) where C is the number of changes on the view — a single linear
/// scan of `VIEW_CHANGES`.
pub fn collect_view_change_ids<T: ViewTxnT>(
    txn: &T,
    view: &atomic_core::pristine::ViewState,
) -> Result<HashSet<NodeId>, RepositoryError> {
    let mut ids = HashSet::new();
    let iter = txn
        .iter_changes(view, 0)
        .map_err(|e| RepositoryError::Database(e.to_string()))?;
    for result in iter {
        let (_seq, node_id, _merkle) =
            result.map_err(|e| RepositoryError::Database(e.to_string()))?;
        ids.insert(node_id);
    }
    Ok(ids)
}

/// Collect all change `NodeId`s visible from a view, including parent views.
///
/// For **draft** views this walks the full overlay chain (other draft
/// ancestors) and then the shared ancestor chain, collecting every change
/// that contributes to the view's effective perspective.  This mirrors the
/// filter built inside `get_file_content_via_overlay` and must be used
/// wherever `materialize` needs to decide which vertices are alive.
///
/// For **shared** views this is identical to `collect_view_change_ids`.
///
/// # Why this is needed
///
/// The `change_filter` passed to `materialize_view` controls which graph
/// vertices are considered "alive".  If the filter only contains the draft
/// view's own changes, vertices introduced by the shared `dev` view (the
/// base content) fail the filter and are excluded — producing empty or
/// incomplete file output.
pub fn collect_visible_change_ids<T: ViewTxnT>(
    txn: &T,
    view: &atomic_core::pristine::ViewState,
) -> Result<HashSet<NodeId>, RepositoryError> {
    // Start with the current view's own changes.
    let mut ids = collect_view_change_ids(txn, view)?;

    if view.kind.is_draft() {
        // Include changes from every draft ancestor in the overlay chain.
        let chain = txn
            .resolve_view_chain(view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        for &ancestor_id in &chain {
            if ancestor_id == view.id {
                continue; // already included above
            }
            if let Some(ancestor) = txn
                .get_view_by_id(ancestor_id)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            {
                let ancestor_ids = collect_view_change_ids(txn, &ancestor)?;
                ids.extend(ancestor_ids);
            }
        }

        // Walk the parent chain past all draft ancestors to find the nearest
        // shared view and include its changes (the global graph base).
        let mut cursor = view.parent;
        while let Some(pid) = cursor {
            let parent = txn
                .get_view_by_id(pid)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            match parent {
                Some(p) if p.kind.is_shared() => {
                    let shared_ids = collect_view_change_ids(txn, &p)?;
                    ids.extend(shared_ids);
                    break;
                }
                Some(p) => cursor = p.parent,
                None => break,
            }
        }
    }

    Ok(ids)
}

/// Collect all change `NodeId`s visible from a view and expand their indexed
/// dependency closure without loading `.change` files.
///
/// This is the dependency-aware helper interactive commands should use when
/// they need a graph change filter. The dependency data comes from pristine's
/// `CHANGE_DEPS` index, which is populated when changes are recorded or
/// inserted. Missing dependency hashes are ignored for filtering because no
/// local graph edges can exist for an unregistered dependency.
///
/// If a visible change predates the dependency index, this helper deliberately
/// does **not** fall back to loading the `.change` file. Bulk object-store reads
/// belong in an explicit repair/backfill path, not in `status`.
pub fn collect_visible_change_ids_with_deps<T: ViewTxnT>(
    txn: &T,
    view: &atomic_core::pristine::ViewState,
) -> Result<HashSet<NodeId>, RepositoryError> {
    let mut ids = collect_visible_change_ids(txn, view)?;
    expand_indexed_dependency_closure(txn, &mut ids)?;
    Ok(ids)
}

/// Fold a view's **effective visible change set** into an order-invariant
/// [`SetId`].
///
/// The identity is derived on demand (never stored) by mapping every visible
/// change `NodeId` — the view's own changes plus everything inherited through
/// its parent chain, exactly as [`collect_visible_change_ids`] computes it —
/// back to its external [`Merkle`](atomic_core::types::Merkle) hash and folding
/// those hashes into a `SetId`.
///
/// Because `SetId` is order-invariant, the arbitrary iteration order of the
/// underlying `HashSet` does not affect the result: two views holding the same
/// set of changes in any order (e.g. before and after a `view split` +
/// reinsert) yield the **same** `SetId`, even though their order-sensitive
/// `Merkle` states legitimately differ.
///
/// # Domain
///
/// This folds the view's applyable **change** set. Sidecars
/// (`.provenance`/`.attest`) are never part of `VIEW_CHANGES` and so are
/// naturally excluded — matching the canonical cross-producer domain documented
/// on [`atomic_core::types::SetId`].
///
/// # Complexity
///
/// O(C) in the number of visible changes: one pass to collect the ids and one
/// external-hash lookup per change.
pub fn view_set_id<T: ViewTxnT + GraphTxnT>(
    txn: &T,
    view: &atomic_core::pristine::ViewState,
) -> Result<SetId, RepositoryError> {
    let ids = collect_visible_change_ids(txn, view)?;
    let mut acc = SetId::ZERO;
    for id in ids {
        let hash = txn
            .get_external(id)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| {
                RepositoryError::Database(format!("change {} has no external hash", id.0))
            })?;
        acc = acc.add(&hash);
    }
    Ok(acc)
}

impl Repository {
    /// Derive the order-invariant [`SetId`] of a view's effective visible
    /// change set.
    ///
    /// This is the content identity that answers "do these two views hold the
    /// same set of changes?" regardless of the order the changes were applied
    /// in — the property the order-sensitive `Merkle` state cannot provide. See
    /// [`view_set_id`] and [`atomic_core::types::SetId`] for the construction,
    /// the domain contract, and the convergence-identity tradeoff.
    pub fn view_set_id(&self, name: &str) -> Result<SetId, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let view = txn
            .get_view(name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: name.to_string(),
            })?;
        view_set_id(&txn, &view)
    }
}

/// Expand `ids` in-place using the pristine change dependency index only.
pub fn expand_indexed_dependency_closure<T: GraphTxnT>(
    txn: &T,
    ids: &mut HashSet<NodeId>,
) -> Result<(), RepositoryError> {
    let mut queue: std::collections::VecDeque<NodeId> = ids.iter().copied().collect();
    let mut unindexed_count = 0usize;

    while let Some(change_id) = queue.pop_front() {
        let indexed = txn
            .is_change_deps_indexed(change_id)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        if !indexed {
            unindexed_count += 1;
            continue;
        }

        let deps = txn
            .get_change_deps(change_id)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        for dep_hash in deps {
            if let Some(dep_id) = txn
                .get_internal(&dep_hash)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            {
                if ids.insert(dep_id) {
                    queue.push_back(dep_id);
                }
            }
        }
    }

    if unindexed_count > 0 {
        log::debug!(
            "view filter dependency closure skipped {} unindexed changes; run dependency-index repair to backfill legacy repos",
            unindexed_count
        );
    }

    Ok(())
}
