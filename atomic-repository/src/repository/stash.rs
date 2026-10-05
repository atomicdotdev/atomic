//! Stash: temporary storage of uncommitted working-copy changes.
//!
//! Stashes are **orphan views** paired with a raw-bytes sidecar under
//! `.atomic/stashes/<view-name>/`. The sidecar holds the exact bytes of
//! every stashed file plus a `MANIFEST` listing them; applying a stash is
//! a pure filesystem copy back onto the working copy — completely immune
//! to graph mutations between push and pop.
//!
//! This module is the ONE code path for the stash flow: the CLI command
//! and the daemon's stash RPCs both call it (the shared-core pattern from
//! `provenance_core`). It never goes through the graph/record/apply
//! machinery.
//!
//! Stash view names encode their metadata:
//! `stash/{safe_source}_{timestamp_ms}_{safe_message}` — `list` parses
//! them back out.

use chrono::{DateTime, Utc};

use crate::Repository;

/// Prefix for stash view names.
pub const STASH_PREFIX: &str = "stash/";

/// Default message for stashes without a custom message.
pub const DEFAULT_STASH_MESSAGE: &str = "WIP";

/// One stash: an orphan view plus its parsed metadata.
#[derive(Debug, Clone)]
pub struct StashEntry {
    /// Index in the newest-first listing (`stash@{index}`).
    pub index: usize,
    /// The full stash view name (the wire's stash id).
    pub view_name: String,
    /// The view the stash was pushed from.
    pub source_view: String,
    /// The stash message.
    pub message: String,
    /// Creation time (parsed from the name, UTC).
    pub created_at: DateTime<Utc>,
}

impl StashEntry {
    /// The display reference: `stash@{index}`.
    pub fn reference(&self) -> String {
        format!("stash@{{{}}}", self.index)
    }

    /// Format the stash entry for display:
    /// `stash@{0}: On dev: Fix authentication bug`.
    pub fn display(&self) -> String {
        format!(
            "{}: On {}: {}",
            self.reference(),
            self.source_view,
            self.message
        )
    }
}

impl std::fmt::Display for StashEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.display())
    }
}

/// Options for a stash push.
#[derive(Debug, Clone, Default)]
pub struct StashPushOptions {
    /// Message describing the stashed changes.
    pub message: Option<String>,
    /// Include untracked files in the stash.
    pub include_untracked: bool,
    /// Keep the changes in the working copy after stashing.
    pub keep: bool,
}

impl Repository {
    /// Push a new stash from the dirty working copy. Returns `Ok(None)`
    /// when the working copy is clean (never an error).
    pub fn stash_push(&mut self, options: StashPushOptions) -> Result<Option<StashEntry>, String> {
        use crate::status::StatusOptions;
        let status = self
            .status(StatusOptions::default())
            .map_err(|e| e.to_string())?;
        if status.is_clean() {
            return Ok(None);
        }

        let source_view = self.current_view().to_string();
        let stash_message = options
            .message
            .unwrap_or_else(|| DEFAULT_STASH_MESSAGE.to_string());

        // Stash view name with embedded metadata (verbatim from the CLI).
        let timestamp = Utc::now().timestamp_millis();
        let safe_source = source_view.replace('/', "-");
        let safe_message = stash_message
            .chars()
            .take(30)
            .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_' || *c == ' ')
            .collect::<String>()
            .replace(' ', "-");
        let stash_name = format!(
            "{}{}_{}_{}",
            STASH_PREFIX, safe_source, timestamp, safe_message
        );

        self.create_view(&stash_name).map_err(|e| e.to_string())?;

        // Save the exact bytes of every dirty file to the sidecar dir —
        // a pure working-copy snapshot, no graph interaction.
        let stash_dir = self.dot_dir().join("stashes").join(&stash_name);
        std::fs::create_dir_all(&stash_dir)
            .map_err(|e| format!("failed to create stash dir: {e}"))?;

        let mut stashed_paths: Vec<String> = Vec::new();
        let snapshot_path = |p: String| -> bool {
            let abs = self.root().join(&p);
            if abs.is_file() {
                let rel_dir = stash_dir.join(
                    std::path::Path::new(&p)
                        .parent()
                        .unwrap_or(std::path::Path::new("")),
                );
                let _ = std::fs::create_dir_all(&rel_dir);
                let _ = std::fs::copy(&abs, stash_dir.join(&p));
                true
            } else {
                false
            }
        };

        for entry in status.modified() {
            let p = entry.path().to_string_lossy().to_string();
            if snapshot_path(p) {
                stashed_paths.push(entry.path().to_string_lossy().to_string());
            }
        }
        for entry in status.added() {
            let p = entry.path().to_string_lossy().to_string();
            if snapshot_path(p) {
                stashed_paths.push(entry.path().to_string_lossy().to_string());
            }
        }
        if options.include_untracked {
            for entry in status.untracked() {
                let p = entry.path().to_string_lossy().to_string();
                if snapshot_path(p) {
                    stashed_paths.push(entry.path().to_string_lossy().to_string());
                }
            }
        }

        // The manifest records which files were stashed.
        std::fs::write(stash_dir.join("MANIFEST"), stashed_paths.join("\n"))
            .map_err(|e| format!("failed to write stash manifest: {e}"))?;

        // Restore the working copy to a clean state (unless keep).
        if !options.keep {
            self.materialize().map_err(|e| e.to_string())?;
        }

        Ok(Some(StashEntry {
            index: 0,
            view_name: stash_name,
            source_view,
            message: stash_message,
            created_at: Utc::now(),
        }))
    }

    /// List all stashes, newest first, with display indices assigned.
    pub fn stash_list(&self) -> Result<Vec<StashEntry>, String> {
        let views = self.list_views().map_err(|e| e.to_string())?;

        let mut stashes: Vec<StashEntry> = views
            .into_iter()
            .filter(|name| name.starts_with(STASH_PREFIX))
            .map(|name| {
                // Parse stash metadata from the view name (verbatim from
                // the CLI): stash/{source}_{timestamp}_{message}.
                let parts: Vec<&str> = name
                    .trim_start_matches(STASH_PREFIX)
                    .splitn(3, '_')
                    .collect();

                let (source_view, message, timestamp) = if parts.len() >= 2 {
                    let ts: i64 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
                    let src = if parts[0] == "auto" {
                        "unknown".to_string()
                    } else {
                        parts[0].to_string()
                    };
                    let msg = parts
                        .get(2)
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| DEFAULT_STASH_MESSAGE.to_string());
                    (src, msg, ts)
                } else {
                    ("unknown".to_string(), DEFAULT_STASH_MESSAGE.to_string(), 0)
                };

                let created_at =
                    DateTime::from_timestamp_millis(timestamp).unwrap_or_else(Utc::now);

                StashEntry {
                    index: 0,
                    view_name: name,
                    source_view,
                    message,
                    created_at,
                }
            })
            .collect();

        // Sort by creation time, newest first, and assign indices.
        stashes.sort_by_key(|s| std::cmp::Reverse(s.created_at));
        for (i, stash) in stashes.iter_mut().enumerate() {
            stash.index = i;
        }
        Ok(stashes)
    }

    /// Resolve a stash reference (`stash@{N}`, `N`, or a view name) to
    /// its entry. `None` resolves to the most recent stash.
    pub fn stash_resolve(&self, reference: Option<&str>) -> Result<StashEntry, String> {
        let stashes = self.stash_list()?;
        if stashes.is_empty() {
            return Err("No stashes found".to_string());
        }

        let reference = reference.unwrap_or("0");
        let index = if reference.starts_with("stash@{") && reference.ends_with('}') {
            reference[7..reference.len() - 1].parse::<usize>().ok()
        } else {
            reference.parse::<usize>().ok()
        };
        if let Some(idx) = index {
            return stashes
                .into_iter()
                .find(|s| s.index == idx)
                .ok_or_else(|| format!("stash@{{{idx}}} does not exist"));
        }

        let full_name = if reference.starts_with(STASH_PREFIX) {
            reference.to_string()
        } else {
            format!("{STASH_PREFIX}{reference}")
        };
        stashes
            .into_iter()
            .find(|s| s.view_name == full_name)
            .ok_or_else(|| format!("stash '{reference}' not found"))
    }

    /// Apply a stash to the working copy: copy the sidecar bytes back.
    /// Pure filesystem operation — no graph interaction. Returns the
    /// applied paths.
    pub fn stash_apply(
        &self,
        reference: Option<&str>,
    ) -> Result<(StashEntry, Vec<String>), String> {
        let stash = self.stash_resolve(reference)?;
        let stash_dir = self.dot_dir().join("stashes").join(&stash.view_name);
        let manifest_path = stash_dir.join("MANIFEST");
        if !manifest_path.exists() {
            return Err(format!(
                "stash sidecar not found: {}",
                manifest_path.display()
            ));
        }
        let manifest = std::fs::read_to_string(&manifest_path)
            .map_err(|e| format!("failed to read stash manifest: {e}"))?;

        let mut applied: Vec<String> = Vec::new();
        for line in manifest.lines() {
            let path = line.trim();
            if path.is_empty() {
                continue;
            }
            let src = stash_dir.join(path);
            let dst = self.root().join(path);
            if src.is_file() {
                if let Some(parent) = dst.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if std::fs::copy(&src, &dst).is_ok() {
                    applied.push(path.to_string());
                }
            }
        }
        Ok((stash, applied))
    }

    /// Drop one stash (sidecar + view). Returns the dropped entry.
    pub fn stash_drop(&mut self, reference: Option<&str>) -> Result<StashEntry, String> {
        let stash = self.stash_resolve(reference)?;
        let stash_dir = self.dot_dir().join("stashes").join(&stash.view_name);
        if stash_dir.exists() {
            let _ = std::fs::remove_dir_all(&stash_dir);
        }
        self.delete_view(&stash.view_name)
            .map_err(|e| e.to_string())?;
        Ok(stash)
    }

    /// Drop every stash. Returns the dropped count.
    pub fn stash_clear(&mut self) -> Result<usize, String> {
        let stashes = self.stash_list()?;
        for stash in &stashes {
            let stash_dir = self.dot_dir().join("stashes").join(&stash.view_name);
            if stash_dir.exists() {
                let _ = std::fs::remove_dir_all(&stash_dir);
            }
            self.delete_view(&stash.view_name)
                .map_err(|e| e.to_string())?;
        }
        Ok(stashes.len())
    }
}
