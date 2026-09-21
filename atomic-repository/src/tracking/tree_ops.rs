//! Tree and query operations for file tracking.
//!
//! Low-level database operations for adding, removing, and querying
//! tracked files in the TREE/REV_TREE/DIRECTORIES tables.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use atomic_core::pristine::directory_flags;
use atomic_core::pristine::{MutTxnT, PathClaimTxnT, TreeTxnT};
use atomic_core::types::{Inode, NodeId, Position};

use super::{TrackedFile, TrackingError, TrackingResult};

/// Stable node kind projected into the derived tree indexes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeProjectionKind {
    File,
    Directory,
}

/// One typed effect consumed by [`TreeProjectionPlan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeProjectionOperation {
    Add {
        inode: Inode,
        path: Option<String>,
        position: Option<Position<NodeId>>,
        kind: TreeProjectionKind,
    },
    Delete {
        inode: Inode,
        retire: bool,
    },
    Move {
        inode: Inode,
        path: String,
    },
    Undelete {
        inode: Inode,
        path: String,
        kind: TreeProjectionKind,
    },
    DirectoryOccupancy {
        inode: Inode,
        empty: bool,
    },
    DirectoryFlags {
        inode: Inode,
        flags: u8,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum TreeProjectionError {
    #[error("tree projection database error: {0}")]
    Database(String),
    #[error("incomplete tree projection metadata: {0}")]
    IncompleteMetadata(String),
    #[error("tree projection conflict: {0}")]
    Conflict(String),
}

impl From<TreeProjectionError> for TrackingError {
    fn from(error: TreeProjectionError) -> Self {
        Self::Database(error.to_string())
    }
}

#[derive(Debug, Clone, Default)]
struct PlannedTreeEntry {
    binding: Option<Position<NodeId>>,
    kind: Option<TreeProjectionKind>,
    path: Option<Option<String>>,

    directory_flags: Option<u8>,
    retire: bool,
}

/// Validated, operation-aware mutation of all derived tree indexes.
///
/// Planning verifies touched `TREE`/`REV_TREE` and `INODES`/`REV_INODES`
/// pairs, destination ownership, and directory metadata without writing. The
/// caller then applies the plan inside the same pristine transaction as the
/// authoritative graph operation.
#[derive(Debug, Clone, Default)]
pub struct TreeProjectionPlan {
    entries: HashMap<Inode, PlannedTreeEntry>,
}

impl TreeProjectionPlan {
    pub fn plan<T, I>(txn: &T, operations: I) -> Result<Self, TreeProjectionError>
    where
        T: TreeTxnT + PathClaimTxnT,
        I: IntoIterator<Item = TreeProjectionOperation>,
    {
        let mut plan = Self::default();
        for operation in operations {
            plan.push(operation)?;
        }
        plan.derive_directory_occupancy(txn)?;
        plan.validate(txn)?;
        Ok(plan)
    }

    fn derive_directory_occupancy<T: TreeTxnT + PathClaimTxnT>(
        &mut self,
        txn: &T,
    ) -> Result<(), TreeProjectionError> {
        let mut final_paths = HashMap::<String, Inode>::new();
        for entry in txn
            .iter_tree()
            .map_err(|error| TreeProjectionError::Database(error.to_string()))?
        {
            let (path, inode) =
                entry.map_err(|error| TreeProjectionError::Database(error.to_string()))?;
            final_paths.insert(path, inode);
        }

        let mut affected_parents = HashSet::<String>::new();
        for (inode, entry) in &self.entries {
            let Some(desired) = &entry.path else {
                continue;
            };
            if let Some(current) = txn
                .get_path(*inode)
                .map_err(|error| TreeProjectionError::Database(error.to_string()))?
            {
                if let Some(parent) = direct_parent(&current) {
                    affected_parents.insert(parent.to_string());
                }
                if final_paths.get(&current) == Some(inode) {
                    final_paths.remove(&current);
                }
            }
            if let Some(path) = desired {
                if let Some(parent) = direct_parent(path) {
                    affected_parents.insert(parent.to_string());
                }
                final_paths.insert(path.clone(), *inode);
            }
        }

        for parent in affected_parents {
            let Some(inode) = final_paths.get(&parent).copied() else {
                continue;
            };
            let planned_kind = self.entries.get(&inode).and_then(|entry| entry.kind);
            let is_directory = planned_kind == Some(TreeProjectionKind::Directory)
                || txn
                    .is_directory(inode)
                    .map_err(|error| TreeProjectionError::Database(error.to_string()))?;
            if !is_directory {
                return Err(TreeProjectionError::IncompleteMetadata(format!(
                    "projected parent '{}' is not a directory",
                    parent
                )));
            }
            let empty = !final_paths
                .keys()
                .any(|path| direct_parent(path) == Some(parent.as_str()));
            let entry = self.entries.entry(inode).or_default();
            entry.kind = Some(TreeProjectionKind::Directory);
            entry.directory_flags = Some(if empty {
                directory_flags::explicit_empty()
            } else {
                directory_flags::explicit_with_children()
            });
        }
        Ok(())
    }

    fn push(&mut self, operation: TreeProjectionOperation) -> Result<(), TreeProjectionError> {
        let inode = match &operation {
            TreeProjectionOperation::Add { inode, .. }
            | TreeProjectionOperation::Delete { inode, .. }
            | TreeProjectionOperation::Move { inode, .. }
            | TreeProjectionOperation::Undelete { inode, .. }
            | TreeProjectionOperation::DirectoryOccupancy { inode, .. }
            | TreeProjectionOperation::DirectoryFlags { inode, .. } => *inode,
        };
        let entry = self.entries.entry(inode).or_default();
        match operation {
            TreeProjectionOperation::Add {
                path,
                position,
                kind,
                ..
            } => {
                if let (Some(existing), Some(position)) = (entry.binding, position) {
                    if existing != position {
                        return Err(TreeProjectionError::Conflict(format!(
                            "inode {} was assigned both {} and {}",
                            inode.get(),
                            existing,
                            position
                        )));
                    }
                } else if position.is_some() {
                    entry.binding = position;
                }
                if let Some(existing) = entry.kind {
                    if existing != kind {
                        return Err(TreeProjectionError::Conflict(format!(
                            "inode {} was projected as both a file and a directory",
                            inode.get()
                        )));
                    }
                }
                entry.kind = Some(kind);
                if let Some(path) = path {
                    entry.path = Some(Some(path));
                }
            }
            TreeProjectionOperation::Delete { retire, .. } => {
                entry.path = Some(None);

                entry.retire |= retire;
            }
            TreeProjectionOperation::Move { path, .. } => {
                entry.path = Some(Some(path));
            }
            TreeProjectionOperation::Undelete { path, kind, .. } => {
                if let Some(existing) = entry.kind {
                    if existing != kind {
                        return Err(TreeProjectionError::Conflict(format!(
                            "inode {} was projected as both a file and a directory",
                            inode.get()
                        )));
                    }
                }
                entry.kind = Some(kind);
                entry.path = Some(Some(path));
            }
            TreeProjectionOperation::DirectoryOccupancy { empty, .. } => {
                entry.kind = Some(TreeProjectionKind::Directory);
                entry.directory_flags = Some(if empty {
                    directory_flags::explicit_empty()
                } else {
                    directory_flags::explicit_with_children()
                });
            }
            TreeProjectionOperation::DirectoryFlags { flags, .. } => {
                entry.kind = Some(TreeProjectionKind::Directory);
                entry.directory_flags = Some(flags);
            }
        }
        Ok(())
    }

    fn validated_primary<T: TreeTxnT>(
        txn: &T,
        path: &str,
    ) -> Result<Option<Inode>, TreeProjectionError> {
        let Some(primary) = txn
            .get_inode(path)
            .map_err(|error| TreeProjectionError::Database(error.to_string()))?
        else {
            return Ok(None);
        };
        let reverse = txn
            .get_path(primary)
            .map_err(|error| TreeProjectionError::Database(error.to_string()))?;
        if reverse.as_deref() != Some(path) {
            return Err(TreeProjectionError::IncompleteMetadata(format!(
                "TREE maps '{}' to inode {}, but its REV_TREE row is {:?}",
                path,
                primary.get(),
                reverse
            )));
        }
        Ok(Some(primary))
    }

    fn validate<T: TreeTxnT + PathClaimTxnT>(&self, txn: &T) -> Result<(), TreeProjectionError> {
        txn.validate_tree_bijection()
            .map_err(|error| TreeProjectionError::IncompleteMetadata(error.to_string()))?;
        let mut desired_claims = HashMap::<String, Vec<Inode>>::new();
        for (inode, entry) in &self.entries {
            if let Some(Some(path)) = &entry.path {
                desired_claims.entry(path.clone()).or_default().push(*inode);
            }
        }
        for (path, claims) in &mut desired_claims {
            claims.sort_by_key(|inode| inode.get());
            claims.dedup();
            if claims.len() > 1 {
                return Err(TreeProjectionError::Conflict(format!(
                    "path '{}' has {} projected owners; PATH_CLAIMS must surface the conflict before TREE projection",
                    path,
                    claims.len()
                )));
            }
        }

        let moving_inodes: HashSet<Inode> = self
            .entries
            .iter()
            .filter_map(|(inode, entry)| entry.path.is_some().then_some(*inode))
            .collect();

        for (inode, entry) in &self.entries {
            let current_path = txn
                .get_path(*inode)
                .map_err(|error| TreeProjectionError::Database(error.to_string()))?;
            if let Some(path) = &current_path {
                let primary = Self::validated_primary(txn, path)?.ok_or_else(|| {
                    TreeProjectionError::IncompleteMetadata(format!(
                        "REV_TREE maps inode {} to '{}', but TREE has no occupant",
                        inode.get(),
                        path
                    ))
                })?;
                if primary != *inode {
                    return Err(TreeProjectionError::IncompleteMetadata(format!(
                        "REV_TREE maps inode {} to '{}', but TREE maps it to inode {}",
                        inode.get(),
                        path,
                        primary.get()
                    )));
                }
            }

            if let Some(position) = entry.binding {
                let forward = txn
                    .inode_position(*inode)
                    .map_err(|error| TreeProjectionError::Database(error.to_string()))?;
                let reverse = txn
                    .position_inode(position)
                    .map_err(|error| TreeProjectionError::Database(error.to_string()))?;
                match (forward, reverse) {
                    (None, None) => {}
                    (Some(existing), Some(reverse_inode))
                        if existing == position && reverse_inode == *inode => {}
                    (Some(existing), Some(reverse_inode)) => {
                        return Err(TreeProjectionError::Conflict(format!(
                            "inode {} / position {} disagree with existing pair {} / {}",
                            inode.get(),
                            position,
                            existing,
                            reverse_inode.get()
                        )));
                    }
                    _ => {
                        return Err(TreeProjectionError::IncompleteMetadata(format!(
                            "inode {} and position {} have a one-sided INODES/REV_INODES mapping",
                            inode.get(),
                            position
                        )));
                    }
                }
            }

            if entry.retire && entry.binding.is_some() {
                return Err(TreeProjectionError::Conflict(format!(
                    "inode {} cannot be bound and retired in one projection",
                    inode.get()
                )));
            }

            if let Some(kind) = entry.kind {
                let is_directory = txn
                    .is_directory(*inode)
                    .map_err(|error| TreeProjectionError::Database(error.to_string()))?;
                if kind == TreeProjectionKind::File && is_directory {
                    return Err(TreeProjectionError::Conflict(format!(
                        "inode {} is already marked as a directory",
                        inode.get()
                    )));
                }
            }

            let Some(Some(path)) = &entry.path else {
                continue;
            };
            if let Some(occupant) = Self::validated_primary(txn, path)? {
                if occupant == *inode && current_path.as_deref() != Some(path.as_str()) {
                    return Err(TreeProjectionError::IncompleteMetadata(format!(
                        "TREE maps '{}' to inode {}, but REV_TREE has no matching path",
                        path,
                        inode.get()
                    )));
                }
                if occupant != *inode {
                    // The occupant vacates the path when the same plan moves
                    // it elsewhere or retires its path (`Some(None)`); both
                    // free the path for the new binding in this projection.
                    let occupant_moves_away = moving_inodes.contains(&occupant)
                        && self
                            .entries
                            .get(&occupant)
                            .and_then(|entry| entry.path.as_ref())
                            .map(|desired| desired.as_deref() != Some(path.as_str()))
                            .unwrap_or(false);
                    let already_claims_path = current_path.as_deref() == Some(path.as_str());
                    let planned_primary = desired_claims
                        .get(path)
                        .is_some_and(|claims| claims.contains(&occupant));
                    if !occupant_moves_away && !already_claims_path && !planned_primary {
                        return Err(TreeProjectionError::Conflict(format!(
                            "path '{}' is already owned by inode {}",
                            path,
                            occupant.get()
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    pub fn apply<T: MutTxnT>(&self, txn: &mut T) -> Result<(), TreeProjectionError> {
        // Revalidate immediately before the first write. This makes incomplete
        // metadata and destination conflicts fail without partially updating a
        // forward/reverse pair.
        self.validate(txn)?;

        let mut removals = Vec::new();
        for (inode, entry) in &self.entries {
            if entry.path.is_none() && !entry.retire {
                continue;
            }
            let current_path = txn
                .get_path(*inode)
                .map_err(|error| TreeProjectionError::Database(error.to_string()))?;
            let desired_path = entry.path.as_ref().and_then(|path| path.as_ref());
            if current_path.as_deref() != desired_path.map(String::as_str) {
                if let Some(path) = current_path {
                    let primary = Self::validated_primary(txn, &path)?.ok_or_else(|| {
                        TreeProjectionError::IncompleteMetadata(format!(
                            "cannot remove inode {} from '{}': TREE has no primary",
                            inode.get(),
                            path
                        ))
                    })?;
                    if primary != *inode {
                        return Err(TreeProjectionError::IncompleteMetadata(format!(
                            "TREE/REV_TREE disagree while removing '{}' for inode {}",
                            path,
                            inode.get()
                        )));
                    }
                    removals.push((path, *inode));
                }
            }
        }
        removals.sort_by(|left, right| (&left.0, left.1.get()).cmp(&(&right.0, right.1.get())));
        for (path, inode) in removals {
            let removed = txn
                .del_tree(&path)
                .map_err(|error| TreeProjectionError::Database(error.to_string()))?;
            if removed != Some(inode) {
                return Err(TreeProjectionError::IncompleteMetadata(format!(
                    "TREE changed while removing '{}' for inode {}",
                    path,
                    inode.get()
                )));
            }
        }

        let mut ordered_entries: Vec<(Inode, &PlannedTreeEntry)> = self
            .entries
            .iter()
            .map(|(inode, entry)| (*inode, entry))
            .collect();
        ordered_entries.sort_by_key(|(inode, _)| inode.get());
        for (inode, entry) in &ordered_entries {
            if let Some(position) = entry.binding {
                if txn
                    .inode_position(*inode)
                    .map_err(|error| TreeProjectionError::Database(error.to_string()))?
                    .is_none()
                {
                    txn.put_inode(*inode, position)
                        .map_err(|error| TreeProjectionError::Database(error.to_string()))?;
                }
            }
            if entry.kind == Some(TreeProjectionKind::Directory)
                && txn
                    .get_directory_flags(*inode)
                    .map_err(|error| TreeProjectionError::Database(error.to_string()))?
                    .is_none()
            {
                txn.put_directory(*inode, directory_flags::explicit_empty())
                    .map_err(|error| TreeProjectionError::Database(error.to_string()))?;
            }
        }

        let mut desired_paths = Vec::<(String, Inode)>::new();
        for (inode, entry) in &ordered_entries {
            if let Some(Some(path)) = &entry.path {
                desired_paths.push((path.clone(), *inode));
            }
        }
        desired_paths.sort_by(|left, right| left.0.cmp(&right.0));
        for (path, inode) in desired_paths {
            match Self::validated_primary(txn, &path)? {
                Some(existing) if existing == inode => {}
                Some(existing) => {
                    return Err(TreeProjectionError::Conflict(format!(
                        "path '{}' is already owned by inode {}",
                        path,
                        existing.get()
                    )));
                }
                None => txn
                    .put_tree(&path, inode)
                    .map_err(|error| TreeProjectionError::Database(error.to_string()))?,
            }
        }

        for (inode, entry) in &ordered_entries {
            if let Some(flags) = entry.directory_flags {
                txn.put_directory(*inode, flags)
                    .map_err(|error| TreeProjectionError::Database(error.to_string()))?;
            }
        }

        for (inode, entry) in ordered_entries {
            if entry.retire {
                txn.del_directory(inode)
                    .map_err(|error| TreeProjectionError::Database(error.to_string()))?;
                txn.del_inode(inode)
                    .map_err(|error| TreeProjectionError::Database(error.to_string()))?;
            }
        }

        txn.validate_tree_bijection()
            .map_err(|error| TreeProjectionError::IncompleteMetadata(error.to_string()))?;
        Ok(())
    }
}

fn direct_parent(path: &str) -> Option<&str> {
    path.rsplit_once('/').map(|(parent, _)| parent)
}

// Core Tracking Functions

/// Add a single file to tracking.
///
/// This is the low-level function that actually modifies the database.
/// It does NOT check if the file exists on disk or is already tracked.
///
/// # Arguments
///
/// * `txn` - A mutable transaction
/// * `path` - The normalized path string
/// * `is_directory` - Whether this is a directory
///
/// # Returns
///
/// The allocated inode for the file.
pub fn add_to_tree<T: MutTxnT>(
    txn: &mut T,
    path: &str,
    is_directory: bool,
) -> TrackingResult<Inode> {
    // Allocate a new inode
    let inode = txn
        .alloc_inode()
        .map_err(|e| TrackingError::Database(e.to_string()))?;

    let kind = if is_directory {
        TreeProjectionKind::Directory
    } else {
        TreeProjectionKind::File
    };
    TreeProjectionPlan::plan(
        txn,
        [TreeProjectionOperation::Add {
            inode,
            path: Some(path.to_string()),
            position: None,
            kind,
        }],
    )?
    .apply(txn)?;

    Ok(inode)
}

/// Add an empty directory to tracking explicitly.
///
/// This is distinct from `add_to_tree` because it specifically handles
/// empty directories that need to be tracked even without children.
/// The directory will be marked with `DIR_EXPLICIT | DIR_EMPTY` flags.
///
/// # Arguments
///
/// * `txn` - A mutable transaction
/// * `path` - The normalized directory path
///
/// # Returns
///
/// The allocated inode for the directory.
///
/// # Example
///
/// ```rust,ignore
/// use atomic_core::pristine::Pristine;
/// use atomic_repository::tracking::add_directory_to_tree;
///
/// let pristine = Pristine::open(path)?;
/// let mut txn = pristine.write_txn()?;
///
/// // Track an empty directory
/// let inode = add_directory_to_tree(&mut txn, "src/empty_module")?;
/// txn.commit()?;
/// ```
pub fn add_directory_to_tree<T: MutTxnT>(txn: &mut T, path: &str) -> TrackingResult<Inode> {
    // Allocate a new inode
    let inode = txn
        .alloc_inode()
        .map_err(|e| TrackingError::Database(e.to_string()))?;

    TreeProjectionPlan::plan(
        txn,
        [
            TreeProjectionOperation::Add {
                inode,
                path: Some(path.to_string()),
                position: None,
                kind: TreeProjectionKind::Directory,
            },
            TreeProjectionOperation::DirectoryOccupancy { inode, empty: true },
        ],
    )?
    .apply(txn)?;

    Ok(inode)
}

/// Check if an inode represents a directory.
///
/// # Arguments
///
/// * `txn` - A transaction (read or write)
/// * `inode` - The inode to check
///
/// # Returns
///
/// `true` if this inode is marked as a directory in the DIRECTORIES table.
pub fn is_directory_inode<T: TreeTxnT>(txn: &T, inode: Inode) -> TrackingResult<bool> {
    txn.is_directory(inode)
        .map_err(|e| TrackingError::Database(e.to_string()))
}

/// Get directory flags for an inode.
///
/// # Arguments
///
/// * `txn` - A transaction (read or write)
/// * `inode` - The inode to check
///
/// # Returns
///
/// The directory flags if this inode is a directory, `None` if it's a file.
pub fn get_directory_flags<T: TreeTxnT>(txn: &T, inode: Inode) -> TrackingResult<Option<u8>> {
    txn.get_directory_flags(inode)
        .map_err(|e| TrackingError::Database(e.to_string()))
}

/// Update directory flags (e.g., when adding/removing children).
///
/// # Arguments
///
/// * `txn` - A mutable transaction
/// * `inode` - The directory's inode
/// * `flags` - New flags to set
pub fn update_directory_flags<T: MutTxnT>(
    txn: &mut T,
    inode: Inode,
    flags: u8,
) -> TrackingResult<()> {
    TreeProjectionPlan::plan(
        txn,
        [TreeProjectionOperation::DirectoryFlags { inode, flags }],
    )?
    .apply(txn)?;
    Ok(())
}

/// Mark a directory as having children (not empty).
///
/// This is called when a file is added under a tracked directory.
///
/// # Arguments
///
/// * `txn` - A mutable transaction
/// * `inode` - The directory's inode
pub fn mark_directory_has_children<T: MutTxnT + TreeTxnT>(
    txn: &mut T,
    inode: Inode,
) -> TrackingResult<()> {
    if txn
        .get_directory_flags(inode)
        .map_err(|e| TrackingError::Database(e.to_string()))?
        .is_some()
    {
        TreeProjectionPlan::plan(
            txn,
            [TreeProjectionOperation::DirectoryOccupancy {
                inode,
                empty: false,
            }],
        )?
        .apply(txn)?;
    }
    Ok(())
}

/// Mark a directory as empty (no children).
///
/// This is called when the last file is removed from a tracked directory.
///
/// # Arguments
///
/// * `txn` - A mutable transaction
/// * `inode` - The directory's inode
pub fn mark_directory_empty<T: MutTxnT + TreeTxnT>(
    txn: &mut T,
    inode: Inode,
) -> TrackingResult<()> {
    if txn
        .get_directory_flags(inode)
        .map_err(|e| TrackingError::Database(e.to_string()))?
        .is_some()
    {
        TreeProjectionPlan::plan(
            txn,
            [TreeProjectionOperation::DirectoryOccupancy { inode, empty: true }],
        )?
        .apply(txn)?;
    }
    Ok(())
}

/// Remove a single file from tracking.
///
/// This is the low-level function that actually modifies the database.
///
/// # Arguments
///
/// * `txn` - A mutable transaction
/// * `path` - The normalized path string
///
/// # Returns
///
/// The inode that was removed, if any.
pub fn remove_from_tree<T: MutTxnT>(txn: &mut T, path: &str) -> TrackingResult<Option<Inode>> {
    let Some(inode) = txn
        .get_inode(path)
        .map_err(|e| TrackingError::Database(e.to_string()))?
    else {
        return Ok(None);
    };
    TreeProjectionPlan::plan(
        txn,
        [TreeProjectionOperation::Delete {
            inode,
            retire: true,
        }],
    )?
    .apply(txn)?;
    Ok(Some(inode))
}

/// Remove a directory from tracking.
///
/// This only removes the directory if it has no tracked children.
/// To force removal of a non-empty directory, use `remove_directory_recursive`.
///
/// # Arguments
///
/// * `txn` - A mutable transaction
/// * `path` - The normalized directory path
///
/// # Returns
///
/// The inode that was removed.
///
/// # Errors
///
/// Returns `DirectoryNotEmpty` if the directory has tracked children.
pub fn remove_directory_from_tree<T: MutTxnT + TreeTxnT>(
    txn: &mut T,
    path: &str,
) -> TrackingResult<Inode> {
    // Get the inode first
    let inode = txn
        .get_inode(path)
        .map_err(|e| TrackingError::Database(e.to_string()))?
        .ok_or_else(|| TrackingError::NotTracked {
            path: path.to_string(),
        })?;

    // Check if it's actually a directory
    if !is_directory_inode(txn, inode)? {
        return Err(TrackingError::NotDirectory {
            path: path.to_string(),
        });
    }

    // A directory is non-empty when the projected tree contains a direct
    // child, not merely another string sharing its prefix.
    let mut has_children = false;
    for entry in txn
        .iter_tree()
        .map_err(|e| TrackingError::Database(e.to_string()))?
    {
        let (candidate, _) = entry.map_err(|e| TrackingError::Database(e.to_string()))?;
        if direct_parent(&candidate) == Some(path) {
            has_children = true;
            break;
        }
    }

    if has_children {
        return Err(TrackingError::DirectoryNotEmpty {
            path: path.to_string(),
        });
    }

    TreeProjectionPlan::plan(
        txn,
        [TreeProjectionOperation::Delete {
            inode,
            retire: true,
        }],
    )?
    .apply(txn)?;
    Ok(inode)
}

/// Check if a path is tracked.
///
/// # Arguments
///
/// * `txn` - A transaction (read or write)
/// * `path` - The normalized path string
pub fn is_tracked<T: TreeTxnT>(txn: &T, path: &str) -> TrackingResult<bool> {
    let result = txn
        .get_inode(path)
        .map_err(|e| TrackingError::Database(e.to_string()))?;
    Ok(result.is_some())
}

/// Get the inode for a tracked path.
///
/// # Arguments
///
/// * `txn` - A transaction (read or write)
/// * `path` - The normalized path string
pub fn get_inode<T: TreeTxnT>(txn: &T, path: &str) -> TrackingResult<Option<Inode>> {
    txn.get_inode(path)
        .map_err(|e| TrackingError::Database(e.to_string()))
}

/// Get the path for an inode.
///
/// # Arguments
///
/// * `txn` - A transaction (read or write)
/// * `inode` - The inode to look up
pub fn get_path<T: TreeTxnT>(txn: &T, inode: Inode) -> TrackingResult<Option<String>> {
    txn.get_path(inode)
        .map_err(|e| TrackingError::Database(e.to_string()))
}

/// List all tracked files.
///
/// # Arguments
///
/// * `txn` - A transaction (read or write)
///
/// # Returns
///
/// A vector of all tracked files and directories.
pub fn list_tracked<T: TreeTxnT>(txn: &T) -> TrackingResult<Vec<TrackedFile>> {
    let iter = txn
        .iter_tree()
        .map_err(|e| TrackingError::Database(e.to_string()))?;

    let mut results = Vec::new();
    for result in iter {
        let (path, inode) = result.map_err(|e| TrackingError::Database(e.to_string()))?;
        let is_directory = is_directory_inode(txn, inode)?;
        results.push(TrackedFile::new(PathBuf::from(path), inode, is_directory));
    }

    Ok(results)
}

/// List all tracked directories.
///
/// # Arguments
///
/// * `txn` - A transaction (read or write)
///
/// # Returns
///
/// A vector of tracked directories.
pub fn list_tracked_directories<T: TreeTxnT>(txn: &T) -> TrackingResult<Vec<TrackedFile>> {
    let all_tracked = list_tracked(txn)?;
    Ok(all_tracked.into_iter().filter(|f| f.is_directory).collect())
}

/// List all explicitly tracked empty directories.
///
/// # Arguments
///
/// * `txn` - A transaction (read or write)
///
/// # Returns
///
/// A vector of explicitly tracked empty directories.
pub fn list_explicit_empty_directories<T: TreeTxnT>(txn: &T) -> TrackingResult<Vec<TrackedFile>> {
    let all_tracked = list_tracked(txn)?;
    let mut results = Vec::new();

    for file in all_tracked {
        if file.is_directory {
            if let Some(flags) = get_directory_flags(txn, file.inode)? {
                if directory_flags::is_explicit(flags) && directory_flags::is_empty(flags) {
                    results.push(file);
                }
            }
        }
    }

    Ok(results)
}

/// Move/rename a tracked file.
///
/// This updates the path → inode mapping while preserving the inode,
/// so the file's history is maintained.
///
/// # Arguments
///
/// * `txn` - A mutable transaction
/// * `from` - The current path
/// * `to` - The new path
pub fn move_tracked<T: MutTxnT + TreeTxnT>(
    txn: &mut T,
    from: &str,
    to: &str,
) -> TrackingResult<Inode> {
    // Get the inode for the source
    let inode = txn
        .get_inode(from)
        .map_err(|e| TrackingError::Database(e.to_string()))?
        .ok_or_else(|| TrackingError::NotTracked {
            path: from.to_string(),
        })?;

    // Check destination doesn't exist
    if txn
        .get_inode(to)
        .map_err(|e| TrackingError::Database(e.to_string()))?
        .is_some()
    {
        return Err(TrackingError::DestinationExists {
            path: to.to_string(),
        });
    }

    TreeProjectionPlan::plan(
        txn,
        [TreeProjectionOperation::Move {
            inode,
            path: to.to_string(),
        }],
    )?
    .apply(txn)?;
    Ok(inode)
}

/// Get all tracked paths under a directory prefix.
///
/// # Arguments
///
/// * `txn` - A transaction (read or write)
/// * `prefix` - The directory prefix to search under
pub fn tracked_under_prefix<T: TreeTxnT>(
    txn: &T,
    prefix: &str,
) -> TrackingResult<Vec<(String, Inode)>> {
    let iter = txn
        .iter_tree()
        .map_err(|e| TrackingError::Database(e.to_string()))?;

    let prefix_normalized = if prefix.ends_with('/') {
        prefix.to_string()
    } else {
        format!("{}/", prefix)
    };

    let mut results = Vec::new();
    for result in iter {
        let (path, inode) = result.map_err(|e| TrackingError::Database(e.to_string()))?;
        if path.starts_with(&prefix_normalized) || path == prefix.trim_end_matches('/') {
            results.push((path, inode));
        }
    }

    Ok(results)
}
