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
//! The content-modification form (`atomic revise` without `--reword`)
//! re-captures the working copy: the same unrecord/re-apply surgery, but
//! the replacement change is RECORDED from the working copy (the
//! client-side editor flow composes the message; the record itself is
//! the domain's).
//!
//! This module is the ONE code path for both flows: the CLI command and
//! the daemon's Revise RPC both call it (the shared-core pattern from
//! `provenance_core`).

use atomic_core::change::{Author, Change, ChangeHeader};
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

    /// Revise one recorded change by re-capturing the working copy into
    /// it: unrecord from the top of the current view down to (and
    /// including) the target, RECORD a replacement change from the
    /// working copy (the message and optional author the client composed
    /// — the editor flow is client-side by design), and re-apply the
    /// pending changes above the target in source order. A record failure
    /// rolls the stack back (everything unrecorded is re-applied, newest
    /// first) and surfaces the error.
    pub fn revise_content(
        &mut self,
        target: &Hash,
        message: &str,
        author: Option<Author>,
        paths: Vec<String>,
    ) -> Result<ReviseOutcome, crate::RepositoryError> {
        // Resolve the target's sequence and the pending changes above it.
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

        // Step 2: the new header — the composed message, the author
        // override or the original's first author (the same preservation
        // the CLI's content mode applies).
        let original = self.load_change(target)?;
        let author = author.or_else(|| original.hashed.header.authors.first().cloned());
        let mut header_builder = ChangeHeader::builder().message(message);
        if let Some(author) = author {
            header_builder = header_builder.author(author);
        }
        let header = header_builder.build();

        // Step 3: record the revised change from the working copy (the
        // record pipeline reads it — the daemon holds the repository
        // root's working tree).
        let mut options = crate::RecordOptions::default();
        if !paths.is_empty() {
            options = options.paths(paths);
        }
        let outcome = self.record(header, options).map_err(|error| {
            // Rollback: re-apply everything unrecorded, newest first, and
            // surface the error (never a silent partial stack).
            for hash in pending.iter().rev() {
                let _ = self.reinsert_change(hash, None);
            }
            match error {
                crate::record::RecordError::Repository(error) => error,
                other => crate::RepositoryError::InvalidOperation {
                    message: other.to_string(),
                },
            }
        })?;
        let new_hash = *outcome.hash();

        // Step 4: re-apply the pending changes in source order.
        let mut reinserted = Vec::with_capacity(pending.len());
        for hash in &pending {
            self.reinsert_change(hash, None)?;
            reinserted.push(*hash);
        }

        Ok(ReviseOutcome {
            new_hash,
            reinserted,
        })
    }
}
