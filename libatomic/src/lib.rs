//! libatomic — the canonical Atomic contract.
//!
//! This crate owns the one Atomic protobuf contract: the canonical
//! sources live under `proto/` (package `atomic`), `build.rs` compiles
//! them with protox (a pure-Rust protoc replacement — no system protoc)
//! and tonic-build, and `proto/check_contract.py` gates the declared
//! descriptor invariants (add-only; scopes, callers, effects and
//! capabilities on every method; read/write discipline; retired fields;
//! fencing).
//!
//! The generated module is [`atomic`] — every file in the contract
//! declares `package atomic`, so all messages and the generated service
//! clients/servers land in that single module — with a transport-neutral
//! [`proto`] alias for callers that prefer it.
//!
//! The contract is transport-neutral by design: any process or tooling
//! that speaks Atomic imports this crate for the types. This crate ships
//! the contract and the codegen only — implementations live elsewhere.

pub mod atomic {
    // The generated module is machine-written; lints do not apply to it.
    #![allow(clippy::all)]
    tonic::include_proto!("atomic");
}

/// The generated contract module under its transport-neutral name.
pub use crate::atomic as proto;
