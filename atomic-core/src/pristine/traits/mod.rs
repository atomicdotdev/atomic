//! Database trait abstractions for pristine storage
//!
//! This module defines the trait interfaces for interacting with the pristine
//! database. These traits provide a clean abstraction layer that separates
//! interface from implementation, enables testing with mock implementations,
//! and documents expected behavior.
//!
//! # Trait Hierarchy
//!
//! ```text
//!                     MutTxnT
//!         (Full read-write access)
//!                      │
//!          ┌───────────┼───────────┐
//!          ▼           ▼           ▼
//!      ViewTxnT    TreeTxnT   GraphTxnT
//!     (View ops)  (File ops)  (Graph queries)
//!          │           │           │
//!          └───────────┼───────────┘
//!                      ▼
//!                  GraphTxnT
//!                (Base trait)
//! ```

mod graph;
mod mutate;
mod tree;
mod vertex_ext;
mod view;

#[cfg(test)]
mod tests;

pub use graph::GraphTxnT;
pub use mutate::MutTxnT;
pub use tree::TreeTxnT;
pub use vertex_ext::VertexExt;
pub use view::{ViewScope, ViewState, ViewTxnT};
