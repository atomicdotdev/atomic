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

pub(crate) mod capability_txn;
mod crdt_read;
mod embeddings;
mod file_index_v2;
mod graph;
mod mutate;
mod native_derived;
mod operation;
mod path_claim;
pub mod tag;
mod tree;
mod triples;
mod vault;
mod vertex_ext;
mod view;
mod working_copy;

#[cfg(test)]
mod tests;

pub use capability_txn::{CapabilityMutTxnT, CapabilityTxnT};
pub use crdt_read::CrdtTxnT;
pub use embeddings::{EmbeddingsMutTxnT, EmbeddingsTxnT};
pub use file_index_v2::{FileIndexV2MutTxnT, FileIndexV2TxnT};
pub use graph::GraphTxnT;
pub use mutate::MutTxnT;
pub use native_derived::{NativeDerivedIndexes, NativeDerivedIndexesMutTxnT};
pub use operation::{OperationMutTxnT, OperationTxnT};
pub use path_claim::{PathClaimMutTxnT, PathClaimTxnT};
pub use tag::{
    BridgeEventCaptureMutTxnT, BridgeEventCaptureTxnT, GitCommitClosureMutTxnT,
    GitCommitClosureTxnT, GitShaIndexMutTxnT, GitShaIndexTxnT, TagKind, TagMutTxnT, TagRecord,
    TagTxnT,
};
pub use tree::{FileIndexEntry, FileIndexMetadata, TreeTxnT};
pub use triples::{KgMutTxnT, KgTxnT};
pub use vault::{VaultEntryMeta, VaultMutTxnT, VaultTxnT};
pub use vertex_ext::VertexExt;
pub use view::{
    EffectiveProjectionClosure, GraphVisibilityClosure, StoredConflict, StoredConflictKind,
    ViewMembershipSet, ViewScope, ViewState, ViewTxnT,
};
pub use working_copy::{
    decode_working_copy_record, encode_working_copy_record, WorkingCopyMutTxnT, WorkingCopyRecord,
    WorkingCopyTxnT, WORKING_COPY_RECORD_V1_SIZE, WORKING_COPY_RECORD_VERSION,
};
