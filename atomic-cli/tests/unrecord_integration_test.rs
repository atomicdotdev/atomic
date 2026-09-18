//! Exercise hash selection and safe unrecord through the real CLI.
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::Command;

use atomic_core::change::{Author, Change, ChangeHeader};
use atomic_core::types::{Base32, Hash};
use atomic_repository::history::HistoryOptions;
use atomic_repository::{RecordOptions, Repository};
use tempfile::TempDir;

fn run(root: &Path, args: &[&str], succeeds: bool) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_atomic"))
        .args(args)
        .current_dir(root)
        .output()
        .expect("run CLI");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.status.success(), succeeds, "{args:?}: {text}");
    text
}

fn record(repo: &Repository, message: &str) -> Hash {
    *repo
        .record(
            ChangeHeader::builder()
                .message(message)
                .author(Author::new("Test", Some("test@example.com")))
                .build(),
            RecordOptions::default(),
        )
        .unwrap()
        .hash()
}

fn fixture() -> (TempDir, Vec<Hash>) {
    let dir = TempDir::new().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let mut hashes = Vec::new();
    for name in ["a.txt", "b.txt", "c.txt"] {
        fs::write(dir.path().join(name), name).unwrap();
        repo.add(name, Default::default()).unwrap();
        hashes.push(record(&repo, name));
    }
    (dir, hashes)
}

fn history(repo: &Repository) -> Vec<Hash> {
    repo.log(HistoryOptions::default().include_inherited(true))
        .unwrap()
        .into_iter()
        .map(|entry| entry.hash)
        .collect()
}

#[test]
fn full_hash_removes_middle_change_preserving_files_store_and_other_view() {
    let (dir, hashes) = fixture();
    let sibling;
    {
        let mut repo = Repository::open(dir.path()).unwrap();
        let source = repo.current_view().to_string();
        repo.create_view_from("sibling", &source).unwrap();
        sibling = repo
            .log(
                HistoryOptions::default()
                    .view("sibling")
                    .include_inherited(true),
            )
            .unwrap();
    }
    fs::write(dir.path().join("b.txt"), "unrecorded local edits").unwrap();
    run(dir.path(), &["unrecord", &hashes[1].to_base32()], true);
    let repo = Repository::open(dir.path()).unwrap();
    assert_eq!(history(&repo), vec![hashes[0], hashes[2]]);
    assert!(repo.load_change(&hashes[1]).is_ok());
    assert!(repo
        .get_file_content("b.txt")
        .unwrap()
        .unwrap_or_default()
        .is_empty());
    assert_eq!(
        repo.get_file_content_on_view("b.txt", "sibling").unwrap(),
        Some(b"b.txt".to_vec())
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("b.txt")).unwrap(),
        "unrecorded local edits"
    );
    for name in ["a.txt", "c.txt"] {
        assert_eq!(fs::read_to_string(dir.path().join(name)).unwrap(), name);
    }
    let after = repo
        .log(
            HistoryOptions::default()
                .view("sibling")
                .include_inherited(true),
        )
        .unwrap();
    assert_eq!(
        after.iter().map(|e| e.hash).collect::<Vec<_>>(),
        sibling.iter().map(|e| e.hash).collect::<Vec<_>>()
    );
    // The retained change is usable, not just a leftover object on disk.
    repo.insert_change(&hashes[1], Default::default()).unwrap();
    assert_eq!(
        repo.get_file_content("b.txt").unwrap(),
        Some(b"b.txt".to_vec())
    );
}

#[test]
fn lowercase_unique_prefix_and_dry_run() {
    let (dir, hashes) = fixture();
    let prefix = hashes[1].to_base32()[..16].to_ascii_lowercase();
    let preview = run(dir.path(), &["unrecord", &prefix, "--dry-run"], true);
    assert!(preview.contains(&hashes[1].to_base32()));
    assert_eq!(history(&Repository::open(dir.path()).unwrap()), hashes);
    run(dir.path(), &["unrecord", &prefix], true);
    assert_eq!(
        history(&Repository::open(dir.path()).unwrap()),
        vec![hashes[0], hashes[2]]
    );
}

#[test]
fn no_argument_still_removes_last_change() {
    let (dir, hashes) = fixture();
    run(dir.path(), &["unrecord", "-n"], true);
    assert_eq!(history(&Repository::open(dir.path()).unwrap()), hashes);
    run(dir.path(), &["unrecord"], true);
    assert_eq!(history(&Repository::open(dir.path()).unwrap()), hashes[..2]);
}

#[test]
fn malformed_unknown_and_nonmember_hashes_do_not_mutate() {
    let (dir, hashes) = fixture();
    let absent = Hash::of(b"not in this repository").to_base32();
    for target in ["", "0123456789", "A/B", &"A".repeat(53), &absent] {
        run(dir.path(), &["unrecord", target], false);
        run(dir.path(), &["unrecord", target, "-n"], false);
        assert_eq!(history(&Repository::open(dir.path()).unwrap()), hashes);
    }
    // The object still exists in storage after removal, but is not a member.
    run(dir.path(), &["unrecord", &hashes[1].to_base32()], true);
    assert!(run(dir.path(), &["unrecord", &hashes[1].to_base32()], false).contains("not in view"));
    assert!(run(
        dir.path(),
        &["unrecord", &hashes[1].to_base32(), "-n"],
        false
    )
    .contains("not in view"));
    assert_eq!(
        history(&Repository::open(dir.path()).unwrap()),
        vec![hashes[0], hashes[2]]
    );
}

#[test]
fn ambiguous_prefix_is_rejected_without_mutation() {
    let (dir, hashes) = fixture();
    let prefix = {
        let repo = Repository::open(dir.path()).unwrap();
        let mut seen = HashMap::new();
        let mut collision = None;
        // 33 distinct hashes guarantee a collision in the first Base32 digit.
        for i in 0..33 {
            let hash = repo
                .save_change(&Change::empty(ChangeHeader::new(format!("stored {i}"))))
                .unwrap();
            let first = hash.to_base32()[..1].to_string();
            if seen.insert(first.clone(), hash).is_some() {
                collision = Some(first);
                break;
            }
        }
        collision.unwrap()
    };
    assert!(run(dir.path(), &["unrecord", &prefix], false)
        .to_lowercase()
        .contains("ambiguous"));
    assert!(run(dir.path(), &["unrecord", &prefix, "-n"], false)
        .to_lowercase()
        .contains("ambiguous"));
    assert_eq!(history(&Repository::open(dir.path()).unwrap()), hashes);
}

#[test]
fn dependent_change_blocks_both_preview_and_execution() {
    let (dir, mut hashes) = fixture();
    {
        let repo = Repository::open(dir.path()).unwrap();
        fs::write(dir.path().join("a.txt"), "updated\n").unwrap();
        let dependent = record(&repo, "edit a");
        assert!(repo
            .load_change(&dependent)
            .unwrap()
            .dependencies()
            .contains(&hashes[0]));
        hashes.push(dependent);
    }
    for dry in [false, true] {
        let hash = hashes[0].to_base32();
        let mut args = vec!["unrecord", &hash];
        if dry {
            args.push("-n");
        }
        assert!(run(dir.path(), &args, false).contains("depends on it"));
        assert_eq!(history(&Repository::open(dir.path()).unwrap()), hashes);
        assert_eq!(
            fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "updated\n"
        );
    }
    run(dir.path(), &["unrecord", &hashes[3].to_base32()], true);
    run(dir.path(), &["unrecord", &hashes[0].to_base32()], true);
    assert_eq!(
        history(&Repository::open(dir.path()).unwrap()),
        hashes[1..3]
    );
}

#[test]
fn inherited_changes_are_rejected_in_forked_view() {
    let (dir, hashes) = fixture();
    {
        let mut repo = Repository::open(dir.path()).unwrap();
        let source = repo.current_view().to_string();
        repo.create_view_from("child", &source).unwrap();
        repo.switch_view("child").unwrap();
    }
    for dry in [false, true] {
        let hash = hashes[2].to_base32();
        let mut args = vec!["unrecord", &hash];
        if dry {
            args.push("-n");
        }
        assert!(run(dir.path(), &args, false).contains("inherited"));
        assert_eq!(history(&Repository::open(dir.path()).unwrap()), hashes);
    }
}

#[test]
fn empty_view_reports_nothing_to_unrecord() {
    let dir = TempDir::new().unwrap();
    drop(Repository::init(dir.path()).unwrap());
    assert!(run(dir.path(), &["unrecord"], false).contains("nothing to unrecord"));
}
