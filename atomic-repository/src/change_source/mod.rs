//! Candidate change sources and canonical filesystem re-verification.

mod core;
mod fsmonitor;
#[cfg(test)]
mod performance;
mod process;
mod scan;
mod selected;
mod token;
mod watchman;

pub use core::{
    CandidateObservation, CanonicalTrackedPath, ChangeCandidate, ChangeCandidateReason,
    ChangeSource, ChangeSourceError, ChangeSourceFallbackReason, ChangeSourceKind,
    ChangeSourceMetrics, ChangeSourceRequest, ChangeSourceResult, ChangeSourceStats,
    ChangeSourceToken, ObservedFile, ObservedKind, VerifiedCandidate, VerifiedCandidateResult,
    VerifiedCandidateRoot, VerifiedChange, VERIFIED_CANDIDATE_ROOT_VERSION,
};
pub use fsmonitor::FsmonitorChangeSource;
pub use process::{CommandOutput, CommandRunner, CommandSpec, ProcessCommandRunner};
pub use scan::{
    conversion_policy_fingerprint, now_timestamp, verify_candidates,
    verify_candidates_with_provider, FilesystemProvider, RealFilesystemProvider, ScanChangeSource,
};
pub use selected::{ChangeSourceEnvironment, SelectedChangeSource};
pub use token::{
    ChangeSourceTokenStore, TokenInvalidationReason, TokenLoadResult, TokenStoreError,
    CHANGE_SOURCE_TOKEN_VERSION,
};
pub use watchman::WatchmanChangeSource;
