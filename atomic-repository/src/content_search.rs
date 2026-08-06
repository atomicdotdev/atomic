//! Content search powered by syntext.
//!
//! Provides full-text search over source code in the repository working copy.
//! The index lives at `.atomic/content-index/` and is built/updated via
//! [`build_content_index`] or [`update_content_index`].

use std::path::Path;

use syntext::index::{ExternalFileRecord, Index};
use syntext::{Config, IndexError, SearchOptions};

/// Build (or rebuild) the content search index for the repository.
///
/// Indexes files in the working copy while respecting `.gitignore`, `.ignore`,
/// `.atomicignore` (and the global ignore file), and skipping Atomic-internal
/// directories (`.atomic`, `.git`, `.vault`). The index is stored at
/// `.atomic/content-index/`.
///
/// Discovery is owned here rather than delegated to syntext's `Index::build`:
/// syntext's walker respects `.gitignore`/`.ignore` but NOT `.atomicignore`,
/// and it descends into hidden dirs like `.atomic`/`.vault`, so it would index
/// build artifacts, dependencies, and Atomic internals. We reuse syntext's
/// walker (for its symlink resolution and size handling), then drop the paths
/// Atomic excludes before handing the corpus to `build_from_file_records`.
pub fn build_content_index(repo_root: &Path) -> Result<(), ContentSearchError> {
    let config = content_config(repo_root);

    let (files, _skips) = syntext::index::walk::enumerate_files(&config)?;
    let ignore_rules = crate::ignore::IgnoreRules::load_for_enrichment(repo_root);
    let records: Vec<ExternalFileRecord> = files
        .into_iter()
        .filter(|(_absolute, relative, _size)| {
            !crate::ignore::is_enrichment_internal(relative)
                && !ignore_rules.is_ignored(relative, false)
        })
        .map(
            |(absolute_path, relative_path, size_bytes)| ExternalFileRecord {
                absolute_path,
                relative_path,
                size_bytes,
            },
        )
        .collect();

    let _index = Index::build_from_file_records(config, records)?;
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

/// Incrementally refresh the content index for a set of repo-relative paths
/// that changed (e.g. the files touched by a recorded change) and persist the
/// result to disk.
///
/// Each path is re-read from disk: added/modified files are re-indexed and
/// deleted files are removed, keeping the index in sync with the working copy
/// without re-walking the whole tree. Ignored and Atomic-internal paths are
/// skipped so build artifacts, dependencies, and internals never enter the
/// index.
///
/// syntext's per-file edits land in an in-memory overlay that is not visible to
/// a later `Index::open` (e.g. the next `search` in a fresh process). To make
/// the update durable we `compact()` afterward, folding the overlay into fresh
/// on-disk base segments — cheaper than [`build_content_index`] because
/// unchanged files are reused from existing segments rather than re-read.
///
/// No-op when the content index does not yet exist — building it is the job of
/// [`build_content_index`]; this only maintains an existing index.
pub fn update_content_index_paths<I, P>(
    repo_root: &Path,
    paths: I,
) -> Result<(), ContentSearchError>
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    if !has_content_index(repo_root) {
        return Ok(());
    }

    let ignore_rules = crate::ignore::IgnoreRules::load_for_enrichment(repo_root);
    let config = content_config(repo_root);
    let index = Index::open(config)?;

    let mut any = false;
    for path in paths {
        let rel = path.as_ref();
        if crate::ignore::is_enrichment_internal(rel) || ignore_rules.is_ignored(rel, false) {
            continue;
        }
        // syntext strips `repo_root` to derive the relative path, so hand it an
        // absolute path. The file need not exist — a missing file is treated as
        // a deletion and removed from the index.
        let absolute = repo_root.join(rel);
        index.notify_change(&absolute)?;
        any = true;
    }

    if !any {
        return Ok(());
    }

    // Commit the pending overlay and fold it into on-disk base segments so the
    // change survives to the next `Index::open`. When the changed set is large
    // relative to the index (syntext caps the overlay at 50% of base docs),
    // `compact` reports `OverlayFull`; the sanctioned recovery is a full
    // (filtered) rebuild.
    match index.compact() {
        Ok(()) => Ok(()),
        Err(IndexError::OverlayFull { .. }) => {
            drop(index);
            build_content_index(repo_root)
        }
        Err(e) => Err(e.into()),
    }
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

    // Drop matches in ignored or Atomic-internal paths. syntext's index walk
    // respects `.gitignore`/`.ignore` but NOT `.atomicignore`, and it descends
    // into hidden dirs like `.atomic`/`.vault`, so build artifacts and
    // dependencies excluded only by `.atomicignore` (plus Atomic internals)
    // would otherwise surface here and, via the KG search's content-only file
    // promotion, as `file:` results. Filter at this shared consumption point
    // so both `query search` and `query code` stay clean regardless of what
    // the index contains.
    let ignore_rules = crate::ignore::IgnoreRules::load_for_enrichment(repo_root);
    let raw_matches: Vec<syntext::SearchMatch> = raw_matches
        .into_iter()
        .filter(|m| {
            !crate::ignore::is_enrichment_internal(&m.path)
                && !ignore_rules.is_ignored(&m.path, false)
        })
        .collect();
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
    dir_facets.sort_by_key(|entry| std::cmp::Reverse(entry.1));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_content_excludes_ignored_and_internal_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // A distinctive token present in a source file plus in files that
        // must be excluded: a `.atomicignore`'d build dir, a `.atomicignore`'d
        // dependency dir (with many files), and `.atomic`/`.vault` internals.
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("node_modules/dep")).unwrap();
        std::fs::create_dir_all(root.join("dist")).unwrap();
        std::fs::create_dir_all(root.join(".vault")).unwrap();
        std::fs::write(root.join("src/index.ts"), b"const marker = wombatxyz;\n").unwrap();
        for i in 0..20 {
            std::fs::write(
                root.join(format!("node_modules/dep/lib{i}.js")),
                b"var wombatxyz = 1;\n",
            )
            .unwrap();
        }
        std::fs::write(root.join("dist/index.js"), b"var wombatxyz = 1;\n").unwrap();
        std::fs::write(root.join(".vault/note.md"), b"wombatxyz in vault\n").unwrap();
        std::fs::write(root.join(".atomicignore"), b"node_modules/\ndist/\n").unwrap();

        build_content_index(root).unwrap();

        // The index itself must exclude the ignored/internal files: with 20
        // node_modules files plus dist/.vault, an unfiltered build would index
        // 20+ docs. A filtered build indexes only the handful of real sources.
        let stats = content_index_stats(root).expect("index stats");
        assert!(
            stats.total_documents < 10,
            "index should exclude ignored/internal files, but indexed {} documents",
            stats.total_documents
        );

        let result = search_content(root, "wombatxyz", ContentSearchOptions::default()).unwrap();
        let paths: Vec<&str> = result.matches.iter().map(|m| m.path.as_str()).collect();

        assert!(
            paths.contains(&"src/index.ts"),
            "source file should match, got: {paths:?}"
        );
        assert!(
            !paths.iter().any(|p| p.starts_with("node_modules/")),
            ".atomicignore'd dependency must be excluded, got: {paths:?}"
        );
        assert!(
            !paths.iter().any(|p| p.starts_with("dist/")),
            ".atomicignore'd build artifact must be excluded, got: {paths:?}"
        );
        assert!(
            !paths
                .iter()
                .any(|p| p.starts_with(".vault/") || p.starts_with(".atomic/")),
            "Atomic-internal paths must be excluded, got: {paths:?}"
        );
    }

    #[test]
    fn update_content_index_paths_persists_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.ts"), b"const alpha = 1;\n").unwrap();

        build_content_index(root).unwrap();

        // Add a new file on disk and incrementally update the index.
        std::fs::write(root.join("src/b.ts"), b"const betamarker = 2;\n").unwrap();
        update_content_index_paths(root, ["src/b.ts"]).unwrap();

        // A fresh search (new Index::open) must see the persisted addition.
        let result = search_content(root, "betamarker", ContentSearchOptions::default()).unwrap();
        let paths: Vec<&str> = result.matches.iter().map(|m| m.path.as_str()).collect();
        assert!(
            paths.contains(&"src/b.ts"),
            "incrementally indexed file must be searchable after reopen, got: {paths:?}"
        );
    }
}
