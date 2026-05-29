//! Filesystem-backed working copy implementation.
//!
//! Provides [`FileSystem`] which implements [`WorkingCopy`] and
//! `WorkingCopyRead` for real filesystem operations, and [`FileWriter`]
//! for buffered file output.

mod paths;
mod walk;

#[cfg(test)]
mod tests;

use super::traits::WorkingCopy;
use crate::types::Inode;
use paths::{normalize_path, set_permissions};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

// Re-export walk and paths items that are part of the public API
pub use paths::{DEFAULT_DIR_MODE, DEFAULT_EXEC_MODE, DEFAULT_FILE_MODE};

// CONSTANTS

/// The name of the Atomic metadata directory, which is always ignored.
pub const DOT_DIR: &str = ".atomic";

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
    pub fn from_root<P: AsRef<Path>>(root: P) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }

    /// Get the root directory path.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a relative path to an absolute path within the root.
    ///
    /// This method sanitizes the path to prevent directory traversal attacks.
    ///
    /// # Errors
    ///
    /// Returns an error if the path would escape the root.
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
    pub fn exists(&self, path: &str) -> bool {
        self.resolve_path(path).map(|p| p.exists()).unwrap_or(false)
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

// HASHING WRITER

/// A writer wrapper that computes a Blake3 content hash while writing.
///
/// Wraps any `Write` implementation and feeds all written bytes into a
/// `blake3::Hasher`. After writing is complete, call `finalize()` to
/// get the content hash.
///
/// This eliminates the need to re-read files from disk to compute their
/// content hash for the FILE_INDEX.
pub struct HashingWriter<W> {
    inner: W,
    hasher: blake3::Hasher,
}

impl<W> HashingWriter<W> {
    /// Wrap a writer with a hashing layer.
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: blake3::Hasher::new(),
        }
    }

    /// Finalize the hash and return the content hash.
    ///
    /// This can be called after all writes are complete (and flushed).
    pub fn finalize(&self) -> crate::types::Merkle {
        crate::types::Merkle(self.hasher.finalize().into())
    }

    /// Get a mutable reference to the inner writer.
    pub fn inner_mut(&mut self) -> &mut W {
        &mut self.inner
    }

    /// Consume this wrapper and return the inner writer.
    pub fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
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
    fn create_dir_all(&self, path: &str) -> Result<(), Self::Error> {
        let abs_path = self.resolve_path(path)?;
        fs::create_dir_all(&abs_path)
    }

    /// Remove a file or directory.
    ///
    /// If `recursive` is true, removes directories and their contents.
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
    fn set_permissions(&self, path: &str, permissions: u16) -> Result<(), Self::Error> {
        let abs_path = self.resolve_path(path)?;
        set_permissions(&abs_path, permissions)
    }

    /// Open a file for writing, creating it if necessary.
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
