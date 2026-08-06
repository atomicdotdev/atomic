//! Intent listing and detail reads.

use atomic_repository::{IntentInfo, Repository};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::attestations::{inputs_from_entry, load_intent_attestation, AttestationStatus};
use crate::error::{FacadeError, FacadeResult};

/// One row of the intent list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentSummary {
    /// Normalized intent id (e.g. `PIMO-1`).
    pub id: String,
    /// Title.
    pub title: String,
    /// Status (e.g. `open`, `in_progress`, `done`).
    pub status: String,
    /// Priority (e.g. `low`, `medium`, `high`).
    pub priority: String,
    /// Assignee, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    /// Number of linked goals.
    pub goals: u32,
    /// Ids of intents blocking this one.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub blocked_by: Vec<String>,
}

impl From<&IntentInfo> for IntentSummary {
    fn from(info: &IntentInfo) -> Self {
        Self {
            id: info.id.clone(),
            title: info.title.clone(),
            status: info.status.clone(),
            priority: info.priority.clone(),
            assignee: info.assignee.clone(),
            goals: info.goals,
            blocked_by: info.blocked_by.clone(),
        }
    }
}

/// Full detail of a stored intent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentDetail {
    /// Normalized intent id.
    pub id: String,
    /// Frontmatter as a JSON object.
    pub frontmatter: Value,
    /// Markdown body (without the frontmatter block).
    pub body: String,
    /// Vault-relative path of the entry, when the manifest records one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vault_path: Option<String>,
    /// Freshness of the intent's attestation.
    pub attestation: AttestationStatus,
    /// The signed JSON-LD node, when an attestation exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attested_node: Option<Value>,
}

/// Intents from the vault manifest, optionally filtered by status
/// (`None` or `Some("all")` lists everything).
pub fn list_intents(
    repo: &Repository,
    status: Option<&str>,
) -> FacadeResult<Vec<IntentSummary>> {
    let infos = repo.vault_intent_list(status)?;
    Ok(infos.iter().map(IntentSummary::from).collect())
}

/// A single intent's content plus its attestation state.
///
/// `id` accepts the same forms as the CLI: `PIMO-1`, `pimo-1`, a bare
/// number, a ULID, or a unique prefix.
pub fn intent_detail(repo: &Repository, id: &str) -> FacadeResult<IntentDetail> {
    // An id the resolver rejects means no such intent — surface it as a
    // client-facing NotFound. Infrastructure failures (database, IO) must
    // stay server errors, not 404s.
    let normalized = repo.resolve_intent_key(id).map_err(|e| match &e {
        atomic_repository::RepositoryError::InvalidOperation { .. } => FacadeError::NotFound {
            kind: "intent",
            id: id.to_string(),
        },
        _ if e.is_not_found() => FacadeError::NotFound {
            kind: "intent",
            id: id.to_string(),
        },
        _ => FacadeError::Repository(e),
    })?;
    let entry = repo.vault_intent_show(&normalized)?;
    let inputs = inputs_from_entry("intent", &entry)?;
    let attested = load_intent_attestation(repo, &normalized, &inputs)?;

    let vault_path = vault_path_for(repo, &normalized)?;

    Ok(IntentDetail {
        id: normalized,
        frontmatter: Value::Object(inputs.frontmatter),
        body: inputs.body,
        vault_path,
        attestation: attested.status(),
        attested_node: attested
            .node()
            .map(serde_json::to_value)
            .transpose()
            .map_err(|e| FacadeError::Malformed {
                message: format!("attested node did not serialize: {e}"),
            })?,
    })
}

/// The vault-relative path of a stored intent via the manifest, if recorded.
fn vault_path_for(repo: &Repository, id: &str) -> FacadeResult<Option<String>> {
    let manifest = repo.vault_manifest()?;
    Ok(manifest
        .intents
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(id))
        .map(|(_, summary)| summary.vault_path.clone())
        .filter(|p| !p.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_repository::IntentCreateOptions;

    fn repo_with_intent() -> (Repository, String, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();
        let result = repo
            .vault_intent_create(IntentCreateOptions {
                title: "Facade test intent".to_string(),
                priority: Some("medium".to_string()),
                assignee: None,
                labels: Vec::new(),
                session_id: None,
                turn_id: None,
            })
            .unwrap();
        (repo, result.id, dir)
    }

    #[test]
    fn list_and_detail_roundtrip() {
        let (repo, id, _dir) = repo_with_intent();

        let summaries = list_intents(&repo, None).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].title, "Facade test intent");

        let detail = intent_detail(&repo, &id).unwrap();
        assert_eq!(detail.attestation, AttestationStatus::None);
        assert!(detail.frontmatter.is_object());

        // JSON output carries the expected top-level fields.
        let json = serde_json::to_value(&detail).unwrap();
        assert!(json.get("id").is_some());
        assert!(json.get("body").is_some());
        assert_eq!(json.get("attestation").unwrap(), "none");
    }

    #[test]
    fn detail_not_found_is_client_error() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        let err = intent_detail(&repo, "NOPE-99").unwrap_err();
        assert!(err.is_client_error(), "unexpected error: {err}");
    }
}
