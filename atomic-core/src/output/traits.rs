//! Working copy abstraction traits
//!
//! This module defines the core traits for interacting with the working copy
//! (the actual files on disk or in memory). These traits abstract away the
//! file system, enabling:
//!
//! - **Testing**: Use in-memory implementations for fast, isolated tests
//! - **Flexibility**: Support different backends (filesystem, memory, remote)
//! - **Dry-runs**: Use a "sink" implementation that discards output
//!
//! # Trait Hierarchy
//!
//! ```text
//! WorkingCopyRead           VertexBuffer
//!        │                       │
//!        │ (extends)             │ (implements)
//!        ▼                       ▼
//!   WorkingCopy            ConflictWriter
//!        │                   Writer
//!        │ (implements)
//!        ▼
//!   FileSystem
//!   Memory
//!   Sink
//! ```
//!
//! # Design Philosophy
//!
//! 1. **Read/Write Separation**: `WorkingCopyRead` handles read operations,
//!    `WorkingCopy` adds write operations. This allows read-only operations
//!    to work with immutable references.
//!
//! 2. **Error Abstraction**: Each implementation defines its own error type,
//!    enabling appropriate error handling for each backend.
//!
//! 3. **Minimal Interface**: Only the operations needed for VCS functionality
//!    are exposed, not full filesystem semantics.
//!
//! # Example: Implementing WorkingCopy
//!
//! ```rust,ignore
//! use atomic_core::output::{WorkingCopy, WorkingCopyRead, FileMetadata};
//! use atomic_core::types::Inode;
//!
//! struct MyWorkingCopy {
//!     root: std::path::PathBuf,
//! }
//!
//! impl WorkingCopyRead for MyWorkingCopy {
//!     type Error = std::io::Error;
//!
//!     fn file_metadata(&self, path: &str) -> Result<FileMetadata, Self::Error> {
//!         // Implementation...
//!     }
//!
//!     fn read_file(&self, path: &str, buffer: &mut Vec<u8>) -> Result<(), Self::Error> {
//!         // Implementation...
//!     }
//!
//!     fn modified_time(&self, path: &str) -> Result<std::time::SystemTime, Self::Error> {
//!         // Implementation...
//!     }
//! }
//!
//! impl WorkingCopy for MyWorkingCopy {
//!     type Writer = std::fs::File;
//!
//!     fn create_dir_all(&self, path: &str) -> Result<(), Self::Error> {
//!         std::fs::create_dir_all(self.root.join(path))
//!     }
//!
//!     // ... other methods
//! }
//! ```

use crate::types::{Base32, GraphNode, Hash, Inode, NodeId};
use std::io::Write;
use std::time::SystemTime;

// ============================================================================
// FILE METADATA
// ============================================================================

/// File permission and type metadata.
///
/// This struct captures the essential metadata about a file needed for
/// version control operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct FileMetadata {
    /// Unix-style permissions (e.g., 0o644 for regular files, 0o755 for executables)
    pub permissions: u16,
    /// Whether this is a directory
    pub is_dir: bool,
    /// Whether this is a symbolic link
    pub is_symlink: bool,
}

impl FileMetadata {
    /// Create metadata for a regular file with default permissions.
    ///
    /// Default permissions are 0o644 (readable by all, writable by owner).
    pub fn file() -> Self {
        Self {
            permissions: 0o644,
            is_dir: false,
            is_symlink: false,
        }
    }

    /// Create metadata for an executable file.
    ///
    /// Permissions are 0o755 (readable and executable by all, writable by owner).
    pub fn executable() -> Self {
        Self {
            permissions: 0o755,
            is_dir: false,
            is_symlink: false,
        }
    }

    /// Create metadata for a directory.
    ///
    /// Permissions are 0o755 (readable and executable by all, writable by owner).
    pub fn directory() -> Self {
        Self {
            permissions: 0o755,
            is_dir: true,
            is_symlink: false,
        }
    }

    /// Create metadata for a symbolic link.
    pub fn symlink() -> Self {
        Self {
            permissions: 0o777, // Symlinks typically have no meaningful permissions
            is_dir: false,
            is_symlink: true,
        }
    }

    /// Create metadata with custom permissions.
    pub fn with_permissions(mut self, permissions: u16) -> Self {
        self.permissions = permissions;
        self
    }

    /// Check if this represents an executable file.
    pub fn is_executable(&self) -> bool {
        !self.is_dir && !self.is_symlink && (self.permissions & 0o111) != 0
    }

    /// Encode metadata to bytes for storage.
    ///
    /// Format: [permissions_lo, permissions_hi, flags]
    pub fn to_bytes(&self) -> [u8; 3] {
        let flags = if self.is_dir {
            0x01
        } else if self.is_symlink {
            0x02
        } else {
            0x00
        };
        [
            (self.permissions & 0xFF) as u8,
            ((self.permissions >> 8) & 0xFF) as u8,
            flags,
        ]
    }

    /// Decode metadata from bytes.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 3 {
            return None;
        }
        let permissions = u16::from(bytes[0]) | (u16::from(bytes[1]) << 8);
        let (is_dir, is_symlink) = match bytes[2] {
            0x01 => (true, false),
            0x02 => (false, true),
            _ => (false, false),
        };
        Some(Self {
            permissions,
            is_dir,
            is_symlink,
        })
    }
}

// ============================================================================
// WORKING COPY READ TRAIT
// ============================================================================

/// Read-only operations on the working copy.
///
/// This trait provides methods for reading files and metadata from the
/// working copy. It's separate from `WorkingCopy` to allow read-only
/// access without requiring mutable references.
///
/// # Error Handling
///
/// Implementations define their own error type, which must implement
/// `std::error::Error + Send`. This allows for appropriate error handling
/// based on the backend (e.g., `std::io::Error` for filesystem).
pub trait WorkingCopyRead {
    /// The error type for this working copy implementation.
    type Error: std::error::Error + Send + 'static;

    /// Get metadata for a file or directory.
    ///
    /// # Arguments
    ///
    /// * `path` - Relative path within the working copy
    ///
    /// # Returns
    ///
    /// The file's metadata, or an error if the file doesn't exist or
    /// cannot be accessed.
    fn file_metadata(&self, path: &str) -> Result<FileMetadata, Self::Error>;

    /// Read the contents of a file into a buffer.
    ///
    /// # Arguments
    ///
    /// * `path` - Relative path within the working copy
    /// * `buffer` - Buffer to append file contents to
    ///
    /// # Returns
    ///
    /// `Ok(())` on success, or an error if the file cannot be read.
    ///
    /// # Note
    ///
    /// The buffer is appended to, not replaced. Clear it first if you
    /// want only this file's contents.
    fn read_file(&self, path: &str, buffer: &mut Vec<u8>) -> Result<(), Self::Error>;

    /// Get the last modification time of a file.
    ///
    /// # Arguments
    ///
    /// * `path` - Relative path within the working copy
    ///
    /// # Returns
    ///
    /// The file's modification time, or an error if it cannot be determined.
    fn modified_time(&self, path: &str) -> Result<SystemTime, Self::Error>;

    /// Check if a path exists in the working copy.
    ///
    /// # Arguments
    ///
    /// * `path` - Relative path within the working copy
    ///
    /// # Returns
    ///
    /// `true` if the path exists, `false` otherwise.
    fn exists(&self, path: &str) -> bool {
        self.file_metadata(path).is_ok()
    }

    /// Check if a path is a directory.
    ///
    /// # Arguments
    ///
    /// * `path` - Relative path within the working copy
    ///
    /// # Returns
    ///
    /// `true` if the path exists and is a directory, `false` otherwise.
    fn is_directory(&self, path: &str) -> bool {
        self.file_metadata(path).map(|m| m.is_dir).unwrap_or(false)
    }

    /// Walk the directory tree and return all file paths.
    ///
    /// Returns all files (not directories) under the given path prefix.
    /// The `.atomic` directory and its contents are excluded.
    ///
    /// # Arguments
    ///
    /// * `prefix` - Path prefix to start from (empty string for root)
    ///
    /// # Returns
    ///
    /// A vector of file paths relative to the working copy root,
    /// sorted alphabetically. Returns an empty vector if the path
    /// doesn't exist or walking is not supported.
    ///
    /// # Default Implementation
    ///
    /// The default implementation returns an empty vector. Implementations
    /// that support directory walking should override this method.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use atomic_core::output::WorkingCopyRead;
    ///
    /// let files = working_copy.walk_files("")?;
    /// for path in files {
    ///     println!("Found: {}", path);
    /// }
    /// ```
    fn walk_files(&self, _prefix: &str) -> Result<Vec<String>, Self::Error>;
}

// ============================================================================
// WORKING COPY TRAIT
// ============================================================================

/// Full read-write operations on the working copy.
///
/// This trait extends `WorkingCopyRead` with methods for modifying the
/// working copy: creating files, deleting files, renaming, etc.
///
/// # Thread Safety
///
/// Working copy operations are typically not thread-safe. Use appropriate
/// synchronization if accessing from multiple threads.
pub trait WorkingCopy: WorkingCopyRead {
    /// The writer type returned by `write_file`.
    type Writer: Write;

    /// Check if a path is writable.
    ///
    /// This can be used to skip files that should not be modified,
    /// such as those with special attributes or user-specified exclusions.
    ///
    /// # Arguments
    ///
    /// * `path` - Relative path within the working copy
    ///
    /// # Returns
    ///
    /// `true` if the file can be written, `false` if it should be skipped.
    fn is_writable(&self, _path: &str) -> Result<bool, Self::Error> {
        Ok(true)
    }

    /// Create a directory and all parent directories.
    ///
    /// # Arguments
    ///
    /// * `path` - Relative path of the directory to create
    ///
    /// # Returns
    ///
    /// `Ok(())` on success (including if the directory already exists).
    fn create_dir_all(&self, path: &str) -> Result<(), Self::Error>;

    /// Remove a file or directory.
    ///
    /// # Arguments
    ///
    /// * `path` - Relative path to remove
    /// * `recursive` - If true and path is a directory, remove contents recursively
    ///
    /// # Returns
    ///
    /// `Ok(())` on success. May return an error if the path doesn't exist
    /// or cannot be removed.
    fn remove_path(&self, path: &str, recursive: bool) -> Result<(), Self::Error>;

    /// Rename or move a file/directory.
    ///
    /// # Arguments
    ///
    /// * `from` - Current relative path
    /// * `to` - New relative path
    ///
    /// # Returns
    ///
    /// `Ok(())` on success. May return an error if the source doesn't exist
    /// or the destination cannot be written.
    fn rename(&self, from: &str, to: &str) -> Result<(), Self::Error>;

    /// Set file permissions.
    ///
    /// # Arguments
    ///
    /// * `path` - Relative path of the file
    /// * `permissions` - Unix-style permissions (e.g., 0o644)
    ///
    /// # Returns
    ///
    /// `Ok(())` on success.
    fn set_permissions(&self, path: &str, permissions: u16) -> Result<(), Self::Error>;

    /// Open a file for writing.
    ///
    /// Creates the file if it doesn't exist, or truncates it if it does.
    /// Parent directories are NOT automatically created.
    ///
    /// # Arguments
    ///
    /// * `path` - Relative path of the file
    /// * `inode` - The inode associated with this file (for tracking)
    ///
    /// # Returns
    ///
    /// A writer that can be used to write content to the file.
    fn write_file(&self, path: &str, inode: Inode) -> Result<Self::Writer, Self::Error>;
}

// ============================================================================
// VERTEX BUFFER TRAIT
// ============================================================================

/// Conflict marker constants.
pub mod markers {
    /// Start of a conflict region
    pub const START: &str = ">>>>>>>";
    /// Separator between conflict sides
    pub const SEPARATOR: &str = "=======";
    /// End of a conflict region
    pub const END: &str = "<<<<<<<";
}

/// A buffer for outputting span contents with conflict handling.
///
/// This trait abstracts the process of writing file contents from the
/// repository graph to a destination (file, memory buffer, etc.).
/// It handles:
///
/// - Writing content lines
/// - Outputting conflict markers
/// - Tracking line numbers for conflict reporting
///
/// # Conflict Markers
///
/// When conflicts are detected during output, the buffer emits markers:
///
/// ```text
/// >>>>>>> 1 [ABCD1234 First change]
/// Content from first side
/// ======= 1
/// Content from second side
/// <<<<<<< 1
/// ```
///
/// The number after the marker is a conflict ID for matching start/end.
pub trait VertexBuffer {
    /// Output the content of a span.
    ///
    /// # Arguments
    ///
    /// * `span` - The span whose content is being output
    /// * `get_contents` - A function that fills a buffer with the span's content
    ///
    /// # Type Parameters
    ///
    /// * `E` - Error type, must be convertible from `std::io::Error`
    /// * `F` - Content retrieval function
    ///
    /// # Returns
    ///
    /// `Ok(())` on success, or an error if writing fails.
    fn output_line<E, F>(&mut self, node: GraphNode<NodeId>, get_contents: F) -> Result<(), E>
    where
        E: From<std::io::Error>,
        F: FnOnce(&mut [u8]) -> Result<(), E>;

    /// Output a conflict marker.
    ///
    /// # Arguments
    ///
    /// * `marker` - The marker string (START, SEPARATOR, or END)
    /// * `id` - Unique ID for this conflict
    /// * `changes` - Optional list of change hashes involved in this conflict
    ///
    /// # Returns
    ///
    /// `Ok(())` on success.
    fn output_conflict_marker(
        &mut self,
        marker: &str,
        id: usize,
        changes: Option<&[Hash]>,
    ) -> Result<(), std::io::Error>;

    /// Begin a conflict region (order conflict).
    ///
    /// # Arguments
    ///
    /// * `id` - Unique ID for this conflict
    /// * `changes` - Optional change hashes for the first side
    fn begin_conflict(
        &mut self,
        id: usize,
        changes: Option<&[Hash]>,
    ) -> Result<(), std::io::Error> {
        self.output_conflict_marker(markers::START, id, changes)
    }

    /// Begin a zombie conflict region.
    ///
    /// Zombie conflicts occur when deleted content has live connections.
    fn begin_zombie_conflict(
        &mut self,
        id: usize,
        changes: Option<&[Hash]>,
    ) -> Result<(), std::io::Error> {
        self.output_conflict_marker(markers::START, id, changes)
    }

    /// Begin a cyclic conflict region.
    ///
    /// Cyclic conflicts occur when the graph has circular dependencies.
    fn begin_cyclic_conflict(&mut self, id: usize) -> Result<(), std::io::Error> {
        self.output_conflict_marker(markers::START, id, None)
    }

    /// Move to the next side of a conflict.
    ///
    /// # Arguments
    ///
    /// * `id` - The conflict ID (must match the begin call)
    /// * `changes` - Optional change hashes for this side
    fn conflict_next(&mut self, id: usize, changes: Option<&[Hash]>) -> Result<(), std::io::Error> {
        self.output_conflict_marker(markers::SEPARATOR, id, changes)
    }

    /// End a conflict region.
    ///
    /// # Arguments
    ///
    /// * `id` - The conflict ID (must match the begin call)
    fn end_conflict(&mut self, id: usize) -> Result<(), std::io::Error> {
        self.output_conflict_marker(markers::END, id, None)
    }

    /// End a zombie conflict region.
    fn end_zombie_conflict(&mut self, id: usize) -> Result<(), std::io::Error> {
        self.end_conflict(id)
    }

    /// End a cyclic conflict region.
    fn end_cyclic_conflict(&mut self, id: usize) -> Result<(), std::io::Error> {
        self.output_conflict_marker(markers::END, id, None)
    }
}

// ============================================================================
// SINK IMPLEMENTATION (DISCARDS OUTPUT)
// ============================================================================

/// A working copy implementation that discards all output.
///
/// This is useful for:
/// - Testing without side effects
/// - Checking for errors without writing files
/// - Dry-run operations
#[derive(Debug, Clone, Default)]
pub struct Sink;

impl Sink {
    /// Create a new sink.
    pub fn new() -> Self {
        Self
    }
}

/// Error type for sink operations (never actually occurs).
#[derive(Debug, Clone)]
pub struct SinkError;

impl std::fmt::Display for SinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "sink error (should never occur)")
    }
}

impl std::error::Error for SinkError {}

impl WorkingCopyRead for Sink {
    type Error = SinkError;

    fn file_metadata(&self, _path: &str) -> Result<FileMetadata, Self::Error> {
        Ok(FileMetadata::file())
    }

    fn read_file(&self, _path: &str, _buffer: &mut Vec<u8>) -> Result<(), Self::Error> {
        Ok(())
    }

    fn modified_time(&self, _path: &str) -> Result<SystemTime, Self::Error> {
        Ok(SystemTime::UNIX_EPOCH)
    }

    fn walk_files(&self, _prefix: &str) -> Result<Vec<String>, Self::Error> {
        Ok(Vec::new())
    }
}

impl WorkingCopy for Sink {
    type Writer = std::io::Sink;

    fn is_writable(&self, _path: &str) -> Result<bool, Self::Error> {
        Ok(false)
    }

    fn create_dir_all(&self, _path: &str) -> Result<(), Self::Error> {
        Ok(())
    }

    fn remove_path(&self, _path: &str, _recursive: bool) -> Result<(), Self::Error> {
        Ok(())
    }

    fn rename(&self, _from: &str, _to: &str) -> Result<(), Self::Error> {
        Ok(())
    }

    fn set_permissions(&self, _path: &str, _permissions: u16) -> Result<(), Self::Error> {
        Ok(())
    }

    fn write_file(&self, _path: &str, _inode: Inode) -> Result<Self::Writer, Self::Error> {
        Ok(std::io::sink())
    }
}

// ============================================================================
// BASIC VERTEX BUFFER WRITER
// ============================================================================

/// A basic `VertexBuffer` implementation that writes to any `Write` type.
///
/// This implementation:
/// - Writes span contents directly
/// - Outputs conflict markers with optional change information
/// - Tracks newline state for proper marker formatting
pub struct Writer<W: Write> {
    /// The underlying writer
    writer: W,
    /// Buffer for span content
    buffer: Vec<u8>,
    /// Whether the last byte written was a newline
    at_newline: bool,
}

impl<W: Write> Writer<W> {
    /// Create a new writer.
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            buffer: Vec::new(),
            at_newline: true,
        }
    }

    /// Get a reference to the underlying writer.
    pub fn inner(&self) -> &W {
        &self.writer
    }

    /// Get a mutable reference to the underlying writer.
    pub fn inner_mut(&mut self) -> &mut W {
        &mut self.writer
    }

    /// Consume this writer and return the underlying writer.
    pub fn into_inner(self) -> W {
        self.writer
    }
}

impl<W: Write> VertexBuffer for Writer<W> {
    fn output_line<E, F>(&mut self, node: GraphNode<NodeId>, get_contents: F) -> Result<(), E>
    where
        E: From<std::io::Error>,
        F: FnOnce(&mut [u8]) -> Result<(), E>,
    {
        // Resize buffer to fit span content
        let len = node.len();
        self.buffer.resize(len, 0);

        // Get content from change
        get_contents(&mut self.buffer)?;

        // Write content
        self.writer.write_all(&self.buffer)?;

        // Track newline state
        if !self.buffer.is_empty() {
            self.at_newline = self.buffer.ends_with(b"\n");
        }

        Ok(())
    }

    fn output_conflict_marker(
        &mut self,
        marker: &str,
        id: usize,
        changes: Option<&[Hash]>,
    ) -> Result<(), std::io::Error> {
        // Ensure we're on a new line
        if !self.at_newline {
            self.writer.write_all(b"\n")?;
        }

        // Write marker and ID
        write!(self.writer, "{} {}", marker, id)?;

        // Write change hashes if provided
        if let Some(hashes) = changes {
            for hash in hashes {
                let b32 = hash.to_base32();
                let short = if b32.len() >= 8 { &b32[..8] } else { &b32 };
                write!(self.writer, " [{}]", short)?;
            }
        }

        self.writer.write_all(b"\n")?;
        self.at_newline = true;
        Ok(())
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ChangePosition;

    // -------------------------------------------------------------------------
    // FileMetadata Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_file_metadata_file() {
        let meta = FileMetadata::file();
        assert_eq!(meta.permissions, 0o644);
        assert!(!meta.is_dir);
        assert!(!meta.is_symlink);
        assert!(!meta.is_executable());
    }

    #[test]
    fn test_file_metadata_executable() {
        let meta = FileMetadata::executable();
        assert_eq!(meta.permissions, 0o755);
        assert!(!meta.is_dir);
        assert!(!meta.is_symlink);
        assert!(meta.is_executable());
    }

    #[test]
    fn test_file_metadata_directory() {
        let meta = FileMetadata::directory();
        assert_eq!(meta.permissions, 0o755);
        assert!(meta.is_dir);
        assert!(!meta.is_symlink);
        assert!(!meta.is_executable()); // Directories are not "executable files"
    }

    #[test]
    fn test_file_metadata_symlink() {
        let meta = FileMetadata::symlink();
        assert_eq!(meta.permissions, 0o777);
        assert!(!meta.is_dir);
        assert!(meta.is_symlink);
    }

    #[test]
    fn test_file_metadata_with_permissions() {
        let meta = FileMetadata::file().with_permissions(0o600);
        assert_eq!(meta.permissions, 0o600);
        assert!(!meta.is_executable());
    }

    #[test]
    fn test_file_metadata_bytes_roundtrip() {
        let test_cases = [
            FileMetadata::file(),
            FileMetadata::executable(),
            FileMetadata::directory(),
            FileMetadata::symlink(),
            FileMetadata::file().with_permissions(0o600),
        ];

        for original in test_cases {
            let bytes = original.to_bytes();
            let recovered = FileMetadata::from_bytes(&bytes).expect("should decode");
            assert_eq!(original, recovered);
        }
    }

    #[test]
    fn test_file_metadata_from_bytes_too_short() {
        assert!(FileMetadata::from_bytes(&[0, 0]).is_none());
        assert!(FileMetadata::from_bytes(&[0]).is_none());
        assert!(FileMetadata::from_bytes(&[]).is_none());
    }

    #[test]
    fn test_file_metadata_default() {
        let meta = FileMetadata::default();
        assert_eq!(meta.permissions, 0);
        assert!(!meta.is_dir);
        assert!(!meta.is_symlink);
    }

    #[test]
    fn test_file_metadata_clone() {
        let original = FileMetadata::executable();
        let cloned = original;
        assert_eq!(original, cloned);
    }

    #[test]
    fn test_file_metadata_debug() {
        let meta = FileMetadata::file();
        let debug = format!("{:?}", meta);
        assert!(debug.contains("permissions"));
    }

    #[test]
    fn test_file_metadata_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(FileMetadata::file());
        set.insert(FileMetadata::executable());
        set.insert(FileMetadata::file()); // duplicate
        assert_eq!(set.len(), 2);
    }

    // -------------------------------------------------------------------------
    // Sink Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_sink_new() {
        let sink = Sink::new();
        assert!(format!("{:?}", sink).contains("Sink"));
    }

    #[test]
    fn test_sink_default() {
        let sink = Sink::default();
        assert!(format!("{:?}", sink).contains("Sink"));
    }

    #[test]
    fn test_sink_clone() {
        let original = Sink::new();
        let cloned = original.clone();
        assert!(format!("{:?}", cloned).contains("Sink"));
    }

    #[test]
    fn test_sink_file_metadata() {
        let sink = Sink::new();
        let meta = sink.file_metadata("/any/path").unwrap();
        assert_eq!(meta, FileMetadata::file());
    }

    #[test]
    fn test_sink_read_file() {
        let sink = Sink::new();
        let mut buffer = Vec::new();
        sink.read_file("/any/path", &mut buffer).unwrap();
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_sink_modified_time() {
        let sink = Sink::new();
        let time = sink.modified_time("/any/path").unwrap();
        assert_eq!(time, SystemTime::UNIX_EPOCH);
    }

    #[test]
    fn test_sink_exists() {
        let sink = Sink::new();
        assert!(sink.exists("/any/path"));
    }

    #[test]
    fn test_sink_is_directory() {
        let sink = Sink::new();
        assert!(!sink.is_directory("/any/path"));
    }

    #[test]
    fn test_sink_is_writable() {
        let sink = Sink::new();
        assert!(!sink.is_writable("/any/path").unwrap());
    }

    #[test]
    fn test_sink_create_dir_all() {
        let sink = Sink::new();
        sink.create_dir_all("/any/path").unwrap();
    }

    #[test]
    fn test_sink_remove_path() {
        let sink = Sink::new();
        sink.remove_path("/any/path", true).unwrap();
    }

    #[test]
    fn test_sink_rename() {
        let sink = Sink::new();
        sink.rename("/from", "/to").unwrap();
    }

    #[test]
    fn test_sink_set_permissions() {
        let sink = Sink::new();
        sink.set_permissions("/any/path", 0o755).unwrap();
    }

    #[test]
    fn test_sink_write_file() {
        let sink = Sink::new();
        let mut writer = sink.write_file("/any/path", Inode::ROOT).unwrap();
        writer.write_all(b"test").unwrap();
    }

    #[test]
    fn test_sink_error_display() {
        let err = SinkError;
        assert!(err.to_string().contains("sink"));
    }

    #[test]
    fn test_sink_error_debug() {
        let err = SinkError;
        let debug = format!("{:?}", err);
        assert!(debug.contains("SinkError"));
    }

    // -------------------------------------------------------------------------
    // Writer Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_writer_new() {
        let buf = Vec::new();
        let writer = Writer::new(buf);
        assert!(writer.inner().is_empty());
    }

    #[test]
    fn test_writer_into_inner() {
        let buf = vec![1, 2, 3];
        let writer = Writer::new(buf);
        let inner = writer.into_inner();
        assert_eq!(inner, vec![1, 2, 3]);
    }

    #[test]
    fn test_writer_output_line_simple() {
        let buf: Vec<u8> = Vec::new();
        let mut writer = Writer::new(buf);

        let node = GraphNode {
            change: NodeId::new(1),
            start: ChangePosition::new(0),
            end: ChangePosition::new(5),
        };

        let result: Result<(), std::io::Error> = writer.output_line(node, |buf| {
            buf.copy_from_slice(b"hello");
            Ok(())
        });
        result.unwrap();

        assert_eq!(writer.into_inner(), b"hello");
    }

    #[test]
    fn test_writer_output_line_with_newline() {
        let buf: Vec<u8> = Vec::new();
        let mut writer = Writer::new(buf);

        let node = GraphNode {
            change: NodeId::new(1),
            start: ChangePosition::new(0),
            end: ChangePosition::new(6),
        };

        let result: Result<(), std::io::Error> = writer.output_line(node, |buf| {
            buf.copy_from_slice(b"hello\n");
            Ok(())
        });
        result.unwrap();

        assert_eq!(writer.into_inner(), b"hello\n");
    }

    #[test]
    fn test_writer_output_conflict_marker_basic() {
        let buf: Vec<u8> = Vec::new();
        let mut writer = Writer::new(buf);

        writer
            .output_conflict_marker(markers::START, 1, None)
            .unwrap();

        let output = String::from_utf8(writer.into_inner()).unwrap();
        assert!(output.contains(">>>>>>>"));
        assert!(output.contains("1"));
        assert!(output.ends_with('\n'));
    }

    #[test]
    fn test_writer_output_conflict_marker_with_changes() {
        let buf: Vec<u8> = Vec::new();
        let mut writer = Writer::new(buf);

        let hash = Hash::of(b"test change");
        writer
            .output_conflict_marker(markers::START, 42, Some(&[hash]))
            .unwrap();

        let output = String::from_utf8(writer.into_inner()).unwrap();
        assert!(output.contains(">>>>>>>"));
        assert!(output.contains("42"));
        assert!(output.contains("[")); // Has change hash bracket
    }

    #[test]
    fn test_writer_begin_conflict() {
        let buf: Vec<u8> = Vec::new();
        let mut writer = Writer::new(buf);

        writer.begin_conflict(1, None).unwrap();

        let output = String::from_utf8(writer.into_inner()).unwrap();
        assert!(output.contains(markers::START));
    }

    #[test]
    fn test_writer_conflict_next() {
        let buf: Vec<u8> = Vec::new();
        let mut writer = Writer::new(buf);

        writer.conflict_next(1, None).unwrap();

        let output = String::from_utf8(writer.into_inner()).unwrap();
        assert!(output.contains(markers::SEPARATOR));
    }

    #[test]
    fn test_writer_end_conflict() {
        let buf: Vec<u8> = Vec::new();
        let mut writer = Writer::new(buf);

        writer.end_conflict(1).unwrap();

        let output = String::from_utf8(writer.into_inner()).unwrap();
        assert!(output.contains(markers::END));
    }

    #[test]
    fn test_writer_full_conflict_sequence() {
        let buf: Vec<u8> = Vec::new();
        let mut writer = Writer::new(buf);

        // Simulate a full conflict
        writer.begin_conflict(1, None).unwrap();

        // First side content
        let v1 = GraphNode {
            change: NodeId::new(1),
            start: ChangePosition::new(0),
            end: ChangePosition::new(7),
        };
        let _: Result<(), std::io::Error> = writer.output_line(v1, |buf| {
            buf.copy_from_slice(b"side 1\n");
            Ok(())
        });

        writer.conflict_next(1, None).unwrap();

        // Second side content
        let v2 = GraphNode {
            change: NodeId::new(2),
            start: ChangePosition::new(0),
            end: ChangePosition::new(7),
        };
        let _: Result<(), std::io::Error> = writer.output_line(v2, |buf| {
            buf.copy_from_slice(b"side 2\n");
            Ok(())
        });

        writer.end_conflict(1).unwrap();

        let output = String::from_utf8(writer.into_inner()).unwrap();
        assert!(output.contains(markers::START));
        assert!(output.contains("side 1"));
        assert!(output.contains(markers::SEPARATOR));
        assert!(output.contains("side 2"));
        assert!(output.contains(markers::END));
    }

    #[test]
    fn test_writer_marker_after_no_newline() {
        let buf: Vec<u8> = Vec::new();
        let mut writer = Writer::new(buf);

        // Write content without trailing newline
        let v = GraphNode {
            change: NodeId::new(1),
            start: ChangePosition::new(0),
            end: ChangePosition::new(4),
        };
        let _: Result<(), std::io::Error> = writer.output_line(v, |buf| {
            buf.copy_from_slice(b"test");
            Ok(())
        });

        // Now write a conflict marker - should add newline first
        writer.begin_conflict(1, None).unwrap();

        let output = String::from_utf8(writer.into_inner()).unwrap();
        // Should have newline before marker
        assert!(output.contains("test\n>>>>>>>"));
    }

    #[test]
    fn test_writer_zombie_conflict() {
        let buf: Vec<u8> = Vec::new();
        let mut writer = Writer::new(buf);

        writer.begin_zombie_conflict(5, None).unwrap();
        writer.end_zombie_conflict(5).unwrap();

        let output = String::from_utf8(writer.into_inner()).unwrap();
        assert!(output.contains(markers::START));
        assert!(output.contains(markers::END));
        assert!(output.contains("5"));
    }

    #[test]
    fn test_writer_cyclic_conflict() {
        let buf: Vec<u8> = Vec::new();
        let mut writer = Writer::new(buf);

        writer.begin_cyclic_conflict(3).unwrap();
        writer.end_cyclic_conflict(3).unwrap();

        let output = String::from_utf8(writer.into_inner()).unwrap();
        assert!(output.contains(markers::START));
        assert!(output.contains(markers::END));
        assert!(output.contains("3"));
    }

    #[test]
    fn test_writer_empty_vertex() {
        let buf: Vec<u8> = Vec::new();
        let mut writer = Writer::new(buf);

        let node = GraphNode {
            change: NodeId::new(1),
            start: ChangePosition::new(0),
            end: ChangePosition::new(0), // Empty span
        };

        let result: Result<(), std::io::Error> = writer.output_line(node, |_buf| Ok(()));
        result.unwrap();

        assert!(writer.into_inner().is_empty());
    }

    #[test]
    fn test_writer_inner_mut() {
        let buf: Vec<u8> = Vec::new();
        let mut writer = Writer::new(buf);

        writer.inner_mut().push(42);

        assert_eq!(writer.inner()[0], 42);
    }

    // -------------------------------------------------------------------------
    // Marker Constants Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_markers_values() {
        assert_eq!(markers::START, ">>>>>>>");
        assert_eq!(markers::SEPARATOR, "=======");
        assert_eq!(markers::END, "<<<<<<<");
    }

    #[test]
    fn test_markers_distinct() {
        assert_ne!(markers::START, markers::SEPARATOR);
        assert_ne!(markers::SEPARATOR, markers::END);
        assert_ne!(markers::START, markers::END);
    }
}
