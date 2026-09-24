//! `Repository::materialize_view_entries`: a view's tree, rendered in memory
//! for a remote sandbox — the same bytes a checkout writes, per view, and
//! nothing touched on disk.

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
        .record(header, RecordOptions::default())
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
    repo.add("README.md", Default::default()).unwrap();
    repo.add("src/main.rs", Default::default()).unwrap();
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
    repo.split_view(SplitOptions::new("older", vec![second]))
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
