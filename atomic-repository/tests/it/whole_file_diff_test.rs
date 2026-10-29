//! Whole-file storage rewrites must still expose the actual review delta.
use atomic_core::change::{Change, ChangeHeader};
use atomic_core::diff::display::LineStatus;
use atomic_core::types::Hash;
use atomic_repository::diff_engine::{
    change_file_diffs, whole_file_replacement_text, DiffFormat, DiffOutputConfig, FileChangeStatus,
};
use atomic_repository::{RecordOptions, Repository};
use tempfile::TempDir;

fn record(repo: &Repository) -> Hash {
    *repo
        .record(
            repo.require_working_copy_id().unwrap(),
            ChangeHeader::builder().message("test").build(),
            RecordOptions::default(),
        )
        .unwrap()
        .hash()
}

fn replacement(before: &[u8], after: &[u8]) -> (TempDir, Repository, Hash, Change) {
    let dir = TempDir::new().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    std::fs::write(dir.path().join("file.txt"), before).unwrap();
    repo.add(
        repo.require_working_copy_id().unwrap(),
        "file.txt",
        Default::default(),
    )
    .unwrap();
    record(&repo);
    std::fs::write(dir.path().join("file.txt"), after).unwrap();
    let hash = record(&repo);
    let change = repo.change_store().load_change(&hash).unwrap();
    assert_eq!(
        whole_file_replacement_text(&change.file_ops()[0], &change.contents, || Some(
            before.to_vec()
        )),
        Some((before.to_vec(), after.to_vec())),
        "fixture must exercise a whole-file rewrite"
    );
    (dir, repo, hash, change)
}

#[test]
fn replacement_diff_uses_historical_text_for_all_output_formats() {
    let before = b"keep\nconst A = 1;\nconst B = 2;\ntail\n";
    let after = b"keep\nconst A = 3;\nconst B = 4;\ntail\n";
    let (dir, repo, hash, change) = replacement(before, after);
    // Later edits and reopening must not substitute today's text for this diff.
    std::fs::write(dir.path().join("file.txt"), b"later\n").unwrap();
    record(&repo);
    drop(repo);
    let repo = Repository::open(dir.path()).unwrap();
    for format in [
        DiffFormat::Unified,
        DiffFormat::Stat,
        DiffFormat::NameStatus,
    ] {
        for context in [0, 3] {
            let config = DiffOutputConfig::new()
                .with_format(format)
                .with_context(context);
            let (files, _) = change_file_diffs(&repo, &change, &hash, &config).unwrap();
            assert_eq!(files.len(), 1);
            assert_eq!(files[0].status, FileChangeStatus::Modified);
            assert_eq!(files[0].stats.insertions, 2);
            assert_eq!(files[0].stats.deletions, 2);
            let changes: Vec<_> = files[0]
                .hunks
                .iter()
                .flat_map(|h| &h.lines)
                .filter(|l| l.status != LineStatus::Unchanged)
                .map(|l| (l.status, l.content.trim_end().to_owned()))
                .collect();
            assert_eq!(
                changes,
                vec![
                    (LineStatus::Removed, "const A = 1;".into()),
                    (LineStatus::Added, "const A = 3;".into()),
                    (LineStatus::Removed, "const B = 2;".into()),
                    (LineStatus::Added, "const B = 4;".into()),
                ]
            );
        }
    }
}

#[test]
fn replacement_deletion_does_not_show_survivors_as_added() {
    let (_dir, repo, hash, change) =
        replacement(b"keep\nold one\nold two\ntail\n", b"keep\ntail\n");
    let (files, _) = change_file_diffs(&repo, &change, &hash, &DiffOutputConfig::new()).unwrap();
    assert_eq!(files[0].stats.insertions, 0);
    assert_eq!(files[0].stats.deletions, 2);
    let removed: Vec<_> = files[0]
        .hunks
        .iter()
        .flat_map(|h| &h.lines)
        .filter(|l| l.status == LineStatus::Removed)
        .map(|l| l.content.trim_end())
        .collect();
    assert_eq!(removed, ["old one", "old two"]);
}

#[test]
fn replacement_truncation_keeps_modified_file_status() {
    let (_dir, repo, hash, change) = replacement(b"one\ntwo\n", b"");
    let (files, _) = change_file_diffs(
        &repo,
        &change,
        &hash,
        &DiffOutputConfig::new().with_context(0),
    )
    .unwrap();
    assert_eq!(files[0].status, FileChangeStatus::Modified);
    assert_eq!(files[0].stats.insertions, 0);
    assert_eq!(files[0].stats.deletions, 2);
    assert_eq!(repo.get_file_content("file.txt").unwrap(), Some(Vec::new()));
}

#[test]
fn replacement_diff_preserves_blank_lines_and_unterminated_last_line() {
    let (_dir, repo, hash, change) = replacement(b"head\n\nold\n", b"head\n\nnew");
    let (files, _) = change_file_diffs(&repo, &change, &hash, &DiffOutputConfig::new()).unwrap();
    assert_eq!(files[0].stats.insertions, 1);
    assert_eq!(files[0].stats.deletions, 1);
    assert_eq!(
        repo.get_file_content("file.txt").unwrap(),
        Some(b"head\n\nnew".to_vec())
    );
}

#[test]
fn deleting_one_blank_line_is_not_a_whole_file_rewrite() {
    let dir = TempDir::new().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let before = b"\nkeep\ntail\n";
    std::fs::write(dir.path().join("file.txt"), before).unwrap();
    repo.add(
        repo.require_working_copy_id().unwrap(),
        "file.txt",
        Default::default(),
    )
    .unwrap();
    record(&repo);
    std::fs::write(dir.path().join("file.txt"), b"keep\ntail\n").unwrap();
    let hash = record(&repo);
    let change = repo.change_store().load_change(&hash).unwrap();
    assert!(
        whole_file_replacement_text(&change.file_ops()[0], &change.contents, || Some(
            before.to_vec()
        ))
        .is_none()
    );
    let (files, _) = change_file_diffs(&repo, &change, &hash, &DiffOutputConfig::new()).unwrap();
    assert_eq!(files[0].stats.insertions, 0);
    assert_eq!(files[0].stats.deletions, 1);
}

#[test]
fn replacement_byte_ranges_preserve_crlf() {
    let after = b"head\r\n\r\nnew one\r\nnew two\r\n";
    let (_dir, repo, hash, change) = replacement(b"head\r\n\r\nold one\r\nold two\r\n", after);
    let (files, _) = change_file_diffs(&repo, &change, &hash, &DiffOutputConfig::new()).unwrap();
    assert_eq!(files[0].stats.insertions, 2);
    assert_eq!(files[0].stats.deletions, 2);
    assert_eq!(
        repo.get_file_content("file.txt").unwrap(),
        Some(after.to_vec())
    );
}
