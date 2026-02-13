//! Local context for human-readable change display
//!
//! When recording and displaying changes, we need to provide context that
//! helps humans understand what's being modified. This includes:
//!
//! - File paths (where the change occurs)
//! - Line numbers (for text files)
//! - Byte positions (for binary files or precise locations)
//! - Inode references (for graph-based lookups)
//!
//! This information is NOT part of the change hash - it's metadata for
//! display purposes only. The actual graph operations use positions and
//! vertices, but humans need paths and line numbers.
//!
//! # Example
//!
//! ```rust
//! use atomic_core::change::{Local, LocalByte};
//! use atomic_core::Position;
//!
//! // Text file context
//! let local = Local::new("src/main.rs", 42);
//! assert_eq!(local.path, "src/main.rs");
//! assert_eq!(local.line, 42);
//!
//! // Byte-level context (for binary or precise positioning)
//! let local_byte = LocalByte::new("data.bin", 100, 1024);
//! assert_eq!(local_byte.byte, 1024);
//! ```

use crate::{Inode, Position};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};

/// Local context for text file changes.
///
/// This provides human-readable context for a change location:
/// - The file path where the change occurs
/// - The line number within that file
///
/// This is used for displaying diffs and change descriptions to users.
/// The actual change operations use graph positions, not line numbers.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Local {
    /// Path to the file (relative to repository root)
    pub path: String,

    /// Line number (1-indexed) where the change occurs
    ///
    /// For insertions, this is the line after which content is inserted.
    /// For deletions, this is the first line being deleted.
    pub line: u64,
}

impl Local {
    /// Create a new local context.
    ///
    /// # Arguments
    ///
    /// * `path` - The file path relative to repository root
    /// * `line` - The line number (1-indexed)
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::Local;
    ///
    /// let local = Local::new("src/lib.rs", 100);
    /// assert_eq!(local.path, "src/lib.rs");
    /// assert_eq!(local.line, 100);
    /// ```
    pub fn new(path: impl Into<String>, line: u64) -> Self {
        Self {
            path: path.into(),
            line,
        }
    }

    /// Create a local context from a Path.
    ///
    /// # Arguments
    ///
    /// * `path` - The file path
    /// * `line` - The line number (1-indexed)
    pub fn from_path(path: &Path, line: u64) -> Self {
        Self {
            path: path.to_string_lossy().into_owned(),
            line,
        }
    }

    /// Get the path as a PathBuf.
    pub fn path_buf(&self) -> PathBuf {
        PathBuf::from(&self.path)
    }

    /// Get the file name portion of the path.
    pub fn file_name(&self) -> Option<&str> {
        Path::new(&self.path).file_name()?.to_str()
    }

    /// Get the parent directory of the path.
    pub fn parent(&self) -> Option<&str> {
        Path::new(&self.path).parent()?.to_str()
    }

    /// Check if this refers to the root of a file (line 0 or 1).
    #[inline]
    pub fn is_file_start(&self) -> bool {
        self.line <= 1
    }
}

impl Default for Local {
    fn default() -> Self {
        Self {
            path: String::new(),
            line: 0,
        }
    }
}

impl fmt::Display for Local {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.path, self.line)
    }
}

impl From<(&str, u64)> for Local {
    fn from((path, line): (&str, u64)) -> Self {
        Self::new(path, line)
    }
}

impl From<(String, u64)> for Local {
    fn from((path, line): (String, u64)) -> Self {
        Self { path, line }
    }
}

/// Local context with byte-level precision.
///
/// This extends `Local` with:
/// - An inode reference for graph lookups
/// - A byte offset for precise positioning
///
/// This is used when byte-level precision is needed, such as:
/// - Binary file changes
/// - Exact cursor positioning in editors
/// - Conflict resolution at byte boundaries
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LocalByte {
    /// Path to the file (relative to repository root)
    pub path: String,

    /// Line number (1-indexed) where the change occurs
    pub line: u64,

    /// Inode of the file in the graph
    ///
    /// This allows efficient lookup of the file's position in the
    /// repository graph without path traversal.
    #[serde(default)]
    pub inode: Option<Position<Inode>>,

    /// Byte offset within the file
    ///
    /// This is the exact byte position, useful for:
    /// - Binary files where lines don't apply
    /// - Precise positioning within a line
    /// - Conflict markers at specific positions
    pub byte: u64,
}

impl LocalByte {
    /// Create a new byte-level local context.
    ///
    /// # Arguments
    ///
    /// * `path` - The file path relative to repository root
    /// * `line` - The line number (1-indexed)
    /// * `byte` - The byte offset within the file
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::LocalByte;
    ///
    /// let local = LocalByte::new("data.bin", 0, 1024);
    /// assert_eq!(local.byte, 1024);
    /// ```
    pub fn new(path: impl Into<String>, line: u64, byte: u64) -> Self {
        Self {
            path: path.into(),
            line,
            inode: None,
            byte,
        }
    }

    /// Create a byte-level context with an inode reference.
    ///
    /// # Arguments
    ///
    /// * `path` - The file path relative to repository root
    /// * `line` - The line number (1-indexed)
    /// * `inode` - The file's inode position in the graph
    /// * `byte` - The byte offset within the file
    pub fn with_inode(
        path: impl Into<String>,
        line: u64,
        inode: Position<Inode>,
        byte: u64,
    ) -> Self {
        Self {
            path: path.into(),
            line,
            inode: Some(inode),
            byte,
        }
    }

    /// Convert to a simple `Local` (dropping byte and inode info).
    pub fn to_local(&self) -> Local {
        Local {
            path: self.path.clone(),
            line: self.line,
        }
    }

    /// Get the path as a PathBuf.
    pub fn path_buf(&self) -> PathBuf {
        PathBuf::from(&self.path)
    }

    /// Check if this has an associated inode.
    #[inline]
    pub fn has_inode(&self) -> bool {
        self.inode.is_some()
    }
}

impl Default for LocalByte {
    fn default() -> Self {
        Self {
            path: String::new(),
            line: 0,
            inode: None,
            byte: 0,
        }
    }
}

impl fmt::Display for LocalByte {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}@{}", self.path, self.line, self.byte)
    }
}

impl From<Local> for LocalByte {
    /// Convert a `Local` to `LocalByte` with byte offset 0.
    fn from(local: Local) -> Self {
        Self {
            path: local.path,
            line: local.line,
            inode: None,
            byte: 0,
        }
    }
}

impl From<LocalByte> for Local {
    /// Convert a `LocalByte` to `Local` (dropping byte and inode info).
    fn from(local_byte: LocalByte) -> Self {
        local_byte.to_local()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ChangePosition;

    // ========================================================================
    // Local Tests
    // ========================================================================

    #[test]
    fn test_local_new() {
        let local = Local::new("src/main.rs", 42);
        assert_eq!(local.path, "src/main.rs");
        assert_eq!(local.line, 42);
    }

    #[test]
    fn test_local_from_path() {
        let path = Path::new("src/lib.rs");
        let local = Local::from_path(path, 100);
        assert_eq!(local.path, "src/lib.rs");
        assert_eq!(local.line, 100);
    }

    #[test]
    fn test_local_path_buf() {
        let local = Local::new("src/main.rs", 1);
        let path_buf = local.path_buf();
        assert_eq!(path_buf, PathBuf::from("src/main.rs"));
    }

    #[test]
    fn test_local_file_name() {
        let local = Local::new("src/lib/module.rs", 1);
        assert_eq!(local.file_name(), Some("module.rs"));
    }

    #[test]
    fn test_local_parent() {
        let local = Local::new("src/lib/module.rs", 1);
        assert_eq!(local.parent(), Some("src/lib"));
    }

    #[test]
    fn test_local_is_file_start() {
        assert!(Local::new("test.rs", 0).is_file_start());
        assert!(Local::new("test.rs", 1).is_file_start());
        assert!(!Local::new("test.rs", 2).is_file_start());
    }

    #[test]
    fn test_local_default() {
        let local = Local::default();
        assert!(local.path.is_empty());
        assert_eq!(local.line, 0);
    }

    #[test]
    fn test_local_display() {
        let local = Local::new("src/main.rs", 42);
        assert_eq!(format!("{}", local), "src/main.rs:42");
    }

    #[test]
    fn test_local_from_tuple() {
        let local: Local = ("test.rs", 10).into();
        assert_eq!(local.path, "test.rs");
        assert_eq!(local.line, 10);

        let local: Local = (String::from("test.rs"), 20).into();
        assert_eq!(local.path, "test.rs");
        assert_eq!(local.line, 20);
    }

    #[test]
    fn test_local_equality() {
        let l1 = Local::new("test.rs", 10);
        let l2 = Local::new("test.rs", 10);
        let l3 = Local::new("test.rs", 20);

        assert_eq!(l1, l2);
        assert_ne!(l1, l3);
    }

    #[test]
    fn test_local_json_roundtrip() {
        let local = Local::new("src/main.rs", 42);
        let json = serde_json::to_string(&local).unwrap();
        let parsed: Local = serde_json::from_str(&json).unwrap();
        assert_eq!(local, parsed);
    }

    #[test]
    fn test_local_json_full_roundtrip() {
        let local = Local::new("path/to/deep/file.txt", 999);
        let json = serde_json::to_string(&local).unwrap();
        let parsed: Local = serde_json::from_str(&json).unwrap();
        assert_eq!(local, parsed);
    }

    // ========================================================================
    // LocalByte Tests
    // ========================================================================

    #[test]
    fn test_local_byte_new() {
        let local = LocalByte::new("data.bin", 0, 1024);
        assert_eq!(local.path, "data.bin");
        assert_eq!(local.line, 0);
        assert_eq!(local.byte, 1024);
        assert!(local.inode.is_none());
    }

    #[test]
    fn test_local_byte_with_inode() {
        let inode_pos = Position::new(Inode::new(42), ChangePosition::new(0));
        let local = LocalByte::with_inode("test.rs", 10, inode_pos, 500);

        assert!(local.has_inode());
        assert_eq!(local.inode.unwrap().change.get(), 42);
    }

    #[test]
    fn test_local_byte_to_local() {
        let local_byte = LocalByte::new("test.rs", 10, 500);
        let local = local_byte.to_local();

        assert_eq!(local.path, "test.rs");
        assert_eq!(local.line, 10);
    }

    #[test]
    fn test_local_byte_path_buf() {
        let local = LocalByte::new("src/main.rs", 1, 0);
        let path_buf = local.path_buf();
        assert_eq!(path_buf, PathBuf::from("src/main.rs"));
    }

    #[test]
    fn test_local_byte_default() {
        let local = LocalByte::default();
        assert!(local.path.is_empty());
        assert_eq!(local.line, 0);
        assert_eq!(local.byte, 0);
        assert!(local.inode.is_none());
    }

    #[test]
    fn test_local_byte_display() {
        let local = LocalByte::new("data.bin", 5, 1024);
        assert_eq!(format!("{}", local), "data.bin:5@1024");
    }

    #[test]
    fn test_local_byte_from_local() {
        let local = Local::new("test.rs", 42);
        let local_byte: LocalByte = local.into();

        assert_eq!(local_byte.path, "test.rs");
        assert_eq!(local_byte.line, 42);
        assert_eq!(local_byte.byte, 0);
    }

    #[test]
    fn test_local_from_local_byte() {
        let local_byte = LocalByte::new("test.rs", 42, 1000);
        let local: Local = local_byte.into();

        assert_eq!(local.path, "test.rs");
        assert_eq!(local.line, 42);
    }

    #[test]
    fn test_local_byte_json_roundtrip() {
        let local = LocalByte::new("data.bin", 10, 2048);
        let json = serde_json::to_string(&local).unwrap();
        let parsed: LocalByte = serde_json::from_str(&json).unwrap();
        assert_eq!(local, parsed);
    }

    #[test]
    fn test_local_byte_json_with_inode() {
        let inode_pos = Position::new(Inode::new(42), ChangePosition::new(100));
        let local = LocalByte::with_inode("test.rs", 10, inode_pos, 500);

        let json = serde_json::to_string(&local).unwrap();
        let parsed: LocalByte = serde_json::from_str(&json).unwrap();

        assert_eq!(local.path, parsed.path);
        assert_eq!(local.byte, parsed.byte);
        assert!(parsed.has_inode());
    }

    #[test]
    fn test_local_byte_json_full_roundtrip() {
        let inode_pos = Position::new(Inode::new(42), ChangePosition::new(100));
        let local = LocalByte::with_inode("binary.dat", 5, inode_pos, 4096);
        let json = serde_json::to_string(&local).unwrap();
        let parsed: LocalByte = serde_json::from_str(&json).unwrap();
        assert_eq!(local, parsed);
    }

    // ========================================================================
    // Edge Cases
    // ========================================================================

    #[test]
    fn test_empty_path() {
        let local = Local::new("", 0);
        assert!(local.path.is_empty());
        assert!(local.file_name().is_none());
    }

    #[test]
    fn test_root_file() {
        let local = Local::new("Cargo.toml", 1);
        assert_eq!(local.file_name(), Some("Cargo.toml"));
        // Parent of a root-level file
        assert_eq!(local.parent(), Some(""));
    }

    #[test]
    fn test_deep_path() {
        let local = Local::new("a/b/c/d/e/f.txt", 1);
        assert_eq!(local.file_name(), Some("f.txt"));
        assert_eq!(local.parent(), Some("a/b/c/d/e"));
    }

    #[test]
    fn test_max_values() {
        let local = Local::new("test.rs", u64::MAX);
        assert_eq!(local.line, u64::MAX);

        let local_byte = LocalByte::new("test.bin", u64::MAX, u64::MAX);
        assert_eq!(local_byte.line, u64::MAX);
        assert_eq!(local_byte.byte, u64::MAX);
    }
}
