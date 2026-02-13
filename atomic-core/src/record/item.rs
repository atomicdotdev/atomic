//! Record item types for tracking file changes
//!
//! This module defines the data structures used during the recording process
//! to track which files have been added, deleted, or modified, and to
//! communicate necessary database updates when a change is applied locally.
//!
//! # Key Types
//!
//! - [`InodeUpdate`]: Describes updates to the inode/tree tables
//! - [`RecordItem`]: Tracks a file being processed during recording
//! - [`FileMetadata`]: Metadata about a file (permissions, type)
//!
//! # Inode Updates
//!
//! When a change is recorded and then applied locally, the tree and inode
//! tables need to be updated to reflect the new state. `InodeUpdate` captures
//! these necessary updates:
//!
//! - **Add**: A new file was added, create inode → position mapping
//! - **Deleted**: A file was removed, remove inode → position mapping
//!
//! # Recording Workflow
//!
//! During recording, each file in the working copy is represented as a
//! `RecordItem` which tracks:
//!
//! - The file's path and inode
//! - Its parent directory's position in the graph
//! - Whether it's new, modified, or deleted
//!
//! ```text
//! Working Copy Scan
//!        │
//!        ▼
//! ┌─────────────────┐
//! │   RecordItem    │
//! │  (per file)     │
//! ├─────────────────┤
//! │ - path          │
//! │ - inode         │
//! │ - parent_pos    │
//! │ - metadata      │
//! └─────────────────┘
//!        │
//!        ▼
//!   Diff & GraphOp Generation
//!        │
//!        ▼
//! ┌─────────────────┐
//! │  InodeUpdate    │
//! │ (for apply)     │
//! └─────────────────┘
//! ```
//!
//! # Example
//!
//! ```rust
//! use atomic_core::record::{InodeUpdate, FileMetadata};
//! use atomic_core::types::{ChangePosition, Inode};
//!
//! // When a new file is added
//! let update = InodeUpdate::Add {
//!     pos: ChangePosition::new(100),
//!     inode: Inode::new(42),
//! };
//!
//! assert!(update.is_add());
//! assert_eq!(update.inode(), Inode::new(42));
//!
//! // When a file is deleted
//! let update = InodeUpdate::Deleted {
//!     inode: Inode::new(42),
//! };
//!
//! assert!(update.is_deleted());
//! ```

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::types::{ChangePosition, Inode, NodeId, Position};

/// Updates to the inode/tree tables when applying a locally-recorded change.
///
/// When a change is recorded locally and then applied, the database tables
/// that map inodes to graph positions need to be updated. This enum describes
/// those updates.
///
/// # Why This Exists
///
/// The recording process generates hunks that describe graph modifications,
/// but it also needs to track metadata about file additions and deletions
/// so that the tree/inode tables can be updated when the change is applied.
///
/// This separation allows:
/// - Recording to focus on generating correct hunks
/// - Application to efficiently update all necessary tables
/// - The same change format to work for remote and local application
///
/// # Example
///
/// ```rust
/// use atomic_core::record::InodeUpdate;
/// use atomic_core::types::{ChangePosition, Inode};
///
/// // Track a file addition
/// let add = InodeUpdate::Add {
///     pos: ChangePosition::new(50),
///     inode: Inode::new(1),
/// };
///
/// // Track a file deletion
/// let del = InodeUpdate::Deleted {
///     inode: Inode::new(2),
/// };
///
/// // Check the update type
/// assert!(add.is_add());
/// assert!(del.is_deleted());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum InodeUpdate {
    /// A new file was added to the repository.
    ///
    /// When applied, this creates a mapping from the inode to the
    /// graph position where the file's content span lives.
    Add {
        /// The position in the change where the file's inode span is.
        ///
        /// This is used to construct the full graph position when the
        /// change is applied (combined with the change's NodeId).
        pos: ChangePosition,

        /// The inode assigned to this new file.
        ///
        /// This is the stable identifier that will be used in the
        /// tree and inode tables.
        inode: Inode,
    },

    /// A file was deleted from the repository.
    ///
    /// When applied, this removes the inode from the tree/inode tables.
    /// The graph vertices are marked as deleted via edge operations.
    Deleted {
        /// The inode of the deleted file.
        inode: Inode,
    },
}

impl InodeUpdate {
    /// Create an Add update for a new file.
    ///
    /// # Arguments
    ///
    /// * `pos` - The position in the change where the inode span is
    /// * `inode` - The inode assigned to the new file
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::InodeUpdate;
    /// use atomic_core::types::{ChangePosition, Inode};
    ///
    /// let update = InodeUpdate::add(ChangePosition::new(100), Inode::new(5));
    /// assert!(update.is_add());
    /// ```
    pub fn add(pos: ChangePosition, inode: Inode) -> Self {
        Self::Add { pos, inode }
    }

    /// Create a Deleted update for a removed file.
    ///
    /// # Arguments
    ///
    /// * `inode` - The inode of the deleted file
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::InodeUpdate;
    /// use atomic_core::types::Inode;
    ///
    /// let update = InodeUpdate::deleted(Inode::new(5));
    /// assert!(update.is_deleted());
    /// ```
    pub fn deleted(inode: Inode) -> Self {
        Self::Deleted { inode }
    }

    /// Check if this is an Add update.
    ///
    /// # Returns
    ///
    /// `true` if this represents a file addition.
    pub fn is_add(&self) -> bool {
        matches!(self, Self::Add { .. })
    }

    /// Check if this is a Deleted update.
    ///
    /// # Returns
    ///
    /// `true` if this represents a file deletion.
    pub fn is_deleted(&self) -> bool {
        matches!(self, Self::Deleted { .. })
    }

    /// Get the inode associated with this update.
    ///
    /// # Returns
    ///
    /// The inode for both Add and Deleted variants.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::InodeUpdate;
    /// use atomic_core::types::{ChangePosition, Inode};
    ///
    /// let add = InodeUpdate::add(ChangePosition::new(0), Inode::new(42));
    /// assert_eq!(add.inode(), Inode::new(42));
    ///
    /// let del = InodeUpdate::deleted(Inode::new(42));
    /// assert_eq!(del.inode(), Inode::new(42));
    /// ```
    pub fn inode(&self) -> Inode {
        match self {
            Self::Add { inode, .. } => *inode,
            Self::Deleted { inode } => *inode,
        }
    }

    /// Get the position for an Add update.
    ///
    /// # Returns
    ///
    /// `Some(pos)` if this is an Add, `None` if Deleted.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::InodeUpdate;
    /// use atomic_core::types::{ChangePosition, Inode};
    ///
    /// let add = InodeUpdate::add(ChangePosition::new(100), Inode::new(1));
    /// assert_eq!(add.position(), Some(ChangePosition::new(100)));
    ///
    /// let del = InodeUpdate::deleted(Inode::new(1));
    /// assert_eq!(del.position(), None);
    /// ```
    pub fn position(&self) -> Option<ChangePosition> {
        match self {
            Self::Add { pos, .. } => Some(*pos),
            Self::Deleted { .. } => None,
        }
    }
}

/// Metadata about a file in the working copy.
///
/// This captures the essential file system metadata needed during recording
/// to determine file type and permissions.
///
/// # Permissions Encoding
///
/// The permissions are stored as a u16 following the Unix permission model:
/// - Bits 0-8: rwx permissions (owner, group, other)
/// - Bit 9: Sticky bit
/// - Bit 10: Set-GID
/// - Bit 11: Set-UID
///
/// For simplicity, we primarily care about:
/// - Is it executable? (any execute bit set)
/// - Is it a directory?
///
/// # Example
///
/// ```rust
/// use atomic_core::record::FileMetadata;
///
/// // A regular file with read/write permissions
/// let regular = FileMetadata::new(0o644, false);
/// assert!(!regular.is_dir());
/// assert!(!regular.is_executable());
///
/// // An executable file
/// let executable = FileMetadata::new(0o755, false);
/// assert!(executable.is_executable());
///
/// // A directory
/// let dir = FileMetadata::new(0o755, true);
/// assert!(dir.is_dir());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileMetadata {
    /// Unix-style permissions (mode bits).
    permissions: u16,

    /// Whether this is a directory.
    is_directory: bool,
}

impl FileMetadata {
    /// Create new file metadata.
    ///
    /// # Arguments
    ///
    /// * `permissions` - Unix permission bits (e.g., 0o644)
    /// * `is_directory` - Whether this is a directory
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::FileMetadata;
    ///
    /// let meta = FileMetadata::new(0o755, false);
    /// assert!(meta.is_executable());
    /// assert!(!meta.is_dir());
    /// ```
    pub fn new(permissions: u16, is_directory: bool) -> Self {
        Self {
            permissions,
            is_directory,
        }
    }

    /// Create metadata for a regular file with default permissions (0o644).
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::FileMetadata;
    ///
    /// let meta = FileMetadata::regular_file();
    /// assert!(!meta.is_dir());
    /// assert!(!meta.is_executable());
    /// ```
    pub fn regular_file() -> Self {
        Self::new(0o644, false)
    }

    /// Create metadata for an executable file (0o755).
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::FileMetadata;
    ///
    /// let meta = FileMetadata::executable();
    /// assert!(meta.is_executable());
    /// assert!(!meta.is_dir());
    /// ```
    pub fn executable() -> Self {
        Self::new(0o755, false)
    }

    /// Create metadata for a directory (0o755).
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::FileMetadata;
    ///
    /// let meta = FileMetadata::directory();
    /// assert!(meta.is_dir());
    /// ```
    pub fn directory() -> Self {
        Self::new(0o755, true)
    }

    /// Get the permission bits.
    ///
    /// # Returns
    ///
    /// The Unix permission bits.
    pub fn permissions(&self) -> u16 {
        self.permissions
    }

    /// Check if this is a directory.
    ///
    /// # Returns
    ///
    /// `true` if this metadata represents a directory.
    pub fn is_dir(&self) -> bool {
        self.is_directory
    }

    /// Check if this file is executable.
    ///
    /// A file is considered executable if any of the execute bits
    /// (owner, group, or other) are set.
    ///
    /// # Returns
    ///
    /// `true` if any execute bit is set.
    pub fn is_executable(&self) -> bool {
        // Execute bits are at positions 0, 3, 6 (other, group, owner)
        (self.permissions & 0o111) != 0
    }

    /// Create metadata that's the same but with executable permission.
    ///
    /// # Returns
    ///
    /// New metadata with the owner execute bit set.
    pub fn with_executable(self) -> Self {
        Self {
            permissions: self.permissions | 0o100, // Set owner execute
            ..self
        }
    }

    /// Create metadata that's the same but without executable permission.
    ///
    /// # Returns
    ///
    /// New metadata with all execute bits cleared.
    pub fn without_executable(self) -> Self {
        Self {
            permissions: self.permissions & !0o111, // Clear all execute bits
            ..self
        }
    }

    /// Encode this metadata as a single byte for compact storage.
    ///
    /// The encoding uses:
    /// - Bit 0: is_directory
    /// - Bit 1: is_executable
    ///
    /// # Returns
    ///
    /// A single byte encoding the essential metadata.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::FileMetadata;
    ///
    /// let meta = FileMetadata::executable();
    /// let encoded = meta.to_byte();
    /// let decoded = FileMetadata::from_byte(encoded);
    /// assert_eq!(meta.is_executable(), decoded.is_executable());
    /// ```
    pub fn to_byte(&self) -> u8 {
        let mut byte = 0u8;
        if self.is_directory {
            byte |= 0b01;
        }
        if self.is_executable() {
            byte |= 0b10;
        }
        byte
    }

    /// Decode metadata from a single byte.
    ///
    /// # Arguments
    ///
    /// * `byte` - The encoded metadata byte
    ///
    /// # Returns
    ///
    /// Decoded file metadata with default permissions based on type.
    pub fn from_byte(byte: u8) -> Self {
        let is_directory = (byte & 0b01) != 0;
        let is_executable = (byte & 0b10) != 0;

        let permissions = if is_directory {
            0o755
        } else if is_executable {
            0o755
        } else {
            0o644
        };

        Self {
            permissions,
            is_directory,
        }
    }
}

impl Default for FileMetadata {
    fn default() -> Self {
        Self::regular_file()
    }
}

/// A file being processed during recording.
///
/// This structure tracks all the information needed to record changes
/// for a single file in the working copy.
///
/// # Lifecycle
///
/// 1. Created when scanning the working copy
/// 2. Used to look up the file's current state in the pristine
/// 3. Compared with working copy contents to generate hunks
/// 4. May produce an `InodeUpdate` if the file was added/deleted
///
/// # Example
///
/// ```rust
/// use atomic_core::record::RecordItem;
/// use atomic_core::types::{Inode, Position, NodeId, ChangePosition};
///
/// // Create a record item for a file
/// let item = RecordItem::new(
///     "src/main.rs".into(),
///     Inode::new(42),
///     Position::new(NodeId::ROOT, ChangePosition::new(0)),
/// );
///
/// assert_eq!(item.path().to_str().unwrap(), "src/main.rs");
/// assert_eq!(item.inode(), Inode::new(42));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordItem {
    /// The path relative to the repository root.
    path: PathBuf,

    /// The basename (file name without directory).
    basename: String,

    /// The file's inode (stable identifier).
    inode: Inode,

    /// The parent directory's inode.
    parent_inode: Inode,

    /// The parent directory's position in the graph.
    ///
    /// This is the graph position of the parent's inode span,
    /// used to establish the file's location in the tree.
    parent_position: Position<NodeId>,

    /// File metadata (permissions, type).
    metadata: FileMetadata,
}

impl RecordItem {
    /// Create a new record item.
    ///
    /// # Arguments
    ///
    /// * `path` - The file path relative to repo root
    /// * `inode` - The file's inode
    /// * `parent_position` - The parent directory's graph position
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::RecordItem;
    /// use atomic_core::types::{Inode, Position, NodeId, ChangePosition};
    ///
    /// let item = RecordItem::new(
    ///     "README.md".into(),
    ///     Inode::new(1),
    ///     Position::ROOT,
    /// );
    /// ```
    pub fn new(path: PathBuf, inode: Inode, parent_position: Position<NodeId>) -> Self {
        let basename = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();

        let parent_inode = Inode::ROOT;

        Self {
            path,
            basename,
            inode,
            parent_inode,
            parent_position,
            metadata: FileMetadata::default(),
        }
    }

    /// Create a record item for the repository root.
    ///
    /// This is a special item representing the root directory.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::RecordItem;
    /// use atomic_core::types::Inode;
    ///
    /// let root = RecordItem::root();
    /// assert!(root.is_root());
    /// assert_eq!(root.inode(), Inode::ROOT);
    /// ```
    pub fn root() -> Self {
        Self {
            path: PathBuf::new(),
            basename: String::new(),
            inode: Inode::ROOT,
            parent_inode: Inode::ROOT,
            parent_position: Position::ROOT,
            metadata: FileMetadata::directory(),
        }
    }

    /// Create a record item with full details.
    ///
    /// # Arguments
    ///
    /// * `path` - The file path
    /// * `inode` - The file's inode
    /// * `parent_inode` - The parent directory's inode
    /// * `parent_position` - The parent's graph position
    /// * `metadata` - File metadata
    pub fn with_details(
        path: PathBuf,
        inode: Inode,
        parent_inode: Inode,
        parent_position: Position<NodeId>,
        metadata: FileMetadata,
    ) -> Self {
        let basename = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();

        Self {
            path,
            basename,
            inode,
            parent_inode,
            parent_position,
            metadata,
        }
    }

    /// Get the file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Get the file basename.
    pub fn basename(&self) -> &str {
        &self.basename
    }

    /// Get the file's inode.
    pub fn inode(&self) -> Inode {
        self.inode
    }

    /// Get the parent directory's inode.
    pub fn parent_inode(&self) -> Inode {
        self.parent_inode
    }

    /// Get the parent directory's graph position.
    pub fn parent_position(&self) -> Position<NodeId> {
        self.parent_position
    }

    /// Get the file metadata.
    pub fn metadata(&self) -> FileMetadata {
        self.metadata
    }

    /// Check if this is the repository root.
    pub fn is_root(&self) -> bool {
        self.inode == Inode::ROOT
    }

    /// Check if this is a directory.
    pub fn is_dir(&self) -> bool {
        self.metadata.is_dir()
    }

    /// Set the metadata for this item.
    pub fn set_metadata(&mut self, metadata: FileMetadata) {
        self.metadata = metadata;
    }

    /// Set the parent inode.
    pub fn set_parent_inode(&mut self, parent: Inode) {
        self.parent_inode = parent;
    }

    /// Get the full path as a string.
    ///
    /// # Returns
    ///
    /// The path as a string, or an empty string if the path is invalid UTF-8.
    pub fn path_string(&self) -> String {
        self.path.to_string_lossy().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // InodeUpdate Tests
    // =========================================================================

    #[test]
    fn test_inode_update_add() {
        let pos = ChangePosition::new(100);
        let inode = Inode::new(42);
        let update = InodeUpdate::add(pos, inode);

        assert!(update.is_add());
        assert!(!update.is_deleted());
        assert_eq!(update.inode(), inode);
        assert_eq!(update.position(), Some(pos));
    }

    #[test]
    fn test_inode_update_deleted() {
        let inode = Inode::new(42);
        let update = InodeUpdate::deleted(inode);

        assert!(!update.is_add());
        assert!(update.is_deleted());
        assert_eq!(update.inode(), inode);
        assert_eq!(update.position(), None);
    }

    #[test]
    fn test_inode_update_equality() {
        let pos = ChangePosition::new(100);
        let inode = Inode::new(42);

        let update1 = InodeUpdate::add(pos, inode);
        let update2 = InodeUpdate::add(pos, inode);
        let update3 = InodeUpdate::add(ChangePosition::new(200), inode);

        assert_eq!(update1, update2);
        assert_ne!(update1, update3);
    }

    #[test]
    fn test_inode_update_hash() {
        use std::collections::HashSet;

        let mut set = HashSet::new();
        set.insert(InodeUpdate::add(ChangePosition::new(1), Inode::new(1)));
        set.insert(InodeUpdate::add(ChangePosition::new(2), Inode::new(2)));
        set.insert(InodeUpdate::deleted(Inode::new(3)));

        assert_eq!(set.len(), 3);
    }

    #[test]
    fn test_inode_update_serialization() {
        let update = InodeUpdate::add(ChangePosition::new(100), Inode::new(42));
        let json = serde_json::to_string(&update).unwrap();
        let deserialized: InodeUpdate = serde_json::from_str(&json).unwrap();
        assert_eq!(update, deserialized);

        let update = InodeUpdate::deleted(Inode::new(42));
        let json = serde_json::to_string(&update).unwrap();
        let deserialized: InodeUpdate = serde_json::from_str(&json).unwrap();
        assert_eq!(update, deserialized);
    }

    #[test]
    fn test_inode_update_debug() {
        let update = InodeUpdate::add(ChangePosition::new(100), Inode::new(42));
        let debug = format!("{:?}", update);
        assert!(debug.contains("Add"));
        assert!(debug.contains("100"));
        assert!(debug.contains("42"));
    }

    // =========================================================================
    // FileMetadata Tests
    // =========================================================================

    #[test]
    fn test_file_metadata_new() {
        let meta = FileMetadata::new(0o644, false);
        assert_eq!(meta.permissions(), 0o644);
        assert!(!meta.is_dir());
        assert!(!meta.is_executable());
    }

    #[test]
    fn test_file_metadata_regular_file() {
        let meta = FileMetadata::regular_file();
        assert_eq!(meta.permissions(), 0o644);
        assert!(!meta.is_dir());
        assert!(!meta.is_executable());
    }

    #[test]
    fn test_file_metadata_executable() {
        let meta = FileMetadata::executable();
        assert_eq!(meta.permissions(), 0o755);
        assert!(!meta.is_dir());
        assert!(meta.is_executable());
    }

    #[test]
    fn test_file_metadata_directory() {
        let meta = FileMetadata::directory();
        assert_eq!(meta.permissions(), 0o755);
        assert!(meta.is_dir());
    }

    #[test]
    fn test_file_metadata_is_executable_owner() {
        let meta = FileMetadata::new(0o700, false);
        assert!(meta.is_executable());
    }

    #[test]
    fn test_file_metadata_is_executable_group() {
        let meta = FileMetadata::new(0o070, false);
        assert!(meta.is_executable());
    }

    #[test]
    fn test_file_metadata_is_executable_other() {
        let meta = FileMetadata::new(0o007, false);
        assert!(meta.is_executable());
    }

    #[test]
    fn test_file_metadata_with_executable() {
        let meta = FileMetadata::regular_file();
        assert!(!meta.is_executable());

        let meta = meta.with_executable();
        assert!(meta.is_executable());
    }

    #[test]
    fn test_file_metadata_without_executable() {
        let meta = FileMetadata::executable();
        assert!(meta.is_executable());

        let meta = meta.without_executable();
        assert!(!meta.is_executable());
    }

    #[test]
    fn test_file_metadata_to_byte() {
        // Regular file
        let meta = FileMetadata::regular_file();
        assert_eq!(meta.to_byte(), 0b00);

        // Executable
        let meta = FileMetadata::executable();
        assert_eq!(meta.to_byte(), 0b10);

        // Directory
        let meta = FileMetadata::directory();
        assert_eq!(meta.to_byte(), 0b11); // directory + executable

        // Non-executable directory
        let meta = FileMetadata::new(0o644, true);
        assert_eq!(meta.to_byte(), 0b01);
    }

    #[test]
    fn test_file_metadata_from_byte() {
        // Regular file
        let meta = FileMetadata::from_byte(0b00);
        assert!(!meta.is_dir());
        assert!(!meta.is_executable());

        // Executable
        let meta = FileMetadata::from_byte(0b10);
        assert!(!meta.is_dir());
        assert!(meta.is_executable());

        // Directory
        let meta = FileMetadata::from_byte(0b01);
        assert!(meta.is_dir());

        // Executable directory
        let meta = FileMetadata::from_byte(0b11);
        assert!(meta.is_dir());
        assert!(meta.is_executable());
    }

    #[test]
    fn test_file_metadata_byte_roundtrip() {
        let metas = [
            FileMetadata::regular_file(),
            FileMetadata::executable(),
            FileMetadata::directory(),
            FileMetadata::new(0o600, false),
            FileMetadata::new(0o700, true),
        ];

        for meta in metas {
            let byte = meta.to_byte();
            let decoded = FileMetadata::from_byte(byte);
            assert_eq!(meta.is_dir(), decoded.is_dir());
            assert_eq!(meta.is_executable(), decoded.is_executable());
        }
    }

    #[test]
    fn test_file_metadata_default() {
        let meta = FileMetadata::default();
        assert_eq!(meta, FileMetadata::regular_file());
    }

    #[test]
    fn test_file_metadata_equality() {
        let meta1 = FileMetadata::new(0o755, true);
        let meta2 = FileMetadata::new(0o755, true);
        let meta3 = FileMetadata::new(0o755, false);

        assert_eq!(meta1, meta2);
        assert_ne!(meta1, meta3);
    }

    #[test]
    fn test_file_metadata_serialization() {
        let meta = FileMetadata::executable();
        let json = serde_json::to_string(&meta).unwrap();
        let deserialized: FileMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(meta, deserialized);
    }

    // =========================================================================
    // RecordItem Tests
    // =========================================================================

    #[test]
    fn test_record_item_new() {
        let path = PathBuf::from("src/main.rs");
        let inode = Inode::new(42);
        let parent_pos = Position::ROOT;

        let item = RecordItem::new(path.clone(), inode, parent_pos);

        assert_eq!(item.path(), path.as_path());
        assert_eq!(item.basename(), "main.rs");
        assert_eq!(item.inode(), inode);
        assert_eq!(item.parent_position(), parent_pos);
        assert!(!item.is_root());
    }

    #[test]
    fn test_record_item_root() {
        let item = RecordItem::root();

        assert!(item.is_root());
        assert_eq!(item.inode(), Inode::ROOT);
        assert_eq!(item.parent_inode(), Inode::ROOT);
        assert!(item.is_dir());
        assert_eq!(item.basename(), "");
    }

    #[test]
    fn test_record_item_with_details() {
        let path = PathBuf::from("docs/README.md");
        let inode = Inode::new(10);
        let parent_inode = Inode::new(5);
        let parent_pos = Position::new(NodeId::new(1), ChangePosition::new(50));
        let metadata = FileMetadata::executable();

        let item = RecordItem::with_details(path.clone(), inode, parent_inode, parent_pos, metadata);

        assert_eq!(item.path(), path.as_path());
        assert_eq!(item.basename(), "README.md");
        assert_eq!(item.inode(), inode);
        assert_eq!(item.parent_inode(), parent_inode);
        assert_eq!(item.parent_position(), parent_pos);
        assert_eq!(item.metadata(), metadata);
    }

    #[test]
    fn test_record_item_path_string() {
        let item = RecordItem::new(
            PathBuf::from("src/lib.rs"),
            Inode::new(1),
            Position::ROOT,
        );
        assert_eq!(item.path_string(), "src/lib.rs");
    }

    #[test]
    fn test_record_item_set_metadata() {
        let mut item = RecordItem::new(
            PathBuf::from("script.sh"),
            Inode::new(1),
            Position::ROOT,
        );

        assert!(!item.metadata().is_executable());

        item.set_metadata(FileMetadata::executable());
        assert!(item.metadata().is_executable());
    }

    #[test]
    fn test_record_item_set_parent_inode() {
        let mut item = RecordItem::new(
            PathBuf::from("file.txt"),
            Inode::new(1),
            Position::ROOT,
        );

        assert_eq!(item.parent_inode(), Inode::ROOT);

        item.set_parent_inode(Inode::new(10));
        assert_eq!(item.parent_inode(), Inode::new(10));
    }

    #[test]
    fn test_record_item_is_dir() {
        let mut item = RecordItem::new(
            PathBuf::from("directory"),
            Inode::new(1),
            Position::ROOT,
        );

        assert!(!item.is_dir());

        item.set_metadata(FileMetadata::directory());
        assert!(item.is_dir());
    }

    #[test]
    fn test_record_item_empty_basename() {
        // Root has empty basename
        let root = RecordItem::root();
        assert_eq!(root.basename(), "");

        // File at root level
        let item = RecordItem::new(
            PathBuf::from("file.txt"),
            Inode::new(1),
            Position::ROOT,
        );
        assert_eq!(item.basename(), "file.txt");
    }

    #[test]
    fn test_record_item_deep_path() {
        let item = RecordItem::new(
            PathBuf::from("a/b/c/d/e/file.txt"),
            Inode::new(1),
            Position::ROOT,
        );
        assert_eq!(item.basename(), "file.txt");
        assert_eq!(item.path_string(), "a/b/c/d/e/file.txt");
    }

    #[test]
    fn test_record_item_equality() {
        let item1 = RecordItem::new(
            PathBuf::from("file.txt"),
            Inode::new(1),
            Position::ROOT,
        );
        let item2 = RecordItem::new(
            PathBuf::from("file.txt"),
            Inode::new(1),
            Position::ROOT,
        );
        let item3 = RecordItem::new(
            PathBuf::from("other.txt"),
            Inode::new(2),
            Position::ROOT,
        );

        assert_eq!(item1, item2);
        assert_ne!(item1, item3);
    }

    #[test]
    fn test_record_item_debug() {
        let item = RecordItem::new(
            PathBuf::from("test.rs"),
            Inode::new(42),
            Position::ROOT,
        );
        let debug = format!("{:?}", item);
        assert!(debug.contains("test.rs"));
        assert!(debug.contains("42"));
    }
}
