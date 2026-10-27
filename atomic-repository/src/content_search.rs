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
///
/// Transient lock conflicts (e.g. a parallel test or a just-dropped `Index`
/// handle still releasing its flock) are retried with backoff; only a
/// persistent conflict surfaces as an error.
pub fn build_content_index(repo_root: &Path) -> Result<(), ContentSearchError> {
    with_lock_retry(|| {
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
    })
}

/// Refresh the content index for an import, skipping the full rebuild when
/// the existing index is already current (RFC §21 measured budgets, CB-13C
/// AC-3).
///
/// `rebuild_if_stale` is HEAD-based: a full rebuild stamps the current Git
/// HEAD as the index's `base_commit`, so an import that does not move Git
/// leaves the index current and the next import's refresh is a no-op. Only
/// a missing or unusable index falls back to the full walk-and-rebuild —
/// which is the cold-import behavior this preserves.
pub fn refresh_content_index(repo_root: &Path) -> Result<(), ContentSearchError> {
    match update_content_index(repo_root) {
        Ok(()) => Ok(()),
        // Missing or unusable index: fall back to the full walk-and-rebuild
        // (the cold-import behavior this preserves).
        Err(_) => build_content_index(repo_root),
    }
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

    // The open + compact below race syntext's own lock-downgrade windows and
    // any concurrent reader/writer on the same index dir, so retry the whole
    // update on a lock conflict rather than treating it as a hard failure.
    // Collect the paths once: the retry closure may run multiple times.
    let paths: Vec<std::path::PathBuf> = paths
        .into_iter()
        .map(|p| p.as_ref().to_path_buf())
        .collect();
    with_lock_retry(|| {
        let ignore_rules = crate::ignore::IgnoreRules::load_for_enrichment(repo_root);
        let config = content_config(repo_root);
        let index = Index::open(config)?;

        let mut any = false;
        for path in &paths {
            let rel = path.as_path();
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
    })
}

/// Search the content index.
///
/// Returns matching lines with file paths, line numbers, and content.
/// Supports path filtering, file type filtering, and case-insensitive search.
///
/// Results are ranked: source files (`src/`, `lib/`, `pkg/`, `internal/`)
/// are prioritized over config, test, build, and generated files. Within
/// each tier, results follow path order and preserve the verifier's within-file order.
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
    Ok(rank_content_matches(raw_matches, display_limit))
}

/// Recorded-query byte and regex-automaton cutoff (10 MiB).
pub(crate) const RECORDED_CONTENT_LIMIT: usize = 10 * 1024 * 1024;

/// An immutable path/filter plan and bounded collector for recorded content.
/// Paths are scanned in result order (tier, then canonical path), so retaining
/// the first N records is equivalent to the exhaustive stable tier sort.
pub(crate) struct RecordedContentSearch {
    re: regex::bytes::Regex,
    paths: Vec<String>,
    display_limit: usize,
}

impl RecordedContentSearch {
    pub(crate) fn new<'path>(
        paths: impl IntoIterator<Item = &'path str>,
        pattern: &str,
        opts: ContentSearchOptions,
    ) -> Result<Self, ContentSearchError> {
        // Compile even for empty scopes and zero display limits.
        let re = regex::bytes::RegexBuilder::new(pattern)
            .case_insensitive(opts.case_insensitive)
            .size_limit(RECORDED_CONTENT_LIMIT)
            .dfa_size_limit(RECORDED_CONTENT_LIMIT)
            .build()
            .map_err(|error| ContentSearchError::InvalidPattern(error.to_string()))?;
        let mut paths: Vec<_> = paths.into_iter().map(std::path::PathBuf::from).collect();
        paths.sort();
        let index = syntext::path::PathIndex::build(&paths);
        let filter = syntext::path::filter::build_filter(
            &index,
            opts.file_type.as_deref(),
            opts.exclude_type.as_deref(),
            opts.path_filter.as_deref(),
        );
        let mut paths: Vec<_> = paths
            .into_iter()
            .enumerate()
            .filter(|(id, _)| {
                filter
                    .as_ref()
                    .is_none_or(|filter| filter.file_ids.contains(*id as u32))
            })
            .map(|(_, path)| path.to_string_lossy().into_owned())
            .collect();
        paths.sort_by(|a, b| recorded_path_order(a, b));
        Ok(Self {
            re,
            paths,
            display_limit: opts.max_results.unwrap_or(50),
        })
    }

    pub(crate) fn run<E>(
        self,
        mut retrieve: impl FnMut(&str) -> Result<Option<Vec<u8>>, E>,
    ) -> Result<ContentSearchResult, E> {
        let mut matches = Vec::new();
        let mut total_matches = 0;
        let mut directories = std::collections::BTreeMap::<String, usize>::new();
        for path in self.paths {
            let Some(content) = retrieve(&path)? else {
                continue;
            };
            if content.len() > RECORDED_CONTENT_LIMIT || syntext::index::is_binary(&content) {
                continue;
            }
            let mut line_start = 0;
            let mut file_count = 0;
            // This borrowed-line adapter implements syntext 1.4's verifier
            // contract: first match per line, CR stripped only before LF, no
            // artificial trailing empty line, and file-level binary detection.
            // Differential tests below keep it pinned to verify_regex, including
            // NULs beyond the first 8192 bytes. No dependency fork is required.
            for (index, raw_line) in content.split_inclusive(|byte| *byte == b'\n').enumerate() {
                let line = raw_line
                    .strip_suffix(b"\n")
                    .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
                    .unwrap_or(raw_line);
                if let Some(found) = self.re.find(line) {
                    file_count += 1;
                    if matches.len() < self.display_limit {
                        matches.push(ContentMatch {
                            path: path.clone(),
                            line_number: (index + 1) as u32,
                            line_content: String::from_utf8_lossy(line).into_owned(),
                            byte_offset: (line_start + found.start()) as u64,
                            submatch_start: found.start(),
                            submatch_end: found.end(),
                        });
                        #[cfg(any(test, feature = "query-test-instrumentation"))]
                        crate::query_diagnostics::retained(matches.len());
                    }
                }
                line_start += raw_line.len();
            }
            if file_count > 0 {
                total_matches += file_count;
                *directories.entry(directory_bucket(&path)).or_default() += file_count;
            }
        }
        Ok(ContentSearchResult {
            matches,
            total_matches,
            dir_facets: directory_facets(directories),
        })
    }
}

fn recorded_path_order(a: &str, b: &str) -> std::cmp::Ordering {
    path_tier(a).cmp(&path_tier(b)).then_with(|| a.cmp(b))
}

fn directory_bucket(path: &str) -> String {
    path.splitn(3, '/').take(2).collect::<Vec<_>>().join("/")
}

fn directory_facets(counts: impl IntoIterator<Item = (String, usize)>) -> Vec<(String, usize)> {
    let mut facets: Vec<_> = counts.into_iter().collect();
    facets.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    facets.truncate(10);
    facets
}

/// Search graph-rendered bytes without consulting the working-copy index.
/// Path/type matching and line verification use the same syntext machinery as
/// indexed search; filtering and ranking happen before the display limit.
#[cfg(test)]
pub(crate) fn search_file_contents(
    files: &std::collections::BTreeMap<String, Vec<u8>>,
    pattern: &str,
    opts: ContentSearchOptions,
) -> Result<ContentSearchResult, ContentSearchError> {
    let re = regex::bytes::RegexBuilder::new(pattern)
        .case_insensitive(opts.case_insensitive)
        .size_limit(10 * 1024 * 1024)
        .dfa_size_limit(10 * 1024 * 1024)
        .build()
        .map_err(|error| ContentSearchError::InvalidPattern(error.to_string()))?;
    let paths: Vec<_> = files.keys().map(std::path::PathBuf::from).collect();
    let path_index = syntext::path::PathIndex::build(&paths);
    let filter = syntext::path::filter::build_filter(
        &path_index,
        opts.file_type.as_deref(),
        opts.exclude_type.as_deref(),
        opts.path_filter.as_deref(),
    );
    let mut matches = Vec::new();
    for (id, (path, content)) in files.iter().enumerate() {
        if content.len() > 10 * 1024 * 1024
            || filter
                .as_ref()
                .is_some_and(|filter| !filter.file_ids.contains(id as u32))
        {
            continue;
        }
        matches.extend(syntext::search::verifier::verify_regex(
            &re,
            Path::new(path),
            content,
        ));
    }
    Ok(rank_content_matches(
        matches,
        opts.max_results.unwrap_or(50),
    ))
}

fn rank_content_matches(
    raw_matches: Vec<syntext::SearchMatch>,
    display_limit: usize,
) -> ContentSearchResult {
    let total_matches = raw_matches.len();
    #[cfg(any(test, feature = "query-test-instrumentation"))]
    crate::query_diagnostics::retained(total_matches);

    // Build per-directory match counts for the facets summary.
    let mut dir_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for m in &raw_matches {
        let dir = directory_bucket(&m.path.to_string_lossy());
        *dir_counts.entry(dir).or_insert(0) += 1;
    }

    let dir_facets = directory_facets(dir_counts);

    // Rank matches: source code files first, then everything else.
    let mut ranked: Vec<_> = raw_matches.iter().collect();
    // syntext's indexed results and the recorded oracle already arrive in path
    // order. Use the same stable comparator as the bounded path plan; records
    // within each file retain the verifier's original order.
    ranked
        .sort_by(|a, b| recorded_path_order(&a.path.to_string_lossy(), &b.path.to_string_lossy()));

    let matches: Vec<ContentMatch> = ranked
        .into_iter()
        .take(display_limit)
        .map(|m| ContentMatch {
            path: m.path.to_string_lossy().to_string(),
            line_number: m.line_number,
            line_content: String::from_utf8_lossy(&m.line_content).to_string(),
            byte_offset: m.byte_offset,
            submatch_start: m.submatch_start,
            submatch_end: m.submatch_end,
        })
        .collect();

    ContentSearchResult {
        matches,
        total_matches,
        dir_facets,
    }
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
    /// Recorded search counts all eligible matches while retaining at most this
    /// many records. Working-copy indexed search retains its candidate overfetch.
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
// Lock-conflict retry
// ---------------------------------------------------------------------------

/// How many times to retry an operation that failed with a lock conflict,
/// and the base delay between attempts (doubled each time).
const LOCK_RETRY_ATTEMPTS: u32 = 8;
const LOCK_RETRY_BASE_DELAY_MS: u64 = 25;

/// Run `op`, retrying with exponential backoff when it fails with a lock
/// conflict.
///
/// syntext documents lock downgrades in `build`/`compact` as brief windows
/// where a concurrent `open` can surface `LockConflict`, with the caller
/// expected to retry. Locks also live as long as the `Index` handle, so an
/// immediately preceding call in this process (e.g. `build` right before an
/// `update`) may still be holding one while its handle drops or a parallel
/// test's handles overlap. A transient conflict must not fail the operation.
fn with_lock_retry<T>(
    op: impl Fn() -> Result<T, ContentSearchError>,
) -> Result<T, ContentSearchError> {
    let mut delay_ms = LOCK_RETRY_BASE_DELAY_MS;
    for attempt in 0..=LOCK_RETRY_ATTEMPTS {
        match op() {
            Err(ContentSearchError::LockConflict) if attempt < LOCK_RETRY_ATTEMPTS => {
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                delay_ms *= 2;
            }
            other => return other,
        }
    }
    unreachable!("loop returns on the final attempt")
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
            IndexError::IndexNotFound(_) => ContentSearchError::IndexNotFound,
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

    fn streaming(
        files: &std::collections::BTreeMap<String, Vec<u8>>,
        pattern: &str,
        opts: ContentSearchOptions,
    ) -> ContentSearchResult {
        RecordedContentSearch::new(files.keys().map(String::as_str), pattern, opts)
            .unwrap()
            .run::<std::convert::Infallible>(|path| Ok(Some(files[path].clone())))
            .unwrap()
    }

    #[test]
    fn recorded_stream_matches_exhaustive_verifier_boundaries_and_filters() {
        let files = std::collections::BTreeMap::from([
            (
                "src/z.rs".into(),
                b"one needle needle\r\n\r\nNEEDLE\nlast\r".to_vec(),
            ),
            ("src/a.rs".into(), b"\n\nneedle\n".to_vec()),
            ("tests/a.rs".into(), b"needle\n".to_vec()),
            ("src/empty.rs".into(), Vec::new()),
            ("src/binary.rs".into(), b"needle\0\n".to_vec()),
            ("src/invalid.rs".into(), b"before\xffneedle\n".to_vec()),
            (
                "src/unicode.rs".into(),
                "é needle🦀\r\n終わり needle".as_bytes().to_vec(),
            ),
            ("notes.md".into(), b"needle\n".to_vec()),
            (
                "src/late-nul.rs".into(),
                [vec![b'x'; 8200], b"\nneedle\0\n".to_vec()].concat(),
            ),
        ]);
        for pattern in [
            "needle",
            "",
            "^",
            "$",
            "^$",
            "needle.*",
            r"(?-u)\xFF|\x00",
            "needle\\n",
        ] {
            for max_results in [None, Some(0), Some(1), Some(5), Some(100)] {
                for (path_filter, file_type, exclude_type) in [
                    (None, None, None),
                    (Some("src/"), Some("rs"), None),
                    (Some("**/*.rs"), None, Some("rs")),
                    (Some("notes.md"), None, None),
                    (Some("missing/**"), Some("unknown_type"), None),
                    (None, Some("unknown_type"), None),
                    (Some("."), None, None),
                    (Some("/src/"), None, None),
                    (Some("../src/"), None, None),
                ] {
                    let opts = ContentSearchOptions {
                        max_results,
                        case_insensitive: true,
                        path_filter: path_filter.map(str::to_owned),
                        file_type: file_type.map(str::to_owned),
                        exclude_type: exclude_type.map(str::to_owned),
                    };
                    let expected = search_file_contents(&files, pattern, opts.clone()).unwrap();
                    let actual = streaming(&files, pattern, opts);
                    assert_eq!(
                        serde_json::to_value(actual).unwrap(),
                        serde_json::to_value(expected).unwrap(),
                        "pattern={pattern:?} limit={max_results:?} filter={path_filter:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn randomized_recorded_stream_matches_exhaustive_order_counts_and_offsets() {
        use rand::{Rng, SeedableRng};
        let mut rng = rand::rngs::StdRng::seed_from_u64(11);
        for case in 0..80 {
            let mut files = std::collections::BTreeMap::new();
            for path in ["docs/a.md", "src/z.rs", "src/a.rs", "tests/b.rs", "root.py"] {
                let mut bytes: Vec<u8> = (0..rng.gen_range(0..500))
                    .map(|_| {
                        let alphabet = b"aaaazz\r\n\xff ";
                        alphabet[rng.gen_range(0..alphabet.len())]
                    })
                    .collect();
                if case % 7 == 0 && !bytes.is_empty() {
                    bytes[0] = 0;
                }
                files.insert(path.into(), bytes);
            }
            for pattern in ["a+", "z|a", "^$", "", r"(?-u)\xFF"] {
                for max_results in [Some(0), Some(1), Some(10), None] {
                    let opts = ContentSearchOptions {
                        max_results,
                        ..Default::default()
                    };
                    assert_eq!(
                        serde_json::to_value(streaming(&files, pattern, opts.clone())).unwrap(),
                        serde_json::to_value(search_file_contents(&files, pattern, opts).unwrap())
                            .unwrap(),
                        "case={case} pattern={pattern}"
                    );
                }
            }
        }
    }

    #[test]
    fn recorded_stream_counts_dense_matches_without_retaining_them() {
        let files = std::collections::BTreeMap::from([(
            "src/dense.rs".into(),
            b"needle\n".repeat(100_000),
        )]);
        for limit in [0, 1, 10] {
            let (result, resources) = crate::query_diagnostics::measure(|| {
                streaming(
                    &files,
                    "needle",
                    ContentSearchOptions {
                        max_results: Some(limit),
                        ..Default::default()
                    },
                )
            });
            assert_eq!(result.total_matches, 100_000);
            assert_eq!(result.matches.len(), limit);
            assert_eq!(result.dir_facets, [("src/dense.rs".into(), 100_000)]);
            assert_eq!(resources.displayed_records_high_water, limit);
        }
        for paths in [vec![], vec!["missing.rs"]] {
            assert!(RecordedContentSearch::new(
                paths,
                "[",
                ContentSearchOptions {
                    max_results: Some(0),
                    ..Default::default()
                }
            )
            .is_err());
        }
        let tied: std::collections::BTreeMap<_, _> = (0..12)
            .rev()
            .map(|i| (format!("dir{i:02}/a.rs"), b"needle\n".to_vec()))
            .collect();
        let result = streaming(&tied, "needle", ContentSearchOptions::default());
        assert_eq!(
            result.dir_facets,
            (0..10)
                .map(|i| (format!("dir{i:02}/a.rs"), 1))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn recorded_stream_applies_the_exact_content_cutoff() {
        let files = std::collections::BTreeMap::from([
            ("src/exact.rs".into(), vec![b'x'; RECORDED_CONTENT_LIMIT]),
            (
                "src/oversized.rs".into(),
                vec![b'x'; RECORDED_CONTENT_LIMIT + 1],
            ),
        ]);
        let result = streaming(&files, "^x", ContentSearchOptions::default());
        assert_eq!(result.total_matches, 1);
        assert_eq!(result.matches[0].path, "src/exact.rs");
    }

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
