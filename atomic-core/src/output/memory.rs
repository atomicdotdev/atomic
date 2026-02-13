//! In-memory working copy implementation
//!
//! This module provides a fully in-memory implementation of the working copy
//! traits, primarily designed for:
//!
//! - **Testing**: Fast, isolated tests without filesystem side effects
//! - **Embedded use**: Running Atomic in memory-only environments
//! - **Dry-run operations**: Preview changes without modifying disk
//!
//! # Architecture
//!
//! The `Memory` working copy stores files in a `HashMap` with paths as keys.
//! Each entry contains:
//!
//! - File contents (bytes)
//! - File metadata (permissions, type)
//! - Modification timestamp
//! - Associated inode
//!
//! ```text
//! Memory Working Copy
//! ┌─────────────────────────────────────────────────────────────┐
//! │  files: HashMap<String, MemoryFile>                        │
//! │  ┌─────────────────────────────────────────────────────┐   │
//! │  │ "src/main.rs" -> MemoryFile {                       │   │
//! │  │     contents: [102, 110, 32, 109, 97, 105, 110, ...]│   │
//! │  │     metadata: FileMetadata { permissions: 0o644 }   │   │
//! │  │     modified: SystemTime { ... }                    │   │
//! │  │     inode: Inode(42)                                │   │
//! │  │ }                                                   │   │
//! │  └─────────────────────────────────────────────────────┘   │
//! │  next_inode: AtomicU64(43)                                 │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Thread Safety
//!
//! The `Memory` implementation uses interior mutability via `RefCell`,
//! making it **not thread-safe**. This is intentional for performance
//! in single-threaded test scenarios. For concurrent access, wrap in
//! appropriate synchronization primitives.
//!
//! # Example: Basic Usage
//!
//! ```rust
//! use atomic_core::output::{Memory, WorkingCopy, WorkingCopyRead};
//! use std::io::Write;
//!
//! // Create an empty in-memory working copy
//! let wc = Memory::new();
//!
//! // Create a directory and write a file
//! wc.create_dir_all("src").unwrap();
//! let inode = wc.allocate_inode();
//! let mut writer = wc.write_file("src/main.rs", inode).unwrap();
//! writer.write_all(b"fn main() {}").unwrap();
//! drop(writer);
//!
//! // Read the file back
//! let mut buffer = Vec::new();
//! wc.read_file("src/main.rs", &mut buffer).unwrap();
//! assert_eq!(buffer, b"fn main() {}");
//! ```
//!
//! # Example: Testing VCS Operations
//!
//! ```rust,ignore
//! use atomic_core::output::Memory;
//!
//! #[test]
//! fn test_file_output() {
//!     let wc = Memory::new();
//!
//!     // Populate initial state
//!     wc.add_file("README.md", b"# Hello");
//!
//!     // Run output operation
//!     output_repository(&wc, &txn, &channel)?;
//!
//!     // Verify results
//!     let content = wc.get_file_contents("README.md").unwrap();
//!     assert!(content.starts_with(b"# Hello"));
//! }
//! ```

use super::traits::{FileMetadata, WorkingCopy, WorkingCopyRead};
use crate::types::Inode;
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{self, Cursor, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

// ============================================================================
// ERROR TYPE
// ============================================================================

/// Error type for memory working copy operations.
///
/// This error type mirrors common filesystem errors while being
/// specific to the in-memory implementation.
#[derive(Debug, Clone)]
pub enum MemoryError {
    /// The requested file or directory was not found.
    NotFound {
        /// The path that was not found
        path: String,
    },

    /// The path is not a directory (e.g., trying to create a file in a file).
    NotADirectory {
        /// The path that is not a directory
        path: String,
    },

    /// The path is a directory when a file was expected.
    IsADirectory {
        /// The path that is a directory
        path: String,
    },

    /// Permission denied for the operation.
    PermissionDenied {
        /// The path where permission was denied
        path: String,
    },

    /// The file already exists.
    AlreadyExists {
        /// The path that already exists
        path: String,
    },

    /// Directory is not empty (for non-recursive removal).
    DirectoryNotEmpty {
        /// The path to the non-empty directory
        path: String,
    },
}

impl std::fmt::Display for MemoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MemoryError::NotFound { path } => write!(f, "File not found: {}", path),
            MemoryError::NotADirectory { path } => write!(f, "Not a directory: {}", path),
            MemoryError::IsADirectory { path } => write!(f, "Is a directory: {}", path),
            MemoryError::PermissionDenied { path } => write!(f, "Permission denied: {}", path),
            MemoryError::AlreadyExists { path } => write!(f, "Already exists: {}", path),
            MemoryError::DirectoryNotEmpty { path } => write!(f, "Directory not empty: {}", path),
        }
    }
}

impl std::error::Error for MemoryError {}

// ============================================================================
// MEMORY FILE ENTRY
// ============================================================================

/// An in-memory file or directory entry.
#[derive(Debug, Clone)]
struct MemoryFile {
    /// File contents (empty for directories).
    contents: Vec<u8>,
    /// File metadata.
    metadata: FileMetadata,
    /// Last modification time.
    modified: SystemTime,
    /// Associated inode.
    inode: Inode,
}

impl MemoryFile {
    /// Create a new file entry.
    fn new_file(contents: Vec<u8>, inode: Inode) -> Self {
        Self {
            contents,
            metadata: FileMetadata::file(),
            modified: SystemTime::now(),
            inode,
        }
    }

    /// Create a new directory entry.
    fn new_directory(inode: Inode) -> Self {
        Self {
            contents: Vec::new(),
            metadata: FileMetadata::directory(),
            modified: SystemTime::now(),
            inode,
        }
    }
}

// ============================================================================
// MEMORY WRITER
// ============================================================================

/// A writer that captures content and writes it to the memory working copy
/// when dropped or explicitly flushed.
pub struct MemoryWriter {
    /// Path being written to.
    path: String,
    /// Buffer for content being written.
    buffer: Cursor<Vec<u8>>,
    /// Reference back to the working copy's files.
    files: *const RefCell<HashMap<String, MemoryFile>>,
    /// Inode for this file.
    inode: Inode,
}

impl Write for MemoryWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        // Flush writes the content to the in-memory storage
        let files = unsafe { &*self.files };
        let contents = self.buffer.get_ref().clone();
        files.borrow_mut().insert(
            self.path.clone(),
            MemoryFile {
                contents,
                metadata: FileMetadata::file(),
                modified: SystemTime::now(),
                inode: self.inode,
            },
        );
        Ok(())
    }
}

impl Drop for MemoryWriter {
    fn drop(&mut self) {
        // Ensure content is written on drop
        let _ = self.flush();
    }
}

// ============================================================================
// MEMORY WORKING COPY
// ============================================================================

/// An in-memory working copy implementation.
///
/// Stores all files and directories in memory using a `HashMap`.
/// Provides full `WorkingCopy` trait implementation for testing and
/// embedded use cases.
///
/// # Thread Safety
///
/// This implementation is **not thread-safe**. It uses `RefCell` for
/// interior mutability, which will panic on concurrent access.
///
/// # Inode Allocation
///
/// Inodes are allocated from an atomic counter, ensuring unique values
/// across the lifetime of the working copy.
pub struct Memory {
    /// Map of path to file entry.
    files: RefCell<HashMap<String, MemoryFile>>,
    /// Counter for allocating new inodes.
    next_inode: AtomicU64,
    /// Paths that are marked as non-writable.
    non_writable: RefCell<Vec<String>>,
}

impl Memory {
    /// Create a new, empty in-memory working copy.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::Memory;
    ///
    /// let wc = Memory::new();
    /// ```
    pub fn new() -> Self {
        Self {
            files: RefCell::new(HashMap::new()),
            next_inode: AtomicU64::new(1), // Start from 1, 0 is ROOT
            non_writable: RefCell::new(Vec::new()),
        }
    }

    /// Allocate a new unique inode.
    ///
    /// Each call returns a different inode value.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::Memory;
    ///
    /// let wc = Memory::new();
    /// let inode1 = wc.allocate_inode();
    /// let inode2 = wc.allocate_inode();
    /// assert_ne!(inode1, inode2);
    /// ```
    pub fn allocate_inode(&self) -> Inode {
        Inode::new(self.next_inode.fetch_add(1, Ordering::SeqCst))
    }

    /// Add a file with the given contents.
    ///
    /// This is a convenience method for testing. Creates parent directories
    /// automatically.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::{Memory, WorkingCopyRead};
    ///
    /// let wc = Memory::new();
    /// wc.add_file("src/main.rs", b"fn main() {}");
    ///
    /// let mut buf = Vec::new();
    /// wc.read_file("src/main.rs", &mut buf).unwrap();
    /// assert_eq!(buf, b"fn main() {}");
    /// ```
    pub fn add_file(&self, path: &str, contents: &[u8]) {
        // Create parent directories
        if let Some(parent) = path_parent(path) {
            self.ensure_directories(&parent);
        }

        let inode = self.allocate_inode();
        self.files.borrow_mut().insert(
            path.to_string(),
            MemoryFile::new_file(contents.to_vec(), inode),
        );
    }

    /// Add a directory.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::{Memory, WorkingCopyRead};
    ///
    /// let wc = Memory::new();
    /// wc.add_directory("src/utils");
    ///
    /// assert!(wc.is_directory("src/utils"));
    /// ```
    pub fn add_directory(&self, path: &str) {
        self.ensure_directories(path);
    }

    /// Mark a path as non-writable.
    ///
    /// Files marked as non-writable will return `false` from `is_writable()`.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::{Memory, WorkingCopy};
    ///
    /// let wc = Memory::new();
    /// wc.add_file("readonly.txt", b"data");
    /// wc.mark_non_writable("readonly.txt");
    ///
    /// assert!(!wc.is_writable("readonly.txt").unwrap());
    /// ```
    pub fn mark_non_writable(&self, path: &str) {
        self.non_writable.borrow_mut().push(path.to_string());
    }

    /// Get the raw contents of a file.
    ///
    /// Returns `None` if the file doesn't exist or is a directory.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::Memory;
    ///
    /// let wc = Memory::new();
    /// wc.add_file("test.txt", b"hello");
    ///
    /// let contents = wc.get_file_contents("test.txt").unwrap();
    /// assert_eq!(contents, b"hello");
    /// ```
    pub fn get_file_contents(&self, path: &str) -> Option<Vec<u8>> {
        let files = self.files.borrow();
        files.get(path).and_then(|f| {
            if f.metadata.is_dir {
                None
            } else {
                Some(f.contents.clone())
            }
        })
    }

    /// Get the inode for a file.
    ///
    /// Returns `None` if the file doesn't exist.
    pub fn get_inode(&self, path: &str) -> Option<Inode> {
        let files = self.files.borrow();
        files.get(path).map(|f| f.inode)
    }

    /// List all files (not directories) in the working copy.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::Memory;
    ///
    /// let wc = Memory::new();
    /// wc.add_file("a.txt", b"");
    /// wc.add_file("b.txt", b"");
    ///
    /// let files = wc.list_files();
    /// assert_eq!(files.len(), 2);
    /// ```
    pub fn list_files(&self) -> Vec<String> {
        let files = self.files.borrow();
        files
            .iter()
            .filter(|(_, f)| !f.metadata.is_dir)
            .map(|(p, _)| p.clone())
            .collect()
    }

    /// List all paths (files and directories) in the working copy.
    pub fn list_all_paths(&self) -> Vec<String> {
        let files = self.files.borrow();
        files.keys().cloned().collect()
    }

    /// Check if the working copy is empty.
    pub fn is_empty(&self) -> bool {
        self.files.borrow().is_empty()
    }

    /// Get the number of entries (files + directories).
    pub fn len(&self) -> usize {
        self.files.borrow().len()
    }

    /// Clear all files and directories.
    pub fn clear(&self) {
        self.files.borrow_mut().clear();
        self.non_writable.borrow_mut().clear();
    }

    /// Ensure all directories in a path exist.
    fn ensure_directories(&self, path: &str) {
        let mut current = String::new();
        for component in path.split('/') {
            if component.is_empty() {
                continue;
            }
            if !current.is_empty() {
                current.push('/');
            }
            current.push_str(component);

            let mut files = self.files.borrow_mut();
            if !files.contains_key(&current) {
                let inode = Inode::new(self.next_inode.fetch_add(1, Ordering::SeqCst));
                files.insert(current.clone(), MemoryFile::new_directory(inode));
            }
        }
    }

    /// Get entries within a directory.
    #[allow(dead_code)]
    fn list_directory_contents(&self, path: &str) -> Vec<String> {
        let files = self.files.borrow();
        let prefix = if path.is_empty() {
            String::new()
        } else {
            format!("{}/", path)
        };

        files
            .keys()
            .filter(|p| {
                if prefix.is_empty() {
                    !p.contains('/')
                } else {
                    p.starts_with(&prefix) && !p[prefix.len()..].contains('/')
                }
            })
            .cloned()
            .collect()
    }
}

impl Default for Memory {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Memory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let files = self.files.borrow();
        f.debug_struct("Memory")
            .field("file_count", &files.len())
            .field("files", &files.keys().collect::<Vec<_>>())
            .finish()
    }
}

// ============================================================================
// TRAIT IMPLEMENTATIONS
// ============================================================================

impl WorkingCopyRead for Memory {
    type Error = MemoryError;

    fn file_metadata(&self, path: &str) -> Result<FileMetadata, Self::Error> {
        let files = self.files.borrow();
        files
            .get(path)
            .map(|f| f.metadata)
            .ok_or_else(|| MemoryError::NotFound {
                path: path.to_string(),
            })
    }

    fn read_file(&self, path: &str, buffer: &mut Vec<u8>) -> Result<(), Self::Error> {
        let files = self.files.borrow();
        match files.get(path) {
            Some(file) if file.metadata.is_dir => Err(MemoryError::IsADirectory {
                path: path.to_string(),
            }),
            Some(file) => {
                buffer.extend_from_slice(&file.contents);
                Ok(())
            }
            None => Err(MemoryError::NotFound {
                path: path.to_string(),
            }),
        }
    }

    fn modified_time(&self, path: &str) -> Result<SystemTime, Self::Error> {
        let files = self.files.borrow();
        files
            .get(path)
            .map(|f| f.modified)
            .ok_or_else(|| MemoryError::NotFound {
                path: path.to_string(),
            })
    }

    fn exists(&self, path: &str) -> bool {
        self.files.borrow().contains_key(path)
    }

    fn is_directory(&self, path: &str) -> bool {
        self.files
            .borrow()
            .get(path)
            .map(|f| f.metadata.is_dir)
            .unwrap_or(false)
    }

    fn walk_files(&self, prefix: &str) -> Result<Vec<String>, Self::Error> {
        let files = self.files.borrow();
        let mut result: Vec<String> = files
            .iter()
            .filter(|(path, file)| {
                // Only include files, not directories
                if file.metadata.is_dir {
                    return false;
                }
                // Skip .atomic directory
                if path.starts_with(".atomic/") || *path == ".atomic" {
                    return false;
                }
                // Filter by prefix if provided
                if prefix.is_empty() {
                    true
                } else if prefix.ends_with('/') {
                    path.starts_with(prefix)
                } else {
                    path.starts_with(&format!("{}/", prefix)) || *path == prefix
                }
            })
            .map(|(path, _)| path.clone())
            .collect();
        result.sort();
        Ok(result)
    }
}

impl WorkingCopy for Memory {
    type Writer = MemoryWriter;

    fn is_writable(&self, path: &str) -> Result<bool, Self::Error> {
        let non_writable = self.non_writable.borrow();
        Ok(!non_writable.iter().any(|p| p == path))
    }

    fn create_dir_all(&self, path: &str) -> Result<(), Self::Error> {
        self.ensure_directories(path);
        Ok(())
    }

    fn remove_path(&self, path: &str, recursive: bool) -> Result<(), Self::Error> {
        let mut files = self.files.borrow_mut();

        // Check if path exists
        if !files.contains_key(path) {
            return Err(MemoryError::NotFound {
                path: path.to_string(),
            });
        }

        // Check if it's a directory with contents
        let is_dir = files.get(path).map(|f| f.metadata.is_dir).unwrap_or(false);
        if is_dir && !recursive {
            // Check for children
            let prefix = format!("{}/", path);
            if files.keys().any(|p| p.starts_with(&prefix)) {
                return Err(MemoryError::DirectoryNotEmpty {
                    path: path.to_string(),
                });
            }
        }

        if recursive {
            // Remove all paths that start with this path
            let prefix = format!("{}/", path);
            files.retain(|p, _| !p.starts_with(&prefix) && p != path);
        } else {
            files.remove(path);
        }

        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), Self::Error> {
        let mut files = self.files.borrow_mut();

        // Get the entry
        let entry = files.remove(from).ok_or_else(|| MemoryError::NotFound {
            path: from.to_string(),
        })?;

        // Insert at new location
        files.insert(to.to_string(), entry);

        // If it's a directory, also rename all children
        let from_prefix = format!("{}/", from);
        let to_prefix = format!("{}/", to);

        let children: Vec<_> = files
            .keys()
            .filter(|p| p.starts_with(&from_prefix))
            .cloned()
            .collect();

        for child in children {
            if let Some(entry) = files.remove(&child) {
                let new_path = child.replacen(&from_prefix, &to_prefix, 1);
                files.insert(new_path, entry);
            }
        }

        Ok(())
    }

    fn set_permissions(&self, path: &str, permissions: u16) -> Result<(), Self::Error> {
        let mut files = self.files.borrow_mut();

        match files.get_mut(path) {
            Some(file) => {
                file.metadata.permissions = permissions;
                Ok(())
            }
            None => Err(MemoryError::NotFound {
                path: path.to_string(),
            }),
        }
    }

    fn write_file(&self, path: &str, inode: Inode) -> Result<Self::Writer, Self::Error> {
        // Ensure parent directory exists
        if let Some(parent) = path_parent(path) {
            self.ensure_directories(&parent);
        }

        Ok(MemoryWriter {
            path: path.to_string(),
            buffer: Cursor::new(Vec::new()),
            files: &self.files as *const RefCell<HashMap<String, MemoryFile>>,
            inode,
        })
    }
}

// ============================================================================
// HELPER FUNCTIONS
// ============================================================================

/// Get the parent path of a file path.
///
/// Returns `None` if the path has no parent (is a top-level file).
fn path_parent(path: &str) -> Option<String> {
    let path = path.trim_end_matches('/');
    match path.rfind('/') {
        Some(pos) if pos > 0 => Some(path[..pos].to_string()),
        _ => None,
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------------
    // Memory Basic Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_memory_new() {
        let wc = Memory::new();
        assert!(wc.is_empty());
        assert_eq!(wc.len(), 0);
    }

    #[test]
    fn test_memory_default() {
        let wc = Memory::default();
        assert!(wc.is_empty());
    }

    #[test]
    fn test_memory_debug() {
        let wc = Memory::new();
        wc.add_file("test.txt", b"content");
        let debug = format!("{:?}", wc);
        assert!(debug.contains("Memory"));
        assert!(debug.contains("test.txt"));
    }

    #[test]
    fn test_memory_allocate_inode() {
        let wc = Memory::new();
        let i1 = wc.allocate_inode();
        let i2 = wc.allocate_inode();
        let i3 = wc.allocate_inode();
        assert_ne!(i1, i2);
        assert_ne!(i2, i3);
        assert_ne!(i1, i3);
    }

    #[test]
    fn test_memory_clear() {
        let wc = Memory::new();
        wc.add_file("a.txt", b"");
        wc.add_file("b.txt", b"");
        assert_eq!(wc.len(), 2);

        wc.clear();
        assert!(wc.is_empty());
    }

    // -------------------------------------------------------------------------
    // File Operations Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_add_file() {
        let wc = Memory::new();
        wc.add_file("test.txt", b"hello world");

        let contents = wc.get_file_contents("test.txt").unwrap();
        assert_eq!(contents, b"hello world");
    }

    #[test]
    fn test_add_file_creates_parent_dirs() {
        let wc = Memory::new();
        wc.add_file("a/b/c/test.txt", b"nested");

        assert!(wc.is_directory("a"));
        assert!(wc.is_directory("a/b"));
        assert!(wc.is_directory("a/b/c"));
        assert!(wc.exists("a/b/c/test.txt"));
    }

    #[test]
    fn test_add_directory() {
        let wc = Memory::new();
        wc.add_directory("src/utils");

        assert!(wc.is_directory("src"));
        assert!(wc.is_directory("src/utils"));
    }

    #[test]
    fn test_list_files() {
        let wc = Memory::new();
        wc.add_file("a.txt", b"");
        wc.add_file("b/c.txt", b"");
        wc.add_directory("empty_dir");

        let files = wc.list_files();
        assert_eq!(files.len(), 2);
        assert!(files.contains(&"a.txt".to_string()));
        assert!(files.contains(&"b/c.txt".to_string()));
    }

    #[test]
    fn test_get_inode() {
        let wc = Memory::new();
        wc.add_file("test.txt", b"");

        let inode = wc.get_inode("test.txt");
        assert!(inode.is_some());
    }

    #[test]
    fn test_get_inode_not_found() {
        let wc = Memory::new();
        assert!(wc.get_inode("nonexistent").is_none());
    }

    // -------------------------------------------------------------------------
    // WorkingCopyRead Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_file_metadata() {
        let wc = Memory::new();
        wc.add_file("test.txt", b"");

        let meta = wc.file_metadata("test.txt").unwrap();
        assert!(!meta.is_dir);
        assert_eq!(meta.permissions, 0o644);
    }

    #[test]
    fn test_file_metadata_directory() {
        let wc = Memory::new();
        wc.add_directory("mydir");

        let meta = wc.file_metadata("mydir").unwrap();
        assert!(meta.is_dir);
    }

    #[test]
    fn test_file_metadata_not_found() {
        let wc = Memory::new();
        let result = wc.file_metadata("nonexistent");
        assert!(matches!(result, Err(MemoryError::NotFound { .. })));
    }

    #[test]
    fn test_read_file() {
        let wc = Memory::new();
        wc.add_file("test.txt", b"content here");

        let mut buffer = Vec::new();
        wc.read_file("test.txt", &mut buffer).unwrap();
        assert_eq!(buffer, b"content here");
    }

    #[test]
    fn test_read_file_appends() {
        let wc = Memory::new();
        wc.add_file("test.txt", b"world");

        let mut buffer = b"hello ".to_vec();
        wc.read_file("test.txt", &mut buffer).unwrap();
        assert_eq!(buffer, b"hello world");
    }

    #[test]
    fn test_read_file_not_found() {
        let wc = Memory::new();
        let mut buffer = Vec::new();
        let result = wc.read_file("nonexistent", &mut buffer);
        assert!(matches!(result, Err(MemoryError::NotFound { .. })));
    }

    #[test]
    fn test_read_file_is_directory() {
        let wc = Memory::new();
        wc.add_directory("mydir");

        let mut buffer = Vec::new();
        let result = wc.read_file("mydir", &mut buffer);
        assert!(matches!(result, Err(MemoryError::IsADirectory { .. })));
    }

    #[test]
    fn test_modified_time() {
        let wc = Memory::new();
        wc.add_file("test.txt", b"");

        let time = wc.modified_time("test.txt").unwrap();
        assert!(time > SystemTime::UNIX_EPOCH);
    }

    #[test]
    fn test_exists() {
        let wc = Memory::new();
        assert!(!wc.exists("test.txt"));

        wc.add_file("test.txt", b"");
        assert!(wc.exists("test.txt"));
    }

    #[test]
    fn test_is_directory() {
        let wc = Memory::new();
        wc.add_file("file.txt", b"");
        wc.add_directory("dir");

        assert!(!wc.is_directory("file.txt"));
        assert!(wc.is_directory("dir"));
        assert!(!wc.is_directory("nonexistent"));
    }

    // -------------------------------------------------------------------------
    // WorkingCopy Write Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_is_writable() {
        let wc = Memory::new();
        assert!(wc.is_writable("any/path").unwrap());
    }

    #[test]
    fn test_is_writable_marked() {
        let wc = Memory::new();
        wc.add_file("readonly.txt", b"");
        wc.mark_non_writable("readonly.txt");

        assert!(!wc.is_writable("readonly.txt").unwrap());
        assert!(wc.is_writable("other.txt").unwrap());
    }

    #[test]
    fn test_create_dir_all() {
        let wc = Memory::new();
        wc.create_dir_all("a/b/c/d").unwrap();

        assert!(wc.is_directory("a"));
        assert!(wc.is_directory("a/b"));
        assert!(wc.is_directory("a/b/c"));
        assert!(wc.is_directory("a/b/c/d"));
    }

    #[test]
    fn test_remove_path_file() {
        let wc = Memory::new();
        wc.add_file("test.txt", b"");

        wc.remove_path("test.txt", false).unwrap();
        assert!(!wc.exists("test.txt"));
    }

    #[test]
    fn test_remove_path_not_found() {
        let wc = Memory::new();
        let result = wc.remove_path("nonexistent", false);
        assert!(matches!(result, Err(MemoryError::NotFound { .. })));
    }

    #[test]
    fn test_remove_path_empty_directory() {
        let wc = Memory::new();
        wc.add_directory("empty");

        wc.remove_path("empty", false).unwrap();
        assert!(!wc.exists("empty"));
    }

    #[test]
    fn test_remove_path_non_empty_directory_error() {
        let wc = Memory::new();
        wc.add_file("dir/file.txt", b"");

        let result = wc.remove_path("dir", false);
        assert!(matches!(result, Err(MemoryError::DirectoryNotEmpty { .. })));
    }

    #[test]
    fn test_remove_path_recursive() {
        let wc = Memory::new();
        wc.add_file("dir/a.txt", b"");
        wc.add_file("dir/sub/b.txt", b"");

        wc.remove_path("dir", true).unwrap();
        assert!(!wc.exists("dir"));
        assert!(!wc.exists("dir/a.txt"));
        assert!(!wc.exists("dir/sub"));
        assert!(!wc.exists("dir/sub/b.txt"));
    }

    #[test]
    fn test_rename_file() {
        let wc = Memory::new();
        wc.add_file("old.txt", b"content");

        wc.rename("old.txt", "new.txt").unwrap();

        assert!(!wc.exists("old.txt"));
        assert!(wc.exists("new.txt"));
        assert_eq!(wc.get_file_contents("new.txt").unwrap(), b"content");
    }

    #[test]
    fn test_rename_directory() {
        let wc = Memory::new();
        wc.add_file("old/a.txt", b"a");
        wc.add_file("old/b.txt", b"b");

        wc.rename("old", "new").unwrap();

        assert!(!wc.exists("old"));
        assert!(!wc.exists("old/a.txt"));
        assert!(wc.exists("new"));
        assert!(wc.exists("new/a.txt"));
        assert!(wc.exists("new/b.txt"));
    }

    #[test]
    fn test_rename_not_found() {
        let wc = Memory::new();
        let result = wc.rename("nonexistent", "new");
        assert!(matches!(result, Err(MemoryError::NotFound { .. })));
    }

    #[test]
    fn test_set_permissions() {
        let wc = Memory::new();
        wc.add_file("test.txt", b"");

        wc.set_permissions("test.txt", 0o755).unwrap();

        let meta = wc.file_metadata("test.txt").unwrap();
        assert_eq!(meta.permissions, 0o755);
        assert!(meta.is_executable());
    }

    #[test]
    fn test_set_permissions_not_found() {
        let wc = Memory::new();
        let result = wc.set_permissions("nonexistent", 0o755);
        assert!(matches!(result, Err(MemoryError::NotFound { .. })));
    }

    #[test]
    fn test_write_file() {
        let wc = Memory::new();
        let inode = wc.allocate_inode();

        {
            let mut writer = wc.write_file("test.txt", inode).unwrap();
            writer.write_all(b"hello").unwrap();
            writer.write_all(b" world").unwrap();
        }

        let contents = wc.get_file_contents("test.txt").unwrap();
        assert_eq!(contents, b"hello world");
    }

    #[test]
    fn test_write_file_creates_parent_dirs() {
        let wc = Memory::new();
        let inode = wc.allocate_inode();

        {
            let mut writer = wc.write_file("a/b/test.txt", inode).unwrap();
            writer.write_all(b"nested").unwrap();
        }

        assert!(wc.is_directory("a"));
        assert!(wc.is_directory("a/b"));
        assert_eq!(wc.get_file_contents("a/b/test.txt").unwrap(), b"nested");
    }

    #[test]
    fn test_write_file_overwrites() {
        let wc = Memory::new();
        wc.add_file("test.txt", b"old content");

        let inode = wc.allocate_inode();
        {
            let mut writer = wc.write_file("test.txt", inode).unwrap();
            writer.write_all(b"new content").unwrap();
        }

        assert_eq!(wc.get_file_contents("test.txt").unwrap(), b"new content");
    }

    // -------------------------------------------------------------------------
    // MemoryError Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_memory_error_display() {
        let err = MemoryError::NotFound {
            path: "test.txt".to_string(),
        };
        assert!(err.to_string().contains("test.txt"));
        assert!(err.to_string().contains("not found"));

        let err = MemoryError::NotADirectory {
            path: "file.txt".to_string(),
        };
        assert!(err.to_string().contains("Not a directory"));

        let err = MemoryError::IsADirectory {
            path: "dir".to_string(),
        };
        assert!(err.to_string().contains("Is a directory"));

        let err = MemoryError::PermissionDenied {
            path: "secret".to_string(),
        };
        assert!(err.to_string().contains("Permission denied"));

        let err = MemoryError::AlreadyExists {
            path: "existing".to_string(),
        };
        assert!(err.to_string().contains("Already exists"));

        let err = MemoryError::DirectoryNotEmpty {
            path: "notempty".to_string(),
        };
        assert!(err.to_string().contains("not empty"));
    }

    #[test]
    fn test_memory_error_debug() {
        let err = MemoryError::NotFound {
            path: "test".to_string(),
        };
        let debug = format!("{:?}", err);
        assert!(debug.contains("NotFound"));
    }

    // -------------------------------------------------------------------------
    // Helper Function Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_path_parent() {
        assert_eq!(path_parent("a/b/c"), Some("a/b".to_string()));
        assert_eq!(path_parent("a/b"), Some("a".to_string()));
        assert_eq!(path_parent("a"), None);
        assert_eq!(path_parent(""), None);
        assert_eq!(path_parent("a/b/"), Some("a".to_string()));
    }

    #[test]
    fn test_list_directory_contents() {
        let wc = Memory::new();
        wc.add_file("a.txt", b"");
        wc.add_file("b.txt", b"");
        wc.add_file("dir/c.txt", b"");

        let contents = wc.list_directory_contents("");
        // Contents at root: a.txt, b.txt, and the "dir" directory itself
        assert_eq!(contents.len(), 3);
        assert!(contents.contains(&"a.txt".to_string()));
        assert!(contents.contains(&"b.txt".to_string()));
        assert!(contents.contains(&"dir".to_string()));

        let dir_contents = wc.list_directory_contents("dir");
        assert_eq!(dir_contents.len(), 1);
        assert!(dir_contents.contains(&"dir/c.txt".to_string()));
    }

    // -------------------------------------------------------------------------
    // Edge Cases
    // -------------------------------------------------------------------------

    #[test]
    fn test_empty_file() {
        let wc = Memory::new();
        wc.add_file("empty.txt", b"");

        let contents = wc.get_file_contents("empty.txt").unwrap();
        assert!(contents.is_empty());
    }

    #[test]
    fn test_binary_file() {
        let wc = Memory::new();
        let binary_data: Vec<u8> = (0..=255).collect();
        wc.add_file("binary.bin", &binary_data);

        let contents = wc.get_file_contents("binary.bin").unwrap();
        assert_eq!(contents, binary_data);
    }

    #[test]
    fn test_unicode_path() {
        let wc = Memory::new();
        wc.add_file("日本語/テスト.txt", b"unicode content");

        assert!(wc.exists("日本語/テスト.txt"));
        assert!(wc.is_directory("日本語"));
    }

    #[test]
    fn test_large_file() {
        let wc = Memory::new();
        let large_data = vec![0u8; 1_000_000]; // 1MB
        wc.add_file("large.bin", &large_data);

        let contents = wc.get_file_contents("large.bin").unwrap();
        assert_eq!(contents.len(), 1_000_000);
    }

    #[test]
    fn test_deeply_nested_path() {
        let wc = Memory::new();
        let deep_path = "a/b/c/d/e/f/g/h/i/j/file.txt";
        wc.add_file(deep_path, b"deep");

        assert!(wc.exists(deep_path));
        assert!(wc.is_directory("a/b/c/d/e/f/g/h/i/j"));
    }

    #[test]
    fn test_get_contents_of_directory_returns_none() {
        let wc = Memory::new();
        wc.add_directory("mydir");

        assert!(wc.get_file_contents("mydir").is_none());
    }

    // -------------------------------------------------------------------------
    // walk_files (trait method) tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_walk_files_empty() {
        let wc = Memory::new();
        let files = wc.walk_files("").unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn test_walk_files_single_file() {
        let wc = Memory::new();
        wc.add_file("test.txt", b"content");

        let files = wc.walk_files("").unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0], "test.txt");
    }

    #[test]
    fn test_walk_files_multiple_files_sorted() {
        let wc = Memory::new();
        wc.add_file("c.txt", b"");
        wc.add_file("a.txt", b"");
        wc.add_file("b.txt", b"");

        let files = wc.walk_files("").unwrap();

        assert_eq!(files.len(), 3);
        assert_eq!(files[0], "a.txt");
        assert_eq!(files[1], "b.txt");
        assert_eq!(files[2], "c.txt");
    }

    #[test]
    fn test_walk_files_nested() {
        let wc = Memory::new();
        wc.add_file("root.txt", b"");
        wc.add_file("src/main.rs", b"");
        wc.add_file("src/lib.rs", b"");
        wc.add_file("src/utils/helper.rs", b"");

        let files = wc.walk_files("").unwrap();

        assert_eq!(files.len(), 4);
        assert!(files.contains(&"root.txt".to_string()));
        assert!(files.contains(&"src/main.rs".to_string()));
        assert!(files.contains(&"src/lib.rs".to_string()));
        assert!(files.contains(&"src/utils/helper.rs".to_string()));
    }

    #[test]
    fn test_walk_files_with_prefix() {
        let wc = Memory::new();
        wc.add_file("root.txt", b"");
        wc.add_file("src/main.rs", b"");
        wc.add_file("src/lib.rs", b"");
        wc.add_file("tests/test.rs", b"");

        let files = wc.walk_files("src").unwrap();

        assert_eq!(files.len(), 2);
        assert!(files.contains(&"src/main.rs".to_string()));
        assert!(files.contains(&"src/lib.rs".to_string()));
        assert!(!files.contains(&"root.txt".to_string()));
        assert!(!files.contains(&"tests/test.rs".to_string()));
    }

    #[test]
    fn test_walk_files_with_prefix_trailing_slash() {
        let wc = Memory::new();
        wc.add_file("src/main.rs", b"");
        wc.add_file("src/lib.rs", b"");

        let files = wc.walk_files("src/").unwrap();

        assert_eq!(files.len(), 2);
    }

    #[test]
    fn test_walk_files_excludes_atomic_dir() {
        let wc = Memory::new();
        wc.add_file("file.txt", b"");
        wc.add_file(".atomic/pristine", b"");
        wc.add_file(".atomic/config.toml", b"");

        let files = wc.walk_files("").unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0], "file.txt");
    }

    #[test]
    fn test_walk_files_excludes_directories() {
        let wc = Memory::new();
        wc.add_file("file.txt", b"");
        wc.add_directory("empty_dir");
        wc.add_directory("src");

        let files = wc.walk_files("").unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0], "file.txt");
    }

    #[test]
    fn test_walk_files_nonexistent_prefix() {
        let wc = Memory::new();
        wc.add_file("file.txt", b"");

        let files = wc.walk_files("nonexistent").unwrap();

        assert!(files.is_empty());
    }
}
