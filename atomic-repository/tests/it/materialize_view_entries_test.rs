//! `Repository::materialize_view_entries`: a view's tree, rendered in memory
//! for a client — the same bytes a checkout writes, per view, and nothing
//! touched on disk.

use std::fs;
use std::path::Path;

use atomic_core::change::{Author, ChangeHeader};
use atomic_core::types::Hash;
use atomic_repository::{RecordOptions, Repository, SplitOptions, ViewEntry, ViewEntryKind};
use tempfile::TempDir;

fn write(repo_path: &Path, name: &str, content: &str) {
    let path = repo_path.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn record(repo: &Repository, message: &str) -> Hash {
    let header = ChangeHeader::builder()
        .message(message)
        .author(Author::new("Test", Some("test@example.com")))
        .build();
    *repo
        .record(
            repo.require_working_copy_id().unwrap(),
            header,
            RecordOptions::default(),
        )
        .expect("record")
        .hash()
}

fn entries(repo: &Repository, view: &str) -> (Vec<ViewEntry>, atomic_repository::ViewSnapshot) {
    let mut out = Vec::new();
    let snapshot = repo
        .materialize_view_entries::<()>(view, |e| {
            out.push(e);
            Ok(())
        })
        .expect("materialize")
        .expect("sink");
    (out, snapshot)
}

#[test]
fn entries_are_the_view_s_recorded_tree() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().to_path_buf();
    let repo = Repository::init(&root).expect("init");
    let view = repo.current_view().to_string();

    write(&root, "README.md", "hello\n");
    write(&root, "src/main.rs", "fn main() {}\n");
    repo.add(
        repo.require_working_copy_id().unwrap(),
        "README.md",
        Default::default(),
    )
    .unwrap();
    repo.add(
        repo.require_working_copy_id().unwrap(),
        "src/main.rs",
        Default::default(),
    )
    .unwrap();
    record(&repo, "first");
    write(&root, "README.md", "hello, world\n");
    let second = record(&repo, "second");
    // Present on disk, never recorded: not part of the view.
    write(&root, "scratch.txt", "not recorded\n");

    let before: Vec<_> = fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    let (list, snapshot) = entries(&repo, &view);
    let after: Vec<_> = fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(
        before.len(),
        after.len(),
        "nothing written to the working tree"
    );

    let paths: Vec<&str> = list.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(
        paths,
        vec!["src", "README.md", "src/main.rs"],
        "directories first, then files in order"
    );
    let directory = list.iter().find(|entry| entry.path == "src").unwrap();
    assert_eq!(directory.kind, ViewEntryKind::Directory);
    assert_eq!(
        directory.mode, 0o755,
        "clients need traversable directory permissions"
    );
    let readme = list.iter().find(|e| e.path == "README.md").unwrap();
    assert_eq!(readme.kind, ViewEntryKind::File);
    assert_eq!(readme.content, b"hello, world\n");
    assert_eq!(readme.hash, Hash::of(b"hello, world\n"));
    assert_eq!(readme.conflict_marker_line, None);
    assert!(readme.inode > 0);
    assert_eq!(snapshot.view, view);
    assert_eq!(snapshot.change_count, 2);
    assert!(!snapshot.state.is_empty());

    // Another view without the second change renders its own content.
    let mut repo = repo;
    repo.split_view(
        repo.require_working_copy_id().unwrap(),
        SplitOptions::new("older", vec![second]),
    )
    .expect("split");
    let (older_on_source, older_snapshot) = entries(&repo, &view);
    let readme = older_on_source
        .iter()
        .find(|e| e.path == "README.md")
        .unwrap();
    assert_eq!(
        readme.content, b"hello\n",
        "the source view no longer has the second change"
    );
    assert_ne!(older_snapshot.state, snapshot.state);
}

/// Canonical projection renders either closure through a readonly handle;
/// it must never return the currently selected TREE under another view's name.
#[test]
fn a_read_only_repository_renders_each_views_canonical_tree() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().to_path_buf();
    let mut repo = Repository::init(&root).expect("init");
    let current = repo.current_view().to_string();

    write(&root, "README.md", "hello\n");
    repo.add(
        repo.require_working_copy_id().unwrap(),
        "README.md",
        Default::default(),
    )
    .unwrap();
    record(&repo, "first");
    write(&root, "README.md", "hello, world\n");
    let second = record(&repo, "second");

    repo.split_view(
        repo.require_working_copy_id().unwrap(),
        SplitOptions::new("older", vec![second]),
    )
    .expect("split");

    // Writable: both views render, and they differ. `split_view` moves the
    // change out of the source view, so the new view is the one with it.
    let (on_current, _) = entries(&repo, &current);
    let (on_older, _) = entries(&repo, "older");
    let body = |v: &Vec<ViewEntry>| {
        v.iter()
            .find(|e| e.path == "README.md")
            .map(|e| e.content.clone())
            .unwrap()
    };
    assert_eq!(body(&on_current), b"hello\n");
    assert_eq!(body(&on_older), b"hello, world\n");

    // The canonical graph can project either closure through a readonly handle;
    // rendering must not depend on TREE's currently materialized view.
    drop(repo);
    let readonly = Repository::open_readonly(&root).expect("open read-only");

    let mut on_current = Vec::new();
    readonly
        .materialize_view_entries::<()>(&current, |e| {
            on_current.push(e);
            Ok(())
        })
        .expect("materialize the current view")
        .expect("sink");
    assert_eq!(body(&on_current), b"hello\n");

    let mut on_older = Vec::new();
    readonly
        .materialize_view_entries::<()>("older", |e| {
            on_older.push(e);
            Ok(())
        })
        .expect("readonly canonical projection")
        .expect("sink");
    assert_eq!(body(&on_older), b"hello, world\n");
}
