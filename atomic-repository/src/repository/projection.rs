use super::*;

use atomic_core::pristine::EffectiveProjectionClosure;

fn map_pristine_error(error: atomic_core::pristine::PristineError) -> RepositoryError {
    RepositoryError::Database(error.to_string())
}

/// Build the one canonical visibility closure for a view projection.
///
/// The closure includes the full parent-chain membership and every transitive
/// dependency, is deterministic and dependency-first, and fails closed on
/// missing, incomplete, or cyclic dependency metadata. Graph traversal,
/// attributes, semantic state, SetId, export, and bindings must all derive
/// visibility from this value.
pub fn effective_projection_closure<T: ViewTxnT>(
    txn: &T,
    view: &atomic_core::pristine::ViewState,
) -> Result<EffectiveProjectionClosure, RepositoryError> {
    let membership = view_membership(txn, view)?;
    // Strict: status must fail closed on an incomplete dependency index
    // (dev's fail-closed invariant). Derived-index realignment uses the
    // lenient constructor explicitly where legacy tolerance is required.
    EffectiveProjectionClosure::try_from_membership(txn, &membership).map_err(|e| {
        eprintln!(
            "DBG-STRICT-CLOSURE {:?}",
            std::backtrace::Backtrace::force_capture()
        );
        map_pristine_error(e)
    })
}
