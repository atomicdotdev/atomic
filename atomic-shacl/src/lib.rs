//! Recording the Why — a real SHACL engine over Turtle shapes, run in **shadow**.
//!
//! This crate wraps the pre-1.0 `oxirs-shacl` engine and validates canonical
//! Intent/Memory nodes against real Turtle shapes. During the spike it is
//! **non-authoritative**: nothing here gates a signature or a persist. Its job
//! is to answer, measurably, "can oxirs reproduce the hand-coded gate?" via a
//! differential corpus (see `tests/`).
//!
//! ## Isolation charter
//! The oxirs stack (a large, pre-1.0 transitive tree) lives behind the
//! off-by-default `oxirs-engine` feature and is named ONLY in this crate.
//! `atomic-canonical` stays pure and sync and never depends on this crate.
//! Enforced by a CI purity check.

#![forbid(unsafe_code)]

#[cfg(feature = "oxirs-engine")]
pub mod engine;
#[cfg(feature = "oxirs-engine")]
pub mod shapes;
#[cfg(feature = "oxirs-engine")]
pub mod shim;
