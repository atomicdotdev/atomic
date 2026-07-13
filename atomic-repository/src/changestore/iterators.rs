//! Filesystem iterators for the change store.
//!
//! Contains iterators that walk the two-level directory structure to find
//! change files, attestation files, and provenance graph files.

use std::fs;
use std::path::Path;

use atomic_core::types::{Base32, Hash};

use super::{ChangeStoreError, ChangeStoreResult, CHANGE_EXTENSION};

// ═══════════════════════════════════════════════════════════════════════
// ChangeIterator
// ═══════════════════════════════════════════════════════════════════════

/// Iterator over changes stored in the filesystem.
///
/// This iterator walks the two-level directory structure and yields
/// the hash of each valid change file found.
pub(crate) struct ChangeIterator {
    /// Iterator over subdirectories (the two-character prefixes)
    dir_iter: Option<fs::ReadDir>,
    /// Iterator over files in the current subdirectory
    file_iter: Option<fs::ReadDir>,
}

impl ChangeIterator {
    /// Create a new iterator starting from the changes directory.
    pub(crate) fn new(changes_dir: &Path) -> Self {
        let dir_iter = fs::read_dir(changes_dir).ok();
        Self {
            dir_iter,
            file_iter: None,
        }
    }

    /// Try to get the next hash from the current file iterator.
    fn next_from_files(&mut self) -> Option<ChangeStoreResult<Hash>> {
        let file_iter = self.file_iter.as_mut()?;

        for entry_result in file_iter {
            match entry_result {
                Ok(entry) => {
                    let path = entry.path();

                    // Skip if not a file
                    if !path.is_file() {
                        continue;
                    }

                    // Check for .change extension
                    if path.extension().is_none_or(|e| e != CHANGE_EXTENSION) {
                        continue;
                    }

                    // Extract the hash from the filename
                    if let Some(hash) = Self::hash_from_path(&path) {
                        return Some(Ok(hash));
                    }
                }
                Err(e) => return Some(Err(ChangeStoreError::Io(e))),
            }
        }

        None
    }

    /// Extract a hash from a change file path.
    fn hash_from_path(path: &Path) -> Option<Hash> {
        let stem = path.file_stem()?.to_str()?;
        Hash::from_base32(stem.as_bytes())
    }
}

impl Iterator for ChangeIterator {
    type Item = ChangeStoreResult<Hash>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            // Try to get next from current file iterator
            if let Some(result) = self.next_from_files() {
                return Some(result);
            }

            // Move to next subdirectory
            let dir_iter = self.dir_iter.as_mut()?;

            loop {
                match dir_iter.next()? {
                    Ok(entry) => {
                        let path = entry.path();
                        if path.is_dir() {
                            self.file_iter = fs::read_dir(&path).ok();
                            break;
                        }
                    }
                    Err(e) => return Some(Err(ChangeStoreError::Io(e))),
                }
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// AttestationIterator
// ═══════════════════════════════════════════════════════════════════════

/// Iterator over attestation hashes in the changes directory.
///
/// Walks the two-level directory structure looking for `.attest` files.
#[allow(dead_code)]
pub(crate) struct AttestationIterator<'a> {
    changes_dir: &'a Path,
    prefix_dirs: Option<fs::ReadDir>,
    current_files: Option<fs::ReadDir>,
}

impl<'a> AttestationIterator<'a> {
    pub(crate) fn new(changes_dir: &'a Path) -> Self {
        let prefix_dirs = fs::read_dir(changes_dir).ok();
        Self {
            changes_dir,
            prefix_dirs,
            current_files: None,
        }
    }

    fn next_from_files(&mut self) -> Option<ChangeStoreResult<Hash>> {
        loop {
            let files = self.current_files.as_mut()?;
            match files.next() {
                Some(Ok(entry)) => {
                    let path = entry.path();
                    if !path.is_file() {
                        continue;
                    }
                    if path
                        .extension()
                        .is_none_or(|e| e != atomic_core::change::ATTESTATION_EXTENSION)
                    {
                        continue;
                    }
                    // Extract hash from filename (strip extension)
                    let stem = match path.file_stem().and_then(|s| s.to_str()) {
                        Some(s) => s.to_string(),
                        None => continue,
                    };
                    match Hash::from_base32(stem.as_bytes()) {
                        Some(hash) => return Some(Ok(hash)),
                        None => continue,
                    }
                }
                Some(Err(e)) => return Some(Err(e.into())),
                None => {
                    self.current_files = None;
                }
            }
        }
    }
}

impl<'a> Iterator for AttestationIterator<'a> {
    type Item = ChangeStoreResult<Hash>;

    fn next(&mut self) -> Option<Self::Item> {
        // Try current files first
        if let Some(result) = self.next_from_files() {
            return Some(result);
        }

        // Move to next prefix directory
        loop {
            let prefix_dirs = self.prefix_dirs.as_mut()?;
            match prefix_dirs.next() {
                Some(Ok(entry)) => {
                    let path = entry.path();
                    if !path.is_dir() {
                        continue;
                    }
                    self.current_files = fs::read_dir(&path).ok();
                    if let Some(result) = self.next_from_files() {
                        return Some(result);
                    }
                }
                Some(Err(e)) => return Some(Err(e.into())),
                None => return None,
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// ProvenanceIterator
// ═══════════════════════════════════════════════════════════════════════

/// Iterator over provenance graph hashes in the changes directory.
///
/// Walks the two-level directory structure looking for `.provenance` files.
pub(crate) struct ProvenanceIterator {
    prefix_dirs: Option<fs::ReadDir>,
    current_files: Option<fs::ReadDir>,
}

impl ProvenanceIterator {
    pub(crate) fn new(changes_dir: &Path) -> Self {
        let prefix_dirs = fs::read_dir(changes_dir).ok();
        Self {
            prefix_dirs,
            current_files: None,
        }
    }

    fn next_from_files(&mut self) -> Option<ChangeStoreResult<Hash>> {
        loop {
            let files = self.current_files.as_mut()?;
            match files.next() {
                Some(Ok(entry)) => {
                    let path = entry.path();
                    if !path.is_file() {
                        continue;
                    }
                    if path
                        .extension()
                        .is_none_or(|e| e != atomic_core::change::PROVENANCE_GRAPH_EXTENSION)
                    {
                        continue;
                    }
                    // Extract hash from filename (strip extension)
                    let stem = match path.file_stem().and_then(|s| s.to_str()) {
                        Some(s) => s.to_string(),
                        None => continue,
                    };
                    match Hash::from_base32(stem.as_bytes()) {
                        Some(hash) => return Some(Ok(hash)),
                        None => continue,
                    }
                }
                Some(Err(e)) => return Some(Err(e.into())),
                None => {
                    self.current_files = None;
                }
            }
        }
    }
}

impl Iterator for ProvenanceIterator {
    type Item = ChangeStoreResult<Hash>;

    fn next(&mut self) -> Option<Self::Item> {
        // Try current files first
        if let Some(result) = self.next_from_files() {
            return Some(result);
        }

        // Move to next prefix directory
        loop {
            let prefix_dirs = self.prefix_dirs.as_mut()?;
            match prefix_dirs.next() {
                Some(Ok(entry)) => {
                    let path = entry.path();
                    if !path.is_dir() {
                        continue;
                    }
                    self.current_files = fs::read_dir(&path).ok();
                    if let Some(result) = self.next_from_files() {
                        return Some(result);
                    }
                }
                Some(Err(e)) => return Some(Err(e.into())),
                None => return None,
            }
        }
    }
}
