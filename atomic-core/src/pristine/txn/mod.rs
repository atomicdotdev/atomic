//! Transaction implementations for pristine storage
//!
//! This module provides the concrete redb-based implementation of the pristine
//! storage layer. It is split into submodules for maintainability:
//!
//! - `pristine` - The main database handle
//! - `read` - Read-only transaction implementation
//! - `write` - Read-write transaction implementation
//! - `helpers` - Serialization and utility functions

mod helpers;
mod pristine;
mod read;
mod write;

pub use helpers::AdjIterator;
pub use pristine::Pristine;
pub use read::ReadTxn;
pub use write::WriteTxn;
