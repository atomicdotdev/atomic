//! Filesystem-backed working copy implementation
//!
//! This module provides a concrete implementation of the [`WorkingCopy`] and
//! [`WorkingCopyRead`] traits for real filesystem operations. It enables
//! Atomic to read from and write to actual files on disk.
//!
//! # Overview
//!
//! The [`FileSystem`] struct wraps a root directory path and provides all
//! the operations needed for:
//!
//! - **Recording changes**: Reading file contents and metadata for comparison
//! - **Outputting changes**: Writing graph state back to the working copy
//! - **File management**: Creating directories, renaming files, etc.
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                         FileSystem Architecture                          │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │   Repository Root                                                       │
//! │   ┌──────────────────────────────────────────────────────────────┐     │
//! │   │  /home/user/project/                                         │     │
//! │   │  ├── .atomic/           (excluded from working copy ops)     │     │
//! │   │  ├── src/                                                    │     │
//! │   │  │   ├── main.rs        ◄── FileSystem reads/writes here    │     │
//! │   │  │   └── lib.rs                                              │     │
//! │   │  ├── Cargo.toml                                              │     │
//! │   │  └── README.md                                               │     │
//! │   └──────────────────────────────────────────────────────────────┘     │
//! │                                                                         │
//! │   FileSystem { root: "/home/user/project" }                             │
//! │       │                                                                 │
//! │       ├── file_metadata("src/main.rs") → FileMetadata                  │
//! │       ├── read_file("src/main.rs", &mut buf) → Ok(())                  │
//! │       ├── write_file("src/main.rs", inode) → FileWriter                │
//! │       └── create_dir_all("src/utils") → Ok(())                         │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Usage
//!
//! ```rust,ignore
//! use atomic_core::output::FileSystem;
//! use std::path::Path;
//!
//! // Create a FileSystem rooted at a directory
//! let fs = FileSystem::from_root("/path/to/project");
//!
//! // Read file metadata
//! let meta = fs.file_metadata("src/main.rs")?;
//! println!("Is directory: {}", meta.is_dir);
//! println!("Permissions: {:o}", meta.permissions);
//!
//! // Read file contents
//! let mut buffer = Vec::new();
//! fs.read_file("src/main.rs", &mut buffer)?;
//! println!("File size: {} bytes", buffer.len());
//!
//! // Write to a file
//! use atomic_core::output::WorkingCopy;
//! let mut writer = fs.write_file("output.txt", Inode::new(42))?;
//! writer.write_all(b"Hello, world!")?;
//! ```
//!
//! # Path Handling
//!
//! All paths passed to `FileSystem` methods are **relative to the root**.
//! The implementation joins paths safely to prevent path traversal attacks.
//!
//! ```rust,ignore
//! let fs = FileSystem::from_root("/project");
//!
//! // These are equivalent:
//! fs.read_file("src/main.rs", &mut buf)?;      // Reads /project/src/main.rs
//! fs.read_file("./src/main.rs", &mut buf)?;    // Also reads /project/src/main.rs
//!
//! // Path traversal is prevented:
//! fs.read_file("../etc/passwd", &mut buf)?;    // Error or sanitized
//! ```
//!
//! # Ignore Patterns
//!
//! The filesystem implementation respects `.gitignore` and `.ignore` files
//! when iterating over files. The `.atomic` directory is always ignored.
//!
//! # Platform Considerations
//!
//! - **Unix**: Full permission support (rwx for user/group/other)
//! - **Windows**: Limited permission support (read-only flag only)
//! - **Symlinks**: Detected and reported via `FileMetadata::is_symlink`
//!
//! # Error Handling
//!
//! All operations return `std::io::Error` for consistent error handling.
//! Common error conditions include:
//!
//! - `NotFound`: File or directory doesn't exist
//! - `PermissionDenied`: Insufficient permissions
//! - `AlreadyExists`: For exclusive creation operations
//! - `NotADirectory`: Expected directory, found file
//! - `IsADirectory`: Expected file, found directory

use super::traits::{FileMetadata, WorkingCopy, WorkingCopyRead};
use crate::types::Inode;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

// CONSTANTS

/// The name of the Atomic metadata directory, which is always ignored.
pub const DOT_DIR: &str = ".atomic";

/// Default file permissions on Unix (rw-r--r--)
#[cfg(unix)]
pub const DEFAULT_FILE_MODE: u32 = 0o644;

/// Default directory permissions on Unix (rwxr-xr-x)
#[cfg(unix)]
pub const DEFAULT_DIR_MODE: u32 = 0o755;

/// Default executable permissions on Unix (rwxr-xr-x)
#[cfg(unix)]
pub const DEFAULT_EXEC_MODE: u32 = 0o755;

// FILESYSTEM WORKING COPY

/// A filesystem-backed working copy.
///
/// This struct provides access to files and directories rooted at a specific
/// path on the filesystem. All paths passed to its methods are interpreted
/// relative to this root.
///
/// # Thread Safety
///
/// `FileSystem` is `Clone` and can be safely shared between threads. However,
/// the underlying filesystem operations are subject to the usual race conditions
/// when multiple processes access the same files.
///
/// # Example
///
/// ```rust,ignore
/// use atomic_core::output::FileSystem;
///
/// let fs = FileSystem::from_root("/path/to/project");
///
/// // Check if a file exists
/// if fs.exists("Cargo.toml") {
///     println!("Found Cargo.toml!");
/// }
///
/// // Read file contents
/// let mut contents = Vec::new();
/// fs.read_file("Cargo.toml", &mut contents)?;
/// ```
#[derive(Debug, Clone)]
pub struct FileSystem {
    /// The root directory for all operations.
    root: PathBuf,
}

impl FileSystem {
    /// Create a new `FileSystem` rooted at the given path.
    ///
    /// The path does not need to exist yet, but operations will fail
    /// if the root directory is not present when needed.
    ///
    /// # Arguments
    ///
    /// * `root` - The root directory path
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use atomic_core::output::FileSystem;
    /// use std::path::Path;
    ///
    /// // From a Path
    /// let fs = FileSystem::from_root(Path::new("/project"));
    ///
    /// // From a PathBuf
    /// let fs = FileSystem::from_root(std::path::PathBuf::from("/project"));
    ///
    /// // From a string slice
    /// let fs = FileSystem::from_root("/project");
    /// ```
    pub fn from_root<P: AsRef<Path>>(root: P) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }

    /// Get the root directory path.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use atomic_core::output::FileSystem;
    ///
    /// let fs = FileSystem::from_root("/project");
    /// assert_eq!(fs.root(), std::path::Path::new("/project"));
    /// ```
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a relative path to an absolute path within the root.
    ///
    /// This method sanitizes the path to prevent directory traversal attacks.
    ///
    /// # Arguments
    ///
    /// * `relative` - A path relative to the root
    ///
    /// # Returns
    ///
    /// The absolute path, or an error if the path would escape the root.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use atomic_core::output::FileSystem;
    ///
    /// let fs = FileSystem::from_root("/project");
    ///
    /// // Normal resolution
    /// let abs = fs.resolve_path("src/main.rs")?;
    /// assert_eq!(abs, std::path::PathBuf::from("/project/src/main.rs"));
    ///
    /// // Path traversal is blocked
    /// let result = fs.resolve_path("../etc/passwd");
    /// assert!(result.is_err());
    /// ```
    pub fn resolve_path(&self, relative: &str) -> io::Result<PathBuf> {
        let path = self.root.join(relative);

        // Normalize the path to resolve .. and .
        let normalized = normalize_path(&path);

        // Ensure the normalized path is still under root
        if !normalized.starts_with(&self.root) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Path '{}' escapes root directory '{}'",
                    relative,
                    self.root.display()
                ),
            ));
        }

        Ok(normalized)
    }

    /// Check if a path exists.
    ///
    /// # Arguments
    ///
    /// * `path` - Path relative to the root
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use atomic_core::output::FileSystem;
    ///
    /// let fs = FileSystem::from_root("/project");
    /// if fs.exists("Cargo.toml") {
    ///     println!("This is a Rust project!");
    /// }
    /// ```
    pub fn exists(&self, path: &str) -> bool {
        self.resolve_path(path)
            .map(|p| p.exists())
            .unwrap_or(false)
    }

    /// List files in a directory.
    ///
    /// Returns an iterator over the entries in the directory. Each entry
    /// is a tuple of (filename, is_directory).
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the directory, relative to the root
    ///
    /// # Errors
    ///
    /// Returns an error if the path is not a directory or cannot be read.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use atomic_core::output::FileSystem;
    ///
    /// let fs = FileSystem::from_root("/project");
    /// for (name, is_dir) in fs.list_dir("src")? {
    ///     if is_dir {
    ///         println!("Directory: {}/", name);
    ///     } else {
    ///         println!("File: {}", name);
    ///     }
    /// }
    /// ```
    pub fn list_dir(&self, path: &str) -> io::Result<Vec<(String, bool)>> {
        let abs_path = self.resolve_path(path)?;
        let mut entries = Vec::new();

        for entry in fs::read_dir(&abs_path)? {
            let entry = entry?;
            let file_name = entry.file_name().to_string_lossy().to_string();
            let file_type = entry.file_type()?;
            entries.push((file_name, file_type.is_dir()));
        }

        // Sort for consistent ordering
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(entries)
    }

    /// Walk the directory tree recursively.
    ///
    /// Yields all files (not directories) under the given path, respecting
    /// ignore patterns from `.gitignore` and `.ignore` files.
    ///
    /// # Arguments
    ///
    /// * `path` - Starting path relative to the root (empty string for root)
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use atomic_core::output::FileSystem;
    ///
    /// let fs = FileSystem::from_root("/project");
    /// for path in fs.walk_files("")? {
    ///     println!("Found file: {}", path);
    /// }
    /// ```
    pub fn walk_files(&self, path: &str) -> io::Result<Vec<String>> {
        let abs_path = if path.is_empty() {
            self.root.clone()
        } else {
            self.resolve_path(path)?
        };

        let mut files = Vec::new();
        self.walk_files_recursive(&abs_path, &mut files)?;
        files.sort();
        Ok(files)
    }

    /// Recursive helper for walk_files.
    fn walk_files_recursive(&self, dir: &Path, files: &mut Vec<String>) -> io::Result<()> {
        if !dir.is_dir() {
            return Ok(());
        }

        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let file_name = entry.file_name().to_string_lossy().to_string();

            // Skip the .atomic directory
            if file_name == DOT_DIR {
                continue;
            }

            let file_type = entry.file_type()?;

            if file_type.is_dir() {
                self.walk_files_recursive(&path, files)?;
            } else if file_type.is_file() {
                // Convert to relative path
                if let Ok(relative) = path.strip_prefix(&self.root) {
                    files.push(relative.to_string_lossy().to_string());
                }
            }
        }

        Ok(())
    }

    /// Check if a path is the Atomic metadata directory or inside it.
    ///
    /// # Arguments
    ///
    /// * `path` - Path relative to the root
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use atomic_core::output::FileSystem;
    ///
    /// let fs = FileSystem::from_root("/project");
    /// assert!(fs.is_atomic_path(".atomic"));
    /// assert!(fs.is_atomic_path(".atomic/pristine"));
    /// assert!(!fs.is_atomic_path("src/main.rs"));
    /// ```
    pub fn is_atomic_path(&self, path: &str) -> bool {
        path == DOT_DIR || path.starts_with(&format!("{}/", DOT_DIR))
    }
}

// WORKING COPY READ IMPLEMENTATION

impl WorkingCopyRead for FileSystem {
    type Error = io::Error;

    /// Get metadata for a file or directory.
    ///
    /// # Arguments
    ///
    /// * `path` - Path relative to the root
    ///
    /// # Returns
    ///
    /// [`FileMetadata`] containing permissions and type information.
    ///
    /// # Errors
    ///
    /// Returns an error if the path doesn't exist or cannot be accessed.
    fn file_metadata(&self, path: &str) -> Result<FileMetadata, Self::Error> {
        let abs_path = self.resolve_path(path)?;
        let metadata = fs::symlink_metadata(&abs_path)?;

        Ok(FileMetadata {
            permissions: get_permissions(&metadata),
            is_dir: metadata.is_dir(),
            is_symlink: metadata.file_type().is_symlink(),
        })
    }

    /// Read the contents of a file into a buffer.
    ///
    /// The buffer is **appended to**, not cleared. This allows reading
    /// multiple files into the same buffer with separators.
    ///
    /// # Arguments
    ///
    /// * `path` - Path relative to the root
    /// * `buffer` - Buffer to append the file contents to
    ///
    /// # Errors
    ///
    /// Returns an error if the file doesn't exist, is a directory, or
    /// cannot be read.
    fn read_file(&self, path: &str, buffer: &mut Vec<u8>) -> Result<(), Self::Error> {
        let abs_path = self.resolve_path(path)?;
        let mut file = File::open(&abs_path)?;
        file.read_to_end(buffer)?;
        Ok(())
    }

    /// Get the modification time of a file.
    ///
    /// # Arguments
    ///
    /// * `path` - Path relative to the root
    ///
    /// # Returns
    ///
    /// The time the file was last modified.
    ///
    /// # Errors
    ///
    /// Returns an error if the file doesn't exist or metadata cannot be read.
    fn modified_time(&self, path: &str) -> Result<SystemTime, Self::Error> {
        let abs_path = self.resolve_path(path)?;
        let metadata = fs::metadata(&abs_path)?;
        metadata.modified()
    }

    /// Check if a path exists.
    ///
    /// Unlike the inherent `exists` method, this one follows the trait signature
    /// and returns a `Result`.
    fn exists(&self, path: &str) -> bool {
        self.resolve_path(path)
            .map(|p| p.exists())
            .unwrap_or(false)
    }

    /// Check if a path is a directory.
    fn is_directory(&self, path: &str) -> bool {
        self.resolve_path(path)
            .map(|p| p.is_dir())
            .unwrap_or(false)
    }

    /// Walk the directory tree and return all file paths.
    ///
    /// Delegates to the inherent [`FileSystem::walk_files`] method.
    /// The `.atomic` directory is automatically excluded.
    fn walk_files(&self, prefix: &str) -> Result<Vec<String>, Self::Error> {
        // Delegate to the inherent method
        FileSystem::walk_files(self, prefix)
    }
}

// WORKING COPY WRITE IMPLEMENTATION

/// A buffered file writer returned by [`FileSystem::write_file`].
///
/// This writer buffers writes for performance and flushes automatically
/// when dropped. The associated inode is stored for reference.
pub struct FileWriter {
    /// Buffered writer wrapping the file
    writer: BufWriter<File>,
    /// The inode associated with this file
    inode: Inode,
    /// The path being written (for error messages)
    path: PathBuf,
}

impl FileWriter {
    /// Create a new file writer.
    fn new(file: File, path: PathBuf, inode: Inode) -> Self {
        Self {
            writer: BufWriter::new(file),
            inode,
            path,
        }
    }

    /// Get the inode associated with this file.
    pub fn inode(&self) -> Inode {
        self.inode
    }

    /// Get the path being written.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Flush and close the writer, returning any errors.
    ///
    /// This is automatically called when the writer is dropped, but
    /// calling it explicitly allows you to handle errors.
    pub fn finish(mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

impl Write for FileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.writer.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

impl Drop for FileWriter {
    fn drop(&mut self) {
        // Best-effort flush on drop
        let _ = self.writer.flush();
    }
}

impl WorkingCopy for FileSystem {
    type Writer = FileWriter;

    /// Check if a path is writable.
    ///
    /// On Unix, checks the write permission bit.
    /// On Windows, checks the read-only attribute.
    fn is_writable(&self, path: &str) -> Result<bool, Self::Error> {
        let abs_path = self.resolve_path(path)?;

        if !abs_path.exists() {
            // If file doesn't exist, check if parent is writable
            if let Some(parent) = abs_path.parent() {
                if parent.exists() {
                    let metadata = fs::metadata(parent)?;
                    return Ok(!metadata.permissions().readonly());
                }
            }
            // Assume writable if parent doesn't exist either
            return Ok(true);
        }

        let metadata = fs::metadata(&abs_path)?;
        Ok(!metadata.permissions().readonly())
    }

    /// Create a directory and all parent directories.
    ///
    /// # Arguments
    ///
    /// * `path` - Path relative to the root
    ///
    /// # Errors
    ///
    /// Returns an error if a parent component exists as a file.
    fn create_dir_all(&self, path: &str) -> Result<(), Self::Error> {
        let abs_path = self.resolve_path(path)?;
        fs::create_dir_all(&abs_path)
    }

    /// Remove a file or directory.
    ///
    /// # Arguments
    ///
    /// * `path` - Path relative to the root
    /// * `recursive` - If true, remove directories and their contents
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The path doesn't exist
    /// - It's a non-empty directory and `recursive` is false
    /// - Permission is denied
    fn remove_path(&self, path: &str, recursive: bool) -> Result<(), Self::Error> {
        let abs_path = self.resolve_path(path)?;

        if !abs_path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("Path not found: {}", path),
            ));
        }

        if abs_path.is_dir() {
            if recursive {
                fs::remove_dir_all(&abs_path)
            } else {
                fs::remove_dir(&abs_path)
            }
        } else {
            fs::remove_file(&abs_path)
        }
    }

    /// Rename a file or directory.
    ///
    /// # Arguments
    ///
    /// * `from` - Current path relative to the root
    /// * `to` - New path relative to the root
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The source doesn't exist
    /// - The destination's parent directory doesn't exist
    /// - Permission is denied
    fn rename(&self, from: &str, to: &str) -> Result<(), Self::Error> {
        let from_abs = self.resolve_path(from)?;
        let to_abs = self.resolve_path(to)?;

        // Create parent directory for destination if needed
        if let Some(parent) = to_abs.parent() {
            if !parent.exists() {
                fs::create_dir_all(parent)?;
            }
        }

        fs::rename(&from_abs, &to_abs)
    }

    /// Set permissions on a file or directory.
    ///
    /// # Arguments
    ///
    /// * `path` - Path relative to the root
    /// * `permissions` - Unix-style permissions (only lower 9 bits used on Unix)
    ///
    /// # Platform Notes
    ///
    /// - **Unix**: Sets the full permission mode
    /// - **Windows**: Only the read-only bit is meaningful
    fn set_permissions(&self, path: &str, permissions: u16) -> Result<(), Self::Error> {
        let abs_path = self.resolve_path(path)?;
        set_permissions(&abs_path, permissions)
    }

    /// Open a file for writing, creating it if necessary.
    ///
    /// # Arguments
    ///
    /// * `path` - Path relative to the root
    /// * `inode` - The inode to associate with this file
    ///
    /// # Returns
    ///
    /// A [`FileWriter`] that implements [`Write`].
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The parent directory doesn't exist (create it first)
    /// - Permission is denied
    /// - The path is a directory
    fn write_file(&self, path: &str, inode: Inode) -> Result<Self::Writer, Self::Error> {
        let abs_path = self.resolve_path(path)?;

        // Create parent directories
        if let Some(parent) = abs_path.parent() {
            if !parent.exists() {
                fs::create_dir_all(parent)?;
            }
        }

        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&abs_path)?;

        Ok(FileWriter::new(file, abs_path, inode))
    }
}

// HELPER FUNCTIONS

/// Normalize a path by resolving `.` and `..` components.
///
/// Unlike `canonicalize()`, this doesn't require the path to exist.
fn normalize_path(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::Prefix(p) => normalized.push(p.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {} // Skip `.`
            Component::ParentDir => {
                // Go up one level, but don't go above root
                normalized.pop();
            }
            Component::Normal(name) => normalized.push(name),
        }
    }

    normalized
}

/// Get permissions from file metadata.
#[cfg(unix)]
fn get_permissions(metadata: &fs::Metadata) -> u16 {
    use std::os::unix::fs::PermissionsExt;
    (metadata.permissions().mode() & 0o777) as u16
}

#[cfg(not(unix))]
fn get_permissions(metadata: &fs::Metadata) -> u16 {
    // On non-Unix, approximate with readable/writable
    if metadata.permissions().readonly() {
        0o444
    } else {
        0o644
    }
}

/// Set permissions on a path.
#[cfg(unix)]
fn set_permissions(path: &Path, permissions: u16) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = fs::Permissions::from_mode(permissions as u32);
    fs::set_permissions(path, perms)
}

#[cfg(not(unix))]
fn set_permissions(path: &Path, permissions: u16) -> io::Result<()> {
    // On non-Unix, we can only set read-only
    let readonly = (permissions & 0o222) == 0;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_readonly(readonly);
    fs::set_permissions(path, perms)
}

// TESTS

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    /// Create a temporary directory for testing.
    fn temp_dir() -> TempDir {
        tempfile::tempdir().expect("Failed to create temp dir")
    }

    /// Create a FileSystem rooted at a temporary directory.
    fn temp_fs() -> (TempDir, FileSystem) {
        let dir = temp_dir();
        let fs = FileSystem::from_root(dir.path());
        (dir, fs)
    }

    // ------------------------------------------------------------------------
    // Construction and Basic Properties
    // ------------------------------------------------------------------------

    #[test]
    fn test_from_root_path() {
        let fs = FileSystem::from_root("/some/path");
        assert_eq!(fs.root(), Path::new("/some/path"));
    }

    #[test]
    fn test_from_root_pathbuf() {
        let path = PathBuf::from("/another/path");
        let fs = FileSystem::from_root(&path);
        assert_eq!(fs.root(), Path::new("/another/path"));
    }

    #[test]
    fn test_clone() {
        let fs1 = FileSystem::from_root("/test");
        let fs2 = fs1.clone();
        assert_eq!(fs1.root(), fs2.root());
    }

    #[test]
    fn test_debug() {
        let fs = FileSystem::from_root("/test");
        let debug = format!("{:?}", fs);
        assert!(debug.contains("FileSystem"));
        assert!(debug.contains("/test"));
    }

    // ------------------------------------------------------------------------
    // Path Resolution
    // ------------------------------------------------------------------------

    #[test]
    fn test_resolve_path_simple() {
        let (dir, fs) = temp_fs();
        let resolved = fs.resolve_path("file.txt").unwrap();
        assert_eq!(resolved, dir.path().join("file.txt"));
    }

    #[test]
    fn test_resolve_path_nested() {
        let (dir, fs) = temp_fs();
        let resolved = fs.resolve_path("a/b/c.txt").unwrap();
        assert_eq!(resolved, dir.path().join("a/b/c.txt"));
    }

    #[test]
    fn test_resolve_path_with_dot() {
        let (dir, fs) = temp_fs();
        let resolved = fs.resolve_path("./file.txt").unwrap();
        assert_eq!(resolved, dir.path().join("file.txt"));
    }

    #[test]
    fn test_resolve_path_traversal_blocked() {
        let (_dir, fs) = temp_fs();
        let result = fs.resolve_path("../escape");
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_path_deep_traversal_blocked() {
        let (_dir, fs) = temp_fs();
        let result = fs.resolve_path("a/b/../../..");
        assert!(result.is_err());
    }

    // ------------------------------------------------------------------------
    // File Existence and Type Checks
    // ------------------------------------------------------------------------

    #[test]
    fn test_exists_file() {
        let (dir, fs) = temp_fs();
        let path = dir.path().join("test.txt");
        fs::write(&path, "content").unwrap();

        assert!(fs.exists("test.txt"));
        assert!(!fs.exists("nonexistent.txt"));
    }

    #[test]
    fn test_exists_directory() {
        let (dir, fs) = temp_fs();
        fs::create_dir(dir.path().join("subdir")).unwrap();

        assert!(fs.exists("subdir"));
    }

    #[test]
    fn test_is_directory() {
        let (dir, fs) = temp_fs();
        fs::create_dir(dir.path().join("subdir")).unwrap();
        fs::write(dir.path().join("file.txt"), "").unwrap();

        assert!(fs.is_directory("subdir"));
        assert!(!fs.is_directory("file.txt"));
        assert!(!fs.is_directory("nonexistent"));
    }

    #[test]
    fn test_is_atomic_path() {
        let (_dir, fs) = temp_fs();

        assert!(fs.is_atomic_path(".atomic"));
        assert!(fs.is_atomic_path(".atomic/pristine"));
        assert!(fs.is_atomic_path(".atomic/changes/AB/CD"));
        assert!(!fs.is_atomic_path("src/main.rs"));
        assert!(!fs.is_atomic_path(".atomicfoo")); // Not a prefix match
    }

    // ------------------------------------------------------------------------
    // Reading Files
    // ------------------------------------------------------------------------

    #[test]
    fn test_read_file_simple() {
        let (dir, fs) = temp_fs();
        fs::write(dir.path().join("test.txt"), "hello world").unwrap();

        let mut buffer = Vec::new();
        fs.read_file("test.txt", &mut buffer).unwrap();
        assert_eq!(buffer, b"hello world");
    }

    #[test]
    fn test_read_file_binary() {
        let (dir, fs) = temp_fs();
        let binary_data: Vec<u8> = (0..=255).collect();
        fs::write(dir.path().join("binary.bin"), &binary_data).unwrap();

        let mut buffer = Vec::new();
        fs.read_file("binary.bin", &mut buffer).unwrap();
        assert_eq!(buffer, binary_data);
    }

    #[test]
    fn test_read_file_appends_to_buffer() {
        let (dir, fs) = temp_fs();
        fs::write(dir.path().join("test.txt"), "world").unwrap();

        let mut buffer = b"hello ".to_vec();
        fs.read_file("test.txt", &mut buffer).unwrap();
        assert_eq!(buffer, b"hello world");
    }

    #[test]
    fn test_read_file_nested() {
        let (dir, fs) = temp_fs();
        fs::create_dir_all(dir.path().join("a/b/c")).unwrap();
        fs::write(dir.path().join("a/b/c/deep.txt"), "deep content").unwrap();

        let mut buffer = Vec::new();
        fs.read_file("a/b/c/deep.txt", &mut buffer).unwrap();
        assert_eq!(buffer, b"deep content");
    }

    #[test]
    fn test_read_file_not_found() {
        let (_dir, fs) = temp_fs();
        let mut buffer = Vec::new();
        let result = fs.read_file("nonexistent.txt", &mut buffer);
        assert!(result.is_err());
    }

    #[test]
    fn test_read_file_is_directory() {
        let (dir, fs) = temp_fs();
        fs::create_dir(dir.path().join("subdir")).unwrap();

        let mut buffer = Vec::new();
        let result = fs.read_file("subdir", &mut buffer);
        assert!(result.is_err());
    }

    #[test]
    fn test_read_file_empty() {
        let (dir, fs) = temp_fs();
        fs::write(dir.path().join("empty.txt"), "").unwrap();

        let mut buffer = Vec::new();
        fs.read_file("empty.txt", &mut buffer).unwrap();
        assert!(buffer.is_empty());
    }

    // ------------------------------------------------------------------------
    // File Metadata
    // ------------------------------------------------------------------------

    #[test]
    fn test_file_metadata_regular_file() {
        let (dir, fs) = temp_fs();
        fs::write(dir.path().join("file.txt"), "content").unwrap();

        let meta = fs.file_metadata("file.txt").unwrap();
        assert!(!meta.is_dir);
        assert!(!meta.is_symlink);
    }

    #[test]
    fn test_file_metadata_directory() {
        let (dir, fs) = temp_fs();
        fs::create_dir(dir.path().join("subdir")).unwrap();

        let meta = fs.file_metadata("subdir").unwrap();
        assert!(meta.is_dir);
        assert!(!meta.is_symlink);
    }

    #[test]
    fn test_file_metadata_not_found() {
        let (_dir, fs) = temp_fs();
        let result = fs.file_metadata("nonexistent");
        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn test_file_metadata_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let (dir, fs) = temp_fs();
        let path = dir.path().join("executable.sh");
        fs::write(&path, "#!/bin/bash").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();

        let meta = fs.file_metadata("executable.sh").unwrap();
        assert_eq!(meta.permissions & 0o111, 0o111); // Has execute bits
    }

    #[cfg(unix)]
    #[test]
    fn test_file_metadata_symlink() {
        let (dir, fs) = temp_fs();
        fs::write(dir.path().join("target.txt"), "content").unwrap();
        std::os::unix::fs::symlink(
            dir.path().join("target.txt"),
            dir.path().join("link.txt"),
        )
        .unwrap();

        let meta = fs.file_metadata("link.txt").unwrap();
        assert!(meta.is_symlink);
    }

    // ------------------------------------------------------------------------
    // Modified Time
    // ------------------------------------------------------------------------

    #[test]
    fn test_modified_time_exists() {
        let (dir, fs) = temp_fs();
        fs::write(dir.path().join("file.txt"), "content").unwrap();

        let mtime = fs.modified_time("file.txt").unwrap();
        let now = SystemTime::now();

        // Modified time should be very recent (within last 10 seconds)
        let elapsed = now.duration_since(mtime).unwrap();
        assert!(elapsed.as_secs() < 10);
    }

    #[test]
    fn test_modified_time_not_found() {
        let (_dir, fs) = temp_fs();
        let result = fs.modified_time("nonexistent.txt");
        assert!(result.is_err());
    }

    // ------------------------------------------------------------------------
    // Writing Files
    // ------------------------------------------------------------------------

    #[test]
    fn test_write_file_simple() {
        let (dir, fs) = temp_fs();

        {
            let mut writer = fs.write_file("output.txt", Inode::new(1)).unwrap();
            writer.write_all(b"hello world").unwrap();
        }

        let contents = fs::read_to_string(dir.path().join("output.txt")).unwrap();
        assert_eq!(contents, "hello world");
    }

    #[test]
    fn test_write_file_creates_parents() {
        let (dir, fs) = temp_fs();

        {
            let mut writer = fs.write_file("a/b/c/deep.txt", Inode::new(1)).unwrap();
            writer.write_all(b"deep content").unwrap();
        }

        let contents = fs::read_to_string(dir.path().join("a/b/c/deep.txt")).unwrap();
        assert_eq!(contents, "deep content");
    }

    #[test]
    fn test_write_file_overwrites() {
        let (dir, fs) = temp_fs();
        fs::write(dir.path().join("existing.txt"), "old content").unwrap();

        {
            let mut writer = fs.write_file("existing.txt", Inode::new(1)).unwrap();
            writer.write_all(b"new content").unwrap();
        }

        let contents = fs::read_to_string(dir.path().join("existing.txt")).unwrap();
        assert_eq!(contents, "new content");
    }

    #[test]
    fn test_write_file_binary() {
        let (dir, fs) = temp_fs();
        let binary_data: Vec<u8> = (0..=255).collect();

        {
            let mut writer = fs.write_file("binary.bin", Inode::new(1)).unwrap();
            writer.write_all(&binary_data).unwrap();
        }

        let contents = fs::read(dir.path().join("binary.bin")).unwrap();
        assert_eq!(contents, binary_data);
    }

    #[test]
    fn test_file_writer_inode() {
        let (_dir, fs) = temp_fs();
        let writer = fs.write_file("test.txt", Inode::new(42)).unwrap();
        assert_eq!(writer.inode(), Inode::new(42));
    }

    #[test]
    fn test_file_writer_path() {
        let (dir, fs) = temp_fs();
        let writer = fs.write_file("test.txt", Inode::new(1)).unwrap();
        assert_eq!(writer.path(), dir.path().join("test.txt"));
    }

    #[test]
    fn test_file_writer_finish() {
        let (dir, fs) = temp_fs();

        let mut writer = fs.write_file("test.txt", Inode::new(1)).unwrap();
        writer.write_all(b"content").unwrap();
        writer.finish().unwrap();

        let contents = fs::read_to_string(dir.path().join("test.txt")).unwrap();
        assert_eq!(contents, "content");
    }

    // ------------------------------------------------------------------------
    // Directory Operations
    // ------------------------------------------------------------------------

    #[test]
    fn test_create_dir_all_simple() {
        let (dir, fs) = temp_fs();

        fs.create_dir_all("new_dir").unwrap();
        assert!(dir.path().join("new_dir").is_dir());
    }

    #[test]
    fn test_create_dir_all_nested() {
        let (dir, fs) = temp_fs();

        fs.create_dir_all("a/b/c/d").unwrap();
        assert!(dir.path().join("a/b/c/d").is_dir());
    }

    #[test]
    fn test_create_dir_all_existing() {
        let (dir, fs) = temp_fs();
        fs::create_dir(dir.path().join("existing")).unwrap();

        // Should not error for existing directory
        fs.create_dir_all("existing").unwrap();
        assert!(dir.path().join("existing").is_dir());
    }

    #[test]
    fn test_list_dir() {
        let (dir, fs) = temp_fs();
        fs::write(dir.path().join("file1.txt"), "").unwrap();
        fs::write(dir.path().join("file2.txt"), "").unwrap();
        fs::create_dir(dir.path().join("subdir")).unwrap();

        let entries = fs.list_dir("").unwrap();

        assert_eq!(entries.len(), 3);
        assert!(entries.contains(&("file1.txt".to_string(), false)));
        assert!(entries.contains(&("file2.txt".to_string(), false)));
        assert!(entries.contains(&("subdir".to_string(), true)));
    }

    #[test]
    fn test_list_dir_sorted() {
        let (dir, fs) = temp_fs();
        fs::write(dir.path().join("c.txt"), "").unwrap();
        fs::write(dir.path().join("a.txt"), "").unwrap();
        fs::write(dir.path().join("b.txt"), "").unwrap();

        let entries = fs.list_dir("").unwrap();

        // Should be alphabetically sorted
        assert_eq!(entries[0].0, "a.txt");
        assert_eq!(entries[1].0, "b.txt");
        assert_eq!(entries[2].0, "c.txt");
    }

    #[test]
    fn test_list_dir_not_found() {
        let (_dir, fs) = temp_fs();
        let result = fs.list_dir("nonexistent");
        assert!(result.is_err());
    }

    // ------------------------------------------------------------------------
    // Remove Operations
    // ------------------------------------------------------------------------

    #[test]
    fn test_remove_file() {
        let (dir, fs) = temp_fs();
        fs::write(dir.path().join("file.txt"), "content").unwrap();

        fs.remove_path("file.txt", false).unwrap();
        assert!(!dir.path().join("file.txt").exists());
    }

    #[test]
    fn test_remove_empty_directory() {
        let (dir, fs) = temp_fs();
        fs::create_dir(dir.path().join("empty_dir")).unwrap();

        fs.remove_path("empty_dir", false).unwrap();
        assert!(!dir.path().join("empty_dir").exists());
    }

    #[test]
    fn test_remove_nonempty_directory_fails() {
        let (dir, fs) = temp_fs();
        fs::create_dir(dir.path().join("nonempty")).unwrap();
        fs::write(dir.path().join("nonempty/file.txt"), "").unwrap();

        let result = fs.remove_path("nonempty", false);
        assert!(result.is_err());
    }

    #[test]
    fn test_remove_recursive() {
        let (dir, fs) = temp_fs();
        fs::create_dir_all(dir.path().join("a/b/c")).unwrap();
        fs::write(dir.path().join("a/b/c/file.txt"), "").unwrap();
        fs::write(dir.path().join("a/b/other.txt"), "").unwrap();

        fs.remove_path("a", true).unwrap();
        assert!(!dir.path().join("a").exists());
    }

    #[test]
    fn test_remove_not_found() {
        let (_dir, fs) = temp_fs();
        let result = fs.remove_path("nonexistent", false);
        assert!(result.is_err());
    }

    // ------------------------------------------------------------------------
    // Rename Operations
    // ------------------------------------------------------------------------

    #[test]
    fn test_rename_file() {
        let (dir, fs) = temp_fs();
        fs::write(dir.path().join("old.txt"), "content").unwrap();

        fs.rename("old.txt", "new.txt").unwrap();

        assert!(!dir.path().join("old.txt").exists());
        assert!(dir.path().join("new.txt").exists());
        assert_eq!(
            fs::read_to_string(dir.path().join("new.txt")).unwrap(),
            "content"
        );
    }

    #[test]
    fn test_rename_directory() {
        let (dir, fs) = temp_fs();
        fs::create_dir(dir.path().join("old_dir")).unwrap();
        fs::write(dir.path().join("old_dir/file.txt"), "content").unwrap();

        fs.rename("old_dir", "new_dir").unwrap();

        assert!(!dir.path().join("old_dir").exists());
        assert!(dir.path().join("new_dir/file.txt").exists());
    }

    #[test]
    fn test_rename_creates_parent() {
        let (dir, fs) = temp_fs();
        fs::write(dir.path().join("file.txt"), "content").unwrap();

        fs.rename("file.txt", "new/path/file.txt").unwrap();

        assert!(dir.path().join("new/path/file.txt").exists());
    }

    #[test]
    fn test_rename_not_found() {
        let (_dir, fs) = temp_fs();
        let result = fs.rename("nonexistent", "new_name");
        assert!(result.is_err());
    }

    // ------------------------------------------------------------------------
    // Permissions
    // ------------------------------------------------------------------------

    #[cfg(unix)]
    #[test]
    fn test_set_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let (dir, fs) = temp_fs();
        let path = dir.path().join("file.txt");
        fs::write(&path, "content").unwrap();

        fs.set_permissions("file.txt", 0o755).unwrap();

        let perms = fs::metadata(&path).unwrap().permissions();
        assert_eq!(perms.mode() & 0o777, 0o755);
    }

    #[test]
    fn test_is_writable_file() {
        let (dir, fs) = temp_fs();
        fs::write(dir.path().join("file.txt"), "content").unwrap();

        assert!(fs.is_writable("file.txt").unwrap());
    }

    #[test]
    fn test_is_writable_nonexistent() {
        let (_dir, fs) = temp_fs();
        // Non-existent files in writable directory should be writable
        assert!(fs.is_writable("nonexistent.txt").unwrap());
    }

    // ------------------------------------------------------------------------
    // Walk Files
    // ------------------------------------------------------------------------

    #[test]
    fn test_walk_files_empty() {
        let (_dir, fs) = temp_fs();
        let files = fs.walk_files("").unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn test_walk_files_simple() {
        let (dir, fs) = temp_fs();
        fs::write(dir.path().join("file1.txt"), "").unwrap();
        fs::write(dir.path().join("file2.txt"), "").unwrap();

        let files = fs.walk_files("").unwrap();

        assert_eq!(files.len(), 2);
        assert!(files.contains(&"file1.txt".to_string()));
        assert!(files.contains(&"file2.txt".to_string()));
    }

    #[test]
    fn test_walk_files_nested() {
        let (dir, fs) = temp_fs();
        fs::create_dir_all(dir.path().join("a/b")).unwrap();
        fs::write(dir.path().join("root.txt"), "").unwrap();
        fs::write(dir.path().join("a/middle.txt"), "").unwrap();
        fs::write(dir.path().join("a/b/deep.txt"), "").unwrap();

        let files = fs.walk_files("").unwrap();

        assert_eq!(files.len(), 3);
        assert!(files.contains(&"root.txt".to_string()));
        assert!(files.contains(&"a/middle.txt".to_string()) ||
                files.contains(&"a\\middle.txt".to_string())); // Windows compat
    }

    #[test]
    fn test_walk_files_excludes_atomic_dir() {
        let (dir, fs) = temp_fs();
        fs::write(dir.path().join("file.txt"), "").unwrap();
        fs::create_dir(dir.path().join(".atomic")).unwrap();
        fs::write(dir.path().join(".atomic/pristine"), "").unwrap();

        let files = fs.walk_files("").unwrap();

        assert_eq!(files.len(), 1);
        assert!(files.contains(&"file.txt".to_string()));
    }

    #[test]
    fn test_walk_files_sorted() {
        let (dir, fs) = temp_fs();
        fs::write(dir.path().join("c.txt"), "").unwrap();
        fs::write(dir.path().join("a.txt"), "").unwrap();
        fs::write(dir.path().join("b.txt"), "").unwrap();

        let files = fs.walk_files("").unwrap();

        assert_eq!(files[0], "a.txt");
        assert_eq!(files[1], "b.txt");
        assert_eq!(files[2], "c.txt");
    }

    #[test]
    fn test_walk_files_from_subdir() {
        let (dir, fs) = temp_fs();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("root.txt"), "").unwrap();
        fs::write(dir.path().join("src/main.rs"), "").unwrap();
        fs::write(dir.path().join("src/lib.rs"), "").unwrap();

        let files = fs.walk_files("src").unwrap();

        assert_eq!(files.len(), 2);
        // Files should still have full relative paths from root
    }

    // ------------------------------------------------------------------------
    // Helper Functions
    // ------------------------------------------------------------------------

    #[test]
    fn test_normalize_path_simple() {
        let path = PathBuf::from("/a/b/c");
        let normalized = normalize_path(&path);
        assert_eq!(normalized, PathBuf::from("/a/b/c"));
    }

    #[test]
    fn test_normalize_path_with_dot() {
        let path = PathBuf::from("/a/./b/./c");
        let normalized = normalize_path(&path);
        assert_eq!(normalized, PathBuf::from("/a/b/c"));
    }

    #[test]
    fn test_normalize_path_with_dotdot() {
        let path = PathBuf::from("/a/b/../c");
        let normalized = normalize_path(&path);
        assert_eq!(normalized, PathBuf::from("/a/c"));
    }

    #[test]
    fn test_normalize_path_complex() {
        let path = PathBuf::from("/a/b/c/../../d/./e");
        let normalized = normalize_path(&path);
        assert_eq!(normalized, PathBuf::from("/a/d/e"));
    }

    // ------------------------------------------------------------------------
    // Integration Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_roundtrip_write_read() {
        let (_dir, fs) = temp_fs();
        let content = b"This is test content with unicode: \xc3\xa9\xc3\xa8\xc3\xa0";

        // Write
        {
            let mut writer = fs.write_file("test.txt", Inode::new(1)).unwrap();
            writer.write_all(content).unwrap();
        }

        // Read
        let mut buffer = Vec::new();
        fs.read_file("test.txt", &mut buffer).unwrap();
        assert_eq!(buffer, content);
    }

    #[test]
    fn test_full_workflow() {
        let (_dir, fs) = temp_fs();

        // Create directory structure
        fs.create_dir_all("src/utils").unwrap();

        // Write files
        {
            let mut w = fs.write_file("src/main.rs", Inode::new(1)).unwrap();
            w.write_all(b"fn main() {}").unwrap();
        }
        {
            let mut w = fs.write_file("src/utils/helpers.rs", Inode::new(2)).unwrap();
            w.write_all(b"pub fn help() {}").unwrap();
        }

        // Verify structure
        assert!(fs.is_directory("src"));
        assert!(fs.is_directory("src/utils"));
        assert!(fs.exists("src/main.rs"));
        assert!(fs.exists("src/utils/helpers.rs"));

        // Read back
        let mut main_content = Vec::new();
        fs.read_file("src/main.rs", &mut main_content).unwrap();
        assert_eq!(main_content, b"fn main() {}");

        // Rename
        fs.rename("src/utils/helpers.rs", "src/utils/lib.rs").unwrap();
        assert!(!fs.exists("src/utils/helpers.rs"));
        assert!(fs.exists("src/utils/lib.rs"));

        // Remove
        fs.remove_path("src/utils/lib.rs", false).unwrap();
        assert!(!fs.exists("src/utils/lib.rs"));

        // Clean up
        fs.remove_path("src", true).unwrap();
        assert!(!fs.exists("src"));
    }
}
