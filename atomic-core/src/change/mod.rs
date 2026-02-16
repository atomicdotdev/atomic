//! Change representation for Atomic VCS
//!
//! A **Change** (also called a "patch") is the fundamental unit of modification
//! in Atomic. Changes are:
//!
//! - **Content-addressed**: Identified by a Blake3 hash of their content
//! - **Self-describing**: Contain all metadata needed for application
//! - **Composable**: Can be applied in different orders (when independent)
//! - **Invertible**: Can be reversed to undo their effects
//!
//! # Change Structure
//!
//! A change file has four sections:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                     Offsets (48 bytes)                      │
//! │  version, hashed_len, unhashed_off/len, contents_off/len    │
//! ├─────────────────────────────────────────────────────────────┤
//! │                  Hashed Section (zstd compressed)           │
//! │  header, dependencies, extra_known, metadata, file_ops,     │
//! │  contents_hash                                              │
//! ├─────────────────────────────────────────────────────────────┤
//! │                  Unhashed Section (optional JSON)           │
//! │  Extra metadata that doesn't affect the change hash         │
//! ├─────────────────────────────────────────────────────────────┤
//! │                  Contents (raw bytes)                       │
//! │  The actual file content referenced by operations           │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! # CRDT Operations
//!
//! Changes contain **CRDT operations** organized hierarchically:
//!
//! - **FileOps** (Trunk level): File operations (create, delete, move, undelete)
//! - **LineOps** (Branch level): Line operations (insert, delete, restore)
//! - **LeafOp** (Leaf level): Token operations (insert, delete, replace)
//!
//! This hierarchical model (Trunk → Branch → Leaf) enables:
//! - Token-level diff and blame
//! - Conflict-free merging when changes are independent
//! - Efficient synchronization
//!
//! # Dependencies
//!
//! Changes track their dependencies explicitly:
//!
//! - **dependencies**: Changes that MUST be applied first
//! - **extra_known**: Changes that were known but not directly needed
//!
//! This enables:
//! - Correct ordering during application
//! - Conflict detection when dependencies conflict
//! - Efficient synchronization (know what to send/receive)
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_core::change::{Change, ChangeHeader, GraphOp, Author};
//!
//! // Create a change header
//! let header = ChangeHeader {
//!     message: "Add README file".to_string(),
//!     description: Some("Initial project documentation".to_string()),
//!     timestamp: chrono::Utc::now(),
//!     authors: vec![Author::new("Alice", Some("alice@example.com"))],
//! };
//!
//! // Build the change (typically done via RecordBuilder)
//! let change = Change::new(header, hunks, contents);
//!
//! // Serialize to file
//! let hash = change.serialize(&mut file)?;
//! println!("Change hash: {}", hash);
//! ```
//!
//! # Module Organization
//!
//! - [`ops`]: CRDT operations (FileOps, LineOps) for change storage
//! - [`atom`]: Primitive graph operations (Insertion, EdgeUpdate, NewEdge) - DEPRECATED
//! - [`encoding`]: Text file encoding detection and handling
//! - [`header`]: Change metadata (ChangeHeader, Author)
//! - [`graph_op`]: High-level modification units - DEPRECATED
//! - [`local`]: Local context for human-readable output
//! - [`provenance`]: AI provenance tracking (vendor, model, tokens, cost)
//! - [`credit`]: AI-aware line-level attribution (like git blame)
//! - [`change`]: Complete change structure and serialization
//! - [`format_v3`]: V3 change file format — streaming, compressed, postcard-serialized (Phase 1)

mod atom;
pub mod attestation;
mod change;
mod credit;
mod encoding;
pub mod format_v3;
mod graph_op;
mod header;
mod local;
pub mod ops;
mod provenance;
mod store;

// Re-export all public types
// Allow deprecated types - these are re-exported for backward compatibility

pub use atom::{Atom, EdgeUpdate, Insertion, NewEdge};
pub use attestation::{
    AttestAgent, Attestation, AttestationBuilder, AttestationError, CodeChangeStats, ModelUsage,
    ATTESTATION_EXTENSION,
};
pub use change::{Change, ChangeError, HashedChange};
pub use credit::{Credit, CreditRange, CreditStats, CreditType, FileCredits, LineCredit};
pub use encoding::Encoding;
pub use header::{Author, ChangeHeader};

pub use graph_op::{AtomRef, GraphOp, HunkAtomIter};
pub use local::{Local, LocalByte};
pub use ops::{FileOps, FileOpsStats, LineOps};
pub use provenance::{
    AITool, AIVendor, Cost, PromptContent, Provenance, SuggestionType, TokenUsage,
};
pub use store::{ChangeStore, MemoryChangeStore, MemoryStoreError};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_module_exports() {
        // Verify all expected types are exported
        let test_pos =
            crate::Position::new(crate::Hash::of(b"test"), crate::ChangePosition::new(0));

        let _: Atom<crate::Hash> = Atom::Insertion(Insertion {
            predecessors: vec![],
            successors: vec![],
            flag: crate::EdgeFlags::BLOCK,
            start: crate::ChangePosition::new(0),
            end: crate::ChangePosition::new(10),
            inode: test_pos,
        });

        let _encoding = Encoding::Utf8;

        let _author = Author::new("Test", None::<String>);

        let _header = ChangeHeader::default();
    }
}
