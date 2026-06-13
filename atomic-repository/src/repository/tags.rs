//! Tag operations on Repository — backed by redb TAG_RECORDS.

use atomic_core::pristine::{
    GraphTxnT, MutTxnT, TagKind, TagMutTxnT, TagRecord, TagTxnT, ViewTxnT,
};
use atomic_core::types::Merkle;
use chrono::Utc;

use crate::error::RepositoryError;
use crate::repository::Repository;

/// Serialize a [`TagRecord`] for network transfer.
pub fn serialize_tag(tag: &TagRecord) -> Result<Vec<u8>, RepositoryError> {
    postcard::to_allocvec(tag)
        .map_err(|e| RepositoryError::Serialization(format!("Tag serialization: {}", e)))
}

/// Deserialize a [`TagRecord`] received from a remote.
pub fn deserialize_tag(bytes: &[u8]) -> Result<TagRecord, RepositoryError> {
    postcard::from_bytes(bytes)
        .map_err(|e| RepositoryError::Serialization(format!("Tag deserialization: {}", e)))
}

impl Repository {
    /// Create a named tag on the current view at the current sequence.
    pub fn create_tag(
        &self,
        name: &str,
        message: Option<&str>,
        kind: TagKind,
    ) -> Result<TagRecord, RepositoryError> {
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let view = txn
            .get_view(&self.current_view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: self.current_view.clone(),
            })?;

        // Get the hash of the change at the current sequence
        let change_hash = if view.change_count > 0 {
            let seq = view.change_count - 1;
            let change_id = txn
                .get_change_at_seq(&view, seq)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .ok_or_else(|| {
                    RepositoryError::Database(format!("No change at sequence {}", seq))
                })?;
            txn.get_external(change_id)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .unwrap_or(Merkle::ZERO)
        } else {
            Merkle::ZERO
        };

        let tag = TagRecord {
            name: name.to_string(),
            view: self.current_view.clone(),
            sequence: view.change_count.saturating_sub(1),
            state: view.state,
            change_hash,
            timestamp: Utc::now(),
            author: None,
            message: message.map(|s| s.to_string()),
            kind,
            metadata: None,
        };

        txn.put_tag(&tag)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(tag)
    }

    /// Create a named tag with optional metadata on the current view.
    ///
    /// This is the full-featured variant of [`create_tag`](Self::create_tag)
    /// that allows attaching extensible JSON metadata (Git provenance, CI
    /// status, review approvals, etc.).
    pub fn create_tag_with_metadata(
        &self,
        name: &str,
        message: Option<&str>,
        kind: TagKind,
        metadata: Option<serde_json::Value>,
    ) -> Result<TagRecord, RepositoryError> {
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let view = txn
            .get_view(&self.current_view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: self.current_view.clone(),
            })?;

        // Get the hash of the change at the current sequence
        let change_hash = if view.change_count > 0 {
            let seq = view.change_count - 1;
            let change_id = txn
                .get_change_at_seq(&view, seq)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .ok_or_else(|| {
                    RepositoryError::Database(format!("No change at sequence {}", seq))
                })?;
            txn.get_external(change_id)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .unwrap_or(Merkle::ZERO)
        } else {
            Merkle::ZERO
        };

        let tag = TagRecord {
            name: name.to_string(),
            view: self.current_view.clone(),
            sequence: view.change_count.saturating_sub(1),
            state: view.state,
            change_hash,
            timestamp: Utc::now(),
            author: None,
            message: message.map(|s| s.to_string()),
            kind,
            metadata,
        };

        txn.put_tag(&tag)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(tag)
    }

    /// Save a tag received from a remote.
    ///
    /// Unlike [`create_tag`](Self::create_tag), this preserves all fields from
    /// the incoming [`TagRecord`] (including sequence, state, timestamp). Used
    /// by `pull` to replicate tags from a remote.
    pub fn save_synced_tag(&self, tag: &TagRecord) -> Result<(), RepositoryError> {
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        txn.put_tag(tag)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(())
    }

    /// Look up a tag by name in the current view.
    pub fn get_tag(&self, name: &str) -> Result<Option<TagRecord>, RepositoryError> {
        self.get_tag_from_view(name, &self.current_view)
    }

    /// Look up a tag by name in a specific view.
    pub fn get_tag_from_view(
        &self,
        name: &str,
        view: &str,
    ) -> Result<Option<TagRecord>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.get_tag(view, name)
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Look up a tag by name across all views.
    pub fn get_tag_any_view(&self, name: &str) -> Result<Option<TagRecord>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let all = txn
            .list_all_tags()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(all.into_iter().find(|t| t.name == name))
    }

    /// List tags in the current view.
    pub fn list_tags(&self) -> Result<Vec<TagRecord>, RepositoryError> {
        self.list_tags_for_view(&self.current_view)
    }

    /// List tags in a specific view.
    pub fn list_tags_for_view(&self, view: &str) -> Result<Vec<TagRecord>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.list_tags(view)
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// List all tags across all views.
    pub fn list_all_tags(&self) -> Result<Vec<TagRecord>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.list_all_tags()
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// List view names that have tags.
    pub fn list_tag_views(&self) -> Result<Vec<String>, RepositoryError> {
        let tags = self.list_all_tags()?;
        let mut views: Vec<String> = tags.iter().map(|t| t.view.clone()).collect();
        views.sort();
        views.dedup();
        Ok(views)
    }

    /// Delete a tag by name from the current view.
    pub fn delete_tag(&self, name: &str) -> Result<bool, RepositoryError> {
        self.delete_tag_from_view(name, &self.current_view)
    }

    /// Delete a tag by name from a specific view.
    pub fn delete_tag_from_view(&self, name: &str, view: &str) -> Result<bool, RepositoryError> {
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let deleted = txn
            .del_tag(view, name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(deleted)
    }

    /// Count tags in the current view.
    pub fn tag_count(&self) -> Result<usize, RepositoryError> {
        Ok(self.list_tags()?.len())
    }

    /// Count tags in a specific view.
    pub fn tag_count_for_view(&self, view: &str) -> Result<usize, RepositoryError> {
        Ok(self.list_tags_for_view(view)?.len())
    }

    /// Count all tags across all views.
    pub fn tag_count_all(&self) -> Result<usize, RepositoryError> {
        Ok(self.list_all_tags()?.len())
    }
}
