//! libatomic — the one Atomic service-layer library.
//!
//! This crate owns BOTH halves of the service layer:
//!
//!  * the **contract** — the canonical protobuf sources under `proto/`,
//!    the tonic/prost codegen (`build.rs`, protox — no system protoc),
//!    and the descriptor contract gate (`proto/check_contract.py`,
//!    add-only at 97 RPCs). The atomic repo is the contract's single
//!    source of truth; there is no vendored copy synced anywhere.
//!    The generated module is [`atomic`] (also re-exported as [`proto`]),
//!    and `atomic-client` re-exports it so `atomic_client::proto` keeps
//!    its import path.
//!  * the **handlers** — the entire service-layer implementation under
//!    [`daemon`]: the repository registry and per-repository gates
//!    ([`daemon::state`]), the tonic service impls and their plain inner
//!    functions (`daemon::services*`), the provenance journal core and
//!    sink, and the proto/domain converters. ONE implementation: the
//!    `atomicd` transport (the reactor) serves these handlers over the
//!    socket, and the CLI's local service mode calls the very same
//!    handlers in-process — never a second copy.
//!
//! The transport (socket bind, single-instance lock, tonic `Server`
//! wiring, the `atomicd` binary) deliberately does NOT live here; see
//! the reactor crate for the shell that consumes this library.

pub mod atomic {
    // The generated module is machine-written; lints do not apply to it.
    #![allow(clippy::all)]
    tonic::include_proto!("atomic");
}

/// The generated contract module under its transport-neutral name
/// (`atomic_client::proto` re-exports this path).
pub use crate::atomic as proto;

pub mod daemon;
