//! Resumable pending provenance journal stored alongside redb-native changes.

use atomic_core::change::session::{
    encode_session_event_key, encode_session_turn_key, session_turn_namespace, SessionEvent,
    SessionTurn,
};
use atomic_core::pristine::tables;
use atomic_core::types::Hash;
use redb::{ReadableDatabase, ReadableTable};
use serde::{Deserialize, Serialize};

use super::{RedbChangeStore, RedbStoreError, RedbStoreResult};

const SCHEMA_VERSION: u64 = 1;
const SCHEMA_VERSION_KEY: &str = "schema_version";
const NEXT_ID_KEY: &str = "next_provenance_id";

/// Database-local identity for a pending provenance turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProvenanceId(u64);

impl ProvenanceId {
    /// Construct an ID returned by a trusted store.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Return the database-local numeric value.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Why a pending turn stopped before completion.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopCause {
    UserRequested,
    LeaseExpired,
    ProcessExited,
    HookFailure,
    SystemShutdown,
    Abandoned,
}

/// Recoverable checkpoint for a stopped turn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopState {
    pub cause: StopCause,
    pub observed_at: i64,
    pub last_event_seq: Option<u64>,
    pub resumable: bool,
}

/// Current state of a reserved provenance turn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProvenanceTurnState {
    Running,
    Stopped(StopState),
    Checkpointing,
    Completed,
    Abandoned(StopState),
}

/// Mutable metadata for one pending or completed provenance turn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredProvenanceTurn {
    pub schema_version: u8,
    pub provenance_id: ProvenanceId,
    pub session_id: String,
    pub turn_number: u32,
    pub state: ProvenanceTurnState,
    pub generation: u64,
    pub next_event_seq: u64,
    pub created_at: i64,
    pub updated_at: i64,
    pub completed_at: Option<i64>,
    pub final_hash: Option<Hash>,
    #[serde(default)]
    pub checkpoint_attempt: Option<ProvenanceCheckpointAttempt>,
}

/// Immutable source inputs captured when a turn checkpoint is prepared.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvenanceCheckpointSource {
    pub agent_name: String,
    pub agent_display_name: String,
    pub agent_vendor: String,
    pub change_hashes: Vec<Hash>,
    pub previous_provenance: Option<Hash>,
    pub plan_id: Option<String>,
    pub ledger_turn_number: u32,
}

/// Durable phase of one checkpoint attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProvenanceCheckpointPhase {
    Prepared,
    HashBound,
    Published,
}

/// Persisted recovery record for a frozen turn finalization.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvenanceCheckpointAttempt {
    pub attempt_generation: u64,
    /// Exclusive journal sequence cutoff frozen by the prepare transaction.
    pub frozen_event_count: u64,
    pub source: ProvenanceCheckpointSource,
    pub phase: ProvenanceCheckpointPhase,
    pub provenance_hash: Option<Hash>,
    pub session_turn: Option<SessionTurn>,
    pub manifest_hash: Option<Hash>,
    pub prepared_at: i64,
    pub updated_at: i64,
}

impl StoredProvenanceTurn {
    fn to_bytes(&self) -> RedbStoreResult<Vec<u8>> {
        postcard::to_allocvec(self)
            .map_err(|error| RedbStoreError::Serialization(error.to_string()))
    }

    fn from_bytes(bytes: &[u8]) -> RedbStoreResult<Self> {
        match postcard::from_bytes(bytes) {
            Ok(turn) => Ok(turn),
            Err(_) => postcard::from_bytes::<StoredProvenanceTurnV1>(bytes)
                .map(Into::into)
                .map_err(|error| RedbStoreError::Serialization(error.to_string())),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredProvenanceTurnV1 {
    schema_version: u8,
    provenance_id: ProvenanceId,
    session_id: String,
    turn_number: u32,
    state: ProvenanceTurnState,
    generation: u64,
    next_event_seq: u64,
    created_at: i64,
    updated_at: i64,
    completed_at: Option<i64>,
    final_hash: Option<Hash>,
}

impl From<StoredProvenanceTurnV1> for StoredProvenanceTurn {
    fn from(value: StoredProvenanceTurnV1) -> Self {
        Self {
            schema_version: value.schema_version,
            provenance_id: value.provenance_id,
            session_id: value.session_id,
            turn_number: value.turn_number,
            state: value.state,
            generation: value.generation,
            next_event_seq: value.next_event_seq,
            created_at: value.created_at,
            updated_at: value.updated_at,
            completed_at: value.completed_at,
            final_hash: value.final_hash,
            checkpoint_attempt: None,
        }
    }
}

/// Immutable source inputs captured when a turn checkpoint is prepared.
/// An immutable journal entry with a caller-stable idempotency key.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredProvenanceEvent {
    pub event_id: String,
    pub seq: u64,
    pub event: SessionEvent,
}

/// Opaque lossless envelope bytes committed by the repository owner service.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredProvenanceEnvelope {
    pub event_id: String,
    pub seq: u64,
    pub envelope: Vec<u8>,
}

/// Position within a frozen journal. Offsets allow even one large envelope to
/// cross transport pages without changing its persisted bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FrozenProvenanceCursor {
    pub seq: u64,
    pub offset: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrozenProvenanceFragment {
    pub cursor: FrozenProvenanceCursor,
    pub bytes: Vec<u8>,
    pub complete: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrozenProvenancePage {
    pub fragments: Vec<FrozenProvenanceFragment>,
    /// The exclusive freeze cutoff with offset zero denotes the end.
    pub next: FrozenProvenanceCursor,
}

/// Bound metadata as well as payloads, including journals of empty envelopes.
pub const MAX_FROZEN_PAGE_FRAGMENTS: usize = 256;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
enum StoredJournalRecord {
    Legacy(StoredProvenanceEvent),
    Envelope(StoredProvenanceEnvelope),
}

impl StoredJournalRecord {
    fn to_bytes(&self) -> RedbStoreResult<Vec<u8>> {
        postcard::to_allocvec(self)
            .map_err(|error| RedbStoreError::Serialization(error.to_string()))
    }

    fn from_bytes(bytes: &[u8]) -> RedbStoreResult<Self> {
        match postcard::from_bytes(bytes) {
            Ok(record) => Ok(record),
            Err(_) => postcard::from_bytes(bytes)
                .map(StoredJournalRecord::Legacy)
                .map_err(|error| RedbStoreError::Serialization(error.to_string())),
        }
    }
}

fn turn_key(session_id: &str, turn_number: u32) -> [u8; 40] {
    encode_session_turn_key(session_turn_namespace(session_id), turn_number)
}

fn event_id_key(id: ProvenanceId, event_id: &str) -> [u8; 32] {
    let mut input = Vec::with_capacity(8 + event_id.len());
    input.extend_from_slice(&id.get().to_be_bytes());
    input.extend_from_slice(event_id.as_bytes());
    *Hash::of(&input).as_bytes()
}

fn load_turn(
    table: &redb::Table<u64, &[u8]>,
    id: ProvenanceId,
) -> RedbStoreResult<StoredProvenanceTurn> {
    let value = table
        .get(id.get())?
        .ok_or(RedbStoreError::ProvenanceTurnNotFound { id: id.get() })?;
    StoredProvenanceTurn::from_bytes(value.value())
}

fn checkpoint_source_matches_except_ordinal(
    left: &ProvenanceCheckpointSource,
    right: &ProvenanceCheckpointSource,
) -> bool {
    left.agent_name == right.agent_name
        && left.agent_display_name == right.agent_display_name
        && left.agent_vendor == right.agent_vendor
        && left.change_hashes == right.change_hashes
        && left.previous_provenance == right.previous_provenance
        && left.plan_id == right.plan_id
}

fn ensure_generation(turn: &StoredProvenanceTurn, expected: u64) -> RedbStoreResult<()> {
    if turn.generation != expected {
        return Err(RedbStoreError::ProvenanceFenced {
            id: turn.provenance_id.get(),
            expected,
            actual: turn.generation,
        });
    }
    Ok(())
}

impl RedbChangeStore {
    pub(crate) fn initialize_provenance_tables(
        txn: &redb::WriteTransaction,
    ) -> RedbStoreResult<()> {
        let mut meta = txn.open_table(tables::PROVENANCE_STORE_META)?;
        let _ = txn.open_table(tables::PROVENANCE_TURN_INDEX)?;
        let _ = txn.open_table(tables::PROVENANCE_TURNS)?;
        let _ = txn.open_table(tables::PROVENANCE_JOURNAL_EVENTS)?;
        let _ = txn.open_table(tables::PROVENANCE_EVENT_INDEX)?;
        let _ = txn.open_table(tables::PROVENANCE_FINAL_HASHES)?;

        let stored_version = meta.get(SCHEMA_VERSION_KEY)?.map(|value| value.value());
        match stored_version {
            None => {
                meta.insert(NEXT_ID_KEY, 1)?;
                meta.insert(SCHEMA_VERSION_KEY, SCHEMA_VERSION)?;
            }
            Some(SCHEMA_VERSION) if meta.get(NEXT_ID_KEY)?.is_some() => {}
            Some(SCHEMA_VERSION) => {
                return Err(RedbStoreError::Corrupt(
                    "provenance store is missing its next-id counter".to_string(),
                ));
            }
            Some(version) => return Err(RedbStoreError::UnsupportedProvenanceSchema(version)),
        }
        Ok(())
    }

    /// Reserve or retrieve the stable local identity for a session turn.
    pub fn reserve_provenance_turn(
        &self,
        session_id: &str,
        turn_number: u32,
        now: i64,
    ) -> RedbStoreResult<StoredProvenanceTurn> {
        let txn = self.db.begin_write()?;
        let key = turn_key(session_id, turn_number);
        let mut index = txn.open_table(tables::PROVENANCE_TURN_INDEX)?;
        let mut turns = txn.open_table(tables::PROVENANCE_TURNS)?;

        if let Some(existing) = index.get(&key)? {
            let turn = load_turn(&turns, ProvenanceId::new(existing.value()))?;
            if turn.session_id != session_id || turn.turn_number != turn_number {
                return Err(RedbStoreError::Corrupt(
                    "session-turn index does not match its provenance row".to_string(),
                ));
            }
            return Ok(turn);
        }

        let mut meta = txn.open_table(tables::PROVENANCE_STORE_META)?;
        let id = meta
            .get(NEXT_ID_KEY)?
            .ok_or_else(|| RedbStoreError::Corrupt("missing provenance id counter".to_string()))?
            .value();
        let next = id
            .checked_add(1)
            .ok_or(RedbStoreError::ProvenanceIdExhausted)?;
        meta.insert(NEXT_ID_KEY, next)?;

        let turn = StoredProvenanceTurn {
            schema_version: SCHEMA_VERSION as u8,
            provenance_id: ProvenanceId::new(id),
            session_id: session_id.to_string(),
            turn_number,
            state: ProvenanceTurnState::Running,
            generation: 1,
            next_event_seq: 0,
            created_at: now,
            updated_at: now,
            completed_at: None,
            final_hash: None,
            checkpoint_attempt: None,
        };
        let bytes = turn.to_bytes()?;
        turns.insert(id, bytes.as_slice())?;
        index.insert(&key, id)?;
        drop(meta);
        drop(turns);
        drop(index);
        txn.commit()?;
        Ok(turn)
    }

    /// Return a reserved provenance turn by local ID.
    pub fn get_provenance_turn(
        &self,
        id: ProvenanceId,
    ) -> RedbStoreResult<Option<StoredProvenanceTurn>> {
        let txn = self.db.begin_read()?;
        let turns = txn.open_table(tables::PROVENANCE_TURNS)?;
        turns
            .get(id.get())?
            .map(|value| StoredProvenanceTurn::from_bytes(value.value()))
            .transpose()
    }

    /// Return a reserved turn by external session identity and turn number.
    pub fn get_provenance_turn_for(
        &self,
        session_id: &str,
        turn_number: u32,
    ) -> RedbStoreResult<Option<StoredProvenanceTurn>> {
        let txn = self.db.begin_read()?;
        let index = txn.open_table(tables::PROVENANCE_TURN_INDEX)?;
        let turns = txn.open_table(tables::PROVENANCE_TURNS)?;
        let key = turn_key(session_id, turn_number);
        let Some(id) = index.get(&key)?.map(|value| value.value()) else {
            return Ok(None);
        };
        let value = turns
            .get(id)?
            .ok_or(RedbStoreError::ProvenanceTurnNotFound { id })?;
        let turn = StoredProvenanceTurn::from_bytes(value.value())?;
        if turn.session_id != session_id || turn.turn_number != turn_number {
            return Err(RedbStoreError::Corrupt(
                "session-turn index does not match its provenance row".to_string(),
            ));
        }
        Ok(Some(turn))
    }

    /// Return the completed turn bound to a final provenance hash.
    pub fn get_provenance_turn_by_hash(
        &self,
        hash: &Hash,
    ) -> RedbStoreResult<Option<StoredProvenanceTurn>> {
        let txn = self.db.begin_read()?;
        let hashes = txn.open_table(tables::PROVENANCE_FINAL_HASHES)?;
        let turns = txn.open_table(tables::PROVENANCE_TURNS)?;
        let Some(id) = hashes.get(hash.as_bytes())?.map(|value| value.value()) else {
            return Ok(None);
        };
        let value = turns
            .get(id)?
            .ok_or(RedbStoreError::ProvenanceTurnNotFound { id })?;
        Ok(Some(StoredProvenanceTurn::from_bytes(value.value())?))
    }

    /// Append one immutable event, assigning its sequence transactionally.
    pub fn append_provenance_event(
        &self,
        id: ProvenanceId,
        expected_generation: u64,
        event_id: &str,
        mut event: SessionEvent,
        now: i64,
    ) -> RedbStoreResult<StoredProvenanceEvent> {
        let txn = self.db.begin_write()?;
        let mut turns = txn.open_table(tables::PROVENANCE_TURNS)?;
        let mut events = txn.open_table(tables::PROVENANCE_JOURNAL_EVENTS)?;
        let mut event_index = txn.open_table(tables::PROVENANCE_EVENT_INDEX)?;
        let id_key = event_id_key(id, event_id);

        if let Some(existing_key) = event_index.get(&id_key)? {
            let existing = events.get(existing_key.value())?.ok_or_else(|| {
                RedbStoreError::Corrupt("event index points to no event".to_string())
            })?;
            let StoredJournalRecord::Legacy(stored) =
                StoredJournalRecord::from_bytes(existing.value())?
            else {
                return Err(RedbStoreError::ProvenanceEventConflict {
                    id: id.get(),
                    event_id: event_id.to_string(),
                });
            };
            event.seq = stored.seq;
            if stored.event_id != event_id || stored.event != event {
                return Err(RedbStoreError::ProvenanceEventConflict {
                    id: id.get(),
                    event_id: event_id.to_string(),
                });
            }
            return Ok(stored);
        }

        let mut turn = load_turn(&turns, id)?;
        ensure_generation(&turn, expected_generation)?;
        if !matches!(turn.state, ProvenanceTurnState::Running) {
            return Err(RedbStoreError::InvalidProvenanceState {
                id: id.get(),
                expected: "running",
            });
        }

        let seq = turn.next_event_seq;
        event.seq = seq;
        let stored = StoredProvenanceEvent {
            event_id: event_id.to_string(),
            seq,
            event,
        };
        let key = encode_session_event_key(id.get(), seq);
        let event_bytes = StoredJournalRecord::Legacy(stored.clone()).to_bytes()?;
        events.insert(&key, event_bytes.as_slice())?;
        event_index.insert(&id_key, &key)?;

        turn.next_event_seq = seq
            .checked_add(1)
            .ok_or(RedbStoreError::ProvenanceEventSequenceExhausted { id: id.get() })?;
        turn.updated_at = now;
        let turn_bytes = turn.to_bytes()?;
        turns.insert(id.get(), turn_bytes.as_slice())?;
        drop(event_index);
        drop(events);
        drop(turns);
        txn.commit()?;
        Ok(stored)
    }

    /// Append one serialized lossless envelope with generation fencing.
    ///
    /// The store treats envelope bytes as opaque so `atomic-repository` does
    /// not depend on the agent schema crate. A successful return occurs only
    /// after the event and updated turn sequence are durably committed.
    pub fn append_provenance_envelope(
        &self,
        id: ProvenanceId,
        expected_generation: u64,
        event_id: &str,
        envelope: &[u8],
        now: i64,
    ) -> RedbStoreResult<StoredProvenanceEnvelope> {
        let mut stored = self.append_provenance_envelopes(
            id,
            expected_generation,
            &[(event_id, envelope)],
            now,
        )?;
        Ok(stored.remove(0))
    }

    /// Append a batch in one durable transaction, with acknowledgments in input order.
    ///
    /// All new events and the sequence frontier commit together, or none do on error.
    /// Duplicate IDs return the original stored bytes and sequence, including duplicates
    /// earlier in this batch. As with single-event retries, existing IDs can be
    /// acknowledged after the turn is fenced; any new event must pass the fence.
    /// Empty and entirely duplicate batches perform no durable commit.
    pub fn append_provenance_envelopes(
        &self,
        id: ProvenanceId,
        expected_generation: u64,
        envelopes: &[(&str, &[u8])],
        now: i64,
    ) -> RedbStoreResult<Vec<StoredProvenanceEnvelope>> {
        if envelopes.is_empty() {
            return Ok(Vec::new());
        }
        let txn = self.db.begin_write()?;
        let mut turns = txn.open_table(tables::PROVENANCE_TURNS)?;
        let mut events = txn.open_table(tables::PROVENANCE_JOURNAL_EVENTS)?;
        let mut event_index = txn.open_table(tables::PROVENANCE_EVENT_INDEX)?;
        let mut turn: Option<StoredProvenanceTurn> = None;
        let mut acknowledgements = Vec::with_capacity(envelopes.len());

        for &(event_id, envelope) in envelopes {
            let id_key = event_id_key(id, event_id);
            if let Some(existing_key) = event_index.get(&id_key)? {
                let existing = events.get(existing_key.value())?.ok_or_else(|| {
                    RedbStoreError::Corrupt("event index points to no event".to_string())
                })?;
                let StoredJournalRecord::Envelope(stored) =
                    StoredJournalRecord::from_bytes(existing.value())?
                else {
                    return Err(RedbStoreError::ProvenanceEventConflict {
                        id: id.get(),
                        event_id: event_id.to_string(),
                    });
                };
                if stored.event_id != event_id {
                    return Err(RedbStoreError::ProvenanceEventConflict {
                        id: id.get(),
                        event_id: event_id.to_string(),
                    });
                }
                // Event ID is the idempotency contract; retry observation times may differ.
                acknowledgements.push(stored);
                continue;
            }

            if turn.is_none() {
                let loaded = load_turn(&turns, id)?;
                ensure_generation(&loaded, expected_generation)?;
                if !matches!(loaded.state, ProvenanceTurnState::Running) {
                    return Err(RedbStoreError::InvalidProvenanceState {
                        id: id.get(),
                        expected: "running",
                    });
                }
                turn = Some(loaded);
            }
            let turn = turn.as_mut().expect("new envelope loaded the turn");
            let seq = turn.next_event_seq;
            turn.next_event_seq = seq
                .checked_add(1)
                .ok_or(RedbStoreError::ProvenanceEventSequenceExhausted { id: id.get() })?;
            let stored = StoredProvenanceEnvelope {
                event_id: event_id.to_string(),
                seq,
                envelope: envelope.to_vec(),
            };
            let key = encode_session_event_key(id.get(), seq);
            let event_bytes = StoredJournalRecord::Envelope(stored.clone()).to_bytes()?;
            events.insert(&key, event_bytes.as_slice())?;
            event_index.insert(&id_key, &key)?;
            acknowledgements.push(stored);
        }

        if let Some(mut turn) = turn {
            turn.updated_at = now;
            let turn_bytes = turn.to_bytes()?;
            turns.insert(id.get(), turn_bytes.as_slice())?;
            drop(event_index);
            drop(events);
            drop(turns);
            txn.commit()?;
        }
        Ok(acknowledgements)
    }

    /// Load serialized lossless envelope entries in committed sequence order.
    pub fn load_provenance_envelopes(
        &self,
        id: ProvenanceId,
    ) -> RedbStoreResult<Vec<StoredProvenanceEnvelope>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(tables::PROVENANCE_JOURNAL_EVENTS)?;
        let start = encode_session_event_key(id.get(), 0);
        let end = encode_session_event_key(id.get(), u64::MAX);
        let mut result = Vec::new();
        for entry in table.range::<&[u8; 16]>(&start..=&end)? {
            let (_, value) = entry?;
            if let StoredJournalRecord::Envelope(stored) =
                StoredJournalRecord::from_bytes(value.value())?
            {
                result.push(stored);
            }
        }
        Ok(result)
    }

    /// Load all legacy session events for a turn in sequence order.
    pub fn load_provenance_events(
        &self,
        id: ProvenanceId,
    ) -> RedbStoreResult<Vec<StoredProvenanceEvent>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(tables::PROVENANCE_JOURNAL_EVENTS)?;
        let start = encode_session_event_key(id.get(), 0);
        let end = encode_session_event_key(id.get(), u64::MAX);
        let mut result = Vec::new();
        for entry in table.range::<&[u8; 16]>(&start..=&end)? {
            let (_, value) = entry?;
            if let StoredJournalRecord::Legacy(stored) =
                StoredJournalRecord::from_bytes(value.value())?
            {
                result.push(stored);
            }
        }
        Ok(result)
    }

    /// Freeze the current journal frontier and persist one recoverable attempt.
    pub fn prepare_provenance_checkpoint(
        &self,
        id: ProvenanceId,
        expected_generation: u64,
        source: ProvenanceCheckpointSource,
        now: i64,
    ) -> RedbStoreResult<ProvenanceCheckpointAttempt> {
        let txn = self.db.begin_write()?;
        let mut turns = txn.open_table(tables::PROVENANCE_TURNS)?;
        let mut turn = load_turn(&turns, id)?;

        if let Some(mut attempt) = turn.checkpoint_attempt.clone() {
            if attempt.source == source {
                return Ok(attempt);
            }
            if attempt.phase != ProvenanceCheckpointPhase::Published
                && checkpoint_source_matches_except_ordinal(&attempt.source, &source)
            {
                // Pre-cutover AgentSession counters may include turns that were
                // never indexed in SESSION_TURNS. The immutable ledger supplies
                // the authoritative append ordinal; graph/hash identity is
                // unchanged, so a bound attempt can be repaired safely.
                attempt.source.ledger_turn_number = source.ledger_turn_number;
                if let Some(session_turn) = attempt.session_turn.as_mut() {
                    session_turn.turn_number = source.ledger_turn_number;
                }
                attempt.updated_at = now;
                turn.checkpoint_attempt = Some(attempt.clone());
                let bytes = turn.to_bytes()?;
                turns.insert(id.get(), bytes.as_slice())?;
                drop(turns);
                txn.commit()?;
                return Ok(attempt);
            }
            return Err(RedbStoreError::ProvenanceCheckpointConflict { id: id.get() });
        }

        ensure_generation(&turn, expected_generation)?;
        if !matches!(turn.state, ProvenanceTurnState::Running) {
            return Err(RedbStoreError::InvalidProvenanceState {
                id: id.get(),
                expected: "running",
            });
        }
        turn.generation = turn
            .generation
            .checked_add(1)
            .ok_or(RedbStoreError::ProvenanceGenerationExhausted { id: id.get() })?;
        turn.state = ProvenanceTurnState::Checkpointing;
        turn.updated_at = now;
        let attempt = ProvenanceCheckpointAttempt {
            attempt_generation: turn.generation,
            frozen_event_count: turn.next_event_seq,
            source,
            phase: ProvenanceCheckpointPhase::Prepared,
            provenance_hash: None,
            session_turn: None,
            manifest_hash: None,
            prepared_at: now,
            updated_at: now,
        };
        turn.checkpoint_attempt = Some(attempt.clone());
        let bytes = turn.to_bytes()?;
        turns.insert(id.get(), bytes.as_slice())?;
        drop(turns);
        txn.commit()?;
        Ok(attempt)
    }

    /// Load only envelopes below the persisted exclusive freeze cutoff.
    pub fn load_frozen_provenance_envelopes(
        &self,
        id: ProvenanceId,
    ) -> RedbStoreResult<Vec<StoredProvenanceEnvelope>> {
        let txn = self.db.begin_read()?;
        let turns = txn.open_table(tables::PROVENANCE_TURNS)?;
        let turn = turns
            .get(id.get())?
            .ok_or(RedbStoreError::ProvenanceTurnNotFound { id: id.get() })?;
        let turn = StoredProvenanceTurn::from_bytes(turn.value())?;
        let attempt = turn
            .checkpoint_attempt
            .ok_or(RedbStoreError::InvalidProvenanceState {
                id: id.get(),
                expected: "checkpoint prepared",
            })?;
        if attempt.frozen_event_count == 0 {
            return Ok(Vec::new());
        }

        let table = txn.open_table(tables::PROVENANCE_JOURNAL_EVENTS)?;
        let start = encode_session_event_key(id.get(), 0);
        let end = encode_session_event_key(id.get(), attempt.frozen_event_count - 1);
        let mut result = Vec::new();
        for entry in table.range::<&[u8; 16]>(&start..=&end)? {
            let (_, value) = entry?;
            if let StoredJournalRecord::Envelope(stored) =
                StoredJournalRecord::from_bytes(value.value())?
            {
                result.push(stored);
            }
        }
        Ok(result)
    }

    /// Read a bounded part of the persisted checkpoint. Every page validates the
    /// same attempt generation and cutoff in its own read transaction, so a
    /// retry or owner restart needs no process-local cursor state.
    pub fn load_frozen_provenance_page(
        &self,
        id: ProvenanceId,
        attempt_generation: u64,
        frozen_event_count: u64,
        cursor: FrozenProvenanceCursor,
        max_bytes: usize,
    ) -> RedbStoreResult<FrozenProvenancePage> {
        let invalid_cursor = || RedbStoreError::InvalidProvenanceCursor { id: id.get() };
        if max_bytes == 0 || cursor.seq > frozen_event_count {
            return Err(invalid_cursor());
        }
        let txn = self.db.begin_read()?;
        let turns = txn.open_table(tables::PROVENANCE_TURNS)?;
        let turn = turns
            .get(id.get())?
            .ok_or(RedbStoreError::ProvenanceTurnNotFound { id: id.get() })?;
        let turn = StoredProvenanceTurn::from_bytes(turn.value())?;
        let attempt = turn
            .checkpoint_attempt
            .ok_or(RedbStoreError::InvalidProvenanceState {
                id: id.get(),
                expected: "checkpoint prepared",
            })?;
        if attempt.attempt_generation != attempt_generation {
            return Err(RedbStoreError::ProvenanceFenced {
                id: id.get(),
                expected: attempt_generation,
                actual: attempt.attempt_generation,
            });
        }
        if attempt.frozen_event_count != frozen_event_count {
            return Err(RedbStoreError::ProvenanceCheckpointConflict { id: id.get() });
        }
        let end_cursor = FrozenProvenanceCursor {
            seq: frozen_event_count,
            offset: 0,
        };
        if cursor.seq == frozen_event_count {
            if cursor.offset != 0 {
                return Err(invalid_cursor());
            }
            return Ok(FrozenProvenancePage {
                fragments: Vec::new(),
                next: end_cursor,
            });
        }
        let table = txn.open_table(tables::PROVENANCE_JOURNAL_EVENTS)?;
        let start = encode_session_event_key(id.get(), cursor.seq);
        let end = encode_session_event_key(id.get(), frozen_event_count - 1);
        let mut fragments = Vec::new();
        let mut remaining = max_bytes;
        for entry in table.range::<&[u8; 16]>(&start..=&end)? {
            let (_, value) = entry?;
            let record = StoredJournalRecord::from_bytes(value.value())?;
            let stored = match record {
                StoredJournalRecord::Envelope(stored) => stored,
                StoredJournalRecord::Legacy(stored) => {
                    if stored.seq == cursor.seq && cursor.offset != 0 {
                        return Err(invalid_cursor());
                    }
                    continue;
                }
            };
            if fragments.is_empty() && cursor.offset != 0 && stored.seq != cursor.seq {
                return Err(invalid_cursor());
            }
            let offset = if stored.seq == cursor.seq {
                cursor.offset
            } else {
                0
            };
            if offset > stored.envelope.len() || (offset != 0 && offset == stored.envelope.len()) {
                return Err(invalid_cursor());
            }
            let length = remaining.min(stored.envelope.len() - offset);
            let complete = offset + length == stored.envelope.len();
            let next = if complete {
                FrozenProvenanceCursor {
                    seq: stored.seq + 1,
                    offset: 0,
                }
            } else {
                FrozenProvenanceCursor {
                    seq: stored.seq,
                    offset: offset + length,
                }
            };
            fragments.push(FrozenProvenanceFragment {
                cursor: FrozenProvenanceCursor {
                    seq: stored.seq,
                    offset,
                },
                bytes: stored.envelope[offset..offset + length].to_vec(),
                complete,
            });
            remaining -= length;
            if remaining == 0 || fragments.len() == MAX_FROZEN_PAGE_FRAGMENTS {
                return Ok(FrozenProvenancePage { fragments, next });
            }
        }
        if fragments.is_empty() && cursor.offset != 0 {
            return Err(invalid_cursor());
        }
        Ok(FrozenProvenancePage {
            fragments,
            next: end_cursor,
        })
    }

    /// Bind the prepared graph hash and exact immutable session turn once.
    pub fn bind_provenance_checkpoint_hash(
        &self,
        id: ProvenanceId,
        expected_generation: u64,
        hash: Hash,
        session_turn: SessionTurn,
        now: i64,
    ) -> RedbStoreResult<ProvenanceCheckpointAttempt> {
        let txn = self.db.begin_write()?;
        let mut turns = txn.open_table(tables::PROVENANCE_TURNS)?;
        let mut hashes = txn.open_table(tables::PROVENANCE_FINAL_HASHES)?;
        let mut turn = load_turn(&turns, id)?;
        let mut attempt =
            turn.checkpoint_attempt
                .clone()
                .ok_or(RedbStoreError::InvalidProvenanceState {
                    id: id.get(),
                    expected: "checkpoint prepared",
                })?;

        if attempt.provenance_hash == Some(hash)
            && attempt.session_turn.as_ref() == Some(&session_turn)
        {
            return Ok(attempt);
        }
        if attempt.provenance_hash.is_some() || attempt.session_turn.is_some() {
            return Err(RedbStoreError::ProvenanceCheckpointConflict { id: id.get() });
        }
        ensure_generation(&turn, expected_generation)?;
        if attempt.attempt_generation != expected_generation
            || !matches!(turn.state, ProvenanceTurnState::Checkpointing)
            || session_turn.provenance_hash != hash
            || session_turn.session_id != turn.session_id
            || session_turn.turn_number != attempt.source.ledger_turn_number
        {
            return Err(RedbStoreError::ProvenanceCheckpointConflict { id: id.get() });
        }
        if let Some(existing) = hashes.get(hash.as_bytes())? {
            if existing.value() != id.get() {
                return Err(RedbStoreError::ProvenanceFinalHashAlreadyBound {
                    existing_id: existing.value(),
                });
            }
        }

        hashes.insert(hash.as_bytes(), id.get())?;
        attempt.phase = ProvenanceCheckpointPhase::HashBound;
        attempt.provenance_hash = Some(hash);
        attempt.session_turn = Some(session_turn);
        attempt.updated_at = now;
        turn.final_hash = Some(hash);
        turn.updated_at = now;
        turn.checkpoint_attempt = Some(attempt.clone());
        let bytes = turn.to_bytes()?;
        turns.insert(id.get(), bytes.as_slice())?;
        drop(hashes);
        drop(turns);
        txn.commit()?;
        Ok(attempt)
    }

    /// Acknowledge atomic SESSION_TURNS/head publication and complete the turn.
    pub fn acknowledge_provenance_checkpoint(
        &self,
        id: ProvenanceId,
        expected_generation: u64,
        manifest_hash: Hash,
        completed_at: i64,
    ) -> RedbStoreResult<StoredProvenanceTurn> {
        let txn = self.db.begin_write()?;
        let mut turns = txn.open_table(tables::PROVENANCE_TURNS)?;
        let mut turn = load_turn(&turns, id)?;
        let mut attempt =
            turn.checkpoint_attempt
                .clone()
                .ok_or(RedbStoreError::InvalidProvenanceState {
                    id: id.get(),
                    expected: "checkpoint hash bound",
                })?;

        if matches!(turn.state, ProvenanceTurnState::Completed)
            && attempt.manifest_hash == Some(manifest_hash)
        {
            return Ok(turn);
        }
        ensure_generation(&turn, expected_generation)?;
        if attempt.attempt_generation != expected_generation
            || attempt.phase != ProvenanceCheckpointPhase::HashBound
            || attempt.provenance_hash.is_none()
            || attempt.session_turn.is_none()
        {
            return Err(RedbStoreError::ProvenanceCheckpointNotBound { id: id.get() });
        }
        if attempt.manifest_hash.is_some() {
            return Err(RedbStoreError::ProvenanceCheckpointPublicationConflict { id: id.get() });
        }

        attempt.phase = ProvenanceCheckpointPhase::Published;
        attempt.manifest_hash = Some(manifest_hash);
        attempt.updated_at = completed_at;
        turn.state = ProvenanceTurnState::Completed;
        turn.completed_at = Some(completed_at);
        turn.updated_at = completed_at;
        turn.generation = turn
            .generation
            .checked_add(1)
            .ok_or(RedbStoreError::ProvenanceGenerationExhausted { id: id.get() })?;
        turn.checkpoint_attempt = Some(attempt);
        let bytes = turn.to_bytes()?;
        turns.insert(id.get(), bytes.as_slice())?;
        drop(turns);
        txn.commit()?;
        Ok(turn)
    }

    /// Stop a running turn while keeping its committed event frontier.
    pub fn stop_provenance_turn(
        &self,
        id: ProvenanceId,
        expected_generation: u64,
        mut stop: StopState,
    ) -> RedbStoreResult<StoredProvenanceTurn> {
        let txn = self.db.begin_write()?;
        let mut turns = txn.open_table(tables::PROVENANCE_TURNS)?;
        let mut turn = load_turn(&turns, id)?;
        if let ProvenanceTurnState::Stopped(existing) = &turn.state {
            if existing.cause == stop.cause && existing.resumable == stop.resumable {
                return Ok(turn);
            }
        }
        if matches!(
            turn.state,
            ProvenanceTurnState::Completed | ProvenanceTurnState::Abandoned(_)
        ) {
            return Ok(turn);
        }
        ensure_generation(&turn, expected_generation)?;
        if !matches!(turn.state, ProvenanceTurnState::Running) {
            return Err(RedbStoreError::InvalidProvenanceState {
                id: id.get(),
                expected: "running",
            });
        }
        stop.last_event_seq = turn.next_event_seq.checked_sub(1);
        turn.generation = turn
            .generation
            .checked_add(1)
            .ok_or(RedbStoreError::ProvenanceGenerationExhausted { id: id.get() })?;
        turn.updated_at = stop.observed_at;
        turn.state = ProvenanceTurnState::Stopped(stop);
        let bytes = turn.to_bytes()?;
        turns.insert(id.get(), bytes.as_slice())?;
        drop(turns);
        txn.commit()?;
        Ok(turn)
    }

    /// Resume a stopped turn with a new fencing generation and the same ID.
    pub fn resume_provenance_turn(
        &self,
        id: ProvenanceId,
        expected_generation: u64,
        now: i64,
    ) -> RedbStoreResult<StoredProvenanceTurn> {
        let txn = self.db.begin_write()?;
        let mut turns = txn.open_table(tables::PROVENANCE_TURNS)?;
        let mut turn = load_turn(&turns, id)?;
        if matches!(turn.state, ProvenanceTurnState::Running) {
            if turn.generation == expected_generation
                || turn.generation == expected_generation.saturating_add(1)
            {
                return Ok(turn);
            }
            ensure_generation(&turn, expected_generation)?;
        }
        ensure_generation(&turn, expected_generation)?;
        match &turn.state {
            ProvenanceTurnState::Stopped(stop) if stop.resumable => {}
            // A Checkpointing turn has a prepared-but-unbound checkpoint.
            // Resuming supersedes the stale attempt; the next Stop re-prepares
            // from the intact journal instead of failing the whole session.
            ProvenanceTurnState::Checkpointing => {}
            ProvenanceTurnState::Stopped(_) | ProvenanceTurnState::Abandoned(_) => {
                return Err(RedbStoreError::InvalidProvenanceTransition { id: id.get() })
            }
            ProvenanceTurnState::Completed => return Ok(turn),
            _ => {
                return Err(RedbStoreError::InvalidProvenanceState {
                    id: id.get(),
                    expected: "resumable stop",
                })
            }
        }
        turn.generation = turn
            .generation
            .checked_add(1)
            .ok_or(RedbStoreError::ProvenanceGenerationExhausted { id: id.get() })?;
        turn.updated_at = now;
        turn.state = ProvenanceTurnState::Running;
        let bytes = turn.to_bytes()?;
        turns.insert(id.get(), bytes.as_slice())?;
        drop(turns);
        txn.commit()?;
        Ok(turn)
    }

    /// Explicitly seal a pending turn without publishing a checkpoint.
    pub fn abandon_provenance_turn(
        &self,
        id: ProvenanceId,
        expected_generation: u64,
        mut stop: StopState,
    ) -> RedbStoreResult<StoredProvenanceTurn> {
        let txn = self.db.begin_write()?;
        let mut turns = txn.open_table(tables::PROVENANCE_TURNS)?;
        let mut turn = load_turn(&turns, id)?;
        if matches!(
            turn.state,
            ProvenanceTurnState::Completed | ProvenanceTurnState::Abandoned(_)
        ) {
            return Ok(turn);
        }
        ensure_generation(&turn, expected_generation)?;
        if !matches!(
            turn.state,
            ProvenanceTurnState::Running | ProvenanceTurnState::Stopped(_)
        ) {
            return Err(RedbStoreError::InvalidProvenanceTransition { id: id.get() });
        }
        stop.cause = StopCause::Abandoned;
        stop.resumable = false;
        stop.last_event_seq = turn.next_event_seq.checked_sub(1);
        turn.generation = turn
            .generation
            .checked_add(1)
            .ok_or(RedbStoreError::ProvenanceGenerationExhausted { id: id.get() })?;
        turn.updated_at = stop.observed_at;
        turn.state = ProvenanceTurnState::Abandoned(stop);
        let bytes = turn.to_bytes()?;
        turns.insert(id.get(), bytes.as_slice())?;
        drop(turns);
        txn.commit()?;
        Ok(turn)
    }

    /// Freeze a stopped turn for deterministic finalization.
    pub fn begin_provenance_checkpoint(
        &self,
        id: ProvenanceId,
        expected_generation: u64,
        now: i64,
    ) -> RedbStoreResult<StoredProvenanceTurn> {
        self.transition_provenance_turn(
            id,
            expected_generation,
            ProvenanceTurnState::Checkpointing,
            now,
        )
    }

    fn transition_provenance_turn(
        &self,
        id: ProvenanceId,
        expected_generation: u64,
        next: ProvenanceTurnState,
        now: i64,
    ) -> RedbStoreResult<StoredProvenanceTurn> {
        let txn = self.db.begin_write()?;
        let mut turns = txn.open_table(tables::PROVENANCE_TURNS)?;
        let mut turn = load_turn(&turns, id)?;
        ensure_generation(&turn, expected_generation)?;

        let valid = matches!(
            (&turn.state, &next),
            (
                ProvenanceTurnState::Running,
                ProvenanceTurnState::Stopped(_)
            ) | (
                ProvenanceTurnState::Stopped(StopState {
                    resumable: true,
                    ..
                }),
                ProvenanceTurnState::Running
            ) | (
                ProvenanceTurnState::Stopped(_),
                ProvenanceTurnState::Checkpointing
            ) | (
                ProvenanceTurnState::Checkpointing,
                ProvenanceTurnState::Stopped(_)
            )
        );
        if !valid {
            return Err(RedbStoreError::InvalidProvenanceTransition { id: id.get() });
        }

        turn.generation = turn
            .generation
            .checked_add(1)
            .ok_or(RedbStoreError::ProvenanceGenerationExhausted { id: id.get() })?;
        turn.updated_at = now;
        turn.state = next;
        let bytes = turn.to_bytes()?;
        turns.insert(id.get(), bytes.as_slice())?;
        drop(turns);
        txn.commit()?;
        Ok(turn)
    }

    /// Atomically complete a checkpoint and bind its content hash once.
    pub fn bind_final_provenance_hash(
        &self,
        id: ProvenanceId,
        expected_generation: u64,
        hash: Hash,
        completed_at: i64,
    ) -> RedbStoreResult<StoredProvenanceTurn> {
        let txn = self.db.begin_write()?;
        let mut turns = txn.open_table(tables::PROVENANCE_TURNS)?;
        let mut hashes = txn.open_table(tables::PROVENANCE_FINAL_HASHES)?;
        let mut turn = load_turn(&turns, id)?;

        if turn.final_hash == Some(hash) && matches!(turn.state, ProvenanceTurnState::Completed) {
            return Ok(turn);
        }
        if turn.final_hash.is_some() {
            return Err(RedbStoreError::ProvenanceFinalHashConflict { id: id.get() });
        }
        ensure_generation(&turn, expected_generation)?;
        if !matches!(turn.state, ProvenanceTurnState::Checkpointing) {
            return Err(RedbStoreError::InvalidProvenanceState {
                id: id.get(),
                expected: "checkpointing",
            });
        }
        if let Some(existing) = hashes.get(hash.as_bytes())? {
            if existing.value() != id.get() {
                return Err(RedbStoreError::ProvenanceFinalHashAlreadyBound {
                    existing_id: existing.value(),
                });
            }
        }

        hashes.insert(hash.as_bytes(), id.get())?;
        turn.state = ProvenanceTurnState::Completed;
        turn.final_hash = Some(hash);
        turn.completed_at = Some(completed_at);
        turn.updated_at = completed_at;
        turn.generation = turn
            .generation
            .checked_add(1)
            .ok_or(RedbStoreError::ProvenanceGenerationExhausted { id: id.get() })?;
        let bytes = turn.to_bytes()?;
        turns.insert(id.get(), bytes.as_slice())?;
        drop(hashes);
        drop(turns);
        txn.commit()?;
        Ok(turn)
    }
}

#[cfg(test)]
mod compatibility_tests {
    use super::*;

    #[test]
    fn checkpoint_field_defaults_when_reading_pre_checkpoint_turn() {
        let legacy = StoredProvenanceTurnV1 {
            schema_version: 1,
            provenance_id: ProvenanceId::new(7),
            session_id: "legacy".to_string(),
            turn_number: 1,
            state: ProvenanceTurnState::Running,
            generation: 1,
            next_event_seq: 2,
            created_at: 3,
            updated_at: 4,
            completed_at: None,
            final_hash: None,
        };
        let bytes = postcard::to_allocvec(&legacy).unwrap();
        let decoded = StoredProvenanceTurn::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.provenance_id, ProvenanceId::new(7));
        assert!(decoded.checkpoint_attempt.is_none());
    }
}
