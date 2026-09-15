//! Per-repository owner for the redb-native change and provenance store.
//!
//! redb permits one writable process to open a database. This service elects
//! that process with an OS-backed file lock and exposes a small, versioned local
//! protocol so short-lived agent hooks never open `changes.redb` themselves.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context};
use atomic_agent::{
    JournalAppendAck, JournalCheckpointAttempt, JournalCheckpointSource, JournalStopCause,
    JournalTurnLifecycle, JournalTurnReservation, JournalTurnStatus, ProvenanceJournalEnvelope,
    ProvenanceJournalSink,
};
use atomic_core::change::session::SessionTurn;
use atomic_core::types::Hash;
use atomic_repository::redb_change_store::{
    FrozenProvenanceCursor, FrozenProvenancePage, ProvenanceCheckpointAttempt,
    ProvenanceCheckpointSource, ProvenanceId, ProvenanceTurnState, RedbChangeStore, StopCause,
    StopState, StoredProvenanceTurn, MAX_FROZEN_PAGE_FRAGMENTS,
};
use atomic_repository::Repository;
use clap::{Args, Subcommand};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Notify;
use uuid::Uuid;

use crate::commands::Command;
use crate::error::CliResult;

const PROTOCOL_VERSION: u16 = 1;
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
// A JSON u8 takes at most four bytes including its comma. Reserve 256 bytes
// per fragment for metadata (including 64-bit cursors), plus the next cursor.
const FROZEN_PAGE_BYTES: usize = 1024 * 1024;
const FROZEN_PAGE_METADATA_BYTES: usize = 256 * MAX_FROZEN_PAGE_FRAGMENTS + 256;
const OWNER_LOCK_FILE: &str = "changes-owner.lock";
const START_ATTEMPTS: usize = 80;
const START_RETRY_DELAY: Duration = Duration::from_millis(25);

#[derive(Debug, Args)]
#[command(arg_required_else_help = true)]
pub struct DatabaseOwner {
    #[command(subcommand)]
    command: OwnerCommand,
}

#[derive(Debug, Subcommand)]
enum OwnerCommand {
    /// Start the owner if needed, or reconnect to the existing owner.
    Start(RepositoryPath),
    /// Check owner health without starting it.
    Ping(RepositoryPath),
    /// Reserve an idempotent provenance turn through the owner.
    Reserve(ReserveArgs),
    /// Ask the owner to shut down after replying.
    Shutdown(RepositoryPath),
    /// Run the foreground owner process.
    #[command(hide = true)]
    Serve(RepositoryPath),
}

#[derive(Clone, Debug, Args)]
struct RepositoryPath {
    /// Path inside the repository to own.
    #[arg(long, default_value = ".")]
    repository: PathBuf,

    /// Emit a machine-readable response.
    #[arg(long)]
    json: bool,
}

#[derive(Clone, Debug, Args)]
struct ReserveArgs {
    /// Path inside the repository to own.
    #[arg(long, default_value = ".")]
    repository: PathBuf,

    /// Stable external agent session ID.
    #[arg(long)]
    session_id: String,

    /// Turn number within the session.
    #[arg(long)]
    turn: u32,

    /// Event timestamp in Unix seconds.
    #[arg(long)]
    now: i64,

    /// Emit a machine-readable response.
    #[arg(long)]
    json: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct RequestFrame {
    version: u16,
    request_id: String,
    request: OwnerRequest,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum OwnerRequest {
    Ping,
    ReserveProvenanceTurn {
        session_id: String,
        turn_number: u32,
        now: i64,
    },
    AppendProvenanceEnvelopes {
        provenance_id: u64,
        expected_generation: u64,
        envelopes: Vec<WireEnvelope>,
        now: i64,
    },
    PrepareCheckpoint {
        provenance_id: u64,
        expected_generation: u64,
        source: ProvenanceCheckpointSource,
        #[serde(default)]
        reuse_frozen_changes: bool,
        now: i64,
    },
    LoadFrozenEnvelopes {
        provenance_id: u64,
    },
    LoadFrozenEnvelopesPage {
        provenance_id: u64,
        attempt_generation: u64,
        frozen_event_count: u64,
        cursor: FrozenProvenanceCursor,
    },
    BindCheckpointHash {
        provenance_id: u64,
        expected_generation: u64,
        hash: Hash,
        session_turn: SessionTurn,
        now: i64,
    },
    AcknowledgeCheckpoint {
        provenance_id: u64,
        expected_generation: u64,
        manifest_hash: Hash,
        completed_at: i64,
    },
    StopTurn {
        session_id: String,
        turn_number: u32,
        cause: StopCause,
        resumable: bool,
        observed_at: i64,
    },
    ResumeTurn {
        session_id: String,
        turn_number: u32,
        now: i64,
    },
    AbandonTurn {
        session_id: String,
        turn_number: u32,
        observed_at: i64,
    },
    TurnStatus {
        session_id: String,
        turn_number: u32,
    },
    Shutdown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct WireEnvelope {
    event_id: String,
    bytes: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ResponseFrame {
    version: u16,
    request_id: String,
    response: OwnerResponse,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum OwnerResponse {
    Pong {
        pid: u32,
        // Additive v1 capability: old clients ignore it, old owners omit it.
        #[serde(default)]
        frozen_envelope_paging: bool,
    },
    ProvenanceTurn {
        turn: StoredProvenanceTurn,
        committed: bool,
    },
    ProvenanceEnvelopesCommitted {
        acknowledgements: Vec<JournalAppendAck>,
    },
    CheckpointPrepared {
        attempt: ProvenanceCheckpointAttempt,
    },
    FrozenEnvelopes {
        envelopes: Vec<Vec<u8>>,
    },
    FrozenEnvelopesPage {
        page: FrozenProvenancePage,
    },
    CheckpointAcknowledged,
    TurnLifecycle {
        turn: Option<StoredProvenanceTurn>,
    },
    ShuttingDown {
        pid: u32,
    },
    Error {
        code: String,
        message: String,
    },
}

#[derive(Clone, Debug, Serialize)]
struct HealthOutput {
    protocol_version: u16,
    pid: u32,
}

#[derive(Clone, Debug, Serialize)]
struct ReserveOutput {
    protocol_version: u16,
    committed: bool,
    turn: StoredProvenanceTurn,
}

/// Hook-facing client for committed provenance journal RPCs.
pub(crate) struct OwnerJournalSink {
    repository: PathBuf,
}

impl OwnerJournalSink {
    pub(crate) fn new(repository: impl Into<PathBuf>) -> Self {
        Self {
            repository: repository.into(),
        }
    }
}

impl ProvenanceJournalSink for OwnerJournalSink {
    fn reserve_turn(
        &self,
        session_id: &str,
        turn_number: u32,
        now: i64,
    ) -> Result<JournalTurnReservation, String> {
        let repository = self.repository.clone();
        let session_id = session_id.to_string();
        run_outside_async_runtime(move || {
            let response = request_with_reconnect(
                &repository,
                OwnerRequest::ReserveProvenanceTurn {
                    session_id,
                    turn_number,
                    now,
                },
            )?;
            match response {
                OwnerResponse::ProvenanceTurn {
                    turn,
                    committed: true,
                } => Ok(JournalTurnReservation {
                    provenance_id: turn.provenance_id.get(),
                    generation: turn.generation,
                }),
                other => Err(unexpected_response("reserve", other)),
            }
        })
        .map_err(|error| error.to_string())
    }

    fn append(
        &self,
        reservation: JournalTurnReservation,
        envelopes: Vec<ProvenanceJournalEnvelope>,
        now: i64,
    ) -> Result<Vec<JournalAppendAck>, String> {
        let repository = self.repository.clone();
        let envelopes = envelopes
            .into_iter()
            .map(|envelope| {
                Ok(WireEnvelope {
                    event_id: envelope.event_id.clone(),
                    bytes: envelope.to_json_bytes()?,
                })
            })
            .collect::<Result<Vec<_>, atomic_agent::ProvenanceJournalError>>()
            .map_err(|error| error.to_string())?;
        run_outside_async_runtime(move || {
            let response = request_with_reconnect(
                &repository,
                OwnerRequest::AppendProvenanceEnvelopes {
                    provenance_id: reservation.provenance_id,
                    expected_generation: reservation.generation,
                    envelopes,
                    now,
                },
            )?;
            match response {
                OwnerResponse::ProvenanceEnvelopesCommitted { acknowledgements } => {
                    Ok(acknowledgements)
                }
                other => Err(unexpected_response("append provenance", other)),
            }
        })
        .map_err(|error| error.to_string())
    }

    fn prepare_checkpoint(
        &self,
        reservation: JournalTurnReservation,
        source: JournalCheckpointSource,
        now: i64,
    ) -> Result<JournalCheckpointAttempt, String> {
        let repository = self.repository.clone();
        run_outside_async_runtime(move || {
            let response = request_with_reconnect(
                &repository,
                OwnerRequest::PrepareCheckpoint {
                    provenance_id: reservation.provenance_id,
                    expected_generation: reservation.generation,
                    reuse_frozen_changes: source.change_hashes.is_empty(),
                    source: to_store_checkpoint_source(source),
                    now,
                },
            )?;
            match response {
                OwnerResponse::CheckpointPrepared { attempt } => Ok(to_journal_checkpoint_attempt(
                    reservation.provenance_id,
                    attempt,
                )),
                other => Err(unexpected_response("prepare checkpoint", other)),
            }
        })
        .map_err(|error| error.to_string())
    }

    fn load_frozen_envelopes(
        &self,
        checkpoint: &JournalCheckpointAttempt,
    ) -> Result<Vec<Vec<u8>>, String> {
        let repository = self.repository.clone();
        let provenance_id = checkpoint.provenance_id;
        let attempt_generation = checkpoint.attempt_generation;
        let frozen_event_count = checkpoint.frozen_event_count;
        run_outside_async_runtime(move || {
            require_frozen_paging(start_or_reconnect(&repository)?)?;
            let mut cursor = FrozenProvenanceCursor::default();
            let mut envelopes = Vec::new();
            let mut partial = Vec::new();
            while cursor.seq < frozen_event_count {
                let response = request_with_reconnect(
                    &repository,
                    OwnerRequest::LoadFrozenEnvelopesPage {
                        provenance_id,
                        attempt_generation,
                        frozen_event_count,
                        cursor,
                    },
                )?;
                match response {
                    OwnerResponse::FrozenEnvelopesPage { page } => {
                        append_frozen_page(
                            &mut envelopes,
                            &mut partial,
                            &mut cursor,
                            frozen_event_count,
                            page,
                        )?;
                    }
                    other => return Err(unexpected_response("load frozen envelopes page", other)),
                }
            }
            Ok(envelopes)
        })
        .map_err(|error| error.to_string())
    }

    fn bind_checkpoint_hash(
        &self,
        checkpoint: &JournalCheckpointAttempt,
        hash: Hash,
        session_turn: SessionTurn,
        now: i64,
    ) -> Result<JournalCheckpointAttempt, String> {
        let repository = self.repository.clone();
        let provenance_id = checkpoint.provenance_id;
        let expected_generation = checkpoint.attempt_generation;
        run_outside_async_runtime(move || {
            match request_with_reconnect(
                &repository,
                OwnerRequest::BindCheckpointHash {
                    provenance_id,
                    expected_generation,
                    hash,
                    session_turn,
                    now,
                },
            )? {
                OwnerResponse::CheckpointPrepared { attempt } => {
                    Ok(to_journal_checkpoint_attempt(provenance_id, attempt))
                }
                other => Err(unexpected_response("bind checkpoint hash", other)),
            }
        })
        .map_err(|error| error.to_string())
    }

    fn acknowledge_checkpoint(
        &self,
        checkpoint: &JournalCheckpointAttempt,
        manifest_hash: Hash,
        completed_at: i64,
    ) -> Result<(), String> {
        let repository = self.repository.clone();
        let provenance_id = checkpoint.provenance_id;
        let expected_generation = checkpoint.attempt_generation;
        run_outside_async_runtime(move || {
            match request_with_reconnect(
                &repository,
                OwnerRequest::AcknowledgeCheckpoint {
                    provenance_id,
                    expected_generation,
                    manifest_hash,
                    completed_at,
                },
            )? {
                OwnerResponse::CheckpointAcknowledged => Ok(()),
                other => Err(unexpected_response("acknowledge checkpoint", other)),
            }
        })
        .map_err(|error| error.to_string())
    }

    fn stop_turn(
        &self,
        session_id: &str,
        turn_number: u32,
        cause: JournalStopCause,
        resumable: bool,
        observed_at: i64,
    ) -> Result<Option<JournalTurnStatus>, String> {
        let request = OwnerRequest::StopTurn {
            session_id: session_id.to_string(),
            turn_number,
            cause: to_store_stop_cause(cause),
            resumable,
            observed_at,
        };
        request_turn_lifecycle(self.repository.clone(), request).map_err(|error| error.to_string())
    }

    fn resume_turn(
        &self,
        session_id: &str,
        turn_number: u32,
        now: i64,
    ) -> Result<Option<JournalTurnStatus>, String> {
        request_turn_lifecycle(
            self.repository.clone(),
            OwnerRequest::ResumeTurn {
                session_id: session_id.to_string(),
                turn_number,
                now,
            },
        )
        .map_err(|error| error.to_string())
    }

    fn abandon_turn(
        &self,
        session_id: &str,
        turn_number: u32,
        observed_at: i64,
    ) -> Result<Option<JournalTurnStatus>, String> {
        request_turn_lifecycle(
            self.repository.clone(),
            OwnerRequest::AbandonTurn {
                session_id: session_id.to_string(),
                turn_number,
                observed_at,
            },
        )
        .map_err(|error| error.to_string())
    }

    fn turn_status(
        &self,
        session_id: &str,
        turn_number: u32,
    ) -> Result<Option<JournalTurnStatus>, String> {
        request_turn_lifecycle(
            self.repository.clone(),
            OwnerRequest::TurnStatus {
                session_id: session_id.to_string(),
                turn_number,
            },
        )
        .map_err(|error| error.to_string())
    }
}

fn request_turn_lifecycle(
    repository: PathBuf,
    request: OwnerRequest,
) -> anyhow::Result<Option<JournalTurnStatus>> {
    run_outside_async_runtime(
        move || match request_with_reconnect(&repository, request)? {
            OwnerResponse::TurnLifecycle { turn } => Ok(turn.map(to_journal_turn_status)),
            other => Err(unexpected_response("turn lifecycle", other)),
        },
    )
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
        ProvenanceTurnState::Running => JournalTurnLifecycle::Running,
        ProvenanceTurnState::Stopped(stop) => JournalTurnLifecycle::Stopped {
            cause: to_journal_stop_cause(stop.cause),
            observed_at: stop.observed_at,
            last_event_seq: stop.last_event_seq,
            resumable: stop.resumable,
        },
        ProvenanceTurnState::Checkpointing => JournalTurnLifecycle::Checkpointing,
        ProvenanceTurnState::Completed => JournalTurnLifecycle::Completed,
        ProvenanceTurnState::Abandoned(stop) => JournalTurnLifecycle::Abandoned {
            observed_at: stop.observed_at,
            last_event_seq: stop.last_event_seq,
        },
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
    attempt: ProvenanceCheckpointAttempt,
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

impl Command for DatabaseOwner {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            OwnerCommand::Start(args) => {
                let response = start_or_reconnect(&args.repository)?;
                print_health(response, args.json)
            }
            OwnerCommand::Ping(args) => {
                let response = request(&args.repository, OwnerRequest::Ping)?;
                print_health(response, args.json)
            }
            OwnerCommand::Reserve(args) => {
                start_or_reconnect(&args.repository)?;
                let response = request(
                    &args.repository,
                    OwnerRequest::ReserveProvenanceTurn {
                        session_id: args.session_id.clone(),
                        turn_number: args.turn,
                        now: args.now,
                    },
                )?;
                print_reservation(response, args.json)
            }
            OwnerCommand::Shutdown(args) => {
                let response = request(&args.repository, OwnerRequest::Shutdown)?;
                let pid = match response {
                    OwnerResponse::ShuttingDown { pid } => pid,
                    other => return Err(unexpected_response("shutdown", other).into()),
                };
                if args.json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "protocol_version": PROTOCOL_VERSION,
                            "pid": pid,
                            "status": "shutting-down"
                        })
                    );
                } else {
                    println!("database owner {pid} is shutting down");
                }
                Ok(())
            }
            OwnerCommand::Serve(args) => serve(&args.repository),
        }
    }
}

fn print_health(response: OwnerResponse, json: bool) -> CliResult<()> {
    let pid = match response {
        OwnerResponse::Pong { pid, .. } => pid,
        other => return Err(unexpected_response("ping", other).into()),
    };
    let output = HealthOutput {
        protocol_version: PROTOCOL_VERSION,
        pid,
    };
    if json {
        println!(
            "{}",
            serde_json::to_string(&output).map_err(anyhow::Error::from)?
        );
    } else {
        println!("database owner {pid} is healthy (protocol v{PROTOCOL_VERSION})");
    }
    Ok(())
}

fn print_reservation(response: OwnerResponse, json: bool) -> CliResult<()> {
    let (turn, committed) = match response {
        OwnerResponse::ProvenanceTurn { turn, committed } => (turn, committed),
        other => return Err(unexpected_response("reserve", other).into()),
    };
    let output = ReserveOutput {
        protocol_version: PROTOCOL_VERSION,
        committed,
        turn,
    };
    if json {
        println!(
            "{}",
            serde_json::to_string(&output).map_err(anyhow::Error::from)?
        );
    } else {
        println!(
            "reserved provenance turn {} generation {} (committed)",
            output.turn.provenance_id.get(),
            output.turn.generation
        );
    }
    Ok(())
}

fn unexpected_response(operation: &str, response: OwnerResponse) -> anyhow::Error {
    match response {
        OwnerResponse::Error { code, message } => {
            anyhow!("database owner {operation} failed [{code}]: {message}")
        }
        other => anyhow!("database owner returned an unexpected {operation} response: {other:?}"),
    }
}

fn require_frozen_paging(response: OwnerResponse) -> anyhow::Result<()> {
    match response {
        OwnerResponse::Pong { frozen_envelope_paging: true, .. } => Ok(()),
        OwnerResponse::Pong { .. } => Err(anyhow!(
            "database owner does not support frozen journal pagination; run `atomic agent database-owner shutdown --repository <path>` with the updated CLI, then retry the Stop hook"
        )),
        other => Err(unexpected_response("pagination capability", other)),
    }
}

/// Reassemble only complete envelopes, advancing the cursor after a successful
/// page. Retrying an RPC cannot append its bytes twice.
fn append_frozen_page(
    envelopes: &mut Vec<Vec<u8>>,
    partial: &mut Vec<u8>,
    cursor: &mut FrozenProvenanceCursor,
    cutoff: u64,
    page: FrozenProvenancePage,
) -> anyhow::Result<()> {
    let end = FrozenProvenanceCursor {
        seq: cutoff,
        offset: 0,
    };
    if page.next <= *cursor || page.next > end {
        return Err(anyhow!("invalid frozen journal page continuation"));
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
            return Err(anyhow!("invalid frozen journal fragment cursor"));
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
        return Err(anyhow!("incomplete frozen journal page"));
    }
    *cursor = page.next;
    Ok(())
}

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to create database-owner runtime")
}

/// Start the repository owner or reconnect when another process won election.
pub(crate) fn start_or_reconnect(repository: &Path) -> anyhow::Result<OwnerResponse> {
    if let Ok(response) = request(repository, OwnerRequest::Ping) {
        return Ok(response);
    }

    let executable = std::env::current_exe().context("failed to locate atomic executable")?;
    let mut command = ProcessCommand::new(executable);
    command
        .arg("agent")
        .arg("database-owner")
        .arg("serve")
        .arg("--repository")
        .arg(repository)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_detached(&mut command)?;
    let mut child = command
        .spawn()
        .context("failed to spawn repository database owner")?;

    for _ in 0..START_ATTEMPTS {
        if let Ok(response) = request(repository, OwnerRequest::Ping) {
            return Ok(response);
        }
        if let Some(status) = child
            .try_wait()
            .context("failed to inspect database-owner bootstrap")?
        {
            // A concurrent bootstrap may have won election. Give its endpoint
            // the remainder of the retry window before reporting our exit.
            if !status.success() {
                for _ in 0..START_ATTEMPTS {
                    if let Ok(response) = request(repository, OwnerRequest::Ping) {
                        return Ok(response);
                    }
                    std::thread::sleep(START_RETRY_DELAY);
                }
                return Err(anyhow!("database owner exited during bootstrap: {status}"));
            }
        }
        std::thread::sleep(START_RETRY_DELAY);
    }

    Err(anyhow!(
        "database owner did not become healthy before timeout"
    ))
}

fn request_with_reconnect(
    repository: &Path,
    request_body: OwnerRequest,
) -> anyhow::Result<OwnerResponse> {
    start_or_reconnect(repository)?;
    let mut last_error = None;
    for _ in 0..3 {
        match request(repository, request_body.clone()) {
            Ok(response) => return Ok(response),
            Err(error) => {
                last_error = Some(error);
                start_or_reconnect(repository)?;
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("database owner request retry exhausted")))
}

fn run_outside_async_runtime<T, F>(operation: F) -> anyhow::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
{
    std::thread::spawn(operation)
        .join()
        .map_err(|_| anyhow!("database owner client thread panicked"))?
}

/// Send one request to an already-running owner.
fn request(repository: &Path, request_body: OwnerRequest) -> anyhow::Result<OwnerResponse> {
    let dot_dir = Repository::canonical_dot_dir(repository)?;
    let endpoint = endpoint_name(&dot_dir);
    let frame = RequestFrame {
        version: PROTOCOL_VERSION,
        request_id: Uuid::new_v4().to_string(),
        request: request_body,
    };
    let response = runtime()?.block_on(exchange(&endpoint, &frame))?;
    if response.version != PROTOCOL_VERSION {
        return Err(anyhow!(
            "database owner protocol mismatch: client {}, server {}",
            PROTOCOL_VERSION,
            response.version
        ));
    }
    if response.request_id != frame.request_id {
        return Err(anyhow!(
            "database owner response request id mismatch: sent {}, received {}",
            frame.request_id,
            response.request_id
        ));
    }
    match response.response {
        OwnerResponse::Error { code, message } => {
            Err(anyhow!("database owner request failed [{code}]: {message}"))
        }
        response => Ok(response),
    }
}

fn serve(repository: &Path) -> CliResult<()> {
    let dot_dir = Repository::canonical_dot_dir(repository)?;
    let owner_lock = acquire_owner_lock(&dot_dir)?;
    let store_path = Repository::canonical_change_store_path(repository)?;
    let store = Arc::new(
        RedbChangeStore::open(&store_path)
            .with_context(|| format!("failed to open {}", store_path.display()))?,
    );
    let endpoint = endpoint_name(&dot_dir);
    runtime()?.block_on(run_server(&endpoint, store))?;
    FileExt::unlock(&owner_lock).context("failed to release database-owner lock")?;
    Ok(())
}

fn acquire_owner_lock(dot_dir: &Path) -> anyhow::Result<File> {
    let path = dot_dir.join(OWNER_LOCK_FILE);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("failed to open owner lock {}", path.display()))?;
    file.try_lock_exclusive().map_err(|error| {
        anyhow!(
            "another database owner already holds {}: {error}",
            path.display()
        )
    })?;
    Ok(file)
}

fn owner_failpoint(name: &str) {
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

fn handle_request(store: &RedbChangeStore, frame: RequestFrame) -> (ResponseFrame, bool) {
    let request_id = frame.request_id;
    if frame.version != PROTOCOL_VERSION {
        return (
            ResponseFrame {
                version: PROTOCOL_VERSION,
                request_id,
                response: OwnerResponse::Error {
                    code: "unsupported-version".to_string(),
                    message: format!(
                        "client requested protocol {}, owner supports {}",
                        frame.version, PROTOCOL_VERSION
                    ),
                },
            },
            false,
        );
    }

    let (response, shutdown) = match frame.request {
        OwnerRequest::Ping => (
            OwnerResponse::Pong {
                pid: std::process::id(),
                frozen_envelope_paging: true,
            },
            false,
        ),
        OwnerRequest::ReserveProvenanceTurn {
            session_id,
            turn_number,
            now,
        } => match store.reserve_provenance_turn(&session_id, turn_number, now) {
            Ok(turn) => (
                OwnerResponse::ProvenanceTurn {
                    turn,
                    // reserve_provenance_turn returns only after txn.commit().
                    committed: true,
                },
                false,
            ),
            Err(error) => (
                OwnerResponse::Error {
                    code: "provenance-store".to_string(),
                    message: error.to_string(),
                },
                false,
            ),
        },
        OwnerRequest::AppendProvenanceEnvelopes {
            provenance_id,
            expected_generation,
            envelopes,
            now,
        } => {
            owner_failpoint("before-envelope-commit");
            let batch: Vec<_> = envelopes
                .iter()
                .map(|envelope| (envelope.event_id.as_str(), envelope.bytes.as_slice()))
                .collect();
            match store.append_provenance_envelopes(
                ProvenanceId::new(provenance_id),
                expected_generation,
                &batch,
                now,
            ) {
                Err(error) => (
                    OwnerResponse::Error {
                        code: "provenance-store".to_string(),
                        message: error.to_string(),
                    },
                    false,
                ),
                Ok(stored) => {
                    owner_failpoint("after-envelope-commit");
                    let acknowledgements = stored
                        .into_iter()
                        .map(|event| JournalAppendAck {
                            event_id: event.event_id,
                            sequence: event.seq,
                        })
                        .collect();
                    (
                        OwnerResponse::ProvenanceEnvelopesCommitted { acknowledgements },
                        false,
                    )
                }
            }
        }
        OwnerRequest::PrepareCheckpoint {
            provenance_id,
            expected_generation,
            mut source,
            reuse_frozen_changes,
            now,
        } => match (|| {
            let id = ProvenanceId::new(provenance_id);
            // A previous Stop may have recorded the change before its journal
            // read failed. The retry sees a clean working copy. Recover only
            // those frozen hashes, then retain the store's source validation;
            // a new nonempty change set must still conflict.
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
        })() {
            Ok(attempt) => {
                owner_failpoint("after-checkpoint-prepare");
                (OwnerResponse::CheckpointPrepared { attempt }, false)
            }
            Err(error) => (
                OwnerResponse::Error {
                    code: "provenance-checkpoint".to_string(),
                    message: error.to_string(),
                },
                false,
            ),
        },
        OwnerRequest::LoadFrozenEnvelopes { provenance_id } => {
            match store.load_frozen_provenance_envelopes(ProvenanceId::new(provenance_id)) {
                Ok(stored) => (
                    OwnerResponse::FrozenEnvelopes {
                        envelopes: stored.into_iter().map(|entry| entry.envelope).collect(),
                    },
                    false,
                ),
                Err(error) => (
                    OwnerResponse::Error {
                        code: "provenance-checkpoint".to_string(),
                        message: error.to_string(),
                    },
                    false,
                ),
            }
        }
        OwnerRequest::LoadFrozenEnvelopesPage {
            provenance_id,
            attempt_generation,
            frozen_event_count,
            cursor,
        } => {
            if cursor != FrozenProvenanceCursor::default() {
                owner_failpoint("before-frozen-page-continuation");
            }
            // Include the actual escaped request ID in the budget. Reserving the
            // worst-case metadata and four bytes per payload byte bounds the
            // encoded frame, not merely the event count or raw journal size.
            let empty = ResponseFrame {
                version: PROTOCOL_VERSION,
                request_id: request_id.clone(),
                response: OwnerResponse::FrozenEnvelopesPage {
                    page: FrozenProvenancePage {
                        fragments: Vec::new(),
                        next: FrozenProvenanceCursor::default(),
                    },
                },
            };
            let overhead = serde_json::to_vec(&empty)
                .expect("serializable page frame")
                .len();
            let budget = (MAX_FRAME_BYTES.saturating_sub(overhead + FROZEN_PAGE_METADATA_BYTES)
                / 4)
            .min(FROZEN_PAGE_BYTES);
            match store.load_frozen_provenance_page(
                ProvenanceId::new(provenance_id),
                attempt_generation,
                frozen_event_count,
                cursor,
                budget,
            ) {
                Ok(_)
                    if std::env::var("ATOMIC_OWNER_FAILPOINT").ok().as_deref()
                        == Some("frozen-page-unavailable") =>
                {
                    (
                        OwnerResponse::Error {
                            code: "provenance-checkpoint".to_string(),
                            message: "injected frozen page read failure".to_string(),
                        },
                        false,
                    )
                }
                Ok(page) => (OwnerResponse::FrozenEnvelopesPage { page }, false),
                Err(error) => (
                    OwnerResponse::Error {
                        code: "provenance-checkpoint".to_string(),
                        message: error.to_string(),
                    },
                    false,
                ),
            }
        }
        OwnerRequest::BindCheckpointHash {
            provenance_id,
            expected_generation,
            hash,
            session_turn,
            now,
        } => match store.bind_provenance_checkpoint_hash(
            ProvenanceId::new(provenance_id),
            expected_generation,
            hash,
            session_turn,
            now,
        ) {
            Ok(attempt) => {
                owner_failpoint("after-checkpoint-bind");
                (OwnerResponse::CheckpointPrepared { attempt }, false)
            }
            Err(error) => (
                OwnerResponse::Error {
                    code: "provenance-checkpoint".to_string(),
                    message: error.to_string(),
                },
                false,
            ),
        },
        OwnerRequest::AcknowledgeCheckpoint {
            provenance_id,
            expected_generation,
            manifest_hash,
            completed_at,
        } => match store.acknowledge_provenance_checkpoint(
            ProvenanceId::new(provenance_id),
            expected_generation,
            manifest_hash,
            completed_at,
        ) {
            Ok(_) => (OwnerResponse::CheckpointAcknowledged, false),
            Err(error) => (
                OwnerResponse::Error {
                    code: "provenance-checkpoint".to_string(),
                    message: error.to_string(),
                },
                false,
            ),
        },
        OwnerRequest::StopTurn {
            session_id,
            turn_number,
            cause,
            resumable,
            observed_at,
        } => match store.get_provenance_turn_for(&session_id, turn_number) {
            Ok(None) => (OwnerResponse::TurnLifecycle { turn: None }, false),
            Ok(Some(turn)) => match store.stop_provenance_turn(
                turn.provenance_id,
                turn.generation,
                StopState {
                    cause,
                    observed_at,
                    last_event_seq: None,
                    resumable,
                },
            ) {
                Ok(turn) => (OwnerResponse::TurnLifecycle { turn: Some(turn) }, false),
                Err(error) => (
                    OwnerResponse::Error {
                        code: "provenance-lifecycle".to_string(),
                        message: error.to_string(),
                    },
                    false,
                ),
            },
            Err(error) => (
                OwnerResponse::Error {
                    code: "provenance-lifecycle".to_string(),
                    message: error.to_string(),
                },
                false,
            ),
        },
        OwnerRequest::ResumeTurn {
            session_id,
            turn_number,
            now,
        } => match store.get_provenance_turn_for(&session_id, turn_number) {
            Ok(None) => (OwnerResponse::TurnLifecycle { turn: None }, false),
            Ok(Some(turn)) => {
                match store.resume_provenance_turn(turn.provenance_id, turn.generation, now) {
                    Ok(turn) => (OwnerResponse::TurnLifecycle { turn: Some(turn) }, false),
                    Err(error) => (
                        OwnerResponse::Error {
                            code: "provenance-lifecycle".to_string(),
                            message: error.to_string(),
                        },
                        false,
                    ),
                }
            }
            Err(error) => (
                OwnerResponse::Error {
                    code: "provenance-lifecycle".to_string(),
                    message: error.to_string(),
                },
                false,
            ),
        },
        OwnerRequest::AbandonTurn {
            session_id,
            turn_number,
            observed_at,
        } => match store.get_provenance_turn_for(&session_id, turn_number) {
            Ok(None) => (OwnerResponse::TurnLifecycle { turn: None }, false),
            Ok(Some(turn)) => match store.abandon_provenance_turn(
                turn.provenance_id,
                turn.generation,
                StopState {
                    cause: StopCause::Abandoned,
                    observed_at,
                    last_event_seq: None,
                    resumable: false,
                },
            ) {
                Ok(turn) => (OwnerResponse::TurnLifecycle { turn: Some(turn) }, false),
                Err(error) => (
                    OwnerResponse::Error {
                        code: "provenance-lifecycle".to_string(),
                        message: error.to_string(),
                    },
                    false,
                ),
            },
            Err(error) => (
                OwnerResponse::Error {
                    code: "provenance-lifecycle".to_string(),
                    message: error.to_string(),
                },
                false,
            ),
        },
        OwnerRequest::TurnStatus {
            session_id,
            turn_number,
        } => match store.get_provenance_turn_for(&session_id, turn_number) {
            Ok(turn) => (OwnerResponse::TurnLifecycle { turn }, false),
            Err(error) => (
                OwnerResponse::Error {
                    code: "provenance-lifecycle".to_string(),
                    message: error.to_string(),
                },
                false,
            ),
        },
        OwnerRequest::Shutdown => (
            OwnerResponse::ShuttingDown {
                pid: std::process::id(),
            },
            true,
        ),
    };

    (
        ResponseFrame {
            version: PROTOCOL_VERSION,
            request_id,
            response,
        },
        shutdown,
    )
}

async fn handle_connection<S>(
    mut stream: S,
    store: Arc<RedbChangeStore>,
    shutdown: Arc<Notify>,
) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let request: RequestFrame = read_frame(&mut stream).await?;
    let (response, should_shutdown) = handle_request(&store, request);
    write_frame(&mut stream, &response).await?;
    stream.shutdown().await?;
    if should_shutdown {
        // `notify_one` retains a permit if the accept loop is between polls,
        // preventing a shutdown request from being acknowledged but lost.
        shutdown.notify_one();
    }
    Ok(())
}

async fn read_frame<T, S>(stream: &mut S) -> anyhow::Result<T>
where
    T: for<'de> Deserialize<'de>,
    S: AsyncRead + Unpin,
{
    let length = stream.read_u32().await? as usize;
    if length == 0 || length > MAX_FRAME_BYTES {
        return Err(anyhow!("invalid database-owner frame length {length}"));
    }
    let mut payload = vec![0; length];
    stream.read_exact(&mut payload).await?;
    serde_json::from_slice(&payload).context("invalid database-owner JSON frame")
}

async fn write_frame<T, S>(stream: &mut S, value: &T) -> anyhow::Result<()>
where
    T: Serialize,
    S: AsyncWrite + Unpin,
{
    let payload = serde_json::to_vec(value)?;
    if payload.len() > MAX_FRAME_BYTES {
        return Err(anyhow!("database-owner frame exceeds size limit"));
    }
    stream.write_u32(payload.len() as u32).await?;
    stream.write_all(&payload).await?;
    stream.flush().await?;
    Ok(())
}

fn endpoint_name(dot_dir: &Path) -> String {
    let canonical = std::fs::canonicalize(dot_dir).unwrap_or_else(|_| dot_dir.to_path_buf());
    let digest = blake3::hash(canonical.to_string_lossy().as_bytes())
        .to_hex()
        .to_string();
    endpoint_for_digest(&digest[..24])
}

#[cfg(unix)]
fn endpoint_for_digest(digest: &str) -> String {
    // macOS limits Unix-domain socket paths to roughly 104 bytes. Environment
    // temp directories can be much longer, so use the stable short Unix temp
    // root and keep repository identity in the canonical-path digest.
    Path::new("/tmp")
        .join(format!("atomic-owner-{digest}.sock"))
        .to_string_lossy()
        .into_owned()
}

#[cfg(windows)]
fn endpoint_for_digest(digest: &str) -> String {
    format!(r"\\.\pipe\atomic-owner-{digest}")
}

#[cfg(unix)]
async fn exchange(endpoint: &str, frame: &RequestFrame) -> anyhow::Result<ResponseFrame> {
    let mut stream = tokio::net::UnixStream::connect(endpoint)
        .await
        .with_context(|| format!("database owner is not reachable at {endpoint}"))?;
    write_frame(&mut stream, frame).await?;
    read_frame(&mut stream).await
}

#[cfg(windows)]
async fn exchange(endpoint: &str, frame: &RequestFrame) -> anyhow::Result<ResponseFrame> {
    let mut stream = connect_owner_pipe(endpoint, Duration::from_secs(5))
        .await
        .with_context(|| format!("database owner is not reachable at {endpoint}"))?;
    write_frame(&mut stream, frame).await?;
    read_frame(&mut stream).await
}

#[cfg(windows)]
async fn connect_owner_pipe(
    endpoint: &str,
    timeout: Duration,
) -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    use tokio::net::windows::named_pipe::ClientOptions;
    use windows_sys::Win32::Foundation::ERROR_PIPE_BUSY;

    // A live server can temporarily have no listening instance when several
    // hooks connect at once. Retry acquisition before sending any frame; a busy
    // pipe is not evidence that the owner died or needs another bootstrap.
    let started = tokio::time::Instant::now();
    loop {
        match ClientOptions::new().open(endpoint) {
            Err(error)
                if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)
                    && started.elapsed() < timeout =>
            {
                tokio::time::sleep(
                    START_RETRY_DELAY.min(timeout.saturating_sub(started.elapsed())),
                )
                .await;
            }
            result => return result,
        }
    }
}

#[cfg(unix)]
async fn run_server(endpoint: &str, store: Arc<RedbChangeStore>) -> anyhow::Result<()> {
    use tokio::net::UnixListener;

    let path = Path::new(endpoint);
    if path.exists() {
        std::fs::remove_file(path)
            .with_context(|| format!("failed to remove stale endpoint {endpoint}"))?;
    }
    let listener = UnixListener::bind(path)
        .with_context(|| format!("failed to bind database owner at {endpoint}"))?;
    let shutdown = Arc::new(Notify::new());

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let store = Arc::clone(&store);
                let shutdown = Arc::clone(&shutdown);
                tokio::spawn(async move {
                    if let Err(error) = handle_connection(stream, store, shutdown).await {
                        log::warn!("database-owner connection failed: {error}");
                    }
                });
            }
            () = shutdown.notified() => break,
        }
    }

    drop(listener);
    if path.exists() {
        std::fs::remove_file(path)
            .with_context(|| format!("failed to remove endpoint {endpoint}"))?;
    }
    Ok(())
}

#[cfg(windows)]
async fn run_server(endpoint: &str, store: Arc<RedbChangeStore>) -> anyhow::Result<()> {
    use tokio::net::windows::named_pipe::ServerOptions;

    let shutdown = Arc::new(Notify::new());
    let mut server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(endpoint)
        .with_context(|| format!("failed to create database owner pipe {endpoint}"))?;
    loop {
        tokio::select! {
            connected = server.connect() => {
                connected?;
                // Keep an instance available before a fast handler can finish
                // and close the connected pipe. Otherwise clients can observe
                // a missing endpoint between two successful requests.
                let next = ServerOptions::new()
                    .create(endpoint)
                    .with_context(|| format!("failed to create database owner pipe {endpoint}"))?;
                let connected = std::mem::replace(&mut server, next);
                let store = Arc::clone(&store);
                let shutdown = Arc::clone(&shutdown);
                tokio::spawn(async move {
                    if let Err(error) = handle_connection(connected, store, shutdown).await {
                        log::warn!("database-owner connection failed: {error}");
                    }
                });
            }
            () = shutdown.notified() => break,
        }
    }
    Ok(())
}

#[cfg(unix)]
fn configure_detached(_command: &mut ProcessCommand) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(windows)]
fn configure_detached(command: &mut ProcessCommand) -> anyhow::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::Foundation::{
        SetHandleInformation, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
    };

    // Rust's Windows spawn inherits every inheritable handle, even when the
    // child's configured stdio is NUL. A hook's inherited output pipes would
    // therefore stay open in the long-lived owner after the hook exits, making
    // callers wait forever for EOF. Clear inheritance on the original handles;
    // explicit Stdio::inherit() still works by duplicating them during spawn.
    for handle in [
        std::io::stdin().as_raw_handle(),
        std::io::stdout().as_raw_handle(),
        std::io::stderr().as_raw_handle(),
    ] {
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            continue;
        }
        // SAFETY: These are borrowed standard handles. This only clears an
        // inheritance flag; it neither closes nor transfers ownership of them.
        if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } == 0 {
            return Err(std::io::Error::last_os_error())
                .context("failed to prevent database owner from inheriting caller stdio");
        }
    }
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[tokio::test]
    async fn busy_owner_pipe_waits_for_a_new_listening_instance() {
        use tokio::net::windows::named_pipe::{ClientOptions, ServerOptions};
        use windows_sys::Win32::Foundation::ERROR_PIPE_BUSY;

        let endpoint = format!(r"\\.\pipe\atomic-busy-test-{}", Uuid::new_v4());
        let occupied = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&endpoint)
            .unwrap();
        let _held_client = ClientOptions::new().open(&endpoint).unwrap();
        occupied.connect().await.unwrap();
        assert_eq!(
            ClientOptions::new()
                .open(&endpoint)
                .unwrap_err()
                .raw_os_error(),
            Some(ERROR_PIPE_BUSY as i32)
        );

        let mut connecting = Box::pin(connect_owner_pipe(&endpoint, Duration::from_secs(2)));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut connecting)
                .await
                .is_err()
        );
        let next = ServerOptions::new().create(&endpoint).unwrap();
        let _client = tokio::time::timeout(Duration::from_secs(2), connecting)
            .await
            .expect("client should recover when a listener is available")
            .unwrap();
        next.connect().await.unwrap();
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn busy_owner_pipe_wait_has_a_bounded_timeout() {
        use tokio::net::windows::named_pipe::{ClientOptions, ServerOptions};
        use windows_sys::Win32::Foundation::ERROR_PIPE_BUSY;

        let endpoint = format!(r"\\.\pipe\atomic-busy-timeout-{}", Uuid::new_v4());
        let occupied = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&endpoint)
            .unwrap();
        let _held_client = ClientOptions::new().open(&endpoint).unwrap();
        occupied.connect().await.unwrap();
        let started = tokio::time::Instant::now();
        let error = tokio::time::timeout(
            Duration::from_secs(2),
            connect_owner_pipe(&endpoint, Duration::from_millis(60)),
        )
        .await
        .expect("busy pipe wait must not hang")
        .unwrap_err();
        assert!(started.elapsed() >= Duration::from_millis(60));
        assert_eq!(error.raw_os_error(), Some(ERROR_PIPE_BUSY as i32));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn missing_owner_pipe_does_not_use_the_busy_retry_budget() {
        let endpoint = format!(r"\\.\pipe\atomic-missing-test-{}", Uuid::new_v4());
        let error = tokio::time::timeout(
            Duration::from_secs(2),
            connect_owner_pipe(&endpoint, Duration::from_secs(5)),
        )
        .await
        .expect("a missing owner must return promptly for bootstrap")
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn frozen_pages_bound_encoded_frames_and_reassemble_large_envelopes() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbChangeStore::open(dir.path().join("journal.redb")).unwrap();
        let turn = store.reserve_provenance_turn("paged", 1, 1).unwrap();
        // Exercise both the fragment-count limit and worst-case JSON expansion.
        let mut expected = vec![Vec::new(); 257];
        expected.push(vec![255; 2 * FROZEN_PAGE_BYTES + 17]);
        for (index, bytes) in expected.iter().enumerate() {
            store
                .append_provenance_envelope(
                    turn.provenance_id,
                    turn.generation,
                    &index.to_string(),
                    bytes,
                    2,
                )
                .unwrap();
        }
        let attempt = store
            .prepare_provenance_checkpoint(
                turn.provenance_id,
                turn.generation,
                ProvenanceCheckpointSource {
                    agent_name: "codex".into(),
                    agent_display_name: "Codex".into(),
                    agent_vendor: "openai".into(),
                    change_hashes: vec![],
                    previous_provenance: None,
                    plan_id: None,
                    ledger_turn_number: 0,
                },
                3,
            )
            .unwrap();
        assert!(
            serde_json::to_vec(&OwnerResponse::FrozenEnvelopes {
                envelopes: expected.clone()
            })
            .unwrap()
            .len()
                > MAX_FRAME_BYTES
        );
        for request_id in ["page".to_string(), "\\\"".repeat(1536 * 1024)] {
            let mut cursor = FrozenProvenanceCursor::default();
            let mut actual = Vec::new();
            let mut partial = Vec::new();
            let mut pages = 0;
            while cursor.seq < attempt.frozen_event_count {
                let request = RequestFrame {
                    version: PROTOCOL_VERSION,
                    request_id: request_id.clone(),
                    request: OwnerRequest::LoadFrozenEnvelopesPage {
                        provenance_id: turn.provenance_id.get(),
                        attempt_generation: attempt.attempt_generation,
                        frozen_event_count: attempt.frozen_event_count,
                        cursor,
                    },
                };
                // The escaped ID must itself fit in an inbound request.
                assert!(serde_json::to_vec(&request).unwrap().len() < MAX_FRAME_BYTES);
                let (frame, shutdown) = handle_request(&store, request);
                assert!(!shutdown);
                assert!(serde_json::to_vec(&frame).unwrap().len() <= MAX_FRAME_BYTES);
                let OwnerResponse::FrozenEnvelopesPage { page } = frame.response else {
                    panic!("expected page")
                };
                append_frozen_page(
                    &mut actual,
                    &mut partial,
                    &mut cursor,
                    attempt.frozen_event_count,
                    page,
                )
                .unwrap();
                pages += 1;
            }
            assert!(pages >= 4);
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn legacy_owner_requires_an_explicit_restart_for_pagination() {
        let old: OwnerResponse = serde_json::from_str(r#"{"Pong":{"pid":42}}"#).unwrap();
        assert!(require_frozen_paging(old)
            .unwrap_err()
            .to_string()
            .contains("database-owner shutdown"));
        assert!(require_frozen_paging(OwnerResponse::Pong {
            pid: 42,
            frozen_envelope_paging: true
        })
        .is_ok());
    }

    #[test]
    fn checkpoint_read_retry_preserves_source_validation() {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbChangeStore::open(dir.path().join("journal.redb")).unwrap();
        let turn = store.reserve_provenance_turn("retry", 1, 1).unwrap();
        let original = ProvenanceCheckpointSource {
            agent_name: "codex".into(),
            agent_display_name: "Codex".into(),
            agent_vendor: "openai".into(),
            change_hashes: vec![Hash::of(b"original")],
            previous_provenance: None,
            plan_id: None,
            ledger_turn_number: 0,
        };
        let prepared = store
            .prepare_provenance_checkpoint(turn.provenance_id, turn.generation, original.clone(), 2)
            .unwrap();
        let request = |source, generation, reuse_frozen_changes| {
            handle_request(
                &store,
                RequestFrame {
                    version: PROTOCOL_VERSION,
                    request_id: "retry".into(),
                    request: OwnerRequest::PrepareCheckpoint {
                        provenance_id: turn.provenance_id.get(),
                        expected_generation: generation,
                        source,
                        reuse_frozen_changes,
                        now: 3,
                    },
                },
            )
            .0
            .response
        };
        let mut retry = original.clone();
        retry.change_hashes.clear();
        let OwnerResponse::CheckpointPrepared { attempt } =
            request(retry.clone(), prepared.attempt_generation, true)
        else {
            panic!("expected recovery of prepared source")
        };
        assert_eq!(attempt, prepared);
        assert!(matches!(
            request(retry.clone(), turn.generation, true),
            OwnerResponse::Error { .. }
        ));
        assert!(matches!(
            request(retry.clone(), prepared.attempt_generation, false),
            OwnerResponse::Error { .. }
        ));
        retry.agent_name = "another-agent".into();
        assert!(matches!(
            request(retry, prepared.attempt_generation, true),
            OwnerResponse::Error { .. }
        ));
        let mut changed = original;
        changed.change_hashes = vec![Hash::of(b"new change")];
        assert!(matches!(
            request(changed, prepared.attempt_generation, true),
            OwnerResponse::Error { .. }
        ));
    }

    #[tokio::test]
    async fn framing_round_trips_request_id_and_version() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let expected = RequestFrame {
            version: PROTOCOL_VERSION,
            request_id: "request-1".to_string(),
            request: OwnerRequest::Ping,
        };
        let sent = expected.clone();
        let writer = tokio::spawn(async move { write_frame(&mut client, &sent).await.unwrap() });
        let actual: RequestFrame = read_frame(&mut server).await.unwrap();
        writer.await.unwrap();
        assert_eq!(actual.version, expected.version);
        assert_eq!(actual.request_id, expected.request_id);
        assert!(matches!(actual.request, OwnerRequest::Ping));
    }

    #[test]
    fn owner_propagates_stale_generation_fencing() {
        use atomic_repository::redb_change_store::{StopCause, StopState};

        let dir = tempfile::tempdir().unwrap();
        let store = RedbChangeStore::open(dir.path().join("changes.redb")).unwrap();
        let running = store.reserve_provenance_turn("session", 1, 1).unwrap();
        store
            .stop_provenance_turn(
                running.provenance_id,
                running.generation,
                StopState {
                    cause: StopCause::ProcessExited,
                    observed_at: 2,
                    last_event_seq: None,
                    resumable: true,
                },
            )
            .unwrap();
        let frame = RequestFrame {
            version: PROTOCOL_VERSION,
            request_id: "stale-request".to_string(),
            request: OwnerRequest::AppendProvenanceEnvelopes {
                provenance_id: running.provenance_id.get(),
                expected_generation: running.generation,
                envelopes: vec![WireEnvelope {
                    event_id: "event".to_string(),
                    bytes: b"envelope".to_vec(),
                }],
                now: 3,
            },
        };
        let (response, shutdown) = handle_request(&store, frame);
        assert!(!shutdown);
        match response.response {
            OwnerResponse::Error { code, message } => {
                assert_eq!(code, "provenance-store");
                assert!(message.contains("generation is stale"));
            }
            other => panic!("expected fencing error, got {other:?}"),
        }
    }

    #[test]
    fn endpoint_is_stable_and_bounded() {
        let digest = "0123456789abcdef01234567";
        assert_eq!(endpoint_for_digest(digest), endpoint_for_digest(digest));
        assert!(endpoint_for_digest(digest).len() < 100);
    }
}
