//! Provenance graph — causal decision DAG for agent sessions.
//!
//! This module captures the causal chain of decisions that an AI agent makes
//! during a coding session: what the human asked for (goals), what the agent
//! explored (explorations), what it changed (commitments), how it validated
//! its work (verifications), and where it was uncertain (human gates).
//!
//! Hook events are committed to the repository-owner redb journal. The
//! [`accumulator::ProvenanceAccumulator`] deterministically replays a frozen
//! turn frontier when producing an immutable content-addressed graph.
//!
//! # Architecture
//!
//! ```text
//! TurnOrchestrator::dispatch(event)
//!     │
//!     ├── turn/tool hooks → owner journal envelopes
//!     ├── turn end        → freeze committed event frontier
//!     ├── replay          → accumulator + causal DAG
//!     └── checkpoint      → immutable graph + SESSION_TURNS/head
//! ```
//!
//! # Modules
//!
//! - [`types`] — Node, edge, and graph type definitions
//! - [`classify`] — Rule-based tool call classification
//! - [`accumulator`] — Deterministic journal replay and DAG builder
//! - [`detail`] — Typed JSON payloads for Sherpa provenance nodes
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
pub mod detail;
pub mod types;

// Re-export primary types for convenience.
pub use accumulator::{PreparedProvenanceGraph, ProvenanceAccumulator};
pub use classify::{classify_tool_call, summarize_tool_call};
pub use consolidate::consolidate;
pub use detail::{
    CodeFinding, CommitmentDetail, Contributor, ExecutionDetail, GoalDetail, Learnings,
    PhaseTokens, TurnOutcome, TurnTotals, VerificationDetail, COST_PER_M_INPUT, COST_PER_M_OUTPUT,
};
pub use types::{EdgeKind, GraphEdge, GraphNode, GraphStats, NodeKind, SerializedGraph};
