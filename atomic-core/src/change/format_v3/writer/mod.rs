//! Streaming writer for the Change Format V3.
//!
//! The [`ChangeWriter`] writes a complete V3 change file to any [`Write`](std::io::Write)
//! destination, enforcing correct section ordering, compressing each section
//! independently with zstd, and computing the content hash incrementally via
//! `blake3::Hasher`.
//!
//! # Design
//!
//! The writer is a **state machine** that enforces the V3 section ordering:
//!
//! ```text
//! FileHeader → HashTable → HEADER → DEPS → [PROVENANCE] → GRAPH* → SEMANTIC* → CONTENT* → [UNHASHED] → Trailer
//! ```
//!
//! Each transition validates that sections appear in the correct order. Attempting
//! to write a section out of order produces a runtime
//! [`FormatError::UnexpectedSection`](super::error::FormatError::UnexpectedSection).
//!
//! # Incremental Hashing
//!
//! The content hash is computed **incrementally** as sections are written:
//!
//! ```text
//! blake3::Hasher
//!   ← hash table bytes (raw)
//!   ← HEADER section (compressed)
//!   ← DEPS section (compressed)
//!   ← PROVENANCE section (compressed, if present)
//!   ← GRAPH sections (compressed, each one)
//!   ← SEMANTIC sections (compressed, each one)
//!   ← CONTENT chunks (compressed, each one)
//!   → finalize() → content_hash
//! ```
//!
//! The UNHASHED section is explicitly excluded from the hash. The file header
//! and trailer are also excluded (the header contains structural metadata,
//! and the trailer contains the hash itself).
//!
//! # Memory Usage
//!
//! The writer never buffers the entire change in memory. Each section is
//! serialized → compressed → hashed → written in a single pass. Peak memory
//! is proportional to the **largest single section**, not the total change size.
//!
//! # Compression
//!
//! Each section is independently compressed with zstd at a configurable level
//! (default: 3). This enables:
//! - Parallel compression in the future (each section compresses independently)
//! - Selective decompression (read only the sections you need)
//! - Better compression ratios (homogeneous data within each section)
//!
//! # Error Handling
//!
//! All write methods return [`FormatResult<()>`](super::error::FormatResult) (or
//! [`FormatResult<WriteOutcome>`] for `finalize`). Errors include:
//! - I/O errors from the underlying writer
//! - Compression errors from zstd
//! - Section ordering violations
//! - State machine violations (e.g., finalizing before writing required sections)

mod helpers;
pub mod options;
mod sections;
pub mod state_machine;

#[cfg(test)]
mod tests;

// ── Re-exports ─────────────────────────────────────────────────────────

pub use options::{WriteOutcome, WriterOptions, WriterStats};
pub use state_machine::ChangeWriter;
