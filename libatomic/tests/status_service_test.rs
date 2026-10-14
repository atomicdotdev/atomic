//! Status must preserve dev's caller options through the working-copy adapter.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use atomic_core::change::ChangeHeader;
use atomic_repository::{RecordOptions, Repository, TrackingOptions};
use libatomic::atomic::repository_query_service_server::RepositoryQueryService;
use libatomic::atomic::{FileStatus, StatusOptions, StatusRequest};
use libatomic::daemon::services::QueryImpl;
use libatomic::daemon::state::DaemonState;
use tempfile::TempDir;
use tonic::Request;

fn write(root: &Path, path: &str, content: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

#[tokio::test]
async fn caller_options_survive_the_working_copy_status_adapter() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().canonicalize().unwrap();
    {
        let repo = Repository::init(&root).unwrap();
        let working_copy = repo.require_working_copy_id().unwrap();
        for (path, contents) in [
            (".atomicignore", "node_modules/\ndist/\n"),
            ("tracked.txt", "before\n"),
            ("scope/tracked.txt", "before\n"),
        ] {
            write(&root, path, contents);
            repo.add(working_copy, path, TrackingOptions::default())
                .unwrap();
        }
        repo.record(
            working_copy,
            ChangeHeader::new("base"),
            RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true),
        )
        .unwrap();
    }
    for path in [
        "tracked.txt",
        "scope/tracked.txt",
        "outside.txt",
        "scope/untracked.txt",
        "node_modules/cache.js",
        "dist/bundle.js",
    ] {
        write(&root, path, "after\n");
    }

    let state = Arc::new(DaemonState::new());
    let reference = state.register(root.clone()).repository_ref();
    let query = QueryImpl { state };
    let cases = [
        (
            None,
            vec![
                "outside.txt",
                "scope/tracked.txt",
                "scope/untracked.txt",
                "tracked.txt",
            ],
        ),
        (
            Some(StatusOptions {
                include_untracked: Some(false),
                ..Default::default()
            }),
            vec!["scope/tracked.txt", "tracked.txt"],
        ),
        (
            Some(StatusOptions {
                path_filters: vec!["scope".into()],
                ..Default::default()
            }),
            vec!["scope/tracked.txt", "scope/untracked.txt"],
        ),
        (
            Some(StatusOptions {
                include_ignored: Some(true),
                hash_contents: Some(false),
                ..Default::default()
            }),
            vec![
                "dist/bundle.js",
                "node_modules/cache.js",
                "outside.txt",
                "scope/tracked.txt",
                "scope/untracked.txt",
                "tracked.txt",
            ],
        ),
    ];
    for (options, expected) in cases {
        let response = query
            .status(Request::new(StatusRequest {
                repository: Some(reference.clone()),
                options: options.clone(),
            }))
            .await
            .unwrap()
            .into_inner();
        let files: BTreeMap<_, _> = response
            .files
            .iter()
            .map(|entry| (entry.path.as_str(), entry))
            .collect();
        assert_eq!(files.keys().copied().collect::<Vec<_>>(), expected);
        for (path, entry) in &files {
            assert_eq!(
                entry.status,
                if path.ends_with("tracked.txt") && !path.ends_with("untracked.txt") {
                    FileStatus::Modified
                } else {
                    FileStatus::Untracked
                } as i32
            );
        }
        if options
            .as_ref()
            .is_some_and(|options| options.hash_contents == Some(false))
        {
            // Tracked-file verification may still hash for correctness. The
            // option suppresses optional hashes on untracked status entries.
            assert!(files
                .values()
                .filter(|entry| entry.status == FileStatus::Untracked as i32)
                .all(|entry| entry.current_hash.is_none()));
        } else if options.is_none() {
            assert!(files["outside.txt"].current_hash.is_some());
        }
    }
    assert_eq!(
        std::fs::read_to_string(root.join("tracked.txt")).unwrap(),
        "after\n"
    );
}
