//! Tag types, data structures, and validation for Atomic VCS.
//!
//! This module contains the core tag types ([`Tag`], [`TagOptions`], [`TagFilter`],
//! [`TagSort`]), error types ([`TagError`]), and validation functions.

use atomic_core::change::Author;
use atomic_core::types::{Base32, Merkle};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;
use thiserror::Error;

// ============================================================================
// ERROR TYPES
// ============================================================================

/// Result type for tag operations.
pub type TagResult<T> = Result<T, TagError>;

/// Errors that can occur during tag operations.
#[derive(Debug, Error)]
pub enum TagError {
    /// A tag with the given name already exists.
    #[error("Tag already exists: {name}")]
    AlreadyExists { name: String },

    /// The specified tag was not found.
    #[error("Tag not found: {name}")]
    NotFound { name: String },

    /// The tag name is invalid.
    #[error("Invalid tag name '{name}': {reason}")]
    InvalidName { name: String, reason: String },

    /// The specified stack was not found.
    #[error("Stack not found: {name}")]
    StackNotFound { name: String },

    /// The specified state was not found in the stack.
    #[error("State not found in stack: {state}")]
    StateNotFound { state: String },

    /// The tag file is corrupted or invalid.
    #[error("Invalid tag file: {path}")]
    InvalidTagFile { path: PathBuf },

    /// Database error.
    #[error("Database error: {0}")]
    Database(String),

    /// IO error.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// Serialization error.
    #[error("Serialization error: {0}")]
    Serialization(String),
}

// ============================================================================
// TAG
// ============================================================================

/// A tag representing a named state snapshot.
///
/// Tags can be either lightweight (just name + state) or annotated
/// (includes message, author, timestamp).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tag {
    /// The human-readable name of the tag.
    pub name: String,
    /// The stack this tag belongs to.
    pub stack: String,
    /// The sequence number in the stack when tagged.
    pub sequence: u64,
    /// The Merkle state at the tagged point.
    pub state: Merkle,
    /// When the tag was created.
    pub timestamp: DateTime<Utc>,
    /// Optional message describing the tag (for annotated tags).
    pub message: Option<String>,
    /// Optional author of the tag (for annotated tags).
    pub author: Option<Author>,
    /// Whether this is an annotated tag.
    pub annotated: bool,
}

impl Tag {
    /// Create a new lightweight tag.
    pub fn new(
        name: impl Into<String>,
        stack: impl Into<String>,
        sequence: u64,
        state: Merkle,
    ) -> Self {
        Self {
            name: name.into(),
            stack: stack.into(),
            sequence,
            state,
            timestamp: Utc::now(),
            message: None,
            author: None,
            annotated: false,
        }
    }

    /// Create a new annotated tag.
    pub fn annotated(
        name: impl Into<String>,
        stack: impl Into<String>,
        sequence: u64,
        state: Merkle,
        message: impl Into<String>,
        author: Author,
    ) -> Self {
        Self {
            name: name.into(),
            stack: stack.into(),
            sequence,
            state,
            timestamp: Utc::now(),
            message: Some(message.into()),
            author: Some(author),
            annotated: true,
        }
    }

    /// Check if this is an annotated tag.
    pub fn is_annotated(&self) -> bool {
        self.annotated
    }

    /// Check if this is a lightweight tag.
    pub fn is_lightweight(&self) -> bool {
        !self.annotated
    }

    /// Get the tag message if present.
    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    /// Get the tag author if present.
    pub fn author(&self) -> Option<&Author> {
        self.author.as_ref()
    }

    /// Set the message for this tag (makes it annotated).
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self.annotated = true;
        self
    }

    /// Set the author for this tag (makes it annotated).
    pub fn with_author(mut self, author: Author) -> Self {
        self.author = Some(author);
        self.annotated = true;
        self
    }

    /// Set the timestamp for this tag.
    pub fn with_timestamp(mut self, timestamp: DateTime<Utc>) -> Self {
        self.timestamp = timestamp;
        self
    }
}

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} -> {} (seq: {}, stack: {})",
            self.name,
            &self.state.to_base32()[..8],
            self.sequence,
            self.stack
        )
    }
}

// ============================================================================
// TAG OPTIONS
// ============================================================================

/// Options for creating a tag.
#[derive(Debug, Clone, Default)]
pub struct TagOptions {
    /// Optional message for an annotated tag.
    pub message: Option<String>,
    /// Optional author for an annotated tag.
    pub author: Option<Author>,
    /// Stack to tag (None = current stack).
    pub stack: Option<String>,
    /// Specific sequence to tag (None = current HEAD).
    pub sequence: Option<u64>,
    /// Whether to force overwrite an existing tag.
    pub force: bool,
}

impl TagOptions {
    /// Create new default options.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the tag message.
    pub fn message(mut self, msg: impl Into<String>) -> Self {
        self.message = Some(msg.into());
        self
    }

    /// Set the tag author.
    pub fn author(mut self, name: impl Into<String>, email: Option<impl Into<String>>) -> Self {
        self.author = Some(Author::new(name, email));
        self
    }

    /// Set the stack to tag.
    pub fn stack(mut self, name: impl Into<String>) -> Self {
        self.stack = Some(name.into());
        self
    }

    /// Set a specific sequence to tag.
    pub fn sequence(mut self, seq: u64) -> Self {
        self.sequence = Some(seq);
        self
    }

    /// Enable force mode to overwrite existing tags.
    pub fn force(mut self, force: bool) -> Self {
        self.force = force;
        self
    }

    /// Create options for an annotated tag.
    pub fn annotated(message: impl Into<String>) -> Self {
        Self::default().message(message)
    }

    /// Check if this would create an annotated tag.
    pub fn is_annotated(&self) -> bool {
        self.message.is_some() || self.author.is_some()
    }
}

// ============================================================================
// TAG FILTER
// ============================================================================

/// Filter options for listing tags.
#[derive(Debug, Clone, Default)]
pub struct TagFilter {
    /// Filter by stack name.
    pub stack: Option<String>,
    /// Filter by name pattern (glob-like).
    pub pattern: Option<String>,
    /// Only include annotated tags.
    pub annotated_only: bool,
    /// Only include lightweight tags.
    pub lightweight_only: bool,
    /// Sort order.
    pub sort: TagSort,
    /// Maximum number of tags to return.
    pub limit: Option<usize>,
}

impl TagFilter {
    /// Create a new filter with defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Filter by stack.
    pub fn stack(mut self, name: impl Into<String>) -> Self {
        self.stack = Some(name.into());
        self
    }

    /// Filter by name pattern.
    pub fn pattern(mut self, pattern: impl Into<String>) -> Self {
        self.pattern = Some(pattern.into());
        self
    }

    /// Only include annotated tags.
    pub fn annotated_only(mut self) -> Self {
        self.annotated_only = true;
        self.lightweight_only = false;
        self
    }

    /// Only include lightweight tags.
    pub fn lightweight_only(mut self) -> Self {
        self.lightweight_only = true;
        self.annotated_only = false;
        self
    }

    /// Set sort order.
    pub fn sort(mut self, sort: TagSort) -> Self {
        self.sort = sort;
        self
    }

    /// Limit the number of results.
    pub fn limit(mut self, n: usize) -> Self {
        self.limit = Some(n);
        self
    }

    /// Check if a tag matches this filter.
    pub fn matches(&self, tag: &Tag) -> bool {
        // Stack filter
        if let Some(ref stack) = self.stack {
            if tag.stack != *stack {
                return false;
            }
        }

        // Pattern filter (simple prefix/suffix matching)
        if let Some(ref pattern) = self.pattern {
            if !matches_pattern(&tag.name, pattern) {
                return false;
            }
        }

        // Annotated filter
        if self.annotated_only && !tag.is_annotated() {
            return false;
        }

        // Lightweight filter
        if self.lightweight_only && !tag.is_lightweight() {
            return false;
        }

        true
    }
}

// ============================================================================
// TAG SORT
// ============================================================================

/// Sort order for tag listings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TagSort {
    /// Sort by name alphabetically.
    #[default]
    Name,
    /// Sort by creation timestamp (newest first).
    Timestamp,
    /// Sort by sequence number (highest first).
    Sequence,
}

// ============================================================================
// VALIDATION
// ============================================================================

/// Validate a tag name.
///
/// Valid tag names:
/// - Are not empty
/// - Don't contain path separators (/, \)
/// - Don't start with a dot (.)
/// - Don't contain control characters
/// - Are not reserved names (HEAD, etc.)
pub fn validate_tag_name(name: &str) -> TagResult<()> {
    if name.is_empty() {
        return Err(TagError::InvalidName {
            name: name.to_string(),
            reason: "tag name cannot be empty".to_string(),
        });
    }

    if name.starts_with('.') {
        return Err(TagError::InvalidName {
            name: name.to_string(),
            reason: "tag name cannot start with '.'".to_string(),
        });
    }

    if name.contains('/') || name.contains('\\') {
        return Err(TagError::InvalidName {
            name: name.to_string(),
            reason: "tag name cannot contain path separators".to_string(),
        });
    }

    if name.contains(char::is_control) {
        return Err(TagError::InvalidName {
            name: name.to_string(),
            reason: "tag name cannot contain control characters".to_string(),
        });
    }

    // Reserved names
    let reserved = ["HEAD", "ORIG_HEAD", "FETCH_HEAD", "MERGE_HEAD"];
    if reserved.iter().any(|r| name.eq_ignore_ascii_case(r)) {
        return Err(TagError::InvalidName {
            name: name.to_string(),
            reason: format!("'{}' is a reserved name", name),
        });
    }

    // Maximum length
    if name.len() > 256 {
        return Err(TagError::InvalidName {
            name: name.to_string(),
            reason: "tag name cannot exceed 256 characters".to_string(),
        });
    }

    Ok(())
}

/// Check if a tag name matches a glob-like pattern.
///
/// Supports:
/// - `*` - matches any characters
/// - Exact match
pub fn matches_pattern(name: &str, pattern: &str) -> bool {
    // Simple pattern matching
    if pattern == "*" {
        return true;
    }

    if pattern.starts_with('*') && pattern.ends_with('*') {
        // Contains
        let inner = &pattern[1..pattern.len() - 1];
        return name.contains(inner);
    }

    if let Some(suffix) = pattern.strip_prefix('*') {
        // Ends with
        return name.ends_with(suffix);
    }

    if let Some(prefix) = pattern.strip_suffix('*') {
        // Starts with
        return name.starts_with(prefix);
    }

    // Exact match
    name == pattern
}
