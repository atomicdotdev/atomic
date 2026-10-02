//! Machine-readable JSON output for `atomic diff --json`.
//!
//! The document is a versioned projection of the same [`FileDiff`] data the
//! text renderers consume, so a consumer can reconstruct a unified diff or
//! compute its own rollups without re-parsing formatted output.

use super::*;
use serde::Serialize;

/// Version of the `atomic diff --json` document.
const DIFF_JSON_SCHEMA_VERSION: u32 = 1;

/// The root document.
#[derive(Debug, Serialize)]
pub(super) struct JsonDiff {
    schema_version: u32,

    /// The view the diff was taken against, when the diff is working-copy
    /// scoped. `None` for a `-c` change diff, which is not view-scoped.
    view: Option<String>,

    /// The change record this diff describes, when `-c` was given.
    change: Option<JsonDiffChange>,

    files: Vec<JsonDiffFile>,
    stats: JsonDiffStats,
}

impl JsonDiff {
    /// Build the document.
    ///
    /// `change` is `Some` for `atomic diff -c <hash>` and `None` for a
    /// working-copy diff; `view` is populated only for the latter.
    pub(super) fn new(
        file_diffs: &[FileDiff],
        stats: &DiffStats,
        change: Option<(&Change, &Hash)>,
        view: Option<&str>,
    ) -> Self {
        Self {
            schema_version: DIFF_JSON_SCHEMA_VERSION,
            view: view.map(str::to_string),
            change: change.map(|(c, h)| JsonDiffChange::new(c, h)),
            files: file_diffs.iter().map(JsonDiffFile::from).collect(),
            stats: JsonDiffStats::from(stats),
        }
    }

    /// The all-zero document, used when there is nothing to diff so that
    /// `--json` still emits valid parseable JSON.
    pub(super) fn empty(view: Option<&str>) -> Self {
        Self {
            schema_version: DIFF_JSON_SCHEMA_VERSION,
            view: view.map(str::to_string),
            change: None,
            files: Vec::new(),
            stats: JsonDiffStats::default(),
        }
    }
}

/// Identity and message of the change record under inspection.
#[derive(Debug, Serialize)]
struct JsonDiffChange {
    /// Full base32 change hash (not truncated — this is a machine surface).
    hash: String,

    /// Short hash as displayed by the text renderers.
    short_hash: String,

    message: String,
    description: Option<String>,
    authors: Vec<JsonDiffAuthor>,
    date: String,
}

impl JsonDiffChange {
    fn new(change: &Change, hash: &Hash) -> Self {
        let header = &change.hashed.header;
        let base32 = hash.to_base32();
        Self {
            short_hash: base32[..DEFAULT_HASH_LENGTH.min(base32.len())].to_string(),
            hash: base32,
            message: header.message.clone(),
            description: header.description.clone(),
            authors: header.authors.iter().map(JsonDiffAuthor::from).collect(),
            date: header.timestamp.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        }
    }
}

#[derive(Debug, Serialize)]
struct JsonDiffAuthor {
    name: String,
    email: Option<String>,
}

impl From<&atomic_core::change::Author> for JsonDiffAuthor {
    fn from(author: &atomic_core::change::Author) -> Self {
        Self {
            name: author.name.clone(),
            email: author.email.clone(),
        }
    }
}

/// One file's changes.
#[derive(Debug, Serialize)]
struct JsonDiffFile {
    /// Repository-relative path, taken from whichever side exists.
    path: String,

    /// `/dev/null` when the file did not exist before.
    old_path: String,

    /// `/dev/null` when the file no longer exists after.
    new_path: String,

    /// `added`, `deleted`, `modified`, `renamed`, `untracked`, …
    status: &'static str,

    /// The conventional single-character status (`A`, `D`, `M`, …).
    code: char,

    binary: bool,
    insertions: usize,
    deletions: usize,
    hunks: Vec<JsonDiffHunk>,
}

impl From<&FileDiff> for JsonDiffFile {
    fn from(diff: &FileDiff) -> Self {
        Self {
            path: diff.display_path().to_string(),
            old_path: diff.old_path.clone(),
            new_path: diff.new_path.clone(),
            status: diff.status.description(),
            code: diff.status.status_char(),
            binary: diff.is_binary,
            insertions: diff.stats.insertions,
            deletions: diff.stats.deletions,
            hunks: diff.hunks.iter().map(JsonDiffHunk::from).collect(),
        }
    }
}

/// A contiguous region of change, matching unified-diff `@@` semantics.
#[derive(Debug, Serialize)]
struct JsonDiffHunk {
    old_start: usize,
    old_count: usize,
    new_start: usize,
    new_count: usize,
    lines: Vec<JsonDiffLine>,
}

impl From<&DiffHunk> for JsonDiffHunk {
    fn from(hunk: &DiffHunk) -> Self {
        Self {
            old_start: hunk.old_start,
            old_count: hunk.old_count,
            new_start: hunk.new_start,
            new_count: hunk.new_count,
            lines: hunk.lines.iter().map(JsonDiffLine::from).collect(),
        }
    }
}

#[derive(Debug, Serialize)]
struct JsonDiffLine {
    /// `context`, `added`, or `removed`.
    status: &'static str,

    /// Line content without its trailing newline.
    content: String,

    /// 1-based line number in the pre-image; `None` for additions.
    old_line: Option<usize>,

    /// 1-based line number in the post-image; `None` for removals.
    new_line: Option<usize>,
}

impl From<&HunkLine> for JsonDiffLine {
    fn from(line: &HunkLine) -> Self {
        Self {
            status: line_status_name(line.status),
            content: line.content.clone(),
            old_line: line.old_line_num,
            new_line: line.new_line_num,
        }
    }
}

/// Whole-diff rollups.
#[derive(Debug, Serialize, Default)]
struct JsonDiffStats {
    files: usize,
    insertions: usize,
    deletions: usize,
}

impl From<&DiffStats> for JsonDiffStats {
    fn from(stats: &DiffStats) -> Self {
        Self {
            files: stats.file_count(),
            insertions: stats.total_insertions(),
            deletions: stats.total_deletions(),
        }
    }
}

fn line_status_name(status: LineStatus) -> &'static str {
    match status {
        LineStatus::Unchanged => "context",
        LineStatus::Added => "added",
        LineStatus::Removed => "removed",
    }
}

/// Print the document to stdout.
pub(super) fn print_json(document: &JsonDiff) -> CliResult<()> {
    let rendered =
        serde_json::to_string_pretty(document).map_err(|e| CliError::Internal(e.into()))?;
    println!("{rendered}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_file_diff() -> FileDiff {
        let mut diff = FileDiff::modified("src/main.rs");
        let mut hunk = DiffHunk::new(1, 2, 1, 2);
        hunk.add_line(HunkLine::context("fn main() {", 1, 1));
        hunk.add_line(HunkLine::removed("    println!(\"hi\");", 2));
        hunk.add_line(HunkLine::added("    println!(\"hi, world\");", 2));
        diff.add_hunk(hunk);
        diff.stats = FileDiffStats::modified("src/main.rs", 1, 1);
        diff
    }

    fn sample_stats(files: &[FileDiff]) -> DiffStats {
        let mut stats = DiffStats::new();
        for f in files {
            stats.add_file(f.stats.clone());
        }
        stats
    }

    #[test]
    fn empty_document_is_valid_json_with_zero_stats() {
        let doc = JsonDiff::empty(Some("main"));
        let value: serde_json::Value =
            serde_json::to_value(&doc).expect("empty document serializes");

        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["view"], "main");
        assert!(value["change"].is_null());
        assert_eq!(value["files"].as_array().unwrap().len(), 0);
        assert_eq!(value["stats"]["files"], 0);
        assert_eq!(value["stats"]["insertions"], 0);
        assert_eq!(value["stats"]["deletions"], 0);
    }

    #[test]
    fn file_diffs_project_to_per_file_hunks_and_lines() {
        let diffs = vec![sample_file_diff()];
        let doc = JsonDiff::new(&diffs, &sample_stats(&diffs), None, Some("main"));
        let value: serde_json::Value = serde_json::to_value(&doc).expect("serializes");

        let file = &value["files"][0];
        assert_eq!(file["path"], "src/main.rs");
        assert_eq!(file["status"], "modified");
        assert_eq!(file["code"], "M");
        assert_eq!(file["insertions"], 1);
        assert_eq!(file["deletions"], 1);
        assert_eq!(file["binary"], false);

        let hunk = &file["hunks"][0];
        assert_eq!(hunk["old_start"], 1);
        assert_eq!(hunk["new_count"], 2);

        let lines = hunk["lines"].as_array().unwrap();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["status"], "context");
        assert_eq!(lines[0]["old_line"], 1);
        assert_eq!(lines[0]["new_line"], 1);

        // A removal has no post-image line number, and vice versa.
        assert_eq!(lines[1]["status"], "removed");
        assert_eq!(lines[1]["old_line"], 2);
        assert!(lines[1]["new_line"].is_null());

        assert_eq!(lines[2]["status"], "added");
        assert_eq!(lines[2]["new_line"], 2);
        assert!(lines[2]["old_line"].is_null());
    }

    #[test]
    fn aggregate_stats_roll_up_across_files() {
        let diffs = vec![sample_file_diff(), sample_file_diff()];
        let doc = JsonDiff::new(&diffs, &sample_stats(&diffs), None, None);
        let value: serde_json::Value = serde_json::to_value(&doc).expect("serializes");

        assert_eq!(value["stats"]["files"], 2);
        assert_eq!(value["stats"]["insertions"], 2);
        assert_eq!(value["stats"]["deletions"], 2);
        assert!(value["view"].is_null());
    }

    #[test]
    fn added_file_reports_dev_null_as_old_path() {
        let diff = FileDiff::added("src/new.rs");
        let mut added = diff.clone();
        added.stats = FileDiffStats::added("src/new.rs", 3);
        let diffs = vec![added];
        let doc = JsonDiff::new(&diffs, &sample_stats(&diffs), None, None);
        let value: serde_json::Value = serde_json::to_value(&doc).expect("serializes");

        let file = &value["files"][0];
        assert_eq!(file["old_path"], "/dev/null");
        assert_eq!(file["new_path"], "src/new.rs");
        assert_eq!(file["path"], "src/new.rs");
        assert_eq!(file["status"], "added");
        assert_eq!(file["code"], "A");
    }

    #[test]
    fn deleted_file_reports_dev_null_as_new_path() {
        let mut diff = FileDiff::deleted("src/old.rs");
        diff.stats = FileDiffStats::deleted("src/old.rs", 2);
        let diffs = vec![diff];
        let doc = JsonDiff::new(&diffs, &sample_stats(&diffs), None, None);
        let value: serde_json::Value = serde_json::to_value(&doc).expect("serializes");

        let file = &value["files"][0];
        assert_eq!(file["old_path"], "src/old.rs");
        assert_eq!(file["new_path"], "/dev/null");
        assert_eq!(file["status"], "deleted");
        assert_eq!(file["code"], "D");
    }
}
