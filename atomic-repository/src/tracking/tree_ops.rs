//! Tree and query operations for file tracking.
//!
//! Low-level database operations for adding, removing, and querying
//! tracked files in the TREE/REV_TREE/DIRECTORIES tables.

use std::path::PathBuf;

use atomic_core::pristine::directory_flags;
use atomic_core::pristine::{MutTxnT, TreeTxnT};
use atomic_core::types::Inode;

use super::{TrackedFile, TrackingError, TrackingResult};


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

    // Add to tree tables
    txn.put_tree(path, inode)
        .map_err(|e| TrackingError::Database(e.to_string()))?;

    // If this is a directory, mark it in the DIRECTORIES table
    if is_directory {
        txn.put_directory(inode, directory_flags::DIR_EXPLICIT)
            .map_err(|e| TrackingError::Database(e.to_string()))?;
    }

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

    // Add to tree tables
    txn.put_tree(path, inode)
        .map_err(|e| TrackingError::Database(e.to_string()))?;

    // Mark as an explicit empty directory
    txn.put_directory(inode, directory_flags::explicit_empty())
        .map_err(|e| TrackingError::Database(e.to_string()))?;

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
    txn.update_directory_flags(inode, flags)
        .map_err(|e| TrackingError::Database(e.to_string()))
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
    if let Some(flags) = txn
        .get_directory_flags(inode)
        .map_err(|e| TrackingError::Database(e.to_string()))?
    {
        // Remove the DIR_EMPTY flag if present
        let new_flags = flags & !directory_flags::DIR_EMPTY;
        if new_flags != flags {
            txn.update_directory_flags(inode, new_flags)
                .map_err(|e| TrackingError::Database(e.to_string()))?;
        }
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
    if let Some(flags) = txn
        .get_directory_flags(inode)
        .map_err(|e| TrackingError::Database(e.to_string()))?
    {
        // Add the DIR_EMPTY flag
        let new_flags = flags | directory_flags::DIR_EMPTY;
        if new_flags != flags {
            txn.update_directory_flags(inode, new_flags)
                .map_err(|e| TrackingError::Database(e.to_string()))?;
        }
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
    // Remove from tree (this also removes from REV_TREE)
    let inode = txn
        .del_tree(path)
        .map_err(|e| TrackingError::Database(e.to_string()))?;

    // If there was an inode, also remove its position mapping and directory marker
    if let Some(inode) = inode {
        let _ = txn.del_inode(inode);
        // Remove directory marker if present
        let _ = txn.del_directory(inode);
    }

    Ok(inode)
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

    // Check for children
    let children = tracked_under_prefix(txn, path)?;
    let has_children = children.iter().any(|(p, _)| p != path);

    if has_children {
        return Err(TrackingError::DirectoryNotEmpty {
            path: path.to_string(),
        });
    }

    // Safe to remove
    txn.del_tree(path)
        .map_err(|e| TrackingError::Database(e.to_string()))?;
    txn.del_directory(inode)
        .map_err(|e| TrackingError::Database(e.to_string()))?;
    let _ = txn.del_inode(inode);

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

    // Remove old mapping
    txn.del_tree(from)
        .map_err(|e| TrackingError::Database(e.to_string()))?;

    // Add new mapping with same inode
    txn.put_tree(to, inode)
        .map_err(|e| TrackingError::Database(e.to_string()))?;

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
