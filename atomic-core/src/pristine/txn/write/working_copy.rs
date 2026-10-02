//! Persistent working-copy trait implementations for `WriteTxn`.

use super::*;

use crate::pristine::traits::{
    decode_working_copy_record, encode_working_copy_record, WorkingCopyMutTxnT, WorkingCopyRecord,
    WorkingCopyTxnT,
};
use crate::types::WorkingCopyId;

fn decode_working_copy_row(
    key: &[u8; WorkingCopyId::SIZE],
    bytes: &[u8],
) -> PristineResult<WorkingCopyRecord> {
    let key_id = WorkingCopyId::from_bytes(*key);
    let record = decode_working_copy_record(bytes)?;
    if record.id != key_id {
        return Err(PristineError::Inconsistent {
            message: format!(
                "WORKING_COPIES key {} contains record for {}",
                key_id, record.id
            ),
        });
    }
    Ok(record)
}

impl WorkingCopyTxnT for WriteTxn<'_> {
    fn get_working_copy(&self, id: WorkingCopyId) -> PristineResult<Option<WorkingCopyRecord>> {
        let table = match self.txn.open_table(WORKING_COPIES) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Err(PristineError::WorkingCopySchemaUnavailable);
            }
            Err(error) => return Err(error.into()),
        };
        let result = match table.get(id.as_bytes())? {
            Some(value) => decode_working_copy_row(id.as_bytes(), value.value()).map(Some),
            None => Ok(None),
        };
        result
    }

    fn list_working_copies(&self) -> PristineResult<Vec<WorkingCopyRecord>> {
        let table = match self.txn.open_table(WORKING_COPIES) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Err(PristineError::WorkingCopySchemaUnavailable);
            }
            Err(error) => return Err(error.into()),
        };
        let mut records = Vec::new();
        for entry in table.iter()? {
            let (key, value) = entry?;
            records.push(decode_working_copy_row(key.value(), value.value())?);
        }
        Ok(records)
    }
}

impl WorkingCopyMutTxnT for WriteTxn<'_> {
    fn put_working_copy(&mut self, record: &WorkingCopyRecord) -> PristineResult<()> {
        let encoded = encode_working_copy_record(record)?;

        if ViewTxnT::get_view_by_id(self, record.desired_view)?.is_none() {
            return Err(PristineError::ViewNotFound {
                name: format!("desired view id={}", record.desired_view),
            });
        }

        if let Some(existing) = WorkingCopyTxnT::get_working_copy(self, record.id)? {
            if existing.location_fingerprint != record.location_fingerprint {
                return Err(PristineError::WorkingCopyIdentityConflict {
                    requested_id: record.id.to_string(),
                    existing_id: existing.id.to_string(),
                });
            }
        }

        if let Some(existing) =
            WorkingCopyTxnT::find_working_copy_by_location(self, &record.location_fingerprint)?
        {
            if existing.id != record.id {
                return Err(PristineError::WorkingCopyIdentityConflict {
                    requested_id: record.id.to_string(),
                    existing_id: existing.id.to_string(),
                });
            }
        }

        let mut table = match self.txn.open_table(WORKING_COPIES) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Err(PristineError::WorkingCopySchemaUnavailable);
            }
            Err(error) => return Err(error.into()),
        };
        table.insert(record.id.as_bytes(), encoded.as_slice())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pristine::{MutTxnT, Pristine, ViewScope};

    fn record(id: WorkingCopyId, location_byte: u8, desired_view: u64) -> WorkingCopyRecord {
        WorkingCopyRecord {
            id,
            location_fingerprint: Hash::from_bytes([location_byte; 32]),
            desired_view,
            desired_state: Merkle::from_bytes([3; 32]),
            materialized_state: None,
            materialized_manifest: None,
        }
    }

    #[test]
    fn working_copy_record_survives_existing_and_readonly_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let id = WorkingCopyId::from_bytes([1; 16]);
        let expected = {
            let pristine = Pristine::open(&db_path).unwrap();
            let mut txn = pristine.write_txn().unwrap();
            let view = txn.open_or_create_view("main").unwrap();
            let initial = record(id, 2, view.id);
            txn.put_working_copy(&initial).unwrap();

            let mut updated = initial;
            updated.desired_state = Merkle::from_bytes([4; 32]);
            updated.materialized_state = Some(Merkle::from_bytes([5; 32]));
            updated.materialized_manifest = Some(Hash::from_bytes([6; 32]));
            txn.put_working_copy(&updated).unwrap();
            assert_eq!(txn.get_working_copy(id).unwrap(), Some(updated.clone()));
            assert_eq!(
                txn.find_working_copy_by_location(&updated.location_fingerprint)
                    .unwrap(),
                Some(updated.clone())
            );
            txn.commit().unwrap();
            updated
        };

        {
            let pristine = Pristine::open_existing(&db_path).unwrap();
            let txn = pristine.read_txn().unwrap();
            assert_eq!(txn.get_working_copy(id).unwrap(), Some(expected.clone()));
            assert_eq!(txn.list_working_copies().unwrap(), vec![expected.clone()]);
        }

        {
            let pristine = Pristine::open_readonly(&db_path).unwrap();
            let txn = pristine.read_txn().unwrap();
            assert_eq!(txn.get_working_copy(id).unwrap(), Some(expected));
        }
    }

    #[test]
    fn working_copy_upsert_rejects_id_and_location_aliases() {
        let dir = tempfile::tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("pristine")).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        let view = txn.open_or_create_view("main").unwrap();
        let original = record(WorkingCopyId::from_bytes([1; 16]), 2, view.id);
        txn.put_working_copy(&original).unwrap();

        let same_id_other_location = record(original.id, 3, view.id);
        assert!(matches!(
            txn.put_working_copy(&same_id_other_location),
            Err(PristineError::WorkingCopyIdentityConflict { .. })
        ));

        let other_id_same_location = record(WorkingCopyId::from_bytes([4; 16]), 2, view.id);
        assert!(matches!(
            txn.put_working_copy(&other_id_same_location),
            Err(PristineError::WorkingCopyIdentityConflict { .. })
        ));

        assert_eq!(
            txn.get_working_copy(original.id).unwrap(),
            Some(original.clone())
        );
        assert_eq!(txn.list_working_copies().unwrap(), vec![original]);
        txn.abort().unwrap();
    }

    #[test]
    fn working_copy_write_requires_an_existing_desired_view() {
        let dir = tempfile::tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("pristine")).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        let record = record(WorkingCopyId::from_bytes([1; 16]), 2, 99);

        assert!(matches!(
            txn.put_working_copy(&record),
            Err(PristineError::ViewNotFound { .. })
        ));
        assert_eq!(txn.get_working_copy(record.id).unwrap(), None);
        txn.abort().unwrap();
    }

    #[test]
    fn malformed_working_copy_rows_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("pristine")).unwrap();
        let txn = pristine.write_txn().unwrap();
        let id = WorkingCopyId::from_bytes([1; 16]);
        {
            let mut table = txn.txn.open_table(WORKING_COPIES).unwrap();
            table.insert(id.as_bytes(), &[99u8][..]).unwrap();
        }

        assert!(matches!(
            txn.get_working_copy(id),
            Err(PristineError::Serialization { .. })
        ));
        assert!(matches!(
            txn.list_working_copies(),
            Err(PristineError::Serialization { .. })
        ));
        txn.abort().unwrap();
    }

    #[test]
    fn working_copy_reference_blocks_draft_view_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("pristine")).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        let main = txn.open_or_create_view("main").unwrap();
        let draft = txn
            .create_view("feature", ViewScope::Draft, Some(main.id))
            .unwrap();
        let record = record(WorkingCopyId::from_bytes([1; 16]), 2, draft.id);
        txn.put_working_copy(&record).unwrap();

        assert!(matches!(
            txn.del_view(&draft),
            Err(PristineError::ViewHasWorkingCopies { .. })
        ));
        assert!(txn.get_view("feature").unwrap().is_some());
        assert_eq!(txn.get_working_copy(record.id).unwrap(), Some(record));
        txn.abort().unwrap();
    }
}
