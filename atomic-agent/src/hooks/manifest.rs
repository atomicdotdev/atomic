//! Data-driven hook installation from an integration-supplied manifest.
//!
//! Instead of hardcoding each agent's hook definitions in this binary, an
//! integration package (`atomic-codex`, `atomic-claude`, ...) ships a manifest
//! file describing *where* its hooks live and *what* commands to register.
//! `atomic agent enable --hooks <manifest>` merges that manifest into the
//! agent's settings file idempotently, preserving any non-Atomic hooks.
//!
//! Because the definitions live in the integration repo, updating an agent's
//! hook wiring (new event, renamed verb, schema tweak, bug fix) never requires
//! rebuilding the `atomic` binary — only re-publishing the integration package.
//! The merge engine ships with `atomic`, so installation needs no Node, `jq`,
//! `sudo`, or other runtime.
//!
//! # Manifest format
//!
//! ```json
//! {
//!   "target": "~/.codex/hooks.json",
//!   "hooks_key": "hooks",
//!   "command_prefix": "atomic agent hooks codex",
//!   "hooks": {
//!     "SessionStart": [
//!       { "hooks": [ { "type": "command", "command": "test -d .atomic || test -f .atomic-sandbox && atomic agent hooks codex session-start || true", "statusMessage": "Atomic: tracking session" } ] }
//!     ],
//!     "Stop": [
//!       { "hooks": [ { "type": "command", "command": "test -d .atomic || test -f .atomic-sandbox && atomic agent hooks codex stop || true" } ] }
//!     ]
//!   }
//! }
//! ```
//!
//! - `target` — destination settings file (`~` is expanded to the home dir).
//! - `hooks_key` — top-level key under which hooks live (default `"hooks"`).
//! - `command_prefix` — substring identifying this integration's own hook
//!   commands. Entries matching it are removed before re-adding, so re-running
//!   syncs the target to the manifest (picks up command/event changes).
//! - `hooks` — `event -> [group]`, in the target file's native shape. Groups
//!   are merged verbatim, so agent-specific fields (e.g. `matcher`) are kept.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::error::{AgentError, AgentResult};

fn default_hooks_key() -> String {
    "hooks".to_string()
}

/// A hook manifest shipped by an integration package.
#[derive(Debug, Deserialize)]
pub struct HookManifest {
    /// Destination settings file, e.g. `~/.codex/hooks.json`.
    pub target: String,

    /// JSON key under which hooks are stored in the target file.
    #[serde(default = "default_hooks_key")]
    pub hooks_key: String,

    /// Substring identifying this integration's own hook commands.
    pub command_prefix: String,

    /// `event -> [entry]`, in the target file's native shape. Entries are
    /// merged verbatim. Both nested (`{matcher?, hooks: [{command}]}`) and flat
    /// (`{command}` / `{bash}`) entry shapes are supported.
    #[serde(default)]
    pub hooks: Map<String, Value>,

    /// Extra non-hook settings to deep-merge into the target's root object
    /// (e.g. Claude's `permissions.deny` rule). Objects are merged
    /// recursively, arrays are unioned by value, scalars overwrite. Left in
    /// place on uninstall.
    #[serde(default)]
    pub merge: Map<String, Value>,
}

/// Outcome of installing or uninstalling a manifest.
#[derive(Debug, Clone)]
pub struct ManifestOutcome {
    /// Resolved destination file.
    pub target: PathBuf,
    /// Number of hook commands added.
    pub added: usize,
    /// Number of stale Atomic hook commands removed.
    pub removed: usize,
}

/// Merge the manifest's hooks into its target settings file.
///
/// Existing entries whose command contains the manifest's `command_prefix` are
/// removed first, so re-running syncs the target to the manifest (picking up
/// command/event changes). Non-matching hooks are preserved untouched.
pub fn install_from_manifest(manifest_path: &Path) -> AgentResult<ManifestOutcome> {
    let manifest = read_manifest(manifest_path)?;
    let target = expand_target(&manifest.target)?;

    let mut root = read_json_object(&target)?;

    let entry = root
        .entry(manifest.hooks_key.clone())
        .or_insert_with(|| Value::Object(Map::new()));
    if !entry.is_object() {
        *entry = Value::Object(Map::new());
    }
    let hooks = entry
        .as_object_mut()
        .expect("entry was just ensured to be an object");

    // Remove our previous entries first so command/event changes take effect.
    let removed = remove_prefixed(hooks, &manifest.command_prefix);

    let mut added = 0;
    for (event, entries) in &manifest.hooks {
        let Some(src_entries) = entries.as_array() else {
            continue;
        };
        let dst = hooks
            .entry(event.clone())
            .or_insert_with(|| Value::Array(Vec::new()));
        let Some(dst_entries) = dst.as_array_mut() else {
            continue;
        };
        for entry in src_entries {
            dst_entries.push(entry.clone());
            added += count_commands(entry);
        }
    }

    // Deep-merge any extra non-hook settings (e.g. Claude's permissions.deny).
    deep_merge(&mut root, &manifest.merge);

    write_json_object(&target, &root)?;

    Ok(ManifestOutcome {
        target,
        added,
        removed,
    })
}

/// Remove the manifest's hooks from its target settings file.
///
/// Only entries whose command contains the manifest's `command_prefix` are
/// removed; the file and all other hooks are left intact.
pub fn uninstall_from_manifest(manifest_path: &Path) -> AgentResult<ManifestOutcome> {
    let manifest = read_manifest(manifest_path)?;
    let target = expand_target(&manifest.target)?;

    if !target.exists() {
        return Ok(ManifestOutcome {
            target,
            added: 0,
            removed: 0,
        });
    }

    let mut root = read_json_object(&target)?;
    let mut removed = 0;
    if let Some(hooks) = root
        .get_mut(&manifest.hooks_key)
        .and_then(Value::as_object_mut)
    {
        removed = remove_prefixed(hooks, &manifest.command_prefix);
    }

    write_json_object(&target, &root)?;

    Ok(ManifestOutcome {
        target,
        added: 0,
        removed,
    })
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn read_manifest(path: &Path) -> AgentResult<HookManifest> {
    let content = std::fs::read_to_string(path).map_err(|e| AgentError::ConfigError {
        operation: "read manifest".to_string(),
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;
    serde_json::from_str(&content).map_err(|e| AgentError::ConfigError {
        operation: "parse manifest".to_string(),
        path: path.to_path_buf(),
        reason: e.to_string(),
    })
}

fn expand_target(target: &str) -> AgentResult<PathBuf> {
    if target == "~" {
        return dirs::home_dir().ok_or_else(|| AgentError::ConfigError {
            operation: "resolve home".to_string(),
            path: PathBuf::from(target),
            reason: "could not determine home directory".to_string(),
        });
    }
    if let Some(rest) = target.strip_prefix("~/") {
        let home = dirs::home_dir().ok_or_else(|| AgentError::ConfigError {
            operation: "resolve home".to_string(),
            path: PathBuf::from(target),
            reason: "could not determine home directory".to_string(),
        })?;
        return Ok(home.join(rest));
    }
    Ok(PathBuf::from(target))
}

fn read_json_object(path: &Path) -> AgentResult<Map<String, Value>> {
    if !path.exists() {
        return Ok(Map::new());
    }
    let content = std::fs::read_to_string(path).map_err(|e| AgentError::ConfigError {
        operation: "read".to_string(),
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;
    if content.trim().is_empty() {
        return Ok(Map::new());
    }
    let value: Value = serde_json::from_str(&content).map_err(|e| AgentError::ConfigError {
        operation: "parse".to_string(),
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;
    match value {
        Value::Object(map) => Ok(map),
        _ => Ok(Map::new()),
    }
}

fn write_json_object(path: &Path, root: &Map<String, Value>) -> AgentResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| AgentError::ConfigError {
            operation: "create directory".to_string(),
            path: parent.to_path_buf(),
            reason: e.to_string(),
        })?;
    }
    let mut content = serde_json::to_string_pretty(root).map_err(|e| AgentError::ConfigError {
        operation: "serialize".to_string(),
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;
    content.push('\n');
    std::fs::write(path, content).map_err(|e| AgentError::ConfigError {
        operation: "write".to_string(),
        path: path.to_path_buf(),
        reason: e.to_string(),
    })
}

/// Command-carrying keys across agent schemas: Claude/Codex/Cursor use
/// `command`; Copilot uses `bash`/`powershell`.
const COMMAND_KEYS: [&str; 3] = ["command", "bash", "powershell"];

/// Whether a single (flat) hook entry's command field contains `prefix`.
fn entry_command_contains(entry: &Value, prefix: &str) -> bool {
    COMMAND_KEYS.iter().any(|key| {
        entry
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|cmd| cmd.contains(prefix))
    })
}

/// Whether a single (flat) hook entry carries any command field.
fn entry_has_command(entry: &Value) -> bool {
    COMMAND_KEYS
        .iter()
        .any(|key| entry.get(key).and_then(Value::as_str).is_some())
}

/// Remove hook entries whose command contains `prefix`. Handles both nested
/// (`{matcher?, hooks: [{command}]}`) and flat (`{command}`/`{bash}`) shapes.
/// Empty groups and empty event arrays are pruned. Returns commands removed.
fn remove_prefixed(hooks: &mut Map<String, Value>, prefix: &str) -> usize {
    let mut removed = 0;
    for value in hooks.values_mut() {
        let Some(entries) = value.as_array_mut() else {
            continue;
        };
        entries.retain_mut(|entry| {
            // Nested shape: a group with an inner `hooks` array.
            if let Some(inner) = entry.get_mut("hooks").and_then(Value::as_array_mut) {
                let before = inner.len();
                inner.retain(|hook| !entry_command_contains(hook, prefix));
                removed += before - inner.len();
                return !inner.is_empty();
            }
            // Flat shape: the entry itself carries the command.
            if entry_command_contains(entry, prefix) {
                removed += 1;
                return false;
            }
            true
        });
    }
    // Drop event keys whose entry array is now empty.
    hooks.retain(|_, v| v.as_array().map(|a| !a.is_empty()).unwrap_or(true));
    removed
}

/// Count the hook commands carried by a single entry (nested or flat).
fn count_commands(entry: &Value) -> usize {
    if let Some(inner) = entry.get("hooks").and_then(Value::as_array) {
        return inner.iter().filter(|h| entry_has_command(h)).count();
    }
    usize::from(entry_has_command(entry))
}

/// Deep-merge `extra` into `root`: objects merge recursively, arrays union by
/// value (no duplicates), scalars overwrite. Used for non-hook settings such
/// as Claude's `permissions.deny` rule.
fn deep_merge(root: &mut Map<String, Value>, extra: &Map<String, Value>) {
    for (key, value) in extra {
        match root.get_mut(key) {
            Some(existing) => merge_value(existing, value),
            None => {
                root.insert(key.clone(), value.clone());
            }
        }
    }
}

fn merge_value(target: &mut Value, source: &Value) {
    match (target, source) {
        (Value::Object(t), Value::Object(s)) => deep_merge(t, s),
        (Value::Array(t), Value::Array(s)) => {
            for item in s {
                if !t.contains(item) {
                    t.push(item.clone());
                }
            }
        }
        (t, s) => *t = s.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write(path: &Path, value: &Value) {
        std::fs::write(path, serde_json::to_string_pretty(value).unwrap()).unwrap();
    }

    fn manifest_json(target: &Path, prefix: &str) -> Value {
        json!({
            "target": target.to_string_lossy(),
            "command_prefix": prefix,
            "hooks": {
                "SessionStart": [
                    { "hooks": [ { "type": "command", "command": format!("{} session-start", prefix), "statusMessage": "Atomic" } ] }
                ],
                "Stop": [
                    { "hooks": [ { "type": "command", "command": format!("{} stop", prefix) } ] }
                ]
            }
        })
    }

    #[test]
    fn installs_into_empty_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("hooks.json");
        let manifest = dir.path().join("manifest.json");
        write(
            &manifest,
            &manifest_json(&target, "atomic agent hooks codex"),
        );

        let outcome = install_from_manifest(&manifest).unwrap();
        assert_eq!(outcome.added, 2);
        assert_eq!(outcome.removed, 0);

        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();
        assert!(written["hooks"]["SessionStart"].is_array());
        assert!(written["hooks"]["Stop"].is_array());
    }

    #[test]
    fn is_idempotent_and_syncs_changes() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("hooks.json");
        let manifest = dir.path().join("manifest.json");
        write(
            &manifest,
            &manifest_json(&target, "atomic agent hooks codex"),
        );

        install_from_manifest(&manifest).unwrap();
        // Second run removes the 2 stale entries and re-adds 2 — no duplicates.
        let outcome = install_from_manifest(&manifest).unwrap();
        assert_eq!(outcome.added, 2);
        assert_eq!(outcome.removed, 2);

        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();
        assert_eq!(
            written["hooks"]["SessionStart"].as_array().unwrap().len(),
            1
        );
    }

    #[test]
    fn preserves_non_atomic_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("hooks.json");
        // Pre-existing user hook plus unrelated top-level config.
        write(
            &target,
            &json!({
                "model": "gpt-5",
                "hooks": {
                    "Stop": [ { "hooks": [ { "type": "command", "command": "my-own-hook" } ] } ]
                }
            }),
        );
        let manifest = dir.path().join("manifest.json");
        write(
            &manifest,
            &manifest_json(&target, "atomic agent hooks codex"),
        );

        install_from_manifest(&manifest).unwrap();

        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();
        // Unrelated config preserved.
        assert_eq!(written["model"], "gpt-5");
        // User's own Stop hook preserved alongside the Atomic one.
        let stop = written["hooks"]["Stop"].as_array().unwrap();
        let commands: Vec<&str> = stop
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap())
            .map(|h| h["command"].as_str().unwrap())
            .collect();
        assert!(commands.contains(&"my-own-hook"));
        assert!(commands
            .iter()
            .any(|c| c.contains("atomic agent hooks codex stop")));
    }

    #[test]
    fn uninstall_removes_only_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("hooks.json");
        write(
            &target,
            &json!({
                "hooks": {
                    "Stop": [ { "hooks": [ { "type": "command", "command": "my-own-hook" } ] } ]
                }
            }),
        );
        let manifest = dir.path().join("manifest.json");
        write(
            &manifest,
            &manifest_json(&target, "atomic agent hooks codex"),
        );

        install_from_manifest(&manifest).unwrap();
        let outcome = uninstall_from_manifest(&manifest).unwrap();
        assert_eq!(outcome.removed, 2);

        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();
        let commands: Vec<&str> = written["hooks"]["Stop"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap())
            .map(|h| h["command"].as_str().unwrap())
            .collect();
        assert_eq!(commands, vec!["my-own-hook"]);
    }

    #[test]
    fn handles_flat_cursor_shape() {
        // Cursor: event -> [ { command } ] (flat, no inner "hooks").
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("hooks.json");
        write(
            &target,
            &json!({
                "version": 1,
                "hooks": {
                    "sessionStart": [ { "command": "my-own-cursor-hook" } ]
                }
            }),
        );
        let manifest = dir.path().join("manifest.json");
        write(
            &manifest,
            &json!({
                "target": target.to_string_lossy(),
                "command_prefix": "atomic agent hooks cursor",
                "hooks": {
                    "sessionStart": [ { "command": "atomic agent hooks cursor session-start" } ],
                    "stop": [ { "command": "atomic agent hooks cursor stop" } ]
                }
            }),
        );

        let outcome = install_from_manifest(&manifest).unwrap();
        assert_eq!(outcome.added, 2);

        // Idempotent: re-run replaces our 2, keeps the user's hook.
        let again = install_from_manifest(&manifest).unwrap();
        assert_eq!(again.added, 2);
        assert_eq!(again.removed, 2);

        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();
        assert_eq!(written["version"], 1);
        let start: Vec<&str> = written["hooks"]["sessionStart"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["command"].as_str().unwrap())
            .collect();
        assert!(start.contains(&"my-own-cursor-hook"));
        assert!(start
            .iter()
            .any(|c| c.contains("atomic agent hooks cursor session-start")));
    }

    #[test]
    fn merges_extra_settings_idempotently() {
        // Claude-style: deep-merge a permissions.deny rule alongside hooks.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("settings.json");
        write(
            &target,
            &json!({ "permissions": { "deny": ["Read(./secret/**)"] } }),
        );
        let manifest = dir.path().join("manifest.json");
        write(
            &manifest,
            &json!({
                "target": target.to_string_lossy(),
                "command_prefix": "atomic agent hooks claude-code",
                "hooks": {
                    "Stop": [ { "matcher": "", "hooks": [ { "type": "command", "command": "atomic agent hooks claude-code stop" } ] } ]
                },
                "merge": { "permissions": { "deny": ["Read(./.atomic/metadata/**)"] } }
            }),
        );

        install_from_manifest(&manifest).unwrap();
        install_from_manifest(&manifest).unwrap(); // idempotent

        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();
        let deny: Vec<&str> = written["permissions"]["deny"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        // User's rule preserved; Atomic's added exactly once.
        assert_eq!(
            deny,
            vec!["Read(./secret/**)", "Read(./.atomic/metadata/**)"]
        );
        // The nested Stop hook is registered.
        assert!(written["hooks"]["Stop"].is_array());
    }

    #[test]
    fn expand_target_handles_tilde() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(
            expand_target("~/.codex/hooks.json").unwrap(),
            home.join(".codex/hooks.json")
        );
        assert_eq!(
            expand_target("/abs/path.json").unwrap(),
            PathBuf::from("/abs/path.json")
        );
    }
}
