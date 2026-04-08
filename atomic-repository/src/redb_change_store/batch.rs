//! Batch types for layer-selective reads.
//!
//! This module contains the [`StoredSection`] type used when loading
//! individual sections from the redb change store.

use atomic_core::change::format_v3::SectionType;

// ═══════════════════════════════════════════════════════════════════════
// StoredSection — a single stored section blob
// ═══════════════════════════════════════════════════════════════════════

/// A single section loaded from redb.
///
/// Contains the decompressed payload and the section type. This is the
/// redb equivalent of [`format_v3::reader::ReadSection`] but loaded
/// from table values instead of a file stream.
#[derive(Clone, Debug)]
pub struct StoredSection {
    /// The type of section.
    pub section_type: SectionType,

    /// The decompressed payload bytes.
    pub payload: Vec<u8>,

    /// The file path this section belongs to (for GRAPH/SEMANTIC sections).
    /// Empty string for metadata sections and content chunks.
    pub path: String,

    /// File index within the change (for ordering).
    pub file_index: u32,
}
