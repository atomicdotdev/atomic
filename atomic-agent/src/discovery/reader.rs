//! Low-level I/O helpers for reading agent storage during discovery.
//!
//! These functions handle JSONL streaming, single-document JSON, and
//! read-only SQLite access.

use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use rusqlite::{Connection, OpenFlags};

use crate::error::{AgentError, AgentResult};

/// Maximum JSON file size accepted by [`read_json`] to prevent OOM on oversized files.
pub const MAX_JSON_BYTES: u64 = 64 * 1024 * 1024; // 64 MiB

// JSONL Helpers

/// Read every line of a JSONL file, parsing each as a [`serde_json::Value`].
///
/// Streams via `BufReader::lines()` — does not load the full file at once.
/// Empty and whitespace-only lines are skipped silently; malformed lines are
/// logged via `log::warn!` and skipped (parse errors do not abort the read).
/// Lines with invalid UTF-8 encoding are logged via `log::warn!` and skipped;
/// all other I/O errors are propagated as [`AgentError::DiscoveryReadFailed`].
/// A leading UTF-8 BOM (`\u{FEFF}`) on the first line is stripped automatically.
pub fn read_jsonl(path: &Path) -> AgentResult<Vec<serde_json::Value>> {
    let file = fs::File::open(path).map_err(|e| AgentError::DiscoveryReadFailed {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;
    let reader = BufReader::new(file);
    let mut values = Vec::new();
    let mut bom_stripped = false;

    for line_result in reader.lines() {
        let line = match line_result {
            Ok(line) => line,
            Err(err) if err.kind() == std::io::ErrorKind::InvalidData => {
                log::warn!(
                    "discovery reader: skipping line with invalid UTF-8 in {}: {}",
                    path.display(),
                    err
                );
                continue;
            }
            Err(err) => {
                return Err(AgentError::DiscoveryReadFailed {
                    path: path.to_path_buf(),
                    reason: err.to_string(),
                });
            }
        };

        let trimmed = line.trim();
        let trimmed = if !bom_stripped {
            bom_stripped = true;
            trimmed.strip_prefix('\u{FEFF}').unwrap_or(trimmed)
        } else {
            trimmed
        };

        if trimmed.is_empty() {
            continue;
        }

        match serde_json::from_str(trimmed) {
            Ok(value) => values.push(value),
            Err(err) => {
                log::warn!(
                    "discovery reader: skipping malformed JSONL line in {}: {}",
                    path.display(),
                    err
                );
            }
        }
    }

    Ok(values)
}

/// Read a JSONL file starting at byte `offset` for cursor-based scanning.
///
/// Returns `(parsed values, new byte offset)` where the new offset is the
/// stream position after the read, capped at the file's current size. The
/// returned offset is a valid input for a subsequent call to resume reading.
/// If `offset` is at or past EOF, returns `(vec![], file_size)` without error.
/// Same malformed-line and invalid-UTF-8 tolerance as [`read_jsonl`].
/// A leading UTF-8 BOM (`\u{FEFF}`) on the first line is stripped automatically.
///
/// **Offset contract**: `offset` MUST be `0` or a value previously returned by
/// `read_jsonl_since` on the same file. Hand-constructed offsets that land
/// mid-line will cause the partial line to be skipped as malformed without
/// recovery. After file truncation or rotation, treat the cursor as invalidated
/// and restart from `0`.
pub fn read_jsonl_since(path: &Path, offset: u64) -> AgentResult<(Vec<serde_json::Value>, u64)> {
    let file = fs::File::open(path).map_err(|e| AgentError::DiscoveryReadFailed {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;

    let file_size =
        file.metadata()
            .map(|m| m.len())
            .map_err(|e| AgentError::DiscoveryReadFailed {
                path: path.to_path_buf(),
                reason: e.to_string(),
            })?;

    if offset >= file_size {
        return Ok((Vec::new(), file_size));
    }

    let mut file = file;
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| AgentError::DiscoveryReadFailed {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })?;

    let mut reader = BufReader::new(file);
    let mut values = Vec::new();
    let mut bom_stripped = false;

    for line_result in reader.by_ref().lines() {
        let line = match line_result {
            Ok(line) => line,
            Err(err) if err.kind() == std::io::ErrorKind::InvalidData => {
                log::warn!(
                    "discovery reader: skipping line with invalid UTF-8 in {}: {}",
                    path.display(),
                    err
                );
                continue;
            }
            Err(err) => {
                return Err(AgentError::DiscoveryReadFailed {
                    path: path.to_path_buf(),
                    reason: err.to_string(),
                });
            }
        };

        let trimmed = line.trim();
        let trimmed = if !bom_stripped {
            bom_stripped = true;
            trimmed.strip_prefix('\u{FEFF}').unwrap_or(trimmed)
        } else {
            trimmed
        };

        if trimmed.is_empty() {
            continue;
        }

        match serde_json::from_str(trimmed) {
            Ok(value) => values.push(value),
            Err(err) => {
                log::warn!(
                    "discovery reader: skipping malformed JSONL line in {}: {}",
                    path.display(),
                    err
                );
            }
        }
    }

    // Recover the underlying file to get its current stream position, capped
    // at the file's size so the returned offset is always a valid resume point.
    let new_offset = reader
        .into_inner()
        .stream_position()
        .map_err(|e| AgentError::DiscoveryReadFailed {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })?
        .min(file_size);

    Ok((values, new_offset))
}

// JSON Helper

/// Read a single JSON document from `path`.
///
/// Rejects files larger than [`MAX_JSON_BYTES`] (64 MiB) to prevent OOM.
/// Strips an optional UTF-8 BOM (`\u{FEFF}`) from the start of the file
/// before parsing. Any I/O or parse error is returned as
/// [`AgentError::DiscoveryReadFailed`].
pub fn read_json(path: &Path) -> AgentResult<serde_json::Value> {
    let metadata = fs::metadata(path).map_err(|e| AgentError::DiscoveryReadFailed {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;

    if metadata.len() > MAX_JSON_BYTES {
        return Err(AgentError::DiscoveryReadFailed {
            path: path.to_path_buf(),
            reason: format!(
                "JSON file too large ({} bytes, max {} bytes)",
                metadata.len(),
                MAX_JSON_BYTES
            ),
        });
    }

    let raw = fs::read_to_string(path).map_err(|e| AgentError::DiscoveryReadFailed {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;

    let content = raw.strip_prefix('\u{FEFF}').unwrap_or(&raw);

    serde_json::from_str(content).map_err(|e| AgentError::DiscoveryReadFailed {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })
}

// SQLite Helper

/// Open a SQLite database in strictly read-only mode (no file creation).
///
/// Returns [`AgentError::DiscoveryReadFailed`] if the file does not exist or
/// cannot be opened. The `SQLITE_OPEN_CREATE` flag is intentionally absent so
/// that a missing file surfaces as an error rather than an empty database.
///
/// **Thread safety**: `rusqlite::Connection` is `Send` but NOT `Sync`. Adapters
/// implementing [`crate::discovery::TraceDiscovery`] (which is `Send + Sync`)
/// MUST NOT store a `Connection` in their struct directly. Instead, open a fresh
/// connection in each `list_traces` / `read_events` call, OR store the database
/// path and wrap any cached connection in `std::sync::Mutex`.
pub fn open_sqlite_readonly(path: &Path) -> AgentResult<Connection> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|e| {
        AgentError::DiscoveryReadFailed {
            path: path.to_path_buf(),
            reason: e.to_string(),
        }
    })
}

// Tests

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::tempdir;

    use super::*;
    use crate::error::AgentError;

    // --- read_jsonl ---

    #[test]
    fn test_read_jsonl_happy_path() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, r#"{{"a":1}}"#).unwrap();
        writeln!(f, r#"{{"b":2}}"#).unwrap();
        writeln!(f, r#"{{"c":3}}"#).unwrap();

        let values = read_jsonl(&path).unwrap();
        assert_eq!(values.len(), 3);
        assert_eq!(values[0]["a"], 1);
        assert_eq!(values[1]["b"], 2);
        assert_eq!(values[2]["c"], 3);
    }

    #[test]
    fn test_read_jsonl_skips_empty_and_whitespace_lines() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, r#"{{"a":1}}"#).unwrap();
        writeln!(f).unwrap(); // empty line
        writeln!(f, "   ").unwrap(); // whitespace-only line
        writeln!(f, r#"{{"b":2}}"#).unwrap();

        let values = read_jsonl(&path).unwrap();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0]["a"], 1);
        assert_eq!(values[1]["b"], 2);
    }

    #[test]
    fn test_read_jsonl_skips_malformed_lines() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, r#"{{"a":1}}"#).unwrap();
        writeln!(f, "this is not json").unwrap();
        writeln!(f, r#"{{"b":2}}"#).unwrap();
        writeln!(f, r#"{{"c":3}}"#).unwrap();

        let values = read_jsonl(&path).unwrap();
        assert_eq!(values.len(), 3);
        assert_eq!(values[0]["a"], 1);
        assert_eq!(values[1]["b"], 2);
        assert_eq!(values[2]["c"], 3);
    }

    #[test]
    fn test_read_jsonl_file_not_found() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nonexistent.jsonl");

        let err = read_jsonl(&path).unwrap_err();
        assert!(matches!(err, AgentError::DiscoveryReadFailed { .. }));
    }

    #[test]
    fn test_read_jsonl_skips_invalid_utf8() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("invalid_utf8.jsonl");
        // Build the file as raw bytes: valid JSON line, invalid UTF-8 line, valid JSON line.
        let mut content: Vec<u8> = Vec::new();
        content.extend_from_slice(b"{\"a\":1}\n");
        content.extend_from_slice(&[0xC3, 0x28, b'\n']); // invalid UTF-8 continuation
        content.extend_from_slice(b"{\"b\":2}\n");
        fs::write(&path, &content).unwrap();

        let values = read_jsonl(&path).unwrap();
        assert_eq!(values.len(), 2, "invalid UTF-8 line should be skipped");
        assert_eq!(values[0]["a"], 1);
        assert_eq!(values[1]["b"], 2);
    }

    #[test]
    fn test_read_jsonl_strips_leading_bom() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bom.jsonl");
        // Write UTF-8 BOM followed by a valid JSON line.
        let mut content: Vec<u8> = vec![0xEF, 0xBB, 0xBF]; // UTF-8 BOM bytes
        content.extend_from_slice(b"{\"k\":\"v\"}\n");
        fs::write(&path, &content).unwrap();

        let values = read_jsonl(&path).unwrap();
        assert_eq!(
            values.len(),
            1,
            "BOM-prefixed line should parse successfully"
        );
        assert_eq!(values[0]["k"], "v");
    }

    // --- read_jsonl_since ---

    #[test]
    fn test_read_jsonl_since_offset_zero_matches_read_jsonl() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, r#"{{"a":1}}"#).unwrap();
        writeln!(f, r#"{{"b":2}}"#).unwrap();
        writeln!(f, r#"{{"c":3}}"#).unwrap();

        let expected = read_jsonl(&path).unwrap();
        let (values, _offset) = read_jsonl_since(&path, 0).unwrap();
        assert_eq!(values, expected);
    }

    #[test]
    fn test_read_jsonl_since_resumes_from_offset() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, r#"{{"a":1}}"#).unwrap();
        writeln!(f, r#"{{"b":2}}"#).unwrap();

        // Read the whole file, capturing the final offset.
        let (_values, final_offset) = read_jsonl_since(&path, 0).unwrap();

        // Reading again from that offset should return no new values.
        let (values2, offset2) = read_jsonl_since(&path, final_offset).unwrap();
        assert!(values2.is_empty());
        assert_eq!(offset2, final_offset);
    }

    #[test]
    fn test_read_jsonl_since_past_eof() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, r#"{{"a":1}}"#).unwrap();

        let past_eof = 99_999u64;
        let (values, returned_offset) = read_jsonl_since(&path, past_eof).unwrap();
        assert!(values.is_empty());
        let file_size = fs::metadata(&path).unwrap().len();
        assert_eq!(
            returned_offset, file_size,
            "past-EOF offset must clamp to file size"
        );
    }

    // --- read_json ---

    #[test]
    fn test_read_json_happy_path() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.json");
        fs::write(&path, r#"{"k":"v"}"#).unwrap();

        let value = read_json(&path).unwrap();
        assert_eq!(value["k"], "v");
    }

    #[test]
    fn test_read_json_strips_bom() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bom.json");
        // Write UTF-8 BOM followed by valid JSON.
        let mut content = vec![0xEF, 0xBB, 0xBF]; // UTF-8 BOM bytes
        content.extend_from_slice(r#"{"k":"v"}"#.as_bytes());
        fs::write(&path, &content).unwrap();

        let value = read_json(&path).unwrap();
        assert_eq!(value["k"], "v");
    }

    #[test]
    fn test_read_json_trailing_comma_errors() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.json");
        fs::write(&path, r#"{"k":"v",}"#).unwrap();

        let err = read_json(&path).unwrap_err();
        assert!(matches!(err, AgentError::DiscoveryReadFailed { .. }));
    }

    #[test]
    fn test_read_json_file_not_found() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nonexistent.json");

        let err = read_json(&path).unwrap_err();
        assert!(matches!(err, AgentError::DiscoveryReadFailed { .. }));
    }

    #[test]
    fn test_read_json_rejects_oversize() {
        use std::fs::OpenOptions;

        let dir = tempdir().unwrap();
        let path = dir.path().join("huge.json");

        // Create a sparse file one byte over the limit — no actual disk allocation needed.
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .open(&path)
            .unwrap();
        file.set_len(MAX_JSON_BYTES + 1).unwrap();

        let err = read_json(&path).unwrap_err();
        assert!(
            matches!(err, AgentError::DiscoveryReadFailed { .. }),
            "expected DiscoveryReadFailed, got: {:?}",
            err
        );
        let msg = err.to_string();
        assert!(
            msg.contains("too large"),
            "error message should mention 'too large', got: {msg}"
        );
    }

    // --- open_sqlite_readonly ---

    #[test]
    fn test_open_sqlite_readonly_happy_path() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.db");

        // Create a DB and write a row via a writable connection.
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT);
                 INSERT INTO items (name) VALUES ('hello');",
            )
            .unwrap();
        }

        // Open read-only and confirm we can query the row.
        let conn = open_sqlite_readonly(&db_path).unwrap();
        let name: String = conn
            .query_row("SELECT name FROM items WHERE id = 1", [], |row| row.get(0))
            .unwrap();
        assert_eq!(name, "hello");
    }

    #[test]
    fn test_open_sqlite_readonly_rejects_missing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("missing.db");

        let err = open_sqlite_readonly(&path).unwrap_err();
        assert!(matches!(err, AgentError::DiscoveryReadFailed { .. }));
        // Confirm no file was auto-created.
        assert!(!path.exists());
    }

    #[test]
    fn test_open_sqlite_readonly_blocks_writes() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.db");

        // Create the DB with a table via a writable connection.
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch("CREATE TABLE items (id INTEGER PRIMARY KEY);")
                .unwrap();
        }

        // Open read-only and attempt a write — must fail.
        let conn = open_sqlite_readonly(&db_path).unwrap();
        let result = conn.execute("INSERT INTO items (id) VALUES (1)", []);
        assert!(result.is_err(), "write on read-only connection should fail");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("readonly"),
            "expected 'readonly' in error, got: {err_msg}"
        );
    }
}
