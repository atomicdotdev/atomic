//! Session correlation and a tracing layer for durable operational diagnostics.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

pub use atomic_repository::redb_change_store::SessionLogEvent;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::Layer;

/// Correlation survives process boundaries through payload fields or explicit
/// `ATOMIC_SESSION_ID` / `ATOMIC_PARENT_SESSION_ID` environment variables.
#[derive(Clone, Debug, Default)]
pub struct HookContext {
    pub session_id: Option<String>,
    pub parent_session_id: Option<String>,
}

fn usable(value: Option<&str>) -> Option<String> {
    value
        .filter(|value| !value.is_empty() && *value != "unknown")
        .map(str::to_owned)
}

impl HookContext {
    /// Best-effort extraction keeps invalid JSON attributable when the caller
    /// supplies correlation via the environment; otherwise it stays unassigned.
    pub fn from_input(input: &[u8]) -> Self {
        let value = serde_json::from_slice::<serde_json::Value>(input).ok();
        Self::from_json(value.as_ref()).with_environment()
    }

    pub fn from_event(event: &crate::event::TurnEvent) -> Self {
        Self::from_event_payload(event).with_environment()
    }

    /// Service-side extraction must use caller fields, never daemon env vars.
    pub fn from_event_payload(event: &crate::event::TurnEvent) -> Self {
        let mut context = Self::from_json(event.raw_json.as_ref());
        context.session_id = usable(Some(&event.session_id)).or(context.session_id);
        context
    }

    fn with_environment(mut self) -> Self {
        self.session_id = self
            .session_id
            .or_else(|| usable(std::env::var("ATOMIC_SESSION_ID").ok().as_deref()));
        self.parent_session_id = self
            .parent_session_id
            .or_else(|| usable(std::env::var("ATOMIC_PARENT_SESSION_ID").ok().as_deref()));
        self
    }

    fn from_json(value: Option<&serde_json::Value>) -> Self {
        fn field(value: Option<&serde_json::Value>, names: &[&str]) -> Option<String> {
            names
                .iter()
                .find_map(|name| usable(value?.get(name)?.as_str()))
        }
        let nested = value.and_then(|value| value.get("raw_json"));
        Self {
            session_id: field(
                value,
                &["session_id", "sessionID", "thread-id", "conversation_id"],
            ),
            parent_session_id: field(value, &["parent_session_id", "parentSessionID", "parentID"])
                .or_else(|| {
                    field(
                        nested,
                        &["parent_session_id", "parentSessionID", "parentID"],
                    )
                }),
        }
    }
}

#[derive(Default)]
struct Fields(BTreeMap<String, String>);
impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().into(), value.into());
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().into(), format!("{value:?}"));
    }
}

// Capture synchronously, then persist outside the subscriber callback. This
// avoids blocking/re-entering the async service runtime from tracing's on_event.
struct HookLogLayer(Arc<Mutex<Option<SessionLogEvent>>>);
impl<S: Subscriber> Layer<S> for HookLogLayer {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        if event.metadata().target() != "atomic::hook" {
            return;
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        let mut get = |name: &str| fields.0.remove(name).unwrap_or_default();
        let event = SessionLogEvent {
            event_id: get("event_id"),
            session_id: usable(Some(&get("session_id"))),
            parent_session_id: usable(Some(&get("parent_session_id"))),
            agent: get("agent"),
            display_name: get("display_name"),
            verb: get("verb"),
            outcome: get("outcome"),
            detail: Some(get("detail")).filter(|detail| !detail.is_empty()),
            at: get("at"),
        };
        *self.0.lock().unwrap() = Some(event);
    }
}

/// Emit structured tracing fields and synchronously commit the captured event
/// through the caller's service-owned sink. Logging failure preserves dispatch.
///
/// # Examples
///
/// ```
/// use atomic_agent::hook_log::{record_result, HookContext};
/// let context = HookContext {
///     session_id: Some("child-session".into()),
///     parent_session_id: Some("parent-session".into()),
/// };
/// let result = record_result(&context, "opencode", "OpenCode", "receive",
///     Ok::<_, String>(42), |event| {
///         assert_eq!(event.parent_session_id.as_deref(), Some("parent-session"));
///         // Forward this event to the service-owned append operation.
///         Ok(())
///     });
/// assert_eq!(result.unwrap(), 42);
/// ```
pub fn record_result<T, E: std::fmt::Display>(
    context: &HookContext,
    agent: &str,
    display_name: &str,
    verb: &str,
    result: Result<T, E>,
    sink: impl FnOnce(&SessionLogEvent) -> Result<(), String>,
) -> Result<T, E> {
    let captured = Arc::new(Mutex::new(None));
    let subscriber = tracing_subscriber::registry().with(HookLogLayer(captured.clone()));
    let event_id = uuid::Uuid::new_v4().to_string();
    let at = chrono::Utc::now().to_rfc3339();
    let detail = result
        .as_ref()
        .err()
        .map(ToString::to_string)
        .unwrap_or_default();
    let outcome = if result.is_ok() { "ok" } else { "error" };
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: "atomic::hook",
            event_id = event_id.as_str(), at = at.as_str(), agent, display_name, verb, outcome,
            session_id = context.session_id.as_deref().unwrap_or(""),
            parent_session_id = context.parent_session_id.as_deref().unwrap_or(""),
            detail = detail.as_str(), "hook dispatch completed");
    });
    let event = captured.lock().unwrap().take();
    if let Some(event) = event {
        if let Err(error) = sink(&event) {
            log::warn!("failed to persist hook log: {error}");
        }
    }
    result
}

/// Durable transport-outage spool. Each event has its own immutable file, so
/// independent processes never share a read-modify-write critical section.
pub fn spool(root: &std::path::Path, event: &SessionLogEvent) -> Result<(), String> {
    use std::io::Write;
    let id = uuid::Uuid::parse_str(&event.event_id).map_err(|error| error.to_string())?;
    let dir = root.join(".atomic/hook-log-pending");
    std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let temporary = dir.join(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&serde_json::to_vec(event)?)?;
        file.sync_all()?;
        std::fs::rename(&temporary, dir.join(format!("{id}.json")))
    })()
    .map_err(|error: std::io::Error| error.to_string());
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

/// Pending transport failures remain inspectable before a service recovers.
pub fn pending(
    root: &std::path::Path,
) -> Result<Vec<(std::path::PathBuf, SessionLogEvent)>, String> {
    let dir = root.join(".atomic/hook-log-pending");
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.to_string()),
    };
    let mut events = Vec::new();
    for entry in entries {
        let path = entry.map_err(|error| error.to_string())?.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.to_string()),
        };
        events.push((
            path,
            serde_json::from_slice(&bytes).map_err(|error| error.to_string())?,
        ));
    }
    Ok(events)
}

/// Replay pending events idempotently while the service owns the database gate.
pub fn flush_pending(
    root: &std::path::Path,
    store: &atomic_repository::redb_change_store::RedbChangeStore,
) -> Result<(), String> {
    for (path, event) in pending(root)? {
        store
            .append_session_log(&event)
            .map_err(|error| error.to_string())?;
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracing_keeps_concurrent_contexts_and_dispatch_errors_intact() {
        let events = Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for index in 0..32 {
                let events = &events;
                scope.spawn(move || {
                    let context = HookContext {
                        session_id: Some(format!("child-{index}")),
                        parent_session_id: Some("parent".into()),
                    };
                    let result = record_result(
                        &context,
                        "opencode",
                        "OpenCode",
                        "before-tool",
                        Err::<(), _>(format!("failure-{index}")),
                        |event| {
                            events.lock().unwrap().push(event.clone());
                            Ok(())
                        },
                    );
                    assert_eq!(result.unwrap_err(), format!("failure-{index}"));
                });
            }
        });
        let events = events.into_inner().unwrap();
        assert_eq!(events.len(), 32);
        let ids: std::collections::HashSet<_> =
            events.iter().map(|event| &event.event_id).collect();
        assert_eq!(ids.len(), 32);
        for event in events {
            let index = event
                .session_id
                .unwrap()
                .strip_prefix("child-")
                .unwrap()
                .to_string();
            assert_eq!(
                event.detail.as_deref(),
                Some(format!("failure-{index}").as_str())
            );
            assert_eq!(event.parent_session_id.as_deref(), Some("parent"));
        }
    }

    #[test]
    fn a_failed_sink_preserves_the_dispatch_result() {
        let result = record_result(
            &HookContext::default(),
            "opencode",
            "OpenCode",
            "receive",
            Ok::<_, String>(42),
            |_| Err("offline".into()),
        );
        assert_eq!(result.unwrap(), 42);
    }

    #[test]
    fn concurrent_outage_spool_replays_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        std::thread::scope(|scope| {
            for index in 0..24 {
                let root = dir.path();
                scope.spawn(move || {
                    let context = HookContext {
                        session_id: Some(format!("child-{index}")),
                        parent_session_id: Some("parent".into()),
                    };
                    record_result(
                        &context,
                        "opencode",
                        "OpenCode",
                        "receive",
                        Err::<(), _>("transport failed"),
                        |event| spool(root, event),
                    )
                    .unwrap_err();
                });
            }
        });
        let events = pending(dir.path()).unwrap();
        assert_eq!(events.len(), 24);
        let store = atomic_repository::redb_change_store::RedbChangeStore::open(
            dir.path().join("log.redb"),
        )
        .unwrap();
        // Simulate a lost ACK: one pending event was already committed.
        store.append_session_log(&events[0].1).unwrap();
        flush_pending(dir.path(), &store).unwrap();
        assert!(pending(dir.path()).unwrap().is_empty());
        assert_eq!(store.load_session_log(None, 0, 512).unwrap().len(), 24);
    }
}
