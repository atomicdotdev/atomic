//! Vault goal lifecycle: start, stop, resume.
//!
//! Goals are the vault's conversation transcripts — who ran what,
//! what tools were called, what was found. They use haikunator-style
//! names like "swift-meadow-a3f2" for easy reference.
//!
//! Goals are pure vault operations — they manage vault entries (redb +
//! markdown) but do NOT touch views. View creation, switching, and
//! merging are the agent's responsibility via the AtomicTool.

use super::*;
use atomic_core::pristine::vault::{GoalSummary, TokenCounts, VaultEntry, VaultEntryType};
use atomic_core::pristine::{VaultMutTxnT, VaultTxnT};

/// Options for starting a new goal.
#[derive(Debug, Clone, Default)]
pub struct GoalStartOptions {
    /// Override the generated goal name.
    pub name: Option<String>,
    /// Developer name/email.
    pub developer: Option<String>,
    /// Link to an intent ID (e.g., "PIMO-3").
    pub intent: Option<String>,
    /// AI model being used.
    pub model: Option<String>,
}

/// Result of starting a goal.
#[derive(Debug, Clone)]
pub struct GoalStartResult {
    /// The goal name (e.g., "swift-meadow-a3f2").
    pub name: String,
    /// Vault-relative path to the goal directory.
    pub goal_dir: String,
    /// Vault-relative path to the _goal.md file.
    pub goal_file: String,
}

/// Options for stopping a goal.
#[derive(Debug, Clone, Default)]
pub struct GoalStopOptions {
    /// Promote: mark as completed for team consumption.
    pub promote: bool,
    /// Discard: delete the goal entirely.
    pub discard: bool,
}

/// Result of stopping a goal.
#[derive(Debug, Clone)]
pub struct GoalStopResult {
    pub name: String,
    /// "completed", "suspended", or "discarded"
    pub status: String,
    /// Number of vault entries removed (only non-zero for discard).
    pub paths_removed: usize,
}

/// Summary info for listing goals.
#[derive(Debug, Clone)]
pub struct GoalInfo {
    pub name: String,
    pub developer: String,
    pub status: String,
    pub intent: Option<String>,
    pub started_at: String,
    pub turns: u32,
    pub tokens: TokenCounts,
}

impl Repository {
    /// Start a new vault goal.
    ///
    /// Creates a goal directory under `.vault/goals/` with
    /// a `_goal.md` file containing initial metadata.
    ///
    /// Goal names are Docker-style haikunator names (e.g., "swift-meadow-a3f2")
    /// unless overridden with `options.name`.
    ///
    /// This is a pure vault operation — it does NOT create or switch views.
    /// View management is the agent's responsibility via the AtomicTool.
    pub fn vault_goal_start(
        &self,
        options: GoalStartOptions,
    ) -> Result<GoalStartResult, RepositoryError> {
        if !self.has_vault()? {
            return Err(RepositoryError::InvalidOperation {
                message: "Vault not initialized. Run `atomic init --vault` first.".to_string(),
            });
        }

        // Generate or use provided name
        let name = options
            .name
            .unwrap_or_else(super::vault_names::generate_goal_name);

        // Check for name collision
        let goal_dir = format!("goals/{}", name);
        let goal_file = format!("{}/{}", goal_dir, "_goal.md");
        if self.vault_retrieve(&goal_file)?.is_some() {
            return Err(RepositoryError::InvalidOperation {
                message: format!("Goal '{}' already exists", name),
            });
        }

        let now = chrono::Utc::now().to_rfc3339();
        let developer = options.developer.clone().unwrap_or_else(|| {
            let identity = self.resolve_vault_identity();
            identity.to_string()
        });

        // Build frontmatter
        let mut fm = serde_json::Map::new();
        fm.insert(
            "goal_id".to_string(),
            serde_json::Value::String(name.clone()),
        );
        if let Some(ref dev) = options.developer {
            fm.insert(
                "developer".to_string(),
                serde_json::Value::String(dev.clone()),
            );
        } else {
            let identity = self.resolve_vault_identity();
            fm.insert(
                "developer".to_string(),
                serde_json::Value::String(identity.to_string()),
            );
            fm.insert(
                "provenance".to_string(),
                serde_json::Value::String(identity.to_provenance_string()),
            );
        }
        if let Some(ref model) = options.model {
            fm.insert(
                "model".to_string(),
                serde_json::Value::String(model.clone()),
            );
        }
        fm.insert(
            "status".to_string(),
            serde_json::Value::String("active".to_string()),
        );
        if let Some(ref intent) = options.intent {
            fm.insert(
                "intent".to_string(),
                serde_json::Value::String(intent.clone()),
            );
        }
        fm.insert(
            "started_at".to_string(),
            serde_json::Value::String(now.clone()),
        );
        fm.insert("turns".to_string(), serde_json::Value::Number(0.into()));
        let frontmatter_json = serde_json::to_string(&fm).unwrap_or_else(|_| "{}".to_string());

        // Build initial content
        let content = format!("# Goal: {}\n", name);

        // Store in vault (this updates merkle, file_count, total_bytes in
        // the manifest but leaves the goals map to higher-level code).
        self.vault_store(
            &goal_file,
            VaultEntryType::Session,
            content.into_bytes(),
            frontmatter_json,
        )?;

        // Update manifest with goal summary
        {
            let mut txn = self
                .pristine
                .write_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            let mut manifest = txn
                .get_vault_manifest()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;

            manifest.goals.insert(
                name.clone(),
                GoalSummary {
                    developer,
                    intent: options.intent.clone(),
                    status: "active".to_string(),
                    started_at: now.clone(),
                    turns: 0,
                    tokens: TokenCounts::default(),
                    tool_results: Vec::new(),
                    findings_hash: None,
                    bytes: 0,
                },
            );

            txn.put_vault_manifest(&manifest)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            txn.commit()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }

        // Materialize to disk
        self.vault_materialize(&goal_file)?;

        Ok(GoalStartResult {
            name,
            goal_dir,
            goal_file,
        })
    }

    /// Stop a vault goal.
    ///
    /// With `promote: true`: marks the goal as "completed".
    /// With `discard: true`: deletes the goal entirely.
    /// Default (neither flag): marks as "suspended".
    ///
    /// This is a pure vault operation — it does NOT switch or delete views.
    /// View management is the agent's responsibility via the AtomicTool.
    pub fn vault_goal_stop(
        &self,
        goal_name: &str,
        options: GoalStopOptions,
    ) -> Result<GoalStopResult, RepositoryError> {
        let goal_file = format!("goals/{}/_goal.md", goal_name);

        let entry =
            self.vault_retrieve(&goal_file)?
                .ok_or_else(|| RepositoryError::InvalidOperation {
                    message: format!("Goal '{}' not found", goal_name),
                })?;

        // Parse frontmatter
        let fm: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&entry.frontmatter_json).unwrap_or_default();

        if options.discard {
            // Delete all entries under this goal directory
            let prefix = format!("goals/{}/", goal_name);
            let entries = self.vault_list(&prefix, None)?;
            let count = entries.len();
            for meta in &entries {
                self.vault_delete(&meta.path)?;
            }

            // Remove from manifest
            {
                let mut txn = self
                    .pristine
                    .write_txn()
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
                let mut manifest = txn
                    .get_vault_manifest()
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
                manifest.goals.remove(goal_name);
                txn.put_vault_manifest(&manifest)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
                txn.commit()
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
            }

            // Delete from disk
            let disk_dir = self.vault_dir().join("goals").join(goal_name);
            if disk_dir.exists() {
                std::fs::remove_dir_all(&disk_dir)?;
            }

            return Ok(GoalStopResult {
                name: goal_name.to_string(),
                status: "discarded".to_string(),
                paths_removed: count,
            });
        }

        // Promote or suspend
        let new_status = if options.promote {
            "completed"
        } else {
            "suspended"
        };
        let now = chrono::Utc::now().to_rfc3339();

        // Update the entry's frontmatter
        let mut new_fm = fm.clone();
        new_fm.insert(
            "status".to_string(),
            serde_json::Value::String(new_status.to_string()),
        );
        new_fm.insert(
            "ended_at".to_string(),
            serde_json::Value::String(now.clone()),
        );
        let new_fm_json = serde_json::to_string(&new_fm).unwrap_or_else(|_| "{}".to_string());

        // Re-store with updated frontmatter
        self.vault_store(
            &goal_file,
            VaultEntryType::Session,
            entry.content_bytes.clone(),
            new_fm_json,
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
            if let Some(summary) = manifest.goals.get_mut(goal_name) {
                summary.status = new_status.to_string();
            }
            txn.put_vault_manifest(&manifest)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            txn.commit()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }

        // Re-materialize
        self.vault_materialize(&goal_file)?;

        Ok(GoalStopResult {
            name: goal_name.to_string(),
            status: new_status.to_string(),
            paths_removed: 0,
        })
    }

    /// Resume a suspended or completed goal.
    ///
    /// Sets the goal status back to "active" and returns the goal
    /// metadata for loading into context.
    ///
    /// This is a pure vault operation — it does NOT switch views.
    /// View management is the agent's responsibility via the AtomicTool.
    pub fn vault_goal_resume(&self, goal_name: &str) -> Result<GoalInfo, RepositoryError> {
        let goal_file = format!("goals/{}/_goal.md", goal_name);

        let entry =
            self.vault_retrieve(&goal_file)?
                .ok_or_else(|| RepositoryError::InvalidOperation {
                    message: format!("Goal '{}' not found", goal_name),
                })?;

        // Parse frontmatter
        let fm: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&entry.frontmatter_json).unwrap_or_default();

        let current_status = fm
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        if current_status == "active" {
            return Err(RepositoryError::InvalidOperation {
                message: format!("Goal '{}' is already active", goal_name),
            });
        }

        // Update status to active
        let mut new_fm = fm.clone();
        new_fm.insert(
            "status".to_string(),
            serde_json::Value::String("active".to_string()),
        );
        // Remove ended_at since we're resuming
        new_fm.remove("ended_at");
        let new_fm_json = serde_json::to_string(&new_fm).unwrap_or_else(|_| "{}".to_string());

        self.vault_store(
            &goal_file,
            VaultEntryType::Session,
            entry.content_bytes.clone(),
            new_fm_json,
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
            if let Some(summary) = manifest.goals.get_mut(goal_name) {
                summary.status = "active".to_string();
            }
            txn.put_vault_manifest(&manifest)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            txn.commit()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }

        self.vault_materialize(&goal_file)?;

        let info = GoalInfo {
            name: goal_name.to_string(),
            developer: fm
                .get("developer")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            status: "active".to_string(),
            intent: fm.get("intent").and_then(|v| v.as_str()).map(String::from),
            started_at: fm
                .get("started_at")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            turns: fm.get("turns").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            tokens: TokenCounts::default(),
        };

        Ok(info)
    }

    /// List vault goals.
    ///
    /// Optionally filtered by status (`"active"`, `"completed"`,
    /// `"suspended"`, or `"all"`). Passing `None` returns all goals.
    pub fn vault_goal_list(
        &self,
        status_filter: Option<&str>,
    ) -> Result<Vec<GoalInfo>, RepositoryError> {
        let manifest = self.vault_manifest()?;
        let mut goals: Vec<GoalInfo> = Vec::new();

        for (name, summary) in &manifest.goals {
            if let Some(filter) = status_filter {
                if filter != "all" && summary.status != filter {
                    continue;
                }
            }
            goals.push(GoalInfo {
                name: name.clone(),
                developer: summary.developer.clone(),
                status: summary.status.clone(),
                intent: summary.intent.clone(),
                started_at: summary.started_at.clone(),
                turns: summary.turns,
                tokens: summary.tokens.clone(),
            });
        }

        // Sort by started_at descending (most recent first)
        goals.sort_by(|a, b| b.started_at.cmp(&a.started_at));

        Ok(goals)
    }

    /// Show a goal's full content.
    ///
    /// Returns the [`VaultEntry`] for the goal's `_goal.md` file.
    pub fn vault_goal_show(&self, goal_name: &str) -> Result<VaultEntry, RepositoryError> {
        let goal_file = format!("goals/{}/_goal.md", goal_name);
        self.vault_retrieve(&goal_file)?
            .ok_or_else(|| RepositoryError::InvalidOperation {
                message: format!("Goal '{}' not found", goal_name),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_goal_start() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        let result = repo
            .vault_goal_start(GoalStartOptions {
                developer: Some("alice".to_string()),
                intent: Some("PIMO-1".to_string()),
                ..Default::default()
            })
            .unwrap();

        assert!(!result.name.is_empty());
        assert!(result.goal_file.starts_with("goals/"));
        assert!(result.goal_file.ends_with("/_goal.md"));

        // Should be in manifest
        let manifest = repo.vault_manifest().unwrap();
        assert!(manifest.goals.contains_key(&result.name));
        assert_eq!(manifest.goals[&result.name].status, "active");
        assert_eq!(manifest.goals[&result.name].developer, "alice");

        // Should be materialized on disk
        assert!(repo.vault_dir().join(&result.goal_file).exists());
    }

    #[test]
    fn test_goal_start_custom_name() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        let result = repo
            .vault_goal_start(GoalStartOptions {
                name: Some("my-goal".to_string()),
                ..Default::default()
            })
            .unwrap();

        assert_eq!(result.name, "my-goal");
    }

    #[test]
    fn test_goal_start_duplicate_name() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        repo.vault_goal_start(GoalStartOptions {
            name: Some("unique-name".to_string()),
            ..Default::default()
        })
        .unwrap();

        let result = repo.vault_goal_start(GoalStartOptions {
            name: Some("unique-name".to_string()),
            ..Default::default()
        });
        assert!(result.is_err());
    }

    #[test]
    fn test_goal_stop_promote() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        repo.vault_goal_start(GoalStartOptions {
            name: Some("test-promote".to_string()),
            ..Default::default()
        })
        .unwrap();

        let stop = repo
            .vault_goal_stop(
                "test-promote",
                GoalStopOptions {
                    promote: true,
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(stop.status, "completed");

        // Manifest should reflect completed
        let manifest = repo.vault_manifest().unwrap();
        assert_eq!(manifest.goals["test-promote"].status, "completed");
    }

    #[test]
    fn test_goal_stop_discard() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        repo.vault_goal_start(GoalStartOptions {
            name: Some("test-discard".to_string()),
            ..Default::default()
        })
        .unwrap();

        let stop = repo
            .vault_goal_stop(
                "test-discard",
                GoalStopOptions {
                    discard: true,
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(stop.status, "discarded");

        // Should be removed from manifest
        let manifest = repo.vault_manifest().unwrap();
        assert!(!manifest.goals.contains_key("test-discard"));

        // Should be removed from redb
        assert!(repo
            .vault_retrieve("goals/test-discard/_goal.md")
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_goal_stop_suspend() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        repo.vault_goal_start(GoalStartOptions {
            name: Some("test-suspend".to_string()),
            ..Default::default()
        })
        .unwrap();

        let stop = repo
            .vault_goal_stop("test-suspend", GoalStopOptions::default())
            .unwrap();
        assert_eq!(stop.status, "suspended");

        // Manifest should reflect suspended
        let manifest = repo.vault_manifest().unwrap();
        assert_eq!(manifest.goals["test-suspend"].status, "suspended");
    }

    #[test]
    fn test_goal_resume() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        repo.vault_goal_start(GoalStartOptions {
            name: Some("test-resume".to_string()),
            developer: Some("bob".to_string()),
            ..Default::default()
        })
        .unwrap();

        // Suspend first
        repo.vault_goal_stop("test-resume", GoalStopOptions::default())
            .unwrap();

        // Resume
        let info = repo.vault_goal_resume("test-resume").unwrap();
        assert_eq!(info.name, "test-resume");
        assert_eq!(info.status, "active");
        assert_eq!(info.developer, "bob");

        // Manifest should show active
        let manifest = repo.vault_manifest().unwrap();
        assert_eq!(manifest.goals["test-resume"].status, "active");
    }

    #[test]
    fn test_goal_resume_already_active() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        repo.vault_goal_start(GoalStartOptions {
            name: Some("already-active".to_string()),
            ..Default::default()
        })
        .unwrap();

        // Resuming an active goal should fail
        assert!(repo.vault_goal_resume("already-active").is_err());
    }

    #[test]
    fn test_goal_list() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        repo.vault_goal_start(GoalStartOptions {
            name: Some("goal-a".to_string()),
            ..Default::default()
        })
        .unwrap();
        repo.vault_goal_start(GoalStartOptions {
            name: Some("goal-b".to_string()),
            ..Default::default()
        })
        .unwrap();

        // Stop one
        repo.vault_goal_stop(
            "goal-a",
            GoalStopOptions {
                promote: true,
                ..Default::default()
            },
        )
        .unwrap();

        // List all
        let all = repo.vault_goal_list(Some("all")).unwrap();
        assert_eq!(all.len(), 2);

        // List active only
        let active = repo.vault_goal_list(Some("active")).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].name, "goal-b");

        // List completed
        let completed = repo.vault_goal_list(Some("completed")).unwrap();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].name, "goal-a");
    }

    #[test]
    fn test_goal_show() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        repo.vault_goal_start(GoalStartOptions {
            name: Some("show-me".to_string()),
            ..Default::default()
        })
        .unwrap();

        let entry = repo.vault_goal_show("show-me").unwrap();
        assert_eq!(entry.entry_type, VaultEntryType::Session);
        let content = String::from_utf8_lossy(&entry.content_bytes);
        assert!(content.contains("# Goal: show-me"));
    }

    #[test]
    fn test_goal_show_not_found() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        assert!(repo.vault_goal_show("nonexistent").is_err());
    }

    #[test]
    fn test_goal_frontmatter_has_core_fields() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        repo.vault_goal_start(GoalStartOptions {
            name: Some("fm-check".to_string()),
            developer: Some("alice".to_string()),
            intent: Some("PIMO-1".to_string()),
            model: Some("claude-4".to_string()),
        })
        .unwrap();

        let entry = repo.vault_goal_show("fm-check").unwrap();
        let fm: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&entry.frontmatter_json).unwrap();

        assert_eq!(fm.get("goal_id").and_then(|v| v.as_str()), Some("fm-check"),);
        assert_eq!(fm.get("developer").and_then(|v| v.as_str()), Some("alice"),);
        assert_eq!(fm.get("intent").and_then(|v| v.as_str()), Some("PIMO-1"),);
        assert_eq!(fm.get("model").and_then(|v| v.as_str()), Some("claude-4"),);
        assert_eq!(fm.get("status").and_then(|v| v.as_str()), Some("active"),);
        assert!(fm.get("started_at").is_some());

        // View fields should NOT be in frontmatter (views are agent-managed)
        assert!(fm.get("view").is_none());
        assert!(fm.get("previous_view").is_none());
    }
}
