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
                // Replace → BranchOp::Modify / Delete / Insert
                // ══════════════════════════════════════════════════════════
                //
                // The diff algorithm already determined that these old lines
                // were *replaced* by these new lines.  We preserve that
                // semantic information by emitting:
                //
                //   • BranchOp::Modify  — for each old↔new pair (a line
                //     that changed but kept its identity).  Carries both
                //     old and new content so every consumer can render
                //     word-level diffs without heuristic re-pairing.
                //
                //   • BranchOp::Delete  — for old lines with no match
                //     (pure removals within the block).
                //
                //   • BranchOp::Insert  — for new lines with no match
                //     (pure additions within the block).
                //
                // Pairing strategy:
                //   equal counts  → positional (old[0]↔new[0], …)
                //   unequal counts → greedy best-match by bigram Jaccard,
                //     then positional fallback for any remaining unpaired.
                //
                // This is the ONLY place pairing is determined.  No
                // downstream heuristic, threshold tuning, or post-
                // processing is needed.

                let min_len = (*old_len).min(*new_len);
                let _max_len = (*old_len).max(*new_len);

                // ── Build token lists for old/new lines ──────────────────

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

                let build_new_leaf_ops = |line_idx: usize,
                                          alloc_leaf: &mut dyn FnMut() -> LeafId,
                                          stats: &mut CrdtBuildStats|
                 -> Vec<LeafOp> {
                    if line_idx < new_lines.len() {
                        let mut prev_leaf: Option<LeafId> = None;
                        new_lines[line_idx]
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
                    }
                };

                // ── Determine old→new pairing ────────────────────────────

                // paired[oi] = Some(ni) means old line oi pairs with new line ni
                let mut paired: Vec<Option<usize>> = vec![None; *old_len];
                let mut matched_new: Vec<bool> = vec![false; *new_len];

                if *old_len == *new_len {
                    // Equal counts: positional pairing — always correct
                    // because the diff algorithm already anchored these
                    // lines to the same position range.
                    for i in 0..min_len {
                        paired[i] = Some(i);
                        matched_new[i] = true;
                    }
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

                    // Build all scores, then greedily assign best matches.
                    //
                    // Only pairs with score ≥ 0.3 are accepted.  Poor
                    // matches are left as unpaired Deletes so the
                    // consolidation pass (which scans across ALL line_ops,
                    // not just within one Replace block) can find the real
                    // partner in a different block.
                    //
                    // Example: the diff may split `function hello()` into
                    // Replace #0 and `function hello(name:)` into Replace
                    // #2. Without the threshold, the Replace #0 handler
                    // would force-pair `function hello()` with an unrelated
                    // import line, creating a bad Modify that the
                    // consolidation pass can't fix.
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

                    // Sort descending by score — best matches first
                    scores
                        .sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

                    for (oi, ni, _score) in &scores {
                        if paired[*oi].is_some() || matched_new[*ni] {
                            continue;
                        }
                        paired[*oi] = Some(*ni);
                        matched_new[*ni] = true;
                    }

                    // No positional fallback — unpaired old lines become
                    // Delete ops, available for cross-block consolidation.
                }

                // ── Emit ops in new-file order ───────────────────────────
                //
                // Walk new lines 0..new_len in order.  For each:
                //   • If paired with an old line → BranchOp::Modify
                //   • If unpaired → BranchOp::Insert
                //
                // Then emit any unpaired old lines as BranchOp::Delete.

                // Build reverse map: ni → oi
                let mut new_to_old: Vec<Option<usize>> = vec![None; *new_len];
                for (oi, maybe_ni) in paired.iter().enumerate() {
                    if let Some(ni) = maybe_ni {
                        new_to_old[*ni] = Some(oi);
                    }
                }

                // Unpaired deletes first (old lines that have no match)
                for (oi, paired_item) in paired.iter().enumerate().take(*old_len) {
                    if paired_item.is_some() {
                        continue;
                    }
                    let old_line_idx = old_pos + oi;
                    let branch_id = alloc_branch();
                    let content = build_old_leaf_ops(old_line_idx);
                    let line_op = BuilderLineOps::delete(branch_id, content)
                        .with_old_line_num(old_line_idx + 1);
                    collected_line_ops.push(line_op);
                    stats.lines_deleted += 1;
                }

                // Walk new lines in order
                for (ni, new_to_old_entry) in new_to_old.iter().enumerate().take(*new_len) {
                    let new_line_idx = new_pos + ni;

                    if let Some(oi) = *new_to_old_entry {
                        // Paired → emit BranchOp::Modify
                        let old_line_idx = old_pos + oi;
                        let branch_id = alloc_branch();
                        let old_leaf_ops = build_old_leaf_ops(old_line_idx);
                        let new_leaf_ops =
                            build_new_leaf_ops(new_line_idx, &mut alloc_leaf, &mut stats);
                        let line_op = BuilderLineOps::modify(branch_id, old_leaf_ops, new_leaf_ops)
                            .with_old_line_num(old_line_idx + 1)
                            .with_new_line_num(new_line_idx + 1);
                        collected_line_ops.push(line_op);
                        stats.lines_modified += 1;
                        prev_branch = Some(branch_id);
                    } else {
                        // Unpaired → emit BranchOp::Insert
                        let branch_id = alloc_branch();
                        let leaf_ops =
                            build_new_leaf_ops(new_line_idx, &mut alloc_leaf, &mut stats);
                        let line_op = BuilderLineOps::insert(branch_id, prev_branch, leaf_ops)
                            .with_new_line_num(new_line_idx + 1);
                        collected_line_ops.push(line_op);
                        stats.lines_added += 1;
                        prev_branch = Some(branch_id);
                    }
                }

                _old_line_idx = old_pos + old_len;
                _new_line_idx = new_pos + new_len;
            }
        }
    }

    // ── Consolidation pass: promote Delete+Insert → Modify ───────────
    //
    // Replace blocks already emit BranchOp::Modify directly.  But the
    // diff algorithm can also produce *separate* Delete and Insert ops
    // for lines that are similar but at distant positions (e.g. a
    // function signature that moved down the file).
    //
    // This pass scans ALL Delete ops and finds their best matching
    // Insert op (by character-bigram Jaccard similarity ≥ 0.3).  When
    // a match is found the Delete is removed, the Insert is replaced
    // by a Modify carrying both old and new content, and the Modify
    // occupies the Insert's original position in the stream.
    //
    // This means the Modify appears at the point in the new-file order
    // where the new content lives — surrounded by its neighbouring
    // inserts — which gives the diff viewer the correct alignment.
    //
    // This is the ONLY place non-Replace similarity matching happens.
    // No downstream consumer needs to guess.
    {
        // ── Extract text from leaf ops ─────────────────────────────────
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

        // ── Collect Delete indices and their text ──────────────────────
        #[allow(clippy::type_complexity)]
        let mut del_entries: Vec<(usize, String, std::collections::HashSet<(u8, u8)>)> = Vec::new();
        #[allow(clippy::type_complexity)]
        let mut ins_entries: Vec<(usize, String, std::collections::HashSet<(u8, u8)>)> = Vec::new();

        for (idx, op) in collected_line_ops.iter().enumerate() {
            if op.is_modify() {
                // Already a Modify (from Replace handler) — skip
                continue;
            }
            let text = extract_text(op);
            let trimmed = text.trim().to_string();
            if trimmed.len() < 2 {
                continue;
            }
            let bg = bigrams(&trimmed);
            if bg.is_empty() {
                continue;
            }
            if op.is_delete() {
                del_entries.push((idx, trimmed, bg));
            } else if op.is_insert() {
                ins_entries.push((idx, trimmed, bg));
            }
        }

        // ── Build all (del, ins, score) triples, sort by score desc ────
        let mut candidates: Vec<(usize, usize, f64)> = Vec::new();

        for (di, (_del_idx, _del_text, del_bg)) in del_entries.iter().enumerate() {
            for (ii, (_ins_idx, _ins_text, ins_bg)) in ins_entries.iter().enumerate() {
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

        // ── Greedy assignment: best score first ────────────────────────
        let mut matched_del: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut matched_ins: std::collections::HashSet<usize> = std::collections::HashSet::new();
        // Maps: insert_index_in_collected → delete_index_in_collected
        let mut promote: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();

        for (di, ii, _score) in &candidates {
            if matched_del.contains(di) || matched_ins.contains(ii) {
                continue;
            }
            matched_del.insert(*di);
            matched_ins.insert(*ii);
            let del_idx = del_entries[*di].0;
            let ins_idx = ins_entries[*ii].0;
            promote.insert(ins_idx, del_idx);
        }

        // ── Rebuild the ops list ───────────────────────────────────────
        //
        // Walk collected_line_ops in original order.
        //   • Skip Deletes that were matched (they merge into a Modify).
        //   • When we reach an Insert that was matched, replace it with
        //     a Modify carrying both old (from the Delete) and new
        //     (from the Insert) content.
        //   • Everything else passes through unchanged.
        let del_indices_to_skip: std::collections::HashSet<usize> =
            promote.values().copied().collect();

        let n = collected_line_ops.len();
        // We need to move ops out of the vec, so replace with placeholders
        let placeholder_id = BranchId::new(NodeId::new(0), 0);
        let make_placeholder = || {
            BuilderLineOps::new(
                placeholder_id,
                BranchOp::Restore {
                    branch: placeholder_id,
                },
            )
        };

        // Take ownership of all ops via swap
        let mut slots: Vec<BuilderLineOps> = collected_line_ops
            .iter_mut()
            .map(|op| std::mem::replace(op, make_placeholder()))
            .collect();

        let mut consolidated: Vec<BuilderLineOps> = Vec::with_capacity(n);

        for idx in 0..n {
            if del_indices_to_skip.contains(&idx) {
                // This Delete will merge into its paired Insert's Modify
                continue;
            }

            if let Some(&del_idx) = promote.get(&idx) {
                // This Insert is paired with a Delete → emit Modify
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
                // Pass through unchanged
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
