//! Error types for the atomic-agent crate.
//!
//! This module defines the error hierarchy for agent integration operations
//! including Watchman communication, hook parsing, session management, turn
//! lifecycle, and change recording.
//!
//! # Error Categories
//!
//! Errors are organized by subsystem:
//!
//! - **Watchman**: File watcher connection and query failures
//! - **Hook**: Agent hook parsing and configuration errors
//! - **Session**: Session state persistence and lifecycle errors
//! - **Turn**: Turn state machine violations
//! - **Record**: Change recording failures
//! - **Config**: Agent configuration file read/write errors
//! - **Identity**: Agent identity and delegation errors
//!
//! # Example
//!
//! ```rust
//! use atomic_agent::error::{AgentError, AgentResult};
//!
//! fn do_something() -> AgentResult<()> {
//!     Err(AgentError::SessionNotFound {
//!         session_id: "2026-01-15-abc123".to_string(),
//!     })
//! }
//! ```

use std::path::PathBuf;

use thiserror::Error;

/// Result type alias for agent operations.
pub type AgentResult<T> = Result<T, AgentError>;

/// Errors that can occur during agent operations.
#[derive(Debug, Error)]
pub enum AgentError {
    // Watchman / File Watcher Errors
    /// Watchman daemon is not running or not installed.
    ///
    /// This is a soft error — the system falls back to the polling-based
    /// `FallbackWatcher` when Watchman is unavailable.
    #[error("Watchman is not running (install from https://facebook.github.io/watchman/ or the fallback watcher will be used)")]
    WatchmanNotRunning,

    /// Failed to connect to the Watchman daemon.
    #[error("Failed to connect to Watchman: {reason}")]
    WatchmanConnectionFailed {
        /// Human-readable reason for the connection failure.
        reason: String,
    },

    /// A Watchman query or command failed.
    #[error("Watchman query failed: {reason}")]
    WatchmanQueryFailed {
        /// Human-readable reason for the query failure.
        reason: String,
    },

    /// Failed to resolve the repository root with Watchman.
    #[error("Watchman failed to resolve watch root for path: {path}")]
    WatchmanResolveRoot {
        /// The path that could not be resolved.
        path: PathBuf,
    },

    // Hook Parsing Errors
    /// Failed to parse JSON input from an agent hook callback.
    #[error("Failed to parse hook input for {agent} ({hook_type}): {reason}")]
    HookParseFailed {
        /// Which agent sent the hook (e.g., "claude-code", "gemini-cli").
        agent: String,
        /// The hook type that failed to parse (e.g., "stop", "user-prompt-submit").
        hook_type: String,
        /// What went wrong during parsing.
        reason: String,
    },

    /// Hook input was empty (stdin had no data).
    #[error("Empty hook input from {agent} for hook type {hook_type}")]
    HookInputEmpty {
        /// Which agent sent the empty input.
        agent: String,
        /// The hook type that received empty input.
        hook_type: String,
    },

    /// A required field was missing from the hook input JSON.
    #[error("Missing required field '{field}' in {agent} hook input for {hook_type}")]
    HookFieldMissing {
        /// Which agent sent the input.
        agent: String,
        /// The hook type.
        hook_type: String,
        /// The field that was missing.
        field: String,
    },

    // Agent Configuration Errors
    /// Unknown agent name — not registered in the `AgentRegistry`.
    #[error("Unknown agent: '{name}' (available agents: {available})")]
    AgentNotFound {
        /// The agent name that was requested.
        name: String,
        /// Comma-separated list of available agent names.
        available: String,
    },

    /// The requested adapter id is not registered in the [`crate::discovery::DiscoveryRegistry`].
    #[error("Unknown discovery adapter: '{name}' (available adapters: {available})")]
    AdapterNotFound {
        /// The adapter id that was requested.
        name: String,
        /// Comma-separated list of available adapter ids.
        available: String,
    },

    /// Hooks are already installed for this agent.
    #[error("Hooks already installed for {agent} (use --force to reinstall)")]
    AlreadyInstalled {
        /// The agent that already has hooks.
        agent: String,
    },

    /// Failed to read or write an agent's configuration file.
    #[error("Failed to {operation} agent config at {path}: {reason}")]
    ConfigError {
        /// "read" or "write".
        operation: String,
        /// Path to the configuration file.
        path: PathBuf,
        /// What went wrong.
        reason: String,
    },

    /// The repository does not have `.atomic/` initialized.
    #[error("Not an Atomic repository (no .atomic/ directory found at {path})")]
    NotARepository {
        /// The path that was checked.
        path: PathBuf,
    },

    // Session Errors
    /// No active session found for the given session ID.
    #[error("Session not found: '{session_id}'")]
    SessionNotFound {
        /// The session ID that was looked up.
        session_id: String,
    },

    /// Failed to save session state to disk.
    #[error("Failed to save session state for '{session_id}': {reason}")]
    SessionSaveFailed {
        /// The session ID.
        session_id: String,
        /// What went wrong.
        reason: String,
    },

    /// Failed to load session state from disk.
    #[error("Failed to load session state for '{session_id}': {reason}")]
    SessionLoadFailed {
        /// The session ID.
        session_id: String,
        /// What went wrong (e.g., corrupted JSON, IO error).
        reason: String,
    },

    /// Session ID contains unsafe characters (path traversal attempt).
    #[error("Invalid session ID '{session_id}': must not contain path separators or '..'")]
    SessionIdInvalid {
        /// The rejected session ID.
        session_id: String,
    },

    /// Multiple concurrent sessions detected that may conflict.
    #[error(
        "Concurrent session conflict: session '{existing}' is already active in this workspace"
    )]
    SessionConflict {
        /// The existing active session.
        existing: String,
        /// The new session that was attempted.
        new_session: String,
    },

    // Turn State Machine Errors
    /// `end_turn` was called without a preceding `begin_turn`.
    #[error("No active turn — end_turn called without begin_turn for session '{session_id}'")]
    TurnNotActive {
        /// The session ID.
        session_id: String,
    },

    /// `begin_turn` was called while another turn is already active.
    ///
    /// This can happen if the agent crashed mid-turn and restarted.
    /// The orchestrator treats this as Ctrl-C recovery and continues.
    #[error("Turn already active for session '{session_id}' (treating as Ctrl-C recovery)")]
    TurnAlreadyActive {
        /// The session ID.
        session_id: String,
    },

    /// A session-level operation was attempted but the session has ended.
    #[error("Session '{session_id}' has ended — cannot {operation}")]
    SessionEnded {
        /// The session ID.
        session_id: String,
        /// What was attempted.
        operation: String,
    },

    // Recording Errors
    /// Failed to record a turn as an Atomic change.
    #[error("Failed to record turn {turn_number} for session '{session_id}': {reason}")]
    RecordFailed {
        /// The session ID.
        session_id: String,
        /// Which turn number failed.
        turn_number: u32,
        /// What went wrong.
        reason: String,
    },

    /// The turn had no file changes — nothing to record.
    ///
    /// This is informational, not a fatal error. The orchestrator
    /// silently skips empty turns.
    #[error("Turn {turn_number} for session '{session_id}' had no file changes")]
    EmptyTurn {
        /// The session ID.
        session_id: String,
        /// The turn number.
        turn_number: u32,
    },

    /// Failed to create or access the agent's Atomic view.
    #[error("Failed to {operation} view '{view_name}': {reason}")]
    ViewError {
        /// "create", "open", or "switch".
        operation: String,
        /// The view name (e.g., "agent-2026-01-15-abc123").
        view_name: String,
        /// What went wrong.
        reason: String,
    },

    // Identity Errors
    /// Failed to create or retrieve an agent identity.
    #[error("Agent identity error for '{agent}': {reason}")]
    IdentityError {
        /// The agent name.
        agent: String,
        /// What went wrong.
        reason: String,
    },

    /// Failed to create a delegation from user to agent.
    #[error("Delegation error: {reason}")]
    DelegationError {
        /// What went wrong.
        reason: String,
    },

    // Transcript Errors
    /// Failed to read or parse a transcript file.
    #[error("Failed to read transcript at {path}: {reason}")]
    TranscriptReadFailed {
        /// Path to the transcript file.
        path: PathBuf,
        /// What went wrong.
        reason: String,
    },

    /// I/O or parse failure while reading agent storage during discovery.
    #[error("Failed to read discovery source at {path}: {reason}")]
    DiscoveryReadFailed {
        /// Path to the discovery source file.
        path: PathBuf,
        /// What went wrong.
        reason: String,
    },

    /// Failed to encode or decode a `SessionEnvelope`.
    #[error("Session envelope codec error: {reason}")]
    EnvelopeCodecError {
        /// What went wrong during encode/decode.
        reason: String,
    },

    // Generic Wrappers
    /// An IO error occurred.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// A JSON serialization/deserialization error occurred.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// A postcard serialization/deserialization error occurred.
    #[error("Postcard error: {0}")]
    Postcard(#[from] postcard::Error),

    /// An error from the underlying `atomic-repository` crate.
    #[error("Repository error: {0}")]
    Repository(String),

    /// An error from the underlying `atomic-identity` crate.
    #[error("Identity error: {0}")]
    Identity(String),

    /// Catch-all for unexpected internal errors.
    #[error("Internal error: {0}")]
    Internal(String),
}

impl AgentError {
    /// Returns `true` if this error is recoverable and the operation can be retried.
    pub fn is_recoverable(&self) -> bool {
        matches!(
            self,
            AgentError::WatchmanNotRunning
                | AgentError::WatchmanConnectionFailed { .. }
                | AgentError::WatchmanQueryFailed { .. }
                | AgentError::EmptyTurn { .. }
                | AgentError::TurnAlreadyActive { .. }
        )
    }

    /// Returns `true` if this error indicates Watchman is unavailable
    /// and the fallback watcher should be used.
    pub fn is_watchman_unavailable(&self) -> bool {
        matches!(
            self,
            AgentError::WatchmanNotRunning | AgentError::WatchmanConnectionFailed { .. }
        )
    }

    /// Returns `true` if this error is a session state machine violation
    /// that might indicate a crashed or interrupted agent.
    pub fn is_state_violation(&self) -> bool {
        matches!(
            self,
            AgentError::TurnNotActive { .. }
                | AgentError::TurnAlreadyActive { .. }
                | AgentError::SessionEnded { .. }
        )
    }

    /// Returns a user-facing suggestion for how to fix the error, if applicable.
    pub fn suggestion(&self) -> Option<&'static str> {
        match self {
            AgentError::WatchmanNotRunning => {
                Some("Install Watchman or the fallback file watcher will be used automatically.")
            }
            AgentError::AlreadyInstalled { .. } => {
                Some("Use `atomic agent enable --force` to reinstall hooks.")
            }
            AgentError::NotARepository { .. } => {
                Some("Run `atomic init` to initialize a repository first.")
            }
            AgentError::SessionIdInvalid { .. } => {
                Some("Session IDs must be alphanumeric with hyphens and underscores only.")
            }
            AgentError::TurnAlreadyActive { .. } => Some(
                "This may indicate the previous turn was interrupted. Continuing with recovery.",
            ),
            AgentError::AgentNotFound { .. } => {
                Some("Use `atomic agent enable --help` to see available agents.")
            }
            AgentError::AdapterNotFound { .. } => {
                Some("Run `atomic agent discover --list` to see available discovery adapters.")
            }
            _ => None,
        }
    }

    /// Returns a suggested exit code for CLI usage.
    pub fn exit_code(&self) -> i32 {
        match self {
            // User errors
            AgentError::NotARepository { .. } => 1,
            AgentError::AgentNotFound { .. } => 1,
            AgentError::SessionIdInvalid { .. } => 1,

            // Operational errors (retryable)
            AgentError::WatchmanNotRunning => 0, // Not fatal — fallback used
            AgentError::EmptyTurn { .. } => 0,   // Not fatal — no-op turn

            // Infrastructure errors
            AgentError::WatchmanConnectionFailed { .. } => 2,
            AgentError::Io(_) => 2,

            // Data errors
            AgentError::HookParseFailed { .. } => 3,
            AgentError::SessionLoadFailed { .. } => 3,
            AgentError::EnvelopeCodecError { .. } => 3,

            // Recording errors
            AgentError::RecordFailed { .. } => 4,
            AgentError::ViewError { .. } => 4,

            // Everything else
            _ => 1,
        }
    }
}

// Conversions from external error types

impl From<walkdir::Error> for AgentError {
    fn from(err: walkdir::Error) -> Self {
        AgentError::Io(std::io::Error::other(err.to_string()))
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display_watchman_not_running() {
        let err = AgentError::WatchmanNotRunning;
        let msg = err.to_string();
        assert!(msg.contains("Watchman is not running"));
    }

    #[test]
    fn test_error_display_session_not_found() {
        let err = AgentError::SessionNotFound {
            session_id: "2026-01-15-abc123".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("2026-01-15-abc123"));
        assert!(msg.contains("not found"));
    }

    #[test]
    fn test_error_display_hook_parse_failed() {
        let err = AgentError::HookParseFailed {
            agent: "claude-code".to_string(),
            hook_type: "stop".to_string(),
            reason: "missing session_id field".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("claude-code"));
        assert!(msg.contains("stop"));
        assert!(msg.contains("missing session_id"));
    }

    #[test]
    fn test_error_display_record_failed() {
        let err = AgentError::RecordFailed {
            session_id: "sess-1".to_string(),
            turn_number: 3,
            reason: "no tracked files".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("turn 3"));
        assert!(msg.contains("sess-1"));
        assert!(msg.contains("no tracked files"));
    }

    #[test]
    fn test_error_display_config_error() {
        let err = AgentError::ConfigError {
            operation: "write".to_string(),
            path: PathBuf::from(".claude/settings.json"),
            reason: "permission denied".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("write"));
        assert!(msg.contains(".claude/settings.json"));
        assert!(msg.contains("permission denied"));
    }

    #[test]
    fn test_error_display_agent_not_found() {
        let err = AgentError::AgentNotFound {
            name: "cursor".to_string(),
            available: "claude-code, gemini-cli, codex, opencode".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("cursor"));
        assert!(msg.contains("claude-code"));
    }

    #[test]
    fn test_error_display_turn_not_active() {
        let err = AgentError::TurnNotActive {
            session_id: "sess-1".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("No active turn"));
        assert!(msg.contains("sess-1"));
    }

    #[test]
    fn test_error_display_session_conflict() {
        let err = AgentError::SessionConflict {
            existing: "sess-1".to_string(),
            new_session: "sess-2".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("sess-1"));
        assert!(msg.contains("Concurrent session conflict"));
    }

    #[test]
    fn test_error_display_envelope_codec() {
        let err = AgentError::EnvelopeCodecError {
            reason: "unknown schema version 99".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("schema version 99"));
    }

    // Classification tests

    #[test]
    fn test_is_recoverable() {
        assert!(AgentError::WatchmanNotRunning.is_recoverable());
        assert!(AgentError::WatchmanConnectionFailed {
            reason: "timeout".to_string()
        }
        .is_recoverable());
        assert!(AgentError::WatchmanQueryFailed {
            reason: "stale".to_string()
        }
        .is_recoverable());
        assert!(AgentError::EmptyTurn {
            session_id: "s".to_string(),
            turn_number: 1,
        }
        .is_recoverable());
        assert!(AgentError::TurnAlreadyActive {
            session_id: "s".to_string(),
        }
        .is_recoverable());

        // Not recoverable
        assert!(!AgentError::SessionNotFound {
            session_id: "s".to_string()
        }
        .is_recoverable());
        assert!(!AgentError::RecordFailed {
            session_id: "s".to_string(),
            turn_number: 1,
            reason: "fail".to_string(),
        }
        .is_recoverable());
    }

    #[test]
    fn test_is_watchman_unavailable() {
        assert!(AgentError::WatchmanNotRunning.is_watchman_unavailable());
        assert!(AgentError::WatchmanConnectionFailed {
            reason: "refused".to_string()
        }
        .is_watchman_unavailable());

        // Query failures are NOT "unavailable" — Watchman is running but query failed
        assert!(!AgentError::WatchmanQueryFailed {
            reason: "bad expr".to_string()
        }
        .is_watchman_unavailable());
    }

    #[test]
    fn test_is_state_violation() {
        assert!(AgentError::TurnNotActive {
            session_id: "s".to_string()
        }
        .is_state_violation());
        assert!(AgentError::TurnAlreadyActive {
            session_id: "s".to_string()
        }
        .is_state_violation());
        assert!(AgentError::SessionEnded {
            session_id: "s".to_string(),
            operation: "record".to_string(),
        }
        .is_state_violation());

        assert!(!AgentError::WatchmanNotRunning.is_state_violation());
    }

    // Suggestion tests

    #[test]
    fn test_suggestion_watchman() {
        assert!(AgentError::WatchmanNotRunning.suggestion().is_some());
    }

    #[test]
    fn test_suggestion_already_installed() {
        let err = AgentError::AlreadyInstalled {
            agent: "claude-code".to_string(),
        };
        let suggestion = err.suggestion().unwrap();
        assert!(suggestion.contains("--force"));
    }

    #[test]
    fn test_suggestion_not_a_repository() {
        let err = AgentError::NotARepository {
            path: PathBuf::from("/tmp"),
        };
        let suggestion = err.suggestion().unwrap();
        assert!(suggestion.contains("atomic init"));
    }

    #[test]
    fn test_suggestion_none_for_generic_errors() {
        let err = AgentError::Internal("something broke".to_string());
        assert!(err.suggestion().is_none());
    }

    // Exit code tests

    #[test]
    fn test_exit_code_user_errors() {
        assert_eq!(
            AgentError::NotARepository {
                path: PathBuf::from("/tmp")
            }
            .exit_code(),
            1
        );
        assert_eq!(
            AgentError::AgentNotFound {
                name: "x".to_string(),
                available: "y".to_string()
            }
            .exit_code(),
            1
        );
    }

    #[test]
    fn test_exit_code_non_fatal() {
        assert_eq!(AgentError::WatchmanNotRunning.exit_code(), 0);
        assert_eq!(
            AgentError::EmptyTurn {
                session_id: "s".to_string(),
                turn_number: 1,
            }
            .exit_code(),
            0
        );
    }

    #[test]
    fn test_exit_code_recording_errors() {
        assert_eq!(
            AgentError::RecordFailed {
                session_id: "s".to_string(),
                turn_number: 1,
                reason: "x".to_string(),
            }
            .exit_code(),
            4
        );
        assert_eq!(
            AgentError::ViewError {
                operation: "create".to_string(),
                view_name: "s".to_string(),
                reason: "x".to_string(),
            }
            .exit_code(),
            4
        );
    }

    // Conversion tests

    #[test]
    fn test_from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let agent_err: AgentError = io_err.into();
        assert!(matches!(agent_err, AgentError::Io(_)));
        assert!(agent_err.to_string().contains("file not found"));
    }

    #[test]
    fn test_from_json_error() {
        let json_err = serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
        let agent_err: AgentError = json_err.into();
        assert!(matches!(agent_err, AgentError::Json(_)));
    }

    #[test]
    fn test_error_is_send_sync() {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}
        assert_send::<AgentError>();
        assert_sync::<AgentError>();
    }
}
