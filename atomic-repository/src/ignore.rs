//! Ignore pattern matching for Atomic VCS
//!
//! This module parses and applies `.atomicignore` patterns using gitignore syntax.
//! It supports both global ignore rules (from `~/.config/atomic/ignore`) and
//! repository-local rules (from `.atomicignore` in the repository root).
//!
//! # Pattern Syntax
//!
//! The ignore file uses the same syntax as `.gitignore`:
//!
//! - Blank lines are ignored
//! - Lines starting with `#` are comments
//! - `!` prefix negates a pattern (whitelist)
//! - `/` at the end matches directories only
//! - `*` matches anything except `/`
//! - `**` matches anything including `/`
//! - `?` matches any single character except `/`
//!
//! # Priority
//!
//! Local `.atomicignore` rules take precedence over global rules.
//! Within each file, later rules override earlier ones.
//!
//! # Example
//!
//! ```text
//! # .atomicignore
//! target/
//! *.log
//! !important.log
//! ```

use std::path::Path;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use thiserror::Error;

// Error Types

/// Result type for ignore operations.
pub type IgnoreResult<T> = Result<T, IgnoreError>;

/// Errors that can occur during ignore pattern operations.
#[derive(Error, Debug)]
pub enum IgnoreError {
    /// Failed to parse ignore file
    #[error("failed to parse ignore file at {path}: {details}")]
    ParseError {
        /// Path to the ignore file
        path: String,
        /// Error details
        details: String,
    },

    /// I/O error reading ignore file
    #[error("I/O error reading ignore file: {0}")]
    Io(#[from] std::io::Error),
}

// IgnoreRules

/// Ignore rules loaded from various sources.
///
/// This struct aggregates ignore patterns from:
/// - Global config: `~/.config/atomic/ignore`
/// - Repository-local: `.atomicignore` in repository root
///
/// # Example
///
/// ```rust,ignore
/// use atomic_repository::ignore::IgnoreRules;
/// use std::path::Path;
///
/// let rules = IgnoreRules::load(Path::new("/path/to/repo"));
///
/// // Check if a file should be ignored
/// if rules.is_ignored(Path::new("target/debug/main"), false) {
///     println!("File is ignored");
/// }
///
/// // Check if a directory should be ignored
/// if rules.is_ignored(Path::new("node_modules"), true) {
///     println!("Directory is ignored");
/// }
/// ```
#[derive(Debug)]
pub struct IgnoreRules {
    /// Global ignore rules from ~/.config/atomic/ignore
    global: Option<Gitignore>,
    /// Repository-level rules from .atomicignore
    local: Option<Gitignore>,
    /// Repository root path (for relative path resolution)
    repo_root: std::path::PathBuf,
}

impl IgnoreRules {
    /// Load ignore rules for a repository.
    ///
    /// This loads rules from both global and local ignore files.
    /// Missing files are silently ignored (not an error).
    ///
    /// # Arguments
    ///
    /// * `repo_root` - The root directory of the repository
    ///
    /// # Returns
    ///
    /// An `IgnoreRules` instance with all loaded patterns.
    pub fn load(repo_root: &Path) -> Self {
        let global = load_global_ignore();
        let local = load_local_ignore(repo_root);
        Self {
            global,
            local,
            repo_root: repo_root.to_path_buf(),
        }
    }

    /// Load ignore rules for knowledge-graph enrichment.
    ///
    /// Like [`load`](Self::load), but the repository-local rules are built
    /// from `.atomicignore`, `.gitignore`, and `.ignore` (in that order, so
    /// later files take precedence on conflicting patterns). This lets
    /// `atomic query enrich` skip build artifacts and dependencies that those
    /// files exclude (e.g. `node_modules/`, `dist/`, `target/`) instead of
    /// creating knowledge-graph nodes for them.
    ///
    /// Only the files at the repository root are consulted; nested ignore
    /// files in subdirectories are not (matching the existing `.atomicignore`
    /// behavior of [`load`](Self::load)).
    pub fn load_for_enrichment(repo_root: &Path) -> Self {
        let global = load_global_ignore();
        let local = load_local_enrichment_ignore(repo_root);
        Self {
            global,
            local,
            repo_root: repo_root.to_path_buf(),
        }
    }

    /// Create an empty IgnoreRules (ignores nothing except built-in patterns).
    ///
    /// This is useful for testing or when you want to start fresh.
    pub fn empty(repo_root: &Path) -> Self {
        Self {
            global: None,
            local: None,
            repo_root: repo_root.to_path_buf(),
        }
    }

    /// Load ignore rules from a specific file only.
    ///
    /// This is useful for testing or when you want to use a custom ignore file.
    ///
    /// # Arguments
    ///
    /// * `repo_root` - The root directory of the repository
    /// * `ignore_path` - Path to the ignore file
    ///
    /// # Returns
    ///
    /// An `IgnoreRules` instance with patterns from the specified file.
    pub fn from_file(repo_root: &Path, ignore_path: &Path) -> IgnoreResult<Self> {
        let local = if ignore_path.exists() {
            let mut builder = GitignoreBuilder::new(repo_root);
            if let Some(err) = builder.add(ignore_path) {
                return Err(IgnoreError::ParseError {
                    path: ignore_path.display().to_string(),
                    details: err.to_string(),
                });
            }
            builder.build().ok()
        } else {
            None
        };

        Ok(Self {
            global: None,
            local,
            repo_root: repo_root.to_path_buf(),
        })
    }

    /// Check if a path should be ignored.
    ///
    /// This checks both local and global rules. Local rules take precedence.
    /// Whitelist patterns (`!pattern`) can un-ignore files.
    ///
    /// # Arguments
    ///
    /// * `path` - The path to check (relative to repository root)
    /// * `is_dir` - Whether the path is a directory
    ///
    /// # Returns
    ///
    /// `true` if the path should be ignored, `false` otherwise.
    pub fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        // Check local rules first (higher priority)
        if let Some(ref local) = self.local {
            match local.matched_path_or_any_parents(path, is_dir) {
                ignore::Match::Ignore(_) => return true,
                ignore::Match::Whitelist(_) => return false,
                ignore::Match::None => {}
            }
        }

        // Then check global rules
        if let Some(ref global) = self.global {
            match global.matched_path_or_any_parents(path, is_dir) {
                ignore::Match::Ignore(_) => return true,
                ignore::Match::Whitelist(_) => return false,
                ignore::Match::None => {}
            }
        }

        false
    }

    /// Check if a path should be ignored, using absolute path.
    ///
    /// This converts the absolute path to a relative path before checking.
    ///
    /// # Arguments
    ///
    /// * `abs_path` - The absolute path to check
    /// * `is_dir` - Whether the path is a directory
    ///
    /// # Returns
    ///
    /// `true` if the path should be ignored, `false` otherwise.
    pub fn is_ignored_abs(&self, abs_path: &Path, is_dir: bool) -> bool {
        if let Ok(rel_path) = abs_path.strip_prefix(&self.repo_root) {
            self.is_ignored(rel_path, is_dir)
        } else {
            // Path is not under repo root, don't ignore
            false
        }
    }

    /// Check if local ignore rules are loaded.
    pub fn has_local_rules(&self) -> bool {
        self.local.is_some()
    }

    /// Check if global ignore rules are loaded.
    pub fn has_global_rules(&self) -> bool {
        self.global.is_some()
    }

    /// Get the repository root path.
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// Reload ignore rules from disk.
    ///
    /// This is useful if the ignore files have changed.
    pub fn reload(&mut self) {
        self.global = load_global_ignore();
        self.local = load_local_ignore(&self.repo_root);
    }

    /// Get the number of patterns in local rules.
    ///
    /// Returns 0 if no local rules are loaded.
    pub fn local_pattern_count(&self) -> usize {
        self.local.as_ref().map(|g| g.len()).unwrap_or(0)
    }

    /// Get the number of patterns in global rules.
    ///
    /// Returns 0 if no global rules are loaded.
    pub fn global_pattern_count(&self) -> usize {
        self.global.as_ref().map(|g| g.len()).unwrap_or(0)
    }

    /// Debug method: check why a path matches or doesn't match.
    ///
    /// Returns a string describing the match result for debugging.
    pub fn debug_match(&self, path: &Path, is_dir: bool) -> String {
        let mut result = format!("Checking path: {:?}, is_dir: {}\n", path.display(), is_dir);

        result.push_str(&format!(
            "Local rules loaded: {}, pattern count: {}\n",
            self.local.is_some(),
            self.local_pattern_count()
        ));

        if let Some(ref local) = self.local {
            let matched = local.matched_path_or_any_parents(path, is_dir);
            result.push_str(&format!("Local match result: {:?}\n", matched));
        }

        result.push_str(&format!(
            "Global rules loaded: {}, pattern count: {}\n",
            self.global.is_some(),
            self.global_pattern_count()
        ));

        if let Some(ref global) = self.global {
            let matched = global.matched_path_or_any_parents(path, is_dir);
            result.push_str(&format!("Global match result: {:?}\n", matched));
        }

        result.push_str(&format!(
            "Final is_ignored: {}\n",
            self.is_ignored(path, is_dir)
        ));

        result
    }
}

impl Default for IgnoreRules {
    fn default() -> Self {
        Self {
            global: None,
            local: None,
            repo_root: std::path::PathBuf::new(),
        }
    }
}

// Helper Functions

/// Load global ignore rules from ~/.config/atomic/ignore
fn load_global_ignore() -> Option<Gitignore> {
    let config_dir = atomic_config::global_config_dir()?;
    let ignore_path = config_dir.join("ignore");

    if ignore_path.exists() {
        let mut builder = GitignoreBuilder::new(&config_dir);
        // builder.add() returns Option<Error> - log if there's an issue
        if let Some(err) = builder.add(&ignore_path) {
            log::warn!(
                "Failed to parse global ignore at {}: {}",
                ignore_path.display(),
                err
            );
            return None;
        }
        match builder.build() {
            Ok(gitignore) => Some(gitignore),
            Err(err) => {
                log::warn!(
                    "Failed to build global ignore rules from {}: {}",
                    ignore_path.display(),
                    err
                );
                None
            }
        }
    } else {
        None
    }
}

/// Load repository-local ignore rules from .atomicignore
fn load_local_ignore(repo_root: &Path) -> Option<Gitignore> {
    let ignore_path = repo_root.join(".atomicignore");

    if ignore_path.exists() {
        let mut builder = GitignoreBuilder::new(repo_root);
        // builder.add() returns Option<Error> - log if there's an issue
        if let Some(err) = builder.add(&ignore_path) {
            log::warn!(
                "Failed to parse .atomicignore at {}: {}",
                ignore_path.display(),
                err
            );
            return None;
        }
        match builder.build() {
            Ok(gitignore) => Some(gitignore),
            Err(err) => {
                log::warn!(
                    "Failed to build ignore rules from {}: {}",
                    ignore_path.display(),
                    err
                );
                None
            }
        }
    } else {
        None
    }
}

/// Load repository-local ignore rules for enrichment from `.atomicignore`,
/// `.gitignore`, and `.ignore` at the repository root.
///
/// All existing files are merged into a single matcher. Later files take
/// precedence on conflicting patterns (`.atomicignore` < `.gitignore` <
/// `.ignore`), following the `ignore` crate's last-match-wins semantics.
/// Returns `None` when none of the files exist or all fail to parse.
fn load_local_enrichment_ignore(repo_root: &Path) -> Option<Gitignore> {
    let mut builder = GitignoreBuilder::new(repo_root);
    let mut added = false;

    for name in [".atomicignore", ".gitignore", ".ignore"] {
        let ignore_path = repo_root.join(name);
        if !ignore_path.exists() {
            continue;
        }
        if let Some(err) = builder.add(&ignore_path) {
            log::warn!(
                "Failed to parse {} at {}: {}",
                name,
                ignore_path.display(),
                err
            );
            continue;
        }
        added = true;
    }

    if !added {
        return None;
    }

    match builder.build() {
        Ok(gitignore) => Some(gitignore),
        Err(err) => {
            log::warn!(
                "Failed to build enrichment ignore rules for {}: {}",
                repo_root.display(),
                err
            );
            None
        }
    }
}

/// True if `path` lives in an Atomic-internal directory that must never be
/// enriched into the knowledge graph or indexed for content search.
///
/// Covers the always-ignored `.atomic`/`.git` (shared with `status`) plus
/// `.vault`, which is internal for enrichment/search only: its entries are
/// otherwise tracked content, but they surface as their own semantic KG nodes
/// (`intent:`, `memory:`, `goal:`, `session:`…), so a `file:.vault/…` node or
/// content hit would be pure duplication. `.vault` is deliberately *not* added
/// to the shared `ALWAYS_IGNORED` set because `status`/tracking must keep
/// seeing it.
pub(crate) fn is_enrichment_internal(path: &Path) -> bool {
    if crate::status::is_always_ignored(path) {
        return true;
    }
    path.components().any(|component| {
        matches!(component, std::path::Component::Normal(name) if name.to_str() == Some(".vault"))
    })
}

/// Get the path to the global ignore file.
pub fn global_ignore_path() -> Option<std::path::PathBuf> {
    atomic_config::global_config_dir().map(|p| p.join("ignore"))
}

/// Get the path to the local ignore file for a repository.
pub fn local_ignore_path(repo_root: &Path) -> std::path::PathBuf {
    repo_root.join(".atomicignore")
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ------------------------------------------------------------------------
    // IgnoreRules Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_ignore_rules_empty() {
        let temp = TempDir::new().unwrap();
        let rules = IgnoreRules::empty(temp.path());

        assert!(!rules.has_local_rules());
        assert!(!rules.has_global_rules());
        assert!(!rules.is_ignored(Path::new("src/main.rs"), false));
        assert!(!rules.is_ignored(Path::new("target/debug"), true));
    }

    #[test]
    fn test_ignore_rules_load_missing_files() {
        let temp = TempDir::new().unwrap();
        let rules = IgnoreRules::load(temp.path());

        // Should not error when files don't exist
        assert!(!rules.has_local_rules());
        assert!(!rules.is_ignored(Path::new("anything.txt"), false));
    }

    #[test]
    fn test_load_for_enrichment_honors_gitignore_and_ignore() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join(".atomicignore"), "dist/\n").unwrap();
        std::fs::write(temp.path().join(".gitignore"), "node_modules/\n").unwrap();
        std::fs::write(temp.path().join(".ignore"), "scratch/\n").unwrap();

        let rules = IgnoreRules::load_for_enrichment(temp.path());

        // Patterns from all three files are honored.
        assert!(rules.is_ignored(Path::new("dist/index.js"), false));
        assert!(rules.is_ignored(Path::new("node_modules/typescript/lib.d.ts"), false));
        assert!(rules.is_ignored(Path::new("scratch/tmp.txt"), false));

        // Non-matching source files are not ignored.
        assert!(!rules.is_ignored(Path::new("src/index.ts"), false));
    }

    #[test]
    fn test_load_for_enrichment_ignores_nothing_without_files() {
        let temp = TempDir::new().unwrap();
        let rules = IgnoreRules::load_for_enrichment(temp.path());

        assert!(!rules.has_local_rules());
        assert!(!rules.is_ignored(Path::new("node_modules/pkg/index.js"), false));
    }

    #[test]
    fn test_load_for_enrichment_load_does_not_read_gitignore() {
        // The general-purpose `load` must remain `.atomicignore`-only so that
        // enabling gitignore semantics for enrichment does not silently change
        // VCS-wide behavior (status, tracking, switch).
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join(".gitignore"), "node_modules/\n").unwrap();

        let rules = IgnoreRules::load(temp.path());
        assert!(!rules.is_ignored(Path::new("node_modules/pkg/index.js"), false));
    }

    #[test]
    fn test_ignore_rules_basic_patterns() {
        let temp = TempDir::new().unwrap();
        let ignore_path = temp.path().join(".atomicignore");
        std::fs::write(&ignore_path, "target/\n*.log\n").unwrap();

        let rules = IgnoreRules::load(temp.path());

        assert!(rules.has_local_rules());
        assert!(rules.is_ignored(Path::new("target"), true));
        assert!(rules.is_ignored(Path::new("target/debug"), true));
        assert!(rules.is_ignored(Path::new("app.log"), false));
        assert!(rules.is_ignored(Path::new("logs/debug.log"), false));
        assert!(!rules.is_ignored(Path::new("src/main.rs"), false));
    }

    #[test]
    fn test_ignore_rules_whitelist() {
        let temp = TempDir::new().unwrap();
        let ignore_path = temp.path().join(".atomicignore");
        std::fs::write(&ignore_path, "*.log\n!important.log\n").unwrap();

        let rules = IgnoreRules::load(temp.path());

        assert!(rules.is_ignored(Path::new("debug.log"), false));
        assert!(rules.is_ignored(Path::new("error.log"), false));
        assert!(!rules.is_ignored(Path::new("important.log"), false));
    }

    #[test]
    fn test_ignore_rules_comments_and_blanks() {
        let temp = TempDir::new().unwrap();
        let ignore_path = temp.path().join(".atomicignore");
        std::fs::write(
            &ignore_path,
            "# This is a comment\n\n*.tmp\n\n# Another comment\nbuild/\n",
        )
        .unwrap();

        let rules = IgnoreRules::load(temp.path());

        assert!(rules.is_ignored(Path::new("file.tmp"), false));
        assert!(rules.is_ignored(Path::new("build"), true));
        assert!(!rules.is_ignored(Path::new("# This is a comment"), false));
    }

    #[test]
    fn test_ignore_rules_glob_patterns() {
        let temp = TempDir::new().unwrap();
        let ignore_path = temp.path().join(".atomicignore");
        std::fs::write(&ignore_path, "**/*.pyc\nsrc/**/*.bak\n").unwrap();

        let rules = IgnoreRules::load(temp.path());

        assert!(rules.is_ignored(Path::new("module.pyc"), false));
        assert!(rules.is_ignored(Path::new("lib/module.pyc"), false));
        assert!(rules.is_ignored(Path::new("lib/sub/module.pyc"), false));
        assert!(rules.is_ignored(Path::new("src/file.bak"), false));
        assert!(rules.is_ignored(Path::new("src/sub/file.bak"), false));
        assert!(!rules.is_ignored(Path::new("other/file.bak"), false));
    }

    #[test]
    fn test_ignore_rules_directory_only() {
        let temp = TempDir::new().unwrap();
        let ignore_path = temp.path().join(".atomicignore");
        std::fs::write(&ignore_path, "logs/\n").unwrap();

        let rules = IgnoreRules::load(temp.path());

        // Directory should be ignored
        assert!(rules.is_ignored(Path::new("logs"), true));
        // File named "logs" should NOT be ignored (pattern has trailing /)
        assert!(!rules.is_ignored(Path::new("logs"), false));
    }

    #[test]
    fn test_ignore_rules_is_ignored_abs() {
        let temp = TempDir::new().unwrap();
        let ignore_path = temp.path().join(".atomicignore");
        std::fs::write(&ignore_path, "target/\n").unwrap();

        let rules = IgnoreRules::load(temp.path());

        let abs_target = temp.path().join("target");
        let abs_src = temp.path().join("src/main.rs");

        assert!(rules.is_ignored_abs(&abs_target, true));
        assert!(!rules.is_ignored_abs(&abs_src, false));
    }

    #[test]
    fn test_ignore_rules_is_ignored_abs_outside_repo() {
        let temp = TempDir::new().unwrap();
        let ignore_path = temp.path().join(".atomicignore");
        std::fs::write(&ignore_path, "*\n").unwrap();

        let rules = IgnoreRules::load(temp.path());

        // Path outside repo should not be ignored
        assert!(!rules.is_ignored_abs(Path::new("/some/other/path"), false));
    }

    #[test]
    fn test_ignore_rules_from_file() {
        let temp = TempDir::new().unwrap();
        let custom_ignore = temp.path().join("custom.ignore");
        std::fs::write(&custom_ignore, "*.txt\n").unwrap();

        let rules = IgnoreRules::from_file(temp.path(), &custom_ignore).unwrap();

        assert!(rules.has_local_rules());
        assert!(rules.is_ignored(Path::new("readme.txt"), false));
        assert!(!rules.is_ignored(Path::new("readme.md"), false));
    }

    #[test]
    fn test_ignore_rules_from_file_missing() {
        let temp = TempDir::new().unwrap();
        let missing = temp.path().join("missing.ignore");

        let rules = IgnoreRules::from_file(temp.path(), &missing).unwrap();

        // Should not error, just have no rules
        assert!(!rules.has_local_rules());
    }

    #[test]
    fn test_ignore_rules_reload() {
        let temp = TempDir::new().unwrap();
        let ignore_path = temp.path().join(".atomicignore");

        // Start with no ignore file
        let mut rules = IgnoreRules::load(temp.path());
        assert!(!rules.has_local_rules());
        assert!(!rules.is_ignored(Path::new("target"), true));

        // Create ignore file
        std::fs::write(&ignore_path, "target/\n").unwrap();

        // Reload and verify
        rules.reload();
        assert!(rules.has_local_rules());
        assert!(rules.is_ignored(Path::new("target"), true));
    }

    #[test]
    fn test_ignore_rules_repo_root() {
        let temp = TempDir::new().unwrap();
        let rules = IgnoreRules::load(temp.path());

        assert_eq!(rules.repo_root(), temp.path());
    }

    #[test]
    fn test_ignore_rules_pattern_count() {
        let temp = TempDir::new().unwrap();
        let ignore_path = temp.path().join(".atomicignore");
        std::fs::write(&ignore_path, "target/\n*.log\nnode_modules\n").unwrap();

        let rules = IgnoreRules::load(temp.path());

        assert!(rules.has_local_rules());
        assert_eq!(rules.local_pattern_count(), 3);
    }

    #[test]
    fn test_ignore_rules_debug_match() {
        let temp = TempDir::new().unwrap();
        let ignore_path = temp.path().join(".atomicignore");
        std::fs::write(&ignore_path, "node_modules\n").unwrap();

        let rules = IgnoreRules::load(temp.path());

        let debug_output = rules.debug_match(Path::new("node_modules/foo.js"), false);
        assert!(debug_output.contains("Local rules loaded: true"));
        assert!(debug_output.contains("Final is_ignored: true"));
    }

    #[test]
    fn test_ignore_rules_default() {
        let rules = IgnoreRules::default();

        assert!(!rules.has_local_rules());
        assert!(!rules.has_global_rules());
        assert!(rules.repo_root().as_os_str().is_empty());
    }

    // ------------------------------------------------------------------------
    // Pattern Tests (comprehensive)
    // ------------------------------------------------------------------------

    #[test]
    fn test_pattern_asterisk() {
        let temp = TempDir::new().unwrap();
        let ignore_path = temp.path().join(".atomicignore");
        std::fs::write(&ignore_path, "*.o\n").unwrap();

        let rules = IgnoreRules::load(temp.path());

        assert!(rules.is_ignored(Path::new("main.o"), false));
        assert!(rules.is_ignored(Path::new("lib.o"), false));
        assert!(!rules.is_ignored(Path::new("main.c"), false));
    }

    #[test]
    fn test_pattern_question_mark() {
        let temp = TempDir::new().unwrap();
        let ignore_path = temp.path().join(".atomicignore");
        std::fs::write(&ignore_path, "file?.txt\n").unwrap();

        let rules = IgnoreRules::load(temp.path());

        assert!(rules.is_ignored(Path::new("file1.txt"), false));
        assert!(rules.is_ignored(Path::new("filea.txt"), false));
        assert!(!rules.is_ignored(Path::new("file.txt"), false));
        assert!(!rules.is_ignored(Path::new("file12.txt"), false));
    }

    #[test]
    fn test_pattern_double_asterisk() {
        let temp = TempDir::new().unwrap();
        let ignore_path = temp.path().join(".atomicignore");
        std::fs::write(&ignore_path, "**/temp\n").unwrap();

        let rules = IgnoreRules::load(temp.path());

        assert!(rules.is_ignored(Path::new("temp"), false));
        assert!(rules.is_ignored(Path::new("a/temp"), false));
        assert!(rules.is_ignored(Path::new("a/b/c/temp"), false));
    }

    #[test]
    fn test_pattern_leading_slash() {
        let temp = TempDir::new().unwrap();
        let ignore_path = temp.path().join(".atomicignore");
        std::fs::write(&ignore_path, "/root_only.txt\n").unwrap();

        let rules = IgnoreRules::load(temp.path());

        assert!(rules.is_ignored(Path::new("root_only.txt"), false));
        assert!(!rules.is_ignored(Path::new("sub/root_only.txt"), false));
    }

    #[test]
    fn test_pattern_bracket_range() {
        let temp = TempDir::new().unwrap();
        let ignore_path = temp.path().join(".atomicignore");
        std::fs::write(&ignore_path, "file[0-9].txt\n").unwrap();

        let rules = IgnoreRules::load(temp.path());

        assert!(rules.is_ignored(Path::new("file0.txt"), false));
        assert!(rules.is_ignored(Path::new("file5.txt"), false));
        assert!(rules.is_ignored(Path::new("file9.txt"), false));
        assert!(!rules.is_ignored(Path::new("filea.txt"), false));
    }

    // ------------------------------------------------------------------------
    // Real-World Pattern Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_rust_patterns() {
        let temp = TempDir::new().unwrap();
        let ignore_path = temp.path().join(".atomicignore");
        std::fs::write(
            &ignore_path,
            "target/\n**/*.rs.bk\nCargo.lock\n.idea/\n.vscode/\n",
        )
        .unwrap();

        let rules = IgnoreRules::load(temp.path());

        assert!(rules.is_ignored(Path::new("target"), true));
        assert!(rules.is_ignored(Path::new("target/debug/main"), false));
        assert!(rules.is_ignored(Path::new("src/main.rs.bk"), false));
        assert!(rules.is_ignored(Path::new("Cargo.lock"), false));
        assert!(rules.is_ignored(Path::new(".idea"), true));
        assert!(rules.is_ignored(Path::new(".vscode"), true));
        assert!(!rules.is_ignored(Path::new("src/main.rs"), false));
        assert!(!rules.is_ignored(Path::new("Cargo.toml"), false));
    }

    #[test]
    fn test_node_patterns() {
        let temp = TempDir::new().unwrap();
        let ignore_path = temp.path().join(".atomicignore");
        std::fs::write(
            &ignore_path,
            "node_modules/\ndist/\nbuild/\ncoverage/\n*.log\n.env\n.env.local\n",
        )
        .unwrap();

        let rules = IgnoreRules::load(temp.path());

        assert!(rules.is_ignored(Path::new("node_modules"), true));
        assert!(rules.is_ignored(Path::new("node_modules/lodash/index.js"), false));
        assert!(rules.is_ignored(Path::new("dist"), true));
        assert!(rules.is_ignored(Path::new("npm-debug.log"), false));
        assert!(rules.is_ignored(Path::new(".env"), false));
        assert!(rules.is_ignored(Path::new(".env.local"), false));
        assert!(!rules.is_ignored(Path::new("src/index.js"), false));
        assert!(!rules.is_ignored(Path::new("package.json"), false));
    }

    #[test]
    fn test_node_modules_no_trailing_newline() {
        // This tests the case where the file has no trailing newline
        let temp = TempDir::new().unwrap();
        let ignore_path = temp.path().join(".atomicignore");
        // Note: no trailing newline!
        std::fs::write(&ignore_path, "node_modules").unwrap();

        let rules = IgnoreRules::load(temp.path());

        // Should still work without trailing newline
        assert!(
            rules.has_local_rules(),
            "Should have local rules even without trailing newline"
        );
        assert!(
            rules.is_ignored(Path::new("node_modules"), true),
            "node_modules directory should be ignored"
        );
        assert!(
            rules.is_ignored(
                Path::new("node_modules/typescript/lib/lib.es2015.proxy.d.ts"),
                false
            ),
            "Files inside node_modules should be ignored"
        );
    }

    #[test]
    fn test_node_modules_no_trailing_slash() {
        // This tests the common case where users write "node_modules" without trailing slash
        let temp = TempDir::new().unwrap();
        let ignore_path = temp.path().join(".atomicignore");
        std::fs::write(&ignore_path, "node_modules\n").unwrap();

        let rules = IgnoreRules::load(temp.path());

        // Should ignore the directory itself
        assert!(
            rules.is_ignored(Path::new("node_modules"), true),
            "node_modules directory should be ignored"
        );

        // Should ignore subdirectories
        assert!(
            rules.is_ignored(Path::new("node_modules/typescript"), true),
            "node_modules/typescript should be ignored"
        );

        // Should ignore files inside node_modules
        assert!(
            rules.is_ignored(
                Path::new("node_modules/typescript/lib/lib.es2015.proxy.d.ts"),
                false
            ),
            "Files inside node_modules should be ignored"
        );

        // Should ignore deeply nested files
        assert!(
            rules.is_ignored(
                Path::new("node_modules/@types/node/child_process.d.ts"),
                false
            ),
            "Deeply nested files in node_modules should be ignored"
        );

        // Should NOT ignore unrelated files
        assert!(
            !rules.is_ignored(Path::new("src/index.js"), false),
            "src/index.js should NOT be ignored"
        );
        assert!(
            !rules.is_ignored(Path::new("package.json"), false),
            "package.json should NOT be ignored"
        );
    }

    #[test]
    fn test_python_patterns() {
        let temp = TempDir::new().unwrap();
        let ignore_path = temp.path().join(".atomicignore");
        std::fs::write(
            &ignore_path,
            "__pycache__/\n*.py[cod]\n*.so\n.venv/\nvenv/\n.pytest_cache/\n",
        )
        .unwrap();

        let rules = IgnoreRules::load(temp.path());

        assert!(rules.is_ignored(Path::new("__pycache__"), true));
        assert!(rules.is_ignored(Path::new("module.pyc"), false));
        assert!(rules.is_ignored(Path::new("module.pyo"), false));
        assert!(rules.is_ignored(Path::new("module.pyd"), false));
        assert!(rules.is_ignored(Path::new("extension.so"), false));
        assert!(rules.is_ignored(Path::new(".venv"), true));
        assert!(rules.is_ignored(Path::new("venv"), true));
        assert!(!rules.is_ignored(Path::new("module.py"), false));
        assert!(!rules.is_ignored(Path::new("requirements.txt"), false));
    }

    // ------------------------------------------------------------------------
    // Helper Function Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_local_ignore_path() {
        let path = local_ignore_path(Path::new("/repo"));
        assert_eq!(path, Path::new("/repo/.atomicignore"));
    }

    #[test]
    fn test_global_ignore_path() {
        // This may return None if no config dir is available
        let _ = global_ignore_path();
        // Just verify it doesn't panic
    }

    // ------------------------------------------------------------------------
    // Edge Cases
    // ------------------------------------------------------------------------

    #[test]
    fn test_empty_ignore_file() {
        let temp = TempDir::new().unwrap();
        let ignore_path = temp.path().join(".atomicignore");
        std::fs::write(&ignore_path, "").unwrap();

        let rules = IgnoreRules::load(temp.path());

        // Empty file should still count as having local rules
        assert!(rules.has_local_rules());
        assert!(!rules.is_ignored(Path::new("anything.txt"), false));
    }

    #[test]
    fn test_comments_only_ignore_file() {
        let temp = TempDir::new().unwrap();
        let ignore_path = temp.path().join(".atomicignore");
        std::fs::write(&ignore_path, "# Just a comment\n# Another comment\n").unwrap();

        let rules = IgnoreRules::load(temp.path());

        assert!(rules.has_local_rules());
        assert!(!rules.is_ignored(Path::new("anything.txt"), false));
    }

    #[test]
    fn test_ignore_hidden_files() {
        let temp = TempDir::new().unwrap();
        let ignore_path = temp.path().join(".atomicignore");
        std::fs::write(&ignore_path, ".*\n!.gitignore\n").unwrap();

        let rules = IgnoreRules::load(temp.path());

        assert!(rules.is_ignored(Path::new(".hidden"), false));
        assert!(rules.is_ignored(Path::new(".env"), false));
        assert!(!rules.is_ignored(Path::new(".gitignore"), false));
    }

    #[test]
    fn test_complex_negation() {
        let temp = TempDir::new().unwrap();
        let ignore_path = temp.path().join(".atomicignore");
        std::fs::write(
            &ignore_path,
            "logs/\n!logs/important/\nlogs/important/*.log\n!logs/important/keep.log\n",
        )
        .unwrap();

        let rules = IgnoreRules::load(temp.path());

        assert!(rules.is_ignored(Path::new("logs"), true));
        assert!(rules.is_ignored(Path::new("logs/debug.log"), false));
        // Negation patterns restore files
        assert!(!rules.is_ignored(Path::new("logs/important"), true));
        // But nested patterns can re-ignore
        assert!(rules.is_ignored(Path::new("logs/important/error.log"), false));
        // And nested negations can restore again
        assert!(!rules.is_ignored(Path::new("logs/important/keep.log"), false));
    }
}
