//! Atomic Remote - HTTP client library for Atomic VCS remote operations.
//!
//! This crate provides the client-side implementation of the Atomic remote protocol,
//! enabling communication with `atomic-api` servers for push, pull, and clone operations.
//!
//! # Overview
//!
//! The `atomic-remote` crate is designed as a clean-room implementation of the Atomic
//! remote protocol. It provides:
//!
//! - **HTTP client** (`HttpRemote`) for communicating with `atomic-api` servers
//! - **Protocol types** for parsing and generating protocol messages
//! - **Error handling** with informative, actionable error messages
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────┐
//! │                    Your Application                      │
//! │                  (atomic-cli, etc.)                      │
//! └────────────────────────┬────────────────────────────────┘
//!                          │
//!                          ▼
//! ┌─────────────────────────────────────────────────────────┐
//! │                    atomic-remote                         │
//! │  ┌─────────────┐  ┌─────────────┐  ┌─────────────────┐ │
//! │  │ HttpRemote  │  │   types     │  │     error       │ │
//! │  │  (client)   │  │ (protocol)  │  │   (errors)      │ │
//! │  └──────┬──────┘  └─────────────┘  └─────────────────┘ │
//! └─────────┼───────────────────────────────────────────────┘
//!           │ HTTP
//!           ▼
//! ┌─────────────────────────────────────────────────────────┐
//! │                     atomic-api                           │
//! │              (Remote Repository Server)                  │
//! └─────────────────────────────────────────────────────────┘
//! ```
//!
//! # Quick Start
//!
//! ```ignore
//! use atomic_remote::http::HttpRemote;
//!
//! async fn example() -> Result<(), Box<dyn std::error::Error>> {
//!     // Connect to a remote repository
//!     let remote = HttpRemote::new(
//!         "https://api.example.com/tenant/t/portfolio/p/project/myrepo/code"
//!     )?;
//!
// Query the current state of the "main" view
//!     let state = remote.get_state("main").await?;
//!     println!("Remote state: {:?}", state);
//!
//!     // Get the changelist (history of changes)
//!     let entries = remote.get_changelist("main", 0).await?;
//!     for entry in entries {
//!         println!("#{}: {} {}", entry.sequence, entry.hash,
//!             if entry.tagged { "[tagged]" } else { "" });
//!     }
//!     Ok(())
//! }
//! ```
//!
//! # Protocol
//!
//! This crate implements the Atomic HTTP protocol as served by `atomic-api`:
//!
//! | Operation | Endpoint | Description |
//! |-----------|----------|-------------|
//! | Get State | `GET ?view={s}&state=` | Query view state |
//! | Get Changelist | `GET ?view={s}&changelist={from}` | Get changes from position |
//! | Download Change | `GET ?change={hash}` | Download change file |
//! | Download Tag | `GET ?tag={state}` | Download short tag data |
//! | Upload Change | `POST ?insert={hash}&view={s}` | Upload and insert change to view |
//! | Upload Tag | `POST ?tagup={state}&view={s}` | Upload tag |
//!
//! ## Changelist Format
//!
//! The changelist is returned as lines in the format:
//! - Regular change: `{sequence}.{hash}.{merkle}`
//! - Tagged change: `{sequence}.{hash}.{merkle}.` (trailing dot)
//!
//! ## State Format
//!
//! State responses are in the format:
//! - With state: `{position} {merkle} {tag_merkle}`
//! - Empty view: `-`
//!
//! # User-Agent
//!
//! All requests include a `User-Agent: atomic-{version}` header. This is critical
//! for `atomic-api` servers to distinguish CLI requests from web browser requests,
//! enabling proper binary data handling in proxy chains.
//!
//! # Error Handling
//!
//! Errors are represented by the [`RemoteError`] enum, which provides:
//! - Informative error messages
//! - Classification methods (`is_retryable()`, `is_auth_error()`, `is_not_found()`)
//! - User-friendly suggestions via `suggestion()`
//!
//! ```
//! use atomic_remote::error::RemoteError;
//!
//! fn handle_error(err: RemoteError) {
//!     eprintln!("Error: {}", err);
//!
//!     if let Some(suggestion) = err.suggestion() {
//!         eprintln!("Hint: {}", suggestion);
//!     }
//!
//!     if err.is_retryable() {
//!         eprintln!("This error may succeed if retried.");
//!     }
//! }
//! ```
//!
//! # Feature Flags
//!
//! - `integration-tests` - Enable integration tests that require a running `atomic-api` server

// Modules

pub mod error;
pub mod http;
pub mod streaming;
pub mod sync;
pub mod types;

// Re-exports

// Error types
pub use error::{RemoteError, RemoteResult};

// HTTP client
pub use http::{HttpRemote, HttpRemoteConfig};

// Protocol types
pub use types::{
    ChangelistEntry, Node, NodeType, ParseChangelistError, ParseNodeTypeError, ParseStateError,
    PullDelta, PushDelta, StateResponse,
};

// Sync engine
pub use sync::{RemoteDelta, RemoteState, SyncEngine, SyncStats};

// Streaming protocol
pub use streaming::{
    ChunkManifest, ChunkManifestEntry, ChunkNegotiation, Layer, LayerSelection,
    StreamingPullOptions, StreamingPushOptions, TransferProgress, TransferStats,
};

// Constants

/// The current protocol version.
///
/// This is used for compatibility checking with remote servers.
pub const PROTOCOL_VERSION: u32 = 1;

/// The crate version, used in User-Agent headers.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_protocol_version() {
        assert_eq!(PROTOCOL_VERSION, 1);
    }

    #[test]
    fn test_version_string() {
        // Version should be a valid semver
        assert!(!VERSION.is_empty());
        let parts: Vec<&str> = VERSION.split('.').collect();
        assert!(parts.len() >= 2, "Version should have at least major.minor");
    }

    #[test]
    fn test_reexports_available() {
        // Verify that all re-exports are accessible
        let _ = RemoteError::other("test");
        let _ = StateResponse::empty();
        let _ = ChangelistEntry::new(0, "hash", "merkle", false);
        let _ = Node::change("hash", "state");
        let _ = NodeType::Change;
        let _ = PushDelta::new();
        let _ = PullDelta::new();
        let _ = RemoteState::empty();
        let _ = RemoteDelta::in_sync();
        let _ = SyncStats::default();

        // Streaming protocol types
        let _ = LayerSelection::all();
        let _ = LayerSelection::thin_pull();
        let _ = ChunkManifest::empty();
        let _ = ChunkManifestEntry::new(0, [0u8; 32], 0, 0);
        let _ = StreamingPushOptions::default();
        let _ = StreamingPullOptions::default();
        let _ = TransferStats::default();
    }
}
