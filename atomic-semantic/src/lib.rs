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

pub use entity::{
    ChangeType, Confidence, Entity, EntityChange, EntityKind, FileSummary, Reference,
};
pub use parsers::{is_supported, Language, LanguageParser, ParserRegistry, ALL_EXTENSIONS};
