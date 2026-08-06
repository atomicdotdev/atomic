//! Memory listing and detail reads.
//!
//! Memories are flat files at `memory/<id>.md` read through the generic
//! vault path — there is no per-memory repository method. The default
//! `memory/MEMORY.md` index scaffold (frontmatter `type: index`) is not a
//! canonical memory and is excluded from listings.

use atomic_repository::Repository;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::attestations::{
    inputs_from_entry, load_memory_attestation, memory_id, normalize_memory_path,
    AttestationStatus,
};
use crate::error::{FacadeError, FacadeResult};

/// One row of the memory list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemorySummary {
    /// The id stem (filename without `memory/` and `.md`).
    pub id: String,
    /// Vault-relative path (`memory/<id>.md`).
    pub path: String,
    /// Last update timestamp (RFC 3339).
    pub updated_at: String,
}

/// Full detail of a stored memory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryDetail {
    /// The id stem.
    pub id: String,
    /// Vault-relative path.
    pub path: String,
    /// Frontmatter as a JSON object.
    pub frontmatter: Value,
    /// Markdown body (without the frontmatter block).
    pub body: String,
    /// Whether this is a freeform memory (no canonical identity/kind fields).
    pub freeform: bool,
    /// Freshness of the memory's attestation.
    pub attestation: AttestationStatus,
    /// The signed JSON-LD node, when an attestation exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attested_node: Option<Value>,
}

/// Memories in the vault, most recently updated first.
pub fn list_memories(repo: &Repository, limit: Option<usize>) -> FacadeResult<Vec<MemorySummary>> {
    let entries = repo.vault_list("memory/", None)?;

    let mut items: Vec<MemorySummary> = entries
        .iter()
        .filter(|e| !e.path.starts_with("attestations/"))
        .filter(|e| !is_index_scaffold(repo, &e.path))
        .map(|e| MemorySummary {
            id: memory_id(&e.path),
            path: e.path.clone(),
            updated_at: e.updated_at.clone(),
        })
        .collect();

    // updated_at is RFC 3339, so lexical order == chronological order;
    // id is a stable tiebreaker.
    items.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then_with(|| a.id.cmp(&b.id)));
    if let Some(limit) = limit {
        items.truncate(limit);
    }
    Ok(items)
}

/// A single memory's content plus its attestation state.
///
/// `id_or_path` accepts a bare id, `memory/<id>`, `<id>.md`, or the full
/// `memory/<id>.md` path.
pub fn memory_detail(repo: &Repository, id_or_path: &str) -> FacadeResult<MemoryDetail> {
    let path = normalize_memory_path(id_or_path);
    let entry = repo
        .vault_retrieve(&path)?
        .ok_or_else(|| FacadeError::NotFound {
            kind: "memory",
            id: id_or_path.to_string(),
        })?;

    let inputs = inputs_from_entry("memory", &entry)?;
    let attested = load_memory_attestation(repo, id_or_path, &inputs)?;
    let freeform = is_freeform(&inputs.frontmatter);

    Ok(MemoryDetail {
        id: memory_id(id_or_path),
        path,
        frontmatter: Value::Object(inputs.frontmatter.clone()),
        body: inputs.body.clone(),
        freeform,
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

/// A freeform memory has none of the canonical identity/kind fields.
fn is_freeform(frontmatter: &serde_json::Map<String, Value>) -> bool {
    ["@id", "uid", "id", "memoryKind", "kind"]
        .iter()
        .all(|key| !frontmatter.contains_key(*key))
}

/// The default `memory/MEMORY.md` scaffold carries `type: index` frontmatter
/// and is not a canonical memory. Fail-open: unreadable entries are treated
/// as real memories so nothing is silently hidden.
fn is_index_scaffold(repo: &Repository, path: &str) -> bool {
    let Ok(Some(entry)) = repo.vault_retrieve(path) else {
        return false;
    };
    serde_json::from_str::<Value>(&entry.frontmatter_json)
        .ok()
        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_owned))
        .as_deref()
        == Some("index")
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::pristine::VaultEntryType;

    fn repo_with_memory() -> (Repository, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();
        repo.vault_store(
            "memory/test-note.md",
            VaultEntryType::Memory,
            b"A fact worth keeping.\n".to_vec(),
            "{}".to_string(),
        )
        .unwrap();
        (repo, dir)
    }

    #[test]
    fn list_excludes_index_scaffold() {
        let (repo, _dir) = repo_with_memory();

        let memories = list_memories(&repo, None).unwrap();
        assert_eq!(memories.len(), 1, "only the real memory: {memories:?}");
        assert_eq!(memories[0].id, "test-note");
        assert_eq!(memories[0].path, "memory/test-note.md");
    }

    #[test]
    fn detail_accepts_all_path_forms() {
        let (repo, _dir) = repo_with_memory();

        for form in [
            "test-note",
            "test-note.md",
            "memory/test-note",
            "memory/test-note.md",
        ] {
            let detail = memory_detail(&repo, form).unwrap();
            assert_eq!(detail.id, "test-note");
            assert!(detail.freeform);
            assert_eq!(detail.attestation, AttestationStatus::None);
            assert!(detail.body.contains("A fact worth keeping."));
        }
    }

    #[test]
    fn detail_not_found_is_client_error() {
        let (repo, _dir) = repo_with_memory();
        let err = memory_detail(&repo, "does-not-exist").unwrap_err();
        assert!(err.is_client_error(), "unexpected error: {err}");
    }
}
