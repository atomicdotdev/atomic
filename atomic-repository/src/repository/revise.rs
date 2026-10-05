//! Revise: reword a recorded change in place on the view stack.
//!
//! The reword form of `atomic revise` is a stack surgery: unrecord from
//! the top of the current view down to (and including) the target,
//! rebuild the change with a new header (message, optional author) while
//! keeping the EXACT original hunks/file-ops/contents/deps, sign it with
//! the default identity (reword bypasses the record pipeline, so the
//! signature is the revised change's provenance), insert it, and re-apply
//! the pending changes above the target in source order.
//!
//! This module is the ONE code path for the reword flow: the CLI command
//! and the daemon's Revise RPC both call it (the shared-core pattern from
//! `provenance_core`). The content-modification form of revise —
//! re-capturing the working copy through an interactive editor — is
//! client-side orchestration and stays out of the domain.

use atomic_core::change::{Author, Change};
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
        // Resolve the target's sequence and the pending changes above it
        // on the current view.
        let history = self.log(crate::HistoryOptions::default())?;
        let Some(entry) = history.iter().find(|e| &e.hash == target) else {
            return Err(crate::RepositoryError::ChangeNotFound {
                hash: target.to_base32(),
            });
        };
        let sequence = entry.sequence;

        let stack_info = self.get_view_info(&self.current_view)?;
        let changes_to_unrecord = stack_info.change_count as usize - sequence as usize;

        // Pending changes (above the target), oldest first for re-apply.
        let mut pending: Vec<(u64, Hash)> = history
            .into_iter()
            .filter(|e| e.sequence > sequence)
            .map(|e| (e.sequence, e.hash))
            .collect();
        pending.sort_by_key(|(sequence, _)| *sequence);
        let pending = pending
            .into_iter()
            .map(|(_, hash)| hash)
            .collect::<Vec<_>>();

        // Step 1: unrecord from the top down to (and including) the target.
        for _ in 0..changes_to_unrecord {
            self.unrecord_last(crate::UnrecordOptions::default())?;
        }

        // Step 2: rebuild the change — same hunks/file-ops/contents/deps,
        // new header.
        let original = self.load_change(target)?;
        let mut new_header = original.hashed.header.clone();
        new_header.message = message.to_string();
        if let Some(author) = author {
            new_header.authors = vec![author];
        }
        let mut new_change = Change::with_file_ops(
            new_header,
            original.hashed.hunks.clone(),
            original.hashed.file_ops.clone(),
            original.contents.clone(),
            original.hashed.dependencies.clone(),
        );

        // Sign with the default identity — the CLI's exact block: a
        // revised change is signed like any new record.
        if let Ok(store) = atomic_identity::IdentityStore::open_default() {
            let signing_identity = store.get_default().ok().flatten();
            if let Some(identity) = signing_identity {
                if let Ok(keypair) = store.load_keypair(&identity.id, None) {
                    let _ = new_change.sign_with(
                        &atomic_canonical::did::did_for_public_key(&identity.public_key),
                        keypair.secret.as_bytes(),
                        chrono::Utc::now().timestamp(),
                    );
                }
            }
        }

        // Step 3: save and insert the reworded change.
        let new_hash = match self
            .save_change(&new_change)
            .and_then(|hash| self.insert_change(&hash, Default::default()).map(|_| hash))
        {
            Ok(hash) => hash,
            Err(error) => {
                // Rollback: re-apply everything unrecorded, newest first,
                // and surface the error (never a silent partial stack).
                for hash in pending.iter().rev() {
                    let _ = self.reinsert_change(hash, None);
                }
                return Err(error);
            }
        };

        // Step 4: re-apply the pending changes in source order.
        let mut reinserted = Vec::with_capacity(pending.len());
        for hash in &pending {
            match self.reinsert_change(hash, None) {
                Ok(_) => reinserted.push(*hash),
                Err(error) => {
                    // Surface the failure; the stack below is intact.
                    return Err(error);
                }
            }
        }

        Ok(RewordOutcome {
            new_hash,
            reinserted,
        })
    }
}
