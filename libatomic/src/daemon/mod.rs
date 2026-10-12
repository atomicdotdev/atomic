//! The service-layer handlers: the domain half of the daemon.
//!
//! ONE implementation, two callers: the reactor's transport shell
//! (`atomicd`) serves these handlers over a private Unix-domain socket,
//! and the Atomic CLI's local service mode calls the very same handlers
//! in-process (`ATOMIC_SERVICE=local`) — there is no second copy of
//! service-layer logic anywhere.
//!
//! One state ([`state::DaemonState`]) multiplexes many repositories
//! (RFC D3): each resolved root is registered under its persistent ID
//! and opened lazily, exactly once per request. redb permits one
//! writable process per database file; each RPC acquires the
//! repository's serialization gate, opens the database, runs, and drops
//! the handle (the in-process local mode relies on the same
//! single-writer invariant: no concurrent daemon on the same machine).
//!
//! Module layout:
//! - [`state`] — the repository registry, per-repo gates, error mapping.
//! - [`services`] — DaemonService + RepositoryQuery/Mutation handlers.
//! - [`services_agent`] — Vault / Attestation / Knowledge / Provenance service impls.
//! - [`services_provenance`] — the ProvenanceService journal-port wire handlers.
//! - [`services_query`] — the heavy query handlers + ViewService.
//! - [`services_maintenance`] — MaintenanceService (doctor).
//! - [`services_sandbox`] — SandboxService: local sandbox trees, grant
//!   admin, and the remote-sandbox data ops.
//! - [`sandbox_grants`] — sandbox grants and the host's grant-store seam.
//! - [`sandbox_wire`] — kernel ↔ protobuf for the sandbox data ops (both
//!   sides: the handlers and a host's cache-side link).
//! - [`services_tag`] — TagService (tag create/delete/list/show).
//! - [`services_sync`] — SyncService (remote registry, push/pull).
//! - [`provenance_core`] — the shared provenance journal core.
//! - [`journal_sink`] — the in-process provenance sink (DirectJournalSink).
//! - [`convert`] — domain ↔ protobuf converters.

// The handlers use tonic's `Status` as their error type — the tonic
// server-trait convention. `Status` sits above clippy's default
// large-Err budget; boxing it through every call site would churn the
// handlers for no gain (per-request handling makes the size a non-issue).
#![allow(clippy::result_large_err)]

pub mod convert;
pub mod journal_sink;
pub mod provenance_core;
pub mod sandbox_grants;
pub mod sandbox_wire;
pub mod services;
pub mod services_agent;
pub mod services_maintenance;
pub mod services_provenance;
pub mod services_query;
pub mod services_sandbox;
pub mod services_sync;
pub mod services_sync_remote;
pub mod services_tag;
pub mod services_triage;
pub mod state;
