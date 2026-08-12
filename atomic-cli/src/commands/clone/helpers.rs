//! Helper functions for the clone command.
//!
//! This module provides utility functions used by the clone command, including:
//!
//! - URL parsing and repository name inference
//! - Target path validation
//! - Cleanup guard for error recovery
//! - Error conversion from remote errors to CLI errors
//! - Formatting utilities
//!
//! # Overview
//!
//! The clone command needs to:
//!
//! 1. Parse remote URLs and infer repository names
//! 2. Validate that the target directory doesn't exist
//! 3. Provide cleanup on error (remove partial clone)
//! 4. Download and save changes to the local repository
//! 5. Display progress and state information to the user
//!
//! This module provides the helper functions that support these operations.

use std::collections::HashSet;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use bytes::Bytes;

use atomic_core::change::Change;
use atomic_core::types::{Base32, Hash};
use atomic_remote::RemoteError;
use atomic_repository::{Repository, ViewManifest};

use crate::error::{CliError, CliResult};

// URL Parsing

/// Infer the repository name from a URL.
///
/// Extracts a suitable directory name from various URL formats:
///
/// - `https://example.com/org/project/code` → `project`
/// - `https://example.com/repo.git` → `repo`
/// - `https://example.com/tenant/t/portfolio/p/project/pr/code` → `pr`
///
/// # Arguments
///
/// * `url` - The remote URL to parse
///
/// # Returns
///
/// `Some(name)` if a name could be inferred, `None` otherwise.
///
/// # Example
///
/// ```rust
/// use atomic::commands::clone::helpers::infer_repo_name;
///
/// assert_eq!(infer_repo_name("https://example.com/org/project/code"), Some("project".to_string()));
/// assert_eq!(infer_repo_name("https://example.com/repo.git"), Some("repo".to_string()));
/// assert_eq!(infer_repo_name("https://example.com/tenant/t/portfolio/p/project/pr/code"), Some("pr".to_string()));
/// ```
pub fn infer_repo_name(url: &str) -> Option<String> {
    // Remove trailing slashes
    let url = url.trim_end_matches('/');

    // Try to parse as URL
    if let Ok(parsed) = url::Url::parse(url) {
        let path = parsed.path();
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

        // Check for atomic-api URL pattern: /tenant/t/portfolio/p/project/pr/code
        if segments.len() >= 6 && segments.last() == Some(&"code") {
            // Project name is the second-to-last segment (pr in the pattern)
            return segments.get(segments.len() - 2).map(|s| s.to_string());
        }

        // Check for .git suffix
        if let Some(last) = segments.last() {
            let name = last.trim_end_matches(".git");
            if !name.is_empty() && name != "code" {
                return Some(name.to_string());
            }
        }

        // Use second-to-last segment if last is "code" or empty
        if segments.len() >= 2 {
            if let Some(second_last) = segments.get(segments.len() - 2) {
                if !second_last.is_empty() {
                    return Some(second_last.to_string());
                }
            }
        }

        // Fallback to last non-empty segment
        for segment in segments.iter().rev() {
            if !segment.is_empty() && *segment != "code" {
                return Some(segment.to_string());
            }
        }
    }

    // If URL parsing failed, try simple path extraction
    let parts: Vec<&str> = url.rsplit('/').collect();
    for part in parts {
        let name = part.trim_end_matches(".git");
        if !name.is_empty() && name != "code" {
            return Some(name.to_string());
        }
    }

    None
}

// Path Validation

/// Validate that the target path is suitable for cloning.
///
/// Checks that:
/// 1. The path doesn't already exist as a directory
/// 2. The path doesn't already exist as a file
/// 3. The parent directory exists (or can be created)
///
/// # Arguments
///
/// * `path` - The target path to validate
///
/// # Returns
///
/// `Ok(())` if the path is valid, `Err` with appropriate error otherwise.
///
/// # Errors
///
/// - `CliError::RepositoryExists` if a directory/file exists at the path
/// - `CliError::InvalidPath` if the parent directory cannot be created
///
/// # Example
///
/// ```rust,ignore
/// use atomic::commands::clone::helpers::validate_target_path;
/// use std::path::Path;
///
/// // Returns Ok if path doesn't exist
/// validate_target_path(Path::new("/tmp/new-repo")).unwrap();
///
/// // Returns Err if path exists
/// validate_target_path(Path::new("/")).unwrap_err();
/// ```
pub fn validate_target_path(path: &Path) -> CliResult<()> {
    if path.exists() {
        return Err(CliError::RepositoryExists {
            path: path.to_path_buf(),
        });
    }

    // Check that the parent directory exists or can be created
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            // Try to create parent directories
            std::fs::create_dir_all(parent).map_err(|e| CliError::InvalidPath {
                path: parent.to_path_buf(),
                source: Some(e),
            })?;
        }
    }

    Ok(())
}

/// Resolve the target path for cloning.
///
/// If `path` is provided, uses that. Otherwise, infers from the URL.
///
/// # Arguments
///
/// * `url` - The remote URL
/// * `path` - Optional explicit path
///
/// # Returns
///
/// The resolved target path.
///
/// # Example
///
/// ```rust
/// use atomic::commands::clone::helpers::resolve_target_path;
///
/// // Explicit path takes precedence
/// let path = resolve_target_path("https://example.com/repo", Some("my-repo".to_string()));
/// assert_eq!(path.to_str().unwrap(), "my-repo");
///
/// // Infers from URL if no path provided
/// let path = resolve_target_path("https://example.com/org/project/code", None);
/// assert_eq!(path.to_str().unwrap(), "project");
/// ```
pub fn resolve_target_path(url: &str, path: Option<String>) -> PathBuf {
    match path {
        Some(p) => PathBuf::from(p),
        None => {
            let name = infer_repo_name(url).unwrap_or_else(|| "repo".to_string());
            PathBuf::from(name)
        }
    }
}

// Cleanup Guard

/// A guard that cleans up a directory on drop if not disabled.
///
/// This is used to ensure that if a clone fails partway through, the partially
/// created directory is removed, leaving no trace of the failed operation.
///
/// # Usage
///
/// ```rust,ignore
/// use atomic::commands::clone::helpers::CleanupGuard;
/// use std::path::PathBuf;
///
/// fn clone_repo(path: &Path) -> Result<(), Error> {
///     let guard = CleanupGuard::new(path.to_path_buf());
///
///     // ... perform clone operations ...
///
///     // If we get here, the clone succeeded
///     guard.disable();
///     Ok(())
/// }
/// ```
///
/// If the function returns early due to an error (without calling `disable()`),
/// the guard's `Drop` implementation will remove the directory.
#[derive(Debug)]
pub struct CleanupGuard {
    /// The path to clean up.
    path: PathBuf,

    /// Whether cleanup is enabled.
    enabled: bool,
}

impl CleanupGuard {
    /// Create a new cleanup guard for the given path.
    ///
    /// # Arguments
    ///
    /// * `path` - The path to clean up on drop
    ///
    /// # Returns
    ///
    /// A new `CleanupGuard` with cleanup enabled.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::clone::helpers::CleanupGuard;
    /// use std::path::PathBuf;
    ///
    /// let guard = CleanupGuard::new(PathBuf::from("/tmp/test-repo"));
    /// assert!(!guard.is_disabled());
    /// ```
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            enabled: true,
        }
    }

    /// Disable cleanup.
    ///
    /// Call this when the operation succeeds and you don't want the
    /// directory to be removed.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic::commands::clone::helpers::CleanupGuard;
    /// use std::path::PathBuf;
    ///
    /// let guard = CleanupGuard::new(PathBuf::from("/tmp/test-repo"));
    /// guard.disable();
    /// assert!(guard.is_disabled());
    /// ```
    pub fn disable(mut self) {
        self.enabled = false;
    }

    /// Check if cleanup has been disabled.
    pub fn is_disabled(&self) -> bool {
        !self.enabled
    }

    /// Get a reference to the guarded path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        if self.enabled && self.path.exists() {
            // Best effort cleanup - ignore errors
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

// Change Operations

/// Save a downloaded change to the repository's change store.
///
/// Writes the change data to the appropriate location in the repository's
/// `.atomic/changes/` directory structure.
///
/// # Arguments
///
/// * `repo` - The repository to save to
/// * `hash` - The expected hash of the change (for verification)
/// * `data` - The raw change file data
///
/// # Returns
///
/// `Ok(())` on success, or an appropriate error.
///
/// # Errors
///
/// - `CliError::Internal` if the hash doesn't match the data
/// - `CliError::Internal` if the save fails for any reason
///
/// # Example
///
/// ```rust,ignore
/// let data = remote.download_change(&hash_str).await?;
/// save_downloaded_change(&repo, &expected_hash, data)?;
/// ```
pub fn save_downloaded_change(repo: &Repository, hash: &Hash, data: Bytes) -> CliResult<()> {
    // Deserialize the change from bytes
    let mut cursor = Cursor::new(&data[..]);
    let (change, computed_hash) = Change::deserialize(&mut cursor)
        .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to deserialize change: {}", e)))?;

    // Verify the hash matches what we expected
    if computed_hash != *hash {
        return Err(CliError::Internal(anyhow::anyhow!(
            "Hash mismatch: expected {}, got {}",
            hash.to_base32(),
            computed_hash.to_base32()
        )));
    }

    // Save to the change store
    repo.save_change(&change)
        .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to save change: {}", e)))?;

    Ok(())
}

// View Manifests

/// Parse and verify a view manifest downloaded from the remote.
///
/// Validates three things before the manifest is trusted:
///
/// 1. The text parses structurally (header + base32 change log).
/// 2. The header names the view that was requested.
/// 3. The declared merkle state equals the fold of the change log.
///
/// # Arguments
///
/// * `view` - The view name that was requested from the remote
/// * `text` - The raw manifest text returned by the server
/// * `url` - The remote URL (for error context)
pub fn parse_remote_manifest(view: &str, text: &str, url: &str) -> CliResult<ViewManifest> {
    let manifest = ViewManifest::parse(text).map_err(|e| CliError::RemoteError {
        message: format!("Corrupt manifest for view '{}': {}", view, e),
        url: Some(url.to_string()),
    })?;

    if manifest.name != view {
        return Err(CliError::RemoteError {
            message: format!(
                "Manifest name mismatch: requested view '{}', server sent '{}'",
                view, manifest.name
            ),
            url: Some(url.to_string()),
        });
    }

    manifest.verify().map_err(|e| CliError::RemoteError {
        message: format!("Corrupt manifest for view '{}': {}", view, e),
        url: Some(url.to_string()),
    })?;

    Ok(manifest)
}

/// Order manifests so every parent is applied before its children.
///
/// A manifest is ready when its declared parent is `None` (a root view),
/// already applied locally (`already_applied`), or emitted earlier in the
/// order. Repeatedly emits ready manifests until no progress can be made.
///
/// # Returns
///
/// `(ordered, stuck)` — indices into `manifests`. `ordered` is a valid
/// root→leaf apply order; `stuck` holds manifests that can never apply
/// (their parent chain has a cycle or references a view that is neither
/// in the set nor already applied).
pub fn manifest_apply_order(
    manifests: &[ViewManifest],
    already_applied: &HashSet<String>,
) -> (Vec<usize>, Vec<usize>) {
    let mut ordered: Vec<usize> = Vec::with_capacity(manifests.len());
    let mut emitted: HashSet<&str> = HashSet::with_capacity(manifests.len());
    let mut remaining: Vec<usize> = (0..manifests.len()).collect();

    loop {
        let before = ordered.len();
        remaining.retain(|&i| {
            let ready = match manifests[i].parent.as_deref() {
                None => true,
                Some(p) => already_applied.contains(p) || emitted.contains(p),
            };
            if ready {
                emitted.insert(manifests[i].name.as_str());
                ordered.push(i);
                false
            } else {
                true
            }
        });
        if ordered.len() == before {
            break;
        }
    }

    (ordered, remaining)
}

/// The union of change hashes across a set of manifests.
///
/// Changes are content-addressed and shared across views (a draft's log
/// includes its inherited prefix), so the union is deduplicated: each hash
/// appears once, at its first occurrence. Iterating manifests root→leaf
/// therefore yields parents' changes before children's own suffixes.
pub fn change_union(manifests: &[ViewManifest]) -> Vec<Hash> {
    let mut seen: HashSet<Hash> = HashSet::new();
    let mut union = Vec::new();
    for manifest in manifests {
        for hash in &manifest.changes {
            if seen.insert(*hash) {
                union.push(*hash);
            }
        }
    }
    union
}

// Inventory Support Detection

/// Interpretation of an `?views` inventory response under `--all-views`.
#[derive(Debug, PartialEq, Eq)]
pub enum InventoryOutcome {
    /// The inventory lists views beyond those already applied — clone them.
    Views(Vec<String>),
    /// The inventory lists only views the primary chain already applied —
    /// a genuinely single-chain remote; nothing more to do.
    NothingNew,
    /// The inventory is empty. A server that supports `?views` always lists
    /// at least the view that was just cloned, so an empty inventory is the
    /// signature of a server without inventory support (its generic info
    /// blob parses to zero views). Under `--all-views` this must be a hard
    /// error, never a silent single-view clone.
    Unsupported,
}

/// Classify an `?views` inventory for `--all-views`.
///
/// `applied` is the set of view names the primary manifest chain already
/// reconstructed (always non-empty by the time additional views are
/// considered).
pub fn classify_inventory(inventory: &[String], applied: &HashSet<String>) -> InventoryOutcome {
    if inventory.is_empty() {
        return InventoryOutcome::Unsupported;
    }
    let others: Vec<String> = inventory
        .iter()
        .filter(|name| !applied.contains(*name))
        .cloned()
        .collect();
    if others.is_empty() {
        InventoryOutcome::NothingNew
    } else {
        InventoryOutcome::Views(others)
    }
}

// Error Conversion

/// Convert a remote error to a CLI error.
///
/// Maps the various remote error types to appropriate CLI error types
/// with user-friendly messages and suggestions.
///
/// # Arguments
///
/// * `err` - The remote error to convert
/// * `url` - The remote URL (for context in error messages)
///
/// # Returns
///
/// A `CliError` that can be displayed to the user.
///
/// # Example
///
/// ```rust,ignore
/// let result = remote.get_state("main").await;
/// if let Err(e) = result {
///     return Err(convert_remote_error(e, &remote_url));
/// }
/// ```
pub fn convert_remote_error(err: RemoteError, url: &str) -> CliError {
    match err {
        RemoteError::ConnectionFailed { .. } => CliError::RemoteError {
            message: format!("Failed to connect: {}", err),
            url: Some(url.to_string()),
        },
        RemoteError::AuthenticationFailed { .. } => CliError::AuthenticationFailed {
            remote: url.to_string(),
        },
        RemoteError::RepositoryNotFound { .. } => CliError::RemoteError {
            message: "Repository not found on remote".to_string(),
            url: Some(url.to_string()),
        },
        RemoteError::ViewNotFound { view } => CliError::RemoteError {
            message: format!("View '{}' not found on remote", view),
            url: Some(url.to_string()),
        },
        RemoteError::ChangeNotFound { hash } => CliError::ChangeNotFound { hash },
        RemoteError::TagNotFound { state } => CliError::RemoteError {
            message: format!("Tag not found for state: {}", state),
            url: Some(url.to_string()),
        },
        RemoteError::MissingDependencies {
            count,
            missing_hashes,
        } => CliError::MissingDependency {
            change: "requested change".to_string(),
            dependency: if missing_hashes.is_empty() {
                format!("{} dependencies", count)
            } else {
                missing_hashes.join(", ")
            },
        },
        RemoteError::StateMismatch {
            remote_state,
            requested_state,
        } => CliError::Conflict {
            description: format!(
                "State mismatch: remote is at {}, requested {}",
                remote_state, requested_state
            ),
        },
        RemoteError::Timeout { seconds } => CliError::RemoteError {
            message: format!("Request timed out after {} seconds", seconds),
            url: Some(url.to_string()),
        },
        RemoteError::EmptyView { view } => CliError::RemoteError {
            message: format!("View '{}' is empty", view),
            url: Some(url.to_string()),
        },
        _ => CliError::RemoteError {
            message: err.to_string(),
            url: Some(url.to_string()),
        },
    }
}

// Formatting Utilities

/// Format a count with proper pluralization.
///
/// # Arguments
///
/// * `count` - The number of items
/// * `singular` - The singular form of the word (e.g., "change")
///
/// # Returns
///
/// A string like "1 change" or "5 changes".
///
/// # Example
///
/// ```rust
/// use atomic::commands::clone::helpers::format_count;
///
/// assert_eq!(format_count(0, "change"), "0 changes");
/// assert_eq!(format_count(1, "change"), "1 change");
/// assert_eq!(format_count(5, "change"), "5 changes");
/// ```
pub fn format_count(count: usize, singular: &str) -> String {
    if count == 1 {
        format!("{} {}", count, singular)
    } else {
        format!("{} {}s", count, singular)
    }
}

/// Format bytes in a human-readable way.
///
/// # Arguments
///
/// * `bytes` - The number of bytes
///
/// # Returns
///
/// A formatted string like "1.5 KB" or "2.3 MB".
///
/// # Example
///
/// ```rust
/// use atomic::commands::clone::helpers::format_bytes;
///
/// assert_eq!(format_bytes(512), "512 B");
/// assert_eq!(format_bytes(1536), "1.5 KB");
/// ```
/// Normalize a URL by removing trailing slashes.
///
/// # Arguments
///
/// * `url` - The URL to normalize
///
/// # Returns
///
/// The URL with any trailing slashes removed.
pub fn normalize_url(url: &str) -> String {
    url.trim_end_matches('/').to_string()
}

/// Clean up a directory on error (e.g., failed clone).
///
/// Silently removes the directory and all its contents. Does nothing
/// if the path doesn't exist.
///
/// # Arguments
///
/// * `path` - The directory to remove
pub fn cleanup_on_error(path: &Path) {
    if path.exists() {
        let _ = std::fs::remove_dir_all(path);
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // URL Parsing Tests

    /// Test inferring repo name from atomic-api URL pattern.
    #[test]
    fn test_infer_repo_name_atomic_api() {
        let url = "https://api.example.com/tenant/t/portfolio/p/project/pr/code";
        assert_eq!(infer_repo_name(url), Some("pr".to_string()));
    }

    /// Test inferring repo name from simple URL.
    #[test]
    fn test_infer_repo_name_simple() {
        let url = "https://example.com/org/my-project/code";
        assert_eq!(infer_repo_name(url), Some("my-project".to_string()));
    }

    /// Test inferring repo name from URL with .git suffix.
    #[test]
    fn test_infer_repo_name_git_suffix() {
        let url = "https://github.com/user/repo.git";
        assert_eq!(infer_repo_name(url), Some("repo".to_string()));
    }

    /// Test inferring repo name from URL without /code.
    #[test]
    fn test_infer_repo_name_no_code() {
        let url = "https://example.com/project";
        assert_eq!(infer_repo_name(url), Some("project".to_string()));
    }

    /// Test inferring repo name from URL with trailing slash.
    #[test]
    fn test_infer_repo_name_trailing_slash() {
        let url = "https://example.com/org/project/";
        assert_eq!(infer_repo_name(url), Some("project".to_string()));
    }

    /// Test inferring repo name from minimal URL.
    #[test]
    fn test_infer_repo_name_minimal() {
        let url = "https://example.com/repo";
        assert_eq!(infer_repo_name(url), Some("repo".to_string()));
    }

    /// Test normalize_url removes trailing slash.
    #[test]
    fn test_normalize_url() {
        assert_eq!(normalize_url("https://example.com/"), "https://example.com");
        assert_eq!(
            normalize_url("https://example.com/repo/"),
            "https://example.com/repo"
        );
        assert_eq!(
            normalize_url("https://example.com/repo"),
            "https://example.com/repo"
        );
    }

    // Path Validation Tests

    /// Test validate_target_path with non-existent path.
    #[test]
    fn test_validate_target_path_not_exists() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("new-repo");

        assert!(validate_target_path(&target).is_ok());
    }

    /// Test validate_target_path with existing directory.
    #[test]
    fn test_validate_target_path_exists() {
        let temp = tempdir().unwrap();
        let target = temp.path();

        let result = validate_target_path(target);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            CliError::RepositoryExists { .. }
        ));
    }

    /// Test resolve_target_path with explicit path.
    #[test]
    fn test_resolve_target_path_explicit() {
        let path = resolve_target_path("https://example.com/repo", Some("my-dir".to_string()));
        assert_eq!(path, PathBuf::from("my-dir"));
    }

    /// Test resolve_target_path with inferred path.
    #[test]
    fn test_resolve_target_path_inferred() {
        let path = resolve_target_path("https://example.com/org/project/code", None);
        assert_eq!(path, PathBuf::from("project"));
    }

    /// Test resolve_target_path fallback.
    #[test]
    fn test_resolve_target_path_fallback() {
        // This is a somewhat contrived URL that might not parse well
        let path = resolve_target_path("", None);
        assert_eq!(path, PathBuf::from("repo"));
    }

    // CleanupGuard Tests

    /// Test CleanupGuard creation.
    #[test]
    fn test_cleanup_guard_new() {
        let guard = CleanupGuard::new(PathBuf::from("/tmp/test"));
        assert!(!guard.is_disabled());
        assert_eq!(guard.path(), Path::new("/tmp/test"));
    }

    /// Test CleanupGuard disable.
    #[test]
    fn test_cleanup_guard_disable() {
        let guard = CleanupGuard::new(PathBuf::from("/tmp/test"));
        guard.disable();
        // Can't check is_disabled() after disable() since it consumes self
    }

    /// Test CleanupGuard actually cleans up on drop.
    #[test]
    fn test_cleanup_guard_drop_cleans_up() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("to-cleanup");

        // Create the directory
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("file.txt"), b"content").unwrap();
        assert!(target.exists());

        // Create guard and let it drop
        {
            let _guard = CleanupGuard::new(target.clone());
            // Guard drops here
        }

        // Directory should be removed
        assert!(!target.exists());
    }

    /// Test CleanupGuard doesn't clean up when disabled.
    #[test]
    fn test_cleanup_guard_drop_disabled() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("to-keep");

        // Create the directory
        std::fs::create_dir(&target).unwrap();
        assert!(target.exists());

        // Create guard, disable, and let it drop
        {
            let guard = CleanupGuard::new(target.clone());
            guard.disable();
            // Guard drops here
        }

        // Directory should still exist
        assert!(target.exists());
    }

    /// Test CleanupGuard handles non-existent path gracefully.
    #[test]
    fn test_cleanup_guard_nonexistent() {
        let guard = CleanupGuard::new(PathBuf::from("/nonexistent/path/12345"));
        // Should not panic when dropped
        drop(guard);
    }

    /// Test cleanup_on_error function.
    #[test]
    fn test_cleanup_on_error() {
        let temp = tempdir().unwrap();
        let target = temp.path().join("failed-clone");

        std::fs::create_dir(&target).unwrap();
        assert!(target.exists());

        cleanup_on_error(&target);

        assert!(!target.exists());
    }

    /// Test cleanup_on_error with non-existent path.
    #[test]
    fn test_cleanup_on_error_nonexistent() {
        // Should not panic
        cleanup_on_error(Path::new("/nonexistent/path/12345"));
    }

    // Error Conversion Tests

    /// Test converting connection failed error.
    #[test]
    fn test_convert_connection_failed() {
        let err = RemoteError::connection_failed(
            "http://example.com",
            std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused"),
        );
        let cli_err = convert_remote_error(err, "http://example.com");

        match cli_err {
            CliError::RemoteError { message, url } => {
                assert!(message.contains("Failed to connect"));
                assert_eq!(url, Some("http://example.com".to_string()));
            }
            _ => panic!("Expected RemoteError"),
        }
    }

    /// Test converting authentication failed error.
    #[test]
    fn test_convert_auth_failed() {
        let err = RemoteError::auth_failed("http://example.com", "bad token");
        let cli_err = convert_remote_error(err, "http://example.com");

        assert!(matches!(cli_err, CliError::AuthenticationFailed { .. }));
    }

    /// Test converting repository not found error.
    #[test]
    fn test_convert_repo_not_found() {
        let err = RemoteError::repo_not_found("http://example.com/repo");
        let cli_err = convert_remote_error(err, "http://example.com/repo");

        match cli_err {
            CliError::RemoteError { message, .. } => {
                assert!(message.contains("not found"));
            }
            _ => panic!("Expected RemoteError"),
        }
    }

    /// Test converting view not found error.
    #[test]
    fn test_convert_view_not_found() {
        let err = RemoteError::view_not_found("missing-stack");
        let cli_err = convert_remote_error(err, "http://example.com");

        match cli_err {
            CliError::RemoteError { message, .. } => {
                assert!(message.contains("missing-stack"));
            }
            _ => panic!("Expected RemoteError"),
        }
    }

    /// Test converting change not found error.
    #[test]
    fn test_convert_change_not_found() {
        let err = RemoteError::change_not_found("ABC123");
        let cli_err = convert_remote_error(err, "http://example.com");

        match cli_err {
            CliError::ChangeNotFound { hash } => {
                assert_eq!(hash, "ABC123");
            }
            _ => panic!("Expected ChangeNotFound"),
        }
    }

    /// Test converting missing dependencies error.
    #[test]
    fn test_convert_missing_deps() {
        let err = RemoteError::missing_deps(vec!["DEP1".to_string(), "DEP2".to_string()]);
        let cli_err = convert_remote_error(err, "http://example.com");

        assert!(matches!(cli_err, CliError::MissingDependency { .. }));
    }

    /// Test converting timeout error.
    #[test]
    fn test_convert_timeout() {
        let err = RemoteError::timeout(30);
        let cli_err = convert_remote_error(err, "http://example.com");

        match cli_err {
            CliError::RemoteError { message, .. } => {
                assert!(message.contains("30 seconds"));
            }
            _ => panic!("Expected RemoteError"),
        }
    }

    /// Test converting empty view error.
    #[test]
    fn test_convert_empty_view() {
        let err = RemoteError::empty_view("main");
        let cli_err = convert_remote_error(err, "http://example.com");

        match cli_err {
            CliError::RemoteError { message, .. } => {
                assert!(message.contains("main"));
                assert!(message.contains("empty"));
            }
            _ => panic!("Expected RemoteError"),
        }
    }

    /// Test converting tag not found error.
    #[test]
    fn test_convert_tag_not_found() {
        let err = RemoteError::tag_not_found("STATE123");
        let cli_err = convert_remote_error(err, "http://example.com");

        match cli_err {
            CliError::RemoteError { message, .. } => {
                assert!(message.contains("Tag not found"));
                assert!(message.contains("STATE123"));
            }
            _ => panic!("Expected RemoteError"),
        }
    }

    /// Test converting state mismatch error.
    #[test]
    fn test_convert_state_mismatch() {
        let err = RemoteError::state_mismatch("ABC", "DEF");
        let cli_err = convert_remote_error(err, "http://example.com");

        assert!(matches!(cli_err, CliError::Conflict { .. }));
    }

    // Formatting Tests

    /// Test format_count with zero.
    #[test]
    fn test_format_count_zero() {
        assert_eq!(format_count(0, "change"), "0 changes");
    }

    /// Test format_count with one (singular).
    #[test]
    fn test_format_count_one() {
        assert_eq!(format_count(1, "change"), "1 change");
    }

    /// Test format_count with many (plural).
    #[test]
    fn test_format_count_many() {
        assert_eq!(format_count(5, "change"), "5 changes");
        assert_eq!(format_count(100, "file"), "100 files");
    }

    /// Test format_count with different words.
    #[test]
    fn test_format_count_different_words() {
        assert_eq!(format_count(1, "tag"), "1 tag");
        assert_eq!(format_count(2, "tag"), "2 tags");
    }

    /// Test format_bytes with small values.
    #[test]
    fn test_format_bytes_small() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1023), "1023 B");
    }

    /// Test format_bytes with kilobytes.
    #[test]
    fn test_format_bytes_kb() {
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1536), "1.5 KB");
    }

    /// Test format_bytes with megabytes.
    #[test]
    fn test_format_bytes_mb() {
        assert_eq!(format_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(format_bytes(2 * 1024 * 1024 + 512 * 1024), "2.5 MB");
    }

    /// Test format_bytes with gigabytes.
    #[test]
    fn test_format_bytes_gb() {
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.0 GB");
    }

    // View Manifest Helper Tests

    use atomic_core::pristine::ViewScope;
    use std::collections::HashSet;

    /// Deterministic test hash.
    fn h(n: u8) -> Hash {
        Hash::of(&[n])
    }

    /// Build a manifest with a computed (consistent) state.
    fn manifest(
        name: &str,
        scope: ViewScope,
        parent: Option<&str>,
        changes: Vec<Hash>,
    ) -> ViewManifest {
        ViewManifest::new(name, scope, parent.map(String::from), changes)
    }

    /// A leaf→root chain orders root→leaf for apply.
    #[test]
    fn test_manifest_apply_order_chain_root_to_leaf() {
        // Collected in walk order (leaf first): feature → dev → main.
        let manifests = vec![
            manifest("feature", ViewScope::Draft, Some("dev"), vec![h(1), h(2)]),
            manifest("dev", ViewScope::Shared, Some("main"), vec![h(1)]),
            manifest("main", ViewScope::Shared, None, vec![]),
        ];

        let (ordered, stuck) = manifest_apply_order(&manifests, &HashSet::new());
        assert!(stuck.is_empty());
        let names: Vec<&str> = ordered
            .iter()
            .map(|&i| manifests[i].name.as_str())
            .collect();
        assert_eq!(names, vec!["main", "dev", "feature"]);
    }

    /// A parent already applied locally counts as satisfied.
    #[test]
    fn test_manifest_apply_order_uses_already_applied() {
        let manifests = vec![manifest(
            "feature",
            ViewScope::Draft,
            Some("dev"),
            vec![h(1)],
        )];
        let applied: HashSet<String> = ["dev".to_string()].into_iter().collect();

        let (ordered, stuck) = manifest_apply_order(&manifests, &applied);
        assert_eq!(ordered, vec![0]);
        assert!(stuck.is_empty());
    }

    /// A parent cycle can never be ordered: both manifests end up stuck.
    #[test]
    fn test_manifest_apply_order_detects_cycle() {
        let manifests = vec![
            manifest("a", ViewScope::Shared, Some("b"), vec![h(1)]),
            manifest("b", ViewScope::Shared, Some("a"), vec![h(2)]),
        ];

        let (ordered, stuck) = manifest_apply_order(&manifests, &HashSet::new());
        assert!(ordered.is_empty());
        assert_eq!(stuck.len(), 2);
    }

    /// A parent that is neither in the set nor applied leaves the child
    /// stuck without blocking unrelated manifests.
    #[test]
    fn test_manifest_apply_order_missing_parent_is_stuck() {
        let manifests = vec![
            manifest("orphan", ViewScope::Draft, Some("ghost"), vec![h(1)]),
            manifest("main", ViewScope::Shared, None, vec![h(2)]),
        ];

        let (ordered, stuck) = manifest_apply_order(&manifests, &HashSet::new());
        assert_eq!(ordered, vec![1]);
        assert_eq!(stuck, vec![0]);
    }

    /// The union dedupes hashes shared between manifests (a draft's
    /// inherited prefix repeats its ancestors' changes) and preserves
    /// first-occurrence order.
    #[test]
    fn test_change_union_dedupes_across_manifests() {
        let manifests = vec![
            manifest("main", ViewScope::Shared, None, vec![h(1), h(2)]),
            manifest(
                "dev",
                ViewScope::Shared,
                Some("main"),
                vec![h(1), h(2), h(3)],
            ),
            manifest(
                "feature",
                ViewScope::Draft,
                Some("dev"),
                vec![h(1), h(2), h(3), h(4)],
            ),
        ];

        let union = change_union(&manifests);
        assert_eq!(union, vec![h(1), h(2), h(3), h(4)]);
    }

    /// The union of no manifests (or all-empty manifests) is empty.
    #[test]
    fn test_change_union_empty() {
        assert!(change_union(&[]).is_empty());
        let manifests = vec![manifest("main", ViewScope::Shared, None, vec![])];
        assert!(change_union(&manifests).is_empty());
    }

    /// A valid manifest for the requested view parses and verifies.
    #[test]
    fn test_parse_remote_manifest_round_trip() {
        let m = manifest("dev", ViewScope::Shared, Some("main"), vec![h(1), h(2)]);
        let parsed = parse_remote_manifest("dev", &m.to_text(), "http://example.com").unwrap();
        assert_eq!(parsed, m);
    }

    /// A manifest naming a different view than requested is rejected.
    #[test]
    fn test_parse_remote_manifest_rejects_name_mismatch() {
        let m = manifest("other", ViewScope::Shared, None, vec![h(1)]);
        let err = parse_remote_manifest("dev", &m.to_text(), "http://example.com").unwrap_err();
        match err {
            CliError::RemoteError { message, .. } => {
                assert!(message.contains("Manifest name mismatch"));
            }
            other => panic!("Expected RemoteError, got {:?}", other),
        }
    }

    /// A declared state that doesn't fold from the log is rejected.
    #[test]
    fn test_parse_remote_manifest_rejects_state_mismatch() {
        let mut m = manifest("dev", ViewScope::Shared, None, vec![h(1)]);
        m.state = atomic_core::types::Merkle::of(b"tampered");
        let err = parse_remote_manifest("dev", &m.to_text(), "http://example.com").unwrap_err();
        match err {
            CliError::RemoteError { message, .. } => {
                assert!(message.contains("Corrupt manifest"));
            }
            other => panic!("Expected RemoteError, got {:?}", other),
        }
    }

    /// An empty inventory is the no-support signature: a supporting server
    /// always lists at least the view that was just cloned.
    #[test]
    fn test_classify_inventory_empty_is_unsupported() {
        let applied: HashSet<String> = ["dev".to_string()].into();
        assert_eq!(
            classify_inventory(&[], &applied),
            InventoryOutcome::Unsupported
        );
    }

    /// An inventory listing only already-applied views is a genuinely
    /// single-chain remote — quiet success.
    #[test]
    fn test_classify_inventory_only_applied_is_nothing_new() {
        let applied: HashSet<String> = ["dev".to_string()].into();
        assert_eq!(
            classify_inventory(&["dev".to_string()], &applied),
            InventoryOutcome::NothingNew
        );
    }

    /// Views beyond the applied set are returned for cloning; applied ones
    /// are filtered out.
    #[test]
    fn test_classify_inventory_returns_unapplied_views() {
        let applied: HashSet<String> = ["dev".to_string()].into();
        let inventory = vec!["dev".to_string(), "snowy-mountain-75eb".to_string()];
        assert_eq!(
            classify_inventory(&inventory, &applied),
            InventoryOutcome::Views(vec!["snowy-mountain-75eb".to_string()])
        );
    }
}
