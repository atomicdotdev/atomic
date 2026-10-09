//! The RPC-based provenance journal sink — the daemon's
//! ProvenanceService spoken through the generated client.
//!
//! This is the journal transport, full stop: same trait
//! ([`atomic_agent::ProvenanceJournalSink`]), same semantics (generation
//! fencing, caller-supplied `now`, append idempotency by event ID, the
//! reuse_frozen_changes recovery, paged frozen reads with strict
//! continuity), with the transport being the `atomic` gRPC contract
//! over the daemon socket. The legacy owner protocol is deleted; the
//! daemon is the only writer of the repository databases, so every
//! journal consumer (hooks, the receive fallback, lifecycle) routes
//! here.
//!
//! # Transport notes
//!
//!  * `run_outside_async_runtime` is a plain thread and a fresh
//!    current-thread runtime per call — the sink may be invoked from
//!    inside a tokio context, where `block_on` would panic.
//!  * `request_with_reconnect` keeps the three-attempt retry; only
//!    transport failures reconnect and retry — a domain refusal
//!    (fenced generation, invalid cursor, ...) is terminal, because a
//!    FailedPrecondition can never succeed on retry. Reconnects restart
//!    the daemon per D4 (`connect_or_start`), mirroring the legacy
//!    owner's per-request `start_or_reconnect`: a daemon that dies
//!    mid-operation is brought back, and the retried operation converges
//!    through the store's idempotency (append by event ID, rebind,
//!    re-ack) exactly as the legacy owner's crash tests proved.
//!  * `chunk_wire_envelopes`/`wire_envelope_frame_cost` keep the 1 MiB
//!    wire chunk budget: protobuf encodes bytes near 1:1 and tonic's
//!    default decode limit is 4 MiB, so the legacy 4 MiB JSON-frame
//!    budget would exceed a default decode. A single envelope larger
//!    than the budget still rides its own chunk (the daemon's provenance
//!    decode limit is the contract's 64 MiB), and a full frozen page
//!    (~1 MiB payload + proto overhead) always flows.
//!
//! # D4 policy
//!
//! The journal has no alternative writer, so [`sink_for`] always routes
//! here and `ATOMIC_RPC` does not apply to the journal transport (it
//! governs command routing in [`crate::commands::rpc`] only). The sink
//! starts the daemon when it is down (`connect_or_start`); if even that
//! fails (missing binary, unwritable runtime dir) `sink_for` returns a
//! [`RefusedJournalSink`] whose every operation returns a clear error —
//! the orchestrator fails the turn loudly rather than silently dropping
//! provenance.

use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, Context};
use atomic_agent::{
    JournalAppendAck, JournalCheckpointAttempt, JournalCheckpointSource, JournalStopCause,
    JournalTurnLifecycle, JournalTurnReservation, JournalTurnStatus, ProvenanceJournalEnvelope,
    ProvenanceJournalError, ProvenanceJournalSink,
};
use atomic_client::proto as pb;
use atomic_client::AtomicClient;
use atomic_core::change::session::SessionTurn;
use atomic_core::types::Hash;
use clap::Args;
use libatomic::daemon::journal_sink::DirectJournalSink;
use uuid::Uuid;

use crate::commands::Command;
use crate::error::{CliError, CliResult};

/// Wire append chunk budget (see the module doc for the deviation from
/// the legacy 4 MiB JSON-frame budget).
const APPEND_CHUNK_BUDGET_BYTES: usize = 1024 * 1024;

/// The journal contract's frame ceiling — mirrors GetCapabilities
/// Limits.max_message_bytes and the daemon's provenance decode limit.
const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

// ---------------------------------------------------------------------------
// the sink
// ---------------------------------------------------------------------------

pub(crate) struct ProvenanceRpcSink {
    /// The resolved persistent repository reference (path discovery
    /// happened once, when the sink was opened).
    reference: pb::RepositoryRef,
}

impl ProvenanceRpcSink {
    /// Connect to the daemon — starting it when it is down (D4) — and
    /// resolve the repository. Returns `None` (never an error) only when
    /// the daemon could not be started; the caller then uses the refusal
    /// sink, whose operations carry the startup failure into the turn.
    pub(crate) fn open(root: &Path) -> Option<Self> {
        let root = root.to_path_buf();
        run_outside_async_runtime(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("failed to create provenance rpc runtime")?;
            rt.block_on(async {
                let client = AtomicClient::connect_or_start()
                    .await
                    .map_err(|e| anyhow!(e))?;
                let resolved = resolve_repository(&client, &root).await?;
                Ok(Self {
                    reference: resolved,
                })
            })
        })
        .ok()
    }
}

/// The journal sink every consumer installs, following the service mode
/// ([`crate::service`]): local journals through libatomic's
/// [`DirectJournalSink`] — the same in-process core the transport's own
/// dispatch handler uses; reactor journals over the daemon's
/// ProvenanceService (D4 start-or-retry). When even the chosen backend
/// cannot open, a [`RefusedJournalSink`] makes the failure loud — the
/// orchestrator fails the turn with the reason instead of silently
/// dropping provenance.
pub(crate) fn sink_for(root: &Path) -> Arc<dyn ProvenanceJournalSink> {
    match crate::service::mode() {
        crate::service::Mode::Local => match DirectJournalSink::open(root) {
            Ok(sink) => Arc::new(sink),
            Err(reason) => Arc::new(RefusedJournalSink::local(reason)),
        },
        crate::service::Mode::Reactor => match ProvenanceRpcSink::open(root) {
            Some(sink) => Arc::new(sink),
            None => Arc::new(RefusedJournalSink::reactor()),
        },
    }
}

/// The no-backend sink: every operation refuses with the reason the chosen
/// backend could not provide a journal. Never silently drops provenance —
/// the orchestrator surfaces these as `ProvenanceJournalFailed`.
struct RefusedJournalSink {
    reason: String,
}

impl RefusedJournalSink {
    /// The local service could not open the in-process journal core
    /// (changes.redb) — e.g. a redb single-writer lock held by a
    /// concurrent process on this machine.
    fn local(reason: String) -> Self {
        Self {
            reason: format!("provenance journal unavailable (local service): {reason}"),
        }
    }

    /// The reactor transport could not be started (missing
    /// `ATOMIC_DAEMON_BIN`, unwritable runtime dir).
    fn reactor() -> Self {
        Self {
            reason: "provenance journal unavailable: the atomic daemon could not be started \
                     (check ATOMIC_DAEMON_BIN and the socket runtime dir)"
                .to_string(),
        }
    }
}

async fn resolve_repository(
    client: &AtomicClient,
    root: &Path,
) -> anyhow::Result<pb::RepositoryRef> {
    let mut daemon = pb::daemon_service_client::DaemonServiceClient::new(client.channel.clone());
    let resolved = daemon
        .resolve_repository(pb::ResolveRepositoryRequest {
            path: root.display().to_string(),
        })
        .await
        .context("resolve repository")?
        .into_inner();
    resolved
        .repository
        .ok_or_else(|| anyhow!("daemon resolved no repository"))
}

fn request_meta(now: i64) -> Option<pb::RequestMeta> {
    Some(pb::RequestMeta {
        request_id: Uuid::new_v4().to_string(),
        observed_at: Some(prost_types::Timestamp {
            seconds: now,
            nanos: 0,
        }),
    })
}

fn provenance_client(
    client: AtomicClient,
) -> pb::provenance_service_client::ProvenanceServiceClient<tonic::transport::Channel> {
    pb::provenance_service_client::ProvenanceServiceClient::new(client.channel.clone())
        .max_decoding_message_size(MAX_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_MESSAGE_BYTES)
}

/// The legacy `request_with_reconnect`, ported: up to three attempts,
/// reconnecting through `connect_or_start` between transport failures —
/// a daemon that died mid-operation is restarted (D4), so a retried
/// idempotent operation (append by event ID, rebind, re-ack) converges
/// after a crash, mirroring the legacy owner's `start_or_reconnect`.
/// Domain refusals are terminal (see the module doc).
async fn request_with_reconnect<T, F, Fut>(operation: &str, request: F) -> anyhow::Result<T>
where
    F: Fn(AtomicClient) -> Fut,
    Fut: std::future::Future<Output = Result<T, tonic::Status>>,
{
    let mut last_error: Option<anyhow::Error> = None;
    for _ in 0..3 {
        let client = match AtomicClient::connect_or_start().await {
            Ok(client) => client,
            Err(error) => {
                last_error = Some(anyhow!("daemon rpc {operation} failed: {error}"));
                continue;
            }
        };
        match request(client).await {
            Ok(value) => return Ok(value),
            Err(status) => {
                if status.code() == tonic::Code::Unavailable {
                    last_error = Some(anyhow!(
                        "daemon rpc {operation} failed: {}",
                        status.message()
                    ));
                    continue;
                }
                // A domain refusal carries the store's verbatim message
                // (e.g. "generation is stale", the injected page error).
                return Err(anyhow!(
                    "daemon rpc {operation} failed: {}",
                    status.message()
                ));
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("daemon rpc {operation} retry exhausted")))
}

fn run_outside_async_runtime<T, F>(operation: F) -> anyhow::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
{
    std::thread::spawn(operation)
        .join()
        .map_err(|_| anyhow!("provenance rpc client thread panicked"))?
}

fn current_thread_runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to create provenance rpc runtime")
}

fn hash_proto(hash: &Hash) -> pb::Hash {
    pb::Hash {
        value: hash.as_bytes().to_vec(),
        algorithm: pb::HashAlgorithm::Blake3 as i32,
    }
}

fn hash_domain(hash: &Option<pb::Hash>) -> anyhow::Result<Hash> {
    let bytes: [u8; 32] = hash
        .as_ref()
        .ok_or_else(|| anyhow!("hash missing in response"))?
        .value
        .clone()
        .try_into()
        .map_err(|_| anyhow!("hash is not 32 bytes"))?;
    Ok(Hash::from_bytes(bytes))
}

fn checkpoint_source_proto(source: &JournalCheckpointSource) -> pb::CheckpointSource {
    pb::CheckpointSource {
        agent_name: source.agent_name.clone(),
        agent_display_name: source.agent_display_name.clone(),
        agent_vendor: source.agent_vendor.clone(),
        change_hashes: source.change_hashes.iter().map(hash_proto).collect(),
        previous_provenance: source.previous_provenance.as_ref().map(hash_proto),
        plan_id: source.plan_id.clone(),
        ledger_turn_number: source.ledger_turn_number,
    }
}

fn checkpoint_source_domain(source: &Option<pb::CheckpointSource>) -> JournalCheckpointSource {
    let Some(source) = source else {
        return JournalCheckpointSource {
            agent_name: String::new(),
            agent_display_name: String::new(),
            agent_vendor: String::new(),
            change_hashes: Vec::new(),
            previous_provenance: None,
            plan_id: None,
            ledger_turn_number: 0,
        };
    };
    JournalCheckpointSource {
        agent_name: source.agent_name.clone(),
        agent_display_name: source.agent_display_name.clone(),
        agent_vendor: source.agent_vendor.clone(),
        change_hashes: source
            .change_hashes
            .iter()
            .filter_map(|hash| hash.value.clone().try_into().ok())
            .map(|bytes: [u8; 32]| Hash::from_bytes(bytes))
            .collect(),
        previous_provenance: source.previous_provenance.as_ref().and_then(|hash| {
            hash.value
                .clone()
                .try_into()
                .ok()
                .map(|bytes: [u8; 32]| Hash::from_bytes(bytes))
        }),
        plan_id: source.plan_id.clone(),
        ledger_turn_number: source.ledger_turn_number,
    }
}

/// Rebuild the journal attempt from the wire attempt. The wire session
/// turn carries only the identity pair; the provenance hash rides the
/// attempt itself, and the timestamp comes from the prepared-at stamp —
/// mirroring exactly what the daemon persisted (see the server's
/// `enriched_session_turn`).
fn journal_checkpoint_attempt(
    provenance_id: u64,
    attempt: &pb::ProvenanceCheckpointAttempt,
) -> JournalCheckpointAttempt {
    JournalCheckpointAttempt {
        provenance_id,
        attempt_generation: attempt.attempt_generation,
        frozen_event_count: attempt.frozen_event_count,
        source: checkpoint_source_domain(&attempt.source),
        provenance_hash: attempt.provenance_hash.as_ref().and_then(|h| {
            h.value
                .clone()
                .try_into()
                .ok()
                .map(|bytes: [u8; 32]| Hash::from_bytes(bytes))
        }),
        session_turn: attempt.session_turn.as_ref().map(|turn| SessionTurn {
            session_id: turn.session_id.clone(),
            turn_number: turn.turn_number,
            goal: None,
            provenance_hash: attempt
                .provenance_hash
                .as_ref()
                .and_then(|hash| hash.value.clone().try_into().ok())
                .map(|bytes: [u8; 32]| Hash::from_bytes(bytes))
                .unwrap_or(atomic_core::types::Merkle::ZERO),
            change_hashes: Vec::new(),
            previous_provenance: None,
            timestamp: attempt
                .prepared_at
                .as_ref()
                .map(|stamp| stamp.seconds)
                .unwrap_or(0),
            plan_id: None,
            todos: Vec::new(),
            boundary_start: None,
            boundary_end: None,
            outcome: None,
        }),
        manifest_hash: attempt.manifest_hash.as_ref().and_then(|hash| {
            hash.value
                .clone()
                .try_into()
                .ok()
                .map(|bytes: [u8; 32]| Hash::from_bytes(bytes))
        }),
    }
}

fn journal_stop_cause(cause: i32) -> Option<JournalStopCause> {
    match pb::StopCause::try_from(cause) {
        Ok(pb::StopCause::UserRequested) => Some(JournalStopCause::UserRequested),
        Ok(pb::StopCause::LeaseExpired) => Some(JournalStopCause::LeaseExpired),
        Ok(pb::StopCause::ProcessExited) => Some(JournalStopCause::ProcessExited),
        Ok(pb::StopCause::HookFailure) => Some(JournalStopCause::HookFailure),
        Ok(pb::StopCause::SystemShutdown) => Some(JournalStopCause::SystemShutdown),
        Ok(pb::StopCause::Abandoned) => Some(JournalStopCause::Abandoned),
        _ => None,
    }
}

fn journal_turn_status(turn: &pb::StoredProvenanceTurn) -> anyhow::Result<JournalTurnStatus> {
    use pb::ProvenanceTurnState as State;
    let lifecycle = match State::try_from(turn.state) {
        Ok(State::Running) => JournalTurnLifecycle::Running,
        Ok(State::Checkpointing) => JournalTurnLifecycle::Checkpointing,
        Ok(State::Completed) => JournalTurnLifecycle::Completed,
        Ok(State::Stopped) => {
            let stop = turn
                .stop_state
                .as_ref()
                .ok_or_else(|| anyhow!("stopped turn without stop state"))?;
            JournalTurnLifecycle::Stopped {
                cause: journal_stop_cause(stop.cause)
                    .ok_or_else(|| anyhow!("unknown stop cause {}", stop.cause))?,
                observed_at: stop.observed_at.as_ref().map(|t| t.seconds).unwrap_or(0),
                last_event_seq: stop.last_event_seq,
                resumable: stop.resumable,
            }
        }
        Ok(State::Abandoned) => {
            let stop = turn
                .stop_state
                .as_ref()
                .ok_or_else(|| anyhow!("abandoned turn without stop state"))?;
            JournalTurnLifecycle::Abandoned {
                observed_at: stop.observed_at.as_ref().map(|t| t.seconds).unwrap_or(0),
                last_event_seq: stop.last_event_seq,
            }
        }
        _ => return Err(anyhow!("unknown turn state {}", turn.state)),
    };
    Ok(JournalTurnStatus {
        provenance_id: turn.provenance_id,
        generation: turn.generation,
        lifecycle,
    })
}

/// The proto PageCursor carries no ordering; compare (seq, offset).
fn cursor_cmp(left: &pb::PageCursor, right: &pb::PageCursor) -> std::cmp::Ordering {
    (left.seq, left.offset).cmp(&(right.seq, right.offset))
}

// ---------------------------------------------------------------------------
// append chunking (ported; see the module doc for the budget deviation)
// ---------------------------------------------------------------------------

fn wire_envelope_frame_cost(envelope: &pb::WireEnvelope) -> usize {
    envelope.envelope.len() + 2 * envelope.event_id.len() + 128
}

pub(crate) fn chunk_wire_envelopes(envelopes: Vec<pb::WireEnvelope>) -> Vec<Vec<pb::WireEnvelope>> {
    let mut chunks: Vec<Vec<pb::WireEnvelope>> = Vec::new();
    let mut current: Vec<pb::WireEnvelope> = Vec::new();
    let mut current_cost = 0usize;
    for envelope in envelopes {
        let cost = wire_envelope_frame_cost(&envelope);
        if !current.is_empty() && current_cost + cost > APPEND_CHUNK_BUDGET_BYTES {
            chunks.push(std::mem::take(&mut current));
            current_cost = 0;
        }
        current_cost += cost;
        current.push(envelope);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

// ---------------------------------------------------------------------------
// frozen page reassembly (ported verbatim from the legacy owner client)
// ---------------------------------------------------------------------------

/// Reassemble only complete envelopes, advancing the cursor after a
/// successful page. Retrying an RPC cannot append its bytes twice.
fn append_frozen_page(
    envelopes: &mut Vec<Vec<u8>>,
    partial: &mut Vec<u8>,
    cursor: &mut pb::PageCursor,
    cutoff: u64,
    page: pb::FrozenPage,
) -> anyhow::Result<()> {
    let next = page
        .next
        .ok_or_else(|| anyhow!("frozen page without a continuation cursor"))?;
    let end = pb::PageCursor {
        seq: cutoff,
        offset: 0,
    };
    if cursor_cmp(&next, cursor) != std::cmp::Ordering::Greater
        || cursor_cmp(&next, &end) == std::cmp::Ordering::Greater
    {
        return Err(anyhow!("invalid frozen journal page continuation"));
    }
    let mut position = *cursor;
    for fragment in page.fragments {
        let fragment_cursor = fragment
            .cursor
            .ok_or_else(|| anyhow!("frozen fragment without a cursor"))?;
        // Legacy journal records can leave sequence gaps, but never inside an envelope.
        if fragment_cursor.seq >= cutoff
            || cursor_cmp(&fragment_cursor, &position) == std::cmp::Ordering::Less
            || (fragment_cursor != position
                && (position.offset != 0 || fragment_cursor.offset != 0))
            || (!fragment.complete && fragment.bytes.is_empty())
        {
            return Err(anyhow!("invalid frozen journal fragment cursor"));
        }
        partial.extend_from_slice(&fragment.bytes);
        position = if fragment.complete {
            envelopes.push(std::mem::take(partial));
            pb::PageCursor {
                seq: fragment_cursor.seq + 1,
                offset: 0,
            }
        } else {
            pb::PageCursor {
                seq: fragment_cursor.seq,
                offset: partial.len() as u64,
            }
        };
    }
    if cursor_cmp(&next, &position) == std::cmp::Ordering::Less
        || (next != position && (position.offset != 0 || next.offset != 0))
        || (next == end && !partial.is_empty())
    {
        return Err(anyhow!("incomplete frozen journal page"));
    }
    *cursor = next;
    Ok(())
}

// ---------------------------------------------------------------------------
// the trait
// ---------------------------------------------------------------------------

impl ProvenanceJournalSink for ProvenanceRpcSink {
    fn reserve_turn(
        &self,
        session_id: &str,
        turn_number: u32,
        now: i64,
    ) -> Result<JournalTurnReservation, String> {
        let reference = self.reference.clone();
        let session_id = session_id.to_string();
        run_outside_async_runtime(move || {
            current_thread_runtime()?.block_on(async {
                let response = request_with_reconnect("reserve turn", |client| {
                    let reference = reference.clone();
                    let session_id = session_id.clone();
                    async move {
                        provenance_client(client)
                            .reserve_turn(pb::ReserveTurnRequest {
                                repository: Some(reference),
                                meta: request_meta(now),
                                session_id,
                                turn_number,
                                view: None,
                            })
                            .await
                            .map(|response| response.into_inner())
                    }
                })
                .await?;
                let turn = response
                    .turn
                    .ok_or_else(|| anyhow!("reserve returned no turn"))?;
                Ok(JournalTurnReservation {
                    provenance_id: turn.provenance_id,
                    generation: turn.generation,
                })
            })
        })
        .map_err(|error: anyhow::Error| error.to_string())
    }

    fn append(
        &self,
        reservation: JournalTurnReservation,
        envelopes: Vec<ProvenanceJournalEnvelope>,
        now: i64,
    ) -> Result<Vec<JournalAppendAck>, String> {
        let reference = self.reference.clone();
        let wire = envelopes
            .iter()
            .map(|envelope| {
                Ok(pb::WireEnvelope {
                    event_id: envelope.event_id.clone(),
                    type_url: String::new(),
                    envelope: envelope.to_json_bytes()?,
                })
            })
            .collect::<Result<Vec<_>, ProvenanceJournalError>>()
            .map_err(|error| error.to_string())?;
        let chunks = chunk_wire_envelopes(wire);
        run_outside_async_runtime(move || {
            current_thread_runtime()?.block_on(async {
                let mut acknowledgements = Vec::new();
                for chunk in chunks {
                    let response = request_with_reconnect("append provenance", |client| {
                        let chunk = chunk.clone();
                        let reference = reference.clone();
                        async move {
                            provenance_client(client)
                                .append_envelopes(pb::AppendEnvelopesRequest {
                                    repository: Some(reference.clone()),
                                    meta: request_meta(now),
                                    provenance_id: reservation.provenance_id,
                                    expected_generation: reservation.generation,
                                    envelopes: chunk,
                                })
                                .await
                                .map(|response| response.into_inner())
                        }
                    })
                    .await?;
                    acknowledgements.extend(response.acknowledgements.into_iter().map(|ack| {
                        JournalAppendAck {
                            event_id: ack.event_id,
                            sequence: ack.sequence,
                        }
                    }));
                }
                Ok(acknowledgements)
            })
        })
        .map_err(|error: anyhow::Error| error.to_string())
    }

    fn prepare_checkpoint(
        &self,
        reservation: JournalTurnReservation,
        source: JournalCheckpointSource,
        now: i64,
    ) -> Result<JournalCheckpointAttempt, String> {
        let reference = self.reference.clone();
        // The legacy client computed reuse the same way: an empty change
        // set with a recorded change means a previous Stop's journal read
        // failed after the change was recorded.
        let reuse_frozen_changes = source.change_hashes.is_empty();
        let source = checkpoint_source_proto(&source);
        let provenance_id = reservation.provenance_id;
        run_outside_async_runtime(move || {
            current_thread_runtime()?.block_on(async {
                let response = request_with_reconnect("prepare checkpoint", |client| {
                    let reference = reference.clone();
                    let source = source.clone();
                    async move {
                        provenance_client(client)
                            .prepare_checkpoint(pb::PrepareCheckpointRequest {
                                repository: Some(reference),
                                meta: request_meta(now),
                                provenance_id,
                                expected_generation: reservation.generation,
                                source: Some(source),
                                reuse_frozen_changes,
                            })
                            .await
                            .map(|response| response.into_inner())
                    }
                })
                .await?;
                let attempt = response
                    .attempt
                    .ok_or_else(|| anyhow!("prepare returned no attempt"))?;
                Ok(journal_checkpoint_attempt(provenance_id, &attempt))
            })
        })
        .map_err(|error: anyhow::Error| error.to_string())
    }

    fn load_frozen_envelopes(
        &self,
        checkpoint: &JournalCheckpointAttempt,
    ) -> Result<Vec<Vec<u8>>, String> {
        let reference = self.reference.clone();
        let provenance_id = checkpoint.provenance_id;
        let attempt_generation = checkpoint.attempt_generation;
        let frozen_event_count = checkpoint.frozen_event_count;
        run_outside_async_runtime(move || {
            current_thread_runtime()?.block_on(async {
                let mut cursor = pb::PageCursor::default();
                let mut envelopes = Vec::new();
                let mut partial = Vec::new();
                let mut first = true;
                while cursor.seq < frozen_event_count {
                    let response = request_with_reconnect("load frozen envelopes page", |client| {
                        let cursor = cursor;
                        let first = first;
                        let reference = reference.clone();
                        async move {
                            provenance_client(client)
                                .load_frozen_envelopes(pb::LoadFrozenEnvelopesRequest {
                                    repository: Some(reference.clone()),
                                    provenance_id,
                                    attempt_generation,
                                    frozen_event_count,
                                    cursor: if first { None } else { Some(cursor) },
                                    budget: None,
                                })
                                .await
                                .map(|response| response.into_inner())
                        }
                    })
                    .await?;
                    first = false;
                    let page = response
                        .page
                        .ok_or_else(|| anyhow!("frozen read returned no page"))?;
                    append_frozen_page(
                        &mut envelopes,
                        &mut partial,
                        &mut cursor,
                        frozen_event_count,
                        page,
                    )?;
                }
                Ok(envelopes)
            })
        })
        .map_err(|error: anyhow::Error| error.to_string())
    }

    fn bind_checkpoint_hash(
        &self,
        checkpoint: &JournalCheckpointAttempt,
        hash: Hash,
        session_turn: SessionTurn,
        now: i64,
    ) -> Result<JournalCheckpointAttempt, String> {
        let reference = self.reference.clone();
        let provenance_id = checkpoint.provenance_id;
        let expected_generation = checkpoint.attempt_generation;
        let hash = hash_proto(&hash);
        let wire_turn = pb::SessionTurn {
            session_id: session_turn.session_id.clone(),
            turn_number: session_turn.turn_number,
        };
        run_outside_async_runtime(move || {
            current_thread_runtime()?.block_on(async {
                let response = request_with_reconnect("bind checkpoint hash", |client| {
                    let reference = reference.clone();
                    let hash = hash.clone();
                    let wire_turn = wire_turn.clone();
                    async move {
                        provenance_client(client)
                            .bind_checkpoint_hash(pb::BindCheckpointHashRequest {
                                repository: Some(reference),
                                meta: request_meta(now),
                                provenance_id,
                                expected_generation,
                                hash: Some(hash),
                                session_turn: Some(wire_turn),
                            })
                            .await
                            .map(|response| response.into_inner())
                    }
                })
                .await?;
                let attempt = response
                    .attempt
                    .ok_or_else(|| anyhow!("bind returned no attempt"))?;
                Ok(journal_checkpoint_attempt(provenance_id, &attempt))
            })
        })
        .map_err(|error: anyhow::Error| error.to_string())
    }

    fn acknowledge_checkpoint(
        &self,
        checkpoint: &JournalCheckpointAttempt,
        manifest_hash: Hash,
        completed_at: i64,
    ) -> Result<(), String> {
        let reference = self.reference.clone();
        let provenance_id = checkpoint.provenance_id;
        let expected_generation = checkpoint.attempt_generation;
        let manifest_hash = hash_proto(&manifest_hash);
        run_outside_async_runtime(move || {
            current_thread_runtime()?.block_on(async {
                request_with_reconnect("acknowledge checkpoint", |client| {
                    let reference = reference.clone();
                    let manifest_hash = manifest_hash.clone();
                    async move {
                        provenance_client(client)
                            .acknowledge_checkpoint(pb::AcknowledgeCheckpointRequest {
                                repository: Some(reference),
                                meta: request_meta(completed_at),
                                provenance_id,
                                expected_generation,
                                manifest_hash: Some(manifest_hash),
                                completed_at: Some(prost_types::Timestamp {
                                    seconds: completed_at,
                                    nanos: 0,
                                }),
                            })
                            .await
                            .map(|_| ())
                    }
                })
                .await
            })
        })
        .map_err(|error: anyhow::Error| error.to_string())
    }

    fn stop_turn(
        &self,
        session_id: &str,
        turn_number: u32,
        cause: JournalStopCause,
        resumable: bool,
        observed_at: i64,
    ) -> Result<Option<JournalTurnStatus>, String> {
        let cause = match cause {
            JournalStopCause::UserRequested => pb::StopCause::UserRequested,
            JournalStopCause::LeaseExpired => pb::StopCause::LeaseExpired,
            JournalStopCause::ProcessExited => pb::StopCause::ProcessExited,
            JournalStopCause::HookFailure => pb::StopCause::HookFailure,
            JournalStopCause::SystemShutdown => pb::StopCause::SystemShutdown,
            JournalStopCause::Abandoned => pb::StopCause::Abandoned,
        };
        update_turn(
            self,
            session_id,
            turn_number,
            pb::TurnAction::Stop,
            Some(cause),
            Some(resumable),
            observed_at,
        )
    }

    fn resume_turn(
        &self,
        session_id: &str,
        turn_number: u32,
        now: i64,
    ) -> Result<Option<JournalTurnStatus>, String> {
        update_turn(
            self,
            session_id,
            turn_number,
            pb::TurnAction::Resume,
            None,
            None,
            now,
        )
    }

    fn abandon_turn(
        &self,
        session_id: &str,
        turn_number: u32,
        observed_at: i64,
    ) -> Result<Option<JournalTurnStatus>, String> {
        update_turn(
            self,
            session_id,
            turn_number,
            pb::TurnAction::Abandon,
            None,
            None,
            observed_at,
        )
    }

    fn turn_status(
        &self,
        session_id: &str,
        turn_number: u32,
    ) -> Result<Option<JournalTurnStatus>, String> {
        let reference = self.reference.clone();
        let session_id = session_id.to_string();
        run_outside_async_runtime(move || {
            current_thread_runtime()?.block_on(async {
                let response = request_with_reconnect("turn status", |client| {
                    let reference = reference.clone();
                    let session_id = session_id.clone();
                    async move {
                        provenance_client(client)
                            .get_turn(pb::GetTurnRequest {
                                repository: Some(reference),
                                session_id,
                                turn_number,
                            })
                            .await
                            .map(|response| response.into_inner())
                    }
                })
                .await?;
                match response.turn {
                    None => Ok(None),
                    Some(turn) => Ok(Some(journal_turn_status(&turn)?)),
                }
            })
        })
        .map_err(|error: anyhow::Error| error.to_string())
    }
}

/// The lifecycle transitions: GetTurn first (a lookup never grants a
/// generation — the request's expected_generation is the turn's own
/// current one), then one UpdateTurn. An unknown turn is `Ok(None)`,
/// exactly like the legacy owner's TurnLifecycle{turn: None}.
fn update_turn(
    sink: &ProvenanceRpcSink,
    session_id: &str,
    turn_number: u32,
    action: pb::TurnAction,
    cause: Option<pb::StopCause>,
    resumable: Option<bool>,
    observed_at: i64,
) -> Result<Option<JournalTurnStatus>, String> {
    let reference = sink.reference.clone();
    let session_id = session_id.to_string();
    run_outside_async_runtime(move || {
        current_thread_runtime()?.block_on(async {
            let stored = request_with_reconnect("turn lookup", |client| {
                let session_id = session_id.clone();
                let reference = reference.clone();
                async move {
                    provenance_client(client)
                        .get_turn(pb::GetTurnRequest {
                            repository: Some(reference.clone()),
                            session_id,
                            turn_number,
                        })
                        .await
                        .map(|response| response.into_inner())
                }
            })
            .await?;
            let Some(turn) = stored.turn else {
                return Ok(None);
            };
            let response = request_with_reconnect("update turn", |client| {
                let reference = reference.clone();
                let turn = turn.clone();
                async move {
                    provenance_client(client)
                        .update_turn(pb::UpdateTurnRequest {
                            repository: Some(reference),
                            meta: request_meta(observed_at),
                            session_id: turn.session_id.clone(),
                            turn_number,
                            action: action as i32,
                            cause: cause.map(|cause| cause as i32),
                            resumable,
                            // The looked-up turn's own current generation —
                            // a lookup never grants one, so this is exactly
                            // what the legacy owner used internally.
                            expected_generation: turn.generation,
                        })
                        .await
                        .map(|response| response.into_inner())
                }
            })
            .await?;
            match response.turn {
                None => Ok(None),
                Some(turn) => Ok(Some(journal_turn_status(&turn)?)),
            }
        })
    })
    .map_err(|error: anyhow::Error| error.to_string())
}

impl ProvenanceJournalSink for RefusedJournalSink {
    fn reserve_turn(
        &self,
        _session_id: &str,
        _turn_number: u32,
        _now: i64,
    ) -> Result<JournalTurnReservation, String> {
        Err(self.reason.clone())
    }

    fn append(
        &self,
        _reservation: JournalTurnReservation,
        _envelopes: Vec<ProvenanceJournalEnvelope>,
        _now: i64,
    ) -> Result<Vec<JournalAppendAck>, String> {
        Err(self.reason.clone())
    }

    fn prepare_checkpoint(
        &self,
        _reservation: JournalTurnReservation,
        _source: JournalCheckpointSource,
        _now: i64,
    ) -> Result<JournalCheckpointAttempt, String> {
        Err(self.reason.clone())
    }

    fn load_frozen_envelopes(
        &self,
        _checkpoint: &JournalCheckpointAttempt,
    ) -> Result<Vec<Vec<u8>>, String> {
        Err(self.reason.clone())
    }

    fn bind_checkpoint_hash(
        &self,
        _checkpoint: &JournalCheckpointAttempt,
        _hash: Hash,
        _session_turn: SessionTurn,
        _now: i64,
    ) -> Result<JournalCheckpointAttempt, String> {
        Err(self.reason.clone())
    }

    fn acknowledge_checkpoint(
        &self,
        _checkpoint: &JournalCheckpointAttempt,
        _manifest_hash: Hash,
        _completed_at: i64,
    ) -> Result<(), String> {
        Err(self.reason.clone())
    }

    fn stop_turn(
        &self,
        _session_id: &str,
        _turn_number: u32,
        _cause: JournalStopCause,
        _resumable: bool,
        _observed_at: i64,
    ) -> Result<Option<JournalTurnStatus>, String> {
        Err(self.reason.clone())
    }

    fn resume_turn(
        &self,
        _session_id: &str,
        _turn_number: u32,
        _now: i64,
    ) -> Result<Option<JournalTurnStatus>, String> {
        Err(self.reason.clone())
    }

    fn abandon_turn(
        &self,
        _session_id: &str,
        _turn_number: u32,
        _observed_at: i64,
    ) -> Result<Option<JournalTurnStatus>, String> {
        Err(self.reason.clone())
    }

    fn turn_status(
        &self,
        _session_id: &str,
        _turn_number: u32,
    ) -> Result<Option<JournalTurnStatus>, String> {
        Err(self.reason.clone())
    }
}

// ---------------------------------------------------------------------------
// hidden self-test command (drives the real sink over the real socket)
// ---------------------------------------------------------------------------

/// Drive `ProvenanceRpcSink` through the full trait flow against a
/// reachable daemon. Hidden (like the self-test commands generally are):
/// it exists for the integration suite, not for users.
#[derive(Debug, Args)]
#[command(hide = true)]
pub struct JournalRpcSelftest {
    /// Path inside the repository to journal for.
    #[arg(long, default_value = ".")]
    repository: std::path::PathBuf,

    /// Stable external agent session ID.
    #[arg(long, default_value = "rpc-selftest")]
    session_id: String,

    /// Turn number within the session.
    #[arg(long, default_value = "1")]
    turn: u32,

    /// How many envelopes to append.
    #[arg(long, default_value = "6")]
    envelopes: usize,

    /// Payload bytes per envelope.
    #[arg(long, default_value = "524288")]
    envelope_bytes: usize,

    /// Emit a machine-readable JSON verdict.
    #[arg(long)]
    json: bool,
}

impl Command for JournalRpcSelftest {
    fn run(&self) -> CliResult<()> {
        // This selftest exists to exercise the REACTOR journal transport
        // (D4 start-or-retry, wire chunking) — it must never start a
        // daemon in local mode. Refuse early, with the opt-in.
        if !matches!(crate::service::mode(), crate::service::Mode::Reactor) {
            return Err(CliError::InvalidArgument {
                message: "the journal RPC selftest exercises the reactor transport; \
                          set ATOMIC_SERVICE=reactor (it D4-starts the daemon when down)"
                    .to_string(),
            });
        }
        let root = crate::commands::find_repository_root_from(&self.repository)?;
        let Some(sink) = ProvenanceRpcSink::open(&root) else {
            return Err(CliError::InvalidArgument {
                message: "no reachable atomic daemon; the selftest exercises the RPC sink"
                    .to_string(),
            });
        };
        let sink = Arc::new(sink);

        let now = 1_700_000_000i64;
        let reservation = sink
            .reserve_turn(&self.session_id, self.turn, now)
            .map_err(|reason| CliError::Internal(anyhow!("reserve failed: {reason}")))?;

        let mut envelopes = Vec::new();
        for index in 0..self.envelopes {
            let event = atomic_agent::ProvenanceJournalEvent::Tool {
                phase: atomic_agent::ProvenanceToolPhase::After,
                tool_name: "selftest".to_string(),
                tool_call_id: Some(format!("selftest-{index}")),
                input: None,
                output: Some(serde_json::Value::String("x".repeat(self.envelope_bytes))),
                status: Some("completed".to_string()),
                duration_ms: Some(4),
                raw: None,
            };
            envelopes.push(ProvenanceJournalEnvelope::new(
                format!("selftest-{index}"),
                &self.session_id,
                self.turn,
                reservation.generation,
                now * 1000 + index as i64,
                event,
            ));
        }
        let expected: Vec<Vec<u8>> = envelopes
            .iter()
            .map(|envelope| envelope.to_json_bytes().unwrap())
            .collect();
        // The same chunking the sink performs — reported so the test can
        // assert a multi-envelope batch really split.
        let wire: Vec<_> = envelopes
            .iter()
            .map(|envelope| pb::WireEnvelope {
                event_id: envelope.event_id.clone(),
                type_url: String::new(),
                envelope: envelope.to_json_bytes().unwrap(),
            })
            .collect();
        let chunk_count = chunk_wire_envelopes(wire).len();

        let acknowledgements = sink
            .append(reservation, envelopes, now + 1)
            .map_err(|reason| CliError::Internal(anyhow!("append failed: {reason}")))?;

        let source = JournalCheckpointSource {
            agent_name: "selftest".to_string(),
            agent_display_name: "Selftest".to_string(),
            agent_vendor: "atomic".to_string(),
            change_hashes: Vec::new(),
            previous_provenance: None,
            plan_id: None,
            ledger_turn_number: self.turn,
        };
        let checkpoint = sink
            .prepare_checkpoint(reservation, source, now + 2)
            .map_err(|reason| CliError::Internal(anyhow!("prepare failed: {reason}")))?;

        let frozen = sink
            .load_frozen_envelopes(&checkpoint)
            .map_err(|reason| CliError::Internal(anyhow!("load frozen failed: {reason}")))?;
        let reassembled_equal = frozen == expected;

        let graph_hash = Hash::of(format!("selftest-graph-{}", self.session_id).as_bytes());
        let session_turn = SessionTurn {
            session_id: self.session_id.clone(),
            turn_number: self.turn,
            goal: None,
            provenance_hash: graph_hash,
            change_hashes: Vec::new(),
            previous_provenance: None,
            timestamp: now + 3,
            plan_id: None,
            todos: Vec::new(),
            boundary_start: None,
            boundary_end: None,
            outcome: None,
        };
        let bound = sink
            .bind_checkpoint_hash(&checkpoint, graph_hash, session_turn, now + 3)
            .map_err(|reason| CliError::Internal(anyhow!("bind failed: {reason}")))?;
        let bound_hash = bound.provenance_hash == Some(graph_hash);

        let manifest_hash = Hash::of(format!("selftest-manifest-{}", self.session_id).as_bytes());
        sink.acknowledge_checkpoint(&bound, manifest_hash, now + 4)
            .map_err(|reason| CliError::Internal(anyhow!("acknowledge failed: {reason}")))?;

        // Lifecycle on a second turn: stop then observe the status.
        let second = sink
            .reserve_turn(&self.session_id, self.turn + 1, now + 5)
            .map_err(|reason| CliError::Internal(anyhow!("second reserve failed: {reason}")))?;
        let _ = second;
        let stopped = sink
            .stop_turn(
                &self.session_id,
                self.turn + 1,
                JournalStopCause::UserRequested,
                true,
                now + 6,
            )
            .map_err(|reason| CliError::Internal(anyhow!("stop failed: {reason}")))?;
        let status = sink
            .turn_status(&self.session_id, self.turn + 1)
            .map_err(|reason| CliError::Internal(anyhow!("status failed: {reason}")))?;

        let report = serde_json::json!({
            "reservation": {
                "provenance_id": reservation.provenance_id,
                "generation": reservation.generation,
            },
            "acks": acknowledgements.len(),
            "ack_sequences": acknowledgements.iter().map(|ack| ack.sequence).collect::<Vec<_>>(),
            "chunk_count": chunk_count,
            "frozen_envelopes": frozen.len(),
            "reassembled_equal": reassembled_equal,
            "attempt_generation": checkpoint.attempt_generation,
            "bound_hash": bound_hash,
            "stopped": stopped.is_some(),
            "status_lifecycle": format!("{:?}", status.map(|status| status.lifecycle)),
        });
        if self.json {
            println!("{report}");
        } else {
            println!("provenance rpc selftest: {report}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire_envelope(index: usize, bytes: usize) -> pb::WireEnvelope {
        pb::WireEnvelope {
            event_id: format!("event-{index}"),
            type_url: String::new(),
            envelope: vec![0u8; bytes],
        }
    }

    #[test]
    fn chunking_splits_oversized_batches() {
        let envelopes: Vec<_> = (0..6)
            .map(|i| wire_envelope(i, 2 * APPEND_CHUNK_BUDGET_BYTES))
            .collect();
        let chunks = chunk_wire_envelopes(envelopes);
        assert_eq!(chunks.len(), 6);
        for chunk in &chunks {
            assert_eq!(chunk.len(), 1);
        }
    }

    #[test]
    fn chunking_packs_small_batches() {
        let envelopes: Vec<_> = (0..1_000).map(|i| wire_envelope(i, 16)).collect();
        let chunks = chunk_wire_envelopes(envelopes);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), 1_000);
    }

    #[test]
    fn chunking_splits_a_multi_megabyte_tool_batch() {
        // The selftest shape: six ~500 KiB wire envelopes against a 1 MiB
        // budget — two per chunk, three chunks.
        let envelopes: Vec<_> = (0..6).map(|i| wire_envelope(i, 500 * 1024)).collect();
        let chunks = chunk_wire_envelopes(envelopes);
        assert_eq!(chunks.len(), 3);
        for chunk in &chunks {
            assert_eq!(chunk.len(), 2);
        }
    }

    #[test]
    fn chunking_preserves_all_envelopes_in_order() {
        let envelopes: Vec<_> = (0..2_000).map(|i| wire_envelope(i, 64 * 1024)).collect();
        let chunks = chunk_wire_envelopes(envelopes);
        let flattened: Vec<_> = chunks.into_iter().flatten().collect();
        assert_eq!(flattened.len(), 2_000);
        for (index, envelope) in flattened.iter().enumerate() {
            assert_eq!(envelope.event_id, format!("event-{index}"));
        }
    }

    fn fragment(seq: u64, offset: u64, bytes: &[u8], complete: bool) -> pb::FrozenFragment {
        pb::FrozenFragment {
            cursor: Some(pb::PageCursor { seq, offset }),
            bytes: bytes.to_vec(),
            complete,
        }
    }

    #[test]
    fn frozen_pages_reassemble_split_envelopes_and_reject_bad_continuations() {
        let mut envelopes = Vec::new();
        let mut partial = Vec::new();
        let mut cursor = pb::PageCursor::default();

        // One envelope split across two pages.
        let first = pb::FrozenPage {
            fragments: vec![fragment(0, 0, b"hel", false)],
            next: Some(pb::PageCursor { seq: 0, offset: 3 }),
        };
        append_frozen_page(&mut envelopes, &mut partial, &mut cursor, 2, first).unwrap();
        assert!(partial == b"hel");
        let second = pb::FrozenPage {
            fragments: vec![fragment(0, 3, b"lo", true), fragment(1, 0, b"world", true)],
            next: Some(pb::PageCursor { seq: 2, offset: 0 }),
        };
        append_frozen_page(&mut envelopes, &mut partial, &mut cursor, 2, second).unwrap();
        assert!(partial.is_empty());
        assert_eq!(envelopes, vec![b"hello".to_vec(), b"world".to_vec()]);
        assert_eq!(cursor.seq, 2);

        // A page that does not advance the cursor is refused.
        let mut envelopes = Vec::new();
        let mut partial = Vec::new();
        let mut cursor = pb::PageCursor::default();
        let stuck = pb::FrozenPage {
            fragments: vec![fragment(0, 0, b"hello", true)],
            next: Some(pb::PageCursor::default()),
        };
        assert!(
            append_frozen_page(&mut envelopes, &mut partial, &mut cursor, 2, stuck)
                .unwrap_err()
                .to_string()
                .contains("invalid frozen journal page continuation")
        );

        // A fragment that rewinds inside an envelope is refused.
        let mut envelopes = Vec::new();
        let mut partial = Vec::new();
        let mut cursor = pb::PageCursor::default();
        let rewound = pb::FrozenPage {
            fragments: vec![fragment(0, 0, b"hel", false), fragment(0, 1, b"lo", true)],
            next: Some(pb::PageCursor { seq: 1, offset: 0 }),
        };
        assert!(
            append_frozen_page(&mut envelopes, &mut partial, &mut cursor, 2, rewound)
                .unwrap_err()
                .to_string()
                .contains("invalid frozen journal fragment cursor")
        );

        // An incomplete fragment with no bytes is refused.
        let empty = pb::FrozenPage {
            fragments: vec![fragment(0, 0, b"", false)],
            next: Some(pb::PageCursor { seq: 1, offset: 0 }),
        };
        let mut envelopes = Vec::new();
        let mut partial = Vec::new();
        let mut cursor = pb::PageCursor::default();
        assert!(
            append_frozen_page(&mut envelopes, &mut partial, &mut cursor, 2, empty)
                .unwrap_err()
                .to_string()
                .contains("invalid frozen journal fragment cursor")
        );
    }
}
