//! Tag trait implementations for WriteTxn.

use super::*;

use crate::pristine::tables::{
    BRIDGE_EVENT_CAPTURES, BRIDGE_EVENT_CAPTURE_ANCHORS, GIT_COMMIT_CLOSURES, GIT_SHA_INDEX,
    TAG_NAME_INDEX, TAG_RECORDS,
};
use crate::pristine::traits::tag::{
    BridgeEventCaptureMutTxnT, BridgeEventCaptureTxnT, GitCommitClosureMutTxnT,
    GitCommitClosureTxnT, GitShaIndexMutTxnT, GitShaIndexTxnT, TagMutTxnT, TagRecord, TagTxnT,
};

// TagTxnT Implementation

impl<'a> TagTxnT for WriteTxn<'a> {
    fn get_tag(&self, view: &str, name: &str) -> PristineResult<Option<TagRecord>> {
        let key = format!("{}\0{}", view, name);
        let entity_id = {
            let table = self.txn.open_table(TAG_NAME_INDEX)?;
            let result = table.get(key.as_str())?;
            match result {
                Some(v) => v.value(),
                None => return Ok(None),
            }
        };
        let table = self.txn.open_table(TAG_RECORDS)?;
        let result = table.get(entity_id)?;
        match result {
            Some(v) => {
                let record: TagRecord =
                    postcard::from_bytes(v.value()).map_err(|e| PristineError::Serialization {
                        message: format!(
                            "failed to deserialize TagRecord for '{}\\0{}': {}",
                            view, name, e
                        ),
                    })?;
                Ok(Some(record))
            }
            None => Ok(None),
        }
    }

    fn list_tags(&self, view: &str) -> PristineResult<Vec<TagRecord>> {
        let prefix = format!("{}\0", view);

        // Collect entity_ids via prefix scan on TAG_NAME_INDEX
        let entity_ids: Vec<u64> = {
            let table = self.txn.open_table(TAG_NAME_INDEX)?;
            let mut ids = Vec::new();
            for item in table.range(prefix.as_str()..)? {
                let (key, value) = item?;
                let k = key.value();
                if !k.starts_with(prefix.as_str()) {
                    break;
                }
                ids.push(value.value());
            }
            ids
        };

        // Read TAG_RECORDS for each entity_id
        let table = self.txn.open_table(TAG_RECORDS)?;
        let mut tags = Vec::new();
        for entity_id in entity_ids {
            let result = table.get(entity_id)?;
            if let Some(v) = result {
                let record: TagRecord =
                    postcard::from_bytes(v.value()).map_err(|e| PristineError::Serialization {
                        message: format!(
                            "failed to deserialize TagRecord (entity_id={}): {}",
                            entity_id, e
                        ),
                    })?;
                tags.push(record);
            }
        }
        Ok(tags)
    }

    fn list_all_tags(&self) -> PristineResult<Vec<TagRecord>> {
        let table = self.txn.open_table(TAG_RECORDS)?;
        let mut tags = Vec::new();
        for item in table.iter()? {
            let (_key, value) = item?;
            let record: TagRecord =
                postcard::from_bytes(value.value()).map_err(|e| PristineError::Serialization {
                    message: format!("failed to deserialize TagRecord: {}", e),
                })?;
            tags.push(record);
        }
        Ok(tags)
    }

    fn find_tag_by_hash(&self, hash: &Hash) -> PristineResult<Option<TagRecord>> {
        // Look up entity_id via INTERNAL
        let entity_id = {
            let table = self.txn.open_table(INTERNAL)?;
            let result = table.get(hash.as_bytes())?;
            match result {
                Some(v) => v.value(),
                None => return Ok(None),
            }
        };
        // Check it's actually a TAG
        {
            let table = self.txn.open_table(NODE_TYPES)?;
            let result = table.get(entity_id)?;
            match result {
                Some(v) if v.value() == node_type::TAG => {}
                _ => return Ok(None),
            };
        }
        // Read the record
        let table = self.txn.open_table(TAG_RECORDS)?;
        let result = table.get(entity_id)?;
        match result {
            Some(v) => {
                let record: TagRecord =
                    postcard::from_bytes(v.value()).map_err(|e| PristineError::Serialization {
                        message: format!(
                            "failed to deserialize TagRecord (entity_id={}): {}",
                            entity_id, e
                        ),
                    })?;
                Ok(Some(record))
            }
            None => Ok(None),
        }
    }
}

// TagMutTxnT Implementation

impl<'a> TagMutTxnT for WriteTxn<'a> {
    fn put_tag(&mut self, tag: &TagRecord) -> PristineResult<NodeId> {
        let hash = tag.content_hash();

        // Register in entity tables (INTERNAL, EXTERNAL, NODE_TYPES)
        let entity_id = self.register_entity(&hash, node_type::TAG)?;

        // Serialize and write to TAG_RECORDS
        let bytes = postcard::to_allocvec(tag).map_err(|e| PristineError::Serialization {
            message: format!("failed to serialize TagRecord: {}", e),
        })?;
        {
            let mut table = self.txn.open_table(TAG_RECORDS)?;
            table.insert(entity_id.get(), bytes.as_slice())?;
        }

        // Write TAG_NAME_INDEX
        let key = format!("{}\0{}", tag.view, tag.name);
        {
            let mut table = self.txn.open_table(TAG_NAME_INDEX)?;
            table.insert(key.as_str(), entity_id.get())?;
        }

        Ok(entity_id)
    }

    fn del_tag(&mut self, view: &str, name: &str) -> PristineResult<bool> {
        let key = format!("{}\0{}", view, name);

        // Look up entity_id
        let entity_id = {
            let table = self.txn.open_table(TAG_NAME_INDEX)?;
            let result = table.get(key.as_str())?;
            match result {
                Some(v) => v.value(),
                None => return Ok(false),
            }
        };

        // Remove from TAG_NAME_INDEX
        {
            let mut table = self.txn.open_table(TAG_NAME_INDEX)?;
            table.remove(key.as_str())?;
        }

        // Remove from TAG_RECORDS
        {
            let mut table = self.txn.open_table(TAG_RECORDS)?;
            table.remove(entity_id)?;
        }

        Ok(true)
    }

    fn del_tags_for_view(&mut self, view: &str) -> PristineResult<usize> {
        let prefix = format!("{}\0", view);

        // Collect keys and entity_ids to delete (can't mutate while iterating)
        let to_delete: Vec<(String, u64)> = {
            let table = self.txn.open_table(TAG_NAME_INDEX)?;
            let mut entries = Vec::new();
            for item in table.range(prefix.as_str()..)? {
                let (key, value) = item?;
                let k = key.value();
                if !k.starts_with(prefix.as_str()) {
                    break;
                }
                entries.push((k.to_string(), value.value()));
            }
            entries
        };

        let count = to_delete.len();

        // Delete from TAG_NAME_INDEX
        {
            let mut table = self.txn.open_table(TAG_NAME_INDEX)?;
            for (key, _) in &to_delete {
                table.remove(key.as_str())?;
            }
        }

        // Delete from TAG_RECORDS
        {
            let mut table = self.txn.open_table(TAG_RECORDS)?;
            for (_, entity_id) in &to_delete {
                table.remove(*entity_id)?;
            }
        }

        Ok(count)
    }
}

// ============================================================================
// GIT SHA INDEX
// ============================================================================

impl<'a> GitShaIndexTxnT for WriteTxn<'a> {
    fn get_by_git_sha(&self, sha: &str) -> PristineResult<Option<NodeId>> {
        let table = self.txn.open_table(GIT_SHA_INDEX)?;
        let guard = table.get(sha)?;
        let result = guard.map(|v| v.value());
        Ok(result.map(NodeId::new))
    }

    fn has_git_sha(&self, sha: &str) -> PristineResult<bool> {
        Ok(self.get_by_git_sha(sha)?.is_some())
    }

    fn list_git_shas(&self) -> PristineResult<Vec<String>> {
        let table = self.txn.open_table(GIT_SHA_INDEX)?;
        let mut shas = Vec::new();
        for item in table.iter()? {
            let (key, _) = item?;
            shas.push(key.value().to_string());
        }
        Ok(shas)
    }

    fn find_by_git_sha_prefix(&self, prefix: &str) -> PristineResult<Option<NodeId>> {
        let table = self.txn.open_table(GIT_SHA_INDEX)?;
        let upper = format!("{}g", prefix);
        let mut matches = Vec::new();
        for item in table.range(prefix..upper.as_str())? {
            let (key, value) = item?;
            matches.push((key.value().to_string(), NodeId::new(value.value())));
            if matches.len() > 1 {
                return Err(PristineError::AmbiguousPrefix {
                    prefix: prefix.to_string(),
                    matches: matches.iter().map(|(k, _)| k.clone()).collect(),
                });
            }
        }
        Ok(matches.into_iter().next().map(|(_, id)| id))
    }
}

impl<'a> GitShaIndexMutTxnT for WriteTxn<'a> {
    fn put_git_sha(&mut self, sha: &str, entity_id: NodeId) -> PristineResult<()> {
        let mut table = self.txn.open_table(GIT_SHA_INDEX)?;
        table.insert(sha, entity_id.get())?;
        Ok(())
    }

    fn del_git_sha(&mut self, sha: &str) -> PristineResult<bool> {
        let mut table = self.txn.open_table(GIT_SHA_INDEX)?;
        let removed = table.remove(sha)?;
        Ok(removed.is_some())
    }
}

// ============================================================================
// GIT COMMIT INTERPRETATION CLOSURES (CB-9B review R1)
// ============================================================================

/// Encode a closure as u32 count + concatenated 32-byte hashes.
fn encode_git_commit_closure(closure: &[Hash]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(4 + closure.len() * 32);
    bytes.extend_from_slice(&(closure.len() as u32).to_le_bytes());
    for hash in closure {
        bytes.extend_from_slice(hash.as_bytes());
    }
    bytes
}

fn decode_git_commit_closure(bytes: &[u8]) -> PristineResult<Vec<Hash>> {
    if bytes.len() < 4 {
        return Err(PristineError::Serialization {
            message: format!(
                "git commit closure is truncated: {} byte(s), need at least 4",
                bytes.len()
            ),
        });
    }
    let count = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    if bytes.len() != 4 + count * 32 {
        return Err(PristineError::Serialization {
            message: format!(
                "git commit closure claims {count} hashes but holds {} bytes",
                bytes.len()
            ),
        });
    }
    let mut closure = Vec::with_capacity(count);
    for index in 0..count {
        let start = 4 + index * 32;
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&bytes[start..start + 32]);
        closure.push(Hash::from_bytes(hash));
    }
    Ok(closure)
}

impl<'a> GitCommitClosureTxnT for WriteTxn<'a> {
    fn get_git_commit_closure(&self, sha: &str) -> PristineResult<Option<Vec<Hash>>> {
        let decoded = {
            let table = match self.txn.open_table(GIT_COMMIT_CLOSURES) {
                Ok(table) => table,
                Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
                Err(error) => return Err(PristineError::from(error)),
            };
            let found = table.get(sha)?;
            match found {
                Some(bytes) => bytes.value().to_vec(),
                None => return Ok(None),
            }
        };
        Ok(Some(decode_git_commit_closure(&decoded)?))
    }

    fn has_git_commit_closure(&self, sha: &str) -> PristineResult<bool> {
        match self.txn.open_table(GIT_COMMIT_CLOSURES) {
            Err(redb::TableError::TableDoesNotExist(_)) => Ok(false),
            Err(error) => Err(PristineError::from(error)),
            Ok(table) => Ok(table.get(sha)?.is_some()),
        }
    }
}

impl<'a> GitCommitClosureMutTxnT for WriteTxn<'a> {
    fn put_git_commit_closure(&mut self, sha: &str, closure: &[Hash]) -> PristineResult<()> {
        let encoded = encode_git_commit_closure(closure);
        let mut table = self.txn.open_table(GIT_COMMIT_CLOSURES)?;
        if let Some(existing) = table.get(sha)? {
            if existing.value() != encoded.as_slice() {
                return Err(PristineError::Serialization {
                    message: format!(
                        "git commit {sha} already has a different persisted interpretation \
                         closure; Git ancestry is immutable, so this is corruption or a \
                         conflicting reimport"
                    ),
                });
            }
            return Ok(());
        }
        table.insert(sha, encoded.as_slice())?;
        Ok(())
    }
}

// ============================================================================
// BRIDGE EVENT CAPTURES (CB-9B review R4)
// ============================================================================

impl<'a> BridgeEventCaptureTxnT for WriteTxn<'a> {
    fn get_bridge_event_capture(&self, hash: &[u8; 32]) -> PristineResult<Option<Vec<u8>>> {
        match self.txn.open_table(BRIDGE_EVENT_CAPTURES) {
            Err(redb::TableError::TableDoesNotExist(_)) => Ok(None),
            Err(error) => Err(PristineError::from(error)),
            Ok(table) => Ok(table.get(hash)?.map(|v| v.value().to_vec())),
        }
    }

    fn get_bridge_event_capture_anchor(&self, hash: &[u8; 32]) -> PristineResult<Option<[u8; 32]>> {
        match self.txn.open_table(BRIDGE_EVENT_CAPTURE_ANCHORS) {
            Err(redb::TableError::TableDoesNotExist(_)) => Ok(None),
            Err(error) => Err(PristineError::from(error)),
            Ok(table) => Ok(table.get(hash)?.map(|v| *v.value())),
        }
    }

    fn get_bridge_ref_capture_token(
        &self,
        operation: &[u8; 32],
    ) -> PristineResult<Option<[u8; 32]>> {
        match self.txn.open_table(BRIDGE_REF_CAPTURE_TOKENS) {
            Err(redb::TableError::TableDoesNotExist(_)) => Ok(None),
            Err(error) => Err(PristineError::from(error)),
            Ok(table) => Ok(table.get(operation)?.map(|v| *v.value())),
        }
    }
}

impl<'a> BridgeEventCaptureMutTxnT for WriteTxn<'a> {
    fn put_bridge_event_capture(&mut self, bytes: &[u8]) -> PristineResult<[u8; 32]> {
        let digest: [u8; 32] = blake3::hash(bytes).into();
        let mut table = self.txn.open_table(BRIDGE_EVENT_CAPTURES)?;
        if let Some(existing) = table.get(&digest)? {
            if existing.value() != bytes {
                return Err(PristineError::Serialization {
                    message: "bridge event capture hash collision: existing capture has \
                              different bytes"
                        .to_string(),
                });
            }
            return Ok(digest);
        }
        table.insert(&digest, bytes)?;
        Ok(digest)
    }

    fn put_bridge_ref_capture_token(
        &mut self,
        operation: &[u8; 32],
        token: &[u8; 32],
    ) -> PristineResult<()> {
        let mut table = self.txn.open_table(BRIDGE_REF_CAPTURE_TOKENS)?;
        if let Some(existing) = table.get(operation)? {
            if existing.value() != token {
                return Err(PristineError::Serialization {
                    message: "bridge ref write already has a different minted capture \
                              context; a prepared capture context is immutable"
                        .to_string(),
                });
            }
            return Ok(());
        }
        table.insert(operation, token)?;
        Ok(())
    }

    fn put_anchored_bridge_event_capture(
        &mut self,
        bytes: &[u8],
        operation: &crate::types::OperationId,
    ) -> PristineResult<[u8; 32]> {
        use crate::operation::{
            effect_lease_matches_oid, post_rewrite_event_pairs, EffectTarget, OperationKind,
            OperationScope,
        };
        use crate::pristine::OperationTxnT;
        // The anchor is accepted only while `operation` exists and is the
        // active head of its scope (review C2): this is the RFC §5.4
        // "during an active captured operation" precondition, enforced at
        // write time inside the same transaction that writes the anchor.
        let stored =
            self.get_operation(*operation)?
                .ok_or_else(|| PristineError::Serialization {
                    message: "cannot anchor a bridge event capture to an unknown operation"
                        .to_string(),
                })?;
        let scope = match stored.payload().working_copy {
            Some(working_copy) => OperationScope::WorkingCopy(working_copy),
            None => OperationScope::Repository,
        };
        let heads = self.get_operation_heads(scope)?;
        if !heads.as_slice().contains(operation) {
            return Err(PristineError::Serialization {
                message: "cannot anchor a bridge event capture to an operation that is not \
                          the active head of its scope"
                    .to_string(),
            });
        }
        // "Active" also means not yet finalized: an operation with a verified
        // receipt has completed, so a capture written now is after the fact,
        // not during the operation (review C2).
        if self
            .get_effect_receipts(*operation)?
            .iter()
            .any(|receipt| receipt.payload().kind == crate::operation::EffectReceiptKind::Verified)
        {
            return Err(PristineError::Serialization {
                message: "cannot anchor a bridge event capture to an operation that is \
                          already verified"
                    .to_string(),
            });
        }
        // Review D3: the anchor is bound to the operation it represents —
        // the captured event must DESCRIBE this operation's rewrite. The
        // event bytes must carry a post-rewrite rewrite pair set (the Git
        // `<old> <new>` wire format or the durable JSON evidence record the
        // hook journals), and the prepared operation must carry a GitRef
        // effect whose before/after leases are exactly those OIDs for EVERY
        // named pair. Anything else cannot establish the RFC §5.4
        // relationship and is refused: a retrospective advisory capture may
        // never be promoted by anchoring it to an unrelated active ref-write
        // (fixture cb9b-current-reanchored-sibling anchored an ordinary
        // sibling pair to refs/heads/UNRELATED operations this way).
        let pairs =
            post_rewrite_event_pairs(bytes).ok_or_else(|| PristineError::Serialization {
                message: "cannot anchor a bridge event capture whose bytes do not name a \
                          post-rewrite rewrite pair of full-length OIDs"
                    .to_string(),
            })?;
        let describes_operation = matches!(stored.payload().kind, OperationKind::ExportGitRefs)
            && pairs.iter().all(|(old_oid, new_oid)| {
                stored.payload().delta.effects.iter().any(|effect| {
                    matches!(effect.target, EffectTarget::GitRef { .. })
                        && effect_lease_matches_oid(&effect.expected_old, old_oid)
                        && effect_lease_matches_oid(&effect.expected_new, new_oid)
                })
            });
        if !describes_operation {
            return Err(PristineError::Serialization {
                message: "cannot anchor a bridge event capture to an operation whose effect \
                          leases do not describe exactly this rewrite; the anchor must bind \
                          the event to the operation that performed it"
                    .to_string(),
            });
        }
        let digest: [u8; 32] = blake3::hash(bytes).into();
        // Review D3: anchors are immutable and create-only. An existing
        // anchor for this capture is either the same operation (idempotent)
        // or a conflicting re-anchoring that must refuse — the historical
        // unconditional insert silently REPLACED the anchor and let one
        // digest be re-anchored to unrelated operations. This immutability
        // check precedes the capture-context checks so a conflicting
        // re-anchoring is always refused by the immutability contract.
        {
            let anchors = self.txn.open_table(BRIDGE_EVENT_CAPTURE_ANCHORS)?;
            let existing_bytes: Option<[u8; 32]> =
                anchors.get(&digest)?.map(|guard| *guard.value());
            if let Some(existing) = existing_bytes {
                let existing = crate::types::OperationId::from_bytes(existing);
                if existing == *operation {
                    return Ok(digest);
                }
                return Err(PristineError::Serialization {
                    message: "bridge event capture is already anchored to a different \
                              operation; anchors are immutable and cannot be re-anchored"
                        .to_string(),
                });
            }
        }
        // Review E2: matching OID leases alone do not authenticate a rewrite.
        // A ref-only movement between the same OIDs is a DIFFERENT tier (RFC
        // §5.4), and an earlier advisory capture between the same OIDs must
        // never be retroactively promoted onto a later prepared write. The
        // captured bytes must carry the exact capture token minted for THIS
        // operation when it was prepared — the unforgeable binding between
        // capture creation and the prepared genuine rewrite context. Captures
        // without a token (bare wire pairs, older advisory JSON) or with a
        // foreign token are refused.
        let minted = self
            .get_bridge_ref_capture_token(operation.as_bytes())?
            .ok_or_else(|| PristineError::Serialization {
                message: "cannot anchor a bridge event capture to an operation with no \
                              minted capture context; only a capture produced within the \
                              operation's prepared capture context can bind to it"
                    .to_string(),
            })?;
        let event_token =
            crate::operation::post_rewrite_event_capture_token(bytes).ok_or_else(|| {
                PristineError::Serialization {
                    message: "cannot anchor a bridge event capture whose bytes carry no \
                              operation capture token; a capture produced outside the \
                              operation's prepared capture context is advisory and can \
                              never be promoted"
                        .to_string(),
                }
            })?;
        let minted_hex = minted
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        if event_token != minted_hex {
            return Err(PristineError::Serialization {
                message: "cannot anchor a bridge event capture whose capture token does not \
                          match the operation's minted capture context; retrospective \
                          promotion of an earlier advisory capture is refused"
                    .to_string(),
            });
        }
        let stored_digest = self.put_bridge_event_capture(bytes)?;
        debug_assert_eq!(stored_digest, digest);
        let mut anchors = self.txn.open_table(BRIDGE_EVENT_CAPTURE_ANCHORS)?;
        anchors.insert(&digest, operation.as_bytes())?;
        Ok(digest)
    }
}
