use super::*;

use crate::pristine::{
    decode_set_id_index_entry, encode_set_id_index_entry, SetIdIndexEntry, SetIdIndexMutTxnT,
    SetIdIndexTxnT,
};

impl SetIdIndexTxnT for WriteTxn<'_> {
    fn get_set_id_index(&self, view_id: u64) -> PristineResult<Option<SetIdIndexEntry>> {
        let table = self.txn.open_table(VIEW_SET_ID_INDEX)?;
        let result = match table.get(view_id)? {
            Some(value) => decode_set_id_index_entry(value.value()).map(Some),
            None => Ok(None),
        };
        result
    }
}

impl SetIdIndexMutTxnT for WriteTxn<'_> {
    fn put_set_id_index(&mut self, view_id: u64, entry: SetIdIndexEntry) -> PristineResult<()> {
        let bytes = encode_set_id_index_entry(entry);
        self.txn
            .open_table(VIEW_SET_ID_INDEX)?
            .insert(view_id, bytes.as_slice())?;
        Ok(())
    }

    fn clear_set_id_index(&mut self) -> PristineResult<()> {
        let mut table = self.txn.open_table(VIEW_SET_ID_INDEX)?;
        let keys = table
            .iter()?
            .map(|row| row.map(|(key, _)| key.value()))
            .collect::<Result<Vec<_>, _>>()?;
        for key in keys {
            table.remove(key)?;
        }
        Ok(())
    }
}
