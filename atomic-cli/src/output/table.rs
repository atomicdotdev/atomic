//! Table formatting utilities for structured CLI output.
//!
//! This module provides a simple, flexible table formatting system for
//! displaying tabular data in the Atomic CLI. Tables are commonly used
//! for listing stacks, showing change history, and displaying status.
//!
//! # Design Philosophy
//!
//! The table system prioritizes:
//! 1. **Simplicity**: Easy to create and populate tables
//! 2. **Flexibility**: Support for various column alignments and widths
//! 3. **Readability**: Clean output that's easy to scan
//! 4. **Accessibility**: Works well with and without colors
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic::output::table::{Table, Alignment};
//!
//! let mut table = Table::new();
//! table.set_header(vec!["Stack", "State", "Changes"]);
//! table.add_row(vec!["main", "ABC123...", "42"]);
//! table.add_row(vec!["feature", "DEF456...", "7"]);
//! println!("{}", table);
//! ```
//!
//! # Output Example
//!
//! ```text
//! Stack     State       Changes
//! ─────     ─────       ───────
//! main      ABC123...   42
//! feature   DEF456...   7
//! ```

use std::fmt;

// Alignment

/// Column alignment options for table cells.
///
/// Determines how content is positioned within a column when the column
/// is wider than the content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Alignment {
    /// Align content to the left (default).
    ///
    /// Extra space is added to the right of the content.
    #[default]
    Left,

    /// Align content to the right.
    ///
    /// Extra space is added to the left of the content.
    #[allow(dead_code)] // used in apply() match arm
    Right,

    /// Center content.
    ///
    /// Extra space is distributed evenly on both sides.
    Center,
}

impl Alignment {
    /// Apply this alignment to a string within a given width.
    ///
    /// # Arguments
    ///
    /// * `text` - The text to align
    /// * `width` - The total width to fill
    ///
    /// # Returns
    ///
    /// A string padded to the specified width according to this alignment.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let aligned = Alignment::Right.apply("42", 10);
    /// assert_eq!(aligned, "        42");
    /// ```
    pub fn apply(&self, text: &str, width: usize) -> String {
        let text_width = console::measure_text_width(text);
        if text_width >= width {
            return text.to_string();
        }

        let padding = width - text_width;
        match self {
            Alignment::Left => format!("{}{}", text, " ".repeat(padding)),
            Alignment::Right => format!("{}{}", " ".repeat(padding), text),
            Alignment::Center => {
                let left_pad = padding / 2;
                let right_pad = padding - left_pad;
                format!("{}{}{}", " ".repeat(left_pad), text, " ".repeat(right_pad))
            }
        }
    }
}

// Column Configuration

/// Configuration for a table column.
///
/// Each column can have its own alignment and optional minimum/maximum width.
#[derive(Debug, Clone)]
pub struct Column {
    /// Column header text
    pub header: String,

    /// Column alignment
    pub alignment: Alignment,

    /// Minimum column width (0 means no minimum)
    pub min_width: usize,

    /// Maximum column width (0 means no maximum)
    pub max_width: usize,
}

impl Column {
    /// Create a new column with the given header.
    ///
    /// # Arguments
    ///
    /// * `header` - The column header text
    ///
    /// # Returns
    ///
    /// A new column with default alignment (left) and no width constraints.
    pub fn new<S: Into<String>>(header: S) -> Self {
        Self {
            header: header.into(),
            alignment: Alignment::Left,
            min_width: 0,
            max_width: 0,
        }
    }

    /// Set the alignment for this column.
    ///
    /// # Arguments
    ///
    /// * `alignment` - The alignment to use
    ///
    /// # Returns
    ///
    /// Self for method chaining.
    pub fn align(mut self, alignment: Alignment) -> Self {
        self.alignment = alignment;
        self
    }

    /// Set the minimum width for this column.
    ///
    /// # Arguments
    ///
    /// * `width` - The minimum width in characters
    ///
    /// # Returns
    ///
    /// Self for method chaining.
    pub fn min_width(mut self, width: usize) -> Self {
        self.min_width = width;
        self
    }

    /// Set the maximum width for this column.
    ///
    /// # Arguments
    ///
    /// * `width` - The maximum width in characters (0 means no maximum)
    ///
    /// # Returns
    ///
    /// Self for method chaining.
    pub fn max_width(mut self, width: usize) -> Self {
        self.max_width = width;
        self
    }
}

// Row

/// A single row in the table.
///
/// Each row contains a vector of cell values as strings.
#[derive(Debug, Clone, Default)]
pub struct Row {
    /// The cell values for this row
    pub cells: Vec<String>,
}

impl Row {
    /// Create a new empty row.
    pub fn new() -> Self {
        Self { cells: Vec::new() }
    }

    /// Get the number of cells in this row.
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    /// Check if this row has no cells.
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// Add a cell to this row (builder style).
    pub fn add_cell<S: Into<String>>(mut self, cell: S) -> Self {
        self.cells.push(cell.into());
        self
    }

    /// Create a row from a vector of values.
    ///
    /// # Arguments
    ///
    /// * `cells` - The cell values
    ///
    /// # Returns
    ///
    /// A new row containing the given values.
    pub fn from_vec<I, S>(cells: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            cells: cells.into_iter().map(|s| s.into()).collect(),
        }
    }
}

impl<I, S> From<I> for Row
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    fn from(cells: I) -> Self {
        Row::from_vec(cells)
    }
}

// Table

/// A formatted table for CLI output.
///
/// Tables consist of an optional header row, column configurations,
/// and data rows. The table automatically calculates column widths
/// based on content.
///
/// # Example
///
/// ```rust,ignore
/// let mut table = Table::new();
/// table.set_header(vec!["Name", "Value"]);
/// table.add_row(vec!["foo", "42"]);
/// table.add_row(vec!["bar", "100"]);
/// println!("{}", table);
/// ```
#[derive(Debug, Clone, Default)]
pub struct Table {
    /// Column configurations
    columns: Vec<Column>,

    /// Data rows (excluding header)
    rows: Vec<Row>,

    /// Whether to show the header separator line
    show_header_separator: bool,

    /// Column separator string
    column_separator: String,

    /// Whether to use colors in output
    #[allow(dead_code)] // set in constructor
    use_colors: bool,
}

impl Table {
    /// Create a new empty table.
    ///
    /// # Returns
    ///
    /// A new table with default settings.
    pub fn new() -> Self {
        Self {
            columns: Vec::new(),
            rows: Vec::new(),
            show_header_separator: true,
            column_separator: "  ".to_string(),
            use_colors: true,
        }
    }

    /// Check if the table has no rows.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Get the number of data rows (excluding the header).
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// Get the number of columns.
    pub fn column_count(&self) -> usize {
        self.columns.len()
    }

    /// Set the header row from simple string values.
    ///
    /// This creates columns with default settings from header strings.
    pub fn set_header<I, S>(&mut self, headers: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.columns = headers.into_iter().map(|h| Column::new(h)).collect();
        self
    }

    /// Set whether to show the header separator line.
    pub fn show_header_separator(&mut self, show: bool) -> &mut Self {
        self.show_header_separator = show;
        self
    }

    /// Set the column separator string.
    pub fn column_separator(&mut self, sep: impl Into<String>) -> &mut Self {
        self.column_separator = sep.into();
        self
    }

    /// Set the columns with full configuration.
    ///
    /// Use this when you need to customize alignment or width constraints.
    ///
    /// # Arguments
    ///
    /// * `columns` - The column configurations
    ///
    /// # Returns
    ///
    /// Self for method chaining.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let mut table = Table::new();
    /// table.set_columns(vec![
    ///     Column::new("Name").align(Alignment::Left),
    ///     Column::new("Count").align(Alignment::Right),
    /// ]);
    /// ```
    pub fn set_columns(&mut self, columns: Vec<Column>) -> &mut Self {
        self.columns = columns;
        self
    }

    /// Add a row to the table.
    ///
    /// # Arguments
    ///
    /// * `row` - The row data
    ///
    /// # Returns
    ///
    /// Self for method chaining.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// table.add_row(vec!["main", "ABC123", "42"]);
    /// ```
    pub fn add_row<I, S>(&mut self, row: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.rows.push(Row::from_vec(row));
        self
    }

    /// Calculate the width for each column.
    ///
    /// Width is determined by the maximum width of all cells in the column,
    /// including the header, constrained by min/max width settings.
    fn calculate_widths(&self) -> Vec<usize> {
        let mut widths: Vec<usize> = self
            .columns
            .iter()
            .map(|c| {
                let header_width = console::measure_text_width(&c.header);
                header_width.max(c.min_width)
            })
            .collect();

        // Consider row content
        for row in &self.rows {
            for (i, cell) in row.cells.iter().enumerate() {
                if i < widths.len() {
                    let cell_width = console::measure_text_width(cell);
                    widths[i] = widths[i].max(cell_width);
                }
            }
        }

        // Apply max width constraints
        for (i, col) in self.columns.iter().enumerate() {
            if col.max_width > 0 && i < widths.len() {
                widths[i] = widths[i].min(col.max_width);
            }
        }

        widths
    }

    /// Truncate text to fit within a maximum width.
    fn truncate(text: &str, max_width: usize) -> String {
        if max_width == 0 {
            return text.to_string();
        }

        let text_width = console::measure_text_width(text);
        if text_width <= max_width {
            return text.to_string();
        }

        if max_width <= 3 {
            return "...".chars().take(max_width).collect();
        }

        // Find a safe truncation point
        let mut result = String::new();
        let mut current_width = 0;
        let target_width = max_width - 3; // Leave room for "..."

        for ch in text.chars() {
            let char_width = console::measure_text_width(&ch.to_string());
            if current_width + char_width > target_width {
                break;
            }
            result.push(ch);
            current_width += char_width;
        }

        result.push_str("...");
        result
    }

    /// Render the table to a string.
    fn render(&self) -> String {
        if self.columns.is_empty() {
            return String::new();
        }

        let widths = self.calculate_widths();
        let mut output = String::new();

        // Render header
        let header_cells: Vec<String> = self
            .columns
            .iter()
            .enumerate()
            .map(|(i, col)| {
                let text = Self::truncate(&col.header, col.max_width);
                col.alignment.apply(&text, widths[i])
            })
            .collect();
        output.push_str(&header_cells.join(&self.column_separator));
        output.push('\n');

        // Render header separator
        if self.show_header_separator {
            let separator_cells: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
            output.push_str(&separator_cells.join(&self.column_separator));
            output.push('\n');
        }

        // Render rows
        for row in &self.rows {
            let row_cells: Vec<String> = self
                .columns
                .iter()
                .enumerate()
                .map(|(i, col)| {
                    let cell_text = row.cells.get(i).map(|s| s.as_str()).unwrap_or("");
                    let text = Self::truncate(cell_text, col.max_width);
                    col.alignment.apply(&text, widths[i])
                })
                .collect();
            output.push_str(&row_cells.join(&self.column_separator));
            output.push('\n');
        }

        // Remove trailing newline
        if output.ends_with('\n') {
            output.pop();
        }

        output
    }
}

impl fmt::Display for Table {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.render())
    }
}

// Key-Value Table

/// A simple two-column key-value table.
///
/// Useful for displaying metadata, configuration, or summary information.
///
/// # Example
///
/// ```rust,ignore
/// let table = KeyValueTable::new()
///     .add("Name", "Alice")
///     .add("Age", "30");
/// println!("{}", table);
/// ```
#[derive(Debug, Clone, Default)]
pub struct KeyValueTable {
    /// Key-value pairs
    entries: Vec<(String, String)>,
    /// Separator between key and value
    separator: String,
    /// Whether to align values (pad keys to same width)
    align_values: bool,
}

impl KeyValueTable {
    /// Create a new empty key-value table.
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            separator: ": ".to_string(),
            align_values: true,
        }
    }

    /// Add a key-value pair.
    pub fn add<K: Into<String>, V: Into<String>>(mut self, key: K, value: V) -> Self {
        self.entries.push((key.into(), value.into()));
        self
    }

    /// Set the separator between keys and values.
    pub fn separator<S: Into<String>>(mut self, sep: S) -> Self {
        self.separator = sep.into();
        self
    }

    /// Set whether to align values by padding keys.
    pub fn align_values(mut self, align: bool) -> Self {
        self.align_values = align;
        self
    }

    /// Get the number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if the table is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Render the table to a string.
    fn render(&self) -> String {
        if self.entries.is_empty() {
            return String::new();
        }

        let max_key_width = if self.align_values {
            self.entries.iter().map(|(k, _)| k.len()).max().unwrap_or(0)
        } else {
            0
        };

        let mut output = String::new();
        for (key, value) in &self.entries {
            if self.align_values {
                output.push_str(&format!(
                    "{:width$}{}{}\n",
                    key,
                    self.separator,
                    value,
                    width = max_key_width
                ));
            } else {
                output.push_str(&format!("{}{}{}\n", key, self.separator, value));
            }
        }

        // Remove trailing newline
        if output.ends_with('\n') {
            output.pop();
        }

        output
    }
}

impl fmt::Display for KeyValueTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.render())
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------------
    // Alignment Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_alignment_left() {
        let aligned = Alignment::Left.apply("test", 10);
        assert_eq!(aligned, "test      ");
    }

    #[test]
    fn test_alignment_right() {
        let aligned = Alignment::Right.apply("test", 10);
        assert_eq!(aligned, "      test");
    }

    #[test]
    fn test_alignment_center() {
        let aligned = Alignment::Center.apply("test", 10);
        assert_eq!(aligned, "   test   ");
    }

    #[test]
    fn test_alignment_center_odd() {
        let aligned = Alignment::Center.apply("test", 11);
        assert_eq!(aligned, "   test    ");
    }

    #[test]
    fn test_alignment_no_padding_needed() {
        let aligned = Alignment::Left.apply("test", 4);
        assert_eq!(aligned, "test");
    }

    #[test]
    fn test_alignment_text_too_wide() {
        let aligned = Alignment::Left.apply("testing", 4);
        assert_eq!(aligned, "testing");
    }

    // -------------------------------------------------------------------------
    // Column Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_column_new() {
        let col = Column::new("Name");
        assert_eq!(col.header, "Name");
        assert_eq!(col.alignment, Alignment::Left);
        assert_eq!(col.min_width, 0);
        assert_eq!(col.max_width, 0);
    }

    #[test]
    fn test_column_builder() {
        let col = Column::new("Value")
            .align(Alignment::Right)
            .min_width(10)
            .max_width(50);

        assert_eq!(col.header, "Value");
        assert_eq!(col.alignment, Alignment::Right);
        assert_eq!(col.min_width, 10);
        assert_eq!(col.max_width, 50);
    }

    // -------------------------------------------------------------------------
    // Row Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_row_new() {
        let row = Row::new();
        assert!(row.is_empty());
        assert_eq!(row.len(), 0);
    }

    #[test]
    fn test_row_from_vec() {
        let row = Row::from_vec(vec!["a", "b", "c"]);
        assert_eq!(row.len(), 3);
        assert_eq!(row.cells, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_row_add_cell() {
        let row = Row::new().add_cell("a").add_cell("b");
        assert_eq!(row.len(), 2);
    }

    #[test]
    fn test_row_from_iterator() {
        let row: Row = vec!["x", "y", "z"].into();
        assert_eq!(row.len(), 3);
    }

    // -------------------------------------------------------------------------
    // Table Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_table_new() {
        let table = Table::new();
        assert!(table.is_empty());
        assert_eq!(table.row_count(), 0);
        assert_eq!(table.column_count(), 0);
    }

    #[test]
    fn test_table_set_header() {
        let mut table = Table::new();
        table.set_header(vec!["A", "B", "C"]);
        assert_eq!(table.column_count(), 3);
    }

    #[test]
    fn test_table_add_row() {
        let mut table = Table::new();
        table.set_header(vec!["Name", "Value"]);
        table.add_row(vec!["foo", "42"]);
        table.add_row(vec!["bar", "100"]);
        assert_eq!(table.row_count(), 2);
    }

    #[test]
    fn test_table_render_basic() {
        let mut table = Table::new();
        table.set_header(vec!["Name", "Value"]);
        table.add_row(vec!["foo", "42"]);

        let output = table.to_string();
        assert!(output.contains("Name"));
        assert!(output.contains("Value"));
        assert!(output.contains("foo"));
        assert!(output.contains("42"));
    }

    #[test]
    fn test_table_render_with_alignment() {
        let mut table = Table::new();
        table.set_columns(vec![
            Column::new("Name").align(Alignment::Left),
            Column::new("Count").align(Alignment::Right),
        ]);
        table.add_row(vec!["foo", "1"]);
        table.add_row(vec!["barbaz", "100"]);

        let output = table.to_string();
        let lines: Vec<&str> = output.lines().collect();
        assert!(lines.len() >= 3); // header + separator + 2 rows
    }

    #[test]
    fn test_table_no_header_separator() {
        let mut table = Table::new();
        table.set_header(vec!["A", "B"]);
        table.show_header_separator(false);
        table.add_row(vec!["1", "2"]);

        let output = table.to_string();
        assert!(!output.contains("─"));
    }

    #[test]
    fn test_table_custom_column_separator() {
        let mut table = Table::new();
        table.set_header(vec!["A", "B"]);
        table.column_separator(" | ");
        table.add_row(vec!["1", "2"]);

        let output = table.to_string();
        assert!(output.contains(" | "));
    }

    #[test]
    fn test_table_empty() {
        let table = Table::new();
        let output = table.to_string();
        assert!(output.is_empty());
    }

    #[test]
    fn test_table_truncation() {
        let truncated = Table::truncate("Hello, World!", 8);
        assert_eq!(truncated, "Hello...");
    }

    #[test]
    fn test_table_truncation_no_need() {
        let truncated = Table::truncate("Hi", 10);
        assert_eq!(truncated, "Hi");
    }

    #[test]
    fn test_table_truncation_very_short() {
        let truncated = Table::truncate("Hello", 2);
        assert_eq!(truncated, "..");
    }

    // -------------------------------------------------------------------------
    // KeyValueTable Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_kv_table_new() {
        let table = KeyValueTable::new();
        assert!(table.is_empty());
        assert_eq!(table.len(), 0);
    }

    #[test]
    fn test_kv_table_add() {
        let table = KeyValueTable::new().add("Name", "Alice").add("Age", "30");

        assert_eq!(table.len(), 2);
    }

    #[test]
    fn test_kv_table_render() {
        let table = KeyValueTable::new().add("Name", "Alice").add("Age", "30");

        let output = table.to_string();
        assert!(output.contains("Name"));
        assert!(output.contains("Alice"));
        assert!(output.contains("Age"));
        assert!(output.contains("30"));
    }

    #[test]
    fn test_kv_table_custom_separator() {
        let table = KeyValueTable::new().separator(" = ").add("x", "1");

        let output = table.to_string();
        assert!(output.contains("x = 1"));
    }

    #[test]
    fn test_kv_table_no_alignment() {
        let table = KeyValueTable::new()
            .align_values(false)
            .add("Short", "1")
            .add("VeryLongKey", "2");

        let output = table.to_string();
        let lines: Vec<&str> = output.lines().collect();
        // Without alignment, each line should start immediately after the key
        assert!(lines[0].starts_with("Short:"));
        assert!(lines[1].starts_with("VeryLongKey:"));
    }

    #[test]
    fn test_kv_table_empty() {
        let table = KeyValueTable::new();
        let output = table.to_string();
        assert!(output.is_empty());
    }

    // -------------------------------------------------------------------------
    // Edge Case Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_table_missing_cells() {
        let mut table = Table::new();
        table.set_header(vec!["A", "B", "C"]);
        table.add_row(vec!["1"]); // Only one cell for three columns

        // Should not panic
        let _ = table.to_string();
    }

    #[test]
    fn test_table_unicode() {
        let mut table = Table::new();
        table.set_header(vec!["名前", "値"]);
        table.add_row(vec!["テスト", "42"]);

        let _ = table.to_string();
    }

    #[test]
    fn test_table_emoji() {
        let mut table = Table::new();
        table.set_header(vec!["Status", "Name"]);
        table.add_row(vec!["✓", "Complete"]);
        table.add_row(vec!["✗", "Failed"]);

        let _ = table.to_string();
    }

    #[test]
    fn test_alignment_default() {
        assert_eq!(Alignment::default(), Alignment::Left);
    }

    #[test]
    fn test_row_default() {
        let row = Row::default();
        assert!(row.is_empty());
    }

    #[test]
    fn test_table_default() {
        let table = Table::default();
        assert!(table.is_empty());
    }

    #[test]
    fn test_kv_table_default() {
        let table = KeyValueTable::default();
        assert!(table.is_empty());
    }
}
