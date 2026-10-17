//! Tag operations on Repository — backed by redb TAG_RECORDS.

use atomic_core::operation::{
    ActorRef, MetadataTarget, MetadataTransition, MetadataValue, OperationKind, OperationScope,
    RepoStateRef, ViewStateRef,
};
use atomic_core::pristine::{GraphTxnT, TagKind, TagRecord, TagTxnT, ViewTxnT, WorkingCopyTxnT};
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
        self.create_tag_with_metadata(name, message, kind, None)
    }

    /// Create a named tag on an explicit view.
    pub fn create_tag_on_view(
        &self,
        view: &str,
        name: &str,
        message: Option<&str>,
        kind: TagKind,
    ) -> Result<TagRecord, RepositoryError> {
        self.create_tag_with_metadata_on_view(view, name, message, kind, None)
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
        let view = self.current_view.clone();
        self.create_tag_with_metadata_on_view(&view, name, message, kind, metadata)
    }

    /// Create a named tag with optional metadata on an explicit view.
    pub fn create_tag_with_metadata_on_view(
        &self,
        view: &str,
        name: &str,
        message: Option<&str>,
        kind: TagKind,
        metadata: Option<serde_json::Value>,
    ) -> Result<TagRecord, RepositoryError> {
        let tag = TagRecord {
            name: name.to_string(),
            view: view.to_string(),
            sequence: 0,
            state: Merkle::ZERO,
            change_hash: Merkle::ZERO,
            timestamp: Utc::now(),
            author: None,
            message: message.map(|s| s.to_string()),
            kind,
            metadata,
        };

        self.apply_tag_transition(&tag.view, &tag.name, Some(&tag), true)?;
        self.get_tag_from_view(&tag.name, &tag.view)?
            .ok_or_else(|| RepositoryError::TagNotFound {
                name: tag.name.clone(),
            })
    }

    /// Save a tag received from a remote.
    ///
    /// Unlike [`create_tag`](Self::create_tag), this preserves all fields from
    /// the incoming [`TagRecord`] (including sequence, state, timestamp). Used
    /// by `pull` to replicate tags from a remote.
    pub fn save_synced_tag(&self, tag: &TagRecord) -> Result<(), RepositoryError> {
        self.apply_tag_transition(&tag.view, &tag.name, Some(tag), false)?;
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
        self.apply_tag_transition(view, name, None, false)
    }

    fn apply_tag_transition(
        &self,
        view: &str,
        name: &str,
        replacement: Option<&TagRecord>,
        normalize_to_view_head: bool,
    ) -> Result<bool, RepositoryError> {
        let working_copy = self.require_working_copy_id()?;
        let operation_lock = self.try_lock_operation(working_copy)?;
        if let super::operation::OperationHeadState::Diverged(heads) =
            self.consolidate_operation_heads_locked(&operation_lock)?
        {
            return Err(RepositoryError::OperationHeadsDiverged {
                scope: OperationScope::WorkingCopy(working_copy).to_string(),
                heads: heads.iter().map(ToString::to_string).collect(),
            });
        }
        let mut replacement = replacement.cloned();
        let (state, view_sequence, view_state, view_change_hash) = {
            let txn = self
                .pristine
                .read_txn()
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
            let view_state = txn
                .get_view(view)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| RepositoryError::ViewNotFound {
                    name: view.to_string(),
                })?;
            let working_copy_state = super::operation::working_copy_state_ref(
                txn.get_working_copy(working_copy)
                    .map_err(|error| RepositoryError::Database(error.to_string()))?
                    .ok_or(RepositoryError::WorkingCopyRecordNotFound { id: working_copy })?,
            );
            let change_hash = if view_state.change_count > 0 {
                let sequence = view_state.change_count - 1;
                let change_id = txn
                    .get_change_at_seq(&view_state, sequence)
                    .map_err(|error| RepositoryError::Database(error.to_string()))?
                    .ok_or_else(|| {
                        RepositoryError::Database(format!("No change at sequence {sequence}"))
                    })?;
                txn.get_external(change_id)
                    .map_err(|error| RepositoryError::Database(error.to_string()))?
                    .unwrap_or(Merkle::ZERO)
            } else {
                Merkle::ZERO
            };
            (
                RepoStateRef {
                    view: Some(ViewStateRef {
                        name: view.to_string(),
                        state: view_state.state,
                        set_id: None,
                    }),
                    working_copy: Some(working_copy_state),
                    git: None,
                },
                view_state.change_count.saturating_sub(1),
                view_state.state,
                change_hash,
            )
        };
        if normalize_to_view_head {
            if let Some(tag) = &mut replacement {
                tag.view = view.to_string();
                tag.name = name.to_string();
                tag.sequence = view_sequence;
                tag.state = view_state;
                tag.change_hash = view_change_hash;
            }
        }
        let existing = self.get_tag_from_view(name, view)?;
        if existing == replacement {
            return Ok(existing.is_some());
        }
        let old_value = existing
            .as_ref()
            .map(serialize_tag)
            .transpose()?
            .map(MetadataValue::Bytes)
            .unwrap_or(MetadataValue::Absent);
        let new_value = replacement
            .as_ref()
            .map(serialize_tag)
            .transpose()?
            .map(MetadataValue::Bytes)
            .unwrap_or(MetadataValue::Absent);
        let evidence = replacement
            .as_ref()
            .map(TagRecord::content_hash)
            .or_else(|| existing.as_ref().map(TagRecord::content_hash))
            .into_iter()
            .collect();
        let operation = self.prepare_metadata_operation(
            &operation_lock,
            OperationKind::Tag,
            None,
            state.clone(),
            state,
            vec![MetadataTransition {
                target: MetadataTarget::Tag {
                    view: view.to_string(),
                    name: name.to_string(),
                },
                expected_old: old_value,
                expected_new: new_value,
            }],
            evidence,
            ActorRef::System {
                name: "repository-tag".to_string(),
            },
            super::operation::current_operation_timestamp_ms(),
        )?;
        self.apply_operation_metadata_locked(&operation_lock, operation.id())?;
        self.finalize_operation_verified(&operation_lock, operation.id())?;
        Ok(existing.is_some())
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
