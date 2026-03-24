//! Line splitting utilities.
//!
//! This module provides utilities for splitting byte sequences into lines
//! or chunks for diffing. It supports both the default newline separator
//! and custom separators for special use cases.
//!
//! # Separators
//!
//! The default separator is the newline character (`\n`), which works for
//! most text files. However, some use cases require different separators:
//!
//! - **CRLF files**: Use `\r\n` as separator on Windows
//! - **Record-oriented data**: Use custom record delimiters
//! - **Paragraph diffing**: Use blank lines (`\n\n`) as separator
//!
//! # Design
//!
//! The [`LineSplit`] iterator is zero-copy - it returns slices into the
//! original content rather than allocating new strings. This is efficient
//! for large files where we want to avoid copying data.
//!
//! # Example
//!
//! ```rust
//! use atomic_core::diff::{LineSplit, Separator};
//!
//! let content = b"line1\nline2\nline3";
//! let lines: Vec<_> = LineSplit::new(content, &Separator::Newline).collect();
//!
//! assert_eq!(lines.len(), 3);
//! ```

use super::line::Line;

/// A separator pattern for splitting content into lines.
///
/// The separator defines how content is split into comparable units.
/// For most text files, this is the newline character, but custom
/// separators can be used for special formats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Separator {
    /// Split on newline (`\n`) - the default for Unix text files.
    Newline,

    /// Split on carriage return + newline (`\r\n`) - for Windows text files.
    CrLf,

    /// Split on any byte sequence.
    ///
    /// The separator bytes are included at the end of each chunk
    /// (except possibly the last chunk if content doesn't end with separator).
    Custom(Vec<u8>),

    /// Split on a single byte.
    ///
    /// More efficient than Custom for single-byte separators.
    Byte(u8),
}

impl Default for Separator {
    /// Returns the default separator (newline).
    fn default() -> Self {
        Separator::Newline
    }
}

impl Separator {
    /// Create a separator from a byte slice.
    ///
    /// Optimizes for common cases:
    /// - Single `\n` → `Separator::Newline`
    /// - `\r\n` → `Separator::CrLf`
    /// - Single byte → `Separator::Byte`
    /// - Otherwise → `Separator::Custom`
    pub fn from_bytes(bytes: &[u8]) -> Self {
        match bytes {
            b"\n" => Separator::Newline,
            b"\r\n" => Separator::CrLf,
            [b] => Separator::Byte(*b),
            _ => Separator::Custom(bytes.to_vec()),
        }
    }

    /// Get the separator as a byte slice.
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Separator::Newline => b"\n",
            Separator::CrLf => b"\r\n",
            Separator::Byte(b) => std::slice::from_ref(b),
            Separator::Custom(v) => v.as_slice(),
        }
    }

    /// Get the length of the separator in bytes.
    pub fn len(&self) -> usize {
        match self {
            Separator::Newline => 1,
            Separator::CrLf => 2,
            Separator::Byte(_) => 1,
            Separator::Custom(v) => v.len(),
        }
    }

    /// Check if the separator is empty.
    pub fn is_empty(&self) -> bool {
        matches!(self, Separator::Custom(v) if v.is_empty())
    }

    /// Find the next occurrence of this separator in the given bytes.
    ///
    /// Returns the index of the start of the separator, or None if not found.
    fn find_in(&self, haystack: &[u8]) -> Option<usize> {
        match self {
            Separator::Newline => haystack.iter().position(|&b| b == b'\n'),
            Separator::Byte(b) => haystack.iter().position(|&x| x == *b),
            Separator::CrLf => {
                // Find \r\n sequence
                if haystack.len() < 2 {
                    return None;
                }
                haystack.windows(2).position(|window| window == b"\r\n")
            }
            Separator::Custom(needle) => {
                if needle.is_empty() || needle.len() > haystack.len() {
                    return None;
                }
                // Simple substring search
                haystack
                    .windows(needle.len())
                    .position(|window| window == needle.as_slice())
            }
        }
    }
}

/// An iterator that splits content into lines based on a separator.
///
/// This is a zero-copy iterator - it yields [`Line`] references into
/// the original content without allocating new memory for each line.
///
/// # Example
///
/// ```rust
/// use atomic_core::diff::{LineSplit, Separator, Line};
///
/// let content = b"a\nb\nc\n";
/// let mut split = LineSplit::new(content, &Separator::Newline);
///
/// assert_eq!(split.next().unwrap().content(), b"a\n");
/// assert_eq!(split.next().unwrap().content(), b"b\n");
/// assert_eq!(split.next().unwrap().content(), b"c\n");
/// assert!(split.next().is_none());
/// ```
pub struct LineSplit<'a> {
    /// The remaining content to split.
    content: &'a [u8],

    /// The separator pattern.
    separator: &'a Separator,

    /// Current position in the original content (for tracking).
    position: usize,

    /// Whether we've finished iterating.
    finished: bool,
}

impl<'a> LineSplit<'a> {
    /// Create a new line splitter.
    ///
    /// # Arguments
    ///
    /// * `content` - The bytes to split into lines
    /// * `separator` - The separator pattern to use
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::diff::{LineSplit, Separator};
    ///
    /// let content = b"line1\nline2\n";
    /// let lines: Vec<_> = LineSplit::new(content, &Separator::Newline).collect();
    /// assert_eq!(lines.len(), 2);
    /// ```
    pub fn new(content: &'a [u8], separator: &'a Separator) -> Self {
        Self {
            content,
            separator,
            position: 0,
            finished: content.is_empty(),
        }
    }

    /// Create a line splitter using the default newline separator.
    ///
    /// Convenience method equivalent to `LineSplit::new(content, &Separator::Newline)`.
    pub fn lines(content: &'a [u8]) -> Self {
        // We need a static reference for the default separator
        static DEFAULT_SEP: Separator = Separator::Newline;
        Self::new(content, &DEFAULT_SEP)
    }

    /// Get the current byte position in the original content.
    pub fn position(&self) -> usize {
        self.position
    }

    /// Check if iteration is complete.
    pub fn is_finished(&self) -> bool {
        self.finished
    }
}

impl<'a> Iterator for LineSplit<'a> {
    type Item = Line<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }

        match self.separator.find_in(self.content) {
            Some(sep_pos) => {
                // Found separator - return line including separator
                let sep_len = self.separator.len();
                let line_end = sep_pos + sep_len;
                let line = &self.content[..line_end];

                // Advance past this line
                self.content = &self.content[line_end..];
                self.position += line_end;

                // Check if this is the last line
                let is_last = self.content.is_empty();
                if is_last {
                    self.finished = true;
                    Some(Line::new_last(line))
                } else {
                    Some(Line::new(line))
                }
            }
            None => {
                // No more separators - return remaining content as final line
                self.finished = true;
                if self.content.is_empty() {
                    None
                } else {
                    let line = self.content;
                    self.content = &[];
                    Some(Line::new_last(line))
                }
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        if self.finished {
            (0, Some(0))
        } else {
            // At least 1 item, at most content.len() / separator.len() + 1
            let max_lines = self.content.len() / self.separator.len().max(1) + 1;
            (1, Some(max_lines))
        }
    }
}

/// Split content into lines using the default newline separator.
///
/// This is a convenience function that collects all lines into a Vec.
///
/// # Example
///
/// ```rust
/// use atomic_core::diff::Line;
///
/// let lines = Line::from_bytes(b"a\nb\nc\n");
/// assert_eq!(lines.len(), 3);
/// ```
#[allow(dead_code)]
pub(crate) fn split_lines(content: &[u8]) -> Vec<Line<'_>> {
    Line::from_bytes(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Separator Tests
    #[test]
    fn test_separator_default() {
        assert_eq!(Separator::default(), Separator::Newline);
    }

    #[test]
    fn test_separator_from_bytes_newline() {
        assert_eq!(Separator::from_bytes(b"\n"), Separator::Newline);
    }

    #[test]
    fn test_separator_from_bytes_crlf() {
        assert_eq!(Separator::from_bytes(b"\r\n"), Separator::CrLf);
    }

    #[test]
    fn test_separator_from_bytes_single() {
        assert_eq!(Separator::from_bytes(b":"), Separator::Byte(b':'));
    }

    #[test]
    fn test_separator_from_bytes_custom() {
        let sep = Separator::from_bytes(b"|||");
        assert!(matches!(sep, Separator::Custom(_)));
    }

    #[test]
    fn test_separator_as_bytes() {
        assert_eq!(Separator::Newline.as_bytes(), b"\n");
        assert_eq!(Separator::CrLf.as_bytes(), b"\r\n");
        assert_eq!(Separator::Byte(b':').as_bytes(), b":");
        assert_eq!(Separator::Custom(b"||".to_vec()).as_bytes(), b"||");
    }

    #[test]
    fn test_separator_len() {
        assert_eq!(Separator::Newline.len(), 1);
        assert_eq!(Separator::CrLf.len(), 2);
        assert_eq!(Separator::Byte(b':').len(), 1);
        assert_eq!(Separator::Custom(b"|||".to_vec()).len(), 3);
    }

    #[test]
    fn test_separator_is_empty() {
        assert!(!Separator::Newline.is_empty());
        assert!(Separator::Custom(Vec::new()).is_empty());
    }

    #[test]
    fn test_separator_find_newline() {
        let sep = Separator::Newline;
        assert_eq!(sep.find_in(b"hello\nworld"), Some(5));
        assert_eq!(sep.find_in(b"no newline"), None);
        assert_eq!(sep.find_in(b"\n"), Some(0));
    }

    #[test]
    fn test_separator_find_crlf() {
        let sep = Separator::CrLf;
        assert_eq!(sep.find_in(b"hello\r\nworld"), Some(5));
        assert_eq!(sep.find_in(b"hello\nworld"), None);
        assert_eq!(sep.find_in(b"hello\rworld"), None);
        assert_eq!(sep.find_in(b"\r\n"), Some(0));
    }

    #[test]
    fn test_separator_find_byte() {
        let sep = Separator::Byte(b':');
        assert_eq!(sep.find_in(b"key:value"), Some(3));
        assert_eq!(sep.find_in(b"no colon"), None);
    }

    #[test]
    fn test_separator_find_custom() {
        let sep = Separator::Custom(b"||".to_vec());
        assert_eq!(sep.find_in(b"a||b||c"), Some(1));
        assert_eq!(sep.find_in(b"a|b|c"), None);
    }

    // LineSplit Tests
    #[test]
    fn test_linesplit_simple() {
        let content = b"a\nb\nc\n";
        let lines: Vec<_> = LineSplit::new(content, &Separator::Newline).collect();

        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].content(), b"a\n");
        assert_eq!(lines[1].content(), b"b\n");
        assert_eq!(lines[2].content(), b"c\n");
    }

    #[test]
    fn test_linesplit_no_trailing_newline() {
        let content = b"a\nb\nc";
        let lines: Vec<_> = LineSplit::new(content, &Separator::Newline).collect();

        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].content(), b"a\n");
        assert_eq!(lines[1].content(), b"b\n");
        assert_eq!(lines[2].content(), b"c");
        assert!(lines[2].is_last());
    }

    #[test]
    fn test_linesplit_empty() {
        let content = b"";
        let lines: Vec<_> = LineSplit::new(content, &Separator::Newline).collect();
        assert!(lines.is_empty());
    }

    #[test]
    fn test_linesplit_single_line() {
        let content = b"hello\n";
        let lines: Vec<_> = LineSplit::new(content, &Separator::Newline).collect();

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].content(), b"hello\n");
        assert!(lines[0].is_last());
    }

    #[test]
    fn test_linesplit_single_line_no_newline() {
        let content = b"hello";
        let lines: Vec<_> = LineSplit::new(content, &Separator::Newline).collect();

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].content(), b"hello");
        assert!(lines[0].is_last());
    }

    #[test]
    fn test_linesplit_only_newlines() {
        let content = b"\n\n\n";
        let lines: Vec<_> = LineSplit::new(content, &Separator::Newline).collect();

        assert_eq!(lines.len(), 3);
        for line in &lines {
            assert_eq!(line.content(), b"\n");
        }
    }

    #[test]
    fn test_linesplit_crlf() {
        let content = b"a\r\nb\r\nc\r\n";
        let lines: Vec<_> = LineSplit::new(content, &Separator::CrLf).collect();

        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].content(), b"a\r\n");
        assert_eq!(lines[1].content(), b"b\r\n");
        assert_eq!(lines[2].content(), b"c\r\n");
    }

    #[test]
    fn test_linesplit_custom_separator() {
        let content = b"field1||field2||field3";
        let sep = Separator::Custom(b"||".to_vec());
        let lines: Vec<_> = LineSplit::new(content, &sep).collect();

        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].content(), b"field1||");
        assert_eq!(lines[1].content(), b"field2||");
        assert_eq!(lines[2].content(), b"field3");
    }

    #[test]
    fn test_linesplit_byte_separator() {
        let content = b"a:b:c:";
        let sep = Separator::Byte(b':');
        let lines: Vec<_> = LineSplit::new(content, &sep).collect();

        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].content(), b"a:");
        assert_eq!(lines[1].content(), b"b:");
        assert_eq!(lines[2].content(), b"c:");
    }

    #[test]
    fn test_linesplit_lines_convenience() {
        let content = b"a\nb\n";
        let lines: Vec<_> = LineSplit::lines(content).collect();

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].content(), b"a\n");
        assert_eq!(lines[1].content(), b"b\n");
    }

    #[test]
    fn test_linesplit_position_tracking() {
        let content = b"aa\nbbb\ncccc\n";
        let mut split = LineSplit::new(content, &Separator::Newline);

        assert_eq!(split.position(), 0);

        split.next();
        assert_eq!(split.position(), 3); // "aa\n"

        split.next();
        assert_eq!(split.position(), 7); // "bbb\n"

        split.next();
        assert_eq!(split.position(), 12); // "cccc\n"

        assert!(split.is_finished());
    }

    #[test]
    fn test_linesplit_size_hint() {
        let content = b"a\nb\nc\n";
        let split = LineSplit::new(content, &Separator::Newline);

        let (min, max) = split.size_hint();
        assert!(min >= 1);
        assert!(max.is_some());
        assert!(max.unwrap() >= 3);
    }

    #[test]
    fn test_linesplit_size_hint_finished() {
        let content = b"";
        let split = LineSplit::new(content, &Separator::Newline);

        assert_eq!(split.size_hint(), (0, Some(0)));
    }

    // split_lines Tests
    #[test]
    fn test_split_lines() {
        let lines = split_lines(b"hello\nworld\n");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].content(), b"hello\n");
        assert_eq!(lines[1].content(), b"world\n");
    }

    #[test]
    fn test_split_lines_empty() {
        let lines = split_lines(b"");
        assert!(lines.is_empty());
    }

    #[test]
    fn test_last_line_marked() {
        let lines = split_lines(b"a\nb\nc");
        assert!(!lines[0].is_last());
        assert!(!lines[1].is_last());
        assert!(lines[2].is_last());
    }

    // Edge Cases
    #[test]
    fn test_mixed_line_endings() {
        // When using newline separator, \r\n appears as \r at end of line
        let content = b"unix\nwindows\r\nmac\r";
        let lines: Vec<_> = LineSplit::new(content, &Separator::Newline).collect();

        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].content(), b"unix\n");
        assert_eq!(lines[1].content(), b"windows\r\n");
        assert_eq!(lines[2].content(), b"mac\r");
    }

    #[test]
    fn test_binary_content() {
        let content = &[0x00, 0x01, 0x0a, 0x02, 0x03, 0x0a];
        let lines: Vec<_> = LineSplit::new(content, &Separator::Newline).collect();

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].content(), &[0x00, 0x01, 0x0a]);
        assert_eq!(lines[1].content(), &[0x02, 0x03, 0x0a]);
    }

    #[test]
    fn test_very_long_line() {
        let mut content = vec![b'x'; 10000];
        content.push(b'\n');
        content.extend_from_slice(&[b'y'; 100]);
        content.push(b'\n');

        let lines: Vec<_> = LineSplit::new(&content, &Separator::Newline).collect();

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].len(), 10001);
        assert_eq!(lines[1].len(), 101);
    }
}
