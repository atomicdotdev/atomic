//! Atomic Semantic — AST extraction for code intelligence.
//!
//! Provides multi-language code entity extraction using tree-sitter.
//! Supports Rust, TypeScript, Python, Go, and Java.
//!
//! # Usage
//!
//! ```rust,ignore
//! use atomic_semantic::{ParserRegistry, Entity, EntityKind};
//!
//! let mut registry = ParserRegistry::new();
//! let entities = registry.extract("src/auth.rs", source_code);
//!
//! for entity in &entities {
//!     println!("{}: {} ({}:{})", entity.kind, entity.name, entity.file, entity.line);
//! }
//! ```

pub mod entity;
pub mod parser;
pub mod parsers;

/// Find the largest byte index ≤ `max_bytes` that falls on a UTF-8 char boundary.
///
/// Prevents panics when truncating strings that contain multi-byte characters
/// (e.g., emoji, CJK, accented chars).
pub(crate) fn truncate_to_char_boundary(s: &str, max_bytes: usize) -> usize {
    if max_bytes >= s.len() {
        return s.len();
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

pub use entity::{
    ChangeType, Confidence, Entity, EntityChange, EntityKind, FileSummary, Reference,
};
pub use parsers::{is_supported, Language, LanguageParser, ParserRegistry, ALL_EXTENSIONS};
