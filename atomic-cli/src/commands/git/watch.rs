//! CB-13D (RFC §11.2 rules 3–7): the optional metadata-only bridge watch
//! daemon.
//!
//! The daemon is a latency accelerator, never the guarantee: correctness
//! comes from command-boundary reconciliation, and a stopped or killed
//! daemon must never change the next command's outcome. It watches the Git
//! metadata paths resolved through Git's own common-dir/worktree APIs,
//! treats every observed difference as a wake-up hint, waits for
//! quiescence (RFC §11.2 rule 3: at least 250 ms silence and absence of
//! Git index/ref locks and sequence operations), and then runs exactly the
//! shared workspace transaction path a CLI command uses — under a
//! metadata-only effect budget. It never materializes files and never
//! moves Git refs: an export-classified run is refused with a
//! notice/remediation instead (RFC §11.2 rule 4).
//!
//! Self-event suppression (RFC §11.2 rule 5) is journal/state evidence,
//! not a blind time window: the daemon re-observes the metadata truth
//! after each of its own passes and records that as the baseline, so its
//! own writes are never re-reported as external transitions; a bridge
//! operation still in flight (durable but not yet verified in the
//! operation journal) defers the next reactive pass. The Watchman path is
//! additionally bracketed with the balanced `atomic-bridge` state
//! enter/leave shared with every bridge transaction.
//!
//! Daemon death (kill, disconnect, sleep/resume) leaves only recoverable
//! metadata operations behind: incomplete operation heads are gated and
//! recovered by the ordinary writable-open path, and the next command
//! boundary observes the truth directly.

use std::path::Path;
use std::time::{Duration, Instant};

use clap::Parser;

use atomic_agent::turn::session::SessionStore;
use atomic_repository::repository::{observe_git_metadata, GitQuiescence, ReconcileEffectBudget};

use super::bridge::{
    bridge_config, reconcile_transaction_budgeted, WatchNoticeKind, WatchNoticeSink,
};
use super::observation::{observe_git, GitObservation, HeadObservation};
use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_info, print_warning};

/// Bounded metadata re-observation interval for the hint loop. Polling is
/// the always-available hint source; a hint is only a wake-up — truth is
/// always re-enumerated through Git APIs, so a missed or duplicated poll
/// cannot corrupt anything.
pub(crate) const DEFAULT_POLL_INTERVAL_MS: u64 = 500;
/// Enforced lower bound for the poll interval; a faster loop buys nothing
/// and only burns the quiescence window.
pub(crate) const MIN_POLL_INTERVAL_MS: u64 = 100;

/// Run the optional metadata-only bridge watch daemon (CB-13D, RFC §11.2).
///
/// The daemon is opt-in only: `[git.bridge] enabled = true` **and**
/// `[git.bridge.watch] enabled = true` must both be recorded in the
/// repository configuration. Without the daemon, command boundaries fully
/// reconcile — nothing else changes.
#[derive(Parser, Debug, Default)]
#[command(name = "watch")]
pub struct Watch {
    /// Run exactly one reactive pass against the current metadata state
    /// and exit (equivalence/fault-corpus mode). Without this flag the
    /// daemon loops until interrupted.
    #[arg(long)]
    pub once: bool,
    /// Metadata re-observation interval in milliseconds. Values are
    /// clamped up to the enforced minimum; the reactive silence window
    /// comes from `[git.bridge.watch] quiet_ms` and is floored at the
    /// RFC's 250 ms.
    #[arg(long, default_value_t = DEFAULT_POLL_INTERVAL_MS)]
    pub poll_ms: u64,
}

/// One observed Git metadata truth state, compared for external-change
/// hints. Every field is re-enumerated through Git APIs (`observe_git`
/// resolves paths through libgit2 and reads HEAD/index/refs state); the
/// comparison itself carries no event semantics — a difference is only a
/// wake-up hint.
///
/// The Git-owned transaction state (`busy_signature`) is part of the
/// observed truth (review R6): a new lock or sequence marker with
/// unchanged HEAD/index/ref digests must still fire a hint, because the
/// daemon must surface the unsafe state to active sessions even when no
/// digest moved.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct MetadataHint {
    head_oid: Option<String>,
    head_symref: Option<String>,
    head_tree: Option<String>,
    index_digest: Option<String>,
    refs_digest: Option<String>,
    /// Signature of the Git-owned transaction state (lock/sequence
    /// evaluation), `None` when quiescent.
    busy_signature: Option<String>,
}

impl MetadataHint {
    /// CB-13D ::24 R3: the post-pass baseline taken INSIDE the verified
    /// boundary — the HEAD/symref/tree/index identities bind to the
    /// checkpoint the pass actually reconciled, so an external commit
    /// landing after the verified state is never acknowledged as the
    /// daemon's own (it differs and fires the next hint). Only the refs
    /// digest is sampled fresh: the metadata-only daemon never moves refs,
    /// so the checkpoint cannot bind it, and an external ref movement must
    /// still fire.
    pub(crate) fn verified_baseline(root: &Path) -> CliResult<Self> {
        let fresh = Self::observe(root)?;
        let verified = super::bridge::read_workspace_metadata(root).ok().flatten();
        Ok(match verified {
            Some(checkpoint) => Self {
                head_oid: Some(checkpoint.git_head.clone()),
                head_symref: checkpoint.git_head_symref.clone(),
                head_tree: Some(checkpoint.git_tree.clone()),
                index_digest: checkpoint.git_index_digest.clone(),
                refs_digest: fresh.refs_digest,
                busy_signature: None,
            },
            None => fresh,
        })
    }

    /// Observe the current Git metadata truth through Git's own APIs.
    pub(crate) fn observe(root: &Path) -> CliResult<Self> {
        let observation = observe_git(root).map_err(|error| CliError::StaleBaseline {
            report: format!("bridge watch cannot observe Git metadata truth: {error}"),
        })?;
        let base = match observation {
            GitObservation::NoGit { .. } => return Ok(Self::default()),
            GitObservation::Repository(repository) => Self {
                head_oid: repository.head.oid().map(|oid| oid.to_string()),
                head_symref: match &repository.head {
                    HeadObservation::Attached { symref, .. } => Some(symref.clone()),
                    _ => None,
                },
                head_tree: repository.head_tree_oid.map(|oid| oid.to_string()),
                index_digest: Some(repository.index.canonical_digest.0.clone()),
                refs_digest: Some(repository.refs_digest.0.clone()),
                busy_signature: None,
            },
        };
        // The Git-owned transaction state is truth the digests cannot see:
        // a lock or marker file's presence changes what the bridge may do
        // even when every digest is unchanged (review R6).
        let metadata = observe_git_metadata(root).map_err(|error| CliError::StaleBaseline {
            report: format!("bridge watch cannot observe Git state for the hint: {error}"),
        })?;
        Ok(Self {
            busy_signature: match GitQuiescence::evaluate(&metadata) {
                GitQuiescence::Quiescent { .. } => None,
                GitQuiescence::Busy { reason, detail } => Some(format!("{reason}: {detail}")),
            },
            ..base
        })
    }

    /// Whether this state differs from `baseline` — i.e. whether a hint
    /// fired. Equal states (including the daemon's own prior writes)
    /// never trigger a reactive pass.
    pub(crate) fn differs_from(&self, baseline: &Self) -> bool {
        self != baseline
    }
}

impl Command for Watch {
    fn run(&self) -> CliResult<()> {
        let root = crate::commands::find_repository_root()?;
        run_watch(&root, self.once, self.poll_ms)
    }
}

/// Whether a bridge operation is still in flight in the operation journal
/// (durable but not yet verified). RFC §11.2 rule 5: events attributable to
/// an in-flight operation are suppressed — the native path uses this
/// journal evidence instead of a time window. The daemon defers reacting
/// until the journal shows every repository- and working-copy-scope head
/// verified or recovered; the ordinary writable-open path gates incomplete
/// heads either way, so deferral cannot mask a real fault.
///
/// Review R4: the check covers *both* scopes and every head (not only the
/// single repository head): a working-copy head stuck mid-verification
/// after a daemon crash is just as much un-attributed in-flight work as a
/// repository head, and a diverged head set must defer too.
pub(crate) fn head_operation_in_flight(
    repository: &atomic_repository::Repository,
) -> CliResult<Option<String>> {
    use atomic_core::operation::OperationScope;
    use atomic_repository::repository::OperationVerificationState;

    let repository_log = repository
        .operation_log(OperationScope::Repository, Some(1), false)
        .map_err(CliError::Repository)?;
    let in_flight = repository_log
        .entries
        .iter()
        .find(|entry| entry.is_head)
        .filter(|entry| !matches!(entry.verification, OperationVerificationState::Verified))
        .map(|entry| entry.operation.id().to_string());
    if in_flight.is_some() {
        return Ok(in_flight);
    }
    // The working-copy scope: the daemon runs against this worktree, so its
    // own prior pass's operation (crashed before verification) lives here.
    let working_copy = repository
        .require_working_copy_id()
        .map_err(CliError::Repository)?;
    let working_copy_log = repository
        .operation_log(OperationScope::WorkingCopy(working_copy), Some(1), false)
        .map_err(CliError::Repository)?;
    Ok(working_copy_log
        .entries
        .iter()
        .find(|entry| entry.is_head)
        .filter(|entry| !matches!(entry.verification, OperationVerificationState::Verified))
        .map(|entry| entry.operation.id().to_string()))
}

/// Wait until the Git metadata state is quiescent (RFC §11.2 rule 3: no
/// Git index lock, no ref locks, no sequence markers) and has stayed
/// unchanged for at least `quiet`. Any change or busy state restarts the
/// silence window. Returns the final observation on success.
pub(crate) fn wait_for_quiescence(
    root: &Path,
    quiet: Duration,
    poll: Duration,
    deadline: Option<Instant>,
) -> CliResult<()> {
    let mut last_change = Instant::now();
    let mut stable = MetadataHint::observe(root)?;
    loop {
        if let Some(deadline) = deadline {
            if Instant::now() >= deadline {
                return Err(CliError::StaleBaseline {
                    report: "bridge watch: quiescence deadline reached while a Git-owned \
                             transaction was still in flight; deferring to the next command \
                             boundary"
                        .to_string(),
                });
            }
        }
        let observation = observe_git_metadata(root).map_err(|error| CliError::StaleBaseline {
            report: format!("bridge watch cannot observe Git state for quiescence: {error}"),
        })?;
        match GitQuiescence::evaluate(&observation) {
            GitQuiescence::Quiescent { .. } => {}
            GitQuiescence::Busy { reason, detail } => {
                log::warn!("bridge watch: not quiescent ({reason}): {detail}; waiting");
                last_change = Instant::now();
                std::thread::sleep(poll);
                stable = MetadataHint::observe(root)?;
                continue;
            }
        }
        let current = MetadataHint::observe(root)?;
        if current != stable {
            stable = current;
            last_change = Instant::now();
        }
        if last_change.elapsed() >= quiet {
            return Ok(());
        }
        std::thread::sleep(poll);
    }
}

/// Outcome of one reactive pass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PassOutcome {
    /// The shared transaction path was invoked. The observed metadata truth
    /// immediately after the pass is carried so the caller re-baselines on
    /// the verified observation, not on an arbitrary later sample (review
    /// R3): an external change that lands after the pass is still a hint.
    Attempted {
        /// Truth observed right after the reconcile completed.
        observed: MetadataHint,
    },
    /// No external hint: nothing to do.
    NoChange,
    /// An external hint fired but the pass deferred (unsafe state, pending
    /// recovery, or an in-flight bridge operation); the notice was surfaced
    /// instead.
    Deferred,
}

/// One reactive pass: optionally gate on an external-change hint, wait for
/// quiescence, reconcile under the metadata-only budget, and report the
/// outcome.
///
/// `hint_baseline` is the journal-evidence gate: a pass runs only when the
/// observed truth differs from the baseline the caller last accounted for.
/// `None` (single-pass mode) always attempts: the shared transaction
/// classifier itself decides and a fully-aligned workspace is a no-op.
/// `notice_baseline` de-duplicates unsafe-state notices: a deferral that
/// has already been noticed for the *same* observed truth stays silent,
/// while any new truth notices again. A deferral never consumes the hint
/// — the caller keeps the hint baseline so the pending reconcile is
/// retried once Git releases the state.
fn reactive_pass(
    root: &Path,
    hint_baseline: Option<&MetadataHint>,
    notice_baseline: Option<&MetadataHint>,
    quiet: Duration,
    poll: Duration,
    sessions: &SessionStore,
) -> CliResult<PassOutcome> {
    let current = MetadataHint::observe(root)?;
    if let Some(baseline) = hint_baseline {
        if !current.differs_from(baseline) {
            return Ok(PassOutcome::NoChange);
        }
    }
    // The hint fired. Unsafe states surface, never act (RFC §11.2 rule 6):
    // an evaluation that is not quiescent right now becomes a notice and
    // the daemon waits instead of repairing.
    let observation = observe_git_metadata(root).map_err(|error| CliError::StaleBaseline {
        report: format!("bridge watch cannot observe Git state: {error}"),
    })?;
    if let GitQuiescence::Busy { reason, detail } = GitQuiescence::evaluate(&observation) {
        let should_notice = notice_baseline.is_none_or(|noticed| current.differs_from(noticed));
        if should_notice {
            notify_active_sessions(
                sessions,
                root,
                WatchNoticeKind::UnsafeState,
                &format!("Git-owned state in flight ({reason}): {detail}"),
                "wait for Git to finish, then run 'atomic git bridge reconcile' if needed",
            );
        }
        return Ok(PassOutcome::Deferred);
    }
    wait_for_quiescence(root, quiet, poll, None)?;
    // The prior-operation journal evidence AND the pending-recovery gate
    // (review R1): the budgeted open refuses before any recovery runs, and
    // a bridge operation still in flight defers this pass entirely; its own
    // completion will be part of the re-baselined truth.
    let repo = match atomic_repository::Repository::open_with_budget(
        root,
        atomic_repository::repository::ReconcileEffectBudget::MetadataOnly,
    ) {
        Ok(repo) => repo,
        Err(atomic_repository::RepositoryError::ReactiveDeferred { detail }) => {
            let should_notice = notice_baseline.is_none_or(|noticed| current.differs_from(noticed));
            if should_notice {
                notify_active_sessions(
                    sessions,
                    root,
                    WatchNoticeKind::UnsafeState,
                    &format!("reactive reconcile deferred: {detail}"),
                    "run 'atomic git bridge reconcile' explicitly to complete the pending work",
                );
            }
            return Ok(PassOutcome::Deferred);
        }
        Err(error) => return Err(CliError::Repository(error)),
    };
    if let Some(operation) = head_operation_in_flight(&repo)? {
        drop(repo);
        let should_notice = notice_baseline.is_none_or(|noticed| current.differs_from(noticed));
        if should_notice {
            notify_active_sessions(
                sessions,
                root,
                WatchNoticeKind::UnsafeState,
                &format!(
                    "bridge operation {operation} is still in flight; reactive reconcile deferred"
                ),
                "run 'atomic op log' to inspect the operation",
            );
        }
        return Ok(PassOutcome::Deferred);
    }
    drop(repo);
    // The shared transaction path with a metadata-only budget, bracketed
    // with the balanced atomic-bridge state enter/leave (Watchman) so the
    // bridge's own writes are suppressed from watchers.
    let sessions_for_notice =
        SessionStore::for_repo(root).map_err(|error| CliError::InvalidRepository {
            reason: format!("cannot open the session store: {error}"),
        })?;
    let sink = move |kind: WatchNoticeKind, detail: &str| {
        let remediation = match kind {
            WatchNoticeKind::ExternalHeadChange => {
                // CB-13D ::24 R6: the notice is emitted before the pass
                // classifies (latency), so it must not promise "no action
                // required" — an unsafe state DEFERS to an explicit
                // reconcile and a follow-up UnsafeState notice names the
                // remediation.
                "the workspace reconciles automatically when safe; an unsafe state                  defers to an explicit 'atomic git bridge reconcile'"
            }
            WatchNoticeKind::UnsafeState => "run 'atomic git bridge reconcile' once Git is idle",
            WatchNoticeKind::ExportSuppressed => "run 'atomic git bridge reconcile' explicitly",
        };
        notify_active_sessions(&sessions_for_notice, root, kind, detail, remediation);
    };
    bracket_pass(root, &sink)?;
    // CB-13D ::24 R3: the baseline binds to the VERIFIED CHECKPOINT the
    // pass actually reconciled — the post-pass sample is taken inside the
    // verified boundary, not after it. A fresh sample here could
    // acknowledge an external commit that landed between the reconcile and
    // the sample as the daemon's own, leaving the checkpoint stale until
    // another hint. The HEAD/symref/tree/index identities come from the
    // checkpoint; only the refs digest is sampled (the metadata-only
    // daemon never moves refs, so the checkpoint cannot bind it), and an
    // external ref movement therefore still fires the next hint. An
    // observation failure keeps the caller's baseline untouched — the next
    // poll re-detects the daemon's own writes and self-heals through an
    // aligned no-op pass.
    let observed = MetadataHint::verified_baseline(root)?;
    Ok(PassOutcome::Attempted { observed })
}

/// Run one metadata-only reconcile pass inside the balanced atomic-bridge
/// watcher bracket.
fn bracket_pass(root: &Path, sink: &(dyn Fn(WatchNoticeKind, &str) + '_)) -> CliResult<()> {
    let sink_ref: &WatchNoticeSink<'_> = sink;
    let operation = format!("bridge-watch:{}", std::process::id());
    let root_owned = root.to_path_buf();
    atomic_agent::watcher::bridge::bracket_bridge_transaction(
        atomic_agent::watcher::bridge::watcher_config(root),
        &operation,
        move || {
            reconcile_transaction_budgeted(
                &root_owned,
                ReconcileEffectBudget::MetadataOnly,
                Some(sink_ref),
            )
        },
    )
}

/// Write one pending notice for every active managed session (RFC §11.2
/// rule 6: "pushed into the session as a notice before the agent's next
/// tool call"). Notice writing is advisory: failures are logged and never
/// change the daemon's own outcome.
fn notify_active_sessions(
    sessions: &SessionStore,
    root: &Path,
    kind: WatchNoticeKind,
    detail: &str,
    remediation: &str,
) {
    let kind_code = match kind {
        WatchNoticeKind::ExternalHeadChange => "external-head-change",
        WatchNoticeKind::UnsafeState => "unsafe-state",
        WatchNoticeKind::ExportSuppressed => "external-head-change",
    };
    let active = match sessions.active_sessions() {
        Ok(active) => active,
        Err(error) => {
            log::warn!("bridge watch: cannot list active sessions for notices: {error}");
            return;
        }
    };
    if active.is_empty() {
        print_warning(&format!(
            "bridge watch: {kind_code}: {detail} ({remediation})"
        ));
        return;
    }
    let delivered = active.len();
    for session in active {
        if let Err(error) =
            sessions.write_watch_notice(&session.session_id, kind_code, detail, remediation)
        {
            log::warn!(
                "bridge watch: could not write the watch notice for session {}: {error}",
                session.session_id
            );
        }
    }
    print_info(&format!(
        "bridge watch: {kind_code} notice delivered to {delivered} active session(s)"
    ));
}

/// The daemon entry point: consent, then the hint loop (or a single pass).
pub(crate) fn run_watch(root: &Path, once: bool, poll_ms: u64) -> CliResult<()> {
    let config = bridge_config(root)?;
    if !config.git.bridge.enabled {
        return Err(CliError::InvalidRepository {
            reason: "the bridge is not enabled for this repository (set [git.bridge] \
                     enabled = true); the watch daemon is an optional accelerator on top of \
                     command-boundary reconciliation"
                .to_string(),
        });
    }
    if let Some(refusal) = config.git.bridge.watch.startup_refusal() {
        return Err(CliError::InvalidRepository {
            reason: refusal.to_string(),
        });
    }
    // RFC §11.2 tier precedence (review R4): an explicit `git.watch = "off"`
    // overrides the bridge-watch consent. The daemon is a tier accelerator,
    // not an independent consent surface — the explicit off wins.
    if config.git.watch == atomic_config::GitWatch::Off {
        return Err(CliError::InvalidRepository {
            reason: "the bridge watch daemon is disabled by [git] watch = \"off\"; \
                     command-boundary reconciliation remains the correctness guarantee"
                .to_string(),
        });
    }
    // RFC §11.2 rule 7: CI and containers default off. An explicit opt-in
    // still runs — this log line keeps that choice observable instead of
    // silently succeeding.
    let environment = atomic_config::Environment::detect();
    if environment != atomic_config::Environment::Desktop {
        log::warn!(
            "bridge watch: explicitly opted in while running in a {environment:?} environment; \
             the RFC default-off rule is satisfied by the disabled default, not by refusal"
        );
    }
    let quiet = Duration::from_millis(config.git.bridge.watch.effective_quiet_ms());
    let poll = Duration::from_millis(poll_ms.max(MIN_POLL_INTERVAL_MS));
    let sessions = SessionStore::for_repo(root).map_err(|error| CliError::InvalidRepository {
        reason: format!("cannot open the session store: {error}"),
    })?;

    // Baseline: the metadata truth as the daemon first observes it. Only
    // deltas from this (or from the daemon's own prior writes, folded in by
    // re-baselining after each pass) are external hints.
    let mut baseline = MetadataHint::observe(root)?;
    // The last truth an unsafe-state notice was already emitted for; a
    // deferral notices once per distinct truth and retries silently.
    let mut notice_gate: Option<MetadataHint> = None;
    if once {
        // Single-pass mode always attempts: the shared transaction
        // classifier decides, a fully-aligned workspace is a no-op, and a
        // deferred pass reports its notice and exits nonzero so callers
        // (and the fault corpus) can distinguish "acted" from "deferred".
        match reactive_pass(root, None, None, quiet, poll, &sessions)? {
            PassOutcome::Attempted { .. } | PassOutcome::NoChange => return Ok(()),
            PassOutcome::Deferred => {
                return Err(CliError::StaleBaseline {
                    report: "bridge watch: the external transition was noticed but the \
                             reconcile was deferred (unsafe state, pending recovery, or \
                             in-flight bridge operation); the notice names the remediation"
                        .to_string(),
                });
            }
        }
    }
    print_info(&format!(
        "bridge watch: watching Git metadata (poll {} ms, quiet {} ms); Ctrl-C to stop; \
         command boundaries remain the correctness guarantee",
        poll.as_millis(),
        quiet.as_millis()
    ));
    loop {
        std::thread::sleep(poll);
        // CB-13D ::24 R4 runtime revocation: the startup config was
        // sampled once; a consent withdrawn while the daemon runs is
        // consumed at the next poll and the daemon exits cleanly
        // (command boundaries remain the correctness guarantee).
        match bridge_config(root) {
            Ok(config)
                if config.git.bridge.enabled
                    && config.git.watch != atomic_config::GitWatch::Off => {}
            _ => {
                print_info(
                    "bridge watch: consent withdrawn while running; the daemon exits                      (command boundaries remain the correctness guarantee)",
                );
                return Ok(());
            }
        }
        let current = match MetadataHint::observe(root) {
            Ok(current) => current,
            Err(error) => {
                // Degraded observation (disconnect, sleep/resume): degrade
                // to command-boundary observation; the next successful
                // observation re-baselines.
                log::warn!("bridge watch: observation failed, degrading: {error}");
                baseline = MetadataHint::default();
                continue;
            }
        };
        if !current.differs_from(&baseline) {
            continue;
        }
        match reactive_pass(
            root,
            Some(&baseline),
            notice_gate.as_ref(),
            quiet,
            poll,
            &sessions,
        ) {
            Ok(PassOutcome::Attempted { observed }) => {
                // Re-baseline on the VERIFIED observation taken right after
                // the pass completed (review R3), not on an arbitrary later
                // sample: the daemon's own writes are never external hints
                // (RFC §11.2 rule 5, journal/state evidence), while an
                // external change that lands after the pass still differs
                // and fires the next hint. The pending hint is fully
                // accounted for.
                baseline = observed;
                notice_gate = None;
            }
            // A deferral never consumes the hint: the baseline stays, so
            // the pending reconcile is retried once the unsafe state
            // clears. Only the notice is de-duplicated per truth.
            Ok(PassOutcome::Deferred) => {
                notice_gate = Some(current);
            }
            Ok(PassOutcome::NoChange) => {
                baseline = current;
                notice_gate = None;
            }
            Err(error) => {
                // Review R3: a retryable failure (lock contention with a
                // concurrent command) must NOT consume the pending hint —
                // the daemon keeps the hint and retries once the competing
                // operation releases. A terminal failure consumes the hint
                // (a permanent refusal re-noticed every poll would be
                // noise); the next truth change re-detects divergence.
                let retryable = matches!(
                    &error,
                    CliError::Repository(repository_error) if repository_error.is_lock_contended()
                );
                log::warn!("bridge watch: reactive reconcile failed: {error}");
                notice_gate = Some(current.clone());
                if !retryable {
                    baseline = current;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {

    /// CB-13D ::24 R3 deterministic race fixture: an external commit lands
    /// between the reconciled state and the post-pass observation. The
    /// verified baseline must bind to the CHECKPOINT (the reconciled
    /// truth), never to the live HEAD — the old post-pass sample
    /// acknowledged the external commit as the daemon's own and left the
    /// checkpoint stale until another hint. Fails before (the helper did
    /// not exist and the sample bound the live HEAD) and passes after.
    #[test]
    fn verified_baseline_binds_to_the_checkpoint_not_the_live_head() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let dot = root.join(".atomic");
        std::fs::create_dir_all(dot.join("bridge")).unwrap();

        // The reconciled checkpoint (what the pass actually verified).
        let checkpoint = r#"{"version":2,"view":"main","atomic_state":"AAA","git_head_symref":"refs/heads/main","git_head":"1111111111111111111111111111111111111111","git_tree":"2222222222222222222222222222222222222222"}"#;
        std::fs::write(dot.join("bridge/workspace.json"), checkpoint).unwrap();

        // An external commit landed AFTER the verified state: the live
        // HEAD (and its tree) moved past the checkpoint.
        let hint = MetadataHint::verified_baseline(root).unwrap();
        assert_eq!(
            hint.head_oid.as_deref(),
            Some("1111111111111111111111111111111111111111"),
            "the baseline binds to the reconciled checkpoint, not the live HEAD"
        );
        assert_eq!(
            hint.head_tree.as_deref(),
            Some("2222222222222222222222222222222222222222"),
            "the tree binds to the checkpoint"
        );
    }

    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_DIRECTORY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let sequence = TEST_DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "atomic-bridge-watch-{}-{unique}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn init_git_commit(root: &Path, message: &str) {
        use git2::Signature;
        let repository = git2::Repository::init(root).unwrap();
        fs::write(root.join("tracked.txt"), message.as_bytes()).unwrap();
        let mut index = repository.index().unwrap();
        index.add_path(Path::new("tracked.txt")).unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repository.find_tree(tree_oid).unwrap();
        let signature = Signature::now("CB-13D Tests", "cb13d@example.com").unwrap();
        let parent = repository
            .head()
            .ok()
            .and_then(|head| head.peel_to_commit().ok());
        let parents: Vec<&git2::Commit> = parent.iter().collect();
        repository
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                message,
                &tree,
                &parents,
            )
            .unwrap();
    }

    #[test]
    fn hint_observes_truth_through_git_apis() {
        let directory = TestDirectory::new();
        let root = directory.0.clone();
        init_git_commit(&root, "one");
        let first = MetadataHint::observe(&root).unwrap();
        assert!(first.head_oid.is_some());

        // Same state re-observes equal: no self-trigger.
        let again = MetadataHint::observe(&root).unwrap();
        assert!(!again.differs_from(&first));

        // An external commit changes the truth (hint).
        init_git_commit(&root, "two");
        let second = MetadataHint::observe(&root).unwrap();
        assert!(second.differs_from(&first));
        assert_ne!(second.head_oid, first.head_oid);

        // A ref movement is also observable truth (hint), without HEAD.
        let repository = git2::Repository::open(&root).unwrap();
        let head = repository.head().unwrap().peel_to_commit().unwrap();
        repository
            .reference("refs/heads/other", head.id(), true, "test ref movement")
            .unwrap();
        let third = MetadataHint::observe(&root).unwrap();
        assert!(third.differs_from(&second));
        assert_eq!(third.head_oid, second.head_oid);
    }

    #[test]
    fn missing_git_observes_the_default_hint() {
        let directory = TestDirectory::new();
        let hint = MetadataHint::observe(&directory.0).unwrap();
        assert_eq!(hint, MetadataHint::default());
    }

    /// RFC §11.2 rule 3: quiescence requires the full silence window; a
    /// change inside the window restarts it.
    #[test]
    fn quiescence_requires_silence_for_the_full_window() {
        let directory = TestDirectory::new();
        let root = directory.0.clone();
        init_git_commit(&root, "stable");

        let quiet = Duration::from_millis(250);
        let poll = Duration::from_millis(60);
        let start = Instant::now();
        wait_for_quiescence(&root, quiet, poll, None).unwrap();
        let waited = start.elapsed();
        assert!(
            waited >= quiet,
            "the wait must cover at least the silence window, waited {waited:?}"
        );
    }

    /// RFC §11.2 rule 3: locks block quiescence until Git releases them.
    #[test]
    fn quiescence_waits_for_git_locks_to_clear() {
        let directory = TestDirectory::new();
        let root = directory.0.clone();
        let repository = git2::Repository::init(&root).unwrap();
        let git_dir = repository.path().to_path_buf();
        drop(repository);
        fs::create_dir_all(git_dir.join("refs/heads")).unwrap();
        let lock = git_dir.join("refs/heads/main.lock");
        fs::write(&lock, b"").unwrap();

        let handle = {
            let root = root.clone();
            let lock = lock.clone();
            std::thread::spawn(move || {
                // Hold the lock for 400 ms, then release it.
                std::thread::sleep(Duration::from_millis(400));
                fs::remove_file(&lock).unwrap();
                let _ = root;
            })
        };
        let quiet = Duration::from_millis(250);
        let poll = Duration::from_millis(50);
        wait_for_quiescence(&root, quiet, poll, None).unwrap();
        handle.join().unwrap();
    }

    /// RFC §11.2 rule 3 (review R2): a held `HEAD.lock` fences quiescence —
    /// a reactive pass must not enter while HEAD's own lock is present.
    #[test]
    fn quiescence_waits_for_head_lock_to_clear() {
        let directory = TestDirectory::new();
        let root = directory.0.clone();
        let repository = git2::Repository::init(&root).unwrap();
        let git_dir = repository.path().to_path_buf();
        drop(repository);
        let head_lock = git_dir.join("HEAD.lock");
        fs::write(&head_lock, b"").unwrap();

        let handle = {
            let head_lock = head_lock.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(400));
                fs::remove_file(&head_lock).unwrap();
            })
        };
        let quiet = Duration::from_millis(250);
        let poll = Duration::from_millis(50);
        wait_for_quiescence(&root, quiet, poll, None).unwrap();
        handle.join().unwrap();
    }

    /// RFC §11.2 rule 6 (review R6): a new Git-owned transaction state is
    /// part of the hint — a lock or marker appearing with unchanged
    /// HEAD/index/ref digests must still fire a wake-up, or the daemon
    /// could never surface the unsafe state to an active session.
    #[test]
    fn git_owned_transaction_state_fires_a_hint_without_digest_changes() {
        let directory = TestDirectory::new();
        let root = directory.0.clone();
        init_git_commit(&root, "stable");
        let quiescent = MetadataHint::observe(&root).unwrap();
        assert!(
            quiescent.busy_signature.is_none(),
            "an idle repository observes quiescent"
        );

        let repository = git2::Repository::open(&root).unwrap();
        let git_dir = repository.path().to_path_buf();
        drop(repository);
        fs::write(git_dir.join("HEAD.lock"), b"").unwrap();
        let locked = MetadataHint::observe(&root).unwrap();
        assert!(
            locked.differs_from(&quiescent),
            "a new HEAD.lock must fire a hint even though every digest is unchanged"
        );
        assert!(locked.busy_signature.is_some());

        // After Git releases the lock, the hint returns to the baseline.
        fs::remove_file(git_dir.join("HEAD.lock")).unwrap();
        let released = MetadataHint::observe(&root).unwrap();
        assert!(!released.differs_from(&quiescent));
    }

    #[test]
    fn in_flight_operation_is_detected_from_the_journal() {
        // Without a repository the repository-scope log is unreachable;
        // with one, an empty scope reports no in-flight head.
        let directory = TestDirectory::new();
        let repo = atomic_repository::Repository::init(&directory.0).unwrap();
        let in_flight = head_operation_in_flight(&repo).unwrap();
        assert!(
            in_flight.is_none(),
            "an empty journal has no in-flight head"
        );
    }
}
