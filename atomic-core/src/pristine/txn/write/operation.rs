//! Persistent operation journal implementations for `WriteTxn`.

use super::*;

use crate::operation::{
    decode_effect_receipt, decode_operation, decode_operation_heads, decode_operation_scope,
    encode_effect_receipt, encode_operation, encode_operation_heads, encode_operation_scope,
    EffectReceipt, EffectReceiptKind, Operation, OperationHeads, OperationScope,
};
use crate::pristine::traits::{OperationMutTxnT, OperationTxnT};
use crate::types::{EffectReceiptId, OperationId};

fn operation_serialization_error(error: impl std::fmt::Display) -> PristineError {
    PristineError::Serialization {
        message: error.to_string(),
    }
}

fn decode_operation_row(key: &[u8; OperationId::SIZE], bytes: &[u8]) -> PristineResult<Operation> {
    let key_id = OperationId::from_bytes(*key);
    let operation = decode_operation(bytes).map_err(operation_serialization_error)?;
    if operation.id() != key_id {
        return Err(PristineError::Inconsistent {
            message: format!(
                "OPERATIONS key {} contains canonical operation {}",
                key_id,
                operation.id()
            ),
        });
    }
    Ok(operation)
}

fn decode_effect_receipt_row(key: &[u8; 64], bytes: &[u8]) -> PristineResult<EffectReceipt> {
    let (key_operation, key_receipt) = decode_effect_receipt_key(key);
    let receipt = decode_effect_receipt(bytes).map_err(operation_serialization_error)?;
    if receipt.id() != key_receipt || receipt.payload().operation != key_operation {
        return Err(PristineError::Inconsistent {
            message: format!(
                "EFFECT_RECEIPTS key ({key_operation}, {key_receipt}) contains receipt ({}, {})",
                receipt.payload().operation,
                receipt.id()
            ),
        });
    }
    Ok(receipt)
}

impl OperationTxnT for WriteTxn<'_> {
    fn get_operation(&self, id: OperationId) -> PristineResult<Option<Operation>> {
        let table = match self.txn.open_table(OPERATIONS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Err(PristineError::OperationSchemaUnavailable);
            }
            Err(error) => return Err(error.into()),
        };
        let result = match table.get(id.as_bytes())? {
            Some(value) => decode_operation_row(id.as_bytes(), value.value()).map(Some),
            None => Ok(None),
        };
        result
    }

    fn list_operations(&self) -> PristineResult<Vec<Operation>> {
        let table = match self.txn.open_table(OPERATIONS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Err(PristineError::OperationSchemaUnavailable);
            }
            Err(error) => return Err(error.into()),
        };
        let mut operations = Vec::new();
        for entry in table.iter()? {
            let (key, value) = entry?;
            operations.push(decode_operation_row(key.value(), value.value())?);
        }
        Ok(operations)
    }

    fn get_operation_heads(&self, scope: OperationScope) -> PristineResult<OperationHeads> {
        let table = match self.txn.open_table(OP_HEADS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Err(PristineError::OperationSchemaUnavailable);
            }
            Err(error) => return Err(error.into()),
        };
        let key = encode_operation_scope(scope);
        let heads = match table.get(&key)? {
            Some(value) => {
                decode_operation_heads(value.value()).map_err(operation_serialization_error)?
            }
            None => OperationHeads::new(Vec::new()),
        };
        for head in heads.as_slice() {
            if OperationTxnT::get_operation(self, *head)?.is_none() {
                return Err(PristineError::OperationNotFound {
                    id: head.to_string(),
                });
            }
        }
        Ok(heads)
    }

    fn list_operation_heads(&self) -> PristineResult<Vec<(OperationScope, OperationHeads)>> {
        let table = match self.txn.open_table(OP_HEADS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Err(PristineError::OperationSchemaUnavailable);
            }
            Err(error) => return Err(error.into()),
        };
        let mut rows = Vec::new();
        for entry in table.iter()? {
            let (key, value) = entry?;
            let scope =
                decode_operation_scope(key.value()).map_err(operation_serialization_error)?;
            let heads =
                decode_operation_heads(value.value()).map_err(operation_serialization_error)?;
            for head in heads.as_slice() {
                if OperationTxnT::get_operation(self, *head)?.is_none() {
                    return Err(PristineError::OperationNotFound {
                        id: head.to_string(),
                    });
                }
            }
            rows.push((scope, heads));
        }
        Ok(rows)
    }

    fn get_effect_receipts(&self, operation: OperationId) -> PristineResult<Vec<EffectReceipt>> {
        if OperationTxnT::get_operation(self, operation)?.is_none() {
            return Err(PristineError::OperationNotFound {
                id: operation.to_string(),
            });
        }
        let table = match self.txn.open_table(EFFECT_RECEIPTS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Err(PristineError::OperationSchemaUnavailable);
            }
            Err(error) => return Err(error.into()),
        };
        let start = encode_effect_receipt_key(operation, EffectReceiptId::from_bytes([0; 32]));
        let end = encode_effect_receipt_key(operation, EffectReceiptId::from_bytes([u8::MAX; 32]));
        let mut receipts = Vec::new();
        for entry in table.range::<&[u8; 64]>(&start..=&end)? {
            let (key, value) = entry?;
            receipts.push(decode_effect_receipt_row(key.value(), value.value())?);
        }
        Ok(receipts)
    }
}

impl OperationMutTxnT for WriteTxn<'_> {
    fn put_operation(&mut self, operation: &Operation) -> PristineResult<()> {
        let encoded = encode_operation(operation).map_err(operation_serialization_error)?;
        for parent in &operation.payload().parents {
            if OperationTxnT::get_operation(self, *parent)?.is_none() {
                return Err(PristineError::OperationParentNotFound {
                    operation: operation.id().to_string(),
                    parent: parent.to_string(),
                });
            }
        }

        let mut table = match self.txn.open_table(OPERATIONS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Err(PristineError::OperationSchemaUnavailable);
            }
            Err(error) => return Err(error.into()),
        };
        if let Some(existing) = table.get(operation.id().as_bytes())? {
            let existing_bytes = existing.value();
            decode_operation_row(operation.id().as_bytes(), existing_bytes)?;
            if existing_bytes != encoded.as_slice() {
                return Err(PristineError::Inconsistent {
                    message: format!(
                        "append-only OPERATIONS row {} already contains different bytes",
                        operation.id()
                    ),
                });
            }
            return Ok(());
        }
        table.insert(operation.id().as_bytes(), encoded.as_slice())?;
        Ok(())
    }

    fn compare_and_set_operation_heads(
        &mut self,
        scope: OperationScope,
        expected: &[OperationId],
        replacement: &[OperationId],
    ) -> PristineResult<()> {
        let expected = OperationHeads::new(expected.to_vec());
        let replacement = OperationHeads::new(replacement.to_vec());
        let actual = OperationTxnT::get_operation_heads(self, scope)?;
        if actual != expected {
            return Err(PristineError::OperationHeadConflict {
                scope: scope.to_string(),
                expected: expected
                    .as_slice()
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
                actual: actual.as_slice().iter().map(ToString::to_string).collect(),
            });
        }
        for head in replacement.as_slice() {
            if OperationTxnT::get_operation(self, *head)?.is_none() {
                return Err(PristineError::OperationNotFound {
                    id: head.to_string(),
                });
            }
        }

        let key = encode_operation_scope(scope);
        let mut table = match self.txn.open_table(OP_HEADS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Err(PristineError::OperationSchemaUnavailable);
            }
            Err(error) => return Err(error.into()),
        };
        if replacement.is_empty() {
            table.remove(&key)?;
        } else {
            let encoded =
                encode_operation_heads(&replacement).map_err(operation_serialization_error)?;
            table.insert(&key, encoded.as_slice())?;
        }
        Ok(())
    }

    fn append_effect_receipt(&mut self, receipt: &EffectReceipt) -> PristineResult<()> {
        let operation = OperationTxnT::get_operation(self, receipt.payload().operation)?
            .ok_or_else(|| PristineError::OperationNotFound {
                id: receipt.payload().operation.to_string(),
            })?;
        if let Some(ordinal) = receipt.payload().effect_ordinal {
            let effect = operation
                .payload()
                .delta
                .effects
                .get(ordinal as usize)
                .filter(|effect| effect.ordinal == ordinal)
                .ok_or_else(|| PristineError::EffectPlanNotFound {
                    operation: operation.id().to_string(),
                    ordinal,
                })?;
            let successful_observation = match receipt.payload().kind {
                EffectReceiptKind::Applied | EffectReceiptKind::RolledBack => {
                    receipt.payload().observed_old.as_ref() == Some(&effect.expected_old)
                        && receipt.payload().observed_new.as_ref() == Some(&effect.expected_new)
                }
                EffectReceiptKind::Recovered => {
                    receipt.payload().observed_old.as_ref() == Some(&effect.expected_new)
                        && receipt.payload().observed_new.as_ref() == Some(&effect.expected_new)
                }
                EffectReceiptKind::LeaseRejected => true,
                EffectReceiptKind::Verified => false,
            };
            if !successful_observation {
                return Err(PristineError::Inconsistent {
                    message: format!(
                        "effect receipt {} does not match operation {} effect {} leases",
                        receipt.id(),
                        operation.id(),
                        ordinal
                    ),
                });
            }
        } else if receipt.payload().kind != EffectReceiptKind::Verified {
            return Err(PristineError::EffectPlanNotFound {
                operation: operation.id().to_string(),
                ordinal: u32::MAX,
            });
        } else {
            let existing = OperationTxnT::get_effect_receipts(self, operation.id())?;
            let completed: std::collections::BTreeSet<u32> = existing
                .iter()
                .filter_map(|existing| match existing.payload().kind {
                    EffectReceiptKind::Applied
                    | EffectReceiptKind::RolledBack
                    | EffectReceiptKind::Recovered => existing.payload().effect_ordinal,
                    EffectReceiptKind::Verified | EffectReceiptKind::LeaseRejected => None,
                })
                .collect();
            if let Some(missing) = operation
                .payload()
                .delta
                .effects
                .iter()
                .find(|effect| !completed.contains(&effect.ordinal))
            {
                return Err(PristineError::Inconsistent {
                    message: format!(
                        "operation {} cannot be verified before effect {} has a successful receipt",
                        operation.id(),
                        missing.ordinal
                    ),
                });
            }
        }

        let encoded = encode_effect_receipt(receipt).map_err(operation_serialization_error)?;
        let key = encode_effect_receipt_key(receipt.payload().operation, receipt.id());
        let mut table = match self.txn.open_table(EFFECT_RECEIPTS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Err(PristineError::OperationSchemaUnavailable);
            }
            Err(error) => return Err(error.into()),
        };
        if let Some(existing) = table.get(&key)? {
            let existing_bytes = existing.value();
            decode_effect_receipt_row(&key, existing_bytes)?;
            if existing_bytes != encoded.as_slice() {
                return Err(PristineError::Inconsistent {
                    message: format!(
                        "append-only EFFECT_RECEIPTS row {} already contains different bytes",
                        receipt.id()
                    ),
                });
            }
            return Ok(());
        }
        table.insert(&key, encoded.as_slice())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operation::{
        ActorRef, EffectPlan, EffectReceiptPayload, EffectTarget, EffectValue, OperationKind,
        OperationPayload, RepoStateDelta, RepoStateRef,
    };
    use crate::pristine::{OperationMutTxnT, Pristine};
    use crate::types::Hash;

    fn anchor() -> Operation {
        anchor_at(1)
    }

    fn anchor_at(timestamp_ms: i64) -> Operation {
        Operation::new(OperationPayload {
            parents: Vec::new(),
            kind: OperationKind::Anchor,
            relation: None,
            working_copy: None,
            before: RepoStateRef::EMPTY,
            delta: RepoStateDelta {
                after: RepoStateRef::EMPTY,
                metadata: Vec::new(),
                effects: Vec::new(),
            },
            git_observed: Vec::new(),
            evidence: Vec::new(),
            actor: ActorRef::System {
                name: "test".into(),
            },
            timestamp_ms,
            lossy: Vec::new(),
        })
        .unwrap()
    }

    fn child(parent: OperationId) -> Operation {
        Operation::new(OperationPayload {
            parents: vec![parent],
            kind: OperationKind::Materialize,
            relation: None,
            working_copy: None,
            before: RepoStateRef::EMPTY,
            delta: RepoStateDelta {
                after: RepoStateRef::EMPTY,
                metadata: Vec::new(),
                effects: vec![EffectPlan {
                    ordinal: 0,
                    target: EffectTarget::FilesystemPath {
                        path: "file.txt".into(),
                    },
                    expected_old: EffectValue::Absent,
                    expected_new: EffectValue::Verification(Hash::from_bytes([9; 32])),
                }],
            },
            git_observed: Vec::new(),
            evidence: Vec::new(),
            actor: ActorRef::System {
                name: "test".into(),
            },
            timestamp_ms: 2,
            lossy: Vec::new(),
        })
        .unwrap()
    }

    fn applied_receipt(operation: OperationId) -> EffectReceipt {
        EffectReceipt::new(EffectReceiptPayload {
            operation,
            effect_ordinal: Some(0),
            attempt: 0,
            kind: EffectReceiptKind::Applied,
            observed_old: Some(EffectValue::Absent),
            observed_new: Some(EffectValue::Verification(Hash::from_bytes([9; 32]))),
            timestamp_ms: 3,
        })
        .unwrap()
    }

    #[test]
    fn operations_list_in_id_order_and_survive_readonly_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pristine");
        let mut inserted = vec![anchor_at(1), anchor_at(2), anchor_at(3)];
        inserted.sort_by_key(Operation::id);
        inserted.reverse();
        let mut expected = inserted.clone();
        expected.sort_by_key(Operation::id);

        {
            let pristine = Pristine::open(&path).unwrap();
            let mut txn = pristine.write_txn().unwrap();
            for operation in &inserted {
                txn.put_operation(operation).unwrap();
            }
            assert_eq!(txn.list_operations().unwrap(), expected);
            txn.commit().unwrap();
        }

        let pristine = Pristine::open_readonly(&path).unwrap();
        let txn = pristine.read_txn().unwrap();
        assert_eq!(txn.list_operations().unwrap(), expected);
    }

    #[test]
    fn operation_heads_and_receipts_survive_readonly_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pristine");
        let anchor = anchor();
        let child = child(anchor.id());
        let receipt = applied_receipt(child.id());

        {
            let pristine = Pristine::open(&path).unwrap();
            let mut txn = pristine.write_txn().unwrap();
            txn.put_operation(&anchor).unwrap();
            txn.put_operation(&child).unwrap();
            txn.compare_and_set_operation_heads(
                OperationScope::Repository,
                &[],
                &[child.id(), child.id()],
            )
            .unwrap();
            txn.append_effect_receipt(&receipt).unwrap();
            txn.commit().unwrap();
        }

        let pristine = Pristine::open_readonly(&path).unwrap();
        let txn = pristine.read_txn().unwrap();
        assert_eq!(txn.get_operation(anchor.id()).unwrap(), Some(anchor));
        assert_eq!(txn.get_operation(child.id()).unwrap(), Some(child.clone()));
        assert_eq!(
            txn.get_operation_heads(OperationScope::Repository)
                .unwrap()
                .as_slice(),
            &[child.id()]
        );
        assert_eq!(txn.get_effect_receipts(child.id()).unwrap(), vec![receipt]);
    }

    #[test]
    fn immutable_rows_are_idempotent_and_parent_checked() {
        let dir = tempfile::tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("pristine")).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        let anchor = anchor();
        let child = child(anchor.id());

        assert!(matches!(
            txn.put_operation(&child),
            Err(PristineError::OperationParentNotFound { .. })
        ));
        txn.put_operation(&anchor).unwrap();
        txn.put_operation(&anchor).unwrap();
        txn.put_operation(&child).unwrap();
        let premature_verified = EffectReceipt::new(EffectReceiptPayload {
            operation: child.id(),
            effect_ordinal: None,
            attempt: 0,
            kind: EffectReceiptKind::Verified,
            observed_old: None,
            observed_new: None,
            timestamp_ms: 3,
        })
        .unwrap();
        assert!(matches!(
            txn.append_effect_receipt(&premature_verified),
            Err(PristineError::Inconsistent { .. })
        ));
        let receipt = applied_receipt(child.id());
        txn.append_effect_receipt(&receipt).unwrap();
        txn.append_effect_receipt(&receipt).unwrap();
        txn.append_effect_receipt(&premature_verified).unwrap();
        assert_eq!(txn.get_effect_receipts(child.id()).unwrap().len(), 2);

        let missing_effect = EffectReceipt::new(EffectReceiptPayload {
            operation: child.id(),
            effect_ordinal: Some(1),
            attempt: 0,
            kind: EffectReceiptKind::Applied,
            observed_old: None,
            observed_new: None,
            timestamp_ms: 4,
        })
        .unwrap();
        assert!(matches!(
            txn.append_effect_receipt(&missing_effect),
            Err(PristineError::EffectPlanNotFound { ordinal: 1, .. })
        ));
        txn.abort().unwrap();
    }

    #[test]
    fn operation_heads_use_compare_and_set_and_require_existing_targets() {
        let dir = tempfile::tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("pristine")).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        let anchor = anchor();
        txn.put_operation(&anchor).unwrap();
        txn.compare_and_set_operation_heads(OperationScope::Repository, &[], &[anchor.id()])
            .unwrap();

        assert!(matches!(
            txn.compare_and_set_operation_heads(OperationScope::Repository, &[], &[anchor.id()]),
            Err(PristineError::OperationHeadConflict { .. })
        ));
        assert!(matches!(
            txn.compare_and_set_operation_heads(
                OperationScope::Repository,
                &[anchor.id()],
                &[OperationId::from_bytes([99; 32])],
            ),
            Err(PristineError::OperationNotFound { .. })
        ));
        txn.abort().unwrap();
    }

    #[test]
    fn reads_recompute_ids_and_fail_closed_on_malformed_rows() {
        let dir = tempfile::tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("pristine")).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        let anchor = anchor();
        let child = child(anchor.id());
        txn.put_operation(&anchor).unwrap();
        txn.put_operation(&child).unwrap();

        let encoded = encode_operation(&anchor).unwrap();
        let wrong = OperationId::from_bytes([7; 32]);
        {
            let mut table = txn.txn.open_table(OPERATIONS).unwrap();
            table.insert(wrong.as_bytes(), encoded.as_slice()).unwrap();
        }
        assert!(matches!(
            txn.get_operation(wrong),
            Err(PristineError::Inconsistent { .. })
        ));
        assert!(matches!(
            txn.list_operations(),
            Err(PristineError::Inconsistent { .. })
        ));

        let receipt = applied_receipt(child.id());
        let receipt_bytes = encode_effect_receipt(&receipt).unwrap();
        let wrong_receipt = EffectReceiptId::from_bytes([8; 32]);
        let wrong_key = encode_effect_receipt_key(child.id(), wrong_receipt);
        {
            let mut table = txn.txn.open_table(EFFECT_RECEIPTS).unwrap();
            table.insert(&wrong_key, receipt_bytes.as_slice()).unwrap();
        }
        assert!(matches!(
            txn.get_effect_receipts(child.id()),
            Err(PristineError::Inconsistent { .. })
        ));

        let scope_key = encode_operation_scope(OperationScope::Repository);
        {
            let mut table = txn.txn.open_table(OP_HEADS).unwrap();
            table.insert(&scope_key, &[99u8][..]).unwrap();
        }
        assert!(matches!(
            txn.get_operation_heads(OperationScope::Repository),
            Err(PristineError::Serialization { .. })
        ));

        let dangling = OperationId::from_bytes([10; 32]);
        let dangling_heads = encode_operation_heads(&OperationHeads::new(vec![dangling])).unwrap();
        {
            let mut table = txn.txn.open_table(OP_HEADS).unwrap();
            table.insert(&scope_key, dangling_heads.as_slice()).unwrap();
        }
        assert!(matches!(
            txn.get_operation_heads(OperationScope::Repository),
            Err(PristineError::OperationNotFound { .. })
        ));
        assert!(matches!(
            txn.get_effect_receipts(OperationId::from_bytes([11; 32])),
            Err(PristineError::OperationNotFound { .. })
        ));
        txn.abort().unwrap();
    }
}
