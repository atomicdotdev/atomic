//! Core types for the Change Format V3 serialization layer.
//!
//! This module defines the foundational types used throughout the V3 format:
//!
//! - [`HashIndex`]: A `u16` reference into the hash deduplication table
//! - [`CompactPosition`]: A position using `HashIndex` instead of full 32-byte hashes
//! - [`SectionType`]: Discriminator for the different section kinds in a V3 file
//! - [`FileHeader`]: The fixed 64-byte header at the start of every V3 change file
//! - [`FileHeaderFlags`]: Bitfield flags in the file header
//! - [`FileHeaderBuilder`]: Fluent builder for constructing file headers
//! - [`SectionHeader`]: The framing header for each section (type + compressed length)
//! - [`ContentChunkHeader`]: Extended header for content chunks (includes chunk hash)
//! - [`Trailer`]: The 32-byte blake3 hash at the end of the file
//!
//! # Design Rationale
//!
//! ## Hash Deduplication
//!
//! In V1/V2, every `Position<Option<Hash>>` stores a full 32-byte hash (plus 1 byte
//! for the `Option` discriminant = 33 bytes). A typical change references the same
//! few hashes (its own hash + dependency hashes) thousands of times throughout its
//! hunks. For an initial record of 194K LOC, this wastes ~18 MB.
//!
//! V3 stores unique hashes once in a dedup table at the top of the file, then
//! references them by `u16` index. This turns a 33-byte `Option<Hash>` into a
//! 1-3 byte postcard varint. Combined with postcard's varint encoding for the
//! position offset, a full `Position` shrinks from 41 bytes to 3-5 bytes.
//!
//! ## Section Types
//!
//! The file is divided into independently compressed sections, each tagged with
//! a `SectionType` byte. This enables:
//! - **Selective loading**: Read only GRAPH sections to apply, only SEMANTIC for review
//! - **Parallel compression**: Each section compresses independently
//! - **Streaming**: Process sections as they arrive over the network
//! - **Random access**: Seek to a specific section without deserializing everything
//!
//! ## Fixed Header
//!
//! The 64-byte header is intentionally fixed-size so readers can validate a file
//! with a single `read_exact(64)` call. The `reserved` field provides room for
//! future flags without changing the header size.
//!
//! # Wire Format
//!
//! ```text
//! FileHeader (64 bytes, fixed, uncompressed)
//! ├── magic: [u8; 4]            = b"ATOM"
//! ├── version: u32              = 1
//! ├── flags: u32                = bitfield (see FileHeaderFlags)
//! ├── hash_table_entries: u32   = count of unique hashes
//! ├── graph_section_count: u32  = number of GRAPH sections
//! ├── semantic_section_count: u32 = number of SEMANTIC sections
//! ├── contents_chunks: u32      = number of CONTENT chunks
//! ├── total_uncompressed: u64   = sum of all uncompressed section sizes
//! └── reserved: [u8; 28]        = zeros
//!
//! SectionHeader (5 bytes per section)
//! ├── section_type: u8          = SectionType discriminant
//! └── compressed_len: u32       = length of compressed payload
//!
//! ContentChunkHeader (45 bytes per content chunk)
//! ├── section_type: u8          = CONTENT (0x20)
//! ├── chunk_index: u32          = sequential chunk number
//! ├── chunk_hash: [u8; 32]      = blake3 of uncompressed chunk data
//! └── compressed_len: u32       = length of compressed payload
//! ```

pub mod builder;
pub mod hash_index;
pub mod header;
pub mod section;

#[cfg(test)]
mod tests;

// ── Re-exports ─────────────────────────────────────────────────────────

// Hash index types
pub use hash_index::{is_none_index, CompactPosition, HashIndex, HASH_INDEX_NONE, HASH_INDEX_SELF};

// Header types
pub use header::{FileHeader, FileHeaderFlags};

// Builder
pub use builder::{FileHeaderBuilder, Trailer};

// Section types
pub use section::{ContentChunkHeader, SectionHeader, SectionType};
