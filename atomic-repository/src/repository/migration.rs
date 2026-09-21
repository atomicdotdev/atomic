use super::*;

use std::collections::{HashMap, HashSet};
use std::path::Path;

use atomic_core::pristine::{
    decode_inode_vertex, decode_position, encode_path_claim_event, encode_position, GraphTxnT,
    PathClaimEntry, TreeTxnT, ViewTxnT, INODES, INODE_GRAPH, PATH_CLAIMS, PATH_CLAIM_SCHEMA_KEY,
    PATH_CLAIM_SCHEMA_VERSION, PRISTINE_META, REV_INODES, REV_TREE, TREE,
};
use atomic_core::ChangePosition;
use redb::{ReadableDatabase, ReadableMultimapTable, ReadableTable};

pub(super) fn migrate_path_claims_if_required(
    pristine_path: &Path,
    pristine: Pristine,
    change_store: &ChangeStore,
    current_view: &str,
) -> Result<Pristine, RepositoryError> {
    if !pristine
        .path_claim_migration_required()
        .map_err(|error| RepositoryError::Database(error.to_string()))?
    {
        return Ok(pristine);
    }

    drop(pristine);
    let recovered_inodes = inode_graph_position_index(pristine_path)?;
    let pristine = Pristine::open(pristine_path).map_err(RepositoryError::from)?;
    let plan = prepare_migration(&pristine, change_store, current_view, &recovered_inodes)?;
    drop(pristine);
    commit_migration(pristine_path, plan)?;
    Pristine::open_existing(pristine_path).map_err(RepositoryError::from)
}

struct MigrationPlan {
    claims: Vec<PathClaimEntry>,
    tree: Vec<(String, Inode)>,
    recovered_inodes: Vec<(Inode, Position<NodeId>)>,
}

fn inode_graph_position_index(
    pristine_path: &Path,
) -> Result<HashMap<Position<NodeId>, Inode>, RepositoryError> {
    let db = redb::Builder::new()
        .set_cache_size(8 * 1024 * 1024 * 1024)
        .open(pristine_path)
        .map_err(|error| RepositoryError::Database(error.to_string()))?;
    let read = db
        .begin_read()
        .map_err(|error| RepositoryError::Database(error.to_string()))?;
    let table = read
        .open_multimap_table(INODE_GRAPH)
        .map_err(|error| RepositoryError::Database(error.to_string()))?;
    let inode_table = read
        .open_table(INODES)
        .map_err(|error| RepositoryError::Database(error.to_string()))?;
    let mut inode_roots = HashMap::new();
    for row in inode_table
        .iter()
        .map_err(|error| RepositoryError::Database(error.to_string()))?
    {
        let (inode, encoded) = row.map_err(|error| RepositoryError::Database(error.to_string()))?;
        let (change, pos) = decode_position(encoded.value());
        inode_roots.insert(
            Inode::new(inode.value()),
            Position::new(NodeId::new(change), ChangePosition::new(pos)),
        );
    }
    let mut positions = HashMap::<Position<NodeId>, Inode>::new();
    for row in table
        .iter()
        .map_err(|error| RepositoryError::Database(error.to_string()))?
    {
        let (key, _) = row.map_err(|error| RepositoryError::Database(error.to_string()))?;
        let (inode, change, start, _) = decode_inode_vertex(key.value());
        let change = NodeId::new(change);
        if change.is_root() {
            continue;
        }
        let position = Position::new(change, ChangePosition::new(start));
        let inode = Inode::new(inode);
        if inode_roots
            .get(&inode)
            .is_some_and(|root| *root != position)
        {
            continue;
        }
        // INODE_GRAPH may index shared historical vertices under a later inode.
        // Inodes are allocated monotonically, so the earliest eligible scope is
        // the original local owner created by the structural FileAdd.
        positions
            .entry(position)
            .and_modify(|existing| {
                if inode.get() < existing.get() {
                    *existing = inode;
                }
            })
            .or_insert(inode);
    }
    Ok(positions)
}

fn prepare_migration(
    pristine: &Pristine,
    change_store: &ChangeStore,
    current_view: &str,
    recovered_inode_index: &HashMap<Position<NodeId>, Inode>,
) -> Result<MigrationPlan, RepositoryError> {
    let txn = pristine
        .read_txn()
        .map_err(|error| RepositoryError::Database(error.to_string()))?;

    let mut view_names = txn
        .list_views()
        .map_err(|error| RepositoryError::Database(error.to_string()))?;
    view_names.sort();
    let mut reachable = Vec::new();
    let mut seen = HashSet::new();
    for view_name in view_names {
        let view = txn
            .get_view(&view_name)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.clone(),
            })?;
        let visibility = graph_visibility_closure(&txn, &view)?;
        for change_id in visibility.iter_dependency_first().copied() {
            if !change_id.is_root() && seen.insert(change_id) {
                reachable.push(change_id);
            }
        }
    }

    // Stable dependency-first order independent of view enumeration. A change's
    // direct dependencies always precede it in every validated closure; use a
    // small DFS over the reachable union to preserve that invariant globally.
    let reachable_set: HashSet<NodeId> = reachable.iter().copied().collect();
    let mut ordered = Vec::with_capacity(reachable.len());
    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();
    for change_id in reachable {
        visit_change(
            &txn,
            change_id,
            &reachable_set,
            &mut visiting,
            &mut visited,
            &mut ordered,
        )?;
    }

    let mut claims = Vec::new();
    for change_id in ordered {
        let hash = txn
            .get_external(change_id)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .ok_or_else(|| {
                RepositoryError::Database(format!(
                    "reachable change {} has no external hash",
                    change_id.get()
                ))
            })?;
        let change = change_store.load_change(&hash).map_err(|error| {
            RepositoryError::Database(format!(
                "cannot replay reachable change {} during PATH_CLAIMS migration: {}",
                hash, error
            ))
        })?;
        let mut change_claims =
            super::name_resolution::path_claim_events_for_change_with_prior_and_inodes(
                &txn,
                change_id,
                &change,
                &claims,
                recovered_inode_index,
            )?;
        claims.append(&mut change_claims);
    }

    let mut recovered_by_inode = HashMap::<Inode, Position<NodeId>>::new();
    for claim in &claims {
        let position = claim.event.claim.claimant;
        if txn
            .position_inode(position)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .is_some()
        {
            continue;
        }
        let inode = recovered_inode_index
            .get(&position)
            .copied()
            .ok_or_else(|| RepositoryError::InvalidOperation {
                message: format!(
                    "path claim '{}' has no inode binding or eligible INODE_GRAPH owner for {}",
                    claim.path, position
                ),
            })?;
        if let Some(existing) = txn
            .inode_position(inode)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
        {
            if existing != position {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "INODE_GRAPH recovery maps inode {} to {}, but INODES maps it to {}",
                        inode.get(),
                        position,
                        existing
                    ),
                });
            }
        }
        if let Some(existing) = recovered_by_inode.insert(inode, position) {
            if existing != position {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "INODE_GRAPH recovery maps inode {} to both {} and {}",
                        inode.get(),
                        existing,
                        position
                    ),
                });
            }
        }
    }
    let mut recovered_inodes: Vec<_> = recovered_by_inode.into_iter().collect();
    recovered_inodes.sort_by_key(|(inode, _)| inode.get());
    let recovered_inode_ids: HashSet<_> =
        recovered_inodes.iter().map(|(inode, _)| *inode).collect();

    let current = txn
        .get_view(current_view)
        .map_err(|error| RepositoryError::Database(error.to_string()))?
        .ok_or_else(|| RepositoryError::ViewNotFound {
            name: current_view.to_string(),
        })?;
    let full_visibility = graph_visibility_closure(&txn, &current)?;
    let visibility = super::name_resolution::path_claim_visibility_for_view(
        &txn,
        change_store,
        &current,
        &full_visibility,
    )?;
    let reduced = super::name_resolution::reduce_path_claim_entries_with_inodes(
        &txn,
        &visibility,
        &claims,
        recovered_inode_index,
    )?;

    let mut tree = HashMap::<String, Inode>::new();
    let mut reverse = HashMap::<Inode, String>::new();
    for side in reduced.present {
        if let Some(previous) = tree.insert(side.path.clone(), side.inode) {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "PATH_CLAIMS migration projected '{}' to both inode {} and {}",
                    side.path,
                    previous.get(),
                    side.inode.get()
                ),
            });
        }
        if let Some(previous) = reverse.insert(side.inode, side.path.clone()) {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "PATH_CLAIMS migration projected inode {} to both '{}' and '{}'",
                    side.inode.get(),
                    previous,
                    side.path
                ),
            });
        }
    }

    // Preserve staged, not-yet-recorded tracking rows. They have no structural
    // GraphOp yet and therefore deliberately do not appear in PATH_CLAIMS.
    for entry in txn
        .iter_tree()
        .map_err(|error| RepositoryError::Database(error.to_string()))?
    {
        let (path, inode) = entry.map_err(|error| RepositoryError::Database(error.to_string()))?;
        if txn
            .inode_position(inode)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .is_some()
            || recovered_inode_ids.contains(&inode)
        {
            continue;
        }
        let reverse_path = txn
            .get_path(inode)
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        if reverse_path.as_deref() != Some(path.as_str()) {
            // A forward-only TREE row has neither a recorded path claim nor a
            // bijective staged identity. It is an unverifiable derived cache
            // entry, not authority for migration.
            continue;
        }
        if tree.contains_key(&path) || reverse.contains_key(&inode) {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "staged path '{}' conflicts with the authoritative PATH_CLAIMS projection",
                    path
                ),
            });
        }
        tree.insert(path.clone(), inode);
        reverse.insert(inode, path);
    }

    let mut tree: Vec<_> = tree.into_iter().collect();
    tree.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(MigrationPlan {
        claims,
        tree,
        recovered_inodes,
    })
}

fn visit_change<T: GraphTxnT>(
    txn: &T,
    change_id: NodeId,
    reachable: &HashSet<NodeId>,
    visiting: &mut HashSet<NodeId>,
    visited: &mut HashSet<NodeId>,
    ordered: &mut Vec<NodeId>,
) -> Result<(), RepositoryError> {
    if visited.contains(&change_id) {
        return Ok(());
    }
    if !visiting.insert(change_id) {
        return Err(RepositoryError::InvalidOperation {
            message: format!(
                "PATH_CLAIMS migration found a dependency cycle at change {}",
                change_id.get()
            ),
        });
    }
    for dependency in txn
        .get_indexed_change_deps(change_id)
        .map_err(|error| RepositoryError::Database(error.to_string()))?
    {
        let dependency_id = txn
            .get_internal(&dependency)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .ok_or_else(|| {
                RepositoryError::Database(format!(
                    "change {} has unregistered dependency {}",
                    change_id.get(),
                    dependency
                ))
            })?;
        if reachable.contains(&dependency_id) {
            visit_change(txn, dependency_id, reachable, visiting, visited, ordered)?;
        }
    }
    visiting.remove(&change_id);
    visited.insert(change_id);
    ordered.push(change_id);
    Ok(())
}

fn commit_migration(path: &Path, plan: MigrationPlan) -> Result<(), RepositoryError> {
    let db = redb::Builder::new()
        .set_cache_size(8 * 1024 * 1024 * 1024)
        .open(path)
        .map_err(|error| RepositoryError::Database(error.to_string()))?;
    let write = db
        .begin_write()
        .map_err(|error| RepositoryError::Database(error.to_string()))?;

    {
        let mut inodes = write
            .open_table(INODES)
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        let mut reverse = write
            .open_table(REV_INODES)
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        for (inode, position) in &plan.recovered_inodes {
            let encoded = encode_position(position.change.get(), position.pos.get());
            let existing_position = inodes
                .get(inode.get())
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .map(|value| *value.value());
            if existing_position.is_some_and(|existing| existing != encoded) {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "cannot restore inode {} at {}: INODES already has another position",
                        inode.get(),
                        position
                    ),
                });
            }
            let existing_inode = reverse
                .get(&encoded)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .map(|value| value.value());
            if existing_inode.is_some_and(|existing| existing != inode.get()) {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "cannot restore inode {} at {}: REV_INODES already has inode {}",
                        inode.get(),
                        position,
                        existing_inode.unwrap()
                    ),
                });
            }
            inodes
                .insert(inode.get(), &encoded)
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
            reverse
                .insert(&encoded, inode.get())
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
        }
    }

    let claim_paths = {
        let table = write
            .open_multimap_table(PATH_CLAIMS)
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        let mut paths = Vec::new();
        for row in table
            .iter()
            .map_err(|error| RepositoryError::Database(error.to_string()))?
        {
            let (path, _) = row.map_err(|error| RepositoryError::Database(error.to_string()))?;
            paths.push(path.value().to_string());
        }
        paths
    };
    {
        let mut table = write
            .open_multimap_table(PATH_CLAIMS)
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        for path in claim_paths {
            table
                .remove_all(path.as_str())
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
        }
        for entry in &plan.claims {
            let encoded = encode_path_claim_event(&entry.event);
            table
                .insert(entry.path.as_str(), &encoded)
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
        }
    }

    let tree_keys = {
        let table = write
            .open_table(TREE)
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        let mut keys = Vec::new();
        for row in table
            .iter()
            .map_err(|error| RepositoryError::Database(error.to_string()))?
        {
            let (path, _) = row.map_err(|error| RepositoryError::Database(error.to_string()))?;
            keys.push(path.value().to_string());
        }
        keys
    };
    let reverse_keys = {
        let table = write
            .open_table(REV_TREE)
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        let mut keys = Vec::new();
        for row in table
            .iter()
            .map_err(|error| RepositoryError::Database(error.to_string()))?
        {
            let (inode, _) = row.map_err(|error| RepositoryError::Database(error.to_string()))?;
            keys.push(inode.value());
        }
        keys
    };
    {
        let mut table = write
            .open_table(TREE)
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        for path in tree_keys {
            table
                .remove(path.as_str())
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
        }
    }
    {
        let mut table = write
            .open_table(REV_TREE)
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        for inode in reverse_keys {
            table
                .remove(inode)
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
        }
    }
    {
        let mut tree = write
            .open_table(TREE)
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        let mut reverse = write
            .open_table(REV_TREE)
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        for (path, inode) in &plan.tree {
            tree.insert(path.as_str(), inode.get())
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
            reverse
                .insert(inode.get(), path.as_str())
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
        }
    }

    validate_raw_bijection(&write)?;
    {
        let mut metadata = write
            .open_table(PRISTINE_META)
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        metadata
            .insert(PATH_CLAIM_SCHEMA_KEY, PATH_CLAIM_SCHEMA_VERSION)
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
    }
    write
        .commit()
        .map_err(|error| RepositoryError::Database(error.to_string()))
}

fn validate_raw_bijection(write: &redb::WriteTransaction) -> Result<(), RepositoryError> {
    let tree = write
        .open_table(TREE)
        .map_err(|error| RepositoryError::Database(error.to_string()))?;
    let reverse = write
        .open_table(REV_TREE)
        .map_err(|error| RepositoryError::Database(error.to_string()))?;
    let mut forward_count = 0usize;
    let mut seen_inodes = HashSet::new();
    for row in tree
        .iter()
        .map_err(|error| RepositoryError::Database(error.to_string()))?
    {
        let (path, inode) = row.map_err(|error| RepositoryError::Database(error.to_string()))?;
        forward_count += 1;
        if !seen_inodes.insert(inode.value()) {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "PATH_CLAIMS migration projected inode {} more than once",
                    inode.value()
                ),
            });
        }
        let reverse_path = reverse
            .get(inode.value())
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        if reverse_path.as_ref().map(|value| value.value()) != Some(path.value()) {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "PATH_CLAIMS migration produced a non-bijective TREE row for '{}'",
                    path.value()
                ),
            });
        }
    }
    let mut reverse_count = 0usize;
    for row in reverse
        .iter()
        .map_err(|error| RepositoryError::Database(error.to_string()))?
    {
        let (inode, path) = row.map_err(|error| RepositoryError::Database(error.to_string()))?;
        reverse_count += 1;
        let forward_inode = tree
            .get(path.value())
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        if forward_inode.as_ref().map(|value| value.value()) != Some(inode.value()) {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "PATH_CLAIMS migration produced a reverse-only row for inode {}",
                    inode.value()
                ),
            });
        }
    }
    if forward_count != reverse_count {
        return Err(RepositoryError::InvalidOperation {
            message: format!(
                "PATH_CLAIMS migration produced {} TREE rows and {} REV_TREE rows",
                forward_count, reverse_count
            ),
        });
    }
    Ok(())
}
