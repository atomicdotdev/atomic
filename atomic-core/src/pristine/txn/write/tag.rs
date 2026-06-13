//! Tag trait implementations for WriteTxn.

use super::*;

use crate::pristine::tables::{GIT_SHA_INDEX, TAG_NAME_INDEX, TAG_RECORDS};
use crate::pristine::traits::tag::{
    GitShaIndexMutTxnT, GitShaIndexTxnT, TagMutTxnT, TagRecord, TagTxnT,
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
