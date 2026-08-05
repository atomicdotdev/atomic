use super::*;

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::Write;

const DEFERRED_TREE_JOURNAL: &str = "deferred-tree-ops.json";
const DEFERRED_TREE_JOURNAL_VERSION: u32 = 1;
const DEFERRED_TREE_ALIGNMENT_PENDING: &str = "deferred-tree-alignment.pending";
const DEFERRED_TREE_ALIGNMENT_LOCK: &str = "deferred-tree-alignment.lock";

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum DeferredTreeAction {
    Set { path: String },
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(super) struct DeferredTreeOp {
    /// Change whose visibility activates this TREE operation.
    change: Hash,
    /// Stable inode position, stored with an external change hash so the
    /// journal never depends on process-local lookup state.
    inode: Position<Hash>,
    /// Path visible before this inode's first journaled operation. Only the
    /// first event for an inode uses this baseline; later events are selected
    /// solely by change visibility.
    baseline_path: Option<String>,
    action: DeferredTreeAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeferredTreeJournal {
    version: u32,
    #[serde(default)]
    ops: Vec<DeferredTreeOp>,
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
        }
    }
}

#[derive(Debug, Clone)]
struct DesiredTreePath {
    desired_path: Option<String>,
    /// Journal order of the visible Set that currently claims this path.
    /// A visible Set outranks an inherited baseline, and later visible Sets
    /// resolve two view overlays that independently created the same path.
    last_set_order: Option<usize>,
}

fn desired_tree_paths(
    ops: &[DeferredTreeOp],
    visible_changes: &HashSet<Hash>,
) -> HashMap<Position<Hash>, DesiredTreePath> {
    let mut desired = HashMap::new();

    for (order, op) in ops.iter().enumerate() {
        let state = desired.entry(op.inode).or_insert_with(|| DesiredTreePath {
            desired_path: op.baseline_path.clone(),
            last_set_order: None,
        });
        if !visible_changes.contains(&op.change) {
            continue;
        }

        match &op.action {
            DeferredTreeAction::Set { path } => {
                state.desired_path = Some(path.clone());
                state.last_set_order = Some(order);
            }
            DeferredTreeAction::Delete => {
                state.desired_path = None;
                state.last_set_order = None;
            }
        }
    }

    // TREE is a one-to-one path↔inode index, while two overlay-visible changes
    // can independently add the same path. Match the lifecycle order Atomic
    // established when those changes were recorded/imported: the latest
    // visible Set owns the path. Baseline-only duplicates remain untouched so
    // apply_deferred_tree_ops_in_txn still fails closed on corrupt TREE state.
    let mut claims: HashMap<String, Vec<(Position<Hash>, Option<usize>)>> = HashMap::new();
    for (inode, state) in &desired {
        if let Some(path) = &state.desired_path {
            claims
                .entry(path.clone())
                .or_default()
                .push((*inode, state.last_set_order));
        }
    }
    for claimants in claims.into_values().filter(|claimants| claimants.len() > 1) {
        let Some(max_order) = claimants.iter().filter_map(|(_, order)| *order).max() else {
            continue;
        };
        if claimants
            .iter()
            .filter(|(_, order)| *order == Some(max_order))
            .count()
            != 1
        {
            continue;
        }
        for (inode, order) in claimants {
            if order != Some(max_order) {
                desired
                    .get_mut(&inode)
                    .expect("claimant exists")
                    .desired_path = None;
            }
        }
    }

    desired
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

fn remember_op_paths(op: &DeferredTreeOp, paths: &mut HashSet<String>) {
    if let Some(path) = &op.baseline_path {
        paths.insert(path.clone());
    }
    if let DeferredTreeAction::Set { path } = &op.action {
        paths.insert(path.clone());
    }
}

fn op_touches_paths(op: &DeferredTreeOp, paths: &HashSet<String>) -> bool {
    op.baseline_path
        .as_ref()
        .is_some_and(|path| paths.contains(path))
        || matches!(
            &op.action,
            DeferredTreeAction::Set { path } if paths.contains(path)
        )
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

fn inode_is_visible_on_another_view<T: GraphTxnT + TreeTxnT + ViewTxnT>(
    txn: &T,
    position: Position<Hash>,
    current_view: &str,
) -> Result<bool, RepositoryError> {
    let Some(internal_change) = txn
        .get_internal(&position.change)
        .map_err(|e| RepositoryError::Database(e.to_string()))?
    else {
        return Ok(false);
    };
    for name in txn
        .list_views()
        .map_err(|e| RepositoryError::Database(e.to_string()))?
    {
        if name == current_view {
            continue;
        }
        let Some(view) = txn
            .get_view(&name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        else {
            continue;
        };
        if collect_visible_change_ids(txn, &view)?.contains(&internal_change) {
            return Ok(true);
        }
    }
    Ok(false)
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

impl Repository {
    fn deferred_tree_journal_path(&self) -> PathBuf {
        self.dot_dir.join(DEFERRED_TREE_JOURNAL)
    }

    fn load_deferred_tree_journal(&self) -> Result<DeferredTreeJournal, RepositoryError> {
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
    pub(super) fn append_deferred_tree_ops<T: GraphTxnT + TreeTxnT + ViewTxnT>(
        &self,
        txn: &T,
        ops: &[DeferredTreeOp],
        current_view: &str,
        include_new_inodes: bool,
    ) -> Result<(), RepositoryError> {
        if ops.is_empty() {
            return Ok(());
        }

        let mut journal = self.load_deferred_tree_journal()?;
        let mut changed = false;
        let mut shared_creators = HashMap::new();
        let tracked_inodes: HashSet<Position<Hash>> =
            journal.ops.iter().map(|op| op.inode).collect();
        let mut tracked_paths = HashSet::new();
        for op in &journal.ops {
            remember_op_paths(op, &mut tracked_paths);
        }
        let mut batch_participates = include_new_inodes;
        if !batch_participates {
            for op in ops {
                let inode_is_tracked = tracked_inodes.contains(&op.inode);
                let path_is_tracked = op_touches_paths(op, &tracked_paths);
                let inode_is_shared = if inode_is_tracked || path_is_tracked {
                    false
                } else if let Some(shared) = shared_creators.get(&op.inode.change) {
                    *shared
                } else {
                    let shared = inode_is_visible_on_another_view(txn, op.inode, current_view)?;
                    shared_creators.insert(op.inode.change, shared);
                    shared
                };
                if inode_is_tracked || path_is_tracked || inode_is_shared {
                    batch_participates = true;
                    break;
                }
            }
        }
        if !batch_participates {
            return Ok(());
        }

        // A Change is Atomic's visibility unit. Once one path operation joins
        // a deferred lifecycle, retain every path operation from that change;
        // otherwise a rename encoded as tracked-delete + new-inode-add would
        // lose its destination half during replay.
        for op in ops {
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
            .read(true)
            .write(true)
            .open(self.dot_dir.join(DEFERRED_TREE_ALIGNMENT_LOCK))?;
        lock.lock_exclusive()?;
        Ok(lock)
    }

    fn clear_deferred_tree_alignment_pending(&self) -> Result<(), RepositoryError> {
        match std::fs::remove_file(self.deferred_tree_alignment_pending_path()) {
            Ok(()) => self.sync_dot_dir(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(RepositoryError::Io(error)),
        }
    }

    pub(super) fn has_pending_deferred_tree_alignment(&self) -> bool {
        self.deferred_tree_alignment_pending_path().is_file()
    }

    fn apply_deferred_tree_ops_in_txn(
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

        let desired = desired_tree_paths(&journal.ops, &visible_hashes);
        let mut updates = Vec::new();
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
            let current_path = txn
                .get_path(inode)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            if current_path == state.desired_path {
                continue;
            }
            if let Some(path) = &current_path {
                affected_paths.insert(path.clone());
            }
            if let Some(path) = &state.desired_path {
                affected_paths.insert(path.clone());
            }
            updates.push((inode, current_path, state.desired_path));
        }

        // Remove all stale sources first so rename chains and swaps are safe.
        for (_, current_path, _) in &updates {
            if let Some(path) = current_path {
                txn.del_tree(path)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
            }
        }

        // Never overwrite an unrelated destination: put_tree would update
        // TREE but leave the old occupant's REV_TREE entry stale.
        for (inode, _, desired_path) in &updates {
            let Some(path) = desired_path else {
                continue;
            };
            if let Some(occupant) = txn
                .get_inode(path)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            {
                if occupant != *inode {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "cannot replay deferred path to '{}': path is owned by another inode",
                            path
                        ),
                    });
                }
            }
        }

        for (inode, _, desired_path) in updates {
            if let Some(path) = desired_path {
                txn.put_tree(&path, inode)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
            }
        }
        Ok(affected_paths)
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
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let journal = self.load_deferred_tree_journal()?;
        self.apply_deferred_tree_ops_in_txn(&mut txn, &journal, &pending.source_view)?;
        self.write_current_view(&pending.source_view)?;
        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        self.current_view = pending.source_view;
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
        // Clearing the marker is the commit point for this recoverable
        // transition. Propagate cleanup or directory-sync failures so a switch
        // is never reported durable while recovery may still roll it back.
        self.clear_deferred_tree_alignment_pending()?;
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

        let base = desired_tree_paths(&ops, &HashSet::new());
        assert_eq!(base[&inode].desired_path.as_deref(), Some("a.txt"));

        let visible = HashSet::from([first, second]);
        let moved = desired_tree_paths(&ops, &visible);
        assert_eq!(moved[&inode].desired_path.as_deref(), Some("c.txt"));
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

        let desired = desired_tree_paths(&ops, &HashSet::from([moved, deleted]));
        assert_eq!(desired[&inode].desired_path, None);
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

        let source = desired_tree_paths(&ops, &HashSet::from([foreground]));
        assert_eq!(source[&inode].desired_path.as_deref(), Some("source.txt"));

        let target = desired_tree_paths(&ops, &HashSet::from([deferred]));
        assert_eq!(target[&inode].desired_path.as_deref(), Some("target.txt"));
    }

    #[test]
    fn planner_chooses_latest_visible_inode_for_same_path() {
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

        let target = desired_tree_paths(&ops, &HashSet::from([target_add]));
        assert_eq!(
            target[&target_inode].desired_path.as_deref(),
            Some("same.txt")
        );
        assert_eq!(target[&source_inode].desired_path, None);

        let overlay = desired_tree_paths(&ops, &HashSet::from([target_add, source_add]));
        assert_eq!(overlay[&target_inode].desired_path, None);
        assert_eq!(
            overlay[&source_inode].desired_path.as_deref(),
            Some("same.txt")
        );
    }
}
