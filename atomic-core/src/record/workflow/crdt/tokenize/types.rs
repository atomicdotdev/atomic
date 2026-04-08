//! Token types for content tokenization.
//!
//! This module contains the core data types produced by tokenization:
//! [`TokenizedToken`], [`TokenizedLine`], and [`TokenStats`].

use crate::diff::token::TokenKind;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::ops::Range;

// ============================================================================
// TOKENIZED TOKEN
// ============================================================================

/// A single token with its metadata.
///
/// Represents a token extracted from a line, including its type,
/// content, and position information.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenizedToken {
    /// The semantic kind of this token.
    kind: TokenKind,

    /// The raw bytes of this token.
    content: Vec<u8>,

    /// Byte range within the line (start..end).
    byte_start: u32,
    byte_end: u32,
}

impl TokenizedToken {
    /// Creates a new tokenized token.
    pub fn new(kind: TokenKind, content: Vec<u8>, byte_range: Range<usize>) -> Self {
        Self {
            kind,
            content,
            byte_start: byte_range.start as u32,
            byte_end: byte_range.end as u32,
        }
    }

    /// Returns the token kind.
    #[inline]
    pub fn kind(&self) -> TokenKind {
        self.kind
    }

    /// Returns the token content as bytes.
    #[inline]
    pub fn content(&self) -> &[u8] {
        &self.content
    }

    /// Returns the token content as a string (lossy UTF-8 conversion).
    pub fn as_str(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.content)
    }

    /// Returns the byte range within the line.
    #[inline]
    pub fn byte_range(&self) -> Range<usize> {
        self.byte_start as usize..self.byte_end as usize
    }

    /// Returns the byte length of this token.
    #[inline]
    pub fn len(&self) -> usize {
        self.content.len()
    }

    /// Returns true if this token is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
    }

    /// Returns true if this token is significant (not whitespace).
    #[inline]
    pub fn is_significant(&self) -> bool {
        self.kind.is_significant()
    }

    /// Returns true if this token is whitespace.
    #[inline]
    pub fn is_whitespace(&self) -> bool {
        self.kind.is_whitespace()
    }
}

impl fmt::Display for TokenizedToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({:?})", self.kind, self.as_str())
    }
}

// ============================================================================
// TOKENIZED LINE
// ============================================================================

/// A tokenized line with its tokens and metadata.
///
/// Represents a complete line of content that has been split into
/// tokens. It includes the line number, raw content, and token sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenizedLine {
    /// The line number (0-indexed).
    line_number: usize,

    /// The raw content of this line (without newline).
    content: Vec<u8>,

    /// The tokens in this line.
    tokens: Vec<TokenizedToken>,

    /// FNV-1a hash of the line content for fast equality checks.
    content_hash: u64,
}

impl TokenizedLine {
    /// FNV-1a offset basis for 64-bit hashes.
    const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;

    /// FNV-1a prime for 64-bit hashes.
    const FNV_PRIME: u64 = 0x00000100000001B3;

    /// Creates a new tokenized line.
    pub fn new(line_number: usize, content: Vec<u8>, tokens: Vec<TokenizedToken>) -> Self {
        let content_hash = Self::compute_hash(&content);
        Self {
            line_number,
            content,
            tokens,
            content_hash,
        }
    }

    /// Creates an empty tokenized line.
    pub fn empty(line_number: usize) -> Self {
        Self {
            line_number,
            content: Vec::new(),
            tokens: Vec::new(),
            content_hash: Self::FNV_OFFSET_BASIS,
        }
    }

    /// Computes the FNV-1a hash of the given bytes.
    fn compute_hash(bytes: &[u8]) -> u64 {
        let mut hash = Self::FNV_OFFSET_BASIS;
        for &byte in bytes {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(Self::FNV_PRIME);
        }
        hash
    }

    /// Returns the line number (0-indexed).
    #[inline]
    pub fn line_number(&self) -> usize {
        self.line_number
    }

    /// Returns the raw content of this line.
    #[inline]
    pub fn content(&self) -> &[u8] {
        &self.content
    }

    /// Returns the content as a string (lossy UTF-8 conversion).
    pub fn as_str(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.content)
    }

    /// Returns the tokens in this line.
    #[inline]
    pub fn tokens(&self) -> &[TokenizedToken] {
        &self.tokens
    }

    /// Returns an iterator over the tokens.
    pub fn iter_tokens(&self) -> impl Iterator<Item = &TokenizedToken> {
        self.tokens.iter()
    }

    /// Returns the number of tokens in this line.
    #[inline]
    pub fn token_count(&self) -> usize {
        self.tokens.len()
    }

    /// Returns true if this line is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
    }

    /// Returns the byte length of this line.
    #[inline]
    pub fn len(&self) -> usize {
        self.content.len()
    }

    /// Returns the content hash for fast equality checks.
    #[inline]
    pub fn content_hash(&self) -> u64 {
        self.content_hash
    }

    /// Returns true if this line has the same content as another.
    ///
    /// Uses hash comparison for fast equality, falling back to byte
    /// comparison if hashes match (to handle collisions).
    pub fn content_eq(&self, other: &TokenizedLine) -> bool {
        self.content_hash == other.content_hash && self.content == other.content
    }

    /// Returns the number of significant (non-whitespace) tokens.
    pub fn significant_token_count(&self) -> usize {
        self.tokens.iter().filter(|t| t.is_significant()).count()
    }

    /// Returns true if this line contains only whitespace.
    pub fn is_whitespace_only(&self) -> bool {
        self.tokens.iter().all(|t| t.is_whitespace())
    }

    /// Consumes this line and returns its tokens.
    pub fn into_tokens(self) -> Vec<TokenizedToken> {
        self.tokens
    }
}

impl fmt::Display for TokenizedLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Line {}: {} tokens, {} bytes",
            self.line_number,
            self.tokens.len(),
            self.content.len()
        )
    }
}

// ============================================================================
// TOKEN STATS
// ============================================================================

/// Statistics about tokenization results.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenStats {
    /// Total number of lines processed.
    pub lines: usize,
    /// Total number of tokens generated.
    pub tokens: usize,
    /// Number of significant (non-whitespace) tokens.
    pub significant_tokens: usize,
    /// Number of whitespace tokens.
    pub whitespace_tokens: usize,
    /// Total bytes processed.
    pub bytes: usize,
    /// Number of empty lines.
    pub empty_lines: usize,
    /// Number of whitespace-only lines.
    pub whitespace_only_lines: usize,
    /// Longest line in bytes.
    pub max_line_length: usize,
    /// Maximum tokens in a single line.
    pub max_tokens_per_line: usize,
}

impl TokenStats {
    /// Creates new empty statistics.
    pub fn new() -> Self {
        Self::default()
    }

    /// Updates statistics with a tokenized line.
    pub fn add_line(&mut self, line: &TokenizedLine) {
        self.lines += 1;
        self.tokens += line.token_count();
        self.bytes += line.len();

        let significant = line.significant_token_count();
        self.significant_tokens += significant;
        self.whitespace_tokens += line.token_count() - significant;

        if line.is_empty() {
            self.empty_lines += 1;
        } else if line.is_whitespace_only() {
            self.whitespace_only_lines += 1;
        }

        if line.len() > self.max_line_length {
            self.max_line_length = line.len();
        }

        if line.token_count() > self.max_tokens_per_line {
            self.max_tokens_per_line = line.token_count();
        }
    }

    /// Merges another stats instance into this one.
    pub fn merge(&mut self, other: &TokenStats) {
        self.lines += other.lines;
        self.tokens += other.tokens;
        self.significant_tokens += other.significant_tokens;
        self.whitespace_tokens += other.whitespace_tokens;
        self.bytes += other.bytes;
        self.empty_lines += other.empty_lines;
        self.whitespace_only_lines += other.whitespace_only_lines;
        self.max_line_length = self.max_line_length.max(other.max_line_length);
        self.max_tokens_per_line = self.max_tokens_per_line.max(other.max_tokens_per_line);
    }

    /// Returns the average tokens per line.
    pub fn avg_tokens_per_line(&self) -> f64 {
        if self.lines == 0 {
            0.0
        } else {
            self.tokens as f64 / self.lines as f64
        }
    }

    /// Returns the average line length in bytes.
    pub fn avg_line_length(&self) -> f64 {
        if self.lines == 0 {
            0.0
        } else {
            self.bytes as f64 / self.lines as f64
        }
    }
}

impl fmt::Display for TokenStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} lines, {} tokens ({} significant), {} bytes",
            self.lines, self.tokens, self.significant_tokens, self.bytes
        )
    }
}
