//! # atomic-facade
//!
//! The native API layer over Atomic's command-level read operations.
//!
//! Everything an integration needs is already on the laptop, inside the
//! repository. This crate lets **local embedders** — UIs, tools, agent
//! harnesses — read that data in-process as typed values: every function
//! is what a CLI command does *minus* the terminal. It takes an open
//! [`atomic_repository::Repository`], returns serde-serializable DTOs,
//! and never prints, prompts, or exits — no shelling out to the `atomic`
//! binary, no parsing its output.
//!
//! This is deliberately **not** a server contract. Atomic is local-first;
//! the storage server is primarily transport, and any server-side read
//! surface is a separate, intentional decision — not something this crate
//! implies or requires.
//!
//! For changes and log, JSON field names and skip rules deliberately mirror
//! the CLI's `-f json` output, so serialized responses and CLI output stay
//! interchangeable. The view/intent/memory DTOs are richer shapes than the
//! CLI's terse `--json` rows — new surface, not a CLI contract.
//!
//! All reads are synchronous (redb + filesystem). When embedding in an
//! async runtime, wrap calls in `spawn_blocking`, open repositories with
//! `Repository::open_readonly`, and share one `Pristine` per repository
//! via `Repository::open_with_pristine` — redb allows only one open
//! `Database` per file.
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

pub use attestations::{AttestationStatus, Attested, LiftInputs};
pub use changes::{change_detail, list_log, ChangeDetail, LogEntry, LogQuery};
pub use error::{FacadeError, FacadeResult};
pub use identifier::{resolve_change, ChangeIdentifier};
pub use intents::{intent_detail, list_intents, IntentDetail, IntentSummary};
pub use memories::{list_memories, memory_detail, MemoryDetail, MemorySummary};
pub use provenance::{CostDto, ProvenanceDto, TokenUsageDto};
pub use views::{list_views, view_summary, ViewSummary};
