use super::*;

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
