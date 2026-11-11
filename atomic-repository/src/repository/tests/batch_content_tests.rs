use super::*;
use atomic_core::pristine::{CapabilityMutTxnT, MutTxnT, SPAN_RANGE_CAPABILITY};

fn record_all(repo: &TestRepository, message: &str) {
    repo.record(
        ChangeHeader::new(message),
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
            .sync_vault(false)
            .enrich_kg(false),
    )
    .unwrap();
}

fn enable_spans(repo: &TestRepository) {
    let mut txn = repo.pristine().write_txn().unwrap();
    txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
    txn.commit().unwrap();
}

fn write_and_add(temp: &tempfile::TempDir, repo: &mut TestRepository, path: &str, bytes: &[u8]) {
    let full = temp.path().join(path);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(full, bytes).unwrap();
    repo.add(path, TrackingOptions::default()).unwrap();
}

/// Batched reads must be byte-identical to the single-path reads they replace,
/// for graph-origin files, span-rooted files, empty files, directories and
/// absent paths, on every view they are asked about.
#[test]
fn batch_reads_and_presence_match_single_path_reads() {
    for spans in [false, true] {
        let (temp, mut repo) = create_temp_repo();
        write_and_add(&temp, &mut repo, "legacy.txt", b"legacy\n");
        record_all(&repo, "graph origin");
        if spans {
            enable_spans(&repo);
        }
        write_and_add(&temp, &mut repo, "a.txt", b"alpha\n");
        write_and_add(&temp, &mut repo, "b.txt", b"beta\n");
        write_and_add(&temp, &mut repo, "empty.txt", b"");
        write_and_add(&temp, &mut repo, "dir/c.txt", b"gamma\n");
        record_all(&repo, "new files");
        std::fs::write(temp.path().join("b.txt"), b"beta edited\n").unwrap();
        record_all(&repo, "edit b");

        repo.create_view_from("feature", "dev").unwrap();
        repo.switch_view("feature").unwrap();
        std::fs::write(temp.path().join("a.txt"), b"alpha on feature\n").unwrap();
        record_all(&repo, "feature edit");

        if spans {
            use atomic_core::pristine::span_patch_storage::SpanPatchReadTxn;
            use atomic_core::pristine::TreeTxnT;
            let txn = repo.pristine().read_txn().unwrap();
            let inode = txn.get_inode("a.txt").unwrap().unwrap();
            assert!(
                txn.span_root(inode).unwrap().is_some(),
                "fixture must record a.txt as a span-rooted file"
            );
            let legacy = txn.get_inode("legacy.txt").unwrap().unwrap();
            assert!(
                txn.span_root(legacy).unwrap().is_none(),
                "fixture must keep legacy.txt graph-origin"
            );
        }

        let paths: Vec<String> = [
            "a.txt",
            "b.txt",
            "empty.txt",
            "legacy.txt",
            "dir/c.txt",
            "dir",
            "missing.txt",
        ]
        .map(str::to_string)
        .into();
        for view in ["dev", "feature"] {
            let batch = repo.get_file_contents_on_view(&paths, view).unwrap();
            assert_eq!(batch.len(), paths.len());
            for (path, bytes) in paths.iter().zip(&batch) {
                assert_eq!(
                    bytes,
                    &repo.get_file_content_on_view(path, view).unwrap(),
                    "spans={spans} view={view} path={path}"
                );
            }
            let expected_a: &[u8] = if view == "feature" {
                b"alpha on feature\n"
            } else {
                b"alpha\n"
            };
            assert_eq!(batch[0].as_deref(), Some(expected_a));
            assert_eq!(batch[1].as_deref(), Some(b"beta edited\n".as_slice()));
            assert_eq!(batch[2].as_deref(), Some(b"".as_slice()));
            assert_eq!(batch[3].as_deref(), Some(b"legacy\n".as_slice()));
            assert_eq!(batch[4].as_deref(), Some(b"gamma\n".as_slice()));
            assert_eq!(batch[5], None, "directories have no content");
            assert_eq!(batch[6], None, "absent paths have no content");

            let presence = repo.recorded_presence_on_view(&paths, view).unwrap();
            let expected: Vec<RecordedPresence> = batch
                .iter()
                .map(|bytes| {
                    if bytes.is_some() {
                        RecordedPresence::Present
                    } else {
                        RecordedPresence::Absent
                    }
                })
                .collect();
            assert_eq!(presence, expected, "spans={spans} view={view}");
        }
    }
}

/// An unknown view fails the batch exactly like the single-path read.
#[test]
fn batch_reads_reject_unknown_views() {
    let (temp, mut repo) = create_temp_repo();
    write_and_add(&temp, &mut repo, "a.txt", b"alpha\n");
    record_all(&repo, "one file");
    let paths = vec!["a.txt".to_string()];
    assert!(matches!(
        repo.get_file_contents_on_view(&paths, "nope"),
        Err(RepositoryError::ViewNotFound { .. })
    ));
    assert!(matches!(
        repo.recorded_presence_on_view(&paths, "nope"),
        Err(RepositoryError::ViewNotFound { .. })
    ));
    assert_eq!(repo.get_file_contents_on_view(&[], "dev").unwrap(), vec![]);
}

/// The streaming read visits every path in the requested order with the same
/// bytes the collected read returns, and a visitor error stops the walk and
/// comes back unchanged.
#[test]
fn streaming_reads_visit_in_order_and_propagate_visitor_errors() {
    for spans in [false, true] {
        let (temp, mut repo) = create_temp_repo();
        if spans {
            enable_spans(&repo);
        }
        write_and_add(&temp, &mut repo, "b.txt", b"beta\n");
        write_and_add(&temp, &mut repo, "a.txt", b"alpha\n");
        write_and_add(&temp, &mut repo, "dir/c.txt", b"gamma\n");
        record_all(&repo, "files");
        let paths: Vec<String> = ["dir/c.txt", "missing.txt", "a.txt", "dir", "b.txt"]
            .map(str::to_string)
            .into();
        let collected = repo.get_file_contents_on_view(&paths, "dev").unwrap();
        let mut visited = Vec::new();
        repo.for_each_file_content_on_view(&paths, "dev", |path, bytes| {
            visited.push((path.to_string(), bytes));
            Ok(())
        })
        .unwrap();
        let visited_paths: Vec<&str> = visited.iter().map(|(path, _)| path.as_str()).collect();
        assert_eq!(
            visited_paths,
            paths.iter().map(String::as_str).collect::<Vec<_>>()
        );
        let visited_bytes: Vec<Option<Vec<u8>>> =
            visited.into_iter().map(|(_, bytes)| bytes).collect();
        assert_eq!(visited_bytes, collected, "spans={spans}");

        let mut seen = Vec::new();
        let error = repo
            .for_each_file_content_on_view(&paths, "dev", |path, _| {
                seen.push(path.to_string());
                if path == "a.txt" {
                    return Err(RepositoryError::InvalidOperation {
                        message: "visitor refused".into(),
                    });
                }
                Ok(())
            })
            .unwrap_err();
        assert!(error.to_string().contains("visitor refused"), "{error}");
        assert_eq!(seen, ["dir/c.txt", "missing.txt", "a.txt"], "spans={spans}");
    }
}
