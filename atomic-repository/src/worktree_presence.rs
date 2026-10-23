//! Exact-name presence checks for Git worktree paths.
//!
//! `Path::symlink_metadata` answers "does something resolve here", which is
//! the wrong question for a path Git has deleted:
//!
//! - a case-insensitive filesystem (the macOS default) resolves `Foo.md`
//!   while only `foo.md` exists, so a case-only rename looks like a file
//!   that was never removed;
//! - an ancestor that became a symlink to a directory lets `a/b/f` resolve
//!   through the link although Git has no entry at `a/b/f` any more (the
//!   `.agents/skills/<name> -> ../../.claude/skills/<name>` layout).
//!
//! [`WorktreeEntries`] walks a relative path one component at a time
//! without following symlinks and requires every component to match a
//! directory entry byte-for-byte. Directory listings are memoized so a
//! commit that removes a large tree costs one `read_dir` per directory.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

/// Memoized exact-name lookup rooted at a worktree directory.
pub struct WorktreeEntries {
    root: PathBuf,
    listings: HashMap<PathBuf, Option<HashSet<OsString>>>,
}

impl WorktreeEntries {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            listings: HashMap::new(),
        }
    }

    /// Metadata of `relative` when every component of the path exists on
    /// disk under its exact name and every ancestor is a real directory.
    ///
    /// Returns `Ok(None)` when any component is missing, differs only by
    /// case or normalization, or sits behind a symlinked or regular-file
    /// ancestor. Symlinks are never followed; the final component's own
    /// metadata is returned as-is (a symlink reports itself).
    pub fn exact_metadata(&mut self, relative: &Path) -> io::Result<Option<fs::Metadata>> {
        let mut current = self.root.clone();
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Ok(None);
            };
            if !self.directory_has_entry(&current, name)? {
                return Ok(None);
            }
            current.push(name);
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) => Ok(Some(metadata)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn directory_has_entry(
        &mut self,
        directory: &Path,
        name: &std::ffi::OsStr,
    ) -> io::Result<bool> {
        if !self.listings.contains_key(directory) {
            let listing = list_real_directory(directory)?;
            self.listings.insert(directory.to_path_buf(), listing);
        }
        Ok(self
            .listings
            .get(directory)
            .and_then(Option::as_ref)
            .is_some_and(|entries| entries.contains(name)))
    }
}

/// The entry names of `directory` when it is a real directory (not a
/// symlink to one); `None` when it is absent, a file, or a symlink.
fn list_real_directory(directory: &Path) -> io::Result<Option<HashSet<OsString>>> {
    let metadata = match fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.is_dir() {
        return Ok(None);
    }
    let mut entries = HashSet::new();
    for entry in fs::read_dir(directory)? {
        entries.insert(entry?.file_name());
    }
    Ok(Some(entries))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> PathBuf {
        let root = tempfile::tempdir().unwrap().keep();
        fs::create_dir_all(root.join("dir/sub")).unwrap();
        fs::write(root.join("dir/sub/file.txt"), b"x").unwrap();
        fs::write(root.join("foo.md"), b"y").unwrap();
        root
    }

    #[test]
    fn exact_path_reports_metadata() {
        let root = fixture();
        let mut entries = WorktreeEntries::new(&root);
        let metadata = entries
            .exact_metadata(Path::new("dir/sub/file.txt"))
            .unwrap()
            .expect("file exists under its exact name");
        assert!(metadata.is_file());
        assert!(entries
            .exact_metadata(Path::new("dir/sub"))
            .unwrap()
            .is_some_and(|m| m.is_dir()));
    }

    #[test]
    fn missing_component_is_absent() {
        let root = fixture();
        let mut entries = WorktreeEntries::new(&root);
        assert!(entries
            .exact_metadata(Path::new("dir/nope/file.txt"))
            .unwrap()
            .is_none());
        assert!(entries
            .exact_metadata(Path::new("dir/sub/other.txt"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn name_differing_only_by_case_is_absent() {
        // On a case-insensitive filesystem `symlink_metadata("Foo.md")`
        // succeeds although only `foo.md` exists; the exact lookup must not.
        let root = fixture();
        let mut entries = WorktreeEntries::new(&root);
        assert!(entries
            .exact_metadata(Path::new("Foo.md"))
            .unwrap()
            .is_none());
        assert!(entries
            .exact_metadata(Path::new("foo.md"))
            .unwrap()
            .is_some());
    }

    #[cfg(unix)]
    #[test]
    fn path_behind_symlinked_ancestor_is_absent() {
        let root = fixture();
        std::os::unix::fs::symlink("dir/sub", root.join("link")).unwrap();
        let mut entries = WorktreeEntries::new(&root);
        // `link/file.txt` resolves through the symlink on disk, but no real
        // directory entry `link/file.txt` exists.
        assert!(root.join("link/file.txt").symlink_metadata().is_ok());
        assert!(entries
            .exact_metadata(Path::new("link/file.txt"))
            .unwrap()
            .is_none());
        // The symlink itself is an exact entry and reports as a symlink.
        let link = entries.exact_metadata(Path::new("link")).unwrap().unwrap();
        assert!(link.file_type().is_symlink());
    }

    #[test]
    fn non_normal_components_are_absent() {
        let root = fixture();
        let mut entries = WorktreeEntries::new(&root);
        assert!(entries
            .exact_metadata(Path::new("../foo.md"))
            .unwrap()
            .is_none());
        assert!(entries
            .exact_metadata(Path::new("./foo.md"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn listings_are_memoized_per_directory() {
        let root = fixture();
        let mut entries = WorktreeEntries::new(&root);
        entries
            .exact_metadata(Path::new("dir/sub/file.txt"))
            .unwrap();
        entries
            .exact_metadata(Path::new("dir/sub/other.txt"))
            .unwrap();
        assert_eq!(
            entries.listings.len(),
            3,
            "root, dir and dir/sub listed once each"
        );
    }
}
