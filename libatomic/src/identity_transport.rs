//! Project-identity transport orchestration: adoption after clone/pull,
//! binding before push.
//!
//! The wire surface lives in [`atomic_remote::HttpRemote`]
//! (`fetch_project_identity`/`bind_project_identity`) and the local write
//! surface in [`atomic_repository::Repository::adopt_remote_project_identity`].
//! This module is the glue the CLI and the daemon share so both transports
//! behave identically:
//!
//! - **clone/pull** adopt the remote's identity: none/`Upgrade` locally → the
//!   remote identity takes over (a replaced `Upgrade` mint invalidates the
//!   derived KG, which is rebuilt here); an established local identity that
//!   differs → reported as divergence, never overwritten.
//! - **push** binds first: an unbound remote adopts the local identity; a
//!   remote bound to a *different* identity refuses the push before any
//!   objects move.
//!
//! Remotes without the identity endpoints (older storage) degrade to the
//! Phase-1 behavior: keep whatever was minted locally.

use std::fmt;

use atomic_core::kg::project_id::ProjectIdentityRecord;
use atomic_remote::http::identity::{BindOutcome, IdentityFetch};
use atomic_remote::HttpRemote;
use atomic_repository::{Repository, RepositoryError};

/// What a fetch-and-reconcile pass decided.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// The project id (base32) adopted from the remote, if any.
    pub adopted: Option<String>,
    /// Whether a locally-minted `Upgrade` identity was replaced (the derived
    /// KG was rebuilt as part of adoption).
    pub replaced_upgrade: bool,
    /// `(remote project id, local project id)` when the remote identity
    /// differs from an established local one.
    pub diverged: Option<(String, String)>,
}

impl fmt::Display for ReconcileReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some((remote, local)) = &self.diverged {
            write!(
                f,
                "remote project identity {remote} differs from local {local}; local identity kept"
            )
        } else if let Some(adopted) = &self.adopted {
            if self.replaced_upgrade {
                write!(
                    f,
                    "Adopted project identity {adopted} (replaced local placeholder)"
                )
            } else {
                write!(f, "Adopted project identity {adopted}")
            }
        } else {
            write!(f, "project identity unchanged")
        }
    }
}

/// Build a transport record from a fetched identity document: recompute the
/// canonical hash, mark it `Adopted` (it was never minted locally).
fn record_from_document(document: &serde_json::Value) -> Result<ProjectIdentityRecord, String> {
    let object = serde_json::to_vec(document).map_err(|e| e.to_string())?;
    let hash = atomic_repository::project_identity_hash(document);
    let created_at = document
        .get("dcterms:created")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    Ok(ProjectIdentityRecord {
        object,
        hash,
        mint_reason: atomic_core::kg::project_id::MintReason::Adopted,
        created_at,
        previous: None,
    })
}

/// Fetch the remote's project identity and reconcile it into the local
/// repository. Call after a successful clone/pull fetch.
pub async fn reconcile_after_fetch(
    repo: &Repository,
    remote: &HttpRemote,
) -> Result<ReconcileReport, RepositoryError> {
    let fetched = fetch_identity(remote).await?;
    match &fetched {
        None => Ok(ReconcileReport::default()),
        Some(document) => apply_fetched_identity(repo, document),
    }
}

/// Fetch-only half of [`reconcile_after_fetch`], for transports that must keep
/// repository writes off the async context (the daemon runs them inside
/// `spawn_blocking`). `None` = nothing to adopt.
pub async fn fetch_identity(
    remote: &HttpRemote,
) -> Result<Option<serde_json::Value>, RepositoryError> {
    let fetched = match remote.fetch_project_identity().await {
        Ok(fetched) => fetched,
        // Transport trouble is not a fetch failure: identity adoption is
        // additive and must never fail the pull that just succeeded.
        Err(error) => {
            tracing::info!("project-identity fetch skipped: {error}");
            return Ok(None);
        }
    };
    match fetched {
        IdentityFetch::Bound(document) => Ok(Some(document)),
        IdentityFetch::Unbound | IdentityFetch::Unsupported => Ok(None),
    }
}

/// Repo-write half of [`reconcile_after_fetch`]: apply a fetched identity
/// document to the local repository. Skip entirely when nothing was fetched.
pub fn apply_fetched_identity(
    repo: &Repository,
    document: &serde_json::Value,
) -> Result<ReconcileReport, RepositoryError> {
    let mut report = ReconcileReport::default();
    let record = match record_from_document(document) {
        Ok(record) => record,
        Err(error) => {
            tracing::info!("project-identity adoption skipped: {error}");
            return Ok(report);
        }
    };
    match repo.adopt_remote_project_identity(&record)? {
        atomic_repository::IdentityAdoption::Adopted => {
            report.adopted = Some(
                record
                    .project_id()
                    .map_err(|e| RepositoryError::InvalidOperation {
                        message: e.to_string(),
                    })?
                    .to_base32(),
            );
        }
        atomic_repository::IdentityAdoption::ReplacedUpgrade => {
            report.adopted = Some(
                record
                    .project_id()
                    .map_err(|e| RepositoryError::InvalidOperation {
                        message: e.to_string(),
                    })?
                    .to_base32(),
            );
            report.replaced_upgrade = true;
            // The adopted id re-keys every project-qualified IRI: rebuild the
            // derived graph before anything reads it.
            repo.rebuild_kg(false)?;
        }
        atomic_repository::IdentityAdoption::AlreadyCurrent => {}
        atomic_repository::IdentityAdoption::Diverged => {
            let remote_id = record
                .project_id()
                .map_err(|e| RepositoryError::InvalidOperation {
                    message: e.to_string(),
                })?;
            let local_id = repo
                .project_id()
                .map_err(|e| RepositoryError::InvalidOperation {
                    message: e.to_string(),
                })?;
            report.diverged = Some((remote_id.to_base32(), local_id.to_base32()));
        }
    }
    Ok(report)
}

/// The outcome of a pre-push bind attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushBind {
    /// The remote now carries (or already carried) the local identity.
    Bound(String),
    /// The remote predates identity transport; pushed unbound.
    Unsupported,
}

/// Bind the local identity to the remote before a push, refusing a push that
/// would land on a project bound to a *different* identity. Pass the local
/// record when the repository has one; `None` (no local identity) is a no-op.
pub async fn bind_before_push(
    local: &ProjectIdentityRecord,
    remote: &HttpRemote,
) -> Result<PushBind, RepositoryError> {
    let document: serde_json::Value =
        serde_json::from_slice(&local.object).map_err(|e| RepositoryError::InvalidOperation {
            message: format!("local identity is not valid JSON: {e}"),
        })?;
    let local_id = local
        .project_id()
        .map_err(|e| RepositoryError::InvalidOperation {
            message: e.to_string(),
        })?
        .to_base32();

    match remote
        .bind_project_identity(&document)
        .await
        .map_err(|error| RepositoryError::InvalidOperation {
            message: format!("project-identity bind failed: {error}"),
        })? {
        BindOutcome::Bound => Ok(PushBind::Bound(local_id)),
        BindOutcome::Unsupported => Ok(PushBind::Unsupported),
        BindOutcome::Conflict { remote: remote_doc } => {
            let remote_id = remote_doc
                .as_ref()
                .and_then(|doc| doc.get("atomic:projectId"))
                .and_then(|v| v.as_str())
                .unwrap_or("<unreadable>")
                .to_string();
            Err(RepositoryError::InvalidOperation {
                message: format!(
                    "remote project identity is {remote_id}; local is {local_id}. \
                     These are different projects — run 'atomic repo identity adopt' \
                     to take the remote lineage, or 'atomic repo identity fork' to \
                     start a new project, then push accordingly."
                ),
            })
        }
    }
}

/// Explicit divergence remediation (`atomic repo identity adopt`): fetch the
/// remote's binding and unconditionally replace the local identity, then
/// rebuild the derived KG under the adopted id. Errors when the remote has no
/// binding (or predates the transport) — there is nothing to adopt.
pub async fn adopt_from_remote(
    repo: &Repository,
    remote: &HttpRemote,
) -> Result<String, RepositoryError> {
    let Some(document) = fetch_identity(remote).await? else {
        return Err(RepositoryError::InvalidOperation {
            message: "remote has no bound project identity to adopt".into(),
        });
    };
    let record = record_from_document(&document)
        .map_err(|e| RepositoryError::InvalidOperation { message: e })?;
    let adopted = repo.force_adopt_remote_identity(&record)?;
    let project_id = adopted
        .project_id()
        .map_err(|e| RepositoryError::InvalidOperation {
            message: e.to_string(),
        })?
        .to_base32();
    // The adopted id re-keys every project-qualified IRI.
    repo.rebuild_kg(false)?;
    Ok(project_id)
}
