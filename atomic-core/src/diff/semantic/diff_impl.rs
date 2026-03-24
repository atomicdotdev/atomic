use super::*;

// Core diffing functions

/// Compute a semantic diff between two byte slices.
///
/// This is the main entry point for semantic diffing. It:
/// 1. Splits content into lines
/// 2. Computes line-level diff
/// 3. For modified lines, computes token-level diff
///
/// # Arguments
///
/// * `old` - The original content
/// * `new` - The modified content
///
/// # Returns
///
/// A [`SemanticDiff`] with line and token level changes.
///
/// # Example
///
/// ```rust
/// use atomic_core::diff::semantic::semantic_diff;
///
/// let old = b"let x = 1;\n";
/// let new = b"let x = 42;\n";
///
/// let diff = semantic_diff(old, new);
/// assert!(diff.has_changes());
/// ```
pub fn semantic_diff<'a>(old: &'a [u8], new: &'a [u8]) -> SemanticDiff<'a> {
    semantic_diff_with_config(old, new, &SemanticDiffConfig::default())
}

/// Compute a semantic diff with custom configuration.
///
/// # Arguments
///
/// * `old` - The original content
/// * `new` - The modified content
/// * `config` - Configuration for the diff operation
///
/// # Returns
///
/// A [`SemanticDiff`] with line and token level changes.
pub fn semantic_diff_with_config<'a>(
    old: &'a [u8],
    new: &'a [u8],
    config: &SemanticDiffConfig,
) -> SemanticDiff<'a> {
    // Parse into semantic lines
    let old_lines = SemanticLine::from_bytes(old);
    let new_lines = SemanticLine::from_bytes(new);

    // Compute line-level diff
    let old_raw: Vec<Line> = old_lines.iter().map(|sl| sl.line.clone()).collect();
    let new_raw: Vec<Line> = new_lines.iter().map(|sl| sl.line.clone()).collect();
    let line_diff = diff(&old_raw, &new_raw, config.algorithm);

    // Process the diff operations into semantic changes
    let mut changes = Vec::new();
    let mut stats = SemanticDiffStats::default();

    for op in line_diff.iter() {
        match op {
            DiffOp::Equal { .. } => {
                // Skip unchanged lines (unless including context)
            }

            DiffOp::Insert {
                old_pos: _,
                new_pos,
                len,
            } => {
                // Lines were added
                for i in 0..*len {
                    let line_idx = new_pos + i;
                    if line_idx < new_lines.len() {
                        let line = new_lines[line_idx].clone();

                        // Skip blank lines if configured
                        if config.ignore_blank_lines && line.is_blank() {
                            continue;
                        }

                        // All tokens are insertions
                        let tokens = create_insertion_tokens(&line);
                        stats.tokens_inserted += tokens.len();

                        changes.push(LineChange::Added {
                            line_num: line_idx + 1,
                            line,
                            tokens,
                        });
                        stats.lines_added += 1;
                    }
                }
            }

            DiffOp::Delete {
                old_pos,
                new_pos: _,
                len,
            } => {
                // Lines were deleted
                for i in 0..*len {
                    let line_idx = old_pos + i;
                    if line_idx < old_lines.len() {
                        let line = old_lines[line_idx].clone();

                        // Skip blank lines if configured
                        if config.ignore_blank_lines && line.is_blank() {
                            continue;
                        }

                        // All tokens are deletions
                        let tokens = create_deletion_tokens(&line);
                        stats.tokens_deleted += tokens.len();

                        changes.push(LineChange::Deleted {
                            line_num: line_idx + 1,
                            line,
                            tokens,
                        });
                        stats.lines_deleted += 1;
                    }
                }
            }

            DiffOp::Replace {
                old_pos,
                old_len,
                new_pos,
                new_len,
            } => {
                // Lines were modified - this is where token-level diff shines
                let min_len = (*old_len).min(*new_len);

                // Process paired lines (modified)
                for i in 0..min_len {
                    let old_idx = old_pos + i;
                    let new_idx = new_pos + i;

                    if old_idx < old_lines.len() && new_idx < new_lines.len() {
                        let before = old_lines[old_idx].clone();
                        let after = new_lines[new_idx].clone();

                        // Skip if both are blank and configured to ignore
                        if config.ignore_blank_lines && before.is_blank() && after.is_blank() {
                            continue;
                        }

                        // Compute token-level diff for this line pair
                        let token_changes =
                            compute_token_changes(&before, &after, &config.word_config);

                        // Update stats
                        for tc in &token_changes {
                            match tc {
                                TokenChange::Inserted { .. } => stats.tokens_inserted += 1,
                                TokenChange::Deleted { .. } => stats.tokens_deleted += 1,
                                TokenChange::Replaced { .. } => stats.tokens_replaced += 1,
                                TokenChange::Unchanged { .. } => {}
                            }
                        }

                        changes.push(LineChange::Modified {
                            old_line_num: old_idx + 1,
                            new_line_num: new_idx + 1,
                            before,
                            after,
                            token_changes,
                        });
                        stats.lines_modified += 1;
                    }
                }

                // Process extra deleted lines (old_len > new_len)
                for i in min_len..*old_len {
                    let line_idx = old_pos + i;
                    if line_idx < old_lines.len() {
                        let line = old_lines[line_idx].clone();

                        if config.ignore_blank_lines && line.is_blank() {
                            continue;
                        }

                        let tokens = create_deletion_tokens(&line);
                        stats.tokens_deleted += tokens.len();

                        changes.push(LineChange::Deleted {
                            line_num: line_idx + 1,
                            line,
                            tokens,
                        });
                        stats.lines_deleted += 1;
                    }
                }

                // Process extra inserted lines (new_len > old_len)
                for i in min_len..*new_len {
                    let line_idx = new_pos + i;
                    if line_idx < new_lines.len() {
                        let line = new_lines[line_idx].clone();

                        if config.ignore_blank_lines && line.is_blank() {
                            continue;
                        }

                        let tokens = create_insertion_tokens(&line);
                        stats.tokens_inserted += tokens.len();

                        changes.push(LineChange::Added {
                            line_num: line_idx + 1,
                            line,
                            tokens,
                        });
                        stats.lines_added += 1;
                    }
                }
            }
        }
    }

    SemanticDiff {
        changes,
        stats,
        old_lines,
        new_lines,
    }
}

// Helper functions for token change creation

/// Create token changes for a line that was entirely added.
pub(super) fn create_insertion_tokens<'a>(line: &SemanticLine<'a>) -> Vec<TokenChange<'a>> {
    let mut offset = 0;
    line.tokens()
        .iter()
        .map(|token| {
            let start = offset;
            let end = start + token.content().len();
            offset = end;
            TokenChange::Inserted {
                token: token.clone(),
                new_range: start..end,
            }
        })
        .collect()
}

/// Create token changes for a line that was entirely deleted.
pub(super) fn create_deletion_tokens<'a>(line: &SemanticLine<'a>) -> Vec<TokenChange<'a>> {
    let mut offset = 0;
    line.tokens()
        .iter()
        .map(|token| {
            let start = offset;
            let end = start + token.content().len();
            offset = end;
            TokenChange::Deleted {
                token: token.clone(),
                old_range: start..end,
            }
        })
        .collect()
}

/// Compute token-level changes between two lines.
///
/// This is the core of the token-level diff - it compares the tokens
/// of two lines and produces a sequence of token changes.
pub(super) fn compute_token_changes<'a>(
    before: &SemanticLine<'a>,
    after: &SemanticLine<'a>,
    config: &WordDiffConfig,
) -> Vec<TokenChange<'a>> {
    // Use word diff to find token-level changes
    let word_result = word_diff_with_config(
        before.content_without_newline(),
        after.content_without_newline(),
        config,
    );

    convert_word_diff_to_token_changes(&word_result, before, after)
}

/// Convert word diff operations to token changes.
pub(super) fn convert_word_diff_to_token_changes<'a>(
    word_result: &WordDiffResult<'a>,
    _before: &SemanticLine<'a>,
    _after: &SemanticLine<'a>,
) -> Vec<TokenChange<'a>> {
    let mut changes = Vec::new();

    let old_tokens = word_result.old_tokens();
    let new_tokens = word_result.new_tokens();

    for op in word_result.ops() {
        match op {
            WordDiffOp::Equal {
                old_range,
                new_range,
            } => {
                // Tokens that are unchanged
                for (old_idx, new_idx) in old_range.clone().zip(new_range.clone()) {
                    if old_idx < old_tokens.len() && new_idx < new_tokens.len() {
                        let token = old_tokens[old_idx].clone();
                        let old_byte_range = token_byte_range(&old_tokens, old_idx);
                        let new_byte_range = token_byte_range(&new_tokens, new_idx);

                        changes.push(TokenChange::Unchanged {
                            token,
                            old_range: old_byte_range,
                            new_range: new_byte_range,
                        });
                    }
                }
            }

            WordDiffOp::Insert {
                old_pos: _,
                new_range,
            } => {
                // Tokens that were inserted
                for new_idx in new_range.clone() {
                    if new_idx < new_tokens.len() {
                        let token = new_tokens[new_idx].clone();
                        let new_byte_range = token_byte_range(&new_tokens, new_idx);

                        changes.push(TokenChange::Inserted {
                            token,
                            new_range: new_byte_range,
                        });
                    }
                }
            }

            WordDiffOp::Delete {
                old_range,
                new_pos: _,
            } => {
                // Tokens that were deleted
                for old_idx in old_range.clone() {
                    if old_idx < old_tokens.len() {
                        let token = old_tokens[old_idx].clone();
                        let old_byte_range = token_byte_range(&old_tokens, old_idx);

                        changes.push(TokenChange::Deleted {
                            token,
                            old_range: old_byte_range,
                        });
                    }
                }
            }

            WordDiffOp::Replace {
                old_range,
                new_range,
            } => {
                // Tokens that were replaced
                // If the counts match, pair them up as replacements
                // Otherwise, treat as deletes followed by inserts
                let old_count = old_range.len();
                let new_count = new_range.len();

                if old_count == new_count {
                    // One-to-one replacement
                    for (old_idx, new_idx) in old_range.clone().zip(new_range.clone()) {
                        if old_idx < old_tokens.len() && new_idx < new_tokens.len() {
                            let old_token = old_tokens[old_idx].clone();
                            let new_token = new_tokens[new_idx].clone();
                            let old_byte_range = token_byte_range(&old_tokens, old_idx);
                            let new_byte_range = token_byte_range(&new_tokens, new_idx);

                            changes.push(TokenChange::Replaced {
                                old_token,
                                new_token,
                                old_range: old_byte_range,
                                new_range: new_byte_range,
                            });
                        }
                    }
                } else {
                    // Different counts - emit deletes then inserts
                    for old_idx in old_range.clone() {
                        if old_idx < old_tokens.len() {
                            let token = old_tokens[old_idx].clone();
                            let old_byte_range = token_byte_range(&old_tokens, old_idx);

                            changes.push(TokenChange::Deleted {
                                token,
                                old_range: old_byte_range,
                            });
                        }
                    }

                    for new_idx in new_range.clone() {
                        if new_idx < new_tokens.len() {
                            let token = new_tokens[new_idx].clone();
                            let new_byte_range = token_byte_range(&new_tokens, new_idx);

                            changes.push(TokenChange::Inserted {
                                token,
                                new_range: new_byte_range,
                            });
                        }
                    }
                }
            }
        }
    }

    changes
}

/// Calculate the byte range for a token at a given index.
pub(super) fn token_byte_range(tokens: &[Token<'_>], index: usize) -> Range<usize> {
    let mut offset = 0;
    for (i, token) in tokens.iter().enumerate() {
        let len = token.content().len();
        if i == index {
            return offset..(offset + len);
        }
        offset += len;
    }
    // Fallback (shouldn't happen with valid indices)
    offset..offset
}
