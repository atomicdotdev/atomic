use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use atomic_core::change::InodeKind;
use atomic_core::pristine::{FileIndexTimestamp, FileIndexV2Entry};
use atomic_core::types::Hash;

use crate::content_filter::ContentFilter;
use crate::repository::RepoPath;

use super::core::*;

/// Injectable metadata/content boundary used by the production verifier and
/// deterministic large-repository performance tests.
pub trait FilesystemProvider: Send + Sync {
    fn observe(
        &self,
        root: &Path,
        path: &RepoPath,
        reason: ChangeCandidateReason,
    ) -> Result<ChangeCandidate, ChangeSourceError>;

    fn canonical_content_id(
        &self,
        file: &ObservedFile,
        path: &RepoPath,
        filter: &dyn ContentFilter,
    ) -> Result<Hash, ChangeSourceError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RealFilesystemProvider;

impl FilesystemProvider for RealFilesystemProvider {
    fn observe(
        &self,
        root: &Path,
        path: &RepoPath,
        reason: ChangeCandidateReason,
    ) -> Result<ChangeCandidate, ChangeSourceError> {
        observe_candidate(root, path.clone(), reason)
    }

    fn canonical_content_id(
        &self,
        file: &ObservedFile,
        path: &RepoPath,
        filter: &dyn ContentFilter,
    ) -> Result<Hash, ChangeSourceError> {
        read_canonical_content(file, path, filter)
    }
}

/// Default source: stat every tracked path and return a complete candidate set.
#[derive(Clone, Copy, Debug, Default)]
pub struct ScanChangeSource;

impl ScanChangeSource {
    pub fn changes_with_provider(
        &self,
        request: ChangeSourceRequest<'_>,
        provider: &dyn FilesystemProvider,
    ) -> Result<ChangeSourceResult, ChangeSourceError> {
        let mut candidates = Vec::with_capacity(request.tracked_paths.len());
        let mut stats = ChangeSourceStats {
            tracked_paths: request.tracked_paths.len(),
            ..ChangeSourceStats::default()
        };
        for path in request.tracked_paths {
            stats.metadata_reads += 1;
            candidates.push(provider.observe(
                request.root,
                path,
                ChangeCandidateReason::FullScan,
            )?);
        }
        candidates.sort_by(|left, right| left.path.cmp(&right.path));
        stats.candidates = candidates.len();
        Ok(ChangeSourceResult {
            root: request.root.to_path_buf(),
            source: ChangeSourceKind::Scan,
            fallback_source: None,
            token: scan_token(&candidates),
            candidates,
            complete: true,
            fallback: None,
            stats,
        })
    }
}

impl ChangeSource for ScanChangeSource {
    fn changes(
        &self,
        request: ChangeSourceRequest<'_>,
    ) -> Result<ChangeSourceResult, ChangeSourceError> {
        self.changes_with_provider(request, &RealFilesystemProvider)
    }
}

pub(crate) fn observe_candidate(
    root: &Path,
    path: RepoPath,
    reason: ChangeCandidateReason,
) -> Result<ChangeCandidate, ChangeSourceError> {
    let native = path
        .to_native()
        .map_err(|error| ChangeSourceError::Path(error.to_string()))?;
    let native_path = root.join(native);
    let observation = match fs::symlink_metadata(&native_path) {
        Ok(metadata) => CandidateObservation::Present(observe_metadata(native_path, &metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => CandidateObservation::Missing,
        Err(error) => {
            return Err(ChangeSourceError::Io {
                path: path.escaped(),
                message: error.to_string(),
            })
        }
    };
    Ok(ChangeCandidate {
        path,
        reason,
        observation,
    })
}

/// Reverify source candidates and produce a deterministic complete result.
///
/// Incremental-source omissions are accepted only when a complete, non-racy,
/// policy- and graph-bound V2 lease exists. The scan source is complete and
/// therefore never relies on omission semantics.
pub fn verify_candidates<F>(
    source: ChangeSourceResult,
    tracked: &[CanonicalTrackedPath],
    index: &BTreeMap<RepoPath, FileIndexV2Entry>,
    conversion_policy: Hash,
    index_write_time: FileIndexTimestamp,
    filter: &dyn ContentFilter,
    canonical_content: F,
) -> Result<VerifiedCandidateResult, ChangeSourceError>
where
    F: FnMut(&RepoPath) -> Result<Hash, ChangeSourceError>,
{
    verify_candidates_with_provider(
        source,
        tracked,
        index,
        conversion_policy,
        index_write_time,
        filter,
        &RealFilesystemProvider,
        canonical_content,
    )
}

pub fn verify_candidates_with_provider<F>(
    source: ChangeSourceResult,
    tracked: &[CanonicalTrackedPath],
    index: &BTreeMap<RepoPath, FileIndexV2Entry>,
    conversion_policy: Hash,
    index_write_time: FileIndexTimestamp,
    filter: &dyn ContentFilter,
    provider: &dyn FilesystemProvider,
    mut canonical_content: F,
) -> Result<VerifiedCandidateResult, ChangeSourceError>
where
    F: FnMut(&RepoPath) -> Result<Hash, ChangeSourceError>,
{
    let expected: BTreeMap<_, _> = tracked
        .iter()
        .map(|entry| (entry.path.clone(), entry))
        .collect();
    let mut source = source;
    let original_source = source.source;
    let mut fallback = source.fallback.clone();
    let mut force_full = fallback.is_some();

    for entry in tracked {
        match index.get(&entry.path) {
            None => {
                force_full = true;
                fallback.get_or_insert(ChangeSourceFallbackReason::MissingIndex);
            }
            Some(row) if row.conversion_policy != Some(conversion_policy) => {
                force_full = true;
                fallback.get_or_insert(ChangeSourceFallbackReason::ConversionPolicyChanged);
            }
            Some(row)
                if !row.is_complete()
                    || row.canonical_mode != Some(entry.canonical_mode)
                    || row.canonical_kind != Some(entry.canonical_kind) =>
            {
                force_full = true;
                fallback.get_or_insert(ChangeSourceFallbackReason::CanonicalAttributesChanged);
            }
            _ => {}
        }
    }

    if !source.complete {
        let candidate_paths: BTreeSet<_> = source
            .candidates
            .iter()
            .map(|candidate| candidate.path.clone())
            .collect();
        for entry in tracked {
            if candidate_paths.contains(&entry.path) {
                continue;
            }
            source.stats.metadata_reads += 1;
            let lease_valid = index.get(&entry.path).is_some_and(|row| {
                omission_lease_matches(
                    provider,
                    &source.root,
                    entry,
                    row,
                    conversion_policy,
                    index_write_time,
                )
                .unwrap_or(false)
            });
            if !lease_valid {
                force_full = true;
                fallback.get_or_insert_with(|| {
                    ChangeSourceFallbackReason::SourceError(format!(
                        "omission lease invalid for {}",
                        entry.path.escaped()
                    ))
                });
            }
        }
        if force_full {
            let paths: Vec<_> = tracked.iter().map(|entry| entry.path.clone()).collect();
            source = ScanChangeSource.changes_with_provider(
                ChangeSourceRequest {
                    root: &source.root,
                    tracked_paths: &paths,
                    previous_token: None,
                },
                provider,
            )?;
            source.fallback_source = Some(original_source);
            source.fallback = fallback.clone();
        }
    }

    let observed: BTreeMap<_, _> = source
        .candidates
        .iter()
        .map(|candidate| (candidate.path.clone(), candidate))
        .collect();
    let mut stats = source.stats;
    let mut verified = Vec::with_capacity(tracked.len());
    let mut index_updates = Vec::new();
    let mut index_deletions = Vec::new();
    let mut root_rows = Vec::with_capacity(tracked.len());

    for entry in tracked {
        let candidate = observed.get(&entry.path).copied();
        if candidate.is_none() && (source.complete || force_full) {
            return Err(ChangeSourceError::InvalidIndex {
                path: entry.path.escaped(),
                message: "complete scan omitted a tracked path".into(),
            });
        }
        let cached = index.get(&entry.path);
        let observation = candidate.map(|candidate| &candidate.observation);
        let lease_can_cover_omission = candidate.is_none()
            && cached.is_some_and(|row| {
                row.is_complete()
                    && !row.is_racy(index_write_time)
                    && row.conversion_policy == Some(conversion_policy)
                    && row.canonical_mode == Some(entry.canonical_mode)
                    && row.canonical_kind == Some(entry.canonical_kind)
            });
        if lease_can_cover_omission {
            let row = cached.expect("checked above");
            stats.cache_leases += 1;
            let content_id = row.content_id;
            verified.push(VerifiedCandidate {
                path: entry.path.clone(),
                change: VerifiedChange::Unchanged,
                content_id,
                identity_replaced: false,
                rehashed: false,
            });
            root_rows.push(root_row(entry, VerifiedChange::Unchanged, content_id));
            continue;
        }

        let Some(observation) = observation else {
            return Err(ChangeSourceError::InvalidIndex {
                path: entry.path.escaped(),
                message: "incremental source omission has no valid cache lease".into(),
            });
        };
        match observation {
            CandidateObservation::Missing => {
                index_deletions.push(entry.path.clone());
                verified.push(VerifiedCandidate {
                    path: entry.path.clone(),
                    change: VerifiedChange::Deleted,
                    content_id: None,
                    identity_replaced: false,
                    rehashed: false,
                });
                root_rows.push(root_row(entry, VerifiedChange::Deleted, None));
            }
            CandidateObservation::Present(file) => {
                let observed_kind = canonical_kind(file.kind);
                let type_changed = observed_kind != Some(entry.canonical_kind);
                let permissions_changed = file.mode != Some(entry.canonical_mode);
                let observed_row = cached.and_then(|row| {
                    observed_index_row(
                        file,
                        row.content_id?,
                        entry.canonical_mode,
                        entry.canonical_kind,
                        row.racy_after?,
                        conversion_policy,
                    )
                    .ok()
                });
                let identity_replaced =
                    cached.is_some_and(|row| row.device != file.device || row.inode != file.inode);
                let racy = cached.is_some_and(|row| row.is_racy(index_write_time));
                if racy {
                    stats.racy_candidates += 1;
                }
                let lease_valid = !force_full
                    && !type_changed
                    && !permissions_changed
                    && cached
                        .zip(observed_row.as_ref())
                        .is_some_and(|(row, observed)| {
                            row.metadata_matches(observed, index_write_time)
                        });
                if lease_valid {
                    stats.cache_leases += 1;
                    let content_id = cached.and_then(|row| row.content_id);
                    verified.push(VerifiedCandidate {
                        path: entry.path.clone(),
                        change: VerifiedChange::Unchanged,
                        content_id,
                        identity_replaced,
                        rehashed: false,
                    });
                    root_rows.push(root_row(entry, VerifiedChange::Unchanged, content_id));
                    continue;
                }

                stats.cache_misses += 1;
                let content_id = provider.canonical_content_id(file, &entry.path, filter)?;
                stats.content_reads += 1;
                stats.content_hashes += 1;
                let expected_content = canonical_content(&entry.path)?;
                let change = if type_changed {
                    VerifiedChange::TypeChanged
                } else if permissions_changed {
                    VerifiedChange::PermissionsChanged
                } else if content_id == expected_content {
                    VerifiedChange::Unchanged
                } else {
                    VerifiedChange::Modified
                };
                // A V2 row is a clean-content lease, not merely a stat cache.
                // Never backfill dirty/type/mode candidates: doing so could let a
                // later status or record pass trust modified bytes as canonical.
                if change == VerifiedChange::Unchanged {
                    let row = observed_index_row(
                        file,
                        content_id,
                        entry.canonical_mode,
                        entry.canonical_kind,
                        index_write_time,
                        conversion_policy,
                    )?;
                    index_updates.push((entry.path.clone(), row));
                }
                verified.push(VerifiedCandidate {
                    path: entry.path.clone(),
                    change,
                    content_id: Some(content_id),
                    identity_replaced,
                    rehashed: true,
                });
                root_rows.push(root_row(entry, change, Some(content_id)));
            }
        }
    }

    let tracked_set: BTreeSet<_> = expected.keys().cloned().collect();
    if source
        .candidates
        .iter()
        .any(|candidate| !tracked_set.contains(&candidate.path))
    {
        fallback.get_or_insert(ChangeSourceFallbackReason::SourceError(
            "source returned an untracked candidate".into(),
        ));
    }
    verified.sort_by(|left, right| left.path.cmp(&right.path));
    index_updates.sort_by(|left, right| left.0.cmp(&right.0));
    index_deletions.sort();
    root_rows.sort();

    let metrics = ChangeSourceMetrics {
        source: source.source,
        candidate_count: stats.candidates,
        metadata_reads: stats.metadata_reads,
        content_reads: stats.content_reads,
        content_hashes: stats.content_hashes,
        valid_lease_hits: stats.cache_leases,
        racy_rehashes: stats.racy_candidates,
        fallback_count: usize::from(fallback.is_some()),
        fallback_source: source
            .fallback_source
            .or_else(|| fallback.as_ref().map(|_| original_source)),
        fallback_reason: fallback.clone(),
    };

    Ok(VerifiedCandidateResult {
        token: source.token,
        candidates: verified,
        root: VerifiedCandidateRoot {
            version: VERIFIED_CANDIDATE_ROOT_VERSION,
            hash: Hash::of(&canonical_root_bytes(&root_rows)),
        },
        index_updates,
        index_deletions,
        fallback,
        stats,
        metrics,
    })
}

/// Fingerprint the policy inputs status can observe without mutating config.
/// Any attributes, Atomic/Git config, sparse-checkout, or tracked attribute-file
/// change invalidates all V2 leases for the transaction.
pub fn conversion_policy_fingerprint(
    root: &Path,
    tracked_paths: &[RepoPath],
) -> Result<Hash, ChangeSourceError> {
    let mut inputs = vec![
        RepoPath::from_bytes(b".atomic/config.toml")
            .map_err(|error| ChangeSourceError::Path(error.to_string()))?,
        RepoPath::from_bytes(b".git/config")
            .map_err(|error| ChangeSourceError::Path(error.to_string()))?,
        RepoPath::from_bytes(b".git/info/attributes")
            .map_err(|error| ChangeSourceError::Path(error.to_string()))?,
        RepoPath::from_bytes(b".git/info/sparse-checkout")
            .map_err(|error| ChangeSourceError::Path(error.to_string()))?,
    ];
    inputs.push(
        RepoPath::from_bytes(b".gitattributes")
            .map_err(|error| ChangeSourceError::Path(error.to_string()))?,
    );
    for tracked in tracked_paths {
        let components: Vec<_> = tracked.components().collect();
        if components.len() <= 1 {
            continue;
        }
        let mut directory = Vec::new();
        for component in &components[..components.len() - 1] {
            if !directory.is_empty() {
                directory.push(b'/');
            }
            directory.extend_from_slice(component);
            let mut attributes = directory.clone();
            attributes.extend_from_slice(b"/.gitattributes");
            inputs.push(
                RepoPath::new(attributes)
                    .map_err(|error| ChangeSourceError::Path(error.to_string()))?,
            );
        }
    }
    inputs.sort();
    inputs.dedup();
    let mut bytes = b"atomic.status-conversion-policy-v1\0".to_vec();
    for path in inputs {
        bytes.extend_from_slice(&(path.as_bytes().len() as u64).to_le_bytes());
        bytes.extend_from_slice(path.as_bytes());
        let native = path
            .to_native()
            .map_err(|error| ChangeSourceError::Path(error.to_string()))?;
        match fs::read(root.join(native)) {
            Ok(content) => {
                bytes.push(1);
                bytes.extend_from_slice(&(content.len() as u64).to_le_bytes());
                bytes.extend_from_slice(&content);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => bytes.push(0),
            // Linked Git worktrees keep `.git` as a gitfile, so reserved paths
            // like `.git/config` are not readable through the worktree root
            // (their truth lives in the common directory). Treat that shape
            // like an absent input instead of failing status; the fingerprint
            // stays deterministic for a given worktree shape (CB-7A linked
            // worktrees).
            Err(error) if error.raw_os_error() == Some(20) => bytes.push(0),
            Err(error) => {
                return Err(ChangeSourceError::Io {
                    path: path.escaped(),
                    message: error.to_string(),
                })
            }
        }
    }
    Ok(Hash::of(&bytes))
}

pub fn now_timestamp() -> FileIndexTimestamp {
    let duration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    FileIndexTimestamp {
        seconds: duration.as_secs() as i64,
        nanoseconds: duration.subsec_nanos(),
    }
}

fn scan_token(candidates: &[ChangeCandidate]) -> ChangeSourceToken {
    let mut bytes = b"atomic.scan-token-v1\0".to_vec();
    for candidate in candidates {
        bytes.extend_from_slice(&(candidate.path.as_bytes().len() as u64).to_le_bytes());
        bytes.extend_from_slice(candidate.path.as_bytes());
        match &candidate.observation {
            CandidateObservation::Missing => bytes.push(0),
            CandidateObservation::Present(file) => {
                bytes.push(1);
                bytes.extend_from_slice(&file.device.unwrap_or(u64::MAX).to_le_bytes());
                bytes.extend_from_slice(&file.inode.unwrap_or(u64::MAX).to_le_bytes());
                bytes.extend_from_slice(&file.size.to_le_bytes());
            }
        }
    }
    ChangeSourceToken(Hash::of(&bytes).as_bytes().to_vec())
}

fn canonical_kind(kind: ObservedKind) -> Option<InodeKind> {
    match kind {
        ObservedKind::Regular => Some(InodeKind::Regular),
        ObservedKind::Symlink => Some(InodeKind::Symlink),
        ObservedKind::Directory => Some(InodeKind::Gitlink),
        ObservedKind::Other => None,
    }
}

fn omission_lease_matches(
    provider: &dyn FilesystemProvider,
    root: &Path,
    entry: &CanonicalTrackedPath,
    row: &FileIndexV2Entry,
    conversion_policy: Hash,
    index_write_time: FileIndexTimestamp,
) -> Result<bool, ChangeSourceError> {
    if !row.is_complete()
        || row.is_racy(index_write_time)
        || row.conversion_policy != Some(conversion_policy)
        || row.canonical_mode != Some(entry.canonical_mode)
        || row.canonical_kind != Some(entry.canonical_kind)
    {
        return Ok(false);
    }
    let candidate =
        provider.observe(root, &entry.path, ChangeCandidateReason::MetadataUncertain)?;
    let CandidateObservation::Present(file) = candidate.observation else {
        return Ok(false);
    };
    let Some(content_id) = row.content_id else {
        return Ok(false);
    };
    let Some(racy_after) = row.racy_after else {
        return Ok(false);
    };
    let observed = observed_index_row(
        &file,
        content_id,
        entry.canonical_mode,
        entry.canonical_kind,
        racy_after,
        conversion_policy,
    )?;
    Ok(row.metadata_matches(&observed, index_write_time))
}

fn read_canonical_content(
    file: &ObservedFile,
    path: &RepoPath,
    filter: &dyn ContentFilter,
) -> Result<Hash, ChangeSourceError> {
    let bytes = match file.kind {
        ObservedKind::Regular => fs::read(&file.native_path),
        ObservedKind::Symlink => read_link_bytes(&file.native_path),
        ObservedKind::Directory => match fs::read(file.native_path.join(".git")) {
            Ok(bytes) => Ok(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(error),
        },
        ObservedKind::Other => Ok(Vec::new()),
    }
    .map_err(|error| ChangeSourceError::Io {
        path: path.escaped(),
        message: error.to_string(),
    })?;
    let canonical = if file.kind == ObservedKind::Regular {
        let native = path
            .to_native()
            .map_err(|error| ChangeSourceError::Path(error.to_string()))?;
        filter
            .clean(&native, &bytes)
            .map_err(|error| ChangeSourceError::Filter {
                path: path.escaped(),
                message: error.to_string(),
            })?
            .bytes
    } else {
        bytes
    };
    Ok(Hash::of(&canonical))
}

#[cfg(unix)]
fn read_link_bytes(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    fs::read_link(path).map(|target| target.as_os_str().as_bytes().to_vec())
}

#[cfg(not(unix))]
fn read_link_bytes(_path: &Path) -> std::io::Result<Vec<u8>> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "lossless symlink targets are unsupported",
    ))
}

fn observed_index_row(
    file: &ObservedFile,
    content_id: Hash,
    mode: u16,
    kind: InodeKind,
    racy_after: FileIndexTimestamp,
    conversion_policy: Hash,
) -> Result<FileIndexV2Entry, ChangeSourceError> {
    FileIndexV2Entry::complete(
        file.device.ok_or_else(|| ChangeSourceError::InvalidIndex {
            path: file.native_path.display().to_string(),
            message: "device identity unavailable".into(),
        })?,
        file.inode.ok_or_else(|| ChangeSourceError::InvalidIndex {
            path: file.native_path.display().to_string(),
            message: "inode identity unavailable".into(),
        })?,
        file.mtime.ok_or_else(|| ChangeSourceError::InvalidIndex {
            path: file.native_path.display().to_string(),
            message: "mtime unavailable".into(),
        })?,
        file.ctime.ok_or_else(|| ChangeSourceError::InvalidIndex {
            path: file.native_path.display().to_string(),
            message: "ctime unavailable".into(),
        })?,
        file.size,
        content_id,
        mode,
        kind,
        racy_after,
        conversion_policy,
    )
    .map_err(|error| ChangeSourceError::InvalidIndex {
        path: file.native_path.display().to_string(),
        message: error.to_string(),
    })
}

#[cfg(unix)]
fn observe_metadata(native_path: std::path::PathBuf, metadata: &fs::Metadata) -> ObservedFile {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let file_type = metadata.file_type();
    ObservedFile {
        native_path,
        device: Some(metadata.dev()),
        inode: Some(metadata.ino()),
        mtime: FileIndexTimestamp::new(metadata.mtime(), metadata.mtime_nsec() as u32).ok(),
        ctime: FileIndexTimestamp::new(metadata.ctime(), metadata.ctime_nsec() as u32).ok(),
        size: metadata.size(),
        mode: Some((metadata.permissions().mode() & 0o777) as u16),
        kind: if file_type.is_symlink() {
            ObservedKind::Symlink
        } else if file_type.is_file() {
            ObservedKind::Regular
        } else if file_type.is_dir() {
            ObservedKind::Directory
        } else {
            ObservedKind::Other
        },
    }
}

#[cfg(not(unix))]
fn observe_metadata(native_path: std::path::PathBuf, metadata: &fs::Metadata) -> ObservedFile {
    ObservedFile {
        native_path,
        device: None,
        inode: None,
        mtime: None,
        ctime: None,
        size: metadata.len(),
        mode: None,
        kind: if metadata.file_type().is_symlink() {
            ObservedKind::Symlink
        } else if metadata.is_file() {
            ObservedKind::Regular
        } else if metadata.is_dir() {
            ObservedKind::Directory
        } else {
            ObservedKind::Other
        },
    }
}

fn root_row(
    entry: &CanonicalTrackedPath,
    change: VerifiedChange,
    content_id: Option<Hash>,
) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(entry.path.as_bytes().len() as u64).to_le_bytes());
    bytes.extend_from_slice(entry.path.as_bytes());
    bytes.push(match change {
        VerifiedChange::Unchanged => 0,
        VerifiedChange::Modified => 1,
        VerifiedChange::Deleted => 2,
        VerifiedChange::TypeChanged => 3,
        VerifiedChange::PermissionsChanged => 4,
    });
    bytes.extend_from_slice(&entry.canonical_mode.to_le_bytes());
    bytes.push(entry.canonical_kind.as_byte());
    if let Some(content_id) = content_id {
        bytes.extend_from_slice(content_id.as_bytes());
    } else {
        bytes.extend_from_slice(&[0; 32]);
    }
    bytes
}

fn canonical_root_bytes(rows: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = b"atomic.verified-candidates-v1\0".to_vec();
    bytes.extend_from_slice(&(rows.len() as u64).to_le_bytes());
    for row in rows {
        bytes.extend_from_slice(&(row.len() as u64).to_le_bytes());
        bytes.extend_from_slice(row);
    }
    bytes
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::content_filter::{ContentFilterError, FilteredContent};
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    struct ExactFilter;

    impl ContentFilter for ExactFilter {
        fn clean(
            &self,
            _path: &Path,
            working_bytes: &[u8],
        ) -> Result<FilteredContent, ContentFilterError> {
            Ok(FilteredContent {
                bytes: working_bytes.to_vec(),
                warnings: Vec::new(),
            })
        }

        fn smudge(
            &self,
            _path: &Path,
            repository_bytes: &[u8],
        ) -> Result<FilteredContent, ContentFilterError> {
            Ok(FilteredContent {
                bytes: repository_bytes.to_vec(),
                warnings: Vec::new(),
            })
        }
    }

    fn tracked(path: &RepoPath) -> Vec<CanonicalTrackedPath> {
        vec![CanonicalTrackedPath {
            path: path.clone(),
            canonical_mode: 0o644,
            canonical_kind: InodeKind::Regular,
        }]
    }

    fn source(root: &Path, paths: &[RepoPath]) -> ChangeSourceResult {
        ScanChangeSource
            .changes(ChangeSourceRequest {
                root,
                tracked_paths: paths,
                previous_token: None,
            })
            .unwrap()
    }

    #[test]
    fn cold_and_warm_scans_produce_the_same_root_and_warm_avoids_content_reads() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("tracked"), b"canonical").unwrap();
        let path = RepoPath::from_bytes(b"tracked").unwrap();
        let paths = vec![path.clone()];
        let policy = Hash::of(b"policy");
        let expected = Hash::of(b"canonical");
        let boundary = now_timestamp();

        let cold = verify_candidates(
            source(dir.path(), &paths),
            &tracked(&path),
            &BTreeMap::new(),
            policy,
            boundary,
            &ExactFilter,
            |_| Ok(expected),
        )
        .unwrap();
        assert_eq!(cold.stats.metadata_reads, 1);
        assert_eq!(cold.stats.content_reads, 1);
        assert_eq!(cold.stats.cache_misses, 1);
        assert_eq!(cold.index_updates.len(), 1);

        let index = cold.index_updates.clone().into_iter().collect();
        let warm = verify_candidates(
            source(dir.path(), &paths),
            &tracked(&path),
            &index,
            policy,
            now_timestamp(),
            &ExactFilter,
            |_| Ok(expected),
        )
        .unwrap();
        assert_eq!(warm.root, cold.root);
        assert_eq!(warm.stats.metadata_reads, 1);
        assert_eq!(warm.stats.content_reads, 0);
        assert_eq!(warm.stats.cache_leases, 1);

        let mut racy_row = cold.index_updates[0].1.clone();
        racy_row.racy_after = racy_row.mtime;
        let racy_index = [(path.clone(), racy_row)].into_iter().collect();
        let racy = verify_candidates(
            source(dir.path(), &paths),
            &tracked(&path),
            &racy_index,
            policy,
            now_timestamp(),
            &ExactFilter,
            |_| Ok(expected),
        )
        .unwrap();
        assert_eq!(racy.metrics.racy_rehashes, 1);
        assert_eq!(racy.metrics.content_hashes, 1);
    }

    #[test]
    fn same_size_and_restored_mtime_attack_is_rehashed() {
        let dir = tempdir().unwrap();
        let native = dir.path().join("tracked");
        fs::write(&native, b"abcdefgh").unwrap();
        let original_mtime = fs::metadata(&native).unwrap().modified().unwrap();
        let path = RepoPath::from_bytes(b"tracked").unwrap();
        let paths = vec![path.clone()];
        let policy = Hash::of(b"policy");
        let expected = Hash::of(b"abcdefgh");
        let cold = verify_candidates(
            source(dir.path(), &paths),
            &tracked(&path),
            &BTreeMap::new(),
            policy,
            now_timestamp(),
            &ExactFilter,
            |_| Ok(expected),
        )
        .unwrap();
        let index: BTreeMap<_, _> = cold.index_updates.into_iter().collect();

        fs::write(&native, b"ABCDEFGH").unwrap();
        let file = fs::OpenOptions::new().write(true).open(&native).unwrap();
        file.set_times(fs::FileTimes::new().set_modified(original_mtime))
            .unwrap();
        let attacked = verify_candidates(
            source(dir.path(), &paths),
            &tracked(&path),
            &index,
            policy,
            now_timestamp(),
            &ExactFilter,
            |_| Ok(expected),
        )
        .unwrap();
        assert_eq!(attacked.stats.content_reads, 1);
        assert_eq!(attacked.candidates[0].change, VerifiedChange::Modified);
    }

    #[test]
    fn replacement_chmod_type_and_delete_are_never_hidden_by_the_cache() {
        let dir = tempdir().unwrap();
        let native = dir.path().join("tracked");
        fs::write(&native, b"same").unwrap();
        let path = RepoPath::from_bytes(b"tracked").unwrap();
        let paths = vec![path.clone()];
        let policy = Hash::of(b"policy");
        let expected = Hash::of(b"same");
        let cold = verify_candidates(
            source(dir.path(), &paths),
            &tracked(&path),
            &BTreeMap::new(),
            policy,
            now_timestamp(),
            &ExactFilter,
            |_| Ok(expected),
        )
        .unwrap();
        let index: BTreeMap<_, _> = cold.index_updates.into_iter().collect();

        let replacement = dir.path().join("replacement");
        fs::write(&replacement, b"same").unwrap();
        fs::rename(&replacement, &native).unwrap();
        let replaced = verify_candidates(
            source(dir.path(), &paths),
            &tracked(&path),
            &index,
            policy,
            now_timestamp(),
            &ExactFilter,
            |_| Ok(expected),
        )
        .unwrap();
        assert!(replaced.candidates[0].identity_replaced);
        assert!(replaced.candidates[0].rehashed);
        assert_eq!(replaced.candidates[0].change, VerifiedChange::Unchanged);

        fs::set_permissions(&native, fs::Permissions::from_mode(0o755)).unwrap();
        let chmod = verify_candidates(
            source(dir.path(), &paths),
            &tracked(&path),
            &index,
            policy,
            now_timestamp(),
            &ExactFilter,
            |_| Ok(expected),
        )
        .unwrap();
        assert_eq!(
            chmod.candidates[0].change,
            VerifiedChange::PermissionsChanged
        );

        fs::remove_file(&native).unwrap();
        fs::create_dir(&native).unwrap();
        let type_change = verify_candidates(
            source(dir.path(), &paths),
            &tracked(&path),
            &index,
            policy,
            now_timestamp(),
            &ExactFilter,
            |_| Ok(expected),
        )
        .unwrap();
        assert_eq!(
            type_change.candidates[0].change,
            VerifiedChange::TypeChanged
        );

        fs::remove_dir(&native).unwrap();
        let deleted = verify_candidates(
            source(dir.path(), &paths),
            &tracked(&path),
            &index,
            policy,
            now_timestamp(),
            &ExactFilter,
            |_| Ok(expected),
        )
        .unwrap();
        assert_eq!(deleted.candidates[0].change, VerifiedChange::Deleted);
        assert_eq!(deleted.index_deletions, paths);
    }

    #[test]
    fn policy_change_forces_complete_rehash_and_backfill() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("tracked"), b"same").unwrap();
        let path = RepoPath::from_bytes(b"tracked").unwrap();
        let paths = vec![path.clone()];
        let old_policy = Hash::of(b"old");
        let new_policy = Hash::of(b"new");
        let expected = Hash::of(b"same");
        let cold = verify_candidates(
            source(dir.path(), &paths),
            &tracked(&path),
            &BTreeMap::new(),
            old_policy,
            now_timestamp(),
            &ExactFilter,
            |_| Ok(expected),
        )
        .unwrap();
        let index: BTreeMap<_, _> = cold.index_updates.into_iter().collect();
        let changed = verify_candidates(
            source(dir.path(), &paths),
            &tracked(&path),
            &index,
            new_policy,
            now_timestamp(),
            &ExactFilter,
            |_| Ok(expected),
        )
        .unwrap();
        assert_eq!(
            changed.fallback,
            Some(ChangeSourceFallbackReason::ConversionPolicyChanged)
        );
        assert_eq!(changed.stats.content_reads, 1);
        assert_eq!(changed.index_updates.len(), 1);
    }
}
