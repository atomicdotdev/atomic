//! Revise immutable history through an isolated, verified replay. Both native
//! CLI and libatomic use this domain path. Working files remain untouched, and
//! original objects stay available to other views and recovery.

use atomic_core::change::Author;
use atomic_core::types::{Base32, Hash};

use crate::Repository;

/// The outcome of a reword.
#[derive(Debug, Clone)]
pub struct RewordOutcome {
    /// The new change hash (new header ⇒ new hash).
    pub new_hash: Hash,
    /// The changes re-applied above the target, in source order.
    pub reinserted: Vec<Hash>,
}

/// The outcome of a content-mode revise.
#[derive(Debug, Clone)]
pub struct ReviseOutcome {
    /// The newly recorded change's hash.
    pub new_hash: Hash,
    /// The changes re-applied above the target, in source order.
    pub reinserted: Vec<Hash>,
}

mod replay;

impl Repository {
    /// Reword one recorded change on the current view: same hunks, new
    /// header (message, optional author), signed with the default
    /// identity, with the pending changes above it re-applied.
    pub fn reword_change(
        &mut self,
        target: &Hash,
        message: &str,
        author: Option<Author>,
    ) -> Result<RewordOutcome, crate::RepositoryError> {
        let result = self.revise_stack(target, message, author, None)?;
        Ok(RewordOutcome {
            new_hash: result.new_hash,
            reinserted: result.reinserted,
        })
    }

    /// Replace a change with its recorded paths plus selected working-copy edits.
    /// Independent later changes retain their hashes and order. Dependent changes
    /// are re-recorded against the revised graph in an isolated replay first.
    /// On failure, restore the original membership through journaled operations
    /// without materializing over the user's working bytes.
    pub fn revise_content(
        &mut self,
        target: &Hash,
        message: &str,
        author: Option<Author>,
        paths: Vec<String>,
    ) -> Result<ReviseOutcome, crate::RepositoryError> {
        self.revise_stack(target, message, author, Some(&paths))
    }

    fn revise_stack(
        &mut self,
        target: &Hash,
        message: &str,
        author: Option<Author>,
        paths: Option<&[String]>,
    ) -> Result<ReviseOutcome, crate::RepositoryError> {
        use crate::{InsertOptions, RepositoryError, UnrecordOptions};

        let working_copy = self.require_working_copy_id()?;
        // Use the existing workspace lock hierarchy for the complete surgery,
        // including rollback. Nested domain operations reuse this outer lease.
        let _lease = self.try_lock_workspace_operation(working_copy)?;
        let view = self.desired_view_name(working_copy)?;
        let history = self.get_view_changes(Some(&view))?;
        let target_index = history
            .iter()
            .position(|(_, hash)| hash == target)
            .ok_or_else(|| RepositoryError::ChangeNotFound {
                hash: target.to_base32(),
            })?;
        let original = self.load_change(target)?;
        // Reinsertion keeps the existing publication gate. Check the complete
        // suffix before removing it, including the target needed on rollback.
        if self.get_view_info(&view)?.scope == atomic_core::pristine::ViewScope::Shared {
            let roots: Vec<_> = history[target_index..]
                .iter()
                .map(|(_, hash)| *hash)
                .collect();
            let closure = super::provenance_gate::reachable_closure(self, &roots)?;
            let provider = super::provenance_gate::local_session_mac_key_provider(self);
            self.enforce_publication_gate("revise reinsertion", &closure, Some(&provider))?;
        }

        let mut header = original.hashed.header.clone();
        header.message = message.to_string();
        if let Some(author) = author {
            header.authors = vec![author];
        }
        let plan = replay::plan(self, &view, &history[target_index..], header, paths)?;
        // Persist only fully verified immutable replacement objects before any
        // source membership changes. Import through the normal graph API.
        for hash in &plan.hashes {
            if !self.has_change(hash) {
                let change = plan.repo.load_change(hash)?;
                let bytes = std::fs::read(plan.repo.change_store.change_path(hash))?;
                self.save_change_bytes(hash, &bytes, &change)?;
            }
        }
        if self.get_view_info(&view)?.scope == atomic_core::pristine::ViewScope::Shared {
            let closure = super::provenance_gate::reachable_closure(self, &plan.hashes)?;
            let provider = super::provenance_gate::local_session_mac_key_provider(self);
            self.enforce_publication_gate("revised history", &closure, Some(&provider))?;
        }
        let result = (|| {
            for (_, hash) in history[target_index..].iter().rev() {
                self.unrecord(hash, UnrecordOptions::default().view(&view))?;
            }
            for hash in &plan.hashes {
                self.insert_change(hash, InsertOptions::default().view(&view))?;
            }
            plan.verify(self, &view)?;
            Ok(ReviseOutcome {
                new_hash: plan.hashes[0],
                reinserted: plan.hashes[1..].to_vec(),
            })
        })();
        if let Err(error) = result {
            if let Err(rollback) = self.restore_revise_history(&view, &history) {
                return Err(RepositoryError::InvalidOperation {
                    message: format!("revise failed: {error}; history rollback failed: {rollback}"),
                });
            }
            return Err(error);
        }
        result
    }

    fn restore_revise_history(
        &self,
        view: &str,
        original: &[(u64, Hash)],
    ) -> Result<(), crate::RepositoryError> {
        let current = self.get_view_changes(Some(view))?;
        let common = current
            .iter()
            .zip(original)
            .take_while(|(a, b)| a == b)
            .count();
        for (_, hash) in current[common..].iter().rev() {
            self.unrecord(hash, crate::UnrecordOptions::default().view(view))?;
        }
        for (_, hash) in &original[common..] {
            self.insert_change(hash, crate::InsertOptions::default().view(view))?;
        }
        Ok(())
    }
}
