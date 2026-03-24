//! Configuration options for CRDT apply operations.
//!
//! This module provides the [`ApplyOptions`] struct which controls how CRDT
//! operations are applied to the pristine database. It uses a builder pattern
//! for ergonomic configuration.
//!
//! # Overview
//!
//! The apply options control various aspects of the apply process:
//!
//! - **Conflict handling**: How to respond to detected conflicts
//! - **Validation**: Level of validation to perform
//! - **Performance**: Trade-offs between safety and speed
//! - **Idempotency**: How to handle already-applied operations
//!
//! # Example
//!
//! ```rust
//! use atomic_core::crdt::apply::options::ApplyOptions;
//!
//! // Default options (safe defaults)
//! let options = ApplyOptions::default();
//! assert!(options.validate_ordering());
//! assert!(!options.allow_duplicate_ids());
//!
//! // Strict options for production
//! let strict = ApplyOptions::strict();
//! assert!(strict.fail_on_conflict());
//!
//! // Lenient options for recovery scenarios
//! let lenient = ApplyOptions::lenient();
//! assert!(lenient.allow_duplicate_ids());
//!
//! // Custom configuration with builder
//! let custom = ApplyOptions::builder()
//!     .validate_ordering(true)
//!     .track_conflicts(true)
//!     .fail_on_conflict(false)
//!     .build();
//! ```
//!
//! # Presets
//!
//! Three presets are available for common use cases:
//!
//! | Preset | Use Case | Validation | Conflicts |
//! |--------|----------|------------|-----------|
//! | `default()` | Normal operation | Full | Track |
//! | `strict()` | Production safety | Full | Fail |
//! | `lenient()` | Recovery/repair | Minimal | Allow |

use serde::{Deserialize, Serialize};
use std::fmt;

// ApplyOptions

/// Configuration options for applying CRDT operations.
///
/// Controls validation, conflict handling, and performance characteristics
/// of the apply process.
///
/// # Default Values
///
/// - `validate_ordering`: `true` - Verify CRDT ordering constraints
/// - `validate_references`: `true` - Verify referenced entities exist
/// - `track_conflicts`: `true` - Record conflicts for later inspection
/// - `fail_on_conflict`: `false` - Continue after conflicts (track them)
/// - `allow_duplicate_ids`: `false` - Reject duplicate CRDT IDs
/// - `update_reverse_indexes`: `true` - Maintain reverse lookup tables
/// - `max_operations`: `None` - No limit on operations
///
/// # Example
///
/// ```rust
/// use atomic_core::crdt::apply::options::ApplyOptions;
///
/// let options = ApplyOptions::builder()
///     .validate_ordering(true)
///     .fail_on_conflict(true)
///     .max_operations(Some(1000))
///     .build();
///
/// assert!(options.validate_ordering());
/// assert!(options.fail_on_conflict());
/// assert_eq!(options.max_operations(), Some(1000));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyOptions {
    /// Whether to validate CRDT ordering constraints.
    ///
    /// When enabled, the apply process verifies that insertions respect
    /// the CRDT ordering invariants (e.g., "after" references are valid).
    validate_ordering: bool,

    /// Whether to validate that referenced entities exist.
    ///
    /// When enabled, the apply process verifies that all referenced
    /// TrunkIds, BranchIds, and LeafIds exist before operating on them.
    validate_references: bool,

    /// Whether to track detected conflicts.
    ///
    /// When enabled, conflicts are recorded in the [`ApplyContext`] for
    /// later inspection. This allows the caller to decide how to handle them.
    ///
    /// [`ApplyContext`]: super::context::ApplyContext
    track_conflicts: bool,

    /// Whether to fail immediately on conflict detection.
    ///
    /// When enabled, the first detected conflict causes the apply to fail.
    /// When disabled, conflicts are tracked (if `track_conflicts` is true)
    /// and the apply continues.
    fail_on_conflict: bool,

    /// Whether to allow duplicate CRDT IDs.
    ///
    /// When enabled, operations with IDs that already exist are silently
    /// skipped (idempotent behavior). When disabled, duplicate IDs cause
    /// an error.
    ///
    /// This is useful for recovery scenarios where changes might be
    /// partially applied.
    allow_duplicate_ids: bool,

    /// Whether to update reverse index tables.
    ///
    /// When enabled, reverse lookup tables (PATH_TRUNK, INODE_TRUNK) are
    /// updated during apply. Disabling this can improve performance but
    /// requires a separate index rebuild.
    update_reverse_indexes: bool,

    /// Maximum number of operations to apply.
    ///
    /// When set, the apply process stops after this many operations.
    /// This can be used for batched processing or to limit resource usage.
    max_operations: Option<usize>,

    /// Whether to verify content ranges are valid.
    ///
    /// When enabled, content byte ranges are verified against the actual
    /// content blob length.
    validate_content_ranges: bool,
}

impl Default for ApplyOptions {
    /// Creates options with safe defaults for normal operation.
    fn default() -> Self {
        Self {
            validate_ordering: true,
            validate_references: true,
            track_conflicts: true,
            fail_on_conflict: false,
            allow_duplicate_ids: false,
            update_reverse_indexes: true,
            max_operations: None,
            validate_content_ranges: true,
        }
    }
}

impl ApplyOptions {
    // Constructors and Presets

    /// Creates a new builder for configuring options.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::crdt::apply::options::ApplyOptions;
    ///
    /// let options = ApplyOptions::builder()
    ///     .validate_ordering(true)
    ///     .fail_on_conflict(true)
    ///     .build();
    /// ```
    #[inline]
    pub fn builder() -> ApplyOptionsBuilder {
        ApplyOptionsBuilder::new()
    }

    /// Creates strict options for production use.
    ///
    /// Strict options enable all validation and fail on any conflict.
    /// This is the safest configuration.
    ///
    /// # Configuration
    ///
    /// - All validation enabled
    /// - Fail on conflict: `true`
    /// - Allow duplicate IDs: `false`
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::crdt::apply::options::ApplyOptions;
    ///
    /// let options = ApplyOptions::strict();
    /// assert!(options.fail_on_conflict());
    /// assert!(!options.allow_duplicate_ids());
    /// ```
    pub fn strict() -> Self {
        Self {
            validate_ordering: true,
            validate_references: true,
            track_conflicts: true,
            fail_on_conflict: true,
            allow_duplicate_ids: false,
            update_reverse_indexes: true,
            max_operations: None,
            validate_content_ranges: true,
        }
    }

    /// Creates lenient options for recovery scenarios.
    ///
    /// Lenient options minimize validation and allow operations that
    /// might otherwise fail. Use this for repairing corrupted repositories.
    ///
    /// # Configuration
    ///
    /// - Minimal validation
    /// - Fail on conflict: `false`
    /// - Allow duplicate IDs: `true`
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::crdt::apply::options::ApplyOptions;
    ///
    /// let options = ApplyOptions::lenient();
    /// assert!(!options.fail_on_conflict());
    /// assert!(options.allow_duplicate_ids());
    /// ```
    pub fn lenient() -> Self {
        Self {
            validate_ordering: false,
            validate_references: false,
            track_conflicts: true,
            fail_on_conflict: false,
            allow_duplicate_ids: true,
            update_reverse_indexes: true,
            max_operations: None,
            validate_content_ranges: false,
        }
    }

    /// Creates options optimized for performance.
    ///
    /// These options disable some validation for faster apply at the
    /// cost of reduced safety. Use only when applying trusted changes.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::crdt::apply::options::ApplyOptions;
    ///
    /// let options = ApplyOptions::fast();
    /// assert!(!options.validate_ordering());
    /// ```
    pub fn fast() -> Self {
        Self {
            validate_ordering: false,
            validate_references: true, // Still check references for safety
            track_conflicts: false,
            fail_on_conflict: false,
            allow_duplicate_ids: false,
            update_reverse_indexes: true,
            max_operations: None,
            validate_content_ranges: false,
        }
    }

    // Accessors

    /// Returns whether ordering validation is enabled.
    #[inline]
    pub fn validate_ordering(&self) -> bool {
        self.validate_ordering
    }

    /// Returns whether reference validation is enabled.
    #[inline]
    pub fn validate_references(&self) -> bool {
        self.validate_references
    }

    /// Returns whether conflict tracking is enabled.
    #[inline]
    pub fn track_conflicts(&self) -> bool {
        self.track_conflicts
    }

    /// Returns whether to fail on conflict.
    #[inline]
    pub fn fail_on_conflict(&self) -> bool {
        self.fail_on_conflict
    }

    /// Returns whether duplicate IDs are allowed.
    #[inline]
    pub fn allow_duplicate_ids(&self) -> bool {
        self.allow_duplicate_ids
    }

    /// Returns whether reverse indexes should be updated.
    #[inline]
    pub fn update_reverse_indexes(&self) -> bool {
        self.update_reverse_indexes
    }

    /// Returns the maximum number of operations, if set.
    #[inline]
    pub fn max_operations(&self) -> Option<usize> {
        self.max_operations
    }

    /// Returns whether content range validation is enabled.
    #[inline]
    pub fn validate_content_ranges(&self) -> bool {
        self.validate_content_ranges
    }

    // Utility Methods

    /// Returns `true` if any validation is enabled.
    #[inline]
    pub fn has_validation(&self) -> bool {
        self.validate_ordering || self.validate_references || self.validate_content_ranges
    }

    /// Returns `true` if this configuration is "strict" (production-safe).
    #[inline]
    pub fn is_strict(&self) -> bool {
        self.validate_ordering
            && self.validate_references
            && self.fail_on_conflict
            && !self.allow_duplicate_ids
            && self.validate_content_ranges
    }

    /// Returns `true` if this configuration is "lenient" (recovery mode).
    #[inline]
    pub fn is_lenient(&self) -> bool {
        !self.validate_ordering
            && !self.validate_references
            && self.allow_duplicate_ids
            && !self.validate_content_ranges
    }

    /// Checks if the given operation count exceeds the limit.
    ///
    /// Returns `true` if there is a limit and it has been exceeded.
    #[inline]
    pub fn exceeds_limit(&self, count: usize) -> bool {
        self.max_operations.is_some_and(|max| count >= max)
    }
}

impl fmt::Display for ApplyOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mode = if self.is_strict() {
            "strict"
        } else if self.is_lenient() {
            "lenient"
        } else {
            "custom"
        };

        write!(f, "ApplyOptions({})", mode)?;

        if let Some(max) = self.max_operations {
            write!(f, " [max: {}]", max)?;
        }

        Ok(())
    }
}

// ApplyOptionsBuilder

/// Builder for [`ApplyOptions`].
///
/// Provides a fluent interface for configuring apply options.
///
/// # Example
///
/// ```rust
/// use atomic_core::crdt::apply::options::ApplyOptionsBuilder;
///
/// let options = ApplyOptionsBuilder::new()
///     .validate_ordering(true)
///     .track_conflicts(true)
///     .fail_on_conflict(false)
///     .max_operations(Some(500))
///     .build();
///
/// assert!(options.validate_ordering());
/// assert!(!options.fail_on_conflict());
/// assert_eq!(options.max_operations(), Some(500));
/// ```
#[derive(Debug, Clone)]
pub struct ApplyOptionsBuilder {
    options: ApplyOptions,
}

impl Default for ApplyOptionsBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ApplyOptionsBuilder {
    /// Creates a new builder with default options.
    #[inline]
    pub fn new() -> Self {
        Self {
            options: ApplyOptions::default(),
        }
    }

    /// Creates a builder starting from strict options.
    #[inline]
    pub fn from_strict() -> Self {
        Self {
            options: ApplyOptions::strict(),
        }
    }

    /// Creates a builder starting from lenient options.
    #[inline]
    pub fn from_lenient() -> Self {
        Self {
            options: ApplyOptions::lenient(),
        }
    }

    /// Sets whether to validate CRDT ordering constraints.
    #[inline]
    pub fn validate_ordering(mut self, validate: bool) -> Self {
        self.options.validate_ordering = validate;
        self
    }

    /// Sets whether to validate that referenced entities exist.
    #[inline]
    pub fn validate_references(mut self, validate: bool) -> Self {
        self.options.validate_references = validate;
        self
    }

    /// Sets whether to track detected conflicts.
    #[inline]
    pub fn track_conflicts(mut self, track: bool) -> Self {
        self.options.track_conflicts = track;
        self
    }

    /// Sets whether to fail immediately on conflict detection.
    #[inline]
    pub fn fail_on_conflict(mut self, fail: bool) -> Self {
        self.options.fail_on_conflict = fail;
        self
    }

    /// Sets whether to allow duplicate CRDT IDs.
    #[inline]
    pub fn allow_duplicate_ids(mut self, allow: bool) -> Self {
        self.options.allow_duplicate_ids = allow;
        self
    }

    /// Sets whether to update reverse index tables.
    #[inline]
    pub fn update_reverse_indexes(mut self, update: bool) -> Self {
        self.options.update_reverse_indexes = update;
        self
    }

    /// Sets the maximum number of operations to apply.
    #[inline]
    pub fn max_operations(mut self, max: Option<usize>) -> Self {
        self.options.max_operations = max;
        self
    }

    /// Sets whether to validate content ranges.
    #[inline]
    pub fn validate_content_ranges(mut self, validate: bool) -> Self {
        self.options.validate_content_ranges = validate;
        self
    }

    /// Enables all validation options.
    #[inline]
    pub fn with_full_validation(mut self) -> Self {
        self.options.validate_ordering = true;
        self.options.validate_references = true;
        self.options.validate_content_ranges = true;
        self
    }

    /// Disables all validation options.
    #[inline]
    pub fn without_validation(mut self) -> Self {
        self.options.validate_ordering = false;
        self.options.validate_references = false;
        self.options.validate_content_ranges = false;
        self
    }

    /// Builds the configured [`ApplyOptions`].
    #[inline]
    pub fn build(self) -> ApplyOptions {
        self.options
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    // Default Tests

    #[test]
    fn test_default_options() {
        let options = ApplyOptions::default();

        assert!(options.validate_ordering());
        assert!(options.validate_references());
        assert!(options.track_conflicts());
        assert!(!options.fail_on_conflict());
        assert!(!options.allow_duplicate_ids());
        assert!(options.update_reverse_indexes());
        assert!(options.max_operations().is_none());
        assert!(options.validate_content_ranges());
    }

    #[test]
    fn test_default_has_validation() {
        let options = ApplyOptions::default();
        assert!(options.has_validation());
    }

    #[test]
    fn test_default_is_not_strict_or_lenient() {
        let options = ApplyOptions::default();
        // Default is not strict because fail_on_conflict is false
        assert!(!options.is_strict());
        assert!(!options.is_lenient());
    }

    // Preset Tests

    #[test]
    fn test_strict_preset() {
        let options = ApplyOptions::strict();

        assert!(options.validate_ordering());
        assert!(options.validate_references());
        assert!(options.track_conflicts());
        assert!(options.fail_on_conflict());
        assert!(!options.allow_duplicate_ids());
        assert!(options.update_reverse_indexes());
        assert!(options.validate_content_ranges());
        assert!(options.is_strict());
        assert!(!options.is_lenient());
    }

    #[test]
    fn test_lenient_preset() {
        let options = ApplyOptions::lenient();

        assert!(!options.validate_ordering());
        assert!(!options.validate_references());
        assert!(options.track_conflicts());
        assert!(!options.fail_on_conflict());
        assert!(options.allow_duplicate_ids());
        assert!(options.update_reverse_indexes());
        assert!(!options.validate_content_ranges());
        assert!(!options.is_strict());
        assert!(options.is_lenient());
    }

    #[test]
    fn test_fast_preset() {
        let options = ApplyOptions::fast();

        assert!(!options.validate_ordering());
        assert!(options.validate_references()); // Still checks references
        assert!(!options.track_conflicts());
        assert!(!options.fail_on_conflict());
        assert!(!options.allow_duplicate_ids());
        assert!(options.update_reverse_indexes());
        assert!(!options.validate_content_ranges());
    }

    #[test]
    fn test_fast_has_some_validation() {
        let options = ApplyOptions::fast();
        // Fast still validates references
        assert!(options.has_validation());
    }

    // Builder Tests

    #[test]
    fn test_builder_default() {
        let options = ApplyOptions::builder().build();
        assert_eq!(options, ApplyOptions::default());
    }

    #[test]
    fn test_builder_from_strict() {
        let options = ApplyOptionsBuilder::from_strict().build();
        assert_eq!(options, ApplyOptions::strict());
    }

    #[test]
    fn test_builder_from_lenient() {
        let options = ApplyOptionsBuilder::from_lenient().build();
        assert_eq!(options, ApplyOptions::lenient());
    }

    #[test]
    fn test_builder_validate_ordering() {
        let options = ApplyOptions::builder().validate_ordering(false).build();
        assert!(!options.validate_ordering());
    }

    #[test]
    fn test_builder_validate_references() {
        let options = ApplyOptions::builder().validate_references(false).build();
        assert!(!options.validate_references());
    }

    #[test]
    fn test_builder_track_conflicts() {
        let options = ApplyOptions::builder().track_conflicts(false).build();
        assert!(!options.track_conflicts());
    }

    #[test]
    fn test_builder_fail_on_conflict() {
        let options = ApplyOptions::builder().fail_on_conflict(true).build();
        assert!(options.fail_on_conflict());
    }

    #[test]
    fn test_builder_allow_duplicate_ids() {
        let options = ApplyOptions::builder().allow_duplicate_ids(true).build();
        assert!(options.allow_duplicate_ids());
    }

    #[test]
    fn test_builder_update_reverse_indexes() {
        let options = ApplyOptions::builder()
            .update_reverse_indexes(false)
            .build();
        assert!(!options.update_reverse_indexes());
    }

    #[test]
    fn test_builder_max_operations() {
        let options = ApplyOptions::builder().max_operations(Some(100)).build();
        assert_eq!(options.max_operations(), Some(100));
    }

    #[test]
    fn test_builder_max_operations_none() {
        let options = ApplyOptions::builder().max_operations(None).build();
        assert!(options.max_operations().is_none());
    }

    #[test]
    fn test_builder_validate_content_ranges() {
        let options = ApplyOptions::builder()
            .validate_content_ranges(false)
            .build();
        assert!(!options.validate_content_ranges());
    }

    #[test]
    fn test_builder_with_full_validation() {
        let options = ApplyOptionsBuilder::from_lenient()
            .with_full_validation()
            .build();
        assert!(options.validate_ordering());
        assert!(options.validate_references());
        assert!(options.validate_content_ranges());
    }

    #[test]
    fn test_builder_without_validation() {
        let options = ApplyOptionsBuilder::from_strict()
            .without_validation()
            .build();
        assert!(!options.validate_ordering());
        assert!(!options.validate_references());
        assert!(!options.validate_content_ranges());
    }

    #[test]
    fn test_builder_chaining() {
        let options = ApplyOptions::builder()
            .validate_ordering(true)
            .validate_references(true)
            .track_conflicts(true)
            .fail_on_conflict(true)
            .allow_duplicate_ids(false)
            .update_reverse_indexes(true)
            .max_operations(Some(500))
            .validate_content_ranges(true)
            .build();

        assert!(options.validate_ordering());
        assert!(options.validate_references());
        assert!(options.track_conflicts());
        assert!(options.fail_on_conflict());
        assert!(!options.allow_duplicate_ids());
        assert!(options.update_reverse_indexes());
        assert_eq!(options.max_operations(), Some(500));
        assert!(options.validate_content_ranges());
    }

    // Utility Method Tests

    #[test]
    fn test_has_validation_all_disabled() {
        let options = ApplyOptions::builder()
            .validate_ordering(false)
            .validate_references(false)
            .validate_content_ranges(false)
            .build();
        assert!(!options.has_validation());
    }

    #[test]
    fn test_has_validation_ordering_only() {
        let options = ApplyOptions::builder()
            .validate_ordering(true)
            .validate_references(false)
            .validate_content_ranges(false)
            .build();
        assert!(options.has_validation());
    }

    #[test]
    fn test_has_validation_references_only() {
        let options = ApplyOptions::builder()
            .validate_ordering(false)
            .validate_references(true)
            .validate_content_ranges(false)
            .build();
        assert!(options.has_validation());
    }

    #[test]
    fn test_has_validation_content_only() {
        let options = ApplyOptions::builder()
            .validate_ordering(false)
            .validate_references(false)
            .validate_content_ranges(true)
            .build();
        assert!(options.has_validation());
    }

    #[test]
    fn test_exceeds_limit_no_limit() {
        let options = ApplyOptions::builder().max_operations(None).build();
        assert!(!options.exceeds_limit(0));
        assert!(!options.exceeds_limit(1000));
        assert!(!options.exceeds_limit(usize::MAX));
    }

    #[test]
    fn test_exceeds_limit_with_limit() {
        let options = ApplyOptions::builder().max_operations(Some(100)).build();
        assert!(!options.exceeds_limit(0));
        assert!(!options.exceeds_limit(99));
        assert!(options.exceeds_limit(100));
        assert!(options.exceeds_limit(101));
    }

    #[test]
    fn test_exceeds_limit_zero() {
        let options = ApplyOptions::builder().max_operations(Some(0)).build();
        assert!(options.exceeds_limit(0));
    }

    // Display Tests

    #[test]
    fn test_display_strict() {
        let options = ApplyOptions::strict();
        let display = options.to_string();
        assert!(display.contains("strict"));
    }

    #[test]
    fn test_display_lenient() {
        let options = ApplyOptions::lenient();
        let display = options.to_string();
        assert!(display.contains("lenient"));
    }

    #[test]
    fn test_display_custom() {
        let options = ApplyOptions::default();
        let display = options.to_string();
        assert!(display.contains("custom"));
    }

    #[test]
    fn test_display_with_max() {
        let options = ApplyOptions::builder().max_operations(Some(500)).build();
        let display = options.to_string();
        assert!(display.contains("500"));
    }

    // Serialization Tests

    #[test]
    fn test_serde_roundtrip() {
        let options = ApplyOptions::builder()
            .validate_ordering(true)
            .fail_on_conflict(true)
            .max_operations(Some(42))
            .build();

        let json = serde_json::to_string(&options).unwrap();
        let restored: ApplyOptions = serde_json::from_str(&json).unwrap();

        assert_eq!(options, restored);
    }

    #[test]
    fn test_serde_strict() {
        let options = ApplyOptions::strict();
        let json = serde_json::to_string(&options).unwrap();
        let restored: ApplyOptions = serde_json::from_str(&json).unwrap();
        assert_eq!(options, restored);
    }

    #[test]
    fn test_serde_lenient() {
        let options = ApplyOptions::lenient();
        let json = serde_json::to_string(&options).unwrap();
        let restored: ApplyOptions = serde_json::from_str(&json).unwrap();
        assert_eq!(options, restored);
    }

    // Clone and Debug Tests

    #[test]
    fn test_clone() {
        let options = ApplyOptions::builder().max_operations(Some(100)).build();
        let cloned = options.clone();
        assert_eq!(options, cloned);
    }

    #[test]
    fn test_debug_format() {
        let options = ApplyOptions::default();
        let debug = format!("{:?}", options);
        assert!(debug.contains("ApplyOptions"));
        assert!(debug.contains("validate_ordering"));
    }

    #[test]
    fn test_builder_clone() {
        let builder = ApplyOptions::builder()
            .validate_ordering(true)
            .max_operations(Some(50));
        let cloned = builder.clone();
        let options1 = builder.build();
        let options2 = cloned.build();
        assert_eq!(options1, options2);
    }

    #[test]
    fn test_builder_default_trait() {
        let builder1 = ApplyOptionsBuilder::default();
        let builder2 = ApplyOptionsBuilder::new();
        assert_eq!(builder1.build(), builder2.build());
    }
}
