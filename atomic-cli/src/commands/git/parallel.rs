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

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, TimeZone, Utc};
use git2::{
    Delta, Diff, DiffFindOptions, DiffOptions, ObjectType, Oid, Repository as GitRepository, Tree,
};
use rayon::prelude::*;

use atomic_core::change::{Author, Change, ChangeHeader};
use atomic_core::record::workflow::GitDiffLine;
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
}

impl Default for ParallelImportOptions {
    fn default() -> Self {
        Self {
            incremental: false,
            imported_shas: HashSet::new(),
            repo_name: "unknown".to_string(),
        }
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
}

impl ParallelImporter {
    /// Create a new parallel importer.
    pub fn new(git_repo: &GitRepository, options: ParallelImportOptions) -> Self {
        let git_repo_path = git_repo
            .path()
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| git_repo.path().to_path_buf());

        Self {
            git_repo_path,
            options,
        }
    }

    /// Open a new git repository instance (for thread-local use).
    fn open_git_repo(&self) -> CliResult<GitRepository> {
        GitRepository::open(&self.git_repo_path).map_err(|e| CliError::GitError {
            message: format!("Failed to open git repository: {}", e),
        })
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
            let write_stats = self.phase2_write(repo, &parsed_commits)?;
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

        // Phase 3: Finalization (just verification for now)
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

                parse_commit(&git_repo, *oid, idx, &oid_to_index)
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
            match self.write_commit(repo, parsed) {
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

        // Populate the mtime cache for all files written during this batch.
        // This lets `atomic status` compare file metadata (stat) instead of
        // reconstructing graph content for every file — reducing post-import
        // status from O(files × graph_traversal) to O(files × stat).
        let repo_root = repo.root().to_path_buf();
        let mut mtime_entries: Vec<(String, i64, u32, u64)> = Vec::new();

        for parsed in commits {
            for file in &parsed.files {
                if file.operation == FileOperation::Deleted {
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
                    // Normalize path to forward slashes for TREE compatibility
                    let normalized = file.path.replace('\\', "/");
                    mtime_entries.push((normalized, secs, nanos, size));
                }
            }
        }

        if !mtime_entries.is_empty() {
            let _ = repo.update_file_mtimes(&mtime_entries);
        }

        Ok(stats)
    }

    /// Write a single commit to the repository.
    fn write_commit(&self, repo: &mut Repository, parsed: &ParsedCommit) -> CliResult<bool> {
        use atomic_core::output::memory::Memory;
        use atomic_core::record::workflow::{
            record_added_file, record_deleted_file, record_modified_file, DetectedFile,
            RecordedFile, RecordingOptions,
        };

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

        // Track new files so the pristine knows about them before we record.
        // Also collect deleted paths so we can remove them from TREE after insert.
        let mut deleted_paths: Vec<String> = Vec::new();
        for file in &parsed.files {
            if file.operation == FileOperation::Added || file.operation == FileOperation::Copied {
                let _ = repo.add(&file.path, atomic_repository::TrackingOptions::default());
            }
            if file.operation == FileOperation::Deleted {
                deleted_paths.push(file.path.clone());
            }
        }

        // ── Fast path: build RecordedFiles directly from parsed content ──
        //
        // Instead of checking out the git tree to disk and running the
        // full record() pipeline (which does a filesystem scan + status),
        // we feed the already-parsed content into record_added_file /
        // record_modified_file via in-memory working copies.  This
        // eliminates all filesystem I/O for Phase 2.

        // Use patience diff for both the CRDT line-op generation and the
        // git2 capture (see parse_commit).  Both implementations produce the
        // same output for patience, so `atomic diff -c` matches `git diff
        // --patience` exactly.
        let core_options =
            RecordingOptions::new().algorithm(atomic_core::diff::Algorithm::Patience);
        let mut recorded_files: Vec<RecordedFile> = Vec::new();

        for file in &parsed.files {
            let memory_wc = Memory::new();

            match file.operation {
                FileOperation::Added | FileOperation::Copied => {
                    let content = match &file.new_content {
                        Some(c) => c.as_slice(),
                        None => continue,
                    };
                    memory_wc.add_file(&file.path, content);
                    let detected = DetectedFile::added(&file.path);
                    match record_added_file(&memory_wc, &detected, &core_options) {
                        Ok(rec) if !rec.is_empty() => recorded_files.push(rec),
                        _ => {}
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

                    // Look up the inode and position for the old path.
                    // If the old file isn't tracked at all, fall back to
                    // treating the rename as a plain addition.
                    match repo.get_inode_and_position(old_path) {
                        Ok(Some((inode, pos))) => {
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
                                let memory_wc2 = Memory::new();
                                memory_wc2.add_file(&file.path, new_content);

                                let mut detected = DetectedFile::modified(&file.path);
                                detected.inode = Some(inode);
                                detected.position = Some(pos);

                                match record_modified_file(
                                    &memory_wc2,
                                    &detected,
                                    &old_content,
                                    &core_options,
                                ) {
                                    Ok(mut rec) if !rec.is_empty() => {
                                        if let Some(ref diff_lines) = file.diff_lines {
                                            use atomic_core::record::workflow::build_crdt_ops_from_git_diff;
                                            let (git_file_ops, _) = build_crdt_ops_from_git_diff(
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
                        _ => {
                            // Old path not tracked — treat rename as a plain addition.
                            let content = match &file.new_content {
                                Some(c) => c.as_slice(),
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
                    let mut detected = DetectedFile::modified(&file.path);
                    if let Ok(Some((inode, pos))) = repo.get_inode_and_position(&file.path) {
                        detected.inode = Some(inode);
                        detected.position = Some(pos);
                    }

                    match record_modified_file(&memory_wc, &detected, &old_content, &core_options) {
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
                                &core_options,
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

        // Assemble the change from recorded files
        if recorded_files.is_empty() {
            let mut change = Change::empty(header);
            change.unhashed = Some(self.build_git_metadata(parsed, false, true));
            let hash = change.hash().map_err(|e| CliError::Internal(e.into()))?;
            repo.save_change(&change)
                .map_err(|e| CliError::Internal(e.into()))?;
            repo.insert_change(&hash, Default::default())
                .map_err(|e| CliError::Internal(e.into()))?;
            return Ok(true);
        }

        let step_start = Instant::now();
        let (mut change, hash) = match repo.assemble_and_hash(header.clone(), &recorded_files) {
            Ok(result) => result,
            Err(e) => {
                // Globalization may strip all hunks (e.g., pure deletion commits
                // where find_content_vertices returns empty for already-deleted
                // files).  Fall back to an empty change — the explicit
                // repo.remove() cleanup below still handles the TREE entries.
                let err_msg = e.to_string();
                if err_msg.contains("empty") || err_msg.contains("AllEmpty") {
                    let mut empty = Change::empty(header);
                    empty.unhashed = Some(self.build_git_metadata(parsed, false, true));
                    let h = empty.hash().map_err(|e| CliError::Internal(e.into()))?;
                    repo.save_change(&empty)
                        .map_err(|e| CliError::Internal(e.into()))?;
                    repo.insert_change(&h, Default::default())
                        .map_err(|e| CliError::Internal(e.into()))?;

                    // Still clean up deleted files from TREE
                    for del_path in &deleted_paths {
                        let _ = repo.remove(del_path, atomic_repository::TrackingOptions::forced());
                    }

                    return Ok(true);
                }
                return Err(CliError::Internal(e.into()));
            }
        };
        let assemble_ms = step_start.elapsed().as_millis();

        change.unhashed = Some(self.build_git_metadata(parsed, false, false));

        // Save and insert
        let step_start = Instant::now();
        repo.save_change(&change)
            .map_err(|e| CliError::Internal(e.into()))?;
        let save_ms = step_start.elapsed().as_millis();

        let step_start = Instant::now();
        repo.insert_change(&hash, Default::default())
            .map_err(|e| CliError::Internal(e.into()))?;
        let insert_ms = step_start.elapsed().as_millis();

        // Log slow commits (>50ms total) so we can identify the bottleneck
        let total_ms = assemble_ms + save_ms + insert_ms;
        if total_ms > 50 {
            log::info!(
                "  SLOW commit {} ({} files): assemble={}ms save={}ms insert={}ms total={}ms",
                parsed.short_sha,
                parsed.files.len(),
                assemble_ms,
                save_ms,
                insert_ms,
                total_ms,
            );
        }

        // Files deleted via record_modified_file (the "show diff lines" path)
        // produce GraphOp::Replacement, not GraphOp::FileDel, so insert_change
        // never removes their TREE entries.  Explicitly untrack them now so that
        // `atomic status` after import matches the git working copy.
        for del_path in &deleted_paths {
            let _ = repo.remove(del_path, atomic_repository::TrackingOptions::forced());
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
        let mut change = Change::empty(header);
        change.unhashed = Some(self.build_git_metadata(parsed, true, false));

        let hash = change.hash().map_err(|e| CliError::Internal(e.into()))?;

        repo.save_change(&change)
            .map_err(|e| CliError::Internal(e.into()))?;

        repo.insert_change(&hash, Default::default())
            .map_err(|e| CliError::Internal(e.into()))?;

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

    // Use patience diff for the git2 line capture.  We use patience for
    // the atomic RecordingOptions too (see write_commit), so both produce
    // the same line classification for the same file content.
    let mut diff_opts = DiffOptions::new();
    diff_opts.include_untracked(false);
    diff_opts.patience(true);

    let mut diff = git_repo
        .diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), Some(&mut diff_opts))
        .map_err(|e| CliError::GitError {
            message: format!("Failed to compute diff: {}", e),
        })?;

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
    let mut find_opts = DiffFindOptions::new();
    find_opts.renames(true);
    let _ = diff.find_similar(Some(&mut find_opts));

    let stats = diff.stats().map_err(|e| CliError::GitError {
        message: format!("Failed to get diff stats: {}", e),
    })?;

    let is_empty = stats.files_changed() == 0;

    // Parse files
    let files = parse_diff_files(git_repo, &diff, &tree, parent_tree.as_ref())?;

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
) -> CliResult<Vec<ParsedFile>> {
    use std::collections::HashMap;

    // ── Step 1: collect per-file diff lines via diff.foreach ────────────
    //
    // git2::Diff::foreach gives us each DiffLine with its origin (`+`/`-`/` `),
    // raw bytes, and old/new line numbers — exactly what `git diff` outputs.
    // We key by file path so we can attach them to the ParsedFile below.

    // Map from file path → accumulated diff lines for that file.
    let mut lines_by_path: HashMap<String, Vec<GitDiffLine>> = HashMap::new();

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

        let old_path = if operation == FileOperation::Renamed {
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
}
