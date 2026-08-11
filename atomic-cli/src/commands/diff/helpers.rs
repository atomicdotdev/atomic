//! Change diff helpers for the diff command.
//!
//! This module contains methods for showing diffs of specific changes:
//! resolving change references, printing change headers, and computing
//! diffs from both the semantic layer (FileOps) and raw content.

use super::output::*;
use super::*;
use atomic_core::record::workflow::GitDiffLine;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct GitMetadataDiffFile {
    path: String,
    lines: Vec<GitDiffLine>,
}

/// A changed line paired with the insert/delete offset preceding it.
///
/// `net_before` is (insertions - deletions) before this line, which is what
/// maps the line between old-file and new-file coordinates: an added line
/// `n` was inserted after old line `n - 1 - net_before`, and a removed line
/// `l` sat at new-file boundary `l - 1 + net_before`.
#[derive(Debug, Clone)]
pub(super) struct NumberedLine {
    line: HunkLine,
    net_before: isize,
}

impl NumberedLine {
    pub(super) fn added(content: impl Into<String>, new_num: usize, net_before: isize) -> Self {
        Self {
            line: HunkLine::added(content, new_num),
            net_before,
        }
    }

    pub(super) fn removed(content: impl Into<String>, old_num: usize, net_before: isize) -> Self {
        Self {
            line: HunkLine::removed(content, old_num),
            net_before,
        }
    }

    /// Old-file position: the line number for removals, or the insertion
    /// boundary (the number of the preceding old line) for additions.
    fn old_pos(&self) -> usize {
        match self.line.status {
            LineStatus::Removed => self.line.old_line_num.unwrap_or(1),
            _ => {
                (self.line.new_line_num.unwrap_or(1) as isize - 1 - self.net_before).max(0) as usize
            }
        }
    }

    /// New-file position: the line number for additions, or the deletion
    /// boundary (the number of the preceding new line) for removals.
    fn new_pos(&self) -> usize {
        match self.line.status {
            LineStatus::Added => self.line.new_line_num.unwrap_or(1),
            _ => {
                (self.line.old_line_num.unwrap_or(1) as isize - 1 + self.net_before).max(0) as usize
            }
        }
    }
}

impl Diff {
    /// Print a message when there are no changes.
    pub(super) fn print_no_changes(&self) {
        print_info("No changes detected");
    }

    /// Check whether a repository-relative path passes the positional
    /// file filter (`atomic diff --change <hash> <file>...`).
    ///
    /// Returns `true` when no files were specified (no filtering) or when
    /// the path exactly matches one of the given files. A leading `./` on
    /// either side is tolerated.
    pub(super) fn file_matches_filter(&self, path: &str) -> bool {
        if self.files.is_empty() {
            return true;
        }
        let path = path.strip_prefix("./").unwrap_or(path);
        self.files.iter().any(|f| {
            let f = f.strip_prefix("./").unwrap_or(f.as_str());
            f == path
        })
    }

    /// Filter computed file diffs down to the paths given on the command
    /// line, rebuilding the aggregate stats from the surviving entries.
    ///
    /// This is a no-op when no positional files were specified.
    pub(super) fn filter_file_diffs(
        &self,
        file_diffs: Vec<FileDiff>,
        stats: DiffStats,
    ) -> (Vec<FileDiff>, DiffStats) {
        if self.files.is_empty() {
            return (file_diffs, stats);
        }
        let file_diffs: Vec<FileDiff> = file_diffs
            .into_iter()
            .filter(|d| {
                self.file_matches_filter(&d.old_path) || self.file_matches_filter(&d.new_path)
            })
            .collect();
        let mut stats = DiffStats::new();
        for d in &file_diffs {
            stats.add_file(d.stats.clone());
        }
        (file_diffs, stats)
    }

    /// Group numbered changed lines into diff hunks with true file offsets.
    ///
    /// `changed` must be in file order. Changes separated by more than
    /// `2 * context` unchanged lines become separate hunks; closer changes
    /// merge with the gap emitted as context. Context line content is taken
    /// from `before_lines` (the file's recorded before-content); when it is
    /// `None`, hunks are emitted with zero context but still carry true
    /// offsets derived from the stored line numbers.
    ///
    /// Hunk headers follow git's unified-diff conventions: a pure insertion
    /// at boundary `p` is `@@ -p,0 +n,k @@`, a pure deletion is
    /// `@@ -l,k +p,0 @@`, and context lines count toward both sides.
    pub(super) fn hunks_from_changed_lines(
        changed: &[NumberedLine],
        before_lines: Option<&[Vec<u8>]>,
        context: usize,
    ) -> Vec<DiffHunk> {
        let mut hunks = Vec::new();
        if changed.is_empty() {
            return hunks;
        }

        let old_len = before_lines.map(|l| l.len()).unwrap_or(0);
        let ctx = if before_lines.is_some() { context } else { 0 };
        let merge_gap = 2 * ctx as isize;

        // 1. Group into runs of nearby changes, measured in old-file
        //    coordinates.
        let mut runs: Vec<(usize, usize)> = Vec::new();
        let mut run_start = 0usize;
        let mut run_hi = changed[0].old_pos();
        for (i, nl) in changed.iter().enumerate().skip(1) {
            let lo = nl.old_pos();
            // Unchanged lines between the run and this change. An addition
            // sits at a boundary, so the gap is measured from the boundary
            // itself; a removal occupies a line, hence the extra -1.
            let gap = match nl.line.status {
                LineStatus::Removed => lo as isize - run_hi as isize - 1,
                _ => lo as isize - run_hi as isize,
            };
            if gap > merge_gap {
                runs.push((run_start, i));
                run_start = i;
            }
            run_hi = run_hi.max(nl.old_pos());
        }
        runs.push((run_start, changed.len()));

        // 2. Emit one hunk per run.
        for (start, end) in runs {
            let lines = &changed[start..end];

            let removed_count = lines.iter().filter(|l| l.line.is_removed()).count();
            let added_count = lines.len() - removed_count;

            let old_lo = lines.iter().map(|l| l.old_pos()).min().unwrap_or(0);
            let old_hi = lines.iter().map(|l| l.old_pos()).max().unwrap_or(0);
            let new_lo = lines.iter().map(|l| l.new_pos()).min().unwrap_or(0);

            // Old-file range covered by the hunk, including context.
            let (old_start, old_end, old_count) = if removed_count > 0 {
                let s = old_lo.saturating_sub(ctx).max(1);
                let e = if ctx > 0 {
                    old_hi.saturating_add(ctx).min(old_len)
                } else {
                    old_hi
                };
                let e = e.max(s);
                (s, e, e - s + 1)
            } else {
                // Pure insertion at boundary old_lo. With context, the old
                // side covers the surviving lines around the boundary;
                // without, it is the boundary itself with a count of 0.
                let s = (old_lo + 1).saturating_sub(ctx).max(1);
                let e = if ctx > 0 {
                    old_hi.saturating_add(ctx).min(old_len)
                } else {
                    0
                };
                if ctx > 0 && s <= e {
                    (s, e, e - s + 1)
                } else {
                    (old_lo, old_lo, 0)
                }
            };

            // Emit context and changed lines in file order. `offset` is the
            // running old→new line shift used to number context lines.
            let mut hunk_lines: Vec<HunkLine> = Vec::new();
            let mut offset: isize = lines[0].net_before;
            let mut ctx_cursor = old_start;
            let mut pre_ctx = 0usize;

            for (idx, nl) in lines.iter().enumerate() {
                if ctx > 0 {
                    let fill_end = match nl.line.status {
                        LineStatus::Removed => nl.old_pos().saturating_sub(1),
                        _ => nl.old_pos(),
                    };
                    while ctx_cursor <= fill_end && ctx_cursor <= old_end {
                        if let Some(content) = before_lines.and_then(|l| l.get(ctx_cursor - 1)) {
                            let new_num = (ctx_cursor as isize + offset).max(1) as usize;
                            hunk_lines.push(HunkLine::context(
                                String::from_utf8_lossy(content).into_owned(),
                                ctx_cursor,
                                new_num,
                            ));
                            if idx == 0 {
                                pre_ctx += 1;
                            }
                        }
                        ctx_cursor += 1;
                    }
                }

                hunk_lines.push(nl.line.clone());
                match nl.line.status {
                    LineStatus::Removed => {
                        offset -= 1;
                        ctx_cursor = ctx_cursor.max(nl.old_pos() + 1);
                    }
                    _ => {
                        offset += 1;
                    }
                }
            }

            // Trailing context.
            if ctx > 0 {
                while ctx_cursor <= old_end {
                    if let Some(content) = before_lines.and_then(|l| l.get(ctx_cursor - 1)) {
                        let new_num = (ctx_cursor as isize + offset).max(1) as usize;
                        hunk_lines.push(HunkLine::context(
                            String::from_utf8_lossy(content).into_owned(),
                            ctx_cursor,
                            new_num,
                        ));
                    }
                    ctx_cursor += 1;
                }
            }

            let context_count = hunk_lines.iter().filter(|l| l.is_context()).count();
            let new_count = context_count + added_count;
            let new_start = if new_count == 0 {
                // Pure deletion: anchor at the new-file boundary preceding
                // the deleted lines.
                new_lo
            } else {
                // New-file position of the first changed line, minus the
                // context lines emitted before it.
                let first = &lines[0];
                let anchor = match first.line.status {
                    LineStatus::Removed => first.new_pos() + 1,
                    _ => first.line.new_line_num.unwrap_or(1),
                };
                anchor.saturating_sub(pre_ctx).max(1)
            };

            let mut hunk = DiffHunk::new(old_start, old_count, new_start, new_count);
            hunk.lines = hunk_lines;
            hunks.push(hunk);
        }

        hunks
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

        // Git-imported changes carry Git's captured +/- lines in unhashed
        // metadata. Use that directly for review output before considering
        // FileOps or the expensive graph reconstruction fallback. Graph-first
        // imported changes may intentionally have no FileOps yet.
        if let Some((file_diffs, stats)) = Self::build_git_import_file_diffs(&change) {
            let (file_diffs, stats) = self.filter_file_diffs(file_diffs, stats);
            return self.print_change_file_diffs(&change, &hash, config, file_diffs, stats);
        }

        // Check if change has semantic layer (file_ops)
        if change.has_file_ops() {
            // Use the semantic layer for human-readable diff
            return self.show_change_diff_from_file_ops(repo, &change, &hash, config);
        }

        // Fallback: compute diff from content (legacy path)
        self.show_change_diff_computed(repo, &change, &hash, config)
    }

    /// Show diff using the semantic layer (FileOps).
    ///
    /// This is the preferred path - it displays line-level and token-level
    /// changes directly from the stored CRDT operations, without recomputing.
    ///
    /// Building the `Vec<FileDiff>` is delegated to the reusable
    /// [`change_file_diffs`] builder; this method applies the positional file
    /// filter and prints, preserving `atomic diff -c <hash>` output exactly.
    fn show_change_diff_from_file_ops(
        &self,
        repo: &Repository,
        change: &Change,
        change_hash: &Hash,
        config: &DiffOutputConfig,
    ) -> CliResult<()> {
        let (file_diffs, stats) = change_file_diffs(repo, change, change_hash, config)?;
        let (file_diffs, stats) = self.filter_file_diffs(file_diffs, stats);
        self.print_change_file_diffs(change, change_hash, config, file_diffs, stats)
    }

    fn print_change_file_diffs(
        &self,
        change: &Change,
        change_hash: &Hash,
        config: &DiffOutputConfig,
        file_diffs: Vec<FileDiff>,
        stats: DiffStats,
    ) -> CliResult<()> {
        if file_diffs.is_empty() {
            self.print_no_changes();
            return Ok(());
        }

        if config.format == DiffFormat::Unified {
            self.print_change_header(change, change_hash, config);
        }

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

    fn is_git_import_change(change: &Change) -> bool {
        change
            .unhashed
            .as_ref()
            .and_then(|value| value.get("git"))
            .is_some()
    }

    pub(super) fn build_git_import_file_diffs(
        change: &Change,
    ) -> Option<(Vec<FileDiff>, DiffStats)> {
        let diff_files_value = change
            .unhashed
            .as_ref()?
            .get("git")?
            .get("diff_lines")?
            .clone();

        let diff_files: Vec<GitMetadataDiffFile> = serde_json::from_value(diff_files_value).ok()?;
        let mut file_diffs = Vec::new();
        let mut stats = DiffStats::new();

        for entry in diff_files {
            let mut insertions = 0usize;
            let mut deletions = 0usize;
            let mut hunk_lines = Vec::new();
            let mut old_start = None;
            let mut new_start = None;

            for line in entry.lines {
                let content = String::from_utf8_lossy(&line.content)
                    .trim_end_matches('\n')
                    .to_string();
                match line.origin {
                    '+' => {
                        let new_num = line.new_lineno.unwrap_or((insertions + 1) as u32) as usize;
                        if new_start.is_none() {
                            new_start = Some(new_num);
                        }
                        hunk_lines.push(HunkLine::added(content, new_num));
                        insertions += 1;
                    }
                    '-' => {
                        let old_num = line.old_lineno.unwrap_or((deletions + 1) as u32) as usize;
                        if old_start.is_none() {
                            old_start = Some(old_num);
                        }
                        hunk_lines.push(HunkLine::removed(content, old_num));
                        deletions += 1;
                    }
                    _ => {}
                }
            }

            if hunk_lines.is_empty() {
                continue;
            }

            let status = match (insertions > 0, deletions > 0) {
                (true, false) => FileChangeStatus::Added,
                (false, true) => FileChangeStatus::Deleted,
                _ => FileChangeStatus::Modified,
            };

            let mut file_diff = match status {
                FileChangeStatus::Added => FileDiff::added(&entry.path),
                FileChangeStatus::Deleted => FileDiff::deleted(&entry.path),
                _ => FileDiff::modified(&entry.path),
            };

            let mut hunk = DiffHunk::new(
                old_start.unwrap_or(1),
                deletions,
                new_start.unwrap_or(1),
                insertions,
            );
            hunk.lines = hunk_lines;
            file_diff.add_hunk(hunk);
            file_diff.stats = match status {
                FileChangeStatus::Added => FileDiffStats::added(&entry.path, insertions),
                FileChangeStatus::Deleted => FileDiffStats::deleted(&entry.path, deletions),
                _ => FileDiffStats::modified(&entry.path, insertions, deletions),
            };

            stats.add_file(file_diff.stats.clone());
            file_diffs.push(file_diff);
        }

        Some((file_diffs, stats))
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
            let rm_orig_idx = rm_entries[*ri].0;
            let add_orig_idx = add_entries[*ai].0;

            // Only pull an add forward, never backward — the add must come
            // after the remove in the original order, or "pulling it
            // forward" would instead duplicate a line already emitted at
            // its earlier original position.
            //
            // Intervening Added lines between the remove and its matched add
            // are allowed: pairing readability (showing what a buried
            // modification actually changed) matters more here than
            // preserving their exact relative display position, which is
            // the whole point of this heuristic — see the "buried
            // modification" and "pairing at scale" cases this exists for.
            if add_orig_idx <= rm_orig_idx {
                continue;
            }

            matched_rm.insert(*ri);
            matched_add.insert(*ai);
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

        // Get all files modified by this change, honoring the positional
        // file filter (`diff --change <hash> <file>`)
        let modified_files: Vec<_> = get_files_in_change(change)
            .into_iter()
            .filter(|path| self.file_matches_filter(path))
            .collect();

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

/// Build the real per-file unified diff for a recorded change WITHOUT printing.
///
/// This is the computation `atomic diff -c <hash>` runs, factored out so other
/// surfaces (e.g. the triage report) can embed actual code hunks instead of
/// re-implementing diffing. It returns every changed file's [`FileDiff`] plus
/// the aggregate [`DiffStats`], using the semantic layer (FileOps) with true
/// hunk offsets and before-content context, or Git's captured `+/-` lines for
/// git-imported changes.
///
/// It applies NO positional file filter (it has no `Diff` instance); the
/// `atomic diff` path applies `filter_file_diffs` afterward, which yields the
/// same result as the previous in-loop filtering.
pub(crate) fn change_file_diffs(
    repo: &Repository,
    change: &Change,
    change_hash: &Hash,
    config: &DiffOutputConfig,
) -> CliResult<(Vec<FileDiff>, DiffStats)> {
    // Git-imported changes carry Git's captured +/- lines in unhashed metadata.
    if let Some((file_diffs, stats)) = Diff::build_git_import_file_diffs(change) {
        return Ok((file_diffs, stats));
    }

    let file_ops = change.file_ops();

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

        // Pass 1: flatten line operations into numbered changed lines.
        let mut insertions = 0usize;
        let mut deletions = 0usize;
        let mut new_line_num = 1usize;
        let mut old_line_num = 1usize;
        let mut net: isize = 0;
        let mut changed: Vec<NumberedLine> = Vec::new();

        for line_op in line_ops {
            match line_op.operation() {
                BranchOp::Insert { content, .. } => {
                    let line_content = Diff::reconstruct_line_from_leaf_ops(content);
                    let line_num = line_op.new_line_num().unwrap_or(new_line_num);
                    changed.push(NumberedLine::added(line_content, line_num, net));
                    new_line_num = line_num + 1;
                    insertions += 1;
                    net += 1;
                }
                BranchOp::Delete { content, .. } => {
                    let line_content = if content.is_empty() {
                        String::from("<deleted line>")
                    } else {
                        Diff::reconstruct_line_from_leaf_ops(content)
                    };
                    let line_num = line_op.old_line_num().unwrap_or(old_line_num);
                    changed.push(NumberedLine::removed(line_content, line_num, net));
                    old_line_num = line_num + 1;
                    deletions += 1;
                    net -= 1;
                }
                BranchOp::Modify {
                    old_content,
                    new_content,
                    ..
                } => {
                    let old_line_content = if old_content.is_empty() {
                        String::from("<modified line>")
                    } else {
                        Diff::reconstruct_line_from_leaf_ops(old_content)
                    };
                    let new_line_content = Diff::reconstruct_line_from_leaf_ops(new_content);

                    let old_ln = line_op.old_line_num().unwrap_or(old_line_num);
                    let new_ln = line_op.new_line_num().unwrap_or(new_line_num);

                    changed.push(NumberedLine::removed(old_line_content, old_ln, net));
                    net -= 1;
                    changed.push(NumberedLine::added(new_line_content, new_ln, net));
                    net += 1;

                    old_line_num = old_ln + 1;
                    new_line_num = new_ln + 1;
                    deletions += 1;
                    insertions += 1;
                }
                BranchOp::Restore { .. } => {
                    let line_num = line_op.new_line_num().unwrap_or(new_line_num);
                    changed.push(NumberedLine::added(
                        String::from("<restored line>"),
                        line_num,
                        net,
                    ));
                    new_line_num = line_num + 1;
                    insertions += 1;
                    net += 1;
                }
                BranchOp::Reparent { .. } => {
                    // Position-only change — no visible delta in unified output.
                }
            }
        }

        // Fetch the file's recorded before-content so hunks can be padded with
        // context lines. Only needed for unified output with a non-zero
        // --context; failures degrade to zero context.
        let before_lines: Option<Vec<Vec<u8>>> =
            if config.format == DiffFormat::Unified && config.context_lines > 0 {
                match repo.get_file_content_before_change(file_path, change_hash) {
                    Ok(Some(content)) => {
                        Some(content.split(|&b| b == b'\n').map(|l| l.to_vec()).collect())
                    }
                    _ => None,
                }
            } else {
                None
            };

        // Pass 2: group changed lines into hunks at their true file offsets.
        let mut hunks =
            Diff::hunks_from_changed_lines(&changed, before_lines.as_deref(), config.context_lines);

        for hunk in &mut hunks {
            // Re-pair Delete+Insert lines by content similarity (skipped for
            // git-imports, which carry Git's authoritative line ordering).
            if !Diff::is_git_import_change(change) {
                hunk.lines = Diff::repair_diff_lines(std::mem::take(&mut hunk.lines));
            }
        }

        for hunk in hunks {
            file_diff.add_hunk(hunk);
        }

        file_diff.stats = match change_status {
            FileChangeStatus::Added => FileDiffStats::added(file_path, insertions),
            FileChangeStatus::Deleted => FileDiffStats::deleted(file_path, deletions),
            _ => FileDiffStats::modified(file_path, insertions, deletions),
        };

        stats.add_file(file_diff.stats.clone());
        file_diffs.push(file_diff);
    }

    Ok((file_diffs, stats))
}
