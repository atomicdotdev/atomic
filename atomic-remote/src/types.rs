//! Core types for the Atomic remote protocol.
//!
//! This module defines the fundamental data structures used to communicate
//! with remote Atomic repositories. These types are protocol-agnostic and
//! can be used with HTTP, SSH, or local transports.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

// NodeType

/// The type of a node in the Atomic DAG.
///
/// Atomic's history is represented as a Directed Acyclic Graph (DAG) where
/// nodes can be either changes (patches) or tags (named snapshots).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeType {
    /// A change (patch) that modifies the repository state.
    Change,
    /// A tag marking a specific state in the repository.
    Tag,
}

impl NodeType {
    /// Get the protocol marker character for this node type.
    ///
    /// Used in changelist protocol responses to distinguish changes from tags.
    pub fn marker(&self) -> char {
        match self {
            Self::Change => 'C',
            Self::Tag => 'T',
        }
    }

    /// Parse a node type from its protocol marker.
    pub fn from_marker(c: char) -> Option<Self> {
        match c {
            'C' => Some(Self::Change),
            'T' => Some(Self::Tag),
            _ => None,
        }
    }
}

impl fmt::Display for NodeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Change => write!(f, "change"),
            Self::Tag => write!(f, "tag"),
        }
    }
}

impl FromStr for NodeType {
    type Err = ParseNodeTypeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "change" | "c" => Ok(Self::Change),
            "tag" | "t" => Ok(Self::Tag),
            _ => Err(ParseNodeTypeError(s.to_string())),
        }
    }
}

/// Error when parsing a node type from a string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseNodeTypeError(String);

impl fmt::Display for ParseNodeTypeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid node type: '{}' (expected 'change' or 'tag')",
            self.0
        )
    }
}

impl std::error::Error for ParseNodeTypeError {}

// Node

/// A node in the Atomic DAG, representing either a change or a tag.
///
/// Each node is identified by a hash and carries the Merkle state of the
/// channel after this node was applied.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Node {
    /// The hash identifying this node (base32 encoded).
    pub hash: String,

    /// The type of this node.
    pub node_type: NodeType,

    /// The Merkle state after this node was applied (base32 encoded).
    pub state: String,
}

impl Node {
    /// Create a new change node.
    pub fn change(hash: impl Into<String>, state: impl Into<String>) -> Self {
        Self {
            hash: hash.into(),
            node_type: NodeType::Change,
            state: state.into(),
        }
    }

    /// Create a new tag node.
    pub fn tag(hash: impl Into<String>, state: impl Into<String>) -> Self {
        Self {
            hash: hash.into(),
            node_type: NodeType::Tag,
            state: state.into(),
        }
    }

    /// Check if this node is a change.
    pub fn is_change(&self) -> bool {
        self.node_type == NodeType::Change
    }

    /// Check if this node is a tag.
    pub fn is_tag(&self) -> bool {
        self.node_type == NodeType::Tag
    }
}

impl fmt::Display for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({} -> {})", self.node_type, self.hash, self.state)
    }
}

// ChangelistEntry

/// An entry in a channel's changelist.
///
/// The changelist is an ordered log of all changes applied to a channel.
/// Each entry records the sequence number, change hash, resulting state,
/// and whether the entry is tagged.
///
/// Protocol format:
/// - Regular: `{sequence}.{hash}.{merkle}`
/// - Tagged:  `{sequence}.{hash}.{merkle}.` (trailing dot)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangelistEntry {
    /// The sequence number of this entry (0-indexed position in the log).
    pub sequence: u64,

    /// The hash of the change (base32 encoded).
    pub hash: String,

    /// The Merkle state after this change was applied (base32 encoded).
    pub merkle: String,

    /// Whether this entry is tagged.
    pub tagged: bool,
}

impl ChangelistEntry {
    /// Create a new changelist entry.
    pub fn new(
        sequence: u64,
        hash: impl Into<String>,
        merkle: impl Into<String>,
        tagged: bool,
    ) -> Self {
        Self {
            sequence,
            hash: hash.into(),
            merkle: merkle.into(),
            tagged,
        }
    }

    /// Parse a changelist entry from the protocol format.
    ///
    /// Format: `{sequence}.{hash}.{merkle}` or `{sequence}.{hash}.{merkle}.`
    ///
    /// # Examples
    ///
    /// ```
    /// use atomic_remote::types::ChangelistEntry;
    ///
    /// let entry = ChangelistEntry::parse("0.ABC123.DEF456").unwrap();
    /// assert_eq!(entry.sequence, 0);
    /// assert_eq!(entry.hash, "ABC123");
    /// assert_eq!(entry.merkle, "DEF456");
    /// assert!(!entry.tagged);
    ///
    /// let tagged = ChangelistEntry::parse("1.GHI789.JKL012.").unwrap();
    /// assert!(tagged.tagged);
    /// ```
    pub fn parse(line: &str) -> Result<Self, ParseChangelistError> {
        let line = line.trim();
        if line.is_empty() {
            return Err(ParseChangelistError::Empty);
        }

        // Check for trailing dot (tagged entry)
        let (content, tagged) = if line.ends_with('.') {
            // Could be tagged (4 parts with empty last) or just ending in dot
            let trimmed = line.trim_end_matches('.');
            // Count dots in trimmed content
            let dot_count = trimmed.chars().filter(|&c| c == '.').count();
            if dot_count >= 2 {
                (trimmed, true)
            } else {
                (line, false)
            }
        } else {
            (line, false)
        };

        let parts: Vec<&str> = content.split('.').collect();

        if parts.len() < 3 {
            return Err(ParseChangelistError::InvalidFormat(line.to_string()));
        }

        let sequence: u64 = parts[0]
            .parse()
            .map_err(|_| ParseChangelistError::InvalidSequence(parts[0].to_string()))?;

        let hash = parts[1].to_string();
        let merkle = parts[2].to_string();

        if hash.is_empty() {
            return Err(ParseChangelistError::InvalidFormat(line.to_string()));
        }
        if merkle.is_empty() {
            return Err(ParseChangelistError::InvalidFormat(line.to_string()));
        }

        Ok(Self {
            sequence,
            hash,
            merkle,
            tagged,
        })
    }

    /// Format this entry as a protocol line.
    pub fn to_protocol_line(&self) -> String {
        if self.tagged {
            format!("{}.{}.{}.", self.sequence, self.hash, self.merkle)
        } else {
            format!("{}.{}.{}", self.sequence, self.hash, self.merkle)
        }
    }

    /// Convert this entry to a Node.
    ///
    /// Respects the `tagged` field: tagged entries become `Node::tag`,
    /// regular entries become `Node::change`.
    pub fn to_node(&self) -> Node {
        if self.tagged {
            Node::tag(&self.hash, &self.merkle)
        } else {
            Node::change(&self.hash, &self.merkle)
        }
    }
}

impl fmt::Display for ChangelistEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_protocol_line())
    }
}

/// Error when parsing a changelist entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseChangelistError {
    /// The line was empty.
    Empty,
    /// The line format was invalid.
    InvalidFormat(String),
    /// The sequence number was invalid.
    InvalidSequence(String),
}

impl fmt::Display for ParseChangelistError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "empty changelist line"),
            Self::InvalidFormat(s) => write!(f, "invalid changelist format: '{}'", s),
            Self::InvalidSequence(s) => write!(f, "invalid sequence number: '{}'", s),
        }
    }
}

impl std::error::Error for ParseChangelistError {}

// StateResponse

/// Response from a state query.
///
/// When querying a channel's state, the server returns the current position,
/// the Merkle state at that position, and optionally the Merkle state of
/// the most recent tag.
///
/// Protocol format: `{position} {merkle} {tag_merkle} [{set_id}]` or `-` for
/// an empty channel. The trailing `{set_id}` is optional and only emitted by
/// servers that advertise the order-invariant [`SetId`](atomic_core::types::SetId);
/// older servers omit it and clients fall back to the Merkle dichotomy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StateResponse {
    /// The channel has state.
    State {
        /// The current position (sequence number of the last change).
        position: u64,
        /// The Merkle state at this position (base32 encoded).
        merkle: String,
        /// The Merkle state of the most recent tag, or empty if no tags.
        tag_merkle: String,
        /// The order-invariant `SetId` of the view's effective change set
        /// (base32 encoded), or `None` when the server does not advertise it.
        ///
        /// This is an **additive, optional** field: it enables an
        /// order-invariant sync fast path without changing the existing
        /// Merkle-based protocol. Absent on older servers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        set_id: Option<String>,
    },
    /// The channel is empty.
    Empty,
}

impl StateResponse {
    /// Create a state response with values (no advertised `SetId`).
    pub fn state(position: u64, merkle: impl Into<String>, tag_merkle: impl Into<String>) -> Self {
        Self::State {
            position,
            merkle: merkle.into(),
            tag_merkle: tag_merkle.into(),
            set_id: None,
        }
    }

    /// Create a state response that also advertises the order-invariant
    /// [`SetId`](atomic_core::types::SetId) of the view's change set.
    pub fn state_with_set_id(
        position: u64,
        merkle: impl Into<String>,
        tag_merkle: impl Into<String>,
        set_id: impl Into<String>,
    ) -> Self {
        Self::State {
            position,
            merkle: merkle.into(),
            tag_merkle: tag_merkle.into(),
            set_id: Some(set_id.into()),
        }
    }

    /// Create an empty state response.
    pub fn empty() -> Self {
        Self::Empty
    }

    /// Parse a state response from the protocol format.
    ///
    /// Format: `{position} {merkle} {tag_merkle}` or `-`
    ///
    /// # Examples
    ///
    /// ```
    /// use atomic_remote::types::StateResponse;
    ///
    /// let state = StateResponse::parse("42 ABC123 DEF456").unwrap();
    /// match state {
    ///     StateResponse::State { position, merkle, tag_merkle, .. } => {
    ///         assert_eq!(position, 42);
    ///         assert_eq!(merkle, "ABC123");
    ///         assert_eq!(tag_merkle, "DEF456");
    ///     }
    ///     _ => panic!("expected state"),
    /// }
    ///
    /// let empty = StateResponse::parse("-").unwrap();
    /// assert!(matches!(empty, StateResponse::Empty));
    /// ```
    pub fn parse(line: &str) -> Result<Self, ParseStateError> {
        let line = line.trim();

        if line.is_empty() {
            return Err(ParseStateError::Empty);
        }

        if line == "-" {
            return Ok(Self::Empty);
        }

        let parts: Vec<&str> = line.split_whitespace().collect();

        if parts.len() < 3 {
            return Err(ParseStateError::InvalidFormat(line.to_string()));
        }

        let position: u64 = parts[0]
            .parse()
            .map_err(|_| ParseStateError::InvalidPosition(parts[0].to_string()))?;

        let merkle = parts[1].to_string();
        let tag_merkle = parts[2].to_string();
        // Optional 4th token: the order-invariant SetId. Absent on older
        // servers, which emit only three tokens.
        let set_id = parts.get(3).map(|s| s.to_string());

        Ok(Self::State {
            position,
            merkle,
            tag_merkle,
            set_id,
        })
    }

    /// Check if the channel is empty.
    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Empty)
    }

    /// Get the position if the channel has state.
    pub fn position(&self) -> Option<u64> {
        match self {
            Self::State { position, .. } => Some(*position),
            Self::Empty => None,
        }
    }

    /// Get the merkle state if the channel has state.
    pub fn merkle(&self) -> Option<&str> {
        match self {
            Self::State { merkle, .. } => Some(merkle),
            Self::Empty => None,
        }
    }

    /// Get the tag merkle if the channel has state.
    pub fn tag_merkle(&self) -> Option<&str> {
        match self {
            Self::State { tag_merkle, .. } => Some(tag_merkle),
            Self::Empty => None,
        }
    }

    /// Get the advertised order-invariant `SetId` (base32), if the server
    /// provided one.
    pub fn set_id(&self) -> Option<&str> {
        match self {
            Self::State { set_id, .. } => set_id.as_deref(),
            Self::Empty => None,
        }
    }

    /// Format this response as a protocol line.
    pub fn to_protocol_line(&self) -> String {
        match self {
            Self::State {
                position,
                merkle,
                tag_merkle,
                set_id,
            } => match set_id {
                Some(sid) => format!("{} {} {} {}", position, merkle, tag_merkle, sid),
                None => format!("{} {} {}", position, merkle, tag_merkle),
            },
            Self::Empty => "-".to_string(),
        }
    }
}

impl fmt::Display for StateResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_protocol_line())
    }
}

/// Error when parsing a state response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseStateError {
    /// The line was empty.
    Empty,
    /// The line format was invalid.
    InvalidFormat(String),
    /// The position was invalid.
    InvalidPosition(String),
}

impl fmt::Display for ParseStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "empty state response"),
            Self::InvalidFormat(s) => write!(f, "invalid state format: '{}'", s),
            Self::InvalidPosition(s) => write!(f, "invalid position: '{}'", s),
        }
    }
}

impl std::error::Error for ParseStateError {}

// PushDelta

/// The set of changes to push to a remote.
///
/// Calculated by comparing local and remote changelists.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PushDelta {
    /// Nodes (changes/tags) to upload.
    pub to_upload: Vec<Node>,

    /// Changes that were unrecorded on the remote since our last sync.
    pub remote_unrecords: Vec<String>,

    /// Changes on the remote that we don't have locally.
    pub unknown_changes: Vec<String>,
}

impl PushDelta {
    /// Create a new empty push delta.
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if there's nothing to push.
    pub fn is_empty(&self) -> bool {
        self.to_upload.is_empty()
    }

    /// Get the number of nodes to upload.
    pub fn upload_count(&self) -> usize {
        self.to_upload.len()
    }

    /// Check if there are remote unrecords to report.
    pub fn has_remote_unrecords(&self) -> bool {
        !self.remote_unrecords.is_empty()
    }

    /// Check if there are unknown changes on the remote.
    pub fn has_unknown_changes(&self) -> bool {
        !self.unknown_changes.is_empty()
    }
}

// PullDelta

/// The set of changes to pull from a remote.
///
/// Calculated by comparing local and remote changelists.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PullDelta {
    /// Nodes (changes/tags) to download.
    pub to_download: Vec<Node>,

    /// The remote's current state.
    pub remote_state: Option<StateResponse>,

    /// Changes we have locally that aren't on the remote (potential conflict).
    pub local_only: Vec<String>,
}

impl PullDelta {
    /// Create a new empty pull delta.
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if there's nothing to pull.
    pub fn is_empty(&self) -> bool {
        self.to_download.is_empty()
    }

    /// Get the number of nodes to download.
    pub fn download_count(&self) -> usize {
        self.to_download.len()
    }

    /// Check if there are local-only changes (potential conflict).
    pub fn has_local_only(&self) -> bool {
        !self.local_only.is_empty()
    }
}

// RemoteViewInfo

/// Metadata about a single view on a remote repository.
///
/// Returned by the `?views` inventory endpoint. Clients use this to
/// enumerate the remote's views — for example to pull or recreate every
/// view, not just the default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteViewInfo {
    /// The view name.
    pub name: String,

    /// The view scope: `"shared"` or `"draft"`.
    pub scope: String,

    /// The parent view name, or `None` for a root view.
    pub parent: Option<String>,

    /// Number of changes in the view (includes inherited changes for drafts).
    pub change_count: u64,

    /// The view's current base32 Merkle state, or `None` if the view is empty.
    pub state: Option<String>,

    /// Order-invariant `SetId` (base32) of the view's effective change set, when
    /// the server advertises it (6th wire field). This is the convergence
    /// identity used to validate that a pull/clone reproduced the exact set;
    /// `None` against servers that predate it.
    pub set_id: Option<String>,
}

/// Outcome of a bare view-ref compare-and-swap (`PUT /refs/views/{name}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefUpdate {
    /// The ref moved to the new snapshot key (fast-forward or genesis).
    Committed,
    /// The move was rejected — divergent history or a lost concurrent CAS.
    /// Carries the server's explanation.
    Conflict(String),
}

impl RemoteViewInfo {
    /// Parse one protocol line into a [`RemoteViewInfo`].
    ///
    /// Wire format (tab-separated):
    /// `name\tscope\tparent\tchange_count\tstate\tset_id`, where `parent`,
    /// `state`, and `set_id` use `-`/absence to mean "none". The trailing
    /// `set_id` is optional so older 5-field servers still parse.
    ///
    /// Returns `None` for blank lines or lines that don't have at least the
    /// `name` and `scope` fields, so unrelated output (e.g. an older server's
    /// JSON fallback) is skipped rather than misparsed.
    pub fn parse(line: &str) -> Option<Self> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }

        let mut fields = line.split('\t');
        let name = fields.next()?.trim();
        let scope = fields.next()?.trim();
        if name.is_empty() || scope.is_empty() {
            return None;
        }

        let parent = fields.next().map(str::trim).filter(|p| !p.is_empty());
        let change_count = fields
            .next()
            .and_then(|c| c.trim().parse::<u64>().ok())
            .unwrap_or(0);
        let state = fields.next().map(str::trim).filter(|s| !s.is_empty());
        let set_id = fields.next().map(str::trim).filter(|s| !s.is_empty());

        let none_or = |v: Option<&str>| match v {
            Some("-") | None => None,
            Some(other) => Some(other.to_string()),
        };

        Some(Self {
            name: name.to_string(),
            scope: scope.to_string(),
            parent: none_or(parent),
            change_count,
            state: none_or(state),
            set_id: none_or(set_id),
        })
    }

    /// Whether the view is a draft view.
    pub fn is_draft(&self) -> bool {
        self.scope.eq_ignore_ascii_case("draft")
    }

    /// Whether the view is empty (has no changes).
    pub fn is_empty(&self) -> bool {
        self.change_count == 0
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------------
    // NodeType tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_node_type_marker() {
        assert_eq!(NodeType::Change.marker(), 'C');
        assert_eq!(NodeType::Tag.marker(), 'T');
    }

    #[test]
    fn test_node_type_from_marker() {
        assert_eq!(NodeType::from_marker('C'), Some(NodeType::Change));
        assert_eq!(NodeType::from_marker('T'), Some(NodeType::Tag));
        assert_eq!(NodeType::from_marker('X'), None);
    }

    #[test]
    fn test_node_type_display() {
        assert_eq!(NodeType::Change.to_string(), "change");
        assert_eq!(NodeType::Tag.to_string(), "tag");
    }

    #[test]
    fn test_node_type_from_str() {
        assert_eq!("change".parse::<NodeType>().unwrap(), NodeType::Change);
        assert_eq!("CHANGE".parse::<NodeType>().unwrap(), NodeType::Change);
        assert_eq!("c".parse::<NodeType>().unwrap(), NodeType::Change);
        assert_eq!("tag".parse::<NodeType>().unwrap(), NodeType::Tag);
        assert_eq!("T".parse::<NodeType>().unwrap(), NodeType::Tag);
        assert!("invalid".parse::<NodeType>().is_err());
    }

    // -------------------------------------------------------------------------
    // Node tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_node_change() {
        let node = Node::change("ABC123", "DEF456");
        assert!(node.is_change());
        assert!(!node.is_tag());
        assert_eq!(node.hash, "ABC123");
        assert_eq!(node.state, "DEF456");
    }

    #[test]
    fn test_node_tag() {
        let node = Node::tag("ABC123", "DEF456");
        assert!(node.is_tag());
        assert!(!node.is_change());
    }

    #[test]
    fn test_node_display() {
        let node = Node::change("ABC", "DEF");
        assert_eq!(node.to_string(), "change(ABC -> DEF)");
    }

    // -------------------------------------------------------------------------
    // ChangelistEntry tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_changelist_entry_new() {
        let entry = ChangelistEntry::new(0, "ABC", "DEF", false);
        assert_eq!(entry.sequence, 0);
        assert_eq!(entry.hash, "ABC");
        assert_eq!(entry.merkle, "DEF");
        assert!(!entry.tagged);
    }

    #[test]
    fn test_changelist_entry_parse_regular() {
        let entry = ChangelistEntry::parse("0.ABC123.DEF456").unwrap();
        assert_eq!(entry.sequence, 0);
        assert_eq!(entry.hash, "ABC123");
        assert_eq!(entry.merkle, "DEF456");
        assert!(!entry.tagged);
    }

    #[test]
    fn test_changelist_entry_parse_tagged() {
        let entry = ChangelistEntry::parse("42.ABC123.DEF456.").unwrap();
        assert_eq!(entry.sequence, 42);
        assert_eq!(entry.hash, "ABC123");
        assert_eq!(entry.merkle, "DEF456");
        assert!(entry.tagged);
    }

    #[test]
    fn test_changelist_entry_parse_with_whitespace() {
        let entry = ChangelistEntry::parse("  5.HASH.STATE  \n").unwrap();
        assert_eq!(entry.sequence, 5);
        assert_eq!(entry.hash, "HASH");
        assert_eq!(entry.merkle, "STATE");
    }

    #[test]
    fn test_changelist_entry_parse_errors() {
        assert!(matches!(
            ChangelistEntry::parse(""),
            Err(ParseChangelistError::Empty)
        ));
        assert!(matches!(
            ChangelistEntry::parse("invalid"),
            Err(ParseChangelistError::InvalidFormat(_))
        ));
        assert!(matches!(
            ChangelistEntry::parse("abc.HASH.STATE"),
            Err(ParseChangelistError::InvalidSequence(_))
        ));
    }

    #[test]
    fn test_changelist_entry_to_protocol_line() {
        let regular = ChangelistEntry::new(0, "ABC", "DEF", false);
        assert_eq!(regular.to_protocol_line(), "0.ABC.DEF");

        let tagged = ChangelistEntry::new(1, "GHI", "JKL", true);
        assert_eq!(tagged.to_protocol_line(), "1.GHI.JKL.");
    }

    #[test]
    fn test_changelist_entry_roundtrip() {
        let original = ChangelistEntry::new(42, "HASH", "STATE", true);
        let line = original.to_protocol_line();
        let parsed = ChangelistEntry::parse(&line).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn test_changelist_entry_to_node() {
        let entry = ChangelistEntry::new(0, "ABC", "DEF", false);
        let node = entry.to_node();
        assert_eq!(node.hash, "ABC");
        assert_eq!(node.state, "DEF");
        assert!(node.is_change());
    }

    // -------------------------------------------------------------------------
    // StateResponse tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_state_response_state() {
        let state = StateResponse::state(42, "ABC", "DEF");
        assert!(!state.is_empty());
        assert_eq!(state.position(), Some(42));
        assert_eq!(state.merkle(), Some("ABC"));
        assert_eq!(state.tag_merkle(), Some("DEF"));
    }

    #[test]
    fn test_state_response_empty() {
        let state = StateResponse::empty();
        assert!(state.is_empty());
        assert_eq!(state.position(), None);
        assert_eq!(state.merkle(), None);
    }

    #[test]
    fn test_state_response_parse_state() {
        let state = StateResponse::parse("42 ABC123 DEF456").unwrap();
        match state {
            StateResponse::State {
                position,
                merkle,
                tag_merkle,
                ..
            } => {
                assert_eq!(position, 42);
                assert_eq!(merkle, "ABC123");
                assert_eq!(tag_merkle, "DEF456");
            }
            _ => panic!("expected State"),
        }
    }

    #[test]
    fn test_state_response_parse_empty() {
        let state = StateResponse::parse("-").unwrap();
        assert!(matches!(state, StateResponse::Empty));
    }

    #[test]
    fn test_state_response_parse_with_whitespace() {
        let state = StateResponse::parse("  10 ABC DEF  \n").unwrap();
        assert_eq!(state.position(), Some(10));
    }

    #[test]
    fn test_state_response_parse_errors() {
        assert!(matches!(
            StateResponse::parse(""),
            Err(ParseStateError::Empty)
        ));
        assert!(matches!(
            StateResponse::parse("invalid"),
            Err(ParseStateError::InvalidFormat(_))
        ));
        assert!(matches!(
            StateResponse::parse("abc DEF GHI"),
            Err(ParseStateError::InvalidPosition(_))
        ));
    }

    #[test]
    fn test_state_response_to_protocol_line() {
        let state = StateResponse::state(42, "ABC", "DEF");
        assert_eq!(state.to_protocol_line(), "42 ABC DEF");

        let empty = StateResponse::empty();
        assert_eq!(empty.to_protocol_line(), "-");
    }

    #[test]
    fn test_state_response_set_id_optional() {
        // No SetId: three-token line, set_id() is None (older server).
        let without = StateResponse::state(42, "ABC", "DEF");
        assert_eq!(without.set_id(), None);
        assert_eq!(without.to_protocol_line(), "42 ABC DEF");

        // With SetId: four-token line, round-trips and set_id() is Some.
        let with = StateResponse::state_with_set_id(42, "ABC", "DEF", "SID32");
        assert_eq!(with.set_id(), Some("SID32"));
        assert_eq!(with.to_protocol_line(), "42 ABC DEF SID32");
        assert_eq!(
            StateResponse::parse("42 ABC DEF SID32").unwrap(),
            with,
            "the optional SetId token round-trips through parse"
        );
    }

    #[test]
    fn test_state_response_parse_ignores_missing_set_id() {
        // A legacy three-token line parses to set_id = None (backward compat).
        let parsed = StateResponse::parse("7 MERK TAG").unwrap();
        assert_eq!(parsed.set_id(), None);
    }

    #[test]
    fn test_state_response_set_id_json_backward_compat() {
        // Older JSON without `set_id` deserializes (serde default = None).
        let legacy = r#"{"State":{"position":1,"merkle":"M","tag_merkle":"T"}}"#;
        let parsed: StateResponse = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.set_id(), None);

        // And a None set_id is skipped on serialize (no `set_id` key emitted).
        let json = serde_json::to_string(&StateResponse::state(1, "M", "T")).unwrap();
        assert!(
            !json.contains("set_id"),
            "None set_id must not be serialized"
        );
    }

    #[test]
    fn test_state_response_roundtrip() {
        let original = StateResponse::state(100, "MERKLE", "TAG");
        let line = original.to_protocol_line();
        let parsed = StateResponse::parse(&line).unwrap();
        assert_eq!(original, parsed);
    }

    // -------------------------------------------------------------------------
    // PushDelta tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_push_delta_new() {
        let delta = PushDelta::new();
        assert!(delta.is_empty());
        assert_eq!(delta.upload_count(), 0);
        assert!(!delta.has_remote_unrecords());
        assert!(!delta.has_unknown_changes());
    }

    #[test]
    fn test_push_delta_with_nodes() {
        let mut delta = PushDelta::new();
        delta.to_upload.push(Node::change("ABC", "DEF"));
        assert!(!delta.is_empty());
        assert_eq!(delta.upload_count(), 1);
    }

    #[test]
    fn test_push_delta_with_unrecords() {
        let mut delta = PushDelta::new();
        delta.remote_unrecords.push("ABC".to_string());
        assert!(delta.has_remote_unrecords());
    }

    #[test]
    fn test_push_delta_with_unknown() {
        let mut delta = PushDelta::new();
        delta.unknown_changes.push("XYZ".to_string());
        assert!(delta.has_unknown_changes());
    }

    // -------------------------------------------------------------------------
    // PullDelta tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_pull_delta_new() {
        let delta = PullDelta::new();
        assert!(delta.is_empty());
        assert_eq!(delta.download_count(), 0);
        assert!(!delta.has_local_only());
    }

    #[test]
    fn test_pull_delta_with_nodes() {
        let mut delta = PullDelta::new();
        delta.to_download.push(Node::change("ABC", "DEF"));
        assert!(!delta.is_empty());
        assert_eq!(delta.download_count(), 1);
    }

    #[test]
    fn test_pull_delta_with_local_only() {
        let mut delta = PullDelta::new();
        delta.local_only.push("LOCAL".to_string());
        assert!(delta.has_local_only());
    }

    // -------------------------------------------------------------------------
    // RemoteViewInfo tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_remote_view_info_parse_full() {
        let v = RemoteViewInfo::parse("dev\tshared\t-\t3\t2AAAAAAAAAAA").unwrap();
        assert_eq!(v.name, "dev");
        assert_eq!(v.scope, "shared");
        assert_eq!(v.parent, None);
        assert_eq!(v.change_count, 3);
        assert_eq!(v.state.as_deref(), Some("2AAAAAAAAAAA"));
        assert!(!v.is_draft());
        assert!(!v.is_empty());
    }

    #[test]
    fn test_remote_view_info_parse_draft_with_parent() {
        let v = RemoteViewInfo::parse("feature\tdraft\tdev\t2\tXYZ").unwrap();
        assert_eq!(v.name, "feature");
        assert!(v.is_draft());
        assert_eq!(v.parent.as_deref(), Some("dev"));
        assert_eq!(v.change_count, 2);
    }

    #[test]
    fn test_remote_view_info_parse_empty_view() {
        let v = RemoteViewInfo::parse("fresh\tshared\t-\t0\t-").unwrap();
        assert_eq!(v.change_count, 0);
        assert!(v.is_empty());
        assert_eq!(v.state, None);
        assert_eq!(v.parent, None);
    }

    #[test]
    fn test_remote_view_info_parse_name_and_scope_only() {
        // Trailing fields are optional; missing ones default sensibly.
        let v = RemoteViewInfo::parse("dev\tshared").unwrap();
        assert_eq!(v.name, "dev");
        assert_eq!(v.change_count, 0);
        assert_eq!(v.parent, None);
        assert_eq!(v.state, None);
    }

    #[test]
    fn test_remote_view_info_parse_rejects_blank_and_malformed() {
        // Blank lines and lines lacking a scope field are skipped, so an old
        // server's JSON info blob (a single tab-less line) yields no views.
        assert!(RemoteViewInfo::parse("").is_none());
        assert!(RemoteViewInfo::parse("   ").is_none());
        assert!(RemoteViewInfo::parse("dev").is_none());
        assert!(RemoteViewInfo::parse(r#"{"workspace":"w","project":"p"}"#).is_none());
    }

    #[test]
    fn test_remote_view_info_parse_trims_whitespace() {
        let v = RemoteViewInfo::parse("  dev\tshared\t-\t1\tABC  ").unwrap();
        assert_eq!(v.name, "dev");
        assert_eq!(v.change_count, 1);
        assert_eq!(v.state.as_deref(), Some("ABC"));
    }
}
