//! Tag management for Atomic VCS
//!
//! Tags are named snapshots of a stack's state at a particular point in time.
//! Unlike Git tags which point to commits, Atomic tags point to Merkle states -
//! cryptographic hashes representing the complete sequence of applied changes.
//!
//! # Overview
//!
//! Tags serve several purposes in Atomic:
//!
//! 1. **Release Points**: Mark stable versions (v1.0.0, v2.0.0-beta, etc.)
//! 2. **Synchronization Anchors**: Known-good states for sync operations
//! 3. **Rollback Targets**: Points to return to if needed
//! 4. **Archive References**: States to export as tarballs
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                          Tag Storage                                    │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  TAGS Table (per stack)                                                 │
//! │  ┌────────────────────────────────────────────────────────────────┐     │
//! │  │  Key: (stack_id, sequence)  │  Value: Merkle State            │     │
//! │  ├─────────────────────────────┼─────────────────────────────────┤     │
//! │  │  (1, 10)                    │  ABC123...                      │     │
//! │  │  (1, 25)                    │  DEF456...                      │     │
//! │  │  (1, 50)                    │  GHI789...                      │     │
//! │  └─────────────────────────────┴─────────────────────────────────┘     │
//! │                                                                         │
//! │  Tag Files (.atomic/tags/{stack}/)                                      │
//! │  ┌────────────────────────────────────────────────────────────────┐     │
//! │  │  main/                                                         │     │
//! │  │    v1.0.0.tag  →  { name, stack, state, timestamp, ... }      │     │
//! │  │    v2.0.0.tag  →  { name, stack, state, timestamp, ... }      │     │
//! │  │  feature/                                                      │     │
//! │  │    v1.0.0.tag  →  { name, stack, state, timestamp, ... }      │     │
//! │  │    release.tag →  { name, stack, state, timestamp, ... }      │     │
//! │  └────────────────────────────────────────────────────────────────┘     │
//! │                                                                         │
//! │  Note: Same tag name can exist in different stacks!                     │
//! │  This enables stack-specific releases and milestones.                   │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Per-Stack Storage
//!
//! Tags are stored in a per-stack directory structure, allowing the same tag
//! name to exist in different stacks. This enables:
//!
//! - **Stack-Specific Releases**: Each stack can have its own "v1.0.0" tag
//! - **Independent Milestones**: Feature stacks can mark their own milestones
//! - **Parallel Development**: Multiple teams can tag their work independently
//!
//! ## File Layout
//!
//! ```text
//! .atomic/tags/
//! ├── main/
//! │   ├── v1.0.0.tag
//! │   └── v2.0.0.tag
//! ├── feature-auth/
//! │   ├── v1.0.0.tag      # Same name as main, different state!
//! │   └── milestone-1.tag
//! └── hotfix/
//!     └── urgent-fix.tag
//! ```
//!
//! ## API Patterns
//!
//! ```rust,ignore
//! // Tags default to current stack
//! let tag = repo.get_tag("v1.0.0")?;
//!
//! // Explicit stack selection
//! let tag = repo.get_tag_from_stack("v1.0.0", "feature")?;
//!
//! // Search all stacks
//! let tag = repo.get_tag_any_stack("v1.0.0")?;
//!
//! // List tags for current stack
//! let tags = repo.list_tags()?;
//!
//! // List tags across all stacks
//! let all_tags = repo.list_all_tags()?;
//!
//! // List stacks that have tags
//! let stacks = repo.list_tag_stacks()?;
//! ```
//!
//! # Tag Types
//!
//! - **Lightweight Tags**: Just a state reference (sequence + merkle)
//! - **Annotated Tags**: Include metadata (message, author, timestamp)
//!
//! # Usage
//!
//! ```rust,ignore
//! use atomic_repository::{Repository, TagOptions};
//!
//! let repo = Repository::open(".")?;
//!
//! // Create a lightweight tag
//! repo.create_tag("v1.0.0", TagOptions::default())?;
//!
//! // Create an annotated tag
//! repo.create_tag("v1.0.0", TagOptions::default()
//!     .message("Release version 1.0.0")
//!     .author("Alice", Some("alice@example.com")))?;
//!
//! // List tags
//! for tag in repo.list_tags()? {
//!     println!("{}: {}", tag.name, tag.state.to_base32());
//! }
//!
//! // Get a specific tag
//! if let Some(tag) = repo.get_tag("v1.0.0")? {
//!     println!("Tagged at sequence {}", tag.sequence);
//! }
//!
//! // Delete a tag
//! repo.delete_tag("v1.0.0")?;
//! ```
//!
//! # Synchronization
//!
//! Tags are important for remote synchronization. When pushing or pulling,
//! tags can be used to:
//!
//! - Verify both sides have the same state
//! - Transfer only changes after a known tag
//! - Ensure archive integrity

use atomic_core::change::Author;
use atomic_core::types::{Base32, Merkle};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};
use thiserror::Error;

// Error Types

/// Result type for tag operations.
pub type TagResult<T> = Result<T, TagError>;

/// Errors that can occur during tag operations.
#[derive(Debug, Error)]
pub enum TagError {
    /// A tag with the given name already exists.
    #[error("Tag already exists: {name}")]
    AlreadyExists {
        /// Name of the existing tag.
        name: String,
    },

    /// The specified tag was not found.
    #[error("Tag not found: {name}")]
    NotFound {
        /// Name of the missing tag.
        name: String,
    },

    /// The tag name is invalid.
    #[error("Invalid tag name '{name}': {reason}")]
    InvalidName {
        /// The invalid name.
        name: String,
        /// Reason it's invalid.
        reason: String,
    },

    /// The specified stack was not found.
    #[error("Stack not found: {name}")]
    StackNotFound {
        /// Name of the missing stack.
        name: String,
    },

    /// The specified state was not found in the stack.
    #[error("State not found in stack: {state}")]
    StateNotFound {
        /// The missing state hash.
        state: String,
    },

    /// The tag file is corrupted or invalid.
    #[error("Invalid tag file: {path}")]
    InvalidTagFile {
        /// Path to the invalid file.
        path: PathBuf,
    },

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

// Tag

/// A tag representing a named state snapshot.
///
/// Tags can be either lightweight (just name + state) or annotated
/// (includes message, author, timestamp).
///
/// # Example
///
/// ```rust,ignore
/// let tag = Tag::new("v1.0.0", 42, merkle_state);
/// println!("{}: sequence {}", tag.name, tag.sequence);
/// ```
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
    ///
    /// # Arguments
    ///
    /// * `name` - The tag name
    /// * `stack` - The stack name
    /// * `sequence` - The sequence number
    /// * `state` - The Merkle state
    ///
    /// # Returns
    ///
    /// A new lightweight `Tag`.
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
    ///
    /// # Arguments
    ///
    /// * `name` - The tag name
    /// * `stack` - The stack name
    /// * `sequence` - The sequence number
    /// * `state` - The Merkle state
    /// * `message` - The tag message
    /// * `author` - The tag author
    ///
    /// # Returns
    ///
    /// A new annotated `Tag`.
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

// TagOptions

/// Options for creating a tag.
///
/// # Example
///
/// ```rust,ignore
/// let options = TagOptions::default()
///     .message("Release 1.0")
///     .author("Alice", Some("alice@example.com"));
/// ```
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

// TagFilter

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

// Tag Name Validation

/// Validate a tag name.
///
/// Valid tag names:
/// - Are not empty
/// - Don't contain path separators (/, \)
/// - Don't start with a dot (.)
/// - Don't contain control characters
/// - Are not reserved names (HEAD, etc.)
///
/// # Arguments
///
/// * `name` - The tag name to validate
///
/// # Returns
///
/// `Ok(())` if valid, or a `TagError::InvalidName` if not.
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
/// - `?` - matches single character
/// - Exact match
fn matches_pattern(name: &str, pattern: &str) -> bool {
    // Simple pattern matching
    if pattern == "*" {
        return true;
    }

    if pattern.starts_with('*') && pattern.ends_with('*') {
        // Contains
        let inner = &pattern[1..pattern.len() - 1];
        return name.contains(inner);
    }

    if pattern.starts_with('*') {
        // Ends with
        let suffix = &pattern[1..];
        return name.ends_with(suffix);
    }

    if pattern.ends_with('*') {
        // Starts with
        let prefix = &pattern[..pattern.len() - 1];
        return name.starts_with(prefix);
    }

    // Exact match
    name == pattern
}

// Tag File Operations

/// Get the path for a tag file (per-stack storage).
///
/// Tags are stored in a per-stack directory structure:
/// `{tags_dir}/{stack}/{name}.tag`
///
/// This allows the same tag name to exist in different stacks,
/// enabling stack-specific releases and milestones.
///
/// # Arguments
///
/// * `tags_dir` - The tags directory (e.g., `.atomic/tags`)
/// * `stack` - The stack name
/// * `name` - The tag name
///
/// # Returns
///
/// The path to the tag file.
///
/// # Example
///
/// ```rust,ignore
/// let path = tag_file_path(Path::new(".atomic/tags"), "main", "v1.0.0");
/// // Returns: .atomic/tags/main/v1.0.0.tag
/// ```
pub fn tag_file_path(tags_dir: &Path, stack: &str, name: &str) -> PathBuf {
    tags_dir.join(stack).join(format!("{}.tag", name))
}

/// Get the stack directory for tags.
///
/// # Arguments
///
/// * `tags_dir` - The tags directory
/// * `stack` - The stack name
///
/// # Returns
///
/// The path to the stack's tag directory.
pub fn stack_tags_dir(tags_dir: &Path, stack: &str) -> PathBuf {
    tags_dir.join(stack)
}

/// Save a tag to a file (per-stack storage).
///
/// The tag is saved to `{tags_dir}/{tag.stack}/{tag.name}.tag`.
/// The stack directory is created if it doesn't exist.
///
/// # Arguments
///
/// * `tags_dir` - The tags directory
/// * `tag` - The tag to save (uses tag.stack for the subdirectory)
///
/// # Returns
///
/// `Ok(())` on success.
///
/// # Errors
///
/// Returns `TagError::AlreadyExists` if a tag with the same name
/// already exists in the same stack.
pub fn save_tag(tags_dir: &Path, tag: &Tag) -> TagResult<()> {
    // Ensure stack directory exists
    let stack_dir = stack_tags_dir(tags_dir, &tag.stack);
    std::fs::create_dir_all(&stack_dir)?;

    let path = tag_file_path(tags_dir, &tag.stack, &tag.name);

    // Check for existing tag
    if path.exists() {
        return Err(TagError::AlreadyExists {
            name: tag.name.clone(),
        });
    }

    let contents = serde_json::to_string_pretty(tag)
        .map_err(|e| TagError::Serialization(e.to_string()))?;

    std::fs::write(&path, contents)?;

    Ok(())
}

/// Save a tag to a file, optionally overwriting (per-stack storage).
///
/// The tag is saved to `{tags_dir}/{tag.stack}/{tag.name}.tag`.
///
/// # Arguments
///
/// * `tags_dir` - The tags directory
/// * `tag` - The tag to save (uses tag.stack for the subdirectory)
/// * `force` - Whether to overwrite existing
///
/// # Returns
///
/// `Ok(())` on success.
pub fn save_tag_force(tags_dir: &Path, tag: &Tag, force: bool) -> TagResult<()> {
    // Ensure stack directory exists
    let stack_dir = stack_tags_dir(tags_dir, &tag.stack);
    std::fs::create_dir_all(&stack_dir)?;

    let path = tag_file_path(tags_dir, &tag.stack, &tag.name);

    // Check for existing tag
    if path.exists() && !force {
        return Err(TagError::AlreadyExists {
            name: tag.name.clone(),
        });
    }

    let contents = serde_json::to_string_pretty(tag)
        .map_err(|e| TagError::Serialization(e.to_string()))?;

    std::fs::write(&path, contents)?;

    Ok(())
}

/// Load a tag from a file (per-stack storage).
///
/// # Arguments
///
/// * `tags_dir` - The tags directory
/// * `stack` - The stack name
/// * `name` - The tag name
///
/// # Returns
///
/// The loaded `Tag`, or `None` if not found.
pub fn load_tag(tags_dir: &Path, stack: &str, name: &str) -> TagResult<Option<Tag>> {
    let path = tag_file_path(tags_dir, stack, name);

    if !path.exists() {
        return Ok(None);
    }

    let contents = std::fs::read_to_string(&path)?;

    let tag: Tag = serde_json::from_str(&contents)
        .map_err(|_| TagError::InvalidTagFile {
            path: path.clone(),
        })?;

    Ok(Some(tag))
}

/// Load a tag by name, searching all stacks.
///
/// This is useful when you don't know which stack a tag belongs to.
/// Returns the first matching tag found.
///
/// # Arguments
///
/// * `tags_dir` - The tags directory
/// * `name` - The tag name
///
/// # Returns
///
/// The loaded `Tag`, or `None` if not found in any stack.
pub fn load_tag_any_stack(tags_dir: &Path, name: &str) -> TagResult<Option<Tag>> {
    if !tags_dir.exists() {
        return Ok(None);
    }

    for entry in std::fs::read_dir(tags_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            let stack = path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");

            if let Some(tag) = load_tag(tags_dir, stack, name)? {
                return Ok(Some(tag));
            }
        }
    }

    Ok(None)
}

/// Delete a tag file (per-stack storage).
///
/// # Arguments
///
/// * `tags_dir` - The tags directory
/// * `stack` - The stack name
/// * `name` - The tag name
///
/// # Returns
///
/// `Ok(true)` if deleted, `Ok(false)` if not found.
pub fn delete_tag(tags_dir: &Path, stack: &str, name: &str) -> TagResult<bool> {
    let path = tag_file_path(tags_dir, stack, name);

    if !path.exists() {
        return Ok(false);
    }

    std::fs::remove_file(&path)?;

    // Clean up empty stack directory
    let stack_dir = stack_tags_dir(tags_dir, stack);
    if stack_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&stack_dir) {
            if entries.count() == 0 {
                let _ = std::fs::remove_dir(&stack_dir);
            }
        }
    }

    Ok(true)
}

/// List all tags for a specific stack.
///
/// # Arguments
///
/// * `tags_dir` - The tags directory
/// * `stack` - The stack name
///
/// # Returns
///
/// A vector of loaded tags for the specified stack.
pub fn list_tags(tags_dir: &Path, stack: &str) -> TagResult<Vec<Tag>> {
    let stack_dir = stack_tags_dir(tags_dir, stack);

    if !stack_dir.exists() {
        return Ok(Vec::new());
    }

    let mut tags = Vec::new();

    for entry in std::fs::read_dir(&stack_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.extension().map_or(false, |ext| ext == "tag") {
            let contents = std::fs::read_to_string(&path)?;
            if let Ok(tag) = serde_json::from_str::<Tag>(&contents) {
                tags.push(tag);
            }
        }
    }

    Ok(tags)
}

/// List all tags across all stacks.
///
/// # Arguments
///
/// * `tags_dir` - The tags directory
///
/// # Returns
///
/// A vector of all loaded tags from all stacks.
pub fn list_all_tags(tags_dir: &Path) -> TagResult<Vec<Tag>> {
    if !tags_dir.exists() {
        return Ok(Vec::new());
    }

    let mut tags = Vec::new();

    // Iterate over stack directories
    for entry in std::fs::read_dir(tags_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            let stack = path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");

            // Get tags from this stack
            let stack_tags = list_tags(tags_dir, stack)?;
            tags.extend(stack_tags);
        }
    }

    Ok(tags)
}

/// List all stack names that have tags.
///
/// # Arguments
///
/// * `tags_dir` - The tags directory
///
/// # Returns
///
/// A vector of stack names that have at least one tag.
pub fn list_tag_stacks(tags_dir: &Path) -> TagResult<Vec<String>> {
    if !tags_dir.exists() {
        return Ok(Vec::new());
    }

    let mut stacks = Vec::new();

    for entry in std::fs::read_dir(tags_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                stacks.push(name.to_string());
            }
        }
    }

    stacks.sort();
    Ok(stacks)
}

/// List tags matching a filter.
///
/// If the filter specifies a stack, only that stack is searched.
/// Otherwise, all stacks are searched.
///
/// # Arguments
///
/// * `tags_dir` - The tags directory
/// * `filter` - The filter to apply
///
/// # Returns
///
/// A filtered and sorted vector of tags.
pub fn list_tags_filtered(tags_dir: &Path, filter: &TagFilter) -> TagResult<Vec<Tag>> {
    // Get tags from appropriate source
    let all_tags = if let Some(ref stack) = filter.stack {
        list_tags(tags_dir, stack)?
    } else {
        list_all_tags(tags_dir)?
    };

    let mut tags: Vec<Tag> = all_tags
        .into_iter()
        .filter(|t| filter.matches(t))
        .collect();

    // Sort
    match filter.sort {
        TagSort::Name => tags.sort_by(|a, b| a.name.cmp(&b.name)),
        TagSort::Timestamp => tags.sort_by(|a, b| b.timestamp.cmp(&a.timestamp)),
        TagSort::Sequence => tags.sort_by(|a, b| b.sequence.cmp(&a.sequence)),
    }

    // Limit
    if let Some(limit) = filter.limit {
        tags.truncate(limit);
    }

    Ok(tags)
}

/// Count tags for a specific stack.
///
/// # Arguments
///
/// * `tags_dir` - The tags directory
/// * `stack` - The stack name
///
/// # Returns
///
/// The number of tags in the specified stack.
pub fn count_tags(tags_dir: &Path, stack: &str) -> TagResult<usize> {
    let stack_dir = stack_tags_dir(tags_dir, stack);

    if !stack_dir.exists() {
        return Ok(0);
    }

    let count = std::fs::read_dir(&stack_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .map_or(false, |ext| ext == "tag")
        })
        .count();

    Ok(count)
}

/// Count all tags across all stacks.
///
/// # Arguments
///
/// * `tags_dir` - The tags directory
///
/// # Returns
///
/// The total number of tags across all stacks.
pub fn count_all_tags(tags_dir: &Path) -> TagResult<usize> {
    if !tags_dir.exists() {
        return Ok(0);
    }

    let mut count = 0;

    for entry in std::fs::read_dir(tags_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            let stack = path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            count += count_tags(tags_dir, stack)?;
        }
    }

    Ok(count)
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // Tag Tests

    #[test]
    fn test_tag_new() {
        let state = Merkle::of(b"test state");
        let tag = Tag::new("v1.0.0", "main", 42, state);

        assert_eq!(tag.name, "v1.0.0");
        assert_eq!(tag.stack, "main");
        assert_eq!(tag.sequence, 42);
        assert_eq!(tag.state, state);
        assert!(!tag.is_annotated());
        assert!(tag.is_lightweight());
    }

    #[test]
    fn test_tag_annotated() {
        let state = Merkle::of(b"test state");
        let author = Author::new("Alice", Some("alice@example.com"));
        let tag = Tag::annotated("v1.0.0", "main", 42, state, "Release 1.0", author);

        assert!(tag.is_annotated());
        assert!(!tag.is_lightweight());
        assert_eq!(tag.message(), Some("Release 1.0"));
        assert!(tag.author().is_some());
    }

    #[test]
    fn test_tag_builder_pattern() {
        let state = Merkle::of(b"test state");
        let tag = Tag::new("v1.0.0", "main", 42, state)
            .with_message("Release notes")
            .with_author(Author::new("Bob", None::<String>));

        assert!(tag.is_annotated());
        assert_eq!(tag.message(), Some("Release notes"));
    }

    #[test]
    fn test_tag_display() {
        let state = Merkle::of(b"test state");
        let tag = Tag::new("v1.0.0", "main", 42, state);

        let display = format!("{}", tag);
        assert!(display.contains("v1.0.0"));
        assert!(display.contains("42"));
        assert!(display.contains("main"));
    }

    #[test]
    fn test_tag_equality() {
        let state = Merkle::of(b"test state");
        let tag1 = Tag::new("v1.0.0", "main", 42, state)
            .with_timestamp(DateTime::from_timestamp(1000, 0).unwrap());
        let tag2 = Tag::new("v1.0.0", "main", 42, state)
            .with_timestamp(DateTime::from_timestamp(1000, 0).unwrap());

        assert_eq!(tag1, tag2);
    }

    // TagOptions Tests

    #[test]
    fn test_tag_options_default() {
        let options = TagOptions::default();

        assert!(options.message.is_none());
        assert!(options.author.is_none());
        assert!(options.stack.is_none());
        assert!(options.sequence.is_none());
        assert!(!options.force);
        assert!(!options.is_annotated());
    }

    #[test]
    fn test_tag_options_builder() {
        let options = TagOptions::new()
            .message("Test message")
            .author("Alice", Some("alice@example.com"))
            .stack("feature")
            .sequence(10)
            .force(true);

        assert_eq!(options.message, Some("Test message".to_string()));
        assert!(options.author.is_some());
        assert_eq!(options.stack, Some("feature".to_string()));
        assert_eq!(options.sequence, Some(10));
        assert!(options.force);
        assert!(options.is_annotated());
    }

    #[test]
    fn test_tag_options_annotated() {
        let options = TagOptions::annotated("Release notes");

        assert!(options.is_annotated());
        assert_eq!(options.message, Some("Release notes".to_string()));
    }

    // TagFilter Tests

    #[test]
    fn test_tag_filter_default() {
        let filter = TagFilter::default();

        assert!(filter.stack.is_none());
        assert!(filter.pattern.is_none());
        assert!(!filter.annotated_only);
        assert!(!filter.lightweight_only);
    }

    #[test]
    fn test_tag_filter_builder() {
        let filter = TagFilter::new()
            .stack("main")
            .pattern("v*")
            .annotated_only()
            .sort(TagSort::Timestamp)
            .limit(10);

        assert_eq!(filter.stack, Some("main".to_string()));
        assert_eq!(filter.pattern, Some("v*".to_string()));
        assert!(filter.annotated_only);
        assert_eq!(filter.sort, TagSort::Timestamp);
        assert_eq!(filter.limit, Some(10));
    }

    #[test]
    fn test_tag_filter_matches_stack() {
        let state = Merkle::of(b"test");
        let tag = Tag::new("v1.0.0", "main", 1, state);

        let filter_main = TagFilter::new().stack("main");
        let filter_other = TagFilter::new().stack("other");

        assert!(filter_main.matches(&tag));
        assert!(!filter_other.matches(&tag));
    }

    #[test]
    fn test_tag_filter_matches_pattern() {
        let state = Merkle::of(b"test");
        let tag = Tag::new("v1.0.0", "main", 1, state);

        assert!(TagFilter::new().pattern("v*").matches(&tag));
        assert!(TagFilter::new().pattern("*0.0").matches(&tag));
        assert!(TagFilter::new().pattern("*1.0*").matches(&tag));
        assert!(!TagFilter::new().pattern("release*").matches(&tag));
    }

    #[test]
    fn test_tag_filter_matches_annotated() {
        let state = Merkle::of(b"test");
        let lightweight = Tag::new("v1", "main", 1, state);
        let annotated = Tag::new("v2", "main", 2, state).with_message("Test");

        let filter_annotated = TagFilter::new().annotated_only();
        let filter_lightweight = TagFilter::new().lightweight_only();

        assert!(!filter_annotated.matches(&lightweight));
        assert!(filter_annotated.matches(&annotated));
        assert!(filter_lightweight.matches(&lightweight));
        assert!(!filter_lightweight.matches(&annotated));
    }

    // Tag Name Validation Tests

    #[test]
    fn test_validate_tag_name_valid() {
        assert!(validate_tag_name("v1.0.0").is_ok());
        assert!(validate_tag_name("release-2023-01").is_ok());
        assert!(validate_tag_name("my_tag").is_ok());
        assert!(validate_tag_name("123").is_ok());
    }

    #[test]
    fn test_validate_tag_name_empty() {
        let result = validate_tag_name("");
        assert!(matches!(result, Err(TagError::InvalidName { .. })));
    }

    #[test]
    fn test_validate_tag_name_starts_with_dot() {
        let result = validate_tag_name(".hidden");
        assert!(matches!(result, Err(TagError::InvalidName { .. })));
    }

    #[test]
    fn test_validate_tag_name_path_separator() {
        assert!(matches!(
            validate_tag_name("path/to/tag"),
            Err(TagError::InvalidName { .. })
        ));
        assert!(matches!(
            validate_tag_name("path\\to\\tag"),
            Err(TagError::InvalidName { .. })
        ));
    }

    #[test]
    fn test_validate_tag_name_reserved() {
        assert!(matches!(
            validate_tag_name("HEAD"),
            Err(TagError::InvalidName { .. })
        ));
        assert!(matches!(
            validate_tag_name("head"),
            Err(TagError::InvalidName { .. })
        ));
    }

    #[test]
    fn test_validate_tag_name_too_long() {
        let long_name = "a".repeat(300);
        assert!(matches!(
            validate_tag_name(&long_name),
            Err(TagError::InvalidName { .. })
        ));
    }

    // Pattern Matching Tests

    #[test]
    fn test_matches_pattern_exact() {
        assert!(matches_pattern("v1.0.0", "v1.0.0"));
        assert!(!matches_pattern("v1.0.0", "v1.0.1"));
    }

    #[test]
    fn test_matches_pattern_wildcard_all() {
        assert!(matches_pattern("anything", "*"));
        assert!(matches_pattern("", "*"));
    }

    #[test]
    fn test_matches_pattern_prefix() {
        assert!(matches_pattern("v1.0.0", "v*"));
        assert!(matches_pattern("v2.0.0", "v*"));
        assert!(!matches_pattern("release", "v*"));
    }

    #[test]
    fn test_matches_pattern_suffix() {
        assert!(matches_pattern("v1.0.0", "*0.0"));
        assert!(!matches_pattern("v1.0.1", "*0.0"));
    }

    #[test]
    fn test_matches_pattern_contains() {
        assert!(matches_pattern("v1.0.0-beta", "*0.0*"));
        assert!(matches_pattern("pre-1.0.0-post", "*0.0*"));
    }

    // Tag File Operations Tests

    #[test]
    fn test_tag_file_path() {
        let tags_dir = Path::new("/repo/.atomic/tags");
        let path = tag_file_path(tags_dir, "main", "v1.0.0");

        assert_eq!(path, PathBuf::from("/repo/.atomic/tags/main/v1.0.0.tag"));
    }

    #[test]
    fn test_stack_tags_dir() {
        let tags_dir = Path::new("/repo/.atomic/tags");
        let path = stack_tags_dir(tags_dir, "feature");

        assert_eq!(path, PathBuf::from("/repo/.atomic/tags/feature"));
    }

    #[test]
    fn test_save_and_load_tag() {
        let temp_dir = TempDir::new().unwrap();
        let tags_dir = temp_dir.path();

        let state = Merkle::of(b"test state");
        let tag = Tag::new("v1.0.0", "main", 42, state);

        // Save
        save_tag(tags_dir, &tag).unwrap();

        // Load
        let loaded = load_tag(tags_dir, "main", "v1.0.0").unwrap().unwrap();

        assert_eq!(loaded.name, "v1.0.0");
        assert_eq!(loaded.sequence, 42);
        assert_eq!(loaded.state, state);
    }

    #[test]
    fn test_save_tag_already_exists() {
        let temp_dir = TempDir::new().unwrap();
        let tags_dir = temp_dir.path();

        let state = Merkle::of(b"test");
        let tag = Tag::new("v1.0.0", "main", 1, state);

        save_tag(tags_dir, &tag).unwrap();
        let result = save_tag(tags_dir, &tag);

        assert!(matches!(result, Err(TagError::AlreadyExists { .. })));
    }

    #[test]
    fn test_save_tag_force() {
        let temp_dir = TempDir::new().unwrap();
        let tags_dir = temp_dir.path();

        let state1 = Merkle::of(b"state1");
        let state2 = Merkle::of(b"state2");
        let tag1 = Tag::new("v1.0.0", "main", 1, state1);
        let tag2 = Tag::new("v1.0.0", "main", 2, state2);

        save_tag(tags_dir, &tag1).unwrap();
        save_tag_force(tags_dir, &tag2, true).unwrap();

        let loaded = load_tag(tags_dir, "main", "v1.0.0").unwrap().unwrap();
        assert_eq!(loaded.sequence, 2);
    }

    #[test]
    fn test_load_tag_not_found() {
        let temp_dir = TempDir::new().unwrap();
        let tags_dir = temp_dir.path();

        let result = load_tag(tags_dir, "main", "nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_load_tag_any_stack() {
        let temp_dir = TempDir::new().unwrap();
        let tags_dir = temp_dir.path();

        let state = Merkle::of(b"test");
        save_tag(tags_dir, &Tag::new("v1.0.0", "main", 1, state)).unwrap();
        save_tag(tags_dir, &Tag::new("v2.0.0", "feature", 2, state)).unwrap();

        // Find tag in main stack
        let tag = load_tag_any_stack(tags_dir, "v1.0.0").unwrap().unwrap();
        assert_eq!(tag.stack, "main");

        // Find tag in feature stack
        let tag = load_tag_any_stack(tags_dir, "v2.0.0").unwrap().unwrap();
        assert_eq!(tag.stack, "feature");

        // Not found in any stack
        let result = load_tag_any_stack(tags_dir, "nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_delete_tag() {
        let temp_dir = TempDir::new().unwrap();
        let tags_dir = temp_dir.path();

        let state = Merkle::of(b"test");
        let tag = Tag::new("v1.0.0", "main", 1, state);

        save_tag(tags_dir, &tag).unwrap();
        assert!(delete_tag(tags_dir, "main", "v1.0.0").unwrap());
        assert!(load_tag(tags_dir, "main", "v1.0.0").unwrap().is_none());
    }

    #[test]
    fn test_delete_tag_not_found() {
        let temp_dir = TempDir::new().unwrap();
        let tags_dir = temp_dir.path();

        assert!(!delete_tag(tags_dir, "main", "nonexistent").unwrap());
    }

    #[test]
    fn test_delete_tag_cleans_empty_stack_dir() {
        let temp_dir = TempDir::new().unwrap();
        let tags_dir = temp_dir.path();

        let state = Merkle::of(b"test");
        save_tag(tags_dir, &Tag::new("v1.0.0", "main", 1, state)).unwrap();

        // Stack directory should exist
        assert!(stack_tags_dir(tags_dir, "main").exists());

        // Delete the only tag
        delete_tag(tags_dir, "main", "v1.0.0").unwrap();

        // Stack directory should be cleaned up
        assert!(!stack_tags_dir(tags_dir, "main").exists());
    }

    #[test]
    fn test_list_tags() {
        let temp_dir = TempDir::new().unwrap();
        let tags_dir = temp_dir.path();

        let state = Merkle::of(b"test");
        save_tag(tags_dir, &Tag::new("v1.0.0", "main", 1, state)).unwrap();
        save_tag(tags_dir, &Tag::new("v2.0.0", "main", 2, state)).unwrap();
        save_tag(tags_dir, &Tag::new("v3.0.0", "main", 3, state)).unwrap();

        let tags = list_tags(tags_dir, "main").unwrap();
        assert_eq!(tags.len(), 3);
    }

    #[test]
    fn test_list_tags_empty_dir() {
        let temp_dir = TempDir::new().unwrap();
        let tags_dir = temp_dir.path();

        let tags = list_tags(tags_dir, "main").unwrap();
        assert!(tags.is_empty());
    }

    #[test]
    fn test_list_tags_nonexistent_dir() {
        let tags_dir = Path::new("/nonexistent/path");

        let tags = list_tags(tags_dir, "main").unwrap();
        assert!(tags.is_empty());
    }

    #[test]
    fn test_list_all_tags() {
        let temp_dir = TempDir::new().unwrap();
        let tags_dir = temp_dir.path();

        let state = Merkle::of(b"test");
        save_tag(tags_dir, &Tag::new("v1.0.0", "main", 1, state)).unwrap();
        save_tag(tags_dir, &Tag::new("v2.0.0", "main", 2, state)).unwrap();
        save_tag(tags_dir, &Tag::new("v1.0.0", "feature", 1, state)).unwrap();

        // list_tags only returns tags from one stack
        let main_tags = list_tags(tags_dir, "main").unwrap();
        assert_eq!(main_tags.len(), 2);

        let feature_tags = list_tags(tags_dir, "feature").unwrap();
        assert_eq!(feature_tags.len(), 1);

        // list_all_tags returns tags from all stacks
        let all_tags = list_all_tags(tags_dir).unwrap();
        assert_eq!(all_tags.len(), 3);
    }

    #[test]
    fn test_list_tag_stacks() {
        let temp_dir = TempDir::new().unwrap();
        let tags_dir = temp_dir.path();

        let state = Merkle::of(b"test");
        save_tag(tags_dir, &Tag::new("v1.0.0", "main", 1, state)).unwrap();
        save_tag(tags_dir, &Tag::new("v1.0.0", "feature", 1, state)).unwrap();
        save_tag(tags_dir, &Tag::new("v1.0.0", "dev", 1, state)).unwrap();

        let stacks = list_tag_stacks(tags_dir).unwrap();
        assert_eq!(stacks.len(), 3);
        assert!(stacks.contains(&"main".to_string()));
        assert!(stacks.contains(&"feature".to_string()));
        assert!(stacks.contains(&"dev".to_string()));
    }

    #[test]
    fn test_same_tag_name_different_stacks() {
        let temp_dir = TempDir::new().unwrap();
        let tags_dir = temp_dir.path();

        let state1 = Merkle::of(b"state1");
        let state2 = Merkle::of(b"state2");

        // Same tag name in different stacks
        save_tag(tags_dir, &Tag::new("release", "main", 10, state1)).unwrap();
        save_tag(tags_dir, &Tag::new("release", "feature", 5, state2)).unwrap();

        // Load from each stack
        let main_tag = load_tag(tags_dir, "main", "release").unwrap().unwrap();
        let feature_tag = load_tag(tags_dir, "feature", "release").unwrap().unwrap();

        assert_eq!(main_tag.sequence, 10);
        assert_eq!(main_tag.state, state1);
        assert_eq!(feature_tag.sequence, 5);
        assert_eq!(feature_tag.state, state2);
    }

    #[test]
    fn test_list_tags_filtered() {
        let temp_dir = TempDir::new().unwrap();
        let tags_dir = temp_dir.path();

        let state = Merkle::of(b"test");
        save_tag(tags_dir, &Tag::new("v1.0.0", "main", 1, state)).unwrap();
        save_tag(tags_dir, &Tag::new("v2.0.0", "main", 2, state).with_message("Annotated")).unwrap();
        save_tag(tags_dir, &Tag::new("release-1", "other", 3, state)).unwrap();

        // Filter by pattern
        let filter = TagFilter::new().pattern("v*");
        let tags = list_tags_filtered(tags_dir, &filter).unwrap();
        assert_eq!(tags.len(), 2);

        // Filter by stack
        let filter = TagFilter::new().stack("main");
        let tags = list_tags_filtered(tags_dir, &filter).unwrap();
        assert_eq!(tags.len(), 2);

        // Filter annotated only
        let filter = TagFilter::new().annotated_only();
        let tags = list_tags_filtered(tags_dir, &filter).unwrap();
        assert_eq!(tags.len(), 1);
    }

    #[test]
    fn test_list_tags_sorted() {
        let temp_dir = TempDir::new().unwrap();
        let tags_dir = temp_dir.path();

        let state = Merkle::of(b"test");
        save_tag(tags_dir, &Tag::new("b-tag", "main", 2, state)).unwrap();
        save_tag(tags_dir, &Tag::new("a-tag", "main", 1, state)).unwrap();
        save_tag(tags_dir, &Tag::new("c-tag", "main", 3, state)).unwrap();

        // Sort by name
        let filter = TagFilter::new().sort(TagSort::Name);
        let tags = list_tags_filtered(tags_dir, &filter).unwrap();
        assert_eq!(tags[0].name, "a-tag");
        assert_eq!(tags[1].name, "b-tag");
        assert_eq!(tags[2].name, "c-tag");

        // Sort by sequence
        let filter = TagFilter::new().sort(TagSort::Sequence);
        let tags = list_tags_filtered(tags_dir, &filter).unwrap();
        assert_eq!(tags[0].sequence, 3);
        assert_eq!(tags[1].sequence, 2);
        assert_eq!(tags[2].sequence, 1);
    }

    #[test]
    fn test_list_tags_with_limit() {
        let temp_dir = TempDir::new().unwrap();
        let tags_dir = temp_dir.path();

        let state = Merkle::of(b"test");
        for i in 0..10 {
            save_tag(tags_dir, &Tag::new(format!("v{}", i), "main", i, state)).unwrap();
        }

        let filter = TagFilter::new().limit(5);
        let tags = list_tags_filtered(tags_dir, &filter).unwrap();
        assert_eq!(tags.len(), 5);
    }

    #[test]
    fn test_count_tags() {
        let temp_dir = TempDir::new().unwrap();
        let tags_dir = temp_dir.path();

        assert_eq!(count_tags(tags_dir, "main").unwrap(), 0);

        let state = Merkle::of(b"test");
        save_tag(tags_dir, &Tag::new("v1", "main", 1, state)).unwrap();
        save_tag(tags_dir, &Tag::new("v2", "main", 2, state)).unwrap();

        assert_eq!(count_tags(tags_dir, "main").unwrap(), 2);
    }

    #[test]
    fn test_count_all_tags() {
        let temp_dir = TempDir::new().unwrap();
        let tags_dir = temp_dir.path();

        assert_eq!(count_all_tags(tags_dir).unwrap(), 0);

        let state = Merkle::of(b"test");
        save_tag(tags_dir, &Tag::new("v1", "main", 1, state)).unwrap();
        save_tag(tags_dir, &Tag::new("v2", "main", 2, state)).unwrap();
        save_tag(tags_dir, &Tag::new("v1", "feature", 1, state)).unwrap();

        assert_eq!(count_tags(tags_dir, "main").unwrap(), 2);
        assert_eq!(count_tags(tags_dir, "feature").unwrap(), 1);
        assert_eq!(count_all_tags(tags_dir).unwrap(), 3);
    }

    // TagError Tests

    #[test]
    fn test_tag_error_display() {
        let err = TagError::AlreadyExists { name: "v1.0.0".to_string() };
        assert!(format!("{}", err).contains("v1.0.0"));

        let err = TagError::NotFound { name: "missing".to_string() };
        assert!(format!("{}", err).contains("missing"));

        let err = TagError::InvalidName {
            name: "bad/name".to_string(),
            reason: "contains slash".to_string(),
        };
        assert!(format!("{}", err).contains("bad/name"));
    }

    // TagSort Tests

    #[test]
    fn test_tag_sort_default() {
        let sort = TagSort::default();
        assert_eq!(sort, TagSort::Name);
    }

    #[test]
    fn test_tag_sort_equality() {
        assert_eq!(TagSort::Name, TagSort::Name);
        assert_ne!(TagSort::Name, TagSort::Timestamp);
    }
}
