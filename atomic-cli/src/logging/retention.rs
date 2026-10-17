//! Keep the newest few daily log files.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Delete all but the newest `keep` `<prefix>.YYYY-MM-DD.<suffix>` files in
/// `dir`, leaving every other file alone. Several processes can prune at once,
/// so a file that is already gone is not a failure; other failures are
/// returned for the caller to report.
pub(super) fn prune(
    dir: &Path,
    prefix: &str,
    suffix: &str,
    keep: usize,
) -> Vec<(PathBuf, io::Error)> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => return vec![(dir.to_path_buf(), error)],
    };
    let mut daily: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .map(|entry| entry.path())
        .filter(|path| is_daily_file(path, prefix, suffix))
        .collect();
    // ISO dates sort by name.
    daily.sort();
    let excess = daily.len().saturating_sub(keep);
    daily
        .into_iter()
        .take(excess)
        .filter_map(|path| match fs::remove_file(&path) {
            Ok(()) => None,
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => Some((path, error)),
        })
        .collect()
}

fn is_daily_file(path: &Path, prefix: &str, suffix: &str) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let date = name
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('.'))
        .and_then(|rest| rest.strip_suffix(suffix))
        .and_then(|rest| rest.strip_suffix('.'));
    date.is_some_and(is_iso_date)
}

fn is_iso_date(text: &str) -> bool {
    text.len() == 10
        && text.bytes().enumerate().all(|(index, byte)| match index {
            4 | 7 => byte == b'-',
            _ => byte.is_ascii_digit(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn keeps_the_newest_daily_files_and_nothing_else_is_touched() {
        let dir = tempfile::tempdir().unwrap();
        for day in 1..=9 {
            fs::write(dir.path().join(format!("atomic.2026-09-{day:02}.log")), "").unwrap();
        }
        for other in [
            "notes.txt",
            "atomic.log",
            "atomic.2026-9-1.log",
            "other.2026-09-01.log",
        ] {
            fs::write(dir.path().join(other), "").unwrap();
        }
        fs::create_dir(dir.path().join("atomic.2026-08-01.log")).unwrap();

        let failures = prune(dir.path(), "atomic", "log", 7);

        assert!(failures.is_empty(), "{failures:?}");
        let expected: Vec<String> = [
            "atomic.2026-08-01.log",
            "atomic.2026-09-03.log",
            "atomic.2026-09-04.log",
            "atomic.2026-09-05.log",
            "atomic.2026-09-06.log",
            "atomic.2026-09-07.log",
            "atomic.2026-09-08.log",
            "atomic.2026-09-09.log",
            "atomic.2026-9-1.log",
            "atomic.log",
            "notes.txt",
            "other.2026-09-01.log",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(names(dir.path()), expected);
    }

    #[test]
    fn fewer_files_than_kept_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("atomic.2026-09-28.log"), "").unwrap();
        assert!(prune(dir.path(), "atomic", "log", 7).is_empty());
        assert_eq!(names(dir.path()), ["atomic.2026-09-28.log"]);
    }

    #[test]
    fn a_missing_directory_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let failures = prune(&missing, "atomic", "log", 7);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].0, missing);
    }
}
