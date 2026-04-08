//! Streaming push/pull protocol for V3 change files.
//!
//! This module defines the protocol types and helpers for streaming V3 change
//! files over HTTP without full-body buffering. The V3 section-based format
//! is inherently streaming — each section is independently compressed and
//! can be processed as it arrives.
//!
//! # Protocol Types
//!
//! | Type | Purpose |
//! |------|---------|
//! | [`LayerSelection`] | Which layers to include in a download |
//! | [`ChunkManifest`] | Ordered list of content chunks in a change |
//! | [`ChunkManifestEntry`] | One entry: (index, hash, compressed_size) |
//! | [`ChunkNegotiation`] | Client's "have" list → Server's "need" response |
//! | [`StreamingPushOptions`] | Configuration for streaming uploads |
//! | [`StreamingPullOptions`] | Configuration for streaming downloads |
//! | [`TransferProgress`] | Per-section progress reporting |
//! | [`TransferStats`] | Summary statistics for a completed transfer |

mod layers;
mod negotiation;
mod progress;
mod types;

#[cfg(test)]
mod tests;

// Re-export all public types at the `streaming` level so that existing
// import paths like `atomic_remote::streaming::LayerSelection` keep working.

pub use layers::{ChunkManifest, LayerSelection};
pub use negotiation::{ChunkNegotiation, StreamingPullOptions, StreamingPushOptions};
pub use progress::{TransferProgress, TransferStats};
pub use types::{ChunkManifestEntry, Layer};
