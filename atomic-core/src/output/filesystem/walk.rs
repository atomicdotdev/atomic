//! Walk, listing, and read operations for [`FileSystem`].
//!
//! This module contains the directory listing, recursive file walking,
//! and the [`WorkingCopyRead`] trait implementation.

use super::paths::get_permissions;
use super::{FileSystem, DOT_DIR};
use crate::output::traits::{FileMetadata, WorkingCopyRead};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;
use std::time::SystemTime;

impl FileSystem {
    /// List files in a directory.
    ///
    /// Returns a sorted list of entries as `(filename, is_directory)` tuples.
    ///
    /// # Errors
    ///
    /// Returns an error if the path is not a directory or cannot be read.
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
    /// ignore patterns. The `.atomic` directory is always excluded.
    pub fn walk_files(&self, path: &str) -> io::Result<Vec<String>> {
        let abs_path = if path.is_empty() {
            self.root().to_path_buf()
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
                if let Ok(relative) = path.strip_prefix(self.root()) {
                    files.push(relative.to_string_lossy().to_string());
                }
            }
        }

        Ok(())
    }

    /// Check if a path is the Atomic metadata directory or inside it.
    pub fn is_atomic_path(&self, path: &str) -> bool {
        path == DOT_DIR || path.starts_with(&format!("{}/", DOT_DIR))
    }
}

// WORKING COPY READ IMPLEMENTATION

impl WorkingCopyRead for FileSystem {
    type Error = io::Error;

    /// Get metadata for a file or directory.
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
    /// The buffer is **appended to**, not cleared.
    fn read_file(&self, path: &str, buffer: &mut Vec<u8>) -> Result<(), Self::Error> {
        let abs_path = self.resolve_path(path)?;
        let mut file = File::open(&abs_path)?;
        file.read_to_end(buffer)?;
        Ok(())
    }

    /// Get the modification time of a file.
    fn modified_time(&self, path: &str) -> Result<SystemTime, Self::Error> {
        let abs_path = self.resolve_path(path)?;
        let metadata = fs::metadata(&abs_path)?;
        metadata.modified()
    }

    /// Check if a path exists.
    fn exists(&self, path: &str) -> bool {
        self.resolve_path(path).map(|p| p.exists()).unwrap_or(false)
    }

    /// Check if a path is a directory.
    fn is_directory(&self, path: &str) -> bool {
        self.resolve_path(path).map(|p| p.is_dir()).unwrap_or(false)
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
