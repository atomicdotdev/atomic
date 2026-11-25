//! Native finalization owned by the review command after its child process exits.

use std::path::Path;

use atomic_agent::event::{HookType, TurnEvent};
use atomic_agent::turn::orchestrator::{ManagedRunContext, TurnOrchestrator};
use atomic_agent::turn::session::SessionStore;
use atomic_core::change::session::SessionStatus;
use atomic_core::types::Hash;
use atomic_repository::Repository;

use crate::error::{CliError, CliResult};

pub(crate) struct PublisherCompletion {
    pub manifest: Hash,
    pub attestations: Vec<Hash>,
}

fn refused(message: impl Into<String>) -> CliError {
    CliError::InvalidArgument {
        message: message.into(),
    }
}

/// This is not a generic "mark ended" API. The caller must own and have waited
/// for the successful publisher process. Its hook-written run stamp and recorded
/// turns are checked before the real session-end state machine is dispatched.
pub(crate) fn finish_publisher_session(
    root: &Path,
    session_id: &str,
    run_id: &str,
    job_id: &str,
    identity: &str,
    view: &str,
) -> CliResult<PublisherCompletion> {
    let repo = Repository::open(root).map_err(CliError::Repository)?;
    let store =
        SessionStore::new(repo.dot_dir().join("sessions")).map_err(|e| refused(e.to_string()))?;
    let session = store
        .load(session_id)
        .map_err(|e| refused(e.to_string()))?
        .ok_or_else(|| {
            refused("publisher produced no native hooked session; no completion can be asserted")
        })?;
    let stamp = session
        .managed_run
        .clone()
        .ok_or_else(|| refused("publisher session has no managed-run binding"))?;
    if stamp.run_id != run_id
        || stamp.owner_agent != "atomic-review-publish"
        || stamp.owner_session_id != job_id
        || session.agent_name != "opencode"
        || session.view_name != view
    {
        return Err(refused(
            "publisher session/run/view binding does not match the owned process",
        ));
    }
    if session.incomplete().is_some() {
        return Err(refused(
            "publisher execution is incomplete; preserve it for recovery",
        ));
    }
    drop(repo);
    let rt = tokio::runtime::Runtime::new().map_err(|e| refused(e.to_string()))?;
    rt.block_on(async {
        let mut orchestrator = TurnOrchestrator::new(root)
            .await
            .map_err(|e| refused(e.to_string()))?;
        orchestrator.set_agent("opencode", "OpenCode");
        orchestrator.set_agent_identity(Some(identity.to_string()));
        orchestrator.set_managed_run(ManagedRunContext {
            stamp,
            view: Some(view.to_string()),
        });
        orchestrator.set_journal_sink(super::provenance_rpc::sink_for(root));
        let result = orchestrator
            .dispatch(TurnEvent::new(session_id, HookType::SessionEnd))
            .await
            .map_err(|e| refused(format!("publisher session_end failed: {e}")))?;
        if result.incomplete.is_some() {
            return Err(refused(
                "publisher session_end retained incomplete execution; publication refused",
            ));
        }
        Ok(())
    })?;
    let repo = Repository::open(root).map_err(CliError::Repository)?;
    let session = store
        .load(session_id)
        .map_err(|e| refused(e.to_string()))?
        .ok_or_else(|| refused("publisher runtime completion disappeared"))?;
    if session.turn_count == 0
        || session.recorded_change_hashes.is_empty()
        || session.is_turn_active()
    {
        return Err(refused(
            "publisher session_end did not flush nonempty completed review evidence",
        ));
    }
    let (record, _) = repo
        .get_session_ledger(session_id)
        .map_err(CliError::Repository)?
        .ok_or_else(|| refused("publisher completion is missing from the native ledger"))?;
    if record.status != SessionStatus::Ended || record.ended_at.is_none() {
        return Err(refused(
            "publisher session_end did not produce a completed native session",
        ));
    }
    let manifest = repo
        .get_session_head(session_id)
        .map_err(CliError::Repository)?
        .ok_or_else(|| refused("publisher session_end produced no immutable session manifest"))?;
    let identities = atomic_identity::IdentityStore::open_default_existing()
        .map_err(|e| refused(e.to_string()))?;
    let signer = identities
        .load_by_name(identity)
        .map_err(|e| refused(e.to_string()))?;
    let did = atomic_canonical::did::did_for_public_key(&signer.public_key);
    let mut attestations = std::collections::BTreeSet::new();
    for change in &session.recorded_change_hashes {
        let covering = repo
            .find_attestations_for_change(change)
            .map_err(CliError::Repository)?;
        let mut verified = false;
        for (hash, attestation) in covering {
            if attestation.session_id == session_id {
                if attestation.signer_did() != Some(did.as_str())
                    || !attestation.verify_with_did(signer.public_key.as_bytes())
                    || !attestation.changes_covered.contains(change)
                {
                    return Err(refused("publisher has invalid or differently signed same-session attestation evidence"));
                }
                verified = true;
                attestations.insert(hash);
            }
        }
        if !verified {
            return Err(refused("publisher session_end lacks native review-key-signed coverage; completion is not publishable"));
        }
    }
    Ok(PublisherCompletion {
        manifest,
        attestations: attestations.into_iter().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_agent::turn::session::{AgentSession, ManagedRunStamp};

    #[test]
    fn absent_native_hook_evidence_cannot_be_finalized() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        assert!(
            finish_publisher_session(dir.path(), "missing", "run", "job", "reviewer", "dev")
                .is_err()
        );
        assert!(repo.get_session_ledger("missing").unwrap().is_none());
    }

    #[test]
    fn another_managed_job_cannot_be_relabelled_as_this_publishers_completion() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let store = SessionStore::new(repo.dot_dir().join("sessions")).unwrap();
        let mut session = AgentSession::new("publisher", "opencode", "OpenCode");
        session.view_name = "dev".into();
        session.managed_run = Some(ManagedRunStamp {
            run_id: "run".into(),
            owner_agent: "atomic-review-publish".into(),
            owner_session_id: "different-job".into(),
            work_item_id: None,
        });
        store.save(&session).unwrap();
        assert!(
            finish_publisher_session(dir.path(), "publisher", "run", "job", "reviewer", "dev")
                .is_err()
        );
        assert!(!store.load("publisher").unwrap().unwrap().is_ended());
    }
}
