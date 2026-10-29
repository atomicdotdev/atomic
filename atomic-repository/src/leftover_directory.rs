//! Directories Git leaves behind.
//!
//! `git mv` of the last file out of a directory removes the file and keeps
//! the emptied directory on disk; Git trees cannot hold it, so nothing on
//! the Git side ever cleans it up. Atomic records directories, and its
//! status treated the leftover as a reappearance of the recorded directory
//! (`new dir`), which made every shim reconcile and verify refuse with
//! `Atomic working copy is not clean` until the user removed it by hand.

use std::path::{Path, PathBuf};

use crate::ignore::IgnoreRules;

/// Whether the directory `relative` under `root` holds any entry that is
/// not ignored, descending into subdirectories. An emptied directory, or one
/// holding only ignored noise such as `.DS_Store`, holds nothing visible: its
/// presence on disk is not a reappearance of a recorded directory. An
/// unreadable directory answers `true`, so the caller keeps the conservative
/// classification; an entry that cannot be read inside a readable directory
/// is skipped, since it cannot be shown either.
pub(crate) fn holds_visible_entries(
    root: &Path,
    relative: &Path,
    rules: Option<&IgnoreRules>,
) -> bool {
    let mut pending: Vec<PathBuf> = vec![relative.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(root.join(&directory)) else {
            return true;
        };
        for entry in entries.flatten() {
            let child = directory.join(entry.file_name());
            let is_directory = entry
                .file_type()
                .map(|file_type| file_type.is_dir())
                .unwrap_or(false);
            if rules.is_some_and(|rules| rules.is_ignored(&child, is_directory)) {
                continue;
            }
            if is_directory {
                pending.push(child);
            } else {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::holds_visible_entries;
    use std::fs;

    #[test]
    fn an_emptied_directory_holds_nothing_visible() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("pkg/nested")).unwrap();
        assert!(!holds_visible_entries(
            root.path(),
            std::path::Path::new("pkg"),
            None
        ));
    }

    #[test]
    fn a_directory_with_a_file_holds_a_visible_entry() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("pkg/nested")).unwrap();
        fs::write(root.path().join("pkg/nested/keep.txt"), b"x").unwrap();
        assert!(holds_visible_entries(
            root.path(),
            std::path::Path::new("pkg"),
            None
        ));
    }

    #[test]
    fn a_missing_directory_is_left_to_the_caller() {
        let root = tempfile::tempdir().unwrap();
        assert!(holds_visible_entries(
            root.path(),
            std::path::Path::new("gone"),
            None
        ));
    }
}
