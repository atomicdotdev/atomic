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

fn render_batch(repo: &TestRepository, paths: &[String], view: &str) -> Vec<Option<Vec<u8>>> {
    let mut rendered = Vec::new();
    repo.for_each_switch_target_file(paths, view, |path, bytes| {
        assert_eq!(path, paths[rendered.len()]);
        rendered.push(bytes);
        Ok(())
    })
    .unwrap();
    rendered
}

#[test]
fn switch_lease_batch_matches_filtered_bytes_and_does_not_reuse_other_views() {
    for spans in [false, true] {
        let (temp, mut repo) = create_temp_repo();
        // Retain a graph-origin file even in the span-capable fixture.
        std::fs::write(temp.path().join("legacy.txt"), b"legacy\n").unwrap();
        repo.add("legacy.txt", TrackingOptions::default()).unwrap();
        record_all(&repo, "graph origin");
        if spans {
            enable_spans(&repo);
        }
        std::fs::write(temp.path().join(".gitattributes"), "*.txt text eol=crlf\n").unwrap();
        for (path, bytes) in [
            ("a.txt", b"alpha\n".as_slice()),
            ("b.txt", b"beta\n"),
            ("empty.txt", b""),
        ] {
            std::fs::write(temp.path().join(path), bytes).unwrap();
            repo.add(path, TrackingOptions::default()).unwrap();
        }
        record_all(&repo, "new files");
        repo.create_view_from("lease-feature", "dev").unwrap();
        repo.switch_view("lease-feature").unwrap();
        std::fs::write(temp.path().join("a.txt"), b"changed\r\n").unwrap();
        record_all(&repo, "feature edit");
        let paths: Vec<String> = ["a.txt", "b.txt", "empty.txt", "legacy.txt", "not-present"]
            .map(str::to_string)
            .into();
        let expected = vec![
            Some(b"alpha\r\n".to_vec()),
            Some(b"beta\r\n".to_vec()),
            Some(vec![]),
            Some(b"legacy\r\n".to_vec()),
            None,
        ];
        let before = repo.working_copy_record(repo.working_copy()).unwrap();
        assert_eq!(render_batch(&repo, &paths, "dev"), expected);
        let mut feature = expected.clone();
        feature[0] = Some(b"changed\r\n".to_vec());
        assert_eq!(render_batch(&repo, &paths, "lease-feature"), feature);
        assert_eq!(render_batch(&repo, &paths, "dev"), expected);
        assert_eq!(
            repo.working_copy_record(repo.working_copy()).unwrap(),
            before
        );
        assert_eq!(
            std::fs::read(temp.path().join("a.txt")).unwrap(),
            b"changed\r\n"
        );

        let mut visited = Vec::new();
        let error = repo
            .for_each_switch_target_file(&paths, "dev", |path, _| {
                visited.push(path.to_owned());
                Err(RepositoryError::InvalidOperation {
                    message: "visitor refused".into(),
                })
            })
            .unwrap_err();
        assert!(error.to_string().contains("visitor refused"));
        assert_eq!(visited, ["a.txt"]);
    }
}

#[test]
fn switch_lease_batch_checks_later_span_indexes_even_after_context_is_built() {
    use atomic_core::pristine::span_patch_storage::SpanPatchReadTxn;
    use atomic_core::pristine::TreeTxnT;
    use redb::{MultimapTableDefinition, TableDefinition};

    for missing_root in [true, false] {
        let (temp, repo) = create_temp_repo();
        enable_spans(&repo);
        for name in ["a.txt", "b.txt"] {
            std::fs::write(temp.path().join(name), name.as_bytes()).unwrap();
            repo.add(name, TrackingOptions::default()).unwrap();
        }
        record_all(&repo, "span roots");
        std::fs::write(temp.path().join("b.txt"), b"edited b").unwrap();
        record_all(&repo, "span edit");
        let paths = ["a.txt".to_string(), "b.txt".to_string()];
        assert_eq!(render_batch(&repo, &paths, "dev").len(), 2);
        let inode = repo
            .pristine()
            .read_txn()
            .unwrap()
            .get_inode("b.txt")
            .unwrap()
            .unwrap();
        let edits = repo
            .pristine()
            .read_txn()
            .unwrap()
            .span_edits_for_inode(inode)
            .unwrap();
        assert!(!edits.is_empty());
        let wc = repo.working_copy();
        let before_record = repo.working_copy_record(wc).unwrap();
        let before_head = repo
            .operation_log(
                atomic_core::operation::OperationScope::WorkingCopy(wc),
                Some(1),
                false,
            )
            .unwrap()
            .head_state;
        drop(repo);
        let db = redb::Database::open(temp.path().join(".atomic/atomic.redb")).unwrap();
        let txn = db.begin_write().unwrap();
        if missing_root {
            let mut roots = txn
                .open_table(TableDefinition::<u64, &[u8; 24]>::new(
                    "span_patch_roots_v1",
                ))
                .unwrap();
            assert!(roots.remove(inode.get()).unwrap().is_some());
        } else {
            let mut rows = txn
                .open_multimap_table(MultimapTableDefinition::<&[u8; 32], &[u8; 40]>::new(
                    "span_patch_edits_v1",
                ))
                .unwrap();
            assert!(rows.remove(&edits[0].0, &edits[0].1).unwrap());
        }
        txn.commit().unwrap();
        drop(db);
        let repo = Repository::open(temp.path()).unwrap();
        let mut visited = Vec::new();
        let error = repo
            .for_each_switch_target_file(&paths, "dev", |path, bytes| {
                visited.push(path.to_string());
                assert_eq!(bytes.unwrap(), b"a.txt");
                Ok(())
            })
            .unwrap_err();
        let expected_error = if missing_root {
            "span root missing"
        } else {
            "span index differs"
        };
        assert!(error.to_string().contains(expected_error), "{error}");
        assert_eq!(visited, ["a.txt"]);
        assert_eq!(repo.working_copy_record(wc).unwrap(), before_record);
        assert_eq!(
            repo.operation_log(
                atomic_core::operation::OperationScope::WorkingCopy(wc),
                Some(1),
                false
            )
            .unwrap()
            .head_state,
            before_head
        );
        for name in ["a.txt", "b.txt"] {
            assert_eq!(
                std::fs::read(temp.path().join(name)).unwrap(),
                if name == "a.txt" {
                    name.as_bytes()
                } else {
                    b"edited b"
                }
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn switch_lease_batch_does_not_smudge_span_symlink_targets() {
    let (temp, repo) = create_temp_repo();
    enable_spans(&repo);
    std::fs::write(
        temp.path().join(".gitattributes"),
        "link ident text eol=crlf\n",
    )
    .unwrap();
    std::os::unix::fs::symlink("target\n$Id$", temp.path().join("link")).unwrap();
    repo.add("link", TrackingOptions::default()).unwrap();
    record_all(&repo, "span symlink");
    assert_eq!(
        render_batch(&repo, &["link".into()], "dev"),
        vec![Some(b"target\n$Id$".to_vec())]
    );
}
