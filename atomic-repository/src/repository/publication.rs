//! Repository-owned publication transactions and deterministic crash probes.

use crate::RepositoryError;

pub(super) fn failpoint(name: &str) -> Result<(), RepositoryError> {
    if std::env::var("ATOMIC_PUBLICATION_FAILPOINT").as_deref() != Ok(name) {
        return Ok(());
    }
    if std::env::var("ATOMIC_PUBLICATION_FAILPOINT_ACTION").as_deref() == Ok("exit") {
        std::process::exit(86);
    }
    Err(RepositoryError::Database(format!(
        "injected publication failure at {name}"
    )))
}
