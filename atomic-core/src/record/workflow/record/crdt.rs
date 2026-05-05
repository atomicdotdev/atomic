//! CRDT operation builders for the recording pipeline.
//!
//! These functions generate CRDT operations (Trunk → Branch → Leaf) alongside
//! the traditional graph hunks during recording. This enables token-level diff,
//! conflict-free merging, and accurate blame attribution.

use crate::change::{Encoding, FileOps};
use crate::crdt::{BranchId, BranchOp, LeafId, LeafOp, TrunkId};
use crate::diff::Algorithm;
use crate::types::NodeId;

use crate::record::workflow::compare::compare_content;
use crate::record::workflow::crdt::{
    ContentTokenizer, CrdtBuildStats, CrdtChangeBuilder, FileOps as BuilderFileOps,
    LineOps as BuilderLineOps,
};

/// Build CRDT operations for a newly added file.
///
/// This tokenizes the content into lines and tokens, creating the full
/// Trunk → Branch → Leaf hierarchy for conflict-free merging.
pub(super) fn build_crdt_ops_for_added_file(
    path: &str,
    content: &[u8],
    encoding: Encoding,
) -> (FileOps, CrdtBuildStats) {
    // Use placeholder change ID - will be resolved during globalization
    let placeholder_change_id = NodeId::new(0);
    let mut builder = CrdtChangeBuilder::new(placeholder_change_id);

    // Add the file with content - this tokenizes into lines and tokens
    let enc = if encoding == Encoding::Binary {
        None
    } else {
        Some(encoding)
    };
    let _trunk_id = builder.add_file_with_content(path, content, enc);

    // Finish and extract the file ops
    let result = builder.finish();
    let stats = result.stats().clone();

    // Extract the FileOps for this file (should be exactly one)
    let (file_ops, _, _) = result.into_parts();
    let builder_file_op = file_ops.into_iter().next().unwrap_or_else(|| {
        BuilderFileOps::new(
            TrunkId::new(placeholder_change_id, 0),
            path.to_string(),
            None,
        )
    });

    // Convert to the canonical change::FileOps type
    (builder_file_op.into_change_ops(), stats)
}

/// Build CRDT operations for a deleted file.
///
/// Creates a TrunkOp::Delete to mark the file as deleted in the CRDT graph.
pub(super) fn build_crdt_ops_for_deleted_file(path: &str) -> (FileOps, CrdtBuildStats) {
    // Use placeholder change ID
    let placeholder_change_id = NodeId::new(0);
    let mut builder = CrdtChangeBuilder::new(placeholder_change_id);

    // Create a trunk ID for the deletion
    // Note: In a full implementation, we'd look up the existing trunk ID
    let trunk_id = TrunkId::new(placeholder_change_id, 0);
    builder.delete_file(trunk_id);

    let result = builder.finish();
    let stats = result.stats().clone();
    let (file_ops, _, _) = result.into_parts();
    let builder_file_op = file_ops
        .into_iter()
        .next()
        .unwrap_or_else(|| BuilderFileOps::delete(trunk_id, path.to_string()));

    // Convert to the canonical change::FileOps type
    (builder_file_op.into_change_ops(), stats)
}

/// Build CRDT operations for a modified file.
///
/// This performs token-level diff analysis to generate fine-grained
/// Branch and Leaf operations for conflict-free merging.
pub(super) fn build_crdt_ops_for_modified_file(
    path: &str,
    old_content: &[u8],
    new_content: &[u8],
    _encoding: Encoding,
    algorithm: Algorithm,
) -> (FileOps, CrdtBuildStats) {
    // Use placeholder change ID
    let placeholder_change_id = NodeId::new(0);

    // Create file ops container (no TrunkOp for modification - file already exists)
    let trunk_id = TrunkId::new(placeholder_change_id, 0);
    let mut file_ops = BuilderFileOps::new(trunk_id, path.to_string(), None);

    let mut stats = CrdtBuildStats::new();
    let mut next_branch_idx: u32 = 0;
    let mut next_leaf_idx: u32 = 0;

    // Helper to allocate branch IDs
    let mut alloc_branch = || {
        let id = BranchId::new(placeholder_change_id, next_branch_idx);
        next_branch_idx += 1;
        id
    };

    // Helper to allocate leaf IDs
    let mut alloc_leaf = || {
        let id = LeafId::new(placeholder_change_id, next_leaf_idx);
        next_leaf_idx += 1;
        id
    };

    // Tokenize old and new content into lines
    let old_tokenizer = ContentTokenizer::new(old_content);
    let new_tokenizer = ContentTokenizer::new(new_content);

    let old_lines: Vec<_> = old_tokenizer.lines().collect();
    let new_lines: Vec<_> = new_tokenizer.lines().collect();

    // Perform line-level diff
    let line_diff = compare_content(old_content, new_content, algorithm);

    let mut collected_line_ops: Vec<BuilderLineOps> = Vec::new();

    // Track which old lines have been processed
    let mut _old_line_idx = 0;
    let mut _new_line_idx = 0;
    let mut prev_branch: Option<BranchId> = None;

    for op in &line_diff.diff_ops {
        match op {
            crate::diff::DiffOp::Equal {
                old_pos,
                new_pos,
                len,
            } => {
                // Equal lines - no CRDT operations needed, but track position
                _old_line_idx = old_pos + len;
                _new_line_idx = new_pos + len;
                // Update prev_branch to reference the last equal line
                // (In a full implementation, we'd look up the existing branch ID)
            }
            crate::diff::DiffOp::Delete {
                old_pos,
                new_pos: _,
                len,
            } => {
                // Deleted lines - create BranchOp::Delete for each with original content
                for i in 0..*len {
                    let line_idx = old_pos + i;
                    let branch_id = alloc_branch();

                    // Capture the original line content for diff display
                    let content = if line_idx < old_lines.len() {
                        let line = &old_lines[line_idx];
                        let mut leaf_ops = Vec::new();
                        for token in line.tokens() {
                            leaf_ops.push(LeafOp::Insert {
                                after: None,
                                kind: token.kind(),
                                content: token.content().to_vec(),
                            });
                        }
                        leaf_ops
                    } else {
                        Vec::new()
                    };

                    let line_op =
                        BuilderLineOps::delete(branch_id, content).with_old_line_num(line_idx + 1);
                    collected_line_ops.push(line_op);
                    stats.lines_deleted += 1;
                }
                _old_line_idx = old_pos + len;
            }
            crate::diff::DiffOp::Insert {
                old_pos: _,
                new_pos,
                len,
            } => {
                // Inserted lines - create BranchOp::Insert with token-level LeafOps
                for i in 0..*len {
                    let line_idx = new_pos + i;
                    if line_idx < new_lines.len() {
                        let line = &new_lines[line_idx];
                        let branch_id = alloc_branch();

                        // Generate LeafOps for tokens in this line
                        let mut leaf_ops = Vec::new();
                        let mut prev_leaf: Option<LeafId> = None;

                        for token in line.tokens() {
                            let leaf_id = alloc_leaf();
                            leaf_ops.push(LeafOp::Insert {
                                after: prev_leaf,
                                kind: token.kind(),
                                content: token.content().to_vec(),
                            });
                            stats.tokens_added += 1;
                            prev_leaf = Some(leaf_id);
                        }

                        let line_op = BuilderLineOps::insert(branch_id, prev_branch, leaf_ops)
                            .with_new_line_num(line_idx + 1);
                        collected_line_ops.push(line_op);
                        stats.lines_added += 1;
                        prev_branch = Some(branch_id);
                    }
                }
                _new_line_idx = new_pos + len;
            }
            crate::diff::DiffOp::Replace {
                old_pos,
                old_len,
                new_pos,
                new_len,
            } => {
                // ══════════════════════════════════════════════════════════
                // Replace → BranchOp::Modify (equal count) or Delete+Insert
                // ══════════════════════════════════════════════════════════
                //
                // When old_len == new_len (1:1 replacement), the diff algorithm
                // anchored each old line to exactly one new line.  We emit
                // BranchOp::Modify so the display layer can show adjacent -/+
                // pairs with word-level highlighting.
                //
                // When counts differ (pure insertions or deletions within the
                // block), we emit all old lines as Delete then all new lines as
                // Insert — matching git's unified diff format exactly.
                //
                // NOTE: For git-imported changes, the CRDT ops are overridden
                // in write_commit() with build_crdt_ops_from_git_diff(), so
                // what we emit here only matters for atomic record (non-import).

                if *old_len == *new_len {
                    // Equal counts: positional 1:1 Modify — always correct
                    // because the diff algorithm anchored these lines together.
                    let build_old_leaf_ops = |line_idx: usize| -> Vec<LeafOp> {
                        if line_idx < old_lines.len() {
                            old_lines[line_idx]
                                .tokens()
                                .iter()
                                .map(|t| LeafOp::Insert {
                                    after: None,
                                    kind: t.kind(),
                                    content: t.content().to_vec(),
                                })
                                .collect()
                        } else {
                            Vec::new()
                        }
                    };

                    for i in 0..*old_len {
                        let old_line_idx = old_pos + i;
                        let new_line_idx = new_pos + i;
                        let branch_id = alloc_branch();
                        let old_leaf_ops = build_old_leaf_ops(old_line_idx);

                        let mut prev_leaf: Option<crate::crdt::LeafId> = None;
                        let new_leaf_ops: Vec<LeafOp> = if new_line_idx < new_lines.len() {
                            new_lines[new_line_idx]
                                .tokens()
                                .iter()
                                .map(|t| {
                                    let leaf_id = alloc_leaf();
                                    let op = LeafOp::Insert {
                                        after: prev_leaf,
                                        kind: t.kind(),
                                        content: t.content().to_vec(),
                                    };
                                    stats.tokens_added += 1;
                                    prev_leaf = Some(leaf_id);
                                    op
                                })
                                .collect()
                        } else {
                            Vec::new()
                        };

                        let line_op = BuilderLineOps::modify(branch_id, old_leaf_ops, new_leaf_ops)
                            .with_old_line_num(old_line_idx + 1)
                            .with_new_line_num(new_line_idx + 1);
                        collected_line_ops.push(line_op);
                        stats.lines_modified += 1;
                        prev_branch = Some(branch_id);
                    }

                    _old_line_idx = old_pos + old_len;
                    _new_line_idx = new_pos + new_len;
                } else {
                    // Unequal counts: use bigram similarity to find the
                    // best match for each old line among the new lines.
                    let bigrams = |s: &str| -> std::collections::HashSet<(u8, u8)> {
                        let bytes = s.trim().as_bytes();
                        let mut set = std::collections::HashSet::new();
                        if bytes.len() >= 2 {
                            for w in bytes.windows(2) {
                                set.insert((w[0], w[1]));
                            }
                        }
                        set
                    };

                    // paired_old[oi] = Some(ni) means old line oi pairs with new line ni
                    let mut paired_old: Vec<Option<usize>> = vec![None; *old_len];
                    let mut matched_new: Vec<bool> = vec![false; *new_len];

                    let mut scores: Vec<(usize, usize, f64)> = Vec::new();
                    for oi in 0..*old_len {
                        let old_idx = old_pos + oi;
                        let old_text = if old_idx < old_lines.len() {
                            String::from_utf8_lossy(old_lines[old_idx].content())
                        } else {
                            continue;
                        };
                        let old_bg = bigrams(old_text.trim());
                        if old_bg.is_empty() {
                            continue;
                        }

                        for ni in 0..*new_len {
                            let new_idx = new_pos + ni;
                            let new_text = if new_idx < new_lines.len() {
                                String::from_utf8_lossy(new_lines[new_idx].content())
                            } else {
                                continue;
                            };
                            let new_bg = bigrams(new_text.trim());
                            if new_bg.is_empty() {
                                continue;
                            }
                            let inter = old_bg.intersection(&new_bg).count();
                            let union = old_bg.union(&new_bg).count();
                            if union > 0 {
                                let score = inter as f64 / union as f64;
                                if score >= 0.3 {
                                    scores.push((oi, ni, score));
                                }
                            }
                        }
                    }

                    scores
                        .sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

                    for (oi, ni, _score) in &scores {
                        if paired_old[*oi].is_some() || matched_new[*ni] {
                            continue;
                        }
                        paired_old[*oi] = Some(*ni);
                        matched_new[*ni] = true;
                    }

                    // Walk old lines: unpaired → Delete
                    for (oi, paired_old_item) in paired_old.iter().enumerate().take(*old_len) {
                        if paired_old_item.is_some() {
                            continue;
                        }
                        let old_line_idx = old_pos + oi;
                        let branch_id = alloc_branch();
                        let content = if old_line_idx < old_lines.len() {
                            old_lines[old_line_idx]
                                .tokens()
                                .iter()
                                .map(|t| LeafOp::Insert {
                                    after: None,
                                    kind: t.kind(),
                                    content: t.content().to_vec(),
                                })
                                .collect()
                        } else {
                            Vec::new()
                        };
                        let line_op = BuilderLineOps::delete(branch_id, content)
                            .with_old_line_num(old_line_idx + 1);
                        collected_line_ops.push(line_op);
                        stats.lines_deleted += 1;
                    }

                    // Walk new lines: paired → Modify, unpaired → Insert
                    for ni in 0..*new_len {
                        let new_line_idx = new_pos + ni;
                        // Find if any old line pairs with this new line
                        let paired_oi = paired_old.iter().position(|m| m == &Some(ni));

                        if let Some(oi) = paired_oi {
                            let old_line_idx = old_pos + oi;
                            let branch_id = alloc_branch();
                            let old_leaf_ops = if old_line_idx < old_lines.len() {
                                old_lines[old_line_idx]
                                    .tokens()
                                    .iter()
                                    .map(|t| LeafOp::Insert {
                                        after: None,
                                        kind: t.kind(),
                                        content: t.content().to_vec(),
                                    })
                                    .collect()
                            } else {
                                Vec::new()
                            };
                            let mut prev_leaf: Option<crate::crdt::LeafId> = None;
                            let new_leaf_ops: Vec<LeafOp> = if new_line_idx < new_lines.len() {
                                new_lines[new_line_idx]
                                    .tokens()
                                    .iter()
                                    .map(|t| {
                                        let leaf_id = alloc_leaf();
                                        let op = LeafOp::Insert {
                                            after: prev_leaf,
                                            kind: t.kind(),
                                            content: t.content().to_vec(),
                                        };
                                        stats.tokens_added += 1;
                                        prev_leaf = Some(leaf_id);
                                        op
                                    })
                                    .collect()
                            } else {
                                Vec::new()
                            };
                            let line_op =
                                BuilderLineOps::modify(branch_id, old_leaf_ops, new_leaf_ops)
                                    .with_old_line_num(old_line_idx + 1)
                                    .with_new_line_num(new_line_idx + 1);
                            collected_line_ops.push(line_op);
                            stats.lines_modified += 1;
                            prev_branch = Some(branch_id);
                        } else {
                            // Unpaired new line → Insert
                            if new_line_idx < new_lines.len() {
                                let branch_id = alloc_branch();
                                let mut prev_leaf: Option<crate::crdt::LeafId> = None;
                                let leaf_ops: Vec<LeafOp> = new_lines[new_line_idx]
                                    .tokens()
                                    .iter()
                                    .map(|t| {
                                        let leaf_id = alloc_leaf();
                                        let op = LeafOp::Insert {
                                            after: prev_leaf,
                                            kind: t.kind(),
                                            content: t.content().to_vec(),
                                        };
                                        stats.tokens_added += 1;
                                        prev_leaf = Some(leaf_id);
                                        op
                                    })
                                    .collect();
                                let line_op =
                                    BuilderLineOps::insert(branch_id, prev_branch, leaf_ops)
                                        .with_new_line_num(new_line_idx + 1);
                                collected_line_ops.push(line_op);
                                stats.lines_added += 1;
                                prev_branch = Some(branch_id);
                            }
                        }
                    }

                    _old_line_idx = old_pos + old_len;
                    _new_line_idx = new_pos + new_len;
                } // end unequal-count branch
            }
        }
    }

    // ── Cross-block Delete+Insert→Modify consolidation ───────────────────
    //
    // After the Replace blocks have handled within-block pairing, this pass
    // promotes any remaining standalone Delete+Insert pairs (from separate
    // DiffOp::Delete and DiffOp::Insert operations) into BranchOp::Modify
    // when the lines are similar (bigram Jaccard ≥ 0.3).
    //
    // NOTE: For git-imported changes, build_crdt_ops_from_git_diff() overrides
    // the entire CRDT output, so this pairing only affects `atomic record`.
    {
        let extract_text = |op: &BuilderLineOps| -> String {
            let leaves = match op.operation() {
                BranchOp::Delete { content, .. } | BranchOp::Insert { content, .. } => content,
                BranchOp::Modify { new_content, .. } => new_content,
                _ => return String::new(),
            };
            let mut text = String::new();
            for leaf in leaves.iter() {
                if let LeafOp::Insert { content: bytes, .. } = leaf {
                    if let Ok(s) = std::str::from_utf8(bytes) {
                        text.push_str(s);
                    }
                }
            }
            text
        };

        let bigrams2 = |s: &str| -> std::collections::HashSet<(u8, u8)> {
            let bytes = s.trim().as_bytes();
            let mut set = std::collections::HashSet::new();
            if bytes.len() >= 2 {
                for w in bytes.windows(2) {
                    set.insert((w[0], w[1]));
                }
            }
            set
        };

        type BigramEntry = (usize, String, std::collections::HashSet<(u8, u8)>);
        let mut del_entries: Vec<BigramEntry> = Vec::new();
        let mut ins_entries: Vec<BigramEntry> = Vec::new();

        for (idx, op) in collected_line_ops.iter().enumerate() {
            if op.is_modify() {
                continue;
            }
            let text = extract_text(op);
            let trimmed = text.trim().to_string();
            if trimmed.len() < 2 {
                continue;
            }
            let bg = bigrams2(&trimmed);
            if bg.is_empty() {
                continue;
            }
            if op.is_delete() {
                del_entries.push((idx, trimmed, bg));
            } else if op.is_insert() {
                ins_entries.push((idx, trimmed, bg));
            }
        }

        let mut candidates: Vec<(usize, usize, f64)> = Vec::new();
        for (di, (_, _, del_bg)) in del_entries.iter().enumerate() {
            for (ii, (_, _, ins_bg)) in ins_entries.iter().enumerate() {
                let inter = del_bg.intersection(ins_bg).count();
                let union = del_bg.union(ins_bg).count();
                if union > 0 {
                    let score = inter as f64 / union as f64;
                    if score >= 0.3 {
                        candidates.push((di, ii, score));
                    }
                }
            }
        }
        candidates.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

        let mut matched_del: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut matched_ins: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut promote: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();

        for (di, ii, _) in &candidates {
            if matched_del.contains(di) || matched_ins.contains(ii) {
                continue;
            }
            matched_del.insert(*di);
            matched_ins.insert(*ii);
            let del_idx = del_entries[*di].0;
            let ins_idx = ins_entries[*ii].0;
            promote.insert(ins_idx, del_idx);
        }

        let del_indices_to_skip: std::collections::HashSet<usize> =
            promote.values().copied().collect();

        let placeholder_id = BranchId::new(NodeId::new(0), 0);
        let make_placeholder = || {
            BuilderLineOps::new(
                placeholder_id,
                BranchOp::Restore {
                    branch: placeholder_id,
                },
            )
        };

        let mut slots: Vec<BuilderLineOps> = collected_line_ops
            .iter_mut()
            .map(|op| std::mem::replace(op, make_placeholder()))
            .collect();

        let mut consolidated: Vec<BuilderLineOps> = Vec::with_capacity(slots.len());

        for idx in 0..slots.len() {
            if del_indices_to_skip.contains(&idx) {
                continue;
            }
            if let Some(&del_idx) = promote.get(&idx) {
                let del_op = &slots[del_idx];
                let ins_op = &slots[idx];
                let old_line_num = del_op.old_line_num();
                let new_line_num = ins_op.new_line_num();
                let old_content = match del_op.operation() {
                    BranchOp::Delete { content, .. } => content.clone(),
                    _ => Vec::new(),
                };
                let new_content = match ins_op.operation() {
                    BranchOp::Insert { content, .. } => content.clone(),
                    _ => Vec::new(),
                };
                let branch_id = del_op.branch_id();
                let mut modify = BuilderLineOps::modify(branch_id, old_content, new_content);
                if let Some(v) = old_line_num {
                    modify = modify.with_old_line_num(v);
                }
                if let Some(v) = new_line_num {
                    modify = modify.with_new_line_num(v);
                }
                consolidated.push(modify);
            } else {
                let op = std::mem::replace(&mut slots[idx], make_placeholder());
                consolidated.push(op);
            }
        }

        collected_line_ops = consolidated;
    }

    // Add final line_ops to file_ops.
    for line_op in collected_line_ops {
        file_ops.add_line_op(line_op);
    }

    // Convert to the canonical change::FileOps type
    (file_ops.into_change_ops(), stats)
}
