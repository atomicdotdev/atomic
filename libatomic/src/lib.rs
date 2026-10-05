//! libatomic — the canonical Atomic contract and its reference
//! implementation.
//!
//! This crate owns both halves of the Atomic service layer:
//!
//!  * the **contract** — the canonical protobuf sources under `proto/`
//!    (package `atomic`), compiled by `build.rs` with protox (a pure-Rust
//!    protoc replacement — no system protoc) and tonic-build into the
//!    [`atomic`] module (aliased as [`proto`]), plus the descriptor
//!    contract gate (`proto/check_contract.py` — add-only; scope, caller,
//!    effect and capability declarations on every method; read/write
//!    discipline; retired fields; generation/snapshot fencing).
//!  * the **handlers** — the reference implementation of that contract
//!    under [`daemon`]: the repository registry and per-repository
//!    serialization gates, the tonic service impls and their plain inner
//!    functions, the provenance journal core and sink, and the
//!    domain/protobuf converters.
//!
//! The handlers are transport-neutral: a server process wires them onto
//! any transport (the generated servers exist for exactly that), and any
//! tool that speaks Atomic can call the same handlers in-process instead
//! of over a wire — one implementation everywhere.

pub mod atomic {
    // The generated module is machine-written; lints do not apply to it.
    #![allow(clippy::all)]
    tonic::include_proto!("atomic");
}

/// The generated contract module under its transport-neutral name.
pub use crate::atomic as proto;

pub mod daemon;
