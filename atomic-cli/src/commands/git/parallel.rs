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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
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
use atomic_core::change::{Encoding, GitDerivation, Local};
use atomic_core::operation::{GitHashAlgorithm, GitObjectId};
use atomic_core::pristine::{GraphTxnT, ViewTxnT};
use atomic_core::record::workflow::graph_op::BuiltHunk;
use atomic_core::record::workflow::GitDiffLine;
use atomic_core::record::workflow::RecordedFile;
use atomic_core::record::workflow::{ancestor_directories, extract_filename, extract_parent};
use atomic_core::types::{
    Base32, ChangePosition, EdgeFlags, GraphNode, Hash as ContentHash, Merkle, Position, SetId,
};
use atomic_repository::{
    compare_project_state, git_resolution_metadata, git_synthesis_metadata,
    graph_visibility_closure, observe_git_index, observe_worktree, verify_prospective_equivalence,
    ContentFilter, ConversionPolicy, EquivalenceClaims, EquivalenceReport, GitAttributesFilter,
    GitResolutionOrigin, GitSynthesisOrigin, LossNote, ManifestDisposition, MoveBasis,
    MoveEvidence, PlatformCapabilities, ProbableMove, ProjectTree, RenameCandidate, RepoPath,
    Repository, RepositoryEntry, RepositoryManifest, VerifiedProspectiveEquivalence,
};

use crate::error::{CliError, CliResult};
use crate::output::{print_info, print_warning};

// ═══════════════════════════════════════════════════════════════════════════
// Data Structures
// ═══════════════════════════════════════════════════════════════════════════

/// Statistics from the import process.
#[derive(Debug, Clone)]
pub struct ImportStats {
    /// Number of commits found in git.
    pub commits_found: usize,
    /// Number of commits successfully parsed in Phase 1.
    pub commits_parsed: usize,
    /// Number of changes written in Phase 2.
    pub changes_written: usize,
    /// CB-13C F4: commits resurrected EXACTLY from verified bindings —
    /// a subset of `changes_written`. Synthesis = `changes_written -
    /// resurrected_exact`. The two units are never conflated in events.
    pub resurrected_exact: usize,
    /// CB-13C F4: per-import correlation ID (one per import run; every
    /// event the run emits carries it so consumers can correlate).
    pub correlation_id: String,
    /// Number of empty commits (no file changes).
    pub empty_commits: usize,
    /// Number of merge commits with duplicate content.
    pub merge_commits: usize,
    /// Commits skipped because they were created by `atomic git push` and
    /// the view already contains the state they reference.
    pub self_push_skipped: usize,
    /// Squash commits represented by inserting their original change records
    /// into the target view instead of writing a new squash change (SPEC §4).
    pub squash_inserted: usize,
    /// Atomic-origin squash commits skipped because their originals were not
    /// present locally, or inserting them diverged from the git tree (SPEC §5).
    pub squash_skipped: usize,
    /// Time spent in Phase 1 (parsing).
    pub phase1_duration: std::time::Duration,
    /// Time spent in Phase 2 (writing).
    pub phase2_duration: std::time::Duration,
    /// Files processed across all commits.
    pub files_processed: usize,
}

/// CB-13C F4: the shared import aggregate emitter. `failed_after_landed`
/// is `Some(n)` when the import failed after `n` commits had already
/// landed (partial-failure accounting): the aggregate is never bypassed
/// by a partial failure.
pub(crate) fn emit_import_synthesis(
    repo: &Repository,
    stats: &ImportStats,
    failed_after_landed: Option<usize>,
) {
    use atomic_repository::repository::observability::{BridgeEventJournal, BridgeEventKind};
    BridgeEventJournal::for_repository(repo).emit_lossy(BridgeEventKind::ImportSynthesis {
        commits_found: stats.commits_found,
        commits_parsed: stats.commits_parsed,
        written: stats.changes_written,
        empty: stats.empty_commits,
        merges: stats.merge_commits,
        self_push_skipped: stats.self_push_skipped,
        squash_inserted: stats.squash_inserted,
        resurrected_exact: stats.resurrected_exact,
        correlation_id: stats.correlation_id.clone(),
        failed_after_landed,
    });
}

impl Default for ImportStats {
    fn default() -> Self {
        // CB-13C F4: every import run carries a fresh correlation ID so
        // its events (including partial-failure aggregates) correlate.
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        Self {
            commits_found: 0,
            commits_parsed: 0,
            changes_written: 0,
            resurrected_exact: 0,
            correlation_id: format!("import-{millis}-{}-{sequence}", std::process::id()),
            empty_commits: 0,
            merge_commits: 0,
            self_push_skipped: 0,
            squash_inserted: 0,
            squash_skipped: 0,
            phase1_duration: std::time::Duration::ZERO,
            phase2_duration: std::time::Duration::ZERO,
            files_processed: 0,
        }
    }
}

/// `atomic git push` trailers parsed from a commit message.
///
/// Used to recognize commits that Atomic itself created: the `Atomic-State`
/// they carry is the Merkle state of the view at push time, so if that state
/// already exists in the view, importing the commit would duplicate changes
/// the view already has.
#[derive(Debug, Clone)]
pub struct PushTrailer {
    /// Value of the `Atomic-View` trailer.
    pub view: String,
    /// Value of the `Atomic-State` trailer.
    pub state: Merkle,
    /// Change roots named by the authoritative trailing block.
    pub changes: Vec<String>,
}

/// Schema tag for the hashed empty-commit fact record.
pub(crate) const EMPTY_COMMIT_FACTS_FORMAT: &str = "atomic:empty-commit:v1";

/// Schema tag for the hashed foreign-commit fact record.
pub(crate) const FOREIGN_COMMIT_FACTS_FORMAT: &str = "atomic:foreign-commit:v1";

/// Raw foreign signature identity and time recorded for foreign commits.
///
/// The lossless raw identity bytes (`*_raw_hex`) and the timezone offset are
/// part of the record: UTF-8 lossy accessors and UTC normalization must not
/// destroy non-UTF-8 names or non-zero offsets (review blocker 6). The raw
/// bytes are hex so the hashed record stays text-safe without loss.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct ForeignSignatureFacts {
    pub name: String,
    pub email: Option<String>,
    pub time: i64,
    /// Timezone offset seconds from the raw commit object.
    #[serde(default)]
    pub time_offset_seconds: i64,
    /// Raw author-name bytes (hex); empty when absent.
    #[serde(default)]
    pub name_raw_hex: String,
    /// Raw author-email bytes (hex); empty when absent.
    #[serde(default)]
    pub email_raw_hex: String,
}

/// Raw committer identity and time (kept separate from the author so the
/// record is explicit about which is which).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct CommitterFacts {
    pub name: String,
    pub email: Option<String>,
    pub time: i64,
    /// Timezone offset seconds from the raw commit object.
    #[serde(default)]
    pub time_offset_seconds: i64,
    /// Raw committer-name bytes (hex); empty when absent.
    #[serde(default)]
    pub name_raw_hex: String,
    /// Raw committer-email bytes (hex); empty when absent.
    #[serde(default)]
    pub email_raw_hex: String,
}

/// Hashed fact record for one empty Git commit (CB-9B).
///
/// Serialized into the change's hashed `metadata` field so two distinct
/// empty Git commits produce distinct change objects even when their
/// headers would otherwise be identical, and so the raw signed foreign
/// object facts survive serialization and round-trips.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct EmptyCommitFacts {
    pub format: &'static str,
    /// Tagged Git tree OID of the empty commit (lowercase hex).
    pub tree: String,
    /// Algorithm of the tree OID.
    pub tree_algorithm: String,
    /// Raw author identity and time from the commit object.
    pub author: ForeignSignatureFacts,
    /// Raw committer identity and time from the commit object.
    pub committer: CommitterFacts,
    /// The complete raw signed commit object bytes (hex), so the exact
    /// signed object can be recovered and re-verified without the source
    /// Git ODB (review blocker 6).
    #[serde(default)]
    pub raw_object_hex: String,
}

/// Hashed fact record for one synthesized non-empty Git commit (CB-9B,
/// review blocker 6). Same lossless contract as [`EmptyCommitFacts`]: raw
/// identity bytes, timezone offsets, and the complete raw signed commit
/// object survive in the hash-covered portion of the change.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct ForeignCommitFacts {
    pub format: &'static str,
    /// Tagged commit OID (lowercase hex).
    pub commit: String,
    /// Tagged tree OID (lowercase hex).
    pub tree: String,
    /// Algorithm of the OIDs.
    pub algorithm: String,
    /// Complete ordered parent OIDs (lowercase hex).
    pub parents: Vec<String>,
    pub author: ForeignSignatureFacts,
    pub committer: CommitterFacts,
    /// The complete raw signed commit object bytes (hex).
    pub raw_object_hex: String,
}

/// Build the hashed foreign-commit fact record for `parsed` (review
/// blocker 6). The raw object bytes come from Phase 1's parse
/// ([`ParsedCommit::raw_object`]); hex encoding keeps the JSON record
/// byte-safe for non-UTF-8 identity data.
pub(crate) fn foreign_commit_facts(parsed: &ParsedCommit) -> CliResult<Vec<u8>> {
    let algorithm = git_algorithm_for_sha(&parsed.git_sha)?;
    let record = ForeignCommitFacts {
        format: FOREIGN_COMMIT_FACTS_FORMAT,
        commit: tagged_oid_hex(&git_object_id_for_sha(algorithm, &parsed.git_sha)?),
        tree: tagged_oid_hex(&git_object_id(algorithm, parsed.tree_oid)?),
        algorithm: git_algorithm_label(algorithm).to_string(),
        parents: parsed
            .parent_oids
            .iter()
            .map(|oid| {
                let tagged = git_object_id(algorithm, *oid)?;
                Ok(tagged_oid_hex(&tagged))
            })
            .collect::<CliResult<Vec<_>>>()?,
        author: ForeignSignatureFacts {
            name: parsed.metadata.author_name.clone(),
            email: parsed.metadata.author_email.clone(),
            time: parsed.metadata.author_time,
            time_offset_seconds: parsed.metadata.author_time_offset_seconds,
            name_raw_hex: hex_bytes(Some(&parsed.metadata.author_name_raw)),
            email_raw_hex: hex_bytes(parsed.metadata.author_email_raw.as_deref()),
        },
        committer: CommitterFacts {
            name: parsed.metadata.committer_name.clone(),
            email: parsed.metadata.committer_email.clone(),
            time: parsed.metadata.committer_time,
            time_offset_seconds: parsed.metadata.committer_time_offset_seconds,
            name_raw_hex: hex_bytes(Some(&parsed.metadata.committer_name_raw)),
            email_raw_hex: hex_bytes(parsed.metadata.committer_email_raw.as_deref()),
        },
        raw_object_hex: hex_bytes(Some(&parsed.raw_object)),
    };
    serde_json::to_vec(&record).map_err(|e| CliError::Internal(e.into()))
}

/// Hex-encode raw bytes (`""` when absent) for the hashed fact records.
pub(crate) fn hex_bytes(bytes: Option<&[u8]>) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let Some(bytes) = bytes else {
        return String::new();
    };
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Build the staged post-assembly expectation for one parsed commit (review
/// blocker 2 / R3).
///
/// `paths` keeps the touched-path contract (every path the commit touched
/// must project exactly as Git recorded it), and `tree` carries the COMPLETE
/// expected repository state — every entry of the commit's verified
/// prospective manifest, including untouched paths carried from the parent
/// tree — so the staged check compares whole canonical trees, modes, kinds,
/// and semantic reconstruction, not only touched bytes. Paths outside the
/// conversion policy's inclusion set are excluded from the expectation
/// exactly as the prospective manifest excludes them.
pub(crate) fn staged_expectation_for(
    parsed: &ParsedCommit,
    prospective: &atomic_repository::ProjectTree,
    semantic_paths: &[String],
) -> atomic_repository::StagedExpectation {
    let mut entries = std::collections::BTreeMap::new();
    for entry in &prospective.manifest.entries {
        if entry.disposition != atomic_repository::ManifestDisposition::Included {
            continue;
        }
        // The canonical String identity: fold keys already carry the
        // reversible escaped ASCII form (review CB-9C R7), so the bytes ARE
        // the key — escaped() must not re-escape a canonical identity.
        entries.insert(
            String::from_utf8_lossy(entry.path.as_bytes()).into_owned(),
            atomic_repository::StagedPathExpectation {
                bytes: entry.repository_bytes.clone(),
                mode: entry.mode,
                kind: entry.kind,
            },
        );
    }
    // Paths the caller required regenerated semantic FileOps for are the
    // ACTUAL recorded delta paths (review F2): the caller computes them from
    // what it recorded — including deletions, which carry semantic FileDel
    // ops — not from the raw Git per-parent diff. A merge whose union
    // already renders the merge tree legitimately records no new FileOps,
    // and a no-op resolution must not be required to invent edits; the
    // staged check separately reconstructs inherited semantic state from
    // the CRDT tables (review F4). Rebuilding this list from `parsed.files`
    // here would demand FileOps for unchanged union content and shadow the
    // caller's argument, so the parameter is used verbatim.
    let semantic_paths = semantic_paths.to_vec();
    atomic_repository::StagedExpectation {
        paths: parsed
            .files
            .iter()
            .map(|file| {
                let expected = if matches!(file.operation, FileOperation::Deleted) {
                    None
                } else {
                    Some(file.new_content.clone().unwrap_or_default())
                };
                (file.path.clone(), expected)
            })
            .collect(),
        tree: Some(atomic_repository::StagedTreeExpectation {
            entries,
            semantic_paths,
        }),
    }
}

/// Lowercase hex of a tagged Git object ID (display/proof only).
pub(crate) fn tagged_oid_hex(oid: &atomic_core::operation::GitObjectId) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(oid.as_bytes().len() * 2);
    for byte in oid.as_bytes() {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Decode a lowercase hex object ID into the tagged GitObjectId (CB-9C).
pub(crate) fn tagged_oid_from_hex(hex: &str) -> Option<atomic_core::operation::GitObjectId> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let decode = |nibble: u8| -> Option<u8> {
        let value = HEX
            .iter()
            .position(|candidate| candidate.eq_ignore_ascii_case(&nibble))?;
        Some(value as u8)
    };
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|offset| {
            Some(decode(hex.as_bytes()[offset])? * 16 + decode(hex.as_bytes()[offset + 1])?)
        })
        .collect::<Option<Vec<u8>>>()?;
    let algorithm = match bytes.len() {
        20 => GitHashAlgorithm::Sha1,
        32 => GitHashAlgorithm::Sha256,
        _ => return None,
    };
    atomic_core::operation::GitObjectId::new(algorithm, bytes).ok()
}

/// Lowercase name of a Git hash algorithm (unhashed provenance tag).
pub(crate) fn git_algorithm_label(algorithm: GitHashAlgorithm) -> &'static str {
    match algorithm {
        GitHashAlgorithm::Sha1 => "sha1",
        GitHashAlgorithm::Sha256 => "sha256",
    }
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
    /// Complete ordered Git parent OIDs — ancestry evidence for the hashed
    /// change origin. Never used as Atomic dependencies.
    pub parent_oids: Vec<Oid>,
    /// Committer time (Unix seconds), the first deterministic tie-break for
    /// bridge sequencing.
    pub committer_time: i64,
    /// Whether this is a merge commit.
    pub is_merge: bool,
    /// Whether git reported 0 files changed.
    pub is_empty: bool,
    /// `atomic git push` trailers, when the commit message ends with them.
    pub push_trailer: Option<PushTrailer>,
    /// Complete Git tree represented by this commit.
    pub tree_oid: Oid,
    /// First-parent tree used as the incremental manifest base.
    pub parent_tree_oid: Option<Oid>,
    /// The complete raw signed commit object bytes, captured in Phase 1 so
    /// the exact signed foreign object can be recovered without the source
    /// Git ODB (review blocker 6).
    pub raw_object: Vec<u8>,
}

impl ParsedCommit {
    /// Full commit message (subject + body), for trailer-aware
    /// classification of merges and squashes.
    fn full_message(&self) -> String {
        match &self.metadata.description {
            Some(desc) => format!("{}\n\n{}", self.metadata.message, desc),
            None => self.metadata.message.clone(),
        }
    }
}

/// Whether to skip importing this commit because `atomic git push` created
/// it and the target view already contains the state it represents.
///
/// The trailer's `Atomic-State` is the Merkle of the view's change sequence
/// at push time; if that state is in the view, every change the commit
/// carries is already there by definition. Importing it would duplicate
/// them (the push → pull → import round trip).
fn should_skip_self_push(parsed: &ParsedCommit, options: &ParallelImportOptions) -> bool {
    if !options.incremental {
        return false;
    }
    match &parsed.push_trailer {
        Some(trailer) => {
            self_push_state_known(trailer, &options.target_view, &options.known_states)
        }
        None => false,
    }
}

/// The self-push rule in one place: a commit created by `atomic git push`
/// carries the target view's Merkle state, so if that view already has the
/// state, the commit adds nothing. Shared by [`should_skip_self_push`] and
/// [`incremental_import_skips`] so the real import and the `--dry-run` forecast
/// can never disagree on it.
fn self_push_state_known(
    trailer: &PushTrailer,
    target_view: &str,
    known_states: &HashSet<Merkle>,
) -> bool {
    trailer.view == target_view && known_states.contains(&trailer.state)
}

/// Whether an incremental import would skip the git commit identified by `sha`
/// with the given commit `message`, for a target view that already contains
/// `imported_shas` and `known_states`.
///
/// This mirrors exactly what the real importer does — the SHA skip in
/// `collect_commit_oids` (already-imported commits) plus the self-push skip in
/// `phase2_write` (commits whose carried view state is already present). It is
/// the single source of truth used by `atomic git import --dry-run` to forecast
/// the real import count. Callers must only invoke it for incremental imports.
pub(crate) fn incremental_import_skips(
    sha: &str,
    message: &str,
    imported_shas: &HashSet<String>,
    target_view: &str,
    known_states: &HashSet<Merkle>,
) -> bool {
    if imported_shas.contains(sha) {
        return true;
    }
    match parse_push_trailer(message) {
        Some(trailer) => self_push_state_known(&trailer, target_view, known_states),
        None => false,
    }
}

/// Metadata extracted from a git commit.
#[derive(Debug, Clone)]
pub struct CommitMetadata {
    /// Author name.
    pub author_name: String,
    /// Author email (if available).
    pub author_email: Option<String>,
    /// Raw author-name bytes as recorded on the commit object (lossless for
    /// non-UTF-8 identities; review blocker 6).
    pub author_name_raw: Vec<u8>,
    /// Raw author-email bytes as recorded on the commit object.
    pub author_email_raw: Option<Vec<u8>>,
    /// Commit timestamp.
    pub timestamp: DateTime<Utc>,
    /// Commit message (first line).
    pub message: String,
    /// Commit description (remaining lines).
    pub description: Option<String>,
    /// Raw author time (Unix seconds) as recorded on the commit object.
    pub author_time: i64,
    /// Raw author timezone offset seconds as recorded on the commit object
    /// (never normalized away; review blocker 6).
    pub author_time_offset_seconds: i64,
    /// Raw committer name.
    pub committer_name: String,
    /// Raw committer email (if available).
    pub committer_email: Option<String>,
    /// Raw committer-name bytes.
    pub committer_name_raw: Vec<u8>,
    /// Raw committer-email bytes.
    pub committer_email_raw: Option<Vec<u8>>,
    /// Raw committer time (Unix seconds).
    pub committer_time: i64,
    /// Raw committer timezone offset seconds.
    pub committer_time_offset_seconds: i64,
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
    /// Canonical Git mode of the resulting path.
    pub new_mode: Option<u32>,
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
    /// Import only the selected branch's first-parent history.
    ///
    /// This is the default single-branch Git import mode. CB-9B: merge
    /// commits on the chain are still the landing event, and every
    /// multi-parent merge now imports the complete closure of all its
    /// parents (the union state must hold every parent's knowledge before
    /// the `GitResolution` is assembled), so only pure side-branch internals
    /// behind no merge are skipped in incremental mode.
    pub mainline_only: bool,
    /// Import only the graph shape of each commit, skipping the semantic
    /// (Trunk → Branch → Leaf) FileOps layer.
    ///
    /// Graph operations and Git diff metadata are still written, so files
    /// materialize and diffs render normally. The per-line CRDT FileOps that
    /// duplicate file content are omitted, which significantly reduces change
    /// size and import time for large repositories.
    pub graph_only: bool,
    /// Keep a foreign Atomic view's working copy untouched while importing.
    ///
    /// When an agent draft is current, the files on disk describe that draft,
    /// not the Git branch view being updated. In that mode the importer must
    /// not reconcile target tracking or target-driven FILE_INDEX deletions
    /// against the draft working copy.
    pub preserve_working_copy: bool,
    /// The view being imported into (the branch name). Compared against the
    /// `Atomic-View` trailer when skipping self-pushed commits.
    pub target_view: String,
    /// Merkle states already present in the target view, used to skip
    /// commits created by `atomic git push`: such a commit carries the
    /// view state it represents, and if that state is already known the
    /// commit adds nothing. Only populated for incremental imports.
    pub known_states: HashSet<Merkle>,
    /// Prove parsed Git trees against the prospective Atomic projection before
    /// source publication. This stays in-memory and never scans the worktree.
    pub validate_equivalence: bool,
}

impl Default for ParallelImportOptions {
    fn default() -> Self {
        Self {
            incremental: false,
            imported_shas: HashSet::new(),
            repo_name: "unknown".to_string(),
            mainline_only: true,
            graph_only: false,
            preserve_working_copy: false,
            target_view: String::new(),
            known_states: HashSet::new(),
            validate_equivalence: true,
        }
    }
}

#[derive(Debug, Clone)]
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

/// CB-9B: this-run ledger of imported commits and their Git ancestry.
///
/// Merge legs must be assembled against their own Git parent's interpreted
/// closure, never against the evolving union of the target view: importing
/// leg A first puts A's changes into the view, and leg B assembled against
/// that state would anchor to A's vertices and record a false dependency on
/// a commit that B's Git history never saw (review blocker 1). The ledger
/// tracks which changes this run introduced per Git commit so each leg can
/// exclude its sibling lineages from the assembly visibility.
impl Default for ClosureLedger {
    fn default() -> Self {
        Self {
            introduced: HashMap::new(),
            ancestry: HashMap::new(),
            closures: HashMap::new(),
            member_origins: None,
        }
    }
}

pub(crate) struct ClosureLedger {
    /// Git commit sha → (Atomic change hash introduced by importing it,
    /// target view it was applied to). This run only.
    introduced: HashMap<String, (ContentHash, String)>,
    /// Memoized complete Git ancestry per commit sha (including the commit
    /// itself). Git ancestry is immutable, so per-sha memoization is sound.
    ancestry: HashMap<String, std::sync::Arc<HashSet<String>>>,
    /// Memoized interpreted closures per commit sha: the persisted closure
    /// row when one exists, otherwise reconstructed from the checked SHA
    /// index plus dependency closures. Git ancestry is immutable, so
    /// per-sha memoization is sound within a run.
    closures: HashMap<String, std::sync::Arc<HashSet<ContentHash>>>,
    /// Full-chain view membership with Git origins, loaded once per run.
    member_origins: Option<Vec<(ContentHash, Option<String>)>>,
}

impl ClosureLedger {
    /// Complete Git ancestry of `sha` (the commit itself and every
    /// transitive parent), memoized. The walk uses the live Git object
    /// database, so it stays exact across batch boundaries and incremental
    /// imports.
    fn ancestry_of(
        &mut self,
        git: &GitRepository,
        sha: &str,
    ) -> CliResult<std::sync::Arc<HashSet<String>>> {
        if let Some(cached) = self.ancestry.get(sha) {
            return Ok(cached.clone());
        }
        let oid = git2::Oid::from_str(sha)
            .map_err(|e| git_error(format!("cannot parse commit oid '{sha}': {e}")))?;
        let commit = git.find_commit(oid).map_err(|e| CliError::GitError {
            message: format!("cannot read commit {sha} for ancestry: {e}"),
        })?;
        let mut set: HashSet<String> = HashSet::new();
        set.insert(sha.to_string());
        for parent in commit.parent_ids() {
            let parent_ancestry = self.ancestry_of(git, &parent.to_string())?;
            set.extend(parent_ancestry.iter().cloned());
        }
        let shared = std::sync::Arc::new(set);
        self.ancestry.insert(sha.to_string(), shared.clone());
        Ok(shared)
    }

    /// The complete interpreted closure of one Git commit (CB-9B review R1):
    /// every Atomic change whose content makes up the commit's interpreted
    /// state. The persisted row written at import time is authoritative
    /// across runs, bindings, and views; without one, the closure is
    /// reconstructed from the checked SHA index plus the change's registered
    /// dependency closure (legacy imports). Both paths are exact; a commit
    /// that can neither be read from the row nor from the index contributes
    /// an empty closure and is traced.
    fn interpreted_closure(
        &mut self,
        repo: &Repository,
        git: &GitRepository,
        sha: &str,
    ) -> CliResult<std::sync::Arc<HashSet<ContentHash>>> {
        if let Some(cached) = self.closures.get(sha) {
            return Ok(cached.clone());
        }
        let mut closure: HashSet<ContentHash> = HashSet::new();
        if let Some(persisted) = repo
            .git_commit_closure(sha)
            .map_err(|e| CliError::Internal(e.into()))?
        {
            trace_git_import(format!(
                "closure {sha}: persisted {} hash(es)",
                persisted.len()
            ));
            closure.extend(persisted);
        } else if let Some(hash) = repo
            .checked_git_sha(sha)
            .map_err(|e| CliError::Internal(e.into()))?
        {
            for dep in repo
                .change_dependency_closure(&hash)
                .map_err(|e| CliError::Internal(e.into()))?
            {
                closure.insert(dep);
            }
        }
        // The interpreted closure is the union of the parents' closures plus
        // this commit's own: derive it from the Git ancestry so an
        // unrecorded commit still reconstructs exactly.
        for member in self.ancestry_of(git, sha)?.iter() {
            if member == sha {
                continue;
            }
            let parent_closure = self.interpreted_closure(repo, git, member)?;
            closure.extend(parent_closure.iter().copied());
        }
        if let Some(own) = repo
            .checked_git_sha(sha)
            .map_err(|e| CliError::Internal(e.into()))?
        {
            closure.insert(own);
        }
        let shared = std::sync::Arc::new(closure);
        self.closures.insert(sha.to_string(), shared.clone());
        Ok(shared)
    }

    /// Full-chain view membership with Git origins, loaded once per run and
    /// refreshed with this run's introductions.
    fn origins_for(
        &mut self,
        repo: &Repository,
        target_view: &str,
    ) -> CliResult<Vec<(ContentHash, Option<String>)>> {
        if self.member_origins.is_none() {
            self.member_origins = Some(
                repo.full_chain_member_origins(target_view)
                    .map_err(|e| CliError::Internal(e.into()))?,
            );
        }
        Ok(self.member_origins.as_ref().expect("loaded above").clone())
    }

    /// Changes in `target_view` (full ancestor chain included) that are NOT
    /// part of `sha`'s interpreted closure — the sibling lineages that must
    /// stay invisible while assembling `sha` (review R1).
    ///
    /// A member is excluded only when its own unhashed provenance names a
    /// Git commit outside `sha`'s ancestry and the member is not part of any
    /// persisted/reconstructed ancestor closure. Non-Git members (native
    /// Atomic work) cannot be attributed and stay visible: their content is
    /// genuinely part of the view state Git commits build on.
    fn exclusions_for(
        &mut self,
        repo: &Repository,
        git: &GitRepository,
        sha: &str,
        target_view: &str,
        has_git_parent: bool,
    ) -> CliResult<Vec<ContentHash>> {
        if !has_git_parent {
            // A root commit has no Git parent closure to assemble against:
            // its tree replaces the view state, so the whole view is the
            // assembly context and nothing is excluded. Bound paths with
            // different content record as state replacements (rewritten
            // history), which is the sanctioned foreign-interpretation path.
            return Ok(Vec::new());
        }
        let ancestry = self.ancestry_of(git, sha)?;
        let closure = self.interpreted_closure(repo, git, sha)?;
        trace_git_import(format!(
            "exclusions {sha}: ancestry={} closure={}",
            ancestry.len(),
            closure.len()
        ));
        let mut exclusions: Vec<ContentHash> = Vec::new();
        for (hash, origin_sha) in self.origins_for(repo, target_view)? {
            if closure.contains(&hash) {
                continue;
            }
            if let Some(origin_sha) = &origin_sha {
                if !ancestry.contains(origin_sha.as_str()) {
                    exclusions.push(hash);
                }
            }
        }
        Ok(exclusions)
    }

    /// Record that importing `sha` introduced `hash` into `target_view`.
    fn record_introduced(&mut self, sha: &str, hash: ContentHash, target_view: &str) {
        self.introduced
            .insert(sha.to_string(), (hash, target_view.to_string()));
        // Refresh the memoized closure for `sha` (review R1): the persisted
        // row is closure(parent) ∪ {own hash}, so the in-run snapshot must
        // advance identically — later commits in this run then see exactly
        // what a later run reconstructs from the persisted rows.
        {
            let existing = self
                .closures
                .entry(sha.to_string())
                .or_insert_with(|| std::sync::Arc::new(HashSet::new()));
            let mut updated: HashSet<ContentHash> = (**existing).clone();
            updated.insert(hash);
            *existing = std::sync::Arc::new(updated);
        }
        // Keep the cached origin snapshot exact for the rest of this run.
        if let Some(members) = self.member_origins.as_mut() {
            members.push((hash, Some(sha.to_ascii_lowercase())));
        }
    }
}

/// One evidence entry behind a reviewable RewriteCandidate link (CB-9B,
/// review blocker 4). Each entry carries precise full-identifier evidence
/// and states its own identity limit through its class:
/// - `post-rewrite-event`: captured operation linkage only (RFC §5.4).
/// - `binding-tree-match`: advisory similarity hint found independently of
///   hooks; tree equality is never identity.
/// - `binding-range-match`: advisory hint that a locally verified binding
///   covers a known intermediate of the rewritten range (descendant of the
///   range base plus edit-path overlap), found independently of final tree
///   equality, blob survival, and message format.
/// - `unauthenticated-event`: journal text with the right shape but no
///   immutable capture or no validated locally interpreted predecessor —
///   explicitly untrusted, never linkage.
#[derive(Debug, Clone)]
pub(crate) struct RewriteEvidence {
    /// Evidence class (`post-rewrite-event` | `binding-tree-match`).
    pub class: &'static str,
    /// Full-length predecessor commit OIDs (lowercase hex).
    pub predecessors: Vec<String>,
    /// Local binding ids backing the evidence.
    pub binding_ids: Vec<String>,
    /// Journal event ids backing the evidence.
    pub event_ids: Vec<String>,
}

/// Whether `value` is a full-length, all-hex Git object ID (40 or 64 chars).
pub(crate) fn is_full_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// The complete lowercase-hex Git ancestry of `sha` (the commit itself and
/// every transitive parent), walked on the live object database. Used by the
/// bound-range containment probe to skip ancestors, whose content is
/// trivially contained in any descendant.
fn standalone_git_ancestry(git: &GitRepository, sha: &str) -> CliResult<HashSet<String>> {
    let oid = git2::Oid::from_str(sha)
        .map_err(|e| git_error(format!("cannot parse commit oid '{sha}': {e}")))?;
    let commit = git.find_commit(oid).map_err(|e| CliError::GitError {
        message: format!("cannot read commit {sha} for ancestry: {e}"),
    })?;
    let mut set: HashSet<String> = HashSet::new();
    set.insert(sha.to_ascii_lowercase());
    for parent in commit.parent_ids() {
        set.extend(standalone_git_ancestry(git, &parent.to_string())?);
    }
    Ok(set)
}

/// The tree of commit `sha`, or `None` when it is not locally readable.
fn commit_tree_of<'r>(git: &'r GitRepository, sha: &str) -> Option<git2::Tree<'r>> {
    let oid = git2::Oid::from_str(sha).ok()?;
    let commit = git.find_commit(oid).ok()?;
    commit.tree().ok()
}

#[derive(Clone)]
pub(crate) struct VerifiedImportCommit {
    parsed: ParsedCommit,
    prospective: ProjectTree,
    verified: VerifiedProspectiveEquivalence,
    /// The commit's RAW Git tree OID. The gate's `verified` evidence carries
    /// the projection-adjusted tree (Git minus declared exclusions); the raw
    /// OID anchors HEAD-stability checks that must observe the exact commit.
    raw_tree_oid: GitObjectId,
}

pub(crate) struct ProspectiveImportPlan {
    commits: Vec<VerifiedImportCommit>,
    /// Explicit Git closure boundaries detected before sequencing; stamped
    /// into every synthesized change's unhashed provenance.
    boundaries: Vec<&'static str>,
}

impl ProspectiveImportPlan {
    pub(crate) fn boundaries(&self) -> &[&'static str] {
        &self.boundaries
    }

    pub(crate) fn expected_git_tree(&self) -> Option<GitObjectId> {
        self.commits
            .last()
            .map(|commit| commit.verified.expected_git_tree().clone())
    }

    /// The last verified commit's RAW Git tree OID (HEAD-stability anchor).
    pub(crate) fn raw_git_tree(&self) -> Option<GitObjectId> {
        self.commits
            .last()
            .map(|commit| commit.raw_tree_oid.clone())
    }

    pub(crate) fn changed_paths(&self) -> Vec<PathBuf> {
        let mut paths = std::collections::BTreeSet::new();
        for commit in &self.commits {
            for file in &commit.parsed.files {
                paths.insert(PathBuf::from(&file.path));
                if let Some(old_path) = &file.old_path {
                    paths.insert(PathBuf::from(old_path));
                }
            }
        }
        paths.into_iter().collect()
    }
}

/// Fold the Git tree `tree_oid` into the tree OID the prospective gate must
/// compare against: the complete Git tree MINUS the entries this import
/// declares excluded — the conversion policy's bridge-private paths
/// (`.atomic*/`, `.vault/`) and the import-ignore patterns (e.g. a Rust
/// workspace's `Cargo.lock`).
///
/// The import synthesizes atomic content only for non-excluded entries, so
/// the projected tree legitimately lacks them. Comparing the projection
/// against the RAW Git tree would refuse every repository whose tracked tree
/// contains such a path ("prospective import tree mismatch") even though
/// nothing was lost: the exclusion set is deterministic, derived from the
/// same policy and matcher that excluded the content, and never chosen after
/// the fact. Every other entry must still be present with its exact Git
/// bytes and mode, so the gate stays unforgeable evidence that nothing ELSE
/// diverged between Git and the projection.
pub(crate) fn import_expected_tree_oid(
    git: &GitRepository,
    tree_oid: Oid,
    policy: &ConversionPolicy,
) -> CliResult<GitObjectId> {
    // Fold the Git tree into the tree OID the prospective gate must compare
    // against: the complete Git tree MINUS the entries the versioned
    // conversion policy declares bridge-private (`.atomic*/`, `.vault/`).
    // No tracked path is ever stripped: RFC tree-fidelity (§8.4 table, line
    // 648) and the colocated status contract (line 844) say tracked entries
    // are never excluded by ignore rules (review ::26 R1/R2). Raw component and
    // prefix bytes are carried through verbatim — never lossy-decoded —
    // and tree objects are encoded canonically
    // (`"<mode> <name>\0<oid>"`, git tree-name ordering) so raw non-UTF-8
    // names and literal `%`-escape lookalikes survive byte-exact.
    fn is_excluded(policy: &ConversionPolicy, raw_path: &[u8]) -> bool {
        if let Ok(path) = RepoPath::from_bytes(raw_path) {
            return policy.exclusions.exclusion(&path).is_some();
        }
        false
    }

    /// Canonical Git tree-object encoding for one directory's entries:
    /// `"<mode:o> <name>\0<oid-raw>"` per entry, in git tree-name order
    /// (byte compare with `/` appended to tree names), hashed through the
    /// workspace's own per-algorithm object hashing.
    fn fold(
        git: &GitRepository,
        tree: &git2::Tree<'_>,
        prefix: Vec<u8>,
        policy: &ConversionPolicy,
        objects: &mut atomic_repository::repository::GitObjectDatabase,
    ) -> CliResult<Option<GitObjectId>> {
        let mut entries: Vec<(Vec<u8>, u32, GitObjectId, bool)> = Vec::new();
        for entry in tree.iter() {
            let name = entry.name_bytes().to_vec();
            let mut path = prefix.clone();
            if !path.is_empty() {
                path.push(b'/');
            }
            path.extend_from_slice(&name);
            match entry.kind() {
                Some(git2::ObjectType::Tree) => {
                    // An excluded directory never reaches the projection;
                    // skip the whole subtree.
                    if is_excluded(policy, &path) {
                        continue;
                    }
                    let sub = entry
                        .to_object(git)
                        .and_then(|object| object.peel_to_tree())
                        .map_err(|error| {
                            git_error(format!(
                                "cannot read Git subtree '{}': {error}",
                                atomic_repository::escape_repo_path(&path)
                            ))
                        })?;
                    let Some(sub_oid) = fold(git, &sub, path, policy, objects)? else {
                        // The whole subtree is excluded; Git trees cannot be
                        // empty, so the expected tree omits it entirely.
                        continue;
                    };
                    entries.push((name, 0o040000, sub_oid, true));
                }
                _ => {
                    if is_excluded(policy, &path) {
                        continue;
                    }
                    entries.push((
                        name,
                        entry.filemode() as u32,
                        GitObjectId::new(policy.object_format, entry.id().as_bytes().to_vec())
                            .map_err(|error| {
                                git_error(format!("Git tree entry OID is invalid: {error}"))
                            })?,
                        false,
                    ));
                }
            }
        }
        if entries.is_empty() {
            // Git trees cannot be empty: report absence so the caller omits
            // this subtree from its parent (the ROOT caller hashes the
            // canonical empty tree below).
            return Ok(None);
        }
        entries.sort_by(|left, right| {
            let mut a = left.0.clone();
            let mut b = right.0.clone();
            if left.3 {
                a.push(b'/');
            }
            if right.3 {
                b.push(b'/');
            }
            a.cmp(&b)
        });
        let mut bytes = Vec::new();
        for (name, mode, oid, is_tree) in &entries {
            bytes.extend_from_slice(
                format!("{mode:o} ", mode = if *is_tree { 0o040000 } else { *mode }).as_bytes(),
            );
            bytes.extend_from_slice(name);
            bytes.push(0);
            bytes.extend_from_slice(oid.as_bytes());
        }
        let oid = objects
            .insert(
                policy.object_format,
                atomic_repository::repository::GitObjectKind::Tree,
                bytes,
            )
            .map_err(|error| git_error(format!("cannot hash expected Git tree: {error}")))?;
        Ok(Some(oid))
    }

    let tree = git
        .find_tree(tree_oid)
        .map_err(|error| git_error(format!("cannot read Git tree {tree_oid}: {error}")))?;
    let mut objects = atomic_repository::repository::GitObjectDatabase::default();
    match fold(git, &tree, Vec::new(), policy, &mut objects)? {
        Some(oid) => Ok(oid),
        // The entire root is excluded (or the commit genuinely carries the
        // empty tree): the projection of an empty state is the CANONICAL
        // empty tree of the object format — never a zero sentinel (review
        // ::26 R3).
        None => objects
            .insert(
                policy.object_format,
                atomic_repository::repository::GitObjectKind::Tree,
                Vec::new(),
            )
            .map_err(|error| git_error(format!("cannot hash the empty Git tree: {error}"))),
    }
}

struct ProspectiveProjectTree {
    policy: ConversionPolicy,
    entries: BTreeMap<RepoPath, RepositoryEntry>,
    manifests: HashMap<Oid, ProjectTree>,
}

/// Convert one Git tree into prospective repository entries, mirroring the
/// canonical tree parser's mode/kind conversion exactly (CB-9B review B4):
/// regular files keep their executable bit, symlinks carry their target
/// bytes, gitlinks carry their hexadecimal object ID, and the conversion
/// policy's exclusions mark their entries `Excluded` like every other
/// projection path.
fn repository_entries_from_git_tree(
    git: &GitRepository,
    tree: git2::Tree<'_>,
    policy: &ConversionPolicy,
) -> CliResult<BTreeMap<RepoPath, RepositoryEntry>> {
    let mut entries = BTreeMap::new();
    let mut queue: Vec<(Vec<u8>, git2::Tree<'_>)> = vec![(Vec::new(), tree)];
    while let Some((prefix, current)) = queue.pop() {
        for entry in current.iter() {
            // CB-9C path fidelity (review R7): tree entry names are RAW bytes
            // — never refused for non-UTF-8, never lossy-converted. The
            // RepoPath identity keeps the exact Git bytes; the String-keyed
            // layers canonicalize through the reversible escape.
            let name = entry.name_bytes();
            // CB-9C path fidelity (review R7): manifest identities are the
            // reversible escape of the raw Git bytes, so the String-keyed
            // layers and the RepoPath-keyed folds agree byte-for-byte; the
            // final tree write decodes.
            let mut path = prefix.clone();
            path.extend_from_slice(atomic_repository::escape_repo_path(name).as_bytes());
            match entry.kind() {
                Some(git2::ObjectType::Tree) => {
                    path.push(b'/');
                    let sub = entry
                        .to_object(git)
                        .and_then(|object| object.peel_to_tree())
                        .map_err(|error| {
                            git_error(format!(
                                "cannot read Git subtree '{}': {error}",
                                atomic_repository::escape_repo_path(name)
                            ))
                        })?;
                    queue.push((path, sub));
                }
                Some(git2::ObjectType::Blob) => {
                    let path = RepoPath::from_bytes(&path).map_err(|e| {
                        git_error(format!(
                            "invalid Git path '{}': {e}",
                            atomic_repository::escape_repo_path(name)
                        ))
                    })?;
                    let disposition = policy
                        .exclusions
                        .exclusion(&path)
                        .map(ManifestDisposition::Excluded)
                        .unwrap_or(ManifestDisposition::Included);
                    let object = entry.to_object(git).map_err(|error| {
                        git_error(format!(
                            "cannot read Git blob for '{}': {error}",
                            atomic_repository::escape_repo_path(name)
                        ))
                    })?;
                    let bytes = object
                        .as_blob()
                        .map(|blob| blob.content().to_vec())
                        .ok_or_else(|| {
                            git_error(format!(
                                "tree entry '{}' is not a blob",
                                atomic_repository::escape_repo_path(name)
                            ))
                        })?;
                    let (kind, mode, gitlink) = match entry.filemode() {
                        0o100644 => (atomic_core::change::InodeKind::Regular, 0o644u16, None),
                        0o100755 => (atomic_core::change::InodeKind::Regular, 0o755u16, None),
                        0o120000 => (atomic_core::change::InodeKind::Symlink, 0o777u16, None),
                        0o160000 => {
                            let oid = git_object_id(policy.object_format, entry.id())?;
                            let bytes = oid_bytes_as_hex(&oid);
                            (atomic_core::change::InodeKind::Gitlink, 0o644u16, Some(oid))
                        }
                        mode => {
                            return Err(git_error(format!(
                                "unsupported parent-tree Git mode {mode:#o} at '{}'",
                                atomic_repository::escape_repo_path(name)
                            )))
                        }
                    };
                    let repository_entry =
                        RepositoryEntry::new(path, bytes, mode, kind, gitlink, disposition)
                            .map_err(|e| {
                                git_error(format!("cannot build prospective entry: {e}"))
                            })?;
                    entries.insert(repository_entry.path.clone(), repository_entry);
                }
                _ => {
                    return Err(git_error(format!(
                        "Git tree entry '{}' has an unsupported object kind",
                        atomic_repository::escape_repo_path(name)
                    )))
                }
            }
        }
    }
    Ok(entries)
}

/// Lowercase hexadecimal encoding of a Git object ID, the canonical
/// repository bytes of a gitlink entry (review B4).
fn oid_bytes_as_hex(oid: &GitObjectId) -> Vec<u8> {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(oid.as_bytes().len() * 2);
    for byte in oid.as_bytes() {
        let _ = write!(out, "{byte:02x}");
    }
    out.into_bytes()
}

impl ProspectiveProjectTree {
    fn new(policy: ConversionPolicy, current: Option<ProjectTree>) -> CliResult<Self> {
        let mut manifests = HashMap::new();
        let entries = if let Some(project) = current {
            let oid = Oid::from_bytes(project.git.root.as_bytes())
                .map_err(|error| git_error(format!("invalid projected Git tree: {error}")))?;
            let entries = project
                .manifest
                .entries
                .iter()
                .cloned()
                .map(|entry| (entry.path.clone(), entry))
                .collect();
            manifests.insert(oid, project);
            entries
        } else {
            BTreeMap::new()
        };
        Ok(Self {
            policy,
            entries,
            manifests,
        })
    }

    fn apply_commit(
        &mut self,
        parsed: &ParsedCommit,
        git: Option<&GitRepository>,
    ) -> CliResult<(ProjectTree, VerifiedProspectiveEquivalence)> {
        if let Some(parent) = parsed.parent_tree_oid {
            if let Some(project) = self.manifests.get(&parent) {
                self.entries = project
                    .manifest
                    .entries
                    .iter()
                    .cloned()
                    .map(|entry| (entry.path.clone(), entry))
                    .collect();
            } else {
                // CB-9B review B4: the parent's manifest is not cached — the
                // baseline is the parent commit's ACTUAL Git tree, not the
                // ambient view state. Seeding from the current view here
                // kept rewritten-away content alive (e.g. an intermediate
                // the squash cancels), so the folded prospective tree
                // diverged from the commit's verified Git tree and the
                // import refused (or published a diverged boundary). The
                // parent tree is the commit's exact parent interpretation.
                let Some(git) = git else {
                    return Err(git_error(
                        "commit's parent tree is not cached and no Git object database is \
                         available to reconstruct it; refusing to fold a prospective state \
                         from stale ambient entries",
                    ));
                };
                let tree = git.find_tree(parent).map_err(|error| {
                    git_error(format!("cannot read parent Git tree {parent}: {error}"))
                })?;
                self.entries = repository_entries_from_git_tree(git, tree, &self.policy)?;
            }
        } else {
            self.entries.clear();
        }

        for file in &parsed.files {
            if file.operation == FileOperation::Renamed {
                if let Some(old_path) = &file.old_path {
                    self.entries.remove(
                        &RepoPath::from_bytes(old_path.as_bytes()).map_err(|e| {
                            git_error(format!("invalid Git path '{old_path}': {e}"))
                        })?,
                    );
                }
            }
            // CB-9C path fidelity: the canonical String identity decodes to
            // the exact raw bytes before entering the RepoPath-keyed fold.
            let path = RepoPath::from_bytes(file.path.as_bytes())
                .map_err(|e| git_error(format!("invalid Git path '{}': {e}", file.path)))?;
            if file.operation == FileOperation::Deleted {
                self.entries.remove(&path);
                continue;
            }
            let bytes = file.new_content.clone().ok_or_else(|| {
                git_error(format!(
                    "commit {} omitted bytes for '{}'",
                    parsed.git_sha, file.path
                ))
            })?;
            let mode = file.new_mode.ok_or_else(|| {
                git_error(format!(
                    "commit {} omitted mode for '{}'",
                    parsed.git_sha, file.path
                ))
            })?;
            let repository_mode = match mode {
                0o100644 => 0o644,
                0o100755 => 0o755,
                // CB-9C: symlinks project the link-target bytes with the
                // platform-independent manifest mode; gitlinks project the
                // lowercase hex object-ID bytes with a Gitlink kind.
                0o120000 => 0o777,
                0o160000 => 0o644,
                _ => {
                    return Err(git_error(format!(
                        "unsupported prospective Git mode {mode:#o} at '{}'",
                        file.path
                    )))
                }
            };
            let disposition = self
                .policy
                .exclusions
                .exclusion(&path)
                .map(ManifestDisposition::Excluded)
                .unwrap_or(ManifestDisposition::Included);
            // CB-9C: the delta's canonical mode determines the projected
            // kind; gitlink entries carry their tagged object identity so
            // the equivalence proof compares the exact gitlink target.
            let (kind, gitlink) = match mode {
                0o160000 => {
                    // The parsed bytes are the lowercase hex encoding of the
                    // tagged object id; decode them into the tagged identity.
                    let hex = std::str::from_utf8(&bytes).map_err(|e| {
                        git_error(format!("invalid gitlink target '{}': {e}", file.path))
                    })?;
                    let oid = tagged_oid_from_hex(hex).ok_or_else(|| {
                        git_error(format!("invalid gitlink target '{}'", file.path))
                    })?;
                    (atomic_core::change::InodeKind::Gitlink, Some(oid))
                }
                0o120000 => (atomic_core::change::InodeKind::Symlink, None),
                _ => (atomic_core::change::InodeKind::Regular, None),
            };
            let entry = RepositoryEntry::new(
                path.clone(),
                bytes,
                repository_mode,
                kind,
                gitlink,
                disposition,
            )
            .map_err(|e| git_error(format!("cannot build prospective entry: {e}")))?;
            self.entries.insert(path, entry);
        }

        let manifest = RepositoryManifest::new(
            SetId::ZERO,
            self.policy.root().content_key,
            self.entries.values().cloned().collect(),
        )
        .map_err(|e| git_error(format!("cannot build prospective manifest: {e}")))?;
        let project = ProjectTree::from_manifest(manifest, &self.policy)
            .map_err(|e| git_error(format!("cannot fold prospective manifest: {e}")))?;
        // CB-9B ::15 AC-3: the gate proves the projection is EXACTLY the
        // commit's complete Git tree minus ONLY the entries the versioned
        // conversion policy declares bridge-private (`.atomic*/`, `.vault/`)
        // — never any ignore-rule exclusion of tracked files (RFC §8.4;
        // review ::26 R1). The exclusion set is deterministic and derived
        // before the fold, so the comparison stays unforgeable evidence that
        // no tracked entry was lost or altered between the Git tree and the
        // projected state.
        let expected = match git {
            Some(git) => import_expected_tree_oid(git, parsed.tree_oid, &self.policy)?,
            None => {
                // Internal/test-only fold without a Git ODB: the raw commit
                // tree is the comparison target (fixtures using this path
                // never carry excluded paths). Production imports always
                // supply Git.
                git_object_id(self.policy.object_format, parsed.tree_oid)?
            }
        };
        let verified = verify_prospective_equivalence(&project, &expected).map_err(|e| {
            git_error(format!(
                "commit {} failed prospective equivalence against the Git tree minus \
                 declared exclusions: {e}",
                parsed.git_sha
            ))
        })?;
        self.manifests.insert(parsed.tree_oid, project.clone());
        Ok((project, verified))
    }
}

pub(crate) fn conversion_policy(git: &GitRepository) -> CliResult<ConversionPolicy> {
    let config = git
        .config()
        .map_err(|error| git_error(format!("cannot read Git configuration: {error}")))?;
    let object_format = match config.get_string("extensions.objectFormat") {
        Ok(value) if value.eq_ignore_ascii_case("sha1") => GitHashAlgorithm::Sha1,
        Ok(value) if value.eq_ignore_ascii_case("sha256") => GitHashAlgorithm::Sha256,
        Ok(value) => {
            return Err(git_error(format!(
                "unsupported Git object algorithm '{value}'"
            )))
        }
        Err(error) if error.code() == git2::ErrorCode::NotFound => GitHashAlgorithm::Sha1,
        Err(error) => {
            return Err(git_error(format!(
                "cannot read Git object algorithm: {error}"
            )))
        }
    };
    let mut policy = ConversionPolicy::new(object_format);
    policy.platform = PlatformCapabilities {
        lossless_unix_paths: cfg!(unix),
        symlinks: config.get_bool("core.symlinks").unwrap_or(cfg!(unix)),
        executable_bit: config.get_bool("core.filemode").unwrap_or(cfg!(unix)),
        case_sensitive: !config.get_bool("core.ignorecase").unwrap_or(false),
        unicode_normalizing: config.get_bool("core.precomposeunicode").unwrap_or(false),
    };
    Ok(policy)
}

pub(crate) fn git_object_id(algorithm: GitHashAlgorithm, oid: Oid) -> CliResult<GitObjectId> {
    GitObjectId::new(algorithm, oid.as_bytes().to_vec()).map_err(|error| {
        git_error(format!(
            "Git object ID {oid} does not match {algorithm:?}: {error}"
        ))
    })
}

/// Tagged Git object ID for a commit SHA string.
pub(crate) fn git_object_id_for_sha(
    algorithm: GitHashAlgorithm,
    sha: &str,
) -> CliResult<GitObjectId> {
    let oid = Oid::from_str(sha)
        .map_err(|error| git_error(format!("invalid Git commit SHA '{sha}': {error}")))?;
    git_object_id(algorithm, oid)
}

/// The tagged Git hash algorithm for a commit SHA, by digest width.
pub(crate) fn git_algorithm_for_sha(sha: &str) -> CliResult<GitHashAlgorithm> {
    match sha.len() {
        40 => Ok(GitHashAlgorithm::Sha1),
        64 => Ok(GitHashAlgorithm::Sha256),
        other => Err(git_error(format!(
            "cannot classify Git hash algorithm for {other}-character SHA '{sha}'"
        ))),
    }
}

/// Explicit Git closure boundaries for one import (CB-9A §7).
///
/// Every detected boundary is surfaced to the operator and stamped into each
/// synthesized change's unhashed provenance; none of them is ever silently
/// treated as a real root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GitClosureBoundary {
    /// `shallow` — history below the shallow boundary is missing.
    Shallow,
    /// Partial clone (`extensions.partialclone` or a promisor remote) —
    /// missing objects may be fetched lazily.
    Promisor,
    /// `info/grafts` rewrites parent links.
    Grafts,
    /// `refs/replace/` substitutes objects.
    ReplaceRefs,
}

impl GitClosureBoundary {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Shallow => "shallow",
            Self::Promisor => "promisor",
            Self::Grafts => "grafts",
            Self::ReplaceRefs => "replace",
        }
    }

    fn detail(self) -> &'static str {
        match self {
            Self::Shallow => {
                "history below the shallow boundary is missing; the shallow commits are imported \
                 as explicit boundary roots, not real roots"
            }
            Self::Promisor => {
                "this is a partial clone; missing objects may be fetched lazily and the closure \
                 cannot be claimed complete"
            }
            Self::Grafts => {
                "info/grafts rewrites Git parent links; sequencing follows the grafted graph"
            }
            Self::ReplaceRefs => {
                "refs/replace/ substitutes Git objects; sequencing follows the replaced graph"
            }
        }
    }
}

/// Detect explicit Git closure boundaries (CB-9A §7).
pub(crate) fn detect_git_boundaries(git: &GitRepository) -> CliResult<Vec<GitClosureBoundary>> {
    let mut boundaries = Vec::new();
    let git_dir = git.path();
    if git_dir.join("shallow").exists() {
        boundaries.push(GitClosureBoundary::Shallow);
    }
    let config = git
        .config()
        .map_err(|error| git_error(format!("cannot read Git configuration: {error}")))?;
    if config.get_string("extensions.partialclone").is_ok() {
        boundaries.push(GitClosureBoundary::Promisor);
    } else if let Ok(mut entries) = config.entries(Some("remote.*.promisor")) {
        while let Some(entry) = entries.next() {
            let is_promisor = entry
                .ok()
                .and_then(|entry| entry.value().map(|value| value == "true"))
                .unwrap_or(false);
            if is_promisor {
                boundaries.push(GitClosureBoundary::Promisor);
                break;
            }
        }
    }
    if git_dir.join("info").join("grafts").exists() {
        boundaries.push(GitClosureBoundary::Grafts);
    }
    if git
        .references()
        .map_err(|error| git_error(format!("cannot list Git references: {error}")))?
        .filter_map(|reference| reference.ok())
        .any(|reference| {
            reference
                .name()
                .map(|name| name.starts_with("refs/replace/"))
                .unwrap_or(false)
        })
    {
        boundaries.push(GitClosureBoundary::ReplaceRefs);
    }
    Ok(boundaries)
}

/// The Git shallow boundary set: hex OIDs listed in `<gitdir>/shallow`.
pub(crate) fn shallow_boundary_oids(git: &GitRepository) -> HashSet<String> {
    let path = git.path().join("shallow");
    let Ok(content) = fs::read_to_string(&path) else {
        return HashSet::new();
    };
    content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Fail closed when any collected commit, its parents, or its tree cannot be
/// resolved from the Git object database (CB-9A §7: missing-object boundaries
/// are explicit, never a silent per-commit skip).
///
/// A parent that is missing *below a declared shallow boundary commit* is the
/// shallow boundary itself: expected, and labeled on every synthesized change.
/// A missing object anywhere else refuses the import.
pub(crate) fn verify_commit_closure_objects_with_shallow(
    git: &GitRepository,
    commit_oids: &[Oid],
    shallow: &HashSet<String>,
) -> CliResult<()> {
    for oid in commit_oids {
        let commit = git.find_commit(*oid).map_err(|error| {
            git_error(format!(
                "Git closure boundary: commit {oid} cannot be read from the object database ({error}); \
                 refusing to synthesize history with missing objects"
            ))
        })?;
        for parent in commit.parent_ids() {
            if git.find_commit(parent).is_err() && !shallow.contains(&oid.to_string()) {
                return Err(git_error(format!(
                    "Git closure boundary: commit {oid} names parent {parent} that cannot be \
                     read from the object database; refusing to synthesize history with missing objects"
                )));
            }
        }
        if commit.tree().is_err() {
            return Err(git_error(format!(
                "Git closure boundary: commit {oid} tree cannot be read; refusing to synthesize \
                 history with missing objects"
            )));
        }
    }
    Ok(())
}

/// Deterministic topological sequencing for the collected closure (CB-9A §7).
///
/// Kahn's algorithm over the collected parent→child edges with a min-heap
/// keyed by (committer time, tagged OID bytes). Identical repositories
/// always produce identical ordering regardless of revwalk internals, and
/// every import labels this as bridge sequencing. Returns the ordered OIDs
/// and the number of sibling ties that had to be broken by the
/// (committer time, OID) rule.
pub(crate) fn sequence_commits_deterministically(
    git: &GitRepository,
    commit_oids: &[Oid],
) -> CliResult<(Vec<Oid>, usize)> {
    use std::cmp::Ordering as CmpOrdering;
    use std::collections::BinaryHeap;

    struct ReadyEntry {
        committer_time: i64,
        oid: Oid,
    }

    impl PartialEq for ReadyEntry {
        fn eq(&self, other: &Self) -> bool {
            self.cmp(other) == CmpOrdering::Equal
        }
    }
    impl Eq for ReadyEntry {}
    impl PartialOrd for ReadyEntry {
        fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
            Some(self.cmp(other))
        }
    }
    // BinaryHeap is a max-heap; Reverse the desired (oldest, lowest-OID) order.
    impl Ord for ReadyEntry {
        fn cmp(&self, other: &Self) -> CmpOrdering {
            other
                .committer_time
                .cmp(&self.committer_time)
                .then_with(|| other.oid.as_bytes().cmp(self.oid.as_bytes()))
        }
    }

    let collected: HashSet<Oid> = commit_oids.iter().copied().collect();
    let mut parents_of: HashMap<Oid, Vec<Oid>> = HashMap::with_capacity(commit_oids.len());
    let mut children_of: HashMap<Oid, Vec<Oid>> = HashMap::with_capacity(commit_oids.len());
    let mut indegree: HashMap<Oid, usize> = HashMap::with_capacity(commit_oids.len());
    let mut committer_time: HashMap<Oid, i64> = HashMap::with_capacity(commit_oids.len());

    for oid in commit_oids {
        let commit = git.find_commit(*oid).map_err(|error| {
            git_error(format!("cannot read commit {oid} for sequencing: {error}"))
        })?;
        committer_time.insert(*oid, commit.time().seconds());
        let mut collected_parents = Vec::new();
        for parent in commit.parent_ids() {
            if collected.contains(&parent) {
                collected_parents.push(parent);
                children_of.entry(parent).or_default().push(*oid);
            }
        }
        indegree.insert(*oid, collected_parents.len());
        parents_of.insert(*oid, collected_parents);
    }

    let mut ready: BinaryHeap<ReadyEntry> = BinaryHeap::new();
    for oid in commit_oids {
        if indegree[oid] == 0 {
            ready.push(ReadyEntry {
                committer_time: committer_time[oid],
                oid: *oid,
            });
        }
    }

    let mut ordered = Vec::with_capacity(commit_oids.len());
    let mut tie_breaks = 0usize;
    while let Some(entry) = ready.pop() {
        if !ready.is_empty() {
            // At least one sibling was ready: the (committer time, OID) rule
            // decided who goes first — record it for the bridge label.
            tie_breaks += 1;
        }
        ordered.push(entry.oid);
        if let Some(children) = children_of.get(&entry.oid) {
            for child in children.clone() {
                let degree = indegree.get_mut(&child).expect("collected child");
                *degree -= 1;
                if *degree == 0 {
                    ready.push(ReadyEntry {
                        committer_time: committer_time[&child],
                        oid: child,
                    });
                }
            }
        }
    }
    if ordered.len() != commit_oids.len() {
        return Err(git_error(format!(
            "bridge sequencing could not order all collected commits ({} of {}); the Git graph \
             is cyclic or incomplete",
            ordered.len(),
            commit_oids.len()
        )));
    }
    Ok((ordered, tie_breaks))
}

/// Refuse to reinterpret prior sequencing after a shallow repository is
/// deepened (CB-9A §7).
///
/// For every collected commit that is not yet indexed in Atomic, refuse when
/// it has an already-indexed *descendant* inside the collected closure:
/// importing it now would fabricate a second, differently-derived history
/// below changes the view already contains. Returns the count of deepened
/// commits so callers can diagnose; the walk order is the deterministic
/// bridge order.
pub(crate) fn detect_deepened_history(
    git: &GitRepository,
    ordered_oids: &[Oid],
    indexed_shas: &HashSet<String>,
) -> CliResult<usize> {
    let collected: HashSet<Oid> = ordered_oids.iter().copied().collect();
    let mut children_of: HashMap<Oid, Vec<Oid>> = HashMap::with_capacity(ordered_oids.len());
    for oid in ordered_oids {
        let commit = git.find_commit(*oid).map_err(|error| {
            git_error(format!(
                "cannot read commit {oid} for deepening check: {error}"
            ))
        })?;
        for parent in commit.parent_ids() {
            if collected.contains(&parent) {
                children_of.entry(parent).or_default().push(*oid);
            }
        }
    }

    // Newest → oldest: a commit is "below an indexed descendant" when it is
    // itself indexed or any child is.
    let mut below_indexed: HashSet<Oid> = HashSet::new();
    for oid in ordered_oids.iter().rev() {
        let indexed = indexed_shas.contains(&oid.to_string());
        let child_below = children_of
            .get(oid)
            .map(|children| children.iter().any(|child| below_indexed.contains(child)))
            .unwrap_or(false);
        if indexed || child_below {
            below_indexed.insert(*oid);
        }
    }

    let mut deepened = Vec::new();
    for oid in ordered_oids {
        let sha = oid.to_string();
        if !indexed_shas.contains(&sha) && below_indexed.contains(oid) {
            deepened.push(sha);
        }
    }
    if deepened.is_empty() {
        return Ok(0);
    }
    Err(git_error(format!(
        "Git history deepened below already-imported commits: {} unindexed commit(s) (oldest \
         {}) sit below indexed descendants; deepening must never silently reinterpret prior \
         bridge sequencing. Import the deepened history into a fresh view or explicitly \
         rebuild the affected view.",
        deepened.len(),
        deepened.last().cloned().unwrap_or_default()
    )))
}

fn validate_import_tree_capabilities(
    git: &GitRepository,
    tree: &Tree<'_>,
    prefix: &str,
) -> CliResult<()> {
    for entry in tree.iter() {
        // CB-9C path fidelity (review R7): names are RAW bytes, canonicalized
        // through the reversible escape for display; non-UTF-8 names no
        // longer refuse here.
        let name = entry.name_bytes();
        let display = atomic_repository::escape_repo_path(name);
        let path = if prefix.is_empty() {
            display
        } else {
            format!("{prefix}/{display}")
        };
        match (entry.kind(), entry.filemode()) {
            (Some(ObjectType::Tree), 0o040000) => {
                let child = git.find_tree(entry.id()).map_err(|error| {
                    git_error(format!("cannot read Git tree '{path}': {error}"))
                })?;
                validate_import_tree_capabilities(git, &child, &path)?;
            }
            // CB-9C: the supported corpus now covers executable regular
            // files, symlinks, and gitlinks. Gitlinks are synthesized as
            // gitlink entries (kind register + lowercase hex object-ID
            // bytes) — never recursed and never emitted as regular files.
            // Symlinks project the link target bytes with a Symlink kind
            // register; executables carry a mode register. Genuinely
            // unsupported modes (set-id bits, other) still refuse.
            (Some(ObjectType::Blob), 0o100644) => {}
            (Some(ObjectType::Blob), 0o100755) => {}
            (Some(ObjectType::Blob), 0o120000) => {}
            (Some(ObjectType::Commit), 0o160000) => {}
            (_, mode) => {
                return Err(git_error(format!(
                    "Git import cannot preserve required mode/kind {mode:#o} at '{path}'"
                )))
            }
        }
    }
    Ok(())
}

pub(crate) fn verify_complete_equivalence(
    repository: &Repository,
    view: &str,
    root: &Path,
    git: &GitRepository,
    context: &str,
) -> CliResult<()> {
    let policy = conversion_policy(git)?;
    let project = repository
        .project_tree(view, &policy)
        .map_err(|error| git_error(format!("cannot project Atomic tree: {error}")))?;
    let index = observe_git_index(root, &policy)
        .map_err(|error| git_error(format!("cannot observe Git index: {error}")))?;
    let filter = GitAttributesFilter::for_repository(root);
    let observed = observe_worktree(root, Some(&index), &filter, &policy)
        .map_err(|error| git_error(format!("cannot observe Git worktree: {error}")))?;
    let tree = git
        .head()
        .and_then(|head| head.peel_to_commit())
        .and_then(|commit| commit.tree())
        .map_err(|error| git_error(format!("cannot read Git HEAD tree: {error}")))?;
    let claims = EquivalenceClaims {
        object_algorithm: Some(policy.object_format),
        git_tree_root: Some(git_object_id(policy.object_format, tree.id())?),
        ..EquivalenceClaims::default()
    };
    let report = compare_project_state(&project, &index, &observed, &policy, &claims);
    if report.is_equivalent() {
        Ok(())
    } else {
        Err(equivalence_error(context, &report))
    }
}

pub(crate) fn equivalence_error(context: &str, report: &EquivalenceReport) -> CliError {
    let detail = report
        .mismatches
        .iter()
        .map(|mismatch| {
            let path = mismatch
                .path
                .as_ref()
                .map(|path| path.escaped())
                .unwrap_or_else(|| "<repository>".to_string());
            format!(
                "{:?}/{:?} {}: expected {}, actual {}",
                mismatch.layer, mismatch.kind, path, mismatch.expected, mismatch.actual
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    git_error(format!("{context} failed complete equivalence: {detail}"))
}

fn git_error(message: impl Into<String>) -> CliError {
    CliError::GitError {
        message: message.into(),
    }
}

fn is_generated_diff_skip_path(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    let name = Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path);
    let lower_name = name.to_ascii_lowercase();
    let lower_path = normalized.to_ascii_lowercase();

    lower_name.ends_with(".lock")
        || lower_name.ends_with(".sum")
        || lower_name.ends_with(".min.css")
        || lower_name.ends_with(".min.js")
        || lower_name.ends_with(".map")
        || matches!(
            lower_name.as_str(),
            "package-lock.json" | "yarn.lock" | "pnpm-lock.yaml" | "npm-shrinkwrap.json"
        )
        || matches!(
            Path::new(&lower_name)
                .extension()
                .and_then(|ext| ext.to_str()),
            Some(
                "png"
                    | "jpg"
                    | "jpeg"
                    | "gif"
                    | "webp"
                    | "ico"
                    | "bmp"
                    | "tiff"
                    | "woff"
                    | "woff2"
                    | "ttf"
                    | "eot"
                    | "otf"
                    | "pdf"
                    | "zip"
                    | "gz"
                    | "tgz"
            )
        )
        || lower_path.ends_with("/website/source/stylesheets/main.css")
        || lower_path == "website/source/stylesheets/main.css"
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
    imported_commits: usize,
}

#[derive(Default)]
struct ImportLineIndex {
    files: HashMap<String, ImportIndexedFile>,
    directories: HashMap<String, Position<Option<ContentHash>>>,
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
                    self.files.insert(
                        path.clone(),
                        ImportIndexedFile {
                            inode_pos,
                            lines,
                            imported_commits: 1,
                        },
                    );
                }
                GraphOp::DirAdd {
                    add_inode, path, ..
                } => {
                    self.directories.insert(
                        path.clone(),
                        Position {
                            change: Some(change_hash),
                            pos: add_inode.start,
                        },
                    );
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

    fn seed_missing_parent_directories(
        &mut self,
        repo: &Repository,
        parsed: &ParsedCommit,
    ) -> Result<(), atomic_repository::RepositoryError> {
        let mut required = HashSet::new();
        for file in &parsed.files {
            if matches!(file.operation, FileOperation::Added | FileOperation::Copied) {
                required.extend(ancestor_directories(&file.path));
            }
        }

        let mut missing: Vec<String> = required
            .into_iter()
            .filter(|path| !self.directories.contains_key(path))
            .collect();
        missing.sort_by_key(|path| (path.matches('/').count(), path.clone()));

        for (path, position) in repo.import_directory_anchor_seeds(&missing)? {
            self.directories.insert(
                path,
                Position {
                    change: Some(position.change),
                    pos: position.pos,
                },
            );
        }
        Ok(())
    }

    fn seed_missing_modified_files(&mut self, repo: &Repository, parsed: &ParsedCommit) {
        for file in &parsed.files {
            if file.operation != FileOperation::Modified || self.files.contains_key(&file.path) {
                continue;
            }
            let Some(old_content) = file.old_content.as_deref() else {
                continue;
            };

            let old_lines: Vec<Vec<u8>> = if Encoding::detect(old_content) == Encoding::Binary
                || is_generated_diff_skip_path(&file.path)
            {
                if old_content.is_empty() {
                    Vec::new()
                } else {
                    vec![old_content.to_vec()]
                }
            } else {
                split_graph_first_lines(old_content)
                    .into_iter()
                    .map(|line| line.to_vec())
                    .collect()
            };

            let seed = match repo.import_line_index_seed(&file.path) {
                Ok(Some(seed)) => seed,
                Ok(None) => continue,
                Err(err) => {
                    trace_git_import(format!(
                        "{}: could not seed line index for {}: {}",
                        parsed.short_sha, file.path, err
                    ));
                    continue;
                }
            };

            if seed.lines.len() != old_lines.len() {
                trace_git_import(format!(
                    "{}: not seeding line index for {}: graph lines={} git old lines={}",
                    parsed.short_sha,
                    file.path,
                    seed.lines.len(),
                    old_lines.len()
                ));
                continue;
            }

            let mut imported_changes = HashSet::new();
            let lines = seed
                .lines
                .iter()
                .zip(old_lines)
                .map(|(line, content)| {
                    imported_changes.insert(line.incoming_by);
                    ImportLine {
                        change: line.change,
                        start: line.start,
                        end: line.end,
                        incoming_by: line.incoming_by,
                        content,
                    }
                })
                .collect();

            self.files.insert(
                file.path.clone(),
                ImportIndexedFile {
                    inode_pos: Position {
                        change: Some(seed.inode_pos.change),
                        pos: seed.inode_pos.pos,
                    },
                    lines,
                    imported_commits: imported_changes.len().max(1),
                },
            );
        }
    }

    fn seed_file_from_graph_content(
        &mut self,
        repo: &Repository,
        path: &str,
        content: &[u8],
        imported_commits_hint: usize,
    ) -> bool {
        let line_contents: Vec<Vec<u8>> =
            if Encoding::detect(content) == Encoding::Binary || is_generated_diff_skip_path(path) {
                if content.is_empty() {
                    Vec::new()
                } else {
                    vec![content.to_vec()]
                }
            } else {
                split_graph_first_lines(content)
                    .into_iter()
                    .map(|line| line.to_vec())
                    .collect()
            };

        let seed = match repo.import_line_index_seed(path) {
            Ok(Some(seed)) => seed,
            Ok(None) => return false,
            Err(err) => {
                trace_git_import(format!(
                    "could not reseed line index for {} after fallback: {}",
                    path, err
                ));
                return false;
            }
        };

        if seed.lines.len() != line_contents.len() {
            trace_git_import(format!(
                "not reseeding line index for {} after fallback: graph lines={} content lines={}",
                path,
                seed.lines.len(),
                line_contents.len()
            ));
            return false;
        }

        let mut imported_changes = HashSet::new();
        let lines = seed
            .lines
            .iter()
            .zip(line_contents)
            .map(|(line, content)| {
                imported_changes.insert(line.incoming_by);
                ImportLine {
                    change: line.change,
                    start: line.start,
                    end: line.end,
                    incoming_by: line.incoming_by,
                    content,
                }
            })
            .collect();

        self.files.insert(
            path.to_string(),
            ImportIndexedFile {
                inode_pos: Position {
                    change: Some(seed.inode_pos.change),
                    pos: seed.inode_pos.pos,
                },
                lines,
                imported_commits: imported_changes.len().max(imported_commits_hint).max(1),
            },
        );
        true
    }

    fn reseed_from_fallback_write(&mut self, repo: &Repository, parsed: &ParsedCommit) {
        for file in &parsed.files {
            match file.operation {
                FileOperation::Deleted => {
                    self.files.remove(&file.path);
                }
                FileOperation::Renamed => {
                    if let Some(old_path) = file.old_path.as_deref() {
                        self.files.remove(old_path);
                    }
                    if let Some(new_content) = file.new_content.as_deref() {
                        let hint = self
                            .files
                            .get(&file.path)
                            .map(|indexed| indexed.imported_commits.saturating_add(1))
                            .unwrap_or(1);
                        self.seed_file_from_graph_content(repo, &file.path, new_content, hint);
                    }
                }
                FileOperation::Added | FileOperation::Copied | FileOperation::Modified => {
                    if let Some(new_content) = file.new_content.as_deref() {
                        let hint = self
                            .files
                            .get(&file.path)
                            .map(|indexed| indexed.imported_commits.saturating_add(1))
                            .unwrap_or(1);
                        self.seed_file_from_graph_content(repo, &file.path, new_content, hint);
                    }
                }
            }
        }
    }
}

#[derive(Debug)]
enum PendingLineIndexUpdate {
    DirectoryAdd {
        path: String,
        inode_pos: Position<Option<ContentHash>>,
    },
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

#[derive(Debug, Clone)]
struct GraphFirstSkip {
    path: String,
    operation: FileOperation,
    reason: &'static str,
}

impl GraphFirstSkip {
    fn new(file: &ParsedFile, reason: &'static str) -> Self {
        Self {
            path: file.path.clone(),
            operation: file.operation,
            reason,
        }
    }
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

fn import_shape_for_file(file: &ParsedFile, line_index: &ImportLineIndex) -> (usize, usize, usize) {
    let indexed = line_index.files.get(&file.path).or_else(|| {
        file.old_path
            .as_deref()
            .and_then(|old| line_index.files.get(old))
    });
    let current_lines = file
        .new_content
        .as_deref()
        .or(file.old_content.as_deref())
        .map(count_line_units)
        .or_else(|| indexed.map(|idx| idx.lines.len()))
        .unwrap_or(0);
    let indexed_lines = indexed.map(|idx| idx.lines.len()).unwrap_or(0);
    let imported_commits = indexed.map(|idx| idx.imported_commits).unwrap_or(0);
    (current_lines, indexed_lines, imported_commits)
}

fn import_shape_summary(parsed: &ParsedCommit, line_index: &ImportLineIndex) -> String {
    let mut entries: Vec<(usize, String)> = parsed
        .files
        .iter()
        .map(|file| {
            let (current_lines, indexed_lines, imported_commits) =
                import_shape_for_file(file, line_index);
            let bytes = file
                .new_content
                .as_ref()
                .or(file.old_content.as_ref())
                .map(|content| content.len())
                .unwrap_or(0);
            let weight = current_lines
                .saturating_mul(imported_commits.max(1))
                .saturating_add(indexed_lines);
            (
                weight,
                format!(
                    "{} op={:?} lines={} indexed_lines={} file_commits={} bytes={}",
                    file.path,
                    file.operation,
                    current_lines,
                    indexed_lines,
                    imported_commits,
                    bytes
                ),
            )
        })
        .collect();
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    entries
        .into_iter()
        .take(3)
        .map(|(_, entry)| entry)
        .collect::<Vec<_>>()
        .join("; ")
}

fn graph_first_skip_summary(skips: &[GraphFirstSkip], parsed: &ParsedCommit) -> String {
    if skips.is_empty() {
        return String::new();
    }

    skips
        .iter()
        .take(5)
        .map(|skip| {
            let lines = parsed
                .files
                .iter()
                .find(|file| file.path == skip.path)
                .and_then(|file| {
                    file.new_content
                        .as_deref()
                        .or(file.old_content.as_deref())
                        .map(count_line_units)
                })
                .unwrap_or(0);
            format!(
                "{} op={:?} reason={} lines={}",
                skip.path, skip.operation, skip.reason, lines
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

pub(crate) fn trace_git_import_enabled() -> bool {
    std::env::var_os("ATOMIC_TRACE_GIT_IMPORT").is_some()
}

pub(crate) fn trace_git_import(message: impl AsRef<str>) {
    if trace_git_import_enabled() {
        eprintln!("[git-import] {}", message.as_ref());
    }
}

struct SlowImportProgress {
    done: Option<mpsc::Sender<()>>,
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
            // First heartbeat after a 5s quiet window, then every 15s. A
            // finished (sent) or abandoned (dropped) channel must STOP the
            // spinner: recv_timeout returns Err instantly on disconnect, and
            // the historical `if rx.recv_timeout(..).is_ok() { return }`
            // pattern spun forever printing thousands of heartbeats that
            // buried the actual refusal (review CB-9C R7).
            let mut first = true;
            loop {
                let quiet = if first {
                    Duration::from_secs(5)
                } else {
                    Duration::from_secs(15)
                };
                match rx.recv_timeout(quiet) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
                reported_for_thread.store(true, Ordering::Relaxed);
                print_info(&format!(
                    "Still importing {} after {}s; {}",
                    commit,
                    started.elapsed().as_secs(),
                    summary
                ));
            }
        });

        Self {
            done: Some(done),
            reported,
            handle: Some(handle),
        }
    }

    /// Stop the spinner and report whether it fired. Idempotent: both the
    /// success path and the failing-synthesis path must close the channel, or
    /// the spinner spins forever on the disconnected receiver.
    fn finish(mut self) -> bool {
        if let Some(done) = self.done.take() {
            let _ = done.send(());
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        self.reported.load(Ordering::Relaxed)
    }
}

impl Drop for SlowImportProgress {
    fn drop(&mut self) {
        // Any early return — an error return, a `?`, or a panic unwind —
        // closes the heartbeat channel so the spinner thread stops instead of
        // spinning on a disconnected channel (review CB-9C R7).
        if let Some(done) = self.done.take() {
            let _ = done.send(());
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
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
    if encoding == Encoding::Binary {
        return file_ops;
    }

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
    graph_only: bool,
) -> Result<(Change, Vec<PendingLineIndexUpdate>, Vec<String>), Vec<GraphFirstSkip>> {
    if parsed.files.is_empty() {
        return Err(vec![GraphFirstSkip {
            path: String::new(),
            operation: FileOperation::Modified,
            reason: "empty_commit",
        }]);
    }

    let mut contents = Vec::new();
    let mut hunks = Vec::new();
    let mut file_ops = Vec::new();
    let mut next_file_idx = 0u32;
    let mut next_branch_idx = 0u32;
    let mut dependencies = HashSet::new();
    let mut pending = Vec::new();
    let mut deleted_paths = Vec::new();
    let mut skips = Vec::new();

    // Git trees do not contain explicit directory entries. Build the required
    // parent anchors first so every nested name attaches to an actual directory
    // inode, including directories introduced earlier in this same change.
    let mut directory_anchors = line_index.directories.clone();
    let mut required_directories = HashSet::new();
    for file in &parsed.files {
        if matches!(file.operation, FileOperation::Added | FileOperation::Copied) {
            required_directories.extend(ancestor_directories(&file.path));
        }
    }
    let mut required_directories: Vec<String> = required_directories.into_iter().collect();
    required_directories.sort_by_key(|path| (path.matches('/').count(), path.clone()));

    for directory_path in required_directories {
        if let Some(anchor) = directory_anchors.get(&directory_path) {
            dependencies.extend(position_hashes(anchor));
            continue;
        }

        let parent_path = extract_parent(&directory_path);
        let parent_pos = if parent_path.is_empty() {
            Position {
                change: Some(ContentHash::NONE),
                pos: ChangePosition::ROOT,
            }
        } else if let Some(anchor) = directory_anchors.get(parent_path).copied() {
            anchor
        } else {
            return Err(vec![GraphFirstSkip {
                path: directory_path,
                operation: FileOperation::Added,
                reason: "missing_parent_directory_anchor",
            }]);
        };
        dependencies.extend(position_hashes(&parent_pos));

        let dirname = extract_filename(&directory_path);
        if dirname.is_empty() {
            return Err(vec![GraphFirstSkip {
                path: directory_path,
                operation: FileOperation::Added,
                reason: "invalid_directory_path",
            }]);
        }
        let name_start = ChangePosition::new(contents.len() as u64);
        contents.extend_from_slice(dirname.as_bytes());
        let name_end = ChangePosition::new(contents.len() as u64);
        let inode_pos = Position {
            change: None,
            pos: name_end,
        };

        hunks.push(GraphOp::DirAdd {
            add_name: Insertion {
                predecessors: vec![parent_pos],
                successors: vec![],
                flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
                start: name_start,
                end: name_end,
                inode: parent_pos,
            },
            add_inode: Insertion {
                predecessors: vec![Position {
                    change: None,
                    pos: name_end,
                }],
                successors: vec![],
                flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
                start: name_end,
                end: name_end,
                inode: inode_pos,
            },
            path: directory_path.clone(),
        });
        pending.push(PendingLineIndexUpdate::DirectoryAdd {
            path: directory_path.clone(),
            inode_pos,
        });
        directory_anchors.insert(directory_path, inode_pos);
    }

    for file in &parsed.files {
        match file.operation {
            FileOperation::Added | FileOperation::Copied => {
                let new_content = file.new_content.as_deref().unwrap_or(&[]);
                let encoding = Encoding::detect(new_content);

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
                let parent_path = extract_parent(&file.path);
                let parent_pos = if parent_path.is_empty() {
                    Position {
                        change: Some(ContentHash::NONE),
                        pos: ChangePosition::ROOT,
                    }
                } else if let Some(anchor) = directory_anchors.get(parent_path).copied() {
                    anchor
                } else {
                    skips.push(GraphFirstSkip::new(file, "missing_parent_directory_anchor"));
                    continue;
                };
                dependencies.extend(position_hashes(&parent_pos));

                let new_line_contents: Vec<Vec<u8>> =
                    if encoding == Encoding::Binary || is_generated_diff_skip_path(&file.path) {
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

                if !graph_only {
                    file_ops.push(build_graph_first_file_ops_for_added_file(
                        &file.path,
                        &new_line_contents,
                        &new_ranges,
                        encoding,
                        next_file_idx,
                        &mut next_branch_idx,
                    ));
                    next_file_idx += 1;
                }
                pending.push(PendingLineIndexUpdate::Add {
                    path: file.path.clone(),
                    inode_pos,
                    new_ranges,
                    new_lines: new_line_contents,
                });
                continue;
            }
            FileOperation::Renamed => {
                let Some(old_path) = file.old_path.as_deref() else {
                    skips.push(GraphFirstSkip::new(file, "rename_missing_old_path"));
                    continue;
                };
                let Some(indexed) = line_index.files.get(old_path) else {
                    skips.push(GraphFirstSkip::new(file, "rename_missing_line_index"));
                    continue;
                };
                let new_content = file.new_content.as_deref().unwrap_or(&[]);
                let encoding = Encoding::detect(new_content);

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

                if !graph_only
                    && encoding != Encoding::Binary
                    && !is_generated_diff_skip_path(&file.path)
                {
                    if let Some(diff_lines) = file.diff_lines.as_ref() {
                        let (ops, _) = atomic_core::record::workflow::build_crdt_ops_from_git_diff(
                            &file.path, diff_lines,
                        );
                        file_ops.push(ops);
                    }
                }

                let replacements =
                    if encoding == Encoding::Binary || is_generated_diff_skip_path(&file.path) {
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
                            match replacement.old_start.checked_sub(1) {
                                Some(idx) => idx,
                                None => {
                                    skips.push(GraphFirstSkip::new(
                                        file,
                                        "rename_replacement_underflow",
                                    ));
                                    continue;
                                }
                            }
                        };
                        let Some(end_idx) = start_idx.checked_add(replacement.old_len) else {
                            skips.push(GraphFirstSkip::new(file, "rename_replacement_overflow"));
                            continue;
                        };
                        if end_idx > indexed.lines.len() {
                            skips.push(GraphFirstSkip::new(
                                file,
                                "rename_replacement_out_of_bounds",
                            ));
                            continue;
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
                let Some(indexed) = line_index.files.get(&file.path) else {
                    skips.push(GraphFirstSkip::new(file, "delete_missing_line_index"));
                    deleted_paths.push(file.path.clone());
                    pending.push(PendingLineIndexUpdate::Delete {
                        path: file.path.clone(),
                    });
                    continue;
                };
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
                if !graph_only {
                    if let Some(diff_lines) = file.diff_lines.as_ref() {
                        let (ops, _) = atomic_core::record::workflow::build_crdt_ops_from_git_diff(
                            &file.path, diff_lines,
                        );
                        file_ops.push(ops);
                    }
                }
                continue;
            }
            FileOperation::Modified => {}
        }

        let Some(indexed) = line_index.files.get(&file.path) else {
            skips.push(GraphFirstSkip::new(file, "modified_missing_line_index"));
            continue;
        };
        let Some(new_content) = file.new_content.as_deref() else {
            skips.push(GraphFirstSkip::new(file, "modified_missing_new_content"));
            continue;
        };
        let encoding = Encoding::detect(new_content);
        let replacements =
            if encoding == Encoding::Binary || is_generated_diff_skip_path(&file.path) {
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
                current_state_replacements(indexed, new_content)
            } else {
                let Some(diff_lines) = file.diff_lines.as_ref() else {
                    skips.push(GraphFirstSkip::new(file, "modified_missing_diff_lines"));
                    continue;
                };
                if !graph_only {
                    let (ops, _) = atomic_core::record::workflow::build_crdt_ops_from_git_diff(
                        &file.path, diff_lines,
                    );
                    file_ops.push(ops);
                }
                let Some(replacements) = parse_git_diff_replacements(diff_lines) else {
                    skips.push(GraphFirstSkip::new(file, "modified_unparseable_diff_lines"));
                    continue;
                };
                replacements
            };
        if replacements.is_empty() {
            continue;
        }
        if std::env::var_os("ATOMIC_TRACE_GIT_IMPORT").is_some() {
            for r in &replacements {
                eprintln!(
                    "[git-import] modify replacement {}: path={} old_start={} old_len={} new_lines={} new_bytes={} indexed_lines={}",
                    parsed.short_sha,
                    file.path,
                    r.old_start,
                    r.old_len,
                    r.new_lines.len(),
                    r.new_lines.iter().map(|l| l.len()).sum::<usize>(),
                    indexed.lines.len(),
                );
            }
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
                let Some(idx) = replacement.old_start.checked_sub(1) else {
                    skips.push(GraphFirstSkip::new(file, "modified_replacement_underflow"));
                    continue;
                };
                idx
            };
            let Some(end_idx) = start_idx.checked_add(replacement.old_len) else {
                skips.push(GraphFirstSkip::new(file, "modified_replacement_overflow"));
                continue;
            };
            if end_idx > indexed.lines.len() {
                skips.push(GraphFirstSkip::new(
                    file,
                    "modified_replacement_out_of_bounds",
                ));
                continue;
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
        if skips.is_empty() {
            skips.push(GraphFirstSkip {
                path: String::new(),
                operation: FileOperation::Modified,
                reason: "no_graph_hunks",
            });
        }
        return Err(skips);
    }

    if !skips.is_empty() {
        return Err(skips);
    }

    let mut dependencies: Vec<ContentHash> = dependencies.into_iter().collect();
    dependencies.sort();
    dependencies.dedup();
    Ok((
        Change::with_file_ops(header, hunks, file_ops, contents, dependencies),
        pending,
        deleted_paths,
    ))
}

fn build_graph_first_skip_reasons(
    parsed: &ParsedCommit,
    line_index: &ImportLineIndex,
) -> Vec<GraphFirstSkip> {
    let mut skips = Vec::new();
    for file in &parsed.files {
        match file.operation {
            FileOperation::Modified => {
                if !line_index.files.contains_key(&file.path) {
                    skips.push(GraphFirstSkip::new(file, "modified_missing_line_index"));
                } else if file.new_content.is_none() {
                    skips.push(GraphFirstSkip::new(file, "modified_missing_new_content"));
                } else if !parsed.is_merge
                    && !is_generated_diff_skip_path(&file.path)
                    && file
                        .new_content
                        .as_deref()
                        .map(Encoding::detect)
                        .is_some_and(|encoding| encoding != Encoding::Binary)
                    && file.diff_lines.is_none()
                {
                    skips.push(GraphFirstSkip::new(file, "modified_missing_diff_lines"));
                }
            }
            FileOperation::Deleted => {
                skips.push(GraphFirstSkip::new(
                    file,
                    "delete_requires_structural_file_del",
                ));
            }
            FileOperation::Renamed => {
                if file.old_path.is_none() {
                    skips.push(GraphFirstSkip::new(file, "rename_missing_old_path"));
                } else if file
                    .old_path
                    .as_deref()
                    .is_some_and(|old| !line_index.files.contains_key(old))
                {
                    skips.push(GraphFirstSkip::new(file, "rename_missing_line_index"));
                }
            }
            FileOperation::Added | FileOperation::Copied => {}
        }
    }
    skips
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
            PendingLineIndexUpdate::DirectoryAdd { path, inode_pos } => {
                line_index.directories.insert(
                    path,
                    Position {
                        change: Some(change_hash),
                        pos: inode_pos.pos,
                    },
                );
            }
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
                line_index.files.insert(
                    path,
                    ImportIndexedFile {
                        inode_pos,
                        lines,
                        imported_commits: 1,
                    },
                );
            }
            PendingLineIndexUpdate::Modify { path, replacements } => {
                let Some(indexed) = line_index.files.get_mut(&path) else {
                    continue;
                };
                if !replacements.is_empty() {
                    indexed.imported_commits += 1;
                }

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
        // Open the Git DIR itself, not its parent: git2 resolves the working
        // tree from a gitdir path (including a linked worktree's
        // `.git/worktrees/<name>`), while the parent of a linked worktree
        // gitdir is `.git/worktrees`, which is not a repository.
        let git_repo_path = git_repo.path().to_path_buf();

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

    pub(crate) fn validate_branch_prospectively(
        &self,
        branch_name: &str,
        source: Option<&Repository>,
    ) -> CliResult<ProspectiveImportPlan> {
        self.validate_branch_prospectively_for_tip(branch_name, source, None)
    }

    /// Same prospective validation with an explicit tip override (§7.5): the
    /// commits behind `tip` import into the view named `branch_name` even when
    /// no local branch carries that tip.
    pub(crate) fn validate_branch_prospectively_for_tip(
        &self,
        branch_name: &str,
        source: Option<&Repository>,
        tip: Option<Oid>,
    ) -> CliResult<ProspectiveImportPlan> {
        let total_start = Instant::now();
        let git = self.open_git_repo()?;
        let policy = conversion_policy(&git)?;
        let step = Instant::now();
        let collected = match tip {
            Some(tip) => self.collect_commit_oids_from_tip(&git, tip)?,
            None => self.collect_commit_oids(&git, branch_name)?,
        };
        trace_git_import(format!("collect commits: {:?}", step.elapsed()));

        // CB-9A §7: closure boundaries are explicit before anything else.
        let boundaries: Vec<&'static str> = detect_git_boundaries(&git)?
            .iter()
            .map(|boundary| boundary.label())
            .collect();
        if !boundaries.is_empty() {
            print_warning(&format!(
                "Git closure boundary for branch '{}': {}",
                branch_name,
                boundaries.join(", ")
            ));
        }

        // CB-9A §7: deterministic topological sequencing — Kahn's algorithm
        // with (committer time, tagged OID) tie-breaks, labeled as bridge
        // sequencing.
        let (commit_oids, tie_breaks) = sequence_commits_deterministically(&git, &collected)?;
        trace_git_import(format!(
            "bridge sequencing: {} commits ordered by (committer time, oid); {} tie-break(s)",
            commit_oids.len(),
            tie_breaks
        ));

        // CB-9A §7: every commit, parent, and tree object must resolve.
        // Missing objects are an explicit boundary, never a silent skip.
        // Parents below declared shallow boundary commits are the boundary
        // itself; anything else refuses.
        let shallow = shallow_boundary_oids(&git);
        verify_commit_closure_objects_with_shallow(&git, &commit_oids, &shallow)?;

        // CB-9A §7: deepening never silently reinterprets prior sequencing.
        // Runs before the indexed-SHA skip so the collected closure still
        // contains the imported commits whose descendants must be checked.
        if !self.options.imported_shas.is_empty() {
            detect_deepened_history(&git, &commit_oids, &self.options.imported_shas)?;
        }

        // Commits already represented in Atomic are skipped: legacy imported
        // bytes and hashes remain authoritative without rewriting. The skip
        // set is derived from view membership (get_incremental_markers), so
        // it cannot outrun verification.
        let commit_oids: Vec<Oid> = commit_oids
            .into_iter()
            .filter(|oid| !self.options.imported_shas.contains(&oid.to_string()))
            .collect();

        let step = Instant::now();
        // The complete-tree expectation (review R3) must start from the
        // current durable projection: the source view when importing from
        // another repository, otherwise this repository's own view (the
        // incremental git-shadow-sync flow). Bridge-private files tracked
        // in the view but absent from Git (e.g. `.atomicignore`) then stay
        // in the expectation exactly as they stay alive in the view.
        let current = match source {
            Some(source) if source.view_exists(branch_name).map_err(CliError::from)? => {
                Some(source.project_tree(branch_name, &policy).map_err(|error| {
                    git_error(format!("cannot project current Atomic tree: {error}"))
                })?)
            }
            _ => None,
        }
        .or_else(|| {
            // Local target view: the durable projection is the base the new
            // commits apply onto. `git_repo_path` may point inside .git; the
            // repository discovery accepts it and resolves the worktree root.
            let repo = Repository::open(&self.git_repo_path).ok()?;
            if repo.view_exists(branch_name).ok()? {
                repo.project_tree(branch_name, &policy).ok()
            } else {
                None
            }
        });
        trace_git_import(format!("project current Atomic tree: {:?}", step.elapsed()));
        if commit_oids.is_empty() {
            return Ok(ProspectiveImportPlan {
                commits: Vec::new(),
                boundaries,
            });
        }

        for oid in &commit_oids {
            let tree = git
                .find_commit(*oid)
                .and_then(|commit| commit.tree())
                .map_err(|error| git_error(format!("cannot read Git tree for {oid}: {error}")))?;
            validate_import_tree_capabilities(&git, &tree, "")?;
        }

        let step = Instant::now();
        let parsed_commits = self.phase1_parse(&commit_oids)?;
        trace_git_import(format!("parse commits: {:?}", step.elapsed()));
        let step = Instant::now();
        let mut prospective = ProspectiveProjectTree::new(policy.clone(), current)?;
        let mut verified_commits = Vec::with_capacity(parsed_commits.len());
        for parsed in parsed_commits {
            let (project, verified) = prospective.apply_commit(&parsed, Some(&git))?;

            let claimed_hashes = parse_atomic_changes_trailer(&parsed.full_message());
            if let Some(trailer) = &parsed.push_trailer {
                if !self_push_state_known(trailer, branch_name, &self.options.known_states) {
                    return Err(git_error(format!(
                        "commit {} carries an unverifiable Atomic-State/Atomic-View claim",
                        parsed.git_sha
                    )));
                }
                self.verify_claimed_state(source, branch_name, &parsed, trailer, &policy)?;
                self.validate_claimed_change_roots(source, &parsed, &trailer.changes)?;
            } else if claimed_hashes.is_some() {
                // Fail-closed identity verification (review ATOM::aaron::2
                // ac-4, RFC §12.8): every Atomic projection writer records
                // the full `Atomic-View`/`Atomic-State` identity headers
                // next to `Atomic-Changes`, so a bare `Atomic-Changes` claim
                // is a shape no Atomic projection ever emits and is refused
                // as forged. A commit carrying the full record-projection
                // header set verifies its claimed closure against the store
                // when one exists; on a fresh bootstrap (the CB-6B binding
                // transport flow) there is no store to verify against, so
                // the claim is recorded as unverified provenance — content
                // is recomputed from the Git tree either way, and the
                // binding layer establishes trust later.
                let message = parsed.full_message();
                let projection_header = parse_header_trailer(&message, "Atomic-View").is_some()
                    && parse_header_trailer(&message, "Atomic-State").is_some();
                if !projection_header {
                    return Err(git_error(format!(
                        "commit {} claims Atomic provenance with a bare Atomic-Changes \
                         header; no Atomic projection emits that shape and the claim is \
                         unverifiable — refusing the forged claim",
                        parsed.git_sha
                    )));
                }
                if source.is_none() {
                    trace_git_import(format!(
                        "commit {} claims an Atomic-Changes closure; unverified \
                         because no Atomic repository exists yet at import time",
                        parsed.git_sha
                    ));
                } else {
                    self.verify_claimed_change_closure(
                        source,
                        branch_name,
                        &parsed,
                        claimed_hashes.as_deref(),
                        &policy,
                    )?;
                }
            }

            verified_commits.push(VerifiedImportCommit {
                raw_tree_oid: git_object_id(policy.object_format, parsed.tree_oid)?,
                parsed,
                prospective: project,
                verified,
            });
        }

        trace_git_import(format!("prospective fold: {:?}", step.elapsed()));
        if let Some(last) = verified_commits.last() {
            self.verify_changed_worktree_paths(
                &last.prospective,
                &verified_commits,
                branch_name,
                &git,
            )?;
        }
        trace_git_import(format!("prospective total: {:?}", total_start.elapsed()));
        Ok(ProspectiveImportPlan {
            commits: verified_commits,
            boundaries,
        })
    }

    fn verify_changed_worktree_paths(
        &self,
        prospective: &ProjectTree,
        commits: &[VerifiedImportCommit],
        branch_name: &str,
        git: &GitRepository,
    ) -> CliResult<()> {
        if git
            .head()
            .ok()
            .and_then(|head| head.shorthand().map(str::to_owned))
            .as_deref()
            != Some(branch_name)
        {
            return Ok(());
        }
        let root = git
            .workdir()
            .ok_or_else(|| git_error("checked-out import branch has no worktree"))?;
        let filter = GitAttributesFilter::for_repository(root);
        let mut changed = std::collections::BTreeSet::new();
        for commit in commits {
            for file in &commit.parsed.files {
                changed.insert(file.path.as_str());
                if let Some(old_path) = file.old_path.as_deref() {
                    changed.insert(old_path);
                }
            }
        }
        let expected: BTreeMap<_, _> = prospective
            .manifest
            .entries
            .iter()
            .map(|entry| (entry.path.as_bytes(), entry))
            .collect();
        for path in changed {
            let repo_path = RepoPath::from_bytes(path.as_bytes())
                .map_err(|error| git_error(format!("invalid changed path '{path}': {error}")))?;
            // CB-9C path fidelity: the disk holds the raw Git bytes; the
            // canonical String identity decodes before filesystem access.
            let physical = root.join(canonical_tree_path(path));
            let Some(entry) = expected.get(repo_path.as_bytes()) else {
                if physical.symlink_metadata().is_ok() {
                    return Err(git_error(format!(
                        "deleted Git path '{path}' is still present in the worktree"
                    )));
                }
                continue;
            };
            if entry.disposition != ManifestDisposition::Included
                || entry.kind != atomic_core::change::InodeKind::Regular
            {
                continue;
            }
            let bytes = fs::read(&physical).map_err(|error| {
                git_error(format!(
                    "cannot read changed worktree path '{path}': {error}"
                ))
            })?;
            let cleaned = filter.clean(Path::new(path), &bytes).map_err(|error| {
                git_error(format!(
                    "cannot clean changed worktree path '{path}': {error}"
                ))
            })?;
            if cleaned.bytes != entry.repository_bytes {
                return Err(git_error(format!(
                    "changed worktree path '{path}' does not match the verified Git tree"
                )));
            }
        }
        Ok(())
    }

    fn verify_claimed_state(
        &self,
        source: Option<&Repository>,
        branch_name: &str,
        parsed: &ParsedCommit,
        trailer: &PushTrailer,
        policy: &ConversionPolicy,
    ) -> CliResult<()> {
        let source = source.ok_or_else(|| {
            git_error(format!(
                "commit {} claims Atomic state but no Atomic repository exists",
                parsed.git_sha
            ))
        })?;
        let state_project = source
            .project_tree_at_state(branch_name, trailer.state, policy)
            .map_err(|error| {
                git_error(format!(
                    "commit {} carries an unverifiable Atomic state: {error}",
                    parsed.git_sha
                ))
            })?;
        let expected = git_object_id(policy.object_format, parsed.tree_oid)?;
        verify_prospective_equivalence(&state_project, &expected).map_err(|error| {
            git_error(format!(
                "commit {} carries a forged Atomic state: {error}",
                parsed.git_sha
            ))
        })?;
        Ok(())
    }

    fn validate_claimed_change_roots(
        &self,
        source: Option<&Repository>,
        parsed: &ParsedCommit,
        encoded_hashes: &[String],
    ) -> CliResult<()> {
        let source = source.ok_or_else(|| {
            git_error(format!(
                "commit {} claims Atomic changes but no Atomic repository exists",
                parsed.git_sha
            ))
        })?;
        for encoded in encoded_hashes {
            let hash = ContentHash::from_base32(encoded.as_bytes()).ok_or_else(|| {
                git_error(format!(
                    "commit {} carries invalid Atomic-Changes value '{encoded}'",
                    parsed.git_sha
                ))
            })?;
            source.load_change(&hash).map_err(|error| {
                git_error(format!(
                    "commit {} references unavailable Atomic change {encoded}: {error}",
                    parsed.git_sha
                ))
            })?;
        }
        Ok(())
    }

    fn verify_claimed_change_closure(
        &self,
        source: Option<&Repository>,
        branch_name: &str,
        parsed: &ParsedCommit,
        encoded_hashes: Option<&[String]>,
        policy: &ConversionPolicy,
    ) -> CliResult<()> {
        let source = source.ok_or_else(|| {
            git_error(format!(
                "commit {} claims Atomic provenance but no Atomic repository exists",
                parsed.git_sha
            ))
        })?;
        let encoded_hashes = encoded_hashes.ok_or_else(|| {
            git_error(format!(
                "commit {} claims Atomic state without an Atomic-Changes closure",
                parsed.git_sha
            ))
        })?;
        let hashes = encoded_hashes
            .iter()
            .map(|encoded| {
                ContentHash::from_base32(encoded.as_bytes()).ok_or_else(|| {
                    git_error(format!(
                        "commit {} carries invalid Atomic-Changes value '{encoded}'",
                        parsed.git_sha
                    ))
                })
            })
            .collect::<CliResult<Vec<_>>>()?;
        let closure = source
            .project_tree_for_change_closure(branch_name, &hashes, policy)
            .map_err(|error| {
                git_error(format!(
                    "commit {} carries an unverifiable Atomic change closure: {error}",
                    parsed.git_sha
                ))
            })?;
        let expected = git_object_id(policy.object_format, parsed.tree_oid)?;
        verify_prospective_equivalence(&closure, &expected).map_err(|error| {
            git_error(format!(
                "commit {} carries a forged Atomic change closure: {error}",
                parsed.git_sha
            ))
        })?;
        Ok(())
    }

    /// Import commits from a branch into an Atomic repository.
    ///
    /// Commits are processed in **batches** to keep memory bounded and show
    /// progress sooner. Each batch: parse in parallel → write sequentially.
    ///
    /// Imports use a fixed 1,000-commit batch size. This keeps progress and
    /// memory behavior predictable across small and large repositories.
    pub fn import_branch(
        &self,
        branch_name: &str,
        repo: &mut Repository,
    ) -> CliResult<ImportStats> {
        let plan = self.validate_branch_prospectively(branch_name, Some(repo))?;
        self.import_prevalidated(branch_name, repo, plan)
    }

    pub(crate) fn import_prevalidated(
        &self,
        branch_name: &str,
        repo: &mut Repository,
        plan: ProspectiveImportPlan,
    ) -> CliResult<ImportStats> {
        let working_copy = repo
            .require_working_copy_id()
            .map_err(|e| CliError::Internal(e.into()))?;
        let mut stats = ImportStats {
            commits_found: plan.commits.len(),
            commits_parsed: plan.commits.len(),
            ..ImportStats::default()
        };
        if plan.commits.is_empty() {
            return Ok(stats);
        }

        let total = plan.commits.len();
        print_info(&format!("Importing {total} preflight-verified commits..."));
        let import_start = Instant::now();
        let mut line_index = ImportLineIndex::default();
        let write_start = Instant::now();
        let (write_stats, all_imported_commits) =
            self.phase2_write(repo, &plan.commits, &mut line_index, plan.boundaries())?;
        // CB-13C F4: a phase-2 failure is a FAILED partial import — the
        // landed commits are durable and accounted in the stats; the
        // caller surfaces the failure and the aggregate still emits.
        if let Some(failure) = &write_stats.failure {
            stats.changes_written = write_stats.changes_written;
            stats.empty_commits = write_stats.empty_commits;
            stats.merge_commits = write_stats.merge_commits;
            stats.resurrected_exact = write_stats.resurrected_exact;
            stats.files_processed = write_stats.files_processed;
            let landed = stats.changes_written + stats.empty_commits + stats.merge_commits;
            // CB-13C F4: the partial failure is accounted — the aggregate
            // emits with the landed count before the error propagates.
            emit_import_synthesis(repo, &stats, Some(landed));
            return Err(CliError::Internal(anyhow::anyhow!(
                "partial import failed after {landed} landed commit(s): {failure}"
            )));
        }
        let write_elapsed = write_start.elapsed();
        stats.phase2_duration = write_elapsed;
        stats.changes_written = write_stats.changes_written;
        stats.resurrected_exact = write_stats.resurrected_exact;
        stats.empty_commits = write_stats.empty_commits;
        stats.merge_commits = write_stats.merge_commits;
        stats.self_push_skipped = write_stats.self_push_skipped;
        stats.squash_inserted = write_stats.squash_inserted;
        stats.squash_skipped = write_stats.squash_skipped;
        stats.files_processed = write_stats.files_processed;

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

        if stats.self_push_skipped > 0 {
            print_info(&format!(
                "Skipped {} commit{} created by `atomic git push` (state already in view)",
                stats.self_push_skipped,
                if stats.self_push_skipped == 1 {
                    ""
                } else {
                    "s"
                },
            ));
        }

        // Post-import classification: detect merge/squash commits and
        // create ReviewGate tags.
        if self.options.incremental && !all_imported_commits.is_empty() {
            let classify_start = Instant::now();
            match self.classify_and_tag_imports(repo, &all_imported_commits) {
                Ok(class_stats) => {
                    if class_stats.merges > 0 || class_stats.squashes > 0 {
                        print_info(&format!(
                            "Classification: {} normal, {} merges, {} squashes ({:.1}s)",
                            class_stats.normal,
                            class_stats.merges,
                            class_stats.squashes,
                            classify_start.elapsed().as_secs_f64()
                        ));
                    }
                }
                Err(e) => {
                    print_warning(&format!("Post-import classification failed: {}", e));
                }
            }
        }

        if self.options.validate_equivalence {
            // The typed prospective plan already proved the complete target
            // tree. Disk-driven reconciliation would both duplicate that work
            // and incorrectly make ambient working-copy files authoritative.
            self.phase3_finalize(&stats)?;
            return Ok(stats);
        }

        if self.options.preserve_working_copy {
            // FILE_INDEX describes the physical working copy, even while this
            // handle imports into another view. Drop stale cache entries for
            // draft-deleted files without removing their global TREE entries;
            // a later view switch must still be able to materialize them from
            // the target graph.
            let repo_root = repo.root().to_path_buf();
            for file in repo.list_tracked_files().unwrap_or_default() {
                if !repo_root.join(&file.path).exists() {
                    let _ = repo.del_file_index(working_copy, &file.path.to_string_lossy());
                }
            }
            self.phase3_finalize(&stats)?;
            return Ok(stats);
        }

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
                let _ = repo.remove(
                    working_copy,
                    &file.path,
                    atomic_repository::TrackingOptions::forced(),
                );
                let _ = repo.del_file_index(working_copy, &file.path.to_string_lossy());
                orphan_count += 1;
            }
        }

        // Direction 2: add TREE entries for files on disk NOT in TREE
        // Walk the working copy (respecting .atomicignore / .gitignore)
        // and track any untracked files.  Also populate FILE_INDEX.

        let mut new_index_entries: Vec<(String, i64, u32, u64, ContentHash)> = Vec::new();

        // Use status to find untracked files — it already handles
        // ignore rules and filesystem walking.
        if let Ok(status) = repo.status(working_copy, atomic_repository::StatusOptions::default()) {
            for entry in status.untracked() {
                let path_str = entry.path().to_string_lossy().replace('\\', "/");

                // Add to tracking
                let _ = repo.add(
                    working_copy,
                    &path_str,
                    atomic_repository::TrackingOptions::default(),
                );

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
            let _ = repo.update_file_index(working_copy, &new_index_entries);
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

    /// Determine the default import batch size.
    fn batch_size_for(_total: usize) -> usize {
        1_000
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
        self.collect_commit_oids_from_target(git_repo, target_oid)
    }

    /// Collect the commit closure behind an explicit tip instead of a branch
    /// reference (§7.5 detached-HEAD adoption: the commit exists without a
    /// local branch, and inventing refs is never an import side effect).
    pub(crate) fn collect_commit_oids_from_tip(
        &self,
        git_repo: &GitRepository,
        tip: Oid,
    ) -> CliResult<Vec<Oid>> {
        self.collect_commit_oids_from_target(git_repo, tip)
    }

    fn collect_commit_oids_from_target(
        &self,
        git_repo: &GitRepository,
        target_oid: Oid,
    ) -> CliResult<Vec<Oid>> {
        // Incremental single-branch import is the latency-sensitive path. Walk
        // the first-parent chain from the tip and stop at the imported frontier
        // instead of rev-walking and filtering the repository's entire history.
        // CB-9B: every multi-parent merge on the chain also imports the
        // complete closure of its other parents — the union state must contain
        // every parent's knowledge before the resolution is assembled.
        if self.options.incremental && self.options.mainline_only {
            let mut newest_first = Vec::new();
            let mut visited: HashSet<String> = HashSet::new();
            let mut queue = std::collections::VecDeque::new();
            queue.push_back(target_oid);
            while let Some(oid) = queue.pop_front() {
                if self.options.imported_shas.contains(&oid.to_string()) {
                    continue;
                }
                if !visited.insert(oid.to_string()) {
                    continue;
                }
                newest_first.push(oid);
                let commit = git_repo.find_commit(oid).map_err(|e| CliError::GitError {
                    message: format!("Failed to load commit {oid}: {e}"),
                })?;
                for parent in commit.parent_ids() {
                    queue.push_back(parent);
                }
            }
            newest_first.reverse();
            return Ok(newest_first);
        }

        let mut revwalk = git_repo.revwalk().map_err(|e| CliError::GitError {
            message: format!("Failed to create revwalk: {}", e),
        })?;
        revwalk.push(target_oid).map_err(|e| CliError::GitError {
            message: format!("Failed to push target to revwalk: {}", e),
        })?;
        // CB-9B: mainline_only no longer simplifies to first-parent history.
        // Multi-parent merges import every parent closure, so the complete
        // reachable closure is collected and deterministically sequenced.
        revwalk
            .set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)
            .map_err(|e| CliError::GitError {
                message: format!("Failed to set sorting: {}", e),
            })?;

        // CB-9A: collect the complete reachable closure unfiltered —
        // deterministic sequencing, the missing-object boundary check, and
        // the deepening guard all run against the full closure before
        // already-indexed commits are dropped in validate_branch_prospectively.
        let mut oids = Vec::new();
        for oid_result in revwalk {
            let oid = oid_result.map_err(|e| CliError::GitError {
                message: format!("Revwalk error: {}", e),
            })?;
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

                let parsed = parse_commit(&git_repo, *oid, idx, &oid_to_index)?;
                Ok(parsed)
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
        commits: &[VerifiedImportCommit],
        line_index: &mut ImportLineIndex,
        boundaries: &[&'static str],
    ) -> CliResult<(WriteStats, Vec<ImportedCommitInfo>)> {
        let working_copy = repo
            .require_working_copy_id()
            .map_err(|e| CliError::Internal(e.into()))?;
        let mut stats = WriteStats::default();
        let mut imported_commits = Vec::new();
        let total = commits.len();
        let phase2_start = Instant::now();
        let mut batch_start = Instant::now();
        let git = self.open_git_repo()?;
        let mut ledger = ClosureLedger::default();

        for (idx, candidate) in commits.iter().enumerate() {
            let parsed = &candidate.parsed;
            // Commits created by `atomic git push` whose referenced view
            // state is already present add nothing — skip them entirely.
            if should_skip_self_push(parsed, &self.options) {
                stats.self_push_skipped += 1;
                continue;
            }

            // Squash → insert (SPEC §4): an atomic-origin squash-merge is
            // represented by inserting its original change records into the
            // target view, not by writing a new squash change. Only attempted
            // on incremental imports (the git-shadow-sync workflow), where the
            // originals already exist in the graph.
            if self.options.incremental {
                match self.try_squash_insert(repo, parsed)? {
                    SquashDecision::Inserted(info) => {
                        stats.squash_inserted += 1;
                        stats.files_processed += parsed.files.len();
                        imported_commits.push(info);
                        continue;
                    }
                    SquashDecision::Skipped => {
                        stats.squash_skipped += 1;
                        continue;
                    }
                    SquashDecision::NotSquash => {}
                }
            }

            // CB-9A: a known valid (verified) binding for this commit takes
            // resurrection over synthesis. Trailers, names, and index rows
            // cannot reach this path: only crypto + Git-content verification.
            if let Some(binding) = self.verified_binding_for(repo, &git, parsed)? {
                let info = self.resurrect_commit(repo, &git, parsed, &binding)?;
                stats.changes_written += 1;
                stats.resurrected_exact += 1;
                stats.files_processed += parsed.files.len();
                imported_commits.push(info);
                continue;
            }

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

            // Fail closed on a partial import. Earlier successfully written
            // commits remain indexed, so an incremental retry resumes from
            // them instead of publishing a silent hole in the Git history.
            // CB-13C F4: the failure is RECORDED on the stats (not `?`ed
            // away) so the aggregate accounts for what already landed.
            let info = match self.write_commit(
                repo,
                &git,
                &mut ledger,
                parsed,
                line_index,
                &candidate.verified,
                &candidate.prospective,
                boundaries,
            ) {
                Ok(info) => info,
                Err(error) => {
                    stats.failure = Some(format!("{error}"));
                    break;
                }
            };
            if parsed.is_merge {
                stats.merge_commits += 1;
            } else if parsed.is_empty {
                stats.empty_commits += 1;
            } else {
                stats.changes_written += 1;
            }
            stats.files_processed += parsed.files.len();
            imported_commits.push(info);
        }

        // Populate the file index for all files written during this batch.
        // This lets `atomic status` compare file metadata (stat + content hash)
        // instead of reconstructing graph content for every file — reducing
        // post-import status from O(files × graph_traversal) to O(files × stat).
        use atomic_core::types::Hash;
        let repo_root = repo.root().to_path_buf();
        let mut index_entries: Vec<(String, i64, u32, u64, Hash)> = Vec::new();

        for candidate in commits {
            for file in &candidate.parsed.files {
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
            let _ = repo.update_file_index(working_copy, &index_entries);
        }

        Ok((stats, imported_commits))
    }

    /// Attempt to represent an atomic-origin squash commit by inserting its
    /// original change records into the target view instead of writing a new
    /// squash change (SPEC §4). Verifies that the insert reproduces the git
    /// tree for the touched paths; on divergence or missing originals it rolls
    /// back and skips the commit (SPEC §5, decision D1-A).
    fn try_squash_insert(
        &self,
        repo: &mut Repository,
        parsed: &ParsedCommit,
    ) -> CliResult<SquashDecision> {
        use atomic_repository::InsertOptions;

        // Merges are handled by the normal path + the phase-3 merge tag.
        if parsed.is_merge {
            return Ok(SquashDecision::NotSquash);
        }

        // Must carry Atomic-Changes trailers to be an atomic-origin squash.
        let message = parsed.full_message();
        let original_hashes = match parse_atomic_changes_trailer(&message) {
            Some(h) => h,
            None => return Ok(SquashDecision::NotSquash),
        };
        let pr_number = parse_pr_number(&message);

        // Resolve every original to a Hash and confirm it exists locally. A
        // missing original means we cannot reconstruct the aggregate — skip and
        // advise `atomic pull` rather than fabricate a squash change (SPEC §5).
        let mut parsed_hashes: Vec<ContentHash> = Vec::with_capacity(original_hashes.len());
        for h in &original_hashes {
            match ContentHash::from_base32(h.as_bytes()) {
                Some(hash) if repo.has_change(&hash) => parsed_hashes.push(hash),
                _ => {
                    print_warning(&format!(
                        "Squash {} references change {} which is not present locally; \
                         skipping (run `atomic pull` to fetch it, then re-import).",
                        parsed.short_sha, h
                    ));
                    return Ok(SquashDecision::Skipped);
                }
            }
        }

        let target = self.options.target_view.clone();

        // Insert the originals (with dependency closure) into the target view.
        // Track exactly which changes we newly added, for rollback on divergence.
        let mut newly_applied: Vec<ContentHash> = Vec::new();
        for hash in &parsed_hashes {
            let options = InsertOptions::default()
                .view(target.clone())
                .apply_deps(true);
            match repo.insert_change_rec(hash, options) {
                Ok(outcome) => {
                    newly_applied.extend(outcome.stats.applied_hashes.iter().copied());
                }
                Err(e) => {
                    print_warning(&format!(
                        "Squash {}: failed to insert original {}: {}; skipping.",
                        parsed.short_sha,
                        &hash.to_base32()[..12],
                        e
                    ));
                    rollback_inserts(repo, &target, &newly_applied);
                    return Ok(SquashDecision::Skipped);
                }
            }
        }

        // Verify the insert reproduces the git tree for every touched path
        // (SPEC §5 / D1-A). Divergence means a human resolved a conflict in the
        // PR; we must not silently desync the shared view from git.
        if !self.squash_insert_matches_tree(repo, parsed, &target) {
            print_warning(&format!(
                "Squash {}: inserting its originals diverges from the git tree \
                 (manual conflict resolution?); skipping. The shared view was \
                 left unchanged.",
                parsed.short_sha
            ));
            rollback_inserts(repo, &target, &newly_applied);
            return Ok(SquashDecision::Skipped);
        }

        // Anchor the git SHA to the tip original so re-imports resolve it and
        // `atomic change <sha>` works; the ReviewGate tag owns full provenance.
        let tip = parsed_hashes.last().copied().unwrap_or(ContentHash::ZERO);
        if tip != ContentHash::ZERO {
            let _ = repo.index_git_sha(&parsed.git_sha, &tip);
        }

        // Persist the squash commit's interpreted closure (review R1): the
        // originals and their dependency closures. Descendants imported in
        // later runs read this row to keep the originals visible in the
        // exact parent visibility even though the originals' own Git SHAs
        // are not the squash commit's ancestors.
        if let Err(error) = repo.record_git_commit_closure(&parsed.git_sha, &parsed_hashes) {
            print_warning(&format!(
                "Squash {}: could not persist its interpretation closure ({error}); \
                 later incremental imports may refuse descendants of it.",
                parsed.short_sha
            ));
        }

        Ok(SquashDecision::Inserted(ImportedCommitInfo {
            git_sha: parsed.git_sha.clone(),
            short_sha: parsed.short_sha.clone(),
            atomic_hash: tip,
            is_merge: false,
            message,
            squash_insert: Some(SquashInsert {
                original_hashes,
                pr_number,
            }),
        }))
    }

    /// Whether the target view's materialized content for every path the squash
    /// touches matches the git tree recorded in `parsed`.
    fn squash_insert_matches_tree(
        &self,
        repo: &Repository,
        parsed: &ParsedCommit,
        target: &str,
    ) -> bool {
        for file in &parsed.files {
            match file.operation {
                FileOperation::Deleted => {
                    // The path must be absent (or empty) in the target view.
                    if let Ok(Some(content)) = repo.get_file_content_on_view(&file.path, target) {
                        if !content.is_empty() {
                            return false;
                        }
                    }
                }
                _ => {
                    let expected = match &file.new_content {
                        Some(c) => c,
                        None => continue,
                    };
                    match repo.get_file_content_on_view(&file.path, target) {
                        Ok(Some(actual)) if &actual == expected => {}
                        _ => return false,
                    }
                }
            }
        }
        true
    }

    /// Write a single commit to the repository.
    ///
    /// CB-9A routes root and single-parent commits through normal
    /// recorded-file assembly with a hashed Git origin
    /// ([`Self::write_commit_synthesized`]). Multi-parent commits keep the
    /// legacy graph-first path pending CB-9B merge semantics; empty commits
    /// keep the dedicated empty-commit writer pending CB-9C.
    ///
    /// Returns metadata about the imported commit for post-import
    /// classification and ReviewGate tagging.
    #[allow(clippy::too_many_arguments)]
    fn write_commit(
        &self,
        repo: &mut Repository,
        git: &GitRepository,
        ledger: &mut ClosureLedger,
        parsed: &ParsedCommit,
        _line_index: &mut ImportLineIndex,
        verified: &VerifiedProspectiveEquivalence,
        prospective: &atomic_repository::ProjectTree,
        boundaries: &[&'static str],
    ) -> CliResult<ImportedCommitInfo> {
        // CB-9B F1-adjacent sibling import: a Git commit already interpreted
        // locally (same full SHA) has exactly one Atomic change. Re-synthesizing
        // it in a second branch view allocates a second incarnation with new
        // graph inodes, and the two claimants then surface as an unresolved name
        // conflict for the edited paths (`git import --all` over sibling
        // branches sharing a base). Reference the existing change into the
        // target view instead: metadata-only, dependency-closed, and the exact
        // same interpretation every view sees.
        if let Some(existing) = repo
            .checked_git_sha(&parsed.git_sha)
            .map_err(|e| CliError::Internal(e.into()))?
        {
            repo.ensure_union_state(&self.options.target_view, &[existing])
                .map_err(|e| CliError::Internal(e.into()))?;
            ledger.record_introduced(&parsed.git_sha, existing, &self.options.target_view);
            trace_git_import(format!(
                "write {} reuse existing change {} (commit already interpreted locally; \
                 referenced into view '{}')",
                parsed.short_sha,
                existing.to_base32(),
                self.options.target_view
            ));
            return Ok(ImportedCommitInfo {
                git_sha: parsed.git_sha.clone(),
                short_sha: parsed.short_sha.clone(),
                atomic_hash: existing,
                is_merge: parsed.is_merge,
                message: parsed.full_message(),
                squash_insert: None,
            });
        }
        // CB-9A: root and single-parent commits use normal recorded-file
        // assembly with a hashed Git origin. CB-9B: multi-parent merges
        // assemble a journaled `GitResolution` from the union state, and
        // empty commits stay distinct through the synthesized empty-commit
        // writer.
        let info = if parsed.is_merge {
            self.write_merge_resolution(
                repo,
                git,
                ledger,
                parsed,
                verified,
                prospective,
                boundaries,
            )?
        } else if !parsed.is_empty {
            self.write_commit_synthesized(
                repo,
                git,
                ledger,
                parsed,
                verified,
                prospective,
                boundaries,
            )?
        } else {
            self.write_empty_commit_synthesized(
                repo,
                git,
                ledger,
                parsed,
                verified,
                prospective,
                boundaries,
            )?
        };
        ledger.record_introduced(&parsed.git_sha, info.atomic_hash, &self.options.target_view);
        Ok(info)
    }

    /// Locate a fully verified Git-state binding for one parsed commit
    /// (CB-9A: resurrection over synthesis), with the CB-6C checked-cache
    /// contract: the resolution either returns a validated immutable binding
    /// (the only adoption-authorized result), an explicit unresolved verdict
    /// (a stored binding names this commit but failed verification — never
    /// synthesized over), or nothing (cold path).
    fn verified_binding_for(
        &self,
        repo: &Repository,
        git: &GitRepository,
        parsed: &ParsedCommit,
    ) -> CliResult<Option<atomic_repository::git_binding::GitStateBinding>> {
        use atomic_repository::GitShaResolution;
        let algorithm = git_algorithm_for_sha(&parsed.git_sha)?;
        let commit = git_object_id_for_sha(algorithm, &parsed.git_sha)?;
        match repo
            .resolve_git_sha(git, &commit)
            .map_err(|e| CliError::Internal(e.into()))?
        {
            atomic_repository::GitShaResolution::VerifiedBinding { binding, .. } => {
                Ok(Some(binding))
            }
            atomic_repository::GitShaResolution::Unresolved { detail } => Err(git_error(format!(
                "a stored binding claims commit {} but failed verification; refusing to \
                 synthesize over a known binding claim (recoverable rejection): {detail}",
                parsed.git_sha
            ))),
            atomic_repository::GitShaResolution::Cold => Ok(None),
        }
    }

    /// Resurrect a verified binding's closure into the target view instead
    /// of synthesizing from Git (CB-9A/CB-6C).
    ///
    /// The exact path restores the original changes (hashes, bytes,
    /// dependencies, frontiers, semantic identities, order, Merkle, SetId),
    /// completing the closure through the CB-6B transport when needed, and
    /// proves the recomputed projection against the bound tree before any
    /// membership is published.
    fn resurrect_commit(
        &self,
        repo: &mut Repository,
        git: &GitRepository,
        parsed: &ParsedCommit,
        binding: &atomic_repository::git_binding::GitStateBinding,
    ) -> CliResult<ImportedCommitInfo> {
        let outcome = repo
            .resurrect_binding_exact(git, binding, &self.options.target_view, None)
            .map_err(|e| CliError::Internal(e.into()))?;
        // Persist the resurrected commit's interpreted closure (review R1):
        // every restored or already-present change with its dependency
        // closure, so descendants in later runs keep the exact parent
        // visibility even though restored changes may not carry resolvable
        // SHA provenance.
        let mut restored_hashes: Vec<ContentHash> = outcome.inserted.clone();
        restored_hashes.extend(outcome.already_present.iter().copied());
        if !restored_hashes.is_empty() {
            if let Err(error) = repo.record_git_commit_closure(&parsed.git_sha, &restored_hashes) {
                print_warning(&format!(
                    "Resurrection {}: could not persist its interpretation closure ({error}); \
                     later incremental imports may refuse descendants of it.",
                    parsed.short_sha
                ));
            }
        }
        print_info(&format!(
            "Resurrected verified binding for {} ({} change(s) restored, {} already present{}), \
             tree proof {}",
            parsed.short_sha,
            outcome.inserted.len(),
            outcome.already_present.len(),
            if outcome.restored_raw_commit {
                ", raw commit restored"
            } else {
                ""
            },
            if outcome.proof.projected_tree == outcome.proof.bound_tree {
                "verified"
            } else {
                "MISMATCH"
            },
        ));
        let atomic_hash = outcome
            .inserted
            .last()
            .or(outcome.already_present.last())
            .copied()
            .ok_or_else(|| {
                git_error(format!(
                    "binding {} for {} carries no changes to resurrect",
                    binding.id(),
                    parsed.git_sha
                ))
            })?;
        Ok(ImportedCommitInfo {
            git_sha: parsed.git_sha.clone(),
            short_sha: parsed.short_sha.clone(),
            atomic_hash,
            is_merge: false,
            message: parsed.full_message(),
            squash_insert: None,
        })
    }

    /// Synthesize one root/single-parent commit through normal recorded-file
    /// assembly (CB-9A/CB-9B).
    ///
    /// Each file is diffed against the bytes the commit's Git parent's
    /// interpreted closure holds — never against a sibling merge leg's
    /// content (review blocker 1). When sibling lineages are present in the
    /// target view (this-run ledger), the assembly visibility excludes them,
    /// so the leg anchors only within its own parent's closure and cannot
    /// record a false dependency on a sibling commit. Each result is then
    /// verified against the commit's Git tree through the staged projection
    /// check before the change becomes a visible, verified view member
    /// (review blocker 2). The change carries `ChangeOrigin::GitSynthesized`
    /// with the tagged commit/tree OIDs, complete ordered parents, and
    /// derivation, plus lossless raw foreign facts (review blocker 6); Git
    /// diff lines are never used to override the semantic layer, and Git
    /// parents never become Atomic dependencies.
    #[allow(clippy::too_many_arguments)]
    fn write_commit_synthesized(
        &self,
        repo: &mut Repository,
        git: &GitRepository,
        ledger: &mut ClosureLedger,
        parsed: &ParsedCommit,
        verified: &VerifiedProspectiveEquivalence,
        prospective: &atomic_repository::ProjectTree,
        boundaries: &[&'static str],
    ) -> CliResult<ImportedCommitInfo> {
        use atomic_core::output::memory::Memory;
        use atomic_core::record::workflow::{
            record_added_file, record_deleted_file, record_modified_file, DetectedFile,
            RecordingOptions,
        };

        let working_copy = repo
            .require_working_copy_id()
            .map_err(|e| CliError::Internal(e.into()))?;
        let commit_start = std::time::Instant::now();
        let target = self.options.target_view.clone();

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

        // Hashed origin facts: tagged commit/tree OIDs, complete ordered
        // parents, derivation. Git parents are ancestry evidence only.
        // CB-9B: a squash-shaped unbound commit (squash-merge message format,
        // no Atomic trailers) synthesizes one `Derivation::Squash`
        // interpretation instead of FirstParent.
        let algorithm = git_algorithm_for_sha(&parsed.git_sha)?;
        let squash_shaped = parse_squash_merge_format(&parsed.full_message()).is_some();
        let origin = if parsed.parent_oids.is_empty() {
            GitSynthesisOrigin::with_derivation(
                git_object_id_for_sha(algorithm, &parsed.git_sha)?,
                git_object_id(algorithm, parsed.tree_oid)?,
                Vec::new(),
                if squash_shaped {
                    GitDerivation::Squash
                } else {
                    GitDerivation::Root
                },
            )
        } else {
            if parsed.parent_oids.len() > 1 {
                return Err(git_error(format!(
                    "commit {} has {} parents; multi-parent commits take the GitResolution \
                     path, not single-commit synthesis",
                    parsed.git_sha,
                    parsed.parent_oids.len()
                )));
            }
            GitSynthesisOrigin::with_derivation(
                git_object_id_for_sha(algorithm, &parsed.git_sha)?,
                git_object_id(algorithm, parsed.tree_oid)?,
                vec![git_object_id(algorithm, parsed.parent_oids[0])?],
                if squash_shaped {
                    GitDerivation::Squash
                } else {
                    GitDerivation::FirstParent
                },
            )
        }
        .map_err(|e| CliError::Internal(e.into()))?;

        // CB-9B: rewrite evidence is event/binding-driven, not only
        // message-shaped. A captured post-rewrite event naming this commit
        // (full OID match), or a locally verified binding carrying the same
        // tree (advisory hint found independently of hooks), produces a
        // reviewable RewriteCandidate link. The link is evidence for review
        // only — never identity (RFC §5.4).
        let evidence = self.rewrite_evidence_for(repo, git, parsed)?;
        if !evidence.is_empty() {
            self.record_rewrite_candidate_link(repo, parsed, &evidence)?;
        }

        // CB-9B closure assembly context (review blocker 1 / R1): view
        // members that are NOT part of this commit's interpreted closure —
        // reconstructed from the persisted closure rows (across runs,
        // bindings, and views) plus the checked SHA index — are excluded
        // from the assembly visibility, so this leg anchors only within its
        // own Git parent's interpreted closure with Git parent-tree
        // baselines. Full ancestor chains participate through the filter.
        let excluded = ledger.exclusions_for(
            repo,
            git,
            &parsed.git_sha,
            &target,
            !parsed.parent_oids.is_empty(),
        )?;
        let leg_mode = !excluded.is_empty();

        enum EffectiveOp {
            Added,
            ModifiedFullReplace,
            Modified,
            Deleted,
            Renamed,
            Skip,
        }
        // Baseline bytes per file (review R1/R2):
        // - The commit's own Git parent-tree bytes captured in Phase 1 — the
        //   exact parent interpretation — are the baseline whenever Phase 1
        //   captured them. The staged verification proves the applied graph
        //   renders exactly the commit tree, so sibling content can never
        //   enter the delta.
        // - A modified path with no captured parent bytes falls back to the
        //   exact assembly view bytes (excluded lineages invisible, storage
        //   errors propagated — fail closed, no fabricated baselines).
        // - No conflict-marker screening skips an edit: every foreign delta
        //   is recorded against its exact baseline (review R2, "retain all
        //   foreign deltas"); structural conflicts surface through the graph
        //   and the fail-closed staged equality, never through a silent skip.
        //
        // CB-9C attribute fidelity: every non-deleted file also carries the
        // graph-backed attribute writes (mode/kind) the Git delta requires.
        // Expected materialization derives from the canonical Git mode;
        // current attributes project through the exact assembly visibility,
        // so a chmod-only or kind-only delta records a standalone attribute
        // change instead of silently losing the mode (or failing the staged
        // tree check after publishing nothing).
        #[allow(clippy::type_complexity)] // per-file effective op row
        let effective: CliResult<Vec<(&ParsedFile, EffectiveOp, Vec<u8>, Vec<atomic_core::change::InodeAttr>)>> = parsed
            .files
            .iter()
            .map(|file| {
                // The exact assembly view bytes (excluded lineages invisible,
                // storage errors propagated — fail closed, never swallowed).
                let view_baseline: Option<Vec<u8>> = repo
                    .get_file_content_on_view_excluding(&file.path, &target, &excluded)
                    .map_err(|e| {
                        CliError::Internal(anyhow::anyhow!(
                            "cannot read exact baseline for '{}': {e}",
                            file.path
                        ))
                    })?;
                // Git parent-tree bytes first (the exact parent
                // interpretation); the exact assembly view render second.
                let baseline: Option<Vec<u8>> =
                    file.old_content.clone().or(view_baseline.clone());
                // Expected graph-backed attributes from the commit's Git mode
                // (CB-9C): executable bits, symlink kinds, and gitlink kinds
                // must project exactly or the staged tree check refuses.
                let expected = match file.new_mode {
                    Some(mode) if !matches!(file.operation, FileOperation::Deleted) => {
                        Some(expected_materialization(mode)?)
                    }
                    _ => None,
                };
                let projected = if file.new_mode.is_some() {
                    repo.projected_attributes_on_view_excluding(&file.path, &target, &excluded)
                        .map_err(|e| {
                            CliError::Internal(anyhow::anyhow!(
                                "cannot project current attributes for '{}': {e}",
                                file.path
                            ))
                        })?
                } else {
                    None
                };
                if let Some(projected) = &projected {
                    if projected.is_conflicted() {
                        return Err(CliError::Internal(anyhow::anyhow!(
                            "path '{}' has conflicted inode attribute registers in view '{}'; \
                             refusing to synthesize attributes over an unresolved register",
                            file.path, target
                        )));
                    }
                }
                let attrs = match (&expected, projected.as_ref().map(|p| p.materialization)) {
                    (Some(expected), Some(current)) => {
                        let mut attrs = Vec::new();
                        if current.mode != expected.mode() {
                            attrs.push(atomic_core::change::InodeAttr::Mode(expected.mode()));
                        }
                        if current.kind != expected.kind() {
                            attrs.push(atomic_core::change::InodeAttr::Kind(expected.kind()));
                        }
                        attrs
                    }
                    (Some(expected), None) => {
                        vec![
                            atomic_core::change::InodeAttr::Mode(expected.mode()),
                            atomic_core::change::InodeAttr::Kind(expected.kind()),
                        ]
                    }
                    (None, _) => Vec::new(),
                };
                let op = match file.operation {
                    FileOperation::Added | FileOperation::Copied => {
                        if let Some(bytes) = &view_baseline {
                            if bytes.as_slice() == file.new_content.as_deref().unwrap_or(&[]) {
                                if attrs.is_empty() {
                                    // The view already renders the new bytes (the
                                    // atomic-origin projection of the same
                                    // content): the commit contributes no graph
                                    // delta.
                                    EffectiveOp::Skip
                                } else {
                                    // Bytes match but the graph-backed
                                    // attributes differ (chmod-only or
                                    // kind-only delta): record the attribute
                                    // change, not a silent no-op.
                                    EffectiveOp::Modified
                                }
                            } else if baseline.is_some() {
                                // The path exists in the parent's tree (or
                                // the view holds a superseded incarnation —
                                // rewritten history): the delta is the
                                // baseline bytes → the new tree.
                                EffectiveOp::Modified
                            } else {
                                trace_git_import(format!(
                                    "reclassify {} path={} added→modified-replace (path already bound in view '{}')",
                                    parsed.short_sha, file.path, target
                                ));
                                EffectiveOp::ModifiedFullReplace
                            }
                        } else {
                            EffectiveOp::Added
                        }
                    }
                    FileOperation::Modified => {
                        if baseline.is_none() {
                            return Err(git_error(format!(
                                "commit {} classifies '{}' as modified but neither the Git \
                                 parent tree nor the exact assembly closure holds it; \
                                 refusing to fabricate a baseline",
                                parsed.git_sha, file.path
                            )));
                        }
                        EffectiveOp::Modified
                    }
                    FileOperation::Deleted => {
                        if view_baseline.is_some() || baseline.is_some() {
                            EffectiveOp::Deleted
                        } else {
                            trace_git_import(format!(
                                "reclassify {} path={} deleted→skip (path unbound in view '{}')",
                                parsed.short_sha, file.path, target
                            ));
                            EffectiveOp::Skip
                        }
                    }
                    FileOperation::Renamed => EffectiveOp::Renamed,
                };
                Ok((file, op, baseline.unwrap_or_default(), attrs))
            })
            .collect();
        let effective = effective?;

        // Review CB-9C R3: Git rename detection pairs deleted/added
        // candidates greedily. When several removed sources are
        // byte-identical AND several added destinations are byte-identical,
        // every bijection between them renders the same tree, so the
        // specific pairing Git emitted carries no causal information. Those
        // renames are recorded as delete+add with RenameUnresolved evidence
        // instead of a structural FileMove that would fabricate inode
        // identity. Multiplicities are keyed by exact bytes (the deltas are
        // already in memory); only rename-relevant operations participate.
        let mut old_signature_multiplicity: std::collections::HashMap<&[u8], usize> =
            std::collections::HashMap::new();
        let mut new_signature_multiplicity: std::collections::HashMap<&[u8], usize> =
            std::collections::HashMap::new();
        for (file, op, baseline, _attrs) in &effective {
            match op {
                EffectiveOp::Renamed => {
                    *old_signature_multiplicity
                        .entry(baseline.as_slice())
                        .or_default() += 1;
                    if let Some(bytes) = file.new_content.as_deref() {
                        *new_signature_multiplicity.entry(bytes).or_default() += 1;
                    }
                }
                EffectiveOp::Deleted => {
                    *old_signature_multiplicity
                        .entry(baseline.as_slice())
                        .or_default() += 1;
                }
                EffectiveOp::Added => {
                    if let Some(bytes) = file.new_content.as_deref() {
                        *new_signature_multiplicity.entry(bytes).or_default() += 1;
                    }
                }
                _ => {}
            }
        }
        let renamed_pairing_is_ambiguous = |old_bytes: &[u8], new_bytes: &[u8]| -> bool {
            // Review CB-9C R3 (re-review EYL): alternatives on EITHER
            // endpoint make the greedy pairing non-causal. Two byte-identical
            // deleted sources with one added destination (2→1) could have
            // paired either source; one deleted source with two byte-identical
            // added destinations (1→2) could have paired either destination.
            // In both cases the greedy pairing carries no causal information,
            // so the rename downgrades to canonical delete+add with
            // RenameUnresolved. The multiplicity maps count Deleted and Added
            // operations alongside Renamed, so either-side alternatives are
            // visible here (OR is the minimum identical-byte correction).
            old_signature_multiplicity
                .get(old_bytes)
                .copied()
                .unwrap_or(0)
                > 1
                || new_signature_multiplicity
                    .get(new_bytes)
                    .copied()
                    .unwrap_or(0)
                    > 1
        };

        // Pre-register added paths so tracking metadata exists before
        // recording (same hygiene as the legacy path). Fail closed: a
        // tracking failure must not silently produce an untracked import.
        let mut added_paths: Vec<&str> = Vec::new();
        let mut deleted_paths: Vec<String> = Vec::new();
        for (file, op, _baseline, _attrs) in &effective {
            match op {
                EffectiveOp::Added => {
                    // CB-9C: gitlinks and symlinks are not ordinary disk
                    // files to track — a gitlink directory is foreign state
                    // and a dangling symlink cannot be read. Their graph
                    // FileAdd creates the projection; pre-registering them
                    // here would make disk-driven tracking fail the import.
                    if matches!(file.new_mode, Some(0o160000 | 0o120000)) {
                        continue;
                    }
                    added_paths.push(&file.path);
                }
                EffectiveOp::Deleted => deleted_paths.push(file.path.clone()),
                _ => {}
            }
        }
        if !self.options.preserve_working_copy && !added_paths.is_empty() {
            repo.add_batch(working_copy, &added_paths)
                .map_err(|e| CliError::Internal(e.into()))?;
        }

        // Record every file through the normal workflow: the commit's own
        // baseline bytes as the diff side, normal diff, semantic FileOps.
        // Git's captured diff lines are kept as unhashed provenance but
        // never override the semantic layer. Recording failures fail closed
        // (review blocker 2): an empty or failed record aborts the commit.
        // CB-9C: imported blobs are content-addressed and opaque-trunked —
        // the local-record size cap must not refuse a large tracked binary
        // (the 500 MB opaque corpus).
        let core_options = RecordingOptions::new().unlimited_file_size();
        let mut recorded_files: Vec<RecordedFile> = Vec::new();
        // CB-9C move evidence: Git rename detection is similarity-based, so
        // every imported rename records ProbableMove evidence (never
        // authoritative identity), and renames that fell back to
        // delete+add record an explicit RenameUnresolved loss.
        let mut move_evidence = MoveEvidence::new();
        let record_start = std::time::Instant::now();
        for (file, op, baseline, attrs) in &effective {
            match op {
                EffectiveOp::ModifiedFullReplace => {
                    // The view already binds this path with foreign content
                    // (rewritten history): record the whole-content delta
                    // from the view's bytes to the new tree through the
                    // graph-safe path, so semantic FileOps and stable CRDT
                    // identities are regenerated instead of the opaque
                    // generated-file shortcut (review blocker 3).
                    let new_content = file.new_content.as_deref().ok_or_else(|| {
                        git_error(format!(
                            "commit {} omitted bytes for modified path '{}'",
                            parsed.git_sha, file.path
                        ))
                    })?;
                    let mut rec = repo
                        .record_resolution_modified_file(&file.path, new_content, &target, false)
                        .map_err(|message| {
                            CliError::Internal(anyhow::anyhow!(
                                "cannot record modified path '{}': {message}",
                                file.path
                            ))
                        })?;
                    if rec.is_empty() && !attrs.is_empty() {
                        // The view already renders the new bytes but the
                        // graph-backed attributes differ: the attribute
                        // writes alone are the delta (CB-9C).
                        let mut attr_rec = RecordedFile::new(&file.path);
                        attr_rec.set_kind(atomic_core::record::workflow::DetectionKind::Modified);
                        if let Some((inode, position)) = repo
                            .get_inode_and_position(&file.path)
                            .map_err(|e| CliError::Internal(e.into()))?
                        {
                            attr_rec.set_inode(inode);
                            attr_rec.set_position(position);
                        }
                        for value in attrs {
                            attr_rec.set_attr(*value);
                        }
                        recorded_files.push(attr_rec);
                        continue;
                    }
                    if rec.is_empty() {
                        // The view already renders the new bytes (e.g. the
                        // atomic-origin projection of the same content): the
                        // commit contributes no graph delta, so the skip is
                        // legitimate. An empty record with a real delta is a
                        // failure and stays an error below.
                        if baseline.as_slice() != new_content {
                            return Err(CliError::Internal(anyhow::anyhow!(
                                "cannot record modified path '{}': record is empty",
                                file.path
                            )));
                        }
                        trace_git_import(format!(
                            "reclassify {} path={} modified→no-delta (view already renders the new bytes)",
                            parsed.short_sha, file.path
                        ));
                        continue;
                    }
                    for value in attrs {
                        rec.set_attr(*value);
                    }
                    recorded_files.push(rec);
                }

                EffectiveOp::Added => {
                    let content = file.new_content.as_deref().ok_or_else(|| {
                        git_error(format!(
                            "commit {} omitted bytes for added path '{}'",
                            parsed.git_sha, file.path
                        ))
                    })?;
                    let memory_wc = Memory::new();
                    memory_wc.add_file(&file.path, content);
                    let detected = DetectedFile::added(&file.path);
                    match record_added_file(&memory_wc, &detected, &core_options) {
                        Ok(mut rec) if !rec.is_empty() => {
                            // CB-9C: carry the Git mode/kind as graph-backed
                            // attribute writes so the staged projection holds
                            // executable bits, symlinks, and gitlinks.
                            for value in attrs {
                                rec.set_attr(*value);
                            }
                            recorded_files.push(rec);
                        }
                        Ok(_) => {
                            return Err(CliError::Internal(anyhow::anyhow!(
                                "cannot record added path '{}': record is empty",
                                file.path
                            )));
                        }
                        Err(message) => {
                            return Err(CliError::Internal(anyhow::anyhow!(
                                "cannot record added path '{}': {message}",
                                file.path
                            )));
                        }
                    }
                }

                EffectiveOp::Renamed => {
                    let old_path = file.old_path.as_deref().unwrap_or(&file.path);
                    let parent_path = Path::new(&file.path)
                        .parent()
                        .and_then(|p| p.to_str())
                        .unwrap_or("");
                    let can_emit_move = parent_path.is_empty()
                        || repo
                            .get_inode_and_position(parent_path)
                            .map_err(|e| CliError::Internal(e.into()))?
                            .is_some();
                    let inode_pos = repo
                        .get_inode_and_position(old_path)
                        .map_err(|e| CliError::Internal(e.into()))?;
                    let new_bytes = file.new_content.as_deref().unwrap_or(&[]);
                    let ambiguous = renamed_pairing_is_ambiguous(baseline.as_slice(), new_bytes);
                    match (can_emit_move, inode_pos) {
                        (true, Some((inode, _pos))) if !ambiguous => {
                            // CB-9C: the structural FileMove is a
                            // policy-selected route (RFC 5.3.8) through the
                            // old path's stable binding — that binding proves
                            // the OLD path existed with stable identity, NOT
                            // that Git's similarity pairing is the causal
                            // move. The evidence therefore stays ProbableMove
                            // (review CB-9C R3): AuthoritativeMove is
                            // reserved for independently established move
                            // authority, and a heuristic pairing — however
                            // deterministic — is never that.
                            let old_bytes = baseline.as_slice();
                            let score = similarity_bps(old_bytes, new_bytes);
                            let basis = if old_bytes == new_bytes {
                                MoveBasis::ByteIdentity
                            } else {
                                MoveBasis::ContentSimilarity
                            };
                            move_evidence.insert_probable(ProbableMove::new(
                                old_path,
                                file.path.as_str(),
                                inode,
                                score,
                                basis,
                            ));
                            // CB-9C R2 (re-review EYL): route the rename
                            // through the native `record_moved_file`
                            // pipeline. The old path's stable inode, position,
                            // CRDT trunk, alive branches and exact source
                            // claim are resolved inside; the record carries a
                            // real semantic `TrunkOp::Move` (plus destination
                            // line ops for a move+edit), so a reopened
                            // repository maps the DESTINATION path to the
                            // moved trunk instead of leaving the trunk alive
                            // at the old path with no destination mapping.
                            let new_content_owned =
                                file.new_content.as_deref().unwrap_or_default().to_vec();
                            let old_content = if leg_mode {
                                baseline.clone()
                            } else {
                                self.old_bytes_for(repo, file)
                            };
                            let mut move_rec = repo
                                .record_foreign_moved_file(
                                    &target,
                                    &file.path,
                                    old_path,
                                    &old_content,
                                    &new_content_owned,
                                    false,
                                )
                                .map_err(|message| {
                                    CliError::Internal(anyhow::anyhow!(
                                        "cannot record renamed path '{}': {message}",
                                        file.path
                                    ))
                                })?;
                            // CB-9C: attribute continuity follows the stable
                            // inode — a rename that also chmods or converts
                            // kind emits the attribute delta on the moved
                            // record.
                            for value in attrs {
                                move_rec.set_attr(*value);
                            }
                            recorded_files.push(move_rec);
                        }
                        _ => {
                            // Fall back to delete-old + add-new so the
                            // imported history stays faithful and the final
                            // tree remains clean. The evidence records this
                            // as a loss, not as a resolved move: either the
                            // pairing had no stable binding to route through,
                            // or the candidate set was ambiguous — several
                            // byte-identical sources and destinations where
                            // Git's greedy pairing carries no causal
                            // information (review CB-9C R3). The loss note
                            // lists the competing identical candidates the
                            // greedy match discarded alongside this delta's
                            // own candidate.
                            if old_path != file.path {
                                deleted_paths.push(old_path.to_string());
                                // The downgrade is real delete+add semantics:
                                // the old path must carry a canonical FileDel
                                // record like a plain imported deletion, bound
                                // to the actual deleted trunk (review CB-9C
                                // R2) so the semantic lifecycle tombstones
                                // instead of leaving the old trunk Alive.
                                let deleted_record =
                                    repo.record_foreign_deleted_file(old_path).map_err(
                                        |message| CliError::Internal(anyhow::anyhow!(message)),
                                    )?;
                                if deleted_record.is_empty() {
                                    return Err(CliError::Internal(anyhow::anyhow!(
                                        "cannot import ambiguous rename source '{}': canonical FileDel is empty",
                                        old_path
                                    )));
                                }
                                recorded_files.push(deleted_record);
                                let old_bytes = baseline.as_slice();
                                let score = similarity_bps(old_bytes, new_bytes);
                                let basis = if old_bytes == new_bytes {
                                    MoveBasis::ByteIdentity
                                } else {
                                    MoveBasis::ContentSimilarity
                                };
                                // Review CB-9C R3 (re-review EYL): the loss
                                // note retains the complete competing candidate
                                // RELATION, not only the pairs the greedy
                                // match consumed. For a byte-identical
                                // ambiguous set, every removed source renders
                                // the same tree paired with EVERY added
                                // destination, so the relation is the bounded
                                // cartesian product of the byte-identical
                                // source and destination paths — including the
                                // cross-alternatives (a→d, b→c) the greedy
                                // match discarded.
                                let mut competing_sources: Vec<String> = vec![old_path.to_string()];
                                let mut competing_destinations: Vec<String> =
                                    vec![file.path.clone()];
                                for (other, other_op, other_baseline, _other_attrs) in &effective {
                                    if std::ptr::eq(other as *const _, file as *const _) {
                                        continue;
                                    }
                                    let other_new = other.new_content.as_deref();
                                    match other_op {
                                        EffectiveOp::Renamed => {
                                            if other_baseline.as_slice() == old_bytes {
                                                let source = other
                                                    .old_path
                                                    .clone()
                                                    .unwrap_or_else(|| other.path.clone());
                                                if !competing_sources.contains(&source) {
                                                    competing_sources.push(source);
                                                }
                                            }
                                            if other_new == Some(new_bytes)
                                                && !competing_destinations.contains(&other.path)
                                            {
                                                competing_destinations.push(other.path.clone());
                                            }
                                        }
                                        EffectiveOp::Deleted => {
                                            if other_baseline.as_slice() == old_bytes
                                                && !competing_sources.contains(&other.path)
                                            {
                                                competing_sources.push(other.path.clone());
                                            }
                                        }
                                        EffectiveOp::Added
                                            if other_new == Some(new_bytes)
                                                && !competing_destinations
                                                    .contains(&other.path) =>
                                        {
                                            competing_destinations.push(other.path.clone());
                                        }
                                        _ => {}
                                    }
                                }
                                let mut candidates = Vec::new();
                                for source in &competing_sources {
                                    for destination in &competing_destinations {
                                        candidates.push(RenameCandidate::new(
                                            source,
                                            destination,
                                            score,
                                            basis,
                                        ));
                                    }
                                }
                                move_evidence.insert_loss(LossNote::rename_unresolved(candidates));
                            }
                            let content =
                                match file.new_content.as_deref().or(file.old_content.as_deref()) {
                                    Some(c) => c,
                                    None => continue,
                                };
                            let memory_wc = Memory::new();
                            memory_wc.add_file(&file.path, content);
                            let detected = DetectedFile::added(&file.path);
                            match record_added_file(&memory_wc, &detected, &core_options) {
                                Ok(mut rec) if !rec.is_empty() => {
                                    for value in attrs {
                                        rec.set_attr(*value);
                                    }
                                    recorded_files.push(rec);
                                }
                                Ok(_) => {
                                    return Err(CliError::Internal(anyhow::anyhow!(
                                        "cannot record renamed path '{}': record is empty",
                                        file.path
                                    )));
                                }
                                Err(message) => {
                                    return Err(CliError::Internal(anyhow::anyhow!(
                                        "cannot record renamed path '{}': {message}",
                                        file.path
                                    )));
                                }
                            }
                        }
                    }
                }

                EffectiveOp::Modified => {
                    let new_content = file.new_content.as_deref().ok_or_else(|| {
                        git_error(format!(
                            "commit {} omitted bytes for modified path '{}'",
                            parsed.git_sha, file.path
                        ))
                    })?;
                    let memory_wc = Memory::new();
                    memory_wc.add_file(&file.path, new_content);

                    // The baseline is the commit's own context: in leg mode
                    // the Git parent-tree bytes (sibling content excluded),
                    // otherwise the bytes the repository holds with Git's
                    // parent-tree fallback.
                    let old_content = if leg_mode {
                        baseline.clone()
                    } else {
                        self.old_bytes_for(repo, file)
                    };
                    let mut detected = DetectedFile::modified(&file.path);
                    if let Ok(Some((inode, pos))) = repo.get_inode_and_position(&file.path) {
                        detected.inode = Some(inode);
                        detected.position = Some(pos);
                    }
                    // Bind the edit to the file's existing CRDT trunk and
                    // alive branches (review F4): without the existing
                    // semantic identities the modify would allocate
                    // placeholder branches in a parallel trunk and the
                    // semantic layer could not reconstruct the file across
                    // commits. Falls back to the plain record only when the
                    // path has no inode binding yet.
                    //
                    // Review ::26 R1: a generated file (lockfile/checksum)
                    // was ADDED as one whole-content line, so a positional
                    // line diff against a newline-split model corrupts the
                    // staged graph (a lockfile-only modification rendered
                    // only its first line). Route generated files through
                    // the whole-file replacement record — the same shape
                    // their add used.
                    let recorded = if repo
                        .get_inode_and_position(&file.path)
                        .map_err(|e| CliError::Internal(e.into()))?
                        .is_some()
                    {
                        // CB-11A regression fix: a committed conflict
                        // snapshot resolves a path whose target-view state
                        // is a native conflict. The conflict render carries
                        // marker lines with side provenance that are NOT
                        // graph per-line vertices, so a positional diff
                        // against it produces line deletions whose numbers
                        // do not map to vertices; globalize then falls back
                        // to a structural whole-file FileDel, which kills
                        // the file's path claim and fails the staged tree
                        // check ("staged tree is missing 'tracked.txt'").
                        // Recording the resolution as a forced whole-file
                        // replace regenerates the semantic shape and keeps
                        // the file tracked.
                        let conflicted = !leg_mode
                            && repo
                                .path_has_persisted_conflict(&file.path, &target)
                                .map_err(|e| CliError::Internal(e.into()))?;
                        repo.record_foreign_modified_file(
                            &file.path,
                            &old_content,
                            new_content,
                            conflicted,
                        )
                        .map_err(|message| {
                            CliError::Internal(anyhow::anyhow!(
                                "cannot record modified path '{}': {message}",
                                file.path
                            ))
                        })?
                    } else {
                        record_modified_file(
                            &memory_wc,
                            &detected,
                            &old_content,
                            None,
                            &core_options,
                            None,
                            None,
                        )
                        .map_err(|message| {
                            CliError::Internal(anyhow::anyhow!(
                                "cannot record modified path '{}': {message}",
                                file.path
                            ))
                        })?
                    };
                    let mut recorded = recorded;
                    for value in attrs {
                        recorded.set_attr(*value);
                    }
                    if !recorded.is_empty() {
                        recorded_files.push(recorded);
                    } else if !attrs.is_empty() {
                        // Bytes unchanged but the graph-backed attributes
                        // differ: the attribute writes alone are the delta
                        // (CB-9C chmod-only/kind-only commits).
                        let mut attr_rec = RecordedFile::new(&file.path);
                        attr_rec.set_kind(atomic_core::record::workflow::DetectionKind::Modified);
                        if let Some((inode, position)) = repo
                            .get_inode_and_position(&file.path)
                            .map_err(|e| CliError::Internal(e.into()))?
                        {
                            attr_rec.set_inode(inode);
                            attr_rec.set_position(position);
                        }
                        for value in attrs {
                            attr_rec.set_attr(*value);
                        }
                        recorded_files.push(attr_rec);
                    } else {
                        // The baseline already renders the new bytes: no
                        // graph delta. Only a divergent baseline makes an
                        // empty record a failure.
                        if old_content.as_slice() != new_content {
                            return Err(CliError::Internal(anyhow::anyhow!(
                                "cannot record modified path '{}': record is empty",
                                file.path
                            )));
                        }
                        trace_git_import(format!(
                            "reclassify {} path={} modified→no-delta (baseline already renders the new bytes)",
                            parsed.short_sha, file.path
                        ));
                    }
                }

                EffectiveOp::Deleted => {
                    // Review CB-9C R2 (re-review EYL): bind the semantic
                    // deletion to the ACTUAL deleted trunk — the canonical
                    // placeholder-trunk delete tombstoned nobody and left the
                    // moved-to/created trunk row Alive after graph deletion.
                    let recorded = repo
                        .record_foreign_deleted_file(&file.path)
                        .map_err(|message| CliError::Internal(anyhow::anyhow!(message)))?;
                    recorded_files.push(recorded);
                }

                EffectiveOp::Skip => {}
            }
        }
        let record_ms = record_start.elapsed().as_millis();

        let metadata = self.build_git_metadata(parsed, false, false);
        let mut unhashed = git_synthesis_metadata(&origin, boundaries, Some(metadata));
        // CB-9C: carry the heuristic move evidence in the change's unhashed
        // provenance so reviewers and later tooling can distinguish
        // similarity-class moves from authoritative identity. `unhashed` is
        // the same namespaced merge the record path performs.
        if !move_evidence.is_empty() {
            let value = serde_json::to_value(&move_evidence).map_err(|error| {
                CliError::Internal(anyhow::anyhow!(
                    "cannot serialize import move evidence: {error}"
                ))
            })?;
            let object = unhashed.as_object_mut().ok_or_else(|| {
                CliError::Internal(anyhow::anyhow!(
                    "git synthesis metadata is not a JSON object; cannot attach move evidence"
                ))
            })?;
            object.insert(
                atomic_repository::MOVE_EVIDENCE_UNHASHED_KEY.to_owned(),
                value,
            );
        }
        // Lossless raw foreign facts in the hash-authoritative metadata
        // (review blocker 6): raw identity bytes, timezone offsets, and the
        // complete raw signed commit object.
        let hashed_metadata = foreign_commit_facts(parsed)?;
        // Semantic coverage is proven against what was actually recorded:
        // every recorded file (including deletions, which carry semantic
        // FileDel ops) must appear in the staged change's FileOps.
        // Semantic coverage is proven against what was actually recorded: every
        // recorded file that carries semantic ops (including deletions,
        // which carry semantic FileDel ops) must appear in the staged
        // change's FileOps. Pure move records carry no CRDT ops by design
        // (the graph rename is the delta), so they are not required to
        // invent semantic edits.
        let semantic_paths: Vec<String> = recorded_files
            .iter()
            .filter(|recorded| recorded.crdt_ops().is_some())
            .map(|recorded| recorded.path().to_string())
            .collect();
        let expectation = staged_expectation_for(parsed, prospective, &semantic_paths);
        let progress = SlowImportProgress::start(
            slow_import_commit_label(parsed),
            slow_import_record_summary(parsed, &recorded_files),
        );
        // The parent's persisted interpretation closure: persisted in the
        // same transaction as the change so later runs reconstruct the exact
        // parent visibility from it (review R1).
        let parent_closure: Vec<atomic_core::types::Hash> = if parsed.parent_oids.is_empty() {
            Vec::new()
        } else {
            ledger
                .interpreted_closure(repo, git, &parsed.parent_oids[0].to_string())?
                .iter()
                .copied()
                .collect()
        };
        let outcome = repo
            .synthesize_git_change(
                header,
                &recorded_files,
                origin,
                unhashed,
                hashed_metadata,
                &deleted_paths,
                self.options.preserve_working_copy,
                verified,
                Default::default(),
                &excluded,
                &expectation,
                Some(&parsed.git_sha),
                &parent_closure,
                // CB-9B review B4: a single-parent commit's assembly
                // exclusions are Git-origin members outside its interpreted
                // closure — history the commit's Git ancestry replaced. They
                // leave the target view in the same transaction, so the
                // published boundary equals the isolated interpretation the
                // staged check verifies, and the import never exits 4 after
                // publishing a diverged boundary.
                true,
            )
            .map_err(|e| CliError::Internal(e.into()))?;
        let progress_reported = progress.finish();

        // Index git SHA → Atomic change in GIT_SHA_INDEX
        let _ = repo.index_git_sha(&parsed.git_sha, &outcome.write.hash);
        if !self.options.preserve_working_copy && !deleted_paths.is_empty() {
            let del_refs: Vec<&str> = deleted_paths.iter().map(|s| s.as_str()).collect();
            let _ = repo.del_file_index_batch(working_copy, &del_refs);
        }

        if progress_reported {
            print_info(&format!(
                "Imported {} in {}ms (synthesized assemble={}ms save={}ms apply={}ms commit={}ms, {} recorded file(s), origin=git_synthesized)",
                slow_import_commit_label(parsed),
                commit_start.elapsed().as_millis(),
                outcome.write.timings.assemble_ms,
                outcome.write.timings.save_ms,
                outcome.write.timings.apply_ms,
                outcome.write.timings.commit_ms,
                recorded_files.len(),
            ));
        }
        trace_git_import(format!(
            "write {} synthesized=1 files={} recorded={} record={}ms assemble={}ms save={}ms apply={}ms direct_graph={}ms direct_crdt={}ms commit={}ms total={}ms",
            parsed.short_sha,
            parsed.files.len(),
            recorded_files.len(),
            record_ms,
            outcome.write.timings.assemble_ms,
            outcome.write.timings.save_ms,
            outcome.write.timings.apply_ms,
            outcome.write.timings.direct_graph_ms,
            outcome.write.timings.direct_crdt_ms,
            outcome.write.timings.commit_ms,
            commit_start.elapsed().as_millis()
        ));

        Ok(ImportedCommitInfo {
            git_sha: parsed.git_sha.clone(),
            short_sha: parsed.short_sha.clone(),
            atomic_hash: outcome.write.hash,
            is_merge: parsed.is_merge,
            message: parsed.full_message(),
            squash_insert: None,
        })
    }

    /// The diff baseline for one file: bytes the repository already holds,
    /// falling back to the Git parent-tree bytes captured in Phase 1 for
    /// history whose baseline is not in the graph.
    fn old_bytes_for(&self, repo: &Repository, file: &ParsedFile) -> Vec<u8> {
        match repo.get_file_content(&file.path) {
            Ok(Some(bytes)) => bytes,
            _ => file.old_content.as_deref().unwrap_or(&[]).to_vec(),
        }
    }

    /// Write an empty Git commit (no file changes) as a distinct
    /// `Change::empty` object (CB-9B).
    ///
    /// The change emits no graph facts, but its hashed header carries the
    /// `GitSynthesized` origin with the tagged commit OID, the complete
    /// ordered parents, and `Derivation::EmptyCommit`, plus hashed
    /// `metadata` recording the tagged tree OID and the raw author and
    /// committer identities and times — so distinct empty Git commits
    /// cannot collapse into one hash, and the raw foreign facts survive
    /// serialization.
    #[allow(clippy::too_many_arguments)]
    fn write_empty_commit_synthesized(
        &self,
        repo: &mut Repository,
        git: &GitRepository,
        ledger: &mut ClosureLedger,
        parsed: &ParsedCommit,
        verified: &VerifiedProspectiveEquivalence,
        prospective: &atomic_repository::ProjectTree,
        boundaries: &[&'static str],
    ) -> CliResult<ImportedCommitInfo> {
        let commit_start = Instant::now();

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

        // Hashed empty-commit facts: tagged commit/tree OIDs, ordered
        // parents, raw author/committer identities and times.
        let algorithm = git_algorithm_for_sha(&parsed.git_sha)?;
        let tree_oid = git_object_id(algorithm, parsed.tree_oid)?;
        let origin = GitSynthesisOrigin::with_derivation(
            git_object_id_for_sha(algorithm, &parsed.git_sha)?,
            tree_oid.clone(),
            parsed
                .parent_oids
                .iter()
                .map(|oid| git_object_id(algorithm, *oid))
                .collect::<CliResult<Vec<_>>>()?,
            GitDerivation::EmptyCommit,
        )
        .map_err(|e| CliError::Internal(e.into()))?;

        let empty_facts = EmptyCommitFacts {
            format: EMPTY_COMMIT_FACTS_FORMAT,
            tree: tagged_oid_hex(&tree_oid),
            tree_algorithm: git_algorithm_label(algorithm).to_string(),
            author: ForeignSignatureFacts {
                name: parsed.metadata.author_name.clone(),
                email: parsed.metadata.author_email.clone(),
                time: parsed.metadata.author_time,
                time_offset_seconds: parsed.metadata.author_time_offset_seconds,
                name_raw_hex: hex_bytes(Some(&parsed.metadata.author_name_raw)),
                email_raw_hex: hex_bytes(parsed.metadata.author_email_raw.as_deref()),
            },
            committer: CommitterFacts {
                name: parsed.metadata.committer_name.clone(),
                email: parsed.metadata.committer_email.clone(),
                time: parsed.metadata.committer_time,
                time_offset_seconds: parsed.metadata.committer_time_offset_seconds,
                name_raw_hex: hex_bytes(Some(&parsed.metadata.committer_name_raw)),
                email_raw_hex: hex_bytes(parsed.metadata.committer_email_raw.as_deref()),
            },
            raw_object_hex: hex_bytes(Some(&parsed.raw_object)),
        };
        let hashed_metadata =
            serde_json::to_vec(&empty_facts).map_err(|e| CliError::Internal(e.into()))?;

        let metadata = git_synthesis_metadata(
            &origin,
            boundaries,
            Some(self.build_git_metadata(parsed, true, false)),
        );
        // Exclusions for unrelated same-view lineages and the parent's
        // persisted interpretation closure (review R1): an empty commit
        // emits no graph facts but still persists its closure so descendants
        // reconstruct the exact parent visibility across runs.
        let excluded = ledger.exclusions_for(
            repo,
            git,
            &parsed.git_sha,
            &self.options.target_view,
            !parsed.parent_oids.is_empty(),
        )?;
        let parent_closure: Vec<atomic_core::types::Hash> = if parsed.parent_oids.is_empty() {
            Vec::new()
        } else {
            ledger
                .interpreted_closure(repo, git, &parsed.parent_oids[0].to_string())?
                .iter()
                .copied()
                .collect()
        };
        let outcome = repo
            .synthesize_empty_git_change(
                header,
                origin,
                hashed_metadata,
                metadata,
                self.options.preserve_working_copy,
                verified,
                Default::default(),
                Some(&parsed.git_sha),
                &parent_closure,
                &staged_expectation_for(parsed, prospective, &[]),
                &excluded,
                // CB-9B review B4: an empty commit still advances the exact
                // interpreted closure — superseded exclusions leave the view
                // in the same transaction (see write_commit_synthesized).
                true,
            )
            .map_err(|e| CliError::Internal(e.into()))?;
        // Index git SHA → Atomic change in GIT_SHA_INDEX
        let _ = repo.index_git_sha(&parsed.git_sha, &outcome.write.hash);

        trace_git_import(format!(
            "write {} files=0 recorded=0 assemble={}ms save={}ms apply={}ms commit={}ms total={}ms empty_commit=true origin=empty_commit",
            parsed.short_sha,
            outcome.write.timings.assemble_ms,
            outcome.write.timings.save_ms,
            outcome.write.timings.apply_ms,
            outcome.write.timings.commit_ms,
            commit_start.elapsed().as_millis()
        ));

        Ok(ImportedCommitInfo {
            git_sha: parsed.git_sha.clone(),
            short_sha: parsed.short_sha.clone(),
            atomic_hash: outcome.write.hash,
            is_merge: parsed.is_merge,
            message: parsed.full_message(),
            squash_insert: None,
        })
    }

    /// Write a multi-parent merge commit as a journaled `GitResolution`
    /// from the union state to the merge tree (CB-9B).
    ///
    /// Every parent closure was already imported by the deterministic
    /// sequencing — each leg assembled in its own Git parent filter — so the
    /// target view's membership is the union, and this resolution assembles
    /// against the full view visibility (no exclusions: every leg is in the
    /// merge's ancestry). The resolution's recorded files are the per-path
    /// deltas from the union view bytes to the merge tree, recorded through
    /// the graph-safe path so the resolution carries regenerated semantic
    /// FileOps and stable CRDT identities (review blocker 3). Its
    /// dependencies remain context-derived from globalization, and its
    /// hashed `ChangeOrigin::GitResolution` plus verified `CausalFrontier`
    /// cover every parent closure. The staged projection is verified against
    /// the merge tree inside the applying transaction before publication,
    /// and the frontier is verified against the dependency index (review
    /// blocker 2); both fail closed.
    #[allow(clippy::too_many_arguments)]
    fn write_merge_resolution(
        &self,
        repo: &mut Repository,
        git: &GitRepository,
        ledger: &mut ClosureLedger,
        parsed: &ParsedCommit,
        verified: &VerifiedProspectiveEquivalence,
        prospective: &atomic_repository::ProjectTree,
        boundaries: &[&'static str],
    ) -> CliResult<ImportedCommitInfo> {
        use atomic_core::output::memory::Memory;
        use atomic_core::record::workflow::{
            record_added_file, record_deleted_file, DetectedFile, RecordingOptions,
        };

        let working_copy = repo
            .require_working_copy_id()
            .map_err(|e| CliError::Internal(e.into()))?;
        let commit_start = std::time::Instant::now();
        let target = self.options.target_view.clone();

        // The resolution assembles against the exact parent union (review
        // R1): view members outside the reconstructed parent interpretation
        // closures — full ancestor chains included, across runs and views —
        // stay invisible, so the union is the parents' closures only.
        let excluded = ledger.exclusions_for(repo, git, &parsed.git_sha, &target, true)?;

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

        // The verified causal frontier of the exact parent union: the union
        // of every parent's complete interpreted closure (persisted rows +
        // reconstruction), not the whole view. Every parent closure member is
        // referenced into the target view first so the union state holds all
        // of them; a parent whose change is missing locally fails closed.
        let mut parent_closure_set: std::collections::BTreeSet<atomic_core::types::Hash> =
            std::collections::BTreeSet::new();
        let mut parent_tip_hashes: Vec<atomic_core::types::Hash> = Vec::new();
        for oid in &parsed.parent_oids {
            let sha = oid.to_string();
            let hash = repo
                .checked_git_sha(&sha)
                .map_err(|e| CliError::Internal(e.into()))?
                .ok_or_else(|| {
                    git_error(format!(
                        "merge {} parent {} has no locally verified imported change; \
                         every parent closure must be imported before the resolution",
                        parsed.git_sha, sha
                    ))
                })?;
            parent_tip_hashes.push(hash);
            let closure = ledger.interpreted_closure(repo, git, &sha)?;
            parent_closure_set.extend(closure.iter().copied());
        }
        if parent_closure_set.is_empty() {
            // A parent tip alone is not a complete interpretation closure
            // (review R1): refuse instead of assembling a union that silently
            // drops transitive ancestors.
            return Err(git_error(format!(
                "merge {} parent closures cannot be reconstructed locally; refusing to \
                 invent a partial union state",
                parsed.git_sha
            )));
        }
        let parent_closure: Vec<atomic_core::types::Hash> =
            parent_closure_set.into_iter().collect();
        repo.ensure_union_state(&target, &parent_closure)
            .map_err(|e| CliError::Internal(e.into()))?;
        let frontier = repo
            .view_frontier_for_members(&parent_closure)
            .map_err(|e| CliError::Internal(e.into()))?;
        let _ = parent_tip_hashes;
        let algorithm = git_algorithm_for_sha(&parsed.git_sha)?;
        let origin = GitResolutionOrigin::new(
            git_object_id_for_sha(algorithm, &parsed.git_sha)?,
            parsed
                .parent_oids
                .iter()
                .map(|oid| git_object_id(algorithm, *oid))
                .collect::<CliResult<Vec<_>>>()?,
            frontier,
        )
        .map_err(|e| CliError::Internal(e.into()))?;

        // Record the union→merge-tree delta through the normal workflow.
        // The candidate path set is the union of the per-parent diffs from
        // Phase 1; the recorded delta is classified against the exact union
        // visibility (parents' closures only — not the whole view) so
        // reverting side-parent content and resolving conflicts are both
        // captured without inheriting unrelated lineages. Storage and
        // rendering errors are propagated (fail closed) — only genuine
        // absence is an empty baseline.
        // CB-9C: imported blobs are content-addressed and opaque-trunked —
        // the local-record size cap must not refuse a large tracked binary
        // (the 500 MB opaque corpus).
        let core_options = RecordingOptions::new().unlimited_file_size();
        let mut recorded_files: Vec<RecordedFile> = Vec::new();
        let mut deleted_paths: Vec<String> = Vec::new();
        let mut added_paths: Vec<&str> = Vec::new();
        for file in &parsed.files {
            if file.new_content.is_some()
                && repo
                    .get_file_content_on_view_excluding(&file.path, &target, &excluded)
                    .map_err(|e| {
                        CliError::Internal(anyhow::anyhow!(
                            "cannot read exact union baseline for '{}': {e}",
                            file.path
                        ))
                    })?
                    .is_none()
            {
                added_paths.push(&file.path);
            }
        }
        if !self.options.preserve_working_copy && !added_paths.is_empty() {
            repo.add_batch(working_copy, &added_paths)
                .map_err(|e| CliError::Internal(e.into()))?;
        }
        let record_start = std::time::Instant::now();
        for file in &parsed.files {
            let union_bytes = repo
                .get_file_content_on_view_excluding(&file.path, &target, &excluded)
                .map_err(|e| {
                    CliError::Internal(anyhow::anyhow!(
                        "cannot read exact union baseline for '{}': {e}",
                        file.path
                    ))
                })?;
            match (&file.new_content, union_bytes) {
                (Some(new_content), Some(union_content)) if union_content == *new_content => {
                    // The union already matches the merge tree here.
                }
                (Some(new_content), Some(_union_content)) => {
                    // The union may hold conflicting spans (both sides
                    // edited the same region additively). A resolution
                    // replaces the file's whole content region with the
                    // merge-tree bytes so the losing spans are deleted and
                    // the resolution is authoritative — sequence order
                    // never silently picks a winner, the merge tree does.
                    // The graph-safe path regenerates semantic FileOps with
                    // stable CRDT identities instead of the opaque
                    // generated-file shortcut (review blocker 3); the
                    // conflicted union render forces the whole-file-replace
                    // safety path.
                    let rec = repo
                        .record_resolution_modified_file(&file.path, new_content, &target, true)
                        .map_err(|message| {
                            CliError::Internal(anyhow::anyhow!(
                                "cannot record merge resolution for '{}': {message}",
                                file.path
                            ))
                        })?;
                    if rec.is_empty() {
                        return Err(CliError::Internal(anyhow::anyhow!(
                            "cannot record merge resolution for '{}': record is empty",
                            file.path
                        )));
                    }
                    recorded_files.push(rec);
                }
                (Some(new_content), None) => {
                    let memory_wc = Memory::new();
                    memory_wc.add_file(&file.path, new_content);
                    let detected = DetectedFile::added(&file.path);
                    match record_added_file(&memory_wc, &detected, &core_options) {
                        Ok(rec) if !rec.is_empty() => {
                            recorded_files.push(rec);
                        }
                        Ok(_) => {
                            return Err(CliError::Internal(anyhow::anyhow!(
                                "cannot record merge addition '{}': record is empty",
                                file.path
                            )));
                        }
                        Err(message) => {
                            return Err(CliError::Internal(anyhow::anyhow!(
                                "cannot record merge addition '{}': {message}",
                                file.path
                            )));
                        }
                    }
                }
                (None, Some(_union_content)) => {
                    let (inode, position) = repo
                        .get_inode_and_position(&file.path)
                        .map_err(|error| CliError::Internal(error.into()))?
                        .ok_or_else(|| {
                            git_error(format!(
                                "cannot import merge deletion '{}': no stable inode binding",
                                file.path
                            ))
                        })?;
                    let mut detected = DetectedFile::deleted(&file.path);
                    detected.inode = Some(inode);
                    detected.position = Some(position);
                    let recorded = record_deleted_file(&detected, &core_options)
                        .map_err(|message| CliError::Internal(anyhow::anyhow!(message)))?;
                    if recorded.is_empty() {
                        return Err(CliError::Internal(anyhow::anyhow!(
                            "cannot import merge deletion '{}': canonical FileDel is empty",
                            file.path
                        )));
                    }
                    recorded_files.push(recorded);
                    deleted_paths.push(file.path.clone());
                }
                (None, None) => {}
            }
        }
        let record_ms = record_start.elapsed().as_millis();

        let metadata = git_resolution_metadata(
            &origin,
            boundaries,
            Some(self.build_git_metadata(parsed, recorded_files.is_empty(), true)),
        );
        // Semantic coverage is proven against what was actually recorded:
        // the resolution's recorded files (with regenerated FileOps, review
        // blocker 3) must all appear in the staged change's FileOps. Pure
        // move records carry no CRDT ops by design (the graph rename is the
        // delta), so they are not required to invent semantic edits.
        let semantic_paths: Vec<String> = recorded_files
            .iter()
            .filter(|recorded| recorded.crdt_ops().is_some())
            .map(|recorded| recorded.path().to_string())
            .collect();
        let expectation = staged_expectation_for(parsed, prospective, &semantic_paths);
        let outcome = repo
            .synthesize_git_resolution(
                header,
                &recorded_files,
                origin,
                metadata,
                &deleted_paths,
                self.options.preserve_working_copy,
                verified,
                Default::default(),
                &excluded,
                &expectation,
                Some(&parsed.git_sha),
                &parent_closure,
            )
            .map_err(|e| CliError::Internal(e.into()))?;

        let _ = repo.index_git_sha(&parsed.git_sha, &outcome.write.hash);
        if !self.options.preserve_working_copy && !deleted_paths.is_empty() {
            let del_refs: Vec<&str> = deleted_paths.iter().map(|s| s.as_str()).collect();
            repo.del_file_index_batch(working_copy, &del_refs)
                .map_err(|e| CliError::Internal(e.into()))?;
        }

        trace_git_import(format!(
            "write {} merge_resolution=1 parents={} files={} recorded={} record={}ms assemble={}ms save={}ms apply={}ms commit={}ms total={}ms",
            parsed.short_sha,
            parsed.parent_oids.len(),
            parsed.files.len(),
            recorded_files.len(),
            record_ms,
            outcome.write.timings.assemble_ms,
            outcome.write.timings.save_ms,
            outcome.write.timings.apply_ms,
            outcome.write.timings.commit_ms,
            commit_start.elapsed().as_millis()
        ));

        Ok(ImportedCommitInfo {
            git_sha: parsed.git_sha.clone(),
            short_sha: parsed.short_sha.clone(),
            atomic_hash: outcome.write.hash,
            is_merge: true,
            message: parsed.full_message(),
            squash_insert: None,
        })
    }

    /// Record a reviewable RewriteCandidate link backed by precise evidence
    /// (CB-9B, review blocker 4).
    ///
    /// The tag carries the exact evidence entries: full-length predecessor
    /// OIDs, binding ids, and journal event ids, plus the identity limit for
    /// each class. It never establishes identity: only a verified
    /// predecessor binding or explicit review can.
    fn record_rewrite_candidate_link(
        &self,
        repo: &Repository,
        parsed: &ParsedCommit,
        evidence: &[RewriteEvidence],
    ) -> CliResult<()> {
        let tag_name = format!("rewrite-candidate-{}", parsed.git_sha);
        let metadata = serde_json::json!({
            "git": {
                "sha": parsed.git_sha,
                "merge_strategy": "squash",
            },
            "evidence": evidence
                .iter()
                .map(|entry| {
                    serde_json::json!({
                        "class": entry.class,
                        "predecessor_oids": entry.predecessors,
                        "binding_ids": entry.binding_ids,
                        "event_ids": entry.event_ids,
                        "advisory": true,
                        "authenticated": entry.class == "post-rewrite-event",
                    })
                })
                .collect::<Vec<_>>(),
            "identity_limit": if evidence.iter().any(|entry| entry.class == "post-rewrite-event") {
                "operation-linkage-only: a captured post-rewrite event whose predecessors are \
                 locally interpreted commits links a Git rewrite operation; identity requires a \
                 verified predecessor binding or explicit review (RFC 5.4)"
            } else if evidence.iter().any(|entry| entry.class == "unauthenticated-event") {
                "unauthenticated-evidence-only: no immutably captured operation backs this \
                 journal text; it is not operation linkage and identity requires a verified \
                 predecessor binding or explicit review (RFC 5.4)"
            } else {
                "advisory-hint-only: binding similarity is never identity; identity requires a \
                 verified predecessor binding or explicit review (RFC 5.4)"
            },
        });
        repo.create_tag_with_metadata(
            &tag_name,
            Some(&format!(
                "RewriteCandidate: squash {} linked by captured evidence",
                parsed.short_sha
            )),
            atomic_core::pristine::TagKind::ReviewGate,
            Some(metadata),
        )
        .map(|_| ())
        .map_err(|e| {
            CliError::Internal(anyhow::anyhow!(
                "failed to record rewrite-candidate link for {}: {e}",
                parsed.short_sha
            ))
        })
    }

    /// Collect the rewrite evidence for one parsed commit (CB-9B, review
    /// blocker 4).
    ///
    /// Two independent evidence sources, each with its own identity limit:
    /// 1. **Captured post-rewrite events** — journal records whose
    ///    interpretation matches the advisory post-rewrite contract, whose
    ///    `new_oid` equals this commit's *full* OID (never a prefix or an
    ///    empty/partial value), and whose OIDs are full-length hex. Such a
    ///    record establishes operation linkage only. Malformed or
    ///    non-matching records are counted as untrusted and never become
    ///    linkage.
    /// 2. **Locally verified bindings carrying the same tree** — found by
    ///    scanning the binding store independently of hooks. Tree equality
    ///    is an advisory hint that the squash carries a bound predecessor's
    ///    content; it is similarity-class evidence and never identity.
    ///
    /// # Collect the rewrite evidence for one parsed commit (CB-9B, review
    /// blocker 4 / R4).
    ///
    /// Three evidence tiers plus an explicit untrusted tier, each with its
    /// own identity limit:
    /// 1. **Captured post-rewrite events** — journal records whose
    ///    interpretation matches the advisory post-rewrite contract, whose
    ///    `new_oid` equals this commit's *full* OID (never a prefix or an
    ///    empty/partial value), whose OIDs are full-length hex, whose event
    ///    id is nonempty, whose exact bytes have an immutable captured
    ///    record in the pristine database, whose named predecessors are
    ///    locally interpreted commits (persisted closure row or checked SHA
    ///    index), and whose named pairs describe a rewrite the live Git
    ///    object database verifiably performed — the old OID is NOT an
    ///    ancestor of the new OID (review B3: known objects are not
    ///    authentication). Such a record establishes operation linkage only.
    ///    A journal line that fails any of these — including a durably
    ///    captured event whose predecessor was never interpreted by Atomic,
    ///    or a fully known forged pair that is not a structural rewrite —
    ///    is unauthenticated evidence and is labeled as such (review
    ///    F5/R4/B3): durability authenticates persistence, not the external
    ///    event.
    /// 2. **Locally verified bindings carrying the same tree** — found by
    ///    scanning the binding store independently of hooks. Tree equality
    ///    is an advisory hint that the squash carries a bound predecessor's
    ///    content; it is similarity-class evidence and never identity.
    /// 3. **Locally verified bindings covering a known intermediate of the
    ///    rewritten range** — found independently of hooks, independent of
    ///    final-tree equality, blob survival, and message format (review
    ///    F6/R4/AC-2): the bound commit is a strict descendant of this
    ///    commit's first parent (it lies inside the rewritten range and is
    ///    trivially excluded when it is an ancestor). Known range is
    ///    ancestry-hint evidence (review D4: a descent hint is not verified
    ///    range membership); neither blob nor message similarity
    ///    participates, and nothing here is identity.
    /// 4. **Root-spanning ranges** (review D4/E3): a parentless commit
    ///    provides no ancestry bound at all, so every locally stored
    ///    signature-checked binding is named with the explicit caveat that
    ///    the hint is maximally uncertain and complete coverage is NOT
    ///    claimed. The bound commit does NOT need a local interpretation
    ///    index (review E3): RFC §5.3(6) requires a prior binding to exist
    ///    locally, not that the predecessor was already imported, so a fresh
    ///    clone that stores a valid binding before interpreting its bound
    ///    commit still surfaces the candidate. Bindings are signature-checked,
    ///    not recomputed content-verified; no content or identity claim is
    ///    made from these hints (RFC §5.4).
    fn rewrite_evidence_for(
        &self,
        repo: &Repository,
        git: &GitRepository,
        parsed: &ParsedCommit,
    ) -> CliResult<Vec<RewriteEvidence>> {
        let mut evidence = Vec::new();

        // 1. Captured post-rewrite events with exact full-OID linkage.
        if let Some((predecessors, event_ids, untrusted, unauthenticated)) =
            self.captured_post_rewrite_linkage(repo, &parsed.git_sha)
        {
            if untrusted > 0 {
                trace_git_import(format!(
                    "rewrite-evidence {}: {untrusted} malformed journal record(s) ignored",
                    parsed.short_sha
                ));
            }
            if !predecessors.is_empty() {
                evidence.push(RewriteEvidence {
                    class: "post-rewrite-event",
                    predecessors,
                    binding_ids: Vec::new(),
                    event_ids,
                });
            }
            if unauthenticated > 0 {
                // The journal text has the advisory shape but no immutable
                // captured record backs it: surfaced explicitly as
                // untrusted, never as operation linkage (review R4).
                evidence.push(RewriteEvidence {
                    class: "unauthenticated-event",
                    predecessors: Vec::new(),
                    binding_ids: Vec::new(),
                    event_ids: Vec::new(),
                });
            }
        }

        // 2. Advisory binding hints, found independently of hooks.
        let algorithm = git_algorithm_for_sha(&parsed.git_sha)?;
        let tree_hex = tagged_oid_hex(&git_object_id(algorithm, parsed.tree_oid)?);
        for id in repo
            .binding_ids()
            .map_err(|e| CliError::Internal(e.into()))?
        {
            let Ok(Some(binding)) = repo.load_binding(&id) else {
                continue;
            };
            if atomic_repository::git_binding::verify_binding_cryptography(&binding).is_err() {
                continue;
            }
            let payload = binding.payload();
            let Ok(bound_commit) = payload.git_commit.to_git_object_id() else {
                continue;
            };
            let Ok(bound_tree) = payload.git_tree.to_git_object_id() else {
                continue;
            };
            let bound_tree_hex = tagged_oid_hex(&bound_tree);
            let bound_commit_hex = tagged_oid_hex(&bound_commit);
            if bound_commit_hex == parsed.git_sha {
                continue;
            }
            if bound_tree_hex == tree_hex {
                evidence.push(RewriteEvidence {
                    class: "binding-tree-match",
                    predecessors: vec![bound_commit_hex.clone()],
                    binding_ids: vec![binding.id().to_string()],
                    event_ids: Vec::new(),
                });
                continue;
            }
            // 3. Known-range intermediates (review F6/R4/C3): a commit whose
            // tree differs from every bound tree still gets a reviewable
            // link when the bound commit is a locally known intermediate of
            // the rewritten range — the bound commit is a strict descendant
            // of this commit's first parent (it lies between the range base
            // and the rewritten tip), and it is not the range base itself.
            // The COMPLETE known range is covered without requiring
            // surviving blobs, surviving paths, path overlap, or a
            // recognized squash message (review C3): a bound intermediate
            // whose edits were canceled or renamed away by later commits in
            // the range has no surviving path intersection with the squash
            // and is still a known bound intermediate. Strictly advisory
            // (RFC §5.4): descent from the range base bounds the range as
            // far as locally validated evidence shows, and it is candidate
            // evidence, never identity.
            //
            // Review D4/E3: a ROOT-SPANNING squash has no first parent, so
            // the descent probe above structurally missed its range members —
            // including a locally known bound ROOT (fixture
            // cb9b-current-root-range: a valid published root binding
            // produced zero candidates). For a parentless commit the range
            // spans from the root and no ancestry bound can be established
            // at all, so the honest tier names EVERY locally stored
            // signature-checked binding (already verified above), including
            // bindings whose bound commit has NO local interpretation index
            // (review E3: a fresh clone stores the binding via
            // Repository::store_binding before its bound commit is ever
            // interpreted — its closure row and checked-SHA index are empty,
            // and filtering on that index silently dropped locally known
            // valid evidence). The hint stays explicitly maximally
            // uncertain: signature-checked here means signature validity
            // only (not recomputed binding-content verification), a
            // parentless commit provides no ancestry bound, no content or
            // identity claim is made, and complete coverage is NOT claimed.
            if parsed.parent_oids.is_empty() {
                evidence.push(RewriteEvidence {
                    class: "binding-root-range-match",
                    predecessors: vec![bound_commit_hex],
                    binding_ids: vec![binding.id().to_string()],
                    event_ids: Vec::new(),
                });
                continue;
            }
            if let Some(first_parent_oid) = parsed.parent_oids.first() {
                let first_parent_hex =
                    tagged_oid_hex(&git_object_id(algorithm, *first_parent_oid)?);
                if first_parent_hex != bound_commit_hex {
                    let bound_commit_oid = git2::Oid::from_bytes(bound_commit.as_bytes())
                        .map_err(|e| CliError::Internal(anyhow::anyhow!("{e}")))?;
                    let in_range = git
                        .merge_base(*first_parent_oid, bound_commit_oid)
                        .map(|base| base == *first_parent_oid)
                        .unwrap_or(false);
                    if in_range {
                        evidence.push(RewriteEvidence {
                            class: "binding-range-match",
                            predecessors: vec![bound_commit_hex],
                            binding_ids: vec![binding.id().to_string()],
                            event_ids: Vec::new(),
                        });
                    }
                }
            }
        }

        Ok(evidence)
    }

    /// Scan the advisory post-rewrite journal for records that link this
    /// commit by its exact full OID.
    ///
    /// Returns `(predecessors, event_ids, untrusted_count,
    /// unauthenticated_count)`. A record counts as operation linkage only
    /// when ALL of the following hold (review R4 + F5 + B3 + C2):
    /// - it matches the advisory interpretation contract (record type,
    ///   advisory flag, verbatim identity-limit string) and carries
    ///   full-length hex OIDs with exact new-OID equality,
    /// - its event id is nonempty,
    /// - the exact journal line has an immutable captured record in the
    ///   pristine database that is ANCHORED to a real operation which exists
    ///   in the journal with a verified receipt (review C2: the anchor is the
    ///   only proof the event was captured during an active captured
    ///   operation — the RFC §5.4 precondition), and
    /// - every named predecessor is a locally interpreted commit (a
    ///   persisted closure row or a checked SHA index entry in the pristine
    ///   database).
    ///
    /// # Authentication limits (CB-9B review)
    ///
    /// Git ancestry shape and object existence are NEVER authentication
    /// inputs: arbitrary sibling commits satisfy every ancestry-shape
    /// predicate, and a real-looking amend pair submitted directly to the
    /// hook is captured unanchored (the hook runs outside any Atomic
    /// operation), so it is counted as unauthenticated and surfaced as
    /// explicitly untrusted advisory evidence that never names predecessors
    /// (review C2; immutable storage authenticates persistence, not the
    /// external event's truth).
    fn captured_post_rewrite_linkage(
        &self,
        repo: &Repository,
        sha: &str,
    ) -> Option<(Vec<String>, Vec<String>, usize, usize)> {
        use std::io::BufRead;
        let journal = repo.root().join(".atomic/bridge/git-events.jsonl");
        let file = std::fs::File::open(journal).ok()?;
        let reader = std::io::BufReader::new(file);
        let mut predecessors = Vec::new();
        let mut event_ids = Vec::new();
        let mut untrusted = 0usize;
        let mut unauthenticated = 0usize;
        for line in reader.lines() {
            let Ok(line) = line else {
                continue;
            };
            let Ok(record) = serde_json::from_str::<serde_json::Value>(&line) else {
                untrusted += 1;
                continue;
            };
            if record.get("record_type").and_then(|v| v.as_str()) != Some("post-rewrite") {
                continue;
            }
            // An advisory record must carry the advisory flag and the exact
            // identity-limit interpretation it was journaled with; anything
            // else is arbitrary journal text, not captured-operation proof.
            if record.get("advisory").and_then(|v| v.as_bool()) != Some(true)
                || record.get("interpretation").and_then(|v| v.as_str())
                    != Some(super::hooks::REWRITE_INTERPRETATION)
            {
                untrusted += 1;
                continue;
            }
            let Some(pairs) = record.get("rewritten").and_then(|v| v.as_array()) else {
                untrusted += 1;
                continue;
            };
            let event_id = record
                .get("event_id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let mut matched_oids = Vec::new();
            for pair in pairs {
                let new_oid = pair
                    .get("new_oid")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let old_oid = pair
                    .get("old_oid")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                // Full-length hex OIDs only, and exact equality on the new
                // OID — an empty, partial, or prefixed value is never a
                // match (review blocker 4).
                if !is_full_oid(new_oid) || !is_full_oid(old_oid) {
                    untrusted += 1;
                    continue;
                }
                if !new_oid.eq_ignore_ascii_case(sha) {
                    continue;
                }
                matched_oids.push(old_oid.to_ascii_lowercase());
            }
            if !matched_oids.is_empty() {
                // Shape is valid — now authenticate (review C2): the exact
                // journal line must have an immutable captured record that
                // is ANCHORED to a real operation which exists in the journal
                // with a verified receipt. That anchor is the only proof the
                // event was captured during an active captured operation
                // (RFC §5.4); Git ancestry shape and object existence never
                // prove a captured event — arbitrary sibling commits satisfy
                // every shape predicate (review C2), and a real-looking
                // amend pair submitted directly to the hook is captured
                // unanchored, so it stays advisory. The named predecessors
                // must additionally be locally interpreted commits so the
                // linkage names commits Atomic actually interpreted.
                let interpreted_predecessors = matched_oids.iter().all(|old| {
                    repo.git_commit_closure(old)
                        .map(|closure| closure.is_some())
                        .unwrap_or(false)
                        || repo
                            .checked_git_sha(old)
                            .map(|sha| sha.is_some())
                            .unwrap_or(false)
                });
                let anchored_operation = repo
                    .bridge_event_capture_anchor(line.as_bytes())
                    .ok()
                    .flatten();
                let operation_verified = anchored_operation
                    .map(|operation| {
                        repo.operation_has_verified_receipt(operation)
                            .unwrap_or(false)
                    })
                    .unwrap_or(false);
                // Review D3: the anchor must BIND this exact event to the
                // operation — the operation's GitRef effect leases must
                // describe exactly the rewrite the event names. A bare
                // anchor row is not enough: retrospective anchoring of an
                // old advisory capture to an unrelated ref-write would
                // otherwise manufacture the linkage tier.
                let anchor_binds_event = anchored_operation
                    .map(|operation| {
                        repo.bridge_anchor_binds_operation(line.as_bytes(), operation)
                            .unwrap_or(false)
                    })
                    .unwrap_or(false);
                let authenticated = !event_id.is_empty()
                    && repo.bridge_event_captured(line.as_bytes()).unwrap_or(false)
                    && anchored_operation.is_some()
                    && anchor_binds_event
                    && operation_verified
                    && interpreted_predecessors;
                if authenticated {
                    predecessors.append(&mut matched_oids);
                    event_ids.push(event_id);
                } else {
                    unauthenticated += 1;
                }
            }
        }
        if predecessors.is_empty() && unauthenticated == 0 && untrusted == 0 {
            None
        } else {
            Some((predecessors, event_ids, untrusted, unauthenticated))
        }
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
    // Post-Import Classification
    // ═══════════════════════════════════════════════════════════════════════

    /// Classify newly imported commits and create ReviewGate tags for
    /// merge/squash commits.
    ///
    /// This runs after all phase 2 writes complete. It examines each
    /// newly imported commit's metadata to detect merges and squash
    /// merges, then creates ReviewGate tags linking back to the original
    /// changes.
    fn classify_and_tag_imports(
        &self,
        repo: &mut Repository,
        imported: &[ImportedCommitInfo],
    ) -> CliResult<ClassificationStats> {
        use atomic_core::pristine::TagKind;

        let mut stats = ClassificationStats::default();

        for info in imported {
            // Squash represented by inserted originals (SPEC §4): tag the
            // aggregate over those records rather than re-classifying from the
            // message. The originals are already live in the target view.
            if let Some(sq) = &info.squash_insert {
                stats.squashes += 1;
                let tag_name = if let Some(pr) = sq.pr_number {
                    format!("pr-{}", pr)
                } else {
                    format!("squash-{}", info.short_sha)
                };
                let from = sq.original_hashes.first();
                let to = sq.original_hashes.last();
                let metadata = serde_json::json!({
                    "git": {
                        "sha": info.git_sha,
                        "merge_strategy": "squash",
                        "pr_number": sq.pr_number,
                    },
                    "changes": {
                        "original_hashes": sq.original_hashes,
                        "from": from,
                        "to": to,
                        "count": sq.original_hashes.len(),
                        "inserted": true,
                    }
                });
                if let Err(e) = repo.create_tag_with_metadata(
                    &tag_name,
                    Some(&format!("Squash merge {} (inserted)", info.short_sha)),
                    TagKind::ReviewGate,
                    Some(metadata),
                ) {
                    log::warn!(
                        "Failed to create aggregate ReviewGate tag for squash {}: {}",
                        info.short_sha,
                        e
                    );
                }
                continue;
            }

            let classification = classify_commit(info);
            match classification {
                CommitClassification::Normal => {
                    stats.normal += 1;
                }
                CommitClassification::Merge => {
                    stats.merges += 1;
                    let tag_name = format!("merge-{}", info.short_sha);
                    let metadata = serde_json::json!({
                        "git": {
                            "sha": info.git_sha,
                            "merge_strategy": "merge",
                        }
                    });
                    if let Err(e) = repo.create_tag_with_metadata(
                        &tag_name,
                        Some(&format!("Merge commit {}", info.short_sha)),
                        TagKind::ReviewGate,
                        Some(metadata),
                    ) {
                        log::warn!(
                            "Failed to create ReviewGate tag for merge {}: {}",
                            info.short_sha,
                            e
                        );
                    }
                }
                CommitClassification::Squash {
                    ref original_hashes,
                    ref pr_number,
                } => {
                    stats.squashes += 1;
                    let tag_name = if let Some(pr) = pr_number {
                        format!("pr-{}", pr)
                    } else {
                        format!("squash-{}", info.short_sha)
                    };
                    let metadata = serde_json::json!({
                        "git": {
                            "sha": info.git_sha,
                            "merge_strategy": "squash",
                            "pr_number": pr_number,
                        },
                        "changes": {
                            "original_hashes": original_hashes,
                        }
                    });
                    if let Err(e) = repo.create_tag_with_metadata(
                        &tag_name,
                        Some(&format!("Squash merge {}", info.short_sha)),
                        TagKind::ReviewGate,
                        Some(metadata),
                    ) {
                        log::warn!(
                            "Failed to create ReviewGate tag for squash {}: {}",
                            info.short_sha,
                            e
                        );
                    }
                }
            }
        }

        Ok(stats)
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Phase 3: Finalization
    // ═══════════════════════════════════════════════════════════════════════

    /// Phase 3: Finalization and verification.
    fn phase3_finalize(&self, stats: &ImportStats) -> CliResult<()> {
        // Verify counts
        let expected = stats.commits_parsed;
        let actual = stats.changes_written
            + stats.empty_commits
            + stats.merge_commits
            + stats.self_push_skipped
            + stats.squash_inserted
            + stats.squash_skipped;

        if actual != expected {
            return Err(CliError::GitError {
                message: format!(
                    "Import verification failed: {} commits parsed but {} changes created",
                    expected, actual
                ),
            });
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
    /// CB-13C F4: commits resurrected exactly from verified bindings (a
    /// subset of `changes_written`); synthesis is the remainder.
    resurrected_exact: usize,
    empty_commits: usize,
    merge_commits: usize,
    self_push_skipped: usize,
    /// Squash commits handled by inserting their originals (SPEC §4).
    squash_inserted: usize,
    /// Atomic-origin squash commits skipped (missing originals or divergence).
    squash_skipped: usize,
    files_processed: usize,
    /// CB-13C F4: the phase-2 write error, when the loop stopped early.
    /// Earlier successfully written commits REMAIN durable (fail closed on
    /// a partial import), so the aggregate must account for what landed.
    failure: Option<String>,
}

/// Metadata about an imported commit, retained for post-import classification.
#[derive(Debug, Clone)]
struct ImportedCommitInfo {
    /// Git SHA of the commit.
    git_sha: String,
    /// Short SHA for display.
    short_sha: String,
    /// Atomic change hash (Blake3).
    atomic_hash: ContentHash,
    /// Whether this was a merge commit (2+ parents).
    is_merge: bool,
    /// The commit message (for squash detection).
    message: String,
    /// Set when this squash was represented by inserting its original change
    /// records into the target view (SPEC §4) rather than writing a new change.
    /// Phase 3 turns this into an aggregate ReviewGate tag.
    squash_insert: Option<SquashInsert>,
}

/// Provenance for a squash represented by inserting its originals (SPEC §4).
#[derive(Debug, Clone)]
struct SquashInsert {
    /// The original change hashes named in the squash's trailers, trailer order.
    original_hashes: Vec<String>,
    /// PR/MR number parsed from the squash subject, if any.
    pr_number: Option<u32>,
}

/// Outcome of attempting the squash → insert path in Phase 2 (SPEC §4/§5).
enum SquashDecision {
    /// Handled by inserting the originals; carries the info for tagging.
    Inserted(ImportedCommitInfo),
    /// An atomic-origin squash we could not insert (originals missing, or the
    /// insert diverged from the git tree). The commit is left unimported so a
    /// later run can complete it; caller warns and advises.
    Skipped,
    /// Not an insertable atomic squash — fall through to the normal write path.
    NotSquash,
}

/// Classification of a commit detected during post-import analysis.
#[derive(Debug)]
enum CommitClassification {
    Normal,
    Merge,
    Squash {
        original_hashes: Vec<String>,
        pr_number: Option<u32>,
    },
}

/// Statistics from post-import classification.
#[derive(Debug, Default)]
struct ClassificationStats {
    normal: usize,
    merges: usize,
    squashes: usize,
}

// ═══════════════════════════════════════════════════════════════════════
// Commit Classification
// ═══════════════════════════════════════════════════════════════════════

/// Dry-run classification of an incoming commit, used by the `--dry-run` router
/// to recommend the right import path (SPEC §4.4).
pub(crate) enum ForecastKind {
    /// Git-authored content with no atomic provenance.
    Normal,
    /// Multi-parent merge commit.
    Merge,
    /// Atomic-origin squash whose originals are all present — insertable.
    SquashInsertable { count: usize, pr: Option<u32> },
    /// Atomic-origin squash missing ≥ 1 original locally — needs `atomic pull`.
    SquashRecordsMissing { count: usize },
}

/// Classify an incoming git commit for the `--dry-run` forecast (SPEC §4.4).
///
/// `records_present(hash)` reports whether a named original change hash exists
/// locally; callers wire it to `Repository::has_change`.
pub(crate) fn forecast_commit_kind(
    message: &str,
    is_merge: bool,
    records_present: impl Fn(&str) -> bool,
) -> ForecastKind {
    if is_merge {
        return ForecastKind::Merge;
    }
    match parse_atomic_changes_trailer(message) {
        Some(hashes) => {
            if hashes.iter().all(|h| records_present(h)) {
                ForecastKind::SquashInsertable {
                    count: hashes.len(),
                    pr: parse_pr_number(message),
                }
            } else {
                ForecastKind::SquashRecordsMissing {
                    count: hashes.len(),
                }
            }
        }
        None => ForecastKind::Normal,
    }
}

/// Remove change refs a failed squash-insert added to `view`, newest first, so
/// a divergent or partial insert leaves the shared view exactly as it was
/// (SPEC §5 / D1-A). Best-effort: rollback failures are logged, not fatal.
fn rollback_inserts(repo: &Repository, view: &str, applied: &[ContentHash]) {
    use atomic_repository::UnrecordOptions;
    for hash in applied.iter().rev() {
        let options = UnrecordOptions::default().view(view.to_string());
        if let Err(e) = repo.unrecord(hash, options) {
            print_warning(&format!(
                "Failed to roll back inserted change {} from '{}': {}",
                &hash.to_base32()[..12],
                view,
                e
            ));
        }
    }
}

/// Classify an imported commit as normal, merge, or squash.
fn classify_commit(info: &ImportedCommitInfo) -> CommitClassification {
    // 1. Multi-parent merge commits
    if info.is_merge {
        return CommitClassification::Merge;
    }

    // 2. Atomic-Changes trailer (written by `atomic git push`)
    if let Some(hashes) = parse_atomic_changes_trailer(&info.message) {
        let pr = parse_pr_number(&info.message);
        return CommitClassification::Squash {
            original_hashes: hashes,
            pr_number: pr,
        };
    }

    // 3. Squash merge format (GitHub, GitLab, Azure DevOps)
    if let Some(pr) = parse_squash_merge_format(&info.message) {
        return CommitClassification::Squash {
            original_hashes: Vec::new(),
            pr_number: Some(pr),
        };
    }

    CommitClassification::Normal
}

/// Split the value of a single `Atomic-Changes:` trailer (the text after the
/// colon) into individual, non-empty change hashes.
fn split_atomic_changes_value(value: &str) -> impl Iterator<Item = String> + '_ {
    value
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Parse **every** `Atomic-Changes:` trailer in a commit message.
///
/// A GitHub (or GitLab/Azure) squash-merge concatenates the bodies of all
/// squashed commits, so a single squash commit legitimately carries
/// **multiple** `Atomic-Changes:` blocks — one per materialization commit. We
/// accumulate hashes across every block, de-duplicated with first-seen order
/// preserved, so the ReviewGate links back to the *full* set of original
/// changes rather than just the first block.
///
/// This differs deliberately from [`parse_push_trailer`], which inspects only
/// the message's final paragraph. That function answers "did *I* just push this
/// exact view state?", where only the trailing self-push block is
/// authoritative; this function answers "which original changes does this
/// squash represent?", where every block counts. They share the low-level
/// [`split_atomic_changes_value`] splitter but must keep these distinct
/// notions of which trailers are relevant.
fn parse_atomic_changes_trailer(message: &str) -> Option<Vec<String>> {
    let mut all: Vec<String> = Vec::new();
    for line in message.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Atomic-Changes:") {
            for hash in split_atomic_changes_value(rest) {
                if !all.contains(&hash) {
                    all.push(hash);
                }
            }
        }
    }
    if all.is_empty() {
        None
    } else {
        Some(all)
    }
}

/// Read one `Key: value` identity header from a commit message (the
/// record-projection header format; RFC §8.2).
fn parse_header_trailer(message: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}:");
    for line in message.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(&prefix) {
            let value = rest.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Parse a PR/MR number from a commit message.
///
/// Supports multiple forge formats:
/// - GitHub:     `(#42)` or `Merge pull request #42 from ...`
/// - GitLab:     `See merge request group/project!42`
/// - Bitbucket:  `Merged in branch (pull request #42)`
/// - Azure DevOps: `Merged PR 42: title`
fn parse_pr_number(message: &str) -> Option<u32> {
    for line in message.lines() {
        let line = line.trim();

        // GitHub: "(#N)" anywhere in the line
        if let Some(start) = line.rfind("(#") {
            if let Some(end) = line[start..].find(')') {
                if let Ok(n) = line[start + 2..start + end].parse::<u32>() {
                    return Some(n);
                }
            }
        }

        // GitHub: "Merge pull request #N"
        if let Some(idx) = line.find("pull request #") {
            let after = &line[idx + "pull request #".len()..];
            let num_str: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(n) = num_str.parse::<u32>() {
                return Some(n);
            }
        }

        // GitLab: "See merge request group/project!N" or just "!N"
        if let Some(idx) = line.rfind('!') {
            let after = &line[idx + 1..];
            let num_str: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !num_str.is_empty() {
                if let Ok(n) = num_str.parse::<u32>() {
                    // Verify it's a merge request reference, not a random "!"
                    if line.contains("merge request") || line.ends_with(&format!("!{}", n)) {
                        return Some(n);
                    }
                }
            }
        }

        // Azure DevOps: "Merged PR N: title"
        if let Some(rest) = line.strip_prefix("Merged PR ") {
            let num_str: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(n) = num_str.parse::<u32>() {
                return Some(n);
            }
        }
    }
    None
}

/// Detect squash merge format from various Git forges.
///
/// Supported formats:
/// - GitHub:  `title (#42)\n\n* commit msg 1\n* commit msg 2`
/// - GitLab:  `title\n\nSee merge request group/project!42`
/// - Azure DevOps: `Merged PR 42: title\n\n...`
fn parse_squash_merge_format(message: &str) -> Option<u32> {
    let lines: Vec<&str> = message.lines().collect();
    if lines.len() < 2 {
        return None;
    }

    // GitHub: first line has (#N), followed by blank line + bullet points
    if let Some(pr) = parse_pr_number(lines[0]) {
        if lines.len() >= 3 {
            let has_bullets = lines[2..].iter().any(|l| l.trim().starts_with("* "));
            if has_bullets {
                return Some(pr);
            }
        }
    }

    // GitLab: body contains "See merge request ...!N"
    for line in &lines[1..] {
        if line.contains("See merge request") {
            return parse_pr_number(line);
        }
    }

    // Azure DevOps: "Merged PR N: title"
    if lines[0].starts_with("Merged PR ") {
        return parse_pr_number(lines[0]);
    }

    None
}

// ═══════════════════════════════════════════════════════════════════════
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

    // Capture the complete raw signed commit object bytes (review blocker
    // 6): the hashed fact records retain them so the exact signed object
    // can be recovered and re-verified without the source Git ODB.
    let raw_object = {
        let odb = git_repo.odb().map_err(|e| CliError::GitError {
            message: format!("Failed to open ODB for {}: {}", oid, e),
        })?;
        let object = odb.read(oid).map_err(|e| CliError::GitError {
            message: format!("Failed to read raw commit object {}: {}", oid, e),
        })?;
        object.data().to_vec()
    };

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

    // CB-9B: multi-parent merges must record the complete union→merge-tree
    // delta, not just the first-parent delta. A path where the merge reverts
    // side-parent content or resolves a conflict differs from the union but
    // may equal the first parent, so the file list is the union of the
    // per-parent diffs (deduped by path; Deleted wins over other states).
    if is_merge {
        trace_git_import(format!(
            "parse-union {} candidate_paths={}",
            short_sha,
            files.len()
        ));
        let mut union_paths: std::collections::BTreeMap<String, ParsedFile> =
            std::collections::BTreeMap::new();
        for file in files {
            union_paths.insert(file.path.clone(), file);
        }
        for parent_number in 1..commit.parent_count() {
            let parent_tree = commit
                .parent(parent_number)
                .map_err(|e| CliError::GitError {
                    message: format!("Failed to get parent {parent_number}: {e}"),
                })?
                .tree()
                .map_err(|e| CliError::GitError {
                    message: format!("Failed to get parent tree: {e}"),
                })?;
            let mut side_opts = DiffOptions::new();
            side_opts.include_untracked(false);
            let side_diff = git_repo
                .diff_tree_to_tree(Some(&parent_tree), Some(&tree), Some(&mut side_opts))
                .map_err(|e| CliError::GitError {
                    message: format!("Failed to compute side-parent diff: {e}"),
                })?;
            for delta in side_diff.deltas() {
                let new_file = delta.new_file();
                let old_file = delta.old_file();
                if new_file.mode() == git2::FileMode::Commit
                    || old_file.mode() == git2::FileMode::Commit
                {
                    continue;
                }
                let path = new_file
                    .path()
                    .or_else(|| old_file.path())
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default();
                if path.is_empty() {
                    continue;
                }
                let operation = if tree_entry_exists(git_repo, &tree, &path) {
                    FileOperation::Modified
                } else {
                    FileOperation::Deleted
                };
                let new_content = if operation == FileOperation::Deleted {
                    None
                } else {
                    get_file_content(git_repo, &tree, &path).ok()
                };
                let mode = if operation == FileOperation::Deleted {
                    None
                } else {
                    git_mode_at(&tree, &path)
                };
                union_paths.insert(
                    path.clone(),
                    ParsedFile {
                        path,
                        operation,
                        new_content,
                        old_content: None,
                        diff_lines: None,
                        old_path: None,
                        new_mode: mode,
                    },
                );
            }
        }
        files = union_paths.into_values().collect();
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
        parent_oids: commit.parent_ids().collect(),
        committer_time: commit.time().seconds(),
        is_merge,
        is_empty,
        push_trailer: parse_push_trailer(commit.message().unwrap_or("")),
        tree_oid: tree.id(),
        parent_tree_oid: parent_tree.as_ref().map(Tree::id),
        raw_object,
    })
}

/// Parse `atomic git push` trailers from a commit message.
///
/// Only matches when the trailers form the message's final paragraph — the
/// shape `atomic git push` itself produces. Trailer lines embedded mid-body
/// (e.g. a GitHub squash-merge message quoting the original commit) do NOT
/// match: those commits may carry conflict resolutions and must be imported.
fn parse_push_trailer(message: &str) -> Option<PushTrailer> {
    let last_paragraph = message.trim_end().rsplit("\n\n").next()?;

    let mut view = None;
    let mut state = None;
    let mut changes = Vec::new();
    for line in last_paragraph.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("Atomic-View:") {
            view = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("Atomic-State:") {
            state = Merkle::from_base32(value.trim().as_bytes());
        } else if let Some(value) = line.strip_prefix("Atomic-Changes:") {
            changes.extend(split_atomic_changes_value(value));
        } else if line.starts_with("Atomic-Manifest:")
            || line.starts_with("Atomic-Policy:")
            || line.starts_with("Atomic-Algorithm:")
            || line.starts_with("Atomic-Tree:")
        {
            // These publication claims are verified by the prospective/state
            // project-tree comparisons rather than trusted as identities.
        } else {
            // Non-trailer content in the final paragraph — not a commit
            // produced by `atomic git push`.
            return None;
        }
    }

    Some(PushTrailer {
        view: view?,
        state: state?,
        changes,
    })
}

/// Extract metadata from a git commit.
///
/// Raw facts are captured losslessly (review blocker 6): name/email bytes
/// come from the raw signature bytes (non-UTF-8 safe) and the timezone
/// offsets from the raw commit object, so no UTF-8 or UTC normalization
/// destroys foreign identity data. The display strings remain best-effort
/// lossy conversions for presentation only.
fn extract_commit_metadata(commit: &git2::Commit) -> CliResult<CommitMetadata> {
    let author = commit.author();
    let author_name = author.name().unwrap_or("Unknown").to_string();
    let author_email = author.email().map(|s| s.to_string());
    let author_name_raw = author.name_bytes().to_vec();
    let author_email_raw = {
        let bytes = author.email_bytes();
        if bytes.is_empty() {
            None
        } else {
            Some(bytes.to_vec())
        }
    };
    let committer = commit.committer();
    let committer_name = committer.name().unwrap_or("Unknown").to_string();
    let committer_email = committer.email().map(|s| s.to_string());
    let committer_name_raw = committer.name_bytes().to_vec();
    let committer_email_raw = {
        let bytes = committer.email_bytes();
        if bytes.is_empty() {
            None
        } else {
            Some(bytes.to_vec())
        }
    };

    let time = commit.time();
    let timestamp = Utc
        .timestamp_opt(time.seconds(), 0)
        .single()
        .unwrap_or_else(Utc::now);
    let author_time = author.when().seconds();
    let author_time_offset_seconds = i64::from(author.when().offset_minutes()) * 60;
    let committer_time = committer.when().seconds();
    let committer_time_offset_seconds = i64::from(committer.when().offset_minutes()) * 60;

    let full_message = commit.message().unwrap_or("");
    let (message, description) = parse_commit_message(full_message);

    Ok(CommitMetadata {
        author_name,
        author_email,
        author_name_raw,
        author_email_raw,
        timestamp,
        message,
        description,
        author_time,
        author_time_offset_seconds,
        committer_name,
        committer_email,
        committer_name_raw,
        committer_email_raw,
        committer_time,
        committer_time_offset_seconds,
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

                // The diff-line map uses the same canonical identity as the
                // parsed deltas (reversible escape), so capture and lookup
                // agree for raw non-UTF-8 paths.
                let line_path = delta
                    .new_file()
                    .path()
                    .or_else(|| delta.old_file().path())
                    .map(|p| {
                        use std::os::unix::ffi::OsStrExt;
                        let raw = p.as_os_str().as_bytes();
                        let needs_escaping = raw
                            .iter()
                            .any(|byte| !byte.is_ascii_graphic() || *byte == b'%');
                        if needs_escaping {
                            atomic_repository::escape_repo_path(raw)
                        } else {
                            String::from_utf8_lossy(raw).into_owned()
                        }
                    })
                    .unwrap_or_default();

                lines_by_path
                    .entry(line_path)
                    .or_default()
                    .push(GitDiffLine {
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

    // CB-9C path fidelity (review R7): a Git delta path is carried as RAW
    // bytes and canonicalized through the reversible `%XX` escape when the
    // bytes are not a plain printable-UTF-8 path without `%`. The mapping is
    // injective, so the escaped String identity recovers the exact Git tree
    // bytes (fixture oracle) and normalization never becomes identity.
    // Refusing here (the historical behavior) lost the file from the imported
    // history instead of preserving it.
    let delta_path = |file: &git2::DiffFile| -> CliResult<Option<String>> {
        let Some(path) = file.path() else {
            return Ok(None);
        };
        use std::os::unix::ffi::OsStrExt;
        let raw = path.as_os_str().as_bytes();
        let needs_escaping = raw
            .iter()
            .any(|byte| !byte.is_ascii_graphic() || *byte == b'%');
        if needs_escaping {
            Ok(Some(atomic_repository::escape_repo_path(raw)))
        } else {
            // ASCII-safe and UTF-8 by construction.
            Ok(Some(String::from_utf8_lossy(raw).into_owned()))
        }
    };

    let mut files = Vec::new();

    for delta in diff.deltas() {
        let new_file = delta.new_file();
        let old_file = delta.old_file();

        let new_mode = canonical_git_mode(new_file.mode());
        let old_mode = canonical_git_mode(old_file.mode());
        let is_gitlink = new_mode == 0o160000 || old_mode == 0o160000;

        // CB-9C gitlink fidelity: submodule entries are synthesized as
        // gitlink paths (lowercase hexadecimal object-ID repository bytes and
        // a `Gitlink` kind attribute). They are never recursed into and never
        // emitted as regular files; dropping them silently would lose the
        // submodule identity from every imported state.
        if is_gitlink {
            let path = if operation_is_delete(delta.status()) {
                delta_path(&old_file)?
            } else {
                delta_path(&new_file)?
            };
            let Some(path) = path else { continue };
            let operation = match delta.status() {
                Delta::Added => FileOperation::Added,
                Delta::Modified => FileOperation::Modified,
                Delta::Deleted => FileOperation::Deleted,
                Delta::Renamed => FileOperation::Renamed,
                _ => {
                    return Err(git_error(format!(
                        "unsupported delta status for gitlink path '{}'",
                        path
                    )))
                }
            };
            let target = if operation == FileOperation::Deleted {
                Vec::new()
            } else {
                let algorithm = git_algorithm_for_sha(&new_file.id().to_string())?;
                oid_bytes_as_hex(&git_object_id(algorithm, new_file.id())?)
            };
            // The previous gitlink target: parent-tree entries of kind Commit
            // are not blobs, so `get_file_content` cannot read them — read
            // the entry's tagged object id directly (CB-9C).
            let old_target = if matches!(
                operation,
                FileOperation::Modified | FileOperation::Deleted | FileOperation::Renamed
            ) {
                parent_tree.and_then(|pt| {
                    pt.get_path(&canonical_tree_path(&path))
                        .ok()
                        .filter(|entry| entry.kind() == Some(git2::ObjectType::Commit))
                        .and_then(|entry| {
                            let algorithm = git_algorithm_for_sha(&entry.id().to_string()).ok()?;
                            Some(oid_bytes_as_hex(
                                &git_object_id(algorithm, entry.id()).ok()?,
                            ))
                        })
                })
            } else {
                None
            };
            files.push(ParsedFile {
                path,
                operation,
                new_content: (!target.is_empty()).then_some(target),
                old_content: old_target,
                diff_lines: None,
                old_path: None,
                new_mode: (operation != FileOperation::Deleted).then_some(0o160000),
            });
            continue;
        }

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
            // CB-9C kind fidelity: a regular ↔ symlink typechange is one
            // Modified delta — old bytes → new bytes — with the kind and
            // mode riding the graph-backed attribute registers. Ignoring it
            // would silently lose the conversion from every imported state.
            Delta::Typechange => FileOperation::Modified,
            _ => continue,
        };

        // CB-9C path fidelity (review R7): raw bytes carry through the
        // reversible escape; the tree reads decode the identity.
        let path = match delta_path(&new_file)? {
            Some(path) => path,
            None => delta_path(&old_file)?.unwrap_or_default(),
        };

        let old_path = if matches!(operation, FileOperation::Renamed | FileOperation::Copied) {
            delta_path(&old_file)?
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
            new_mode: (operation != FileOperation::Deleted)
                .then(|| canonical_git_mode(new_file.mode())),
        });
    }

    // CB-9C kind fidelity: a Git typechange (regular ↔ symlink, executable
    // flip on a kind change) reaches this parser either as one `Typechange`
    // delta or — when rename detection is on — as a same-path delete+add
    // pair. Recording the pair as separate FileDel + edit hunks resurrects
    // the inode's original name vertex (the deleted path's stable identity
    // falls back to its first claim), so normalize both shapes into one
    // Modified delta carrying the exact old bytes, new bytes, and the new
    // canonical mode. Kind and mode then ride the graph-backed attribute
    // registers instead of a spurious delete/recreate.
    let mut normalized: Vec<ParsedFile> = Vec::with_capacity(files.len());
    for file in files {
        if file.operation == FileOperation::Added {
            if let Some(previous) = normalized.iter_mut().rev().find(|previous| {
                previous.path == file.path && previous.operation == FileOperation::Deleted
            }) {
                // Merge delete+add into one typechange modification.
                previous.operation = FileOperation::Modified;
                previous.new_content = file.new_content.clone();
                previous.new_mode = file.new_mode;
                continue;
            }
        }
        normalized.push(file);
    }

    Ok(normalized)
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
                    new_mode: git_mode_at(tree, path),
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
                    new_mode: git_mode_at(tree, path),
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
                    new_mode: None,
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
                    new_mode: git_mode_at(tree, path),
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
                    new_mode: git_mode_at(tree, path),
                });
            }
            _ => {}
        }
    }

    Ok(files)
}

fn canonical_git_mode(mode: git2::FileMode) -> u32 {
    match mode {
        git2::FileMode::Tree => 0o040000,
        git2::FileMode::Blob | git2::FileMode::BlobGroupWritable => 0o100644,
        git2::FileMode::BlobExecutable => 0o100755,
        git2::FileMode::Link => 0o120000,
        git2::FileMode::Commit => 0o160000,
        git2::FileMode::Unreadable => 0,
    }
}

fn operation_is_delete(status: git2::Delta) -> bool {
    status == Delta::Deleted
}

/// Approximate byte similarity in basis points (`0..=10_000`) (CB-9C).
///
/// Import rename detection is heuristic evidence, so the score documents the
/// basis rather than claiming a metric: byte-identical content scores full
/// marks, differing content uses a common prefix+suffix ratio over the
/// longer byte sequence. The multiply runs in widened `u64` (review CB-9C
/// R4): a `u32` shared-bytes × 10,000 product overflowed — panic in debug,
/// silent wrap in release — for supported inputs above ~429 KB, which the
/// larger-file corpus requirement makes a normal case, not an exception.
fn similarity_bps(old_bytes: &[u8], new_bytes: &[u8]) -> u16 {
    if old_bytes == new_bytes {
        return 10_000;
    }
    if old_bytes.is_empty() && new_bytes.is_empty() {
        return 10_000;
    }
    let common_prefix = old_bytes
        .iter()
        .zip(new_bytes.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let common_suffix = old_bytes[common_prefix..]
        .iter()
        .rev()
        .zip(new_bytes[common_prefix.min(new_bytes.len())..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let denominator = old_bytes.len().max(new_bytes.len()) as u64;
    let shared = (common_prefix + common_suffix).min(denominator as usize) as u64;

    if denominator == 0 {
        10_000
    } else {
        shared
            .checked_mul(10_000)
            .map(|scaled| (scaled / denominator).min(9_999) as u16)
            .unwrap_or(10_000)
    }
}

/// The graph-backed materialization (kind + mode) one canonical Git mode
/// requires (CB-9C). Symlinks project the platform-independent `0o777` the
/// prospective manifest expects; gitlinks project `0o644` with the lowercase
/// hexadecimal object ID as repository bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ExpectedMaterialization {
    kind: atomic_core::change::InodeKind,
    mode: u16,
}

impl ExpectedMaterialization {
    fn kind(self) -> atomic_core::change::InodeKind {
        self.kind
    }

    fn mode(self) -> u16 {
        self.mode
    }
}

fn expected_materialization(git_mode: u32) -> CliResult<ExpectedMaterialization> {
    let expected = match git_mode {
        0o100644 => ExpectedMaterialization {
            kind: atomic_core::change::InodeKind::Regular,
            mode: 0o644,
        },
        0o100755 => ExpectedMaterialization {
            kind: atomic_core::change::InodeKind::Regular,
            mode: 0o755,
        },
        0o120000 => ExpectedMaterialization {
            kind: atomic_core::change::InodeKind::Symlink,
            mode: 0o777,
        },
        0o160000 => ExpectedMaterialization {
            kind: atomic_core::change::InodeKind::Gitlink,
            mode: 0o644,
        },
        mode => {
            return Err(git_error(format!(
                "unsupported Git file mode {mode:#o}; refusing to synthesize \
                 attributes that cannot project this representation"
            )))
        }
    };
    Ok(expected)
}

fn git_mode_at(tree: &Tree<'_>, path: &str) -> Option<u32> {
    tree.get_path(Path::new(path))
        .ok()
        .map(|entry| entry.filemode() as u32)
}

/// Whether a path exists in the tree (any entry kind).
fn tree_entry_exists(_git_repo: &GitRepository, tree: &Tree, path: &str) -> bool {
    tree.get_path(Path::new(path)).is_ok()
}

/// Get file content from a git tree.
/// A Git tree lookup path from the canonical String identity: the reversible
/// escape decodes back to the exact raw bytes (review CB-9C R7).
fn canonical_tree_path(path: &str) -> PathBuf {
    match atomic_repository::unescape_repo_path(path) {
        Ok(raw) => {
            use std::os::unix::ffi::OsStringExt;
            PathBuf::from(std::ffi::OsString::from_vec(raw))
        }
        Err(_) => PathBuf::from(path),
    }
}

fn get_file_content(git_repo: &GitRepository, tree: &Tree, path: &str) -> CliResult<Vec<u8>> {
    // CB-9C path fidelity: the String identity is the reversible escape of
    // the raw Git bytes, so the tree lookup decodes before reading.
    let entry = tree
        .get_path(&canonical_tree_path(path))
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

    fn added_commit(path: &str, content: &[u8]) -> ParsedCommit {
        let policy = ConversionPolicy::new(GitHashAlgorithm::Sha1);
        let entry = RepositoryEntry::new(
            RepoPath::from_bytes(path.as_bytes()).unwrap(),
            content.to_vec(),
            0o644,
            atomic_core::change::InodeKind::Regular,
            None,
            ManifestDisposition::Included,
        )
        .unwrap();
        let manifest =
            RepositoryManifest::new(SetId::ZERO, policy.root().content_key, vec![entry]).unwrap();
        let project = ProjectTree::from_manifest(manifest, &policy).unwrap();
        let tree_oid = Oid::from_bytes(project.git.root.as_bytes()).unwrap();
        ParsedCommit {
            git_sha: "0123456789abcdef".to_string(),
            short_sha: "01234567".to_string(),
            metadata: CommitMetadata {
                author_name: "Test".to_string(),
                author_email: Some("test@example.com".to_string()),
                author_name_raw: b"Test".to_vec(),
                author_email_raw: Some(b"test@example.com".to_vec()),
                timestamp: Utc::now(),
                message: format!("add {path}"),
                description: None,
                author_time: 1_700_000_000,
                author_time_offset_seconds: 0,
                committer_name: "Test".to_string(),
                committer_email: Some("test@example.com".to_string()),
                committer_name_raw: b"Test".to_vec(),
                committer_email_raw: Some(b"test@example.com".to_vec()),
                committer_time: 1_700_000_000,
                committer_time_offset_seconds: 0,
            },
            files: vec![ParsedFile {
                path: path.to_string(),
                operation: FileOperation::Added,
                new_content: Some(content.to_vec()),
                old_content: None,
                diff_lines: None,
                old_path: None,
                new_mode: Some(0o100644),
            }],
            parent_index: None,
            parent_oids: Vec::new(),
            committer_time: 1_700_000_000,
            is_merge: false,
            is_empty: false,
            push_trailer: None,
            tree_oid,
            parent_tree_oid: None,
            raw_object: Vec::new(),
        }
    }

    fn verified_for(parsed: &ParsedCommit) -> VerifiedProspectiveEquivalence {
        ProspectiveProjectTree::new(ConversionPolicy::new(GitHashAlgorithm::Sha1), None)
            .unwrap()
            .apply_commit(parsed, None)
            .unwrap()
            .1
    }

    #[test]
    fn warm_prospective_preflight_is_deterministic_and_millisecond_scale() {
        let policy = ConversionPolicy::new(GitHashAlgorithm::Sha1);
        let entries = (0..2_000)
            .map(|index| {
                let path =
                    RepoPath::from_bytes(format!("src/file-{index:04}.rs").as_bytes()).unwrap();
                RepositoryEntry::new(
                    path,
                    format!("pub const VALUE_{index}: usize = {index};\n").into_bytes(),
                    0o644,
                    atomic_core::change::InodeKind::Regular,
                    None,
                    ManifestDisposition::Included,
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        let baseline = ProjectTree::from_manifest(
            RepositoryManifest::new(SetId::ZERO, policy.root().content_key, entries.clone())
                .unwrap(),
            &policy,
        )
        .unwrap();
        let mut changed_entries = entries;
        let changed_path = RepoPath::from_bytes(b"src/file-1000.rs").unwrap();
        changed_entries[1_000] = RepositoryEntry::new(
            changed_path,
            b"pub const VALUE_1000: usize = 42;\n".to_vec(),
            0o644,
            atomic_core::change::InodeKind::Regular,
            None,
            ManifestDisposition::Included,
        )
        .unwrap();
        let expected = ProjectTree::from_manifest(
            RepositoryManifest::new(SetId::ZERO, policy.root().content_key, changed_entries)
                .unwrap(),
            &policy,
        )
        .unwrap();
        let mut parsed = added_commit("src/file-1000.rs", b"pub const VALUE_1000: usize = 42;\n");
        parsed.files[0].operation = FileOperation::Modified;
        parsed.parent_tree_oid = Some(Oid::from_bytes(baseline.git.root.as_bytes()).unwrap());
        parsed.tree_oid = Oid::from_bytes(expected.git.root.as_bytes()).unwrap();

        let mut prospective = ProspectiveProjectTree::new(policy, Some(baseline)).unwrap();
        let started = Instant::now();
        let (first, _) = prospective.apply_commit(&parsed, None).unwrap();
        let elapsed = started.elapsed();
        let (second, _) = prospective.apply_commit(&parsed, None).unwrap();
        eprintln!(
            "CB-4B warm prospective preflight: {:.3}ms for 2,000 files / 1 changed entry",
            elapsed.as_secs_f64() * 1_000.0
        );
        assert_eq!(first.manifest.root(), second.manifest.root());
        assert_eq!(first.git.root, expected.git.root);
        assert!(
            elapsed < Duration::from_millis(500),
            "warm prospective preflight took {elapsed:?}"
        );
    }

    #[test]
    fn graph_first_nested_add_uses_parent_inode_anchors_and_dependencies() {
        let parsed = added_commit("src/domain/model.rs", b"pub struct Model;\n");
        let mut line_index = ImportLineIndex::default();
        let (change, pending, _) =
            build_graph_first_change(ChangeHeader::new("nested"), &parsed, &line_index, true)
                .unwrap();
        let root = Position {
            change: Some(ContentHash::NONE),
            pos: ChangePosition::ROOT,
        };

        let domain_anchor = match change.hunks() {
            [GraphOp::DirAdd {
                add_name: src_name,
                add_inode: src_inode,
                path: src_path,
            }, GraphOp::DirAdd {
                add_name: domain_name,
                add_inode: domain_inode,
                path: domain_path,
            }, GraphOp::FileAdd {
                add_name: file_name,
                path: file_path,
                ..
            }, ..] => {
                assert_eq!(src_path, "src");
                assert_eq!(domain_path, "src/domain");
                assert_eq!(file_path, "src/domain/model.rs");
                assert_eq!(src_name.predecessors, vec![root]);
                assert_eq!(src_name.inode, root);

                let src_anchor = Position {
                    change: None,
                    pos: src_inode.start,
                };
                assert_eq!(domain_name.predecessors, vec![src_anchor]);
                assert_eq!(domain_name.inode, src_anchor);

                let domain_anchor = Position {
                    change: None,
                    pos: domain_inode.start,
                };
                assert_eq!(file_name.predecessors, vec![domain_anchor]);
                assert_eq!(file_name.inode, domain_anchor);
                domain_anchor
            }
            hunks => panic!("unexpected graph-first topology: {hunks:#?}"),
        };
        assert!(change.dependencies().is_empty());

        let first_hash = ContentHash::of(b"first imported change");
        apply_line_index_updates(&mut line_index, first_hash, pending);
        let second = added_commit("src/domain/service.rs", b"pub struct Service;\n");
        let (second_change, _, _) = build_graph_first_change(
            ChangeHeader::new("nested sibling"),
            &second,
            &line_index,
            true,
        )
        .unwrap();
        assert!(!second_change
            .hunks()
            .iter()
            .any(|op| matches!(op, GraphOp::DirAdd { .. })));
        let parent = second_change
            .hunks()
            .iter()
            .find_map(|op| match op {
                GraphOp::FileAdd { add_name, .. } => Some(add_name.predecessors[0]),
                _ => None,
            })
            .unwrap();
        assert_eq!(parent.change, Some(first_hash));
        assert_eq!(parent.pos, domain_anchor.pos);
        assert!(second_change.dependencies().contains(&first_hash));
    }

    #[test]
    fn graph_first_nested_add_writes_and_materializes_after_reopen() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = Repository::init(temp.path()).unwrap();
        let parsed = added_commit("src/domain/model.rs", b"git model\n");
        let (change, _, deleted) = build_graph_first_change(
            ChangeHeader::new("git nested"),
            &parsed,
            &ImportLineIndex::default(),
            true,
        )
        .unwrap();
        let first_verified = verified_for(&parsed);
        let first = repo
            .write_import_graph_change(change, &deleted, false, &first_verified, Default::default())
            .unwrap();
        assert_eq!(
            repo.get_file_content("src/domain/model.rs").unwrap(),
            Some(b"git model\n".to_vec())
        );

        // Simulate a later incremental import process: rebuild only the parent
        // directory anchors from durable repository metadata.
        drop(repo);
        let reopened = Repository::open(temp.path()).unwrap();
        let second = added_commit("src/domain/service.rs", b"git service\n");
        let mut rebuilt_index = ImportLineIndex::default();
        rebuilt_index
            .seed_missing_parent_directories(&reopened, &second)
            .unwrap();
        let (second_change, _, second_deleted) = build_graph_first_change(
            ChangeHeader::new("git nested sibling"),
            &second,
            &rebuilt_index,
            true,
        )
        .unwrap();
        assert!(second_change.dependencies().contains(&first.hash));
        let second_verified = verified_for(&second);
        reopened
            .write_import_graph_change(
                second_change,
                &second_deleted,
                false,
                &second_verified,
                Default::default(),
            )
            .unwrap();

        drop(reopened);
        let final_repo = Repository::open(temp.path()).unwrap();
        let working_copy = final_repo.require_working_copy_id().unwrap();
        final_repo.materialize(working_copy).unwrap();
        assert_eq!(
            std::fs::read(temp.path().join("src/domain/model.rs")).unwrap(),
            b"git model\n"
        );
        assert_eq!(
            std::fs::read(temp.path().join("src/domain/service.rs")).unwrap(),
            b"git service\n"
        );
    }

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

    // ═══════════════════════════════════════════════════════════════════
    // Self-push trailer detection (round-trip dedup)
    // ═══════════════════════════════════════════════════════════════════

    fn test_state() -> (String, Merkle) {
        let state = Merkle::of(b"test view state");
        (state.to_base32(), state)
    }

    #[test]
    fn test_parse_push_trailer_full_message() {
        let (state_b32, state) = test_state();
        let msg = format!(
            "feat: add greet\n\nSome body text.\n\nAtomic-View: main\nAtomic-State: {}\nAtomic-Changes: ABC, DEF\n",
            state_b32
        );
        let trailer = parse_push_trailer(&msg).expect("should parse");
        assert_eq!(trailer.view, "main");
        assert_eq!(trailer.state, state);
    }

    #[test]
    fn test_parse_push_trailer_without_changes_trailer() {
        let (state_b32, state) = test_state();
        let msg = format!(
            "Working copy changes\n\nAtomic-View: dev\nAtomic-State: {}",
            state_b32
        );
        let trailer = parse_push_trailer(&msg).expect("should parse");
        assert_eq!(trailer.view, "dev");
        assert_eq!(trailer.state, state);
    }

    #[test]
    fn test_parse_push_trailer_rejects_embedded_trailers() {
        let (state_b32, _) = test_state();
        // GitHub squash-merge shape: the original commit (trailers and all)
        // is quoted mid-message, with more content after it.
        let msg = format!(
            "feat: add greet (#42)\n\n* feat: add greet\n\nAtomic-View: main\nAtomic-State: {}\nAtomic-Changes: ABC\n\nCo-authored-by: Dana <dana@acme.dev>",
            state_b32
        );
        assert!(parse_push_trailer(&msg).is_none());
    }

    #[test]
    fn test_parse_push_trailer_rejects_mixed_final_paragraph() {
        let (state_b32, _) = test_state();
        let msg = format!(
            "feat: add greet\n\nAtomic-View: main\nAtomic-State: {}\nsome stray line",
            state_b32
        );
        assert!(parse_push_trailer(&msg).is_none());
    }

    #[test]
    fn test_parse_push_trailer_rejects_missing_state() {
        assert!(parse_push_trailer("msg\n\nAtomic-View: main").is_none());
    }

    #[test]
    fn test_parse_push_trailer_rejects_invalid_state() {
        assert!(
            parse_push_trailer("msg\n\nAtomic-View: main\nAtomic-State: not-valid-base32!!")
                .is_none()
        );
    }

    #[test]
    fn test_parse_push_trailer_empty_message() {
        assert!(parse_push_trailer("").is_none());
        assert!(parse_push_trailer("plain subject, no trailers").is_none());
    }

    fn self_push_commit(view: &str, state_b32: &str) -> ParsedCommit {
        ParsedCommit {
            git_sha: "aabbccdd11223344".to_string(),
            short_sha: "aabbccdd".to_string(),
            metadata: CommitMetadata {
                author_name: "Test".to_string(),
                author_email: None,
                author_name_raw: b"Test".to_vec(),
                author_email_raw: None,
                timestamp: Utc::now(),
                message: "feat: test".to_string(),
                description: None,
                author_time: 1_700_000_000,
                author_time_offset_seconds: 0,
                committer_name: "Test".to_string(),
                committer_email: None,
                committer_name_raw: b"Test".to_vec(),
                committer_email_raw: None,
                committer_time: 1_700_000_000,
                committer_time_offset_seconds: 0,
            },
            files: Vec::new(),
            parent_index: None,
            parent_oids: Vec::new(),
            committer_time: 1_700_000_000,
            is_merge: false,
            is_empty: true,
            push_trailer: parse_push_trailer(&format!(
                "feat: test\n\nAtomic-View: {}\nAtomic-State: {}",
                view, state_b32
            )),
            tree_oid: Oid::zero(),
            parent_tree_oid: None,
            raw_object: Vec::new(),
        }
    }

    #[test]
    fn test_should_skip_self_push() {
        let (state_b32, state) = test_state();
        let mut options = ParallelImportOptions {
            incremental: true,
            target_view: "main".to_string(),
            ..ParallelImportOptions::default()
        };
        options.known_states.insert(state);

        let parsed = self_push_commit("main", &state_b32);
        assert!(should_skip_self_push(&parsed, &options));

        // Wrong view (e.g. commit pushed from `dev`, now imported on `main`)
        let mut options_dev = options.clone();
        options_dev.target_view = "dev".to_string();
        assert!(!should_skip_self_push(&parsed, &options_dev));

        // Unknown state → must import (content may not be present locally)
        let mut options_empty = options.clone();
        options_empty.known_states = HashSet::new();
        assert!(!should_skip_self_push(&parsed, &options_empty));

        // No trailer → never skipped
        let mut plain = self_push_commit("main", &state_b32);
        plain.push_trailer = None;
        assert!(!should_skip_self_push(&plain, &options));

        // Full (non-incremental) imports never skip
        let mut options_full = options.clone();
        options_full.incremental = false;
        assert!(!should_skip_self_push(&parsed, &options_full));
    }

    #[test]
    fn test_incremental_import_skips() {
        let (state_b32, state) = test_state();
        let self_push_msg = format!(
            "feat: test\n\nAtomic-View: main\nAtomic-State: {}",
            state_b32
        );
        let plain_msg = "feat: ordinary human commit";

        let mut imported = HashSet::new();
        imported.insert("already-imported-sha".to_string());
        let mut known_states = HashSet::new();
        known_states.insert(state);

        // Already-imported SHA is skipped regardless of message.
        assert!(incremental_import_skips(
            "already-imported-sha",
            plain_msg,
            &imported,
            "main",
            &known_states,
        ));

        // Self-push commit whose state is already in the target view is skipped.
        assert!(incremental_import_skips(
            "new-sha",
            &self_push_msg,
            &imported,
            "main",
            &known_states,
        ));

        // Self-push commit for a DIFFERENT view is not skipped.
        assert!(!incremental_import_skips(
            "new-sha",
            &self_push_msg,
            &imported,
            "dev",
            &known_states,
        ));

        // Self-push commit whose state is unknown is not skipped.
        assert!(!incremental_import_skips(
            "new-sha",
            &self_push_msg,
            &imported,
            "main",
            &HashSet::new(),
        ));

        // An ordinary new commit is imported (not skipped).
        assert!(!incremental_import_skips(
            "new-sha",
            plain_msg,
            &imported,
            "main",
            &known_states,
        ));
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
    fn incremental_first_parent_collection_stops_at_indexed_frontier() {
        let dir = tempfile::tempdir().unwrap();
        let git_repo = GitRepository::init(dir.path()).unwrap();
        let signature = git2::Signature::now("Atomic", "atomic@example.com").unwrap();
        let tree_oid = {
            let mut index = git_repo.index().unwrap();
            index.write_tree().unwrap()
        };
        let tree = git_repo.find_tree(tree_oid).unwrap();
        let first_oid = git_repo
            .commit(Some("HEAD"), &signature, &signature, "root", &tree, &[])
            .unwrap();
        let mut parent_oid = first_oid;
        for number in 0..250 {
            let parent = git_repo.find_commit(parent_oid).unwrap();
            parent_oid = git_repo
                .commit(
                    Some("HEAD"),
                    &signature,
                    &signature,
                    &format!("history {number}"),
                    &tree,
                    &[&parent],
                )
                .unwrap();
        }
        let frontier = parent_oid;
        let parent = git_repo.find_commit(parent_oid).unwrap();
        let tip = git_repo
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "one-line incremental commit",
                &tree,
                &[&parent],
            )
            .unwrap();
        let branch = git_repo.head().unwrap().shorthand().unwrap().to_string();
        let options = ParallelImportOptions {
            incremental: true,
            imported_shas: HashSet::from([frontier.to_string()]),
            mainline_only: true,
            ..ParallelImportOptions::default()
        };
        let importer = ParallelImporter::new(&git_repo, options);

        let started = Instant::now();
        let collected = importer.collect_commit_oids(&git_repo, &branch).unwrap();
        let elapsed = started.elapsed();

        assert_eq!(collected, vec![tip]);
        assert!(
            elapsed < Duration::from_secs(1),
            "incremental frontier lookup took {elapsed:?}"
        );
    }

    // ═══════════════════════════════════════════════════════════════════
    // CB-9A: deterministic sequencing, closure boundaries, deepening
    // ═══════════════════════════════════════════════════════════════════

    /// Build a real Git repository with a linear chain of commits, each
    /// committed at the exact same committer time (so every ordering decision
    /// inside a level must come from the tie-break rules).
    fn chained_git_repo(commits: usize, committer_time: i64) -> (tempfile::TempDir, GitRepository) {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = GitRepository::init(temp.path()).unwrap();
        repo.set_head("refs/heads/main").unwrap();
        let signature = git2::Signature::new(
            "Bridge",
            "bridge@example.com",
            &git2::Time::new(committer_time, 0),
        )
        .unwrap();
        let mut parent: Option<git2::Oid> = None;
        for index in 0..commits {
            let name = format!("file-{index:04}.txt");
            fs::write(
                temp.path().join(&name),
                format!("content {index}\n").as_bytes(),
            )
            .unwrap();
            let mut tree_index = repo.index().unwrap();
            tree_index.add_path(Path::new(&name)).unwrap();
            tree_index.write().unwrap();
            let tree = repo.find_tree(tree_index.write_tree().unwrap()).unwrap();
            let parent_commit = parent.map(|oid| repo.find_commit(oid).unwrap());
            let parents: Vec<&git2::Commit> = match &parent_commit {
                Some(commit) => vec![commit],
                None => Vec::new(),
            };
            parent = Some(
                repo.commit(
                    Some("refs/heads/main"),
                    &signature,
                    &signature,
                    &name,
                    &tree,
                    &parents,
                )
                .unwrap(),
            );
        }
        (temp, repo)
    }

    #[test]
    fn bridge_sequencing_orders_parents_before_children_and_is_deterministic() {
        let (_temp, git) = chained_git_repo(4, 1_700_000_000);
        // Collect in an arbitrary order; sequencing must restore the causal
        // order regardless of input order.
        let mut oids: Vec<Oid> = Vec::new();
        let mut walker = git
            .find_commit(
                git.find_branch("main", git2::BranchType::Local)
                    .unwrap()
                    .get()
                    .target()
                    .unwrap(),
            )
            .unwrap();
        loop {
            oids.push(walker.id());
            match walker.parent_count() {
                0 => break,
                _ => walker = walker.parent(0).unwrap(),
            }
        }
        oids.reverse();

        let first = sequence_commits_deterministically(&git, &oids).unwrap().0;
        let mut reversed = oids.clone();
        reversed.reverse();
        let second = sequence_commits_deterministically(&git, &reversed)
            .unwrap()
            .0;
        assert_eq!(first.len(), 4);
        assert_eq!(first, second, "sequencing must be deterministic");
        // Parents precede children.
        for (position, oid) in first.iter().enumerate() {
            let commit = git.find_commit(*oid).unwrap();
            for parent in commit.parent_ids() {
                let parent_position = first.iter().position(|candidate| *candidate == parent);
                assert!(
                    parent_position.map(|p| p < position).unwrap_or(true),
                    "parent {parent} must be sequenced before child {oid}"
                );
            }
        }
    }

    #[test]
    fn bridge_sequencing_breaks_ties_by_committer_time_then_tagged_oid() {
        // Two independent roots committed at the same time: the ready set has
        // two entries, so the (committer time, OID) rule decides the order.
        let temp = tempfile::TempDir::new().unwrap();
        let git = GitRepository::init(temp.path()).unwrap();
        let signature = git2::Signature::new(
            "Bridge",
            "bridge@example.com",
            &git2::Time::new(1_700_000_000, 0),
        )
        .unwrap();
        let mut roots: Vec<Oid> = Vec::new();
        for (name, branch) in [
            ("zebra.txt", "refs/heads/zebra"),
            ("alpha.txt", "refs/heads/alpha"),
        ] {
            fs::write(temp.path().join(name), b"content\n").unwrap();
            let mut tree_index = git.index().unwrap();
            tree_index.add_path(Path::new(name)).unwrap();
            tree_index.write().unwrap();
            let tree = git.find_tree(tree_index.write_tree().unwrap()).unwrap();
            roots.push(
                git.commit(Some(branch), &signature, &signature, name, &tree, &[])
                    .unwrap(),
            );
        }
        let (ordered, tie_breaks) = sequence_commits_deterministically(&git, &roots).unwrap();
        assert_eq!(ordered.len(), 2);
        assert!(
            tie_breaks >= 1,
            "the tie must be recorded as a bridge decision"
        );
        // Same committer time: the lower tagged OID bytes win.
        let mut by_oid = roots.clone();
        by_oid.sort_by_key(|oid| oid.as_bytes().to_vec());
        assert_eq!(ordered, by_oid, "ties break by tagged OID bytes");
        // A later committer time wins over a lower OID.
        let later_signature = git2::Signature::new(
            "Bridge",
            "bridge@example.com",
            &git2::Time::new(1_700_000_100, 0),
        )
        .unwrap();
        fs::write(temp.path().join("late.txt"), b"content\n").unwrap();
        let mut tree_index = git.index().unwrap();
        tree_index.add_path(Path::new("late.txt")).unwrap();
        tree_index.write().unwrap();
        let tree = git.find_tree(tree_index.write_tree().unwrap()).unwrap();
        let late_root = git
            .commit(
                Some("refs/heads/late"),
                &later_signature,
                &later_signature,
                "late",
                &tree,
                &[],
            )
            .unwrap();
        let (ordered_with_late, _) =
            sequence_commits_deterministically(&git, &[late_root, roots[0]]).unwrap();
        assert_eq!(
            ordered_with_late[0], roots[0],
            "the earlier committer time is sequenced first"
        );
        assert_eq!(ordered_with_late[1], late_root);
    }

    #[test]
    fn shallow_and_promisor_boundaries_are_detected_and_labeled() {
        let (_temp, git) = chained_git_repo(2, 1_700_000_000);
        assert!(detect_git_boundaries(&git).unwrap().is_empty());

        // A shallow file marks the shallow boundary.
        fs::write(
            git.path().join("shallow"),
            format!(
                "{}\n",
                git.find_commit(
                    git.find_branch("main", git2::BranchType::Local)
                        .unwrap()
                        .get()
                        .target()
                        .unwrap()
                )
                .unwrap()
                .id()
            ),
        )
        .unwrap();
        let boundaries = detect_git_boundaries(&git).unwrap();
        assert!(boundaries.contains(&GitClosureBoundary::Shallow));

        // A promisor remote marks the partial-clone boundary.
        let mut config = git.config().unwrap();
        config.set_str("remote.origin.promisor", "true").unwrap();
        let boundaries = detect_git_boundaries(&git).unwrap();
        assert!(boundaries.contains(&GitClosureBoundary::Promisor));
    }

    #[test]
    fn missing_parent_objects_fail_closed_unless_declared_shallow() {
        // A real shallow clone: the boundary commit references a parent whose
        // object was never fetched, so the object database is genuinely
        // incomplete below the boundary.
        let (source_temp, _source) = chained_git_repo(3, 1_700_000_000);
        let clone_temp = tempfile::TempDir::new().unwrap();
        let output = Command::new("git")
            .args([
                "clone",
                "--depth",
                "1",
                "--quiet",
                &format!("file://{}", source_temp.path().display()),
                clone_temp.path().to_str().unwrap(),
            ])
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("run git clone --depth 1");
        assert!(
            output.status.success(),
            "shallow clone failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let git = GitRepository::open(clone_temp.path()).unwrap();
        assert!(
            detect_git_boundaries(&git)
                .unwrap()
                .contains(&GitClosureBoundary::Shallow),
            "the clone must report the shallow boundary"
        );
        let tip = git
            .find_branch(
                git.head().unwrap().shorthand().unwrap_or_default(),
                git2::BranchType::Local,
            )
            .or_else(|_| git.find_branch("main", git2::BranchType::Local))
            .or_else(|_| git.find_branch("master", git2::BranchType::Local))
            .unwrap()
            .get()
            .target()
            .unwrap();
        let shallow = shallow_boundary_oids(&git);
        assert!(
            shallow.contains(&tip.to_string()),
            "the tip is the shallow boundary"
        );

        // With the boundary declared, libgit2 grafts the boundary commit as a
        // parentless root and the closure resolves.
        verify_commit_closure_objects_with_shallow(&git, &[tip], &shallow).unwrap();

        // Undeclared, the same object database is an explicit refusal: the
        // commit object still names its parent, but the parent was never
        // fetched. Refusing — never silently fabricating a root. The handle
        // is reopened because libgit2 caches shallow grafts at open time.
        fs::remove_file(git.path().join("shallow")).unwrap();
        drop(git);
        let git = GitRepository::open(clone_temp.path()).unwrap();
        let error = verify_commit_closure_objects_with_shallow(&git, &[tip], &HashSet::new())
            .expect_err("an undeclared missing parent must refuse");
        assert!(
            error.to_string().contains("closure boundary"),
            "explicit boundary diagnostic required, found: {error}"
        );
        drop(source_temp);
    }

    #[test]
    fn deepened_history_never_silently_reinterprets_prior_sequencing() {
        // A → B → C. B was already imported (the shallow boundary at the time
        // of the earlier import). After deepening, A exists below the indexed
        // B: a full import must refuse rather than re-derive A under B.
        let (_temp, git) = chained_git_repo(3, 1_700_000_000);
        let tip = git
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .target()
            .unwrap();
        let b = git.find_commit(tip).unwrap().parent_id(0).unwrap();
        let (ordered, _) = sequence_commits_deterministically(
            &git,
            &[git.find_commit(b).unwrap().parent_id(0).unwrap(), b, tip],
        )
        .unwrap();
        let indexed: HashSet<String> = [b.to_string()].into_iter().collect();
        let error = detect_deepened_history(&git, &ordered, &indexed)
            .expect_err("deepened history must be refused");
        assert!(
            error.to_string().contains("deepened"),
            "explicit deepening diagnostic required, found: {error}"
        );

        // New commits ABOVE the indexed frontier are ordinary work.
        let indexed_above: HashSet<String> = [ordered[0].to_string(), ordered[1].to_string()]
            .into_iter()
            .collect();
        detect_deepened_history(&git, &ordered, &indexed_above)
            .expect("new commits above the frontier are ordinary work");
    }

    #[test]
    fn test_finalize_rejects_partial_import() {
        let dir = tempfile::tempdir().unwrap();
        let git_repo = GitRepository::init(dir.path()).unwrap();
        let importer = ParallelImporter::new(&git_repo, ParallelImportOptions::default());
        let stats = ImportStats {
            commits_parsed: 2,
            changes_written: 1,
            ..ImportStats::default()
        };

        assert!(matches!(
            importer.phase3_finalize(&stats),
            Err(CliError::GitError { .. })
        ));
    }

    #[test]
    fn test_import_batch_size_is_fixed_at_1000() {
        assert_eq!(ParallelImporter::batch_size_for(0), 1_000);
        assert_eq!(ParallelImporter::batch_size_for(82), 1_000);
        assert_eq!(ParallelImporter::batch_size_for(18_000), 1_000);
        assert_eq!(ParallelImporter::batch_size_for(100_000), 1_000);
    }

    #[test]
    fn test_generated_diff_skip_paths_include_terraform_website_assets() {
        assert!(is_generated_diff_skip_path(
            "website/source/stylesheets/main.css"
        ));
        assert!(is_generated_diff_skip_path(
            "website/source/images/logo-static.png"
        ));
        assert!(is_generated_diff_skip_path("package-lock.json"));
        assert!(is_generated_diff_skip_path("dist/app.min.js"));

        assert!(!is_generated_diff_skip_path(
            "website/source/stylesheets/_footer.less"
        ));
        assert!(!is_generated_diff_skip_path(
            "website/source/layouts/docs.erb"
        ));
        assert!(!is_generated_diff_skip_path("internal/style.css"));
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

    #[test]
    fn test_graph_first_binary_file_ops_create_trunk_only() {
        let mut next_branch_idx = 0;
        let ops = build_graph_first_file_ops_for_added_file(
            "website/source/images/logo-static.png",
            &[b"\x89PNG\r\n\x1a\n".to_vec()],
            &[(ChangePosition::new(0), ChangePosition::new(8))],
            Encoding::Binary,
            0,
            &mut next_branch_idx,
        );

        assert_eq!(ops.trunk_id().file_idx(), 0);
        assert!(ops.line_ops().is_empty());
        assert_eq!(next_branch_idx, 0);
    }

    // ═══════════════════════════════════════════════════════════════════
    // Classification tests
    // ═══════════════════════════════════════════════════════════════════

    fn test_info(message: &str, is_merge: bool) -> ImportedCommitInfo {
        ImportedCommitInfo {
            git_sha: "aabbccdd11223344".to_string(),
            short_sha: "aabbccdd".to_string(),
            atomic_hash: ContentHash::ZERO,
            is_merge,
            message: message.to_string(),
            squash_insert: None,
        }
    }

    #[test]
    fn test_classify_normal_commit() {
        let info = test_info("fix: typo in readme", false);
        assert!(matches!(
            classify_commit(&info),
            CommitClassification::Normal
        ));
    }

    #[test]
    fn test_classify_merge_commit() {
        let info = test_info("Merge branch 'feature' into main", true);
        assert!(matches!(
            classify_commit(&info),
            CommitClassification::Merge
        ));
    }

    #[test]
    fn test_classify_squash_with_atomic_trailer() {
        let msg = "feat: add login (#42)\n\nAtomic-Changes: ABC123, DEF456";
        let info = test_info(msg, false);
        match classify_commit(&info) {
            CommitClassification::Squash {
                original_hashes,
                pr_number,
            } => {
                assert_eq!(original_hashes, vec!["ABC123", "DEF456"]);
                assert_eq!(pr_number, Some(42));
            }
            other => panic!("expected Squash, got {:?}", other),
        }
    }

    #[test]
    fn test_classify_github_squash_format() {
        let msg = "Add feature (#99)\n\n* first commit\n* second commit";
        let info = test_info(msg, false);
        match classify_commit(&info) {
            CommitClassification::Squash {
                original_hashes,
                pr_number,
            } => {
                assert!(original_hashes.is_empty());
                assert_eq!(pr_number, Some(99));
            }
            other => panic!("expected Squash, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_pr_number_github_squash() {
        assert_eq!(parse_pr_number("feat: add login (#42)"), Some(42));
    }

    #[test]
    fn test_parse_pr_number_merge_pull_request() {
        assert_eq!(
            parse_pr_number("Merge pull request #123 from user/branch"),
            Some(123)
        );
    }

    #[test]
    fn test_parse_pr_number_none() {
        assert_eq!(parse_pr_number("fix: typo"), None);
    }

    #[test]
    fn test_parse_atomic_changes_trailer() {
        let msg = "msg\n\nAtomic-Changes: HASH1, HASH2, HASH3";
        let result = parse_atomic_changes_trailer(msg);
        assert_eq!(
            result,
            Some(vec![
                "HASH1".to_string(),
                "HASH2".to_string(),
                "HASH3".to_string()
            ])
        );
    }

    #[test]
    fn test_parse_atomic_changes_trailer_none() {
        assert_eq!(parse_atomic_changes_trailer("normal commit"), None);
    }

    #[test]
    fn test_parse_atomic_changes_trailer_collects_all_blocks() {
        // A GitHub squash-merge concatenates every squashed commit body, so the
        // message carries multiple `Atomic-Changes:` blocks. All must survive.
        let msg = "\
squash (#2)

* materialize
Atomic-View: a
Atomic-State: S1
Atomic-Changes: AAA, BBB

* materialize
Atomic-View: b
Atomic-State: S2
Atomic-Changes: CCC

Atomic-Changes: DDD, EEE
";
        let hashes = parse_atomic_changes_trailer(msg).expect("should parse");
        assert_eq!(hashes, vec!["AAA", "BBB", "CCC", "DDD", "EEE"]);
    }

    #[test]
    fn test_parse_atomic_changes_trailer_dedups_preserving_order() {
        // The tip work can repeat a hash already listed in an earlier block;
        // de-duplicate while keeping first-seen order.
        let msg = "\
squash (#3)

Atomic-Changes: AAA, BBB
Atomic-Changes: BBB, CCC, AAA
";
        let hashes = parse_atomic_changes_trailer(msg).expect("should parse");
        assert_eq!(hashes, vec!["AAA", "BBB", "CCC"]);
    }

    #[test]
    fn test_classify_squash_collects_all_trailer_blocks() {
        // End-to-end through classify_commit: the squash ReviewGate must record
        // every original hash, not just the first block's.
        let msg = "\
Materialize session view (#7)

* chore(atomic): materialize
Atomic-View: old-forest-591e
Atomic-State: S1
Atomic-Changes: FIRSTHASH

* chore(atomic): materialize
Atomic-View: calm-violet-8d7f
Atomic-State: S2
Atomic-Changes: SECONDHASH, THIRDHASH
";
        let info = test_info(msg, false);
        match classify_commit(&info) {
            CommitClassification::Squash {
                original_hashes,
                pr_number,
            } => {
                assert_eq!(
                    original_hashes,
                    vec!["FIRSTHASH", "SECONDHASH", "THIRDHASH"]
                );
                assert_eq!(pr_number, Some(7));
            }
            other => panic!("expected Squash, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_squash_format_github() {
        let msg = "Title (#10)\n\n* commit 1\n* commit 2";
        assert_eq!(parse_squash_merge_format(msg), Some(10));
    }

    #[test]
    fn test_parse_squash_format_github_no_bullets() {
        let msg = "Title (#10)\n\nJust a description";
        assert_eq!(parse_squash_merge_format(msg), None);
    }

    #[test]
    fn test_parse_squash_format_github_too_short() {
        assert_eq!(parse_squash_merge_format("Title (#10)"), None);
    }

    #[test]
    fn test_parse_pr_number_gitlab() {
        assert_eq!(
            parse_pr_number("See merge request mygroup/myproject!42"),
            Some(42)
        );
    }

    #[test]
    fn test_parse_pr_number_azure_devops() {
        assert_eq!(parse_pr_number("Merged PR 99: add feature"), Some(99));
    }

    #[test]
    fn test_parse_squash_format_gitlab() {
        let msg = "Add feature\n\nSee merge request mygroup/myproject!55";
        assert_eq!(parse_squash_merge_format(msg), Some(55));
    }

    #[test]
    fn test_parse_squash_format_azure_devops() {
        let msg = "Merged PR 77: add login\n\nDetails here";
        assert_eq!(parse_squash_merge_format(msg), Some(77));
    }

    /// Review CB-9C R4: rename similarity must survive supported file sizes.
    /// A shared span above ~429,496 bytes overflowed the former u32 multiply
    /// (panic in debug, silent wrap in release).
    #[test]
    fn similarity_bps_survives_the_corpus_scale_boundary() {
        let mut old_bytes = Vec::new();
        for _ in 0..450 {
            old_bytes.extend(std::iter::repeat_n(b'x', 1_000));
            old_bytes.push(b'\n');
        }
        assert_eq!(old_bytes.len(), 450_450, "the review fixture's exact size");

        // Renamed with a small appended edit: near-identical content.
        let mut new_bytes = old_bytes.clone();
        new_bytes.extend_from_slice(b"extra\n");
        let score = similarity_bps(&old_bytes, &new_bytes);
        assert!(
            (9_900..=9_999).contains(&score),
            "a 450450-byte rename with a small append scores near-full similarity: {score}"
        );

        // Byte-identical and empty/identical edges stay exact.
        assert_eq!(similarity_bps(&old_bytes, &old_bytes), 10_000);
        assert_eq!(similarity_bps(b"", b""), 10_000);
        // One-sided empty content shares nothing.
        assert_eq!(similarity_bps(b"", b"tail\n"), 0);
        assert_eq!(similarity_bps(b"tail\n", b""), 0);

        // A shared span just past the former u32 overflow boundary still
        // computes exactly in both profiles.
        let big = vec![b'x'; 429_497];
        let mut bigger = big.clone();
        bigger.push(b'y');
        let score = similarity_bps(&big, &bigger);
        assert_eq!(
            score, 9_999,
            "the exact former-overflow boundary stays capped"
        );
    }
}
