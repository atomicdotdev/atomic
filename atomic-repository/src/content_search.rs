//! Content search powered by syntext.
//!
//! Provides full-text search over source code in the repository working copy.
//! The index lives at `.atomic/content-index/` and is built/updated via
//! [`build_content_index`] or [`update_content_index`].

use std::path::Path;

use syntext::index::Index;
use syntext::{Config, IndexError, SearchOptions};

/// Build (or rebuild) the content search index for the repository.
///
/// Indexes all files in the working copy, respecting `.gitignore` and
/// `.atomicignore` patterns. The index is stored at `.atomic/content-index/`.
pub fn build_content_index(repo_root: &Path) -> Result<(), ContentSearchError> {
    let config = content_config(repo_root);
    let _index = Index::build(config)?;
    Ok(())
}

/// Incrementally update the content index after file changes.
///
/// If the repository HEAD has moved since the last full build, this performs
/// a stale-check rebuild. Otherwise the existing index is returned as-is.
/// For finer-grained incremental updates, use [`notify_and_commit`].
pub fn update_content_index(repo_root: &Path) -> Result<(), ContentSearchError> {
    let config = content_config(repo_root);
    let index = Index::open(config)?;
    // rebuild_if_stale returns Ok(Some(stats)) when a rebuild happened,
    // Ok(None) when the index was already current.
    let _stats = index.rebuild_if_stale()?;
    Ok(())
}

/// Notify the index about a single changed file and commit immediately.
///
/// This is the fastest path for keeping the index current after a single
/// file edit — cheaper than a full rebuild or stale check.
pub fn notify_and_commit(repo_root: &Path, changed_path: &Path) -> Result<(), ContentSearchError> {
    let config = content_config(repo_root);
    let index = Index::open(config)?;
    index.notify_change_immediate(changed_path)?;
    Ok(())
}

/// Search the content index.
///
/// Returns matching lines with file paths, line numbers, and content.
/// Supports path filtering, file type filtering, and case-insensitive search.
///
/// Results are ranked: source files (`src/`, `lib/`, `pkg/`, `internal/`)
/// are prioritized over config, test, build, and generated files. Within
/// each tier, results preserve filesystem order from the index.
///
/// The returned [`ContentSearchResult`] includes both the ranked matches
/// (capped to `max_results`) and the `total_matches` count so callers
/// know how many were found before truncation.
pub fn search_content(
    repo_root: &Path,
    pattern: &str,
    opts: ContentSearchOptions,
) -> Result<ContentSearchResult, ContentSearchError> {
    let config = content_config(repo_root);
    let index = Index::open(config)?;

    let display_limit = opts.max_results.unwrap_or(50);

    // Fetch a large candidate set so we can rank before truncating.
    // We ask for 10x the display limit (capped at 2000) to get good
    // coverage across the repo before applying our own ranking.
    let fetch_limit = (display_limit * 10).min(2000);

    let search_opts = SearchOptions {
        path_filter: opts.path_filter,
        file_type: opts.file_type,
        exclude_type: opts.exclude_type,
        max_results: Some(fetch_limit),
        case_insensitive: opts.case_insensitive,
    };

    let raw_matches = index.search(pattern, &search_opts)?;
    let total_matches = raw_matches.len();

    // Build per-directory match counts for the facets summary.
    let mut dir_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for m in &raw_matches {
        let path_str = m.path.to_string_lossy();
        // Use the first two path components as the directory bucket.
        let dir = path_str
            .splitn(3, '/')
            .take(2)
            .collect::<Vec<_>>()
            .join("/");
        *dir_counts.entry(dir).or_insert(0) += 1;
    }

    // Sort directory facets by count (descending).
    let mut dir_facets: Vec<(String, usize)> = dir_counts.into_iter().collect();
    dir_facets.sort_by(|a, b| b.1.cmp(&a.1));
    dir_facets.truncate(10); // top 10 directories

    // Rank matches: source code files first, then everything else.
    let mut ranked: Vec<(usize, &syntext::SearchMatch)> = raw_matches
        .iter()
        .map(|m| (path_tier(m.path.to_string_lossy().as_ref()), m))
        .collect();
    ranked.sort_by_key(|(tier, _)| *tier);

    let matches: Vec<ContentMatch> = ranked
        .into_iter()
        .take(display_limit)
        .map(|(_, m)| ContentMatch {
            path: m.path.to_string_lossy().to_string(),
            line_number: m.line_number,
            line_content: String::from_utf8_lossy(&m.line_content).to_string(),
            byte_offset: m.byte_offset,
            submatch_start: m.submatch_start,
            submatch_end: m.submatch_end,
        })
        .collect();

    Ok(ContentSearchResult {
        matches,
        total_matches,
        dir_facets,
    })
}

/// Assign a ranking tier to a file path.
///
/// Lower tier = higher priority. Source implementation files rank above
/// tests, config, build scripts, docs, and generated files.
fn path_tier(path: &str) -> usize {
    // Tier 0: primary source directories
    let source_prefixes = ["src/", "lib/", "pkg/", "internal/", "cmd/", "app/"];
    for prefix in &source_prefixes {
        if path.starts_with(prefix) {
            // Demote test files even within src/
            if is_test_path(path) {
                return 2;
            }
            return 0;
        }
    }

    // Tier 1: other code files (root-level .rs/.py/.cpp etc.)
    let code_extensions = [
        ".rs", ".go", ".py", ".ts", ".js", ".cpp", ".cc", ".c", ".h", ".hpp", ".java", ".kt",
        ".swift", ".rb", ".cs",
    ];
    if code_extensions.iter().any(|ext| path.ends_with(ext)) {
        if is_test_path(path) {
            return 2;
        }
        return 1;
    }

    // Tier 2: test files
    if is_test_path(path) {
        return 2;
    }

    // Tier 3: docs and markdown
    if path.ends_with(".md") || path.starts_with("docs/") || path.starts_with("doc/") {
        return 3;
    }

    // Tier 4: build scripts, config, CI, generated files
    let low_priority = [
        "buildscripts/",
        "build/",
        ".github/",
        "ci/",
        "scripts/",
        "debian/",
        "rpm/",
        "packaging/",
        "vendor/",
        "third_party/",
        "node_modules/",
        "target/",
    ];
    if low_priority.iter().any(|p| path.starts_with(p)) {
        return 4;
    }
    if path.ends_with(".yml")
        || path.ends_with(".yaml")
        || path.ends_with(".toml")
        || path.ends_with(".json")
        || path.ends_with(".xml")
        || path.ends_with(".cfg")
    {
        return 4;
    }

    // Tier 3: everything else
    3
}

/// Heuristic: is this path a test file?
fn is_test_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.contains("/test/")
        || lower.contains("/tests/")
        || lower.contains("_test.")
        || lower.contains("_test_")
        || lower.contains(".test.")
        || lower.starts_with("test/")
        || lower.starts_with("tests/")
        || lower.starts_with("jstests/")
        || lower.starts_with("testdata/")
}

/// Check whether the content index exists and is usable.
pub fn has_content_index(repo_root: &Path) -> bool {
    let index_dir = repo_root.join(".atomic").join("content-index");
    // The manifest file is the authoritative marker for a built index.
    index_dir.join("manifest.json").exists()
}

/// Return statistics about the content index.
///
/// Returns `None` if the index does not exist or cannot be opened.
pub fn content_index_stats(repo_root: &Path) -> Option<ContentIndexStats> {
    let config = content_config(repo_root);
    let index = Index::open(config).ok()?;
    let stats = index.stats();
    Some(ContentIndexStats {
        total_documents: stats.total_documents,
        total_segments: stats.total_segments,
        total_grams: stats.total_grams,
        index_size_bytes: stats.index_size_bytes,
        overlay_generations: stats.overlay_generations,
        pending_edits: stats.pending_edits,
    })
}

fn content_config(repo_root: &Path) -> Config {
    Config {
        index_dir: repo_root.join(".atomic").join("content-index"),
        repo_root: repo_root.to_path_buf(),
        max_file_size: 10 * 1024 * 1024, // 10 MiB
        verbose: false,
        strict_permissions: false, // .atomic/ may have relaxed perms
        ..Config::default()
    }
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Options for content search.
#[derive(Debug, Clone, Default)]
pub struct ContentSearchOptions {
    /// Glob pattern to restrict search to matching paths (e.g., `"src/repl/"`).
    pub path_filter: Option<String>,
    /// File type filter (e.g., `"rs"`, `"cpp"`, `"py"`).
    pub file_type: Option<String>,
    /// Exclude files of this type.
    pub exclude_type: Option<String>,
    /// Maximum number of results to display (default: 50).
    /// Internally, more results are fetched for ranking.
    pub max_results: Option<usize>,
    /// Case-insensitive matching.
    pub case_insensitive: bool,
}

/// Result of a content search — ranked matches plus metadata.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ContentSearchResult {
    /// Ranked matches (capped to the requested limit).
    pub matches: Vec<ContentMatch>,
    /// Total matches found before truncation.
    /// When this is larger than `matches.len()`, the user should narrow
    /// their query with path or type filters.
    pub total_matches: usize,
    /// Top directories by match count: `(dir_path, count)`.
    /// Helps the user (or LLM) decide how to narrow with `-g`.
    pub dir_facets: Vec<(String, usize)>,
}

/// A content search match.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ContentMatch {
    /// Repository-relative file path.
    pub path: String,
    /// 1-based line number.
    pub line_number: u32,
    /// Content of the matching line.
    pub line_content: String,
    /// Byte offset of the match start in the file.
    pub byte_offset: u64,
    /// Byte offset of the first match within `line_content`.
    pub submatch_start: usize,
    /// Exclusive end byte offset of the first match within `line_content`.
    pub submatch_end: usize,
}

/// Summary statistics about the content index.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ContentIndexStats {
    /// Number of indexed files across all base segments.
    pub total_documents: usize,
    /// Number of on-disk segment files.
    pub total_segments: usize,
    /// Total distinct n-grams across all segments.
    pub total_grams: usize,
    /// Combined on-disk size of all segment files plus the manifest (bytes).
    pub index_size_bytes: u64,
    /// Number of overlay generations since the last full rebuild.
    pub overlay_generations: usize,
    /// Number of dirty file edits buffered in the current overlay.
    pub pending_edits: usize,
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors from content search operations.
#[derive(Debug, thiserror::Error)]
pub enum ContentSearchError {
    #[error("content index not found — run `atomic vault query enrich` to build it")]
    IndexNotFound,

    #[error("index is corrupt and needs rebuilding: {0}")]
    CorruptIndex(String),

    #[error("invalid search pattern: {0}")]
    InvalidPattern(String),

    #[error("index locked by another process")]
    LockConflict,

    #[error("index error: {0}")]
    Index(String),
}

impl From<IndexError> for ContentSearchError {
    fn from(err: IndexError) -> Self {
        match err {
            IndexError::Io(ref e) if e.kind() == std::io::ErrorKind::NotFound => {
                ContentSearchError::IndexNotFound
            }
            IndexError::InvalidPattern(p) => ContentSearchError::InvalidPattern(p),
            IndexError::CorruptIndex(msg) => ContentSearchError::CorruptIndex(msg),
            IndexError::LockConflict(_) => ContentSearchError::LockConflict,
            other => ContentSearchError::Index(other.to_string()),
        }
    }
}
