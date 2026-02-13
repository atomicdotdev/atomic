//! File conflict types for output operations.
//!
//! This module provides types for tracking conflicts detected during repository
//! output. Conflicts occur when the graph contains ambiguous state that cannot
//! be cleanly serialized to file content.
//!
//! # Overview
//!
//! During output, several types of conflicts can be detected:
//!
//! | Type | Description |
//! |------|-------------|
//! | **Name** | Multiple files with the same name |
//! | **Order** | Ambiguous ordering of content lines |
//! | **Cyclic** | Circular dependency in content graph |
//! | **Zombie** | Deleted content that was modified |
//! | **ZombieFile** | Deleted file that was modified |
//!
//! # Conflict Markers
//!
//! When conflicts are detected, the output includes conflict markers similar
//! to other VCS tools:
//!
//! ```text
//! >>>>>>> 1 [ABCDEF12]
//! Content from first change
//! ======= 1 [GHIJKL34]
//! Content from second change
//! <<<<<<< 1
//! ```
//!
//! # Example
//!
//! ```rust
//! use atomic_core::output::repo::{FileConflict, FileConflictType};
//! use atomic_core::types::Hash;
//!
//! // Create a conflict record
//! let conflict = FileConflict::new("src/main.rs", FileConflictType::Order)
//!     .at_line(42)
//!     .with_id(1);
//!
//! assert_eq!(conflict.path, "src/main.rs");
//! assert!(conflict.is_content_conflict());
//! assert!(!conflict.is_name_conflict());
//! ```

use crate::types::Hash;

// ============================================================================
// FILE CONFLICT TYPE
// ============================================================================

/// The type of conflict detected in a file.
///
/// Different conflict types require different resolution strategies:
///
/// - **Name conflicts** require renaming or deleting duplicate files
/// - **Content conflicts** require editing the file to resolve
/// - **Zombie conflicts** may resolve automatically or require intervention
///
/// # Display
///
/// Each conflict type has a string representation for display:
///
/// ```rust
/// use atomic_core::output::repo::FileConflictType;
///
/// assert_eq!(FileConflictType::Order.to_string(), "order");
/// assert_eq!(FileConflictType::Cyclic.to_string(), "cyclic");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileConflictType {
    /// Multiple files have the same name.
    ///
    /// This occurs when different changes create or rename files to the
    /// same path. Resolution requires choosing one name or renaming files.
    Name,

    /// Ambiguous ordering of content.
    ///
    /// This is the most common conflict type. It occurs when multiple
    /// changes modify the same region of a file and there's no way to
    /// automatically determine the correct order.
    Order,

    /// Circular dependency in content graph.
    ///
    /// This is a rare but serious conflict that indicates a bug in the
    /// change application logic or corrupted data. The graph contains
    /// a cycle that makes it impossible to linearize the content.
    Cyclic,

    /// Deleted content that was modified by another change.
    ///
    /// This occurs when one change deletes content that another concurrent
    /// change modified. The deleted content is shown as "zombie" content.
    Zombie,

    /// File was deleted but content was modified.
    ///
    /// Similar to Zombie, but at the file level. One change deleted the
    /// file while another modified it.
    ZombieFile,
}

impl FileConflictType {
    /// Check if this is a name-related conflict.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::FileConflictType;
    ///
    /// assert!(FileConflictType::Name.is_name_conflict());
    /// assert!(!FileConflictType::Order.is_name_conflict());
    /// ```
    pub fn is_name_conflict(self) -> bool {
        matches!(self, Self::Name)
    }

    /// Check if this is a content-related conflict.
    ///
    /// Content conflicts are those that affect the file's content rather
    /// than its name or existence.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::FileConflictType;
    ///
    /// assert!(FileConflictType::Order.is_content_conflict());
    /// assert!(FileConflictType::Cyclic.is_content_conflict());
    /// assert!(FileConflictType::Zombie.is_content_conflict());
    /// assert!(!FileConflictType::Name.is_content_conflict());
    /// ```
    pub fn is_content_conflict(self) -> bool {
        matches!(self, Self::Order | Self::Cyclic | Self::Zombie)
    }

    /// Check if this is a zombie-related conflict.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::FileConflictType;
    ///
    /// assert!(FileConflictType::Zombie.is_zombie_conflict());
    /// assert!(FileConflictType::ZombieFile.is_zombie_conflict());
    /// assert!(!FileConflictType::Order.is_zombie_conflict());
    /// ```
    pub fn is_zombie_conflict(self) -> bool {
        matches!(self, Self::Zombie | Self::ZombieFile)
    }

    /// Get the conflict marker suffix for this type.
    ///
    /// Returns the text appended to conflict markers for this type.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::FileConflictType;
    ///
    /// assert_eq!(FileConflictType::Zombie.marker_suffix(), Some("[zombie]"));
    /// assert_eq!(FileConflictType::Cyclic.marker_suffix(), Some("[cyclic]"));
    /// assert_eq!(FileConflictType::Order.marker_suffix(), None);
    /// ```
    pub fn marker_suffix(self) -> Option<&'static str> {
        match self {
            Self::Zombie | Self::ZombieFile => Some("[zombie]"),
            Self::Cyclic => Some("[cyclic]"),
            Self::Name | Self::Order => None,
        }
    }
}

impl std::fmt::Display for FileConflictType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Name => write!(f, "name"),
            Self::Order => write!(f, "order"),
            Self::Cyclic => write!(f, "cyclic"),
            Self::Zombie => write!(f, "zombie"),
            Self::ZombieFile => write!(f, "zombie_file"),
        }
    }
}

// ============================================================================
// FILE CONFLICT
// ============================================================================

/// A conflict detected in a file during output.
///
/// This struct records all the information about a conflict needed for
/// display and resolution:
///
/// - Where it occurred (path, line)
/// - What type of conflict it is
/// - Which changes are involved
/// - A unique ID for cross-referencing with markers
///
/// # Example
///
/// ```rust
/// use atomic_core::output::repo::{FileConflict, FileConflictType};
/// use atomic_core::types::Hash;
///
/// let hash1 = Hash::of(b"change 1");
/// let hash2 = Hash::of(b"change 2");
///
/// let conflict = FileConflict::new("src/lib.rs", FileConflictType::Order)
///     .at_line(42)
///     .with_changes(vec![hash1, hash2])
///     .with_id(1);
///
/// assert_eq!(conflict.path, "src/lib.rs");
/// assert_eq!(conflict.line, Some(42));
/// assert_eq!(conflict.changes.len(), 2);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileConflict {
    /// Path to the file containing the conflict.
    pub path: String,

    /// The type of conflict.
    pub conflict_type: FileConflictType,

    /// Hashes of changes involved in the conflict.
    ///
    /// For order conflicts, these are the changes whose content overlaps.
    /// For name conflicts, these are the changes that assigned different names.
    pub changes: Vec<Hash>,

    /// Line number where the conflict starts (1-based).
    ///
    /// This is the line number in the output file where the conflict
    /// marker begins. `None` if line tracking is not available.
    pub line: Option<u32>,

    /// Unique identifier for this conflict.
    ///
    /// This ID appears in conflict markers and allows cross-referencing
    /// between the file content and conflict metadata.
    pub id: Option<u32>,
}

impl FileConflict {
    /// Create a new file conflict.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the file containing the conflict
    /// * `conflict_type` - The type of conflict detected
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::{FileConflict, FileConflictType};
    ///
    /// let conflict = FileConflict::new("src/main.rs", FileConflictType::Order);
    /// assert_eq!(conflict.path, "src/main.rs");
    /// assert!(conflict.changes.is_empty());
    /// ```
    pub fn new(path: impl Into<String>, conflict_type: FileConflictType) -> Self {
        Self {
            path: path.into(),
            conflict_type,
            changes: Vec::new(),
            line: None,
            id: None,
        }
    }

    /// Add a single change hash to this conflict.
    ///
    /// # Arguments
    ///
    /// * `hash` - The hash of a change involved in this conflict
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::{FileConflict, FileConflictType};
    /// use atomic_core::types::Hash;
    ///
    /// let hash = Hash::of(b"test");
    /// let conflict = FileConflict::new("file.rs", FileConflictType::Order)
    ///     .with_change(hash);
    ///
    /// assert_eq!(conflict.changes.len(), 1);
    /// ```
    #[must_use]
    pub fn with_change(mut self, hash: Hash) -> Self {
        self.changes.push(hash);
        self
    }

    /// Set all change hashes for this conflict.
    ///
    /// This replaces any existing changes.
    ///
    /// # Arguments
    ///
    /// * `hashes` - The hashes of changes involved
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::{FileConflict, FileConflictType};
    /// use atomic_core::types::Hash;
    ///
    /// let h1 = Hash::of(b"change 1");
    /// let h2 = Hash::of(b"change 2");
    ///
    /// let conflict = FileConflict::new("file.rs", FileConflictType::Order)
    ///     .with_changes(vec![h1, h2]);
    ///
    /// assert_eq!(conflict.changes.len(), 2);
    /// ```
    #[must_use]
    pub fn with_changes(mut self, hashes: Vec<Hash>) -> Self {
        self.changes = hashes;
        self
    }

    /// Set the line number where the conflict starts.
    ///
    /// # Arguments
    ///
    /// * `line` - The 1-based line number
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::{FileConflict, FileConflictType};
    ///
    /// let conflict = FileConflict::new("file.rs", FileConflictType::Order)
    ///     .at_line(42);
    ///
    /// assert_eq!(conflict.line, Some(42));
    /// ```
    #[must_use]
    pub fn at_line(mut self, line: u32) -> Self {
        self.line = Some(line);
        self
    }

    /// Set the conflict ID.
    ///
    /// # Arguments
    ///
    /// * `id` - The unique identifier for this conflict
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::{FileConflict, FileConflictType};
    ///
    /// let conflict = FileConflict::new("file.rs", FileConflictType::Order)
    ///     .with_id(5);
    ///
    /// assert_eq!(conflict.id, Some(5));
    /// ```
    #[must_use]
    pub fn with_id(mut self, id: u32) -> Self {
        self.id = Some(id);
        self
    }

    /// Check if this is a name conflict.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::{FileConflict, FileConflictType};
    ///
    /// let name = FileConflict::new("file.rs", FileConflictType::Name);
    /// let order = FileConflict::new("file.rs", FileConflictType::Order);
    ///
    /// assert!(name.is_name_conflict());
    /// assert!(!order.is_name_conflict());
    /// ```
    pub fn is_name_conflict(&self) -> bool {
        self.conflict_type.is_name_conflict()
    }

    /// Check if this is a content conflict.
    ///
    /// Content conflicts are Order, Cyclic, or Zombie conflicts.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::{FileConflict, FileConflictType};
    ///
    /// let order = FileConflict::new("file.rs", FileConflictType::Order);
    /// let cyclic = FileConflict::new("file.rs", FileConflictType::Cyclic);
    ///
    /// assert!(order.is_content_conflict());
    /// assert!(cyclic.is_content_conflict());
    /// ```
    pub fn is_content_conflict(&self) -> bool {
        self.conflict_type.is_content_conflict()
    }

    /// Check if this is a zombie conflict.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::{FileConflict, FileConflictType};
    ///
    /// let zombie = FileConflict::new("file.rs", FileConflictType::Zombie);
    /// let order = FileConflict::new("file.rs", FileConflictType::Order);
    ///
    /// assert!(zombie.is_zombie_conflict());
    /// assert!(!order.is_zombie_conflict());
    /// ```
    pub fn is_zombie_conflict(&self) -> bool {
        self.conflict_type.is_zombie_conflict()
    }

    /// Get the number of changes involved in this conflict.
    pub fn change_count(&self) -> usize {
        self.changes.len()
    }
}

impl std::fmt::Display for FileConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {} conflict", self.path, self.conflict_type)?;
        if let Some(line) = self.line {
            write!(f, " at line {}", line)?;
        }
        Ok(())
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------------
    // FileConflictType Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_conflict_type_display() {
        assert_eq!(FileConflictType::Name.to_string(), "name");
        assert_eq!(FileConflictType::Order.to_string(), "order");
        assert_eq!(FileConflictType::Cyclic.to_string(), "cyclic");
        assert_eq!(FileConflictType::Zombie.to_string(), "zombie");
        assert_eq!(FileConflictType::ZombieFile.to_string(), "zombie_file");
    }

    #[test]
    fn test_conflict_type_is_name_conflict() {
        assert!(FileConflictType::Name.is_name_conflict());
        assert!(!FileConflictType::Order.is_name_conflict());
        assert!(!FileConflictType::Cyclic.is_name_conflict());
        assert!(!FileConflictType::Zombie.is_name_conflict());
        assert!(!FileConflictType::ZombieFile.is_name_conflict());
    }

    #[test]
    fn test_conflict_type_is_content_conflict() {
        assert!(!FileConflictType::Name.is_content_conflict());
        assert!(FileConflictType::Order.is_content_conflict());
        assert!(FileConflictType::Cyclic.is_content_conflict());
        assert!(FileConflictType::Zombie.is_content_conflict());
        assert!(!FileConflictType::ZombieFile.is_content_conflict());
    }

    #[test]
    fn test_conflict_type_is_zombie_conflict() {
        assert!(!FileConflictType::Name.is_zombie_conflict());
        assert!(!FileConflictType::Order.is_zombie_conflict());
        assert!(!FileConflictType::Cyclic.is_zombie_conflict());
        assert!(FileConflictType::Zombie.is_zombie_conflict());
        assert!(FileConflictType::ZombieFile.is_zombie_conflict());
    }

    #[test]
    fn test_conflict_type_marker_suffix() {
        assert_eq!(FileConflictType::Name.marker_suffix(), None);
        assert_eq!(FileConflictType::Order.marker_suffix(), None);
        assert_eq!(FileConflictType::Cyclic.marker_suffix(), Some("[cyclic]"));
        assert_eq!(FileConflictType::Zombie.marker_suffix(), Some("[zombie]"));
        assert_eq!(FileConflictType::ZombieFile.marker_suffix(), Some("[zombie]"));
    }

    #[test]
    fn test_conflict_type_equality() {
        assert_eq!(FileConflictType::Order, FileConflictType::Order);
        assert_ne!(FileConflictType::Order, FileConflictType::Cyclic);
    }

    #[test]
    fn test_conflict_type_hash() {
        use std::collections::HashSet;

        let mut set = HashSet::new();
        set.insert(FileConflictType::Order);
        set.insert(FileConflictType::Cyclic);
        set.insert(FileConflictType::Order); // Duplicate

        assert_eq!(set.len(), 2);
    }

    #[test]
    fn test_conflict_type_clone() {
        let original = FileConflictType::Zombie;
        let cloned = original;
        assert_eq!(original, cloned);
    }

    // ------------------------------------------------------------------------
    // FileConflict Constructor Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_file_conflict_new() {
        let conflict = FileConflict::new("src/main.rs", FileConflictType::Order);

        assert_eq!(conflict.path, "src/main.rs");
        assert_eq!(conflict.conflict_type, FileConflictType::Order);
        assert!(conflict.changes.is_empty());
        assert!(conflict.line.is_none());
        assert!(conflict.id.is_none());
    }

    #[test]
    fn test_file_conflict_new_with_string() {
        let path = String::from("test.rs");
        let conflict = FileConflict::new(path, FileConflictType::Name);

        assert_eq!(conflict.path, "test.rs");
    }

    // ------------------------------------------------------------------------
    // FileConflict Builder Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_file_conflict_with_change() {
        let hash = Hash::of(b"test change");
        let conflict = FileConflict::new("file.rs", FileConflictType::Order)
            .with_change(hash);

        assert_eq!(conflict.changes.len(), 1);
        assert_eq!(conflict.changes[0], hash);
    }

    #[test]
    fn test_file_conflict_with_multiple_changes() {
        let hash1 = Hash::of(b"change 1");
        let hash2 = Hash::of(b"change 2");

        let conflict = FileConflict::new("file.rs", FileConflictType::Order)
            .with_change(hash1)
            .with_change(hash2);

        assert_eq!(conflict.changes.len(), 2);
    }

    #[test]
    fn test_file_conflict_with_changes() {
        let hash1 = Hash::of(b"change 1");
        let hash2 = Hash::of(b"change 2");

        let conflict = FileConflict::new("file.rs", FileConflictType::Order)
            .with_changes(vec![hash1, hash2]);

        assert_eq!(conflict.changes.len(), 2);
    }

    #[test]
    fn test_file_conflict_with_changes_replaces() {
        let hash1 = Hash::of(b"change 1");
        let hash2 = Hash::of(b"change 2");

        let conflict = FileConflict::new("file.rs", FileConflictType::Order)
            .with_change(hash1)
            .with_changes(vec![hash2]); // Replaces previous

        assert_eq!(conflict.changes.len(), 1);
        assert_eq!(conflict.changes[0], hash2);
    }

    #[test]
    fn test_file_conflict_at_line() {
        let conflict = FileConflict::new("file.rs", FileConflictType::Order)
            .at_line(42);

        assert_eq!(conflict.line, Some(42));
    }

    #[test]
    fn test_file_conflict_at_line_zero() {
        let conflict = FileConflict::new("file.rs", FileConflictType::Order)
            .at_line(0);

        assert_eq!(conflict.line, Some(0));
    }

    #[test]
    fn test_file_conflict_with_id() {
        let conflict = FileConflict::new("file.rs", FileConflictType::Order)
            .with_id(5);

        assert_eq!(conflict.id, Some(5));
    }

    #[test]
    fn test_file_conflict_builder_chaining() {
        let hash = Hash::of(b"test");

        let conflict = FileConflict::new("src/lib.rs", FileConflictType::Cyclic)
            .with_change(hash)
            .at_line(100)
            .with_id(7);

        assert_eq!(conflict.path, "src/lib.rs");
        assert_eq!(conflict.conflict_type, FileConflictType::Cyclic);
        assert_eq!(conflict.changes.len(), 1);
        assert_eq!(conflict.line, Some(100));
        assert_eq!(conflict.id, Some(7));
    }

    // ------------------------------------------------------------------------
    // FileConflict Query Methods Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_file_conflict_is_name_conflict() {
        let name = FileConflict::new("file.rs", FileConflictType::Name);
        let order = FileConflict::new("file.rs", FileConflictType::Order);

        assert!(name.is_name_conflict());
        assert!(!order.is_name_conflict());
    }

    #[test]
    fn test_file_conflict_is_content_conflict() {
        let order = FileConflict::new("file.rs", FileConflictType::Order);
        let cyclic = FileConflict::new("file.rs", FileConflictType::Cyclic);
        let zombie = FileConflict::new("file.rs", FileConflictType::Zombie);
        let name = FileConflict::new("file.rs", FileConflictType::Name);

        assert!(order.is_content_conflict());
        assert!(cyclic.is_content_conflict());
        assert!(zombie.is_content_conflict());
        assert!(!name.is_content_conflict());
    }

    #[test]
    fn test_file_conflict_is_zombie_conflict() {
        let zombie = FileConflict::new("file.rs", FileConflictType::Zombie);
        let zombie_file = FileConflict::new("file.rs", FileConflictType::ZombieFile);
        let order = FileConflict::new("file.rs", FileConflictType::Order);

        assert!(zombie.is_zombie_conflict());
        assert!(zombie_file.is_zombie_conflict());
        assert!(!order.is_zombie_conflict());
    }

    #[test]
    fn test_file_conflict_change_count() {
        let hash1 = Hash::of(b"1");
        let hash2 = Hash::of(b"2");

        let empty = FileConflict::new("file.rs", FileConflictType::Order);
        assert_eq!(empty.change_count(), 0);

        let with_changes = FileConflict::new("file.rs", FileConflictType::Order)
            .with_changes(vec![hash1, hash2]);
        assert_eq!(with_changes.change_count(), 2);
    }

    // ------------------------------------------------------------------------
    // FileConflict Display Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_file_conflict_display_basic() {
        let conflict = FileConflict::new("src/main.rs", FileConflictType::Order);
        let display = conflict.to_string();

        assert!(display.contains("src/main.rs"));
        assert!(display.contains("order"));
        assert!(display.contains("conflict"));
    }

    #[test]
    fn test_file_conflict_display_with_line() {
        let conflict = FileConflict::new("src/main.rs", FileConflictType::Order)
            .at_line(42);
        let display = conflict.to_string();

        assert!(display.contains("line 42"));
    }

    #[test]
    fn test_file_conflict_display_without_line() {
        let conflict = FileConflict::new("src/main.rs", FileConflictType::Order);
        let display = conflict.to_string();

        assert!(!display.contains("line"));
    }

    // ------------------------------------------------------------------------
    // FileConflict Clone and Equality Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_file_conflict_clone() {
        let hash = Hash::of(b"test");
        let original = FileConflict::new("file.rs", FileConflictType::Order)
            .with_change(hash)
            .at_line(10);

        let cloned = original.clone();

        assert_eq!(cloned.path, original.path);
        assert_eq!(cloned.conflict_type, original.conflict_type);
        assert_eq!(cloned.changes, original.changes);
        assert_eq!(cloned.line, original.line);
    }

    #[test]
    fn test_file_conflict_equality() {
        let hash = Hash::of(b"test");

        let c1 = FileConflict::new("file.rs", FileConflictType::Order)
            .with_change(hash)
            .at_line(10);

        let c2 = FileConflict::new("file.rs", FileConflictType::Order)
            .with_change(hash)
            .at_line(10);

        let c3 = FileConflict::new("file.rs", FileConflictType::Cyclic)
            .with_change(hash)
            .at_line(10);

        assert_eq!(c1, c2);
        assert_ne!(c1, c3);
    }

    #[test]
    fn test_file_conflict_debug() {
        let conflict = FileConflict::new("test.rs", FileConflictType::Order);
        let debug_str = format!("{:?}", conflict);

        assert!(debug_str.contains("FileConflict"));
        assert!(debug_str.contains("test.rs"));
    }
}
