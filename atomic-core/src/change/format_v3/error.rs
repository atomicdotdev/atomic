//! Error types for the Change Format V3 serialization layer.
//!
//! This module defines all errors that can occur during reading, writing,
//! and validating V3 change files. Errors are organized into categories:
//!
//! - **Format errors**: Invalid magic bytes, unsupported versions, malformed headers
//! - **Serialization errors**: Postcard encoding/decoding failures
//! - **Compression errors**: Zstd compression/decompression failures
//! - **Hash errors**: Hash table overflow, missing entries, verification failures
//! - **I/O errors**: Underlying read/write failures
//! - **Section errors**: Invalid section types, unexpected ordering, truncation
//!
//! # Error Design
//!
//! All errors implement `std::error::Error` via `thiserror` and carry enough
//! context to produce actionable error messages. For example, a hash mismatch
//! error includes both the expected and computed hashes so the caller can
//! report which change file is corrupt without additional lookups.
//!
//! # Example
//!
//! ```rust
//! use atomic_core::change::format_v3::FormatError;
//!
//! fn example_error_handling() -> Result<(), FormatError> {
//!     // Format errors convert from I/O, postcard, and other sources
//!     let result: Result<Vec<u8>, std::io::Error> = Err(std::io::Error::new(
//!         std::io::ErrorKind::UnexpectedEof,
//!         "truncated file",
//!     ));
//!     let _bytes = result.map_err(FormatError::from)?;
//!     Ok(())
//! }
//! ```

use thiserror::Error;

/// Magic bytes expected at the start of every V3 change file.
///
/// These four bytes (`b"ATOM"`) identify the file as an Atomic V3 change.
/// Any file not starting with these bytes is rejected immediately.
pub const MAGIC: [u8; 4] = *b"ATOM";

/// The only supported format version.
///
/// V3 is a clean break — there is no backward compatibility with V1/V2.
/// The version field exists for future-proofing: if V4 is ever needed,
/// readers can detect it immediately from the header.
pub const FORMAT_VERSION: u32 = 1;

/// Maximum number of unique hashes in the deduplication table.
///
/// This is `u16::MAX - 1` because index `0xFFFF` is reserved as a sentinel
/// for "no hash" (i.e., the root position). In practice, a change with
/// 65,534 unique dependency hashes would be extraordinary — most changes
/// reference fewer than 100 unique hashes.
pub const MAX_HASH_TABLE_ENTRIES: usize = (u16::MAX as usize) - 1;

/// Errors that can occur during V3 change format operations.
///
/// This is the primary error type for the `format_v3` module. All public
/// functions in the module return `Result<T, FormatError>`.
///
/// # Categories
///
/// | Category | Variants | When |
/// |----------|----------|------|
/// | Format | `InvalidMagic`, `UnsupportedVersion`, `InvalidHeader` | Reading file header |
/// | Serialization | `Postcard` | Encoding/decoding with postcard |
/// | Compression | `Compress`, `Decompress` | Zstd operations |
/// | Hash | `HashTableFull`, `HashIndexOutOfBounds`, `HashMismatch` | Hash dedup table ops |
/// | I/O | `Io` | File read/write |
/// | Section | `InvalidSectionType`, `UnexpectedSection`, `SectionTruncated` | Section parsing |
#[derive(Debug, Error)]
pub enum FormatError {
    // ── Format errors ──────────────────────────────────────────────
    /// File does not start with the expected `b"ATOM"` magic bytes.
    ///
    /// This usually means the file is not a V3 change file at all, or
    /// it's a V1/V2 file from before the format migration.
    #[error("invalid magic bytes: expected {:?}, got {got:?}", MAGIC)]
    InvalidMagic {
        /// The bytes actually found at the start of the file.
        got: [u8; 4],
    },

    /// The file's version field doesn't match the supported version.
    ///
    /// This can happen if a newer version of Atomic writes a V4+ format
    /// and an older binary tries to read it.
    #[error("unsupported format version: expected {expected}, got {got}")]
    UnsupportedVersion {
        /// The version this binary supports.
        expected: u32,
        /// The version found in the file.
        got: u32,
    },

    /// The 64-byte file header is malformed or contains invalid field values.
    #[error("invalid header: {reason}")]
    InvalidHeader {
        /// Human-readable explanation of what's wrong.
        reason: String,
    },

    // ── Serialization errors ───────────────────────────────────────
    /// Postcard serialization or deserialization failed.
    ///
    /// This wraps errors from the `postcard` crate, which uses varint
    /// encoding for compact serialization. Common causes:
    /// - Corrupt data (truncated varint, invalid enum discriminant)
    /// - Type mismatch between writer and reader versions
    #[error("postcard serialization error: {0}")]
    Postcard(#[from] postcard::Error),

    // ── Compression errors ─────────────────────────────────────────
    /// Zstd compression failed during writing.
    ///
    /// This is rare in practice — zstd compression usually only fails
    /// if the system is out of memory.
    #[error("compression error: {0}")]
    Compress(String),

    /// Zstd decompression failed during reading.
    ///
    /// Common causes:
    /// - Corrupt compressed data (bit flip, truncation)
    /// - Data was not actually zstd-compressed
    /// - Wrong compression dictionary (not used in V3, but possible future issue)
    #[error("decompression error: {0}")]
    Decompress(String),

    // ── Hash errors ────────────────────────────────────────────────
    /// The hash deduplication table is full (more than 65,534 unique hashes).
    ///
    /// This means the change references more unique dependency hashes than
    /// can be represented by a `u16` index. This is extremely unlikely in
    /// practice — it would require a change that depends on 65,534+ other
    /// changes.
    #[error(
        "hash dedup table is full: cannot store more than {} unique hashes",
        MAX_HASH_TABLE_ENTRIES
    )]
    HashTableFull,

    /// A `HashIndex` references a position beyond the end of the hash table.
    ///
    /// This indicates corrupt data — either the hash table was truncated
    /// or a section contains an index that was never written to the table.
    #[error("hash index {index} is out of bounds (table has {table_size} entries)")]
    HashIndexOutOfBounds {
        /// The invalid index that was encountered.
        index: u16,
        /// The actual number of entries in the hash table.
        table_size: u16,
    },

    /// The computed content hash doesn't match the expected hash in the trailer.
    ///
    /// This means the file was modified after writing, or there's a bug in
    /// the hashing pipeline. Both the expected and computed hashes are
    /// included so the caller can report which file is corrupt.
    #[error("hash mismatch: expected {expected}, computed {computed}")]
    HashMismatch {
        /// The hash stored in the file's trailer.
        expected: String,
        /// The hash computed from the file's hashed sections.
        computed: String,
    },

    /// A hash lookup failed — the requested hash is not in the dedup table.
    ///
    /// This occurs during writing when trying to reference a hash that
    /// wasn't registered with the `HashDedupTable` before serialization.
    #[error("hash not found in dedup table: {hash}")]
    HashNotFound {
        /// Base32 representation of the hash that wasn't found.
        hash: String,
    },

    // ── I/O errors ─────────────────────────────────────────────────
    /// An underlying I/O error occurred during file read or write.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    // ── Section errors ─────────────────────────────────────────────
    /// An unrecognized section type byte was encountered.
    ///
    /// This can happen if a newer format adds section types that this
    /// binary doesn't know about. The unknown byte is included for
    /// debugging.
    #[error("invalid section type: 0x{type_byte:02X}")]
    InvalidSectionType {
        /// The unrecognized section type byte.
        type_byte: u8,
    },

    /// A section appeared in an unexpected position in the file.
    ///
    /// V3 change files have a defined section ordering:
    /// Header → HashTable → ChangeHeader → Deps → Provenance →
    /// Graph sections → Semantic sections → Content chunks → Unhashed → Trailer
    ///
    /// This error fires when a section appears out of order.
    #[error("unexpected section: got {got} where {expected} was expected")]
    UnexpectedSection {
        /// The section type that was found.
        got: String,
        /// What section type(s) were expected at this position.
        expected: String,
    },

    /// A section's compressed data is shorter than its declared length.
    ///
    /// This usually means the file was truncated during writing or transfer.
    #[error("section truncated: declared {declared} bytes but only {actual} bytes available")]
    SectionTruncated {
        /// The number of compressed bytes declared in the section header.
        declared: u32,
        /// The number of bytes actually available before EOF.
        actual: u32,
    },

    /// The file ended before all declared sections were read.
    #[error("unexpected end of file: expected {expected} more sections")]
    UnexpectedEof {
        /// How many more sections were expected based on the file header.
        expected: u32,
    },
}

/// Convenience type alias for `Result<T, FormatError>`.
pub type FormatResult<T> = Result<T, FormatError>;

// ── Trait implementations for error conversions ────────────────────────

impl FormatError {
    /// Returns `true` if this is an I/O error.
    pub fn is_io(&self) -> bool {
        matches!(self, FormatError::Io(_))
    }

    /// Returns `true` if this is a serialization (postcard) error.
    pub fn is_serialization(&self) -> bool {
        matches!(self, FormatError::Postcard(_))
    }

    /// Returns `true` if this is a compression or decompression error.
    pub fn is_compression(&self) -> bool {
        matches!(self, FormatError::Compress(_) | FormatError::Decompress(_))
    }

    /// Returns `true` if this is a hash-related error.
    pub fn is_hash_error(&self) -> bool {
        matches!(
            self,
            FormatError::HashTableFull
                | FormatError::HashIndexOutOfBounds { .. }
                | FormatError::HashMismatch { .. }
                | FormatError::HashNotFound { .. }
        )
    }

    /// Returns `true` if this is a format/structural error.
    pub fn is_format_error(&self) -> bool {
        matches!(
            self,
            FormatError::InvalidMagic { .. }
                | FormatError::UnsupportedVersion { .. }
                | FormatError::InvalidHeader { .. }
                | FormatError::InvalidSectionType { .. }
                | FormatError::UnexpectedSection { .. }
                | FormatError::SectionTruncated { .. }
                | FormatError::UnexpectedEof { .. }
        )
    }

    /// Returns a human-readable suggestion for how to fix this error.
    ///
    /// This is intended for CLI error reporting where a "did you mean?"
    /// style hint can guide the user.
    pub fn suggestion(&self) -> Option<&'static str> {
        match self {
            FormatError::InvalidMagic { .. } => {
                Some("This file may be a V1/V2 change file. Re-record with the current version.")
            }
            FormatError::UnsupportedVersion { .. } => {
                Some("Update Atomic to a version that supports this format.")
            }
            FormatError::HashTableFull => {
                Some("Split this change into smaller changes with fewer dependencies.")
            }
            FormatError::HashMismatch { .. } => {
                Some("The change file may be corrupt. Try re-downloading or re-recording.")
            }
            FormatError::SectionTruncated { .. } | FormatError::UnexpectedEof { .. } => {
                Some("The file appears truncated. Try re-downloading.")
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Constants ──────────────────────────────────────────────────

    #[test]
    fn test_magic_bytes() {
        assert_eq!(MAGIC, [b'A', b'T', b'O', b'M']);
        assert_eq!(&MAGIC, b"ATOM");
    }

    #[test]
    fn test_format_version() {
        assert_eq!(FORMAT_VERSION, 1);
    }

    #[test]
    fn test_max_hash_table_entries() {
        // u16::MAX - 1 because 0xFFFF is reserved as sentinel
        assert_eq!(MAX_HASH_TABLE_ENTRIES, 65534);
        assert_eq!(MAX_HASH_TABLE_ENTRIES, (u16::MAX as usize) - 1);
    }

    // ── Error construction ─────────────────────────────────────────

    #[test]
    fn test_invalid_magic_error() {
        let err = FormatError::InvalidMagic {
            got: [0x00, 0x01, 0x02, 0x03],
        };
        let msg = err.to_string();
        assert!(msg.contains("invalid magic bytes"));
        assert!(msg.contains("[0, 1, 2, 3]"));
    }

    #[test]
    fn test_unsupported_version_error() {
        let err = FormatError::UnsupportedVersion {
            expected: FORMAT_VERSION,
            got: 99,
        };
        let msg = err.to_string();
        assert!(msg.contains("unsupported format version"));
        assert!(msg.contains("expected 1"));
        assert!(msg.contains("got 99"));
    }

    #[test]
    fn test_invalid_header_error() {
        let err = FormatError::InvalidHeader {
            reason: "graph_section_count is zero but semantic_section_count is 5".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("invalid header"));
        assert!(msg.contains("graph_section_count is zero"));
    }

    #[test]
    fn test_postcard_error_conversion() {
        // Create a postcard error by trying to deserialize garbage
        let bad_data = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        let result: Result<String, postcard::Error> = postcard::from_bytes(&bad_data);
        assert!(result.is_err());

        let format_err: FormatError = result.unwrap_err().into();
        assert!(format_err.is_serialization());
        assert!(!format_err.is_io());
        assert!(!format_err.is_compression());
    }

    #[test]
    fn test_compress_error() {
        let err = FormatError::Compress("out of memory".to_string());
        let msg = err.to_string();
        assert!(msg.contains("compression error"));
        assert!(msg.contains("out of memory"));
    }

    #[test]
    fn test_decompress_error() {
        let err = FormatError::Decompress("corrupt zstd frame".to_string());
        let msg = err.to_string();
        assert!(msg.contains("decompression error"));
        assert!(msg.contains("corrupt zstd frame"));
    }

    #[test]
    fn test_hash_table_full_error() {
        let err = FormatError::HashTableFull;
        let msg = err.to_string();
        assert!(msg.contains("hash dedup table is full"));
        assert!(msg.contains("65534"));
    }

    #[test]
    fn test_hash_index_out_of_bounds_error() {
        let err = FormatError::HashIndexOutOfBounds {
            index: 500,
            table_size: 100,
        };
        let msg = err.to_string();
        assert!(msg.contains("hash index 500"));
        assert!(msg.contains("100 entries"));
    }

    #[test]
    fn test_hash_mismatch_error() {
        let err = FormatError::HashMismatch {
            expected: "AAAA".to_string(),
            computed: "BBBB".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("hash mismatch"));
        assert!(msg.contains("AAAA"));
        assert!(msg.contains("BBBB"));
    }

    #[test]
    fn test_hash_not_found_error() {
        let err = FormatError::HashNotFound {
            hash: "DEADBEEF".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("hash not found"));
        assert!(msg.contains("DEADBEEF"));
    }

    #[test]
    fn test_io_error_conversion() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let format_err: FormatError = io_err.into();
        assert!(format_err.is_io());
        assert!(!format_err.is_serialization());
    }

    #[test]
    fn test_invalid_section_type_error() {
        let err = FormatError::InvalidSectionType { type_byte: 0xAB };
        let msg = err.to_string();
        assert!(msg.contains("invalid section type"));
        assert!(msg.contains("0xAB"));
    }

    #[test]
    fn test_unexpected_section_error() {
        let err = FormatError::UnexpectedSection {
            got: "SEMANTIC".to_string(),
            expected: "GRAPH".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("unexpected section"));
        assert!(msg.contains("SEMANTIC"));
        assert!(msg.contains("GRAPH"));
    }

    #[test]
    fn test_section_truncated_error() {
        let err = FormatError::SectionTruncated {
            declared: 1024,
            actual: 512,
        };
        let msg = err.to_string();
        assert!(msg.contains("section truncated"));
        assert!(msg.contains("1024"));
        assert!(msg.contains("512"));
    }

    #[test]
    fn test_unexpected_eof_error() {
        let err = FormatError::UnexpectedEof { expected: 3 };
        let msg = err.to_string();
        assert!(msg.contains("unexpected end of file"));
        assert!(msg.contains("3"));
    }

    // ── Classification methods ─────────────────────────────────────

    #[test]
    fn test_is_io() {
        let io_err: FormatError =
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "broken").into();
        assert!(io_err.is_io());

        let other = FormatError::HashTableFull;
        assert!(!other.is_io());
    }

    #[test]
    fn test_is_serialization() {
        // Only postcard errors are serialization errors
        let err = FormatError::Compress("x".into());
        assert!(!err.is_serialization());

        let io_err: FormatError = std::io::Error::other("x").into();
        assert!(!io_err.is_serialization());
    }

    #[test]
    fn test_is_compression() {
        assert!(FormatError::Compress("x".into()).is_compression());
        assert!(FormatError::Decompress("x".into()).is_compression());
        assert!(!FormatError::HashTableFull.is_compression());
    }

    #[test]
    fn test_is_hash_error() {
        assert!(FormatError::HashTableFull.is_hash_error());
        assert!(FormatError::HashIndexOutOfBounds {
            index: 0,
            table_size: 0
        }
        .is_hash_error());
        assert!(FormatError::HashMismatch {
            expected: String::new(),
            computed: String::new(),
        }
        .is_hash_error());
        assert!(FormatError::HashNotFound {
            hash: String::new(),
        }
        .is_hash_error());

        // Non-hash errors
        assert!(!FormatError::InvalidMagic { got: [0; 4] }.is_hash_error());
    }

    #[test]
    fn test_is_format_error() {
        assert!(FormatError::InvalidMagic { got: [0; 4] }.is_format_error());
        assert!(FormatError::UnsupportedVersion {
            expected: 1,
            got: 2
        }
        .is_format_error());
        assert!(FormatError::InvalidHeader {
            reason: String::new()
        }
        .is_format_error());
        assert!(FormatError::InvalidSectionType { type_byte: 0 }.is_format_error());
        assert!(FormatError::UnexpectedSection {
            got: String::new(),
            expected: String::new(),
        }
        .is_format_error());
        assert!(FormatError::SectionTruncated {
            declared: 0,
            actual: 0
        }
        .is_format_error());
        assert!(FormatError::UnexpectedEof { expected: 0 }.is_format_error());

        // Non-format errors
        assert!(!FormatError::HashTableFull.is_format_error());
        assert!(!FormatError::Compress(String::new()).is_format_error());
    }

    // ── Suggestions ────────────────────────────────────────────────

    #[test]
    fn test_suggestion_for_invalid_magic() {
        let err = FormatError::InvalidMagic { got: [0; 4] };
        let suggestion = err.suggestion();
        assert!(suggestion.is_some());
        assert!(suggestion.unwrap().contains("V1/V2"));
    }

    #[test]
    fn test_suggestion_for_unsupported_version() {
        let err = FormatError::UnsupportedVersion {
            expected: 1,
            got: 2,
        };
        let suggestion = err.suggestion();
        assert!(suggestion.is_some());
        assert!(suggestion.unwrap().contains("Update"));
    }

    #[test]
    fn test_suggestion_for_hash_table_full() {
        let err = FormatError::HashTableFull;
        let suggestion = err.suggestion();
        assert!(suggestion.is_some());
        assert!(suggestion.unwrap().contains("Split"));
    }

    #[test]
    fn test_suggestion_for_hash_mismatch() {
        let err = FormatError::HashMismatch {
            expected: "A".into(),
            computed: "B".into(),
        };
        let suggestion = err.suggestion();
        assert!(suggestion.is_some());
        assert!(suggestion.unwrap().contains("corrupt"));
    }

    #[test]
    fn test_suggestion_for_truncated() {
        let err = FormatError::SectionTruncated {
            declared: 100,
            actual: 50,
        };
        assert!(err.suggestion().is_some());

        let err = FormatError::UnexpectedEof { expected: 1 };
        assert!(err.suggestion().is_some());
    }

    #[test]
    fn test_suggestion_for_no_suggestion_errors() {
        // Some errors don't have specific suggestions
        assert!(FormatError::Compress("x".into()).suggestion().is_none());
        assert!(FormatError::Decompress("x".into()).suggestion().is_none());
        assert!(FormatError::HashIndexOutOfBounds {
            index: 0,
            table_size: 0
        }
        .suggestion()
        .is_none());
    }

    // ── Error source chain ─────────────────────────────────────────

    #[test]
    fn test_io_error_preserves_kind() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "no access");
        let format_err: FormatError = io_err.into();

        if let FormatError::Io(inner) = &format_err {
            assert_eq!(inner.kind(), std::io::ErrorKind::PermissionDenied);
        } else {
            panic!("expected FormatError::Io");
        }
    }

    #[test]
    fn test_error_debug_format() {
        // Ensure Debug is implemented and doesn't panic
        let err = FormatError::InvalidMagic { got: [1, 2, 3, 4] };
        let debug = format!("{:?}", err);
        assert!(debug.contains("InvalidMagic"));
    }

    #[test]
    fn test_error_display_format() {
        // Ensure Display is implemented and produces human-readable output
        let err = FormatError::HashMismatch {
            expected: "AAAA1234".to_string(),
            computed: "BBBB5678".to_string(),
        };
        let display = format!("{}", err);
        assert!(display.contains("hash mismatch"));
        assert!(display.contains("AAAA1234"));
        assert!(display.contains("BBBB5678"));
    }
}
