//! Tag types and trait definitions for Atomic VCS.
//!
//! Tags are first-class entities in the pristine database, stored in the
//! `TAG_RECORDS` sub-table alongside the entity identity layer (INTERNAL,
//! EXTERNAL, NODE_TYPES). This module defines the tag data types and
//! read/write trait methods.

use crate::change::Author;
use crate::pristine::PristineError;
use crate::types::{Base32, Hash, Merkle, NodeId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ============================================================================
// TAG TYPES
// ============================================================================

/// A tag stored in the pristine database.
///
/// Serialized with postcard into the `TAG_RECORDS` table. The entity identity
/// layer (INTERNAL, EXTERNAL, NODE_TYPES with TAG=1) handles hash-to-id
/// mapping and type discrimination. This struct holds the tag-specific payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagRecord {
    /// Tag name (e.g., "v1.2.3", "pr-42").
    pub name: String,
    /// View this tag belongs to.
    pub view: String,
    /// Sequence number in the view when tagged.
    pub sequence: u64,
    /// Merkle state at the tagged point.
    pub state: Merkle,
    /// Hash of the change at this sequence position.
    pub change_hash: Hash,
    /// When the tag was created.
    pub timestamp: DateTime<Utc>,
    /// Who created the tag.
    pub author: Option<Author>,
    /// Human-readable message.
    pub message: Option<String>,
    /// What kind of tag this is.
    pub kind: TagKind,
    /// Extensible metadata (not included in content hash).
    /// Carries Git provenance, CI status, review approvals, etc.
    ///
    /// Stored as JSON text because postcard (the TAG_RECORDS codec) is not
    /// self-describing and cannot round-trip `serde_json::Value` directly.
    #[serde(with = "json_metadata")]
    pub metadata: Option<serde_json::Value>,
}

/// Serde adapter that stores `Option<serde_json::Value>` as an
/// `Option<String>` of JSON text. Postcard handles the string fine;
/// `None` records (the pre-metadata format) decode unchanged.
mod json_metadata {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(
        value: &Option<serde_json::Value>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(v) => serializer.serialize_some(&v.to_string()),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<serde_json::Value>, D::Error> {
        let opt: Option<String> = Option::deserialize(deserializer)?;
        match opt {
            Some(s) => serde_json::from_str(&s)
                .map(Some)
                .map_err(serde::de::Error::custom),
            None => Ok(None),
        }
    }
}

/// What the tag represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TagKind {
    /// Named release bookmark (e.g., "v1.2.3").
    Release = 0,
    /// Review attestation — marks changes as reviewed and approved.
    /// Created by incremental git import when a merge/squash is detected.
    ReviewGate = 1,
    /// Custom/user-defined.
    Custom = 2,
}

impl TagRecord {
    /// Compute the content hash of this tag.
    ///
    /// Covers identity + position + annotation fields. The `metadata` field
    /// is excluded (same pattern as `Change.unhashed`).
    pub fn content_hash(&self) -> Hash {
        let mut hasher = blake3::Hasher::new();
        hasher.update(self.name.as_bytes());
        hasher.update(self.view.as_bytes());
        hasher.update(&self.sequence.to_le_bytes());
        hasher.update(self.state.as_bytes());
        hasher.update(self.change_hash.as_bytes());
        hasher.update(&self.timestamp.timestamp().to_le_bytes());
        if let Some(ref author) = self.author {
            hasher.update(author.name.as_bytes());
            if let Some(ref email) = author.email {
                hasher.update(email.as_bytes());
            }
        }
        if let Some(ref message) = self.message {
            hasher.update(message.as_bytes());
        }
        hasher.update(&[self.kind as u8]);
        Hash::from_bytes(*hasher.finalize().as_bytes())
    }

    /// Check if this is an annotated tag (has message and/or author).
    pub fn is_annotated(&self) -> bool {
        self.message.is_some() || self.author.is_some()
    }
}

impl std::fmt::Display for TagRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} -> {} (seq: {}, view: {})",
            self.name,
            &self.state.to_base32()[..8],
            self.sequence,
            self.view
        )
    }
}

impl std::fmt::Display for TagKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TagKind::Release => write!(f, "release"),
            TagKind::ReviewGate => write!(f, "review-gate"),
            TagKind::Custom => write!(f, "custom"),
        }
    }
}

// ============================================================================
// TAG TRAIT METHODS
// ============================================================================

/// Read-only tag operations, implemented by ReadTxn and WriteTxn.
pub trait TagTxnT {
    /// Look up a named tag by view and name.
    fn get_tag(&self, view: &str, name: &str) -> Result<Option<TagRecord>, PristineError>;

    /// List all named tags for a view.
    fn list_tags(&self, view: &str) -> Result<Vec<TagRecord>, PristineError>;

    /// List all named tags across all views.
    fn list_all_tags(&self) -> Result<Vec<TagRecord>, PristineError>;

    /// Find a tag by its content hash.
    fn find_tag_by_hash(&self, hash: &Hash) -> Result<Option<TagRecord>, PristineError>;
}

/// Write tag operations, implemented by WriteTxn.
pub trait TagMutTxnT: TagTxnT {
    /// Create or overwrite a named tag.
    ///
    /// 1. Computes content hash
    /// 2. Registers entity via INTERNAL/EXTERNAL/NODE_TYPES (TAG=1)
    /// 3. Writes TagRecord to TAG_RECORDS
    /// 4. Writes TAG_NAME_INDEX entry
    ///
    /// Returns the entity NodeId.
    fn put_tag(&mut self, tag: &TagRecord) -> Result<NodeId, PristineError>;

    /// Delete a named tag by view and name.
    ///
    /// Removes from TAG_RECORDS and TAG_NAME_INDEX.
    /// Returns true if the tag existed and was deleted.
    fn del_tag(&mut self, view: &str, name: &str) -> Result<bool, PristineError>;

    /// Delete all tags belonging to a view.
    ///
    /// Used by `del_view` cleanup.
    fn del_tags_for_view(&mut self, view: &str) -> Result<usize, PristineError>;
}

// ============================================================================
// GIT SHA INDEX TRAITS
// ============================================================================

/// Read-only git SHA index operations.
pub trait GitShaIndexTxnT {
    /// Look up an Atomic entity by its Git commit SHA.
    ///
    /// The SHA should be the full 40-character hex string.
    fn get_by_git_sha(&self, sha: &str) -> Result<Option<NodeId>, PristineError>;

    /// Check if a Git SHA has been indexed.
    fn has_git_sha(&self, sha: &str) -> Result<bool, PristineError>;

    /// Look up by SHA prefix (7+ chars). Returns the matching NodeId
    /// or an error if the prefix is ambiguous.
    fn find_by_git_sha_prefix(&self, prefix: &str) -> Result<Option<NodeId>, PristineError>;
}

/// Write git SHA index operations.
pub trait GitShaIndexMutTxnT: GitShaIndexTxnT {
    /// Record a Git SHA → Atomic entity mapping.
    fn put_git_sha(&mut self, sha: &str, entity_id: NodeId) -> Result<(), PristineError>;

    /// Remove a Git SHA mapping.
    fn del_git_sha(&mut self, sha: &str) -> Result<bool, PristineError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_tag(metadata: Option<serde_json::Value>) -> TagRecord {
        TagRecord {
            name: "pr-99".to_string(),
            view: "main".to_string(),
            sequence: 42,
            state: Merkle::of(b"state"),
            change_hash: Hash::of(b"change"),
            timestamp: Utc::now(),
            author: None,
            message: Some("Squash merge abc123".to_string()),
            kind: TagKind::ReviewGate,
            metadata,
        }
    }

    #[test]
    fn test_tag_record_postcard_roundtrip_with_metadata() {
        let metadata = serde_json::json!({
            "git": { "sha": "abc123", "merge_strategy": "squash", "pr_number": 99 },
            "changes": { "original_hashes": ["AAA", "BBB"] }
        });
        let tag = test_tag(Some(metadata.clone()));

        let bytes = postcard::to_allocvec(&tag).unwrap();
        let decoded: TagRecord = postcard::from_bytes(&bytes).unwrap();

        assert_eq!(decoded, tag);
        assert_eq!(decoded.metadata, Some(metadata));
    }

    #[test]
    fn test_tag_record_postcard_roundtrip_without_metadata() {
        let tag = test_tag(None);
        let bytes = postcard::to_allocvec(&tag).unwrap();
        let decoded: TagRecord = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, tag);
        assert!(decoded.metadata.is_none());
    }
}
