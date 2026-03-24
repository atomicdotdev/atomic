use super::*;

// POSITION RESOLUTION

/// Resolve a file path to its inode.
///
/// This function looks up the stable file identifier (inode) for a given
/// path in the repository tree.
///
/// # Arguments
///
/// * `ctx` - The globalization context
/// * `path` - The file path to resolve
///
/// # Returns
///
/// The inode for the file, or an error if the path is not found.
///
/// # Example
///
/// ```rust,ignore
/// let inode = resolve_path_to_inode(&mut ctx, "src/main.rs")?;
/// ```
pub fn resolve_path_to_inode<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    path: &str,
) -> GlobalizeResult<Inode>
where
    T: GraphTxnT + TreeTxnT,
{
    // Check cache first
    if let Some(&inode) = ctx.inode_cache.get(path) {
        return Ok(inode);
    }

    // Look up in tree
    let inode = ctx
        .txn
        .get_inode(path)?
        .ok_or_else(|| GlobalizeError::PathNotFound {
            path: path.to_string(),
        })?;

    // Cache the result
    ctx.inode_cache.insert(path.to_string(), inode);

    Ok(inode)
}

/// Resolve an inode to its graph position.
///
/// This function looks up the position in the repository graph where
/// a file's content root is located.
///
/// # Arguments
///
/// * `ctx` - The globalization context
/// * `inode` - The inode to resolve
///
/// # Returns
///
/// The graph position for the inode, or an error if not found.
///
/// # Example
///
/// ```rust,ignore
/// let position = resolve_inode_to_position(&mut ctx, inode)?;
/// ```
pub fn resolve_inode_to_position<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    inode: Inode,
) -> GlobalizeResult<Position<NodeId>>
where
    T: GraphTxnT + TreeTxnT,
{
    // Check cache first
    if let Some(&pos) = ctx.position_cache.get(&inode) {
        return Ok(pos);
    }

    // Look up in pristine
    let pos = ctx
        .txn
        .inode_position(inode)?
        .ok_or(GlobalizeError::InodeNotFound { inode })?;

    // Cache the result
    ctx.position_cache.insert(inode, pos);

    Ok(pos)
}

/// Resolve a file path to its graph position.
///
/// This is a convenience function that combines path-to-inode and
/// inode-to-position resolution.
///
/// # Arguments
///
/// * `ctx` - The globalization context
/// * `path` - The file path to resolve
///
/// # Returns
///
/// The graph position for the file, or an error if not found.
///
/// # Example
///
/// ```rust,ignore
/// let position = resolve_file_position(&mut ctx, "src/main.rs")?;
/// ```
pub fn resolve_file_position<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    path: &str,
) -> GlobalizeResult<Position<NodeId>>
where
    T: GraphTxnT + TreeTxnT,
{
    let inode = resolve_path_to_inode(ctx, path)?;
    resolve_inode_to_position(ctx, inode)
}

/// Resolve the parent directory's inode for a given path.
///
/// This is used when adding new files - we need to know the parent
/// directory to add the new filename entry.
///
/// # Arguments
///
/// * `ctx` - The globalization context
/// * `path` - The file path whose parent to find
///
/// # Returns
///
/// The inode of the parent directory, or an error if not found.
///
/// # Example
///
/// ```rust,ignore
/// // For path "src/lib/mod.rs", returns inode of "src/lib"
/// let parent_inode = resolve_parent_inode(&mut ctx, "src/lib/mod.rs")?;
/// ```
pub fn resolve_parent_inode<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    path: &str,
) -> GlobalizeResult<Inode>
where
    T: GraphTxnT + TreeTxnT,
{
    // Find the parent path
    let parent_path = std::path::Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    if parent_path.is_empty() {
        // File is at repository root - use the root inode
        // The root directory has a special empty path
        ctx.txn
            .get_inode("")?
            .ok_or_else(|| GlobalizeError::ParentNotFound {
                path: path.to_string(),
            })
    } else {
        resolve_path_to_inode(ctx, &parent_path)
    }
}
