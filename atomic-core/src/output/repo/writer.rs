//! Conflict-aware writer for repository output.
//!
//! This module provides [`ConflictWriter`], a writer wrapper that implements
//! the [`VertexBuffer`] trait and automatically inserts conflict markers
//! when outputting conflicting content.
//!
//! # Overview
//!
//! When outputting repository state to files, conflicts may be detected:
//!
//! - **Order conflicts**: Multiple valid orderings for content
//! - **Zombie conflicts**: Deleted content that was modified
//! - **Cyclic conflicts**: Circular dependencies in the graph
//!
//! The `ConflictWriter` handles these by:
//!
//! 1. Tracking the current line number for conflict reporting
//! 2. Inserting appropriate conflict markers into the output
//! 3. Recording conflict metadata for later reporting
//!
//! # Conflict Marker Format
//!
//! Conflicts are marked in the output using a format similar to other VCS tools:
//!
//! ```text
//! >>>>>>> 1 [ABCD1234]
//! Content from first change
//! ======= 1 [EFGH5678]
//! Content from second change
//! <<<<<<< 1
//! ```
//!
//! The number after each marker is a conflict ID that matches the begin/end pairs.
//! The bracketed text contains the change hash (truncated) that introduced
//! each side of the conflict.
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_core::output::repo::ConflictWriter;
//! use atomic_core::output::VertexBuffer;
//! use atomic_core::types::{NodeId, Position, ChangePosition};
//!
//! // Create a writer wrapping a Vec<u8>
//! let mut buffer = Vec::new();
//! let position = Position::new(NodeId::ROOT, ChangePosition::new(0));
//! let mut writer = ConflictWriter::new(&mut buffer, "test.rs", position);
//!
//! // Begin a conflict
//! writer.begin_conflict(1, None)?;
//!
//! // Write content for each side
//! // (normally done via output_line)
//!
//! // End the conflict
//! writer.end_conflict(1)?;
//!
//! // Check for recorded conflicts
//! let conflicts = writer.take_conflicts();
//! assert_eq!(conflicts.len(), 1);
//! ```
//!
//! # Thread Safety
//!
//! `ConflictWriter` is not thread-safe. For parallel output, create separate
//! writers for each thread and merge conflicts afterward.

use super::conflict::{FileConflict, FileConflictType};
use crate::output::traits::VertexBuffer;
use crate::types::{Base32, GraphNode, Hash, NodeId, Position};
use std::io::Write;

// CONFLICT MARKERS

/// Conflict marker strings.
pub mod markers {
    /// Start of a conflict region.
    pub const START: &str = ">>>>>>>";

    /// Separator between conflict sides.
    pub const SEPARATOR: &str = "=======";

    /// End of a conflict region.
    pub const END: &str = "<<<<<<<";
}

// CONFLICT WRITER

/// A writer that tracks and outputs conflict markers.
///
/// This struct wraps an underlying writer and implements [`VertexBuffer`] to
/// automatically insert conflict markers when outputting conflicting content.
/// It tracks:
///
/// - Current line number (for conflict location reporting)
/// - Whether we're at the start of a line (for proper marker placement)
/// - All conflicts detected during output
/// - Total bytes written
///
/// # Type Parameter
///
/// * `W` - The underlying writer type (must implement `Write`)
///
/// # Example
///
/// ```rust
/// use atomic_core::output::repo::ConflictWriter;
/// use atomic_core::types::{NodeId, Position, ChangePosition};
/// use std::io::Write;
///
/// let mut buffer = Vec::new();
/// let position = Position::new(NodeId::ROOT, ChangePosition::new(0));
/// let mut writer = ConflictWriter::new(&mut buffer, "file.rs", position);
///
/// // Write some content
/// writer.write_all(b"Hello, world!\n").unwrap();
///
/// assert_eq!(writer.bytes_written(), 14);
/// assert_eq!(writer.current_line(), 2); // Started at 1, added 1 newline
/// ```
#[derive(Debug)]
pub struct ConflictWriter<W: Write> {
    /// The underlying writer.
    writer: W,

    /// Path to the file being written (for conflict reporting).
    path: String,

    /// The graph position of the file's inode span.
    position: Position<NodeId>,

    /// Conflicts detected during writing.
    conflicts: Vec<FileConflict>,

    /// Current line number (1-based).
    line: u32,

    /// Counter for generating conflict IDs.
    #[allow(dead_code)]
    next_conflict_id: u32,

    /// Whether we're at the start of a line.
    ///
    /// This is used to ensure conflict markers always appear on their own line.
    at_line_start: bool,

    /// Total bytes written to the underlying writer.
    bytes_written: u64,

    /// Reusable buffer for span content.
    content_buffer: Vec<u8>,
}

impl<W: Write> ConflictWriter<W> {
    /// Create a new conflict writer.
    ///
    /// # Arguments
    ///
    /// * `writer` - The underlying writer to wrap
    /// * `path` - Path to the file being written (for conflict reporting)
    /// * `position` - The graph position of this file's inode
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::ConflictWriter;
    /// use atomic_core::types::{NodeId, Position, ChangePosition};
    ///
    /// let mut buffer = Vec::new();
    /// let pos = Position::new(NodeId::new(1), ChangePosition::new(0));
    /// let writer = ConflictWriter::new(&mut buffer, "src/main.rs", pos);
    ///
    /// assert_eq!(writer.path(), "src/main.rs");
    /// assert!(writer.conflicts().is_empty());
    /// ```
    pub fn new(writer: W, path: &str, position: Position<NodeId>) -> Self {
        Self {
            writer,
            path: path.to_string(),
            position,
            conflicts: Vec::new(),
            line: 1,
            next_conflict_id: 1,
            at_line_start: true,
            bytes_written: 0,
            content_buffer: Vec::with_capacity(4096),
        }
    }

    /// Get the path of the file being written.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Get the graph position of this file.
    pub fn position(&self) -> Position<NodeId> {
        self.position
    }

    /// Get the current line number (1-based).
    ///
    /// This is the line number where the next write will occur.
    pub fn current_line(&self) -> u32 {
        self.line
    }

    /// Get the total bytes written.
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    /// Get a reference to the conflicts detected so far.
    pub fn conflicts(&self) -> &[FileConflict] {
        &self.conflicts
    }

    /// Take ownership of the detected conflicts.
    ///
    /// After calling this, the internal conflict list is empty.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::ConflictWriter;
    /// use atomic_core::types::{NodeId, Position, ChangePosition};
    ///
    /// let mut buffer = Vec::new();
    /// let pos = Position::new(NodeId::ROOT, ChangePosition::new(0));
    /// let mut writer = ConflictWriter::new(&mut buffer, "test.rs", pos);
    ///
    /// // ... write some content with conflicts ...
    ///
    /// let conflicts = writer.take_conflicts();
    /// // conflicts is now owned, writer.conflicts() is empty
    /// assert!(writer.conflicts().is_empty());
    /// ```
    pub fn take_conflicts(&mut self) -> Vec<FileConflict> {
        std::mem::take(&mut self.conflicts)
    }

    /// Check if any conflicts were detected.
    pub fn has_conflicts(&self) -> bool {
        !self.conflicts.is_empty()
    }

    /// Get the number of conflicts detected.
    pub fn conflict_count(&self) -> usize {
        self.conflicts.len()
    }

    /// Consume the writer and return the underlying writer.
    ///
    /// This is useful when you need access to the underlying writer after
    /// output is complete (e.g., to flush or close it).
    pub fn into_inner(self) -> W {
        self.writer
    }

    /// Get a reference to the underlying writer.
    pub fn inner(&self) -> &W {
        &self.writer
    }

    /// Get a mutable reference to the underlying writer.
    pub fn inner_mut(&mut self) -> &mut W {
        &mut self.writer
    }

    /// Allocate the next conflict ID.
    #[allow(dead_code)]
    fn allocate_conflict_id(&mut self) -> u32 {
        let id = self.next_conflict_id;
        self.next_conflict_id += 1;
        id
    }

    /// Ensure we're at the start of a line before writing a marker.
    ///
    /// If we're in the middle of a line, this writes a newline first.
    fn ensure_line_start(&mut self) -> std::io::Result<()> {
        if !self.at_line_start {
            self.writer.write_all(b"\n")?;
            self.bytes_written += 1;
            self.line += 1;
            self.at_line_start = true;
        }
        Ok(())
    }

    /// Count newlines in a byte slice.
    fn count_newlines(data: &[u8]) -> u32 {
        data.iter().filter(|&&b| b == b'\n').count() as u32
    }

    /// Write a conflict marker line.
    ///
    /// The format is: `MARKER ID [HASH]` followed by a newline.
    fn write_marker(&mut self, marker: &str, id: u32, hash: Option<&Hash>) -> std::io::Result<()> {
        self.ensure_line_start()?;

        // Write marker and ID
        write!(self.writer, "{} {}", marker, id)?;
        self.bytes_written += marker.len() as u64 + 1 + count_digits(id) as u64;

        // Write hash if provided
        if let Some(h) = hash {
            let hash_str = h.to_base32();
            // Only include first 8 chars of hash for readability
            let short_hash = if hash_str.len() > 8 {
                &hash_str[..8]
            } else {
                &hash_str
            };
            write!(self.writer, " [{}]", short_hash)?;
            self.bytes_written += 3 + short_hash.len() as u64; // " [" + hash + "]"
        }

        // Write newline
        self.writer.write_all(b"\n")?;
        self.bytes_written += 1;
        self.line += 1;
        self.at_line_start = true;

        Ok(())
    }

    /// Record a conflict with the given type.
    fn record_conflict(&mut self, conflict_type: FileConflictType, id: u32, hash: Option<&Hash>) {
        let mut conflict = FileConflict::new(self.path.clone(), conflict_type)
            .at_line(self.line)
            .with_id(id);

        if let Some(h) = hash {
            conflict = conflict.with_change(*h);
        }

        self.conflicts.push(conflict);
    }
}

/// Count the number of digits in a u32.
fn count_digits(n: u32) -> u32 {
    if n == 0 {
        return 1;
    }
    let mut count = 0;
    let mut num = n;
    while num > 0 {
        count += 1;
        num /= 10;
    }
    count
}

// WRITE TRAIT IMPLEMENTATION

impl<W: Write> Write for ConflictWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.writer.write(buf)?;
        self.bytes_written += n as u64;
        self.line += Self::count_newlines(&buf[..n]);
        self.at_line_start = buf.get(n.saturating_sub(1)).copied() == Some(b'\n');
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

// VERTEX BUFFER IMPLEMENTATION

impl<W: Write> VertexBuffer for ConflictWriter<W> {
    fn output_line<E, F>(&mut self, node: GraphNode<NodeId>, get_contents: F) -> Result<(), E>
    where
        E: From<std::io::Error>,
        F: FnOnce(&mut [u8]) -> Result<(), E>,
    {
        // Calculate node length
        let len = (node.end.get() - node.start.get()) as usize;
        if len == 0 {
            return Ok(());
        }

        // Resize buffer and get contents
        self.content_buffer.clear();
        self.content_buffer.resize(len, 0);
        get_contents(&mut self.content_buffer)?;

        // Track line endings
        let ends_with_newline = self.content_buffer.ends_with(b"\n");
        self.line += Self::count_newlines(&self.content_buffer);

        // Write to underlying writer
        self.writer.write_all(&self.content_buffer)?;
        self.bytes_written += len as u64;

        // Update line start tracking
        if !self.content_buffer.is_empty() {
            self.at_line_start = ends_with_newline;
        }

        Ok(())
    }

    fn output_conflict_marker(
        &mut self,
        marker: &str,
        id: usize,
        changes: Option<&[Hash]>,
    ) -> Result<(), std::io::Error> {
        let hash = changes.and_then(|c| c.first());
        self.write_marker(marker, id as u32, hash)
    }

    fn begin_conflict(
        &mut self,
        id: usize,
        changes: Option<&[Hash]>,
    ) -> Result<(), std::io::Error> {
        let hash = changes.and_then(|c| c.first());
        self.record_conflict(FileConflictType::Order, id as u32, hash);
        self.write_marker(markers::START, id as u32, hash)
    }

    fn begin_zombie_conflict(
        &mut self,
        id: usize,
        changes: Option<&[Hash]>,
    ) -> Result<(), std::io::Error> {
        let hash = changes.and_then(|c| c.first());
        self.record_conflict(FileConflictType::Zombie, id as u32, hash);
        self.write_marker(markers::START, id as u32, hash)
    }

    fn begin_cyclic_conflict(&mut self, id: usize) -> Result<(), std::io::Error> {
        self.record_conflict(FileConflictType::Cyclic, id as u32, None);
        self.write_marker(markers::START, id as u32, None)
    }

    fn conflict_next(&mut self, id: usize, changes: Option<&[Hash]>) -> Result<(), std::io::Error> {
        let hash = changes.and_then(|c| c.first());

        // Add hash to the existing conflict record
        if let Some(h) = hash {
            if let Some(conflict) = self
                .conflicts
                .iter_mut()
                .rev()
                .find(|c| c.id == Some(id as u32))
            {
                conflict.changes.push(*h);
            }
        }

        self.write_marker(markers::SEPARATOR, id as u32, hash)
    }

    fn end_conflict(&mut self, id: usize) -> Result<(), std::io::Error> {
        self.write_marker(markers::END, id as u32, None)
    }

    fn end_zombie_conflict(&mut self, id: usize) -> Result<(), std::io::Error> {
        self.write_marker(markers::END, id as u32, None)
    }

    fn end_cyclic_conflict(&mut self, id: usize) -> Result<(), std::io::Error> {
        self.write_marker(markers::END, id as u32, None)
    }
}

// TESTS

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ChangePosition;

    fn test_position() -> Position<NodeId> {
        Position::new(NodeId::ROOT, ChangePosition::new(0))
    }

    /// Helper to create a writer and run a test function, then return the buffer contents
    fn with_writer<F>(f: F) -> Vec<u8>
    where
        F: FnOnce(&mut ConflictWriter<&mut Vec<u8>>),
    {
        let mut buffer = Vec::new();
        {
            let mut writer = ConflictWriter::new(&mut buffer, "test.rs", test_position());
            f(&mut writer);
        }
        buffer
    }

    // ------------------------------------------------------------------------
    // Constructor Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_new() {
        let mut buffer = Vec::new();
        let writer = ConflictWriter::new(&mut buffer, "test.rs", test_position());

        assert_eq!(writer.path(), "test.rs");
        assert_eq!(writer.current_line(), 1);
        assert_eq!(writer.bytes_written(), 0);
        assert!(writer.conflicts().is_empty());
        assert!(!writer.has_conflicts());
    }

    #[test]
    fn test_position_stored() {
        let pos = Position::new(NodeId::new(42), ChangePosition::new(100));
        let mut buffer = Vec::new();
        let writer = ConflictWriter::new(&mut buffer, "file.rs", pos);

        assert_eq!(writer.position(), pos);
    }

    // ------------------------------------------------------------------------
    // Write Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_write_simple() {
        let buffer = with_writer(|writer| {
            writer.write_all(b"Hello").unwrap();
            assert_eq!(writer.bytes_written(), 5);
        });
        assert_eq!(buffer, b"Hello");
    }

    #[test]
    fn test_write_with_newlines() {
        with_writer(|writer| {
            writer.write_all(b"Line 1\nLine 2\nLine 3\n").unwrap();
            assert_eq!(writer.current_line(), 4); // Started at 1, added 3 newlines
            assert_eq!(writer.bytes_written(), 21);
        });
    }

    #[test]
    fn test_write_tracks_line_start() {
        with_writer(|writer| {
            // Initially at line start
            assert!(writer.at_line_start);

            // After partial line, not at start
            writer.write_all(b"partial").unwrap();
            assert!(!writer.at_line_start);

            // After newline, at start again
            writer.write_all(b"\n").unwrap();
            assert!(writer.at_line_start);
        });
    }

    // ------------------------------------------------------------------------
    // Conflict Marker Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_begin_conflict() {
        let buffer = with_writer(|writer| {
            writer.begin_conflict(1, None).unwrap();

            assert!(writer.has_conflicts());
            assert_eq!(writer.conflict_count(), 1);
            assert_eq!(writer.conflicts()[0].conflict_type, FileConflictType::Order);
        });
        let output = String::from_utf8(buffer).unwrap();
        assert!(output.contains(">>>>>>> 1"));
    }

    #[test]
    fn test_begin_zombie_conflict() {
        with_writer(|writer| {
            writer.begin_zombie_conflict(1, None).unwrap();
            assert_eq!(
                writer.conflicts()[0].conflict_type,
                FileConflictType::Zombie
            );
        });
    }

    #[test]
    fn test_begin_cyclic_conflict() {
        with_writer(|writer| {
            writer.begin_cyclic_conflict(1).unwrap();
            assert_eq!(
                writer.conflicts()[0].conflict_type,
                FileConflictType::Cyclic
            );
        });
    }

    #[test]
    fn test_conflict_with_hash() {
        let hash = Hash::of(b"test change");
        let buffer = with_writer(|writer| {
            writer.begin_conflict(1, Some(&[hash])).unwrap();
        });
        let output = String::from_utf8(buffer).unwrap();
        // Should contain truncated hash
        assert!(output.contains("["));
        assert!(output.contains("]"));
    }

    #[test]
    fn test_full_conflict_sequence() {
        let buffer = with_writer(|writer| {
            writer.begin_conflict(1, None).unwrap();
            writer.write_all(b"Side A\n").unwrap();
            writer.conflict_next(1, None).unwrap();
            writer.write_all(b"Side B\n").unwrap();
            writer.end_conflict(1).unwrap();
        });
        let output = String::from_utf8(buffer).unwrap();
        assert!(output.contains(">>>>>>> 1"));
        assert!(output.contains("Side A"));
        assert!(output.contains("======= 1"));
        assert!(output.contains("Side B"));
        assert!(output.contains("<<<<<<< 1"));
    }

    #[test]
    fn test_ensure_line_start_before_marker() {
        let buffer = with_writer(|writer| {
            // Write partial line without newline
            writer.write_all(b"partial").unwrap();

            // Begin conflict should add newline first
            writer.begin_conflict(1, None).unwrap();
        });
        let output = String::from_utf8(buffer).unwrap();
        assert!(output.starts_with("partial\n>>>>>>>"));
    }

    // ------------------------------------------------------------------------
    // Line Tracking Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_conflict_records_line() {
        with_writer(|writer| {
            writer.write_all(b"Line 1\nLine 2\nLine 3\n").unwrap();
            writer.begin_conflict(1, None).unwrap();

            // Conflict should be recorded at line 4
            assert_eq!(writer.conflicts()[0].line, Some(4));
        });
    }

    // ------------------------------------------------------------------------
    // Take Conflicts Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_take_conflicts() {
        with_writer(|writer| {
            writer.begin_conflict(1, None).unwrap();
            writer.end_conflict(1).unwrap();
            writer.begin_conflict(2, None).unwrap();
            writer.end_conflict(2).unwrap();

            let conflicts = writer.take_conflicts();
            assert_eq!(conflicts.len(), 2);
            assert!(writer.conflicts().is_empty());
        });
    }

    // ------------------------------------------------------------------------
    // Into Inner Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_into_inner() {
        let buffer = with_writer(|writer| {
            writer.write_all(b"test").unwrap();
        });
        assert_eq!(buffer, b"test");
    }

    // ------------------------------------------------------------------------
    // VertexBuffer output_line Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_output_line_basic() {
        let node = GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(0),
            ChangePosition::new(5),
        );

        let buffer = with_writer(|writer| {
            let result: Result<(), std::io::Error> = writer.output_line(node, |buf| {
                buf.copy_from_slice(b"hello");
                Ok(())
            });

            assert!(result.is_ok());
            assert_eq!(writer.bytes_written(), 5);
        });
        assert_eq!(buffer, b"hello");
    }

    #[test]
    fn test_output_line_with_newline() {
        let node = GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(0),
            ChangePosition::new(6),
        );

        with_writer(|writer| {
            let _: Result<(), std::io::Error> = writer.output_line(node, |buf| {
                buf.copy_from_slice(b"hello\n");
                Ok(())
            });

            assert_eq!(writer.current_line(), 2);
            assert!(writer.at_line_start);
        });
    }

    #[test]
    fn test_output_line_empty() {
        // Empty span (start == end)
        let node = GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(0),
            ChangePosition::new(0),
        );

        let buffer = with_writer(|writer| {
            let result: Result<(), std::io::Error> = writer.output_line(node, |_buf| {
                panic!("Should not be called for empty node");
            });

            assert!(result.is_ok());
        });
        assert!(buffer.is_empty());
    }

    // ------------------------------------------------------------------------
    // Count Digits Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_count_digits() {
        assert_eq!(count_digits(0), 1);
        assert_eq!(count_digits(1), 1);
        assert_eq!(count_digits(9), 1);
        assert_eq!(count_digits(10), 2);
        assert_eq!(count_digits(99), 2);
        assert_eq!(count_digits(100), 3);
        assert_eq!(count_digits(1000), 4);
        assert_eq!(count_digits(u32::MAX), 10);
    }

    // ------------------------------------------------------------------------
    // Flush Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_flush() {
        let buffer = with_writer(|writer| {
            writer.write_all(b"test").unwrap();
            writer.flush().unwrap();
        });
        assert_eq!(buffer, b"test");
    }

    // ------------------------------------------------------------------------
    // Multiple Conflicts Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_multiple_conflicts() {
        with_writer(|writer| {
            // First conflict
            writer.begin_conflict(1, None).unwrap();
            writer.write_all(b"A1\n").unwrap();
            writer.conflict_next(1, None).unwrap();
            writer.write_all(b"A2\n").unwrap();
            writer.end_conflict(1).unwrap();

            // Some normal content
            writer.write_all(b"normal\n").unwrap();

            // Second conflict
            writer.begin_conflict(2, None).unwrap();
            writer.write_all(b"B1\n").unwrap();
            writer.conflict_next(2, None).unwrap();
            writer.write_all(b"B2\n").unwrap();
            writer.end_conflict(2).unwrap();

            let conflicts = writer.conflicts();
            assert_eq!(conflicts.len(), 2);
            assert_eq!(conflicts[0].id, Some(1));
            assert_eq!(conflicts[1].id, Some(2));
        });
    }

    // ------------------------------------------------------------------------
    // Conflict Next Adds Hash Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_conflict_next_adds_hash() {
        let hash1 = Hash::of(b"change 1");
        let hash2 = Hash::of(b"change 2");

        with_writer(|writer| {
            writer.begin_conflict(1, Some(&[hash1])).unwrap();
            writer.conflict_next(1, Some(&[hash2])).unwrap();
            writer.end_conflict(1).unwrap();

            // The conflict should have both hashes
            let conflicts = writer.conflicts();
            assert_eq!(conflicts[0].changes.len(), 2);
        });
    }
}
