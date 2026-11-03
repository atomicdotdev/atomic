//! Regression: zero-length graph vertices must not capture a neighbour's
//! context.
//!
//! An empty file, or a file truncated to zero bytes, is recorded with a
//! zero-length vertex. Such a vertex shares its position with the END of
//! whatever content the same change serialized just before it, and with the
//! START of whatever comes after it. Two position-only lookups then went
//! wrong in real repositories:
//!
//! - appending to a file whose last line was recorded in the same change as
//!   another file's truncation attached the new line to the truncated file's
//!   empty vertex (`atomic status` reported the file modified right after a
//!   clean record; `atomic git import` of the objx repository refused at the
//!   commit deleting the truncated `go.sum`);
//! - moving a directory whose first entry is an empty file (a Python
//!   package's `__init__.py`) next to a non-empty sibling failed to apply
//!   (`ambiguous source ... for prior FOLDER edge`), because the empty
//!   file's inode marker starts where the sibling's name vertex starts.

use std::fs;
use std::path::Path;

use atomic_core::change::ChangeHeader;
use atomic_core::WorkingCopyId;
use atomic_repository::{RecordOptions, Repository, StatusOptions, DEFAULT_VIEW};
use tempfile::TempDir;

fn working_copy(repo: &Repository) -> WorkingCopyId {
    repo.require_working_copy_id().expect("working copy id")
}

fn write(root: &Path, path: &str, content: &[u8]) {
    let full = root.join(path);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(full, content).unwrap();
}

fn add(repo: &Repository, path: &str) {
    repo.add(working_copy(repo), path, Default::default())
        .unwrap_or_else(|error| panic!("add '{path}': {error}"));
}

fn record_all(repo: &Repository, message: &str) {
    repo.record(
        working_copy(repo),
        ChangeHeader::new(message),
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true),
    )
    .unwrap_or_else(|error| panic!("record '{message}': {error}"));
}

fn dirty_paths(repo: &Repository) -> Vec<String> {
    repo.status(working_copy(repo), StatusOptions::default())
        .expect("status")
        .entries()
        .iter()
        .map(|entry| entry.path().to_string_lossy().into_owned())
        .collect()
}

fn content(repo: &Repository, path: &str) -> Option<Vec<u8>> {
    repo.get_file_content_on_view(path, DEFAULT_VIEW)
        .unwrap_or_else(|error| panic!("content of '{path}': {error}"))
}

#[test]
fn appending_after_a_line_recorded_alongside_a_truncation_renders() {
    let dir = TempDir::new().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    write(dir.path(), "a.txt", b"a\n");
    write(dir.path(), "notes.txt", b"l1\nl2\nl3\n");
    add(&repo, "a.txt");
    add(&repo, "notes.txt");
    record_all(&repo, "base");

    // One record truncates notes.txt (serialized after a.txt) and appends
    // to a.txt: the truncation's zero-length vertex lands exactly where
    // a.txt's new last line ends.
    write(dir.path(), "notes.txt", b"");
    write(dir.path(), "a.txt", b"a\nb\n");
    record_all(&repo, "truncate + append");
    assert_eq!(content(&repo, "notes.txt"), Some(Vec::new()));
    assert_eq!(content(&repo, "a.txt"), Some(b"a\nb\n".to_vec()));

    // Appending again anchors on that last line, not on the empty vertex.
    write(dir.path(), "a.txt", b"a\nb\nc\n");
    record_all(&repo, "append again");
    assert_eq!(content(&repo, "a.txt"), Some(b"a\nb\nc\n".to_vec()));
    assert_eq!(content(&repo, "notes.txt"), Some(Vec::new()));
    assert_eq!(
        dirty_paths(&repo),
        Vec::<String>::new(),
        "a clean record leaves nothing to record"
    );

    // Deleting and rewriting around that boundary resolves the deletion
    // edges' source the same way (the edge from `b` into `c` starts where
    // the empty vertex sits).
    write(dir.path(), "a.txt", b"a\nb\n");
    record_all(&repo, "delete the appended line");
    assert_eq!(content(&repo, "a.txt"), Some(b"a\nb\n".to_vec()));
    write(dir.path(), "a.txt", b"a\nB\nc\n");
    record_all(&repo, "rewrite the boundary line and append");
    assert_eq!(content(&repo, "a.txt"), Some(b"a\nB\nc\n".to_vec()));
    assert_eq!(content(&repo, "notes.txt"), Some(Vec::new()));
    assert_eq!(dirty_paths(&repo), Vec::<String>::new());
}

#[test]
fn moving_a_directory_whose_empty_file_precedes_a_sibling_records() {
    let dir = TempDir::new().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    write(dir.path(), "pkg/__init__.py", b"");
    write(dir.path(), "pkg/mod.py", b"x = 1\n");
    add(&repo, "pkg/__init__.py");
    add(&repo, "pkg/mod.py");
    record_all(&repo, "base");

    fs::rename(dir.path().join("pkg"), dir.path().join("pkg2")).unwrap();
    record_all(&repo, "move the package");

    assert_eq!(content(&repo, "pkg2/__init__.py"), Some(Vec::new()));
    assert_eq!(content(&repo, "pkg2/mod.py"), Some(b"x = 1\n".to_vec()));
    assert_eq!(content(&repo, "pkg/mod.py"), None);
    assert_eq!(
        dirty_paths(&repo),
        Vec::<String>::new(),
        "the move is fully recorded"
    );
}

#[test]
fn deleting_a_truncated_file_only_deletes_its_own_vertices() {
    let dir = TempDir::new().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    write(dir.path(), "go.sum", b"h1\nh2\nh3\nh4\n");
    write(dir.path(), "map.txt", b"l1\nl2\nl3\nl4\nl5\n");
    add(&repo, "go.sum");
    add(&repo, "map.txt");
    record_all(&repo, "base");

    // go.sum's zero-length vertex and map.txt's rewritten first line share
    // the change's first content position.
    write(dir.path(), "go.sum", b"");
    write(dir.path(), "map.txt", b"l1\nl4\nl5\n");
    record_all(&repo, "truncate beside a whole-file rewrite");
    assert_eq!(content(&repo, "map.txt"), Some(b"l1\nl4\nl5\n".to_vec()));

    fs::remove_file(dir.path().join("go.sum")).unwrap();
    write(dir.path(), "map.txt", b"l1\nl4\nl5\nl6\n");
    record_all(&repo, "delete go.sum, extend map.txt");
    assert_eq!(content(&repo, "go.sum"), None);
    assert_eq!(
        content(&repo, "map.txt"),
        Some(b"l1\nl4\nl5\nl6\n".to_vec())
    );
    assert_eq!(dirty_paths(&repo), Vec::<String>::new());
}
