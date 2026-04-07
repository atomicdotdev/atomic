//! Cross-view insert options and outcome types.
//!
//! These types configure and report on operations that insert changes
//! from one view into another (e.g., cherry-picking or view merging).

use atomic_core::types::{Hash, Merkle};

/// Options for inserting changes between views.
///
/// This struct configures how changes are copied from a source view
/// to a target view.
///
/// # Example
///
/// ```rust,ignore
/// // Insert all changes from feature to main
/// let options = CrossViewInsertOptions::new("feature", "main");
///
/// // Insert only up to a specific tag
/// let options = CrossViewInsertOptions::new("feature", "main")
///     .up_to_tag("v1.0.0");
///
/// // Insert specific changes only
/// let options = CrossViewInsertOptions::new("feature", "main")
///     .only_changes(vec![hash1, hash2]);
/// ```
#[derive(Debug, Clone)]
pub struct CrossViewInsertOptions {
    /// Source view to copy changes from.
    pub from_view: String,

    /// Target view to insert changes to.
    pub to_view: String,

    /// Optional tag to limit changes up to (inclusive).
    /// Only changes up to and including this tag's state will be inserted.
    pub up_to_tag: Option<String>,

    /// Optional specific changes to insert (if empty, insert all missing).
    pub only_changes: Vec<Hash>,

    /// Whether to insert dependencies automatically.
    pub apply_dependencies: bool,

    /// Whether to allow conflicts.
    pub allow_conflicts: bool,

    /// Whether to do a dry run (don't actually insert).
    pub dry_run: bool,
}

impl CrossViewInsertOptions {
    /// Create new cross-view insert options.
    ///
    /// # Arguments
    ///
    /// * `from_view` - Source view name
    /// * `to_view` - Target view name
    pub fn new(from_view: impl Into<String>, to_view: impl Into<String>) -> Self {
        Self {
            from_view: from_view.into(),
            to_view: to_view.into(),
            up_to_tag: None,
            only_changes: Vec::new(),
            apply_dependencies: true,
            allow_conflicts: false,
            dry_run: false,
        }
    }

    /// Limit changes to those up to and including a tag.
    pub fn up_to_tag(mut self, tag: impl Into<String>) -> Self {
        self.up_to_tag = Some(tag.into());
        self
    }

    /// Insert only specific changes.
    pub fn only_changes(mut self, changes: Vec<Hash>) -> Self {
        self.only_changes = changes;
        self
    }

    /// Set whether to insert dependencies automatically.
    pub fn with_dependencies(mut self, apply: bool) -> Self {
        self.apply_dependencies = apply;
        self
    }

    /// Set whether to allow conflicts.
    pub fn allow_conflicts(mut self, allow: bool) -> Self {
        self.allow_conflicts = allow;
        self
    }

    /// Set dry run mode.
    pub fn dry_run(mut self, dry: bool) -> Self {
        self.dry_run = dry;
        self
    }
}

/// Result of a cross-view insert operation.
#[derive(Debug, Clone)]
pub struct CrossViewInsertOutcome {
    /// Number of changes inserted.
    pub changes_applied: usize,

    /// Hashes of changes that were inserted.
    pub applied_hashes: Vec<Hash>,

    /// Hashes of changes that were skipped (already in target).
    pub skipped_hashes: Vec<Hash>,

    /// New state of the target view.
    pub new_state: Merkle,

    /// New sequence number of the target view.
    pub sequence: u64,

    /// Whether any conflicts were detected.
    pub has_conflicts: bool,

    /// Was this a dry run?
    pub was_dry_run: bool,
}

impl CrossViewInsertOutcome {
    /// Create a new outcome.
    pub fn new() -> Self {
        Self {
            changes_applied: 0,
            applied_hashes: Vec::new(),
            skipped_hashes: Vec::new(),
            new_state: Merkle::ZERO,
            sequence: 0,
            has_conflicts: false,
            was_dry_run: false,
        }
    }

    /// Check if any changes were inserted.
    pub fn has_applied(&self) -> bool {
        self.changes_applied > 0
    }

    /// Get the total number of changes processed.
    pub fn total_processed(&self) -> usize {
        self.applied_hashes.len() + self.skipped_hashes.len()
    }
}

impl Default for CrossViewInsertOutcome {
    fn default() -> Self {
        Self::new()
    }
}
