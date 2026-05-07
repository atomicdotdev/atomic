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
//! repo.record(header, options)  ← repo diffs working copy vs pristine
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

#[cfg(test)]
mod tests;

use std::path::Path;

use atomic_core::change::ChangeHeader;

use atomic_repository::status::RepositoryStatus;

use crate::error::{AgentError, AgentResult};
use crate::identity::build_agent_author;
use crate::transcript;

// Re-export primary types
pub use options::{TurnRecordOptions, TurnRecordOutcome};

use message::build_turn_message;
use provenance::{
    build_turn_envelope, build_turn_provenance, build_unhashed_turn_data, should_ignore_untracked,
};

// Change Header Construction

/// Build a `ChangeHeader` for an agent turn.
///
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

/// Record an agent turn as an Atomic change.
///
/// This is the function that bridges the agent world into the VCS world.
/// It builds a `ChangeHeader`, `Provenance`, and `SessionEnvelope`, then
/// calls the repository's `record()` method to create a proper content-addressed,
/// hashable, pushable Atomic change.
///
/// # Arguments
///
/// * `repo_root` — Path to the repository root (where `.atomic/` lives).
///   The repository is opened fresh for each recording to avoid stale state.
/// * `options` — Turn recording options (session, changes, event, turn number)
///
/// # Returns
///
/// A `TurnRecordOutcome` with the change hash, turn number, file count,
/// and message. The change has already been applied to the agent's view.
///
/// # Errors
///
/// Returns `AgentError::EmptyTurn` if the repository has nothing to record
/// (no files changed since the last recorded state).
/// Returns `AgentError::RecordFailed` if the repository record operation fails.
pub fn record_turn(
    repo_root: &Path,
    options: &TurnRecordOptions<'_>,
) -> AgentResult<TurnRecordOutcome> {
    // Step 1: Open the repository
    let repo =
        atomic_repository::Repository::open(repo_root).map_err(|e| AgentError::RecordFailed {
            session_id: options.session.session_id.clone(),
            turn_number: options.turn_number,
            reason: format!("Failed to open repository: {}", e),
        })?;

    // Step 2: Status — find out what the agent changed.
    // Fast mode (no content hashing — record() itself does the full hash check)
    // but include untracked files: agents create new files all the time and we
    // need to see them here to add+record them in Step 3.
    let status = repo
        .status(atomic_repository::status::StatusOptions::fast().with_untracked(true))
        .map_err(|e| AgentError::RecordFailed {
            session_id: options.session.session_id.clone(),
            turn_number: options.turn_number,
            reason: format!("Failed to get repository status: {}", e),
        })?;

    // Check if there's anything to record at all
    if status.is_clean() && status.untracked_count() == 0 {
        return Err(AgentError::EmptyTurn {
            session_id: options.session.session_id.clone(),
            turn_number: options.turn_number,
        });
    }

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

    if !untracked_paths.is_empty() {
        log::info!(
            "Adding {} untracked file{} created by agent",
            untracked_paths.len(),
            if untracked_paths.len() == 1 { "" } else { "s" },
        );

        let tracking_options = atomic_repository::tracking::TrackingOptions::default();
        for path in &untracked_paths {
            if let Err(e) = repo.add(path, tracking_options.clone()) {
                log::warn!("Failed to add '{}': {} (skipping)", path, e);
            }
        }
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
    let record_options = atomic_repository::record::RecordOptions::new()
        .with_all(true)
        .view(options.session.view_name.clone())
        .apply_after_record(true)
        .save_to_store(true)
        .provenance(vec![provenance_entry])
        .metadata_bytes(envelope_bytes);

    let mut outcome = match repo.record(header, record_options) {
        Ok(outcome) => outcome,
        Err(atomic_repository::record::RecordError::NothingToRecord) => {
            return Err(AgentError::EmptyTurn {
                session_id: options.session.session_id.clone(),
                turn_number: options.turn_number,
            });
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

    Ok(TurnRecordOutcome {
        hash,
        turn_number: options.turn_number,
        file_count,
        message,
        recorded_files,
        unhashed_data,
    })
}
