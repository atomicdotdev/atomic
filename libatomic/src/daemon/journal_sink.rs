//! The daemon's in-process provenance journal sink.
//!
//! The turn orchestrator (hosted by `DispatchTurnEvent`) journals through
//! the [`ProvenanceJournalSink`] trait. In the daemon the changes.redb
//! store is already open in THIS process — the sink writes to it directly
//! instead of speaking the dismantled legacy owner protocol over a socket
//! to itself. The semantics are exactly the legacy owner's request
//! handlers: generation fencing, request-ID echo and caller-supplied `now`
//! live in the store layer itself.

use std::sync::Arc;

use atomic_agent::{
    JournalAppendAck, JournalCheckpointAttempt, JournalCheckpointSource, JournalStopCause,
    JournalTurnLifecycle, JournalTurnReservation, JournalTurnStatus, ProvenanceJournalEnvelope,
    ProvenanceJournalSink,
};
use atomic_core::change::session::SessionTurn;
use atomic_core::types::Hash;
use atomic_repository::redb_change_store::{
    FrozenProvenanceCursor, FrozenProvenancePage, ProvenanceCheckpointAttempt as StoreAttempt,
    ProvenanceCheckpointSource, ProvenanceId, StopCause, StopState, StoredProvenanceTurn,
};
use atomic_repository::Repository;

pub struct DirectJournalSink {
    store: Arc<atomic_repository::redb_change_store::RedbChangeStore>,
}

impl DirectJournalSink {
    pub fn open(root: &std::path::Path) -> Result<Self, String> {
        let path =
            Repository::canonical_change_store_path(root).map_err(|error| error.to_string())?;
        let store = atomic_repository::redb_change_store::RedbChangeStore::open(&path)
            .map_err(|error| format!("failed to open {}: {error}", path.display()))?;
        Ok(Self {
            store: Arc::new(store),
        })
    }
}

fn to_store_stop_cause(cause: JournalStopCause) -> StopCause {
    match cause {
        JournalStopCause::UserRequested => StopCause::UserRequested,
        JournalStopCause::LeaseExpired => StopCause::LeaseExpired,
        JournalStopCause::ProcessExited => StopCause::ProcessExited,
        JournalStopCause::HookFailure => StopCause::HookFailure,
        JournalStopCause::SystemShutdown => StopCause::SystemShutdown,
        JournalStopCause::Abandoned => StopCause::Abandoned,
    }
}

fn to_journal_stop_cause(cause: StopCause) -> JournalStopCause {
    match cause {
        StopCause::UserRequested => JournalStopCause::UserRequested,
        StopCause::LeaseExpired => JournalStopCause::LeaseExpired,
        StopCause::ProcessExited => JournalStopCause::ProcessExited,
        StopCause::HookFailure => JournalStopCause::HookFailure,
        StopCause::SystemShutdown => JournalStopCause::SystemShutdown,
        StopCause::Abandoned => JournalStopCause::Abandoned,
    }
}

fn to_journal_turn_status(turn: StoredProvenanceTurn) -> JournalTurnStatus {
    let lifecycle = match turn.state {
        atomic_repository::redb_change_store::ProvenanceTurnState::Running => {
            JournalTurnLifecycle::Running
        }
        atomic_repository::redb_change_store::ProvenanceTurnState::Stopped(stop) => {
            JournalTurnLifecycle::Stopped {
                cause: to_journal_stop_cause(stop.cause),
                observed_at: stop.observed_at,
                last_event_seq: stop.last_event_seq,
                resumable: stop.resumable,
            }
        }
        atomic_repository::redb_change_store::ProvenanceTurnState::Checkpointing => {
            JournalTurnLifecycle::Checkpointing
        }
        atomic_repository::redb_change_store::ProvenanceTurnState::Completed => {
            JournalTurnLifecycle::Completed
        }
        atomic_repository::redb_change_store::ProvenanceTurnState::Abandoned(stop) => {
            JournalTurnLifecycle::Abandoned {
                observed_at: stop.observed_at,
                last_event_seq: stop.last_event_seq,
            }
        }
    };
    JournalTurnStatus {
        provenance_id: turn.provenance_id.get(),
        generation: turn.generation,
        lifecycle,
    }
}

fn to_store_checkpoint_source(source: JournalCheckpointSource) -> ProvenanceCheckpointSource {
    ProvenanceCheckpointSource {
        agent_name: source.agent_name,
        agent_display_name: source.agent_display_name,
        agent_vendor: source.agent_vendor,
        change_hashes: source.change_hashes,
        previous_provenance: source.previous_provenance,
        plan_id: source.plan_id,
        ledger_turn_number: source.ledger_turn_number,
    }
}

fn to_journal_checkpoint_attempt(
    provenance_id: u64,
    attempt: StoreAttempt,
) -> JournalCheckpointAttempt {
    JournalCheckpointAttempt {
        provenance_id,
        attempt_generation: attempt.attempt_generation,
        frozen_event_count: attempt.frozen_event_count,
        source: JournalCheckpointSource {
            agent_name: attempt.source.agent_name,
            agent_display_name: attempt.source.agent_display_name,
            agent_vendor: attempt.source.agent_vendor,
            change_hashes: attempt.source.change_hashes,
            previous_provenance: attempt.source.previous_provenance,
            plan_id: attempt.source.plan_id,
            ledger_turn_number: attempt.source.ledger_turn_number,
        },
        provenance_hash: attempt.provenance_hash,
        session_turn: attempt.session_turn,
        manifest_hash: attempt.manifest_hash,
    }
}

impl ProvenanceJournalSink for DirectJournalSink {
    fn reserve_turn(
        &self,
        session_id: &str,
        turn_number: u32,
        now: i64,
    ) -> Result<JournalTurnReservation, String> {
        let turn = self
            .store
            .reserve_provenance_turn(session_id, turn_number, now)
            .map_err(|error| error.to_string())?;
        Ok(JournalTurnReservation {
            provenance_id: turn.provenance_id.get(),
            generation: turn.generation,
        })
    }

    fn append(
        &self,
        reservation: JournalTurnReservation,
        envelopes: Vec<ProvenanceJournalEnvelope>,
        now: i64,
    ) -> Result<Vec<JournalAppendAck>, String> {
        let batch: Vec<(String, Vec<u8>)> = envelopes
            .iter()
            .map(|envelope| {
                Ok((
                    envelope.event_id.clone(),
                    envelope.to_json_bytes().map_err(|e| e.to_string())?,
                ))
            })
            .collect::<Result<_, String>>()?;
        let refs: Vec<(&str, &[u8])> = batch
            .iter()
            .map(|(id, bytes)| (id.as_str(), bytes.as_slice()))
            .collect();
        let stored = self
            .store
            .append_provenance_envelopes(
                ProvenanceId::new(reservation.provenance_id),
                reservation.generation,
                &refs,
                now,
            )
            .map_err(|error| error.to_string())?;
        Ok(stored
            .into_iter()
            .map(|event| JournalAppendAck {
                event_id: event.event_id,
                sequence: event.seq,
            })
            .collect())
    }

    fn prepare_checkpoint(
        &self,
        reservation: JournalTurnReservation,
        source: JournalCheckpointSource,
        now: i64,
    ) -> Result<JournalCheckpointAttempt, String> {
        let attempt = self
            .store
            .prepare_provenance_checkpoint(
                ProvenanceId::new(reservation.provenance_id),
                reservation.generation,
                to_store_checkpoint_source(source),
                now,
            )
            .map_err(|error| error.to_string())?;
        Ok(to_journal_checkpoint_attempt(
            reservation.provenance_id,
            attempt,
        ))
    }

    fn load_frozen_envelopes(
        &self,
        checkpoint: &JournalCheckpointAttempt,
    ) -> Result<Vec<Vec<u8>>, String> {
        // The daemon is in-process with the store: no frame budgets, no
        // paging — one direct read.
        let stored = self
            .store
            .load_frozen_provenance_envelopes(ProvenanceId::new(checkpoint.provenance_id))
            .map_err(|error| error.to_string())?;
        Ok(stored.into_iter().map(|entry| entry.envelope).collect())
    }

    fn bind_checkpoint_hash(
        &self,
        checkpoint: &JournalCheckpointAttempt,
        hash: Hash,
        session_turn: SessionTurn,
        now: i64,
    ) -> Result<JournalCheckpointAttempt, String> {
        let attempt = self
            .store
            .bind_provenance_checkpoint_hash(
                ProvenanceId::new(checkpoint.provenance_id),
                checkpoint.attempt_generation,
                hash,
                session_turn,
                now,
            )
            .map_err(|error| error.to_string())?;
        Ok(to_journal_checkpoint_attempt(
            checkpoint.provenance_id,
            attempt,
        ))
    }

    fn acknowledge_checkpoint(
        &self,
        checkpoint: &JournalCheckpointAttempt,
        manifest_hash: Hash,
        completed_at: i64,
    ) -> Result<(), String> {
        self.store
            .acknowledge_provenance_checkpoint(
                ProvenanceId::new(checkpoint.provenance_id),
                checkpoint.attempt_generation,
                manifest_hash,
                completed_at,
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    fn stop_turn(
        &self,
        session_id: &str,
        turn_number: u32,
        cause: JournalStopCause,
        resumable: bool,
        observed_at: i64,
    ) -> Result<Option<JournalTurnStatus>, String> {
        let turn = self
            .store
            .get_provenance_turn_for(session_id, turn_number)
            .map_err(|error| error.to_string())?;
        let Some(turn) = turn else { return Ok(None) };
        let stopped = self
            .store
            .stop_provenance_turn(
                turn.provenance_id,
                turn.generation,
                StopState {
                    cause: to_store_stop_cause(cause),
                    observed_at,
                    last_event_seq: None,
                    resumable,
                },
            )
            .map_err(|error| error.to_string())?;
        Ok(Some(to_journal_turn_status(stopped)))
    }

    fn resume_turn(
        &self,
        session_id: &str,
        turn_number: u32,
        now: i64,
    ) -> Result<Option<JournalTurnStatus>, String> {
        let turn = self
            .store
            .get_provenance_turn_for(session_id, turn_number)
            .map_err(|error| error.to_string())?;
        let Some(turn) = turn else { return Ok(None) };
        let resumed = self
            .store
            .resume_provenance_turn(turn.provenance_id, turn.generation, now)
            .map_err(|error| error.to_string())?;
        Ok(Some(to_journal_turn_status(resumed)))
    }

    fn abandon_turn(
        &self,
        session_id: &str,
        turn_number: u32,
        observed_at: i64,
    ) -> Result<Option<JournalTurnStatus>, String> {
        let turn = self
            .store
            .get_provenance_turn_for(session_id, turn_number)
            .map_err(|error| error.to_string())?;
        let Some(turn) = turn else { return Ok(None) };
        let abandoned = self
            .store
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
            .map_err(|error| error.to_string())?;
        Ok(Some(to_journal_turn_status(abandoned)))
    }

    fn turn_status(
        &self,
        session_id: &str,
        turn_number: u32,
    ) -> Result<Option<JournalTurnStatus>, String> {
        let turn = self
            .store
            .get_provenance_turn_for(session_id, turn_number)
            .map_err(|error| error.to_string())?;
        Ok(turn.map(to_journal_turn_status))
    }
}

// The frozen-page machinery stays ported (unused in-process today, but the
// daemon keeps the seam for the hosted/remote surfaces).
#[allow(dead_code)]
fn _frozen_page_seam(page: FrozenProvenancePage) -> (Vec<FrozenProvenanceCursor>, usize) {
    (
        page.fragments.iter().map(|f| f.cursor).collect(),
        page.fragments.len(),
    )
}
