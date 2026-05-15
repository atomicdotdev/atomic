//! Change diff helpers for the diff command.
//!
//! This module contains methods for showing diffs of specific changes:
//! resolving change references, printing change headers, and computing
//! diffs from both the semantic layer (FileOps) and raw content.

use super::output::*;
use super::*;

impl Diff {
    /// Print a message when there are no changes.
    pub(super) fn print_no_changes(&self) {
        print_info("No changes detected");
    }

    /// Show the diff for a specific change by hash or prefix.
    ///
    /// This displays the content introduced by the change using state-based
    /// content retrieval. For each file modified by the change, we retrieve:
    /// - The file content BEFORE the change was applied (parent state)
    /// - The file content AFTER the change was applied (current state)
    ///
    /// Then we compute a proper diff between the two states, with optional
    /// word-level highlighting for code review.
    pub(super) fn show_change_diff(
        &self,
        repo: &Repository,
        change_ref: &str,
        config: &DiffOutputConfig,
    ) -> CliResult<()> {
        // Resolve the change reference (full hash or prefix)
        let hash = self.resolve_change_ref(repo, change_ref)?;

        // Load the change
        let change =
            repo.change_store()
                .load_change(&hash)
                .map_err(|_e| CliError::ChangeNotFound {
                    hash: change_ref.to_string(),
                })?;

        // Check if change has semantic layer (file_ops)
        if change.has_file_ops() {
            // Use the semantic layer for human-readable diff
            return self.show_change_diff_from_file_ops(&change, &hash, config);
        }

        // Fallback: compute diff from content (legacy path)
        self.show_change_diff_computed(repo, &change, &hash, config)
    }

    /// Show diff using the semantic layer (FileOps).
    ///
    /// This is the preferred path - it displays line-level and token-level
    /// changes directly from the stored CRDT operations, without recomputing.
    fn show_change_diff_from_file_ops(
        &self,
        change: &Change,
        change_hash: &Hash,
        config: &DiffOutputConfig,
    ) -> CliResult<()> {
        let file_ops = change.file_ops();

        if file_ops.is_empty() {
            self.print_no_changes();
            return Ok(());
        }

        let mut file_diffs = Vec::new();
        let mut stats = DiffStats::new();

        for ops in file_ops {
            let file_path = ops.path();
            let trunk_op = ops.trunk_op();
            let line_ops = ops.line_ops();

            // Determine file change status from trunk operation
            let change_status = match trunk_op {
                Some(TrunkOp::Create { .. }) => FileChangeStatus::Added,
                Some(TrunkOp::Delete { .. }) => FileChangeStatus::Deleted,
                Some(TrunkOp::Move { .. }) => FileChangeStatus::Renamed,
                Some(TrunkOp::Undelete { .. }) => FileChangeStatus::Modified,
                None => FileChangeStatus::Modified,
            };

            let mut file_diff = match change_status {
                FileChangeStatus::Added => FileDiff::added(file_path),
                FileChangeStatus::Deleted => FileDiff::deleted(file_path),
                FileChangeStatus::Renamed => FileDiff::modified(file_path),
                _ => FileDiff::modified(file_path),
            };

            // Build hunks from line operations
            let mut insertions = 0usize;
            let mut deletions = 0usize;
            let mut new_line_num = 1usize;
            let mut old_line_num = 1usize;

            // Group consecutive operations into hunks
            if !line_ops.is_empty() {
                let mut current_hunk = DiffHunk::new(old_line_num, 0, new_line_num, 0);

                for line_op in line_ops {
                    match line_op.operation() {
                        BranchOp::Insert { content, .. } => {
                            // Reconstruct line content from leaf operations
                            let line_content = Self::reconstruct_line_from_leaf_ops(content);
                            // Use stored line number if available, otherwise use counter
                            let line_num = line_op.new_line_num().unwrap_or(new_line_num);
                            current_hunk.add_line(HunkLine::added(line_content, line_num));
                            new_line_num = line_num + 1;
                            insertions += 1;
                            current_hunk.new_count += 1;
                        }
                        BranchOp::Delete { content, .. } => {
                            // Reconstruct deleted line content from stored leaf operations
                            let line_content = if content.is_empty() {
                                String::from("<deleted line>")
                            } else {
                                Self::reconstruct_line_from_leaf_ops(content)
                            };
                            // Use stored line number if available, otherwise use counter
                            let line_num = line_op.old_line_num().unwrap_or(old_line_num);
                            current_hunk.add_line(HunkLine::removed(line_content, line_num));
                            old_line_num = line_num + 1;
                            deletions += 1;
                            current_hunk.old_count += 1;
                        }
                        BranchOp::Modify {
                            old_content,
                            new_content,
                            ..
                        } => {
                            // A Modify carries both old and new content.
                            // Emit them as adjacent removed + added lines
                            // so print_unified can pair them for word-level
                            // highlighting.
                            let old_line_content = if old_content.is_empty() {
                                String::from("<modified line>")
                            } else {
                                Self::reconstruct_line_from_leaf_ops(old_content)
                            };
                            let new_line_content =
                                Self::reconstruct_line_from_leaf_ops(new_content);

                            let old_ln = line_op.old_line_num().unwrap_or(old_line_num);
                            let new_ln = line_op.new_line_num().unwrap_or(new_line_num);

                            current_hunk.add_line(HunkLine::removed(old_line_content, old_ln));
                            current_hunk.add_line(HunkLine::added(new_line_content, new_ln));

                            old_line_num = old_ln + 1;
                            new_line_num = new_ln + 1;
                            deletions += 1;
                            insertions += 1;
                            current_hunk.old_count += 1;
                            current_hunk.new_count += 1;
                        }
                        BranchOp::Restore { .. } => {
                            // Restore is like an add for display purposes
                            let line_num = line_op.new_line_num().unwrap_or(new_line_num);
                            current_hunk.add_line(HunkLine::added(
                                String::from("<restored line>"),
                                line_num,
                            ));
                            new_line_num = line_num + 1;
                            insertions += 1;
                            current_hunk.new_count += 1;
                        }
                        BranchOp::Reparent { .. } => {
                            // Position-only change — no visible delta in
                            // unified diff output.  A future "blame-aware"
                            // diff might render these explicitly; for now
                            // they're silent.
                        }
                    }
                }

                if current_hunk.has_changes() {
                    // Re-pair Delete+Insert lines by content similarity.
                    //
                    // The CRDT builder emits all Deletes before all Inserts
                    // for unequal-count Replace blocks (to preserve correct
                    // BRANCH_AFTER chain ordering).  This is correct for
                    // the graph, but produces poor diff output because
                    // modified lines appear as scattered -/+ pairs instead
                    // of adjacent ones.
                    //
                    // This post-pass identifies contiguous runs of Removed
                    // lines followed by Added lines and re-interleaves
                    // them: each Removed line is paired with its best-
                    // matching Added line (by bigram Jaccard similarity)
                    // and emitted adjacently (-/+).  Unpaired lines keep
                    // their original position.
                    current_hunk.lines = Self::repair_diff_lines(current_hunk.lines);
                    file_diff.add_hunk(current_hunk);
                }
            }

            // Set stats based on change type
            file_diff.stats = match change_status {
                FileChangeStatus::Added => FileDiffStats::added(file_path, insertions),
                FileChangeStatus::Deleted => FileDiffStats::deleted(file_path, deletions),
                _ => FileDiffStats::modified(file_path, insertions, deletions),
            };

            stats.add_file(file_diff.stats.clone());
            file_diffs.push(file_diff);
        }

        if file_diffs.is_empty() {
            self.print_no_changes();
            return Ok(());
        }

        // Print change header information
        if config.format == DiffFormat::Unified {
            self.print_change_header(change, change_hash, config);
        }

        // Print in the appropriate format
        match config.format {
            DiffFormat::Unified => self.print_unified(&file_diffs, config),
            DiffFormat::Stat => self.print_stat(&stats, config),
            DiffFormat::NameOnly => self.print_name_only(&file_diffs),
            DiffFormat::NameStatus => self.print_name_status(&file_diffs, config),
        }
    }

    /// Reconstruct a line's text content from its leaf operations.
    pub(super) fn reconstruct_line_from_leaf_ops(leaf_ops: &[LeafOp]) -> String {
        let mut line = String::new();
        for leaf_op in leaf_ops {
            match leaf_op {
                LeafOp::Insert { content, .. } => {
                    if let Ok(text) = std::str::from_utf8(content) {
                        line.push_str(text);
                    }
                }
                LeafOp::Replace { new_content, .. } => {
                    if let Ok(text) = std::str::from_utf8(new_content) {
                        line.push_str(text);
                    }
                }
                LeafOp::Delete { .. } | LeafOp::Restore { .. } => {
                    // These don't add content to the line
                }
            }
        }
        line
    }

    /// Re-pair Delete+Insert lines by content similarity for display.
    ///
    /// The CRDT builder emits all Deletes before all Inserts within each
    /// Replace block (to preserve BRANCH_AFTER chain ordering).  When the
    /// Myers diff creates multiple Replace blocks for one file, a deleted
    /// line and its matching insertion may land in different blocks.
    ///
    /// This post-pass collects ALL Removed and Added lines across the
    /// entire hunk, pairs them globally by bigram Jaccard similarity
    /// (≥ 0.3 threshold), then re-emits lines in original order with
    /// each paired Added line pulled forward to appear immediately after
    /// its matching Removed line.
    fn repair_diff_lines(lines: Vec<HunkLine>) -> Vec<HunkLine> {
        use std::collections::{HashMap, HashSet};

        // Helper: compute character bigrams for Jaccard similarity
        fn bigrams(s: &str) -> HashSet<(u8, u8)> {
            let bytes = s.trim().as_bytes();
            let mut set = HashSet::new();
            if bytes.len() >= 2 {
                for w in bytes.windows(2) {
                    set.insert((w[0], w[1]));
                }
            }
            set
        }

        fn jaccard(a: &HashSet<(u8, u8)>, b: &HashSet<(u8, u8)>) -> f64 {
            if a.is_empty() && b.is_empty() {
                return 0.0;
            }
            let inter = a.intersection(b).count();
            let union = a.union(b).count();
            if union == 0 {
                0.0
            } else {
                inter as f64 / union as f64
            }
        }

        // 1. Collect all Removed and Added lines with their original indices
        let mut rm_entries: Vec<(usize, &HunkLine)> = Vec::new();
        let mut add_entries: Vec<(usize, &HunkLine)> = Vec::new();

        for (idx, line) in lines.iter().enumerate() {
            if line.is_removed() {
                rm_entries.push((idx, line));
            } else if line.is_added() {
                add_entries.push((idx, line));
            }
        }

        // Short-circuit: nothing to pair
        if rm_entries.is_empty() || add_entries.is_empty() {
            return lines;
        }

        // 2. Compute bigrams
        let rm_bigrams: Vec<HashSet<(u8, u8)>> = rm_entries
            .iter()
            .map(|(_, l)| bigrams(&l.content))
            .collect();
        let add_bigrams: Vec<HashSet<(u8, u8)>> = add_entries
            .iter()
            .map(|(_, l)| bigrams(&l.content))
            .collect();

        // 3. Greedy best-match pairing across ALL removes and adds
        let mut candidates: Vec<(usize, usize, f64)> = Vec::new();
        for (ri, rb) in rm_bigrams.iter().enumerate() {
            if rb.is_empty() {
                continue;
            }
            for (ai, ab) in add_bigrams.iter().enumerate() {
                if ab.is_empty() {
                    continue;
                }
                let score = jaccard(rb, ab);
                if score >= 0.3 {
                    candidates.push((ri, ai, score));
                }
            }
        }
        candidates.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

        let mut matched_rm: HashSet<usize> = HashSet::new();
        let mut matched_add: HashSet<usize> = HashSet::new();
        // Map: original index of removed line → original index of added line
        let mut rm_to_add: HashMap<usize, usize> = HashMap::new();
        // Set of original indices of added lines that have been paired
        let mut paired_add_indices: HashSet<usize> = HashSet::new();

        for (ri, ai, _score) in &candidates {
            if matched_rm.contains(ri) || matched_add.contains(ai) {
                continue;
            }
            matched_rm.insert(*ri);
            matched_add.insert(*ai);
            let rm_orig_idx = rm_entries[*ri].0;
            let add_orig_idx = add_entries[*ai].0;
            rm_to_add.insert(rm_orig_idx, add_orig_idx);
            paired_add_indices.insert(add_orig_idx);
        }

        // Short-circuit: no pairs found
        if rm_to_add.is_empty() {
            return lines;
        }

        // 4. Re-emit lines in original order, pulling paired adds forward
        let mut result = Vec::with_capacity(lines.len());

        for (idx, line) in lines.iter().enumerate() {
            // Skip added lines that were already emitted after their pair
            if paired_add_indices.contains(&idx) {
                continue;
            }

            result.push(line.clone());

            // If this is a Removed line with a pair, emit the paired add
            if let Some(&add_idx) = rm_to_add.get(&idx) {
                result.push(lines[add_idx].clone());
            }
        }

        result
    }

    /// Show diff by computing from content (legacy fallback).
    ///
    /// Used when a change doesn't have file_ops (old changes or graph-only changes).
    fn show_change_diff_computed(
        &self,
        repo: &Repository,
        change: &Change,
        hash: &Hash,
        config: &DiffOutputConfig,
    ) -> CliResult<()> {
        use atomic_repository::get_files_in_change;

        // Get all files modified by this change
        let modified_files = get_files_in_change(change);

        if modified_files.is_empty() {
            self.print_no_changes();
            return Ok(());
        }

        // Parse algorithm for diffing
        let algorithm = self.parse_algorithm()?;

        // Compute diffs for each file using state-based content retrieval
        let mut file_diffs = Vec::new();
        let mut stats = DiffStats::new();

        for file_path in &modified_files {
            // Get content BEFORE the change was applied
            let before_content = match repo.get_file_content_before_change(file_path, hash) {
                Ok(content) => content.unwrap_or_default(),
                Err(_) => Vec::new(),
            };

            // Get content AFTER the change was applied
            let after_content = match repo.get_file_content_after_change(file_path, hash) {
                Ok(content) => content.unwrap_or_default(),
                Err(_) => Vec::new(),
            };

            // Determine the type of change based on before/after content
            let file_diff = match (before_content.is_empty(), after_content.is_empty()) {
                // File was added (no content before, has content after)
                (true, false) => {
                    let mut diff = FileDiff::added(file_path);
                    let lines: Vec<_> = after_content.split(|&b| b == b'\n').collect();
                    let line_count = lines.len();

                    if !after_content.is_empty() {
                        let mut graph_op = DiffHunk::new(0, 0, 1, line_count);
                        for (i, line_bytes) in lines.iter().enumerate() {
                            let line_content = String::from_utf8_lossy(line_bytes).into_owned();
                            graph_op.add_line(HunkLine::added(line_content, i + 1));
                        }
                        diff.add_hunk(graph_op);
                    }

                    diff.stats = FileDiffStats::added(file_path, line_count);
                    diff
                }

                // File was deleted (has content before, no content after)
                (false, true) => {
                    let mut diff = FileDiff::deleted(file_path);
                    let lines: Vec<_> = before_content.split(|&b| b == b'\n').collect();
                    let line_count = lines.len();

                    if !before_content.is_empty() {
                        let mut graph_op = DiffHunk::new(1, line_count, 0, 0);
                        for (i, line_bytes) in lines.iter().enumerate() {
                            let line_content = String::from_utf8_lossy(line_bytes).into_owned();
                            graph_op.add_line(HunkLine::removed(line_content, i + 1));
                        }
                        diff.add_hunk(graph_op);
                    }

                    diff.stats = FileDiffStats::deleted(file_path, line_count);
                    diff
                }

                // File was modified (has content both before and after)
                (false, false) => {
                    let mut diff = FileDiff::modified(file_path);

                    // Compute diff between old (before) and new (after) content
                    let diff_result = diff_text(&before_content, &after_content, algorithm);

                    if !diff_result.is_unchanged() {
                        let old_lines: Vec<_> = before_content.split(|&b| b == b'\n').collect();
                        let new_lines: Vec<_> = after_content.split(|&b| b == b'\n').collect();

                        // Build hunks with context
                        let hunks = build_hunks_from_diff(
                            &diff_result,
                            &old_lines,
                            &new_lines,
                            config.context_lines,
                        );
                        for graph_op in hunks {
                            diff.add_hunk(graph_op);
                        }
                    }

                    diff.compute_stats();
                    diff
                }

                // No content at all (shouldn't happen for files in change)
                (true, true) => {
                    continue; // Skip files with no content
                }
            };

            stats.add_file(file_diff.stats.clone());
            file_diffs.push(file_diff);
        }

        if file_diffs.is_empty() {
            self.print_no_changes();
            return Ok(());
        }

        // Print change header information
        if config.format == DiffFormat::Unified {
            self.print_change_header(change, hash, config);
        }

        // Print in the appropriate format
        match config.format {
            DiffFormat::Unified => self.print_unified(&file_diffs, config),
            DiffFormat::Stat => self.print_stat(&stats, config),
            DiffFormat::NameOnly => self.print_name_only(&file_diffs),
            DiffFormat::NameStatus => self.print_name_status(&file_diffs, config),
        }
    }

    /// Print header information for a change diff.
    ///
    /// Displays the change hash, message, author, and timestamp before
    /// showing the actual diff content.
    pub(super) fn print_change_header(
        &self,
        change: &Change,
        change_hash: &Hash,
        config: &DiffOutputConfig,
    ) {
        let header = &change.hashed.header;
        let hash_str = change_hash.to_base32();
        let display_hash = hash_str[..DEFAULT_HASH_LENGTH.min(hash_str.len())].to_string();

        // Print change identifier
        if config.color {
            println!("{} {}", emphasis("change"), hash(&display_hash));
        } else {
            println!("change {}", display_hash);
        }

        // Print author(s)
        for author in header.authors.iter() {
            let author_str = if let Some(ref email) = author.email {
                format!("{} <{}>", author.name, email)
            } else {
                author.name.clone()
            };
            if config.color {
                println!("Author: {}", info(&author_str));
            } else {
                println!("Author: {}", author_str);
            }
        }

        // Print timestamp
        let timestamp = header.timestamp.format("%Y-%m-%d %H:%M:%S %Z").to_string();
        if config.color {
            println!("Date:   {}", info(&timestamp));
        } else {
            println!("Date:   {}", timestamp);
        }

        // Print message
        println!();
        if config.color {
            println!("    {}", emphasis(&header.message));
        } else {
            println!("    {}", header.message);
        }

        // Print description if present
        if let Some(ref desc) = header.description {
            println!();
            for line in desc.lines() {
                println!("    {}", line);
            }
        }

        println!();
    }

    /// Resolve a change reference (full hash or prefix) to a full hash.
    pub(super) fn resolve_change_ref(
        &self,
        repo: &Repository,
        change_ref: &str,
    ) -> CliResult<Hash> {
        // Try to parse as a full hash first
        if let Some(hash) = Hash::from_base32(change_ref.as_bytes()) {
            if repo.has_change(&hash) {
                return Ok(hash);
            }
        }

        // Search for matching changes by prefix
        let mut matches: Vec<Hash> = Vec::new();
        let prefix_upper = change_ref.to_uppercase();

        for result in repo.iter_changes() {
            let hash = result.map_err(|e| CliError::Internal(anyhow::anyhow!("{}", e)))?;
            let hash_str = hash.to_base32();
            if hash_str.starts_with(&prefix_upper) {
                matches.push(hash);
            }
        }

        match matches.len() {
            0 => Err(CliError::ChangeNotFound {
                hash: change_ref.to_string(),
            }),
            1 => Ok(matches[0]),
            _ => {
                let match_list: Vec<String> = matches.iter().map(|h| h.to_base32()).collect();
                Err(CliError::AmbiguousHash {
                    hash: format!("{} (matches: {})", change_ref, match_list.join(", ")),
                })
            }
        }
    }
}
