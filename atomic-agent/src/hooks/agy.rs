//! Antigravity CLI (`agy`) hook adapter for Atomic Agent.
//!
//! This module implements the [`AgentHook`] trait for Google's Antigravity
//! CLI — the successor to the deprecated Gemini CLI — handling:
//!
//! - **JSON parsing** of hook callbacks from stdin
//! - **Hook installation** as an Antigravity *plugin*
//! - **Hook removal** preserving non-Atomic hook groups
//! - **Presence detection** via the `.agents/` directory
//!
//! # Why a Plugin, Not `.agents/hooks.json`
//!
//! Antigravity's docs describe project hooks in `.agents/hooks.json`, but in
//! CLI 1.1.4 a project-level file is only *loaded* (surfaced in the `/hooks`
//! panel) — its handlers never *fire*. Hooks delivered through the plugin
//! mechanism (`~/.gemini/config/plugins/<name>/hooks.json`, the same layout
//! `agy plugin install` produces) are registered and executed. Atomic
//! therefore installs its hooks as a plugin named `atomic`:
//!
//! ```text
//! ~/.gemini/config/
//! ├── import_manifest.json        # agy's plugin registry (entry upserted)
//! └── plugins/
//!     └── atomic/
//!         ├── plugin.json         # manifest (created if missing)
//!         ├── hooks.json          # the hooks below
//!         └── skills/             # code-intelligence + atomic-vault
//! ```
//!
//! Plugins are global to the user, so project-level and `--global`
//! installation are the same operation for this adapter.
//!
//! # Bundled Skills
//!
//! The plugin ships Atomic's canonical vault skills (the same files seeded
//! into `.vault/skills/` on every repository) so the agent can learn the
//! knowledge-graph-first code discovery workflow
//! (`atomic vault query search` / `neighbors`) instead of grepping and
//! reading whole files. Antigravity imports plugin skills and exposes them
//! to the agent for task-based selection.
//!
//! # Agent Instruction File
//!
//! The agent-facing instruction file (VCS rules, intent workflow, recording
//! rules) is **required** for the integration to work as designed — without
//! it the agent falls back to grep-and-read and git. Its canonical copy is
//! `atomic-repository/vault/AGENTS.md`, a plain markdown file mirrored as
//! `AGENTS.md` in the atomic-agy integration repository. On install, this
//! adapter writes it into the project's `AGENTS.md` as a managed section
//! (`<!-- atomic:tools:start/end -->` markers) and refreshes it on rerun
//! when the canonical content changes; `uninstall` removes it.
//!
//! # Hook Execution Environment (Important)
//!
//! Antigravity runs plugin hooks with the **plugin directory** as the
//! working directory — not the workspace. A `test -d .atomic` shell guard
//! can therefore never work. Instead, the installed commands are bare
//! `atomic agent hooks agy <verb>` invocations, and the hook handler
//! resolves the repository from the `workspacePaths` field present in every
//! hook payload (see [`AgentHook::repo_root_hints`]).
//!
//! # Hook Events
//!
//! | Antigravity Hook | Atomic HookType | Key Input Fields                              |
//! |------------------|-----------------|-----------------------------------------------|
//! | `PreInvocation`  | TurnStart       | `conversationId`, `transcriptPath`, `invocationNum` |
//! | `Stop`           | TurnEnd         | `conversationId`, `terminationReason`, `fullyIdle`  |
//! | `PostToolUse`    | PostToolUse     | `conversationId`, `stepIdx`, `error`          |
//!
//! `PreToolUse` is intentionally **not** installed: Antigravity requires a
//! `decision` in the hook's stdout response (`allow`/`deny`/`ask`/`force_ask`)
//! and any value Atomic emitted would override the user's own permission
//! policy. `PostInvocation` duplicates `Stop` for provenance purposes and is
//! also skipped.
//!
//! `PreInvocation` fires before *every* model call (several per user prompt),
//! not once per prompt. The orchestrator tolerates repeated `TurnStart`
//! events on an active session, so this is harmless.
//!
//! # Hooks File Format
//!
//! `hooks.json` maps named hook groups to event configurations. Tool events
//! use matcher groups; lifecycle events take a direct list of handlers:
//!
//! ```json
//! {
//!   "atomic": {
//!     "PreInvocation": [
//!       { "type": "command", "command": "atomic agent hooks agy pre-invocation || true" }
//!     ],
//!     "Stop": [
//!       { "type": "command", "command": "atomic agent hooks agy stop || true" }
//!     ],
//!     "PostToolUse": [
//!       {
//!         "matcher": "",
//!         "hooks": [
//!           { "type": "command", "command": "atomic agent hooks agy post-tool-use || true" }
//!         ]
//!       }
//!     ]
//!   }
//! }
//! ```
//!
//! # Stdout Contract
//!
//! Antigravity reads a JSON object from hook stdout. Atomic responds with
//! `{}` for every installed hook: it satisfies the `PostToolUse` contract
//! verbatim and is a no-op for `PreInvocation` (`injectSteps` optional) and
//! `Stop` (any `decision` other than `"continue"` — including a missing
//! field — lets the agent stop). See [`AgentHook::stdout_response`].
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_agent::hooks::agy::AgyHook;
//! use atomic_agent::hooks::AgentHook;
//! use atomic_agent::event::HookType;
//!
//! let hook = AgyHook::new();
//! assert_eq!(hook.name(), "agy");
//! assert_eq!(hook.display_name(), "Antigravity CLI");
//!
//! let input = br#"{"conversationId": "abc-123", "transcriptPath": "/tmp/t.jsonl", "workspacePaths": ["/repo"], "invocationNum": 0}"#;
//! let event = hook.parse_event(HookType::TurnStart, input).unwrap();
//! assert_eq!(event.session_id, "abc-123");
//! ```

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::error::{AgentError, AgentResult};
use crate::event::{HookType, TurnEvent};
use crate::hooks::AgentHook;

// Constants

/// Command prefix used to identify Atomic hooks in the hooks file.
const ATOMIC_HOOK_PREFIX: &str = "atomic agent hooks agy";

/// Name of the hook group (and plugin) Atomic installs its hooks under.
const ATOMIC_GROUP: &str = "atomic";

/// The plugin directory name under `~/.gemini/config/plugins/`.
const PLUGIN_DIR_NAME: &str = "atomic";

/// The hooks file inside the plugin directory.
const HOOKS_FILE: &str = "hooks.json";

/// The plugin manifest inside the plugin directory.
const PLUGIN_MANIFEST_FILE: &str = "plugin.json";

/// agy's plugin registry, at `~/.gemini/config/import_manifest.json`.
const IMPORT_MANIFEST_FILE: &str = "import_manifest.json";

/// The workspace customization directory used for presence detection.
///
/// agy reads workspace skills and MCP config from `.agents/`; users who
/// customize agy for a project create it. (Hooks are *not* installed here —
/// see the module docs for why.)
const AGENTS_DIR: &str = ".agents";

/// The agent context file agy reads from the workspace root.
const AGENTS_MD_FILE: &str = "AGENTS.md";

/// Start marker for the managed Atomic section in `AGENTS.md`.
const SECTION_START: &str = "<!-- atomic:tools:start -->";

/// End marker for the managed Atomic section in `AGENTS.md`.
const SECTION_END: &str = "<!-- atomic:tools:end -->";

/// The agent instruction file — VCS rules, intent workflow, recording
/// rules, skills list.
///
/// The canonical copy lives at `atomic-repository/vault/AGENTS.md` (and is
/// mirrored as `AGENTS.md` in the atomic-agy integration repository) —
/// a plain markdown file, not embedded prose. It is written into the
/// project's `AGENTS.md` between the markers above so the agent gets the
/// standard workflow on `atomic agent enable`, and refreshed on rerun when
/// the canonical content changes. User content outside the markers is
/// preserved.
const SECTION_BODY: &str = include_str!("../../../atomic-repository/vault/AGENTS.md");

/// The plugin manifest written alongside `hooks.json`.
const PLUGIN_MANIFEST: &str = r#"{
  "$schema": "https://antigravity.google/schemas/v1/plugin.json",
  "name": "atomic",
  "description": "Record every agent turn as an Atomic change with full AI provenance."
}
"#;

/// Skills bundled into the plugin so the agent learns Atomic's code
/// discovery workflow (knowledge-graph-first navigation).
///
/// These are the same canonical files the vault seeds into `.vault/skills/`
/// on every repository — sourced via `include_str!` so the content never
/// drifts. Antigravity imports plugin skills and exposes them to the agent
/// (as slash commands and for task-based auto-selection).
const PLUGIN_SKILLS: &[(&str, &str)] = &[
    (
        "code-intelligence.md",
        include_str!("../../../atomic-repository/vault/skills/code-intelligence.md"),
    ),
    (
        "atomic-vault.md",
        include_str!("../../../atomic-repository/vault/skills/atomic-vault.md"),
    ),
    (
        "atomic-vcs.md",
        include_str!("../../../atomic-repository/vault/skills/atomic-vcs.md"),
    ),
];

// Antigravity JSON Input Types

/// Base input fields present in every Antigravity hook callback.
///
/// All hooks receive these camelCase fields via stdin JSON.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct BaseInput {
    #[serde(default, rename = "conversationId")]
    conversation_id: Option<String>,
    #[serde(default, rename = "transcriptPath")]
    transcript_path: Option<String>,
    #[serde(default, rename = "artifactDirectoryPath")]
    artifact_directory_path: Option<String>,
    #[serde(default, rename = "workspacePaths")]
    workspace_paths: Option<Vec<String>>,
}

/// JSON input for the `PreInvocation` hook (TurnStart).
///
/// Fires before each model invocation. `invocationNum` is 0 for the first
/// invocation of an execution loop.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct PreInvocationInput {
    #[serde(default, rename = "conversationId")]
    conversation_id: Option<String>,
    #[serde(default, rename = "transcriptPath")]
    transcript_path: Option<String>,
    #[serde(default, rename = "invocationNum")]
    invocation_num: Option<u64>,
    #[serde(default, rename = "initialNumSteps")]
    initial_num_steps: Option<u64>,
}

/// JSON input for the `Stop` hook (TurnEnd).
///
/// Fires when the execution loop terminates. `fullyIdle` is `false` when
/// background tasks are still running.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct StopInput {
    #[serde(default, rename = "conversationId")]
    conversation_id: Option<String>,
    #[serde(default, rename = "transcriptPath")]
    transcript_path: Option<String>,
    #[serde(default, rename = "executionNum")]
    execution_num: Option<u64>,
    #[serde(default, rename = "terminationReason")]
    termination_reason: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default, rename = "fullyIdle")]
    fully_idle: Option<bool>,
}

/// JSON input for the `PostToolUse` hook.
///
/// Fires after a tool completes. Antigravity does not include the tool name
/// or arguments in this payload — only the step index and an optional error
/// string. Richer detail must come from the transcript (`transcriptPath`).
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct PostToolUseInput {
    #[serde(default, rename = "conversationId")]
    conversation_id: Option<String>,
    #[serde(default, rename = "transcriptPath")]
    transcript_path: Option<String>,
    #[serde(default, rename = "stepIdx")]
    step_idx: Option<u64>,
    #[serde(default)]
    error: Option<String>,
}

// AgyHook

/// Antigravity CLI (`agy`) hook adapter for Atomic Agent.
///
/// Handles hook JSON parsing, installation as an Antigravity plugin under
/// `~/.gemini/config/plugins/atomic/`, and presence detection via the
/// `.agents/` directory.
#[derive(Debug, Default)]
pub struct AgyHook {
    _private: (),
}

impl AgyHook {
    /// Create a new Antigravity CLI hook adapter.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns agy's global config directory: `~/.gemini/config`.
    fn global_config_dir() -> Option<PathBuf> {
        dirs::home_dir().map(|home| home.join(".gemini").join("config"))
    }

    /// Returns the plugin directory under the given config dir.
    fn plugin_dir(config_dir: &Path) -> PathBuf {
        config_dir.join("plugins").join(PLUGIN_DIR_NAME)
    }

    /// Returns the hooks file path under the given config dir.
    fn hooks_path(config_dir: &Path) -> PathBuf {
        Self::plugin_dir(config_dir).join(HOOKS_FILE)
    }

    /// Read and parse the plugin `hooks.json`, if it exists.
    ///
    /// Returns the top-level object mapping hook group names to their
    /// configurations. Unknown groups are preserved verbatim.
    fn read_hooks_file(path: &Path) -> AgentResult<Map<String, Value>> {
        if !path.exists() {
            return Ok(Map::new());
        }

        let content = std::fs::read_to_string(path).map_err(|e| AgentError::ConfigError {
            operation: "read".to_string(),
            path: path.to_path_buf(),
            reason: e.to_string(),
        })?;

        let value: Value = serde_json::from_str(&content).map_err(|e| AgentError::ConfigError {
            operation: "parse".to_string(),
            path: path.to_path_buf(),
            reason: e.to_string(),
        })?;

        match value {
            Value::Object(map) => Ok(map),
            // Tolerate a non-object top level by starting fresh; the file is
            // almost certainly not an Antigravity hooks file.
            _ => Ok(Map::new()),
        }
    }

    /// Write a JSON value to disk, creating parent directories.
    fn write_json(path: &Path, value: &Value) -> AgentResult<()> {
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent).map_err(|e| AgentError::ConfigError {
                    operation: "create directory".to_string(),
                    path: parent.to_path_buf(),
                    reason: e.to_string(),
                })?;
            }
        }

        let content = serde_json::to_string_pretty(value).map_err(|e| AgentError::ConfigError {
            operation: "serialize".to_string(),
            path: path.to_path_buf(),
            reason: e.to_string(),
        })?;

        std::fs::write(path, format!("{}\n", content)).map_err(|e| AgentError::ConfigError {
            operation: "write".to_string(),
            path: path.to_path_buf(),
            reason: e.to_string(),
        })
    }

    /// Install Atomic's hook group into the plugin hooks file, write the
    /// plugin manifest, and register the plugin in agy's import manifest.
    ///
    /// `config_dir` is agy's global config directory (`~/.gemini/config`);
    /// it is a parameter so tests can point at a temporary directory.
    fn install_to(config_dir: &Path, force: bool) -> AgentResult<usize> {
        let hooks_path = Self::hooks_path(config_dir);
        let mut groups = Self::read_hooks_file(&hooks_path)?;

        // Get or create the "atomic" group as an object.
        let group = groups
            .entry(ATOMIC_GROUP.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !group.is_object() {
            // A non-object "atomic" group isn't ours — leave it untouched.
            return Ok(0);
        }

        if force {
            remove_atomic_commands(group);
        }

        let mut count = 0;
        // (event key, verb) — lifecycle events take a direct handler list.
        for (event, verb) in [("PreInvocation", "pre-invocation"), ("Stop", "stop")] {
            let command = format!("{} {} || true", ATOMIC_HOOK_PREFIX, verb);
            if add_direct_handler(group, event, &command) {
                count += 1;
            }
        }

        // Tool events use matcher groups ("" matches all tools).
        let command = format!("{} post-tool-use || true", ATOMIC_HOOK_PREFIX);
        if add_matcher_handler(group, "PostToolUse", &command) {
            count += 1;
        }

        Self::write_json(&hooks_path, &Value::Object(groups))?;

        // Write the plugin manifest if missing (never clobber a user's).
        let manifest_path = Self::plugin_dir(config_dir).join(PLUGIN_MANIFEST_FILE);
        if !manifest_path.exists() {
            std::fs::write(&manifest_path, PLUGIN_MANIFEST).map_err(|e| {
                AgentError::ConfigError {
                    operation: "write plugin manifest".to_string(),
                    path: manifest_path.clone(),
                    reason: e.to_string(),
                }
            })?;
        }

        // Bundle the code-intelligence skills so the agent learns the
        // knowledge-graph-first discovery workflow.
        Self::install_skills(config_dir)?;

        Self::register_import(config_dir)?;

        Ok(count)
    }

    /// Write the bundled skills into the plugin's `skills/` directory.
    fn install_skills(config_dir: &Path) -> AgentResult<()> {
        let skills_dir = Self::plugin_dir(config_dir).join("skills");
        std::fs::create_dir_all(&skills_dir).map_err(|e| AgentError::ConfigError {
            operation: "create skills directory".to_string(),
            path: skills_dir.clone(),
            reason: e.to_string(),
        })?;

        for (name, content) in PLUGIN_SKILLS {
            let path = skills_dir.join(name);
            // Refresh the content on every install — these files are ours.
            std::fs::write(&path, content).map_err(|e| AgentError::ConfigError {
                operation: "write skill".to_string(),
                path,
                reason: e.to_string(),
            })?;
        }

        Ok(())
    }

    /// Remove Atomic's hooks, plugin manifest, and import registration.
    fn uninstall_from(config_dir: &Path) -> AgentResult<()> {
        let hooks_path = Self::hooks_path(config_dir);

        if hooks_path.exists() {
            let mut groups = Self::read_hooks_file(&hooks_path)?;

            if let Some(group) = groups.get_mut(ATOMIC_GROUP) {
                if group.is_object() {
                    remove_atomic_commands(group);
                    if group_is_empty(group) {
                        groups.remove(ATOMIC_GROUP);
                    }
                }
            }

            if groups.is_empty() {
                // No groups left — remove the hooks file entirely.
                std::fs::remove_file(&hooks_path).map_err(|e| AgentError::ConfigError {
                    operation: "remove hooks file".to_string(),
                    path: hooks_path.clone(),
                    reason: e.to_string(),
                })?;
            } else {
                Self::write_json(&hooks_path, &Value::Object(groups))?;
            }
        }

        // Remove the plugin manifest if it is the one Atomic wrote.
        let manifest_path = Self::plugin_dir(config_dir).join(PLUGIN_MANIFEST_FILE);
        if manifest_path.exists() {
            let is_ours = std::fs::read_to_string(&manifest_path)
                .map(|content| content.contains("\"atomic\""))
                .unwrap_or(false);
            if is_ours {
                std::fs::remove_file(&manifest_path).map_err(|e| AgentError::ConfigError {
                    operation: "remove plugin manifest".to_string(),
                    path: manifest_path.clone(),
                    reason: e.to_string(),
                })?;
            }
        }

        // Remove the bundled skills (they are ours — refreshed on install).
        let skills_dir = Self::plugin_dir(config_dir).join("skills");
        for (name, _) in PLUGIN_SKILLS {
            let path = skills_dir.join(name);
            if path.exists() {
                let _ = std::fs::remove_file(path);
            }
        }
        if skills_dir.is_dir() {
            let is_empty = std::fs::read_dir(&skills_dir)
                .map(|mut entries| entries.next().is_none())
                .unwrap_or(false);
            if is_empty {
                let _ = std::fs::remove_dir(&skills_dir);
            }
        }

        // Remove the plugin directory if it is now empty (the user may have
        // added skills/rules of their own — keep those).
        let plugin_dir = Self::plugin_dir(config_dir);
        if plugin_dir.is_dir() {
            let is_empty = std::fs::read_dir(&plugin_dir)
                .map(|mut entries| entries.next().is_none())
                .unwrap_or(false);
            if is_empty {
                let _ = std::fs::remove_dir(&plugin_dir);
            }
        }

        Self::unregister_import(config_dir)?;

        Ok(())
    }

    /// Check whether Atomic hooks are installed in the plugin hooks file.
    fn is_installed_in(config_dir: &Path) -> bool {
        let hooks_path = Self::hooks_path(config_dir);
        if !hooks_path.exists() {
            return false;
        }

        match Self::read_hooks_file(&hooks_path) {
            Ok(groups) => groups
                .get(ATOMIC_GROUP)
                .map(group_has_atomic_commands)
                .unwrap_or(false),
            // Tolerate unparseable files (possible hand edits) by falling
            // back to a raw content scan.
            Err(_) => std::fs::read_to_string(&hooks_path)
                .map(|content| content.contains(ATOMIC_HOOK_PREFIX))
                .unwrap_or(false),
        }
    }

    // Import manifest management

    /// Upsert the `atomic` entry in agy's `import_manifest.json` so the
    /// plugin shows up in `agy plugin list` and can be managed with
    /// `agy plugin enable/disable/uninstall`. Existing fields on the entry
    /// (e.g., a user's `enabled: false`) are preserved.
    fn register_import(config_dir: &Path) -> AgentResult<()> {
        let manifest_path = config_dir.join(IMPORT_MANIFEST_FILE);

        let mut manifest = if manifest_path.exists() {
            let content =
                std::fs::read_to_string(&manifest_path).map_err(|e| AgentError::ConfigError {
                    operation: "read import manifest".to_string(),
                    path: manifest_path.clone(),
                    reason: e.to_string(),
                })?;
            serde_json::from_str::<Value>(&content).unwrap_or_else(|_| Value::Object(Map::new()))
        } else {
            Value::Object(Map::new())
        };

        let Some(root) = manifest.as_object_mut() else {
            return Ok(());
        };
        let imports = root
            .entry("imports".to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        let Some(imports) = imports.as_array_mut() else {
            return Ok(());
        };

        // Find the existing entry, or create one.
        let entry = imports
            .iter_mut()
            .find(|entry| entry.get("name").and_then(Value::as_str) == Some(ATOMIC_GROUP));

        /// Components Atomic installs (hooks + bundled skills).
        const OUR_COMPONENTS: &[&str] = &["hooks", "skills"];

        match entry {
            Some(entry) => {
                // Preserve user-managed fields; only ensure our components
                // are registered.
                if let Some(obj) = entry.as_object_mut() {
                    let components = obj
                        .entry("components".to_string())
                        .or_insert_with(|| Value::Array(Vec::new()));
                    if let Some(list) = components.as_array_mut() {
                        for component in OUR_COMPONENTS {
                            let present = list.iter().any(|c| c.as_str() == Some(component));
                            if !present {
                                list.push(Value::String(component.to_string()));
                            }
                        }
                    }
                }
            }
            None => {
                let mut entry = Map::new();
                entry.insert("name".to_string(), Value::String(ATOMIC_GROUP.to_string()));
                entry.insert(
                    "source".to_string(),
                    Value::String("antigravity".to_string()),
                );
                entry.insert(
                    "importedAt".to_string(),
                    Value::String(chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()),
                );
                entry.insert(
                    "components".to_string(),
                    Value::Array(
                        OUR_COMPONENTS
                            .iter()
                            .map(|c| Value::String(c.to_string()))
                            .collect(),
                    ),
                );
                imports.push(Value::Object(entry));
            }
        }

        Self::write_json(&manifest_path, &manifest)
    }

    /// Remove the `atomic` entry from agy's `import_manifest.json`.
    fn unregister_import(config_dir: &Path) -> AgentResult<()> {
        let manifest_path = config_dir.join(IMPORT_MANIFEST_FILE);
        if !manifest_path.exists() {
            return Ok(());
        }

        let content =
            std::fs::read_to_string(&manifest_path).map_err(|e| AgentError::ConfigError {
                operation: "read import manifest".to_string(),
                path: manifest_path.clone(),
                reason: e.to_string(),
            })?;
        let mut manifest: Value =
            serde_json::from_str(&content).unwrap_or_else(|_| Value::Object(Map::new()));

        let Some(imports) = manifest
            .as_object_mut()
            .and_then(|root| root.get_mut("imports"))
            .and_then(Value::as_array_mut)
        else {
            return Ok(());
        };

        imports.retain(|entry| entry.get("name").and_then(Value::as_str) != Some(ATOMIC_GROUP));

        if imports.is_empty() {
            // Nothing registered — drop the manifest file entirely.
            let _ = std::fs::remove_file(&manifest_path);
            Ok(())
        } else {
            Self::write_json(&manifest_path, &manifest)
        }
    }

    // Global install/uninstall (plugins are inherently global, so these are
    // the same operation as project-level install for this adapter)

    /// Install hooks globally as an Antigravity plugin.
    pub fn install_global(&self, force: bool) -> AgentResult<usize> {
        let config_dir = Self::global_config_dir().ok_or_else(|| AgentError::ConfigError {
            operation: "resolve home".to_string(),
            path: PathBuf::from("~/.gemini/config"),
            reason: "Could not determine home directory".to_string(),
        })?;

        Self::install_to(&config_dir, force)
    }

    /// Remove the global Antigravity plugin hooks.
    pub fn uninstall_global(&self) -> AgentResult<()> {
        let config_dir = match Self::global_config_dir() {
            Some(dir) => dir,
            None => return Ok(()),
        };

        Self::uninstall_from(&config_dir)
    }

    /// Check if hooks are installed in the global plugin.
    pub fn is_installed_global(&self) -> bool {
        match Self::global_config_dir() {
            Some(dir) => Self::is_installed_in(&dir),
            None => false,
        }
    }

    /// Parse the common base fields and build a TurnEvent with the session
    /// ID and transcript path populated.
    fn base_event(
        &self,
        hook_type: HookType,
        conversation_id: Option<String>,
        transcript_path: Option<String>,
    ) -> TurnEvent {
        let session_id = conversation_id
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unknown".to_string());

        let mut event = TurnEvent::new(&session_id, hook_type);
        if let Some(path) = transcript_path {
            event = event.with_transcript_path(path);
        }
        event
    }

    /// Remove the Atomic hook group from a legacy project-level
    /// `.agents/hooks.json`, if one exists from an older Atomic release.
    fn uninstall_legacy_project_file(repo_root: &Path) {
        let legacy_path = repo_root.join(AGENTS_DIR).join(HOOKS_FILE);
        if !legacy_path.exists() {
            return;
        }

        let Ok(mut groups) = Self::read_hooks_file(&legacy_path) else {
            return;
        };

        let mut changed = false;
        if let Some(group) = groups.get_mut(ATOMIC_GROUP) {
            if group.is_object() {
                remove_atomic_commands(group);
                if group_is_empty(group) {
                    groups.remove(ATOMIC_GROUP);
                }
                changed = true;
            }
        }

        if changed {
            let _ = Self::write_json(&legacy_path, &Value::Object(groups));
        }
    }

    // AGENTS.md managed section

    /// Insert or replace the managed Atomic section in the repo's
    /// `AGENTS.md`, creating the file if needed. The section content comes
    /// from the canonical `atomic-repository/vault/AGENTS.md` (mirrored by
    /// the atomic-agy integration repository). Returns `true` if the file
    /// was written.
    fn upsert_agents_md_section(repo_root: &Path) -> AgentResult<bool> {
        let path = repo_root.join(AGENTS_MD_FILE);
        let existing = if path.exists() {
            std::fs::read_to_string(&path).map_err(|e| AgentError::ConfigError {
                operation: "read".to_string(),
                path: path.clone(),
                reason: e.to_string(),
            })?
        } else {
            String::new()
        };

        let section = format!("{}\n{}\n{}", SECTION_START, SECTION_BODY, SECTION_END);

        // Locate an existing managed section.
        let span = existing.find(SECTION_START).and_then(|start| {
            existing[start..]
                .find(SECTION_END)
                .map(|rel_end| (start, start + rel_end + SECTION_END.len()))
        });

        let updated = match span {
            Some((start, end)) => {
                let current = &existing[start..end];
                if current == section {
                    return Ok(false); // already up to date
                }
                format!("{}{}{}", &existing[..start], section, &existing[end..])
            }
            None => {
                if existing.trim().is_empty() {
                    format!("{}\n", section)
                } else {
                    format!("{}\n\n{}\n", existing.trim_end(), section)
                }
            }
        };

        std::fs::write(&path, updated).map_err(|e| AgentError::ConfigError {
            operation: "write".to_string(),
            path,
            reason: e.to_string(),
        })?;

        Ok(true)
    }

    /// Remove a managed Atomic section from the repo's `AGENTS.md`, if one
    /// exists from an older Atomic release that wrote it on install.
    /// Preserves everything else in the file and deletes the file only if
    /// nothing else remains. Returns `true` if the file was changed.
    fn remove_agents_md_section(repo_root: &Path) -> AgentResult<bool> {
        let path = repo_root.join(AGENTS_MD_FILE);
        if !path.exists() {
            return Ok(false);
        }

        let existing = std::fs::read_to_string(&path).map_err(|e| AgentError::ConfigError {
            operation: "read".to_string(),
            path: path.clone(),
            reason: e.to_string(),
        })?;

        let Some((start, end)) = existing.find(SECTION_START).and_then(|start| {
            existing[start..]
                .find(SECTION_END)
                .map(|rel_end| (start, start + rel_end + SECTION_END.len()))
        }) else {
            return Ok(false);
        };

        // Rejoin around the removed section without leaving blank-line
        // debris: the upsert separates the section from preceding content
        // with exactly one blank line, and the section carries one trailing
        // newline.
        let mut left = &existing[..start];
        let mut right = &existing[end..];
        if left.ends_with("\n\n") {
            left = &left[..left.len() - 1];
        }
        if right.starts_with('\n') {
            right = &right[1..];
        }
        if left.is_empty() {
            right = right.trim_start_matches('\n');
        }

        let updated = format!("{}{}", left, right);

        if updated.trim().is_empty() {
            // Only the managed section was here — remove the file.
            std::fs::remove_file(&path).map_err(|e| AgentError::ConfigError {
                operation: "remove".to_string(),
                path,
                reason: e.to_string(),
            })?;
        } else {
            std::fs::write(&path, updated).map_err(|e| AgentError::ConfigError {
                operation: "write".to_string(),
                path,
                reason: e.to_string(),
            })?;
        }

        Ok(true)
    }
}

// AgentHook Implementation

impl AgentHook for AgyHook {
    fn name(&self) -> &str {
        "agy"
    }

    fn display_name(&self) -> &str {
        "Antigravity CLI"
    }

    fn parse_event(&self, hook_type: HookType, input: &[u8]) -> AgentResult<TurnEvent> {
        if input.is_empty() {
            return Err(AgentError::HookInputEmpty {
                agent: self.name().to_string(),
                hook_type: hook_type.as_str().to_string(),
            });
        }

        let raw_json: Value =
            serde_json::from_slice(input).map_err(|e| AgentError::HookParseFailed {
                agent: self.name().to_string(),
                hook_type: hook_type.as_str().to_string(),
                reason: e.to_string(),
            })?;

        match hook_type {
            HookType::TurnStart => {
                // Antigravity: PreInvocation → TurnStart
                let parsed: PreInvocationInput =
                    serde_json::from_value(raw_json.clone()).map_err(|e| {
                        AgentError::HookParseFailed {
                            agent: self.name().to_string(),
                            hook_type: hook_type.as_str().to_string(),
                            reason: e.to_string(),
                        }
                    })?;

                Ok(self
                    .base_event(hook_type, parsed.conversation_id, parsed.transcript_path)
                    .with_raw_json(raw_json))
            }

            HookType::TurnEnd => {
                // Antigravity: Stop → TurnEnd
                let parsed: StopInput = serde_json::from_value(raw_json.clone()).map_err(|e| {
                    AgentError::HookParseFailed {
                        agent: self.name().to_string(),
                        hook_type: hook_type.as_str().to_string(),
                        reason: e.to_string(),
                    }
                })?;

                // Normalize the termination reason into the `finish_reason`
                // field the record pipeline understands (mirrors the Codex
                // adapter's stop normalization).
                let raw_json = normalize_stop_raw(raw_json);

                Ok(self
                    .base_event(hook_type, parsed.conversation_id, parsed.transcript_path)
                    .with_raw_json(raw_json))
            }

            HookType::PostToolUse => {
                let parsed: PostToolUseInput =
                    serde_json::from_value(raw_json.clone()).map_err(|e| {
                        AgentError::HookParseFailed {
                            agent: self.name().to_string(),
                            hook_type: hook_type.as_str().to_string(),
                            reason: e.to_string(),
                        }
                    })?;

                // Normalize `error` into the `status` field the provenance
                // accumulator reads. Antigravity sends no tool name here, so
                // the orchestrator records the call under "unknown" — the
                // transcript (whose path is on the event) holds the detail.
                let raw_json = normalize_tool_raw(raw_json);

                let mut event = self
                    .base_event(hook_type, parsed.conversation_id, parsed.transcript_path)
                    .with_raw_json(raw_json);

                // The step index is the closest thing to a tool call ID in
                // this payload — it is unique within the trajectory.
                if let Some(step) = parsed.step_idx {
                    event = event.with_tool_use_id(step.to_string());
                }

                Ok(event)
            }

            // Antigravity has no session lifecycle hooks; SessionStart /
            // SessionEnd / PreToolUse are never dispatched for this agent.
            _ => Err(AgentError::HookParseFailed {
                agent: self.name().to_string(),
                hook_type: hook_type.as_str().to_string(),
                reason: format!("hook type {:?} is not supported by agy", hook_type),
            }),
        }
    }

    fn install(&self, repo_root: &Path) -> AgentResult<usize> {
        // Two-part install:
        // 1. Global plugin (hooks + skills) — the only mechanism agy fires.
        // 2. Managed AGENTS.md section in the repo — the standard agent
        //    instruction file, required for the agent to follow Atomic's
        //    workflow (KG-first discovery, intent turns, recording rules).
        let count = self.install_global(false)?;
        Self::upsert_agents_md_section(repo_root)?;
        Ok(count)
    }

    fn uninstall(&self, repo_root: &Path) -> AgentResult<()> {
        self.uninstall_global()?;
        Self::uninstall_legacy_project_file(repo_root);
        Self::remove_agents_md_section(repo_root)?;
        Ok(())
    }

    fn is_installed(&self, repo_root: &Path) -> bool {
        // Installed means both halves are present *and current*: the global
        // plugin and the repo's AGENTS.md section. Checking the section
        // content (not just its markers) means an update to the canonical
        // instruction file is picked up by a plain `agent enable` rerun.
        if !self.is_installed_global() {
            return false;
        }

        let path = repo_root.join(AGENTS_MD_FILE);
        let Ok(content) = std::fs::read_to_string(&path) else {
            return false;
        };

        let current_section = format!("{}\n{}\n{}", SECTION_START, SECTION_BODY, SECTION_END);
        content.contains(&current_section)
    }

    fn supported_hooks(&self) -> Vec<HookType> {
        vec![
            HookType::TurnStart,
            HookType::TurnEnd,
            HookType::PostToolUse,
        ]
    }

    fn detect_presence(&self, repo_root: &Path) -> bool {
        repo_root.join(AGENTS_DIR).is_dir()
    }

    fn hook_verbs(&self) -> Vec<&str> {
        vec!["pre-invocation", "stop", "post-tool-use"]
    }

    fn stdout_response(&self, _hook_type: HookType) -> Option<&'static str> {
        // Antigravity reads a JSON object from hook stdout. `{}` satisfies
        // the PostToolUse contract verbatim and is a no-op for PreInvocation
        // and Stop.
        Some("{}")
    }

    fn repo_root_hints(&self, event: &TurnEvent) -> Option<Vec<PathBuf>> {
        // Antigravity executes plugin hooks with the plugin directory as the
        // working directory, so the process cwd is useless for finding the
        // repository. Every hook payload carries the mounted workspaces.
        let paths = event
            .raw_json
            .as_ref()?
            .get("workspacePaths")?
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .map(PathBuf::from)
            .collect::<Vec<_>>();

        if paths.is_empty() {
            None
        } else {
            Some(paths)
        }
    }
}

// Raw JSON Normalization

/// Map Antigravity's `terminationReason` onto the `finish_reason` field used
/// by the record pipeline (mirrors the Codex adapter's stop normalization).
fn normalize_stop_raw(mut raw: Value) -> Value {
    let reason = raw
        .get("terminationReason")
        .and_then(Value::as_str)
        .map(|s| s.to_string());

    if let Some(reason) = reason {
        let finish_reason = match reason.as_str() {
            "model_stop" => "stop",
            "max_steps_exceeded" => "length",
            other => other,
        };
        if let Some(obj) = raw.as_object_mut() {
            obj.entry("finish_reason".to_string())
                .or_insert_with(|| Value::String(finish_reason.to_string()));
        }
    }

    raw
}

/// Map Antigravity's `error` field onto the `status` field the provenance
/// accumulator reads for tool calls.
fn normalize_tool_raw(mut raw: Value) -> Value {
    let has_error = raw
        .get("error")
        .and_then(Value::as_str)
        .is_some_and(|e| !e.is_empty());

    if let Some(obj) = raw.as_object_mut() {
        obj.entry("status".to_string()).or_insert_with(|| {
            Value::String(if has_error { "error" } else { "completed" }.to_string())
        });
    }

    raw
}

// Hook Group Helpers (Value-level, resilient to hand edits)

/// Returns `true` if a hook handler's command is an Atomic hook.
fn is_atomic_command(command: &str) -> bool {
    command.contains(ATOMIC_HOOK_PREFIX)
}

/// Returns `true` if any handler in a direct handler list is an Atomic hook.
fn direct_list_has_atomic(list: &[Value]) -> bool {
    list.iter().any(|handler| {
        handler
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(is_atomic_command)
    })
}

/// Returns `true` if any matcher group contains an Atomic hook.
fn matcher_list_has_atomic(list: &[Value]) -> bool {
    list.iter().any(|group| {
        group
            .get("hooks")
            .and_then(Value::as_array)
            .is_some_and(|hooks| direct_list_has_atomic(hooks))
    })
}

/// Check whether the given event list (direct or matcher-wrapped) already
/// contains an Atomic hook command.
fn event_list_has_atomic(list: &[Value]) -> bool {
    direct_list_has_atomic(list) || matcher_list_has_atomic(list)
}

/// Add a handler to a direct handler list (`PreInvocation`, `Stop`, ...).
///
/// Returns `true` if the handler was added (i.e., no Atomic handler with the
/// same command was already present).
fn add_direct_handler(group: &mut Value, event: &str, command: &str) -> bool {
    let Some(obj) = group.as_object_mut() else {
        return false;
    };

    let entry = obj
        .entry(event.to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    let Some(list) = entry.as_array_mut() else {
        return false;
    };

    if event_list_has_atomic(list) {
        return false;
    }

    let mut handler = Map::new();
    handler.insert("type".to_string(), Value::String("command".to_string()));
    handler.insert("command".to_string(), Value::String(command.to_string()));
    list.push(Value::Object(handler));
    true
}

/// Add a handler inside a matcher group (`PreToolUse`, `PostToolUse`).
///
/// Reuses an existing `""` matcher if present, otherwise appends a new one.
/// Returns `true` if the handler was added.
fn add_matcher_handler(group: &mut Value, event: &str, command: &str) -> bool {
    let Some(obj) = group.as_object_mut() else {
        return false;
    };

    let entry = obj
        .entry(event.to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    let Some(list) = entry.as_array_mut() else {
        return false;
    };

    if event_list_has_atomic(list) {
        return false;
    }

    let mut handler = Map::new();
    handler.insert("type".to_string(), Value::String("command".to_string()));
    handler.insert("command".to_string(), Value::String(command.to_string()));

    // Reuse an existing empty matcher group if one exists.
    for matcher_group in list.iter_mut() {
        if matcher_group.get("matcher").and_then(Value::as_str) == Some("") {
            if let Some(hooks) = matcher_group.get_mut("hooks").and_then(Value::as_array_mut) {
                hooks.push(Value::Object(handler));
                return true;
            }
        }
    }

    let mut matcher_group = Map::new();
    matcher_group.insert("matcher".to_string(), Value::String(String::new()));
    matcher_group.insert(
        "hooks".to_string(),
        Value::Array(vec![Value::Object(handler)]),
    );
    list.push(Value::Object(matcher_group));
    true
}

/// Remove all Atomic hook commands from every event list in a group,
/// dropping emptied matcher groups and empty event lists.
fn remove_atomic_commands(group: &mut Value) {
    let Some(obj) = group.as_object_mut() else {
        return;
    };

    let events: Vec<String> = obj.keys().cloned().collect();
    for event in events {
        let Some(list) = obj.get_mut(&event).and_then(Value::as_array_mut) else {
            continue;
        };

        // Matcher-wrapped lists: clean each group's hooks.
        for item in list.iter_mut() {
            if let Some(hooks) = item.get_mut("hooks").and_then(Value::as_array_mut) {
                hooks.retain(|handler| {
                    !handler
                        .get("command")
                        .and_then(Value::as_str)
                        .is_some_and(is_atomic_command)
                });
            }
        }
        // Drop emptied matcher groups.
        list.retain(|item| match item.get("hooks").and_then(Value::as_array) {
            Some(hooks) => !hooks.is_empty(),
            None => true,
        });
        // Direct handler lists: drop Atomic handlers.
        list.retain(|handler| {
            // Keep matcher groups (handled above) and non-Atomic handlers.
            if handler.get("hooks").is_some() {
                return true;
            }
            !handler
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(is_atomic_command)
        });

        if list.is_empty() {
            obj.remove(&event);
        }
    }
}

/// Returns `true` if the group has no remaining handlers and no keys other
/// than `enabled` (a bare `enabled` flag on an empty group is meaningless).
fn group_is_empty(group: &Value) -> bool {
    match group.as_object() {
        Some(obj) => obj.keys().all(|k| k == "enabled"),
        None => false,
    }
}

/// Returns `true` if any event list in the group contains an Atomic hook.
fn group_has_atomic_commands(group: &Value) -> bool {
    match group.as_object() {
        Some(obj) => obj
            .values()
            .filter_map(Value::as_array)
            .any(|list| event_list_has_atomic(list)),
        None => false,
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    fn make_hook() -> AgyHook {
        AgyHook::new()
    }

    /// Install into a temporary config dir and return it.
    fn temp_config() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    // Identity tests

    #[test]
    fn test_name() {
        assert_eq!(make_hook().name(), "agy");
    }

    #[test]
    fn test_display_name() {
        assert_eq!(make_hook().display_name(), "Antigravity CLI");
    }

    #[test]
    fn test_supported_hooks() {
        let supported = make_hook().supported_hooks();
        assert!(supported.contains(&HookType::TurnStart));
        assert!(supported.contains(&HookType::TurnEnd));
        assert!(supported.contains(&HookType::PostToolUse));
        assert!(!supported.contains(&HookType::SessionStart));
        assert!(!supported.contains(&HookType::PreToolUse));
    }

    #[test]
    fn test_hook_verbs() {
        let hook = make_hook();
        let verbs = hook.hook_verbs();
        assert_eq!(verbs.len(), 3);
        assert!(verbs.contains(&"pre-invocation"));
        assert!(verbs.contains(&"stop"));
        assert!(verbs.contains(&"post-tool-use"));
    }

    #[test]
    fn test_verbs_map_to_hook_types() {
        assert_eq!(
            HookType::from_verb("pre-invocation"),
            Some(HookType::TurnStart)
        );
        assert_eq!(HookType::from_verb("stop"), Some(HookType::TurnEnd));
        assert_eq!(
            HookType::from_verb("post-tool-use"),
            Some(HookType::PostToolUse)
        );
    }

    #[test]
    fn test_stdout_response() {
        let hook = make_hook();
        assert_eq!(hook.stdout_response(HookType::TurnStart), Some("{}"));
        assert_eq!(hook.stdout_response(HookType::TurnEnd), Some("{}"));
        assert_eq!(hook.stdout_response(HookType::PostToolUse), Some("{}"));
    }

    // Parse event tests

    #[test]
    fn test_parse_pre_invocation_turn_start() {
        let hook = make_hook();
        let input = br#"{
            "conversationId": "ec33ebf9-0cba-4100-8142-c61503f6c587",
            "transcriptPath": "/home/u/.gemini/antigravity-cli/brain/ec33/logs/transcript.jsonl",
            "artifactDirectoryPath": "/home/u/.gemini/antigravity-cli/brain/ec33",
            "workspacePaths": ["/workspace/project"],
            "invocationNum": 0,
            "initialNumSteps": 0
        }"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        assert_eq!(event.session_id, "ec33ebf9-0cba-4100-8142-c61503f6c587");
        assert_eq!(event.event_type, HookType::TurnStart);
        assert!(event.transcript_path.is_some());
    }

    #[test]
    fn test_parse_stop_turn_end() {
        let hook = make_hook();
        let input = br#"{
            "conversationId": "ec33ebf9",
            "transcriptPath": "/tmp/transcript.jsonl",
            "workspacePaths": ["/workspace/project"],
            "executionNum": 1,
            "terminationReason": "model_stop",
            "error": "",
            "fullyIdle": true
        }"#;
        let event = hook.parse_event(HookType::TurnEnd, input).unwrap();
        assert_eq!(event.session_id, "ec33ebf9");
        assert_eq!(event.event_type, HookType::TurnEnd);
        // terminationReason is normalized into finish_reason
        let raw = event.raw_json.unwrap();
        assert_eq!(
            raw.get("finish_reason").and_then(Value::as_str),
            Some("stop")
        );
    }

    #[test]
    fn test_parse_stop_max_steps_maps_to_length() {
        let hook = make_hook();
        let input = br#"{"conversationId": "s1", "terminationReason": "max_steps_exceeded"}"#;
        let event = hook.parse_event(HookType::TurnEnd, input).unwrap();
        let raw = event.raw_json.unwrap();
        assert_eq!(
            raw.get("finish_reason").and_then(Value::as_str),
            Some("length")
        );
    }

    #[test]
    fn test_parse_post_tool_use_success() {
        let hook = make_hook();
        let input = br#"{
            "conversationId": "ec33ebf9",
            "transcriptPath": "/tmp/transcript.jsonl",
            "workspacePaths": ["/workspace/project"],
            "stepIdx": 5,
            "error": ""
        }"#;
        let event = hook.parse_event(HookType::PostToolUse, input).unwrap();
        assert_eq!(event.session_id, "ec33ebf9");
        assert_eq!(event.event_type, HookType::PostToolUse);
        assert_eq!(event.tool_use_id.as_deref(), Some("5"));
        let raw = event.raw_json.unwrap();
        assert_eq!(raw.get("status").and_then(Value::as_str), Some("completed"));
    }

    #[test]
    fn test_parse_post_tool_use_error() {
        let hook = make_hook();
        let input = br#"{"conversationId": "ec33ebf9", "stepIdx": 7, "error": "exit status 1"}"#;
        let event = hook.parse_event(HookType::PostToolUse, input).unwrap();
        assert_eq!(event.tool_use_id.as_deref(), Some("7"));
        let raw = event.raw_json.unwrap();
        assert_eq!(raw.get("status").and_then(Value::as_str), Some("error"));
    }

    #[test]
    fn test_parse_missing_conversation_id_defaults_unknown() {
        let hook = make_hook();
        let input = br#"{"invocationNum": 0}"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        assert_eq!(event.session_id, "unknown");
    }

    #[test]
    fn test_parse_empty_input() {
        let hook = make_hook();
        assert!(hook.parse_event(HookType::TurnStart, b"").is_err());
    }

    #[test]
    fn test_parse_invalid_json() {
        let hook = make_hook();
        assert!(hook.parse_event(HookType::TurnStart, b"not json").is_err());
    }

    #[test]
    fn test_parse_unsupported_hook_type() {
        let hook = make_hook();
        let input = br#"{"conversationId": "s1"}"#;
        assert!(hook.parse_event(HookType::SessionStart, input).is_err());
        assert!(hook.parse_event(HookType::PreToolUse, input).is_err());
    }

    // repo_root_hints tests

    #[test]
    fn test_repo_root_hints_from_workspace_paths() {
        let hook = make_hook();
        let input = br#"{
            "conversationId": "s1",
            "workspacePaths": ["/workspace/project", "/workspace/shared"]
        }"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        let hints = hook.repo_root_hints(&event).unwrap();
        assert_eq!(
            hints,
            vec![
                PathBuf::from("/workspace/project"),
                PathBuf::from("/workspace/shared")
            ]
        );
    }

    #[test]
    fn test_repo_root_hints_missing_field() {
        let hook = make_hook();
        let input = br#"{"conversationId": "s1"}"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        assert!(hook.repo_root_hints(&event).is_none());
    }

    #[test]
    fn test_repo_root_hints_empty_array() {
        let hook = make_hook();
        let input = br#"{"conversationId": "s1", "workspacePaths": []}"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        assert!(hook.repo_root_hints(&event).is_none());
    }

    // Detection tests

    #[test]
    fn test_detect_presence_with_agents_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".agents")).unwrap();
        assert!(make_hook().detect_presence(dir.path()));
    }

    #[test]
    fn test_detect_presence_without_agents_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!make_hook().detect_presence(dir.path()));
    }

    // Install / uninstall tests

    #[test]
    fn test_install_creates_plugin_files() {
        let config = temp_config();

        let count = AgyHook::install_to(config.path(), false).unwrap();
        assert_eq!(count, 3);

        let hooks_path = config.path().join("plugins/atomic/hooks.json");
        let manifest_path = config.path().join("plugins/atomic/plugin.json");
        let registry_path = config.path().join("import_manifest.json");

        assert!(hooks_path.exists());
        assert!(manifest_path.exists());
        assert!(registry_path.exists());

        let content = std::fs::read_to_string(&hooks_path).unwrap();
        assert!(content.contains(ATOMIC_HOOK_PREFIX));
        assert!(content.contains("pre-invocation"));
        assert!(content.contains("stop"));
        assert!(content.contains("post-tool-use"));

        let plugin_manifest = std::fs::read_to_string(&manifest_path).unwrap();
        assert!(plugin_manifest.contains("\"atomic\""));

        let registry: Value =
            serde_json::from_str(&std::fs::read_to_string(&registry_path).unwrap()).unwrap();
        let imports = registry["imports"].as_array().unwrap();
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0]["name"], "atomic");
        let components = imports[0]["components"].as_array().unwrap();
        assert!(components.iter().any(|c| c == "hooks"));
        assert!(components.iter().any(|c| c == "skills"));
    }

    #[test]
    fn test_install_bundles_skills() {
        let config = temp_config();
        AgyHook::install_to(config.path(), false).unwrap();

        let skills_dir = config.path().join("plugins/atomic/skills");
        let ci = std::fs::read_to_string(skills_dir.join("code-intelligence.md")).unwrap();
        let av = std::fs::read_to_string(skills_dir.join("atomic-vault.md")).unwrap();

        // Skills keep their agy-compatible frontmatter and KG workflow.
        assert!(ci.contains("name: Code Intelligence"));
        assert!(ci.contains("atomic vault query search"));
        assert!(av.contains("name: Atomic Vault"));
    }

    #[test]
    fn test_install_structure() {
        let config = temp_config();
        AgyHook::install_to(config.path(), false).unwrap();

        let hooks_path = config.path().join("plugins/atomic/hooks.json");
        let parsed: Value =
            serde_json::from_str(&std::fs::read_to_string(&hooks_path).unwrap()).unwrap();

        let group = parsed.get("atomic").unwrap();

        // Lifecycle events are direct handler lists.
        let pre = group.get("PreInvocation").unwrap().as_array().unwrap();
        assert_eq!(pre.len(), 1);
        assert!(pre[0].get("command").is_some());

        let stop = group.get("Stop").unwrap().as_array().unwrap();
        assert_eq!(stop.len(), 1);
        assert!(stop[0].get("command").is_some());

        // Tool events are matcher groups.
        let post = group.get("PostToolUse").unwrap().as_array().unwrap();
        assert_eq!(post.len(), 1);
        assert_eq!(post[0].get("matcher").and_then(Value::as_str), Some(""));
        let hooks = post[0].get("hooks").unwrap().as_array().unwrap();
        assert_eq!(hooks.len(), 1);
        assert!(hooks[0]
            .get("command")
            .and_then(Value::as_str)
            .unwrap()
            .contains("post-tool-use"));

        // Hook commands are bare (no cwd guard) — plugin hooks do not run
        // with the workspace as cwd.
        let command = stop[0].get("command").and_then(Value::as_str).unwrap();
        assert!(!command.contains("test -d"));
    }

    #[test]
    fn test_install_idempotent() {
        let config = temp_config();

        assert_eq!(AgyHook::install_to(config.path(), false).unwrap(), 3);
        assert_eq!(AgyHook::install_to(config.path(), false).unwrap(), 0);
    }

    #[test]
    fn test_install_preserves_existing_manifest_fields() {
        let config = temp_config();

        // Pre-existing registry with a user-disabled atomic entry.
        let existing = serde_json::json!({
            "imports": [
                {"name": "atomic", "source": "antigravity", "enabled": false, "components": []},
                {"name": "other-plugin", "source": "antigravity", "components": ["skills"]}
            ]
        });
        std::fs::write(
            config.path().join("import_manifest.json"),
            serde_json::to_string_pretty(&existing).unwrap(),
        )
        .unwrap();

        AgyHook::install_to(config.path(), false).unwrap();

        let registry: Value = serde_json::from_str(
            &std::fs::read_to_string(config.path().join("import_manifest.json")).unwrap(),
        )
        .unwrap();
        let imports = registry["imports"].as_array().unwrap();
        assert_eq!(imports.len(), 2);

        // The user's disable flag survives; our components are added.
        let atomic_entry = &imports[0];
        assert_eq!(atomic_entry["enabled"], false);
        let components = atomic_entry["components"].as_array().unwrap();
        assert!(components.iter().any(|c| c == "hooks"));
        assert!(components.iter().any(|c| c == "skills"));

        // The other plugin is untouched.
        assert_eq!(imports[1]["name"], "other-plugin");
    }

    #[test]
    fn test_is_installed() {
        let config = temp_config();

        assert!(!AgyHook::is_installed_in(config.path()));
        AgyHook::install_to(config.path(), false).unwrap();
        assert!(AgyHook::is_installed_in(config.path()));
    }

    #[test]
    fn test_uninstall_removes_everything() {
        let config = temp_config();

        AgyHook::install_to(config.path(), false).unwrap();
        assert!(AgyHook::is_installed_in(config.path()));

        AgyHook::uninstall_from(config.path()).unwrap();
        assert!(!AgyHook::is_installed_in(config.path()));

        // hooks.json, plugin.json, skills, the plugin dir, and the registry
        // are all gone.
        assert!(!config.path().join("plugins/atomic/hooks.json").exists());
        assert!(!config.path().join("plugins/atomic/plugin.json").exists());
        assert!(!config.path().join("plugins/atomic/skills").exists());
        assert!(!config.path().join("plugins/atomic").exists());
        assert!(!config.path().join("import_manifest.json").exists());
    }

    #[test]
    fn test_install_preserves_other_groups() {
        let config = temp_config();
        let plugin_dir = config.path().join("plugins/atomic");
        std::fs::create_dir_all(&plugin_dir).unwrap();

        let existing = serde_json::json!({
            "my-linter": {
                "PostToolUse": [
                    {
                        "matcher": "run_command",
                        "hooks": [
                            {"type": "command", "command": "./scripts/lint.sh", "timeout": 10}
                        ]
                    }
                ]
            }
        });
        std::fs::write(
            plugin_dir.join("hooks.json"),
            serde_json::to_string_pretty(&existing).unwrap(),
        )
        .unwrap();

        AgyHook::install_to(config.path(), false).unwrap();

        let content = std::fs::read_to_string(plugin_dir.join("hooks.json")).unwrap();
        assert!(content.contains("my-linter"));
        assert!(content.contains("./scripts/lint.sh"));
        assert!(content.contains(ATOMIC_HOOK_PREFIX));
    }

    #[test]
    fn test_uninstall_preserves_other_groups_and_custom_hooks() {
        let config = temp_config();
        let plugin_dir = config.path().join("plugins/atomic");
        std::fs::create_dir_all(&plugin_dir).unwrap();

        let existing = serde_json::json!({
            "my-linter": {
                "PostToolUse": [
                    {"matcher": "run_command", "hooks": [{"command": "./lint.sh"}]}
                ]
            },
            "atomic": {
                "enabled": true,
                "Stop": [
                    {"type": "command", "command": "./my-own-stop-hook.sh"},
                    {"type": "command", "command": "atomic agent hooks agy stop || true"}
                ]
            }
        });
        std::fs::write(
            plugin_dir.join("hooks.json"),
            serde_json::to_string_pretty(&existing).unwrap(),
        )
        .unwrap();

        AgyHook::uninstall_from(config.path()).unwrap();

        let parsed: Value =
            serde_json::from_str(&std::fs::read_to_string(plugin_dir.join("hooks.json")).unwrap())
                .unwrap();

        // Other group untouched, so hooks.json survives.
        assert!(parsed.get("my-linter").is_some());

        // The user's own handler in the "atomic" group survives.
        let atomic = parsed.get("atomic").unwrap();
        let stop = atomic.get("Stop").unwrap().as_array().unwrap();
        assert_eq!(stop.len(), 1);
        assert_eq!(
            stop[0].get("command").and_then(Value::as_str),
            Some("./my-own-stop-hook.sh")
        );
    }

    #[test]
    fn test_uninstall_nonexistent_is_ok() {
        let config = temp_config();
        assert!(AgyHook::uninstall_from(config.path()).is_ok());
    }

    #[test]
    fn test_install_reuses_existing_empty_matcher() {
        let config = temp_config();
        let plugin_dir = config.path().join("plugins/atomic");
        std::fs::create_dir_all(&plugin_dir).unwrap();

        let existing = serde_json::json!({
            "atomic": {
                "PostToolUse": [
                    {"matcher": "", "hooks": [{"command": "./existing.sh"}]}
                ]
            }
        });
        std::fs::write(
            plugin_dir.join("hooks.json"),
            serde_json::to_string_pretty(&existing).unwrap(),
        )
        .unwrap();

        AgyHook::install_to(config.path(), false).unwrap();

        let parsed: Value =
            serde_json::from_str(&std::fs::read_to_string(plugin_dir.join("hooks.json")).unwrap())
                .unwrap();
        let post = parsed["atomic"]["PostToolUse"].as_array().unwrap();
        // Reused the single "" matcher rather than adding a second one.
        assert_eq!(post.len(), 1);
        assert_eq!(post[0]["hooks"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_uninstall_cleans_legacy_project_file() {
        let repo = tempfile::tempdir().unwrap();
        let agents_dir = repo.path().join(".agents");
        std::fs::create_dir_all(&agents_dir).unwrap();

        // Legacy install from the .agents/hooks.json era.
        let legacy = serde_json::json!({
            "atomic": {
                "Stop": [
                    {"type": "command", "command": "test -d .atomic || test -f .atomic-sandbox && atomic agent hooks agy stop || true"}
                ]
            },
            "other-tool": {
                "Stop": [{"type": "command", "command": "./other.sh"}]
            }
        });
        std::fs::write(
            agents_dir.join("hooks.json"),
            serde_json::to_string_pretty(&legacy).unwrap(),
        )
        .unwrap();

        AgyHook::uninstall_legacy_project_file(repo.path());

        let parsed: Value =
            serde_json::from_str(&std::fs::read_to_string(agents_dir.join("hooks.json")).unwrap())
                .unwrap();
        // Atomic group removed, other tool preserved.
        assert!(parsed.get("atomic").is_none());
        assert!(parsed.get("other-tool").is_some());
    }

    // AGENTS.md managed section tests

    #[test]
    fn test_agents_md_section_created_when_missing() {
        let repo = tempfile::tempdir().unwrap();

        let changed = AgyHook::upsert_agents_md_section(repo.path()).unwrap();
        assert!(changed);

        let content = std::fs::read_to_string(repo.path().join("AGENTS.md")).unwrap();
        assert!(content.contains(SECTION_START));
        assert!(content.contains(SECTION_END));
        // Content comes from the canonical atomic-repository/vault/AGENTS.md.
        assert!(content.contains("Atomic VCS Agent"));
        assert!(content.contains("Do NOT run `atomic add` or `atomic record`"));
        assert!(content.contains("atomic vault query search"));
    }

    #[test]
    fn test_agents_md_section_appended_preserving_user_content() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(
            repo.path().join("AGENTS.md"),
            "# My Project\n\nCustom rules here.\n",
        )
        .unwrap();

        AgyHook::upsert_agents_md_section(repo.path()).unwrap();

        let content = std::fs::read_to_string(repo.path().join("AGENTS.md")).unwrap();
        assert!(content.starts_with("# My Project\n\nCustom rules here.\n"));
        assert!(content.contains(SECTION_START));
    }

    #[test]
    fn test_agents_md_section_idempotent() {
        let repo = tempfile::tempdir().unwrap();

        assert!(AgyHook::upsert_agents_md_section(repo.path()).unwrap());
        // Second run is a no-op.
        assert!(!AgyHook::upsert_agents_md_section(repo.path()).unwrap());
    }

    #[test]
    fn test_agents_md_section_replaced_between_markers() {
        let repo = tempfile::tempdir().unwrap();
        let stale = format!(
            "# Project\n\n{}\nold stale content\n{}\n",
            SECTION_START, SECTION_END
        );
        std::fs::write(repo.path().join("AGENTS.md"), stale).unwrap();

        AgyHook::upsert_agents_md_section(repo.path()).unwrap();

        let content = std::fs::read_to_string(repo.path().join("AGENTS.md")).unwrap();
        assert!(!content.contains("old stale content"));
        assert!(content.contains("Atomic VCS Agent"));
        assert!(content.starts_with("# Project\n\n"));
    }

    #[test]
    fn test_agents_md_section_removed_cleanly() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(
            repo.path().join("AGENTS.md"),
            "# My Project\n\nCustom rules here.\n",
        )
        .unwrap();

        AgyHook::upsert_agents_md_section(repo.path()).unwrap();
        let changed = AgyHook::remove_agents_md_section(repo.path()).unwrap();
        assert!(changed);

        let content = std::fs::read_to_string(repo.path().join("AGENTS.md")).unwrap();
        assert!(!content.contains(SECTION_START));
        assert!(content.contains("# My Project"));
        assert!(content.contains("Custom rules here."));
        // No trailing blank-line debris.
        assert!(!content.ends_with("\n\n"));
    }

    #[test]
    fn test_agents_md_section_removal_deletes_otherwise_empty_file() {
        let repo = tempfile::tempdir().unwrap();

        AgyHook::upsert_agents_md_section(repo.path()).unwrap();
        assert!(repo.path().join("AGENTS.md").exists());

        AgyHook::remove_agents_md_section(repo.path()).unwrap();
        assert!(!repo.path().join("AGENTS.md").exists());
    }

    #[test]
    fn test_agents_md_removal_missing_file_is_ok() {
        let repo = tempfile::tempdir().unwrap();
        assert!(!AgyHook::remove_agents_md_section(repo.path()).unwrap());
    }

    // Helper function tests

    #[test]
    fn test_is_atomic_command() {
        assert!(is_atomic_command("atomic agent hooks agy stop"));
        assert!(is_atomic_command(
            "atomic agent hooks agy pre-invocation || true"
        ));
        assert!(!is_atomic_command("atomic agent hooks gemini-cli stop"));
        assert!(!is_atomic_command("./lint.sh"));
        assert!(!is_atomic_command(""));
    }

    #[test]
    fn test_group_is_empty() {
        assert!(group_is_empty(&serde_json::json!({})));
        assert!(group_is_empty(&serde_json::json!({"enabled": false})));
        assert!(!group_is_empty(&serde_json::json!({
            "Stop": [{"command": "./x.sh"}]
        })));
    }

    #[test]
    fn test_roundtrip_preserves_unknown_fields() {
        let config = temp_config();
        let plugin_dir = config.path().join("plugins/atomic");
        std::fs::create_dir_all(&plugin_dir).unwrap();

        let existing = serde_json::json!({
            "custom-group": {
                "enabled": false,
                "PreToolUse": [
                    {"matcher": "browser_.*", "hooks": [{"command": "./gate.sh", "timeout": 5}]}
                ],
                "FutureEvent": [{"command": "./future.sh"}]
            }
        });
        std::fs::write(
            plugin_dir.join("hooks.json"),
            serde_json::to_string_pretty(&existing).unwrap(),
        )
        .unwrap();

        AgyHook::install_to(config.path(), false).unwrap();
        AgyHook::uninstall_from(config.path()).unwrap();

        let parsed: Value =
            serde_json::from_str(&std::fs::read_to_string(plugin_dir.join("hooks.json")).unwrap())
                .unwrap();
        assert_eq!(parsed, existing);
    }
}
