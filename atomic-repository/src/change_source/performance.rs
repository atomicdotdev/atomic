use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use atomic_core::change::InodeKind;
use atomic_core::pristine::{FileIndexTimestamp, FileIndexV2Entry};
use atomic_core::types::Hash;

use super::*;
use crate::content_filter::{ContentFilter, ContentFilterError, FilteredContent};
use crate::repository::RepoPath;

const PATHS: usize = 100_000;
const CONTENT: &[u8] = b"canonical";

struct ExactFilter;

impl ContentFilter for ExactFilter {
    fn clean(&self, _path: &Path, bytes: &[u8]) -> Result<FilteredContent, ContentFilterError> {
        Ok(FilteredContent {
            bytes: bytes.to_vec(),
            warnings: Vec::new(),
        })
    }

    fn smudge(&self, _path: &Path, bytes: &[u8]) -> Result<FilteredContent, ContentFilterError> {
        Ok(FilteredContent {
            bytes: bytes.to_vec(),
            warnings: Vec::new(),
        })
    }
}

#[derive(Clone, Copy)]
struct SyntheticFilesystem {
    metadata_changed: Option<usize>,
}

impl SyntheticFilesystem {
    fn index(path: &RepoPath) -> usize {
        let bytes = path.as_bytes();
        let digits = &bytes[5..bytes.len() - 4];
        std::str::from_utf8(digits).unwrap().parse().unwrap()
    }

    fn observation(&self, path: &RepoPath) -> ObservedFile {
        let index = Self::index(path);
        let changed = self.metadata_changed == Some(index);
        ObservedFile {
            native_path: path.to_native().unwrap(),
            device: Some(7),
            inode: Some(index as u64 + 1),
            mtime: Some(FileIndexTimestamp {
                seconds: if changed { 2 } else { 1 },
                nanoseconds: 0,
            }),
            ctime: Some(FileIndexTimestamp {
                seconds: if changed { 2 } else { 1 },
                nanoseconds: 0,
            }),
            size: CONTENT.len() as u64,
            mode: Some(0o644),
            kind: ObservedKind::Regular,
        }
    }
}

impl FilesystemProvider for SyntheticFilesystem {
    fn observe(
        &self,
        _root: &Path,
        path: &RepoPath,
        reason: ChangeCandidateReason,
    ) -> Result<ChangeCandidate, ChangeSourceError> {
        Ok(ChangeCandidate {
            path: path.clone(),
            reason,
            observation: CandidateObservation::Present(self.observation(path)),
        })
    }

    fn canonical_content_id(
        &self,
        _file: &ObservedFile,
        _path: &RepoPath,
        _filter: &dyn ContentFilter,
    ) -> Result<Hash, ChangeSourceError> {
        Ok(Hash::of(CONTENT))
    }
}

fn fixture() -> (
    Vec<RepoPath>,
    Vec<CanonicalTrackedPath>,
    BTreeMap<RepoPath, FileIndexV2Entry>,
    Hash,
    FileIndexTimestamp,
) {
    let policy = Hash::of(b"100k-policy");
    let boundary = FileIndexTimestamp {
        seconds: 3,
        nanoseconds: 0,
    };
    let racy_after = FileIndexTimestamp {
        seconds: 2,
        nanoseconds: 0,
    };
    let content = Hash::of(CONTENT);
    let mut paths = Vec::with_capacity(PATHS);
    let mut tracked = Vec::with_capacity(PATHS);
    let mut index = BTreeMap::new();
    for number in 0..PATHS {
        let path = RepoPath::from_bytes(format!("file-{number:06}.txt").as_bytes()).unwrap();
        tracked.push(CanonicalTrackedPath {
            path: path.clone(),
            canonical_mode: 0o644,
            canonical_kind: InodeKind::Regular,
        });
        index.insert(
            path.clone(),
            FileIndexV2Entry::complete(
                7,
                number as u64 + 1,
                FileIndexTimestamp {
                    seconds: 1,
                    nanoseconds: 0,
                },
                FileIndexTimestamp {
                    seconds: 1,
                    nanoseconds: 0,
                },
                CONTENT.len() as u64,
                content,
                0o644,
                InodeKind::Regular,
                racy_after,
                policy,
            )
            .unwrap(),
        );
        paths.push(path);
    }
    (paths, tracked, index, policy, boundary)
}

fn incremental_source(
    kind: ChangeSourceKind,
    provider: &SyntheticFilesystem,
    path: &RepoPath,
) -> ChangeSourceResult {
    let candidate = provider
        .observe(
            Path::new("/synthetic"),
            path,
            ChangeCandidateReason::Explicit,
        )
        .unwrap();
    ChangeSourceResult {
        root: PathBuf::from("/synthetic"),
        source: kind,
        fallback_source: None,
        token: ChangeSourceToken(match kind {
            ChangeSourceKind::Fsmonitor => b"git-fsmonitor-v1:synthetic".to_vec(),
            ChangeSourceKind::Watchman => b"watchman-clock-v1:synthetic".to_vec(),
            _ => Vec::new(),
        }),
        candidates: vec![candidate],
        complete: false,
        fallback: None,
        stats: ChangeSourceStats {
            tracked_paths: PATHS,
            candidates: 1,
            metadata_reads: 1,
            ..ChangeSourceStats::default()
        },
    }
}

#[test]
fn release_100k_canonical_change_source_gates() {
    let (paths, tracked, warm_index, policy, boundary) = fixture();
    let provider = SyntheticFilesystem {
        metadata_changed: None,
    };
    let content = Hash::of(CONTENT);

    let cold_started = Instant::now();
    let cold_source = ScanChangeSource
        .changes_with_provider(
            ChangeSourceRequest {
                root: Path::new("/synthetic"),
                tracked_paths: &paths,
                previous_token: None,
            },
            &provider,
        )
        .unwrap();
    let cold = verify_candidates_with_provider(
        cold_source,
        &tracked,
        &BTreeMap::new(),
        policy,
        boundary,
        &ExactFilter,
        &provider,
        |_| Ok(content),
    )
    .unwrap();
    let cold_elapsed = cold_started.elapsed();
    assert_eq!(cold.metrics.content_hashes, PATHS);
    assert_eq!(cold.metrics.metadata_reads, PATHS);

    let warm_started = Instant::now();
    let warm_source = ScanChangeSource
        .changes_with_provider(
            ChangeSourceRequest {
                root: Path::new("/synthetic"),
                tracked_paths: &paths,
                previous_token: None,
            },
            &provider,
        )
        .unwrap();
    let warm = verify_candidates_with_provider(
        warm_source,
        &tracked,
        &warm_index,
        policy,
        boundary,
        &ExactFilter,
        &provider,
        |_| Ok(content),
    )
    .unwrap();
    let warm_elapsed = warm_started.elapsed();
    assert_eq!(warm.root, cold.root);
    assert_eq!(warm.metrics.valid_lease_hits, PATHS);
    assert_eq!(warm.metrics.content_hashes, 0);

    let mut incremental_times = Vec::new();
    for kind in [ChangeSourceKind::Fsmonitor, ChangeSourceKind::Watchman] {
        let started = Instant::now();
        let incremental = verify_candidates_with_provider(
            incremental_source(kind, &provider, &paths[PATHS / 2]),
            &tracked,
            &warm_index,
            policy,
            boundary,
            &ExactFilter,
            &provider,
            |_| Ok(content),
        )
        .unwrap();
        let elapsed = started.elapsed();
        assert_eq!(incremental.root, warm.root);
        assert_eq!(incremental.metrics.candidate_count, 1);
        assert_eq!(incremental.metrics.content_hashes, 0);
        assert_eq!(incremental.metrics.valid_lease_hits, PATHS);
        incremental_times.push((kind, elapsed));
    }

    let dropped_provider = SyntheticFilesystem {
        metadata_changed: Some(PATHS - 1),
    };
    let dropped = verify_candidates_with_provider(
        ChangeSourceResult {
            candidates: Vec::new(),
            stats: ChangeSourceStats {
                tracked_paths: PATHS,
                ..ChangeSourceStats::default()
            },
            ..incremental_source(ChangeSourceKind::Fsmonitor, &dropped_provider, &paths[0])
        },
        &tracked,
        &warm_index,
        policy,
        boundary,
        &ExactFilter,
        &dropped_provider,
        |_| Ok(content),
    )
    .unwrap();
    assert_eq!(dropped.root, warm.root);
    assert_eq!(dropped.metrics.fallback_count, 1);
    assert_eq!(
        dropped.metrics.fallback_source,
        Some(ChangeSourceKind::Fsmonitor)
    );

    let overflow_source = ScanChangeSource
        .changes_with_provider(
            ChangeSourceRequest {
                root: Path::new("/synthetic"),
                tracked_paths: &paths,
                previous_token: None,
            },
            &provider,
        )
        .unwrap();
    let overflow = verify_candidates_with_provider(
        ChangeSourceResult {
            fallback_source: Some(ChangeSourceKind::Watchman),
            fallback: Some(ChangeSourceFallbackReason::Overflow),
            ..overflow_source
        },
        &tracked,
        &warm_index,
        policy,
        boundary,
        &ExactFilter,
        &provider,
        |_| Ok(content),
    )
    .unwrap();
    assert_eq!(overflow.root, warm.root);
    assert_eq!(overflow.metrics.fallback_count, 1);
    assert_eq!(
        overflow.metrics.fallback_reason,
        Some(ChangeSourceFallbackReason::Overflow)
    );

    eprintln!(
        "CB-4C 100k: cold={cold_elapsed:?}, warm={warm_elapsed:?}, fsmonitor={:?}, watchman={:?}",
        incremental_times[0].1, incremental_times[1].1
    );
    if !cfg!(debug_assertions) {
        assert!(
            cold_elapsed < Duration::from_secs(5),
            "100k cold scan took {cold_elapsed:?}"
        );
        assert!(
            warm_elapsed < Duration::from_millis(500),
            "100k warm scan took {warm_elapsed:?}"
        );
        for (kind, elapsed) in incremental_times {
            assert!(
                elapsed < Duration::from_millis(100),
                "100k {kind:?} one-candidate scan took {elapsed:?}"
            );
        }
    }
}
