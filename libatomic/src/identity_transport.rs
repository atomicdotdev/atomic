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
use atomic_remote::http::identity::{BindOutcome, BindServerError, ConflictRemote, IdentityFetch};
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

/// Render the operator-facing refusal for a `409` bind outcome. Pure so the
/// advice can be unit-tested without any transport.
///
/// The advice depends on what the follow-up read of the remote binding found:
/// a genuinely different project keeps the adopt/fork choice, a re-minted
/// document of the same project only leaves `adopt` (fork would mint a third
/// lineage that conflicts the same way), and a server that contradicts its own
/// `409` by reporting the project unbound points at the storage bug instead.
/// The server's own words are quoted whenever it sent any.
fn format_bind_conflict(
    local_id: &str,
    remote: &ConflictRemote,
    server: Option<&BindServerError>,
) -> String {
    let note = server_note(server);
    match remote {
        ConflictRemote::Bound(document) => {
            let remote_id = document
                .get("atomic:projectId")
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            match remote_id {
                Some(remote_id) if remote_id != local_id => format!(
                    "remote project identity is {remote_id}; local is {local_id}. \
                     These are different projects — run 'atomic repo identity adopt' \
                     to take the remote lineage, or 'atomic repo identity fork' to \
                     start a new project, then push accordingly.{note}"
                ),
                Some(remote_id) => format!(
                    "the remote is bound to a different document of the same project \
                     ({remote_id}) — the bound document was likely re-minted. \
                     Run 'atomic repo identity adopt' to take the remote lineage, \
                     then push; 'fork' would mint a third lineage that conflicts \
                     the same way.{note}"
                ),
                None => format!(
                    "the remote is bound to a project-identity document without a \
                     readable atomic:projectId, so the divergence cannot be named. \
                     Run 'atomic repo identity adopt' to take the remote lineage \
                     regardless, then push.{note}"
                ),
            }
        }
        ConflictRemote::Unbound => format!(
            "the server refused the project-identity bind with a conflict but then \
             reports the project unbound — this contradicts itself and indicates a \
             server-side project-identity ref scoping bug (fixed in \
             atomic-storage#113). Local project identity is {local_id}. Upgrade the \
             storage server and retry the push.{note}"
        ),
        ConflictRemote::Unknown => format!(
            "the push was refused because the remote is already bound to a different \
             project identity, and the remote binding could not be read to name the \
             divergence. Local project identity is {local_id}.{note}"
        ),
    }
}

/// The server's own words from the `409` body, quoted verbatim; empty when the
/// server sent no readable reason.
fn server_note(server: Option<&BindServerError>) -> String {
    let Some(server) = server else {
        return String::new();
    };
    match (&server.code, &server.message) {
        (Some(code), Some(message)) => format!(" (server code {code}: {message})"),
        (Some(code), None) => format!(" (server code {code})"),
        (None, Some(message)) => format!(" (server message: {message})"),
        (None, None) => String::new(),
    }
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
        BindOutcome::Conflict { remote, server } => Err(RepositoryError::InvalidOperation {
            message: format_bind_conflict(&local_id, &remote, server.as_ref()),
        }),
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn local_id() -> String {
        "A".repeat(52)
    }

    fn bound_document(project_id: &str) -> ConflictRemote {
        ConflictRemote::Bound(json!({
            "@context": {"atomic": "https://atomic.dev/ns/workflow#"},
            "@type": "atomic:ProjectIdentity",
            "atomic:projectId": project_id,
        }))
    }

    fn server_error(code: &str, message: &str) -> BindServerError {
        BindServerError {
            code: Some(code.to_owned()),
            message: Some(message.to_owned()),
        }
    }

    #[test]
    fn a_genuinely_different_project_keeps_the_adopt_fork_advice() {
        let remote_id = "B".repeat(52);
        let message = format_bind_conflict(
            &local_id(),
            &bound_document(&remote_id),
            Some(&server_error("IDENTITY_CONFLICT", "project already bound")),
        );
        assert!(
            message.contains(&remote_id) && message.contains(&local_id()),
            "both ids must be named: {message}"
        );
        assert!(
            message.contains("adopt") && message.contains("fork"),
            "genuine divergence keeps both remediations: {message}"
        );
        assert!(
            message.contains("project already bound"),
            "the server's own words are quoted: {message}"
        );
    }

    #[test]
    fn a_re_minted_document_of_the_same_project_advises_adopt_only() {
        let message = format_bind_conflict(
            &local_id(),
            &bound_document(&local_id()),
            Some(&server_error("IDENTITY_CONFLICT", "project already bound")),
        );
        assert!(
            message.contains("same project"),
            "the same-project case is named: {message}"
        );
        assert!(
            message.contains("adopt"),
            "adopt is the remediation: {message}"
        );
        assert!(
            !message.contains("'atomic repo identity fork'"),
            "fork would conflict the same way and must not be advised: {message}"
        );
    }

    #[test]
    fn a_bound_document_without_a_readable_project_id_says_so() {
        let message = format_bind_conflict(
            &local_id(),
            &ConflictRemote::Bound(json!({"@type": "atomic:ProjectIdentity"})),
            None,
        );
        assert!(
            message.contains("without a readable atomic:projectId"),
            "the unreadable document is explained: {message}"
        );
        assert!(
            !message.contains("<unreadable>"),
            "no bare placeholder: {message}"
        );
    }

    #[test]
    fn an_unbound_report_points_at_the_server_bug_not_remediation() {
        let message = format_bind_conflict(
            &local_id(),
            &ConflictRemote::Unbound,
            Some(&server_error(
                "IDENTITY_CONFLICT",
                "ref scoped to sibling project",
            )),
        );
        assert!(
            !message.contains("adopt") && !message.contains("fork"),
            "adopt/fork cannot help against an unbound remote: {message}"
        );
        assert!(
            message.contains("atomic-storage#113"),
            "the server-side bug is named: {message}"
        );
        assert!(
            message.contains("Upgrade the storage server and retry"),
            "the remediation is upgrade-and-retry: {message}"
        );
        assert!(
            message.contains("IDENTITY_CONFLICT")
                && message.contains("ref scoped to sibling project"),
            "the server's code and message are quoted verbatim: {message}"
        );
    }

    #[test]
    fn an_unknown_binding_states_it_could_not_be_read_without_remediation() {
        let message = format_bind_conflict(
            &local_id(),
            &ConflictRemote::Unknown,
            Some(&server_error("IDENTITY_CONFLICT", "project already bound")),
        );
        assert!(
            message.contains("could not be read"),
            "the unreadable binding is stated: {message}"
        );
        assert!(
            !message.contains("adopt") && !message.contains("fork"),
            "no remediation when the divergence is unknown: {message}"
        );
        assert!(
            message.contains("project already bound"),
            "the server's own words are quoted: {message}"
        );
    }
}
