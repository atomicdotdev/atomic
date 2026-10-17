use super::*;

use crate::pristine::bindings::binding_conflict;
use crate::pristine::{BindingMutTxnT, BindingStoreOutcome, BindingTxnT};

fn binding_id_hex(id: &[u8; 32]) -> String {
    id.iter().map(|byte| format!("{byte:02x}")).collect()
}

impl BindingTxnT for WriteTxn<'_> {
    fn get_binding_bytes(&self, id: &[u8; 32]) -> PristineResult<Option<Vec<u8>>> {
        let table = self.txn.open_table(BINDINGS)?;
        let value = table.get(id)?.map(|guard| guard.value().to_vec());
        Ok(value)
    }

    fn iter_binding_ids(&self) -> PristineResult<Vec<[u8; 32]>> {
        let table = self.txn.open_table(BINDINGS)?;
        let mut ids = Vec::new();
        for row in table.iter()? {
            let (key, _) = row?;
            ids.push(*key.value());
        }
        Ok(ids)
    }
}

impl BindingMutTxnT for WriteTxn<'_> {
    fn insert_binding_bytes(
        &mut self,
        id: &[u8; 32],
        bytes: &[u8],
    ) -> PristineResult<BindingStoreOutcome> {
        let mut table = self.txn.open_table(BINDINGS)?;
        if let Some(existing) = table.get(id)? {
            if existing.value() == bytes {
                return Ok(BindingStoreOutcome::Idempotent);
            }
            return Err(binding_conflict(&binding_id_hex(id)));
        }
        table.insert(id, bytes)?;
        Ok(BindingStoreOutcome::Stored)
    }
}
