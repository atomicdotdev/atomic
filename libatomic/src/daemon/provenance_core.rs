//! The provenance journal core — one code path for the in-process dispatch
//! and the wire.
//!
//! Every store operation the legacy owner protocol served (`atomic agent
//! database-owner`, owner.rs) lives here exactly once: the RPC handlers in
//! `services_provenance.rs` and the in-process `DirectJournalSink` are thin
//! wrappers over these functions, so the two surfaces cannot drift.
//!
//! # Legacy semantics preserved verbatim
//!
//!  * Generation fencing on every mutation (`expected_generation`); the
//!    store's fencing error text ("generation is stale") surfaces verbatim.
//!  * Caller-supplied `now` — observation times arrive on the wire, never
//!    from server clocks.
//!  * Append idempotency by event ID — a retried batch re-acknowledges the
//!    originally stored sequences instead of duplicating events.
//!  * PrepareCheckpoint's `reuse_frozen_changes` recovery: a previous Stop
//!    may have recorded the change before its journal read failed; the retry
//!    sees a clean working copy. Only the frozen hashes are recovered, then
//!    the store's own source validation still applies — a new nonempty
//!    change set must conflict.
//!  * Frozen-envelope paging with strict client-side continuity checks
//!    (`append_frozen_page`): page cursors must strictly advance, fragment
//!    cursors must continue the previous page exactly, and a retried RPC
//!    can never append its bytes twice.
//!  * The page budget math from the legacy frame budget: reserve the
//!    worst-case metadata and (for the transport encoding) four payload
//!    bytes per budget byte, then clamp to the 1 MiB page size. The
//!    "frame" here is the encoded protobuf message plus the gRPC length
//!    prefix; the empty-response overhead is measured, not guessed.
//!
//! # Crash-injection failpoints
//!
//! The five named failpoints sit at EXACTLY the same seams as the legacy
//! owner: before-envelope-commit, after-envelope-commit,
//! after-checkpoint-prepare, before-frozen-page-continuation,
//! after-checkpoint-bind — plus the frozen-page-unavailable injected-error
//! variant in the page loader. The environment names
//! (`ATOMIC_OWNER_FAILPOINT`, `ATOMIC_OWNER_FAILPOINT_MARKER`) and the
//! abort semantics are kept IDENTICAL to the dismantled owner so the
//! existing crash-injection test suite keeps working against the daemon;
//! renaming them would break parity with the legacy tests that spawn
//! owners with these env vars.

use std::fs::OpenOptions;

use crate::atomic::ErrorCode;
use atomic_repository::redb_change_store::{
    FrozenProvenanceCursor, FrozenProvenancePage, ProvenanceCheckpointAttempt,
    ProvenanceCheckpointSource, ProvenanceId, ProvenanceTurnState, RedbChangeStore, StopCause,
    StopState, StoredProvenanceEnvelope, StoredProvenanceTurn, MAX_FROZEN_PAGE_FRAGMENTS,
};
use prost::Message;

/// The ceiling a complete frozen page must stay under once encoded. Kept
/// from the legacy owner's frame limit (the journal contract's
/// `Limits.max_message_bytes`); the budget math below divides by four to
/// reserve worst-case transport expansion.
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// The frozen page payload budget (the journal contract's
/// `Limits.frozen_page_bytes`).
pub const FROZEN_PAGE_BYTES: usize = 1024 * 1024;

/// Worst-case per-fragment metadata reserve (including 64-bit cursors),
/// plus the next cursor. Kept from the legacy owner even though protobuf
/// fragments are cheaper to encode — parity over penny-shaving.
pub const FROZEN_PAGE_METADATA_BYTES: usize = 256 * MAX_FROZEN_PAGE_FRAGMENTS + 256;

/// A store failure classified with the journal contract's error code.
pub struct JournalError {
    pub code: ErrorCode,
    pub message: String,
}

impl JournalError {
    fn store(code: ErrorCode, error: impl std::fmt::Display) -> Self {
        Self {
            code,
            message: error.to_string(),
        }
    }
}

/// The crash-injection seam, ported VERBATIM from the legacy owner
/// (owner.rs `owner_failpoint`). The env names are load-bearing: the
/// legacy test suite spawns owners (now daemons) with them and asserts
/// the process aborts and the retry converges.
pub fn owner_failpoint(name: &str) {
    if std::env::var("ATOMIC_OWNER_FAILPOINT").ok().as_deref() != Some(name) {
        return;
    }
    let should_trip = match std::env::var_os("ATOMIC_OWNER_FAILPOINT_MARKER") {
        Some(path) => OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)
            .is_ok(),
        None => true,
    };
    if should_trip {
        std::process::abort();
    }
}

/// Reserve or retrieve the stable identity for a session turn. The first
/// reserve claims the turn; a committed reservation returns only after the
/// store's transaction commits.
pub fn reserve_turn(
    store: &RedbChangeStore,
    session_id: &str,
    turn_number: u32,
    now: i64,
) -> Result<StoredProvenanceTurn, JournalError> {
    store
        .reserve_provenance_turn(session_id, turn_number, now)
        .map_err(|error| JournalError::store(ErrorCode::ProvenanceStore, error))
}

/// Append one durable envelope batch with generation fencing. Duplicate
/// event IDs re-acknowledge their original sequences; a committed batch
/// returns only after the transaction commits.
pub fn append_envelopes(
    store: &RedbChangeStore,
    id: ProvenanceId,
    expected_generation: u64,
    batch: &[(&str, &[u8])],
    now: i64,
) -> Result<Vec<StoredProvenanceEnvelope>, JournalError> {
    owner_failpoint("before-envelope-commit");
    let stored = store
        .append_provenance_envelopes(id, expected_generation, batch, now)
        .map_err(|error| JournalError::store(ErrorCode::ProvenanceStore, error))?;
    owner_failpoint("after-envelope-commit");
    Ok(stored)
}

/// Freeze the journal frontier and persist one recoverable checkpoint
/// attempt, with the legacy `reuse_frozen_changes` recovery: a previous
/// Stop may have recorded the change before its journal read failed. The
/// retry sees a clean working copy; recover only those frozen hashes, then
/// retain the store's source validation — a new nonempty change set must
/// still conflict.
pub fn prepare_checkpoint(
    store: &RedbChangeStore,
    id: ProvenanceId,
    expected_generation: u64,
    mut source: ProvenanceCheckpointSource,
    reuse_frozen_changes: bool,
    now: i64,
) -> Result<ProvenanceCheckpointAttempt, JournalError> {
    let attempt = (|| {
        if reuse_frozen_changes && source.change_hashes.is_empty() {
            if let Some(turn) = store.get_provenance_turn(id)? {
                if turn.generation == expected_generation
                    && matches!(turn.state, ProvenanceTurnState::Checkpointing)
                {
                    if let Some(attempt) = turn.checkpoint_attempt {
                        source.change_hashes = attempt.source.change_hashes;
                    }
                }
            }
        }
        store.prepare_provenance_checkpoint(id, expected_generation, source, now)
    })()
    .map_err(|error| JournalError::store(ErrorCode::ProvenanceCheckpoint, error))?;
    owner_failpoint("after-checkpoint-prepare");
    Ok(attempt)
}

/// The encoded size of one empty frozen-page response frame: the protobuf
/// message plus the 5-byte gRPC length-prefix. The budget reserves this
/// overhead before dividing, exactly like the legacy JSON frame budget.
fn empty_page_frame_overhead() -> usize {
    let empty = crate::atomic::LoadFrozenEnvelopesResponse {
        page: Some(crate::atomic::FrozenPage {
            fragments: Vec::new(),
            next: Some(crate::atomic::PageCursor::default()),
        }),
    };
    empty.encode_to_vec().len() + 5
}

/// The legacy budget math: reserve the worst-case metadata and four
/// payload bytes per budget byte, then clamp to the page size.
pub fn frozen_page_budget() -> usize {
    (MAX_FRAME_BYTES.saturating_sub(empty_page_frame_overhead() + FROZEN_PAGE_METADATA_BYTES) / 4)
        .min(FROZEN_PAGE_BYTES)
}

/// Read one bounded part of the frozen journal. Continuation requests
/// (cursor past the first page) trip the before-frozen-page-continuation
/// seam; a successful read under the injected frozen-page-unavailable
/// failpoint surfaces the legacy injected error text instead.
pub fn load_frozen_page(
    store: &RedbChangeStore,
    id: ProvenanceId,
    attempt_generation: u64,
    frozen_event_count: u64,
    cursor: FrozenProvenanceCursor,
    requested_budget: Option<usize>,
) -> Result<FrozenProvenancePage, JournalError> {
    if cursor != FrozenProvenanceCursor::default() {
        owner_failpoint("before-frozen-page-continuation");
    }
    let budget = requested_budget
        .map(|requested| requested.min(frozen_page_budget()))
        .unwrap_or_else(frozen_page_budget);
    match store.load_frozen_provenance_page(
        id,
        attempt_generation,
        frozen_event_count,
        cursor,
        budget,
    ) {
        Ok(page) => {
            if std::env::var("ATOMIC_OWNER_FAILPOINT").ok().as_deref()
                == Some("frozen-page-unavailable")
            {
                return Err(JournalError {
                    code: ErrorCode::ProvenanceCheckpoint,
                    message: "injected frozen page read failure".to_string(),
                });
            }
            Ok(page)
        }
        Err(error) => Err(JournalError::store(ErrorCode::ProvenanceCheckpoint, error)),
    }
}

/// Reassemble only complete envelopes, advancing the cursor after a
/// successful page. Retrying an RPC cannot append its bytes twice.
/// Ported verbatim from the legacy owner client (owner.rs
/// `append_frozen_page`).
pub fn append_frozen_page(
    envelopes: &mut Vec<Vec<u8>>,
    partial: &mut Vec<u8>,
    cursor: &mut FrozenProvenanceCursor,
    cutoff: u64,
    page: FrozenProvenancePage,
) -> Result<(), String> {
    let end = FrozenProvenanceCursor {
        seq: cutoff,
        offset: 0,
    };
    if page.next <= *cursor || page.next > end {
        return Err("invalid frozen journal page continuation".to_string());
    }
    let mut position = *cursor;
    for fragment in page.fragments {
        // Legacy journal records can leave sequence gaps, but never inside an envelope.
        if fragment.cursor.seq >= cutoff
            || fragment.cursor < position
            || (fragment.cursor != position
                && (position.offset != 0 || fragment.cursor.offset != 0))
            || (!fragment.complete && fragment.bytes.is_empty())
        {
            return Err("invalid frozen journal fragment cursor".to_string());
        }
        partial.extend_from_slice(&fragment.bytes);
        position = if fragment.complete {
            envelopes.push(std::mem::take(partial));
            FrozenProvenanceCursor {
                seq: fragment.cursor.seq + 1,
                offset: 0,
            }
        } else {
            FrozenProvenanceCursor {
                seq: fragment.cursor.seq,
                offset: partial.len(),
            }
        };
    }
    if page.next < position
        || (page.next != position && (position.offset != 0 || page.next.offset != 0))
        || (page.next == end && !partial.is_empty())
    {
        return Err("incomplete frozen journal page".to_string());
    }
    *cursor = page.next;
    Ok(())
}

/// Load the whole frozen journal by paging through it with strict
/// continuity validation. This is the in-process counterpart of the wire
/// sink's paged loop (the daemon's journal sink reuses it directly).
pub fn load_frozen_envelopes(
    store: &RedbChangeStore,
    id: ProvenanceId,
    attempt_generation: u64,
    frozen_event_count: u64,
) -> Result<Vec<Vec<u8>>, JournalError> {
    let mut cursor = FrozenProvenanceCursor::default();
    let mut envelopes = Vec::new();
    let mut partial = Vec::new();
    while cursor.seq < frozen_event_count {
        let page = load_frozen_page(
            store,
            id,
            attempt_generation,
            frozen_event_count,
            cursor,
            None,
        )?;
        append_frozen_page(
            &mut envelopes,
            &mut partial,
            &mut cursor,
            frozen_event_count,
            page,
        )
        .map_err(|message| JournalError {
            code: ErrorCode::ProvenanceCheckpoint,
            message,
        })?;
    }
    Ok(envelopes)
}

/// Bind the prepared graph hash and exact immutable session turn once.
pub fn bind_checkpoint_hash(
    store: &RedbChangeStore,
    id: ProvenanceId,
    expected_generation: u64,
    hash: atomic_core::types::Hash,
    session_turn: atomic_core::change::session::SessionTurn,
    now: i64,
) -> Result<ProvenanceCheckpointAttempt, JournalError> {
    let attempt = store
        .bind_provenance_checkpoint_hash(id, expected_generation, hash, session_turn, now)
        .map_err(|error| JournalError::store(ErrorCode::ProvenanceCheckpoint, error))?;
    owner_failpoint("after-checkpoint-bind");
    Ok(attempt)
}

/// Acknowledge publication and complete the turn (caller-supplied time).
pub fn acknowledge_checkpoint(
    store: &RedbChangeStore,
    id: ProvenanceId,
    expected_generation: u64,
    manifest_hash: atomic_core::types::Hash,
    completed_at: i64,
) -> Result<(), JournalError> {
    store
        .acknowledge_provenance_checkpoint(id, expected_generation, manifest_hash, completed_at)
        .map_err(|error| JournalError::store(ErrorCode::ProvenanceCheckpoint, error))?;
    Ok(())
}

fn lifecycle_lookup(
    store: &RedbChangeStore,
    session_id: &str,
    turn_number: u32,
) -> Result<Option<StoredProvenanceTurn>, JournalError> {
    store
        .get_provenance_turn_for(session_id, turn_number)
        .map_err(|error| JournalError::store(ErrorCode::ProvenanceLifecycle, error))
}

/// Stop a running turn. Like the legacy owner, the looked-up turn's own
/// current generation performs the transition — a lookup never grants a
/// generation, so the request's expected_generation is shape discipline
/// only (it must be present and nonzero).
pub fn stop_turn(
    store: &RedbChangeStore,
    session_id: &str,
    turn_number: u32,
    cause: StopCause,
    resumable: bool,
    observed_at: i64,
) -> Result<Option<StoredProvenanceTurn>, JournalError> {
    let Some(turn) = lifecycle_lookup(store, session_id, turn_number)? else {
        return Ok(None);
    };
    let stopped = store
        .stop_provenance_turn(
            turn.provenance_id,
            turn.generation,
            StopState {
                cause,
                observed_at,
                last_event_seq: None,
                resumable,
            },
        )
        .map_err(|error| JournalError::store(ErrorCode::ProvenanceLifecycle, error))?;
    Ok(Some(stopped))
}

/// Resume a stopped turn with a new fencing generation and the same ID.
pub fn resume_turn(
    store: &RedbChangeStore,
    session_id: &str,
    turn_number: u32,
    now: i64,
) -> Result<Option<StoredProvenanceTurn>, JournalError> {
    let Some(turn) = lifecycle_lookup(store, session_id, turn_number)? else {
        return Ok(None);
    };
    let resumed = store
        .resume_provenance_turn(turn.provenance_id, turn.generation, now)
        .map_err(|error| JournalError::store(ErrorCode::ProvenanceLifecycle, error))?;
    Ok(Some(resumed))
}

/// Explicitly seal a pending turn without publishing a checkpoint.
pub fn abandon_turn(
    store: &RedbChangeStore,
    session_id: &str,
    turn_number: u32,
    observed_at: i64,
) -> Result<Option<StoredProvenanceTurn>, JournalError> {
    let Some(turn) = lifecycle_lookup(store, session_id, turn_number)? else {
        return Ok(None);
    };
    let abandoned = store
        .abandon_provenance_turn(
            turn.provenance_id,
            turn.generation,
            StopState {
                cause: StopCause::Abandoned,
                observed_at,
                last_event_seq: None,
                resumable: false,
            },
        )
        .map_err(|error| JournalError::store(ErrorCode::ProvenanceLifecycle, error))?;
    Ok(Some(abandoned))
}

/// Turn status (the read side of the lifecycle).
pub fn get_turn(
    store: &RedbChangeStore,
    session_id: &str,
    turn_number: u32,
) -> Result<Option<StoredProvenanceTurn>, JournalError> {
    lifecycle_lookup(store, session_id, turn_number)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frozen_page_budget_is_bounded_by_the_page_size() {
        assert_eq!(frozen_page_budget(), FROZEN_PAGE_BYTES);
    }

    #[test]
    fn metadata_reserve_uses_the_legacy_constants() {
        assert_eq!(FROZEN_PAGE_METADATA_BYTES, 256 * 256 + 256);
        assert_eq!(MAX_FROZEN_PAGE_FRAGMENTS, 256);
    }
}
