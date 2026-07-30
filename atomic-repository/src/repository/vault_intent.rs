//! Vault intent lifecycle: create, list, show, update, link.
//!
//! Intents are units of work (like JIRA issues) with auto-generated
//! IDs in the format PREFIX-N (e.g., "PIMO-1", "ATOM-42").
//! The prefix is derived from the project directory name.
//!
//! Intent paths are view-scoped and session-scoped:
//! `intents/<view_name>/<session_id>/<turn_id>/intent.md`
//!
//! When session/turn info is unavailable (manual CLI usage),
//! paths fall back to: `intents/<view_name>/_manual/<N>/intent.md`
//!
//! The intent scaffold template lives at `atomic-repository/vault/templates/intent.md`.

/// Intent scaffold template. `{{title}}` is replaced with the actual title.
const INTENT_TEMPLATE: &str = include_str!("../../vault/templates/intent.md");

use super::*;
use atomic_core::pristine::vault::{IntentSummary, VaultEntry, VaultEntryType, VaultManifest};
use atomic_core::pristine::{VaultMutTxnT, VaultTxnT};
use std::fs;

/// Options for creating a new intent.
#[derive(Debug, Clone)]
pub struct IntentCreateOptions {
    /// Title of the intent (required).
    pub title: String,
    /// Priority: low, medium, high, critical. Defaults to "medium".
    pub priority: Option<String>,
    /// Initial assignee.
    pub assignee: Option<String>,
    /// Labels/tags for categorization.
    pub labels: Vec<String>,
    /// Agent session ID (if running inside an agent session).
    pub session_id: Option<String>,
    /// Turn number within the session (if running inside an agent session).
    pub turn_id: Option<u32>,
}

/// Result of creating an intent.
#[derive(Debug, Clone)]
pub struct IntentCreateResult {
    /// The human display key (e.g., "PIMO::lee-faus::3").
    pub id: String,
    /// The stable primary identity (ULID).
    pub uid: String,
    /// Vault-relative path to the intent directory.
    pub intent_dir: String,
    /// Vault-relative path to the intent.md file.
    pub intent_file: String,
    /// The view the intent was created on.
    pub view_name: String,
}

/// Result of deleting an intent.
#[derive(Debug, Clone)]
pub struct IntentDeleteResult {
    /// The normalized intent ID that was deleted.
    pub id: String,
    /// Vault-relative path to the removed intent.md file.
    pub intent_file: String,
}

/// Options for updating an intent.
///
/// `content` replaces the intent's Markdown body. When it is `None`, the
/// existing body is preserved unchanged.
#[derive(Debug, Clone, Default)]
pub struct IntentUpdateOptions {
    /// New status (backlog, planned, in-progress, review, done).
    pub status: Option<String>,
    /// New assignee.
    pub assignee: Option<String>,
    /// New priority.
    pub priority: Option<String>,
    /// New title.
    pub title: Option<String>,
    /// Replace the source-memory RDF ids that informed this Intent.
    pub informed_by: Option<Vec<String>>,
    /// New Markdown body content. When `None`, the existing body is kept.
    pub content: Option<String>,
    /// Icebox reason. When set, persists `icebox_reason` and stamps
    /// `iceboxed_at` (normally accompanies `status = icebox`).
    pub reason: Option<String>,
    /// Allow rewriting the body after the intent has started or been linked.
    pub force: bool,
}

/// Info for listing intents.
#[derive(Debug, Clone)]
pub struct IntentInfo {
    pub id: String,
    pub title: String,
    pub status: String,
    pub priority: String,
    pub assignee: Option<String>,
    pub goals: u32,
    pub blocked_by: Vec<String>,
}

impl Repository {
    /// Create a new vault intent.
    ///
    /// Allocates a JIRA-style ID (e.g., "PIMO-1") and creates an intent
    /// directory with an `intent.md` scaffold.
    ///
    /// The prefix is derived from the project directory name on first use
    /// (first 4 alphanumeric chars, uppercased).
    ///
    /// Intent paths are view-scoped and session-scoped:
    /// `intents/<view>/<session>/<turn>/intent.md`
    pub fn vault_intent_create(
        &self,
        options: IntentCreateOptions,
    ) -> Result<IntentCreateResult, RepositoryError> {
        if !self.has_vault()? {
            return Err(RepositoryError::InvalidOperation {
                message: "Vault not initialized. Run `atomic vault init` first.".to_string(),
            });
        }

        let priority = options.priority.unwrap_or_else(|| "medium".to_string());
        let view_name = self.current_view().to_string();
        let identity = self.resolve_vault_identity();
        let author = slug_author(&identity.name);

        // Stable primary identity: a ULID. The path, URN, and KG node all key
        // off this, so it never collides regardless of team size or offline
        // work. The human key is a per-author display alias.
        let uid = ulid::Ulid::new().to_string();
        let intent_dir = self.intent_dir_for(&uid);
        let intent_file = self.intent_file_for(&uid);

        // Allocate the human key inside a write transaction.
        let human_key;
        let project;
        let seq;
        {
            let mut txn = self
                .pristine
                .write_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            let mut manifest = txn
                .get_vault_manifest()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;

            // Set the project code on first use, derived from the project dir.
            if manifest.intent_prefix.is_empty() {
                let project_name = self
                    .root
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("vault");
                manifest.intent_prefix = VaultManifest::derive_intent_prefix(project_name);
                if manifest.intent_prefix.is_empty() {
                    manifest.intent_prefix = "VAULT".to_string();
                }
            }

            project = manifest.project_code().to_string();
            seq = manifest.allocate_author_seq(&author);
            human_key = VaultManifest::compose_human_key(&project, &author, seq);

            // The manifest is keyed by the human key; the ULID is stored on the
            // summary as the stable identity. Per-author keys never collide
            // across teammates, and the ULID disambiguates the rare
            // same-author-two-clones case.
            manifest.intents.insert(
                human_key.clone(),
                IntentSummary {
                    status: "backlog".to_string(),
                    priority: priority.clone(),
                    assignee: options.assignee.clone(),
                    goals: 0,
                    blocked_by: Vec::new(),
                    title: options.title.clone(),
                    vault_path: intent_file.clone(),
                    uid: uid.clone(),
                    human_key: human_key.clone(),
                    project: project.clone(),
                    author: author.clone(),
                    seq,
                    session: options.session_id.clone(),
                    turn: options.turn_id,
                },
            );

            txn.put_vault_manifest(&manifest)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            txn.commit()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }

        let now = chrono::Utc::now().to_rfc3339();

        // Build frontmatter. `uid` is the primary identity (lifted to the
        // canonical URN); `id` is the human display key. `project`/`author`/
        // `seq` are the human-key components; `session`/`turn` are provenance,
        // no longer encoded in the path.
        let mut fm = serde_json::Map::new();
        fm.insert("uid".to_string(), serde_json::Value::String(uid.clone()));
        fm.insert(
            "id".to_string(),
            serde_json::Value::String(human_key.clone()),
        );
        fm.insert(
            "project".to_string(),
            serde_json::Value::String(project.clone()),
        );
        fm.insert(
            "author".to_string(),
            serde_json::Value::String(author.clone()),
        );
        fm.insert(
            "seq".to_string(),
            serde_json::Value::Number(serde_json::Number::from(seq)),
        );
        fm.insert(
            "title".to_string(),
            serde_json::Value::String(options.title.clone()),
        );
        fm.insert(
            "status".to_string(),
            serde_json::Value::String("backlog".to_string()),
        );
        fm.insert(
            "priority".to_string(),
            serde_json::Value::String(priority.clone()),
        );
        fm.insert(
            "view".to_string(),
            serde_json::Value::String(view_name.clone()),
        );
        if let Some(ref session_id) = options.session_id {
            fm.insert(
                "session".to_string(),
                serde_json::Value::String(session_id.clone()),
            );
        }
        if let Some(turn_id) = options.turn_id {
            fm.insert(
                "turn".to_string(),
                serde_json::Value::Number(serde_json::Number::from(turn_id)),
            );
        }
        if let Some(ref assignee) = options.assignee {
            fm.insert(
                "assignee".to_string(),
                serde_json::Value::String(assignee.clone()),
            );
        }
        fm.insert(
            "created_at".to_string(),
            serde_json::Value::String(now.clone()),
        );
        if !options.labels.is_empty() {
            let labels: Vec<serde_json::Value> = options
                .labels
                .iter()
                .map(|l| serde_json::Value::String(l.clone()))
                .collect();
            fm.insert("labels".to_string(), serde_json::Value::Array(labels));
        }
        fm.insert("goals".to_string(), serde_json::Value::Array(Vec::new()));
        fm.insert(
            "created_by".to_string(),
            serde_json::Value::String(identity.to_string()),
        );

        let frontmatter_json = serde_json::to_string(&fm).unwrap_or_else(|_| "{}".to_string());

        // Build scaffold content from template. The legacy template shows the
        // human key; the directive scaffold (via `atomic intent new`) uses the
        // ULID for child-id namespacing.
        let content = INTENT_TEMPLATE
            .replace("{{title}}", &options.title)
            .replace("{{id}}", &human_key)
            .replace("{{priority}}", &priority)
            .replace("{{created_by}}", &identity.to_string())
            .replace("{{created_at}}", &now);

        // Store in vault
        self.vault_store(
            &intent_file,
            VaultEntryType::Intent,
            content.into_bytes(),
            frontmatter_json,
        )?;

        // Materialize to disk
        self.vault_materialize(&intent_file)?;

        Ok(IntentCreateResult {
            id: human_key,
            uid,
            intent_dir,
            intent_file,
            view_name,
        })
    }

    /// List vault intents.
    ///
    /// Optionally filtered by status. Pass `Some("all")` or `None` for all intents.
    pub fn vault_intent_list(
        &self,
        status_filter: Option<&str>,
    ) -> Result<Vec<IntentInfo>, RepositoryError> {
        let manifest = self.vault_manifest()?;
        let mut intents: Vec<IntentInfo> = Vec::new();

        for (id, summary) in &manifest.intents {
            if let Some(filter) = status_filter {
                if filter != "all" && summary.status != filter {
                    continue;
                }
            }

            // Use the title stored in the manifest summary.
            // Fall back to path-based lookup only for legacy entries without a title.
            let title = if !summary.title.is_empty() {
                summary.title.clone()
            } else if !summary.vault_path.is_empty() {
                self.vault_retrieve(&summary.vault_path)?
                    .and_then(|entry| {
                        let fm: serde_json::Map<String, serde_json::Value> =
                            serde_json::from_str(&entry.frontmatter_json).ok()?;
                        fm.get("title")?.as_str().map(String::from)
                    })
                    .unwrap_or_else(|| id.clone())
            } else {
                // Legacy entry with neither title nor vault_path — try scanning
                self.find_intent_path(id)?
                    .and_then(|path| self.vault_retrieve(&path).ok().flatten())
                    .and_then(|entry| {
                        let fm: serde_json::Map<String, serde_json::Value> =
                            serde_json::from_str(&entry.frontmatter_json).ok()?;
                        fm.get("title")?.as_str().map(String::from)
                    })
                    .unwrap_or_else(|| id.clone())
            };

            intents.push(IntentInfo {
                id: id.clone(),
                title,
                status: summary.status.clone(),
                priority: summary.priority.clone(),
                assignee: summary.assignee.clone(),
                goals: summary.goals,
                blocked_by: summary.blocked_by.clone(),
            });
        }

        // Sort by ID (which sorts by number within same prefix)
        intents.sort_by(|a, b| a.id.cmp(&b.id));

        Ok(intents)
    }

    /// Show an intent's full content.
    pub fn vault_intent_show(&self, intent_id: &str) -> Result<VaultEntry, RepositoryError> {
        let full_id = self.normalize_intent_id(intent_id)?;
        let intent_file =
            self.find_intent_path(&full_id)?
                .ok_or_else(|| RepositoryError::InvalidOperation {
                    message: format!("Intent '{}' not found", full_id),
                })?;

        self.vault_retrieve(&intent_file)?
            .ok_or_else(|| RepositoryError::InvalidOperation {
                message: format!("Intent '{}' not found", full_id),
            })
    }

    /// Resolve an Intent display ID to its current Vault path.
    ///
    /// This exposes the same legacy-manifest fallback used by `intent show` so
    /// read-only consumers do not need to assume `IntentSummary.vault_path` is
    /// populated.
    pub fn vault_intent_path(&self, intent_id: &str) -> Result<Option<String>, RepositoryError> {
        let full_id = self.normalize_intent_id(intent_id)?;
        self.find_intent_path(&full_id)
    }

    /// Delete an unstarted backlog intent.
    ///
    /// This is intentionally conservative: only backlog intents with no linked
    /// goals can be deleted. Started work should be closed or superseded
    /// instead of removed from the vault.
    pub fn vault_intent_delete(
        &self,
        intent_id: &str,
    ) -> Result<IntentDeleteResult, RepositoryError> {
        let full_id = self.normalize_intent_id(intent_id)?;
        let intent_file =
            self.find_intent_path(&full_id)?
                .ok_or_else(|| RepositoryError::InvalidOperation {
                    message: format!("Intent '{}' not found", full_id),
                })?;

        let manifest = self.vault_manifest()?;
        let summary =
            manifest
                .intents
                .get(&full_id)
                .ok_or_else(|| RepositoryError::InvalidOperation {
                    message: format!("Intent '{}' not found", full_id),
                })?;

        if summary.status != "backlog" {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "Intent '{}' has status '{}'; only backlog intents can be deleted",
                    full_id, summary.status
                ),
            });
        }

        let referenced_goals = manifest
            .goals
            .values()
            .filter(|goal| {
                goal.intent
                    .as_deref()
                    .is_some_and(|id| id.eq_ignore_ascii_case(&full_id))
            })
            .count() as u32;
        let linked_goals = summary.goals.max(referenced_goals);

        if linked_goals > 0 {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "Intent '{}' is linked to {} goal(s); unlink or close the work instead",
                    full_id, linked_goals
                ),
            });
        }

        let deleted = self.vault_delete(&intent_file)?;
        if !deleted {
            return Err(RepositoryError::InvalidOperation {
                message: format!("Intent '{}' not found", full_id),
            });
        }

        {
            let mut txn = self
                .pristine
                .write_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            let mut manifest = txn
                .get_vault_manifest()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            manifest.intents.remove(&full_id);
            txn.put_vault_manifest(&manifest)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            txn.commit()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }

        let materialized = self.vault_dir().join(&intent_file);
        match fs::remove_file(&materialized) {
            Ok(()) => {
                if let Some(parent) = materialized.parent() {
                    let _ = remove_empty_dirs_up_to(parent, &self.vault_dir().join("intents"));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(RepositoryError::Io(e)),
        }

        Ok(IntentDeleteResult {
            id: full_id,
            intent_file,
        })
    }

    /// Update an intent's fields.
    pub fn vault_intent_update(
        &self,
        intent_id: &str,
        options: IntentUpdateOptions,
    ) -> Result<IntentInfo, RepositoryError> {
        let full_id = self.normalize_intent_id(intent_id)?;
        let intent_file =
            self.find_intent_path(&full_id)?
                .ok_or_else(|| RepositoryError::InvalidOperation {
                    message: format!("Intent '{}' not found", full_id),
                })?;

        // Read from disk first so we pick up any user/agent edits to the
        // markdown body. If the file doesn't exist on disk, fall back to
        // the redb entry.
        let disk_path = self.vault_dir().join(&intent_file);
        let (content_bytes, frontmatter_json) = if disk_path.exists() {
            let file_content = std::fs::read_to_string(&disk_path)?;
            let (fm_json, body) = crate::repository::vault::parse_vault_frontmatter(&file_content);
            (body.into_bytes(), fm_json)
        } else {
            let entry = self.vault_retrieve(&intent_file)?.ok_or_else(|| {
                RepositoryError::InvalidOperation {
                    message: format!("Intent '{}' not found", full_id),
                }
            })?;
            (entry.content_bytes.clone(), entry.frontmatter_json.clone())
        };

        // Update frontmatter
        let mut fm: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&frontmatter_json).unwrap_or_default();

        if options.content.is_some() && !options.force {
            let manifest = self.vault_manifest()?;
            let summary = manifest.intents.get(&full_id);
            let current_status = summary
                .map(|intent| intent.status.as_str())
                .or_else(|| fm.get("status").and_then(|value| value.as_str()))
                .unwrap_or("unknown");
            let has_linked_goal = summary.is_some_and(|intent| intent.goals > 0)
                || manifest.goals.values().any(|goal| {
                    goal.intent
                        .as_deref()
                        .is_some_and(|id| id.eq_ignore_ascii_case(&full_id))
                });

            if current_status != "backlog" || has_linked_goal {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "Intent '{}' has started or is linked to a goal; rewriting its body would \
                         change the execution context. Retry with force enabled.",
                        full_id
                    ),
                });
            }
        }

        if let Some(ref status) = options.status {
            fm.insert(
                "status".to_string(),
                serde_json::Value::String(status.clone()),
            );
        }
        if let Some(ref assignee) = options.assignee {
            fm.insert(
                "assignee".to_string(),
                serde_json::Value::String(assignee.clone()),
            );
        }
        if let Some(ref priority) = options.priority {
            fm.insert(
                "priority".to_string(),
                serde_json::Value::String(priority.clone()),
            );
        }
        if let Some(ref title) = options.title {
            fm.insert(
                "title".to_string(),
                serde_json::Value::String(title.clone()),
            );
        }
        if let Some(ref reason) = options.reason {
            fm.insert(
                "icebox_reason".to_string(),
                serde_json::Value::String(reason.clone()),
            );
            fm.insert(
                "iceboxed_at".to_string(),
                serde_json::Value::String(chrono::Utc::now().to_rfc3339()),
            );
        }
        if let Some(ref sources) = options.informed_by {
            let mut unique = Vec::new();
            for source in sources.iter().map(|source| source.trim()) {
                if !source.is_empty() && !unique.iter().any(|existing| existing == source) {
                    unique.push(source.to_string());
                }
            }
            fm.insert(
                "informedBy".to_string(),
                serde_json::Value::Array(
                    unique.into_iter().map(serde_json::Value::String).collect(),
                ),
            );
        }
        let new_fm = serde_json::to_string(&fm).unwrap_or_else(|_| "{}".to_string());

        let new_content = match options.content {
            Some(ref body) => body.clone().into_bytes(),
            None => content_bytes,
        };

        // Gate enforcement: a `done` intent must be internally consistent with
        // its own checklist — every task `done` and every acceptance criterion
        // `met` (the rollup rule), with valid task/criterion vocabulary. Enforce
        // here so `--status done` (or rewriting the body of a done intent)
        // cannot bypass the gate the way a raw frontmatter write would.
        //
        // We validate only when completion is actively asserted — setting `done`
        // or editing the body of a done intent — so unrelated metadata edits are
        // never retroactively blocked. We deliberately scope enforcement to the
        // checklist shapes (TaskShape, AcceptanceCriterionShape) and the rollup
        // (IntentShape/status): the broader structural gate (why, scope-out,
        // proof, attributedTo) belongs to `atomic intent attest`/`validate` and
        // must not force the canonical authoring format onto freeform intents.
        let resulting_status = fm.get("status").and_then(|v| v.as_str()).unwrap_or("");
        let asserting_done = options.status.as_deref() == Some("done")
            || (options.content.is_some() && resulting_status == "done");
        if asserting_done {
            let body_str = String::from_utf8_lossy(&new_content).into_owned();
            // A body we cannot lift carries no canonical checklist to grade, so
            // there is nothing for the rollup to enforce; the full gate at
            // attest/validate time still surfaces malformed directives.
            if let Ok(node) = atomic_canonical::lift::lift_intent(&fm, &body_str) {
                let report = atomic_canonical::validate_intent(&node);
                let blocking: Vec<_> = report
                    .results
                    .iter()
                    .filter(|v| {
                        matches!(v.shape.as_str(), "TaskShape" | "AcceptanceCriterionShape")
                            || (v.shape == "IntentShape" && v.path.as_deref() == Some("status"))
                    })
                    .collect();
                if !blocking.is_empty() {
                    let details = blocking
                        .iter()
                        .map(|v| format!("  - {}", v.message))
                        .collect::<Vec<_>>()
                        .join("\n");
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "Intent '{}' cannot be marked done: its checklist is not complete.\n{}",
                            full_id, details
                        ),
                    });
                }
            }
        }

        self.vault_store(&intent_file, VaultEntryType::Intent, new_content, new_fm)?;

        // Update manifest
        {
            let mut txn = self
                .pristine
                .write_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            let mut manifest = txn
                .get_vault_manifest()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            if let Some(summary) = manifest.intents.get_mut(&full_id) {
                if let Some(ref status) = options.status {
                    summary.status = status.clone();
                }
                if let Some(ref assignee) = options.assignee {
                    summary.assignee = Some(assignee.clone());
                }
                if let Some(ref priority) = options.priority {
                    summary.priority = priority.clone();
                }
                if let Some(ref title) = options.title {
                    summary.title = title.clone();
                }
            }
            txn.put_vault_manifest(&manifest)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            txn.commit()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }

        self.vault_materialize(&intent_file)?;

        let title = fm
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let manifest = self.vault_manifest()?;
        let summary = manifest.intents.get(&full_id);

        Ok(IntentInfo {
            id: full_id,
            title,
            status: summary.map(|s| s.status.clone()).unwrap_or_default(),
            priority: summary.map(|s| s.priority.clone()).unwrap_or_default(),
            assignee: summary.and_then(|s| s.assignee.clone()),
            goals: summary.map(|s| s.goals).unwrap_or(0),
            blocked_by: summary.map(|s| s.blocked_by.clone()).unwrap_or_default(),
        })
    }

    /// Link a goal to an intent.
    pub fn vault_intent_link(
        &self,
        intent_id: &str,
        goal_name: &str,
    ) -> Result<(), RepositoryError> {
        let full_id = self.normalize_intent_id(intent_id)?;
        let intent_file =
            self.find_intent_path(&full_id)?
                .ok_or_else(|| RepositoryError::InvalidOperation {
                    message: format!("Intent '{}' not found", full_id),
                })?;

        // Read from disk first so we pick up any user/agent edits to the
        // markdown body (same pattern as vault_intent_update).
        let disk_path = self.vault_dir().join(&intent_file);
        let (content_bytes, frontmatter_json) = if disk_path.exists() {
            let file_content = std::fs::read_to_string(&disk_path)?;
            let (fm_json, body) = crate::repository::vault::parse_vault_frontmatter(&file_content);
            (body.into_bytes(), fm_json)
        } else {
            let entry = self.vault_retrieve(&intent_file)?.ok_or_else(|| {
                RepositoryError::InvalidOperation {
                    message: format!("Intent '{}' not found", full_id),
                }
            })?;
            (entry.content_bytes.clone(), entry.frontmatter_json.clone())
        };

        // Verify goal exists
        let goal_file = format!("goals/{}/_goal.md", goal_name);
        if self.vault_retrieve(&goal_file)?.is_none() {
            return Err(RepositoryError::InvalidOperation {
                message: format!("Goal '{}' not found", goal_name),
            });
        }

        // Update frontmatter to add goal to the list
        let mut fm: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&frontmatter_json).unwrap_or_default();

        let goals = fm
            .entry("goals".to_string())
            .or_insert_with(|| serde_json::Value::Array(Vec::new()));
        let goals = goals
            .as_array_mut()
            .ok_or_else(|| RepositoryError::InvalidOperation {
                message: format!("Intent '{}' has a non-array goals field", full_id),
            })?;
        let goal_val = serde_json::Value::String(goal_name.to_string());
        if !goals.contains(&goal_val) {
            goals.push(goal_val);
        }
        let goal_count = goals.len() as u32;
        let new_fm = serde_json::to_string(&fm).unwrap_or_else(|_| "{}".to_string());

        self.vault_store(&intent_file, VaultEntryType::Intent, content_bytes, new_fm)?;

        // Update manifest goal count
        {
            let mut txn = self
                .pristine
                .write_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            let mut manifest = txn
                .get_vault_manifest()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            if let Some(summary) = manifest.intents.get_mut(&full_id) {
                summary.goals = goal_count;
            }
            if let Some(goal) = manifest.goals.get_mut(goal_name) {
                goal.intent = Some(full_id.clone());
            }
            txn.put_vault_manifest(&manifest)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            txn.commit()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }

        self.vault_materialize(&intent_file)?;

        Ok(())
    }

    /// Build the vault-relative directory for an intent: `intents/<ulid>/`.
    ///
    /// The ULID alone guarantees a unique, collision-free path — no view,
    /// session, or turn nesting is needed (those are recorded as provenance
    /// metadata on the intent instead).
    fn intent_dir_for(&self, uid: &str) -> String {
        format!("intents/{}", uid)
    }

    /// Build the vault-relative path for an intent file: `intents/<ulid>/intent.md`.
    fn intent_file_for(&self, uid: &str) -> String {
        format!("{}/intent.md", self.intent_dir_for(uid))
    }

    /// Find the vault path for an intent by its resolved manifest key.
    ///
    /// First checks the manifest's `vault_path` field (fast path), then
    /// falls back to scanning `intents/` for a file whose frontmatter `id`
    /// (human key) matches.
    fn find_intent_path(&self, full_id: &str) -> Result<Option<String>, RepositoryError> {
        // Fast path: check the manifest for a stored vault_path
        let manifest = self.vault_manifest()?;
        if let Some(summary) = manifest.intents.get(full_id) {
            if !summary.vault_path.is_empty() {
                // Verify the path still exists
                if self.vault_retrieve(&summary.vault_path)?.is_some() {
                    return Ok(Some(summary.vault_path.clone()));
                }
            }
        }

        // Slow path: scan all intents (paths are now flat `intents/<ulid>/`).
        let prefixes = ["intents/".to_string(), "intents/manual/".to_string()];
        for prefix in &prefixes {
            let entries = self.vault_list(prefix, None)?;
            for meta in entries {
                if !meta.path.ends_with("/intent.md") {
                    continue;
                }
                if let Some(entry) = self.vault_retrieve(&meta.path)? {
                    if let Ok(fm) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(
                        &entry.frontmatter_json,
                    ) {
                        if fm.get("id").and_then(|v| v.as_str()) == Some(full_id) {
                            return Ok(Some(meta.path));
                        }
                    }
                }
            }
        }
        Ok(None)
    }

    /// Resolve a user-supplied intent reference to its manifest key (human
    /// key). Public entry point so callers outside the repository (e.g. the
    /// CLI attestation bridge) share a single resolution implementation.
    pub fn resolve_intent_key(&self, id: &str) -> Result<String, RepositoryError> {
        self.normalize_intent_id(id)
    }

    /// Normalize a user-supplied intent reference to its manifest key (the
    /// human key `PROJECT::author::seq`).
    ///
    /// Accepts, filling in the current project and author so the common case
    /// is just a number:
    /// - `3`               -> `PROJECT::<current-author>::3`
    /// - `alice::3`        -> `PROJECT::alice::3`
    /// - `PIMO::alice::3`  -> exact (project uppercased)
    /// - a ULID or prefix  -> resolved to the matching intent's human key
    /// - a legacy `PIMO-1` -> matched case-insensitively against manifest keys
    fn normalize_intent_id(&self, id: &str) -> Result<String, RepositoryError> {
        use atomic_core::pristine::vault::{parse_intent_reference, IntentRef};

        let manifest = self.vault_manifest()?;
        let project = manifest.project_code().to_string();
        let author = slug_author(&self.resolve_vault_identity().name);

        match parse_intent_reference(id, &project, &author) {
            IntentRef::HumanKey(key) => {
                // Exact manifest key, or a case-insensitive / legacy match.
                if manifest.intents.contains_key(&key) {
                    return Ok(key);
                }
                if let Some(k) = manifest
                    .intents
                    .keys()
                    .find(|k| k.eq_ignore_ascii_case(&key) || k.eq_ignore_ascii_case(id))
                {
                    return Ok(k.clone());
                }
                // No entry yet (e.g. freshly composed key): return the composed
                // form so callers can still address it.
                Ok(key)
            }
            IntentRef::Uid(uid) => {
                // Prefer an exact UID. A prefix is accepted only when it
                // identifies exactly one intent.
                let mut matches: Vec<_> = manifest
                    .intents
                    .values()
                    .filter(|summary| summary.uid.eq_ignore_ascii_case(&uid))
                    .collect();
                if matches.is_empty() {
                    let prefix = uid.to_ascii_uppercase();
                    matches = manifest
                        .intents
                        .values()
                        .filter(|summary| summary.uid.to_ascii_uppercase().starts_with(&prefix))
                        .collect();
                }
                matches.sort_by(|a, b| a.uid.cmp(&b.uid));

                if matches.len() == 1 {
                    let summary = matches[0];
                    return Ok(if summary.human_key.is_empty() {
                        summary.uid.clone()
                    } else {
                        summary.human_key.clone()
                    });
                }
                if matches.len() > 1 {
                    return Err(RepositoryError::AmbiguousIntent {
                        prefix: id.to_string(),
                        matches: matches
                            .into_iter()
                            .map(|summary| summary.uid.clone())
                            .collect(),
                    });
                }
                Err(RepositoryError::InvalidOperation {
                    message: format!("No intent matches reference '{}'", id),
                })
            }
        }
    }
}

/// Slug an identity display name into a human-key author handle.
///
/// Lowercases, converts whitespace runs to single `-`, drops characters other
/// than `[a-z0-9-]`, and collapses/trims hyphens. Interior hyphens are kept —
/// the human key separates fields with `::`, so `lee-faus` stays unambiguous.
/// Falls back to `unknown` when the result would be empty.
pub(crate) fn slug_author(name: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in name.trim().chars() {
        let c = ch.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
            last_dash = false;
        } else if (c.is_whitespace() || c == '-' || c == '_') && !out.is_empty() && !last_dash {
            out.push('-');
            last_dash = true;
        }
        // any other char is dropped
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "unknown".to_string()
    } else {
        out
    }
}

fn remove_empty_dirs_up_to(
    mut dir: &std::path::Path,
    stop_at: &std::path::Path,
) -> Result<(), std::io::Error> {
    while dir.starts_with(stop_at) && dir != stop_at {
        match fs::remove_dir(dir) {
            Ok(()) => {
                if let Some(parent) = dir.parent() {
                    dir = parent;
                } else {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if let Some(parent) = dir.parent() {
                    dir = parent;
                } else {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => break,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn init_repo_with_vault(dir: &std::path::Path) -> Repository {
        let repo = Repository::init(dir).unwrap();
        repo.init_vault().unwrap();
        repo
    }

    /// Helper to build IntentCreateOptions with session info for tests.
    fn create_opts(title: &str) -> IntentCreateOptions {
        IntentCreateOptions {
            title: title.to_string(),
            priority: None,
            assignee: None,
            labels: vec![],
            session_id: Some("test-session".to_string()),
            turn_id: Some(1),
        }
    }

    /// Helper to build IntentCreateOptions for a specific session/turn.
    fn create_opts_with_session(
        title: &str,
        session_id: &str,
        turn_id: u32,
    ) -> IntentCreateOptions {
        IntentCreateOptions {
            title: title.to_string(),
            priority: None,
            assignee: None,
            labels: vec![],
            session_id: Some(session_id.to_string()),
            turn_id: Some(turn_id),
        }
    }

    /// Helper to build IntentCreateOptions for manual mode (no session info).
    fn create_opts_manual(title: &str) -> IntentCreateOptions {
        IntentCreateOptions {
            title: title.to_string(),
            priority: None,
            assignee: None,
            labels: vec![],
            session_id: None,
            turn_id: None,
        }
    }

    #[test]
    fn test_intent_create() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());

        let result = repo
            .vault_intent_create(IntentCreateOptions {
                title: "Fix authentication".to_string(),
                priority: Some("high".to_string()),
                assignee: Some("alice".to_string()),
                labels: vec!["auth".to_string(), "security".to_string()],
                session_id: Some("sess-abc".to_string()),
                turn_id: Some(1),
            })
            .unwrap();

        // Human key ends with the per-author seq (`::1`); the ULID is the id.
        assert!(result.id.ends_with("::1"), "human key was: {}", result.id);
        assert_eq!(result.uid.len(), 26, "uid should be a 26-char ULID");
        // Path is flat and ULID-scoped.
        assert_eq!(
            result.intent_file,
            format!("intents/{}/intent.md", result.uid),
            "Path was: {}",
            result.intent_file
        );
        assert_eq!(result.view_name, repo.current_view());

        // Should be in manifest, keyed by the human key.
        let manifest = repo.vault_manifest().unwrap();
        assert!(manifest.intents.contains_key(&result.id));
        assert_eq!(manifest.intents[&result.id].priority, "high");
        assert_eq!(manifest.intents[&result.id].title, "Fix authentication");
        assert_eq!(manifest.intents[&result.id].vault_path, result.intent_file);
        assert_eq!(manifest.intents[&result.id].uid, result.uid);
        assert_eq!(manifest.intents[&result.id].seq, 1);

        // File should exist on disk
        assert!(repo.vault_dir().join(&result.intent_file).exists());
    }

    #[test]
    fn test_intent_create_auto_increment() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());

        let r1 = repo
            .vault_intent_create(create_opts_with_session("First", "sess-1", 1))
            .unwrap();

        let r2 = repo
            .vault_intent_create(create_opts_with_session("Second", "sess-1", 2))
            .unwrap();

        // Same author → per-author sequence increments.
        assert!(r1.id.ends_with("::1"), "first key: {}", r1.id);
        assert!(r2.id.ends_with("::2"), "second key: {}", r2.id);
        // Distinct ULIDs → distinct flat paths.
        assert_ne!(r1.uid, r2.uid);
        assert_ne!(r1.intent_file, r2.intent_file);
    }

    #[test]
    fn test_intent_list() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());

        repo.vault_intent_create(create_opts_with_session("Task A", "sess-1", 1))
            .unwrap();
        repo.vault_intent_create(create_opts_with_session("Task B", "sess-1", 2))
            .unwrap();

        let all = repo.vault_intent_list(None).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].title, "Task A");
        assert_eq!(all[1].title, "Task B");
    }

    #[test]
    fn test_intent_list_with_status_filter() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());

        let r1 = repo
            .vault_intent_create(create_opts_with_session("Backlog item", "s1", 1))
            .unwrap();

        let r2 = repo
            .vault_intent_create(create_opts_with_session("In progress item", "s1", 2))
            .unwrap();

        // Move second intent to in-progress
        repo.vault_intent_update(
            &r2.id,
            IntentUpdateOptions {
                status: Some("in-progress".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        let backlog = repo.vault_intent_list(Some("backlog")).unwrap();
        assert_eq!(backlog.len(), 1);
        assert_eq!(backlog[0].id, r1.id);

        let in_progress = repo.vault_intent_list(Some("in-progress")).unwrap();
        assert_eq!(in_progress.len(), 1);
        assert_eq!(in_progress[0].id, r2.id);

        let all = repo.vault_intent_list(Some("all")).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn test_intent_show() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());

        let result = repo.vault_intent_create(create_opts("Show me")).unwrap();

        let entry = repo.vault_intent_show(&result.id).unwrap();
        assert_eq!(entry.entry_type, VaultEntryType::Intent);
        let content = String::from_utf8_lossy(&entry.content_bytes);
        assert!(content.contains("# Show me"));
    }

    #[test]
    fn test_intent_update() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());

        let result = repo.vault_intent_create(create_opts("Update me")).unwrap();

        let updated = repo
            .vault_intent_update(
                &result.id,
                IntentUpdateOptions {
                    status: Some("in-progress".to_string()),
                    assignee: Some("bob".to_string()),
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(updated.status, "in-progress");
        assert_eq!(updated.assignee, Some("bob".to_string()));

        // Manifest should reflect update
        let manifest = repo.vault_manifest().unwrap();
        assert_eq!(manifest.intents[&result.id].status, "in-progress");
    }

    #[test]
    fn test_intent_update_preserves_reason_and_lineage_metadata() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());
        let result = repo
            .vault_intent_create(create_opts("Use prior project knowledge"))
            .unwrap();

        repo.vault_intent_update(
            &result.id,
            IntentUpdateOptions {
                status: Some("icebox".to_string()),
                reason: Some("Waiting for the upstream API".to_string()),
                informed_by: Some(vec![
                    "memory:auth-decision".to_string(),
                    "urn:atomic:memory:retry-policy".to_string(),
                    "memory:auth-decision".to_string(),
                ]),
                ..Default::default()
            },
        )
        .unwrap();

        let entry = repo.vault_intent_show(&result.id).unwrap();
        let fm: serde_json::Value = serde_json::from_str(&entry.frontmatter_json).unwrap();
        assert_eq!(fm["icebox_reason"], "Waiting for the upstream API");
        assert!(fm["iceboxed_at"].is_string());
        assert_eq!(
            fm["informedBy"],
            serde_json::json!(["memory:auth-decision", "urn:atomic:memory:retry-policy"])
        );

        let (_, edges) = repo.vault_extract_kg(&result.intent_file, &entry).unwrap();
        assert!(edges.iter().any(|edge| {
            edge.kind == atomic_core::pristine::ontology::predicate::INFORMED_BY
                && edge.to_id == "memory:auth-decision"
        }));
        let retry_policy = edges
            .iter()
            .find(|edge| {
                edge.kind == atomic_core::pristine::ontology::predicate::INFORMED_BY
                    && edge.to_id == "memory:retry-policy"
            })
            .expect("missing retry-policy lineage edge");
        let metadata = retry_policy.metadata.as_ref().unwrap();
        assert_eq!(metadata["rdf_target"], "urn:atomic:memory:retry-policy");
        assert_eq!(
            metadata["derived_from_vault_path"],
            result.intent_file.as_str()
        );
    }

    #[test]
    fn test_intent_update_preserves_disk_edits() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());

        let result = repo
            .vault_intent_create(create_opts("Preserve me"))
            .unwrap();

        // Simulate user/agent editing the intent file on disk
        let intent_path = repo.vault_dir().join(&result.intent_file);
        assert!(intent_path.exists(), "intent file should exist on disk");

        // Read the current file, replace the template body with real content
        let original = std::fs::read_to_string(&intent_path).unwrap();
        assert!(
            original.contains("REPLACE"),
            "template should have REPLACE placeholders"
        );

        // Write a fully filled-in intent body (keep frontmatter, replace body)
        let edited = original.split("---\n").collect::<Vec<_>>();
        // edited[0] is empty (before first ---), edited[1] is frontmatter, edited[2..] is body
        let new_body = r#"
## Problem
The authentication module has no rate limiting.

## Acceptance Criteria
- [ ] Login attempts are rate-limited to 5 per minute
- [ ] Failed attempts return 429 status code

## TODOs
- [ ] `PRES-1/1` Add rate limiter middleware
"#;
        let new_content = format!("---\n{}---\n{}", edited[1], new_body);
        std::fs::write(&intent_path, &new_content).unwrap();

        // Now run update — this should preserve the disk body
        let updated = repo
            .vault_intent_update(
                &result.id,
                IntentUpdateOptions {
                    status: Some("done".to_string()),
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(updated.status, "done");

        // Read the file back from disk — body must still have our edits
        let final_content = std::fs::read_to_string(&intent_path).unwrap();
        assert!(
            final_content.contains("rate limiting"),
            "Problem statement should survive update. Got:\n{}",
            final_content
        );
        assert!(
            final_content.contains("429 status code"),
            "Acceptance criteria should survive update. Got:\n{}",
            final_content
        );
        assert!(
            final_content.contains("PRES-1/1"),
            "TODOs should survive update. Got:\n{}",
            final_content
        );
        assert!(
            !final_content.contains("REPLACE"),
            "Template placeholders should NOT reappear. Got:\n{}",
            final_content
        );

        // Also verify redb has the updated content
        let entry = repo.vault_intent_show(&result.id).unwrap();
        let stored_body = String::from_utf8_lossy(&entry.content_bytes);
        assert!(
            stored_body.contains("rate limiting"),
            "redb body should have disk edits. Got:\n{}",
            stored_body
        );
    }

    #[test]
    fn test_intent_update_done_rejected_when_task_incomplete() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());
        let result = repo
            .vault_intent_create(create_opts("Rate limiting"))
            .unwrap();

        // A canonical body whose task is still open while we assert `done`.
        let body = "\
:::why\nNeed rate limiting.\n:::\n\n\
:::acceptance-criterion{#ac-1 status=met verifiedBy=did:atomic:lee evidence=urn:atomic:change:01J8}\n\
Login attempts are rate-limited.\n:::\n\n\
:::task{#t1 status=open satisfies=ac-1}\nAdd rate limiter middleware.\n:::";

        let err = repo
            .vault_intent_update(
                &result.id,
                IntentUpdateOptions {
                    status: Some("done".to_string()),
                    content: Some(body.to_string()),
                    ..Default::default()
                },
            )
            .unwrap_err();

        let msg = err.to_string();
        assert!(
            msg.contains("checklist is not complete") && msg.contains("every task must be done"),
            "unexpected error: {msg}"
        );

        // The rejected write must not have flipped the manifest status.
        let manifest = repo.vault_manifest().unwrap();
        assert_ne!(manifest.intents[&result.id].status, "done");
    }

    #[test]
    fn test_intent_update_done_allowed_when_checklist_complete() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());
        let result = repo
            .vault_intent_create(create_opts("Rate limiting"))
            .unwrap();

        // Same intent, but every task done and every criterion met.
        let body = "\
:::why\nNeed rate limiting.\n:::\n\n\
:::acceptance-criterion{#ac-1 status=met verifiedBy=did:atomic:lee evidence=urn:atomic:change:01J8}\n\
Login attempts are rate-limited.\n:::\n\n\
:::task{#t1 status=done satisfies=ac-1}\nAdd rate limiter middleware.\n:::";

        let updated = repo
            .vault_intent_update(
                &result.id,
                IntentUpdateOptions {
                    status: Some("done".to_string()),
                    content: Some(body.to_string()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(updated.status, "done");
    }

    #[test]
    fn test_intent_update_body_content() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());

        let result = repo.vault_intent_create(create_opts("Body edit")).unwrap();
        let new_body = "# Body edit\n\n## Problem\n\nThe widget leaks memory.\n";

        repo.vault_intent_update(
            &result.id,
            IntentUpdateOptions {
                content: Some(new_body.to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        let after = repo.vault_intent_show(&result.id).unwrap();
        assert_eq!(String::from_utf8_lossy(&after.content_bytes), new_body);
    }

    #[test]
    fn test_intent_update_without_content_preserves_body() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());

        let result = repo.vault_intent_create(create_opts("Keep body")).unwrap();
        // Read the original content from disk (since update without content reads from disk)
        let intent_path = repo.vault_dir().join(&result.intent_file);
        let disk_content = std::fs::read_to_string(&intent_path).unwrap();
        let (_, original_body) = crate::repository::vault::parse_vault_frontmatter(&disk_content);

        repo.vault_intent_update(
            &result.id,
            IntentUpdateOptions {
                priority: Some("high".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        let after = repo.vault_intent_show(&result.id).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&after.content_bytes),
            original_body,
            "body should be unchanged after frontmatter-only update"
        );
    }

    #[test]
    fn test_intent_update_body_and_frontmatter_together() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());

        let result = repo.vault_intent_create(create_opts("Combined")).unwrap();
        let new_body = "# Combined\n\nFully rewritten.\n";
        let updated = repo
            .vault_intent_update(
                &result.id,
                IntentUpdateOptions {
                    status: Some("review".to_string()),
                    priority: Some("high".to_string()),
                    content: Some(new_body.to_string()),
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(updated.status, "review");
        assert_eq!(updated.priority, "high");
        assert_eq!(
            String::from_utf8_lossy(&repo.vault_intent_show(&result.id).unwrap().content_bytes),
            new_body
        );
    }

    #[test]
    fn test_intent_delete_backlog_removes_manifest_entry_and_file() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());

        let result = repo.vault_intent_create(create_opts("Delete me")).unwrap();
        let materialized = repo.vault_dir().join(&result.intent_file);
        assert!(materialized.exists());

        let deleted = repo.vault_intent_delete(&result.id).unwrap();
        assert_eq!(deleted.id, result.id);
        assert_eq!(deleted.intent_file, result.intent_file);

        let manifest = repo.vault_manifest().unwrap();
        assert!(!manifest.intents.contains_key(&result.id));
        assert!(repo.vault_retrieve(&result.intent_file).unwrap().is_none());
        assert!(!materialized.exists());
    }

    #[test]
    fn test_intent_delete_rejects_non_backlog_intent() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());

        let result = repo.vault_intent_create(create_opts("Started")).unwrap();
        repo.vault_intent_update(
            &result.id,
            IntentUpdateOptions {
                status: Some("in-progress".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        let err = repo.vault_intent_delete(&result.id).unwrap_err();
        assert!(err
            .to_string()
            .contains("only backlog intents can be deleted"));
        assert!(repo.vault_retrieve(&result.intent_file).unwrap().is_some());
    }

    #[test]
    fn test_intent_delete_rejects_linked_goal() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());

        let intent = repo.vault_intent_create(create_opts("Linked")).unwrap();
        repo.vault_store(
            "goals/test-goal/_goal.md",
            VaultEntryType::Session,
            b"# Goal".to_vec(),
            r#"{"goal_id":"test-goal","status":"active"}"#.to_string(),
        )
        .unwrap();
        repo.vault_intent_link(&intent.id, "test-goal").unwrap();

        let err = repo.vault_intent_delete(&intent.id).unwrap_err();
        assert!(err.to_string().contains("linked to 1 goal"));
        assert!(repo.vault_retrieve(&intent.intent_file).unwrap().is_some());
    }

    #[test]
    fn test_intent_delete_rejects_goal_started_with_intent() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());

        let intent = repo
            .vault_intent_create(create_opts("Started from goal"))
            .unwrap();
        repo.vault_goal_start(GoalStartOptions {
            name: Some("test-goal".to_string()),
            intent: Some(intent.id.clone()),
            ..Default::default()
        })
        .unwrap();

        // Goal start stores the canonical relationship on the goal summary.
        // Deletion must not rely only on the intent's cached goal count.
        let manifest = repo.vault_manifest().unwrap();
        assert_eq!(manifest.intents[&intent.id].goals, 0);
        assert_eq!(
            manifest.goals["test-goal"].intent.as_deref(),
            Some(intent.id.as_str())
        );

        let err = repo.vault_intent_delete(&intent.id).unwrap_err();
        assert!(err.to_string().contains("linked to 1 goal"));
        assert!(repo.vault_retrieve(&intent.intent_file).unwrap().is_some());
    }

    #[test]
    fn test_intent_update_body_rejects_started_intent_without_force() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());

        let result = repo.vault_intent_create(create_opts("Started")).unwrap();
        repo.vault_intent_update(
            &result.id,
            IntentUpdateOptions {
                status: Some("in-progress".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        let before = repo.vault_intent_show(&result.id).unwrap().content_bytes;
        let err = repo
            .vault_intent_update(
                &result.id,
                IntentUpdateOptions {
                    content: Some("# Rewritten".to_string()),
                    ..Default::default()
                },
            )
            .unwrap_err();

        assert!(err.to_string().contains("Retry with force enabled"));
        assert_eq!(
            repo.vault_intent_show(&result.id).unwrap().content_bytes,
            before
        );
    }

    #[test]
    fn test_intent_update_body_rejects_goal_reference_without_force() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());

        let intent = repo.vault_intent_create(create_opts("Linked")).unwrap();
        repo.vault_goal_start(GoalStartOptions {
            name: Some("test-goal".to_string()),
            intent: Some(intent.id.clone()),
            ..Default::default()
        })
        .unwrap();

        let err = repo
            .vault_intent_update(
                &intent.id,
                IntentUpdateOptions {
                    content: Some("# Rewritten".to_string()),
                    ..Default::default()
                },
            )
            .unwrap_err();

        assert!(err.to_string().contains("Retry with force enabled"));
    }

    #[test]
    fn test_intent_update_body_force_allows_started_intent() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());

        let result = repo.vault_intent_create(create_opts("Started")).unwrap();
        repo.vault_intent_update(
            &result.id,
            IntentUpdateOptions {
                status: Some("in-progress".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        let new_body = "# Rewritten with explicit force\n";
        repo.vault_intent_update(
            &result.id,
            IntentUpdateOptions {
                content: Some(new_body.to_string()),
                force: true,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(
            String::from_utf8_lossy(&repo.vault_intent_show(&result.id).unwrap().content_bytes),
            new_body
        );
    }

    #[test]
    fn test_intent_link_goal() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());

        // Create intent
        let intent = repo.vault_intent_create(create_opts("Link test")).unwrap();

        // Manually store a goal entry so the link check passes
        repo.vault_store(
            "goals/test-goal/_goal.md",
            VaultEntryType::Session,
            b"# Goal".to_vec(),
            r#"{"goal_id":"test-goal","status":"active"}"#.to_string(),
        )
        .unwrap();

        // Link
        repo.vault_intent_link(&intent.id, "test-goal").unwrap();

        // Manifest should show 1 linked goal
        let manifest = repo.vault_manifest().unwrap();
        assert_eq!(manifest.intents[&intent.id].goals, 1);
    }

    #[test]
    fn test_intent_link_nonexistent_goal() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());

        let intent = repo.vault_intent_create(create_opts("Link test")).unwrap();

        assert!(repo.vault_intent_link(&intent.id, "nonexistent").is_err());
    }

    #[test]
    fn test_normalize_intent_id() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());

        // Create one intent; capture its human key + ULID.
        let created = repo.vault_intent_create(create_opts("Test")).unwrap();
        let human_key = created.id.clone();

        // A bare number resolves to the current project + author + seq.
        assert_eq!(repo.normalize_intent_id("1").unwrap(), human_key);

        // The full human key resolves to itself (project case-normalized).
        assert_eq!(repo.normalize_intent_id(&human_key).unwrap(), human_key);
        assert_eq!(
            repo.normalize_intent_id(&human_key.to_lowercase()).unwrap(),
            human_key
        );

        // A ULID (and a prefix of it) resolves to the same intent's human key.
        assert_eq!(repo.normalize_intent_id(&created.uid).unwrap(), human_key);
        assert_eq!(
            repo.normalize_intent_id(&created.uid[..8]).unwrap(),
            human_key
        );
    }

    #[test]
    fn test_normalize_intent_id_rejects_ambiguous_prefix() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());
        let first = repo.vault_intent_create(create_opts("First")).unwrap();
        let second = repo.vault_intent_create(create_opts("Second")).unwrap();
        let first_uid = "01KTEST0000000000000000001";
        let second_uid = "01KTEST0000000000000000002";

        let mut txn = repo.pristine.write_txn().unwrap();
        let mut manifest = txn.get_vault_manifest().unwrap();
        manifest.intents.get_mut(&first.id).unwrap().uid = first_uid.into();
        manifest.intents.get_mut(&second.id).unwrap().uid = second_uid.into();
        txn.put_vault_manifest(&manifest).unwrap();
        txn.commit().unwrap();

        // A complete UID remains exact.
        assert_eq!(repo.normalize_intent_id(first_uid).unwrap(), first.id);

        // A shared prefix must never pick whichever HashMap entry appears first.
        let error = repo.normalize_intent_id("01KTEST").unwrap_err();
        match error {
            RepositoryError::AmbiguousIntent { prefix, matches } => {
                assert_eq!(prefix, "01KTEST");
                assert_eq!(matches, vec![first_uid.to_string(), second_uid.to_string()]);
            }
            other => panic!("expected ambiguous intent error, got {other:?}"),
        }
    }

    #[test]
    fn test_intent_show_not_found() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());
        assert!(repo.vault_intent_show("VAULT-999").is_err());
    }

    #[test]
    fn test_vault_intent_path_resolves_legacy_manifest_entry() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());
        let intent = repo
            .vault_intent_create(create_opts("Legacy path lookup"))
            .unwrap();

        // Simulate a manifest created before IntentSummary.vault_path existed.
        let mut txn = repo.pristine.write_txn().unwrap();
        let mut manifest = txn.get_vault_manifest().unwrap();
        manifest
            .intents
            .get_mut(&intent.id)
            .unwrap()
            .vault_path
            .clear();
        txn.put_vault_manifest(&manifest).unwrap();
        txn.commit().unwrap();

        assert_eq!(
            repo.vault_intent_path(&intent.id).unwrap(),
            Some(intent.intent_file)
        );
    }

    #[test]
    fn test_intent_create_without_vault() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        // Do NOT init vault
        let result = repo.vault_intent_create(IntentCreateOptions {
            title: "Should fail".to_string(),
            priority: None,
            assignee: None,
            labels: vec![],
            session_id: None,
            turn_id: None,
        });
        assert!(result.is_err());
    }

    #[test]
    fn test_intent_create_default_priority() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());

        let result = repo
            .vault_intent_create(create_opts("Default priority"))
            .unwrap();

        let manifest = repo.vault_manifest().unwrap();
        assert_eq!(manifest.intents[&result.id].priority, "medium");
    }

    #[test]
    fn test_intent_link_idempotent_frontmatter() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());

        let intent = repo
            .vault_intent_create(create_opts("Idempotent link"))
            .unwrap();

        // Create a goal entry
        repo.vault_store(
            "goals/my-goal/_goal.md",
            VaultEntryType::Session,
            b"# Goal".to_vec(),
            r#"{"goal_id":"my-goal","status":"active"}"#.to_string(),
        )
        .unwrap();

        // Link twice
        repo.vault_intent_link(&intent.id, "my-goal").unwrap();
        repo.vault_intent_link(&intent.id, "my-goal").unwrap();

        // Frontmatter goals array should have the entry only once
        let entry = repo.vault_intent_show(&intent.id).unwrap();
        let fm: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&entry.frontmatter_json).unwrap();
        let goals = fm.get("goals").unwrap().as_array().unwrap();
        assert_eq!(goals.len(), 1);

        // The manifest count follows unique links as well.
        let manifest = repo.vault_manifest().unwrap();
        assert_eq!(manifest.intents[&intent.id].goals, 1);
    }

    #[test]
    fn test_normalize_invalid_id() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());

        let result = repo.normalize_intent_id("notanumber");
        assert!(result.is_err());
    }

    #[test]
    fn test_intent_create_manual_mode() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());

        // "Manual" (no session/turn) intents use the same flat ULID path;
        // session/turn are provenance metadata, not a path namespace.
        let result = repo
            .vault_intent_create(create_opts_manual("Manual intent"))
            .unwrap();

        assert_eq!(
            result.intent_file,
            format!("intents/{}/intent.md", result.uid)
        );

        // Should still be retrievable by its human key.
        let entry = repo.vault_intent_show(&result.id).unwrap();
        let content = String::from_utf8_lossy(&entry.content_bytes);
        assert!(content.contains("# Manual intent"));
    }

    #[test]
    fn test_intent_create_view_name_in_result() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());

        let result = repo.vault_intent_create(create_opts("View check")).unwrap();

        // The view_name should match the repo's current view.
        assert_eq!(result.view_name, repo.current_view());
        // The path is flat and ULID-scoped (the view lives in frontmatter now).
        assert_eq!(
            result.intent_file,
            format!("intents/{}/intent.md", result.uid)
        );
    }

    #[test]
    fn test_intent_create_view_in_frontmatter() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());

        let result = repo
            .vault_intent_create(create_opts("Frontmatter view"))
            .unwrap();

        let entry = repo.vault_intent_show(&result.id).unwrap();
        let fm: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&entry.frontmatter_json).unwrap();
        assert_eq!(
            fm.get("view").and_then(|v| v.as_str()),
            Some(repo.current_view())
        );
    }

    #[test]
    fn test_intent_update_syncs_title_to_manifest() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());

        let result = repo
            .vault_intent_create(create_opts("Original title"))
            .unwrap();

        repo.vault_intent_update(
            &result.id,
            IntentUpdateOptions {
                title: Some("Updated title".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        // Manifest should have the updated title
        let manifest = repo.vault_manifest().unwrap();
        assert_eq!(manifest.intents[&result.id].title, "Updated title");

        // List should show the updated title
        let all = repo.vault_intent_list(None).unwrap();
        assert_eq!(all[0].title, "Updated title");
    }
}
