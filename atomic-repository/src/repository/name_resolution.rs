use super::*;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use atomic_core::change::{EdgeUpdate, GraphOp, Insertion};
use atomic_core::pristine::{
    GraphTxnT, GraphVisibilityClosure, PathClaimEntry, PathClaimEvent, PathClaimId, PathClaimKind,
    PathClaimState, PathClaimTxnT, TreeTxnT, ViewMembershipSet, ViewState, ViewTxnT,
};
use atomic_core::types::{EdgeFlags, GraphNode};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ProjectedPathClaim {
    pub(super) path: String,
    pub(super) inode: Inode,
    pub(super) position: Position<NodeId>,
    pub(super) kind: PathClaimKind,
    pub(super) claims: Vec<PathClaimId>,
    pub(super) event_changes: Vec<NodeId>,
}

impl ProjectedPathClaim {
    pub(super) fn is_directory(&self) -> bool {
        self.kind == PathClaimKind::Directory
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ProjectedNameConflict {
    pub(super) paths: Vec<String>,
    pub(super) sides: Vec<ProjectedPathClaim>,
}

impl ProjectedNameConflict {
    pub(super) fn sides_at_path(&self, path: &str) -> Vec<&ProjectedPathClaim> {
        self.sides.iter().filter(|side| side.path == path).collect()
    }

    pub(super) fn is_rename_conflict(&self) -> bool {
        let unique_paths: HashSet<&str> =
            self.sides.iter().map(|side| side.path.as_str()).collect();
        let unique_inodes: HashSet<Inode> = self.sides.iter().map(|side| side.inode).collect();
        unique_paths.len() > 1 && unique_inodes.len() == 1
    }
}

#[derive(Debug, Clone, Default)]
pub(super) struct ReducedPathClaims {
    pub(super) present: Vec<ProjectedPathClaim>,
    pub(super) absent: Vec<super::deferred_tree::ProjectedAbsent>,
    pub(super) conflicts: HashMap<String, ProjectedNameConflict>,
}

pub(super) fn path_claim_visibility_for_view<T>(
    txn: &T,
    store: &ChangeStore,
    view: &ViewState,
    full_visibility: &GraphVisibilityClosure,
) -> Result<GraphVisibilityClosure, RepositoryError>
where
    T: ViewTxnT + PathClaimTxnT,
{
    let entries = txn
        .iter_path_claims()
        .map_err(|error| RepositoryError::Database(error.to_string()))?;
    path_claim_visibility_for_view_with_entries(txn, store, view, full_visibility, &entries)
}

pub(super) fn path_claim_visibility_for_view_with_entries<T>(
    txn: &T,
    store: &ChangeStore,
    view: &ViewState,
    full_visibility: &GraphVisibilityClosure,
    entries: &[PathClaimEntry],
) -> Result<GraphVisibilityClosure, RepositoryError>
where
    T: ViewTxnT,
{
    let mut direct = HashSet::new();
    for entry in txn
        .iter_changes(view, 0)
        .map_err(|error| RepositoryError::Database(error.to_string()))?
    {
        let (_, change_id, _) =
            entry.map_err(|error| RepositoryError::Database(error.to_string()))?;
        direct.insert(change_id);
    }
    let path_event_changes: HashSet<NodeId> = entries
        .iter()
        .map(|entry| entry.event.event_change)
        .collect();
    let mut membership = Vec::new();
    for change_id in full_visibility.iter_dependency_first().copied() {
        let inherited_name_resolution = !direct.contains(&change_id)
            && path_event_changes.contains(&change_id)
            && is_name_resolution_change(txn, store, change_id)?;
        if !inherited_name_resolution {
            membership.push(change_id);
        }
    }
    let membership = ViewMembershipSet::from_ordered(membership);
    GraphVisibilityClosure::try_from_membership(txn, &membership)
        .map_err(|error| RepositoryError::Database(error.to_string()))
}

fn is_name_resolution_change<T: GraphTxnT>(
    txn: &T,
    store: &ChangeStore,
    change_id: NodeId,
) -> Result<bool, RepositoryError> {
    let hash = txn
        .get_external(change_id)
        .map_err(|error| RepositoryError::Database(error.to_string()))?
        .ok_or_else(|| {
            RepositoryError::Database(format!(
                "path-claim change {} has no external hash",
                change_id.get()
            ))
        })?;
    let change = store.load_change(&hash).map_err(|error| {
        RepositoryError::Database(format!(
            "cannot inspect path-claim change {}: {}",
            hash, error
        ))
    })?;
    Ok(change.hunks().iter().any(|operation| {
        matches!(
            operation,
            GraphOp::SolveNameConflict { .. } | GraphOp::UnsolveNameConflict { .. }
        )
    }))
}

pub(super) fn path_claim_events_for_change<T>(
    txn: &T,
    change_id: NodeId,
    change: &Change,
) -> Result<Vec<PathClaimEntry>, RepositoryError>
where
    T: GraphTxnT + TreeTxnT + PathClaimTxnT,
{
    let prior = txn
        .iter_path_claims()
        .map_err(|error| RepositoryError::Database(error.to_string()))?;
    path_claim_events_for_change_with_prior(txn, change_id, change, &prior)
}

pub(super) fn path_claim_events_for_change_with_prior<T>(
    txn: &T,
    change_id: NodeId,
    change: &Change,
    prior: &[PathClaimEntry],
) -> Result<Vec<PathClaimEntry>, RepositoryError>
where
    T: GraphTxnT + TreeTxnT,
{
    path_claim_events_for_change_with_prior_and_inodes(
        txn,
        change_id,
        change,
        prior,
        &HashMap::new(),
    )
}

pub(super) fn path_claim_events_for_change_with_prior_and_inodes<T>(
    txn: &T,
    change_id: NodeId,
    change: &Change,
    prior: &[PathClaimEntry],
    recovered_inodes: &HashMap<Position<NodeId>, Inode>,
) -> Result<Vec<PathClaimEntry>, RepositoryError>
where
    T: GraphTxnT + TreeTxnT,
{
    let mut entries = Vec::new();
    let mut known = prior.to_vec();
    let mut operation_index = 0u32;

    for operation in change.hunks() {
        let entries_before = entries.len();
        match operation {
            GraphOp::FileAdd {
                add_name,
                add_inode,
                path,
                ..
            } => add_insertion_claims(
                txn,
                change_id,
                path,
                PathClaimKind::File,
                Position::new(change_id, add_inode.start),
                add_name,
                &mut operation_index,
                &mut entries,
            )?,
            GraphOp::DirAdd {
                add_name,
                add_inode,
                path,
            } => add_insertion_claims(
                txn,
                change_id,
                path,
                PathClaimKind::Directory,
                Position::new(change_id, add_inode.start),
                add_name,
                &mut operation_index,
                &mut entries,
            )?,
            GraphOp::FileMove { del, add, path } => {
                let claimant = internalize_position(txn, change_id, add.inode)?;
                let kind = claimant_kind_from_prior(txn, claimant, recovered_inodes, &known)?;
                add_edge_update_claims(
                    txn,
                    change_id,
                    None,
                    kind,
                    PathClaimState::Dead,
                    del,
                    false,
                    &known,
                    recovered_inodes,
                    &mut operation_index,
                    &mut entries,
                )?;
                add_insertion_claims(
                    txn,
                    change_id,
                    path,
                    kind,
                    claimant,
                    add,
                    &mut operation_index,
                    &mut entries,
                )?;
            }
            GraphOp::FileDel { del, path, .. } => add_edge_update_claims(
                txn,
                change_id,
                Some(path),
                PathClaimKind::File,
                PathClaimState::Dead,
                del,
                false,
                &known,
                recovered_inodes,
                &mut operation_index,
                &mut entries,
            )?,
            GraphOp::DirDel { del, path } => add_edge_update_claims(
                txn,
                change_id,
                Some(path),
                PathClaimKind::Directory,
                PathClaimState::Dead,
                del,
                false,
                &known,
                recovered_inodes,
                &mut operation_index,
                &mut entries,
            )?,
            GraphOp::FileUndel { undel, path, .. } => add_edge_update_claims(
                txn,
                change_id,
                Some(path),
                PathClaimKind::File,
                PathClaimState::Alive,
                undel,
                false,
                &known,
                recovered_inodes,
                &mut operation_index,
                &mut entries,
            )?,
            GraphOp::DirUndel { undel, path } => add_edge_update_claims(
                txn,
                change_id,
                Some(path),
                PathClaimKind::Directory,
                PathClaimState::Alive,
                undel,
                false,
                &known,
                recovered_inodes,
                &mut operation_index,
                &mut entries,
            )?,
            GraphOp::SolveNameConflict { name, path } => add_edge_update_claims(
                txn,
                change_id,
                Some(path),
                PathClaimKind::File,
                PathClaimState::Dead,
                name,
                true,
                &known,
                recovered_inodes,
                &mut operation_index,
                &mut entries,
            )?,
            GraphOp::UnsolveNameConflict { name, path } => add_edge_update_claims(
                txn,
                change_id,
                Some(path),
                PathClaimKind::File,
                PathClaimState::Alive,
                name,
                true,
                &known,
                recovered_inodes,
                &mut operation_index,
                &mut entries,
            )?,
            _ => {}
        }
        known.extend_from_slice(&entries[entries_before..]);
    }

    Ok(entries)
}

pub(super) fn reduce_path_claims<T>(
    txn: &T,
    visibility: &GraphVisibilityClosure,
) -> Result<ReducedPathClaims, RepositoryError>
where
    T: GraphTxnT + TreeTxnT + PathClaimTxnT,
{
    let entries = txn
        .iter_path_claims()
        .map_err(|error| RepositoryError::Database(error.to_string()))?;
    reduce_path_claim_entries(txn, visibility, &entries)
}

pub(super) fn reduce_path_claim_entries<T>(
    txn: &T,
    visibility: &GraphVisibilityClosure,
    entries: &[PathClaimEntry],
) -> Result<ReducedPathClaims, RepositoryError>
where
    T: GraphTxnT + TreeTxnT,
{
    reduce_path_claim_entries_with_inodes(txn, visibility, entries, &HashMap::new())
}

pub(super) fn reduce_path_claim_entries_with_inodes<T>(
    txn: &T,
    visibility: &GraphVisibilityClosure,
    entries: &[PathClaimEntry],
    recovered_inodes: &HashMap<Position<NodeId>, Inode>,
) -> Result<ReducedPathClaims, RepositoryError>
where
    T: GraphTxnT + TreeTxnT,
{
    let mut by_logical_claim = BTreeMap::<(String, Position<NodeId>), Vec<PathClaimEvent>>::new();
    for entry in entries.iter().cloned() {
        if visibility.contains(entry.event.event_change) {
            by_logical_claim
                .entry((entry.path, entry.event.claim.claimant))
                .or_default()
                .push(entry.event);
        }
    }

    let mut dependency_memo = HashMap::new();
    let mut alive = Vec::new();
    let mut absent = Vec::new();
    let mut ambiguous = HashSet::<(String, Inode)>::new();
    let debug_reduce = std::env::var("ATOMIC_DEBUG_REDUCE").is_ok();
    let reduce_start = std::time::Instant::now();
    let mut dep_calls = 0u64;
    let mut max_events = 0u64;

    for ((path, position), events) in by_logical_claim {
        if debug_reduce && events.len() as u64 > max_events {
            max_events = events.len() as u64;
            eprintln!(
                "REDUCE group path={path:?} events={} ms={}",
                events.len(),
                reduce_start.elapsed().as_millis()
            );
        }
        let mut maximal = Vec::new();
        for candidate in &events {
            let mut superseded = false;
            for other in &events {
                if candidate == other {
                    continue;
                }
                if candidate.event_change != other.event_change {
                    dep_calls += 1;
                }
                if (candidate.event_change == other.event_change
                    && candidate.operation_index < other.operation_index)
                    || (candidate.event_change == candidate.claim.introduced_by
                        && other.event_change != candidate.event_change
                        && other.claim == candidate.claim)
                    || (candidate.event_change != other.event_change
                        && change_depends_on(
                            txn,
                            other.event_change,
                            candidate.event_change,
                            &mut dependency_memo,
                        )?)
                {
                    superseded = true;
                    break;
                }
            }
            if !superseded {
                maximal.push(*candidate);
            }
        }
        maximal.sort();
        maximal.dedup();
        if maximal.is_empty() {
            continue;
        }

        let inode = position_inode(txn, position, recovered_inodes)?.ok_or_else(|| {
            RepositoryError::InvalidOperation {
                message: format!(
                    "path claim '{}' has no inode for graph position {}",
                    path, position
                ),
            }
        })?;
        let kinds: BTreeSet<PathClaimKind> = maximal.iter().map(|event| event.kind).collect();
        if kinds.len() != 1 {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "path claim '{}' changes inode {} between file and directory",
                    path,
                    inode.get()
                ),
            });
        }
        let kind = *kinds.iter().next().expect("non-empty maximal claim kinds");
        let has_alive = maximal
            .iter()
            .any(|event| event.state == PathClaimState::Alive);
        let has_dead = maximal
            .iter()
            .any(|event| event.state == PathClaimState::Dead);
        if has_alive && has_dead {
            ambiguous.insert((path.clone(), inode));
            if debug_reduce {
                let alive_count = maximal
                    .iter()
                    .filter(|e| e.state == PathClaimState::Alive)
                    .count();
                eprintln!(
                    "REDUCE ambiguous path={path:?} inode={} alive_events={} dead_events={}",
                    inode.get(),
                    alive_count,
                    maximal.len() - alive_count
                );
            }
        }

        let alive_events: Vec<_> = maximal
            .iter()
            .filter(|event| event.state == PathClaimState::Alive)
            .copied()
            .collect();
        if !alive_events.is_empty() {
            let mut claims: Vec<_> = alive_events.iter().map(|event| event.claim).collect();
            claims.sort();
            claims.dedup();
            let mut event_changes: Vec<_> = alive_events
                .iter()
                .map(|event| event.event_change)
                .collect();
            event_changes.sort();
            event_changes.dedup();
            alive.push(ProjectedPathClaim {
                path,
                inode,
                position,
                kind,
                claims,
                event_changes,
            });
        } else {
            let mut deleted_by = Vec::new();
            for event in maximal {
                let hash = txn
                    .get_external(event.event_change)
                    .map_err(|error| RepositoryError::Database(error.to_string()))?
                    .ok_or_else(|| {
                        RepositoryError::Database(format!(
                            "path-claim event change {} has no external hash",
                            event.event_change.get()
                        ))
                    })?;
                deleted_by.push(hash);
            }
            deleted_by.sort();
            deleted_by.dedup();
            absent.push(super::deferred_tree::ProjectedAbsent {
                path,
                inode,
                position,
                directory: kind == PathClaimKind::Directory,
                deleted_by,
            });
        }
    }

    alive.sort_by(|left, right| {
        (&left.path, left.inode.get()).cmp(&(&right.path, right.inode.get()))
    });

    let mut path_degree = HashMap::<String, usize>::new();
    let mut inode_degree = HashMap::<Inode, usize>::new();
    for side in &alive {
        *path_degree.entry(side.path.clone()).or_default() += 1;
        *inode_degree.entry(side.inode).or_default() += 1;
    }

    let conflicted: HashSet<(String, Inode)> = alive
        .iter()
        .filter(|side| {
            path_degree.get(&side.path).copied().unwrap_or_default() > 1
                || inode_degree.get(&side.inode).copied().unwrap_or_default() > 1
                || ambiguous.contains(&(side.path.clone(), side.inode))
        })
        .map(|side| (side.path.clone(), side.inode))
        .collect();

    let mut conflicts = HashMap::new();
    let mut visited = HashSet::<(String, Inode)>::new();
    for seed in alive
        .iter()
        .filter(|side| conflicted.contains(&(side.path.clone(), side.inode)))
    {
        let seed_key = (seed.path.clone(), seed.inode);
        if visited.contains(&seed_key) {
            continue;
        }
        let mut component = Vec::new();
        let mut pending = vec![seed_key];
        while let Some((path, inode)) = pending.pop() {
            if !visited.insert((path.clone(), inode)) {
                continue;
            }
            for side in &alive {
                if side.path == path || side.inode == inode {
                    let key = (side.path.clone(), side.inode);
                    if conflicted.contains(&key) && !visited.contains(&key) {
                        pending.push(key.clone());
                    }
                    if key == (path.clone(), inode) {
                        component.push(side.clone());
                    }
                }
            }
        }
        component.sort_by(|left, right| {
            (&left.path, left.inode.get()).cmp(&(&right.path, right.inode.get()))
        });
        let mut paths: Vec<_> = component.iter().map(|side| side.path.clone()).collect();
        paths.sort();
        paths.dedup();
        let conflict = ProjectedNameConflict {
            paths: paths.clone(),
            sides: component,
        };
        for path in paths {
            conflicts.insert(path, conflict.clone());
        }
    }

    if debug_reduce {
        eprintln!(
            "REDUCE alive_len={} ambiguous_len={} dep_calls={} ms={}",
            alive.len(),
            ambiguous.len(),
            dep_calls,
            reduce_start.elapsed().as_millis()
        );
    }
    let present: Vec<ProjectedPathClaim> = alive
        .into_iter()
        .filter(|side| !conflicted.contains(&(side.path.clone(), side.inode)))
        .collect();

    let conflicted_paths: HashSet<&str> = conflicts.keys().map(String::as_str).collect();
    let mut absent_by_path = HashMap::<String, Vec<super::deferred_tree::ProjectedAbsent>>::new();
    for entry in absent {
        absent_by_path
            .entry(entry.path.clone())
            .or_default()
            .push(entry);
    }
    let mut unambiguous_absent = Vec::new();
    for (path, mut entries) in absent_by_path {
        if entries.len() == 1 && !conflicted_paths.contains(path.as_str()) {
            unambiguous_absent.push(entries.remove(0));
        }
    }
    unambiguous_absent.sort_by(|left, right| left.path.cmp(&right.path));

    if debug_reduce {
        eprintln!(
            "REDUCE present_len={} absent_len={} conflicts_len={} ms={}",
            present.len(),
            unambiguous_absent.len(),
            conflicts.len(),
            reduce_start.elapsed().as_millis()
        );
    }
    Ok(ReducedPathClaims {
        present,
        absent: unambiguous_absent,
        conflicts,
    })
}

#[allow(clippy::too_many_arguments)]
fn add_insertion_claims<T>(
    txn: &T,
    change_id: NodeId,
    path: &str,
    kind: PathClaimKind,
    claimant: Position<NodeId>,
    insertion: &Insertion<Option<Hash>>,
    operation_index: &mut u32,
    entries: &mut Vec<PathClaimEntry>,
) -> Result<(), RepositoryError>
where
    T: GraphTxnT + TreeTxnT,
{
    let alive = EdgeFlags::FOLDER | EdgeFlags::BLOCK;
    if insertion.flag != alive || insertion.start >= insertion.end {
        return Err(RepositoryError::InvalidOperation {
            message: format!(
                "structural add for '{}' has an invalid name insertion",
                path
            ),
        });
    }
    let name = GraphNode::new(change_id, insertion.start, insertion.end);
    if insertion.predecessors.is_empty() {
        return Err(RepositoryError::InvalidOperation {
            message: format!("structural add for '{}' has no parent context", path),
        });
    }
    for predecessor in &insertion.predecessors {
        let source = internalize_position(txn, change_id, *predecessor)?;
        let parent = txn
            .find_block_end(source)
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        validate_resulting_claim_edge(txn, parent, name, change_id, PathClaimState::Alive, path)?;
        push_claim_entry(
            entries,
            path,
            change_id,
            next_operation_index(operation_index)?,
            kind,
            PathClaimState::Alive,
            PathClaimId::new(claimant, parent, name, change_id),
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn add_edge_update_claims<T>(
    txn: &T,
    change_id: NodeId,
    path: Option<&str>,
    declared_kind: PathClaimKind,
    state: PathClaimState,
    update: &EdgeUpdate<Option<Hash>>,
    claimant_from_name: bool,
    prior: &[PathClaimEntry],
    recovered_inodes: &HashMap<Position<NodeId>, Inode>,
    operation_index: &mut u32,
    entries: &mut Vec<PathClaimEntry>,
) -> Result<(), RepositoryError>
where
    T: GraphTxnT + TreeTxnT,
{
    let update_claimant = internalize_position(txn, change_id, update.inode)?;

    let alive = EdgeFlags::FOLDER | EdgeFlags::BLOCK;
    let deleted = alive | EdgeFlags::DELETED;
    let expected = match state {
        PathClaimState::Alive => (deleted, alive),
        PathClaimState::Dead => (alive, deleted),
    };
    let mut found = 0usize;
    for edge in &update.edges {
        if edge.previous != expected.0 || edge.flag != expected.1 || edge.to.start >= edge.to.end {
            continue;
        }
        let from = internalize_position(txn, change_id, edge.from)?;
        let parent = txn
            .find_block_end(from)
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        let name = internalize_node(txn, change_id, edge.to)?;
        let event_path = match path {
            Some(path) => path.to_string(),
            None => prior_claim_path(
                txn,
                change_id,
                update_claimant,
                parent,
                name,
                edge.introduced_by,
                prior,
            )?,
        };
        validate_resulting_claim_edge(txn, parent, name, change_id, state, &event_path)?;
        let edge_claimant = claimant_for_name(txn, name, &event_path, recovered_inodes)?;
        let claimant = if claimant_from_name {
            edge_claimant
        } else {
            if edge_claimant != update_claimant {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "structural operation for '{}' names claimant {} but edge reaches {}",
                        event_path, update_claimant, edge_claimant
                    ),
                });
            }
            update_claimant
        };
        let actual_kind = claimant_kind_from_prior(txn, claimant, recovered_inodes, prior)?;
        let kind =
            if declared_kind == PathClaimKind::File && actual_kind == PathClaimKind::Directory {
                actual_kind
            } else {
                declared_kind
            };
        if actual_kind != kind {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "structural operation for '{}' changes its node kind",
                    event_path
                ),
            });
        }
        push_claim_entry(
            entries,
            &event_path,
            change_id,
            next_operation_index(operation_index)?,
            kind,
            state,
            PathClaimId::new(claimant, parent, name, change_id),
        );
        found += 1;
    }
    if found == 0 {
        if let Some(path) = path {
            // File deletion currently records content-edge aliveness while the
            // structural name edge remains unchanged. The explicit FileDel /
            // FileUndel GraphOp is still authoritative for path lifecycle, so
            // carry forward its exact prior structural claim instead of
            // manufacturing an edge identity from TREE.
            let mut claims: Vec<_> = prior
                .iter()
                .filter(|entry| entry.path == path && entry.event.claim.claimant == update_claimant)
                .map(|entry| entry.event.claim)
                .collect();
            claims.sort();
            claims.dedup();
            if !claims.is_empty() {
                let kind = claimant_kind_from_prior(txn, update_claimant, recovered_inodes, prior)?;
                for claim in claims {
                    push_claim_entry(
                        entries,
                        path,
                        change_id,
                        next_operation_index(operation_index)?,
                        kind,
                        state,
                        claim,
                    );
                }
                return Ok(());
            }
        }
        return Err(RepositoryError::InvalidOperation {
            message: format!(
                "structural operation for '{}' has no exact or prior path claim",
                path.unwrap_or("<move source>")
            ),
        });
    }
    Ok(())
}

fn prior_claim_path<T: GraphTxnT>(
    txn: &T,
    self_change: NodeId,
    claimant: Position<NodeId>,
    parent: GraphNode<NodeId>,
    name: GraphNode<NodeId>,
    introduced_by: Option<Hash>,
    prior: &[PathClaimEntry],
) -> Result<String, RepositoryError> {
    let introduced_by = internalize_change(txn, self_change, introduced_by)?;
    let previous = PathClaimId::new(claimant, parent, name, introduced_by);
    let paths: BTreeSet<_> = prior
        .iter()
        .filter(|entry| entry.event.state == PathClaimState::Alive && entry.event.claim == previous)
        .map(|entry| entry.path.clone())
        .collect();
    if paths.len() != 1 {
        return Err(RepositoryError::InvalidOperation {
            message: format!(
                "FileMove source claim {} resolves to {} authoritative paths",
                claimant,
                paths.len()
            ),
        });
    }
    Ok(paths.into_iter().next().expect("one FileMove source path"))
}

fn push_claim_entry(
    entries: &mut Vec<PathClaimEntry>,
    path: &str,
    event_change: NodeId,
    operation_index: u32,
    kind: PathClaimKind,
    state: PathClaimState,
    claim: PathClaimId,
) {
    entries.push(PathClaimEntry::new(
        path,
        PathClaimEvent::new(event_change, operation_index, kind, state, claim),
    ));
}

fn next_operation_index(index: &mut u32) -> Result<u32, RepositoryError> {
    let current = *index;
    *index = index
        .checked_add(1)
        .ok_or_else(|| RepositoryError::InvalidOperation {
            message: "change contains too many path-claim transitions".to_string(),
        })?;
    Ok(current)
}

fn position_inode<T: TreeTxnT>(
    txn: &T,
    position: Position<NodeId>,
    recovered_inodes: &HashMap<Position<NodeId>, Inode>,
) -> Result<Option<Inode>, RepositoryError> {
    if let Some(inode) = recovered_inodes.get(&position).copied() {
        return Ok(Some(inode));
    }
    txn.position_inode(position)
        .map_err(|error| RepositoryError::Database(error.to_string()))
}

fn claimant_kind_from_prior<T: TreeTxnT>(
    txn: &T,
    claimant: Position<NodeId>,
    recovered_inodes: &HashMap<Position<NodeId>, Inode>,
    prior: &[PathClaimEntry],
) -> Result<PathClaimKind, RepositoryError> {
    let kinds: BTreeSet<_> = prior
        .iter()
        .filter(|entry| entry.event.claim.claimant == claimant)
        .map(|entry| entry.event.kind)
        .collect();
    match kinds.len() {
        0 => claimant_kind(txn, claimant, recovered_inodes),
        1 => Ok(*kinds.iter().next().expect("one prior claimant kind")),
        _ => Err(RepositoryError::InvalidOperation {
            message: format!("path claimant {} has multiple filesystem kinds", claimant),
        }),
    }
}

fn claimant_kind<T: TreeTxnT>(
    txn: &T,
    claimant: Position<NodeId>,
    recovered_inodes: &HashMap<Position<NodeId>, Inode>,
) -> Result<PathClaimKind, RepositoryError> {
    let inode = position_inode(txn, claimant, recovered_inodes)?.ok_or_else(|| {
        RepositoryError::InvalidOperation {
            message: format!("path claim references unbound inode position {}", claimant),
        }
    })?;
    Ok(
        if txn
            .is_directory(inode)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
        {
            PathClaimKind::Directory
        } else {
            PathClaimKind::File
        },
    )
}

fn claimant_for_name<T: GraphTxnT + TreeTxnT>(
    txn: &T,
    name: GraphNode<NodeId>,
    path: &str,
    recovered_inodes: &HashMap<Position<NodeId>, Inode>,
) -> Result<Position<NodeId>, RepositoryError> {
    let alive = EdgeFlags::FOLDER | EdgeFlags::BLOCK;
    let mut claimants = BTreeSet::new();
    for edge in txn
        .iter_adjacent(name, EdgeFlags::empty(), EdgeFlags::all())
        .map_err(|error| RepositoryError::Database(error.to_string()))?
    {
        let edge = edge.map_err(|error| RepositoryError::Database(error.to_string()))?;
        if edge.flag() == alive && position_inode(txn, edge.dest(), recovered_inodes)?.is_some() {
            claimants.insert(edge.dest());
        }
    }
    if claimants.len() != 1 {
        return Err(RepositoryError::InvalidOperation {
            message: format!(
                "name vertex for '{}' reaches {} inode claimants",
                path,
                claimants.len()
            ),
        });
    }
    Ok(*claimants.iter().next().expect("one name claimant"))
}

fn validate_resulting_claim_edge<T: GraphTxnT>(
    txn: &T,
    parent: GraphNode<NodeId>,
    name: GraphNode<NodeId>,
    event_change: NodeId,
    state: PathClaimState,
    path: &str,
) -> Result<(), RepositoryError> {
    let alive = EdgeFlags::FOLDER | EdgeFlags::BLOCK;
    let expected = match state {
        PathClaimState::Alive => alive,
        PathClaimState::Dead => alive | EdgeFlags::DELETED,
    };
    for edge in txn
        .iter_adjacent(parent, EdgeFlags::empty(), EdgeFlags::all())
        .map_err(|error| RepositoryError::Database(error.to_string()))?
    {
        let edge = edge.map_err(|error| RepositoryError::Database(error.to_string()))?;
        if edge.flag() == expected
            && edge.dest() == name.start_pos()
            && edge.introduced_by() == event_change
        {
            return Ok(());
        }
    }
    Err(RepositoryError::InvalidOperation {
        message: format!(
            "path-claim replay for '{}' cannot find its resulting {:?} structural edge",
            path, expected
        ),
    })
}

fn internalize_position<T: GraphTxnT>(
    txn: &T,
    self_change: NodeId,
    position: Position<Option<Hash>>,
) -> Result<Position<NodeId>, RepositoryError> {
    Ok(Position::new(
        internalize_change(txn, self_change, position.change)?,
        position.pos,
    ))
}

fn internalize_node<T: GraphTxnT>(
    txn: &T,
    self_change: NodeId,
    node: GraphNode<Option<Hash>>,
) -> Result<GraphNode<NodeId>, RepositoryError> {
    Ok(GraphNode::new(
        internalize_change(txn, self_change, node.change)?,
        node.start,
        node.end,
    ))
}

fn internalize_change<T: GraphTxnT>(
    txn: &T,
    self_change: NodeId,
    change: Option<Hash>,
) -> Result<NodeId, RepositoryError> {
    match change {
        None => Ok(self_change),
        Some(hash) if hash == Hash::NONE => Ok(NodeId::ROOT),
        Some(hash) => txn
            .get_internal(&hash)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .ok_or_else(|| RepositoryError::InvalidOperation {
                message: format!("path claim references unknown change {}", hash),
            }),
    }
}

fn change_depends_on<T: GraphTxnT>(
    txn: &T,
    descendant: NodeId,
    ancestor: NodeId,
    memo: &mut HashMap<(NodeId, NodeId), bool>,
) -> Result<bool, RepositoryError> {
    if descendant == ancestor {
        return Ok(true);
    }
    if let Some(result) = memo.get(&(descendant, ancestor)) {
        return Ok(*result);
    }
    let ancestor_hash = txn
        .get_external(ancestor)
        .map_err(|error| RepositoryError::Database(error.to_string()))?
        .ok_or_else(|| {
            RepositoryError::Database(format!(
                "path-claim dependency {} has no external hash",
                ancestor.get()
            ))
        })?;
    for dependency in txn
        .get_indexed_change_deps(descendant)
        .map_err(|error| RepositoryError::Database(error.to_string()))?
    {
        let dependency_id = txn
            .get_internal(&dependency)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .ok_or_else(|| {
                RepositoryError::Database(format!(
                    "change {} has unregistered dependency {}",
                    descendant.get(),
                    dependency
                ))
            })?;
        if dependency == ancestor_hash || change_depends_on(txn, dependency_id, ancestor, memo)? {
            memo.insert((descendant, ancestor), true);
            return Ok(true);
        }
    }
    memo.insert((descendant, ancestor), false);
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::types::ChangePosition;

    #[test]
    fn conflict_component_classifies_rename_shape() {
        let inode = Inode::new(7);
        let side = |path: &str| ProjectedPathClaim {
            path: path.to_string(),
            inode,
            position: Position::new(NodeId::new(3), ChangePosition::new(9)),
            kind: PathClaimKind::File,
            claims: Vec::new(),
            event_changes: Vec::new(),
        };
        let conflict = ProjectedNameConflict {
            paths: vec!["left".into(), "right".into()],
            sides: vec![side("left"), side("right")],
        };
        assert!(conflict.is_rename_conflict());
    }

    #[test]
    fn same_change_operation_index_supersedes_earlier_claim_state() {
        use atomic_core::pristine::{MutTxnT, Pristine, ViewMembershipSet};
        use atomic_core::types::ChangePosition;
        use tempfile::tempdir;

        let temp = tempdir().unwrap();
        let pristine = Pristine::open(temp.path().join("pristine")).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        let change = txn.register_change(&Hash::of(b"same change")).unwrap();
        txn.put_change_deps(change, &[]).unwrap();
        let inode = txn.alloc_inode().unwrap();
        let position = Position::new(change, ChangePosition::new(9));
        txn.put_inode(inode, position).unwrap();
        let claim = PathClaimId::new(
            position,
            GraphNode::root(),
            GraphNode::new(change, ChangePosition::new(0), ChangePosition::new(4)),
            change,
        );
        let entries = vec![
            PathClaimEntry::new(
                "name",
                PathClaimEvent::new(change, 0, PathClaimKind::File, PathClaimState::Alive, claim),
            ),
            PathClaimEntry::new(
                "name",
                PathClaimEvent::new(change, 1, PathClaimKind::File, PathClaimState::Dead, claim),
            ),
        ];
        let membership = ViewMembershipSet::from_ordered([change]);
        let visibility = GraphVisibilityClosure::try_from_membership(&txn, &membership).unwrap();
        let reduced = reduce_path_claim_entries(&txn, &visibility, &entries).unwrap();
        assert!(reduced.present.is_empty());
        assert_eq!(reduced.absent.len(), 1);
        txn.abort().unwrap();
    }

    #[test]
    fn exact_claim_update_supersedes_its_introduction_without_dependency_metadata() {
        use atomic_core::pristine::{MutTxnT, Pristine, ViewMembershipSet};
        use tempfile::tempdir;

        let temp = tempdir().unwrap();
        let pristine = Pristine::open(temp.path().join("pristine")).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        let introduction = txn
            .register_change(&Hash::of(b"claim introduction"))
            .unwrap();
        let deletion = txn
            .register_change(&Hash::of(b"exact claim deletion"))
            .unwrap();
        txn.put_change_deps(introduction, &[]).unwrap();
        txn.put_change_deps(deletion, &[]).unwrap();
        let inode = txn.alloc_inode().unwrap();
        let position = Position::new(introduction, ChangePosition::new(9));
        txn.put_inode(inode, position).unwrap();
        let claim = PathClaimId::new(
            position,
            GraphNode::root(),
            GraphNode::new(introduction, ChangePosition::new(0), ChangePosition::new(4)),
            introduction,
        );
        let entries = vec![
            PathClaimEntry::new(
                "name",
                PathClaimEvent::new(
                    introduction,
                    0,
                    PathClaimKind::File,
                    PathClaimState::Alive,
                    claim,
                ),
            ),
            PathClaimEntry::new(
                "name",
                PathClaimEvent::new(
                    deletion,
                    0,
                    PathClaimKind::File,
                    PathClaimState::Dead,
                    claim,
                ),
            ),
        ];
        let membership = ViewMembershipSet::from_ordered([introduction, deletion]);
        let visibility = GraphVisibilityClosure::try_from_membership(&txn, &membership).unwrap();
        let reduced = reduce_path_claim_entries(&txn, &visibility, &entries).unwrap();
        assert!(reduced.present.is_empty());
        assert!(reduced.conflicts.is_empty());
        assert_eq!(reduced.absent.len(), 1);
        txn.abort().unwrap();
    }
}
