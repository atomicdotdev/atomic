//! ProvenanceService journal handlers — the wire half of the provenance
//! journal port (Slice 2).
//!
//! Each of the eight journal RPCs maps its request onto the shared
//! [`crate::daemon::provenance_core`] functions (the same code path the
//! in-process dispatch uses), behind the per-repository gate — changes.redb
//! is one writable handle per process, exactly like pristine.
//!
//! Semantics parity with the legacy owner protocol (owner.rs
//! `handle_request`): generation fencing with the store's verbatim error
//! text, caller-supplied `now` (never a server clock), the five named
//! crash-injection failpoints, prepare's reuse_frozen_changes recovery, and
//! strict-continuity frozen-envelope paging. Error classification follows
//! the legacy codes: reserve/append → PROVENANCE_STORE, prepare/load/bind/
//! acknowledge → PROVENANCE_CHECKPOINT, lifecycle → PROVENANCE_LIFECYCLE.

use std::sync::Arc;

use crate::atomic::ErrorCode;
use crate::atomic::{
    AcknowledgeCheckpointRequest, AcknowledgeCheckpointResponse, AppendEnvelopesRequest,
    AppendEnvelopesResponse, BindCheckpointHashRequest, BindCheckpointHashResponse,
    CheckpointSource, EnvelopeAck, FrozenFragment, FrozenPage, GetTurnRequest, GetTurnResponse,
    HashAlgorithm, LoadFrozenEnvelopesRequest, LoadFrozenEnvelopesResponse, PageCursor,
    PrepareCheckpointRequest, PrepareCheckpointResponse, ProvenanceCheckpointAttempt, RequestMeta,
    ReserveTurnRequest, ReserveTurnResponse, StopCause as PbStopCause, StopState as PbStopState,
    StoredProvenanceTurn, TurnAction, UpdateTurnRequest, UpdateTurnResponse,
};
use atomic_core::change::session::SessionTurn;
use atomic_core::types::Hash;
use atomic_repository::redb_change_store::{
    FrozenProvenanceCursor, ProvenanceCheckpointAttempt as StoreAttempt, ProvenanceCheckpointPhase,
    ProvenanceCheckpointSource, ProvenanceId, ProvenanceTurnState, StopCause, StopState,
    StoredProvenanceTurn as StoreTurn,
};
use tonic::{Request, Response, Status};

use super::convert::hash_proto;
use super::provenance_core as core;
use super::services_agent::response_meta;
use super::state::{domain_status, DaemonState};

// ---------------------------------------------------------------------------
// domain ↔ protobuf mapping
// ---------------------------------------------------------------------------

/// The caller-supplied observation time. The server NEVER substitutes its
/// own clock for observation times — `now` rides the request exactly like
/// the legacy protocol's explicit `now` field.
fn observed_now(meta: &Option<RequestMeta>) -> i64 {
    meta.as_ref()
        .and_then(|meta| meta.observed_at.as_ref())
        .map(|stamp| stamp.seconds)
        .unwrap_or(0)
}

fn timestamp(seconds: i64) -> prost_types::Timestamp {
    prost_types::Timestamp { seconds, nanos: 0 }
}

fn stored_turn_proto(turn: &StoreTurn) -> StoredProvenanceTurn {
    use crate::atomic::ProvenanceTurnState as PbState;
    let (state, stop_state) = match &turn.state {
        ProvenanceTurnState::Running => (PbState::Running, None),
        ProvenanceTurnState::Stopped(stop) => (PbState::Stopped, Some(stop_state_proto(stop))),
        ProvenanceTurnState::Checkpointing => (PbState::Checkpointing, None),
        ProvenanceTurnState::Completed => (PbState::Completed, None),
        ProvenanceTurnState::Abandoned(stop) => (PbState::Abandoned, Some(stop_state_proto(stop))),
    };
    StoredProvenanceTurn {
        schema_version: turn.schema_version as u32,
        provenance_id: turn.provenance_id.get(),
        session_id: turn.session_id.clone(),
        turn_number: turn.turn_number,
        state: state as i32,
        generation: turn.generation,
        next_event_seq: turn.next_event_seq,
        created_at: Some(timestamp(turn.created_at)),
        updated_at: Some(timestamp(turn.updated_at)),
        completed_at: turn.completed_at.map(timestamp),
        final_hash: turn.final_hash.as_ref().map(hash_proto),
        checkpoint_attempt: turn.checkpoint_attempt.as_ref().map(attempt_proto),
        stop_state,
    }
}

fn stop_state_proto(stop: &StopState) -> PbStopState {
    PbStopState {
        cause: match stop.cause {
            StopCause::UserRequested => PbStopCause::UserRequested,
            StopCause::LeaseExpired => PbStopCause::LeaseExpired,
            StopCause::ProcessExited => PbStopCause::ProcessExited,
            StopCause::HookFailure => PbStopCause::HookFailure,
            StopCause::SystemShutdown => PbStopCause::SystemShutdown,
            StopCause::Abandoned => PbStopCause::Abandoned,
        } as i32,
        observed_at: Some(timestamp(stop.observed_at)),
        last_event_seq: stop.last_event_seq,
        resumable: stop.resumable,
    }
}

fn checkpoint_source_proto(source: &ProvenanceCheckpointSource) -> CheckpointSource {
    CheckpointSource {
        agent_name: source.agent_name.clone(),
        agent_display_name: source.agent_display_name.clone(),
        agent_vendor: source.agent_vendor.clone(),
        change_hashes: source.change_hashes.iter().map(hash_proto).collect(),
        previous_provenance: source.previous_provenance.as_ref().map(hash_proto),
        plan_id: source.plan_id.clone(),
        ledger_turn_number: source.ledger_turn_number,
    }
}

fn attempt_proto(attempt: &StoreAttempt) -> ProvenanceCheckpointAttempt {
    use crate::atomic::CheckpointPhase as PbPhase;
    let phase = match attempt.phase {
        ProvenanceCheckpointPhase::Prepared => PbPhase::Prepared,
        ProvenanceCheckpointPhase::HashBound => PbPhase::HashBound,
        ProvenanceCheckpointPhase::Published => PbPhase::Published,
    };
    ProvenanceCheckpointAttempt {
        attempt_generation: attempt.attempt_generation,
        frozen_event_count: attempt.frozen_event_count,
        source: Some(checkpoint_source_proto(&attempt.source)),
        phase: phase as i32,
        provenance_hash: attempt.provenance_hash.as_ref().map(hash_proto),
        session_turn: attempt
            .session_turn
            .as_ref()
            .map(|turn| crate::atomic::SessionTurn {
                session_id: turn.session_id.clone(),
                turn_number: turn.turn_number,
            }),
        manifest_hash: attempt.manifest_hash.as_ref().map(hash_proto),
        prepared_at: Some(timestamp(attempt.prepared_at)),
        updated_at: Some(timestamp(attempt.updated_at)),
    }
}

fn hash_domain(hash: &Option<crate::atomic::Hash>) -> Result<Hash, Status> {
    let hash = hash
        .as_ref()
        .ok_or_else(|| domain_status(ErrorCode::InvalidArgument, "hash is required"))?;
    let bytes: [u8; 32] =
        hash.value.clone().try_into().map_err(|_| {
            domain_status(ErrorCode::InvalidArgument, "hash must be exactly 32 bytes")
        })?;
    Ok(Hash::from_bytes(bytes))
}

fn checkpoint_source_domain(source: &CheckpointSource) -> ProvenanceCheckpointSource {
    ProvenanceCheckpointSource {
        agent_name: source.agent_name.clone(),
        agent_display_name: source.agent_display_name.clone(),
        agent_vendor: source.agent_vendor.clone(),
        change_hashes: source
            .change_hashes
            .iter()
            .filter_map(|hash| hash.value.clone().try_into().ok())
            .map(Hash::from_bytes)
            .collect(),
        previous_provenance: source
            .previous_provenance
            .as_ref()
            .and_then(|hash| hash.value.clone().try_into().ok().map(Hash::from_bytes)),
        plan_id: source.plan_id.clone(),
        ledger_turn_number: source.ledger_turn_number,
    }
}

/// The wire SessionTurn carries only the identity pair; the domain bind
/// requires the full stored turn shape. Enrich from the prepared
/// checkpoint's frozen source (change hashes, previous provenance, plan,
/// prepared_at as the stable timestamp) so a retried bind reconstructs
/// the IDENTICAL session turn and hits the store's idempotency path —
/// the goal/todos fields remain a wire-contract limitation (the
/// in-process dispatch path keeps the full session turn).
fn enriched_session_turn(
    store: &atomic_repository::redb_change_store::RedbChangeStore,
    id: ProvenanceId,
    turn: &crate::atomic::SessionTurn,
    hash: Hash,
    now: i64,
) -> SessionTurn {
    let mut enriched = SessionTurn {
        session_id: turn.session_id.clone(),
        turn_number: turn.turn_number,
        goal: None,
        provenance_hash: hash,
        change_hashes: Vec::new(),
        previous_provenance: None,
        timestamp: now,
        plan_id: None,
        todos: Vec::new(),
    };
    if let Ok(Some(stored)) = store.get_provenance_turn(id) {
        if let Some(attempt) = stored.checkpoint_attempt {
            enriched.change_hashes = attempt.source.change_hashes;
            enriched.previous_provenance = attempt.source.previous_provenance;
            enriched.plan_id = attempt.source.plan_id;
            // Deterministic across retries (a retry's observed_at differs;
            // the prepared_at does not) — bind idempotency depends on it.
            enriched.timestamp = attempt.prepared_at;
        }
    }
    enriched
}

fn core_status(error: core::JournalError) -> Status {
    domain_status(error.code, error.message)
}

fn hash_is_blake3(hash: &crate::atomic::Hash) -> bool {
    hash.algorithm == HashAlgorithm::Blake3 as i32 || hash.algorithm == 0
}

// ---------------------------------------------------------------------------
// ProvenanceService — the eight journal RPCs
// ---------------------------------------------------------------------------

pub async fn reserve_turn_impl(
    state: &Arc<DaemonState>,
    request: Request<ReserveTurnRequest>,
) -> Result<Response<ReserveTurnResponse>, Status> {
    let request = request.into_inner();
    let handle = state.resolve(request.repository.as_ref().unwrap())?;
    state.log_rpc("ReserveTurn", Some(&handle));
    let gate_handle = handle.clone();
    let _gate = gate_handle.exclusive().await;
    let meta = request.meta.clone();
    let now = observed_now(&request.meta);
    let session_id = request.session_id.clone();
    let turn_number = request.turn_number;
    let response = tokio::task::spawn_blocking(move || {
        let store = handle.change_store()?;
        let turn =
            core::reserve_turn(&store, &session_id, turn_number, now).map_err(core_status)?;
        // reserve_provenance_turn returns only after txn.commit().
        Ok::<_, Status>(ReserveTurnResponse {
            turn: Some(stored_turn_proto(&turn)),
            committed: true,
            meta: response_meta(&meta),
        })
    })
    .await
    .map_err(|error| Status::internal(error.to_string()))??;
    Ok(Response::new(response))
}

pub async fn append_envelopes_impl(
    state: &Arc<DaemonState>,
    request: Request<AppendEnvelopesRequest>,
) -> Result<Response<AppendEnvelopesResponse>, Status> {
    let request = request.into_inner();
    let handle = state.resolve(request.repository.as_ref().unwrap())?;
    state.log_rpc("AppendEnvelopes", Some(&handle));
    let gate_handle = handle.clone();
    let _gate = gate_handle.exclusive().await;
    let meta = request.meta.clone();
    let now = observed_now(&request.meta);
    let provenance_id = request.provenance_id;
    let expected_generation = request.expected_generation;
    let batch: Vec<(String, Vec<u8>)> = request
        .envelopes
        .into_iter()
        .map(|envelope| (envelope.event_id, envelope.envelope))
        .collect();
    let response = tokio::task::spawn_blocking(move || {
        let store = handle.change_store()?;
        let refs: Vec<(&str, &[u8])> = batch
            .iter()
            .map(|(event_id, bytes)| (event_id.as_str(), bytes.as_slice()))
            .collect();
        let stored = core::append_envelopes(
            &store,
            ProvenanceId::new(provenance_id),
            expected_generation,
            &refs,
            now,
        )
        .map_err(core_status)?;
        let acknowledgements = stored
            .into_iter()
            .map(|event| EnvelopeAck {
                event_id: event.event_id,
                sequence: event.seq,
            })
            .collect();
        Ok::<_, Status>(AppendEnvelopesResponse {
            acknowledgements,
            meta: response_meta(&meta),
        })
    })
    .await
    .map_err(|error| Status::internal(error.to_string()))??;
    Ok(Response::new(response))
}

pub async fn prepare_checkpoint_impl(
    state: &Arc<DaemonState>,
    request: Request<PrepareCheckpointRequest>,
) -> Result<Response<PrepareCheckpointResponse>, Status> {
    let request = request.into_inner();
    let handle = state.resolve(request.repository.as_ref().unwrap())?;
    state.log_rpc("PrepareCheckpoint", Some(&handle));
    let gate_handle = handle.clone();
    let _gate = gate_handle.exclusive().await;
    let meta = request.meta.clone();
    let now = observed_now(&request.meta);
    let provenance_id = request.provenance_id;
    let expected_generation = request.expected_generation;
    let reuse = request.reuse_frozen_changes;
    let source = request
        .source
        .as_ref()
        .ok_or_else(|| domain_status(ErrorCode::InvalidArgument, "checkpoint source is required"))?
        .clone();
    let response = tokio::task::spawn_blocking(move || {
        let store = handle.change_store()?;
        let attempt = core::prepare_checkpoint(
            &store,
            ProvenanceId::new(provenance_id),
            expected_generation,
            checkpoint_source_domain(&source),
            reuse,
            now,
        )
        .map_err(core_status)?;
        Ok::<_, Status>(PrepareCheckpointResponse {
            attempt: Some(attempt_proto(&attempt)),
            meta: response_meta(&meta),
        })
    })
    .await
    .map_err(|error| Status::internal(error.to_string()))??;
    Ok(Response::new(response))
}

pub async fn load_frozen_envelopes_impl(
    state: &Arc<DaemonState>,
    request: Request<LoadFrozenEnvelopesRequest>,
) -> Result<Response<LoadFrozenEnvelopesResponse>, Status> {
    let request = request.into_inner();
    let handle = state.resolve(request.repository.as_ref().unwrap())?;
    state.log_rpc("LoadFrozenEnvelopes", Some(&handle));
    let gate_handle = handle.clone();
    let _gate = gate_handle.exclusive().await;
    let provenance_id = request.provenance_id;
    let attempt_generation = request.attempt_generation;
    let frozen_event_count = request.frozen_event_count;
    let cursor = request
        .cursor
        .map(|cursor| FrozenProvenanceCursor {
            seq: cursor.seq,
            offset: cursor.offset as usize,
        })
        .unwrap_or_default();
    let requested_budget = request
        .budget
        .as_ref()
        .and_then(|budget| budget.max_bytes)
        .map(|max| max as usize);
    let response = tokio::task::spawn_blocking(move || {
        let store = handle.change_store()?;
        let page = core::load_frozen_page(
            &store,
            ProvenanceId::new(provenance_id),
            attempt_generation,
            frozen_event_count,
            cursor,
            requested_budget,
        )
        .map_err(core_status)?;
        let fragments = page
            .fragments
            .into_iter()
            .map(|fragment| FrozenFragment {
                cursor: Some(PageCursor {
                    seq: fragment.cursor.seq,
                    offset: fragment.cursor.offset as u64,
                }),
                bytes: fragment.bytes,
                complete: fragment.complete,
            })
            .collect();
        Ok::<_, Status>(LoadFrozenEnvelopesResponse {
            page: Some(FrozenPage {
                fragments,
                next: Some(PageCursor {
                    seq: page.next.seq,
                    offset: page.next.offset as u64,
                }),
            }),
        })
    })
    .await
    .map_err(|error| Status::internal(error.to_string()))??;
    Ok(Response::new(response))
}

pub async fn bind_checkpoint_hash_impl(
    state: &Arc<DaemonState>,
    request: Request<BindCheckpointHashRequest>,
) -> Result<Response<BindCheckpointHashResponse>, Status> {
    let request = request.into_inner();
    let handle = state.resolve(request.repository.as_ref().unwrap())?;
    state.log_rpc("BindCheckpointHash", Some(&handle));
    let gate_handle = handle.clone();
    let _gate = gate_handle.exclusive().await;
    let meta = request.meta.clone();
    let now = observed_now(&request.meta);
    let provenance_id = request.provenance_id;
    let expected_generation = request.expected_generation;
    let request_hash = request.hash.clone();
    let hash = hash_domain(&request_hash)?;
    if !request_hash.as_ref().map(hash_is_blake3).unwrap_or(false) {
        return Err(domain_status(
            ErrorCode::InvalidArgument,
            "only blake3 hashes are accepted",
        ));
    }
    let session_turn = request
        .session_turn
        .clone()
        .ok_or_else(|| domain_status(ErrorCode::InvalidArgument, "session turn is required"))?;
    let response = tokio::task::spawn_blocking(move || {
        let store = handle.change_store()?;
        let session_turn = enriched_session_turn(
            &store,
            ProvenanceId::new(provenance_id),
            &session_turn,
            hash,
            now,
        );
        let attempt = core::bind_checkpoint_hash(
            &store,
            ProvenanceId::new(provenance_id),
            expected_generation,
            hash,
            session_turn,
            now,
        )
        .map_err(core_status)?;
        Ok::<_, Status>(BindCheckpointHashResponse {
            attempt: Some(attempt_proto(&attempt)),
            meta: response_meta(&meta),
        })
    })
    .await
    .map_err(|error| Status::internal(error.to_string()))??;
    Ok(Response::new(response))
}

pub async fn acknowledge_checkpoint_impl(
    state: &Arc<DaemonState>,
    request: Request<AcknowledgeCheckpointRequest>,
) -> Result<Response<AcknowledgeCheckpointResponse>, Status> {
    let request = request.into_inner();
    let handle = state.resolve(request.repository.as_ref().unwrap())?;
    state.log_rpc("AcknowledgeCheckpoint", Some(&handle));
    let gate_handle = handle.clone();
    let _gate = gate_handle.exclusive().await;
    let meta = request.meta.clone();
    // Caller-supplied completion time — never the server clock.
    let completed_at = request.completed_at.as_ref().map(|stamp| stamp.seconds);
    let completed_at = completed_at
        .ok_or_else(|| domain_status(ErrorCode::InvalidArgument, "completed_at is required"))?;
    let provenance_id = request.provenance_id;
    let expected_generation = request.expected_generation;
    let manifest_hash = hash_domain(&request.manifest_hash)?;
    let response = tokio::task::spawn_blocking(move || {
        let store = handle.change_store()?;
        core::acknowledge_checkpoint(
            &store,
            ProvenanceId::new(provenance_id),
            expected_generation,
            manifest_hash,
            completed_at,
        )
        .map_err(core_status)?;
        Ok::<_, Status>(AcknowledgeCheckpointResponse {
            meta: response_meta(&meta),
        })
    })
    .await
    .map_err(|error| Status::internal(error.to_string()))??;
    Ok(Response::new(response))
}

pub async fn update_turn_impl(
    state: &Arc<DaemonState>,
    request: Request<UpdateTurnRequest>,
) -> Result<Response<UpdateTurnResponse>, Status> {
    let request = request.into_inner();
    let handle = state.resolve(request.repository.as_ref().unwrap())?;
    state.log_rpc("UpdateTurn", Some(&handle));
    let gate_handle = handle.clone();
    let _gate = gate_handle.exclusive().await;
    let meta = request.meta.clone();
    let observed_at = observed_now(&request.meta);
    let session_id = request.session_id.clone();
    let turn_number = request.turn_number;
    let action = TurnAction::try_from(request.action)
        .map_err(|_| domain_status(ErrorCode::InvalidArgument, "unknown turn action"))?;
    if action == TurnAction::Unspecified {
        return Err(domain_status(
            ErrorCode::InvalidArgument,
            "turn action is required",
        ));
    }
    // Required, nonzero: a lookup never grants a generation, so this is
    // request-shape discipline (the transition uses the looked-up
    // turn's own current generation, exactly like the legacy owner).
    if request.expected_generation == 0 {
        return Err(domain_status(
            ErrorCode::InvalidArgument,
            "expected_generation is required and must be nonzero",
        ));
    }
    let response = tokio::task::spawn_blocking(move || {
        let store = handle.change_store()?;
        let turn = match action {
            TurnAction::Stop => {
                let cause = request
                    .cause
                    .and_then(|cause| PbStopCause::try_from(cause).ok())
                    .and_then(|cause| match cause {
                        PbStopCause::UserRequested => Some(StopCause::UserRequested),
                        PbStopCause::LeaseExpired => Some(StopCause::LeaseExpired),
                        PbStopCause::ProcessExited => Some(StopCause::ProcessExited),
                        PbStopCause::HookFailure => Some(StopCause::HookFailure),
                        PbStopCause::SystemShutdown => Some(StopCause::SystemShutdown),
                        PbStopCause::Abandoned => Some(StopCause::Abandoned),
                        PbStopCause::Unspecified => None,
                    })
                    .ok_or_else(|| {
                        domain_status(ErrorCode::InvalidArgument, "stop cause is required")
                    })?;
                let resumable = request.resumable.unwrap_or(false);
                core::stop_turn(
                    &store,
                    &session_id,
                    turn_number,
                    cause,
                    resumable,
                    observed_at,
                )
            }
            TurnAction::Resume => core::resume_turn(&store, &session_id, turn_number, observed_at),
            TurnAction::Abandon => {
                core::abandon_turn(&store, &session_id, turn_number, observed_at)
            }
            TurnAction::Unspecified => unreachable!("validated above"),
        }
        .map_err(core_status)?;
        Ok::<_, Status>(UpdateTurnResponse {
            turn: turn.as_ref().map(stored_turn_proto),
            meta: response_meta(&meta),
        })
    })
    .await
    .map_err(|error| Status::internal(error.to_string()))??;
    Ok(Response::new(response))
}

pub async fn get_turn_impl(
    state: &Arc<DaemonState>,
    request: Request<GetTurnRequest>,
) -> Result<Response<GetTurnResponse>, Status> {
    let request = request.into_inner();
    let handle = state.resolve(request.repository.as_ref().unwrap())?;
    state.log_rpc("GetTurn", Some(&handle));
    let gate_handle = handle.clone();
    let _gate = gate_handle.exclusive().await;
    let session_id = request.session_id.clone();
    let turn_number = request.turn_number;
    let response = tokio::task::spawn_blocking(move || {
        let store = handle.change_store()?;
        let turn = core::get_turn(&store, &session_id, turn_number).map_err(core_status)?;
        Ok::<_, Status>(GetTurnResponse {
            turn: turn.as_ref().map(stored_turn_proto),
        })
    })
    .await
    .map_err(|error| Status::internal(error.to_string()))??;
    Ok(Response::new(response))
}
