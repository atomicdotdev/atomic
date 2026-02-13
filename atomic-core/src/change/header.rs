//! Change header and author types
//!
//! The header contains all metadata about a change:
//! - Commit message and optional description
//! - Timestamp (when the change was created)
//! - Author information
//!
//! This metadata is included in the hashed portion of a change,
//! meaning it contributes to the change's identity hash.
//!
//! # Example
//!
//! ```rust
//! use atomic_core::change::{Author, ChangeHeader};
//!
//! let author = Author::new("Alice", Some("alice@example.com"));
//!
//! let header = ChangeHeader::builder()
//!     .message("Add new feature")
//!     .description("This implements the widget system")
//!     .author(author)
//!     .build();
//!
//! assert_eq!(header.message, "Add new feature");
//! assert!(header.description.is_some());
//! ```

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Author information for a change.
///
/// An author represents a person (or entity) who created a change.
/// At minimum, an author has a name; email and identity key are optional.
///
/// # Identity Key
///
/// The `identity` field can contain a reference to a cryptographic identity
/// (e.g., an Ed25519 public key in base32). This allows verifying that a
/// change was actually created by the claimed author.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Author {
    /// Display name of the author (required)
    pub name: String,

    /// Email address (optional)
    #[serde(default)]
    pub email: Option<String>,

    /// Cryptographic identity reference (optional)
    ///
    /// This can be a public key hash or identity identifier that can be
    /// used to verify the author's signature on changes.
    #[serde(default)]
    pub identity: Option<String>,
}

impl Author {
    /// Create a new author with name and optional email.
    ///
    /// # Arguments
    ///
    /// * `name` - The author's display name
    /// * `email` - Optional email address
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::Author;
    ///
    /// let author = Author::new("Alice", Some("alice@example.com"));
    /// assert_eq!(author.name, "Alice");
    /// assert_eq!(author.email, Some("alice@example.com".to_string()));
    ///
    /// let author = Author::new("Bob", None::<String>);
    /// assert!(author.email.is_none());
    /// ```
    pub fn new(name: impl Into<String>, email: Option<impl Into<String>>) -> Self {
        Self {
            name: name.into(),
            email: email.map(Into::into),
            identity: None,
        }
    }

    /// Create an author with an associated cryptographic identity.
    ///
    /// # Arguments
    ///
    /// * `name` - The author's display name
    /// * `email` - Optional email address
    /// * `identity` - Identity key reference (e.g., base32-encoded public key)
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::Author;
    ///
    /// let author = Author::with_identity(
    ///     "Alice",
    ///     Some("alice@example.com"),
    ///     "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567ABCDEFGHIJKLMNOPQRST",
    /// );
    /// assert!(author.identity.is_some());
    /// ```
    pub fn with_identity(
        name: impl Into<String>,
        email: Option<impl Into<String>>,
        identity: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            email: email.map(Into::into),
            identity: Some(identity.into()),
        }
    }

    /// Check if this author has an associated identity.
    #[inline]
    pub fn has_identity(&self) -> bool {
        self.identity.is_some()
    }

    /// Get a short display string for this author.
    ///
    /// Returns "Name <email>" if email is present, otherwise just "Name".
    pub fn display_short(&self) -> String {
        match &self.email {
            Some(email) => format!("{} <{}>", self.name, email),
            None => self.name.clone(),
        }
    }
}

impl Default for Author {
    fn default() -> Self {
        Self {
            name: String::new(),
            email: None,
            identity: None,
        }
    }
}

impl fmt::Display for Author {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.display_short())
    }
}

impl From<&str> for Author {
    /// Parse an author from a string like "Name <email>".
    ///
    /// If no email is found, the entire string is used as the name.
    fn from(s: &str) -> Self {
        // Try to parse "Name <email>" format
        if let Some(start) = s.find('<') {
            if let Some(end) = s.find('>') {
                if start < end {
                    let name = s[..start].trim().to_string();
                    let email = s[start + 1..end].trim().to_string();
                    return Self {
                        name,
                        email: Some(email),
                        identity: None,
                    };
                }
            }
        }

        // No email found, use entire string as name
        Self {
            name: s.trim().to_string(),
            email: None,
            identity: None,
        }
    }
}

impl From<String> for Author {
    fn from(s: String) -> Self {
        Author::from(s.as_str())
    }
}

/// Metadata header for a change.
///
/// The header contains human-readable information about a change:
/// - A required message (like a git commit message)
/// - An optional longer description
/// - The timestamp when the change was created
/// - A list of authors
///
/// # Builder Pattern
///
/// For convenience, use `ChangeHeader::builder()` to construct headers:
///
/// ```rust
/// use atomic_core::change::ChangeHeader;
///
/// let header = ChangeHeader::builder()
///     .message("Fix bug #123")
///     .build();
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeHeader {
    /// Short summary of the change (required, like git commit subject)
    pub message: String,

    /// Longer description (optional, like git commit body)
    #[serde(default)]
    pub description: Option<String>,

    /// When the change was created
    pub timestamp: DateTime<Utc>,

    /// List of authors (can be empty, but typically has one)
    #[serde(default)]
    pub authors: Vec<Author>,
}

impl ChangeHeader {
    /// Create a new change header with just a message.
    ///
    /// The timestamp is set to the current time.
    ///
    /// # Arguments
    ///
    /// * `message` - The change message
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::ChangeHeader;
    ///
    /// let header = ChangeHeader::new("Add feature X");
    /// assert_eq!(header.message, "Add feature X");
    /// assert!(header.authors.is_empty());
    /// ```
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            description: None,
            timestamp: Utc::now(),
            authors: Vec::new(),
        }
    }

    /// Create a builder for constructing a change header.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use atomic_core::change::{ChangeHeader, Author};
    ///
    /// let header = ChangeHeader::builder()
    ///     .message("Refactor widget system")
    ///     .description("Split into smaller modules for maintainability")
    ///     .author(Author::new("Alice", None::<String>))
    ///     .build();
    /// ```
    pub fn builder() -> ChangeHeaderBuilder {
        ChangeHeaderBuilder::default()
    }

    /// Check if this header has a description.
    #[inline]
    pub fn has_description(&self) -> bool {
        self.description.is_some()
    }

    /// Check if this header has any authors.
    #[inline]
    pub fn has_authors(&self) -> bool {
        !self.authors.is_empty()
    }

    /// Get the first author, if any.
    pub fn first_author(&self) -> Option<&Author> {
        self.authors.first()
    }

    /// Get a summary string for this header.
    ///
    /// Returns the message truncated to the first line.
    pub fn summary(&self) -> &str {
        self.message.lines().next().unwrap_or(&self.message)
    }

    /// Format the header for display.
    ///
    /// Returns a multi-line string suitable for showing to users.
    pub fn format_display(&self) -> String {
        let mut result = String::new();

        // Authors
        for author in &self.authors {
            result.push_str(&format!("Author: {}\n", author));
        }

        // Timestamp
        result.push_str(&format!("Date:   {}\n", self.timestamp.format("%Y-%m-%d %H:%M:%S UTC")));

        // Message
        result.push_str(&format!("\n    {}\n", self.message));

        // Description
        if let Some(ref desc) = self.description {
            result.push('\n');
            for line in desc.lines() {
                result.push_str(&format!("    {}\n", line));
            }
        }

        result
    }
}

impl Default for ChangeHeader {
    fn default() -> Self {
        Self {
            message: String::new(),
            description: None,
            timestamp: Utc::now(),
            authors: Vec::new(),
        }
    }
}

impl fmt::Display for ChangeHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.summary())
    }
}

/// Builder for constructing `ChangeHeader` instances.
///
/// Use `ChangeHeader::builder()` to create a builder.
#[derive(Clone, Debug, Default)]
pub struct ChangeHeaderBuilder {
    message: Option<String>,
    description: Option<String>,
    timestamp: Option<DateTime<Utc>>,
    authors: Vec<Author>,
}

impl ChangeHeaderBuilder {
    /// Set the change message (required).
    pub fn message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    /// Set an optional description.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Set the timestamp (defaults to now if not specified).
    pub fn timestamp(mut self, timestamp: DateTime<Utc>) -> Self {
        self.timestamp = Some(timestamp);
        self
    }

    /// Add an author.
    pub fn author(mut self, author: Author) -> Self {
        self.authors.push(author);
        self
    }

    /// Add multiple authors.
    pub fn authors(mut self, authors: impl IntoIterator<Item = Author>) -> Self {
        self.authors.extend(authors);
        self
    }

    /// Build the change header.
    ///
    /// # Panics
    ///
    /// Panics if no message has been set.
    pub fn build(self) -> ChangeHeader {
        ChangeHeader {
            message: self.message.unwrap_or_default(),
            description: self.description,
            timestamp: self.timestamp.unwrap_or_else(Utc::now),
            authors: self.authors,
        }
    }

    /// Try to build the change header, returning an error if invalid.
    ///
    /// This is a non-panicking alternative to `build()`.
    pub fn try_build(self) -> Result<ChangeHeader, &'static str> {
        if self.message.is_none() || self.message.as_ref().map(|m| m.is_empty()).unwrap_or(true) {
            return Err("Change message is required");
        }

        Ok(ChangeHeader {
            message: self.message.unwrap(),
            description: self.description,
            timestamp: self.timestamp.unwrap_or_else(Utc::now),
            authors: self.authors,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // Author Tests
    // ========================================================================

    #[test]
    fn test_author_new() {
        let author = Author::new("Alice", Some("alice@example.com"));
        assert_eq!(author.name, "Alice");
        assert_eq!(author.email, Some("alice@example.com".to_string()));
        assert!(author.identity.is_none());
    }

    #[test]
    fn test_author_new_no_email() {
        let author = Author::new("Bob", None::<String>);
        assert_eq!(author.name, "Bob");
        assert!(author.email.is_none());
    }

    #[test]
    fn test_author_with_identity() {
        let author = Author::with_identity("Alice", Some("alice@example.com"), "KEYABCDEF");
        assert!(author.has_identity());
        assert_eq!(author.identity, Some("KEYABCDEF".to_string()));
    }

    #[test]
    fn test_author_display_short() {
        let with_email = Author::new("Alice", Some("alice@example.com"));
        assert_eq!(with_email.display_short(), "Alice <alice@example.com>");

        let without_email = Author::new("Bob", None::<String>);
        assert_eq!(without_email.display_short(), "Bob");
    }

    #[test]
    fn test_author_display_trait() {
        let author = Author::new("Alice", Some("alice@example.com"));
        assert_eq!(format!("{}", author), "Alice <alice@example.com>");
    }

    #[test]
    fn test_author_from_str_with_email() {
        let author: Author = "Alice <alice@example.com>".into();
        assert_eq!(author.name, "Alice");
        assert_eq!(author.email, Some("alice@example.com".to_string()));
    }

    #[test]
    fn test_author_from_str_without_email() {
        let author: Author = "Just a name".into();
        assert_eq!(author.name, "Just a name");
        assert!(author.email.is_none());
    }

    #[test]
    fn test_author_from_str_with_spaces() {
        let author: Author = "  Alice Smith  <  alice@example.com  >".into();
        assert_eq!(author.name, "Alice Smith");
        assert_eq!(author.email, Some("alice@example.com".to_string()));
    }

    #[test]
    fn test_author_default() {
        let author = Author::default();
        assert!(author.name.is_empty());
        assert!(author.email.is_none());
        assert!(author.identity.is_none());
    }

    #[test]
    fn test_author_equality() {
        let a1 = Author::new("Alice", Some("alice@example.com"));
        let a2 = Author::new("Alice", Some("alice@example.com"));
        let a3 = Author::new("Alice", None::<String>);

        assert_eq!(a1, a2);
        assert_ne!(a1, a3);
    }

    #[test]
    fn test_author_json_roundtrip() {
        let author = Author::new("Alice", Some("alice@example.com"));
        let json = serde_json::to_string(&author).unwrap();
        let parsed: Author = serde_json::from_str(&json).unwrap();
        assert_eq!(author, parsed);
    }

    #[test]
    fn test_author_json_minimal() {
        // JSON with just name should work
        let json = r#"{"name": "Bob"}"#;
        let author: Author = serde_json::from_str(json).unwrap();
        assert_eq!(author.name, "Bob");
        assert!(author.email.is_none());
    }

    #[test]
    fn test_author_json_with_identity() {
        let author = Author::with_identity("Alice", Some("alice@example.com"), "KEY123");
        let json = serde_json::to_string(&author).unwrap();
        let parsed: Author = serde_json::from_str(&json).unwrap();
        assert_eq!(author, parsed);
    }

    // ========================================================================
    // ChangeHeader Tests
    // ========================================================================

    #[test]
    fn test_header_new() {
        let header = ChangeHeader::new("Test message");
        assert_eq!(header.message, "Test message");
        assert!(header.description.is_none());
        assert!(header.authors.is_empty());
    }

    #[test]
    fn test_header_builder_basic() {
        let header = ChangeHeader::builder()
            .message("Add feature")
            .build();
        assert_eq!(header.message, "Add feature");
    }

    #[test]
    fn test_header_builder_full() {
        let header = ChangeHeader::builder()
            .message("Add feature")
            .description("This adds the widget feature")
            .author(Author::new("Alice", Some("alice@example.com")))
            .author(Author::new("Bob", None::<String>))
            .build();

        assert_eq!(header.message, "Add feature");
        assert_eq!(header.description, Some("This adds the widget feature".to_string()));
        assert_eq!(header.authors.len(), 2);
    }

    #[test]
    fn test_header_builder_with_timestamp() {
        let ts = DateTime::parse_from_rfc3339("2024-01-15T10:30:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let header = ChangeHeader::builder()
            .message("Test")
            .timestamp(ts)
            .build();

        assert_eq!(header.timestamp, ts);
    }

    #[test]
    fn test_header_try_build_success() {
        let result = ChangeHeader::builder()
            .message("Valid message")
            .try_build();
        assert!(result.is_ok());
    }

    #[test]
    fn test_header_try_build_empty_message() {
        let result = ChangeHeader::builder()
            .message("")
            .try_build();
        assert!(result.is_err());
    }

    #[test]
    fn test_header_try_build_no_message() {
        let result = ChangeHeader::builder().try_build();
        assert!(result.is_err());
    }

    #[test]
    fn test_header_has_description() {
        let without = ChangeHeader::new("Test");
        assert!(!without.has_description());

        let with = ChangeHeader::builder()
            .message("Test")
            .description("Details")
            .build();
        assert!(with.has_description());
    }

    #[test]
    fn test_header_has_authors() {
        let without = ChangeHeader::new("Test");
        assert!(!without.has_authors());

        let with = ChangeHeader::builder()
            .message("Test")
            .author(Author::new("Alice", None::<String>))
            .build();
        assert!(with.has_authors());
    }

    #[test]
    fn test_header_first_author() {
        let header = ChangeHeader::builder()
            .message("Test")
            .author(Author::new("Alice", None::<String>))
            .author(Author::new("Bob", None::<String>))
            .build();

        assert_eq!(header.first_author().unwrap().name, "Alice");
    }

    #[test]
    fn test_header_first_author_none() {
        let header = ChangeHeader::new("Test");
        assert!(header.first_author().is_none());
    }

    #[test]
    fn test_header_summary() {
        let single_line = ChangeHeader::new("Simple message");
        assert_eq!(single_line.summary(), "Simple message");

        let multi_line = ChangeHeader::new("First line\nSecond line\nThird line");
        assert_eq!(multi_line.summary(), "First line");
    }

    #[test]
    fn test_header_display() {
        let header = ChangeHeader::new("Short message");
        assert_eq!(format!("{}", header), "Short message");
    }

    #[test]
    fn test_header_format_display() {
        let header = ChangeHeader::builder()
            .message("Test message")
            .author(Author::new("Alice", Some("alice@example.com")))
            .build();

        let formatted = header.format_display();
        assert!(formatted.contains("Author: Alice <alice@example.com>"));
        assert!(formatted.contains("Test message"));
    }

    #[test]
    fn test_header_default() {
        let header = ChangeHeader::default();
        assert!(header.message.is_empty());
        assert!(header.description.is_none());
        assert!(header.authors.is_empty());
    }

    #[test]
    fn test_header_json_roundtrip() {
        let header = ChangeHeader::builder()
            .message("Test message")
            .description("Detailed description")
            .author(Author::new("Alice", Some("alice@example.com")))
            .build();

        let json = serde_json::to_string(&header).unwrap();
        let parsed: ChangeHeader = serde_json::from_str(&json).unwrap();

        assert_eq!(header.message, parsed.message);
        assert_eq!(header.description, parsed.description);
        assert_eq!(header.authors.len(), parsed.authors.len());
    }

    #[test]
    fn test_header_json_minimal() {
        // Minimal JSON should work
        let json = r#"{"message": "Test", "timestamp": "2024-01-15T10:30:00Z"}"#;
        let header: ChangeHeader = serde_json::from_str(json).unwrap();
        assert_eq!(header.message, "Test");
        assert!(header.description.is_none());
        assert!(header.authors.is_empty());
    }

    #[test]
    fn test_header_json_full_roundtrip() {
        let header = ChangeHeader::builder()
            .message("Full JSON test")
            .description("Testing full JSON serialization")
            .author(Author::new("Alice", Some("alice@example.com")))
            .author(Author::new("Bob", None::<String>))
            .build();

        let json = serde_json::to_string_pretty(&header).unwrap();
        let parsed: ChangeHeader = serde_json::from_str(&json).unwrap();

        assert_eq!(header.message, parsed.message);
        assert_eq!(header.description, parsed.description);
        assert_eq!(header.authors.len(), parsed.authors.len());
    }

    // ========================================================================
    // Builder Tests
    // ========================================================================

    #[test]
    fn test_builder_authors_iterator() {
        let authors = vec![
            Author::new("Alice", None::<String>),
            Author::new("Bob", None::<String>),
        ];

        let header = ChangeHeader::builder()
            .message("Test")
            .authors(authors)
            .build();

        assert_eq!(header.authors.len(), 2);
    }

    #[test]
    fn test_builder_chaining() {
        // Ensure all builder methods return Self for chaining
        let _header = ChangeHeader::builder()
            .message("Test")
            .description("Desc")
            .timestamp(Utc::now())
            .author(Author::default())
            .authors(vec![])
            .build();
    }
}
