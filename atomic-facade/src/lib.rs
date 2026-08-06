//! # atomic-facade
//!
//! The native API layer over Atomic's command-level read operations.
//!
//! Every function here is what a CLI command does *minus* the terminal:
//! it takes an open [`atomic_repository::Repository`], returns
//! serde-serializable DTOs, and never prints, prompts, or exits. A server
//! (e.g. `atomic-enterprise/atomic-api`) links this crate and calls the
//! operations in-process instead of shelling out to the `atomic` binary.
//!
//! JSON field names and skip rules deliberately mirror the CLI's `-f json`
//! output, so responses served over HTTP and output printed by the CLI stay
//! interchangeable.
//!
//! All reads are synchronous (redb + filesystem). In an async server, wrap
//! calls in `spawn_blocking`, open repositories with
//! `Repository::open_readonly`, and share one `Pristine` per project via
//! `Repository::open_with_pristine`.
//!
//! | Module | CLI equivalent |
//! |---|---|
//! | [`changes`] | `atomic change -f json`, `atomic log -f json` |
//! | [`views`] | `atomic view list` |
//! | [`intents`] | `atomic intent list` / `show` |
//! | [`memories`] | `atomic memory list` / `show` |
//! | [`attestations`] | the intent/memory attestation dual-read |
//! | [`identifier`] | hash / prefix / `#seq` / `@` resolution |

pub mod attestations;
pub mod changes;
pub mod error;
pub mod identifier;
pub mod intents;
pub mod memories;
pub mod provenance;
pub mod views;

pub use attestations::{Attested, AttestationStatus, LiftInputs};
pub use changes::{change_detail, list_log, ChangeDetail, LogEntry, LogQuery};
pub use error::{FacadeError, FacadeResult};
pub use identifier::{resolve_change, ChangeIdentifier};
pub use intents::{intent_detail, list_intents, IntentDetail, IntentSummary};
pub use memories::{list_memories, memory_detail, MemoryDetail, MemorySummary};
pub use provenance::{CostDto, ProvenanceDto, TokenUsageDto};
pub use views::{list_views, view_summary, ViewSummary};
