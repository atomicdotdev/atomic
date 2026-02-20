use super::*;

// Helper Functions

/// Build hunks from a diff result with context lines.
///
/// This function groups diff operations into hunks with the specified
/// number of context lines around changes.
///
/// # Arguments
///
/// * `diff_result` - The raw diff result
/// * `old_lines` - Lines from the old content
/// * `new_lines` - Lines from the new content
/// * `context` - Number of context lines to include
///
/// # Returns
///
/// A vector of `DiffHunk`s representing the changes with context.
/// Format a stat graph bar for a file's changes.
///
/// Creates a visual bar like `+++---` showing the ratio of insertions
/// to deletions, scaled to fit within `max_width` characters.
///
/// # Arguments
///
/// * `insertions` - Number of lines added
/// * `deletions` - Number of lines deleted
/// * `max_width` - Maximum width of the graph bar
///
/// # Returns
///
/// A string containing `+` and `-` characters representing the change ratio.
pub(crate) fn format_stat_graph(insertions: usize, deletions: usize, max_width: usize) -> String {
    let total = insertions + deletions;
    if total == 0 {
        return String::new();
    }

    let width = total.min(max_width);
    let ins_width = if total <= max_width {
        insertions
    } else {
        (insertions as f64 / total as f64 * max_width as f64).round() as usize
    };
    let del_width = width.saturating_sub(ins_width);

    let mut result = String::with_capacity(width);
    for _ in 0..ins_width {
        result.push('+');
    }
    for _ in 0..del_width {
        result.push('-');
    }
    result
}

pub(crate) fn build_hunks_from_diff(
    diff_result: &DiffResult,
    old_lines: &[&[u8]],
    new_lines: &[&[u8]],
    context: usize,
) -> Vec<DiffHunk> {
    let mut hunks = Vec::new();

    // Simple implementation: create one graph_op for all changes
    // A more sophisticated implementation would group changes by proximity

    let mut current_hunk: Option<DiffHunk> = None;
    let mut old_line = 1;
    let mut new_line = 1;

    for op in diff_result.iter() {
        match op {
            DiffOp::Equal { len, .. } => {
                if let Some(ref mut graph_op) = current_hunk {
                    // Add context lines to current graph_op (up to context limit)
                    let context_count = cmp::min(*len, context);
                    for i in 0..context_count {
                        let content = if new_line - 1 + i < new_lines.len() {
                            String::from_utf8_lossy(new_lines[new_line - 1 + i]).into_owned()
                        } else {
                            String::new()
                        };
                        graph_op.add_line(HunkLine::context(content, old_line + i, new_line + i));
                    }

                    // If we've shown enough context and there's more equal content,
                    // close this graph_op
                    if *len > context * 2 {
                        graph_op.old_count = graph_op
                            .lines
                            .iter()
                            .filter(|l| l.old_line_num.is_some())
                            .count();
                        graph_op.new_count = graph_op
                            .lines
                            .iter()
                            .filter(|l| l.new_line_num.is_some())
                            .count();
                        hunks.push(current_hunk.take().unwrap());
                    }
                }
                old_line += len;
                new_line += len;
            }
            DiffOp::Insert { len, .. } => {
                // Start a new graph_op if we don't have one
                if current_hunk.is_none() {
                    let old_start = old_line.saturating_sub(context).max(1);
                    let new_start = new_line.saturating_sub(context).max(1);
                    current_hunk = Some(DiffHunk::new(old_start, 0, new_start, 0));

                    // Add leading context
                    let context_start = new_line.saturating_sub(context);
                    for i in context_start..new_line {
                        if i > 0 && i <= new_lines.len() {
                            let content = String::from_utf8_lossy(new_lines[i - 1]).into_owned();
                            let old_i = old_line.saturating_sub(new_line - i);
                            current_hunk
                                .as_mut()
                                .unwrap()
                                .add_line(HunkLine::context(content, old_i, i));
                        }
                    }
                }

                // Add inserted lines
                for i in 0..*len {
                    let content = if new_line - 1 + i < new_lines.len() {
                        String::from_utf8_lossy(new_lines[new_line - 1 + i]).into_owned()
                    } else {
                        String::new()
                    };
                    current_hunk
                        .as_mut()
                        .unwrap()
                        .add_line(HunkLine::added(content, new_line + i));
                }
                new_line += len;
            }
            DiffOp::Delete { len, .. } => {
                // Start a new graph_op if we don't have one
                if current_hunk.is_none() {
                    let old_start = old_line.saturating_sub(context).max(1);
                    let new_start = new_line.saturating_sub(context).max(1);
                    current_hunk = Some(DiffHunk::new(old_start, 0, new_start, 0));
                }

                // Add deleted lines
                for i in 0..*len {
                    let content = if old_line - 1 + i < old_lines.len() {
                        String::from_utf8_lossy(old_lines[old_line - 1 + i]).into_owned()
                    } else {
                        String::new()
                    };
                    current_hunk
                        .as_mut()
                        .unwrap()
                        .add_line(HunkLine::removed(content, old_line + i));
                }
                old_line += len;
            }
            DiffOp::Replace {
                old_len, new_len, ..
            } => {
                // Start a new graph_op if we don't have one
                if current_hunk.is_none() {
                    let old_start = old_line.saturating_sub(context).max(1);
                    let new_start = new_line.saturating_sub(context).max(1);
                    current_hunk = Some(DiffHunk::new(old_start, 0, new_start, 0));
                }

                // Interleave deleted and added lines for better word-level diff pairing.
                // This makes it easier to show word-level changes when a line is modified.
                let max_len = (*old_len).max(*new_len);
                for i in 0..max_len {
                    // Add deleted line if available
                    if i < *old_len {
                        let content = if old_line - 1 + i < old_lines.len() {
                            String::from_utf8_lossy(old_lines[old_line - 1 + i]).into_owned()
                        } else {
                            String::new()
                        };
                        current_hunk
                            .as_mut()
                            .unwrap()
                            .add_line(HunkLine::removed(content, old_line + i));
                    }

                    // Add inserted line if available
                    if i < *new_len {
                        let content = if new_line - 1 + i < new_lines.len() {
                            String::from_utf8_lossy(new_lines[new_line - 1 + i]).into_owned()
                        } else {
                            String::new()
                        };
                        current_hunk
                            .as_mut()
                            .unwrap()
                            .add_line(HunkLine::added(content, new_line + i));
                    }
                }

                old_line += old_len;
                new_line += new_len;
            }
        }
    }

    // Finalize any remaining graph_op
    if let Some(mut graph_op) = current_hunk {
        graph_op.old_count = graph_op
            .lines
            .iter()
            .filter(|l| l.old_line_num.is_some() && !matches!(l.status, LineStatus::Added))
            .count();
        graph_op.new_count = graph_op
            .lines
            .iter()
            .filter(|l| l.new_line_num.is_some() && !matches!(l.status, LineStatus::Removed))
            .count();
        hunks.push(graph_op);
    }

    hunks
}

/// Format a diff stat line graph.
///
/// Creates the +/- visual representation for stat output.
///
/// # Arguments
///
/// * `insertions` - Number of insertions
/// * `deletions` - Number of deletions
/// * `max_width` - Maximum width for the graph
///
/// # Returns
///
/// A string containing + and - characters.
/// Print a line with word-level diff highlighting.
///
/// Uses ANSI escape codes to highlight changed tokens:
/// - Deletions: bright red text on light red background
/// - Insertions: bright green text on light green background
pub(super) fn print_word_diff_line(
    content: &[u8],
    hunks: &[atomic_core::diff::ChangeHunk],
    is_deletion: bool,
) {
    for hunk in hunks {
        if hunk.end > content.len() {
            continue;
        }
        let text = String::from_utf8_lossy(&content[hunk.start..hunk.end]);

        match hunk.kind {
            HunkKind::Deleted | HunkKind::Modified if is_deletion => {
                // Bright red text with underline for deletions
                print!("\x1b[91;1;4m{}\x1b[0m", text);
            }
            HunkKind::Inserted | HunkKind::Modified if !is_deletion => {
                // Bright green text with underline for insertions
                print!("\x1b[92;1;4m{}\x1b[0m", text);
            }
            _ => {
                // Normal text (unchanged parts)
                if is_deletion {
                    print!("\x1b[31m{}\x1b[0m", text); // Dim red for context
                } else {
                    print!("\x1b[32m{}\x1b[0m", text); // Dim green for context
                }
            }
        }
    }
}

/// Print a line with semantic token-level diff highlighting.
///
/// Uses the semantic diff engine for precise token-level highlighting.
/// This produces better results than the inline diff for code, as it
/// understands token boundaries (identifiers, operators, strings, etc.)
///
/// # Visual Pattern
///
/// ```text
/// - const result = calculateSum(a, b);        <- light red background
/// + const result = calculateSum(a, b, c);     <- light green background
///                                   ^^^^      <- dark green: ", c" added
/// ```
pub(super) fn print_semantic_word_diff_line(token_changes: &[TokenChange<'_>], is_deletion: bool) {
    for tc in token_changes {
        match tc {
            TokenChange::Unchanged { token, .. } => {
                // Unchanged tokens - dim color for context
                let text = token.as_str();
                if is_deletion {
                    print!("\x1b[31m{}\x1b[0m", text); // Dim red for deletion context
                } else {
                    print!("\x1b[32m{}\x1b[0m", text); // Dim green for insertion context
                }
            }
            TokenChange::Deleted { token, .. } if is_deletion => {
                // Deleted token - bright red with underline
                let text = token.as_str();
                print!("\x1b[91;1;4m{}\x1b[0m", text);
            }
            TokenChange::Inserted { token, .. } if !is_deletion => {
                // Inserted token - bright green with underline
                let text = token.as_str();
                print!("\x1b[92;1;4m{}\x1b[0m", text);
            }
            TokenChange::Replaced {
                old_token,
                new_token,
                ..
            } => {
                if is_deletion {
                    // Show old token in bright red with underline
                    let text = old_token.as_str();
                    print!("\x1b[91;1;4m{}\x1b[0m", text);
                } else {
                    // Show new token in bright green with underline
                    let text = new_token.as_str();
                    print!("\x1b[92;1;4m{}\x1b[0m", text);
                }
            }
            // Skip tokens that don't apply to this line type
            TokenChange::Deleted { .. } | TokenChange::Inserted { .. } => {}
        }
    }
}
