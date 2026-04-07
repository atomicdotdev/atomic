//! Atomic Core - Graph-based version control engine
//!
//! This crate provides the core VCS functionality for Atomic:
//! - Graph data structures (GraphNode, GraphEdge, positions)
//! - Change representation and serialization
//! - Diff algorithms (Myers, Patience)
//! - Change recording and application
//! - Working copy output
//!
//! # Architecture
//!
//! The core is organized into several modules:
//!
//! - `types` - Fundamental data types (NodeId, Hash, GraphNode, GraphEdge, etc.)
//! - `pristine` - Database storage layer (redb-backed)
//! - `change` - Change/patch representation and serialization
//! - `diff` - Diff algorithms (Myers, Patience)
//! - `record` - Recording changes from working copy
//! - `apply` - Applying changes to the graph
//! - `output` - Outputting graph state to working copy
//! - `alive` - Graph traversal and alive/dead classification
//!
//! # Stacks vs Branches
//!
//! Atomic uses "Stacks" instead of branches. Stacks are **views** of the graph -
//! they represent which changes have been applied and in what order. Unlike git
//! branches, stacks don't fork the underlying data; they're perspectives on the
//! same graph.

// Core modules
pub mod change;
pub mod crdt;
pub mod diff;
pub mod pristine;
pub mod types;

// Phase 5: Record and Apply
pub mod apply;
pub mod record;

// Phase 6: Working Copy Output
pub mod output;

// Future modules - to be implemented
// pub mod alive;

// Re-export commonly used types
pub use types::*;

// Re-export change types
pub use change::{
    AITool, AIVendor, Atom, Author, Change, ChangeError, ChangeHeader, Cost, Credit, CreditRange,
    CreditStats, CreditType, EdgeUpdate, Encoding, FileCredits, GraphOp, HashedChange, Insertion,
    LineCredit, Local, LocalByte, NewEdge, PromptContent, Provenance, SuggestionType, TokenUsage,
};

// Re-export pristine types
pub use pristine::{
    GraphTxnT, MutTxnT, Pristine, PristineError, PristineResult, ReadTxn, TreeTxnT, VertexExt,
    ViewState, ViewTxnT, WriteTxn,
};

// Re-export diff types
pub use diff::{
    convert_diff_to_file_ops, convert_diff_to_file_ops_with_config, diff, diff_text,
    diff_with_separator, semantic_diff, semantic_diff_with_config, Algorithm, ConversionConfig,
    ConversionError, ConversionResult, ConversionStats, DiffOp, DiffResult, DiffStats, DisplayLine,
    Line, LineChange, LinePair, LineSplit, LineStatus, Replacement, SemanticDiff,
    SemanticDiffConfig, SemanticDiffStats, SemanticLine, SemanticToCrdt, Separator, SideBySideDiff,
    TokenChange, UnifiedDiff,
};

// Re-export record types
pub use record::{
    FileMetadata, InodeUpdate, RecordBuilder, RecordError, RecordItem, RecordResult, RecordStats,
    Recorded,
};

// Re-export apply types
pub use apply::{
    compute_new_state, is_change_on_view, validate_can_apply, verify_dependencies,
    ApplyChangeResult, ApplyError, ApplyResult, ChangeToApply, LocalApplyError, LocalApplyResult,
    MissingContext, PendingEdge, Workspace, WorkspaceStats, Zombie,
};

// Re-export output types
pub use output::{
    markers, Conflict, ConflictType, ContentError, ContentResult, Memory, MemoryError, OutputError,
    OutputItem, OutputResult, OutputStats, Sink, SinkError, TreeError, TreeResult, VertexBuffer,
    WorkingCopy, WorkingCopyRead, Writer,
};

// Re-export CRDT types
pub use crdt::{
    Branch, BranchId, BranchOp, BranchState, Leaf, LeafId, LeafOp, LeafState, Trunk, TrunkId,
    TrunkOp, TrunkState,
};
// Note: FileMetadata is exported from output module but also exists in record module
// Use output::FileMetadata or record::FileMetadata explicitly to disambiguate

/// The directory name for Atomic repositories
pub const DOT_DIR: &str = ".atomic";

/// The default stack name
pub const DEFAULT_STACK: &str = "main";

/// Crate version for change format compatibility
pub const VERSION: u64 = 1;
