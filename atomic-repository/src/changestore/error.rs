//! Error types for the change store.

use atomic_core::change::ChangeError;
use thiserror::Error;

pub type ChangeStoreResult<T> = Result<T, ChangeStoreError>;

/// Errors that can occur during change store operations.
///
/// These errors cover all failure modes for storing and retrieving changes,
/// including I/O errors, serialization failures, and integrity violations.
#[derive(Debug, Error)]
pub enum ChangeStoreError {
    /// The requested change was not found on disk.
    ///
    /// This can occur when:
    /// - The change was never saved
    /// - The change was deleted
    /// - The hash is incorrect
    #[error("Change not found: {hash}")]
    NotFound {
        /// The base32-encoded hash of the missing change
        hash: String,
    },

    /// An I/O error occurred during a filesystem operation.
    ///
    /// This wraps standard I/O errors and includes context about
    /// what operation was being attempted.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// A change file failed to serialize or deserialize.
    ///
    /// This can occur if:
    /// - The file format version is incompatible
    /// - The file is corrupted
    /// - There's a bug in the serialization code
    #[error("Change serialization error: {0}")]
    Serialization(#[from] ChangeError),

    /// The computed hash doesn't match the expected hash.
    ///
    /// This indicates data corruption or tampering. The change should
    /// not be trusted and may need to be re-downloaded from a remote.
    #[error("Hash mismatch: expected {expected}, computed {computed}")]
    HashMismatch {
        /// The hash we expected (e.g., from the filename)
        expected: String,
        /// The hash we computed from the file contents
        computed: String,
    },

    /// Failed to persist a temporary file.
    ///
    /// This can occur if:
    /// - The target path is on a different filesystem
    /// - Permission denied on the target directory
    /// - Disk is full
    #[error("Failed to persist change file: {0}")]
    Persist(#[from] tempfile::PersistError),

    /// The changes directory doesn't exist and couldn't be created.
    #[error("Changes directory not found: {path}")]
    DirectoryNotFound {
        /// The path that should contain the changes directory
        path: String,
    },

    /// The requested content range is out of bounds.
    ///
    /// This can occur when:
    /// - The span references content beyond the change's content length
    /// - The buffer provided is too small
    #[error("Content out of bounds for change {hash}: requested [{requested_start}..{requested_end}], content length {content_len}")]
    ContentOutOfBounds {
        /// The hash of the change
        hash: String,
        /// The requested start position
        requested_start: usize,
        /// The requested end position
        requested_end: usize,
        /// The actual content length
        content_len: usize,
    },
}

impl ChangeStoreError {
    /// Check if this error indicates the change doesn't exist.
    ///
    /// This is useful for distinguishing "not found" from other errors
    /// when implementing fallback logic.
    pub fn is_not_found(&self) -> bool {
        matches!(self, ChangeStoreError::NotFound { .. })
    }

    /// Check if this error indicates data corruption.
    ///
    /// Corruption errors should trigger re-download from remotes
    /// or error escalation to the user.
    pub fn is_corruption(&self) -> bool {
        matches!(self, ChangeStoreError::HashMismatch { .. })
    }
}
