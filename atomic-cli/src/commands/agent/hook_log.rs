//! Hook diagnostics use the same local/reactor service seam as provenance.

use std::path::Path;

use atomic_agent::hook_health::HookHealth;
pub use atomic_agent::hook_log::HookContext;
use atomic_agent::hook_log::SessionLogEvent;
use libatomic::atomic as pb;

use crate::error::CliResult;
use crate::service::Service;

pub fn append(root: &Path, event: &SessionLogEvent) -> Result<(), String> {
    if let Err(error) = append_service(root, event) {
        log::debug!("hook log service unavailable, spooling event: {error}");
        atomic_agent::hook_log::spool(root, event)?;
    }
    Ok(())
}

fn append_service(root: &Path, event: &SessionLogEvent) -> Result<(), String> {
    let service = Service::open_root(root)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "not an Atomic repository".to_string())?;
    service
        .append_hook_log(pb::AppendHookLogRequest {
            repository: Some(service.reference.clone()),
            meta: crate::commands::rpc::request_meta(),
            event: Some(libatomic::daemon::services_provenance::hook_log_proto(
                event.clone(),
            )),
        })
        .map_err(|error| error.to_string())?;
    Ok(())
}

pub fn record_result<T, E: std::fmt::Display>(
    root: &Path,
    context: &HookContext,
    agent: &str,
    display: &str,
    verb: &str,
    result: Result<T, E>,
) -> Result<T, E> {
    atomic_agent::hook_log::record_result(context, agent, display, verb, result, |event| {
        append(root, event)
    })
}

pub fn read(root: &Path) -> CliResult<Option<HookHealth>> {
    let Some(service) = Service::open_root(root)? else {
        return Ok(None);
    };
    let mut health: Option<HookHealth> = None;
    let mut after_sequence = 0;
    loop {
        let page = service.load_hook_log(pb::LoadHookLogRequest {
            repository: Some(service.reference.clone()),
            meta: crate::commands::rpc::request_meta(),
            session_id: None,
            after_sequence,
            limit: 512,
        })?;
        if page.entries.is_empty() {
            break;
        }
        for entry in page.entries {
            if entry.sequence <= after_sequence {
                return Err(crate::error::CliError::Internal(anyhow::anyhow!(
                    "hook log cursor did not advance"
                )));
            }
            after_sequence = entry.sequence;
            if let Some(event) = entry.event {
                let event = libatomic::daemon::services_provenance::hook_log_domain(event);
                health
                    .get_or_insert_with(|| HookHealth::empty(&event.at))
                    .record_event(&event);
            }
        }
    }
    for (_, event) in atomic_agent::hook_log::pending(root)
        .map_err(|error| crate::error::CliError::Internal(anyhow::anyhow!(error)))?
    {
        health
            .get_or_insert_with(|| HookHealth::empty(&event.at))
            .record_event(&event);
    }
    Ok(health)
}

pub fn pending_health(root: &Path) -> Option<HookHealth> {
    let events = atomic_agent::hook_log::pending(root).ok()?;
    HookHealth::from_events(events.into_iter().map(|(_, event)| event).collect())
}
