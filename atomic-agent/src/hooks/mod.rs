//! Agent hook adapters for AI coding tools.
//!
//! This module defines the [`AgentHook`] trait that each agent adapter implements,
//! and the [`AgentRegistry`] that manages available adapters. The trait normalizes
//! agent-specific hook formats (Claude Code, Copilot, Gemini CLI, Codex, OpenCode) into
//! the common [`TurnEvent`] type.
//!
//! # Architecture
//!
//! Each AI coding agent has its own hook system with different JSON formats,
//! config file locations, and lifecycle event names. The `AgentHook` trait
//! abstracts these differences:
//!
//! ```text
//! ┌─────────────────┐  ┌─────────────────┐  ┌─────────────────┐
//! │  Claude Code     │  │  Gemini CLI      │  │  Codex           │
//! │  .claude/        │  │  .gemini/        │  │  .codex/         │
//! │  settings.json   │  │  settings.json   │  │  config.json     │
//! │  JSONL stdin     │  │  JSON stdin      │  │  JSON stdin      │
//! └────────┬────────┘  └────────┬────────┘  └────────┬────────┘
//!          │                    │                     │
//!          ▼                    ▼                     ▼
//!   ClaudeCodeHook       GeminiCliHook          CodexHook
//!   impl AgentHook       impl AgentHook        impl AgentHook
//!          │                    │                     │
//!          └────────────┬───────┘─────────────────────┘
//!                       ▼
//!               TurnEvent (common type)
//!                       │
//!                       ▼
//!               TurnOrchestrator
//! ```
//!
//! # Hook Installation
//!
//! When `atomic agent enable --agent claude-code` is run, the adapter's
//! `install()` method writes hook entries into the agent's config file.
//! The hooks call back to `atomic agent hooks <agent> <verb>` which
//! reads stdin, parses via `parse_event()`, and feeds the resulting
//! `TurnEvent` to the orchestrator.
//!
//! # Adding a New Agent
//!
//! 1. Create a new file `hooks/<agent_name>.rs`
//! 2. Implement `AgentHook` for your agent struct
//! 3. Register it in the default registry via `register_defaults()`
//! 4. Add the agent's hook verbs to [`HookType::from_verb`](crate::event::HookType::from_verb)
//!
//! # Example
//!
//! ```rust
//! use atomic_agent::hooks::AgentRegistry;
//!
//! let registry = AgentRegistry::with_defaults();
//! let names = registry.list();
//! assert!(names.contains(&"claude-code"));
//! ```

pub mod claude_code;
pub mod cline;
pub mod codex;
pub mod copilot;
pub mod cursor;
pub mod devin;
pub mod gemini_cli;
pub mod hermes;
pub mod kilo;
pub mod kiro;
pub mod manifest;
pub mod opencode;
pub mod pi;
pub mod sherpa;

use std::fmt;
use std::path::Path;

use crate::error::{AgentError, AgentResult};
use crate::event::{HookType, TurnEvent};

// Hook command guard

/// Wrap an `atomic agent hooks …` invocation in the shell guard that installed
/// hooks run behind.
///
/// The hook must only fire where Atomic's graph is reachable, but it must fire
/// in **both** kinds of working tree:
///
/// * a canonical checkout, which has a `.atomic/` directory; and
/// * an agent sandbox, which has **no** `.atomic/` of its own — only a
///   `.atomic-sandbox` pointer file to the canonical graph.
///
/// Guarding on `.atomic` alone silently drops all provenance for sandboxed
/// agents (the guard is false, so the recorder never runs). Adding the sandbox
/// arm lets the hook fire there too; `atomic agent hooks` then resolves the
/// pointer and records into the canonical graph.
///
/// POSIX `&&`/`||` are equal-precedence and left-associative, so
/// `test -d .atomic || test -f .atomic-sandbox && CMD || true` parses as
/// `(((test -d .atomic || test -f .atomic-sandbox) && CMD) || true)` — the
/// recorder runs when either marker is present, and a recorder failure is
/// swallowed so the agent turn is never blocked.
pub(crate) fn guarded_hook_command(inner: &str) -> String {
    format!(
        "test -d .atomic || test -f {pointer} && {inner} || true",
        pointer = atomic_repository::SANDBOX_POINTER,
    )
}

// AgentHook Trait

/// Trait for agent hook adapters.
///
/// Each AI coding agent (Claude Code, Gemini CLI, Codex, OpenCode) implements
/// this trait to:
///
/// 1. **Parse** agent-specific JSON from stdin into a common [`TurnEvent`]
/// 2. **Install** hooks into the agent's configuration file
/// 3. **Uninstall** hooks from the agent's configuration file
/// 4. **Detect** whether hooks are currently installed
///
/// Implementations must be `Send + Sync` because the registry holds them
/// behind shared references and hook handlers may run concurrently.
///
/// # Minimal Implementation
///
/// At minimum, an adapter must implement `name()`, `display_name()`,
/// `parse_event()`, and `supported_hooks()`. The install/uninstall methods
/// can return `Ok(0)` / `Ok(())` for agents that don't support hook
/// installation (e.g., agents detected via file watching instead).
pub trait AgentHook: Send + Sync + fmt::Debug {
    /// Returns the registry key for this agent.
    ///
    /// This is the string used in CLI commands:
    /// `atomic agent enable --agent <name>`
    /// `atomic agent hooks <name> <verb>`
    ///
    /// Convention: lowercase with hyphens (e.g., "claude-code", "gemini-cli").
    fn name(&self) -> &str;

    /// Returns a human-readable display name.
    ///
    /// Used in UI output and log messages (e.g., "Claude Code", "Gemini CLI").
    fn display_name(&self) -> &str;

    /// Parse hook input from an agent callback into a normalized [`TurnEvent`].
    ///
    /// The `input` parameter is the raw bytes read from stdin when the agent
    /// invokes the hook. Each agent sends a different JSON format.
    ///
    /// # Arguments
    ///
    /// * `hook_type` - The type of lifecycle event (from the CLI verb)
    /// * `input` - Raw bytes from stdin (typically JSON)
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::HookParseFailed`] if the input cannot be parsed,
    /// or [`AgentError::HookInputEmpty`] if `input` is empty.
    fn parse_event(&self, hook_type: HookType, input: &[u8]) -> AgentResult<TurnEvent>;

    /// Install hooks into the agent's configuration file.
    ///
    /// This writes entries into the agent's settings file (e.g.,
    /// `.claude/settings.json`) that call back to `atomic agent hooks`.
    ///
    /// # Arguments
    ///
    /// * `repo_root` - The repository root directory (where `.atomic/` lives)
    ///
    /// # Returns
    ///
    /// The number of hooks that were installed. Returns 0 if all hooks
    /// were already present.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::ConfigError`] if the config file cannot be
    /// read or written.
    fn install(&self, repo_root: &Path) -> AgentResult<usize>;

    /// Remove hooks from the agent's configuration file.
    ///
    /// Removes only hooks with the `atomic agent hooks` prefix — other
    /// hooks in the agent's config are preserved.
    ///
    /// # Arguments
    ///
    /// * `repo_root` - The repository root directory
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::ConfigError`] if the config file cannot be
    /// read or written. Returns `Ok(())` if no config file exists (nothing
    /// to uninstall).
    fn uninstall(&self, repo_root: &Path) -> AgentResult<()>;

    /// Check whether hooks are currently installed for this agent.
    ///
    /// Looks for the `atomic agent hooks <name>` prefix in the agent's
    /// configuration file.
    ///
    /// # Arguments
    ///
    /// * `repo_root` - The repository root directory
    fn is_installed(&self, repo_root: &Path) -> bool;

    /// Returns the hook types this agent supports.
    ///
    /// Not all agents support all hook types. For example, some agents
    /// may not have a `PreToolUse` / `PostToolUse` hook.
    fn supported_hooks(&self) -> Vec<HookType>;

    /// Detect whether this agent is present in the repository.
    ///
    /// Checks for agent-specific markers (e.g., `.claude/` directory for
    /// Claude Code, `.gemini/` directory for Gemini CLI).
    ///
    /// Default implementation returns `false`.
    fn detect_presence(&self, repo_root: &Path) -> bool {
        let _ = repo_root;
        false
    }

    /// Returns the hook verbs (CLI subcommand names) this agent uses.
    ///
    /// These become subcommands under `atomic agent hooks <name> <verb>`.
    /// Each verb maps to a [`HookType`] via [`HookType::from_verb`].
    ///
    /// # Example
    ///
    /// Claude Code returns:
    /// `["session-start", "session-end", "stop", "user-prompt-submit", "pre-task", "post-task", "post-todo"]`
    fn hook_verbs(&self) -> Vec<&str>;
}

// AgentRegistry

/// Registry of available agent hook adapters.
///
/// The registry holds all known agent adapters and provides lookup by name,
/// listing, and auto-detection of which agents are present in a repository.
///
/// # Thread Safety
///
/// The registry is immutable after construction. Agent adapters are stored
/// as `Box<dyn AgentHook>` which requires `Send + Sync`.
///
/// # Example
///
/// ```rust
/// use atomic_agent::hooks::AgentRegistry;
///
/// let registry = AgentRegistry::with_defaults();
///
/// // List available agents
/// for name in registry.list() {
///     println!("Available agent: {}", name);
/// }
///
/// // Look up a specific agent
/// if let Some(agent) = registry.get("claude-code") {
///     println!("Found: {}", agent.display_name());
/// }
/// ```
pub struct AgentRegistry {
    agents: Vec<Box<dyn AgentHook>>,
}

impl AgentRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self { agents: Vec::new() }
    }

    /// Create a registry pre-populated with all built-in agent adapters.
    ///
    /// Currently includes:
    /// - Claude Code (`claude-code`)
    /// - Cline (`cline`)
    /// - Codex (`codex`)
    /// - Copilot (`copilot`)
    /// - Cursor (`cursor`)
    /// - Devin Desktop (`devin`)
    /// - Gemini CLI (`gemini-cli`)
    /// - Hermes Agent (`hermes`)
    /// - Kilo Code (`kilo`)
    /// - Kiro (`kiro`)
    /// - OpenCode (`opencode`)
    /// - Pi (`pi`)
    /// - Sherpa (`sherpa`)
    pub fn with_defaults() -> Self {
        let mut registry = Self::new();
        registry.register(Box::new(claude_code::ClaudeCodeHook::new()));
        registry.register(Box::new(cline::ClineHook::new()));
        registry.register(Box::new(codex::CodexHook::new()));
        registry.register(Box::new(copilot::CopilotHook::new()));
        registry.register(Box::new(cursor::CursorHook::new()));
        registry.register(Box::new(devin::DevinHook::new()));
        registry.register(Box::new(gemini_cli::GeminiCliHook::new()));
        registry.register(Box::new(hermes::HermesHook::new()));
        registry.register(Box::new(kilo::KiloHook::new()));
        registry.register(Box::new(kiro::KiroHook::new()));
        registry.register(Box::new(opencode::OpenCodeHook::new()));
        registry.register(Box::new(pi::PiHook::new()));
        registry.register(Box::new(sherpa::SherpaHook::new()));
        registry
    }

    /// Register a new agent adapter.
    ///
    /// If an agent with the same name is already registered, the new one
    /// replaces it.
    pub fn register(&mut self, agent: Box<dyn AgentHook>) {
        // Replace existing agent with the same name
        self.agents.retain(|a| a.name() != agent.name());
        self.agents.push(agent);
    }

    /// Look up an agent adapter by name.
    ///
    /// Returns `None` if no agent with the given name is registered.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_agent::hooks::AgentRegistry;
    ///
    /// let registry = AgentRegistry::with_defaults();
    /// let agent = registry.get("claude-code");
    /// assert!(agent.is_some());
    /// assert!(registry.get("nonexistent").is_none());
    /// ```
    pub fn get(&self, name: &str) -> Option<&dyn AgentHook> {
        self.agents
            .iter()
            .find(|a| a.name() == name)
            .map(|a| a.as_ref())
    }

    /// Look up an agent adapter by name, returning an error if not found.
    ///
    /// This is a convenience method for CLI commands that need to validate
    /// the agent name and produce a helpful error message.
    pub fn require(&self, name: &str) -> AgentResult<&dyn AgentHook> {
        self.get(name).ok_or_else(|| AgentError::AgentNotFound {
            name: name.to_string(),
            available: self.list().to_vec().join(", "),
        })
    }

    /// Returns a list of all registered agent names, sorted alphabetically.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_agent::hooks::AgentRegistry;
    ///
    /// let registry = AgentRegistry::with_defaults();
    /// let names = registry.list();
    /// assert!(!names.is_empty());
    /// ```
    pub fn list(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.agents.iter().map(|a| a.name()).collect();
        names.sort_unstable();
        names
    }

    /// Returns the number of registered agents.
    pub fn count(&self) -> usize {
        self.agents.len()
    }

    /// Returns `true` if no agents are registered.
    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }

    /// Detect which registered agents are present in the given repository.
    ///
    /// Calls `detect_presence()` on each registered agent and returns the
    /// names of agents that are detected.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use atomic_agent::hooks::AgentRegistry;
    /// use std::path::Path;
    ///
    /// let registry = AgentRegistry::with_defaults();
    /// let detected = registry.detect(Path::new("/path/to/repo"));
    /// for name in detected {
    ///     println!("Detected agent: {}", name);
    /// }
    /// ```
    pub fn detect(&self, repo_root: &Path) -> Vec<&str> {
        self.agents
            .iter()
            .filter(|a| a.detect_presence(repo_root))
            .map(|a| a.name())
            .collect()
    }

    /// Returns all agents that have hooks currently installed.
    pub fn installed(&self, repo_root: &Path) -> Vec<&str> {
        self.agents
            .iter()
            .filter(|a| a.is_installed(repo_root))
            .map(|a| a.name())
            .collect()
    }

    /// Returns an iterator over all registered agent adapters.
    pub fn iter(&self) -> impl Iterator<Item = &dyn AgentHook> {
        self.agents.iter().map(|a| a.as_ref())
    }
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self::with_defaults()
    }
}

impl fmt::Debug for AgentRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentRegistry")
            .field("agents", &self.list())
            .finish()
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    // Mock agent for testing

    #[derive(Debug)]
    struct MockAgent {
        agent_name: String,
        agent_display: String,
        presence: bool,
        installed: bool,
    }

    impl MockAgent {
        fn new(name: &str) -> Self {
            Self {
                agent_name: name.to_string(),
                agent_display: format!("Mock {}", name),
                presence: false,
                installed: false,
            }
        }

        fn with_presence(mut self, present: bool) -> Self {
            self.presence = present;
            self
        }

        fn with_installed(mut self, installed: bool) -> Self {
            self.installed = installed;
            self
        }
    }

    impl AgentHook for MockAgent {
        fn name(&self) -> &str {
            &self.agent_name
        }

        fn display_name(&self) -> &str {
            &self.agent_display
        }

        fn parse_event(&self, hook_type: HookType, input: &[u8]) -> AgentResult<TurnEvent> {
            if input.is_empty() {
                return Err(AgentError::HookInputEmpty {
                    agent: self.agent_name.clone(),
                    hook_type: hook_type.as_str().to_string(),
                });
            }

            let raw: serde_json::Value = serde_json::from_slice(input)?;
            let session_id = raw["session_id"].as_str().unwrap_or("unknown").to_string();

            Ok(TurnEvent::new(session_id, hook_type).with_raw_json(raw))
        }

        fn install(&self, _repo_root: &Path) -> AgentResult<usize> {
            Ok(3) // pretend we installed 3 hooks
        }

        fn uninstall(&self, _repo_root: &Path) -> AgentResult<()> {
            Ok(())
        }

        fn is_installed(&self, _repo_root: &Path) -> bool {
            self.installed
        }

        fn supported_hooks(&self) -> Vec<HookType> {
            vec![
                HookType::SessionStart,
                HookType::SessionEnd,
                HookType::TurnStart,
                HookType::TurnEnd,
            ]
        }

        fn detect_presence(&self, _repo_root: &Path) -> bool {
            self.presence
        }

        fn hook_verbs(&self) -> Vec<&str> {
            vec!["session-start", "session-end", "start", "stop"]
        }
    }

    // AgentHook trait tests

    #[test]
    fn test_mock_agent_parse_event_success() {
        let agent = MockAgent::new("test");
        let input = br#"{"session_id": "sess-123"}"#;
        let event = agent.parse_event(HookType::TurnStart, input).unwrap();
        assert_eq!(event.session_id, "sess-123");
        assert_eq!(event.event_type, HookType::TurnStart);
    }

    #[test]
    fn test_mock_agent_parse_event_empty_input() {
        let agent = MockAgent::new("test");
        let result = agent.parse_event(HookType::TurnStart, b"");
        assert!(result.is_err());
        match result.unwrap_err() {
            AgentError::HookInputEmpty { agent, hook_type } => {
                assert_eq!(agent, "test");
                assert_eq!(hook_type, "turn_start");
            }
            other => panic!("Expected HookInputEmpty, got: {:?}", other),
        }
    }

    #[test]
    fn test_mock_agent_parse_event_invalid_json() {
        let agent = MockAgent::new("test");
        let result = agent.parse_event(HookType::TurnStart, b"not json");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), AgentError::Json(_)));
    }

    #[test]
    fn test_mock_agent_parse_event_missing_session_id() {
        let agent = MockAgent::new("test");
        let input = br#"{"other_field": "value"}"#;
        let event = agent.parse_event(HookType::TurnEnd, input).unwrap();
        // Falls back to "unknown" session ID
        assert_eq!(event.session_id, "unknown");
    }

    #[test]
    fn test_mock_agent_install() {
        let agent = MockAgent::new("test");
        let count = agent.install(Path::new("/tmp")).unwrap();
        assert_eq!(count, 3);
    }

    #[test]
    fn test_mock_agent_uninstall() {
        let agent = MockAgent::new("test");
        assert!(agent.uninstall(Path::new("/tmp")).is_ok());
    }

    #[test]
    fn test_mock_agent_supported_hooks() {
        let agent = MockAgent::new("test");
        let hooks = agent.supported_hooks();
        assert_eq!(hooks.len(), 4);
        assert!(hooks.contains(&HookType::TurnStart));
        assert!(hooks.contains(&HookType::TurnEnd));
        assert!(hooks.contains(&HookType::SessionStart));
        assert!(hooks.contains(&HookType::SessionEnd));
    }

    #[test]
    fn test_mock_agent_detect_presence_default_false() {
        let agent = MockAgent::new("test");
        assert!(!agent.detect_presence(Path::new("/tmp")));
    }

    #[test]
    fn test_mock_agent_detect_presence_true() {
        let agent = MockAgent::new("test").with_presence(true);
        assert!(agent.detect_presence(Path::new("/tmp")));
    }

    #[test]
    fn test_mock_agent_hook_verbs() {
        let agent = MockAgent::new("test");
        let verbs = agent.hook_verbs();
        assert_eq!(verbs.len(), 4);
        assert!(verbs.contains(&"session-start"));
        assert!(verbs.contains(&"stop"));
    }

    // AgentRegistry tests

    #[test]
    fn test_registry_new_is_empty() {
        let registry = AgentRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.count(), 0);
        assert!(registry.list().is_empty());
    }

    #[test]
    fn test_registry_with_defaults_has_claude_code() {
        let registry = AgentRegistry::with_defaults();
        assert!(!registry.is_empty());
        assert!(registry.get("claude-code").is_some());
        let names = registry.list();
        assert!(names.contains(&"claude-code"));
    }

    #[test]
    fn test_registry_register_and_get() {
        let mut registry = AgentRegistry::new();
        registry.register(Box::new(MockAgent::new("test-agent")));

        assert_eq!(registry.count(), 1);
        let agent = registry.get("test-agent").unwrap();
        assert_eq!(agent.name(), "test-agent");
        assert_eq!(agent.display_name(), "Mock test-agent");
    }

    #[test]
    fn test_registry_get_nonexistent() {
        let registry = AgentRegistry::new();
        assert!(registry.get("nonexistent").is_none());
    }

    #[test]
    fn test_registry_require_success() {
        let mut registry = AgentRegistry::new();
        registry.register(Box::new(MockAgent::new("test-agent")));
        assert!(registry.require("test-agent").is_ok());
    }

    #[test]
    fn test_registry_require_not_found() {
        let mut registry = AgentRegistry::new();
        registry.register(Box::new(MockAgent::new("alpha")));
        let err = registry.require("beta").unwrap_err();
        match err {
            AgentError::AgentNotFound { name, available } => {
                assert_eq!(name, "beta");
                assert!(available.contains("alpha"));
            }
            other => panic!("Expected AgentNotFound, got: {:?}", other),
        }
    }

    #[test]
    fn test_registry_register_replaces_duplicate() {
        let mut registry = AgentRegistry::new();
        registry.register(Box::new(MockAgent::new("agent-a")));
        registry.register(Box::new(MockAgent::new("agent-a")));
        assert_eq!(registry.count(), 1);
    }

    #[test]
    fn test_registry_list_sorted() {
        let mut registry = AgentRegistry::new();
        registry.register(Box::new(MockAgent::new("zebra")));
        registry.register(Box::new(MockAgent::new("alpha")));
        registry.register(Box::new(MockAgent::new("middle")));

        let names = registry.list();
        assert_eq!(names, vec!["alpha", "middle", "zebra"]);
    }

    #[test]
    fn test_registry_detect_filters_by_presence() {
        let mut registry = AgentRegistry::new();
        registry.register(Box::new(MockAgent::new("present").with_presence(true)));
        registry.register(Box::new(MockAgent::new("absent").with_presence(false)));

        let detected = registry.detect(Path::new("/tmp"));
        assert_eq!(detected.len(), 1);
        assert_eq!(detected[0], "present");
    }

    #[test]
    fn test_registry_detect_none_present() {
        let mut registry = AgentRegistry::new();
        registry.register(Box::new(MockAgent::new("a").with_presence(false)));
        registry.register(Box::new(MockAgent::new("b").with_presence(false)));

        let detected = registry.detect(Path::new("/tmp"));
        assert!(detected.is_empty());
    }

    #[test]
    fn test_registry_installed_filters_by_installed() {
        let mut registry = AgentRegistry::new();
        registry.register(Box::new(MockAgent::new("yes").with_installed(true)));
        registry.register(Box::new(MockAgent::new("no").with_installed(false)));

        let installed = registry.installed(Path::new("/tmp"));
        assert_eq!(installed.len(), 1);
        assert_eq!(installed[0], "yes");
    }

    #[test]
    fn test_registry_iter() {
        let mut registry = AgentRegistry::new();
        registry.register(Box::new(MockAgent::new("a")));
        registry.register(Box::new(MockAgent::new("b")));
        registry.register(Box::new(MockAgent::new("c")));

        let names: Vec<&str> = registry.iter().map(|a| a.name()).collect();
        assert_eq!(names.len(), 3);
        assert!(names.contains(&"a"));
        assert!(names.contains(&"b"));
        assert!(names.contains(&"c"));
    }

    #[test]
    fn test_registry_default_same_as_with_defaults() {
        let default = AgentRegistry::default();
        let explicit = AgentRegistry::with_defaults();
        assert_eq!(default.list(), explicit.list());
    }

    #[test]
    fn test_registry_debug() {
        let mut registry = AgentRegistry::new();
        registry.register(Box::new(MockAgent::new("test")));
        let debug = format!("{:?}", registry);
        assert!(debug.contains("AgentRegistry"));
        assert!(debug.contains("test"));
    }

    // Trait object safety

    #[test]
    fn test_agent_hook_is_object_safe() {
        // This test verifies that AgentHook can be used as a trait object.
        // If this compiles, the trait is object-safe.
        let agent: Box<dyn AgentHook> = Box::new(MockAgent::new("test"));
        assert_eq!(agent.name(), "test");
    }

    #[test]
    fn test_agent_hook_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Box<dyn AgentHook>>();
    }

    // Multiple agents interaction

    #[test]
    fn test_registry_multiple_agents_independent() {
        let mut registry = AgentRegistry::new();
        registry.register(Box::new(MockAgent::new("claude-code").with_presence(true)));
        registry.register(Box::new(MockAgent::new("gemini-cli").with_presence(true)));
        registry.register(Box::new(MockAgent::new("codex").with_presence(false)));

        // Both detected agents work independently
        let detected = registry.detect(Path::new("/tmp"));
        assert_eq!(detected.len(), 2);

        let claude = registry.require("claude-code").unwrap();
        assert_eq!(claude.display_name(), "Mock claude-code");

        let gemini = registry.require("gemini-cli").unwrap();
        assert_eq!(gemini.display_name(), "Mock gemini-cli");
    }
}
