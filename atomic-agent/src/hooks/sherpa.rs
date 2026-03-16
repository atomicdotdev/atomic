//! Sherpa TUI agent hook adapter for Atomic Agent.
//!
//! Handles hook JSON parsing from the Sherpa TUI, which calls
//! `atomic agent hooks sherpa <verb>` via subprocess at each lifecycle event.
//!
//! # Sherpa Hook Architecture
//!
//! Unlike external agents (Claude Code, OpenCode) which are separate processes
//! that call back to `atomic agent hooks`, Sherpa is a TUI that manages its
//! own session lifecycle internally. It spawns `atomic agent hooks sherpa <verb>`
//! as a subprocess at four lifecycle points, piping its session JSON to stdin:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │  Sherpa TUI (atomic-tui)                                                │
//! │                                                                         │
//! │  App::new()         ──▶  atomic agent hooks sherpa session-start       │
//! │  LlmResponse::IntentNode  ──▶  atomic agent hooks sherpa turn-start    │
//! │  State::Verification ──▶  atomic agent hooks sherpa turn-end           │
//! │  /exit | /quit | q   ──▶  atomic agent hooks sherpa session-end        │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Hook Verbs
//!
//! | Verb            | HookType       | Description                           |
//! |-----------------|----------------|---------------------------------------|
//! | `session-start` | SessionStart   | Sherpa TUI launched, session created   |
//! | `session-end`   | SessionEnd     | User quit the TUI                      |
//! | `turn-start`    | TurnStart      | Intent confirmed, Orienting begins     |
//! | `turn-end`      | TurnEnd        | Verification state reached             |
//!
//! # JSON Input Format
//!
//! All hooks receive a JSON payload via stdin derived from Sherpa's `Session`
//! snapshot struct. At minimum every payload contains:
//!
//! ```json
//! {
//!   "session_id": "rapid-wildflower-4475",
//!   "cwd": "/path/to/project",
//!   "model": "claude-sonnet-4-6",
//!   "provider": "anthropic",
//!   "turn_number": 3,
//!   "timestamp": "2025-01-15T10:30:00Z"
//! }
//! ```
//!
//! `turn-end` additionally carries `intent_title` so the orchestrator can use
//! it as the atomic record message:
//!
//! ```json
//! {
//!   "session_id": "rapid-wildflower-4475",
//!   "cwd": "/path/to/project",
//!   "model": "claude-sonnet-4-6",
//!   "provider": "anthropic",
//!   "turn_number": 3,
//!   "intent_title": "Add TypeScript hello world scaffold",
//!   "timestamp": "2025-01-15T10:30:00Z"
//! }
//! ```
//!
//! # Detection
//!
//! Sherpa is considered present if `~/.sherpa/sessions/` exists — the same
//! directory that `atomic-tui/src/session.rs` writes session snapshots to.
//!
//! # Installation
//!
//! Sherpa hooks are self-managed by the TUI — there is no config file to edit.
//! `install()` / `uninstall()` are no-ops that return `Ok`. Detection happens
//! via the `~/.sherpa/sessions/` directory.

use std::path::Path;

use serde::Deserialize;

use crate::error::{AgentError, AgentResult};
use crate::event::{HookType, TurnEvent};
use crate::hooks::AgentHook;

/// The Sherpa sessions directory relative to home (`~/.sherpa/sessions/`).
const SHERPA_SESSIONS_SUBDIR: &[&str] = &[".sherpa", "sessions"];

// ---------------------------------------------------------------------------
// JSON input structs
// ---------------------------------------------------------------------------

/// Common fields present in every Sherpa hook payload.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct SherpaHookInput {
    /// Sherpa session name (e.g. "rapid-wildflower-4475").
    session_id: String,

    /// Absolute path to the project root (the working directory of the TUI).
    #[serde(default)]
    cwd: Option<String>,

    /// LLM model identifier (e.g. "claude-sonnet-4-6").
    #[serde(default)]
    model: Option<String>,

    /// LLM provider (e.g. "anthropic", "openai").
    #[serde(default)]
    provider: Option<String>,

    /// Current turn counter at the time of the event.
    #[serde(default)]
    turn_number: u32,

    /// ISO-8601 timestamp of the event.
    #[serde(default)]
    timestamp: Option<String>,

    /// Intent title — present on `turn-start` and `turn-end`.
    ///
    /// Used as the atomic record commit message on `turn-end`.
    #[serde(default)]
    intent_title: Option<String>,
}

// ---------------------------------------------------------------------------
// SherpaHook
// ---------------------------------------------------------------------------

/// Agent hook adapter for Sherpa TUI sessions.
///
/// Implements [`AgentHook`] so the `TurnOrchestrator` can manage Sherpa
/// sessions with the same stack-fork / record / switch-back lifecycle as
/// Claude Code and OpenCode.
#[derive(Debug, Default)]
pub struct SherpaHook {
    _private: (),
}

impl SherpaHook {
    /// Create a new `SherpaHook`.
    pub fn new() -> Self {
        Self { _private: () }
    }

    /// Returns the path to `~/.sherpa/sessions/` if the home directory is
    /// resolvable, otherwise `None`.
    pub fn sherpa_sessions_dir() -> Option<std::path::PathBuf> {
        dirs::home_dir().map(|home| {
            SHERPA_SESSIONS_SUBDIR
                .iter()
                .fold(home, |acc, component| acc.join(component))
        })
    }

    /// Parse the raw JSON bytes into a [`SherpaHookInput`], returning a
    /// well-typed [`AgentError`] on failure.
    fn parse_input(
        agent: &str,
        hook_type: HookType,
        input: &[u8],
    ) -> AgentResult<(SherpaHookInput, serde_json::Value)> {
        let raw: serde_json::Value =
            serde_json::from_slice(input).map_err(|e| AgentError::HookParseFailed {
                agent: agent.to_string(),
                hook_type: hook_type.as_str().to_string(),
                reason: e.to_string(),
            })?;

        let parsed: SherpaHookInput =
            serde_json::from_value(raw.clone()).map_err(|e| AgentError::HookParseFailed {
                agent: agent.to_string(),
                hook_type: hook_type.as_str().to_string(),
                reason: e.to_string(),
            })?;

        Ok((parsed, raw))
    }
}

impl AgentHook for SherpaHook {
    fn name(&self) -> &str {
        "sherpa"
    }

    fn display_name(&self) -> &str {
        "Sherpa"
    }

    fn parse_event(&self, hook_type: HookType, input: &[u8]) -> AgentResult<TurnEvent> {
        if input.is_empty() {
            return Err(AgentError::HookInputEmpty {
                agent: self.name().to_string(),
                hook_type: hook_type.as_str().to_string(),
            });
        }

        let (parsed, raw) = Self::parse_input(self.name(), hook_type, input)?;

        if parsed.session_id.is_empty() {
            return Err(AgentError::HookParseFailed {
                agent: self.name().to_string(),
                hook_type: hook_type.as_str().to_string(),
                reason: "session_id is empty".to_string(),
            });
        }

        let mut event = TurnEvent::new(parsed.session_id.clone(), hook_type).with_raw_json(raw);

        // Stamp model and provider so the orchestrator can persist them on
        // the AgentSession — same pattern as OpenCode.
        if let Some(model) = &parsed.model {
            if !model.is_empty() {
                if let Some(ref mut raw_json) = event.raw_json {
                    if let Some(obj) = raw_json.as_object_mut() {
                        obj.insert(
                            "model".to_string(),
                            serde_json::Value::String(model.clone()),
                        );
                    }
                }
            }
        }
        if let Some(provider) = &parsed.provider {
            if !provider.is_empty() {
                if let Some(ref mut raw_json) = event.raw_json {
                    if let Some(obj) = raw_json.as_object_mut() {
                        obj.insert(
                            "provider".to_string(),
                            serde_json::Value::String(provider.clone()),
                        );
                    }
                }
            }
        }

        match hook_type {
            HookType::TurnStart => {
                // intent_title doubles as the turn prompt — used by the
                // orchestrator to set session.current_prompt so the atomic
                // record message is meaningful.
                if let Some(title) = &parsed.intent_title {
                    event = event.with_prompt(title.clone());
                }
            }

            HookType::TurnEnd => {
                // intent_title becomes the change message on record_turn.
                if let Some(title) = &parsed.intent_title {
                    event = event.with_prompt(title.clone());
                }

                // turn_number stored in raw_json so the orchestrator can
                // include it in the SessionEnvelope.
                if let Some(ref mut raw_json) = event.raw_json {
                    if let Some(obj) = raw_json.as_object_mut() {
                        obj.insert(
                            "turn_number".to_string(),
                            serde_json::Value::Number(parsed.turn_number.into()),
                        );
                    }
                }
            }

            HookType::SessionStart | HookType::SessionEnd => {
                // No extra fields required beyond the common ones already set.
            }

            HookType::PreToolUse | HookType::PostToolUse => {
                // Sherpa does not currently use tool-use hooks — these are
                // handled inside the TUI itself via TraceEvent. If the verb
                // is ever emitted we parse it correctly but take no action.
            }
        }

        Ok(event)
    }

    /// Sherpa is self-managed — no config file to write.
    fn install(&self, _repo_root: &Path) -> AgentResult<usize> {
        // The Sherpa TUI calls `atomic agent hooks sherpa <verb>` directly.
        // There is no external config file to patch.
        Ok(0)
    }

    /// Sherpa is self-managed — no config file to clean up.
    fn uninstall(&self, _repo_root: &Path) -> AgentResult<()> {
        Ok(())
    }

    /// Sherpa hooks are always "installed" when the TUI binary is present.
    ///
    /// We cannot easily introspect the TUI binary from here, so we return
    /// `true` whenever `~/.sherpa/sessions/` exists (i.e. the TUI has been
    /// run at least once).
    fn is_installed(&self, _repo_root: &Path) -> bool {
        Self::sherpa_sessions_dir()
            .map(|p| p.is_dir())
            .unwrap_or(false)
    }

    fn supported_hooks(&self) -> Vec<HookType> {
        vec![
            HookType::SessionStart,
            HookType::SessionEnd,
            HookType::TurnStart,
            HookType::TurnEnd,
        ]
    }

    /// Sherpa is present when `~/.sherpa/sessions/` exists.
    fn detect_presence(&self, _repo_root: &Path) -> bool {
        Self::sherpa_sessions_dir()
            .map(|p| p.is_dir())
            .unwrap_or(false)
    }

    fn hook_verbs(&self) -> Vec<&str> {
        vec!["session-start", "session-end", "turn-start", "turn-end"]
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_hook() -> SherpaHook {
        SherpaHook::new()
    }

    // --- name / display_name ---

    #[test]
    fn test_name() {
        assert_eq!(make_hook().name(), "sherpa");
    }

    #[test]
    fn test_display_name() {
        assert_eq!(make_hook().display_name(), "Sherpa");
    }

    // --- supported_hooks ---

    #[test]
    fn test_supported_hooks() {
        let hooks = make_hook().supported_hooks();
        assert!(hooks.contains(&HookType::SessionStart));
        assert!(hooks.contains(&HookType::SessionEnd));
        assert!(hooks.contains(&HookType::TurnStart));
        assert!(hooks.contains(&HookType::TurnEnd));
        // Tool-use hooks are NOT in supported list
        assert!(!hooks.contains(&HookType::PreToolUse));
        assert!(!hooks.contains(&HookType::PostToolUse));
    }

    // --- hook_verbs ---

    #[test]
    fn test_hook_verbs() {
        let hook = make_hook();
        let verbs = hook.hook_verbs();
        assert!(verbs.contains(&"session-start"));
        assert!(verbs.contains(&"session-end"));
        assert!(verbs.contains(&"turn-start"));
        assert!(verbs.contains(&"turn-end"));
    }

    // --- install / uninstall are no-ops ---

    #[test]
    fn test_install_is_noop() {
        let hook = make_hook();
        let result = hook.install(Path::new("/tmp"));
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0);
    }

    #[test]
    fn test_uninstall_is_noop() {
        let hook = make_hook();
        let result = hook.uninstall(Path::new("/tmp"));
        assert!(result.is_ok());
    }

    // --- parse_event: empty input ---

    #[test]
    fn test_parse_event_empty_input() {
        let hook = make_hook();
        let result = hook.parse_event(HookType::SessionStart, b"");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, AgentError::HookInputEmpty { .. }));
    }

    // --- parse_event: invalid JSON ---

    #[test]
    fn test_parse_event_invalid_json() {
        let hook = make_hook();
        let result = hook.parse_event(HookType::SessionStart, b"not json");
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            AgentError::HookParseFailed { .. }
        ));
    }

    // --- parse_event: missing session_id ---

    #[test]
    fn test_parse_event_missing_session_id() {
        let hook = make_hook();
        let input = br#"{"cwd": "/tmp", "model": "claude-sonnet-4-6"}"#;
        let result = hook.parse_event(HookType::SessionStart, input);
        assert!(result.is_err());
    }

    // --- parse_event: empty session_id ---

    #[test]
    fn test_parse_event_empty_session_id() {
        let hook = make_hook();
        let input = br#"{"session_id": "", "cwd": "/tmp"}"#;
        let result = hook.parse_event(HookType::SessionStart, input);
        assert!(result.is_err());
    }

    // --- parse_event: session-start ---

    #[test]
    fn test_parse_session_start() {
        let hook = make_hook();
        let input = br#"{
            "session_id": "rapid-wildflower-4475",
            "cwd": "/Users/dev/hello-world",
            "model": "claude-sonnet-4-6",
            "provider": "anthropic",
            "turn_number": 0,
            "timestamp": "2025-01-15T10:30:00Z"
        }"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert_eq!(event.session_id, "rapid-wildflower-4475");
        assert_eq!(event.event_type, HookType::SessionStart);
        assert!(event.prompt.is_none());
    }

    // --- parse_event: session-start captures model/provider ---

    #[test]
    fn test_parse_session_start_model_provider_in_raw() {
        let hook = make_hook();
        let input = br#"{
            "session_id": "tiny-star-99",
            "model": "claude-opus-4",
            "provider": "anthropic"
        }"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        let raw = event.raw_json.unwrap();
        assert_eq!(raw["model"].as_str(), Some("claude-opus-4"));
        assert_eq!(raw["provider"].as_str(), Some("anthropic"));
    }

    // --- parse_event: session-end ---

    #[test]
    fn test_parse_session_end() {
        let hook = make_hook();
        let input = br#"{
            "session_id": "rapid-wildflower-4475",
            "cwd": "/Users/dev/hello-world",
            "turn_number": 3
        }"#;
        let event = hook.parse_event(HookType::SessionEnd, input).unwrap();
        assert_eq!(event.session_id, "rapid-wildflower-4475");
        assert_eq!(event.event_type, HookType::SessionEnd);
    }

    // --- parse_event: turn-start carries intent_title as prompt ---

    #[test]
    fn test_parse_turn_start_sets_prompt() {
        let hook = make_hook();
        let input = br#"{
            "session_id": "rapid-wildflower-4475",
            "model": "claude-sonnet-4-6",
            "provider": "anthropic",
            "turn_number": 1,
            "intent_title": "Create TypeScript hello world CLI"
        }"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        assert_eq!(event.event_type, HookType::TurnStart);
        assert_eq!(
            event.prompt.as_deref(),
            Some("Create TypeScript hello world CLI")
        );
    }

    // --- parse_event: turn-start without intent_title has no prompt ---

    #[test]
    fn test_parse_turn_start_no_intent_title() {
        let hook = make_hook();
        let input = br#"{"session_id": "s1", "turn_number": 1}"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        assert!(event.prompt.is_none());
    }

    // --- parse_event: turn-end carries intent_title as prompt ---

    #[test]
    fn test_parse_turn_end_sets_prompt() {
        let hook = make_hook();
        let input = br#"{
            "session_id": "rapid-wildflower-4475",
            "model": "claude-sonnet-4-6",
            "provider": "anthropic",
            "turn_number": 2,
            "intent_title": "Add TypeScript hello world scaffold"
        }"#;
        let event = hook.parse_event(HookType::TurnEnd, input).unwrap();
        assert_eq!(event.event_type, HookType::TurnEnd);
        assert_eq!(
            event.prompt.as_deref(),
            Some("Add TypeScript hello world scaffold")
        );
    }

    // --- parse_event: turn-end stores turn_number in raw_json ---

    #[test]
    fn test_parse_turn_end_turn_number_in_raw() {
        let hook = make_hook();
        let input = br#"{
            "session_id": "s1",
            "turn_number": 5,
            "intent_title": "Fix auth bug"
        }"#;
        let event = hook.parse_event(HookType::TurnEnd, input).unwrap();
        let raw = event.raw_json.unwrap();
        assert_eq!(raw["turn_number"].as_u64(), Some(5));
    }

    // --- parse_event: extra fields are ignored ---

    #[test]
    fn test_parse_extra_fields_ignored() {
        let hook = make_hook();
        let input = br#"{
            "session_id": "s1",
            "unknown_field": "whatever",
            "another": 42
        }"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert_eq!(event.session_id, "s1");
    }

    // --- default trait ---

    #[test]
    fn test_default() {
        let hook = SherpaHook::default();
        assert_eq!(hook.name(), "sherpa");
    }

    // --- debug ---

    #[test]
    fn test_debug() {
        let hook = make_hook();
        let s = format!("{:?}", hook);
        assert!(s.contains("SherpaHook"));
    }
}
