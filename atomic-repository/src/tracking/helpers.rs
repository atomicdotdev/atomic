//! Path normalization, ignore rules, and file collection helpers.

use std::path::{Path, PathBuf};

use crate::ignore::IgnoreRules;
use crate::status::is_always_ignored;

use super::{TrackingError, TrackingOptions, TrackingResult, MAX_RECURSION_DEPTH};

/// - Removes trailing slashes
/// - Strips absolute path prefix if it matches repo root
/// - No trailing slash (except for root)
/// - Relative to repository root
pub fn normalize_path(path: &Path) -> String {
    normalize_path_with_root(path, None)
}

/// Normalize a path for storage, optionally stripping a repo root prefix.
///
/// This handles the case where absolute paths are accidentally passed in.
/// On macOS, `/tmp` is a symlink to `/private/tmp`, so we try both the
/// given root and its canonical form.
///
/// # Arguments
///
/// * `path` - The path to normalize
/// * `repo_root` - Optional repository root to strip from absolute paths
pub fn normalize_path_with_root(path: &Path, repo_root: Option<&Path>) -> String {
    let mut path_to_normalize = path.to_path_buf();

    // If path is absolute and we have a repo root, try to make it relative.
    // We check both Path::is_absolute() (handles native absolute paths) and
    // a leading '/' in the string representation (handles Unix-style paths on
    // Windows, where "/repo/src" is not considered absolute by the OS but must
    // still be treated as absolute for prefix-stripping purposes).
    let path_str_raw = path.to_string_lossy();
    let is_absolute = path_to_normalize.is_absolute() || path_str_raw.starts_with('/');

    if is_absolute {
        if let Some(root) = repo_root {
            // Try stripping the root directly
            if let Ok(rel) = path_to_normalize.strip_prefix(root) {
                path_to_normalize = rel.to_path_buf();
            } else if let Ok(canonical_root) = root.canonicalize() {
                // On macOS, /tmp -> /private/tmp, so try canonical
                if let Ok(rel) = path_to_normalize.strip_prefix(&canonical_root) {
                    path_to_normalize = rel.to_path_buf();
                }
            } else {
                // Path::strip_prefix uses OS path semantics, which on Windows
                // won't match Unix-style "/repo" against "/repo/src/main.rs"
                // as a proper prefix.  Fall back to string-level stripping so
                // that Unix-style paths work correctly on Windows in tests and
                // cross-platform scenarios.
                let root_str = root.to_string_lossy();
                let root_str = root_str.replace('\\', "/");
                let path_str = path_str_raw.replace('\\', "/");
                let root_with_sep = if root_str.ends_with('/') {
                    root_str.to_string()
                } else {
                    format!("{}/", root_str)
                };
                if let Some(rel) = path_str.strip_prefix(root_with_sep.as_str()) {
                    path_to_normalize = PathBuf::from(rel);
                } else if path_str == root_str {
                    path_to_normalize = PathBuf::new();
                }
            }
        }
    }

    let path_str = path_to_normalize.to_string_lossy();

    // Convert to forward slashes and remove trailing slash
    let normalized = path_str
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_string();

    // Handle empty path (current directory)
    if normalized.is_empty() {
        ".".to_string()
    } else {
        normalized
    }
}

/// Check if a path should be ignored during tracking.
///
/// This checks (in order):
/// 1. Internal directories (.atomic, .git) - always ignored
/// 2. `.atomicignore` patterns (if rules provided)
/// 3. Hidden files (if not included)
///
/// # Arguments
///
/// * `path` - Path to check (relative to repository root)
/// * `include_hidden` - Whether to include hidden files (starting with '.')
///
/// # Example
///
/// ```rust,ignore
/// use atomic_repository::tracking::should_ignore;
///
/// // Without ignore rules
/// assert!(should_ignore(Path::new(".atomic"), true, None));
/// assert!(!should_ignore(Path::new("src/main.rs"), true, None));
///
/// // With ignore rules
/// let rules = IgnoreRules::load(repo_root);
/// assert!(should_ignore(Path::new("target/debug"), true, Some(&rules)));
/// ```
pub fn should_ignore(path: &Path, include_hidden: bool) -> bool {
    should_ignore_with_rules(path, include_hidden, false, None)
}

/// Check if a path should be ignored during tracking, with optional ignore rules.
///
/// This is the full version that accepts optional [`IgnoreRules`] for pattern matching.
///
/// # Arguments
///
/// * `path` - Path to check (relative to repository root)
/// * `include_hidden` - Whether to include hidden files (starting with '.')
/// * `is_dir` - Whether the path is a directory
/// * `rules` - Optional ignore rules from `.atomicignore` files
///
/// # Returns
///
/// `true` if the path should be ignored, `false` otherwise.
pub fn should_ignore_with_rules(
    path: &Path,
    include_hidden: bool,
    is_dir: bool,
    rules: Option<&IgnoreRules>,
) -> bool {
    // Always ignore internal directories
    if is_always_ignored(path) {
        return true;
    }

    // Check ignore rules if provided
    if let Some(rules) = rules {
        if rules.is_ignored(path, is_dir) {
            return true;
        }
    }

    // Check for hidden files
    if !include_hidden {
        if let Some(name) = path.file_name() {
            if let Some(name_str) = name.to_str() {
                if name_str.starts_with('.') {
                    return true;
                }
            }
        }
    }

    false
}

/// Collect all files in a directory for tracking.
///
/// This walks the directory tree and returns paths relative to the given root.
/// Files matching `.atomicignore` patterns are excluded.
///
/// # Arguments
///
/// * `root` - Repository root directory
/// * `path` - Path to collect files from (relative to root)
/// * `options` - Tracking options
///
/// # Returns
///
/// A vector of paths relative to the repository root.
pub fn collect_files_for_tracking(
    root: &Path,
    path: &Path,
    options: &TrackingOptions,
) -> TrackingResult<Vec<PathBuf>> {
    collect_files_for_tracking_with_rules(root, path, options, None)
}

/// Collect all files in a directory for tracking, with optional ignore rules.
///
/// This is the full version that accepts optional [`IgnoreRules`] for pattern matching.
///
/// # Arguments
///
/// * `root` - Repository root directory
/// * `path` - Path to collect files from (relative to root)
/// * `options` - Tracking options
/// * `rules` - Optional ignore rules from `.atomicignore` files
///
/// # Returns
///
/// A vector of paths relative to the repository root.
pub fn collect_files_for_tracking_with_rules(
    root: &Path,
    path: &Path,
    options: &TrackingOptions,
    rules: Option<&IgnoreRules>,
) -> TrackingResult<Vec<PathBuf>> {
    let mut files = Vec::new();
    let abs_path = root.join(path);

    if !abs_path.exists() {
        return Err(TrackingError::PathNotFound {
            path: path.display().to_string(),
        });
    }

    if abs_path.is_file() {
        // Single file
        if !should_ignore_with_rules(path, options.include_hidden, false, rules) {
            files.push(path.to_path_buf());
        }
    } else if abs_path.is_dir() {
        if options.recursive {
            // Walk the directory
            let walker = walkdir::WalkDir::new(&abs_path)
                .max_depth(MAX_RECURSION_DEPTH)
                .follow_links(false)
                .into_iter()
                .filter_entry(|e| {
                    let entry_path = e.path();
                    if let Ok(rel) = entry_path.strip_prefix(root) {
                        let is_dir = e.file_type().is_dir();
                        !should_ignore_with_rules(rel, options.include_hidden, is_dir, rules)
                    } else {
                        true
                    }
                });

            for entry in walker {
                let entry = entry?;
                let entry_path = entry.path();

                // Get path relative to repository root
                if let Ok(rel_path) = entry_path.strip_prefix(root) {
                    // Only track files, not directories
                    // Directories are implicitly tracked through their contents
                    if entry_path.is_dir() {
                        continue;
                    }

                    // Include only files
                    files.push(rel_path.to_path_buf());
                }
            }
        } else {
            // Non-recursive: just add the directory itself
            if !should_ignore_with_rules(path, options.include_hidden, true, rules) {
                files.push(path.to_path_buf());
            }
        }
    }

    Ok(files)
}
