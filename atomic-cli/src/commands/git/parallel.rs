//! Parallel git import pipeline using rayon for concurrent commit parsing.
//!
//! This module provides [`ParallelImporter`], which distributes the expensive
//! git reading and diffing work across all available CPU cores using rayon's
//! work-stealing thread pool.
//!
//! # Architecture
//!
//! The import pipeline has three phases:
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────────────────┐
//! │  Phase 1: PARALLEL GIT PARSE  (rayon - embarrassingly parallel)          │
//! │                                                                          │
//! │  ┌──────────────┐ ┌──────────────┐ ┌──────────────┐                      │
//! │  │  Thread 1    │ │  Thread 2    │ │  Thread N    │                      │
//! │  │              │ │              │ │              │                      │
//! │  │  git show    │ │  git show    │ │  git show    │                      │
//! │  │  diff parent │ │  diff parent │ │  diff parent │                      │
//! │  │  chunk hash  │ │  chunk hash  │ │  chunk hash  │                      │
//! │  │  metadata    │ │  metadata    │ │  metadata    │                      │
//! │  └──────┬───────┘ └──────┬───────┘ └──────┬───────┘                      │
//! │         │                │                │                              │
//! │         └────────────────┼────────────────┘                              │
//! │                          ▼                                               │
//! │              Vec<ParsedCommit>                                           │
//! ├──────────────────────────────────────────────────────────────────────────┤
//! │  Phase 2: SEQUENTIAL WRITE  (single-threaded, hash chaining)             │
//! │                                                                          │
//! │  For each commit in topological order:                                   │
//! │    - Compute Atomic hash (depends on previous)                           │
//! │    - Globalize positions → graph vertices                                │
//! │    - Write change to RedbChangeStore                                     │
//! │    - Apply to graph (GRAPH, TREE, INODES tables)                         │
//! │    - Update view sequence                                                │
//! ├──────────────────────────────────────────────────────────────────────────┤
//! │  Phase 3: FINALIZE  (verification)                                       │
//! │                                                                          │
//! │  - Verify change count matches                                           │
//! │  - Report statistics                                                     │
//! └──────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Performance
//!
//! The key insight is that git reading and diffing is **embarrassingly parallel**
//! — each commit can be parsed independently. The only sequential part is
//! Phase 2 (writing), which maintains the Merkle hash chain.
//!
//! For a 5,000 commit repository:
//! - Phase 1 (parallel parse): ~30s on 8 cores (vs ~4min sequential)
//! - Phase 2 (sequential write): ~5s
//! - Total: ~35s vs ~5min with serial approach

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, TimeZone, Utc};
use git2::{
    Delta, Diff, DiffFindOptions, DiffOptions, ObjectType, Oid, Repository as GitRepository, Tree,
};
use rayon::prelude::*;

use atomic_core::change::{
    Atom, Author, Change, ChangeHeader, EdgeUpdate, GraphOp, Insertion, NewEdge,
};
use atomic_core::change::{Encoding, Local};
use atomic_core::record::workflow::extract_filename;
use atomic_core::record::workflow::graph_op::BuiltHunk;
use atomic_core::record::workflow::GitDiffLine;
use atomic_core::record::workflow::RecordedFile;
use atomic_core::types::{ChangePosition, EdgeFlags, GraphNode, Hash as ContentHash, Position};
use atomic_repository::Repository;

use crate::error::{CliError, CliResult};
use crate::output::{print_info, print_warning};

// ═══════════════════════════════════════════════════════════════════════════
// Data Structures
// ═══════════════════════════════════════════════════════════════════════════

/// Statistics from the import process.
#[derive(Debug, Default, Clone)]
pub struct ImportStats {
    /// Number of commits found in git.
    pub commits_found: usize,
    /// Number of commits successfully parsed in Phase 1.
    pub commits_parsed: usize,
    /// Number of changes written in Phase 2.
    pub changes_written: usize,
    /// Number of empty commits (no file changes).
    pub empty_commits: usize,
    /// Number of merge commits with duplicate content.
    pub merge_commits: usize,
    /// Time spent in Phase 1 (parsing).
    pub phase1_duration: std::time::Duration,
    /// Time spent in Phase 2 (writing).
    pub phase2_duration: std::time::Duration,
    /// Files processed across all commits.
    pub files_processed: usize,
}

/// A parsed git commit ready for Phase 2 processing.
#[derive(Debug, Clone)]
pub struct ParsedCommit {
    /// Git commit SHA.
    pub git_sha: String,
    /// Short SHA for display.
    pub short_sha: String,
    /// Commit metadata.
    pub metadata: CommitMetadata,
    /// Files changed in this commit.
    pub files: Vec<ParsedFile>,
    /// Index of parent commit in the commits array (None for root).
    pub parent_index: Option<usize>,
    /// Whether this is a merge commit.
    pub is_merge: bool,
    /// Whether git reported 0 files changed.
    pub is_empty: bool,
}

/// Metadata extracted from a git commit.
#[derive(Debug, Clone)]
pub struct CommitMetadata {
    /// Author name.
    pub author_name: String,
    /// Author email (if available).
    pub author_email: Option<String>,
    /// Commit timestamp.
    pub timestamp: DateTime<Utc>,
    /// Commit message (first line).
    pub message: String,
    /// Commit description (remaining lines).
    pub description: Option<String>,
}

/// A file changed in a commit.
#[derive(Debug, Clone)]
pub struct ParsedFile {
    /// Relative path in the repository.
    pub path: String,
    /// Type of change.
    pub operation: FileOperation,
    /// New content (for added/modified files).
    pub new_content: Option<Vec<u8>>,
    /// Old content at the parent commit (for modified/deleted files).
    pub old_content: Option<Vec<u8>>,
    /// Git diff lines for this file (populated in Phase 1).
    ///
    /// When `Some`, Phase 2 builds BranchOps directly from these lines
    /// using git's own diff algorithm, rather than re-diffing with ours.
    /// This guarantees that `atomic diff -c` output matches `git diff`.
    pub diff_lines: Option<Vec<GitDiffLine>>,
    /// Old path (for renames).
    pub old_path: Option<String>,
}

/// Type of file operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOperation {
    /// File was added.
    Added,
    /// File was modified.
    Modified,
    /// File was deleted.
    Deleted,
    /// File was renamed (old_path -> path).
    Renamed,
    /// File was copied.
    Copied,
}

/// Options for the parallel importer.
#[derive(Debug, Clone)]
pub struct ParallelImportOptions {
    /// Skip commits that are already imported (by SHA).
    pub incremental: bool,
    /// Set of already-imported git SHAs (for incremental mode).
    pub imported_shas: HashSet<String>,
    /// Repository name (from remote URL or directory).
    pub repo_name: String,
    /// Import-time ignore patterns, usually from the detected `.atomicignore`
    /// template. These are applied before graph construction so generated
    /// build outputs never enter the imported history.
    pub ignored_path_patterns: Vec<String>,
}

impl Default for ParallelImportOptions {
    fn default() -> Self {
        Self {
            incremental: false,
            imported_shas: HashSet::new(),
            repo_name: "unknown".to_string(),
            ignored_path_patterns: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
struct ImportIgnoreMatcher {
    patterns: Vec<String>,
}

impl ImportIgnoreMatcher {
    fn new(patterns: Vec<String>) -> Self {
        let patterns = patterns
            .into_iter()
            .map(|pattern| pattern.trim().replace('\\', "/"))
            .filter(|pattern| !pattern.is_empty() && !pattern.starts_with('#'))
            .collect();
        Self { patterns }
    }

    fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    fn matches(&self, path: &str) -> bool {
        let normalized = path.trim_start_matches('/').replace('\\', "/");
        let basename = Path::new(&normalized)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&normalized);

        self.patterns.iter().any(|pattern| {
            let pattern = pattern.trim_start_matches('/');
            if let Some(dir) = pattern.strip_suffix('/') {
                return normalized == dir
                    || normalized.starts_with(&format!("{dir}/"))
                    || normalized.contains(&format!("/{dir}/"));
            }

            if let Some(suffix) = pattern.strip_prefix("**/*") {
                return normalized.ends_with(suffix);
            }

            if let Some(suffix) = pattern.strip_prefix('*') {
                return basename.ends_with(suffix);
            }

            normalized == pattern || basename == pattern
        })
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// ParallelImporter
// ═══════════════════════════════════════════════════════════════════════════

/// Parallel git importer using the three-phase architecture.
///
/// Note: We store the path to the git repo rather than a reference because
/// git2::Repository is not Sync. Each rayon thread opens its own repo instance.
pub struct ParallelImporter {
    git_repo_path: PathBuf,
    options: ParallelImportOptions,
    ignore_matcher: ImportIgnoreMatcher,
}

fn is_generated_diff_skip_path(path: &str) -> bool {
    let name = Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path);

    name.ends_with(".lock")
        || name.ends_with(".sum")
        || matches!(name, "package-lock.json" | "yarn.lock" | "pnpm-lock.yaml")
}

fn count_line_units(content: &[u8]) -> usize {
    if content.is_empty() {
        0
    } else {
        content.split_inclusive(|&b| b == b'\n').count()
    }
}

#[derive(Clone, Debug)]
struct ImportLine {
    change: ContentHash,
    start: ChangePosition,
    end: ChangePosition,
    incoming_by: ContentHash,
    content: Vec<u8>,
}

impl ImportLine {
    fn node(&self) -> GraphNode<Option<ContentHash>> {
        GraphNode {
            change: Some(self.change),
            start: self.start,
            end: self.end,
        }
    }

    fn start_pos(&self) -> Position<Option<ContentHash>> {
        Position {
            change: Some(self.change),
            pos: self.start,
        }
    }

    fn end_pos(&self) -> Position<Option<ContentHash>> {
        Position {
            change: Some(self.change),
            pos: self.end,
        }
    }
}

#[derive(Clone, Debug)]
struct ImportIndexedFile {
    inode_pos: Position<Option<ContentHash>>,
    lines: Vec<ImportLine>,
}

#[derive(Default)]
struct ImportLineIndex {
    files: HashMap<String, ImportIndexedFile>,
}

impl ImportLineIndex {
    fn update_from_added_change(&mut self, change_hash: ContentHash, change: &Change) {
        for graph_op in change.hunks() {
            match graph_op {
                GraphOp::FileAdd {
                    add_inode,
                    contents,
                    path,
                    ..
                } => {
                    let inode_pos = Position {
                        change: Some(change_hash),
                        pos: add_inode.start,
                    };
                    let mut lines = Vec::new();
                    if let Some(contents) = contents {
                        lines.push(ImportLine {
                            change: change_hash,
                            start: contents.start,
                            end: contents.end,
                            incoming_by: change_hash,
                            content: change.contents
                                [contents.start.as_usize()..contents.end.as_usize()]
                                .to_vec(),
                        });
                    }
                    self.files
                        .insert(path.clone(), ImportIndexedFile { inode_pos, lines });
                }
                GraphOp::Edit {
                    change: Atom::Insertion(insertion),
                    local,
                    ..
                } => {
                    if let Some(indexed) = self.files.get_mut(&local.path) {
                        indexed.lines.push(ImportLine {
                            change: change_hash,
                            start: insertion.start,
                            end: insertion.end,
                            incoming_by: change_hash,
                            content: change.contents
                                [insertion.start.as_usize()..insertion.end.as_usize()]
                                .to_vec(),
                        });
                    }
                }
                _ => {}
            }
        }
    }
}

#[derive(Debug)]
enum PendingLineIndexUpdate {
    Add {
        path: String,
        inode_pos: Position<Option<ContentHash>>,
        new_ranges: Vec<(ChangePosition, ChangePosition)>,
        new_lines: Vec<Vec<u8>>,
    },
    Modify {
        path: String,
        replacements: Vec<PendingLineReplacement>,
    },
    Rename {
        old_path: String,
        new_path: String,
    },
    Delete {
        path: String,
    },
}

#[derive(Debug)]
struct PendingLineReplacement {
    start_idx: usize,
    old_len: usize,
    new_ranges: Vec<(ChangePosition, ChangePosition)>,
    new_lines: Vec<Vec<u8>>,
    successor_incoming_by_current: bool,
}

#[derive(Debug)]
struct GitReplacementBlock {
    old_start: usize,
    old_len: usize,
    new_start: usize,
    new_lines: Vec<Vec<u8>>,
}

fn position_hashes(pos: &Position<Option<ContentHash>>) -> impl Iterator<Item = ContentHash> + '_ {
    pos.change
        .into_iter()
        .filter(|hash| *hash != ContentHash::NONE)
}

fn split_graph_first_lines(content: &[u8]) -> Vec<&[u8]> {
    if content.is_empty() {
        Vec::new()
    } else {
        content.split_inclusive(|&b| b == b'\n').collect()
    }
}

fn trace_git_import_enabled() -> bool {
    std::env::var_os("ATOMIC_TRACE_GIT_IMPORT").is_some()
}

fn trace_git_import(message: impl AsRef<str>) {
    if trace_git_import_enabled() {
        eprintln!("[git-import] {}", message.as_ref());
    }
}

struct SlowImportProgress {
    done: mpsc::Sender<()>,
    reported: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl SlowImportProgress {
    fn start(commit: String, summary: String) -> Self {
        let (done, rx) = mpsc::channel();
        let reported = Arc::new(AtomicBool::new(false));
        let reported_for_thread = Arc::clone(&reported);
        let handle = thread::spawn(move || {
            let started = Instant::now();
            if rx.recv_timeout(Duration::from_secs(5)).is_ok() {
                return;
            }

            reported_for_thread.store(true, Ordering::Relaxed);
            print_info(&format!(
                "Still importing {} after {}s; please be patient. {}",
                commit,
                started.elapsed().as_secs(),
                summary
            ));

            loop {
                if rx.recv_timeout(Duration::from_secs(15)).is_ok() {
                    break;
                }
                print_info(&format!(
                    "Still importing {} after {}s; graph/CRDT writes are still running.",
                    commit,
                    started.elapsed().as_secs()
                ));
            }
        });

        Self {
            done,
            reported,
            handle: Some(handle),
        }
    }

    fn finish(mut self) -> bool {
        let _ = self.done.send(());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        self.reported.load(Ordering::Relaxed)
    }
}

fn truncate_for_progress(input: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in input.chars().enumerate() {
        if idx >= max_chars {
            out.push_str("...");
            return out;
        }
        out.push(ch);
    }
    out
}

fn format_byte_count(bytes: usize) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;

    let bytes_f = bytes as f64;
    if bytes_f >= GIB {
        format!("{:.1}GiB", bytes_f / GIB)
    } else if bytes_f >= MIB {
        format!("{:.1}MiB", bytes_f / MIB)
    } else if bytes_f >= KIB {
        format!("{:.1}KiB", bytes_f / KIB)
    } else {
        format!("{}B", bytes)
    }
}

fn should_detect_renames(diff: &Diff) -> bool {
    let mut adds = 0usize;
    let mut deletes = 0usize;

    for delta in diff.deltas() {
        match delta.status() {
            Delta::Added => adds += 1,
            Delta::Deleted => deletes += 1,
            _ => {}
        }
    }

    if adds == 0 || deletes == 0 {
        return false;
    }

    // libgit2 rename detection is similarity matching over candidate
    // add/delete pairs. On root imports or vendored-tree rewrites this can
    // dominate the whole import before Atomic sees a single ParsedCommit.
    adds.saturating_mul(deletes) <= 250_000
}

fn record_generated_full_replace(
    path: &str,
    new_content: &[u8],
    old_content: &[u8],
    inode_pos: Option<(
        atomic_core::types::Inode,
        atomic_core::types::Position<atomic_core::types::NodeId>,
    )>,
) -> RecordedFile {
    let mut recorded = RecordedFile::new(path);
    recorded.set_kind(atomic_core::record::workflow::DetectionKind::Modified);
    if let Some((inode, pos)) = inode_pos {
        recorded.set_inode(inode);
        recorded.set_position(pos);
    }

    let old_line_count = count_line_units(old_content);
    recorded.set_old_line_count(old_line_count);
    recorded.set_encoding(Encoding::Utf8);

    // Force globalization onto the whole-file replacement path. For generated
    // lockfiles and checksums we do not need expensive line-granular CRDT ops
    // during git import; final content fidelity matters more than preserving
    // every tiny semantic edit inside machine-generated text.
    let deleted_lines: Vec<usize> = (0..=old_line_count).collect();
    let mut hunk = BuiltHunk::new_replace_with_lines(
        Local::new(path, 1),
        Some(Encoding::Utf8),
        deleted_lines,
        0,
        0,
        count_line_units(new_content),
    );
    hunk.content_start = Some(0);
    hunk.content_end = Some(new_content.len() as u64);
    recorded.add_hunk(hunk);
    recorded.set_content(new_content.to_vec());
    recorded.set_opaque_generated(true);
    recorded
}

fn record_git_diff_add_fast(
    path: &str,
    new_content: &[u8],
    diff_lines: &[GitDiffLine],
    kind: atomic_core::record::workflow::DetectionKind,
) -> Option<RecordedFile> {
    let encoding = Encoding::detect(new_content);
    if encoding == Encoding::Binary {
        return None;
    }

    let mut recorded = RecordedFile::new(path);
    recorded.set_kind(kind);
    recorded.set_encoding(encoding);
    recorded.add_hunk(BuiltHunk::new_edit(
        Local::new(path, 1),
        Some(encoding),
        0,
        new_content.len() as u64,
    ));
    recorded.set_content(new_content.to_vec());

    let (git_file_ops, git_stats) =
        atomic_core::record::workflow::build_crdt_ops_from_git_diff(path, diff_lines);
    recorded.set_crdt_ops(git_file_ops);
    recorded.set_crdt_stats(git_stats);
    Some(recorded)
}

fn build_linewise_crdt_ops_for_added_file(
    path: &str,
    content: &[u8],
    encoding: Encoding,
) -> (
    atomic_core::change::FileOps,
    atomic_core::record::workflow::CrdtBuildStats,
) {
    use atomic_core::change::LineOps;
    use atomic_core::crdt::{BranchId, BranchOp, TrunkId};
    use atomic_core::types::NodeId;

    let placeholder_change_id = NodeId::new(0);
    let trunk_id = TrunkId::new(placeholder_change_id, 0);
    let enc = if encoding == Encoding::Binary {
        None
    } else {
        Some(encoding)
    };
    let mut file_ops = atomic_core::change::FileOps::create(trunk_id, path.to_string(), enc);
    let mut stats = atomic_core::record::workflow::CrdtBuildStats::new();
    stats.files_added = 1;

    let mut prev_branch: Option<BranchId> = None;
    for (line_idx, _line) in content.split_inclusive(|&b| b == b'\n').enumerate() {
        let branch_id = BranchId::new(placeholder_change_id, line_idx as u32);
        let line_ops = LineOps::new_with_line_nums(
            branch_id,
            BranchOp::Insert {
                after: prev_branch,
                content: Vec::new(),
            },
            None,
            Some(line_idx + 1),
        );
        file_ops.add_line_op(line_ops);
        stats.lines_added += 1;
        prev_branch = Some(branch_id);
    }

    (file_ops, stats)
}

fn build_graph_first_file_ops_for_added_file(
    path: &str,
    content_lines: &[Vec<u8>],
    ranges: &[(ChangePosition, ChangePosition)],
    encoding: Encoding,
    file_idx: u32,
    next_branch_idx: &mut u32,
) -> atomic_core::change::FileOps {
    use atomic_core::change::LineOps;
    use atomic_core::crdt::{BranchId, BranchOp, LeafId, LeafOp, TrunkId};
    use atomic_core::types::NodeId;

    let placeholder_change_id = NodeId::ROOT;
    let trunk_id = TrunkId::new(placeholder_change_id, file_idx);
    let enc = if encoding == Encoding::Binary {
        None
    } else {
        Some(encoding)
    };
    let mut file_ops = atomic_core::change::FileOps::create(trunk_id, path.to_string(), enc);

    let mut prev_branch: Option<BranchId> = None;
    for (line_idx, line) in content_lines.iter().enumerate() {
        let branch_id = BranchId::new(placeholder_change_id, *next_branch_idx);
        *next_branch_idx += 1;
        let leaf_id = LeafId::new(placeholder_change_id, line_idx as u32);
        let trimmed = line.strip_suffix(b"\n").unwrap_or(line);
        let leaf_ops = if trimmed.is_empty() {
            Vec::new()
        } else {
            vec![LeafOp::Insert {
                after: None,
                kind: atomic_core::diff::TokenKind::Word,
                content: trimmed.to_vec(),
            }]
        };
        let _ = leaf_id;
        let mut line_ops = LineOps::new_with_line_nums(
            branch_id,
            BranchOp::Insert {
                after: prev_branch,
                content: leaf_ops,
            },
            None,
            Some(line_idx + 1),
        );
        if let Some((start, end)) = ranges.get(line_idx) {
            line_ops.set_content_range(*start, *end);
        }
        file_ops.add_line_op(line_ops);
        prev_branch = Some(branch_id);
    }

    file_ops
}

fn record_git_import_add_linewise(path: &str, new_content: &[u8]) -> Option<RecordedFile> {
    let encoding = Encoding::detect(new_content);
    if encoding == Encoding::Binary {
        return None;
    }

    let mut recorded = RecordedFile::new(path);
    recorded.set_kind(atomic_core::record::workflow::DetectionKind::Added);
    recorded.set_encoding(encoding);
    recorded.add_hunk(BuiltHunk::new_edit(
        Local::new(path, 1),
        Some(encoding),
        0,
        new_content.len() as u64,
    ));
    recorded.set_content(new_content.to_vec());

    let (file_ops, stats) = build_linewise_crdt_ops_for_added_file(path, new_content, encoding);
    recorded.set_crdt_ops(file_ops);
    recorded.set_crdt_stats(stats);
    Some(recorded)
}

fn build_graph_first_change(
    header: ChangeHeader,
    parsed: &ParsedCommit,
    line_index: &ImportLineIndex,
) -> Option<(Change, Vec<PendingLineIndexUpdate>, Vec<String>)> {
    if parsed.files.is_empty() {
        return None;
    }

    let mut contents = Vec::new();
    let mut hunks = Vec::new();
    let mut file_ops = Vec::new();
    let mut next_file_idx = 0u32;
    let mut next_branch_idx = 0u32;
    let mut dependencies = HashSet::new();
    let mut pending = Vec::new();
    let mut deleted_paths = Vec::new();

    for file in &parsed.files {
        match file.operation {
            FileOperation::Added | FileOperation::Copied => {
                let new_content = file.new_content.as_deref().unwrap_or(&[]);
                let encoding = Encoding::detect(new_content);
                if encoding == Encoding::Binary {
                    return None;
                }

                let filename = extract_filename(&file.path);
                let name_start = ChangePosition::new(contents.len() as u64);
                contents.extend_from_slice(filename.as_bytes());
                let name_end = ChangePosition::new(contents.len() as u64);
                let inode_pos = Position {
                    change: None,
                    pos: name_end,
                };
                let name_pos = Position {
                    change: None,
                    pos: name_end,
                };
                let parent_pos = Position {
                    change: Some(ContentHash::NONE),
                    pos: ChangePosition::ROOT,
                };

                let new_line_contents: Vec<Vec<u8>> = if is_generated_diff_skip_path(&file.path) {
                    if new_content.is_empty() {
                        Vec::new()
                    } else {
                        vec![new_content.to_vec()]
                    }
                } else {
                    split_graph_first_lines(new_content)
                        .into_iter()
                        .map(|line| line.to_vec())
                        .collect()
                };
                let mut new_ranges = Vec::new();
                for line in &new_line_contents {
                    let start = ChangePosition::new(contents.len() as u64);
                    contents.extend_from_slice(line);
                    let end = ChangePosition::new(contents.len() as u64);
                    new_ranges.push((start, end));
                }

                let first_content = new_ranges.first().map(|&(start, end)| Insertion {
                    predecessors: vec![inode_pos],
                    successors: vec![],
                    flag: EdgeFlags::BLOCK,
                    start,
                    end,
                    inode: inode_pos,
                });

                hunks.push(GraphOp::FileAdd {
                    add_name: Insertion {
                        predecessors: vec![parent_pos],
                        successors: vec![],
                        flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
                        start: name_start,
                        end: name_end,
                        inode: parent_pos,
                    },
                    add_inode: Insertion {
                        predecessors: vec![name_pos],
                        successors: vec![],
                        flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
                        start: name_end,
                        end: name_end,
                        inode: inode_pos,
                    },
                    contents: first_content,
                    path: file.path.clone(),
                    encoding: Some(encoding),
                });

                for (idx, &(start, end)) in new_ranges.iter().enumerate().skip(1) {
                    hunks.push(GraphOp::Edit {
                        change: Atom::Insertion(Insertion {
                            predecessors: vec![Position {
                                change: None,
                                pos: new_ranges[idx - 1].1,
                            }],
                            successors: vec![],
                            flag: EdgeFlags::BLOCK,
                            start,
                            end,
                            inode: inode_pos,
                        }),
                        local: Local::new(&file.path, (idx + 1) as u64),
                        encoding: Some(encoding),
                    });
                }

                file_ops.push(build_graph_first_file_ops_for_added_file(
                    &file.path,
                    &new_line_contents,
                    &new_ranges,
                    encoding,
                    next_file_idx,
                    &mut next_branch_idx,
                ));
                next_file_idx += 1;
                pending.push(PendingLineIndexUpdate::Add {
                    path: file.path.clone(),
                    inode_pos,
                    new_ranges,
                    new_lines: new_line_contents,
                });
                continue;
            }
            FileOperation::Renamed => {
                let old_path = file.old_path.as_deref()?;
                let indexed = line_index.files.get(old_path)?;
                let new_content = file.new_content.as_deref().unwrap_or(&[]);
                let encoding = Encoding::detect(new_content);
                if encoding == Encoding::Binary {
                    return None;
                }

                let new_filename = extract_filename(&file.path);
                let name_start = ChangePosition::new(contents.len() as u64);
                contents.extend_from_slice(new_filename.as_bytes());
                let name_end = ChangePosition::new(contents.len() as u64);

                let old_filename = extract_filename(old_path);
                let old_name_end = indexed.inode_pos.pos;
                let old_name_start = ChangePosition::new(
                    old_name_end.get().saturating_sub(old_filename.len() as u64),
                );
                let parent_pos = Position {
                    change: Some(ContentHash::NONE),
                    pos: ChangePosition::ROOT,
                };

                dependencies.extend(position_hashes(&indexed.inode_pos));

                let del = EdgeUpdate {
                    edges: vec![NewEdge {
                        previous: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
                        flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK | EdgeFlags::DELETED,
                        from: parent_pos,
                        to: GraphNode {
                            change: indexed.inode_pos.change,
                            start: old_name_start,
                            end: old_name_end,
                        },
                        introduced_by: indexed.inode_pos.change,
                    }],
                    inode: indexed.inode_pos,
                };

                hunks.push(GraphOp::FileMove {
                    del,
                    add: Insertion {
                        predecessors: vec![parent_pos],
                        successors: vec![indexed.inode_pos],
                        flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
                        start: name_start,
                        end: name_end,
                        inode: indexed.inode_pos,
                    },
                    path: file.path.clone(),
                });
                pending.push(PendingLineIndexUpdate::Rename {
                    old_path: old_path.to_string(),
                    new_path: file.path.clone(),
                });

                if let Some(diff_lines) = file.diff_lines.as_ref() {
                    let (ops, _) = atomic_core::record::workflow::build_crdt_ops_from_git_diff(
                        &file.path, diff_lines,
                    );
                    file_ops.push(ops);
                }

                let replacements = if is_generated_diff_skip_path(&file.path) {
                    vec![GitReplacementBlock {
                        old_start: if indexed.lines.is_empty() { 0 } else { 1 },
                        old_len: indexed.lines.len(),
                        new_start: 1,
                        new_lines: if new_content.is_empty() {
                            Vec::new()
                        } else {
                            vec![new_content.to_vec()]
                        },
                    }]
                } else {
                    current_state_replacements(indexed, new_content)
                };
                if !replacements.is_empty() {
                    let mut pending_replacements = Vec::new();
                    for replacement in replacements {
                        let start_idx = if replacement.old_len == 0 {
                            replacement.old_start
                        } else {
                            replacement.old_start.checked_sub(1)?
                        };
                        let end_idx = start_idx.checked_add(replacement.old_len)?;
                        if end_idx > indexed.lines.len() {
                            return None;
                        }

                        let predecessor = if start_idx == 0 {
                            indexed.inode_pos
                        } else {
                            indexed.lines[start_idx - 1].end_pos()
                        };
                        let successor = indexed.lines.get(end_idx).map(ImportLine::start_pos);

                        dependencies.extend(position_hashes(&predecessor));
                        if let Some(successor) = successor {
                            dependencies.extend(position_hashes(&successor));
                        }

                        let mut edge_update = EdgeUpdate {
                            edges: Vec::with_capacity(replacement.old_len),
                            inode: indexed.inode_pos,
                        };
                        for line_idx in start_idx..end_idx {
                            let from = if line_idx == 0 {
                                indexed.inode_pos
                            } else {
                                indexed.lines[line_idx - 1].end_pos()
                            };
                            let old_line = &indexed.lines[line_idx];
                            dependencies.insert(old_line.change);
                            dependencies.insert(old_line.incoming_by);
                            edge_update.edges.push(NewEdge {
                                previous: EdgeFlags::BLOCK,
                                flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
                                from,
                                to: old_line.node(),
                                introduced_by: Some(old_line.incoming_by),
                            });
                        }

                        let mut new_ranges = Vec::with_capacity(replacement.new_lines.len());
                        for new_line in &replacement.new_lines {
                            let start = ChangePosition::new(contents.len() as u64);
                            contents.extend_from_slice(new_line);
                            let end = ChangePosition::new(contents.len() as u64);
                            new_ranges.push((start, end));
                        }

                        if new_ranges.is_empty() {
                            hunks.push(GraphOp::Edit {
                                change: Atom::EdgeUpdate(edge_update),
                                local: Local::new(&file.path, replacement.new_start as u64),
                                encoding: Some(encoding),
                            });
                        } else if replacement.old_len == 0 {
                            let first = new_ranges[0];
                            let first_successors = if new_ranges.len() == 1 {
                                successor.into_iter().collect()
                            } else {
                                Vec::new()
                            };
                            hunks.push(GraphOp::Edit {
                                change: Atom::Insertion(Insertion {
                                    predecessors: vec![predecessor],
                                    successors: first_successors,
                                    flag: EdgeFlags::BLOCK,
                                    start: first.0,
                                    end: first.1,
                                    inode: indexed.inode_pos,
                                }),
                                local: Local::new(&file.path, replacement.new_start as u64),
                                encoding: Some(encoding),
                            });
                        } else {
                            let first = new_ranges[0];
                            let first_successors = if new_ranges.len() == 1 {
                                successor.into_iter().collect()
                            } else {
                                Vec::new()
                            };
                            hunks.push(GraphOp::Replacement {
                                change: edge_update,
                                replacement: Insertion {
                                    predecessors: vec![predecessor],
                                    successors: first_successors,
                                    flag: EdgeFlags::BLOCK,
                                    start: first.0,
                                    end: first.1,
                                    inode: indexed.inode_pos,
                                },
                                local: Local::new(&file.path, replacement.new_start as u64),
                                encoding: Some(encoding),
                            });
                        }

                        for (new_idx, &(start, end)) in new_ranges.iter().enumerate().skip(1) {
                            let predecessor = Position {
                                change: None,
                                pos: new_ranges[new_idx - 1].1,
                            };
                            let successors = if new_idx + 1 == new_ranges.len() {
                                successor.into_iter().collect()
                            } else {
                                Vec::new()
                            };
                            hunks.push(GraphOp::Edit {
                                change: Atom::Insertion(Insertion {
                                    predecessors: vec![predecessor],
                                    successors,
                                    flag: EdgeFlags::BLOCK,
                                    start,
                                    end,
                                    inode: indexed.inode_pos,
                                }),
                                local: Local::new(
                                    &file.path,
                                    (replacement.new_start + new_idx) as u64,
                                ),
                                encoding: Some(encoding),
                            });
                        }

                        pending_replacements.push(PendingLineReplacement {
                            start_idx,
                            old_len: replacement.old_len,
                            new_ranges,
                            new_lines: replacement.new_lines,
                            successor_incoming_by_current: successor.is_some(),
                        });
                    }

                    pending.push(PendingLineIndexUpdate::Modify {
                        path: file.path.clone(),
                        replacements: pending_replacements,
                    });
                }
                continue;
            }
            FileOperation::Deleted => {
                let indexed = line_index.files.get(&file.path)?;
                let mut edge_update = EdgeUpdate {
                    edges: Vec::with_capacity(indexed.lines.len()),
                    inode: indexed.inode_pos,
                };
                for line_idx in 0..indexed.lines.len() {
                    let from = if line_idx == 0 {
                        indexed.inode_pos
                    } else {
                        indexed.lines[line_idx - 1].end_pos()
                    };
                    let old_line = &indexed.lines[line_idx];
                    dependencies.insert(old_line.change);
                    dependencies.insert(old_line.incoming_by);
                    edge_update.edges.push(NewEdge {
                        previous: EdgeFlags::BLOCK,
                        flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
                        from,
                        to: old_line.node(),
                        introduced_by: Some(old_line.incoming_by),
                    });
                }
                hunks.push(GraphOp::Edit {
                    change: Atom::EdgeUpdate(edge_update),
                    local: Local::new(&file.path, 1),
                    encoding: file
                        .old_content
                        .as_deref()
                        .map(Encoding::detect)
                        .filter(|enc| *enc != Encoding::Binary)
                        .or(Some(Encoding::Utf8)),
                });
                pending.push(PendingLineIndexUpdate::Delete {
                    path: file.path.clone(),
                });
                deleted_paths.push(file.path.clone());
                if let Some(diff_lines) = file.diff_lines.as_ref() {
                    let (ops, _) = atomic_core::record::workflow::build_crdt_ops_from_git_diff(
                        &file.path, diff_lines,
                    );
                    file_ops.push(ops);
                }
                continue;
            }
            FileOperation::Modified => {}
        }

        let indexed = line_index.files.get(&file.path)?;
        let replacements = if is_generated_diff_skip_path(&file.path) {
            let new_content = file.new_content.as_deref()?;
            vec![GitReplacementBlock {
                old_start: if indexed.lines.is_empty() { 0 } else { 1 },
                old_len: indexed.lines.len(),
                new_start: 1,
                new_lines: if new_content.is_empty() {
                    Vec::new()
                } else {
                    vec![new_content.to_vec()]
                },
            }]
        } else if parsed.is_merge {
            let new_content = file.new_content.as_deref()?;
            current_state_replacements(indexed, new_content)
        } else {
            let diff_lines = file.diff_lines.as_ref()?;
            let (ops, _) =
                atomic_core::record::workflow::build_crdt_ops_from_git_diff(&file.path, diff_lines);
            file_ops.push(ops);
            parse_git_diff_replacements(diff_lines)?
        };
        if replacements.is_empty() {
            continue;
        }

        let encoding = file
            .new_content
            .as_deref()
            .map(Encoding::detect)
            .filter(|enc| *enc != Encoding::Binary)
            .or(Some(Encoding::Utf8));

        let mut pending_replacements = Vec::new();

        for replacement in replacements {
            let start_idx = if replacement.old_len == 0 {
                replacement.old_start
            } else {
                replacement.old_start.checked_sub(1)?
            };
            let end_idx = start_idx.checked_add(replacement.old_len)?;
            if end_idx > indexed.lines.len() {
                return None;
            }

            let predecessor = if start_idx == 0 {
                indexed.inode_pos
            } else {
                indexed.lines[start_idx - 1].end_pos()
            };
            let successor = indexed.lines.get(end_idx).map(ImportLine::start_pos);

            dependencies.extend(position_hashes(&predecessor));
            if let Some(successor) = successor {
                dependencies.extend(position_hashes(&successor));
            }

            let mut edge_update = EdgeUpdate {
                edges: Vec::with_capacity(replacement.old_len),
                inode: indexed.inode_pos,
            };

            for line_idx in start_idx..end_idx {
                let from = if line_idx == 0 {
                    indexed.inode_pos
                } else {
                    indexed.lines[line_idx - 1].end_pos()
                };
                let old_line = &indexed.lines[line_idx];
                dependencies.insert(old_line.change);
                dependencies.insert(old_line.incoming_by);
                edge_update.edges.push(NewEdge {
                    previous: EdgeFlags::BLOCK,
                    flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
                    from,
                    to: old_line.node(),
                    introduced_by: Some(old_line.incoming_by),
                });
            }

            let mut new_ranges = Vec::with_capacity(replacement.new_lines.len());
            for new_line in &replacement.new_lines {
                let start = ChangePosition::new(contents.len() as u64);
                contents.extend_from_slice(new_line);
                let end = ChangePosition::new(contents.len() as u64);
                new_ranges.push((start, end));
            }

            if new_ranges.is_empty() {
                hunks.push(GraphOp::Edit {
                    change: Atom::EdgeUpdate(edge_update),
                    local: Local::new(&file.path, replacement.new_start as u64),
                    encoding,
                });
            } else if replacement.old_len == 0 {
                let first = new_ranges[0];
                let first_successors = if new_ranges.len() == 1 {
                    successor.into_iter().collect()
                } else {
                    Vec::new()
                };
                hunks.push(GraphOp::Edit {
                    change: Atom::Insertion(Insertion {
                        predecessors: vec![predecessor],
                        successors: first_successors,
                        flag: EdgeFlags::BLOCK,
                        start: first.0,
                        end: first.1,
                        inode: indexed.inode_pos,
                    }),
                    local: Local::new(&file.path, replacement.new_start as u64),
                    encoding,
                });
            } else {
                let first = new_ranges[0];
                let first_successors = if new_ranges.len() == 1 {
                    successor.into_iter().collect()
                } else {
                    Vec::new()
                };
                hunks.push(GraphOp::Replacement {
                    change: edge_update,
                    replacement: Insertion {
                        predecessors: vec![predecessor],
                        successors: first_successors,
                        flag: EdgeFlags::BLOCK,
                        start: first.0,
                        end: first.1,
                        inode: indexed.inode_pos,
                    },
                    local: Local::new(&file.path, replacement.new_start as u64),
                    encoding,
                });
            }

            for (new_idx, &(start, end)) in new_ranges.iter().enumerate().skip(1) {
                let predecessor = Position {
                    change: None,
                    pos: new_ranges[new_idx - 1].1,
                };
                let successors = if new_idx + 1 == new_ranges.len() {
                    successor.into_iter().collect()
                } else {
                    Vec::new()
                };
                hunks.push(GraphOp::Edit {
                    change: Atom::Insertion(Insertion {
                        predecessors: vec![predecessor],
                        successors,
                        flag: EdgeFlags::BLOCK,
                        start,
                        end,
                        inode: indexed.inode_pos,
                    }),
                    local: Local::new(&file.path, (replacement.new_start + new_idx) as u64),
                    encoding,
                });
            }

            pending_replacements.push(PendingLineReplacement {
                start_idx,
                old_len: replacement.old_len,
                new_ranges,
                new_lines: replacement.new_lines,
                successor_incoming_by_current: successor.is_some(),
            });
        }

        pending.push(PendingLineIndexUpdate::Modify {
            path: file.path.clone(),
            replacements: pending_replacements,
        });
    }

    if hunks.is_empty() {
        return None;
    }

    let mut dependencies: Vec<ContentHash> = dependencies.into_iter().collect();
    dependencies.sort();
    dependencies.dedup();
    Some((
        Change::with_file_ops(header, hunks, file_ops, contents, dependencies),
        pending,
        deleted_paths,
    ))
}

fn current_state_replacements(
    indexed: &ImportIndexedFile,
    new_content: &[u8],
) -> Vec<GitReplacementBlock> {
    let old_lines = &indexed.lines;
    let new_lines: Vec<Vec<u8>> = split_graph_first_lines(new_content)
        .into_iter()
        .map(|line| line.to_vec())
        .collect();

    let mut prefix = 0usize;
    while prefix < old_lines.len()
        && prefix < new_lines.len()
        && old_lines[prefix].content == new_lines[prefix]
    {
        prefix += 1;
    }

    let mut suffix = 0usize;
    while suffix < old_lines.len().saturating_sub(prefix)
        && suffix < new_lines.len().saturating_sub(prefix)
        && old_lines[old_lines.len() - 1 - suffix].content
            == new_lines[new_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let old_mid_len = old_lines.len().saturating_sub(prefix + suffix);
    let new_mid_end = new_lines.len().saturating_sub(suffix);
    if old_mid_len == 0 && prefix == new_mid_end {
        return Vec::new();
    }

    vec![GitReplacementBlock {
        old_start: if old_mid_len == 0 { prefix } else { prefix + 1 },
        old_len: old_mid_len,
        new_start: prefix + 1,
        new_lines: new_lines[prefix..new_mid_end].to_vec(),
    }]
}

fn parse_git_diff_replacements(lines: &[GitDiffLine]) -> Option<Vec<GitReplacementBlock>> {
    let mut blocks = Vec::new();
    let mut old_start: Option<usize> = None;
    let mut new_start: Option<usize> = None;
    let mut old_len = 0usize;
    let mut new_lines = Vec::new();
    let mut old_cursor = 1usize;
    let mut new_cursor = 1usize;

    let flush = |blocks: &mut Vec<GitReplacementBlock>,
                 old_start: &mut Option<usize>,
                 new_start: &mut Option<usize>,
                 old_len: &mut usize,
                 new_lines: &mut Vec<Vec<u8>>|
     -> Option<()> {
        if *old_len > 0 || !new_lines.is_empty() {
            let old = old_start.take()?;
            let new = new_start.take().unwrap_or(old);
            blocks.push(GitReplacementBlock {
                old_start: old,
                old_len: *old_len,
                new_start: new,
                new_lines: std::mem::take(new_lines),
            });
            *old_len = 0;
        }
        Some(())
    };

    for line in lines {
        match line.origin {
            ' ' => {
                flush(
                    &mut blocks,
                    &mut old_start,
                    &mut new_start,
                    &mut old_len,
                    &mut new_lines,
                )?;
                old_cursor = line
                    .old_lineno
                    .map(|n| n as usize + 1)
                    .unwrap_or(old_cursor + 1);
                new_cursor = line
                    .new_lineno
                    .map(|n| n as usize + 1)
                    .unwrap_or(new_cursor + 1);
            }
            '-' => {
                if old_start.is_none() {
                    old_start = Some(line.old_lineno.map(|n| n as usize).unwrap_or(old_cursor));
                }
                old_cursor = line
                    .old_lineno
                    .map(|n| n as usize + 1)
                    .unwrap_or(old_cursor + 1);
                old_len += 1;
            }
            '+' => {
                if new_start.is_none() {
                    new_start = Some(line.new_lineno.map(|n| n as usize).unwrap_or(new_cursor));
                }
                if old_start.is_none() {
                    old_start = Some(old_cursor.saturating_sub(1));
                }
                new_cursor = line
                    .new_lineno
                    .map(|n| n as usize + 1)
                    .unwrap_or(new_cursor + 1);
                new_lines.push(line.content.clone());
            }
            _ => {}
        }
    }

    flush(
        &mut blocks,
        &mut old_start,
        &mut new_start,
        &mut old_len,
        &mut new_lines,
    )?;

    Some(blocks)
}

fn apply_line_index_updates(
    line_index: &mut ImportLineIndex,
    change_hash: ContentHash,
    pending: Vec<PendingLineIndexUpdate>,
) {
    for update in pending {
        match update {
            PendingLineIndexUpdate::Add {
                path,
                inode_pos,
                new_ranges,
                new_lines,
            } => {
                let inode_pos = Position {
                    change: Some(change_hash),
                    pos: inode_pos.pos,
                };
                let lines = new_ranges
                    .iter()
                    .zip(new_lines)
                    .map(|(&(start, end), content)| ImportLine {
                        change: change_hash,
                        start,
                        end,
                        incoming_by: change_hash,
                        content,
                    })
                    .collect();
                line_index
                    .files
                    .insert(path, ImportIndexedFile { inode_pos, lines });
            }
            PendingLineIndexUpdate::Modify { path, replacements } => {
                let Some(indexed) = line_index.files.get_mut(&path) else {
                    continue;
                };

                let mut offset: isize = 0;
                for replacement in replacements {
                    let adjusted_start = (replacement.start_idx as isize + offset).max(0) as usize;
                    let adjusted_end = adjusted_start
                        .saturating_add(replacement.old_len)
                        .min(indexed.lines.len());
                    let new_lines: Vec<ImportLine> = replacement
                        .new_ranges
                        .iter()
                        .zip(replacement.new_lines)
                        .map(|(&(start, end), content)| ImportLine {
                            change: change_hash,
                            start,
                            end,
                            incoming_by: change_hash,
                            content,
                        })
                        .collect();
                    indexed
                        .lines
                        .splice(adjusted_start..adjusted_end, new_lines);

                    if replacement.successor_incoming_by_current {
                        let successor_idx = adjusted_start + replacement.new_ranges.len();
                        if let Some(successor) = indexed.lines.get_mut(successor_idx) {
                            successor.incoming_by = change_hash;
                        }
                    }

                    offset += replacement.new_ranges.len() as isize - replacement.old_len as isize;
                }
            }
            PendingLineIndexUpdate::Rename { old_path, new_path } => {
                if let Some(indexed) = line_index.files.remove(&old_path) {
                    line_index.files.insert(new_path, indexed);
                }
            }
            PendingLineIndexUpdate::Delete { path } => {
                line_index.files.remove(&path);
            }
        }
    }
}

fn slow_import_commit_label(parsed: &ParsedCommit) -> String {
    let message = truncate_for_progress(&parsed.metadata.message.replace('\n', " "), 72);
    format!("{} \"{}\"", parsed.short_sha, message)
}

fn slow_import_record_summary(parsed: &ParsedCommit, recorded_files: &[RecordedFile]) -> String {
    let mut added = 0usize;
    let mut modified = 0usize;
    let mut deleted = 0usize;
    let mut renamed = 0usize;
    let mut copied = 0usize;
    let mut bytes = 0usize;

    for file in &parsed.files {
        match file.operation {
            FileOperation::Added => added += 1,
            FileOperation::Modified => modified += 1,
            FileOperation::Deleted => deleted += 1,
            FileOperation::Renamed => renamed += 1,
            FileOperation::Copied => copied += 1,
        }
        bytes += file.new_content.as_ref().map(|c| c.len()).unwrap_or(0);
        bytes += file.old_content.as_ref().map(|c| c.len()).unwrap_or(0);
    }

    let mut largest: Vec<(&str, usize)> = recorded_files
        .iter()
        .map(|rec| (rec.path(), rec.content().len()))
        .collect();
    largest.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let top_paths = largest
        .into_iter()
        .take(3)
        .map(|(path, size)| format!("{} ({})", path, format_byte_count(size)))
        .collect::<Vec<_>>();

    let top = if top_paths.is_empty() {
        "top records: none".to_string()
    } else {
        format!("top records: {}", top_paths.join(", "))
    };

    format!(
        "records={}, files={}, bytes={}, ops=+{}/~{}/-{} renames={} copies={}; {}",
        recorded_files.len(),
        parsed.files.len(),
        format_byte_count(bytes),
        added,
        modified,
        deleted,
        renamed,
        copied,
        top
    )
}

impl ParallelImporter {
    /// Create a new parallel importer.
    pub fn new(git_repo: &GitRepository, options: ParallelImportOptions) -> Self {
        let git_repo_path = git_repo
            .path()
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| git_repo.path().to_path_buf());
        let ignore_matcher = ImportIgnoreMatcher::new(options.ignored_path_patterns.clone());

        Self {
            git_repo_path,
            options,
            ignore_matcher,
        }
    }

    /// Open a new git repository instance (for thread-local use).
    fn open_git_repo(&self) -> CliResult<GitRepository> {
        GitRepository::open(&self.git_repo_path).map_err(|e| CliError::GitError {
            message: format!("Failed to open git repository: {}", e),
        })
    }

    fn path_ignored_for_import(&self, path: &str) -> bool {
        !self.ignore_matcher.is_empty() && self.ignore_matcher.matches(path)
    }

    fn file_ignored_for_import(&self, file: &ParsedFile) -> bool {
        match file.operation {
            FileOperation::Renamed => {
                let old_ignored = file
                    .old_path
                    .as_deref()
                    .is_some_and(|old_path| self.path_ignored_for_import(old_path));
                old_ignored && self.path_ignored_for_import(&file.path)
            }
            _ => self.path_ignored_for_import(&file.path),
        }
    }

    fn apply_import_ignores(&self, commit: &mut ParsedCommit) {
        if self.ignore_matcher.is_empty() || commit.files.is_empty() {
            return;
        }

        commit
            .files
            .retain(|file| !self.file_ignored_for_import(file));
        if commit.files.is_empty() {
            commit.is_empty = true;
        }
    }

    /// Import commits from a branch into an Atomic repository.
    ///
    /// Commits are processed in **batches** to keep memory bounded and show
    /// progress sooner. Each batch: parse in parallel → write sequentially.
    ///
    /// Batch sizes are tiered by total commit count:
    ///
    /// | Total commits | Batch size |
    /// |---------------|------------|
    /// | < 5,000       | 250        |
    /// | 5,000–9,999   | 500        |
    /// | 10,000–19,999 | 1,000      |
    /// | ≥ 20,000      | 2,500      |
    pub fn import_branch(
        &self,
        branch_name: &str,
        repo: &mut Repository,
    ) -> CliResult<ImportStats> {
        let mut stats = ImportStats::default();

        // Open git repo for this thread
        let git_repo = self.open_git_repo()?;

        // Collect commit OIDs in topological order
        let commit_oids = self.collect_commit_oids(&git_repo, branch_name)?;
        stats.commits_found = commit_oids.len();

        if commit_oids.is_empty() {
            return Ok(stats);
        }

        let total = commit_oids.len();
        let batch_size = Self::batch_size_for(total);

        print_info(&format!(
            "Importing {} commits in batches of {}...",
            total, batch_size
        ));

        let import_start = Instant::now();
        let mut commits_written = 0usize;
        let mut line_index = ImportLineIndex::default();

        for (batch_idx, chunk) in commit_oids.chunks(batch_size).enumerate() {
            let batch_start = batch_idx * batch_size;
            let batch_end = (batch_start + chunk.len()).min(total);

            print_info(&format!(
                "Batch {}: parsing commits {}-{} of {}...",
                batch_idx + 1,
                batch_start,
                batch_end,
                total
            ));

            // Phase 1: Parallel git parsing for this batch
            let parse_start = Instant::now();
            let parsed_commits = self.phase1_parse(chunk)?;
            let parse_elapsed = parse_start.elapsed();

            stats.phase1_duration += parse_elapsed;
            stats.commits_parsed += parsed_commits.len();

            if parsed_commits.is_empty() {
                continue;
            }

            // Phase 2: Sequential write for this batch
            let write_start = Instant::now();
            let write_stats = self.phase2_write(repo, &parsed_commits, &mut line_index)?;
            let write_elapsed = write_start.elapsed();

            stats.phase2_duration += write_elapsed;
            stats.changes_written += write_stats.changes_written;
            stats.empty_commits += write_stats.empty_commits;
            stats.merge_commits += write_stats.merge_commits;
            stats.files_processed += write_stats.files_processed;

            commits_written +=
                write_stats.changes_written + write_stats.empty_commits + write_stats.merge_commits;

            let total_elapsed = import_start.elapsed();
            let avg_ms = if commits_written > 0 {
                total_elapsed.as_secs_f64() * 1000.0 / commits_written as f64
            } else {
                0.0
            };

            print_info(&format!(
                "Batch {} done: parsed {:.1}s, wrote {:.1}s ({} changes, avg {:.1}ms/commit)",
                batch_idx + 1,
                parse_elapsed.as_secs_f64(),
                write_elapsed.as_secs_f64(),
                write_stats.changes_written,
                avg_ms,
            ));
        }

        let total_elapsed = import_start.elapsed();
        print_info(&format!(
            "Import complete: {} changes written in {:.1}s ({:.1}ms/commit avg)",
            stats.changes_written,
            total_elapsed.as_secs_f64(),
            if stats.changes_written > 0 {
                total_elapsed.as_secs_f64() * 1000.0 / stats.changes_written as f64
            } else {
                0.0
            },
        ));

        // Phase 3: Reconciliation — remove TREE entries for files that
        // don't exist on disk.
        //
        // Merge commits can implicitly delete files by not including them
        // from a second parent.  Our per-commit diff only sees explicit
        // deletions (FileOperation::Deleted), so files dropped during
        // merge resolution leave orphaned TREE entries.
        //
        // The reverse also happens: merge commits can implicitly ADD files
        // from a second parent without an explicit FileOperation::Added in
        // the first-parent diff.  These files exist on disk but have no
        // TREE entry.
        //
        // Fix: after all batches complete, reconcile TREE ↔ working copy
        // in both directions.
        let reconcile_start = Instant::now();
        let tracked = repo.list_tracked_files().unwrap_or_default();
        let repo_root = repo.root().to_path_buf();
        let mut orphan_count = 0usize;
        let mut phantom_count = 0usize;

        // Build a set of tracked paths for fast lookup
        let tracked_set: std::collections::HashSet<String> = tracked
            .iter()
            .map(|f| f.path.to_string_lossy().replace('\\', "/"))
            .collect();

        // Direction 1: remove TREE entries for files NOT on disk
        for file in &tracked {
            let abs = repo_root.join(&file.path);
            if !abs.exists() {
                let _ = repo.remove(&file.path, atomic_repository::TrackingOptions::forced());
                let _ = repo.del_file_index(&file.path.to_string_lossy());
                orphan_count += 1;
            }
        }

        // Direction 2: add TREE entries for files on disk NOT in TREE
        // Walk the working copy (respecting .atomicignore / .gitignore)
        // and track any untracked files.  Also populate FILE_INDEX.

        let mut new_index_entries: Vec<(String, i64, u32, u64, ContentHash)> = Vec::new();

        // Use status to find untracked files — it already handles
        // ignore rules and filesystem walking.
        if let Ok(status) = repo.status(atomic_repository::StatusOptions::default()) {
            for entry in status.untracked() {
                let path_str = entry.path().to_string_lossy().replace('\\', "/");
                if self.path_ignored_for_import(&path_str) {
                    continue;
                }

                // Add to tracking
                let _ = repo.add(&path_str, atomic_repository::TrackingOptions::default());

                // Collect FILE_INDEX entry
                let abs = repo_root.join(entry.path());
                if let Ok(metadata) = std::fs::metadata(&abs) {
                    use std::time::SystemTime;
                    let mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                    let duration = mtime
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap_or_default();
                    let secs = duration.as_secs() as i64;
                    let nanos = duration.subsec_nanos();
                    let size = metadata.len();
                    if let Ok(bytes) = std::fs::read(&abs) {
                        let hash = ContentHash::of(&bytes);
                        new_index_entries.push((path_str.clone(), secs, nanos, size, hash));
                    }
                }

                phantom_count += 1;
            }
        }

        if !new_index_entries.is_empty() {
            let _ = repo.update_file_index(&new_index_entries);
        }

        if orphan_count > 0 || phantom_count > 0 {
            print_info(&format!(
                "Reconciliation: removed {} orphaned, added {} untracked ({:.1}s)",
                orphan_count,
                phantom_count,
                reconcile_start.elapsed().as_secs_f64()
            ));
        }

        // Phase 4: Finalization (verification)
        self.phase3_finalize(&stats)?;

        Ok(stats)
    }

    /// Determine batch size based on total commit count.
    fn batch_size_for(total: usize) -> usize {
        match total {
            0..5_000 => 250,
            5_000..10_000 => 500,
            10_000..20_000 => 1_000,
            _ => 2_500,
        }
    }

    /// Collect commit OIDs in topological order (oldest first).
    fn collect_commit_oids(
        &self,
        git_repo: &GitRepository,
        branch_name: &str,
    ) -> CliResult<Vec<Oid>> {
        let reference = git_repo
            .find_branch(branch_name, git2::BranchType::Local)
            .map_err(|e| CliError::GitError {
                message: format!("Branch '{}' not found: {}", branch_name, e),
            })?;

        let target_oid = reference.get().target().ok_or_else(|| CliError::GitError {
            message: format!("Branch '{}' has no target commit", branch_name),
        })?;

        let mut revwalk = git_repo.revwalk().map_err(|e| CliError::GitError {
            message: format!("Failed to create revwalk: {}", e),
        })?;

        revwalk.push(target_oid).map_err(|e| CliError::GitError {
            message: format!("Failed to push target to revwalk: {}", e),
        })?;

        // Topological order, oldest first
        revwalk
            .set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)
            .map_err(|e| CliError::GitError {
                message: format!("Failed to set sorting: {}", e),
            })?;

        let mut oids = Vec::new();
        for oid_result in revwalk {
            let oid = oid_result.map_err(|e| CliError::GitError {
                message: format!("Revwalk error: {}", e),
            })?;

            // Skip already imported commits in incremental mode
            if self.options.incremental && self.options.imported_shas.contains(&oid.to_string()) {
                continue;
            }

            oids.push(oid);
        }

        Ok(oids)
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Phase 1: Parallel Git Parsing
    // ═══════════════════════════════════════════════════════════════════════

    /// Phase 1: Parse all commits in parallel using rayon.
    fn phase1_parse(&self, commit_oids: &[Oid]) -> CliResult<Vec<ParsedCommit>> {
        // Build a map from OID to index for parent lookups
        let oid_to_index: std::collections::HashMap<Oid, usize> = commit_oids
            .iter()
            .enumerate()
            .map(|(i, oid)| (*oid, i))
            .collect();

        // Progress counter for large repos
        let progress = Arc::new(AtomicUsize::new(0));
        let total = commit_oids.len();

        // Share the repo path for thread-local repo opening
        let repo_path = self.git_repo_path.clone();

        // Parse commits in parallel - each thread opens its own git repo
        let results: Vec<CliResult<ParsedCommit>> = commit_oids
            .par_iter()
            .enumerate()
            .map(|(idx, oid)| {
                // Progress reporting (every 100 commits)
                let count = progress.fetch_add(1, Ordering::Relaxed);
                if total > 100 && count.is_multiple_of(100) {
                    print_info(&format!("  Parsed {}/{} commits...", count, total));
                }

                // Open a thread-local git repo
                let git_repo = GitRepository::open(&repo_path).map_err(|e| CliError::GitError {
                    message: format!("Failed to open git repository: {}", e),
                })?;

                let mut commit = parse_commit(&git_repo, *oid, idx, &oid_to_index)?;
                self.apply_import_ignores(&mut commit);
                Ok(commit)
            })
            .collect();

        // Collect results, filtering out errors (with warnings)
        let mut parsed = Vec::with_capacity(results.len());
        for (idx, result) in results.into_iter().enumerate() {
            match result {
                Ok(commit) => parsed.push(commit),
                Err(e) => {
                    print_warning(&format!("Skipping commit {}: {}", idx, e));
                }
            }
        }

        // Sort by original index to restore topological order
        // (rayon may have processed them out of order)
        parsed.sort_by_key(|c| {
            commit_oids
                .iter()
                .position(|oid| oid.to_string() == c.git_sha)
                .unwrap_or(usize::MAX)
        });

        Ok(parsed)
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Phase 2: Sequential Write
    // ═══════════════════════════════════════════════════════════════════════

    /// Phase 2: Write changes sequentially with hash chaining.
    fn phase2_write(
        &self,
        repo: &mut Repository,
        commits: &[ParsedCommit],
        line_index: &mut ImportLineIndex,
    ) -> CliResult<WriteStats> {
        let mut stats = WriteStats::default();
        let total = commits.len();
        let phase2_start = Instant::now();
        let mut batch_start = Instant::now();

        for (idx, parsed) in commits.iter().enumerate() {
            // Progress reporting with per-batch timing
            if total > 100 && idx % 100 == 0 {
                if idx == 0 {
                    print_info(&format!("  Writing {}/{}...", idx, total));
                } else {
                    let batch_elapsed = batch_start.elapsed();
                    let total_elapsed = phase2_start.elapsed();
                    let avg_per_commit = total_elapsed.as_secs_f64() / idx as f64;
                    print_info(&format!(
                        "  Writing {}/{}... (last 100: {:.2}s, avg: {:.1}ms/commit)",
                        idx,
                        total,
                        batch_elapsed.as_secs_f64(),
                        avg_per_commit * 1000.0,
                    ));
                }
                batch_start = Instant::now();
            }

            // Write the change
            match self.write_commit(repo, parsed, line_index) {
                Ok(written) => {
                    if written {
                        stats.changes_written += 1;
                    } else if parsed.is_empty {
                        stats.empty_commits += 1;
                    } else {
                        stats.merge_commits += 1;
                    }
                    stats.files_processed += parsed.files.len();
                }
                Err(e) => {
                    print_warning(&format!("Failed to write {}: {}", parsed.short_sha, e));
                }
            }
        }

        // Populate the file index for all files written during this batch.
        // This lets `atomic status` compare file metadata (stat + content hash)
        // instead of reconstructing graph content for every file — reducing
        // post-import status from O(files × graph_traversal) to O(files × stat).
        use atomic_core::types::Hash;
        let repo_root = repo.root().to_path_buf();
        let mut index_entries: Vec<(String, i64, u32, u64, Hash)> = Vec::new();

        for parsed in commits {
            for file in &parsed.files {
                if file.operation == FileOperation::Deleted {
                    continue;
                }
                if self.path_ignored_for_import(&file.path) {
                    continue;
                }
                let abs_path = repo_root.join(&file.path);
                if let Ok(metadata) = std::fs::metadata(&abs_path) {
                    use std::time::SystemTime;
                    let mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                    let duration = mtime
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap_or_default();
                    let secs = duration.as_secs() as i64;
                    let nanos = duration.subsec_nanos();
                    let size = metadata.len();
                    let content_hash = std::fs::read(&abs_path)
                        .map(|bytes| Hash::of(&bytes))
                        .unwrap_or(Hash::ZERO);
                    // Normalize path to forward slashes for TREE compatibility
                    let normalized = file.path.replace('\\', "/");
                    index_entries.push((normalized, secs, nanos, size, content_hash));
                }
            }
        }

        if !index_entries.is_empty() {
            let _ = repo.update_file_index(&index_entries);
        }

        Ok(stats)
    }

    /// Write a single commit to the repository.
    fn write_commit(
        &self,
        repo: &mut Repository,
        parsed: &ParsedCommit,
        line_index: &mut ImportLineIndex,
    ) -> CliResult<bool> {
        use atomic_core::output::memory::Memory;
        use atomic_core::record::workflow::{
            record_added_file, record_deleted_file, record_modified_file, DetectedFile,
            RecordedFile, RecordingOptions,
        };

        let commit_start = std::time::Instant::now();

        // Build change header
        let mut header_builder = ChangeHeader::builder()
            .message(&parsed.metadata.message)
            .author(Author::new(
                &parsed.metadata.author_name,
                parsed.metadata.author_email.as_deref(),
            ))
            .timestamp(parsed.metadata.timestamp);

        if let Some(ref desc) = parsed.metadata.description {
            header_builder = header_builder.description(desc);
        }

        let header = header_builder.build();

        // Handle empty commits
        if parsed.is_empty {
            return self.write_empty_commit(repo, parsed, header);
        }

        if let Some((mut graph_change, pending_updates, graph_deleted_paths)) =
            build_graph_first_change(header.clone(), parsed, line_index)
        {
            graph_change.unhashed = Some(self.build_git_metadata(parsed, false, false));
            let progress = SlowImportProgress::start(
                slow_import_commit_label(parsed),
                format!(
                    "graph-first files={}, ops={}; CRDT metadata deferred",
                    parsed.files.len(),
                    graph_change.hunks().len()
                ),
            );
            let write_start = Instant::now();
            let write_result = repo.write_import_graph_change(
                graph_change,
                &graph_deleted_paths,
                Default::default(),
            );
            let write_ms = write_start.elapsed().as_millis();
            let progress_reported = progress.finish();
            let write_outcome = write_result.map_err(|e| CliError::Internal(e.into()))?;
            apply_line_index_updates(line_index, write_outcome.hash, pending_updates);
            if progress_reported || write_ms >= 5_000 {
                print_info(&format!(
                    "Imported {} in {:.1}s (graph-first assemble={}ms apply={}ms direct_graph={}ms direct_crdt={}ms commit={}ms)",
                    slow_import_commit_label(parsed),
                    write_ms as f64 / 1000.0,
                    write_outcome.timings.assemble_ms,
                    write_outcome.timings.apply_ms,
                    write_outcome.timings.direct_graph_ms,
                    write_outcome.timings.direct_crdt_ms,
                    write_outcome.timings.commit_ms
                ));
            }
            trace_git_import(format!(
                "write {} files={} graph_first=1 ops={} apply={}ms direct_graph={}ms direct_crdt={}ms commit={}ms total={}ms",
                parsed.short_sha,
                parsed.files.len(),
                write_outcome.insert.stats.atoms_processed,
                write_outcome.timings.apply_ms,
                write_outcome.timings.direct_graph_ms,
                write_outcome.timings.direct_crdt_ms,
                write_outcome.timings.commit_ms,
                commit_start.elapsed().as_millis()
            ));
            return Ok(true);
        }

        // Track new files so the pristine knows about them before we record.
        // Also collect deleted paths so we can remove them from TREE after insert.
        // Use batch operations to avoid a separate write txn + fsync per file.
        let mut added_paths: Vec<&str> = Vec::new();
        let mut deleted_paths: Vec<String> = Vec::new();
        for file in &parsed.files {
            if file.operation == FileOperation::Added || file.operation == FileOperation::Copied {
                added_paths.push(&file.path);
            }
            if file.operation == FileOperation::Deleted {
                deleted_paths.push(file.path.clone());
            }
        }
        let step = std::time::Instant::now();
        if !added_paths.is_empty() {
            let _ = repo.add_batch(&added_paths);
        }
        let add_batch_ms = step.elapsed().as_millis();

        // ── Fast path: build RecordedFiles directly from parsed content ──
        //
        // Instead of checking out the git tree to disk and running the
        // full record() pipeline (which does a filesystem scan + status),
        // we feed the already-parsed content into record_added_file /
        // record_modified_file via in-memory working copies.  This
        // eliminates all filesystem I/O for Phase 2.

        // Keep the record path on the default diff algorithm so the git2
        // line capture below and `git diff` CLI parity harness both describe
        // the same edit sequence.
        let core_options = RecordingOptions::new();
        let mut recorded_files: Vec<RecordedFile> = Vec::new();

        let record_start = std::time::Instant::now();
        let mut slow_files: Vec<(String, u128)> = Vec::new();

        for file in &parsed.files {
            let file_start = std::time::Instant::now();
            let memory_wc = Memory::new();

            match file.operation {
                FileOperation::Added | FileOperation::Copied => {
                    let content = match &file.new_content {
                        Some(c) => c.as_slice(),
                        None => continue,
                    };
                    if let Some(ref diff_lines) = file.diff_lines {
                        if let Some(rec) = record_git_diff_add_fast(
                            &file.path,
                            content,
                            diff_lines,
                            atomic_core::record::workflow::DetectionKind::Added,
                        ) {
                            recorded_files.push(rec);
                            continue;
                        }
                    }
                    if let Some(rec) = record_git_import_add_linewise(&file.path, content) {
                        recorded_files.push(rec);
                        continue;
                    }
                    memory_wc.add_file(&file.path, content);
                    let detected = DetectedFile::added(&file.path);
                    match record_added_file(&memory_wc, &detected, &core_options) {
                        Ok(rec) if !rec.is_empty() => recorded_files.push(rec),
                        _ => {}
                    }
                    let file_ms = file_start.elapsed().as_millis();
                    if file_ms > 100 {
                        slow_files.push((file.path.clone(), file_ms));
                    }
                }

                FileOperation::Renamed => {
                    // A rename is recorded as a GraphOp::FileMove, which:
                    //   1. Marks the old name edge DELETED in the graph
                    //   2. Inserts a new name edge pointing to the SAME inode
                    //
                    // We do NOT call repo.move_file() here — the TREE update
                    // happens later when insert_change processes the FileMove op.
                    let old_path = file.old_path.as_deref().unwrap_or(&file.path);
                    let parent_path = Path::new(&file.path)
                        .parent()
                        .and_then(|p| p.to_str())
                        .unwrap_or("");
                    let can_emit_move = parent_path.is_empty()
                        || repo
                            .get_inode_and_position(parent_path)
                            .ok()
                            .flatten()
                            .is_some();

                    // Look up the inode and position for the old path.
                    // If the old file isn't tracked at all, fall back to
                    // treating the rename as a plain addition.
                    match (can_emit_move, repo.get_inode_and_position(old_path)) {
                        (true, Ok(Some((inode, pos)))) => {
                            // Build the move RecordedFile. Globalization will
                            // produce a GraphOp::FileMove from this.
                            let mut move_rec = RecordedFile::new(&file.path);
                            move_rec.set_kind(atomic_core::record::workflow::DetectionKind::Moved);
                            move_rec.set_old_path(old_path.to_string());
                            move_rec.set_inode(inode);
                            move_rec.set_position(pos);
                            recorded_files.push(move_rec);

                            // If the content also changed during the rename,
                            // record the modification on the new path separately.
                            let new_content = match &file.new_content {
                                Some(c) => c.as_slice(),
                                None => &[],
                            };

                            // Use old content from Phase 1 (captured from git
                            // parent tree) to avoid an O(N) graph scan.
                            let old_content = file.old_content.as_deref().unwrap_or(&[]).to_vec();

                            if !new_content.is_empty() && old_content != new_content {
                                if is_generated_diff_skip_path(&file.path) {
                                    let rec = record_generated_full_replace(
                                        &file.path,
                                        new_content,
                                        &old_content,
                                        Some((inode, pos)),
                                    );
                                    recorded_files.push(rec);
                                } else {
                                    let memory_wc2 = Memory::new();
                                    memory_wc2.add_file(&file.path, new_content);

                                    let mut detected = DetectedFile::modified(&file.path);
                                    detected.inode = Some(inode);
                                    detected.position = Some(pos);

                                    match record_modified_file(
                                        &memory_wc2,
                                        &detected,
                                        &old_content,
                                        None, // no separate CRDT old content
                                        &core_options,
                                        None, // git-import path has no existing trunk binding here
                                        None, // git-import path overrides CRDT ops below
                                    ) {
                                        Ok(mut rec) if !rec.is_empty() => {
                                            if let Some(ref diff_lines) = file.diff_lines {
                                                use atomic_core::record::workflow::build_crdt_ops_from_git_diff;
                                                let (git_file_ops, _) =
                                                    build_crdt_ops_from_git_diff(
                                                        &file.path, diff_lines,
                                                    );
                                                rec.set_crdt_ops(git_file_ops);
                                            }
                                            recorded_files.push(rec);
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                        _ => {
                            // Fallback: if we cannot anchor the rename to an
                            // existing inode or the new parent is not tracked
                            // as a first-class directory inode, degrade it to
                            // "delete old path + add new path" so the imported
                            // history stays faithful and the final tree
                            // remains clean.
                            if old_path != file.path {
                                deleted_paths.push(old_path.to_string());
                            }

                            let content =
                                match file.new_content.as_deref().or(file.old_content.as_deref()) {
                                    Some(c) => c,
                                    None => continue,
                                };
                            memory_wc.add_file(&file.path, content);
                            let detected = DetectedFile::added(&file.path);
                            match record_added_file(&memory_wc, &detected, &core_options) {
                                Ok(rec) if !rec.is_empty() => recorded_files.push(rec),
                                _ => {}
                            }
                        }
                    }
                }

                FileOperation::Modified => {
                    let new_content = match &file.new_content {
                        Some(c) => c.as_slice(),
                        None => continue,
                    };
                    memory_wc.add_file(&file.path, new_content);

                    // Use old content from Phase 1 (captured from git parent
                    // tree) to avoid an O(N) graph scan per commit.
                    let old_content = file.old_content.as_deref().unwrap_or(&[]).to_vec();

                    // Look up inode + position for this file
                    let lookup_start = std::time::Instant::now();
                    let mut detected = DetectedFile::modified(&file.path);
                    if let Ok(Some((inode, pos))) = repo.get_inode_and_position(&file.path) {
                        detected.inode = Some(inode);
                        detected.position = Some(pos);
                    }
                    let lookup_ms = lookup_start.elapsed().as_millis();

                    if is_generated_diff_skip_path(&file.path) {
                        let inode_pos = detected.inode.zip(detected.position);
                        let rec = record_generated_full_replace(
                            &file.path,
                            new_content,
                            &old_content,
                            inode_pos,
                        );
                        recorded_files.push(rec);
                        let file_ms = file_start.elapsed().as_millis();
                        if file_ms > 100 {
                            slow_files.push((file.path.clone(), file_ms));
                        }
                        if lookup_ms > 10 {
                            log::debug!(
                                "write_commit {}: generated fast path {} lookup={}ms file={}ms",
                                parsed.short_sha,
                                file.path,
                                lookup_ms,
                                file_ms
                            );
                        }
                        continue;
                    }

                    let diff_start = std::time::Instant::now();
                    match record_modified_file(
                        &memory_wc,
                        &detected,
                        &old_content,
                        None,
                        &core_options,
                        None,
                        None,
                    ) {
                        Ok(mut rec) if !rec.is_empty() => {
                            // Override CRDT ops with git's exact diff lines when available.
                            // This guarantees `atomic diff -c` matches `git diff` line-for-line
                            // instead of re-running our Myers algorithm which may differ.
                            if let Some(ref diff_lines) = file.diff_lines {
                                use atomic_core::record::workflow::build_crdt_ops_from_git_diff;
                                let (git_file_ops, _) =
                                    build_crdt_ops_from_git_diff(&file.path, diff_lines);
                                rec.set_crdt_ops(git_file_ops);
                            }
                            recorded_files.push(rec);
                        }
                        _ => {}
                    }
                    let diff_ms = diff_start.elapsed().as_millis();
                    let file_ms = file_start.elapsed().as_millis();
                    if file_ms > 100 {
                        slow_files.push((
                            format!(
                                "{} (lookup={}ms diff={}ms old={}b new={}b)",
                                file.path,
                                lookup_ms,
                                diff_ms,
                                old_content.len(),
                                new_content.len()
                            ),
                            file_ms,
                        ));
                    }
                }

                FileOperation::Deleted => {
                    // Use old content from Phase 1 (captured from git parent
                    // tree) so the diff can show deleted lines — avoids O(N)
                    // graph scan.
                    let old_content = file.old_content.as_deref().unwrap_or(&[]).to_vec();

                    if !old_content.is_empty() {
                        // Record as a modification that removes all content:
                        // old_content = the file's current bytes, new_content = empty.
                        // This produces proper BranchOp::Delete entries with line content
                        // so that `atomic diff -c` can show what was deleted.
                        let del_wc = Memory::new();
                        // Empty new content — the file is being deleted.
                        del_wc.add_file(&file.path, b"");

                        if let Ok(Some((inode, pos))) = repo.get_inode_and_position(&file.path) {
                            let mut detected = DetectedFile::modified(&file.path);
                            detected.inode = Some(inode);
                            detected.position = Some(pos);
                            match record_modified_file(
                                &del_wc,
                                &detected,
                                &old_content,
                                None, // no separate CRDT old content
                                &core_options,
                                None, // git-import path has no existing trunk binding here
                                None, // git-import path overrides CRDT ops below
                            ) {
                                Ok(mut rec) if !rec.is_empty() => {
                                    // Override CRDT ops with git's exact diff lines
                                    // so `atomic diff -c` shows what git shows.
                                    if let Some(ref diff_lines) = file.diff_lines {
                                        use atomic_core::record::workflow::build_crdt_ops_from_git_diff;
                                        let (git_file_ops, _) =
                                            build_crdt_ops_from_git_diff(&file.path, diff_lines);
                                        rec.set_crdt_ops(git_file_ops);
                                    }
                                    recorded_files.push(rec);
                                }
                                _ => {
                                    // Fall back to simple delete if modified recording fails
                                    let mut det = DetectedFile::deleted(&file.path);
                                    if let Ok(Some((inode, pos))) =
                                        repo.get_inode_and_position(&file.path)
                                    {
                                        det.inode = Some(inode);
                                        det.position = Some(pos);
                                    }
                                    if let Ok(rec) = record_deleted_file(&det, &core_options) {
                                        if !rec.is_empty() {
                                            recorded_files.push(rec);
                                        }
                                    }
                                }
                            }
                        } else {
                            // No inode — try simple delete
                            let det = DetectedFile::deleted(&file.path);
                            if let Ok(rec) = record_deleted_file(&det, &core_options) {
                                if !rec.is_empty() {
                                    recorded_files.push(rec);
                                }
                            }
                        }
                    } else {
                        // Empty old content — just record a simple deletion
                        let mut det = DetectedFile::deleted(&file.path);
                        if let Ok(Some((inode, pos))) = repo.get_inode_and_position(&file.path) {
                            det.inode = Some(inode);
                            det.position = Some(pos);
                        }
                        if let Ok(rec) = record_deleted_file(&det, &core_options) {
                            if !rec.is_empty() {
                                recorded_files.push(rec);
                            }
                        }
                    }
                }
            }
        }

        let record_ms = record_start.elapsed().as_millis();
        for (path, ms) in &slow_files {
            log::warn!(
                "write_commit {}: SLOW file recording: {} took {}ms",
                parsed.short_sha,
                path,
                ms
            );
        }
        if record_ms > 200 {
            log::warn!(
                "write_commit {}: recording phase took {}ms ({} files, add_batch={}ms, {} slow files)",
                parsed.short_sha, record_ms, parsed.files.len(), add_batch_ms, slow_files.len()
            );
        }

        let metadata = self.build_git_metadata(parsed, false, recorded_files.is_empty());
        let progress = SlowImportProgress::start(
            slow_import_commit_label(parsed),
            slow_import_record_summary(parsed, &recorded_files),
        );
        let write_start = Instant::now();
        let write_outcome = repo
            .write_import_recorded(
                header,
                &recorded_files,
                metadata,
                &deleted_paths,
                Default::default(),
            )
            .map_err(|e| CliError::Internal(e.into()))?;
        let write_ms = write_start.elapsed().as_millis();
        let progress_reported = progress.finish();
        if !recorded_files.is_empty()
            && recorded_files.iter().all(|recorded| {
                matches!(
                    recorded.kind(),
                    Some(atomic_core::record::workflow::DetectionKind::Added)
                )
            })
        {
            match repo.load_change(&write_outcome.hash) {
                Ok(change) => line_index.update_from_added_change(write_outcome.hash, &change),
                Err(err) => trace_git_import(format!(
                    "graph-first {}: could not seed line index from {}: {}",
                    parsed.short_sha, write_outcome.hash, err
                )),
            }
        }
        if progress_reported || write_ms >= 5_000 {
            print_info(&format!(
                "Imported {} in {:.1}s (assemble={}ms apply={}ms direct_graph={}ms direct_crdt={}ms commit={}ms)",
                slow_import_commit_label(parsed),
                write_ms as f64 / 1000.0,
                write_outcome.timings.assemble_ms,
                write_outcome.timings.apply_ms,
                write_outcome.timings.direct_graph_ms,
                write_outcome.timings.direct_crdt_ms,
                write_outcome.timings.commit_ms
            ));
        }

        // Files deleted via record_modified_file (the "show diff lines" path)
        // produce GraphOp::Replacement, not GraphOp::FileDel, so insert_change
        // never removes their TREE entries.  Explicitly untrack them now so that
        // `atomic status` after import matches the git working copy.
        // Also remove from FILE_INDEX so status doesn't show them as deleted.
        // Batch-remove deleted files from TREE and FILE_INDEX in single write txns.
        if !deleted_paths.is_empty() {
            let cleanup_start = Instant::now();
            let del_refs: Vec<&str> = deleted_paths.iter().map(|s| s.as_str()).collect();
            let _ = repo.del_file_index_batch(&del_refs);
            let cleanup_ms = cleanup_start.elapsed().as_millis();
            trace_git_import(format!(
                "write {} files={} recorded={} add_batch={}ms record={}ms assemble={}ms save={}ms apply={}ms direct_graph={}ms direct_crdt={}ms commit={}ms cleanup={}ms writer_total={}ms total={}ms",
                parsed.short_sha,
                parsed.files.len(),
                recorded_files.len(),
                add_batch_ms,
                record_ms,
                write_outcome.timings.assemble_ms,
                write_outcome.timings.save_ms,
                write_outcome.timings.apply_ms,
                write_outcome.timings.direct_graph_ms,
                write_outcome.timings.direct_crdt_ms,
                write_outcome.timings.commit_ms,
                cleanup_ms,
                write_ms,
                commit_start.elapsed().as_millis()
            ));
        } else {
            trace_git_import(format!(
                "write {} files={} recorded={} add_batch={}ms record={}ms assemble={}ms save={}ms apply={}ms direct_graph={}ms direct_crdt={}ms commit={}ms cleanup=0ms writer_total={}ms total={}ms",
                parsed.short_sha,
                parsed.files.len(),
                recorded_files.len(),
                add_batch_ms,
                record_ms,
                write_outcome.timings.assemble_ms,
                write_outcome.timings.save_ms,
                write_outcome.timings.apply_ms,
                write_outcome.timings.direct_graph_ms,
                write_outcome.timings.direct_crdt_ms,
                write_outcome.timings.commit_ms,
                write_ms,
                commit_start.elapsed().as_millis()
            ));
        }

        Ok(true)
    }

    /// Write an empty commit (no file changes).
    fn write_empty_commit(
        &self,
        repo: &mut Repository,
        parsed: &ParsedCommit,
        header: ChangeHeader,
    ) -> CliResult<bool> {
        let commit_start = Instant::now();
        let metadata = self.build_git_metadata(parsed, true, false);
        let write_outcome = repo
            .write_import_recorded(header, &[], metadata, &[], Default::default())
            .map_err(|e| CliError::Internal(e.into()))?;

        trace_git_import(format!(
            "write {} files=0 recorded=0 add_batch=0ms record=0ms assemble={}ms save={}ms apply={}ms direct_graph={}ms direct_crdt={}ms commit={}ms cleanup=0ms total={}ms empty_commit=true",
            parsed.short_sha,
            write_outcome.timings.assemble_ms,
            write_outcome.timings.save_ms,
            write_outcome.timings.apply_ms,
            write_outcome.timings.direct_graph_ms,
            write_outcome.timings.direct_crdt_ms,
            write_outcome.timings.commit_ms,
            commit_start.elapsed().as_millis()
        ));

        Ok(true)
    }

    /// Build git metadata for the change's unhashed field.
    fn build_git_metadata(
        &self,
        parsed: &ParsedCommit,
        is_empty: bool,
        is_merge: bool,
    ) -> serde_json::Value {
        let mut git = serde_json::json!({
            "repository": self.options.repo_name,
            "sha": parsed.git_sha,
            "short_sha": parsed.short_sha,
        });

        let diff_files: Vec<serde_json::Value> = parsed
            .files
            .iter()
            .filter_map(|file| {
                file.diff_lines.as_ref().map(|lines| {
                    serde_json::json!({
                        "path": file.path,
                        "old_path": file.old_path,
                        "operation": match file.operation {
                            FileOperation::Added => "added",
                            FileOperation::Modified => "modified",
                            FileOperation::Deleted => "deleted",
                            FileOperation::Renamed => "renamed",
                            FileOperation::Copied => "copied",
                        },
                        "lines": lines,
                    })
                })
            })
            .collect();

        if !diff_files.is_empty() {
            git["diff_lines"] = serde_json::Value::Array(diff_files);
        }

        if is_empty {
            git["empty_commit"] = serde_json::json!(true);
        }
        if is_merge {
            git["empty_merge"] = serde_json::json!(true);
        }

        serde_json::json!({ "git": git })
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Phase 3: Finalization
    // ═══════════════════════════════════════════════════════════════════════

    /// Phase 3: Finalization and verification.
    fn phase3_finalize(&self, stats: &ImportStats) -> CliResult<()> {
        // Verify counts
        let expected = stats.commits_parsed;
        let actual = stats.changes_written + stats.empty_commits + stats.merge_commits;

        if actual != expected {
            print_warning(&format!(
                "Verification: {} commits parsed but {} changes created",
                expected, actual
            ));
        }

        Ok(())
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Helper Types
// ═══════════════════════════════════════════════════════════════════════════

/// Statistics from Phase 2 writing.
#[derive(Debug, Default)]
struct WriteStats {
    changes_written: usize,
    empty_commits: usize,
    merge_commits: usize,
    files_processed: usize,
}

// ═══════════════════════════════════════════════════════════════════════════
// Free Functions for Parallel Parsing
// ═══════════════════════════════════════════════════════════════════════════

/// Parse a single git commit (called in parallel from rayon threads).
///
/// This is a free function rather than a method because each rayon thread
/// opens its own git repository instance (git2::Repository is not Sync).
fn parse_commit(
    git_repo: &GitRepository,
    oid: Oid,
    _index: usize,
    oid_to_index: &std::collections::HashMap<Oid, usize>,
) -> CliResult<ParsedCommit> {
    let parse_start = Instant::now();
    let commit = git_repo.find_commit(oid).map_err(|e| CliError::GitError {
        message: format!("Failed to find commit {}: {}", oid, e),
    })?;

    let sha = oid.to_string();
    let short_sha = sha[..8.min(sha.len())].to_string();

    // Extract metadata
    let metadata = extract_commit_metadata(&commit)?;

    // Get parent index
    let parent_index = if commit.parent_count() > 0 {
        commit
            .parent_id(0)
            .ok()
            .and_then(|parent_oid| oid_to_index.get(&parent_oid).copied())
    } else {
        None
    };

    let is_merge = commit.parent_count() > 1;

    // Get trees for diff
    let tree = commit.tree().map_err(|e| CliError::GitError {
        message: format!("Failed to get tree: {}", e),
    })?;

    let parent_tree = if commit.parent_count() > 0 {
        Some(
            commit
                .parent(0)
                .map_err(|e| CliError::GitError {
                    message: format!("Failed to get parent: {}", e),
                })?
                .tree()
                .map_err(|e| CliError::GitError {
                    message: format!("Failed to get parent tree: {}", e),
                })?,
        )
    } else {
        None
    };

    // Use git's default diff algorithm here. Harness parity compares against
    // plain `git diff`, so the captured +/- lines need to reflect the same
    // default edit classification rather than `--patience`.
    let mut diff_opts = DiffOptions::new();
    diff_opts.include_untracked(false);

    let diff_start = Instant::now();
    let mut diff = git_repo
        .diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(&mut diff_opts))
        .map_err(|e| CliError::GitError {
            message: format!("Failed to compute diff: {}", e),
        })?;
    let diff_ms = diff_start.elapsed().as_millis();

    // Apply rename detection — mirrors what git CLI does after computing
    // the initial diff.  This correctly classifies renamed files as R deltas
    // so write_commit can produce GraphOp::FileMove instead of treating them
    // as plain modifications.
    //
    // We enable renames(true) only — NOT renames_from_rewrites(true).
    // renames_from_rewrites tells git2 to consider heavily-modified files
    // as potential rename *sources*, which causes false positives: when a
    // file is modified AND a new file is added with similar content, git2
    // converts the (Modified + Added) pair into a Renamed delta — even
    // though the original file still exists.  Example: modifying
    // src/export/markdown.rs while adding src/export/tests.rs (52%
    // similar) would be misclassified as a rename of markdown→tests,
    // orphaning markdown.rs from the TREE.
    let rename_start = Instant::now();
    let detected_renames = should_detect_renames(&diff);
    if detected_renames {
        let mut find_opts = DiffFindOptions::new();
        find_opts.renames(true);
        let _ = diff.find_similar(Some(&mut find_opts));
    } else {
        log::debug!(
            "parse_commit {}: skipping rename detection for large/add-only diff",
            short_sha
        );
    }
    let rename_ms = rename_start.elapsed().as_millis();

    // Parse files. Pure rename commits can show up as zero-stat directory
    // modifications in libgit2; when that happens, fall back to recursive
    // `git diff-tree -r -M` name-status output for per-file entries.
    let capture_diff_lines = parent_tree.is_some();
    let files_start = Instant::now();
    let mut files = parse_diff_files(
        git_repo,
        &diff,
        &tree,
        parent_tree.as_ref(),
        capture_diff_lines,
    )?;
    let mut parse_files_ms = files_start.elapsed().as_millis();
    if files.is_empty() {
        if let Some(ref pt) = parent_tree {
            let fallback_start = Instant::now();
            let fallback = parse_diff_files_via_git_cli(git_repo, oid, pt.id(), &tree, pt)?;
            parse_files_ms += fallback_start.elapsed().as_millis();
            if !fallback.is_empty() {
                files = fallback;
            }
        }
    }
    let is_empty = files.is_empty();

    trace_git_import(format!(
        "parse {} files={} merge={} empty={} diff={}ms rename={}ms(rename_detect={}) files={}ms total={}ms",
        short_sha,
        files.len(),
        is_merge,
        is_empty,
        diff_ms,
        rename_ms,
        detected_renames,
        parse_files_ms,
        parse_start.elapsed().as_millis()
    ));

    Ok(ParsedCommit {
        git_sha: sha,
        short_sha,
        metadata,
        files,
        parent_index,
        is_merge,
        is_empty,
    })
}

/// Extract metadata from a git commit.
fn extract_commit_metadata(commit: &git2::Commit) -> CliResult<CommitMetadata> {
    let author = commit.author();
    let author_name = author.name().unwrap_or("Unknown").to_string();
    let author_email = author.email().map(|s| s.to_string());

    let time = commit.time();
    let timestamp = Utc
        .timestamp_opt(time.seconds(), 0)
        .single()
        .unwrap_or_else(Utc::now);

    let full_message = commit.message().unwrap_or("");
    let (message, description) = parse_commit_message(full_message);

    Ok(CommitMetadata {
        author_name,
        author_email,
        timestamp,
        message,
        description,
    })
}

/// Parse files from a git diff.
///
/// For each changed file we capture:
///   - The operation type (Added / Modified / Deleted / Renamed / Copied)
///   - The new file content (for adds/modifies)
///   - The old file content (for modifies/deletes)
///   - The exact diff lines that git computed, so Phase 2 can build
///     BranchOps directly from git's diff rather than re-diffing.
fn parse_diff_files(
    git_repo: &GitRepository,
    diff: &Diff,
    tree: &Tree,
    parent_tree: Option<&Tree>,
    capture_diff_lines: bool,
) -> CliResult<Vec<ParsedFile>> {
    use std::collections::HashMap;

    // ── Step 1: collect per-file diff lines via diff.foreach ────────────
    //
    // git2::Diff::foreach gives us each DiffLine with its origin (`+`/`-`/` `),
    // raw bytes, and old/new line numbers — exactly what `git diff` outputs.
    // We key by file path so we can attach them to the ParsedFile below.

    // Map from file path → accumulated diff lines for that file.
    let mut lines_by_path: HashMap<String, Vec<GitDiffLine>> = HashMap::new();

    if capture_diff_lines {
        let _ = diff.foreach(
            &mut |_delta, _progress| true, // file_cb  (no-op)
            None,                          // binary_cb
            None,                          // hunk_cb
            Some(&mut |delta, _hunk, line| {
                let origin = line.origin();
                // We only keep `+`, `-`, and context (` `) lines.
                if origin != '+' && origin != '-' && origin != ' ' {
                    return true;
                }
                let path = delta
                    .new_file()
                    .path()
                    .or_else(|| delta.old_file().path())
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default();

                lines_by_path.entry(path).or_default().push(GitDiffLine {
                    origin,
                    content: line.content().to_vec(),
                    old_lineno: line.old_lineno(),
                    new_lineno: line.new_lineno(),
                });
                true
            }),
        );
    }

    // ── Step 2: build ParsedFile entries from the delta list ─────────────

    let mut files = Vec::new();

    for delta in diff.deltas() {
        let new_file = delta.new_file();
        let old_file = delta.old_file();

        // Skip submodules silently (warnings printed during Phase 2)
        if new_file.mode() == git2::FileMode::Commit || old_file.mode() == git2::FileMode::Commit {
            continue;
        }

        let operation = match delta.status() {
            Delta::Added => FileOperation::Added,
            Delta::Modified => FileOperation::Modified,
            Delta::Deleted => FileOperation::Deleted,
            Delta::Renamed => FileOperation::Renamed,
            Delta::Copied => FileOperation::Copied,
            _ => continue,
        };

        let path = new_file
            .path()
            .or_else(|| old_file.path())
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();

        let old_path = if matches!(operation, FileOperation::Renamed | FileOperation::Copied) {
            old_file.path().map(|p| p.to_string_lossy().to_string())
        } else {
            None
        };

        // New content from the commit's tree
        let new_content = if operation == FileOperation::Added
            || operation == FileOperation::Modified
            || operation == FileOperation::Renamed
            || operation == FileOperation::Copied
        {
            get_file_content(git_repo, tree, &path).ok()
        } else {
            None
        };

        // Old content from the parent commit's tree (for modifies/deletes)
        let old_content = if operation == FileOperation::Modified
            || operation == FileOperation::Deleted
            || operation == FileOperation::Renamed
        {
            parent_tree.and_then(|pt| {
                let lookup_path = old_path.as_deref().unwrap_or(&path);
                get_file_content(git_repo, pt, lookup_path).ok()
            })
        } else {
            None
        };

        // Diff lines captured above
        let diff_lines = lines_by_path.remove(&path);

        files.push(ParsedFile {
            path,
            operation,
            new_content,
            old_content,
            diff_lines,
            old_path,
        });
    }

    Ok(files)
}

fn parse_diff_files_via_git_cli(
    git_repo: &GitRepository,
    commit_oid: Oid,
    parent_oid: Oid,
    tree: &Tree<'_>,
    parent_tree: &Tree<'_>,
) -> CliResult<Vec<ParsedFile>> {
    let repo_root = git_repo.path().parent().ok_or_else(|| CliError::GitError {
        message: "Failed to locate git repository root".to_string(),
    })?;

    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("diff-tree")
        .arg("-r")
        .arg("--name-status")
        .arg("-M")
        .arg(parent_oid.to_string())
        .arg(commit_oid.to_string())
        .output()
        .map_err(|e| CliError::GitError {
            message: format!("Failed to run git diff-tree fallback: {}", e),
        })?;

    if !output.status.success() {
        return Err(CliError::GitError {
            message: format!(
                "git diff-tree fallback failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }

    let mut files = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split('\t');
        let status = parts.next().unwrap_or_default();
        let Some(kind) = status.chars().next() else {
            continue;
        };

        match kind {
            'A' => {
                let Some(path) = parts.next() else { continue };
                files.push(ParsedFile {
                    path: path.to_string(),
                    operation: FileOperation::Added,
                    new_content: get_file_content(git_repo, tree, path).ok(),
                    old_content: None,
                    diff_lines: None,
                    old_path: None,
                });
            }
            'M' => {
                let Some(path) = parts.next() else { continue };
                files.push(ParsedFile {
                    path: path.to_string(),
                    operation: FileOperation::Modified,
                    new_content: get_file_content(git_repo, tree, path).ok(),
                    old_content: get_file_content(git_repo, parent_tree, path).ok(),
                    diff_lines: None,
                    old_path: None,
                });
            }
            'D' => {
                let Some(path) = parts.next() else { continue };
                files.push(ParsedFile {
                    path: path.to_string(),
                    operation: FileOperation::Deleted,
                    new_content: None,
                    old_content: get_file_content(git_repo, parent_tree, path).ok(),
                    diff_lines: None,
                    old_path: None,
                });
            }
            'R' => {
                let Some(old_path) = parts.next() else {
                    continue;
                };
                let Some(path) = parts.next() else { continue };
                files.push(ParsedFile {
                    path: path.to_string(),
                    operation: FileOperation::Renamed,
                    new_content: get_file_content(git_repo, tree, path).ok(),
                    old_content: get_file_content(git_repo, parent_tree, old_path).ok(),
                    diff_lines: None,
                    old_path: Some(old_path.to_string()),
                });
            }
            'C' => {
                let old_path = parts.next();
                let Some(path) = parts.next() else { continue };
                files.push(ParsedFile {
                    path: path.to_string(),
                    operation: FileOperation::Copied,
                    new_content: get_file_content(git_repo, tree, path).ok(),
                    old_content: None,
                    diff_lines: None,
                    old_path: old_path.map(|path| path.to_string()),
                });
            }
            _ => {}
        }
    }

    Ok(files)
}

/// Get file content from a git tree.
fn get_file_content(git_repo: &GitRepository, tree: &Tree, path: &str) -> CliResult<Vec<u8>> {
    let entry = tree
        .get_path(Path::new(path))
        .map_err(|e| CliError::GitError {
            message: format!("Path not found in tree: {}", e),
        })?;

    if entry.kind() != Some(ObjectType::Blob) {
        return Err(CliError::GitError {
            message: "Not a file".to_string(),
        });
    }

    let blob = git_repo
        .find_blob(entry.id())
        .map_err(|e| CliError::GitError {
            message: format!("Failed to find blob: {}", e),
        })?;

    Ok(blob.content().to_vec())
}

/// Parse a git commit message into subject and description.
fn parse_commit_message(message: &str) -> (String, Option<String>) {
    let lines: Vec<&str> = message.lines().collect();

    if lines.is_empty() {
        return ("(no message)".to_string(), None);
    }

    let subject = lines[0].trim().to_string();

    let body_lines: Vec<&str> = lines
        .iter()
        .skip(1)
        .skip_while(|line| line.trim().is_empty())
        .copied()
        .collect();

    let description = if body_lines.is_empty() {
        None
    } else {
        Some(body_lines.join("\n").trim().to_string())
    };

    (subject, description)
}

// ═══════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_commit_message_subject_only() {
        let (subject, desc) = parse_commit_message("Fix bug");
        assert_eq!(subject, "Fix bug");
        assert!(desc.is_none());
    }

    #[test]
    fn test_parse_commit_message_with_body() {
        let (subject, desc) = parse_commit_message("Fix bug\n\nThis fixes the thing.");
        assert_eq!(subject, "Fix bug");
        assert_eq!(desc, Some("This fixes the thing.".to_string()));
    }

    #[test]
    fn test_parse_commit_message_empty() {
        let (subject, desc) = parse_commit_message("");
        assert_eq!(subject, "(no message)");
        assert!(desc.is_none());
    }

    #[test]
    fn test_file_operation_equality() {
        assert_eq!(FileOperation::Added, FileOperation::Added);
        assert_ne!(FileOperation::Added, FileOperation::Modified);
    }

    #[test]
    fn test_import_stats_default() {
        let stats = ImportStats::default();
        assert_eq!(stats.commits_found, 0);
        assert_eq!(stats.changes_written, 0);
    }

    #[test]
    fn test_graph_first_added_file_ops_use_unique_branch_ids_and_ranges() {
        let mut next_branch_idx = 0;
        let first = build_graph_first_file_ops_for_added_file(
            "a.txt",
            &[b"one\n".to_vec(), b"two\n".to_vec()],
            &[
                (ChangePosition::new(0), ChangePosition::new(4)),
                (ChangePosition::new(4), ChangePosition::new(8)),
            ],
            Encoding::Utf8,
            0,
            &mut next_branch_idx,
        );
        let second = build_graph_first_file_ops_for_added_file(
            "b.txt",
            &[b"three\n".to_vec()],
            &[(ChangePosition::new(8), ChangePosition::new(14))],
            Encoding::Utf8,
            1,
            &mut next_branch_idx,
        );

        assert_eq!(first.trunk_id().file_idx(), 0);
        assert_eq!(second.trunk_id().file_idx(), 1);
        assert_eq!(first.line_ops()[0].branch_id().branch_idx(), 0);
        assert_eq!(first.line_ops()[1].branch_id().branch_idx(), 1);
        assert_eq!(second.line_ops()[0].branch_id().branch_idx(), 2);
        assert_eq!(
            first.line_ops()[1].content_range(),
            Some((ChangePosition::new(4), ChangePosition::new(8)))
        );
    }
}
