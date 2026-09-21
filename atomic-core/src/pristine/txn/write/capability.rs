//! Capability-requirement lease access for the write transaction (CB-13B).
//!
//! Requirement rows live in `PRISTINE_META` under `required-capability/<id>`.
//! Insertions are raise-only (an existing higher requirement is never
//! lowered); removal deletes the row so a rolled-back cutover no longer
//! fences clients.

use redb::ReadableTable;

use crate::pristine::capability::RepositoryCapability;
use crate::pristine::error::PristineResult;
use crate::pristine::tables::PRISTINE_META;
use crate::pristine::traits::capability_txn::{
    capability_metadata_key, CapabilityMutTxnT, CapabilityTxnT,
};

use super::WriteTxn;

impl CapabilityTxnT for WriteTxn<'_> {
    fn required_capability_version(&self, id: &str) -> PristineResult<Option<u32>> {
        let table = self.txn.open_table(PRISTINE_META)?;
        let version = table
            .get(capability_metadata_key(id).as_str())?
            .map(|version| version.value());
        Ok(version)
    }
}

impl CapabilityMutTxnT for WriteTxn<'_> {
    fn put_required_capability(&mut self, capability: RepositoryCapability) -> PristineResult<u32> {
        let mut table = self.txn.open_table(PRISTINE_META)?;
        let key = capability_metadata_key(capability.id());
        let existing = table.get(key.as_str())?.map(|version| version.value());
        let durable = match existing {
            Some(existing) if existing >= capability.minimum_version() => existing,
            _ => {
                table.insert(key.as_str(), capability.minimum_version())?;
                capability.minimum_version()
            }
        };
        Ok(durable)
    }

    fn put_required_capability_exact(&mut self, id: &str, version: u32) -> PristineResult<u32> {
        let mut table = self.txn.open_table(PRISTINE_META)?;
        table.insert(capability_metadata_key(id).as_str(), version)?;
        Ok(version)
    }

    fn del_required_capability(&mut self, id: &str) -> PristineResult<bool> {
        let mut table = self.txn.open_table(PRISTINE_META)?;
        let key = capability_metadata_key(id);
        let removed = table.remove(key.as_str())?;
        Ok(removed.is_some())
    }
}
