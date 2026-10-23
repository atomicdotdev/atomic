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
//! without following symlinks and requires every component to name a
//! directory entry exactly: case is significant, Unicode normalization is
//! not (Git records composed names while HFS+ and some network mounts store
//! them decomposed). Directory listings are memoized so a commit that
//! removes a large tree costs one `read_dir` per directory.

use std::borrow::Cow;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use unicode_normalization::{is_nfc, UnicodeNormalization};

/// Memoized exact-name lookup rooted at a worktree directory.
pub struct WorktreeEntries {
    root: PathBuf,
    listings: HashMap<PathBuf, Option<DirectoryListing>>,
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
    /// Returns `Ok(None)` when any component is missing, differs by case,
    /// or sits behind a symlinked or regular-file ancestor. A component
    /// matches an entry that differs only in Unicode normalization, and the
    /// walk continues under the entry's own spelling. Symlinks are never
    /// followed; the final component's own metadata is returned as-is (a
    /// symlink reports itself).
    pub fn exact_metadata(&mut self, relative: &Path) -> io::Result<Option<fs::Metadata>> {
        let mut current = self.root.clone();
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Ok(None);
            };
            let spelled = self
                .listing(&current)?
                .and_then(|listing| listing.spelling_of(name))
                .map(OsStr::to_os_string);
            let Some(spelled) = spelled else {
                return Ok(None);
            };
            current.push(spelled);
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) => Ok(Some(metadata)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn listing(&mut self, directory: &Path) -> io::Result<Option<&DirectoryListing>> {
        let listing = match self.listings.entry(directory.to_path_buf()) {
            Entry::Occupied(occupied) => occupied.into_mut(),
            Entry::Vacant(vacant) => vacant.insert(list_real_directory(directory)?),
        };
        Ok(listing.as_ref())
    }
}

/// The entries of one real directory.
#[derive(Debug, Default)]
struct DirectoryListing {
    /// Every entry name as the filesystem spells it.
    names: HashSet<OsString>,
    /// The composed (NFC) spelling of each name the filesystem stores
    /// decomposed, mapped to the spelling on disk.
    composed: HashMap<String, OsString>,
}

impl DirectoryListing {
    fn read(directory: &Path) -> io::Result<Self> {
        let mut listing = Self::default();
        for entry in fs::read_dir(directory)? {
            let name = entry?.file_name();
            if let Some(text) = name.to_str() {
                if !is_nfc(text) {
                    listing.composed.insert(text.nfc().collect(), name.clone());
                }
            }
            listing.names.insert(name);
        }
        Ok(listing)
    }

    /// The on-disk spelling of `name`: the same bytes, or the entry whose
    /// composed form equals `name`'s composed form.
    fn spelling_of(&self, name: &OsStr) -> Option<&OsStr> {
        if let Some(exact) = self.names.get(name) {
            return Some(exact.as_os_str());
        }
        let text = name.to_str()?;
        let composed: Cow<str> = if is_nfc(text) {
            Cow::Borrowed(text)
        } else {
            Cow::Owned(text.nfc().collect())
        };
        if let Some(stored) = self.names.get(OsStr::new(composed.as_ref())) {
            return Some(stored.as_os_str());
        }
        self.composed
            .get(composed.as_ref())
            .map(OsString::as_os_str)
    }
}

/// The entries of `directory` when it is a real directory (not a symlink
/// to one); `None` when it is absent, a file, or a symlink.
fn list_real_directory(directory: &Path) -> io::Result<Option<DirectoryListing>> {
    let metadata = match fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.is_dir() {
        return Ok(None);
    }
    DirectoryListing::read(directory).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("dir/sub")).unwrap();
        fs::write(root.join("dir/sub/file.txt"), b"x").unwrap();
        fs::write(root.join("foo.md"), b"y").unwrap();
        dir
    }

    #[test]
    fn exact_path_reports_metadata() {
        let dir = fixture();
        let mut entries = WorktreeEntries::new(dir.path());
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
        let dir = fixture();
        let mut entries = WorktreeEntries::new(dir.path());
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
        let dir = fixture();
        let mut entries = WorktreeEntries::new(dir.path());
        assert!(entries
            .exact_metadata(Path::new("Foo.md"))
            .unwrap()
            .is_none());
        assert!(entries
            .exact_metadata(Path::new("foo.md"))
            .unwrap()
            .is_some());
    }

    #[test]
    fn composed_and_decomposed_spellings_name_the_same_entry() {
        let dir = fixture();
        let root = dir.path();
        // Stored composed (as Git records it), queried decomposed.
        fs::write(root.join("caf\u{e9}.txt"), b"c").unwrap();
        // Stored decomposed (as HFS+ returns it), queried composed.
        fs::write(root.join("dir/na\u{308}ive.txt"), b"n").unwrap();
        let mut entries = WorktreeEntries::new(root);
        assert!(entries
            .exact_metadata(Path::new("cafe\u{301}.txt"))
            .unwrap()
            .is_some_and(|m| m.is_file()));
        assert!(entries
            .exact_metadata(Path::new("dir/n\u{e4}ive.txt"))
            .unwrap()
            .is_some_and(|m| m.is_file()));
        // Case stays significant.
        assert!(entries
            .exact_metadata(Path::new("CAF\u{c9}.txt"))
            .unwrap()
            .is_none());
    }

    #[cfg(unix)]
    #[test]
    fn path_behind_symlinked_ancestor_is_absent() {
        let dir = fixture();
        let root = dir.path();
        std::os::unix::fs::symlink("dir/sub", root.join("link")).unwrap();
        let mut entries = WorktreeEntries::new(root);
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
        let dir = fixture();
        let mut entries = WorktreeEntries::new(dir.path());
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
        let dir = fixture();
        let mut entries = WorktreeEntries::new(dir.path());
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
