use super::*;

use atomic_core::pristine::{SetIdIndexEntry, SetIdIndexMutTxnT, SetIdIndexTxnT, ViewState};
use atomic_core::types::SetId;

/// Joint order-sensitive and order-invariant identity of one effective view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewIdentity {
    /// Running, order-sensitive view state.
    pub merkle: Merkle,
    /// SetId V1 over the canonical effective projection closure.
    pub set_id: SetId,
    /// Number of changes in the validated projection closure.
    pub closure_len: u64,
}

impl ViewIdentity {
    fn index_entry(self) -> SetIdIndexEntry {
        SetIdIndexEntry {
            merkle: self.merkle,
            set_id: self.set_id,
            closure_len: self.closure_len,
        }
    }
}

fn map_pristine_error(error: atomic_core::pristine::PristineError) -> RepositoryError {
    RepositoryError::Database(error.to_string())
}

/// Derive SetId V1 from the canonical effective projection closure.
pub fn view_set_id<T: ViewTxnT>(txn: &T, view: &ViewState) -> Result<SetId, RepositoryError> {
    Ok(effective_projection_identity(txn, view)?.set_id)
}

/// Derive both identities from one validated projection closure.
pub fn effective_projection_identity<T: ViewTxnT>(
    txn: &T,
    view: &ViewState,
) -> Result<ViewIdentity, RepositoryError> {
    let closure = effective_projection_closure(txn, view)?;
    let mut set_id = SetId::ZERO;
    for change_id in closure.iter_dependency_first().copied() {
        let hash = txn
            .get_external(change_id)
            .map_err(map_pristine_error)?
            .ok_or_else(|| {
                RepositoryError::Database(format!(
                    "change {} in effective projection has no external hash",
                    change_id.get()
                ))
            })?;
        set_id = set_id.add(&hash);
    }
    Ok(ViewIdentity {
        merkle: view.state,
        set_id,
        closure_len: closure.len() as u64,
    })
}

impl Repository {
    /// Compute the canonical joint identity without trusting the derived index.
    pub fn view_identity(&self, name: &str) -> Result<ViewIdentity, RepositoryError> {
        let txn = self.pristine.read_txn().map_err(map_pristine_error)?;
        let view = txn
            .get_view(name)
            .map_err(map_pristine_error)?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: name.to_string(),
            })?;
        effective_projection_identity(&txn, &view)
    }

    /// Derive the order-invariant identity of the effective projection closure.
    pub fn view_set_id(&self, name: &str) -> Result<SetId, RepositoryError> {
        Ok(self.view_identity(name)?.set_id)
    }

    /// Persist the current canonical identity for one view.
    ///
    /// Existing legacy repositories acquire the additive table when opened
    /// writable. A malformed or newer row is rejected; callers may explicitly
    /// rebuild the derived index with [`Repository::rebuild_set_id_index`].
    pub fn refresh_view_set_id_index(&self, name: &str) -> Result<ViewIdentity, RepositoryError> {
        let mut txn = self.pristine.write_txn().map_err(map_pristine_error)?;
        let view = txn
            .get_view(name)
            .map_err(map_pristine_error)?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: name.to_string(),
            })?;
        let identity = effective_projection_identity(&txn, &view)?;
        if txn.get_set_id_index(view.id).map_err(map_pristine_error)?
            != Some(identity.index_entry())
        {
            txn.put_set_id_index(view.id, identity.index_entry())
                .map_err(map_pristine_error)?;
        }
        txn.commit().map_err(map_pristine_error)?;
        Ok(identity)
    }

    /// Transactionally rebuild every SetId row from canonical view closures.
    pub fn rebuild_set_id_index(&self) -> Result<usize, RepositoryError> {
        let mut txn = self.pristine.write_txn().map_err(map_pristine_error)?;
        let views = txn.snapshot_views().map_err(map_pristine_error)?;
        txn.clear_set_id_index().map_err(map_pristine_error)?;
        for (_, view) in &views {
            let identity = effective_projection_identity(&txn, view)?;
            txn.put_set_id_index(view.id, identity.index_entry())
                .map_err(map_pristine_error)?;
        }
        txn.commit().map_err(map_pristine_error)?;
        Ok(views.len())
    }
}
