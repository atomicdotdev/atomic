//! Discovery adapters for reading agent storage on the provenance import path.
//!
//! This module defines the [`TraceDiscovery`] trait that each discovery adapter
//! implements, and the [`DiscoveryRegistry`] that manages available adapters.
//! Where [`crate::hooks`] is the **write path** — normalizing live hook callbacks
//! into [`crate::event::TurnEvent`] values as an agent runs — the discovery
//! module is the **read path**: adapters scan agent storage already on disk
//! (JSONL transcripts, JSON session files, SQLite databases) and produce
//! normalized [`DiscoveredTrace`] and [`DiscoveredEvent`] values that feed the
//! provenance import pipeline.
//!
//! # Architecture
//!
//! ```text
//! Agent storage on disk
//!   (JSONL / JSON / SQLite)
//!         │
//!         ▼
//!  TraceDiscovery adapter
//!  (list_traces / read_events)
//!         │
//!         ▼
//!  DiscoveredTrace / DiscoveredEvent
//!         │
//!         ▼
//!  Provenance import pipeline
//! ```
//!
//! # Adding a New Adapter
//!
//! 1. Create a new file `discovery/<agent_name>.rs`
//! 2. Implement [`TraceDiscovery`] for your adapter struct
//! 3. Register it in the default registry via [`DiscoveryRegistry::with_defaults`]
//!    (see the comment there for the relevant issue numbers)
//! 4. Add a `pub mod <agent_name>;` declaration to this file
//!
//! # Example
//!
//! ```rust
//! use atomic_agent::discovery::DiscoveryRegistry;
//!
//! let registry = DiscoveryRegistry::with_defaults();
//! let _ = registry;
//! ```

pub mod hermes;
pub mod reader;
pub mod types;

pub use types::{DiscoveredEvent, DiscoveredEventType, DiscoveredTrace, StorageKind};

use std::fmt;

use crate::error::{AgentError, AgentResult};

// TraceDiscovery Trait

/// Read-path counterpart to [`crate::hooks::AgentHook`]. Each adapter knows how
/// to scan a specific agent's on-disk storage and produce normalized traces and
/// events for the provenance import pipeline.
pub trait TraceDiscovery: Send + Sync + fmt::Debug {
    /// Unique adapter id used in registry lookups (kebab-case, e.g., `"claude-code"`).
    fn agent_id(&self) -> &str;

    /// Human-readable display name (e.g., `"Claude Code"`).
    fn display_name(&self) -> &str;

    /// Whether this adapter's storage is currently present on the host.
    ///
    /// Cheap probe (existence check); should not open files or DBs.
    fn is_available(&self) -> bool;

    /// Enumerate all traces this adapter can see. Order is adapter-defined.
    fn list_traces(&self) -> AgentResult<Vec<DiscoveredTrace>>;

    /// Read all events for a single trace, in monotonic order.
    fn read_events(&self, trace_id: &str) -> AgentResult<Vec<DiscoveredEvent>>;

    /// Storage backend this adapter reads from.
    fn storage_kind(&self) -> StorageKind;
}

// DiscoveryRegistry

/// Registry of available discovery adapters.
///
/// The registry holds all known adapters and provides lookup by agent id,
/// listing, and filtering to adapters whose storage is currently present on
/// the host.
///
/// # Thread Safety
///
/// Adapters are stored as `Box<dyn TraceDiscovery>` which is `Send + Sync`.
/// The registry itself requires `&mut self` for `register()`; share across
/// threads via `Arc<Mutex<DiscoveryRegistry>>` or freeze before sharing.
///
/// # Example
///
/// ```rust
/// use atomic_agent::discovery::DiscoveryRegistry;
///
/// let registry = DiscoveryRegistry::with_defaults();
///
/// // List all registered adapter ids
/// for id in registry.list() {
///     println!("Registered adapter: {}", id);
/// }
/// ```
pub struct DiscoveryRegistry {
    adapters: Vec<Box<dyn TraceDiscovery>>,
}

impl DiscoveryRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            adapters: Vec::new(),
        }
    }

    /// Create a registry pre-populated with all built-in discovery adapters.
    ///
    /// Registers all built-in discovery adapters. Currently: Hermes (#27).
    /// Additional adapters land in follow-up issues (#18–#26, #28).
    pub fn with_defaults() -> Self {
        let mut registry = Self::new();
        registry.register(Box::new(crate::discovery::hermes::HermesDiscovery::new()));
        registry
    }

    /// Register a new discovery adapter.
    ///
    /// If an adapter with the same `agent_id()` is already registered, the new
    /// one replaces it.
    pub fn register(&mut self, adapter: Box<dyn TraceDiscovery>) {
        // Replace existing adapter with the same agent_id
        self.adapters.retain(|a| a.agent_id() != adapter.agent_id());
        self.adapters.push(adapter);
    }

    /// Look up a discovery adapter by agent id.
    ///
    /// Returns `None` if no adapter with the given id is registered.
    pub fn get(&self, agent_id: &str) -> Option<&dyn TraceDiscovery> {
        self.adapters
            .iter()
            .find(|a| a.agent_id() == agent_id)
            .map(|a| a.as_ref())
    }

    /// Look up a discovery adapter by agent id, returning an error if not found.
    ///
    /// Returns [`AgentError::AdapterNotFound`] with the sorted list of registered ids
    /// when the given id is not present.
    pub fn require(&self, agent_id: &str) -> AgentResult<&dyn TraceDiscovery> {
        self.get(agent_id).ok_or_else(|| {
            let mut names = self.list();
            names.sort_unstable();
            AgentError::AdapterNotFound {
                name: agent_id.to_string(),
                available: names.join(", "),
            }
        })
    }

    /// Returns the agent ids of all registered adapters, in registration order.
    pub fn list(&self) -> Vec<&str> {
        self.adapters.iter().map(|a| a.agent_id()).collect()
    }

    /// Returns the agent ids of adapters whose storage is currently present on
    /// the host (`is_available() == true`), in registration order.
    pub fn available(&self) -> Vec<&str> {
        self.adapters
            .iter()
            .filter(|a| a.is_available())
            .map(|a| a.agent_id())
            .collect()
    }

    /// Returns the number of registered adapters.
    pub fn count(&self) -> usize {
        self.adapters.len()
    }

    /// Returns `true` if no adapters are registered.
    pub fn is_empty(&self) -> bool {
        self.adapters.is_empty()
    }

    /// Returns an iterator over all registered adapters.
    pub fn iter(&self) -> impl Iterator<Item = &dyn TraceDiscovery> {
        self.adapters.iter().map(|a| a.as_ref())
    }
}

impl Default for DiscoveryRegistry {
    fn default() -> Self {
        Self::with_defaults()
    }
}

impl fmt::Debug for DiscoveryRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DiscoveryRegistry")
            .field("adapters", &self.list())
            .finish()
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::AgentError;

    #[derive(Debug)]
    struct MockDiscovery {
        agent_id: String,
        available: bool,
    }

    impl MockDiscovery {
        fn new(agent_id: &str) -> Self {
            Self {
                agent_id: agent_id.to_string(),
                available: true,
            }
        }

        fn with_available(agent_id: &str, available: bool) -> Self {
            Self {
                agent_id: agent_id.to_string(),
                available,
            }
        }
    }

    impl TraceDiscovery for MockDiscovery {
        fn agent_id(&self) -> &str {
            &self.agent_id
        }

        fn display_name(&self) -> &str {
            "Mock"
        }

        fn is_available(&self) -> bool {
            self.available
        }

        fn list_traces(&self) -> AgentResult<Vec<DiscoveredTrace>> {
            Ok(Vec::new())
        }

        fn read_events(&self, _trace_id: &str) -> AgentResult<Vec<DiscoveredEvent>> {
            Ok(Vec::new())
        }

        fn storage_kind(&self) -> StorageKind {
            StorageKind::Jsonl
        }
    }

    #[test]
    fn test_registry_new_is_empty() {
        let registry = DiscoveryRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.count(), 0);
        assert!(registry.list().is_empty());
    }

    #[test]
    fn test_registry_with_defaults_has_hermes() {
        let registry = DiscoveryRegistry::with_defaults();
        assert!(!registry.is_empty());
        assert!(registry.list().contains(&"hermes"));
    }

    #[test]
    fn test_registry_register_and_get() {
        let mut registry = DiscoveryRegistry::new();
        registry.register(Box::new(MockDiscovery::new("mock-a")));
        let adapter = registry.get("mock-a");
        assert!(adapter.is_some());
        assert_eq!(adapter.unwrap().agent_id(), "mock-a");
    }

    #[test]
    fn test_registry_get_nonexistent() {
        let registry = DiscoveryRegistry::new();
        assert!(registry.get("nope").is_none());
    }

    #[test]
    fn test_registry_require_success() {
        let mut registry = DiscoveryRegistry::new();
        registry.register(Box::new(MockDiscovery::new("mock-a")));
        assert!(registry.require("mock-a").is_ok());
    }

    #[test]
    fn test_registry_require_not_found() {
        let mut registry = DiscoveryRegistry::new();
        registry.register(Box::new(MockDiscovery::new("mock-a")));
        let err = registry.require("missing").unwrap_err();
        match err {
            AgentError::AdapterNotFound { name, available } => {
                assert_eq!(name, "missing");
                assert!(available.contains("mock-a"));
            }
            other => panic!("Expected AdapterNotFound, got: {:?}", other),
        }
    }

    #[test]
    fn test_registry_register_replaces_duplicate() {
        let mut registry = DiscoveryRegistry::new();
        registry.register(Box::new(MockDiscovery::new("mock-a")));
        registry.register(Box::new(MockDiscovery::new("mock-a")));
        assert_eq!(registry.count(), 1);
        assert_eq!(registry.get("mock-a").unwrap().agent_id(), "mock-a");
    }

    #[test]
    fn test_registry_list_preserves_order() {
        let mut registry = DiscoveryRegistry::new();
        registry.register(Box::new(MockDiscovery::new("a")));
        registry.register(Box::new(MockDiscovery::new("b")));
        registry.register(Box::new(MockDiscovery::new("c")));
        assert_eq!(registry.list(), vec!["a", "b", "c"]);
    }

    #[test]
    fn test_registry_available_filters_unavailable() {
        let mut registry = DiscoveryRegistry::new();
        registry.register(Box::new(MockDiscovery::new("present")));
        registry.register(Box::new(MockDiscovery::with_available("absent", false)));

        let available = registry.available();
        assert!(available.contains(&"present"));
        assert!(!available.contains(&"absent"));

        let all = registry.list();
        assert!(all.contains(&"present"));
        assert!(all.contains(&"absent"));
    }

    #[test]
    fn test_registry_iter() {
        let mut registry = DiscoveryRegistry::new();
        registry.register(Box::new(MockDiscovery::new("x")));
        registry.register(Box::new(MockDiscovery::new("y")));
        assert_eq!(registry.iter().count(), 2);
    }

    #[test]
    fn test_registry_debug_format() {
        let mut registry = DiscoveryRegistry::new();
        registry.register(Box::new(MockDiscovery::new("claude-code")));
        registry.register(Box::new(MockDiscovery::new("gemini-cli")));
        let debug = format!("{:?}", registry);
        assert!(debug.contains("DiscoveryRegistry"));
        assert!(debug.contains("claude-code"));
        assert!(debug.contains("gemini-cli"));
    }

    #[test]
    fn test_trace_discovery_is_object_safe() {
        let _: Box<dyn TraceDiscovery> = Box::new(MockDiscovery::new("x"));
    }

    #[test]
    fn test_trace_discovery_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Box<dyn TraceDiscovery>>();
    }
}
