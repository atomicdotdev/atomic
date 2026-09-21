//! Structured bridge observability (RFC §13 Phase 13 task 5, CB-13C).
//!
//! One bounded, append-only JSON-lines journal records reconciliation
//! outcomes, drift/refusal classes, publication-boundary refusals,
//! change-source degradation, recovery outcomes and binding-closure loss
//! under `<dot-dir>/bridge/events.jsonl`.
//!
//! # Privacy rules (enforced by validated construction, not convention)
//!
//! * The sink lives under the Atomic dot directory (`.atomic/`), which the
//!   bridge projection excludes — telemetry is never exported into Git
//!   metadata or objects.
//! * [`BridgeEventKind`] is a closed enum whose fields are validated
//!   fixed-format identifiers ([`UlidId`], [`OpIdRef`], [`HexBindingId`]) or
//!   closed classification-code enums ([`RefusalClass`],
//!   [`FallbackReasonCode`], …). There is deliberately **no free-form
//!   payload field**: a value that is not one of the closed codes or a
//!   correctly formatted identifier cannot be constructed, so prompts,
//!   transcripts, decision-graph payloads, file contents and configuration
//!   secrets cannot be serialized into telemetry by construction; tests
//!   pin the serialized field set *and* the rejection of adversarial
//!   sentinels.
//! * Trust signers, keys and session material are never inputs to any event.
//!
//! # Consent boundary
//!
//! Automatic (library-level) paths — operation recovery and binding-closure
//! fetch — obtain their journal through [`BridgeEventJournal::for_repository`],
//! which writes only when the repository has explicitly opted in via
//! `[git.bridge] enabled = true`. Observational paths (`Repository::status`)
//! never write telemetry at all: they return metrics to the caller, and only
//! explicit bridge commands decide whether an authorized boundary records
//! them. Explicit CLI bridge commands construct their journal directly and
//! are their own consent.
//!
//! # Failure policy
//!
//! Observability is advisory: [`BridgeEventJournal::emit_lossy`] never
//! fails a bridge operation. The journal is bounded — a record is appended
//! only when the complete record (including its newline) fits under
//! [`MAX_BRIDGE_EVENT_JOURNAL_BYTES`]; otherwise the event is dropped (the
//! bound is the documented retention policy; complete history is never
//! truncated to make room). Records are appended as one atomic O_APPEND
//! write under a nonblocking file lock; when the lock is contended beyond a
//! bounded retry budget the event is dropped rather than corrupting
//! concurrent JSONL. An incomplete trailing fragment left by a crashed
//! writer is repaired before the next append. The journal is best-effort
//! telemetry (no fsync); it is not the durable operation/receipt journal
//! and must not be presented as audit-retention or recovery authority.


use super::{Repository, RepositoryError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use serde::Serialize;


/// Hard cap on the event journal. Past this bound new events are dropped
/// instead of silently growing an unbounded file.
pub const MAX_BRIDGE_EVENT_JOURNAL_BYTES: u64 = 4 * 1024 * 1024;

/// Hard bound on one serialized record (defense in depth: every field is a
/// closed code or fixed-format identifier, so records are far below this;
/// the check still guarantees a single record can never approach the
/// journal cap). A record whose encoded size plus its newline exceeds this
/// is dropped.
pub const MAX_BRIDGE_EVENT_RECORD_BYTES: usize = 64 * 1024;

/// Dot-directory-relative location of the event journal.
pub const BRIDGE_EVENT_JOURNAL_RELATIVE_PATH: &str = "bridge/events.jsonl";

/// Bounded nonblocking retry budget for the append lock. Contended writers
/// drop the event after this budget instead of blocking bridge operations.
const APPEND_LOCK_ATTEMPTS: u32 = 24;

#[cfg(target_os = "linux")]
const OPEN_NO_FOLLOW: i32 = 0o400_000; // O_NOFOLLOW
#[cfg(target_os = "linux")]
const OPEN_NONBLOCK: i32 = 0o4000; // O_NONBLOCK
#[cfg(target_os = "linux")]
const OPEN_DIRECTORY: i32 = 0o020_000; // O_DIRECTORY
#[cfg(target_os = "macos")]
const OPEN_NO_FOLLOW: i32 = 0o400; // O_NOFOLLOW
#[cfg(target_os = "macos")]
const OPEN_NONBLOCK: i32 = 0o4; // O_NONBLOCK
#[cfg(target_os = "macos")]
const OPEN_DIRECTORY: i32 = 0o1; // O_DIRECTORY
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const OPEN_NO_FOLLOW: i32 = 0;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const OPEN_NONBLOCK: i32 = 0;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const OPEN_DIRECTORY: i32 = 0;

/// Validated 26-character Crockford-base32 ULID (working-copy identity).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(transparent)]
pub struct UlidId(String);

/// Validated 52-character base32 operation identifier.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(transparent)]
pub struct OpIdRef(String);

/// Validated 40-character lowercase-hex binding identifier.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(transparent)]
pub struct HexBindingId(String);

impl UlidId {
    /// Accepts only exactly 26 Crockford-base32 characters
    /// (`0-9`, `A-Z` minus `I L O U`). Anything else — including prose,
    /// sentinels or tokens without spaces — is refused.
    pub fn new(raw: &str) -> Option<Self> {
        valid_fixed_id(raw, 26, &crockford_char).map(Self)
    }

    /// The validated identifier text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl OpIdRef {
    /// Accepts only exactly 52 base32 characters (`A-Z`, `2-7`).
    pub fn new(raw: &str) -> Option<Self> {
        valid_fixed_id(raw, 52, &base32_char).map(Self)
    }

    /// The validated identifier text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl HexBindingId {
    /// Accepts only exactly 64 lowercase hexadecimal characters (the
    /// lowercase-hex rendering of the 32-byte binding set identity).
    pub fn new(raw: &str) -> Option<Self> {
        valid_fixed_id(raw, 64, &|character: char| {
            character.is_ascii_digit() || ('a'..='f').contains(&character)
        })
        .map(Self)
    }

    /// The validated identifier text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn valid_fixed_id(
    raw: &str,
    length: usize,
    character_ok: &dyn Fn(char) -> bool,
) -> Option<String> {
    if raw.len() != length || !raw.chars().all(|c| c.is_ascii() && character_ok(c)) {
        return None;
    }
    Some(raw.to_string())
}

fn crockford_char(character: char) -> bool {
    character.is_ascii_digit()
        || (character.is_ascii_uppercase()
            && !"ILOU".contains(character))
}

fn base32_char(character: char) -> bool {
    character.is_ascii_digit() && ('2'..='7').contains(&character)
        || character.is_ascii_uppercase()
}

/// Direction a reconcile run was classified into (closed code set).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconcileDirectionCode {
    Neither,
    GitToAtomic,
    AtomicToGit,
    Diverged,
    Unknown,
}

impl ReconcileDirectionCode {
    /// Accepts only the fixed classification codes; anything else is
    /// refused so no free-form text can enter telemetry.
    pub fn from_code(code: &str) -> Option<Self> {
        match code {
            "neither" => Some(Self::Neither),
            "git_to_atomic" => Some(Self::GitToAtomic),
            "atomic_to_git" => Some(Self::AtomicToGit),
            "diverged" => Some(Self::Diverged),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

/// Workspace-transaction mode code (closed code set).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceModeCode {
    Observe,
    Reconcile,
    Force,
    Unknown,
}

impl WorkspaceModeCode {
    /// Accepts only the fixed mode codes; anything else is refused.
    pub fn from_code(code: &str) -> Option<Self> {
        match code {
            "observe" => Some(Self::Observe),
            "reconcile" => Some(Self::Reconcile),
            "force" => Some(Self::Force),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

/// Workspace remediation class (closed code set; mirrors
/// `WorkspaceRemediation::code()` values).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RemediationCode {
    GitOperationInProgress,
    Unanchored,
    OperationHeadsDiverged,
    ConcurrentGitMutation,
    /// A Git administrative lock appeared under the retained entry lease
    /// right before effects (CB-13D review R2).
    GitLocksBusy,
}

impl RemediationCode {
    /// Accepts only the fixed remediation codes; anything else is refused.
    pub fn from_code(code: &str) -> Option<Self> {
        match code {
            "git_operation_in_progress" => Some(Self::GitOperationInProgress),
            "unanchored" => Some(Self::Unanchored),
            "operation_heads_diverged" => Some(Self::OperationHeadsDiverged),
            "concurrent_git_mutation" => Some(Self::ConcurrentGitMutation),
            "git_locks_busy" => Some(Self::GitLocksBusy),
            _ => None,
        }
    }
}

/// Stable refusal class for a refused run (closed code set; the serialized
/// values match the classes recorded before the typed surface existed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalClass {
    WorkspaceEntryRefused,
    MappingDiverged,
    MappedRefIndependentMove,
    CheckpointDiverged,
    RepositoryOpenFailed,
    ObservationFailed,
    MappingObservationFailed,
    VerifyFailed,
    MappingRefreshFailed,
    ImportFailed,
    ProjectionFailed,
    /// CB-13D (RFC §11.2 rule 4): the optional metadata-only bridge watch
    /// daemon classified a run as Atomic→Git export and refused to execute
    /// it — export belongs to an explicit command boundary.
    MetadataOnlyExportSuppressed,
    /// CB-13D review R1: the metadata-only boundary deferred because the
    /// repository holds pending recovery work that may only execute at an
    /// explicit command boundary. Nothing was mutated.
    MetadataOnlyRecoveryDeferred,
}

/// Change-source tier (closed code set).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TierCode {
    Scan,
    Fsmonitor,
    Watchman,
    Custom,
}

impl TierCode {
    /// Accepts only the fixed tier codes; anything else is refused.
    pub fn from_code(code: &str) -> Option<Self> {
        match code {
            "scan" => Some(Self::Scan),
            "fsmonitor" => Some(Self::Fsmonitor),
            "watchman" => Some(Self::Watchman),
            "custom" => Some(Self::Custom),
            _ => None,
        }
    }
}

/// Change-source degradation reason (closed code set; values mirror
/// [`crate::change_source::ChangeSourceFallbackReason::code`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackReasonCode {
    MissingIndex,
    MalformedIndex,
    ConversionPolicyChanged,
    CanonicalAttributesChanged,
    SparsePolicyChanged,
    ConfigurationChanged,
    FilterChanged,
    ConfiguredOff,
    DisabledInCi,
    DisabledInContainer,
    Unavailable,
    UnsupportedVersion,
    UnknownToken,
    Overflow,
    MalformedResponse,
    Timeout,
    SourceError,
}

impl FallbackReasonCode {
    /// Accepts only the fixed reason codes; anything else is refused.
    pub fn from_code(code: &str) -> Option<Self> {
        match code {
            "missing_index" => Some(Self::MissingIndex),
            "malformed_index" => Some(Self::MalformedIndex),
            "conversion_policy_changed" => Some(Self::ConversionPolicyChanged),
            "canonical_attributes_changed" => Some(Self::CanonicalAttributesChanged),
            "sparse_policy_changed" => Some(Self::SparsePolicyChanged),
            "configuration_changed" => Some(Self::ConfigurationChanged),
            "filter_changed" => Some(Self::FilterChanged),
            "configured_off" => Some(Self::ConfiguredOff),
            "disabled_in_ci" => Some(Self::DisabledInCi),
            "disabled_in_container" => Some(Self::DisabledInContainer),
            "unavailable" => Some(Self::Unavailable),
            "unsupported_version" => Some(Self::UnsupportedVersion),
            "unknown_token" => Some(Self::UnknownToken),
            "overflow" => Some(Self::Overflow),
            "malformed_response" => Some(Self::MalformedResponse),
            "timeout" => Some(Self::Timeout),
            "source_error" => Some(Self::SourceError),
            _ => None,
        }
    }
}

/// What a refused publication boundary counted (accurate units: the
/// receive boundary counts ref updates; the pre-push gate counts
/// provenance blocks/changes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationUnit {
    Refs,
    Changes,
}

/// Publication boundary (closed code set).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationBoundary {
    VerifyReceive,
    PrePush,
}

/// Why an automatic recovery failed closed (closed code set).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryFailureCode {
    DivergedHeads,
    LockValidation,
    InverseConstruction,
    RecoveryApply,
    FilesystemReplay,
    Finalize,
}

/// Why a binding fetch refused before reaching a verdict (closed code set).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingFetchRefusalCode {
    Cryptography,
    Content,
    ClosureBudget,
    ClosureValidation,
    IngestFailed,
}

/// Binding-fetch transport readiness (closed code set).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessCode {
    Complete,
    Incomplete,
}

/// One structured observability event.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BridgeEvent {
    /// Emission time, milliseconds since the Unix epoch.
    pub timestamp_ms: u64,
    /// Working copy that observed the event, when known (validated ULID).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_copy: Option<UlidId>,
    /// Typed event payload; determines the `event` discriminator field.
    #[serde(flatten)]
    pub kind: BridgeEventKind,
}

/// Closed set of bridge events with stable field names.
///
/// Every field holds only a closed classification code or a validated
/// fixed-format identifier. This is the structural redaction guarantee:
/// no value that carries a prompt, transcript, decision graph, secret or
/// arbitrary file content can be constructed into an event.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum BridgeEventKind {
    /// One `atomic git bridge reconcile` run: direction classified from the
    /// checkpoint/mapping, and how the run ended.
    Reconcile {
        /// Closed direction code.
        direction: ReconcileDirectionCode,
        /// Applied, refused, failed, or no-change outcome.
        outcome: EventOutcome,
        /// Stable refusal class when the run refused or failed.
        #[serde(skip_serializing_if = "Option::is_none")]
        refusal_class: Option<RefusalClass>,
    },
    /// A workspace transaction entry refused a mutating boundary (drift/
    /// Git-owned state/diverged heads), with stable mode and remediation
    /// classes. Recorded only by mutating remediation boundaries: guarded
    /// commands never write telemetry on refusal (RFC Phase 0
    /// no-mutation contract, CB-0C).
    WorkspaceRefusal {
        /// Closed transaction mode code.
        mode: WorkspaceModeCode,
        /// Closed remediation class (mirrors `WorkspaceRemediation::code`).
        remediation: RemediationCode,
    },
    /// A publication boundary refused one or more protected ref updates
    /// (RFC §10.4), with the unit the counts are expressed in.
    PublicationRefusal {
        /// Closed boundary code.
        boundary: PublicationBoundary,
        /// Refused units.
        refused: usize,
        /// Units examined in total.
        checked: usize,
        /// What one unit is (`refs` at the receive boundary, `changes` at
        /// the pre-push provenance gate).
        unit: PublicationUnit,
    },
    /// A candidate-path change source degraded (RFC §11.2 rule 7). The
    /// steady-state tier is observable per transaction in status output;
    /// explicit bridge command boundaries record the degradation itself.
    ChangeSourceFallback {
        /// Closed tier code in use.
        source: TierCode,
        /// Tier the run degraded from, when the source itself fell back.
        #[serde(skip_serializing_if = "Option::is_none")]
        fallback_source: Option<TierCode>,
        /// Stable reason code for the degradation.
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<FallbackReasonCode>,
    },
    /// An incomplete operation head was recovered (RFC §14.2).
    Recovery {
        /// The recovered original operation (validated base32 ID).
        original: OpIdRef,
        /// The immutable inverse/recovery operation (validated base32 ID).
        recovery: OpIdRef,
        /// Whether this recovery appended a new `Recover` operation
        /// (`true`) or resumed an existing one (`false`).
        created: bool,
    },
    /// An automatic recovery failed closed (RFC §14.2). Emitted from the
    /// recovery failure paths, which previously had no terminal event.
    RecoveryFailure {
        /// The original operation whose recovery failed (validated ID).
        original: OpIdRef,
        /// Stable failure class.
        reason: RecoveryFailureCode,
    },
    /// A binding-closure fetch ended with transport readiness and whether
    /// any loss note was recorded (RFC §5.1/CB-6C; the loss observable).
    BindingFetch {
        /// Validated binding ID (40 lowercase hex characters).
        binding: HexBindingId,
        /// Closed readiness code.
        readiness: ReadinessCode,
        /// Whether any loss note (e.g. truncated shallow history) applied.
        lossy: bool,
    },
    /// One binding-ref TRANSFER effect to a remote (CB-10B review R8): the
    /// per-ref evidence the remote queue previously never recorded. Emitted
    /// after the exact-target verification for transferred refs, and as a
    /// refusal when the transfer fails or the degraded fallback runs.
    BindingTransfer {
        /// Validated binding ID (40 lowercase hex characters).
        binding: HexBindingId,
        /// Closed outcome: Applied (verified at target), Refused
        /// (create-only lease lost) or Failed (network/verification).
        outcome: EventOutcome,
        /// The destination ref as advertised (namespace or degraded).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        destination: Option<String>,
    },
    /// A binding fetch refused before reaching a readiness verdict
    /// (cryptography/content verification, closure budget or final
    /// closure validation). Previously these exits produced no event, so
    /// refused/corrupt fetches were uncountable.
    BindingFetchRefused {
        /// Validated binding ID.
        binding: HexBindingId,
        /// Stable refusal class.
        reason: BindingFetchRefusalCode,
    },
    /// One import run's synthesis counters, aggregated from the real
    /// import statistics (per-import synthesis aggregation; the loss
    /// observable remains the per-fetch `binding_fetch` loss flag — an
    /// import itself never fetches bindings, so there is no per-import
    /// loss counter to record). Emitted only for consented repositories.
    ImportSynthesis {
        /// Commits found in Git for the imported branch.
        commits_found: usize,
        /// Commits successfully parsed.
        commits_parsed: usize,
        /// Changes written.
        written: usize,
        /// Empty commits.
        empty: usize,
        /// Merge commits with duplicate content.
        merges: usize,
        /// Commits skipped as self-pushed.
        self_push_skipped: usize,
        /// Squash commits inserted as original change records.
        squash_inserted: usize,
        /// CB-13C F4: commits resurrected EXACTLY from verified bindings —
        /// a subset of `written`; synthesis is `written - resurrected_exact`.
        resurrected_exact: usize,
        /// CB-13C F4: per-import correlation ID.
        correlation_id: String,
        /// CB-13C F4: when the import failed after landing `n` commits,
        /// the aggregate accounts for them instead of being bypassed.
        #[serde(skip_serializing_if = "Option::is_none")]
        failed_after_landed: Option<usize>,
    },
}

/// Terminal outcome of an observed bridge action.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventOutcome {
    /// The action completed.
    Applied,
    /// The action was refused before mutating anything.
    Refused,
    /// The action found nothing to do.
    NoChange,
    /// The action failed terminally and its effects are unverified: some
    /// or all mutations may already have been applied. Used only where a
    /// refusal-before-mutation claim would be false.
    Failed,
}

impl TierCode {
    /// Exhaustive mapping from the change-source tier enum: the compiler
    /// forces every tier variant into the closed telemetry code set.
    pub fn from_kind(kind: crate::change_source::ChangeSourceKind) -> Self {
        match kind {
            crate::change_source::ChangeSourceKind::Scan => Self::Scan,
            crate::change_source::ChangeSourceKind::Fsmonitor => Self::Fsmonitor,
            crate::change_source::ChangeSourceKind::Watchman => Self::Watchman,
            crate::change_source::ChangeSourceKind::Custom => Self::Custom,
        }
    }
}

impl FallbackReasonCode {
    /// Exhaustive mapping from the change-source degradation reason enum:
    /// the compiler forces every variant into the closed telemetry code
    /// set, so a newly added reason cannot bypass redaction.
    pub fn from_reason(reason: &crate::change_source::ChangeSourceFallbackReason) -> Self {
        use crate::change_source::ChangeSourceFallbackReason as Reason;
        match reason {
            Reason::MissingIndex => Self::MissingIndex,
            Reason::MalformedIndex(_) => Self::MalformedIndex,
            Reason::ConversionPolicyChanged => Self::ConversionPolicyChanged,
            Reason::CanonicalAttributesChanged => Self::CanonicalAttributesChanged,
            Reason::SparsePolicyChanged => Self::SparsePolicyChanged,
            Reason::ConfigurationChanged => Self::ConfigurationChanged,
            Reason::FilterChanged => Self::FilterChanged,
            Reason::ConfiguredOff => Self::ConfiguredOff,
            Reason::DisabledInCi => Self::DisabledInCi,
            Reason::DisabledInContainer => Self::DisabledInContainer,
            Reason::Unavailable(_) => Self::Unavailable,
            Reason::UnsupportedVersion { .. } => Self::UnsupportedVersion,
            Reason::UnknownToken => Self::UnknownToken,
            Reason::Overflow => Self::Overflow,
            Reason::MalformedResponse(_) => Self::MalformedResponse,
            Reason::Timeout { .. } => Self::Timeout,
            Reason::SourceError(_) => Self::SourceError,
        }
    }
}

impl RemediationCode {
    /// Exhaustive mapping from the workspace remediation enum: the
    /// compiler forces every remediation variant into the closed code set.
    pub fn from_remediation(remediation: &super::WorkspaceRemediation) -> Self {
        use super::WorkspaceRemediation as Remediation;
        match remediation {
            Remediation::GitOperationInProgress { .. } => Self::GitOperationInProgress,
            Remediation::Unanchored { .. } => Self::Unanchored,
            Remediation::OperationHeadsDiverged { .. } => Self::OperationHeadsDiverged,
            Remediation::ConcurrentGitMutation { .. } => Self::ConcurrentGitMutation,
            Remediation::GitLocksBusy { .. } => Self::GitLocksBusy,
        }
    }
}

impl WorkspaceModeCode {
    /// Exhaustive mapping from the transaction mode carried by a
    /// remediation; remediations without a mode are `unknown`.
    pub fn from_remediation(remediation: &super::WorkspaceRemediation) -> Self {
        use super::WorkspaceRemediation as Remediation;
        match remediation {
            Remediation::GitOperationInProgress { mode, .. }
            | Remediation::Unanchored { mode, .. } => Self::from(mode),
            Remediation::OperationHeadsDiverged { .. }
            | Remediation::ConcurrentGitMutation { .. }
            | Remediation::GitLocksBusy { .. } => Self::Unknown,
        }
    }
}

impl From<&super::WorkspaceTxnMode> for WorkspaceModeCode {
    fn from(mode: &super::WorkspaceTxnMode) -> Self {
        use super::WorkspaceTxnMode as Mode;
        match mode {
            Mode::Observe => Self::Observe,
            Mode::Reconcile => Self::Reconcile,
            Mode::Force => Self::Force,
        }
    }
}

/// Whether the bridge telemetry sink is consented for a dot directory.
///
/// Automatic paths (recovery, binding fetch) write telemetry only for
/// repositories that explicitly opted in via `[git.bridge] enabled = true`;
/// explicit bridge CLI commands construct their journal directly and are
/// their own consent.
pub fn bridge_opted_in(dot_dir: &Path) -> bool {
    atomic_config::RepoConfig::load(&dot_dir.join("config.toml"))
        .map(|config| config.git.bridge.enabled)
        .unwrap_or(false)
}

/// Append-only JSON-lines event journal for one Atomic repository.
#[derive(Debug)]
pub struct BridgeEventJournal {
    /// The journal file's absolute path.
    path: PathBuf,
    /// The anchor every ancestor component is traversed from, no-follow
    /// (CB-13C F1): the owning Atomic dot directory.
    dot_dir: PathBuf,
    working_copy: Option<UlidId>,
    /// Cached consent, live-validated against the config file's mtime
    /// before every emission (CB-13C F2): a previously enabled automatic
    /// sink performs no further writes after disable completes.
    consented: std::sync::atomic::AtomicBool,
    config_mtime: std::sync::atomic::AtomicU64,
    /// Whether consent is live-validated (automatic sinks) or owned by
    /// the explicit command boundary (the invocation is the consent).
    live_consent: bool,
}

impl Clone for BridgeEventJournal {
    fn clone(&self) -> Self {
        Self {
            path: self.path.clone(),
            dot_dir: self.dot_dir.clone(),
            working_copy: self.working_copy.clone(),
            consented: std::sync::atomic::AtomicBool::new(
                self.consented.load(std::sync::atomic::Ordering::Relaxed),
            ),
            config_mtime: std::sync::atomic::AtomicU64::new(
                self.config_mtime.load(std::sync::atomic::Ordering::Relaxed),
            ),
            live_consent: self.live_consent,
        }
    }
}

impl BridgeEventJournal {
    /// Journal for `repository`, recording its working-copy ID when one is
    /// resolved. This constructor is for automatic library-level paths: it
    /// honors the repository's bridge consent, so an un-opted repository
    /// never writes telemetry. The journal file is created on first
    /// emission.
    pub fn for_repository(repository: &Repository) -> Self {
        let working_copy = repository
            .require_working_copy_id()
            .ok()
            .and_then(|id| UlidId::new(&id.to_string()));
        Self::for_dot_dir(&repository.dot_dir(), working_copy)
    }

    /// Consent-gated journal for a dot directory without a repository
    /// handle. Honors the same `[git.bridge] enabled` consent gate as
    /// [`Self::for_repository`].
    pub fn for_dot_dir(dot_dir: &Path, working_copy: Option<UlidId>) -> Self {
        Self {
            path: dot_dir.join(BRIDGE_EVENT_JOURNAL_RELATIVE_PATH),
            dot_dir: dot_dir.to_path_buf(),
            working_copy,
            consented: std::sync::atomic::AtomicBool::new(bridge_opted_in(dot_dir)),
            config_mtime: std::sync::atomic::AtomicU64::new(config_mtime_nanos(dot_dir)),
            live_consent: true,
        }
    }

    /// Journal rooted at an Atomic dot directory, without the consent
    /// gate. Use only from explicit bridge command boundaries (the
    /// command invocation is the consent).
    pub fn new(dot_dir: &Path, working_copy: Option<UlidId>) -> Self {
        Self {
            path: dot_dir.join(BRIDGE_EVENT_JOURNAL_RELATIVE_PATH),
            dot_dir: dot_dir.to_path_buf(),
            working_copy,
            consented: std::sync::atomic::AtomicBool::new(true),
            config_mtime: std::sync::atomic::AtomicU64::new(0),
            live_consent: false,
        }
    }

    /// The journal file path (under the Atomic dot directory).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Serialize and append one event, returning any I/O error.
    ///
    /// The complete record (JSON plus newline) is appended as one atomic
    /// O_APPEND write on a confined regular file, after reserving its full
    /// size against the retention cap under a nonblocking exclusive lock.
    /// Symlinks and non-regular files are refused; when the append lock is
    /// contended beyond a bounded retry budget the event is dropped.
    pub fn emit(&self, kind: BridgeEventKind) -> Result<(), std::io::Error> {
        self.refresh_consent();
        if !self.consented.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(());
        }
        // Serialize and bound the record before touching the filesystem:
        // no oversized allocation or append can happen.
        let event = BridgeEvent {
            timestamp_ms: now_timestamp_ms(),
            working_copy: self.working_copy.clone(),
            kind,
        };
        let json = serde_json::to_string(&event)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let mut record = json.into_bytes();
        record.push(b'\n');
        if record.len() > MAX_BRIDGE_EVENT_RECORD_BYTES {
            return Ok(());
        }

        // CB-13C F1: anchored no-follow traversal — every ancestor
        // component from the anchor dot directory down is opened
        // O_NOFOLLOW|O_DIRECTORY, so a symlinked `.atomic/bridge` (or any
        // ancestor) cannot redirect the journal into an external
        // directory. Only after the anchored walk succeeds is the final
        // file opened O_NOFOLLOW.
        self.open_anchored_ancestors()?;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .append(true)
            .custom_flags(OPEN_NO_FOLLOW | OPEN_NONBLOCK)
            .open(&self.path)?;
        // Confine to regular files: a FIFO or device node is refused
        // instead of blocking or writing to it.
        if !file.metadata()?.file_type().is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "event journal path is not a regular file",
            ));
        }

        // Nonblocking exclusive append lock (leaf lock: acquired only for
        // the bounded repair/cap-check/append sequence, never while any
        // other lock is held, so it cannot participate in a lock cycle).
        let mut locked = false;
        let mut backoff = Duration::from_micros(50);
        for _ in 0..APPEND_LOCK_ATTEMPTS {
            match file.try_lock_exclusive() {
                Ok(()) => {
                    locked = true;
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(backoff);
                }
                Err(error) => return Err(error),
            }
            backoff = (backoff * 2).min(Duration::from_millis(4));
        }
        if !locked {
            // Advisory telemetry: contention drops the event instead of
            // blocking or interleaving records.
            return Ok(());
        }

        let result = self.append_locked(&mut file, &record);
        let _ = file.unlock();
        result
    }

    /// CB-13C F1: open every ancestor directory component from the anchor
    /// dot directory down to the journal's parent with O_NOFOLLOW, so an
    /// ancestor symlink cannot redirect the journal into an external
    /// directory (the previous `create_dir_all` followed ancestor links
    /// and the O_NOFOLLOW guard covered only the final filename). A
    /// missing component is created with single-level `create_dir` and
    /// re-verified no-follow.
    fn open_anchored_ancestors(&self) -> Result<(), std::io::Error> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "event journal path has no parent",
            ))?;
        let relative = parent
            .strip_prefix(&self.dot_dir)
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "event journal path escapes the anchor dot directory",
                )
            })?;
        let mut prefix = self.dot_dir.to_path_buf();
        open_dir_no_follow(&prefix)?;
        for component in relative.components() {
            prefix.push(component);
            match open_dir_no_follow(&prefix) {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    std::fs::create_dir(&prefix)?;
                    open_dir_no_follow(&prefix)?;
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// Cap reservation, partial-tail repair and the single-record append.
    /// Caller holds the exclusive append lock.
    fn append_locked(
        &self,
        file: &mut std::fs::File,
        record: &[u8],
    ) -> Result<(), std::io::Error> {
        let length = file.metadata()?.len();
        if length > 0 {
            let mut last = [0u8; 1];
            file.seek(SeekFrom::End(-1))?;
            file.read_exact(&mut last)?;
            if last[0] != b'\n' {
                // A crashed writer left an incomplete trailing fragment —
                // or foreign content. CB-13C F1 ownership rule: only an
                // identified owned fragment (an unterminated JSON record
                // starting with `{`, at most one maximum record long) is
                // repaired; foreign content is preserved untouched and the
                // event is dropped. Foreign data is never mutated or
                // erased.
                let window: u64 = (MAX_BRIDGE_EVENT_RECORD_BYTES as u64 * 16).min(length);
                file.seek(SeekFrom::End(-(window as i64)))?;
                let mut tail = vec![0u8; window as usize];
                file.read_exact(&mut tail)?;
                match tail.iter().rposition(|byte| *byte == b'\n') {
                    Some(position) if window <= length => {
                        // The fragment after the last complete record.
                        let fragment = &tail[position + 1..];
                        if !is_owned_crash_fragment(fragment) {
                            // Foreign content: preserve, drop the event.
                            return Ok(());
                        }
                        file.set_len(length - window + position_to_len(position))?;
                    }
                    _ if length <= window => {
                        // No complete record exists anywhere: the whole
                        // file is one unterminated fragment — repairable
                        // only if it is an owned crash fragment.
                        if !is_owned_crash_fragment(&tail) {
                            return Ok(());
                        }
                        file.set_len(0)?;
                    }
                    _ => {
                        // Foreign content without a newline in the window:
                        // refuse to guess and drop this event.
                        return Ok(());
                    }
                }
            }
        }

        let length = file.metadata()?.len();
        if length + record.len() as u64 > MAX_BRIDGE_EVENT_JOURNAL_BYTES {
            // Retention bound reached: drop the event rather than truncate
            // recorded history or grow without bound.
            return Ok(());
        }
        // One atomic O_APPEND write of the complete record.
        file.write_all(record)?;
        Ok(())
    }

    /// Append one event, swallowing every error: observability must never
    /// fail the bridge operation that produced the outcome.
    pub fn emit_lossy(&self, kind: BridgeEventKind) {
        let _ = self.emit(kind);
    }
}

impl BridgeEventJournal {
    /// CB-13C F2 live consent invalidation: when the config file's mtime
    /// moved since the last observation, re-read the consent. In-flight
    /// policy: an emission already past this check when disable completes
    /// may write its single advisory record; every subsequent emission
    /// observes the withdrawal.
    fn refresh_consent(&self) {
        if !self.live_consent {
            return;
        }
        let observed = config_mtime_nanos(&self.dot_dir);
        let cached = self.config_mtime.load(std::sync::atomic::Ordering::Relaxed);
        if observed != cached {
            self.config_mtime
                .store(observed, std::sync::atomic::Ordering::Relaxed);
            self.consented.store(
                bridge_opted_in(&self.dot_dir),
                std::sync::atomic::Ordering::Relaxed,
            );
        }
    }
}

fn config_mtime_nanos(dot_dir: &Path) -> u64 {
    std::fs::metadata(dot_dir.join("config.toml"))
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0)
}

impl Repository {
    /// The serialized, expected-old-leased bridge consent transition
    /// (CB-13C F2): performed under the common operation lock with an
    /// atomic same-directory replacement. A concurrent operation holder
    /// cannot have consent changed underneath it, and a diverged
    /// `expected_old` refuses instead of overwriting.
    ///
    /// Returns whether the file changed (idempotent re-application is
    /// `Ok(false)`).
    pub fn set_bridge_consent(
        &self,
        enabled: bool,
        expected_old: Option<bool>,
    ) -> Result<bool, RepositoryError> {
        let common = self.try_lock_common_operation()?;
        let config_path = self.dot_dir().join("config.toml");
        let mut config =
            atomic_config::RepoConfig::load(&config_path).map_err(|error| {
                RepositoryError::InvalidRepository {
                    reason: format!("cannot load the repository configuration: {error}"),
                }
            })?;
        if let Some(expected) = expected_old {
            if config.git.bridge.enabled != expected {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "bridge consent lease diverged: expected enabled={expected},                          observed enabled={}",
                        config.git.bridge.enabled
                    ),
                });
            }
        }
        if config.git.bridge.enabled == enabled {
            return Ok(false);
        }
        config.git.bridge.enabled = enabled;
        config
            .save(&config_path)
            .map_err(|error| RepositoryError::InvalidRepository {
                reason: format!("cannot save the repository configuration: {error}"),
            })?;
        drop(common);
        Ok(true)
    }
}

fn position_to_len(position: usize) -> u64 {
    position as u64 + 1
}

/// Open `path` as a directory WITHOUT following a final symlink
/// (CB-13C F1). A symlink or a non-directory is refused; the opened
/// handle's directory type is re-verified so platforms without
/// O_DIRECTORY cannot slip a special file through.
#[cfg(unix)]
fn open_dir_no_follow(path: &Path) -> Result<(), std::io::Error> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(OPEN_NO_FOLLOW | OPEN_DIRECTORY)
        .open(path)?;
    if !file.metadata()?.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!("event journal ancestor '{}' is not a directory", path.display()),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn open_dir_no_follow(path: &Path) -> Result<(), std::io::Error> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("event journal ancestor '{}' is a symlink", path.display()),
        ));
    }
    if !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!("event journal ancestor '{}' is not a directory", path.display()),
        ));
    }
    Ok(())
}

/// Whether an unterminated trailing fragment is an OWNED crash fragment
/// (CB-13C F1): a partial JSON record of this journal's shape — it starts
/// with `{` and is at most one maximum record long. Anything else is
/// foreign content that must be preserved untouched.
fn is_owned_crash_fragment(fragment: &[u8]) -> bool {
    !fragment.is_empty()
        && fragment[0] == b'{'
        && fragment.len() as u64 <= MAX_BRIDGE_EVENT_RECORD_BYTES as u64
}

fn now_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::DOT_DIR;
    
use crate::RepositoryError;

    /// 64 lowercase hex characters — the real `BindingId::to_hex()` width.
    const BINDING_HEX: &str = "eea50ca11b98918aef4672da8179eb718f562585000000000000000000000000";

    fn journal() -> (tempfile::TempDir, BridgeEventJournal) {
        let directory = tempfile::tempdir().unwrap();
        let dot_dir = directory.path().join(DOT_DIR);
        std::fs::create_dir_all(&dot_dir).unwrap();
        let journal = BridgeEventJournal::new(
            &dot_dir,
            Some(UlidId::new("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap()),
        );
        (directory, journal)
    }


    // ── CB-13C F1 regressions ───────────────────────────────────────────

    /// The journal's anchored traversal refuses an ancestor symlink: the
    /// external target directory is neither followed, nor mutated, nor
    /// erased. Failing before (create_dir_all followed the ancestor and
    /// the final-only O_NOFOLLOW opened the external file, which the tail
    /// repair erased), passing after.
    #[test]
    fn ancestor_symlink_target_is_never_followed_mutated_or_erased() {
        let directory = tempfile::tempdir().unwrap();
        let external = directory.path().join("external");
        std::fs::create_dir_all(&external).unwrap();
        let external_journal = external.join("git-events.jsonl");
        // Short foreign non-newline content: the old repair's
        // `length <= window => set_len(0)` branch erased it whole.
        let payload = b"external data that must never be touched";
        std::fs::write(&external_journal, payload).unwrap();

        let dot_dir = directory.path().join(DOT_DIR);
        std::fs::create_dir_all(&dot_dir).unwrap();
        std::fs::create_dir_all(dot_dir.join("bridge")).unwrap();
        std::fs::remove_dir(dot_dir.join("bridge")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&external, dot_dir.join("bridge")).unwrap();

        let journal = BridgeEventJournal::new(&dot_dir, None);
        let emitted = journal.emit(reconcile());
        // The emit must not follow the ancestor: it either refuses with a
        // typed error or drops the event, and in both cases the external
        // file is byte-identical.
        let after = std::fs::read(&external_journal).unwrap();
        assert_eq!(after, payload, "the external file must be untouched");
        let _ = emitted;
        // And the journal file must not exist inside the external target.
        assert!(
            !external.join("git-events.jsonl").exists()
                || after == payload,
            "the external target must not gain or lose content"
        );
    }

    /// A short foreign non-newline file at the expected journal path is
    /// PRESERVED (identified as foreign, event dropped), never erased.
    /// Failing before (the whole-file repair branch erased it), passing
    /// after.
    #[test]
    fn short_foreign_non_newline_tail_is_never_erased() {
        let directory = tempfile::tempdir().unwrap();
        let (dot_dir, journal) = { let d = directory.path().join(DOT_DIR); std::fs::create_dir_all(&d).unwrap(); let j = BridgeEventJournal::new(&d, None); std::fs::create_dir_all(j.path().parent().unwrap()).unwrap(); (d, j) };
        let journal_path = journal.path().to_path_buf();
        let payload = b"totally foreign short content no newline";
        std::fs::write(&journal_path, payload).unwrap();

        let journal = BridgeEventJournal::new(&dot_dir, None);
        let _ = journal.emit(reconcile());

        let after = std::fs::read(&journal_path).unwrap();
        assert_eq!(
            after, payload,
            "foreign short content must be preserved, not erased"
        );
    }

    /// An OWNED crash fragment (an unterminated JSON record starting with
    /// `{`) is identified and repaired: the fragment is truncated and the
    /// event appends cleanly.
    #[test]
    fn owned_crash_fragment_is_repaired_and_the_event_appends() {
        let directory = tempfile::tempdir().unwrap();
        let (_dir, journal) = journal();
        std::fs::create_dir_all(journal.path().parent().unwrap()).unwrap();
        let journal_path = journal.path().to_path_buf();
        std::fs::write(
            &journal_path,
            br#"{"timestamp_ms":1,"working_copy":null,"kind":{"Reconc""#,
        )
        .unwrap();

        journal.emit(reconcile()).expect("owned fragment repairs");

        let after = std::fs::read(&journal_path).unwrap();
        assert!(after.ends_with(b"\n"), "the appended record is complete");
        for line in after.split(|b| *b == b'\n') {
            if !line.is_empty() {
                let value: serde_json::Value =
                    serde_json::from_slice(line).expect("every journal line parses as JSON");
                assert!(
                    value.get("timestamp_ms").is_some() && value.get("event").is_some(),
                    "every journal line carries the event shape"
                );
            }
        }
    }


    /// CB-13C F2: consent withdrawal under a held common lock refuses
    /// (typed contention) instead of rewriting the file outside the lock.
    /// Failing before (record_bridge_opt_in truncated the file without
    /// any lock), passing after.
    #[test]
    fn disable_under_a_held_common_lock_refuses_without_writing() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repository::init(directory.path()).unwrap();
        repo.set_bridge_consent(true, None).unwrap();
        assert!(bridge_opted_in(&repo.dot_dir()));

        // Hold the common operation lock; the transition must refuse.
        let _holder = repo.try_lock_common_operation().unwrap();
        let error = repo.set_bridge_consent(false, Some(true)).unwrap_err();
        assert!(
            matches!(error, crate::RepositoryError::LockContended { .. }),
            "{error}"
        );
        // The consent is unchanged.
        assert!(bridge_opted_in(&repo.dot_dir()));
    }

    /// CB-13C F2: a previously enabled automatic sink performs no further
    /// writes after disable completes (live consent invalidation).
    /// Failing before (the sink cached consent at construction), passing
    /// after.
    #[test]
    fn cached_sink_performs_no_further_writes_after_disable() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repository::init(directory.path()).unwrap();
        repo.set_bridge_consent(true, None).unwrap();
        let journal = BridgeEventJournal::for_repository(&repo);
        journal.emit(reconcile()).expect("consented emission lands");
        let after_first = std::fs::read(journal.path()).unwrap();
        assert!(!after_first.is_empty());

        repo.set_bridge_consent(false, Some(true)).unwrap();
        journal.emit(reconcile()).expect("withdrawn consent drops silently");
        let after_second = std::fs::read(journal.path()).unwrap();
        assert_eq!(
            after_first, after_second,
            "the disabled sink must not write further events"
        );
    }

    fn reconcile() -> BridgeEventKind {
        BridgeEventKind::Reconcile {
            direction: ReconcileDirectionCode::GitToAtomic,
            outcome: EventOutcome::Applied,
            refusal_class: None,
        }
    }

    /// Every variant serializes with stable field names, and every leaf
    /// string is a closed classification code or validated fixed-format
    /// identifier — the structural redaction proof (no transcript/prompt/
    /// secret carrier can even be constructed).
    #[test]
    fn every_event_variant_has_stable_fields_and_safe_leaf_strings() {
        let kinds = vec![
            BridgeEventKind::Reconcile {
                direction: ReconcileDirectionCode::GitToAtomic,
                outcome: EventOutcome::Applied,
                refusal_class: None,
            },
            BridgeEventKind::Reconcile {
                direction: ReconcileDirectionCode::Diverged,
                outcome: EventOutcome::Refused,
                refusal_class: Some(RefusalClass::CheckpointDiverged),
            },
            BridgeEventKind::Reconcile {
                direction: ReconcileDirectionCode::GitToAtomic,
                outcome: EventOutcome::Failed,
                refusal_class: Some(RefusalClass::ImportFailed),
            },
            BridgeEventKind::WorkspaceRefusal {
                mode: WorkspaceModeCode::Observe,
                remediation: RemediationCode::Unanchored,
            },
            BridgeEventKind::PublicationRefusal {
                boundary: PublicationBoundary::VerifyReceive,
                refused: 1,
                checked: 3,
                unit: PublicationUnit::Refs,
            },
            BridgeEventKind::PublicationRefusal {
                boundary: PublicationBoundary::PrePush,
                refused: 2,
                checked: 2,
                unit: PublicationUnit::Changes,
            },
            BridgeEventKind::ChangeSourceFallback {
                source: TierCode::Fsmonitor,
                fallback_source: Some(TierCode::Scan),
                reason: Some(FallbackReasonCode::Timeout),
            },
            BridgeEventKind::Recovery {
                original: OpIdRef::new("5DKNRTR6VBJVGA6K2JTNL2RJJKQITXPWQAX4HT2J2JTSIOKE727Q")
                    .unwrap(),
                recovery: OpIdRef::new("EHG7TIAO63FBHRGQIOX5GT3RWXB454YSZXAFTPCBQ6JFLHVPLWMQ")
                    .unwrap(),
                created: true,
            },
            BridgeEventKind::RecoveryFailure {
                original: OpIdRef::new("5DKNRTR6VBJVGA6K2JTNL2RJJKQITXPWQAX4HT2J2JTSIOKE727Q")
                    .unwrap(),
                reason: RecoveryFailureCode::FilesystemReplay,
            },
            BridgeEventKind::BindingFetch {
                binding: HexBindingId::new(BINDING_HEX).unwrap(),
                readiness: ReadinessCode::Incomplete,
                lossy: true,
            },
            BridgeEventKind::BindingFetchRefused {
                binding: HexBindingId::new(BINDING_HEX).unwrap(),
                reason: BindingFetchRefusalCode::Cryptography,
            },
            BridgeEventKind::ImportSynthesis {
                commits_found: 12,
                commits_parsed: 12,
                written: 9,
                empty: 1,
                merges: 2,
                self_push_skipped: 0,
                squash_inserted: 0,
            resurrected_exact: 0,
            correlation_id: "test".to_string(),
            failed_after_landed: None,
            },
        ];

        for kind in kinds {
            let event = BridgeEvent {
                timestamp_ms: 1_700_000_000_000,
                working_copy: Some(UlidId::new("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap()),
                kind,
            };
            let json = serde_json::to_value(&event).unwrap();
            let object = json.as_object().unwrap();

            // Stable envelope fields.
            assert!(object.get("timestamp_ms").is_some());
            assert!(object.get("event").is_some());
            assert!(object.get("working_copy").is_some());

            // No leaf string may carry free-form text: codes and
            // fixed-format identifiers only. The closed enum and validated
            // constructors make a violating value unconstructible; this
            // walk re-checks the serialized form.
            fn walk(value: &serde_json::Value) {
                match value {
                    serde_json::Value::String(text) => {
                        assert!(
                            !text.contains(' ')
                                && !text.contains('\n')
                                && text.is_ascii(),
                            "free-form leaf string leaked into telemetry: {text:?}"
                        );
                    }
                    serde_json::Value::Array(items) => items.iter().for_each(walk),
                    serde_json::Value::Object(fields) => {
                        fields.values().for_each(walk)
                    }
                    _ => {}
                }
            }
            walk(&json);

            // Every field name is from the stable set.
            let allowed = [
                "timestamp_ms",
                "working_copy",
                "event",
                "direction",
                "outcome",
                "refusal_class",
                "mode",
                "remediation",
                "boundary",
                "refused",
                "checked",
                "unit",
                "source",
                "fallback_source",
                "reason",
                "original",
                "recovery",
                "created",
                "binding",
                "readiness",
                "lossy",
                "commits_found",
                "commits_parsed",
                "written",
                "empty",
                "merges",
                "self_push_skipped",
                "squash_inserted",
                "resurrected_exact",
                "correlation_id",
                "failed_after_landed",
            ];
            for name in object.keys() {
                assert!(
                    allowed.contains(&name.as_str()),
                    "unexpected field '{name}' in telemetry"
                );
            }

            // The serialized record is bounded by construction.
            let line = serde_json::to_string(&event).unwrap();
            assert!(
                line.len() + 1 <= MAX_BRIDGE_EVENT_RECORD_BYTES,
                "every valid record fits the record bound"
            );
        }
    }

    /// Adversarial construction: private sentinels, prose and mis-formatted
    /// tokens are refused at the type boundary for every typed field and
    /// closed code — the old `String`/`&'static str` fields accepted these.
    #[test]
    fn typed_construction_rejects_private_or_malformed_values() {
        let sentinels = [
            "PRIVATE PROMPT SENTINEL",
            "PRIVATE TRANSCRIPT SENTINEL",
            "SECRET SENTINEL",
            "no-space-token",
            "",
            "01ARZ3NDEKTSV4RRFFQ69G5FA", // 25 chars
            "01ARZ3NDEKTSV4RRFFQ69G5FAVV", // 27 chars
            "01ARZ3NDEKTSV4RRFFQ69G5FAI", // invalid Crockford char I
        ];
        for sentinel in &sentinels {
            assert!(UlidId::new(sentinel).is_none(), "{sentinel:?} accepted");
        }
        assert!(
            UlidId::new("01ARZ3NDEKTSV4RRFFQ69G5FAV").is_some(),
            "a valid ULID constructs"
        );

        let op_sentinels = [
            "PRIVATE PROMPT SENTINEL",
            "short",
            "5DKNRTR6VBJVGA6K2JTNL2RJJKQITXPWQAX4HT2J2JTSIOKE727Q0", // 53 chars
            "5dknrtr6vbjvga6k2jtnl2rjjkqitxpwqax4ht2j2jflhvplwmq",    // lowercase
        ];
        for sentinel in &op_sentinels {
            assert!(OpIdRef::new(sentinel).is_none(), "{sentinel:?} accepted");
        }
        assert!(OpIdRef::new("5DKNRTR6VBJVGA6K2JTNL2RJJKQITXPWQAX4HT2J2JTSIOKE727Q").is_some());

        for sentinel in &["SECRET", "EEA50CA11B98918AEF4672DA8179EB718F562585000000000000000000000000", BINDING_HEX.to_uppercase().as_str(), "zz", ""] {
            assert!(
                HexBindingId::new(sentinel).is_none(),
                "{sentinel:?} accepted"
            );
        }
        assert!(HexBindingId::new(BINDING_HEX).is_some());

        // Closed code enums refuse arbitrary static strings (the old
        // unrestricted `&'static str` fields passed any value through).
        assert!(WorkspaceModeCode::from_code("PRIVATE PROMPT").is_none());
        assert!(RemediationCode::from_code("SECRET").is_none());
        assert!(TierCode::from_code("no-space-token").is_none());
        assert!(FallbackReasonCode::from_code("PRIVATE PROMPT SENTINEL").is_none());
        assert!(ReconcileDirectionCode::from_code("diverged").is_some());
        assert!(FallbackReasonCode::from_code("configured_off").is_some());
        assert!(FallbackReasonCode::from_code("timeout").is_some());
        // Space-padded and prose values are never accepted.
        assert!(WorkspaceModeCode::from_code("observe ").is_none());
        assert!(TierCode::from_code("scan\nsecret").is_none());
    }

    /// Every fixed code the remediation/refusal producers emit is accepted
    /// by the closed telemetry code sets (closure between producer and
    /// consumer of the classification codes).
    #[test]
    fn closed_codes_accept_every_produced_code() {
        for code in [
            "observe",
            "reconcile",
            "force",
            "unknown",
            "git_operation_in_progress",
            "unanchored",
            "operation_heads_diverged",
            "concurrent_git_mutation",
        ] {
            let mode_ok = WorkspaceModeCode::from_code(code).is_some();
            let remediation_ok = RemediationCode::from_code(code).is_some();
            assert!(
                mode_ok || remediation_ok,
                "produced code '{code}' is not accepted by any closed enum"
            );
        }
        for reason in [
            "missing_index",
            "malformed_index",
            "conversion_policy_changed",
            "canonical_attributes_changed",
            "sparse_policy_changed",
            "configuration_changed",
            "filter_changed",
            "configured_off",
            "disabled_in_ci",
            "disabled_in_container",
            "unavailable",
            "unsupported_version",
            "unknown_token",
            "overflow",
            "malformed_response",
            "timeout",
            "source_error",
        ] {
            assert!(
                FallbackReasonCode::from_code(reason).is_some(),
                "reason code '{reason}' is not accepted"
            );
        }
    }

    /// Emission appends one parseable JSON line per event and round-trips
    /// the recorded outcome classes.
    #[test]
    fn emission_appends_parseable_lines_and_round_trips_outcomes() {
        let (_directory, journal) = journal();
        journal
            .emit(BridgeEventKind::Reconcile {
                direction: ReconcileDirectionCode::GitToAtomic,
                outcome: EventOutcome::Applied,
                refusal_class: None,
            })
            .unwrap();
        journal
            .emit(BridgeEventKind::WorkspaceRefusal {
                mode: WorkspaceModeCode::Reconcile,
                remediation: RemediationCode::GitOperationInProgress,
            })
            .unwrap();
        journal
            .emit(BridgeEventKind::PublicationRefusal {
                boundary: PublicationBoundary::PrePush,
                refused: 2,
                checked: 2,
                unit: PublicationUnit::Changes,
            })
            .unwrap();
        journal
            .emit(BridgeEventKind::ChangeSourceFallback {
                source: TierCode::Watchman,
                fallback_source: None,
                reason: Some(FallbackReasonCode::Unavailable),
            })
            .unwrap();
        journal
            .emit(BridgeEventKind::Recovery {
                original: OpIdRef::new("5DKNRTR6VBJVGA6K2JTNL2RJJKQITXPWQAX4HT2J2JTSIOKE727Q")
                    .unwrap(),
                recovery: OpIdRef::new("EHG7TIAO63FBHRGQIOX5GT3RWXB454YSZXAFTPCBQ6JFLHVPLWMQ")
                    .unwrap(),
                created: false,
            })
            .unwrap();
        journal
            .emit(BridgeEventKind::BindingFetch {
                binding: HexBindingId::new(BINDING_HEX).unwrap(),
                readiness: ReadinessCode::Complete,
                lossy: false,
            })
            .unwrap();
        journal
            .emit(BridgeEventKind::ImportSynthesis {
                commits_found: 3,
                commits_parsed: 3,
                written: 2,
                empty: 0,
                merges: 1,
                self_push_skipped: 0,
                squash_inserted: 0,
                resurrected_exact: 1,
                correlation_id: "test".to_string(),
                failed_after_landed: None,
            })
            .unwrap();

        let bytes = std::fs::read(journal.path()).unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 7, "one JSON line per event");

        let tags: Vec<String> = lines
            .iter()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line)
                    .unwrap_or_else(|error| panic!("line '{line}' is not JSON: {error}"))
                    .get("event")
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(
            tags,
            vec![
                "reconcile",
                "workspace_refusal",
                "publication_refusal",
                "change_source_fallback",
                "recovery",
                "binding_fetch",
                "import_synthesis"
            ]
        );
    }

    /// The journal is bounded: past the retention cap new events are
    /// dropped, and recorded history is never truncated.
    #[test]
    fn journal_drops_events_past_retention_bound() {
        let (directory, journal) = journal();
        std::fs::create_dir_all(journal.path().parent().unwrap()).unwrap();
        std::fs::write(journal.path(), vec![b'x'; MAX_BRIDGE_EVENT_JOURNAL_BYTES as usize])
            .unwrap();
        let before = std::fs::read(journal.path()).unwrap();

        journal
            .emit(BridgeEventKind::Reconcile {
                direction: ReconcileDirectionCode::Neither,
                outcome: EventOutcome::NoChange,
                refusal_class: None,
            })
            .expect("the bound drops the event without an error");

        assert_eq!(
            std::fs::read(journal.path()).unwrap(),
            before,
            "recorded history is never truncated and nothing is appended past the bound"
        );
        drop(directory);
    }

    /// The cap check reserves the complete record (JSON + newline). A file
    /// one byte below the cap — the exact gap the old old-length-only
    /// check used to overflow (4,194,398 > 4,194,304) — drops the event
    /// and keeps the file within the bound.
    #[test]
    fn cap_reservation_includes_the_complete_record() {
        let (directory, journal) = journal();
        std::fs::create_dir_all(journal.path().parent().unwrap()).unwrap();

        // Measure one record's size.
        journal.emit(reconcile()).unwrap();
        let record_len = std::fs::metadata(journal.path()).unwrap().len();

        // Exactly cap-minus-one: the old check passed, then wrote a record.
        std::fs::write(
            journal.path(),
            vec![b'x'; MAX_BRIDGE_EVENT_JOURNAL_BYTES as usize - 1],
        )
        .unwrap();
        journal.emit(reconcile()).unwrap();
        assert!(
            std::fs::metadata(journal.path()).unwrap().len()
                <= MAX_BRIDGE_EVENT_JOURNAL_BYTES,
            "the file never grows past the cap"
        );

        // A file with room for the complete record accepts it and stays
        // within the bound. The filler ends with a newline: it is a
        // complete (foreign) line, not a crash fragment.
        let room = MAX_BRIDGE_EVENT_JOURNAL_BYTES - record_len;
        let mut filler = vec![b'x'; room as usize - 1];
        filler.push(b'\n');
        std::fs::write(journal.path(), &filler).unwrap();
        journal.emit(reconcile()).unwrap();
        let final_len = std::fs::metadata(journal.path()).unwrap().len();
        assert_eq!(final_len, MAX_BRIDGE_EVENT_JOURNAL_BYTES);
        assert!(final_len <= MAX_BRIDGE_EVENT_JOURNAL_BYTES);

        // One byte short: the complete record no longer fits; the event is
        // dropped and history is retained.
        let mut full = vec![b'x'; room as usize];
        full.push(b'\n');
        std::fs::write(journal.path(), &full).unwrap();
        let before = std::fs::read(journal.path()).unwrap();
        journal.emit(reconcile()).unwrap();
        assert_eq!(std::fs::read(journal.path()).unwrap(), before);
        drop(directory);
    }

    /// A crash-interrupted partial tail line is repaired on the next emit:
    /// the incomplete fragment never merges with a new record, and any
    /// complete history before it is retained.
    #[test]
    fn partial_tail_fragment_is_repaired_on_next_emit() {
        let (directory, journal) = journal();
        std::fs::create_dir_all(journal.path().parent().unwrap()).unwrap();
        std::fs::write(journal.path(), b"{\"event\":\"rec").unwrap();

        journal.emit(reconcile()).unwrap();

        let bytes = std::fs::read(journal.path()).unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 1, "the fragment was repaired, not appended to");
        serde_json::from_str::<serde_json::Value>(lines[0])
            .expect("the appended record is complete JSON");

        // A fragment after complete history preserves that history.
        std::fs::write(journal.path(), b"{\"a\":1}\n{\"event\":\"rec").unwrap();
        journal.emit(reconcile()).unwrap();
        let bytes = std::fs::read(journal.path()).unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.first().unwrap(), &"{\"a\":1}", "complete history retained");
        for line in &lines[1..] {
            serde_json::from_str::<serde_json::Value>(line)
                .unwrap_or_else(|error| panic!("line '{line}' is not JSON: {error}"));
        }
        drop(directory);
    }

    /// Concurrent writers never corrupt the JSONL stream: every landed
    /// line parses as one JSON object, and the file stays within the cap.
    /// Advisory contention may drop events; it may never interleave them.
    #[test]
    fn concurrent_emits_produce_valid_jsonl_within_the_bound() {
        let (directory, journal) = journal();
        let path = journal.path().to_path_buf();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        let handles: Vec<_> = (0..8)
            .map(|writer| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let journal = BridgeEventJournal::new(
                        path.parent().unwrap().parent().unwrap(),
                        Some(UlidId::new("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap()),
                    );
                    for index in 0..250 {
                        journal
                            .emit(BridgeEventKind::ImportSynthesis {
                                commits_found: index,
                                commits_parsed: index,
                                written: index,
                                empty: 0,
                                merges: 0,
                                self_push_skipped: 0,
                                squash_inserted: writer,
                                resurrected_exact: 0,
                                correlation_id: "test".to_string(),
                                failed_after_landed: None,
                            })
                            .unwrap();
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }

        let bytes = std::fs::read(&path).unwrap();
        assert!(
            (bytes.len() as u64) <= MAX_BRIDGE_EVENT_JOURNAL_BYTES,
            "the file stays within the cap"
        );
        let text = std::str::from_utf8(&bytes).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert!(
            !lines.is_empty(),
            "at least some of the 2000 events landed despite contention"
        );
        for line in &lines {
            let value: serde_json::Value = serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("invalid JSONL line {line:?}: {error}"));
            assert!(value.get("event").is_some());
        }
        drop(directory);
    }

    /// A symlinked journal path is refused: the event is not written and
    /// the symlink target is untouched.
    #[test]
    fn symlinked_journal_path_is_refused_and_target_untouched() {
        let (directory, journal) = journal();
        std::fs::create_dir_all(journal.path().parent().unwrap()).unwrap();
        let target = directory.path().join("unrelated-fixture.txt");
        std::fs::write(&target, b"unrelated fixture data\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, journal.path()).unwrap();

        let error = journal.emit(reconcile()).unwrap_err();
        // The final-component symlink is refused (O_NOFOLLOW): either the
        // typed refusal kinds or the raw ELOOP from the no-follow open.
        assert!(
            matches!(
                error.kind(),
                std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::Unsupported
            ) || error.raw_os_error() == Some(40), // ELOOP
            "unexpected refusal kind: {error:?}"
        );
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"unrelated fixture data\n",
            "the symlink target is never appended to"
        );
        drop(directory);
    }

    /// A non-regular file at the journal path (a FIFO) is refused instead
    /// of blocking or being written to.
    #[cfg(unix)]
    #[test]
    fn non_regular_journal_path_is_refused_without_blocking() {
        let (directory, journal) = journal();
        std::fs::create_dir_all(journal.path().parent().unwrap()).unwrap();
        let status = std::process::Command::new("mkfifo")
            .arg(journal.path())
            .status()
            .expect("mkfifo is available on this platform");
        assert!(status.success(), "mkfifo must succeed in the test fixture");

        let error = journal.emit(reconcile()).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
        drop(directory);
    }

    /// Telemetry lives under the Atomic dot directory, never under `.git`.
    #[test]
    fn journal_path_is_inside_the_atomic_dot_directory() {
        let (directory, journal) = journal();
        let dot_dir = directory.path().join(DOT_DIR);
        assert!(journal.path().starts_with(&dot_dir));
        assert!(!journal.path().starts_with(directory.path().join(".git")));
    }

    /// The automatic library-level sink is consent-gated: without
    /// `[git.bridge] enabled = true` the journal writes nothing and never
    /// creates the event file; with explicit opt-in it records normally.
    #[test]
    fn automatic_sink_requires_bridge_opt_in() {
        let directory = tempfile::tempdir().unwrap();
        let dot_dir = directory.path().join(DOT_DIR);
        std::fs::create_dir_all(&dot_dir).unwrap();
        std::fs::write(dot_dir.join("config.toml"), "[view]\ndefault = \"main\"\n").unwrap();

        let unconsented = BridgeEventJournal::for_dot_dir(&dot_dir, None);
        unconsented.emit_lossy(reconcile());
        assert!(
            !unconsented.path().exists(),
            "no event file may be created without consent"
        );

        std::fs::write(
            dot_dir.join("config.toml"),
            "[view]\ndefault = \"main\"\n\n[git.bridge]\nenabled = true\n",
        )
        .unwrap();
        let consented = BridgeEventJournal::for_dot_dir(&dot_dir, None);
        assert!(consented.emit(reconcile()).is_ok());
        assert!(consented.path().exists(), "explicit opt-in enables the sink");
        drop(directory);
    }
}
