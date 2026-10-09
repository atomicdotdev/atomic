//! Stash: temporary storage of uncommitted working-copy changes.
//!
//! Stashes are **orphan views** paired with a raw-bytes sidecar under
//! `.atomic/stashes/<view-name>/`. The sidecar holds the exact bytes of
//! every stashed file plus a `MANIFEST` listing files and deletions. Applying
//! a stash restores working-copy operations without changing the graph.
//! Deletions are fenced by the original recorded content to avoid deleting
//! a file edited since the stash was saved.
//!
//! This module is the ONE code path for the stash flow: the CLI command
//! and the daemon's stash RPCs both call it (the shared-core pattern from
//! `provenance_core`). It never goes through the graph/record/apply
//! machinery.
//!
//! Stash view names encode their metadata:
//! `stash/{safe_source}_{timestamp_ms}_{safe_message}` — `list` parses
//! them back out.

use atomic_core::types::{Base32, Hash};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::Repository;

/// Prefix for stash view names.
pub const STASH_PREFIX: &str = "stash/";

/// Default message for stashes without a custom message.
pub const DEFAULT_STASH_MESSAGE: &str = "WIP";

// NUL cannot occur in a filename, so no legacy path list can match this header.
const MANIFEST_V2: &str = "\0atomic-stash-v2\n";

#[derive(Serialize, Deserialize)]
struct StashManifest {
    files: Vec<String>,
    deleted: Vec<StashedDeletion>,
}

#[derive(Serialize, Deserialize)]
struct StashedDeletion {
    path: String,
    recorded_hash: String,
}

// A deletion is a working-copy operation, not a new claim about graph paths.
// Refuse malformed paths and symlink traversal before deleting anything.
fn deletion_path(root: &std::path::Path, path: &str) -> Result<std::path::PathBuf, String> {
    use std::path::Component;
    let relative = std::path::Path::new(path);
    if path.is_empty()
        || relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        || relative.starts_with(".atomic")
        || relative.starts_with(".atomic-sandbox")
    {
        return Err(format!("invalid stashed deletion path: {path}"));
    }
    let mut destination = root.to_path_buf();
    for component in relative.components() {
        destination.push(component);
        match std::fs::symlink_metadata(&destination) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(format!("refusing stashed deletion through symlink: {path}"));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("cannot inspect stashed deletion {path}: {e}")),
        }
    }
    Ok(destination)
}

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

        let deleted = status
            .deleted()
            .map(|entry| {
                let path = entry.path().to_string_lossy().to_string();
                let content = self
                    .get_file_content_on_view(entry.path(), &source_view)
                    .map_err(|e| format!("cannot read deleted file {path}: {e}"))?
                    .ok_or_else(|| format!("no recorded content for deleted file {path}"))?;
                Ok(StashedDeletion {
                    path,
                    recorded_hash: Hash::of(&content).to_base32(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let mut stashed_paths: Vec<String> = Vec::new();
        let snapshot_path = |p: String| -> Result<(), String> {
            let abs = self.root().join(&p);
            let rel_dir = stash_dir.join(
                std::path::Path::new(&p)
                    .parent()
                    .unwrap_or(std::path::Path::new("")),
            );
            std::fs::create_dir_all(&rel_dir)
                .map_err(|e| format!("cannot create stash directory for {p}: {e}"))?;
            std::fs::copy(&abs, stash_dir.join(&p))
                .map_err(|e| format!("cannot stash {p}: {e}"))?;
            Ok(())
        };

        for entry in status.modified() {
            let p = entry.path().to_string_lossy().to_string();
            snapshot_path(p.clone())?;
            stashed_paths.push(p);
        }
        for entry in status.added() {
            let p = entry.path().to_string_lossy().to_string();
            snapshot_path(p.clone())?;
            stashed_paths.push(p);
        }
        if options.include_untracked {
            for entry in status.untracked() {
                let p = entry.path().to_string_lossy().to_string();
                snapshot_path(p.clone())?;
                stashed_paths.push(p);
            }
        }

        // Keep the legacy file-only format readable. Deletion-aware stashes
        // need explicit operations; an absent payload must never mean delete.
        let manifest = if deleted.is_empty() {
            stashed_paths.join("\n")
        } else {
            format!(
                "{MANIFEST_V2}{}",
                serde_json::to_string(&StashManifest {
                    files: stashed_paths,
                    deleted,
                })
                .map_err(|e| format!("cannot encode stash manifest: {e}"))?
            )
        };
        std::fs::write(stash_dir.join("MANIFEST"), manifest)
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

    /// Apply a stash to the working copy: restore bytes and recorded deletions.
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

        let manifest = if let Some(json) = manifest.strip_prefix(MANIFEST_V2) {
            serde_json::from_str::<StashManifest>(json)
                .map_err(|e| format!("invalid stash manifest: {e}"))?
        } else {
            StashManifest {
                files: manifest
                    .lines()
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .map(str::to_string)
                    .collect(),
                deleted: Vec::new(),
            }
        };

        // Validate every deletion before modifying files. A failed apply must
        // leave the stash available for recovery, rather than let pop drop it.
        let mut deletions = Vec::new();
        for deletion in &manifest.deleted {
            let dst = deletion_path(self.root(), &deletion.path)?;
            match std::fs::read(&dst) {
                Ok(content) if Hash::of(&content).to_base32() == deletion.recorded_hash => {}
                Ok(_) => {
                    let path = &deletion.path;
                    return Err(format!(
                        "cannot restore stashed deletion of '{path}': file changed since it was stashed"
                    ));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(format!(
                        "cannot restore stashed deletion of '{}': {e}",
                        deletion.path
                    ))
                }
            }
            deletions.push((deletion.path.clone(), dst));
        }

        let mut applied: Vec<String> = Vec::new();
        for path in &manifest.files {
            let src = stash_dir.join(path);
            let dst = self.root().join(path);
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("cannot restore stashed file {path}: {e}"))?;
            }
            std::fs::copy(&src, &dst)
                .map_err(|e| format!("cannot restore stashed file {path}: {e}"))?;
            applied.push(path.to_string());
        }
        for (path, dst) in deletions {
            match std::fs::remove_file(&dst) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("cannot restore stashed deletion of '{path}': {e}")),
            }
            applied.push(path);
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
