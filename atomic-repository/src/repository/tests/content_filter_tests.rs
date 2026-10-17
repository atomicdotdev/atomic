use super::*;
use crate::record::{extract_move_evidence, LossNote, RecordOptions};

fn record_all(repo: &TestRepository, message: &str) -> crate::record::RecordOutcome {
    repo.record(
        ChangeHeader::new(message),
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true),
    )
    .unwrap()
}

#[test]
fn record_stores_repository_bytes_and_materialize_smudges_working_bytes() {
    let (temp, repo) = create_temp_repo();
    std::fs::write(
        temp.path().join(".gitattributes"),
        "filtered.txt text eol=crlf\n",
    )
    .unwrap();
    std::fs::write(temp.path().join("filtered.txt"), b"one\r\ntwo\r\n").unwrap();
    repo.add("filtered.txt", TrackingOptions::default())
        .unwrap();

    record_all(&repo, "record repository bytes");
    assert_eq!(
        repo.get_file_content("filtered.txt").unwrap(),
        Some(b"one\ntwo\n".to_vec())
    );

    std::fs::remove_file(temp.path().join("filtered.txt")).unwrap();
    repo.materialize_paths(std::collections::HashSet::from(
        ["filtered.txt".to_string()],
    ))
    .unwrap();
    assert_eq!(
        std::fs::read(temp.path().join("filtered.txt")).unwrap(),
        b"one\r\ntwo\r\n"
    );
}

#[test]
fn status_compares_cleaned_repository_bytes_after_fast_path_miss() {
    let (temp, repo) = create_temp_repo();
    std::fs::write(
        temp.path().join(".gitattributes"),
        "filtered.txt text eol=crlf\n",
    )
    .unwrap();
    let path = temp.path().join("filtered.txt");
    std::fs::write(&path, b"same\r\n").unwrap();
    repo.add("filtered.txt", TrackingOptions::default())
        .unwrap();
    record_all(&repo, "record filtered file");

    // Different physical bytes and size, identical clean repository bytes.
    std::fs::write(&path, b"same\n").unwrap();
    let status = repo.status(StatusOptions::default()).unwrap();
    assert!(
        status
            .entries()
            .iter()
            .all(|entry| entry.path() != std::path::Path::new("filtered.txt")),
        "{:#?}",
        status.entries()
    );
}

#[test]
fn rename_similarity_uses_cleaned_repository_bytes() {
    let (temp, repo) = create_temp_repo();
    std::fs::write(temp.path().join(".gitattributes"), "*.txt text eol=lf\n").unwrap();
    std::fs::write(temp.path().join("old.txt"), b"same\n").unwrap();
    repo.add("old.txt", TrackingOptions::default()).unwrap();
    record_all(&repo, "add old path");

    std::fs::remove_file(temp.path().join("old.txt")).unwrap();
    std::fs::write(temp.path().join("new.txt"), b"same\r\n").unwrap();
    let outcome = record_all(&repo, "rename with eol conversion");
    let evidence = extract_move_evidence(outcome.change()).unwrap().unwrap();
    assert!(evidence.probable_moves.iter().any(|moved| {
        moved.old_path == "old.txt"
            && moved.new_path == "new.txt"
            && moved.basis == crate::record::MoveBasis::ByteIdentity
    }));
}

#[cfg(unix)]
#[test]
fn switch_smudges_regular_files_but_preserves_symlink_target_bytes_and_kind() {
    let (temp, mut repo) = create_temp_repo();
    std::fs::write(
        temp.path().join(".gitattributes"),
        "filtered.txt text eol=crlf\nlink ident text eol=crlf\n",
    )
    .unwrap();
    std::fs::write(temp.path().join("seed"), b"seed").unwrap();
    repo.add("seed", TrackingOptions::default()).unwrap();
    record_all(&repo, "seed");
    repo.create_view_from("feature", "dev").unwrap();

    repo.switch_view("feature").unwrap();
    std::fs::write(temp.path().join("filtered.txt"), b"feature\r\n").unwrap();
    std::fs::write(temp.path().join("link"), b"temporary regular file").unwrap();
    repo.add("filtered.txt", TrackingOptions::default())
        .unwrap();
    repo.add("link", TrackingOptions::default()).unwrap();
    record_all(&repo, "feature filtered files");
    std::fs::remove_file(temp.path().join("link")).unwrap();
    std::os::unix::fs::symlink("target\n$Id$", temp.path().join("link")).unwrap();
    record_all(&repo, "feature symlink kind");

    repo.switch_view("dev").unwrap();
    assert!(!temp.path().join("filtered.txt").exists());
    assert!(!temp.path().join("link").exists());
    repo.switch_view("feature").unwrap();
    assert_eq!(
        std::fs::read(temp.path().join("filtered.txt")).unwrap(),
        b"feature\r\n"
    );
    assert!(std::fs::symlink_metadata(temp.path().join("link"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        std::fs::read_link(temp.path().join("link")).unwrap(),
        std::path::Path::new("target\n$Id$")
    );
}

#[test]
fn required_clean_failure_blocks_change_adoption() {
    let (temp, repo) = create_temp_repo();
    std::fs::write(
        temp.path().join(".gitattributes"),
        "blocked.dat filter=blocked\n",
    )
    .unwrap();
    std::fs::write(
        temp.path().join(".atomic/config.toml"),
        "[view]\ndefault = \"dev\"\n\n[filters.drivers.blocked]\nclean = \"exit 17\"\nrequired = true\n",
    )
    .unwrap();
    std::fs::write(temp.path().join("blocked.dat"), b"secret").unwrap();
    repo.add("blocked.dat", TrackingOptions::default()).unwrap();

    let error = repo
        .record(
            ChangeHeader::new("must fail"),
            RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true),
        )
        .unwrap_err();
    assert!(error.to_string().contains("required filter 'blocked'"));
    assert_eq!(repo.get_file_content("blocked.dat").unwrap(), None);
}

#[test]
fn git_tracked_large_binary_uses_opaque_exact_repository_bytes() {
    let (temp, repo) = create_temp_repo();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(temp.path())
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-q"]);
    let bytes = b"large\0binary\r\n";
    std::fs::write(temp.path().join("asset.bin"), bytes).unwrap();
    git(&["add", "asset.bin"]);
    repo.add("asset.bin", TrackingOptions::default()).unwrap();

    let outcome = repo
        .record(
            ChangeHeader::new("opaque asset"),
            RecordOptions::new()
                .with_all(true)
                .with_max_file_size(4)
                .with_skip_binary(true)
                .save_to_store(true)
                .apply_after_record(true),
        )
        .unwrap();

    assert!(outcome
        .change()
        .contents
        .windows(bytes.len())
        .any(|window| window == bytes));
    assert_eq!(
        repo.get_file_content("asset.bin").unwrap(),
        Some(bytes.to_vec())
    );
}

#[test]
fn explicitly_tracked_empty_directory_records_projection_loss() {
    let (temp, repo) = create_temp_repo();
    std::fs::create_dir(temp.path().join("empty")).unwrap();
    repo.add_directory("empty", TrackingOptions::default())
        .unwrap();

    let outcome = record_all(&repo, "track empty directory");
    let evidence = extract_move_evidence(outcome.change()).unwrap().unwrap();
    assert!(evidence
        .loss_notes
        .contains(&LossNote::empty_directory("empty")));

    std::fs::remove_dir(temp.path().join("empty")).unwrap();
    repo.materialize().unwrap();
    assert!(temp.path().join("empty").is_dir());
}
