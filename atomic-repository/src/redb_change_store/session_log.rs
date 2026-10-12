//! Immutable operational diagnostics alongside the session/provenance ledger.

use atomic_core::pristine::tables::{SESSION_LOG, SESSION_LOG_IDS, SESSION_LOG_INDEX};
use redb::{ReadableDatabase, ReadableTable};
use serde::{Deserialize, Serialize};

use super::{RedbChangeStore, RedbStoreError, RedbStoreResult};

/// A single hook observation. `session_id == None` retains pre-parse failures.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLogEvent {
    pub event_id: String,
    pub session_id: Option<String>,
    pub parent_session_id: Option<String>,
    pub agent: String,
    pub display_name: String,
    pub verb: String,
    /// `ok` or `error`.
    pub outcome: String,
    pub detail: Option<String>,
    /// RFC3339 observation time; retries retain this timestamp and event ID.
    pub at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredSessionLogEvent {
    pub sequence: u64,
    pub event: SessionLogEvent,
}

fn session_key(session: &str, sequence: u64) -> [u8; 40] {
    let mut key = [0; 40];
    key[..32].copy_from_slice(atomic_core::types::Hash::of(session.as_bytes()).as_bytes());
    key[32..].copy_from_slice(&sequence.to_be_bytes());
    key
}

impl RedbChangeStore {
    /// Append and index one immutable event atomically. Identical retries return
    /// the original sequence; reusing an ID for different data is rejected.
    /// No running turn or session ledger record is required.
    pub fn append_session_log(&self, event: &SessionLogEvent) -> RedbStoreResult<u64> {
        if event.event_id.is_empty()
            || event.agent.is_empty()
            || event.verb.is_empty()
            || !matches!(event.outcome.as_str(), "ok" | "error")
            || chrono::DateTime::parse_from_rfc3339(&event.at).is_err()
        {
            return Err(RedbStoreError::Serialization(
                "invalid session log event".into(),
            ));
        }
        let bytes = serde_json::to_vec(event)?;
        let txn = self.db().begin_write()?;
        let sequence;
        {
            let mut events = txn.open_table(SESSION_LOG)?;
            let mut ids = txn.open_table(SESSION_LOG_IDS)?;
            if let Some(existing) = ids.get(event.event_id.as_str())? {
                let sequence = existing.value();
                let stored = events.get(sequence)?.ok_or_else(|| {
                    RedbStoreError::Corrupt("session log index points to missing event".into())
                })?;
                if stored.value() != bytes {
                    return Err(RedbStoreError::Serialization(
                        "session log event ID conflict".into(),
                    ));
                }
                return Ok(sequence);
            }
            sequence = events
                .last()?
                .map_or(Some(1), |(key, _)| key.value().checked_add(1))
                .ok_or_else(|| RedbStoreError::Corrupt("session log sequence exhausted".into()))?;
            events.insert(sequence, bytes.as_slice())?;
            ids.insert(event.event_id.as_str(), sequence)?;
            if let Some(session) = &event.session_id {
                txn.open_table(SESSION_LOG_INDEX)?
                    .insert(&session_key(session, sequence), sequence)?;
            }
        }
        txn.commit()?;
        Ok(sequence)
    }

    /// Read a bounded page in committed order. `after` is exclusive; pass the
    /// last returned sequence for continuation. A session filter uses its index.
    pub fn load_session_log(
        &self,
        session: Option<&str>,
        after: u64,
        limit: usize,
    ) -> RedbStoreResult<Vec<StoredSessionLogEvent>> {
        let Some(start) = after.checked_add(1) else {
            return Ok(Vec::new());
        };
        let txn = self.db().begin_read()?;
        let events = txn.open_table(SESSION_LOG)?;
        let mut out = Vec::new();
        let limit = limit.clamp(1, 512);
        if let Some(session) = session {
            let index = txn.open_table(SESSION_LOG_INDEX)?;
            let lo = session_key(session, start);
            let hi = session_key(session, u64::MAX);
            for row in index.range::<&[u8; 40]>(&lo..=&hi)?.take(limit) {
                let (_, sequence) = row?;
                let sequence = sequence.value();
                let value = events.get(sequence)?.ok_or_else(|| {
                    RedbStoreError::Corrupt("session log index points to missing event".into())
                })?;
                out.push(StoredSessionLogEvent {
                    sequence,
                    event: serde_json::from_slice(value.value())?,
                });
            }
        } else {
            for row in events.range(start..)?.take(limit) {
                let (key, value) = row?;
                out.push(StoredSessionLogEvent {
                    sequence: key.value(),
                    event: serde_json::from_slice(value.value())?,
                });
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn event(id: &str, session: Option<&str>) -> SessionLogEvent {
        SessionLogEvent {
            event_id: id.into(),
            session_id: session.map(str::to_owned),
            parent_session_id: session
                .filter(|session| *session != "parent")
                .map(|_| "parent".into()),
            agent: "opencode".into(),
            display_name: "OpenCode".into(),
            verb: "before-tool".into(),
            outcome: "ok".into(),
            detail: None,
            at: "2026-10-12T10:00:00Z".into(),
        }
    }

    #[test]
    fn concurrent_parent_children_are_lossless_and_session_pages_cross_byte_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(RedbChangeStore::open(dir.path().join("store.redb")).unwrap());
        std::thread::scope(|scope| {
            for worker in 0..8 {
                let store = store.clone();
                scope.spawn(move || {
                    let session = if worker == 0 {
                        "parent".to_string()
                    } else {
                        format!("child-{worker}")
                    };
                    for index in 0..80 {
                        let event = event(&format!("{worker}-{index}"), Some(&session));
                        let sequence = store.append_session_log(&event).unwrap();
                        assert_eq!(store.append_session_log(&event).unwrap(), sequence);
                    }
                });
            }
        });
        let first = store.load_session_log(None, 0, 512).unwrap();
        assert_eq!(first.len(), 512);
        let rest = store
            .load_session_log(None, first.last().unwrap().sequence, 512)
            .unwrap();
        assert_eq!(rest.len(), 128);
        for worker in 0..8 {
            let session = if worker == 0 {
                "parent".to_string()
            } else {
                format!("child-{worker}")
            };
            let page = store.load_session_log(Some(&session), 0, 41).unwrap();
            let next = store
                .load_session_log(Some(&session), page.last().unwrap().sequence, 512)
                .unwrap();
            assert_eq!(page.len() + next.len(), 80);
            assert!(page
                .iter()
                .chain(&next)
                .all(|entry| entry.event.session_id.as_deref() == Some(&session)));
            assert!(page.last().unwrap().sequence < next[0].sequence);
        }
    }

    #[test]
    fn idempotency_rejects_changed_payload_and_unassigned_events_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.redb");
        let store = RedbChangeStore::open(&path).unwrap();
        let mut event = event("parse-error", None);
        event.outcome = "error".into();
        assert_eq!(store.append_session_log(&event).unwrap(), 1);
        event.detail = Some("changed".into());
        assert!(store.append_session_log(&event).is_err());
        drop(store);
        let store = RedbChangeStore::open_existing(path).unwrap();
        let events = store.load_session_log(None, 0, 512).unwrap();
        assert_eq!(events.len(), 1);
        assert!(events[0].event.session_id.is_none());
        assert!(events[0].event.detail.is_none());
    }
}
