//! Splitting changes out of a view into a new draft view.
//!
//! A **split** creates a new Draft view that forks from a source view and then
//! removes a chosen set of changes from the source. Because every change lives
//! in the single canonical `GRAPH` and a view is only a change-set *filter*,
//! this is a pure metadata operation — no edges are copied and no new changes
//! are created.
//!
//! # Semantics
//!
//! Given a source view `S = [1,2,3,4,5,6,7]` and a request to split out
//! `{3,4}`, the result is:
//!
//! - `S` becomes `[1,2,5,6,7]` (the requested changes removed).
//! - The new draft `D` is parented on `S` and holds `{3,4}` in its **own**
//!   change log, so `D` sees `own{3,4} ∪ inherit(S)= [1..7]` — the full
//!   pre-split state. `D` is the "escape pod" you keep iterating on while `S`
//!   moves forward without the split-out work.
//!
//! # The dependency safety check
//!
//! Removing changes from the *middle* of a view is only coherent when nothing
//! that stays behind depends on what leaves. We compute the **reverse-
//! dependency closure** of the requested set within `S` using the indexed
//! `REV_CHANGE_DEPS` table:
//!
//! - If the closure equals the requested set, the split is clean.
//! - If the closure is larger, some remaining change depends on a requested
//!   one. We refuse with [`RepositoryError::ViewSplitHasDependents`] unless
//!   `cascade` is set, in which case the dependents are moved too.
//!
//! This guarantees `S` is always left in a coherent state: no change remaining
//! in `S` references a change that was removed.

use std::collections::{HashSet, VecDeque};

use super::*;

use atomic_core::pristine::{PristineError, ViewState};

/// Remove now-empty parent directories of a just-deleted path, deepest-first.
///
/// `std::fs::remove_dir` only succeeds on empty directories, so this is safe:
/// a directory that still holds files is left untouched.
fn remove_empty_ancestors(root: &std::path::Path, removed_rel_path: &str) {
    let mut ancestor = std::path::Path::new(removed_rel_path).parent();
    while let Some(dir) = ancestor {
        if dir.as_os_str().is_empty() || dir == std::path::Path::new(".") {
            break;
        }
        let abs = root.join(dir);
        if abs.is_dir() {
            // Fails harmlessly if the directory is not empty.
            if std::fs::remove_dir(&abs).is_err() {
                break;
            }
        }
        ancestor = dir.parent();
    }
}

/// Options controlling a view split.
#[derive(Debug, Clone)]
pub struct SplitOptions {
    /// Name of the new Draft view to create.
    pub target_view: String,
    /// Source view to split changes out of. `None` uses the current view.
    pub from_view: Option<String>,
    /// Changes to remove from the source and retain in the new draft.
    pub changes: Vec<Hash>,
    /// Also move any changes that depend on the requested set (their reverse-
    /// dependency closure) instead of refusing the split.
    pub cascade: bool,
    /// Analyze only — report what would happen without mutating anything.
    pub dry_run: bool,
    /// Reconcile the working copy after the split.
    ///
    /// Only has an effect when splitting out of the **current** view: the
    /// files touched by the removed changes are refreshed on disk — reverted
    /// to the source's new state, or deleted if no longer present. When false
    /// (the default), the working copy is left as-is and only the stale
    /// `FILE_INDEX` entries are dropped so the next `status` recomputes.
    pub materialize: bool,
}

impl SplitOptions {
    /// Create options to split `changes` out of the current view into
    /// `target_view`.
    pub fn new(target_view: impl Into<String>, changes: Vec<Hash>) -> Self {
        Self {
            target_view: target_view.into(),
            from_view: None,
            changes,
            cascade: false,
            dry_run: false,
            materialize: false,
        }
    }
}

/// A single change involved in a split, paired with its sequence in the source.
#[derive(Debug, Clone)]
pub struct SplitChange {
    /// The change hash.
    pub hash: Hash,
    /// Its sequence number in the source view (pre-split).
    pub sequence: u64,
}

/// The result of a split (or a dry-run preview of one).
#[derive(Debug, Clone)]
pub struct SplitOutcome {
    /// Name of the new draft view.
    pub target_view: String,
    /// Name of the source view.
    pub from_view: String,
    /// Whether this was a dry run (no mutation performed).
    pub was_dry_run: bool,
    /// Whether the split was blocked by dependents (only possible when
    /// `cascade` is false). When blocked, no mutation is performed.
    pub blocked: bool,
    /// The changes explicitly requested, in source-sequence order.
    pub requested: Vec<SplitChange>,
    /// Additional dependent changes in the reverse-dependency closure, in
    /// source-sequence order. When `blocked`, these are the changes that
    /// prevented the split; when moved (cascade), these came along.
    pub dependents: Vec<SplitChange>,
    /// The full set of changes moved into the draft (requested + dependents),
    /// in source-sequence order. Empty when `blocked` or on a blocked dry run.
    pub moved: Vec<SplitChange>,
    /// Source view's own change count after the split (unchanged on dry run).
    pub source_change_count: u64,
    /// New draft's own change count after the split (0 on a blocked/dry run).
    pub target_change_count: u64,
    /// Whether the working copy was reconciled (only when `materialize` was
    /// requested and the split was out of the current view).
    pub working_copy_updated: bool,
    /// Number of affected files rewritten in the working copy.
    pub files_written: usize,
    /// Number of affected files removed from the working copy.
    pub files_removed: usize,
}

/// Analysis of a requested split against a source view.
struct SplitAnalysis {
    /// Requested changes, ordered by source sequence ascending.
    requested: Vec<SplitChange>,
    /// Reverse-dependency dependents (closure minus requested), ordered.
    dependents: Vec<SplitChange>,
    /// Full closure (requested + dependents), ordered ascending — the move set.
    closure: Vec<SplitChange>,
}

/// Compute the reverse-dependency closure of `requested` within `source`.
///
/// Bounds are `T: ViewTxnT` (which requires `GraphTxnT`), so this works on both
/// read and write transactions and can be reused for dry-run previews.
fn analyze_split<T: ViewTxnT>(
    txn: &T,
    source: &ViewState,
    requested: &[Hash],
) -> Result<SplitAnalysis, RepositoryError> {
    let db = |e: PristineError| RepositoryError::Database(e.to_string());

    // Resolve each requested hash to an internal id and confirm it is in the
    // source view's OWN change log.
    let mut requested_ids: HashSet<NodeId> = HashSet::new();
    let mut queue: VecDeque<(Hash, NodeId)> = VecDeque::new();
    for hash in requested {
        let id =
            txn.get_internal(hash)
                .map_err(db)?
                .ok_or_else(|| RepositoryError::ChangeNotFound {
                    hash: hash.to_base32(),
                })?;
        if txn.get_change_seq(source, id).map_err(db)?.is_none() {
            return Err(RepositoryError::ChangeNotInView {
                hash: hash.to_base32(),
                view: source.name.clone(),
            });
        }
        if requested_ids.insert(id) {
            queue.push_back((*hash, id));
        }
    }

    // BFS over reverse dependencies, staying inside the source view.
    let mut closure_ids: HashSet<NodeId> = requested_ids.clone();
    while let Some((hash, _id)) = queue.pop_front() {
        let dependents = txn.get_rev_change_deps(&hash).map_err(db)?;
        for dep_id in dependents {
            // Only dependents that actually live in the source view matter;
            // dependents in other views don't affect this view's coherence.
            if txn.get_change_seq(source, dep_id).map_err(db)?.is_none() {
                continue;
            }
            if closure_ids.insert(dep_id) {
                let dep_hash = txn.get_external(dep_id).map_err(db)?.ok_or_else(|| {
                    RepositoryError::ChangeNotFound {
                        hash: format!("id={}", dep_id.0),
                    }
                })?;
                queue.push_back((dep_hash, dep_id));
            }
        }
    }

    // Build ordered lists keyed by source sequence.
    let ordered = |ids: &HashSet<NodeId>| -> Result<Vec<SplitChange>, RepositoryError> {
        let mut v = Vec::with_capacity(ids.len());
        for &id in ids {
            let seq = txn
                .get_change_seq(source, id)
                .map_err(db)?
                .expect("closure member is in source view");
            let hash = txn.get_external(id).map_err(db)?.ok_or_else(|| {
                RepositoryError::ChangeNotFound {
                    hash: format!("id={}", id.0),
                }
            })?;
            v.push(SplitChange {
                hash,
                sequence: seq,
            });
        }
        v.sort_by_key(|c| c.sequence);
        Ok(v)
    };

    let requested_ordered = ordered(&requested_ids)?;
    let closure_ordered = ordered(&closure_ids)?;
    let requested_hashes: HashSet<Hash> = requested_ordered.iter().map(|c| c.hash).collect();
    let dependents: Vec<SplitChange> = closure_ordered
        .iter()
        .filter(|c| !requested_hashes.contains(&c.hash))
        .cloned()
        .collect();

    Ok(SplitAnalysis {
        requested: requested_ordered,
        dependents,
        closure: closure_ordered,
    })
}

impl Repository {
    /// Split a set of changes out of a view into a new Draft view.
    ///
    /// This is a pure metadata operation guarded by a reverse-dependency
    /// check: a change cannot leave the source while another change that stays
    /// behind still depends on it, unless `cascade` moves the dependents too.
    /// See [`SplitOptions`] for the options and [`SplitOutcome`] for the
    /// result.
    ///
    /// # Errors
    ///
    /// - [`RepositoryError::ViewAlreadyExists`] if `target_view` exists.
    /// - [`RepositoryError::ViewNotFound`] if the source view doesn't exist.
    /// - [`RepositoryError::ChangeNotFound`] / [`RepositoryError::ChangeNotInView`]
    ///   if a requested change is unknown or not in the source view's own log.
    /// - [`RepositoryError::ViewSplitHasDependents`] if changes remaining in the
    ///   source depend on the split-out set and `cascade` is not set.
    pub fn split_view(&mut self, options: SplitOptions) -> Result<SplitOutcome, RepositoryError> {
        let db = |e: PristineError| RepositoryError::Database(e.to_string());

        let from_view_name = options
            .from_view
            .clone()
            .unwrap_or_else(|| self.current_view.clone());

        if options.changes.is_empty() {
            return Err(RepositoryError::InvalidOperation {
                message: "no changes specified to split".to_string(),
            });
        }

        // ── Dry run: analyze against a read transaction, mutate nothing. ──
        if options.dry_run {
            let txn = self.pristine.read_txn().map_err(db)?;
            let source = txn.get_view(&from_view_name).map_err(db)?.ok_or_else(|| {
                RepositoryError::ViewNotFound {
                    name: from_view_name.clone(),
                }
            })?;

            if txn.get_view(&options.target_view).map_err(db)?.is_some() {
                return Err(RepositoryError::ViewAlreadyExists {
                    name: options.target_view.clone(),
                });
            }

            let analysis = analyze_split(&txn, &source, &options.changes)?;
            let blocked = !analysis.dependents.is_empty() && !options.cascade;
            let moved = if blocked {
                Vec::new()
            } else {
                analysis.closure.clone()
            };
            let target_change_count = moved.len() as u64;
            let source_change_count = source.change_count.saturating_sub(moved.len() as u64);

            return Ok(SplitOutcome {
                target_view: options.target_view,
                from_view: from_view_name,
                was_dry_run: true,
                blocked,
                requested: analysis.requested,
                dependents: analysis.dependents,
                moved,
                source_change_count,
                target_change_count,
                working_copy_updated: false,
                files_written: 0,
                files_removed: 0,
            });
        }

        // ── Real split: analysis + mutation in one write transaction. ──
        let mut txn = self.pristine.write_txn().map_err(db)?;

        if txn.get_view(&options.target_view).map_err(db)?.is_some() {
            return Err(RepositoryError::ViewAlreadyExists {
                name: options.target_view.clone(),
            });
        }

        let mut source = txn.get_view(&from_view_name).map_err(db)?.ok_or_else(|| {
            RepositoryError::ViewNotFound {
                name: from_view_name.clone(),
            }
        })?;
        let source_id = source.id;

        let analysis = analyze_split(&txn, &source, &options.changes)?;

        // Enforce the dependency safety check *before* creating anything.
        if !analysis.dependents.is_empty() && !options.cascade {
            return Err(RepositoryError::ViewSplitHasDependents {
                view: from_view_name,
                blocking: analysis
                    .dependents
                    .iter()
                    .map(|c| c.hash.to_base32())
                    .collect(),
            });
        }

        // The move set is the full reverse-dependency closure.
        let move_set = &analysis.closure;

        // Create the workspace dir (filesystem, idempotent) so the draft has a
        // materialization target.
        ensure_workspace_dir(&self.dot_dir, &options.target_view)?;

        // Create the draft parented on the source view. It inherits the
        // source's content through the parent chain; we give it its own
        // references to the moved changes below.
        let mut draft = txn
            .create_view(&options.target_view, ViewScope::Draft, Some(source_id))
            .map_err(db)?;

        // Add moved changes to the draft's own log in dependency (ascending
        // sequence) order.
        for change in move_set.iter() {
            let id = txn.get_internal(&change.hash).map_err(db)?.ok_or_else(|| {
                RepositoryError::ChangeNotFound {
                    hash: change.hash.to_base32(),
                }
            })?;
            txn.put_change(&mut draft, id, &change.hash).map_err(db)?;
        }

        // Remove moved changes from the source view. `del_change` looks each
        // change up by id (not by cached sequence) and recomputes the source's
        // Merkle chain, so removal order doesn't affect correctness; we go
        // descending to minimize sequence-shifting churn.
        for change in move_set.iter().rev() {
            let id = txn.get_internal(&change.hash).map_err(db)?.ok_or_else(|| {
                RepositoryError::ChangeNotFound {
                    hash: change.hash.to_base32(),
                }
            })?;
            txn.del_change(&mut source, id, &change.hash).map_err(db)?;
        }

        txn.update_view(&draft).map_err(db)?;
        txn.update_view(&source).map_err(db)?;

        let source_change_count = source.change_count;
        let target_change_count = draft.change_count;

        txn.commit().map_err(db)?;

        // Working-copy handling only concerns the current view (the only one
        // materialized on disk). Splitting out of another view leaves the
        // working copy untouched.
        let mut working_copy_updated = false;
        let mut files_written = 0usize;
        let mut files_removed = 0usize;

        if from_view_name == self.current_view {
            // Collect the paths touched by the removed changes.
            let mut affected: Vec<String> = Vec::new();
            for change in move_set.iter() {
                if let Ok(loaded) = self.load_change(&change.hash) {
                    for op in loaded.hunks() {
                        if let Some(p) = op.path() {
                            let p = p.to_string();
                            if !affected.contains(&p) {
                                affected.push(p);
                            }
                        }
                    }
                }
            }

            if options.materialize {
                // Reconcile the working copy to the source's new state. A
                // removed change may *delete* a file (no remaining change keeps
                // it alive) or *revert* its content (an earlier change still
                // owns it), so we split affected paths by post-split
                // visibility and handle each accordingly.
                let post_visible = self.visible_file_paths(&self.current_view)?;
                let mut to_remove: Vec<String> = Vec::new();
                let mut to_write: std::collections::HashSet<String> =
                    std::collections::HashSet::new();
                for p in &affected {
                    if post_visible.contains(p) {
                        to_write.insert(p.clone());
                    } else {
                        to_remove.push(p.clone());
                    }
                }

                for p in &to_remove {
                    let abs = self.root.join(p);
                    if abs.is_file() {
                        let _ = std::fs::remove_file(&abs);
                        remove_empty_ancestors(&self.root, p);
                    }
                }
                if !to_remove.is_empty() {
                    let refs: Vec<&str> = to_remove.iter().map(|s| s.as_str()).collect();
                    let _ = self.del_file_index_batch(&refs);
                    files_removed = to_remove.len();
                }

                if !to_write.is_empty() {
                    let n = to_write.len();
                    self.materialize_paths(to_write)?;
                    files_written = n;
                }

                working_copy_updated = true;
            } else if !affected.is_empty() {
                // Conservative default: don't touch the working copy; just drop
                // the stale FILE_INDEX entries so `status` recomputes.
                let refs: Vec<&str> = affected.iter().map(|s| s.as_str()).collect();
                let _ = self.del_file_index_batch(&refs);
            }
        }

        Ok(SplitOutcome {
            target_view: options.target_view,
            from_view: from_view_name,
            was_dry_run: false,
            blocked: false,
            requested: analysis.requested,
            dependents: analysis.dependents,
            moved: analysis.closure,
            source_change_count,
            target_change_count,
            working_copy_updated,
            files_written,
            files_removed,
        })
    }

    /// Return the source view's own change hashes in sequence order.
    ///
    /// Used by the CLI to resolve `--last N` into concrete change hashes. Only
    /// the view's own log is returned (inherited changes are excluded), which
    /// matches what [`split_view`](Self::split_view) can operate on.
    pub fn view_own_change_hashes(&self, view_name: &str) -> Result<Vec<Hash>, RepositoryError> {
        let db = |e: PristineError| RepositoryError::Database(e.to_string());
        let txn = self.pristine.read_txn().map_err(db)?;
        let view =
            txn.get_view(view_name)
                .map_err(db)?
                .ok_or_else(|| RepositoryError::ViewNotFound {
                    name: view_name.to_string(),
                })?;

        let mut out = Vec::new();
        for item in txn.iter_changes(&view, 0).map_err(db)? {
            let (_seq, id, _merkle) = item.map_err(db)?;
            let hash = txn.get_external(id).map_err(db)?.ok_or_else(|| {
                RepositoryError::ChangeNotFound {
                    hash: format!("id={}", id.0),
                }
            })?;
            out.push(hash);
        }
        Ok(out)
    }
}
