use super::*;

use crate::pristine::ref_mapping::{RefMappingMutTxnT, RefMappingTxnT};

impl RefMappingTxnT for WriteTxn<'_> {
    fn get_ref_mapping_bytes(&self, view_id: u64) -> PristineResult<Option<Vec<u8>>> {
        let table = self.txn.open_table(REF_MAPPINGS)?;
        let value = table.get(view_id)?.map(|guard| guard.value().to_vec());
        Ok(value)
    }

    fn iter_ref_mapping_bytes(&self) -> PristineResult<Vec<(u64, Vec<u8>)>> {
        let table = self.txn.open_table(REF_MAPPINGS)?;
        let mut rows = Vec::new();
        for row in table.iter()? {
            let (key, value) = row?;
            rows.push((key.value(), value.value().to_vec()));
        }
        Ok(rows)
    }
}

impl RefMappingMutTxnT for WriteTxn<'_> {
    fn put_ref_mapping_bytes(&mut self, view_id: u64, bytes: &[u8]) -> PristineResult<()> {
        let mut table = self.txn.open_table(REF_MAPPINGS)?;
        table.insert(view_id, bytes)?;
        Ok(())
    }

    fn del_ref_mapping(&mut self, view_id: u64) -> PristineResult<bool> {
        let mut table = self.txn.open_table(REF_MAPPINGS)?;
        let removed = table.remove(view_id)?.is_some();
        Ok(removed)
    }
}
