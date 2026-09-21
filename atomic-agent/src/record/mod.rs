//! Turn-level change recording.
//!
//! This module bridges the agent turn lifecycle into Atomic's change recording
//! system. When a turn ends, `record_turn()` takes the session state and the
//! turn event, and calls `atomic-repository`'s `record()` to create a proper
//! Atomic change with:
//!
//! - **`ChangeHeader`** — message, author, timestamp
//! - **`Provenance`** — AI vendor, model, tokens, cost, prompt hash
//! - **`SessionEnvelope`** — turn number, session context, files, timing
//!   (encoded into `HashedChange.metadata`)
//!
//! # Data Flow
//!
//! ```text
//! TurnEvent + AgentSession
//!     │
//!     ▼
//! record_turn()
//!     │
//!     ├──▶ Build ChangeHeader (message, author, timestamp)
//!     ├──▶ Build Provenance (vendor, model, tool, prompt hash)
//!     ├──▶ Build SessionEnvelope → encode → hashed.metadata
//!     ├──▶ Build RecordOptions (all: true, view, provenance)
//!     │
//!     ▼
//! repo.record(working_copy, header, options)  ← repo diffs working copy vs pristine
//!     │
//!     ▼
//! RecordOutcome { change, hash, stats }
//! ```
//!
//! # Why `all: true` instead of explicit file paths?
//!
//! Each agent hook invocation is a **separate process**. The `TurnStart` hook
//! runs in one process, the agent modifies files, then the `TurnEnd` hook runs
//! in a different process. An in-memory file watcher snapshot taken in process A
//! is gone by the time process B runs.
//!
//! Instead of trying to persist watcher state across processes, we let the
//! repository do what it already does: compare the working copy against the
//! pristine (last recorded state) and record everything that changed. This is
//! the same approach Entire CLI uses with its `manual-commit` strategy.
//!
//! # Session Data in the Change
//!
//! The recorded change carries session data in three slots:
//!
//! | Slot | Data | Hashed? |
//! |---|---|---|
//! | `hashed.provenance` | tokens, cost, model, prompt hash | Yes |
//! | `hashed.metadata` | SessionEnvelope (turn#, session ID, timing) | Yes |
//! | `unhashed` | Transcript (future — Phase 18.5.3) | No |
//!
//! Because provenance and metadata are hashed, they become part of the change's
//! cryptographic identity and commute via patch theory like any other change data.

pub mod message;
pub mod options;
pub mod provenance;
pub mod scope;

#[cfg(test)]
mod tests;

use std::path::Path;

use atomic_core::change::session::{
    GitBoundaryCheckpoint, IncompleteSession, ManagedTurnOutcome, SessionIncompleteOrigin,
    TurnBoundary,
};
use atomic_core::change::ChangeHeader;
use atomic_core::types::Base32;

use atomic_repository::status::RepositoryStatus;

use crate::error::{AgentError, AgentResult};
use crate::identity::build_agent_author;
use crate::transcript;

// Re-export primary types
pub use options::{RecordedGitTransition, TurnRecordOptions, TurnRecordOutcome};

use message::build_turn_message;
use provenance::{
    build_turn_envelope, build_turn_provenance, build_unhashed_turn_data, should_ignore_untracked,
};

// Turn boundary capture and Git-only classification (CB-12A, RFC §10.1/§10.2)

/// The result of trying to record one turn.
///
/// `EmptyTurn` is removed (RFC §10.2): a clean worktree is *classified* as
/// `ObservationOnly` or `RepositoryOperations`, never treated as "nothing
/// happened" and never surfaced as an error.
#[derive(Debug)]
pub enum TurnRecordResult {
    /// Durable content changes were recorded as Atomic changes.
    Recorded(TurnRecordOutcome),
    /// No working-copy content change; the turn is classified instead.
    Classified(ClassifiedTurn),
}

/// One git-only or observation-only turn classification.
#[derive(Debug)]
pub struct ClassifiedTurn {
    /// Semantic outcome (never `ContentChanges` in this variant).
    pub outcome: ManagedTurnOutcome,
    /// Turn-start baseline, when one was captured.
    pub boundary_start: Option<TurnBoundary>,
    /// Turn-end boundary captured during classification.
    pub boundary_end: Option<TurnBoundary>,
    /// Durable incomplete refusal for unexplained Git transitions.
    pub incomplete: Option<IncompleteSession>,
}

impl ClassifiedTurn {
    /// The turn this classification belongs to.
    pub fn turn(&self) -> u32 {
        self.boundary_end.as_ref().map(|b| b.turn).unwrap_or(0)
    }
}

/// Capture one durable turn boundary from a live repository handle.
///
/// Observation only: nothing is interpreted, reconciled or materialized
/// (RFC §12.1/§12.2 — reconciliation happens in repository command
/// boundaries; the agent boundary observes). Fields the repository cannot
/// compute yet stay `None` and are documented on `TurnBoundary`.
pub fn capture_turn_boundary(
    repo: &atomic_repository::Repository,
    repo_root: &Path,
    session_id: &str,
    turn: u32,
) -> Option<TurnBoundary> {
    let working_copy = match repo.require_working_copy_id() {
        Ok(working_copy) => working_copy.to_string(),
        Err(error) => {
            log::warn!("Boundary capture: cannot resolve working-copy identity: {}", error);
            return None;
        }
    };

    let view = repo.current_view().to_string();
    let (view_state, set_id) = match repo.view_identity(&view) {
        Ok(identity) => (Some(identity.merkle), Some(identity.set_id)),
        Err(error) => {
            // The view does not exist yet (e.g. a session view forked at
            // first record) — record the boundary with unknown identity.
            log::debug!(
                "Boundary capture: view '{}' identity unavailable: {}",
                view,
                error
            );
            (None, None)
        }
    };

    let git = match atomic_repository::observe_git_metadata(repo_root) {
        Ok(observation) => Some(git_checkpoint(&observation.token())),
        Err(error) => {
            log::warn!("Boundary capture: git observation failed: {}", error);
            None
        }
    };

    Some(TurnBoundary {
        working_copy,
        operation: None,
        view,
        view_state,
        set_id,
        snapshot: None,
        git,
        // Phase 4 manifest engine pending — recorded as None, never faked.
        manifest: None,
        conversion_policy: None,
        session_id: session_id.to_string(),
        turn,
        at: chrono::Utc::now().timestamp(),
    })
}

/// Map an observation token onto the durable checkpoint type.
pub fn git_checkpoint(token: &atomic_repository::GitObservationToken) -> GitBoundaryCheckpoint {
    let (head_oid, head_symref) = match &token.head {
        atomic_repository::GitHeadObservation::Attached { symref, oid } => {
            (Some(oid.clone()), Some(symref.clone()))
        }
        atomic_repository::GitHeadObservation::Detached { oid } => (Some(oid.clone()), None),
        atomic_repository::GitHeadObservation::Unborn { symref }
        | atomic_repository::GitHeadObservation::MissingTarget { symref } => {
            (None, Some(symref.clone()))
        }
    };
    GitBoundaryCheckpoint {
        head_oid,
        head_symref,
        head_tree: token.head_tree.clone(),
        index_digest: Some(token.index_digest),
        index_tree: token.index_tree.clone(),
        index_locked: token.index_locked,
        repository_state: token.repository_state.clone(),
        markers: token
            .markers
            .iter()
            .map(|marker| format!("{marker:?}"))
            .collect(),
    }
}

/// Describe what moved between two Git checkpoints (observed-operation-only).
fn describe_git_transition(
    start: &GitBoundaryCheckpoint,
    end: &GitBoundaryCheckpoint,
) -> Vec<String> {
    let mut operations = Vec::new();
    if start.head_oid != end.head_oid {
        operations.push(format!(
            "HEAD {} -> {}",
            start.head_oid.as_deref().unwrap_or("<unborn>"),
            end.head_oid.as_deref().unwrap_or("<unborn>")
        ));
    }
    if start.index_digest != end.index_digest {
        operations.push(format!(
            "primary index {} -> {}",
            start
                .index_digest
                .as_ref()
                .map(|digest| digest.to_base32())
                .unwrap_or_else(|| "<unreadable>".to_string()),
            end.index_digest
                .as_ref()
                .map(|digest| digest.to_base32())
                .unwrap_or_else(|| "<unreadable>".to_string())
        ));
    }
    if start.repository_state != end.repository_state || start.markers != end.markers {
        operations.push("git operation state changed".to_string());
    }
    if operations.is_empty() {
        operations.push("non-checkpoint git state changed".to_string());
    }
    operations
}

/// Resolve the sessions directory the same way the pre-commit producer and
/// the orchestrator do: the canonical common `.atomic` (review R9). Reading
/// the worktree-local `.atomic/sessions` instead would strand the producer's
/// captures away from linked-worktree consumers.
fn sessions_dir_for(repo_root: &Path) -> std::path::PathBuf {
    atomic_repository::Repository::canonical_dot_dir(repo_root)
        .map(|dot| dot.join("sessions"))
        .unwrap_or_else(|_| repo_root.join(".atomic").join("sessions"))
}

/// Semantic checkpoint equality (review R5).
///
/// The raw index digest is evidence, not semantics: a stat-only index refresh
/// changes the file bytes (and its digest) without changing the represented
/// tree. When both sides observed an index tree, tree equality proves the
/// index semantically unchanged even if the digest drifted. A digest drift
/// without a readable tree stays INEQUALITY (uncertainty is never equality).
fn checkpoints_semantically_equal(
    start: &GitBoundaryCheckpoint,
    end: &GitBoundaryCheckpoint,
) -> bool {
    let index_equal = match (&start.index_digest, &end.index_digest) {
        (Some(a), Some(b)) if a == b => true,
        (Some(_), Some(_)) => start.index_tree.is_some() && end.index_tree.is_some(),
        _ => start.index_digest == end.index_digest,
    };
    start.head_oid == end.head_oid
        && start.head_symref == end.head_symref
        && start.head_tree == end.head_tree
        && start.index_tree == end.index_tree
        && index_equal
        && start.repository_state == end.repository_state
        && start.markers == end.markers
}

/// Whether the observed HEAD move is explained by an anchored journal record.
///
/// Review R4: the advisory journal is untrusted input. A record explains a
/// transition only when it was written for THIS worktree (the real journal
/// writes the canonical worktree root) and inside the turn window (a stale
/// or future-dated row is replay evidence, not an explanation). Anything else
/// is ignored and the transition stays unexplained.
fn checkout_journaled(
    repo_root: &Path,
    start_oid: Option<&str>,
    end_oid: Option<&str>,
    not_before_unix: i64,
) -> bool {
    let (Some(start_oid), Some(end_oid)) = (start_oid, end_oid) else {
        return false;
    };
    let journal = repo_root.join(".atomic").join("bridge").join("git-events.jsonl");
    let Ok(bytes) = std::fs::read(&journal) else {
        return false;
    };
    // Bound the scan: the journal is append-only; a recent tail is enough.
    let tail_start = bytes.len().saturating_sub(256 * 1024);
    let tail = &bytes[tail_start..];
    let worktree_root = std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    let now_unix = chrono::Utc::now().timestamp();
    let mut records = 0usize;
    for line in tail.split(|byte| *byte == b'\n') {
        if records >= 512 {
            break;
        }
        let Ok(record) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        if record.get("record_type").and_then(|value| value.as_str()) != Some("post-checkout") {
            continue;
        }
        records += 1;
        let old_head = record.get("old_head").and_then(|value| value.as_str());
        let new_head = record.get("new_head").and_then(|value| value.as_str());
        if old_head != Some(start_oid) || new_head != Some(end_oid) {
            continue;
        }
        // Worktree binding: the row must name this worktree (review R4:
        // "worktree /" in the executed probe is a fabricated row).
        let recorded_root = record
            .get("worktree_root")
            .and_then(|value| value.as_str())
            .map(std::path::PathBuf::from)
            .map(|path| std::fs::canonicalize(&path).unwrap_or(path));
        if recorded_root.as_deref() != Some(worktree_root.as_path()) {
            continue;
        }
        // Turn-window anchoring: the row must have been written inside the
        // window, with a generous same-machine clock skew on both sides.
        let recorded_at = record
            .get("recorded_at")
            .and_then(|value| value.as_str())
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
            .map(|time| time.timestamp());
        let Some(recorded_unix) = recorded_at else {
            continue;
        };
        if recorded_unix < not_before_unix.saturating_sub(60)
            || recorded_unix > now_unix.saturating_add(300)
        {
            continue;
        }
        return true;
    }
    false
}

/// The classification of one Git transition between turn boundaries.
struct GitTransitionClassification {
    operations: Vec<String>,
    capture: Option<atomic_core::types::Hash>,
    incomplete: Option<IncompleteSession>,
    /// The exact commit OID when the recorded delta covers it exactly
    /// (CB-12A follow-up AC-8: verified capture + no worktree remainder).
    exact_commit_oid: Option<String>,
}

/// Classify a Git transition (review R2/R4): verified commit-time capture
/// binding, journaled-checkout explanation, or durable refusal. Used by both
/// the clean-turn path and the mixed content+transition path, with identical
/// authority.
#[allow(clippy::too_many_arguments)]
fn classify_git_transition(
    repo_root: &Path,
    options: &TurnRecordOptions<'_>,
    start_git: &GitBoundaryCheckpoint,
    end_git: &GitBoundaryCheckpoint,
    operations: Vec<String>,
    expected_working_copy: &str,
    content_recorded: bool,
) -> GitTransitionClassification {
    let sessions_dir = sessions_dir_for(repo_root);
    let capture = match crate::turn::capture::verify_capture(
        &sessions_dir,
        options.session,
        options.turn_number,
        expected_working_copy,
        start_git.head_oid.as_deref(),
        end_git.head_tree.as_deref(),
        boundary_start_at(options),
    ) {
        Ok(Some(capture)) => {
            let bytes = capture.to_json().unwrap_or_default();
            Some(atomic_core::types::Hash::of(&bytes))
        }
        Ok(None) => None,
        Err(error) => {
            log::warn!(
                "Commit-time capture failed authentication for session {} turn {}: {}",
                options.session.session_id,
                options.turn_number,
                error
            );
            None
        }
    };
    let capture_failed_authentication = capture.is_none()
        && crate::turn::capture::has_capture(
            &sessions_dir,
            &options.session.session_id,
            options.turn_number,
        );

    // A verified capture binds the move, but this is NOT the
    // ManagedGitCommitCaptured classification: the exact baseline→index /
    // index→worktree reassembly (RFC §10.3.2) is not implemented. Per the
    // owner-APPROVED RFC §19 Q2 policy (2026-09-14, allow-as-incomplete):
    // the commit is carried as synthesis with observed-operation-only
    // attribution and a DURABLE incomplete status requiring review — never
    // exact attribution, never an approximate path-level split. The capture
    // hash is retained as recovery evidence.
    if let Some(capture_hash) = capture.clone() {
        log::info!(
            "Verified commit-time capture binds the Git transition for session {} turn {}; \
             durable incomplete until the exact reassembly (RFC §10.3.2) attributes it \
             (RFC §19 Q2 approved allow-as-incomplete, 2026-09-14)",
            options.session.session_id,
            options.turn_number
        );
        let incomplete = IncompleteSession::new(
            "Managed commit captured and authenticated, but the exact baseline→index / \
             index→worktree reassembly (RFC §10.3.2) has not attributed it; carried as \
             synthesis with observed-operation-only attribution per the owner-approved \
             RFC §19 Q2 policy (allow-as-incomplete, 2026-09-14)",
            Vec::<String>::new(),
            String::new(),
            SessionIncompleteOrigin::ManagedCaptureAwaitingReassembly,
        )
        .with_unbound_commits(vec![end_git.head_oid.clone().unwrap_or_default()]);
        let _ = capture_hash;
        // CB-12A follow-up AC-8 (exact separable case): when the turn-end
        // worktree carries NO remainder beyond the commit (worktree ==
        // commit tree), the recorded content delta IS the capture's
        // HEAD→index delta and the change is classified
        // ManagedGitCommitCaptured (exact) — no durable incomplete.
        let remainder = worktree_has_remainder(repo_root);
        if !remainder && content_recorded {
            return GitTransitionClassification {
                operations,
                capture,
                incomplete: None,
                exact_commit_oid: end_git.head_oid.clone(),
            };
        }
        return GitTransitionClassification {
            operations,
            capture,
            incomplete: Some(incomplete),
            exact_commit_oid: None,
        };
    }

    let unbound_commit = end_git.head_oid.clone().unwrap_or_default();

    // A capture file exists but failed authentication: never explained away,
    // even by journaled checkout evidence — a capture can only exist if a
    // commit attempt ran the pre-commit hook.
    if capture_failed_authentication {
        log::warn!(
            "Commit-time capture failed authentication for session {} turn {}; marking \
             session incomplete with observed-operation-only attribution",
            options.session.session_id,
            options.turn_number
        );
        let incomplete = IncompleteSession::new(
            "Git transition between turn boundaries failed commit-time capture \
             authentication; observed-operation-only attribution (RFC §10.3.2)",
            Vec::<String>::new(),
            String::new(),
            SessionIncompleteOrigin::UnattributedGitOperation,
        )
        .with_unbound_commits(vec![unbound_commit]);
        return GitTransitionClassification {
            operations,
            capture: None,
            incomplete: Some(incomplete),
            exact_commit_oid: None,
        };
    }

    // No capture. An anchored journaled checkout explains the move without
    // implying any commit authorship (review R4: worktree- and window-bound).
    let not_before = boundary_start_at(options);
    if checkout_journaled(
        repo_root,
        start_git.head_oid.as_deref(),
        end_git.head_oid.as_deref(),
        not_before,
    ) {
        log::info!(
            "HEAD move for session {} turn {} is explained by an anchored journaled \
             checkout (advisory evidence, RFC §11)",
            options.session.session_id,
            options.turn_number
        );
        return GitTransitionClassification {
            operations,
            capture: None,
            incomplete: None,
            exact_commit_oid: None,
        };
    }

    // Unexplained transition without authenticated commit-time capture:
    // observed-operation-only attribution plus durable incomplete status
    // until reviewed (RFC §10.3.2). Synthesis of the commit itself is NOT
    // implemented and is never faked here.
    log::warn!(
        "Unexplained Git transition for session {} turn {} ({:?}); marking session \
         incomplete with observed-operation-only attribution",
        options.session.session_id,
        options.turn_number,
        operations
    );
    let incomplete = IncompleteSession::new(
        "Git transition between turn boundaries without authenticated commit-time capture \
         (hook bypassed, removed, or --no-verify); observed-operation-only attribution \
         (RFC §10.3.2)",
        Vec::<String>::new(),
        String::new(),
        SessionIncompleteOrigin::UnattributedGitOperation,
    )
    .with_unbound_commits(vec![unbound_commit]);
    GitTransitionClassification {
        operations,
        capture: None,
        incomplete: Some(incomplete),
        exact_commit_oid: None,
    }
}

fn boundary_start_at(options: &TurnRecordOptions<'_>) -> i64 {
    options
        .session
        .boundary_start
        .as_ref()
        .map(|boundary| boundary.at)
        .unwrap_or(0)
}

/// Classify a turn whose working copy is clean (no durable content change).
#[allow(clippy::too_many_arguments)]
fn classify_clean_turn(
    _repo: &atomic_repository::Repository,
    repo_root: &Path,
    options: &TurnRecordOptions<'_>,
    boundary_start: Option<TurnBoundary>,
    boundary_end: TurnBoundary,
) -> ClassifiedTurn {
    let start_git = boundary_start.as_ref().and_then(|boundary| boundary.git.clone());
    let end_git = boundary_end.git.clone();

    // A repository without Git has no checkpoint to classify; a clean
    // turn there is a legitimate ObservationOnly, never an observation
    // gap. Only a Git-bearing repository refuses unverifiable transitions.
    if !repo_root.join(".git").exists() {
        return ClassifiedTurn {
            outcome: ManagedTurnOutcome::ObservationOnly,
            boundary_start,
            boundary_end: Some(boundary_end),
            incomplete: None,
        };
    }

    // No baseline: refuse to claim ObservationOnly, record the observation
    // gap explicitly, and durably refuse attribution (review R3: the missing
    // observation is itself evidence, never a silent success).
    let Some(start_git) = start_git else {
        log::warn!(
            "Turn-end classification for session {} has no turn-start baseline; \
             HEAD/index transition is unverifiable and no attribution is claimed",
            options.session.session_id
        );
        let unbound = boundary_end
            .git
            .as_ref()
            .and_then(|git| git.head_oid.clone())
            .unwrap_or_default();
        let incomplete = IncompleteSession::new(
            "turn-start boundary unavailable; HEAD/index transition unverifiable and \
             no attribution is claimed (RFC §10.2)",
            Vec::<String>::new(),
            String::new(),
            SessionIncompleteOrigin::ObservationUnavailable,
        )
        .with_unbound_commits(vec![unbound]);
        return ClassifiedTurn {
            outcome: ManagedTurnOutcome::RepositoryOperations {
                operations: vec![
                    "turn-start boundary unavailable; HEAD/index transition unverifiable"
                        .to_string(),
                ],
                capture: None,
            },
            boundary_start,
            boundary_end: Some(boundary_end),
            incomplete: Some(incomplete),
        };
    };

    let Some(end_git) = end_git else {
        // Baseline existed but the end observation failed — the same
        // conservative treatment as a missing baseline: durable refusal
        // (review R3).
        log::warn!(
            "Turn-end git observation failed for session {}; \
             no attribution is claimed",
            options.session.session_id
        );
        let incomplete = IncompleteSession::new(
            "turn-end git observation failed; transition unverifiable and no \
             attribution is claimed (RFC §10.2)",
            Vec::<String>::new(),
            String::new(),
            SessionIncompleteOrigin::ObservationUnavailable,
        );
        return ClassifiedTurn {
            outcome: ManagedTurnOutcome::RepositoryOperations {
                operations: vec![
                    "turn-end git observation failed; transition unverifiable".to_string(),
                ],
                capture: None,
            },
            boundary_start,
            boundary_end: Some(boundary_end),
            incomplete: Some(incomplete),
        };
    };

    // Semantic equality over the checkpoint (review R5): timestamps excluded,
    // raw index digest treated as evidence rather than semantics, and the
    // view identity must not have moved either — a view-only move is a real
    // operation, never ObservationOnly.
    let view_identity_equal = match (&boundary_start, &boundary_end) {
        (Some(start), end) => match (&start.view_state, &end.view_state) {
            (Some(a), Some(b)) => a == b,
            (None, None) => true,
            // Unknown on either side: the checkpoint equality below still
            // decides; view identity is not contradicted by an unobserved
            // identity on both ends of an observation gap.
            _ => true,
        },
        (None, _) => true,
    };
    if checkpoints_semantically_equal(&start_git, &end_git) && view_identity_equal {
        return ClassifiedTurn {
            outcome: ManagedTurnOutcome::ObservationOnly,
            boundary_start,
            boundary_end: Some(boundary_end),
            incomplete: None,
        };
    }

    // Git (or the view identity) moved. Classify the transition with full
    // capture/journal/refusal authority (review R2/R4).
    let mut operations = describe_git_transition(&start_git, &end_git);
    if !view_identity_equal {
        operations.push("Atomic view identity moved between turn boundaries".to_string());
    }
    // The CLEAN path records no content: the commit's content never landed
    // in Atomic, so the exact ManagedGitCommitCaptured classification is
    // unavailable (synthesis/import remains).
    let classification = classify_git_transition(
        repo_root,
        options,
        &start_git,
        &end_git,
        operations,
        &boundary_end.working_copy,
        false,
    );
    ClassifiedTurn {
        outcome: ManagedTurnOutcome::RepositoryOperations {
            operations: classification.operations,
            capture: classification.capture,
        },
        boundary_start,
        boundary_end: Some(boundary_end),
        incomplete: classification.incomplete,
    }
}

/// Classify the Git transition of a turn whose working copy was NOT clean
/// (review R2): the content work is attributed normally, but the transition
/// is classified with the same authority as a git-only turn. Returns `None`
/// when there is no observed transition to classify (missing baseline, failed
/// end observation, or semantic checkpoint equality).
fn classify_dirty_turn_git_transition(
    repo_root: &Path,
    options: &TurnRecordOptions<'_>,
    boundary_end: Option<&TurnBoundary>,
) -> Option<RecordedGitTransition> {
    let start_git = options
        .session
        .boundary_start
        .as_ref()
        .and_then(|boundary| boundary.git.clone())?;
    let end_boundary = boundary_end?;
    let end_git = end_boundary.git.clone()?;
    if checkpoints_semantically_equal(&start_git, &end_git) {
        return None;
    }
    let operations = describe_git_transition(&start_git, &end_git);
    // The DIRTY path records the worktree content: when the worktree has
    // no remainder beyond the commit, the recorded delta IS the commit
    // delta (exact — ManagedGitCommitCaptured).
    let classification = classify_git_transition(
        repo_root,
        options,
        &start_git,
        &end_git,
        operations,
        &end_boundary.working_copy,
        true,
    );
    Some(RecordedGitTransition {
        operations: classification.operations,
        capture: classification.capture,
        incomplete: classification.incomplete,
        exact_commit_oid: classification.exact_commit_oid,
    })
}

/// Whether the working tree carries changes beyond the Git HEAD (unstaged
/// or untracked content) — the CB-12A follow-up exact-separability oracle.
/// A clean worktree at the boundary means the turn's recorded content
/// equals the commit tree exactly (no remainder), so the verified capture's
/// HEAD→index delta IS the recorded delta. Read-only; any git failure is
/// conservatively a remainder (never claims exactness).
fn worktree_has_remainder(repo_root: &Path) -> bool {
    let Ok(output) = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(repo_root)
        .output()
    else {
        return true;
    };
    if !output.status.success() {
        return true;
    }
    // Atomic-owned state (.atomic/, .vault/) is never content remainder.
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let remainder_lines: Vec<&str> = stdout
        .lines()
        .filter(|line| {
            let path = line.get(3..).unwrap_or(line);
            !(path.starts_with(".atomic/") || path.starts_with(".vault/"))
        })
        .collect();
    !remainder_lines.is_empty()
}

/// Build a `ChangeHeader` for an agent turn.///
/// The message is built from the file changes and prompt context:
/// - Good prompt: `"Fix the authentication bug in login.rs"`
/// - Slash command or no prompt: `"Add src/main.rs, Cargo.toml"`
///
/// The author is the agent identity.
fn build_turn_header(
    options: &TurnRecordOptions<'_>,
    status: &RepositoryStatus,
    untracked_paths: &[String],
) -> ChangeHeader {
    let message = build_turn_message(options, status, untracked_paths);

    let author = build_agent_author(
        &options.session.agent_name,
        &options.session.agent_display_name,
        &options.session.session_id,
    );

    ChangeHeader::builder()
        .message(message)
        .author(author)
        .build()
}

// record_turn (the main entry point)

fn align_or_repair_session_view(
    repo: &mut atomic_repository::Repository,
    options: &TurnRecordOptions<'_>,
) -> AgentResult<atomic_core::WorkingCopyId> {
    let working_copy =
        repo.require_working_copy_id()
            .map_err(|error| AgentError::RecordFailed {
                session_id: options.session.session_id.clone(),
                turn_number: options.turn_number,
                reason: format!(
                    "Failed to resolve working-copy identity before aligning session view: {}",
                    error
                ),
            })?;

    match repo.align_to_view(working_copy, &options.session.view_name) {
        Ok(()) => Ok(working_copy),
        Err(atomic_repository::RepositoryError::ViewNotFound { .. }) => {
            let parent = options
                .session
                .parent_view()
                .map(str::to_string)
                .unwrap_or_else(|| repo.current_view().to_string());

            match repo.create_view_from(&options.session.view_name, &parent) {
                Ok(()) => {
                    log::warn!(
                        "session view '{}' was missing at record time (SessionStart fork \
                         likely skipped or failed); forked it from '{}' just-in-time",
                        options.session.view_name,
                        parent,
                    );
                }
                Err(atomic_repository::RepositoryError::ViewAlreadyExists { .. }) => {}
                Err(error) => {
                    return Err(AgentError::RecordFailed {
                        session_id: options.session.session_id.clone(),
                        turn_number: options.turn_number,
                        reason: format!(
                            "Session view '{}' does not exist and could not be forked \
                             from '{}': {} (refusing to record onto an implicitly-created \
                             orphan view, which would duplicate existing content)",
                            options.session.view_name, parent, error
                        ),
                    });
                }
            }

            repo.align_to_view(working_copy, &options.session.view_name)
                .map_err(|error| AgentError::RecordFailed {
                    session_id: options.session.session_id.clone(),
                    turn_number: options.turn_number,
                    reason: format!(
                        "Failed to align to session view '{}' after forking it: {}",
                        options.session.view_name, error
                    ),
                })?;
            Ok(working_copy)
        }
        Err(error) => Err(AgentError::RecordFailed {
            session_id: options.session.session_id.clone(),
            turn_number: options.turn_number,
            reason: format!("Failed to align current view before record: {}", error),
        }),
    }
}

/// Record an agent turn as an Atomic change, or classify a clean turn.
///
/// This is the function that bridges the agent world into the VCS world.
/// It builds a `ChangeHeader`, `Provenance`, and `SessionEnvelope`, then
/// calls the repository's `record()` method to create a proper content-addressed,
/// hashable, pushable Atomic change.
///
/// When the working copy is clean, `EmptyTurn` is NOT returned (RFC §10.2
/// removed it): the turn is classified as `ObservationOnly` (semantic
/// checkpoint equality) or `RepositoryOperations` (Git state moved between
/// boundaries) and returned as `TurnRecordResult::Classified` with durable
/// boundary evidence.
///
/// # Arguments
///
/// * `repo_root` — Path to the repository root (where `.atomic/` lives).
///   The repository is opened fresh for each recording to avoid stale state.
/// * `options` — Turn recording options (session, changes, event, turn number)
///
/// # Returns
///
/// A `TurnRecordResult`: `Recorded` with the change outcome, or `Classified`
/// with the semantic turn outcome and durable boundaries.
///
/// # Errors
///
/// Returns `AgentError::RecordFailed` if the repository record operation fails.
pub fn record_turn(
    repo_root: &Path,
    options: &TurnRecordOptions<'_>,
) -> AgentResult<TurnRecordResult> {
    // Scoped manifest gating (dev sync): hook-provided file ownership narrows
    // what this turn may record. `manifest()` returns None for plain turns,
    // making the gate an identity pass-through for unscoped callers.
    let manifest = scope::manifest(options)?;
    if let Some(files) = &manifest {
        scope::validate(repo_root, files, options)?;
        if files.is_empty() {
            return Err(AgentError::EmptyTurn {
                session_id: options.session.session_id.clone(),
                turn_number: options.turn_number,
            });
        }
    }
    // Step 1: Open the repository read-only for the initial status check.
    // This can coexist with other readers. Wait for a transient incompatible
    // writer before deciding whether work or untracked files exist.
    let mut repo = atomic_repository::Repository::open_readonly_wait(
        repo_root,
        std::time::Duration::from_secs(10),
    )
    .map_err(|e| AgentError::RecordFailed {
        session_id: options.session.session_id.clone(),
        turn_number: options.turn_number,
        reason: format!("Failed to open repository (readonly): {}", e),
    })?;

    // Preserve the read-only fast path when the persisted desired view already
    // matches the session. Non-sandbox recording intentionally targets the
    // session view, so repair or align it before status. A provisioned sandbox's
    // persistent working-copy record is authoritative and is never repointed here.
    let session_view_needs_alignment = !repo.is_sandbox()
        && (repo.current_view() != options.session.view_name
            || matches!(
                repo.get_view_info(&options.session.view_name),
                Err(atomic_repository::RepositoryError::ViewNotFound { .. })
            ));
    if session_view_needs_alignment {
        drop(repo);
        let mut repair_repo =
            atomic_repository::Repository::open_existing_wait(repo_root, std::time::Duration::from_secs(10))
                .map_err(|error| {
                AgentError::RecordFailed {
                    session_id: options.session.session_id.clone(),
                    turn_number: options.turn_number,
                    reason: format!("Failed to open repository for view alignment: {}", error),
                }
            })?;
        align_or_repair_session_view(&mut repair_repo, options)?;
        drop(repair_repo);
        repo = atomic_repository::Repository::open_readonly_wait(repo_root, std::time::Duration::from_secs(10))
            .map_err(|error| {
            AgentError::RecordFailed {
                session_id: options.session.session_id.clone(),
                turn_number: options.turn_number,
                reason: format!(
                    "Failed to reopen repository after view alignment: {}",
                    error
                ),
            }
        })?;
    }

    let working_copy =
        repo.require_working_copy_id()
            .map_err(|error| AgentError::RecordFailed {
                session_id: options.session.session_id.clone(),
                turn_number: options.turn_number,
                reason: format!(
                    "Failed to resolve working-copy identity for status: {}",
                    error
                ),
            })?;

    // Step 2: Status — find out what the agent changed.
    // Include untracked files because agent turns commonly create new source,
    // config, and test files. Those must be auto-added before recording so the
    // turn produces an Atomic change with provenance instead of leaving files
    // untracked in the working copy.
    let status = repo
        .status(
            working_copy,
            atomic_repository::status::StatusOptions::fast().with_untracked(true),
        )
        .map_err(|e| AgentError::RecordFailed {
            session_id: options.session.session_id.clone(),
            turn_number: options.turn_number,
            reason: format!("Failed to get repository status: {}", e),
        })?;

    let status = scope::filter(status, manifest.as_ref());

    // Check if there's anything to record at all. A clean turn is classified
    // (RFC §10.2), never reported as empty.
    // A projected name conflict is surfaced as `Conflicted`, which
    // `FileStatus::is_dirty` deliberately does not count as a content change.
    // A turn whose only issue is such a conflict is therefore NOT clean: the
    // recorded change is what supersedes the losing claims (or refuses a third
    // value), so it must reach the record body instead of classifying as
    // observation-only. Otherwise a content-clean name conflict would be
    // silently ignored forever.
    if status.is_clean() && status.conflicted_count() == 0 && status.untracked_count() == 0 {
        return Ok(TurnRecordResult::Classified(classify_clean_turn(
            &repo,
            repo_root,
            options,
            options.session.boundary_start.clone(),
            capture_turn_boundary(&repo, repo_root, &options.session.session_id, options.turn_number)
                .ok_or_else(|| AgentError::RecordFailed {
                    session_id: options.session.session_id.clone(),
                    turn_number: options.turn_number,
                    reason: "Failed to capture turn-end boundary on a clean turn".to_string(),
                })?,
        )));
    }

    // Review R2 (ATOM::aaron::8): a mixed turn — working-copy content AND a
    // Git-side transition — is transition-classified BEFORE ordinary content
    // attribution. Dirty content alone never bypasses capture classification:
    // an unexplained commit inside the turn window carries its durable
    // refusal on the recorded outcome, whatever content is recorded.
    let dirty_boundary_end = capture_turn_boundary(
        &repo,
        repo_root,
        &options.session.session_id,
        options.turn_number,
    );
    let git_transition =
        classify_dirty_turn_git_transition(repo_root, options, dirty_boundary_end.as_ref());

    // Step 3: Add — track any new files the agent created
    // Agents create new files all the time (new modules, tests, configs).
    // These show up as "untracked" in status. We add them before recording
    // so they're included in the change.
    //
    // We filter out common large directories (node_modules, target, etc.)
    // that agents may create as side effects (e.g., `npm install`). These
    // would make the hook extremely slow and are never intended to be
    // version-controlled. The .atomicignore file provides user-level control,
    // but these defaults protect against the common case where no ignore
    // file exists yet.
    let untracked_paths: Vec<String> = status
        .untracked()
        .map(|e| e.path().to_string_lossy().to_string())
        .filter(|p| !should_ignore_untracked(p))
        .collect();

    // Drop the read-only handle before opening with write access.
    // This ensures we don't hold two database handles simultaneously.
    drop(repo);

    // Re-open with write capability for add + record.  `open_existing()`
    // skips the table-init `begin_write()` that `open()` does — the tables
    // already exist and that write lock is the primary cause of hook hangs
    // when another process holds a transaction.
    let mut repo = atomic_repository::Repository::open_existing_wait(
        repo_root,
        std::time::Duration::from_secs(10),
    )
    .map_err(|e| AgentError::RecordFailed {
        session_id: options.session.session_id.clone(),
        turn_number: options.turn_number,
        reason: format!("Failed to open repository for recording: {}", e),
    })?;

    // Re-resolve the identity for this newly opened handle. Non-sandbox turns
    // intentionally target the session view, so verify that desired-view
    // alignment again. Sandboxes retain the desired view registered by
    // provision_sandbox and must not be repointed by the record hook.
    let working_copy = if repo.is_sandbox() {
        repo.require_working_copy_id()
            .map_err(|error| AgentError::RecordFailed {
                session_id: options.session.session_id.clone(),
                turn_number: options.turn_number,
                reason: format!(
                    "Failed to resolve sandbox working-copy identity for recording: {}",
                    error
                ),
            })?
    } else {
        align_or_repair_session_view(&mut repo, options)?
    };

    if let Some(files) = &manifest {
        scope::validate(repo_root, files, options)?;
    }

    if !untracked_paths.is_empty() {
        log::info!(
            "Adding {} untracked file{} created by agent",
            untracked_paths.len(),
            if untracked_paths.len() == 1 { "" } else { "s" },
        );

        let untracked_refs: Vec<&str> = untracked_paths.iter().map(String::as_str).collect();
        if let Err(e) = repo.add_batch(working_copy, &untracked_refs) {
            log::warn!(
                "Failed to add untracked files as a batch: {} (falling back to per-file add)",
                e
            );
            let tracking_options = atomic_repository::tracking::TrackingOptions::default();
            for path in &untracked_paths {
                if let Err(e) = repo.add(working_copy, path, tracking_options.clone()) {
                    log::warn!("Failed to add '{}': {} (skipping)", path, e);
                }
            }
        }
    }

    // Refresh status after auto-adding new files. The initial status sees them
    // as Untracked, which `repo.record(all: true)` does not record directly;
    // after add they become Added entries and are recordable.
    // No untracked walk needed — we already added untracked files above.
    let status = repo
        .status(
            working_copy,
            atomic_repository::status::StatusOptions::fast().with_untracked(false),
        )
        .map_err(|e| AgentError::RecordFailed {
            session_id: options.session.session_id.clone(),
            turn_number: options.turn_number,
            reason: format!("Failed to refresh repository status after add: {}", e),
        })?;

    let status = scope::filter(status, manifest.as_ref());
    if status.is_clean() && status.conflicted_count() == 0 {
        return Ok(TurnRecordResult::Classified(classify_clean_turn(
            &repo,
            repo_root,
            options,
            options.session.boundary_start.clone(),
            capture_turn_boundary(&repo, repo_root, &options.session.session_id, options.turn_number)
                .ok_or_else(|| AgentError::RecordFailed {
                    session_id: options.session.session_id.clone(),
                    turn_number: options.turn_number,
                    reason: "Failed to capture turn-end boundary on a clean turn".to_string(),
                })?,
        )));
    }

    // Step 4: Build SessionEnvelope + Record the Atomic change
    // Build the header AFTER status so the message can describe actual changes
    // instead of parroting slash commands like "/init".
    let header = build_turn_header(options, &status, &untracked_paths);
    let provenance_entry = build_turn_provenance(options);
    let message = build_turn_message(options, &status, &untracked_paths);

    // Build the SessionEnvelope BEFORE recording so it can be included in
    // the change hash via RecordOptions::metadata_bytes(). We use the files
    // from status (what we're about to record) rather than waiting for the
    // outcome — they're the same set since we use `all: true`.
    let status_files: Vec<String> = status
        .entries()
        .iter()
        .filter(|e| e.status().is_dirty())
        .map(|e| e.path().to_string_lossy().to_string())
        .chain(untracked_paths.iter().cloned())
        .collect();

    let envelope = build_turn_envelope(options, &status_files);
    let envelope_bytes = envelope.encode().map_err(|e| AgentError::RecordFailed {
        session_id: options.session.session_id.clone(),
        turn_number: options.turn_number,
        reason: format!("Failed to encode SessionEnvelope: {}", e),
    })?;

    // Record all dirty files. The SessionEnvelope bytes are included in
    // HashedChange.metadata — part of the change's cryptographic identity.
    // This means session structure (turn number, timing, files, agent name)
    // is tamper-evident and commutes via patch theory.
    let mut record_options = atomic_repository::record::RecordOptions::new()
        .with_all(true)
        .view(options.session.view_name.clone())
        .apply_after_record(true)
        .save_to_store(true)
        // The agent already discovered and added every untracked destination
        // above, so a second raw-rename scan cannot find a move.
        .detect_raw_renames(false)
        .sync_vault(false)
        .enrich_kg(false)
        .provenance(vec![provenance_entry])
        .metadata_bytes(envelope_bytes);

    if manifest.is_some() {
        record_options = record_options.with_all(false).paths(status_files.clone());
    }

    let mut outcome = match repo.record(working_copy, header, record_options) {        Ok(outcome) => outcome,
        Err(atomic_repository::record::RecordError::NothingToRecord) => {
            // Nothing recorded even though status looked dirty — classify
            // instead of reporting an empty turn (RFC §10.2).
            return Ok(TurnRecordResult::Classified(classify_clean_turn(
                &repo,
                repo_root,
                options,
                options.session.boundary_start.clone(),
                capture_turn_boundary(&repo, repo_root, &options.session.session_id, options.turn_number)
                    .ok_or_else(|| AgentError::RecordFailed {
                        session_id: options.session.session_id.clone(),
                        turn_number: options.turn_number,
                        reason: "Failed to capture turn-end boundary on a clean turn".to_string(),
                    })?,
            )));
        }
        Err(e) => {
            return Err(AgentError::RecordFailed {
                session_id: options.session.session_id.clone(),
                turn_number: options.turn_number,
                reason: format!("Record failed: {}", e),
            });
        }
    };

    // Step 5: Collect results
    let recorded_files: Vec<String> = outcome
        .recorded_files()
        .iter()
        .map(|s| s.to_string())
        .collect();
    let file_count = recorded_files.len();

    // Step 6: Condense transcript + generate reasoning + attach to unhashed
    //
    // Read the agent's transcript file, condense it into structured entries,
    // optionally generate an AI reasoning summary, anchor code learnings to
    // the CRDT graph, then attach everything to the change's unhashed section.
    //
    // All of this is non-fatal — if any step fails, the change is still valid.
    // The unhashed data is also stored in TurnRecordOutcome so the orchestrator
    // can log/display it.

    let unhashed_data: Option<transcript::UnhashedTurnData> =
        build_unhashed_turn_data(options, &recorded_files, &outcome);

    if let Some(ref data) = unhashed_data {
        let entry_count = data.entry_count();
        let has_reasoning = data.has_reasoning();
        match transcript::attach_unhashed(outcome.change_mut(), data) {
            Ok(()) => {
                log::info!(
                    "Attached transcript ({} entries{}) to change for turn {}",
                    entry_count,
                    if has_reasoning { " + reasoning" } else { "" },
                    options.turn_number,
                );

                // The store wrote the change file during record(), before
                // this unhashed data existed. Re-save so the file on disk
                // carries the transcript. The unhashed section is outside
                // the hash, so the content hash is unchanged and the file
                match atomic_repository::Repository::canonical_dot_dir(repo_root)
                    .map(|dot| dot.join("changes"))
                    .map_err(|e| e.to_string())
                    .and_then(|dir| {
                        atomic_repository::ChangeStore::new(
                            dir,
                            atomic_repository::DEFAULT_CACHE_CAPACITY,
                        )
                        .map_err(|e| e.to_string())
                    }) {
                    Ok(store) => match store.save_change(outcome.change()) {
                        Ok(saved) if saved == *outcome.hash() => {}
                        Ok(saved) => {
                            log::warn!(
                                "Re-saved change hash {} differs from recorded {} — \
                                 unhashed data may be orphaned",
                                saved.to_base32(),
                                outcome.hash().to_base32(),
                            );
                        }
                        Err(e) => {
                            log::warn!(
                                "Failed to persist transcript on change {} (non-fatal): {}",
                                outcome.hash().to_base32(),
                                e
                            );
                        }
                    },
                    Err(e) => {
                        log::warn!(
                            "Could not open change store to persist transcript \
                             (non-fatal): {}",
                            e
                        );
                    }
                }
            }
            Err(e) => {
                log::warn!(
                    "Failed to attach unhashed data to change (non-fatal): {}",
                    e
                );
            }
        }
    }

    let hash = *outcome.hash();

    Ok(TurnRecordResult::Recorded(TurnRecordOutcome {
        hash,
        turn_number: options.turn_number,
        file_count,
        message,
        recorded_files,
        unhashed_data,
        git_transition,
    }))
}
