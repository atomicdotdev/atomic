//! Content tokenization for CRDT Leaf operations.
//!
//! Splits file content into tokens that map to Leaf structures in the
//! hierarchical CRDT graph.

mod rules;
mod types;

#[cfg(test)]
mod tests;

pub use rules::{TokenizeError, TokenizeOptions};
pub use types::{TokenStats, TokenizedLine, TokenizedToken};

use crate::diff::token::TokenKind;
use crate::diff::token::Tokenizer;

// ============================================================================
// CONTENT TOKENIZER
// ============================================================================

/// Main tokenizer for converting content bytes into tokenized lines.
///
/// Provides an iterator-based interface for tokenizing content line by line.
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::crdt::tokenize::{
///     ContentTokenizer, TokenizeOptions,
/// };
///
/// let content = b"fn main() {\n    println!(\"Hello\");\n}\n";
/// let tokenizer = ContentTokenizer::new(content);
///
/// let lines: Vec<_> = tokenizer.lines().collect();
/// assert_eq!(lines.len(), 3);
/// ```
#[derive(Debug, Clone)]
pub struct ContentTokenizer<'a> {
    /// The content being tokenized.
    content: &'a [u8],

    /// Tokenization options.
    options: TokenizeOptions,
}

impl<'a> ContentTokenizer<'a> {
    /// Creates a new tokenizer with default options.
    pub fn new(content: &'a [u8]) -> Self {
        Self {
            content,
            options: TokenizeOptions::default(),
        }
    }

    /// Creates a new tokenizer with the given options.
    pub fn with_options(content: &'a [u8], options: TokenizeOptions) -> Self {
        Self { content, options }
    }

    /// Returns the content being tokenized.
    #[inline]
    pub fn content(&self) -> &'a [u8] {
        self.content
    }

    /// Returns the tokenization options.
    #[inline]
    pub fn options(&self) -> &TokenizeOptions {
        &self.options
    }

    /// Returns an iterator over tokenized lines.
    ///
    /// Lines are processed lazily as the iterator is consumed.
    pub fn lines(&self) -> LineIterator<'a> {
        LineIterator {
            content: self.content,
            options: self.options.clone(),
            position: 0,
            line_number: 0,
        }
    }

    /// Tokenizes all content and returns the result with statistics.
    ///
    /// This eagerly processes all lines and collects statistics.
    pub fn tokenize_all(&self) -> (Vec<TokenizedLine>, TokenStats) {
        let mut lines = Vec::new();
        let mut stats = TokenStats::new();

        for line in self.lines() {
            stats.add_line(&line);
            lines.push(line);
        }

        (lines, stats)
    }

    /// Checks if the content appears to be binary.
    ///
    /// Content is considered binary if:
    /// - It contains null bytes
    /// - It has very long lines (over `max_line_length`)
    /// - It has a high ratio of non-printable characters
    pub fn is_binary(&self) -> bool {
        // Check for null bytes
        if self.content.contains(&0) {
            return true;
        }

        // Check line lengths
        let mut line_start = 0;
        for (i, &byte) in self.content.iter().enumerate() {
            if byte == b'\n' {
                let line_len = i - line_start;
                if line_len > self.options.get_max_line_length() {
                    return true;
                }
                line_start = i + 1;
            }
        }

        // Check final line
        let final_line_len = self.content.len() - line_start;
        if final_line_len > self.options.get_max_line_length() {
            return true;
        }

        // Check ratio of non-printable characters
        let non_printable = self
            .content
            .iter()
            .filter(|&&b| b < 0x20 && b != b'\n' && b != b'\r' && b != b'\t')
            .count();

        if !self.content.is_empty() {
            let ratio = non_printable as f64 / self.content.len() as f64;
            if ratio > 0.1 {
                return true;
            }
        }

        false
    }

    /// Tokenizes a single line of content.
    ///
    /// This is useful for tokenizing individual lines without creating
    /// an iterator over the entire content.
    pub fn tokenize_line(line_content: &[u8], options: &TokenizeOptions) -> TokenizedLine {
        Self::tokenize_line_internal(line_content, 0, options)
    }

    /// Internal method to tokenize a line with a specific line number.
    pub(crate) fn tokenize_line_internal(
        line_content: &[u8],
        line_number: usize,
        options: &TokenizeOptions,
    ) -> TokenizedLine {
        if line_content.is_empty() {
            return TokenizedLine::empty(line_number);
        }

        let config = options.to_tokenizer_config();
        let tokenizer = Tokenizer::with_config(line_content, config);

        let mut tokens = Vec::new();
        let mut prev_whitespace: Option<(usize, Vec<u8>)> = None;

        for token in tokenizer {
            let kind = token.kind();
            let content_bytes = token.content().to_vec();
            let offset = token.offset();
            let len = content_bytes.len();

            // Handle whitespace merging
            if options.merge_whitespace() && kind == TokenKind::Whitespace {
                if let Some((start, mut ws_content)) = prev_whitespace.take() {
                    ws_content.extend_from_slice(&content_bytes);
                    prev_whitespace = Some((start, ws_content));
                } else {
                    prev_whitespace = Some((offset, content_bytes));
                }
                continue;
            }

            // Flush any accumulated whitespace
            if let Some((ws_start, ws_content)) = prev_whitespace.take() {
                let ws_end = ws_start + ws_content.len();
                tokens.push(TokenizedToken::new(
                    TokenKind::Whitespace,
                    ws_content,
                    ws_start..ws_end,
                ));
            }

            // Skip newlines unless explicitly included
            if kind == TokenKind::Newline && !options.get_include_newlines() {
                continue;
            }

            tokens.push(TokenizedToken::new(
                kind,
                content_bytes,
                offset..offset + len,
            ));
        }

        // Flush any remaining whitespace
        if let Some((ws_start, ws_content)) = prev_whitespace {
            let ws_end = ws_start + ws_content.len();
            tokens.push(TokenizedToken::new(
                TokenKind::Whitespace,
                ws_content,
                ws_start..ws_end,
            ));
        }

        TokenizedLine::new(line_number, line_content.to_vec(), tokens)
    }
}

// ============================================================================
// LINE ITERATOR
// ============================================================================

/// Iterator over tokenized lines.
///
/// Splits content into lines and tokenizes each one lazily.
#[derive(Debug, Clone)]
pub struct LineIterator<'a> {
    content: &'a [u8],
    options: TokenizeOptions,
    position: usize,
    line_number: usize,
}

impl<'a> Iterator for LineIterator<'a> {
    type Item = TokenizedLine;

    fn next(&mut self) -> Option<Self::Item> {
        if self.position >= self.content.len() {
            return None;
        }

        // Find the end of this line
        let start = self.position;
        let mut end = start;

        while end < self.content.len() && self.content[end] != b'\n' {
            end += 1;
        }

        // Extract line content (without newline)
        let line_content = &self.content[start..end];

        // Move past the newline
        self.position = if end < self.content.len() {
            end + 1
        } else {
            end
        };

        let line_number = self.line_number;
        self.line_number += 1;

        Some(ContentTokenizer::tokenize_line_internal(
            line_content,
            line_number,
            &self.options,
        ))
    }
}

impl<'a> LineIterator<'a> {
    /// Returns the current line number (0-indexed).
    #[inline]
    pub fn current_line_number(&self) -> usize {
        self.line_number
    }

    /// Returns the current byte position in the content.
    #[inline]
    pub fn current_position(&self) -> usize {
        self.position
    }

    /// Returns true if there is more content to process.
    #[inline]
    pub fn has_more(&self) -> bool {
        self.position < self.content.len()
    }

    /// Returns the remaining bytes to process.
    #[inline]
    pub fn remaining_bytes(&self) -> usize {
        self.content.len().saturating_sub(self.position)
    }
}
