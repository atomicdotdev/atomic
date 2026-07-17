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
    /// The generated JIRA-style ID (e.g., "PIMO-1").
    pub id: String,
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

        // Compute the session-scoped directory and file paths
        let intent_dir = self.intent_dir_for(options.session_id.as_deref(), options.turn_id);
        let intent_file = self.intent_file_for(options.session_id.as_deref(), options.turn_id);

        // Allocate ID inside a write transaction
        let intent_id;
        {
            let mut txn = self
                .pristine
                .write_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            let mut manifest = txn
                .get_vault_manifest()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;

            // Set prefix on first use, derived from project directory name
            if manifest.intent_prefix.is_empty() {
                let project_name = self
                    .root
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("vault");
                manifest.intent_prefix = VaultManifest::derive_intent_prefix(project_name);
                // Fallback if project name yields empty prefix
                if manifest.intent_prefix.is_empty() {
                    manifest.intent_prefix = "VAULT".to_string();
                }
            }

            intent_id = manifest.allocate_intent_id();

            // Add to manifest intents index
            manifest.intents.insert(
                intent_id.clone(),
                IntentSummary {
                    status: "backlog".to_string(),
                    priority: priority.clone(),
                    assignee: options.assignee.clone(),
                    goals: 0,
                    blocked_by: Vec::new(),
                    title: options.title.clone(),
                    vault_path: intent_file.clone(),
                },
            );

            txn.put_vault_manifest(&manifest)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            txn.commit()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }

        let now = chrono::Utc::now().to_rfc3339();

        // Build frontmatter
        let mut fm = serde_json::Map::new();
        fm.insert(
            "id".to_string(),
            serde_json::Value::String(intent_id.clone()),
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

        // Add identity provenance
        let identity = self.resolve_vault_identity();
        fm.insert(
            "created_by".to_string(),
            serde_json::Value::String(identity.to_string()),
        );

        let frontmatter_json = serde_json::to_string(&fm).unwrap_or_else(|_| "{}".to_string());

        // Build scaffold content from template — fill in all placeholders
        let content = INTENT_TEMPLATE
            .replace("{{title}}", &options.title)
            .replace("{{id}}", &intent_id)
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
            id: intent_id,
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
        let new_fm = serde_json::to_string(&fm).unwrap_or_else(|_| "{}".to_string());

        let new_content = match options.content {
            Some(ref body) => body.clone().into_bytes(),
            None => content_bytes,
        };

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

    /// Build the vault-relative directory for an intent.
    ///
    /// Agent mode:  `intents/<view>/<session>/<turn>/`
    /// Manual mode: `intents/manual/<identity>/<N>/`
    fn intent_dir_for(&self, session_id: Option<&str>, turn_id: Option<u32>) -> String {
        let view = self.current_view();
        match (session_id, turn_id) {
            (Some(sid), Some(tid)) => format!("intents/{}/{}/{}", view, sid, tid),
            _ => {
                // Manual usage — scope under the user's identity name
                let identity = self.resolve_vault_identity();
                let manifest = self.vault_manifest().unwrap_or_default();
                format!(
                    "intents/manual/{}/{}",
                    identity.name, manifest.next_intent_id
                )
            }
        }
    }

    /// Build the vault-relative path for an intent file.
    fn intent_file_for(&self, session_id: Option<&str>, turn_id: Option<u32>) -> String {
        format!("{}/intent.md", self.intent_dir_for(session_id, turn_id))
    }

    /// Find the vault path for an intent by its display ID.
    ///
    /// First checks the manifest's `vault_path` field (fast path), then
    /// falls back to scanning vault entries under `intents/<current_view>/`
    /// for a file whose frontmatter `id` field matches the normalized intent ID.
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

        // Slow path: scan vault entries under the current view + manual/
        let prefixes = [
            format!("intents/{}/", self.current_view()),
            "intents/manual/".to_string(),
        ];
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

    /// Normalize an intent ID — accept "PIMO-1", "pimo-1", or just "1".
    fn normalize_intent_id(&self, id: &str) -> Result<String, RepositoryError> {
        // If it already looks like PREFIX-N, uppercase it
        if id.contains('-') {
            return Ok(id.to_uppercase());
        }

        // Just a number — prepend the prefix
        if id.parse::<u32>().is_ok() {
            let manifest = self.vault_manifest()?;
            let prefix = if manifest.intent_prefix.is_empty() {
                "VAULT".to_string()
            } else {
                manifest.intent_prefix.clone()
            };
            return Ok(format!("{}-{}", prefix, id));
        }

        // Unknown format
        Err(RepositoryError::InvalidOperation {
            message: format!(
                "Invalid intent ID: '{}'. Expected format: PREFIX-N or N",
                id
            ),
        })
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

        // ID should be PREFIX-1
        assert!(result.id.ends_with("-1"), "ID was: {}", result.id);
        // Path should be view-scoped and session-scoped
        assert!(
            result.intent_file.contains("/sess-abc/1/intent.md"),
            "Path was: {}",
            result.intent_file
        );
        assert_eq!(result.view_name, repo.current_view());

        // Should be in manifest
        let manifest = repo.vault_manifest().unwrap();
        assert!(manifest.intents.contains_key(&result.id));
        assert_eq!(manifest.intents[&result.id].priority, "high");
        assert_eq!(manifest.intents[&result.id].title, "Fix authentication");
        assert_eq!(manifest.intents[&result.id].vault_path, result.intent_file);
        assert_eq!(manifest.next_intent_id, 2);

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

        assert!(r1.id.ends_with("-1"));
        assert!(r2.id.ends_with("-2"));
        // Different turns → different paths
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

        // Create one intent to set the prefix
        repo.vault_intent_create(create_opts("Test")).unwrap();

        let manifest = repo.vault_manifest().unwrap();
        let prefix = manifest.intent_prefix.clone();

        // Full ID
        let normalized = repo.normalize_intent_id(&format!("{}-1", prefix)).unwrap();
        assert_eq!(normalized, format!("{}-1", prefix));

        // Lowercase
        let normalized = repo
            .normalize_intent_id(&format!("{}-1", prefix.to_lowercase()))
            .unwrap();
        assert_eq!(normalized, format!("{}-1", prefix));

        // Just number
        let normalized = repo.normalize_intent_id("1").unwrap();
        assert_eq!(normalized, format!("{}-1", prefix));
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

        let result = repo
            .vault_intent_create(create_opts_manual("Manual intent"))
            .unwrap();

        // Path should use manual/<identity>/ fallback
        assert!(
            result.intent_file.starts_with("intents/manual/"),
            "Expected manual path, got: {}",
            result.intent_file
        );
        assert!(result.intent_file.ends_with("/intent.md"));
        // Should contain the identity name as a path segment
        let identity = repo.resolve_vault_identity();
        assert!(
            result
                .intent_file
                .contains(&format!("manual/{}/", identity.name)),
            "Expected identity '{}' in path: {}",
            identity.name,
            result.intent_file
        );

        // Should still be retrievable
        let entry = repo.vault_intent_show(&result.id).unwrap();
        let content = String::from_utf8_lossy(&entry.content_bytes);
        assert!(content.contains("# Manual intent"));
    }

    #[test]
    fn test_intent_create_view_name_in_result() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());

        let result = repo.vault_intent_create(create_opts("View check")).unwrap();

        // The view_name should match the repo's current view
        assert_eq!(result.view_name, repo.current_view());
        // The path should contain the view name
        assert!(
            result
                .intent_file
                .starts_with(&format!("intents/{}/", repo.current_view())),
            "Path was: {}",
            result.intent_file
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
