//! Deleting part of a block of lines that an earlier change inserted together.
//!
//! A change that inserts several lines, followed by a change that deletes only
//! some of them, must leave the remaining lines in the recorded graph. Found
//! importing a real project whose history removed one of two import lines that
//! had been added in the same commit.

use std::fs;
use std::path::{Path, PathBuf};

use atomic_core::change::{Author, ChangeHeader};
use atomic_repository::{RecordOptions, Repository};
use tempfile::TempDir;

fn create_test_repo() -> (Repository, TempDir, PathBuf) {
    let temp = TempDir::new().expect("Failed to create temp dir");
    let repo_path = temp.path().to_path_buf();
    let repo = Repository::init(&repo_path).expect("Failed to init repository");
    (repo, temp, repo_path)
}

fn record_change(repo: &Repository, message: &str) {
    let header = ChangeHeader::builder()
        .message(message)
        .author(Author::new("Test", Some("test@example.com")))
        .build();
    repo.record(header, RecordOptions::default())
        .expect("Failed to record");
}

/// Record each version of `path` in turn, then read the file back from the
/// recorded graph.
fn record_versions(versions: &[&str]) -> String {
    let (repo, _temp, repo_path) = create_test_repo();
    let path = "f.txt";
    for (i, content) in versions.iter().enumerate() {
        fs::write(Path::new(&repo_path).join(path), content).expect("Failed to write file");
        if i == 0 {
            repo.add(path, Default::default()).expect("Failed to add");
        }
        record_change(&repo, &format!("v{i}"));
    }
    let content = repo
        .get_file_content_on_view(path, repo.current_view())
        .expect("Failed to get content")
        .expect("File not found in graph");
    String::from_utf8_lossy(&content).into_owned()
}

#[test]
fn delete_first_line_of_block_added_later() {
    assert_eq!(record_versions(&["c\n", "a\nb\nc\n", "b\nc\n"]), "b\nc\n");
}

#[test]
fn delete_second_line_of_block_added_later() {
    assert_eq!(record_versions(&["c\n", "a\nb\nc\n", "a\nc\n"]), "a\nc\n");
}

#[test]
fn delete_first_line_of_block_added_in_the_middle() {
    assert_eq!(
        record_versions(&["x\ny\n", "x\na\nb\ny\n", "x\nb\ny\n"]),
        "x\nb\ny\n"
    );
}

#[test]
fn delete_first_line_of_the_initial_block() {
    assert_eq!(record_versions(&["a\nb\nc\n", "b\nc\n"]), "b\nc\n");
}

#[test]
fn record_again_after_deleting_first_line_of_block() {
    // The next record diffs against the recorded content, so a wrong read
    // here would record a duplicate `b`.
    assert_eq!(
        record_versions(&["c\n", "a\nb\nc\n", "b\nc\n", "b\nc\nd\n"]),
        "b\nc\nd\n"
    );
}

#[test]
fn replace_a_line_in_place() {
    assert_eq!(
        record_versions(&["x\na\ny\n", "x\nA\ny\n", "x\nA\ny\nz\n"]),
        "x\nA\ny\nz\n"
    );
}
