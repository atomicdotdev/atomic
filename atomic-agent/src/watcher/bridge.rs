//! `atomic-bridge` watcher state bracketing for bridge transactions.
//!
//! RFC §11.2 rules 1 and 5: bridge transactions enter the named Watchman state
//! `atomic-bridge` for their duration so IDE plugins and the watcher itself can
//! ignore bridge-originated Git writes, and the existing `FileWatcher` trait is
//! reused for the bracket. Entering and leaving the state is failure-safe: a
//! body that fails still leaves the state, and changes made inside the span
//! are self-suppressed — they are discarded instead of being reported as turn
//! changes, so the bridge's own projection writes never feed back into a
//! reconciliation loop.
//!
//! Watching remains an optional accelerator, never the guarantee (§11.2 rule
//! 7): a failed enter or leave is logged and never blocks or fails the
//! bridge transaction itself.

use std::future::Future;
use std::path::Path;

use crate::error::AgentResult;
use crate::watcher::{create_watcher, FileWatcher, WatcherConfig};

/// The Watchman state name every bridge transaction enters for its duration.
pub const ATOMIC_BRIDGE_STATE: &str = "atomic-bridge";

/// The session name other Watchman subscribers see while a bridge transaction
/// runs; it carries the journaled operation ID so events attributable to the
/// in-flight operation are ignorable.
pub fn bridge_state_session(operation_id: &str) -> String {
    format!("{ATOMIC_BRIDGE_STATE}:{operation_id}")
}

/// Bracket one bridge transaction with `atomic-bridge` enter/leave.
///
/// The watcher enters the `atomic-bridge` state before `body` runs. On
/// success or failure the state is left exactly once, and because the bracket
/// closes with a cancel (not a change query), every filesystem write performed
/// inside the span is discarded rather than surfaced — self-event suppression
/// without a reconciliation loop.
///
/// If the enter call itself fails, `body` still runs: watching is optional and
/// must never weaken safety. If the body fails, the state is still released.
pub async fn with_bridge_state<T, E>(
    watcher: &mut dyn FileWatcher,
    operation_id: &str,
    body: impl Future<Output = Result<T, E>>,
) -> Result<T, E> {
    if let Err(error) = watcher
        .begin_turn(&bridge_state_session(operation_id))
        .await
    {
        log::warn!(
            "atomic-bridge state-enter failed for operation {operation_id}: {error}; \
             continuing without the optional accelerator"
        );
    }

    let outcome = body.await;

    // Failure cleanup and success exit both leave the state. Cancel instead of
    // end_turn: bridge-originated writes inside the span are suppressed rather
    // than reported as turn changes.
    if watcher.is_active() {
        if let Err(error) = watcher.cancel_turn().await {
            log::warn!("atomic-bridge state-leave failed for operation {operation_id}: {error}");
        }
    }
    outcome
}

/// Synchronous bracket for CLI bridge commands.
///
/// Creates the best available watcher, enters `atomic-bridge` with the
/// journaled operation ID, runs `body`, and leaves the state on success and
/// failure alike. Writes performed inside the span are suppressed, so the
/// bridge's own Git writes never trigger a self-reconciliation loop.
pub fn bracket_bridge_transaction<T, E>(
    config: WatcherConfig,
    operation_id: &str,
    body: impl FnOnce() -> Result<T, E>,
) -> Result<T, E> {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            log::warn!(
                "could not create the atomic-bridge watcher runtime: {error}; \
                 continuing without the optional accelerator"
            );
            return body();
        }
    };
    runtime.block_on(async {
        let mut watcher = match create_watcher(config).await {
            Ok(watcher) => watcher,
            Err(error) => {
                log::warn!(
                    "could not create the file watcher for atomic-bridge: {error}; \
                     continuing without the optional accelerator"
                );
                return body();
            }
        };
        with_bridge_state(&mut *watcher, operation_id, async { body() }).await
    })
}

/// Whether the given repo root can open a session-scoped watcher.
pub fn watcher_config(repo_root: &Path) -> WatcherConfig {
    WatcherConfig::new(repo_root)
}

/// AgentResult re-export used by tests asserting the agent error contract.
pub type BridgeStateResult<T> = AgentResult<T>;
