//! Lock-wait budgets for hook-invoked database coordination.
//!
//! Every hook invocation is a separate process, so cross-process coordination
//! (the redb writer lock, the turn-publication lock) is waited on with a
//! bounded budget rather than a blocking queue. The previous fixed budgets
//! (10s everywhere) were tuned for an unloaded laptop and routinely expired
//! under legitimate multi-agent contention — eight concurrent Stops each
//! publishing for a few seconds serialized past 10s, and the late waiters
//! surfaced "timed out waiting for another Stop to publish" as a hook
//! failure. Waiting longer is always safe here: the work being waited on is
//! finite and ordered, so a generous budget trades a slower hook for a
//! correct one. The budgets remain overridable so constrained environments
//! (CI runners, sandboxes) can tighten or loosen them without recompiling.

use std::time::Duration;

/// Budget for opening the repository database when another process may hold
/// the writer lock (session starts, ledger reads, checkpoint publication).
///
/// The shared database-open budget — the daemon's opens wait on the same
/// knob. Override with `ATOMIC_DB_LOCK_WAIT_MS`.
pub fn database_wait() -> Duration {
    atomic_repository::database_lock_wait()
}

/// Budget a Stop waits for another session's Stop to finish publishing its
/// ledger under the cross-process publication lock.
///
/// Override with `ATOMIC_TURN_PUBLICATION_TIMEOUT_MS`.
pub fn publication_timeout() -> Duration {
    env_millis("ATOMIC_TURN_PUBLICATION_TIMEOUT_MS", 60_000)
}

fn env_millis(name: &str, default_millis: u64) -> Duration {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .map_or(Duration::from_millis(default_millis), Duration::from_millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_generous() {
        // The whole point of these budgets: multi-agent contention is
        // legitimate, not pathological. See module docs.
        assert_eq!(database_wait(), Duration::from_secs(30));
        assert_eq!(publication_timeout(), Duration::from_secs(60));
    }
}
