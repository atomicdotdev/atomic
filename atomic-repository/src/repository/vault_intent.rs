//! Vault intent lifecycle: create, list, show, update, link.
//!
//! Intents are units of work (like JIRA issues) with auto-generated
//! IDs in the format PREFIX-N (e.g., "PIMO-1", "ATOM-42").
//! The prefix is derived from the project directory name.

use super::*;
use atomic_core::pristine::vault::{IntentSummary, VaultEntry, VaultEntryType, VaultManifest};
use atomic_core::pristine::{VaultMutTxnT, VaultTxnT};

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
}

/// Options for updating an intent.
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
    pub fn vault_intent_create(
        &self,
        options: IntentCreateOptions,
    ) -> Result<IntentCreateResult, RepositoryError> {
        if !self.has_vault()? {
            return Err(RepositoryError::InvalidOperation {
                message: "Vault not initialized. Run `atomic init --vault` first.".to_string(),
            });
        }

        let priority = options.priority.unwrap_or_else(|| "medium".to_string());

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
                },
            );

            txn.put_vault_manifest(&manifest)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            txn.commit()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }

        let now = chrono::Utc::now().to_rfc3339();

        // Build paths using the intent_id as the directory name
        let intent_dir = format!("intents/{}", intent_id.to_lowercase());
        let intent_file = format!("{}/intent.md", intent_dir);

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

        // Build scaffold content
        let content = format!(
            "# {}\n\n## Context\n\n(Describe the problem or feature here)\n\n## Acceptance Criteria\n\n- [ ] (First criterion)\n\n## Notes\n\n",
            options.title
        );

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

            // Get title from the stored entry's frontmatter
            let intent_file = format!("intents/{}/intent.md", id.to_lowercase());
            let title = self
                .vault_retrieve(&intent_file)?
                .and_then(|entry| {
                    let fm: serde_json::Map<String, serde_json::Value> =
                        serde_json::from_str(&entry.frontmatter_json).ok()?;
                    fm.get("title")?.as_str().map(String::from)
                })
                .unwrap_or_else(|| id.clone());

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
        // Normalize: accept "PIMO-1" or "1" (auto-prepend prefix)
        let full_id = self.normalize_intent_id(intent_id)?;
        let intent_file = format!("intents/{}/intent.md", full_id.to_lowercase());

        self.vault_retrieve(&intent_file)?
            .ok_or_else(|| RepositoryError::InvalidOperation {
                message: format!("Intent '{}' not found", full_id),
            })
    }

    /// Update an intent's fields.
    pub fn vault_intent_update(
        &self,
        intent_id: &str,
        options: IntentUpdateOptions,
    ) -> Result<IntentInfo, RepositoryError> {
        let full_id = self.normalize_intent_id(intent_id)?;
        let intent_file = format!("intents/{}/intent.md", full_id.to_lowercase());

        let entry = self.vault_retrieve(&intent_file)?.ok_or_else(|| {
            RepositoryError::InvalidOperation {
                message: format!("Intent '{}' not found", full_id),
            }
        })?;

        // Update frontmatter
        let mut fm: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&entry.frontmatter_json).unwrap_or_default();

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
        let new_fm = serde_json::to_string(&fm).unwrap_or_else(|_| "{}".to_string());

        // Re-store with updated frontmatter
        self.vault_store(
            &intent_file,
            VaultEntryType::Intent,
            entry.content_bytes.clone(),
            new_fm,
        )?;

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
        let intent_file = format!("intents/{}/intent.md", full_id.to_lowercase());

        let entry = self.vault_retrieve(&intent_file)?.ok_or_else(|| {
            RepositoryError::InvalidOperation {
                message: format!("Intent '{}' not found", full_id),
            }
        })?;

        // Verify goal exists
        let goal_file = format!("goals/{}/_goal.md", goal_name);
        if self.vault_retrieve(&goal_file)?.is_none() {
            return Err(RepositoryError::InvalidOperation {
                message: format!("Goal '{}' not found", goal_name),
            });
        }

        // Update frontmatter to add goal to the list
        let mut fm: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&entry.frontmatter_json).unwrap_or_default();

        let goals = fm
            .entry("goals".to_string())
            .or_insert_with(|| serde_json::Value::Array(Vec::new()));
        if let serde_json::Value::Array(ref mut arr) = goals {
            let goal_val = serde_json::Value::String(goal_name.to_string());
            if !arr.contains(&goal_val) {
                arr.push(goal_val);
            }
        }
        let new_fm = serde_json::to_string(&fm).unwrap_or_else(|_| "{}".to_string());

        self.vault_store(
            &intent_file,
            VaultEntryType::Intent,
            entry.content_bytes.clone(),
            new_fm,
        )?;

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
                summary.goals = summary.goals.saturating_add(1);
            }
            txn.put_vault_manifest(&manifest)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            txn.commit()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }

        self.vault_materialize(&intent_file)?;

        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn init_repo_with_vault(dir: &std::path::Path) -> Repository {
        let repo = Repository::init(dir).unwrap();
        repo.init_vault().unwrap();
        repo
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
            })
            .unwrap();

        // ID should be PREFIX-1
        assert!(result.id.ends_with("-1"), "ID was: {}", result.id);
        assert!(result.intent_file.ends_with("/intent.md"));

        // Should be in manifest
        let manifest = repo.vault_manifest().unwrap();
        assert!(manifest.intents.contains_key(&result.id));
        assert_eq!(manifest.intents[&result.id].priority, "high");
        assert_eq!(manifest.next_intent_id, 2);

        // File should exist on disk
        assert!(repo.vault_dir().join(&result.intent_file).exists());
    }

    #[test]
    fn test_intent_create_auto_increment() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());

        let r1 = repo
            .vault_intent_create(IntentCreateOptions {
                title: "First".to_string(),
                priority: None,
                assignee: None,
                labels: vec![],
            })
            .unwrap();

        let r2 = repo
            .vault_intent_create(IntentCreateOptions {
                title: "Second".to_string(),
                priority: None,
                assignee: None,
                labels: vec![],
            })
            .unwrap();

        assert!(r1.id.ends_with("-1"));
        assert!(r2.id.ends_with("-2"));
    }

    #[test]
    fn test_intent_list() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());

        repo.vault_intent_create(IntentCreateOptions {
            title: "Task A".to_string(),
            priority: None,
            assignee: None,
            labels: vec![],
        })
        .unwrap();
        repo.vault_intent_create(IntentCreateOptions {
            title: "Task B".to_string(),
            priority: None,
            assignee: None,
            labels: vec![],
        })
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
            .vault_intent_create(IntentCreateOptions {
                title: "Backlog item".to_string(),
                priority: None,
                assignee: None,
                labels: vec![],
            })
            .unwrap();

        let r2 = repo
            .vault_intent_create(IntentCreateOptions {
                title: "In progress item".to_string(),
                priority: None,
                assignee: None,
                labels: vec![],
            })
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

        let result = repo
            .vault_intent_create(IntentCreateOptions {
                title: "Show me".to_string(),
                priority: None,
                assignee: None,
                labels: vec![],
            })
            .unwrap();

        let entry = repo.vault_intent_show(&result.id).unwrap();
        assert_eq!(entry.entry_type, VaultEntryType::Intent);
        let content = String::from_utf8_lossy(&entry.content_bytes);
        assert!(content.contains("# Show me"));
    }

    #[test]
    fn test_intent_update() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());

        let result = repo
            .vault_intent_create(IntentCreateOptions {
                title: "Update me".to_string(),
                priority: None,
                assignee: None,
                labels: vec![],
            })
            .unwrap();

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
    fn test_intent_link_goal() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());

        // Create intent
        let intent = repo
            .vault_intent_create(IntentCreateOptions {
                title: "Link test".to_string(),
                priority: None,
                assignee: None,
                labels: vec![],
            })
            .unwrap();

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

        let intent = repo
            .vault_intent_create(IntentCreateOptions {
                title: "Link test".to_string(),
                priority: None,
                assignee: None,
                labels: vec![],
            })
            .unwrap();

        assert!(repo.vault_intent_link(&intent.id, "nonexistent").is_err());
    }

    #[test]
    fn test_normalize_intent_id() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());

        // Create one intent to set the prefix
        repo.vault_intent_create(IntentCreateOptions {
            title: "Test".to_string(),
            priority: None,
            assignee: None,
            labels: vec![],
        })
        .unwrap();

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
    fn test_intent_create_without_vault() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        // Do NOT init vault
        let result = repo.vault_intent_create(IntentCreateOptions {
            title: "Should fail".to_string(),
            priority: None,
            assignee: None,
            labels: vec![],
        });
        assert!(result.is_err());
    }

    #[test]
    fn test_intent_create_default_priority() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());

        let result = repo
            .vault_intent_create(IntentCreateOptions {
                title: "Default priority".to_string(),
                priority: None,
                assignee: None,
                labels: vec![],
            })
            .unwrap();

        let manifest = repo.vault_manifest().unwrap();
        assert_eq!(manifest.intents[&result.id].priority, "medium");
    }

    #[test]
    fn test_intent_link_idempotent_frontmatter() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());

        let intent = repo
            .vault_intent_create(IntentCreateOptions {
                title: "Idempotent link".to_string(),
                priority: None,
                assignee: None,
                labels: vec![],
            })
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

        // But manifest counter increments each time
        let manifest = repo.vault_manifest().unwrap();
        assert_eq!(manifest.intents[&intent.id].goals, 2);
    }

    #[test]
    fn test_normalize_invalid_id() {
        let dir = tempdir().unwrap();
        let repo = init_repo_with_vault(dir.path());

        let result = repo.normalize_intent_id("notanumber");
        assert!(result.is_err());
    }
}
