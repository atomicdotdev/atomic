use std::path::{Path, PathBuf};

use atomic_core::pristine::{FileIndexTimestamp, FileIndexV2Entry};
use atomic_core::types::Hash;
use thiserror::Error;

use crate::repository::RepoPath;

/// Stable identity of a candidate source. Persisted tokens are scoped by this value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ChangeSourceKind {
    #[default]
    Scan,
    Fsmonitor,
    Watchman,
    Custom,
}

impl ChangeSourceKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Scan => "scan",
            Self::Fsmonitor => "fsmonitor",
            Self::Watchman => "watchman",
            Self::Custom => "custom",
        }
    }
}

/// Opaque checkpoint owned by a change-source implementation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChangeSourceToken(pub Vec<u8>);

/// Why a source could not safely provide an incremental candidate set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChangeSourceFallbackReason {
    MissingIndex,
    MalformedIndex(String),
    ConversionPolicyChanged,
    CanonicalAttributesChanged,
    SparsePolicyChanged,
    ConfigurationChanged,
    FilterChanged,
    ConfiguredOff,
    DisabledInCi,
    DisabledInContainer,
    Unavailable(String),
    UnsupportedVersion { found: String, minimum: String },
    UnknownToken,
    Overflow,
    MalformedResponse(String),
    Timeout { milliseconds: u64 },
    SourceError(String),
}

impl ChangeSourceFallbackReason {
    /// Stable observability code for this degradation reason (RFC §13
    /// Phase 13 task 5). Codes are fixed snake_case strings; details live
    /// in logs, never in structured telemetry.
    pub fn code(&self) -> &'static str {
        match self {
            Self::MissingIndex => "missing_index",
            Self::MalformedIndex(_) => "malformed_index",
            Self::ConversionPolicyChanged => "conversion_policy_changed",
            Self::CanonicalAttributesChanged => "canonical_attributes_changed",
            Self::SparsePolicyChanged => "sparse_policy_changed",
            Self::ConfigurationChanged => "configuration_changed",
            Self::FilterChanged => "filter_changed",
            Self::ConfiguredOff => "configured_off",
            Self::DisabledInCi => "disabled_in_ci",
            Self::DisabledInContainer => "disabled_in_container",
            Self::Unavailable(_) => "unavailable",
            Self::UnsupportedVersion { .. } => "unsupported_version",
            Self::UnknownToken => "unknown_token",
            Self::Overflow => "overflow",
            Self::MalformedResponse(_) => "malformed_response",
            Self::Timeout { .. } => "timeout",
            Self::SourceError(_) => "source_error",
        }
    }
}

/// Why a path was selected for canonical re-verification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeCandidateReason {
    FullScan,
    Created,
    Deleted,
    MetadataChanged,
    MetadataUncertain,
    Racy,
    Explicit,
}

/// Portable filesystem facts needed to validate a V2 cache lease.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedFile {
    pub native_path: PathBuf,
    pub device: Option<u64>,
    pub inode: Option<u64>,
    pub mtime: Option<FileIndexTimestamp>,
    pub ctime: Option<FileIndexTimestamp>,
    pub size: u64,
    pub mode: Option<u16>,
    pub kind: ObservedKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObservedKind {
    Regular,
    Directory,
    Symlink,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CandidateObservation {
    Missing,
    Present(ObservedFile),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeCandidate {
    pub path: RepoPath,
    pub reason: ChangeCandidateReason,
    pub observation: CandidateObservation,
}

/// Instrumentation shared by warm and cold source tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChangeSourceStats {
    pub tracked_paths: usize,
    pub candidates: usize,
    pub metadata_reads: usize,
    pub content_reads: usize,
    pub content_hashes: usize,
    pub cache_leases: usize,
    pub cache_misses: usize,
    pub racy_candidates: usize,
}

/// Deterministic metrics emitted for one canonical status transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeSourceMetrics {
    pub source: ChangeSourceKind,
    pub candidate_count: usize,
    pub metadata_reads: usize,
    pub content_reads: usize,
    pub content_hashes: usize,
    pub valid_lease_hits: usize,
    pub racy_rehashes: usize,
    pub fallback_count: usize,
    pub fallback_source: Option<ChangeSourceKind>,
    pub fallback_reason: Option<ChangeSourceFallbackReason>,
}

/// Candidate output. `complete` means omissions are impossible for this result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeSourceResult {
    pub root: PathBuf,
    pub source: ChangeSourceKind,
    pub fallback_source: Option<ChangeSourceKind>,
    pub token: ChangeSourceToken,
    pub candidates: Vec<ChangeCandidate>,
    pub complete: bool,
    pub fallback: Option<ChangeSourceFallbackReason>,
    pub stats: ChangeSourceStats,
}

#[derive(Clone, Copy)]
pub struct ChangeSourceRequest<'a> {
    pub root: &'a Path,
    pub tracked_paths: &'a [RepoPath],
    pub previous_token: Option<&'a ChangeSourceToken>,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ChangeSourceError {
    #[error("change source is unavailable: {0}")]
    Unavailable(String),
    #[error("change source requires a supported version (found {found}, minimum {minimum})")]
    UnsupportedVersion { found: String, minimum: String },
    #[error("change source rejected an unknown token")]
    UnknownToken,
    #[error("change source exceeded its bound of {limit} paths")]
    Overflow { limit: usize },
    #[error("change source returned a malformed response: {0}")]
    MalformedResponse(String),
    #[error("change source timed out after {milliseconds}ms")]
    Timeout { milliseconds: u64 },
    #[error("cannot inspect '{path}': {message}")]
    Io { path: String, message: String },
    #[error("cannot convert repository path: {0}")]
    Path(String),
    #[error("content conversion failed for '{path}': {message}")]
    Filter { path: String, message: String },
    #[error("canonical content lookup failed for '{path}': {message}")]
    CanonicalContent { path: String, message: String },
    #[error("FILE_INDEX_V2 entry for '{path}' is invalid: {message}")]
    InvalidIndex { path: String, message: String },
}

/// Supplies possible changed paths. Results are candidates, never proof.
pub trait ChangeSource: Send + Sync {
    fn changes(
        &self,
        request: ChangeSourceRequest<'_>,
    ) -> Result<ChangeSourceResult, ChangeSourceError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanonicalTrackedPath {
    pub path: RepoPath,
    pub canonical_mode: u16,
    pub canonical_kind: atomic_core::change::InodeKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifiedChange {
    Unchanged,
    Modified,
    Deleted,
    TypeChanged,
    PermissionsChanged,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedCandidate {
    pub path: RepoPath,
    pub change: VerifiedChange,
    pub content_id: Option<Hash>,
    pub identity_replaced: bool,
    pub rehashed: bool,
}

pub const VERIFIED_CANDIDATE_ROOT_VERSION: u8 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VerifiedCandidateRoot {
    pub version: u8,
    pub hash: Hash,
}

/// Canonical result consumed by status and equivalence callers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedCandidateResult {
    pub token: ChangeSourceToken,
    pub candidates: Vec<VerifiedCandidate>,
    pub root: VerifiedCandidateRoot,
    pub index_updates: Vec<(RepoPath, FileIndexV2Entry)>,
    pub index_deletions: Vec<RepoPath>,
    pub fallback: Option<ChangeSourceFallbackReason>,
    pub stats: ChangeSourceStats,
    pub metrics: ChangeSourceMetrics,
}
