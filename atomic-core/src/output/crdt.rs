//! CRDT-based content retrieval for human-readable output.
//!
//! This module provides functions to retrieve file content using the CRDT
//! semantic layer (TRUNKS, BRANCHES, LEAVES tables) rather than the raw
//! graph vertices and edges.
//!
//! # Overview
//!
//! The CRDT tables store semantic information about files:
//! - **TRUNKS**: File metadata (path, inode, encoding, state)
//! - **BRANCHES**: Line metadata (parent trunk, state, content hash)
//! - **LEAVES**: Token metadata (parent branch, kind, state, content range)
//!
//! This enables line-by-line content retrieval with:
//! - Line numbers
//! - Token-level granularity
//! - State awareness (alive vs deleted)
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                    CRDT Content Retrieval                                │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  get_file_lines(path)                                                   │
//! │       │                                                                 │
//! │       ▼                                                                 │
//! │  ┌─────────────────┐                                                   │
//! │  │ PATH_TRUNK      │ ──► TrunkId                                       │
//! │  └─────────────────┘                                                   │
//! │       │                                                                 │
//! │       ▼                                                                 │
//! │  ┌─────────────────┐                                                   │
//! │  │ TRUNK_BRANCHES  │ ──► [BranchId, BranchId, ...]                     │
//! │  └─────────────────┘                                                   │
//! │       │                                                                 │
//! │       ▼ (for each branch)                                              │
//! │  ┌─────────────────┐                                                   │
//! │  │ BRANCH_LEAVES   │ ──► [LeafId, LeafId, ...]                         │
//! │  └─────────────────┘                                                   │
//! │       │                                                                 │
//! │       ▼                                                                 │
//! │  Line { number, content, tokens, state }                               │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_core::output::crdt::{get_file_lines, Line};
//!
//! let lines = get_file_lines(&mut txn, "src/main.rs")?;
//! for line in &lines {
//!     println!("{}: {}", line.number, line.content());
//! }
//! ```

use crate::crdt::queries::iter_trunk_branches_in_file_order;
use crate::crdt::tables::{decode_leaf_id, encode_branch_id, encode_trunk_id, SerializedTrunk};
use crate::crdt::{BranchId, BranchState, LeafId, LeafState, TrunkId, TrunkState};
use crate::diff::token::TokenKind;
use crate::pristine::{MutTxnT, PristineResult};
use crate::types::Inode;

// Token - A single token within a line

/// A token (leaf) within a line.
///
/// Tokens are the atomic units of content in the CRDT model.
/// Each token has a type (word, whitespace, operator, etc.) and content.
#[derive(Debug, Clone)]
pub struct Token {
    /// The token's unique identifier.
    pub id: LeafId,
    /// The token type.
    pub kind: TokenKind,
    /// The token content (bytes).
    pub content: Vec<u8>,
    /// The token's lifecycle state.
    pub state: LeafState,
}

impl Token {
    /// Creates a new token.
    pub fn new(id: LeafId, kind: TokenKind, content: Vec<u8>, state: LeafState) -> Self {
        Self {
            id,
            kind,
            content,
            state,
        }
    }

    /// Returns the token content as a string (lossy UTF-8 conversion).
    pub fn content_str(&self) -> String {
        String::from_utf8_lossy(&self.content).into_owned()
    }

    /// Returns true if the token is alive (not deleted).
    pub fn is_alive(&self) -> bool {
        self.state.is_alive()
    }

    /// Returns true if the token is deleted.
    pub fn is_deleted(&self) -> bool {
        self.state.is_deleted()
    }
}

// Line - A single line within a file

/// A line (branch) within a file.
///
/// Lines contain tokens and track their state and position.
#[derive(Debug, Clone)]
pub struct Line {
    /// The line's unique identifier.
    pub id: BranchId,
    /// The line number (1-based, in document order).
    pub number: usize,
    /// The tokens that make up this line.
    pub tokens: Vec<Token>,
    /// The line's lifecycle state.
    pub state: BranchState,
    /// Content hash for fast equality checks.
    pub line_hash: u64,
}

impl Line {
    /// Creates a new line.
    pub fn new(id: BranchId, number: usize, state: BranchState, line_hash: u64) -> Self {
        Self {
            id,
            number,
            tokens: Vec::new(),
            state,
            line_hash,
        }
    }

    /// Adds a token to this line.
    pub fn add_token(&mut self, token: Token) {
        self.tokens.push(token);
    }

    /// Returns the line content by concatenating all alive tokens.
    pub fn content(&self) -> String {
        self.tokens
            .iter()
            .filter(|t| t.is_alive())
            .map(|t| t.content_str())
            .collect()
    }

    /// Returns the line content including deleted tokens (for history).
    pub fn full_content(&self) -> String {
        self.tokens.iter().map(|t| t.content_str()).collect()
    }

    /// Returns true if the line is alive (not deleted).
    pub fn is_alive(&self) -> bool {
        self.state.is_alive()
    }

    /// Returns true if the line is deleted.
    pub fn is_deleted(&self) -> bool {
        self.state.is_deleted()
    }

    /// Returns the number of alive tokens.
    pub fn token_count(&self) -> usize {
        self.tokens.iter().filter(|t| t.is_alive()).count()
    }

    /// Returns the number of all tokens (including deleted).
    pub fn total_token_count(&self) -> usize {
        self.tokens.len()
    }
}

// File - A complete file with all lines

/// A file (trunk) with all its lines.
#[derive(Debug, Clone)]
pub struct File {
    /// The file's unique identifier.
    pub id: TrunkId,
    /// The file path.
    pub path: String,
    /// The file's inode.
    pub inode: Inode,
    /// The file's lifecycle state.
    pub state: TrunkState,
    /// The file's encoding (as stored).
    pub encoding: u8,
    /// The lines in this file.
    pub lines: Vec<Line>,
}

impl File {
    /// Creates a new file from serialized trunk data.
    pub fn new(id: TrunkId, serialized: &SerializedTrunk) -> Self {
        Self {
            id,
            path: serialized.path.clone(),
            inode: serialized.inode,
            state: serialized.state,
            encoding: serialized.encoding,
            lines: Vec::new(),
        }
    }

    /// Adds a line to this file.
    pub fn add_line(&mut self, line: Line) {
        self.lines.push(line);
    }

    /// Returns the file content by concatenating all alive lines.
    pub fn content(&self) -> String {
        self.lines
            .iter()
            .filter(|l| l.is_alive())
            .map(|l| l.content())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Returns true if the file is alive (not deleted).
    pub fn is_alive(&self) -> bool {
        self.state.is_alive()
    }

    /// Returns the number of alive lines.
    pub fn line_count(&self) -> usize {
        self.lines.iter().filter(|l| l.is_alive()).count()
    }

    /// Returns the number of all lines (including deleted).
    pub fn total_line_count(&self) -> usize {
        self.lines.len()
    }
}

// Retrieval Options

/// Options for content retrieval.
#[derive(Debug, Clone, Default)]
pub struct RetrievalOptions {
    /// Include deleted lines in the result.
    pub include_deleted_lines: bool,
    /// Include deleted tokens in the result.
    pub include_deleted_tokens: bool,
    /// Maximum number of lines to retrieve (None = unlimited).
    pub max_lines: Option<usize>,
}

impl RetrievalOptions {
    /// Creates new default options.
    pub fn new() -> Self {
        Self::default()
    }

    /// Include deleted lines.
    pub fn with_deleted_lines(mut self) -> Self {
        self.include_deleted_lines = true;
        self
    }

    /// Include deleted tokens.
    pub fn with_deleted_tokens(mut self) -> Self {
        self.include_deleted_tokens = true;
        self
    }

    /// Limit the number of lines.
    pub fn with_max_lines(mut self, max: usize) -> Self {
        self.max_lines = Some(max);
        self
    }
}

// Retrieval Functions

/// Retrieve all lines for a file by path.
///
/// This function reads the CRDT tables to reconstruct the file's content
/// line by line, with token-level granularity.
///
/// # Arguments
///
/// * `txn` - A mutable transaction for database access
/// * `path` - The file path to retrieve
///
/// # Returns
///
/// A vector of `Line` structs representing the file's content.
/// Returns an empty vector if the file doesn't exist.
///
/// # Example
///
/// ```rust,ignore
/// let lines = get_file_lines(&mut txn, "src/main.rs")?;
/// for line in &lines {
///     println!("{}: {}", line.number, line.content());
/// }
/// ```
pub fn get_file_lines<T: MutTxnT>(txn: &mut T, path: &str) -> PristineResult<Vec<Line>> {
    get_file_lines_with_options(txn, path, &RetrievalOptions::default())
}

/// Retrieve all lines for a file by path with options.
///
/// Like `get_file_lines` but allows controlling which content is included.
pub fn get_file_lines_with_options<T: MutTxnT>(
    txn: &mut T,
    path: &str,
    options: &RetrievalOptions,
) -> PristineResult<Vec<Line>> {
    // Look up trunk by path
    let trunk_id = match txn.get_trunk_by_path(path)? {
        Some(id) => id,
        None => return Ok(Vec::new()), // File not found
    };

    get_file_lines_by_trunk(txn, trunk_id, options)
}

/// Retrieve all lines for a file by trunk ID.
pub fn get_file_lines_by_trunk<T: MutTxnT>(
    txn: &mut T,
    trunk_id: TrunkId,
    options: &RetrievalOptions,
) -> PristineResult<Vec<Line>> {
    // Get all branches for this trunk **in file order**.  The BRANCH_AFTER
    // chain produces top-of-file → bottom-of-file ordering — TRUNK_BRANCHES
    // alone gives BranchId sort order, which is wrong for prepended lines
    // from later commits.
    let branch_ids: Vec<BranchId> = iter_trunk_branches_in_file_order(txn, trunk_id)?;

    let mut lines = Vec::new();
    let mut line_number = 1usize;

    for branch_id in branch_ids {
        let branch_key = encode_branch_id(&branch_id);

        // Check line limit
        if let Some(max) = options.max_lines {
            if lines.len() >= max {
                break;
            }
        }
        let branch_data = match txn.get_crdt_branch(&branch_key)? {
            Some(data) => data,
            None => continue, // Branch not found, skip
        };

        // Skip deleted lines unless requested
        if branch_data.state.is_deleted() && !options.include_deleted_lines {
            continue;
        }

        let mut line = Line::new(
            branch_id,
            line_number,
            branch_data.state,
            branch_data.line_hash,
        );

        // Get all leaves for this branch
        let leaf_keys: Vec<[u8; 12]> = txn
            .iter_branch_leaves(&branch_key)?
            .collect::<Result<Vec<_>, _>>()?;

        for leaf_key in leaf_keys {
            let leaf_id = decode_leaf_id(&leaf_key);
            let leaf_data = match txn.get_crdt_leaf(&leaf_key)? {
                Some(data) => data,
                None => continue, // Leaf not found, skip
            };

            // Skip deleted tokens unless requested
            if leaf_data.state.is_deleted() && !options.include_deleted_tokens {
                continue;
            }

            // Note: content would need to come from the change's content blob
            // For now, we create a placeholder - actual content retrieval
            // requires access to the change store
            let token = Token::new(
                leaf_id,
                leaf_data.kind,
                Vec::new(), // Content placeholder
                leaf_data.state,
            );
            line.add_token(token);
        }

        // Only increment line number for alive lines
        if branch_data.state.is_alive() {
            line_number += 1;
        }

        lines.push(line);
    }

    Ok(lines)
}

/// Retrieve a complete file with all metadata and lines.
pub fn get_file<T: MutTxnT>(txn: &mut T, path: &str) -> PristineResult<Option<File>> {
    get_file_with_options(txn, path, &RetrievalOptions::default())
}

/// Retrieve a complete file with options.
pub fn get_file_with_options<T: MutTxnT>(
    txn: &mut T,
    path: &str,
    options: &RetrievalOptions,
) -> PristineResult<Option<File>> {
    // Look up trunk by path
    let trunk_id = match txn.get_trunk_by_path(path)? {
        Some(id) => id,
        None => return Ok(None), // File not found
    };

    let trunk_key = encode_trunk_id(&trunk_id);

    // Get trunk metadata
    let trunk_data = match txn.get_crdt_trunk(&trunk_key)? {
        Some(data) => data,
        None => return Ok(None), // Trunk not found
    };

    let mut file = File::new(trunk_id, &trunk_data);

    // Get all lines
    let lines = get_file_lines_by_trunk(txn, trunk_id, options)?;
    for line in lines {
        file.add_line(line);
    }

    Ok(Some(file))
}

/// Check if a file exists in the CRDT tables.
pub fn file_exists<T: MutTxnT>(txn: &mut T, path: &str) -> PristineResult<bool> {
    Ok(txn.get_trunk_by_path(path)?.is_some())
}

/// Get the trunk ID for a file path.
pub fn get_trunk_id<T: MutTxnT>(txn: &mut T, path: &str) -> PristineResult<Option<TrunkId>> {
    txn.get_trunk_by_path(path)
}

// CRDT-driven file output

/// Error type for the CRDT-driven output walker.
#[derive(Debug)]
pub enum CrdtOutputError<E> {
    /// Pristine read failed.
    Pristine(crate::pristine::PristineError),
    /// Change store read failed.
    Store(E),
    /// A branch was alive but had no BRANCH_VERTEX mapping — the CRDT layer
    /// has no way to know which bytes the branch corresponds to.
    ///
    /// Carries the orphan `BranchId` so callers can diagnose which line is
    /// missing its content reference.  This is recoverable: callers can fall
    /// back to the byte-graph walker for the affected file.
    OrphanBranch(crate::crdt::BranchId),
}

impl<E: std::fmt::Display> std::fmt::Display for CrdtOutputError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CrdtOutputError::Pristine(e) => write!(f, "pristine error: {}", e),
            CrdtOutputError::Store(e) => write!(f, "change store error: {}", e),
            CrdtOutputError::OrphanBranch(b) => {
                write!(f, "branch {} has no BRANCH_VERTEX mapping", b)
            }
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for CrdtOutputError<E> {}

impl<E> From<crate::pristine::PristineError> for CrdtOutputError<E> {
    fn from(e: crate::pristine::PristineError) -> Self {
        CrdtOutputError::Pristine(e)
    }
}

/// Reconstruct a file's bytes by walking the CRDT layer.
///
/// This is the alternative to `output::repo::output_file_with_filter`:
/// it derives line order from `iter_trunk_branches_in_file_order` (the
/// CRDT after-chain), filters by `branch.state` for liveness, and pulls
/// each line's bytes via the branch's recorded graph vertex.
///
/// The byte-graph is consulted *only* to fetch content blob ranges — the
/// linear-edge walk (`collect_sorted_content_vertices`, the
/// pick-one-outgoing-edge problem) is bypassed entirely.  That's the
/// whole point: the CRDT decides "what" and "in what order"; the change
/// store provides "the bytes."
///
/// # Behavior
///
/// 1. Look up the trunk for `path`.  No trunk → return `Ok(Vec::new())`
///    (file isn't tracked by the CRDT layer at all).
/// 2. Iterate branches in file order.
/// 3. Skip branches whose state is not alive.
/// 4. For each alive branch, fetch `BRANCH_VERTEX` → `GraphNode` → byte
///    range from the change's content blob.
/// 5. Concatenate.
///
/// # Caveats
///
/// - Reads `branch.state` directly — correct for single-view linear
///   history, *not* multi-view scenarios where the same branch may be
///   alive on one view and deleted on another.  Add a `change_filter`
///   parameter (task #24+) for multi-view support.
/// - An "orphan branch" (alive but no `BRANCH_VERTEX` row) raises
///   [`CrdtOutputError::OrphanBranch`].  Callers can catch and fall
///   back to the byte-graph walker for that file.
pub fn output_file_via_crdt<T, C>(
    txn: &T,
    changes: &C,
    path: &str,
) -> Result<Vec<u8>, CrdtOutputError<C::Error>>
where
    T: crate::pristine::CrdtTxnT + crate::pristine::GraphTxnT,
    C: crate::change::ChangeStore,
{
    use crate::types::Hash;

    let trunk_id = match txn.get_trunk_by_path(path)? {
        Some(t) => t,
        None => return Ok(Vec::new()),
    };

    let mut out: Vec<u8> = Vec::new();

    for branch_id in iter_trunk_branches_in_file_order(txn, trunk_id)? {
        let branch_key = encode_branch_id(&branch_id);

        let branch_data = match txn.get_crdt_branch(&branch_key)? {
            Some(b) => b,
            None => continue, // No row — branch listed in TRUNK_BRANCHES but
                              // missing from BRANCHES.  Treat as deleted.
        };
        if !branch_data.state.is_alive() {
            continue;
        }

        let graph_node = match txn.get_crdt_branch_vertex(&branch_key)? {
            Some(n) => n,
            None => return Err(CrdtOutputError::OrphanBranch(branch_id)),
        };

        let len = graph_node.end.get().saturating_sub(graph_node.start.get()) as usize;
        if len == 0 {
            continue;
        }

        let start = out.len();
        out.resize(start + len, 0);

        // hash_fn re-created per call to keep the &txn borrow re-entrant.
        let hash_fn = |id: crate::types::NodeId| -> Option<Hash> {
            if id.is_root() {
                None
            } else {
                txn.get_external(id).ok().flatten()
            }
        };

        changes
            .get_contents(hash_fn, graph_node, &mut out[start..])
            .map_err(CrdtOutputError::Store)?;
    }

    Ok(out)
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_new() {
        let id = LeafId::new(crate::types::NodeId::new(1), 0);
        let token = Token::new(id, TokenKind::Word, b"hello".to_vec(), LeafState::Alive);

        assert_eq!(token.content_str(), "hello");
        assert!(token.is_alive());
        assert!(!token.is_deleted());
    }

    #[test]
    fn test_token_deleted() {
        let id = LeafId::new(crate::types::NodeId::new(1), 0);
        let token = Token::new(id, TokenKind::Word, b"deleted".to_vec(), LeafState::Deleted);

        assert!(token.is_deleted());
        assert!(!token.is_alive());
    }

    #[test]
    fn test_line_new() {
        let id = BranchId::new(crate::types::NodeId::new(1), 0);
        let line = Line::new(id, 1, BranchState::Alive, 0);

        assert_eq!(line.number, 1);
        assert!(line.is_alive());
        assert_eq!(line.token_count(), 0);
    }

    #[test]
    fn test_line_content() {
        let id = BranchId::new(crate::types::NodeId::new(1), 0);
        let mut line = Line::new(id, 1, BranchState::Alive, 0);

        let leaf_id = LeafId::new(crate::types::NodeId::new(1), 0);
        line.add_token(Token::new(
            leaf_id,
            TokenKind::Word,
            b"hello".to_vec(),
            LeafState::Alive,
        ));

        let leaf_id2 = LeafId::new(crate::types::NodeId::new(1), 1);
        line.add_token(Token::new(
            leaf_id2,
            TokenKind::Whitespace,
            b" ".to_vec(),
            LeafState::Alive,
        ));

        let leaf_id3 = LeafId::new(crate::types::NodeId::new(1), 2);
        line.add_token(Token::new(
            leaf_id3,
            TokenKind::Word,
            b"world".to_vec(),
            LeafState::Alive,
        ));

        assert_eq!(line.content(), "hello world");
        assert_eq!(line.token_count(), 3);
    }

    #[test]
    fn test_line_with_deleted_tokens() {
        let id = BranchId::new(crate::types::NodeId::new(1), 0);
        let mut line = Line::new(id, 1, BranchState::Alive, 0);

        let leaf_id = LeafId::new(crate::types::NodeId::new(1), 0);
        line.add_token(Token::new(
            leaf_id,
            TokenKind::Word,
            b"visible".to_vec(),
            LeafState::Alive,
        ));

        let leaf_id2 = LeafId::new(crate::types::NodeId::new(1), 1);
        line.add_token(Token::new(
            leaf_id2,
            TokenKind::Word,
            b"deleted".to_vec(),
            LeafState::Deleted,
        ));

        assert_eq!(line.content(), "visible");
        assert_eq!(line.full_content(), "visibledeleted");
        assert_eq!(line.token_count(), 1);
        assert_eq!(line.total_token_count(), 2);
    }

    #[test]
    fn test_retrieval_options_default() {
        let options = RetrievalOptions::default();
        assert!(!options.include_deleted_lines);
        assert!(!options.include_deleted_tokens);
        assert!(options.max_lines.is_none());
    }

    #[test]
    fn test_retrieval_options_builder() {
        let options = RetrievalOptions::new()
            .with_deleted_lines()
            .with_deleted_tokens()
            .with_max_lines(100);

        assert!(options.include_deleted_lines);
        assert!(options.include_deleted_tokens);
        assert_eq!(options.max_lines, Some(100));
    }
}
