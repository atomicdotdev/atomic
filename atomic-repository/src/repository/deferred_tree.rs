use super::*;

use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Write;

const DEFERRED_TREE_JOURNAL: &str = "deferred-tree-ops.json";
const DEFERRED_TREE_JOURNAL_VERSION: u32 = 1;
const DEFERRED_TREE_ALIGNMENT_PENDING: &str = "deferred-tree-alignment.pending";
const DEFERRED_TREE_ALIGNMENT_LOCK: &str = "deferred-tree-alignment.lock";

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum DeferredTreeAction {
    Set {
        path: String,
    },
    Delete,
    /// Unbind `path` from the inode's desired state — a name-conflict
    /// resolution that surrenders the name keeps the inode's identity
    /// (and its REV_TREE claim) while ceasing to occupy the path.
    UnlinkName {
        path: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(super) struct DeferredTreeOp {
    /// Change whose visibility activates this TREE operation.
    pub(super) change: Hash,
    /// Stable inode position, stored with an external change hash so the
    /// journal never depends on process-local lookup state.
    pub(super) inode: Position<Hash>,
    /// Path visible before this inode's first journaled operation. Only the
    /// first event for an inode uses this baseline; later events are selected
    /// solely by change visibility.
    pub(super) baseline_path: Option<String>,
    pub(super) action: DeferredTreeAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct DeferredTreeJournal {
    version: u32,
    #[serde(default)]
    ops: Vec<DeferredTreeOp>,
    /// Projection-level causal predecessors captured at first ingestion.
    #[serde(default)]
    predecessors: HashMap<Hash, Vec<Hash>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingTreeAlignment {
    version: u32,
    source_view: String,
    target_view: String,
}

impl Default for DeferredTreeJournal {
    fn default() -> Self {
        Self {
            version: DEFERRED_TREE_JOURNAL_VERSION,
            ops: Vec::new(),
            predecessors: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone)]
struct DesiredTreePaths {
    paths: BTreeSet<String>,
}

fn desired_tree_paths<F>(
    ops: &[DeferredTreeOp],
    predecessors: &HashMap<Hash, Vec<Hash>>,
    visible_changes: &HashSet<Hash>,
    mut depends_on: F,
) -> HashMap<Position<Hash>, DesiredTreePaths>
where
    F: FnMut(Hash, Hash) -> bool,
{
    let mut baselines: HashMap<Position<Hash>, Option<String>> = HashMap::new();
    let mut events: HashMap<Position<Hash>, Vec<(usize, &DeferredTreeOp)>> = HashMap::new();

    for (order, op) in ops.iter().enumerate() {
        baselines
            .entry(op.inode)
            .or_insert_with(|| op.baseline_path.clone());
        if visible_changes.contains(&op.change) {
            events.entry(op.inode).or_default().push((order, op));
        }
    }

    let mut desired = HashMap::new();
    for (inode, baseline) in baselines {
        let visible = events.remove(&inode).unwrap_or_default();
        if visible.is_empty() {
            desired.insert(
                inode,
                DesiredTreePaths {
                    paths: baseline.into_iter().collect(),
                },
            );
            continue;
        }

        let maximal = visible.iter().filter(|(order, op)| {
            !visible.iter().any(|(other_order, other)| {
                (other.change == op.change && other_order > order)
                    || (other.change != op.change
                        && (depends_on(other.change, op.change)
                            || journal_depends_on(predecessors, other.change, op.change)))
            })
        });
        let paths = maximal
            .filter_map(|(_, op)| match &op.action {
                DeferredTreeAction::Set { path } => Some(path.clone()),
                DeferredTreeAction::Delete => None,
                DeferredTreeAction::UnlinkName { .. } => None,
            })
            .collect();
        desired.insert(inode, DesiredTreePaths { paths });
    }

    desired
}

fn journal_depends_on(
    predecessors: &HashMap<Hash, Vec<Hash>>,
    change: Hash,
    ancestor: Hash,
) -> bool {
    let mut pending = vec![change];
    let mut visited = HashSet::new();
    while let Some(hash) = pending.pop() {
        if !visited.insert(hash) {
            continue;
        }
        let Some(direct) = predecessors.get(&hash) else {
            continue;
        };
        if direct.contains(&ancestor) {
            return true;
        }
        pending.extend(direct.iter().copied());
    }
    false
}

fn change_depends_on<T: GraphTxnT>(txn: &T, change: Hash, ancestor: Hash) -> bool {
    if change == ancestor {
        return false;
    }
    let mut pending = vec![change];
    let mut visited = HashSet::new();
    while let Some(hash) = pending.pop() {
        if !visited.insert(hash) {
            continue;
        }
        let Ok(Some(id)) = txn.get_internal(&hash) else {
            continue;
        };
        let Ok(deps) = txn.get_change_deps(id) else {
            continue;
        };
        if deps.contains(&ancestor) {
            return true;
        }
        pending.extend(deps);
    }
    false
}

fn external_inode_position<T: GraphTxnT + TreeTxnT>(
    txn: &T,
    inode: Inode,
) -> Result<Option<Position<Hash>>, RepositoryError> {
    let Some(position) = txn
        .inode_position(inode)
        .map_err(|e| RepositoryError::Database(e.to_string()))?
    else {
        return Ok(None);
    };
    if position.change.is_root() {
        return Ok(None);
    }
    let Some(change) = txn
        .get_external(position.change)
        .map_err(|e| RepositoryError::Database(e.to_string()))?
    else {
        return Ok(None);
    };
    Ok(Some(Position::new(change, position.pos)))
}

fn push_unique(ops: &mut Vec<DeferredTreeOp>, op: DeferredTreeOp) {
    if !ops.iter().any(|existing| {
        existing.change == op.change && existing.inode == op.inode && existing.action == op.action
    }) {
        ops.push(op);
    }
}

fn external_position(
    change_hash: Hash,
    position: Position<Option<Hash>>,
) -> Option<Position<Hash>> {
    let change = match position.change {
        None => change_hash,
        Some(hash) if hash == Hash::NONE => return None,
        Some(hash) => hash,
    };
    Some(Position::new(change, position.pos))
}

fn current_path_for_position<T: GraphTxnT + TreeTxnT>(
    txn: &T,
    position: Position<Hash>,
) -> Result<Option<String>, RepositoryError> {
    let Some(internal_change) = txn
        .get_internal(&position.change)
        .map_err(|e| RepositoryError::Database(e.to_string()))?
    else {
        return Ok(None);
    };
    let Some(inode) = txn
        .position_inode(Position::new(internal_change, position.pos))
        .map_err(|e| RepositoryError::Database(e.to_string()))?
    else {
        return Ok(None);
    };
    txn.get_path(inode)
        .map_err(|e| RepositoryError::Database(e.to_string()))
}

fn push_delete_for_path<T: GraphTxnT + TreeTxnT>(
    txn: &T,
    change: Hash,
    path: &str,
    ops: &mut Vec<DeferredTreeOp>,
) -> Result<(), RepositoryError> {
    let Some(inode) = txn
        .get_inode(path)
        .map_err(|e| RepositoryError::Database(e.to_string()))?
    else {
        return Ok(());
    };
    let Some(position) = external_inode_position(txn, inode)? else {
        return Ok(());
    };
    let op = DeferredTreeOp {
        change,
        inode: position,
        baseline_path: Some(path.to_string()),
        action: DeferredTreeAction::Delete,
    };
    push_unique(ops, op);
    Ok(())
}

fn push_occupant_baseline<T: GraphTxnT + TreeTxnT>(
    txn: &T,
    activating_change: Hash,
    path: &str,
    exclude: Option<Position<Hash>>,
    ops: &mut Vec<DeferredTreeOp>,
) -> Result<(), RepositoryError> {
    let Some(inode) = txn
        .get_inode(path)
        .map_err(|e| RepositoryError::Database(e.to_string()))?
    else {
        return Ok(());
    };
    let Some(position) = external_inode_position(txn, inode)? else {
        return Ok(());
    };
    if Some(position) == exclude {
        return Ok(());
    }

    push_unique(
        ops,
        DeferredTreeOp {
            change: activating_change,
            inode: position,
            baseline_path: Some(path.to_string()),
            action: DeferredTreeAction::Delete,
        },
    );
    Ok(())
}

/// Collect view-scoped TREE lifecycle events before applying hunk-level TREE
/// mutations. Recording all adds, moves, and deletes, not just deferred Git
/// imports, keeps replay baselines valid when a later foreground change moves
/// the same inode on another view.
pub(super) fn collect_tree_ops<T: GraphTxnT + TreeTxnT>(
    txn: &T,
    change_hash: Hash,
    change: &Change,
    deleted_paths: &[String],
) -> Result<Vec<DeferredTreeOp>, RepositoryError> {
    let mut ops = Vec::new();

    for graph_op in change.hunks() {
        match graph_op {
            GraphOp::FileAdd {
                add_inode, path, ..
            }
            | GraphOp::DirAdd {
                add_inode, path, ..
            } => {
                // A foreign view may add the same path as a draft-only file.
                // Capture that current occupant so switching away removes it
                // and switching back can restore it without overwriting
                // TREE/REV_TREE.
                let added_position = Position::new(change_hash, add_inode.start);
                push_occupant_baseline(txn, change_hash, path, Some(added_position), &mut ops)?;
                push_unique(
                    &mut ops,
                    DeferredTreeOp {
                        change: change_hash,
                        inode: added_position,
                        baseline_path: None,
                        action: DeferredTreeAction::Set { path: path.clone() },
                    },
                );
            }
            GraphOp::FileMove { add, path, .. } => {
                let Some(external_position) = external_position(change_hash, add.inode) else {
                    continue;
                };
                push_occupant_baseline(txn, change_hash, path, Some(external_position), &mut ops)?;
                let op = DeferredTreeOp {
                    change: change_hash,
                    inode: external_position,
                    baseline_path: current_path_for_position(txn, external_position)?,
                    action: DeferredTreeAction::Set { path: path.clone() },
                };
                // Keep the event even when TREE already contains `path`.
                // `atomic move` updates tracking before the subsequent record,
                // and that recorded change must still supersede an older
                // deferred rename for views where it is visible.
                push_unique(&mut ops, op);
            }
            GraphOp::SolveNameConflict { name, path } => {
                if let Some(inode) = external_position(change_hash, name.inode) {
                    push_unique(
                        &mut ops,
                        DeferredTreeOp {
                            change: change_hash,
                            inode,
                            baseline_path: current_path_for_position(txn, inode)?
                                .or_else(|| Some(path.clone())),
                            action: if name.edges.is_empty() {
                                DeferredTreeAction::Set { path: path.clone() }
                            } else {
                                DeferredTreeAction::UnlinkName { path: path.clone() }
                            },
                        },
                    );
                }
            }
            GraphOp::FileDel { del, path, .. } | GraphOp::DirDel { del, path } => {
                if let Some(inode) = external_position(change_hash, del.inode) {
                    push_unique(
                        &mut ops,
                        DeferredTreeOp {
                            change: change_hash,
                            inode,
                            baseline_path: current_path_for_position(txn, inode)?
                                .or_else(|| Some(path.clone())),
                            action: DeferredTreeAction::Delete,
                        },
                    );
                } else {
                    push_delete_for_path(txn, change_hash, path, &mut ops)?;
                }
            }
            GraphOp::Edit {
                change: atomic_core::change::Atom::EdgeUpdate(delete),
                local,
                ..
            }
            | GraphOp::Replacement {
                change: delete,
                local,
                ..
            } if deleted_paths.iter().any(|path| path == &local.path) => {
                if let Some(inode) = external_position(change_hash, delete.inode) {
                    push_unique(
                        &mut ops,
                        DeferredTreeOp {
                            change: change_hash,
                            inode,
                            baseline_path: current_path_for_position(txn, inode)?
                                .or_else(|| Some(local.path.clone())),
                            action: DeferredTreeAction::Delete,
                        },
                    );
                }
            }
            _ => {}
        }
    }

    // Some import deletion paths are represented as content replacements,
    // not FileDel hunks, so preserve their TREE intent explicitly as well.
    for path in deleted_paths {
        push_delete_for_path(txn, change_hash, path, &mut ops)?;
    }

    Ok(ops)
}

/// Materialize explicit name selections into TREE using stable graph identity.
/// Recording and cross-view insertion share this operation; no content is copied.
pub(super) fn apply_name_selections<T: MutTxnT>(
    txn: &mut T,
    change_id: NodeId,
    change: &Change,
) -> Result<(), RepositoryError> {
    for op in change.hunks() {
        let GraphOp::SolveNameConflict { name, path } = op else {
            continue;
        };
        if !name.edges.is_empty() {
            continue;
        }
        let unresolved = || RepositoryError::InvalidOperation {
            message: format!("cannot resolve retained identity for {path}: name conflict"),
        };
        let inode_change = match name.inode.change {
            Some(hash) => txn
                .get_internal(&hash)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .ok_or_else(unresolved)?,
            None => change_id,
        };
        let inode = txn
            .position_inode(Position::new(inode_change, name.inode.pos))
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(unresolved)?;
        txn.put_tree(path, inode)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
    }
    Ok(())
}

impl Repository {
    fn deferred_tree_journal_path(&self) -> PathBuf {
        self.dot_dir.join(DEFERRED_TREE_JOURNAL)
    }

    pub(super) fn load_deferred_tree_journal(
        &self,
    ) -> Result<DeferredTreeJournal, RepositoryError> {
        let path = self.deferred_tree_journal_path();
        if !path.is_file() {
            return Ok(DeferredTreeJournal::default());
        }
        let journal: DeferredTreeJournal = serde_json::from_slice(&std::fs::read(path)?)?;
        if journal.version != DEFERRED_TREE_JOURNAL_VERSION {
            return Err(RepositoryError::Serialization(format!(
                "unsupported deferred TREE journal version {}",
                journal.version
            )));
        }
        Ok(journal)
    }

    /// Persist deferred operations atomically. The caller holds pristine's
    /// write transaction, which serializes journal writers across processes.
    pub(super) fn append_deferred_tree_ops<T: GraphTxnT + ViewTxnT>(
        &self,
        txn: &T,
        ops: &[DeferredTreeOp],
        current_view: &str,
    ) -> Result<(), RepositoryError> {
        if ops.is_empty() {
            return Ok(());
        }

        let mut journal = self.load_deferred_tree_journal()?;
        let mut changed = false;
        let view = txn
            .get_view(current_view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: current_view.to_string(),
            })?;
        let visible_ids = collect_visible_change_ids(txn, &view)?;
        let mut visible_hashes = HashSet::with_capacity(visible_ids.len());
        for id in visible_ids {
            if let Some(hash) = txn
                .get_external(id)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            {
                visible_hashes.insert(hash);
            }
        }

        // A Change is Atomic's visibility unit. Once one path operation joins
        // a deferred lifecycle, retain every path operation from that change;
        // otherwise a rename encoded as tracked-delete + new-inode-add would
        // lose its destination half during replay.
        for op in ops {
            if matches!(op.action, DeferredTreeAction::Set { .. }) {
                let mut predecessors: Vec<Hash> = journal
                    .ops
                    .iter()
                    .filter(|existing| {
                        existing.inode == op.inode
                            && existing.change != op.change
                            && visible_hashes.contains(&existing.change)
                            && matches!(existing.action, DeferredTreeAction::Set { .. })
                    })
                    .map(|existing| existing.change)
                    .collect();
                predecessors.sort();
                predecessors.dedup();
                if journal.predecessors.get(&op.change) != Some(&predecessors) {
                    journal.predecessors.insert(op.change, predecessors);
                    changed = true;
                }
            }
            if let Some(existing) = journal.ops.iter_mut().find(|existing| {
                existing.change == op.change
                    && existing.inode == op.inode
                    && existing.action == op.action
            }) {
                if existing.baseline_path.is_none() {
                    existing.baseline_path.clone_from(&op.baseline_path);
                    changed = true;
                }
            } else {
                journal.ops.push(op.clone());
                changed = true;
            }
        }

        if !changed {
            return Ok(());
        }

        let path = self.deferred_tree_journal_path();
        let mut temp = tempfile::NamedTempFile::new_in(&self.dot_dir)?;
        serde_json::to_writer_pretty(temp.as_file_mut(), &journal)?;
        temp.as_file_mut().write_all(b"\n")?;
        temp.as_file().sync_all()?;
        temp.persist(&path).map_err(|error| {
            RepositoryError::Io(std::io::Error::other(format!(
                "failed to persist deferred TREE journal: {}",
                error
            )))
        })?;
        self.sync_dot_dir()?;
        Ok(())
    }

    fn deferred_tree_alignment_pending_path(&self) -> PathBuf {
        self.dot_dir.join(DEFERRED_TREE_ALIGNMENT_PENDING)
    }

    fn write_deferred_tree_alignment_pending(
        &self,
        source_view: &str,
        target_view: &str,
    ) -> Result<(), RepositoryError> {
        let pending = PendingTreeAlignment {
            version: DEFERRED_TREE_JOURNAL_VERSION,
            source_view: source_view.to_string(),
            target_view: target_view.to_string(),
        };
        let path = self.deferred_tree_alignment_pending_path();
        let mut temp = tempfile::NamedTempFile::new_in(&self.dot_dir)?;
        serde_json::to_writer(temp.as_file_mut(), &pending)?;
        temp.as_file_mut().write_all(b"\n")?;
        temp.as_file().sync_all()?;
        temp.persist(&path).map_err(|error| {
            RepositoryError::Io(std::io::Error::other(format!(
                "failed to persist deferred TREE alignment marker: {}",
                error
            )))
        })?;
        self.sync_dot_dir()?;
        Ok(())
    }

    fn load_deferred_tree_alignment_pending(
        &self,
    ) -> Result<PendingTreeAlignment, RepositoryError> {
        let pending: PendingTreeAlignment =
            serde_json::from_slice(&std::fs::read(self.deferred_tree_alignment_pending_path())?)?;
        if pending.version != DEFERRED_TREE_JOURNAL_VERSION {
            return Err(RepositoryError::Serialization(format!(
                "unsupported deferred TREE alignment version {}",
                pending.version
            )));
        }
        Ok(pending)
    }

    fn lock_deferred_tree_alignment(&self) -> Result<std::fs::File, RepositoryError> {
        use fs2::FileExt;

        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.dot_dir.join(DEFERRED_TREE_ALIGNMENT_LOCK))?;
        lock.lock_exclusive()?;
        Ok(lock)
    }

    pub(super) fn clear_deferred_tree_alignment_pending(&self) -> Result<(), RepositoryError> {
        match std::fs::remove_file(self.deferred_tree_alignment_pending_path()) {
            Ok(()) => self.sync_dot_dir(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(RepositoryError::Io(error)),
        }
    }

    pub(super) fn has_pending_deferred_tree_alignment(&self) -> bool {
        self.deferred_tree_alignment_pending_path().is_file()
    }

    pub(super) fn apply_deferred_tree_ops_in_txn(
        &self,
        txn: &mut atomic_core::pristine::WriteTxn<'_>,
        journal: &DeferredTreeJournal,
        view_name: &str,
    ) -> Result<HashSet<String>, RepositoryError> {
        let view = txn
            .get_view(view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.to_string(),
            })?;
        let visible_ids = collect_visible_change_ids(&*txn, &view)?;
        let mut visible_hashes = HashSet::with_capacity(visible_ids.len());
        for change_id in visible_ids {
            if let Some(hash) = txn
                .get_external(change_id)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            {
                visible_hashes.insert(hash);
            }
        }

        let desired = desired_tree_paths(
            &journal.ops,
            &journal.predecessors,
            &visible_hashes,
            |change, ancestor| change_depends_on(&*txn, change, ancestor),
        );
        let mut current_paths: HashMap<Inode, Vec<String>> = HashMap::new();
        for entry in txn
            .iter_tree()
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            let (path, inode) = entry.map_err(|e| RepositoryError::Database(e.to_string()))?;
            current_paths.entry(inode).or_default().push(path);
        }
        let mut bindings = Vec::new();
        let mut affected_paths = HashSet::new();
        for (external_position, state) in desired {
            let Some(internal_change) = txn
                .get_internal(&external_position.change)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            else {
                continue;
            };
            let Some(inode) = txn
                .position_inode(Position::new(internal_change, external_position.pos))
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            else {
                continue;
            };
            let current = current_paths.remove(&inode).unwrap_or_default();
            affected_paths.extend(current.iter().cloned());
            affected_paths.extend(state.paths.iter().cloned());
            bindings.push((external_position, inode, current, state.paths));
        }

        // Remove each inode's own reverse claim and only remove the forward
        // entry when that inode is its current occupant. Deleting by path alone
        // can unbind a different same-name inode.
        for (_, inode, current_paths, _) in &bindings {
            for path in current_paths {
                txn.del_tree_binding(path, *inode)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
            }
        }

        // A path may have multiple visible inode claims. Reinsert all of them
        // in stable identity order: TREE retains one deterministic lookup
        // occupant while REV_TREE preserves every claim for conflict detection.
        bindings.sort_by_key(|(position, _, _, _)| *position);
        for (_, inode, _, desired_paths) in bindings {
            for path in desired_paths {
                txn.put_tree(&path, inode)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
            }
        }
        Ok(affected_paths)
    }

    pub(super) fn refresh_deferred_tree_projection(
        &self,
        view_name: &str,
    ) -> Result<HashSet<String>, RepositoryError> {
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let journal = self.load_deferred_tree_journal()?;
        let affected = self.apply_deferred_tree_ops_in_txn(&mut txn, &journal, view_name)?;
        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(affected)
    }

    /// Recover a switch interrupted between TREE alignment and publishing the
    /// current-view pointer. The advisory lock is released by the OS on process
    /// exit, so a concurrent opener waits for a live switch and only performs
    /// recovery when the marker survives that lock handoff.
    pub(super) fn recover_pending_deferred_tree_alignment(
        &mut self,
    ) -> Result<(), RepositoryError> {
        if self.is_sandbox || !self.has_pending_deferred_tree_alignment() {
            return Ok(());
        }

        let _alignment_lock = self.lock_deferred_tree_alignment()?;
        if !self.has_pending_deferred_tree_alignment() {
            return Ok(());
        }
        let pending = self.load_deferred_tree_alignment_pending()?;
        // Capture both view path sets before restoring the source projection;
        // TREE is a selected-view index and target-only paths may no longer be
        // discoverable after alignment.
        let source_files = self.visible_file_paths(&pending.source_view)?;
        let target_files = self.visible_file_paths(&pending.target_view)?;
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let journal = self.load_deferred_tree_journal()?;
        self.apply_deferred_tree_ops_in_txn(&mut txn, &journal, &pending.source_view)?;
        self.write_current_view(&pending.source_view)?;
        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        self.current_view = pending.source_view.clone();

        // The marker spans filesystem materialization as well as TREE/pointer
        // publication. Remove any target-only tracked files that may have been
        // written before interruption, then reconstruct the source view. If
        // either step fails, retain the marker for the next writable open.
        for path in target_files.difference(&source_files) {
            let abs = self.root.join(path);
            if abs.is_file() {
                std::fs::remove_file(&abs)?;
            }
        }
        self.materialize()?;
        let source_workspace = workspace_path(&self.dot_dir, &pending.source_view);
        if source_workspace.is_dir() {
            self.restore_workspace_to_working_copy(&source_workspace);
        }
        self.clear_deferred_tree_alignment_pending()?;
        Ok(())
    }

    /// Align TREE and publish the view pointer as one recoverable transition.
    /// The database write lock is held while the pointer is written, and the
    /// pending marker lets the next writable open reconcile either side after
    /// a process crash.
    pub(super) fn align_deferred_tree_and_publish_view(
        &mut self,
        view_name: &str,
        retain_marker_for_materialization: bool,
    ) -> Result<HashSet<String>, RepositoryError> {
        let _alignment_lock = self.lock_deferred_tree_alignment()?;
        // `current_view` can intentionally be scoped to a background target
        // via set_current_view_in_memory(). Read the persisted pointer only
        // after taking the alignment lock: it owns the materialized TREE and
        // is therefore the only valid rollback source for this transition.
        let old_view = Self::read_current_view(&self.dot_dir)?;
        let mut txn = match self.pristine.write_txn() {
            Ok(txn) => txn,
            Err(error) => return Err(RepositoryError::Database(error.to_string())),
        };
        let journal = self.load_deferred_tree_journal()?;
        // Publish the marker only after taking the database write lock. Any
        // opener that observes it must wait for this transaction. If the
        // marker survives, recovery restores the source view.
        self.write_deferred_tree_alignment_pending(&old_view, view_name)?;
        let affected_paths =
            match self.apply_deferred_tree_ops_in_txn(&mut txn, &journal, view_name) {
                Ok(paths) => paths,
                Err(error) => {
                    let _ = self.clear_deferred_tree_alignment_pending();
                    return Err(error);
                }
            };

        if let Err(error) = self.write_current_view(view_name) {
            let _ = self.clear_deferred_tree_alignment_pending();
            return Err(error);
        }

        if let Err(error) = txn.commit() {
            // The DB transaction did not publish. Restore the pointer while
            // retaining the marker if restoration itself fails, so the next
            // writable open can reconcile from the persisted pointer.
            if self.write_current_view(&old_view).is_ok() {
                let _ = self.clear_deferred_tree_alignment_pending();
            }
            return Err(RepositoryError::Database(error.to_string()));
        }

        self.current_view = view_name.to_string();
        if !retain_marker_for_materialization {
            self.clear_deferred_tree_alignment_pending()?;
        }
        Ok(affected_paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::types::ChangePosition;

    fn hash(label: &str) -> Hash {
        Hash::of(label.as_bytes())
    }

    #[test]
    fn planner_handles_rename_chains_and_view_visibility() {
        let inode = Position::new(hash("creator"), ChangePosition::new(7));
        let first = hash("move-a-b");
        let second = hash("move-b-c");
        let ops = vec![
            DeferredTreeOp {
                change: first,
                inode,
                baseline_path: Some("a.txt".into()),
                action: DeferredTreeAction::Set {
                    path: "b.txt".into(),
                },
            },
            DeferredTreeOp {
                change: second,
                inode,
                baseline_path: Some("stale-baseline.txt".into()),
                action: DeferredTreeAction::Set {
                    path: "c.txt".into(),
                },
            },
        ];

        let base = desired_tree_paths(&ops, &HashMap::new(), &HashSet::new(), |_, _| false);
        assert_eq!(base[&inode].paths, BTreeSet::from(["a.txt".into()]));

        let visible = HashSet::from([first, second]);
        let moved = desired_tree_paths(&ops, &HashMap::new(), &visible, |change, ancestor| {
            change == second && ancestor == first
        });
        assert_eq!(moved[&inode].paths, BTreeSet::from(["c.txt".into()]));
    }

    #[test]
    fn planner_applies_visible_deletion_after_move() {
        let inode = Position::new(hash("creator"), ChangePosition::new(9));
        let moved = hash("move");
        let deleted = hash("delete");
        let ops = vec![
            DeferredTreeOp {
                change: moved,
                inode,
                baseline_path: Some("old.txt".into()),
                action: DeferredTreeAction::Set {
                    path: "new.txt".into(),
                },
            },
            DeferredTreeOp {
                change: deleted,
                inode,
                baseline_path: Some("new.txt".into()),
                action: DeferredTreeAction::Delete,
            },
        ];

        let desired = desired_tree_paths(
            &ops,
            &HashMap::new(),
            &HashSet::from([moved, deleted]),
            |change, ancestor| change == deleted && ancestor == moved,
        );
        assert!(desired[&inode].paths.is_empty());
    }

    #[test]
    fn planner_keeps_foreground_rename_after_deferred_event() {
        let inode = Position::new(hash("creator"), ChangePosition::new(11));
        let deferred = hash("deferred-target-rename");
        let foreground = hash("foreground-source-rename");
        let ops = vec![
            DeferredTreeOp {
                change: deferred,
                inode,
                baseline_path: Some("base.txt".into()),
                action: DeferredTreeAction::Set {
                    path: "target.txt".into(),
                },
            },
            DeferredTreeOp {
                change: foreground,
                inode,
                // This may already reflect tracking's new path. It must not
                // replace the first event's baseline.
                baseline_path: Some("source.txt".into()),
                action: DeferredTreeAction::Set {
                    path: "source.txt".into(),
                },
            },
        ];

        let source = desired_tree_paths(
            &ops,
            &HashMap::new(),
            &HashSet::from([foreground]),
            |_, _| false,
        );
        assert_eq!(source[&inode].paths, BTreeSet::from(["source.txt".into()]));

        let target =
            desired_tree_paths(&ops, &HashMap::new(), &HashSet::from([deferred]), |_, _| {
                false
            });
        assert_eq!(target[&inode].paths, BTreeSet::from(["target.txt".into()]));
    }

    #[test]
    fn planner_preserves_concurrent_rename_destinations_for_same_inode() {
        let inode = Position::new(hash("creator"), ChangePosition::new(13));
        let left = hash("rename-left");
        let right = hash("rename-right");
        let ops = vec![
            DeferredTreeOp {
                change: left,
                inode,
                baseline_path: Some("original.txt".into()),
                action: DeferredTreeAction::Set {
                    path: "left.txt".into(),
                },
            },
            DeferredTreeOp {
                change: right,
                inode,
                baseline_path: Some("original.txt".into()),
                action: DeferredTreeAction::Set {
                    path: "right.txt".into(),
                },
            },
        ];

        let desired = desired_tree_paths(
            &ops,
            &HashMap::new(),
            &HashSet::from([left, right]),
            |_, _| false,
        );
        assert_eq!(
            desired[&inode].paths,
            BTreeSet::from(["left.txt".into(), "right.txt".into()])
        );
    }

    #[test]
    fn planner_preserves_all_visible_inodes_for_same_path() {
        let target_inode = Position::new(hash("target-creator"), ChangePosition::new(3));
        let source_inode = Position::new(hash("source-creator"), ChangePosition::new(5));
        let target_add = hash("target-add");
        let source_add = hash("source-add");
        let ops = vec![
            DeferredTreeOp {
                change: target_add,
                inode: target_inode,
                baseline_path: None,
                action: DeferredTreeAction::Set {
                    path: "same.txt".into(),
                },
            },
            DeferredTreeOp {
                change: source_add,
                inode: source_inode,
                baseline_path: None,
                action: DeferredTreeAction::Set {
                    path: "same.txt".into(),
                },
            },
        ];

        let target = desired_tree_paths(
            &ops,
            &HashMap::new(),
            &HashSet::from([target_add]),
            |_, _| false,
        );
        assert_eq!(
            target[&target_inode].paths,
            BTreeSet::from(["same.txt".into()])
        );
        assert!(target[&source_inode].paths.is_empty());

        let overlay = desired_tree_paths(
            &ops,
            &HashMap::new(),
            &HashSet::from([target_add, source_add]),
            |_, _| false,
        );
        assert_eq!(
            overlay[&target_inode].paths,
            BTreeSet::from(["same.txt".into()])
        );
        assert_eq!(
            overlay[&source_inode].paths,
            BTreeSet::from(["same.txt".into()])
        );
    }
}
