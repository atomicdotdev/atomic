//! Provenance graph — causal decision DAG for agent sessions.
//!
//! This module captures the causal chain of decisions that an AI agent makes
//! during a coding session: what the human asked for (goals), what the agent
//! explored (explorations), what it changed (commitments), how it validated
//! its work (verifications), and where it was uncertain (human gates).
//!
//! The graph is built incrementally by the [`accumulator::ProvenanceAccumulator`]
//! as hook events arrive via the [`crate::turn::TurnOrchestrator`], and persisted
//! to `.atomic/sessions/{session_id}/graph.json` between process invocations.
//!
//! # Architecture
//!
//! ```text
//! TurnOrchestrator::dispatch(event)
//!     │
//!     ├── handle_turn_start  → accumulator.append_goal(prompt)
//!     ├── handle_tool_use    → accumulator.append_tool_call(tool, args, output)
//!     │                          │
//!     │                          ├── classify::classify_tool_call → NodeKind
//!     │                          ├── classify::summarize_tool_call → summary
//!     │                          └── accumulator.infer_edges → causal DAG
//!     │
//!     ├── handle_turn_end    → accumulator.append_patch_proposal(change_hash)
//!     └── handle_session_end → log stats, finalize graph
//! ```
//!
//! # Modules
//!
//! - [`types`] — Node, edge, and graph type definitions
//! - [`classify`] — Rule-based tool call classification
//! - [`accumulator`] — In-memory DAG builder with persistence
//!
//! # Example
//!
//! ```rust
//! use atomic_agent::provenance::accumulator::ProvenanceAccumulator;
//! use atomic_agent::provenance::types::{NodeKind, EdgeKind};
//!
//! let mut acc = ProvenanceAccumulator::new("session-123");
//!
//! let goal = acc.append_goal("Fix the auth bug", 1000);
//! let read = acc.append_tool_call("read", Some("c1"), None, None, None, None, 1001);
//! let edit = acc.append_tool_call("edit", Some("c2"), None, None, None, None, 1002);
//!
//! assert_eq!(acc.node_count(), 3);
//! assert_eq!(acc.stats().goal_count, 1);
//! assert_eq!(acc.stats().exploration_count, 1);
//! assert_eq!(acc.stats().commitment_count, 1);
//!
//! // Edges are inferred automatically:
//! // goal --led_to-→ read --explored_via-→ edit
//! assert!(acc.edges().iter().any(|e| e.kind == EdgeKind::LedTo));
//! assert!(acc.edges().iter().any(|e| e.kind == EdgeKind::ExploredVia));
//! ```

pub mod accumulator;
pub mod classify;
pub mod consolidate;
pub mod types;

// Re-export primary types for convenience.
pub use accumulator::ProvenanceAccumulator;
pub use classify::{classify_tool_call, summarize_tool_call};
pub use consolidate::consolidate;
pub use types::{EdgeKind, GraphEdge, GraphNode, GraphStats, NodeKind, SerializedGraph};
