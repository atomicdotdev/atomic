//! CRDT operation generation for the record workflow.
//!
//! This module converts working copy content and diff operations into
//! fine-grained CRDT operations using the hierarchical Trunk → Branch → Leaf
//! model. This enables conflict-free merging at the token level.
//!
//! # Architecture Overview
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                    CRDT Record Workflow Integration                      │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  Input                                 Output                           │
//! │  ┌────────────────────────┐          ┌──────────────────────────┐      │
//! │  │ Working Copy Content   │          │ TrunkOp (file level)     │      │
//! │  │        ↓               │   ───►   │ BranchOp (line level)    │      │
//! │  │ Line-Level Diffs       │          │ LeafOp (token level)     │      │
//! │  │        ↓               │          └──────────────────────────┘      │
//! │  │ Token-Level Analysis   │                                            │
//! │  └────────────────────────┘                                            │
//! │                                                                         │
//! │  Processing Pipeline:                                                   │
//! │  ┌─────────────────────────────────────────────────────────────────┐   │
//! │  │ 1. tokenize.rs  - Convert content bytes → Leaf structures      │   │
//! │  │ 2. line_ops.rs  - Convert line diffs → BranchOp operations     │   │
//! │  │ 3. convert.rs   - Convert GraphOp types → CRDT operations         │   │
//! │  │ 4. builder.rs   - Accumulate and finalize CRDT operations      │   │
//! │  └─────────────────────────────────────────────────────────────────┘   │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Module Structure
//!
//! - [`tokenize`] - Tokenization of content into Leaf structures and operations
//! - [`line_ops`] - Line-level diff analysis to Branch operations
//! - [`convert`] - GraphOp to CRDT operation conversion
//! - [`builder`] - Builder for accumulating CRDT operations during recording
//!
//! # Core Concepts
//!
//! ## Hierarchical CRDT Model
//!
//! The CRDT model uses three levels of granularity:
//!
//! - **Trunk**: Represents a file. Created by `TrunkOp::Create`, deleted by
//!   `TrunkOp::Delete`, moved by `TrunkOp::Move`.
//!
//! - **Branch**: Represents a line within a file. Created by `BranchOp::Insert`,
//!   deleted by `BranchOp::Delete`. Each branch contains an ordered sequence
//!   of leaves (tokens).
//!
//! - **Leaf**: Represents a token within a line. Created by `LeafOp::Insert`,
//!   deleted by `LeafOp::Delete`, modified by `LeafOp::Replace`. The `Replace`
//!   operation preserves the leaf's identity for accurate blame tracking.
//!
//! ## Recording Flow
//!
//! 1. **File Addition**: Generate `TrunkOp::Create`, then for each line generate
//!    `BranchOp::Insert` with nested `LeafOp::Insert` for each token.
//!
//! 2. **File Deletion**: Generate `TrunkOp::Delete` which cascades to mark all
//!    branches and leaves as deleted.
//!
//! 3. **File Modification**: Analyze diffs to generate:
//!    - `BranchOp::Delete` for removed lines
//!    - `BranchOp::Insert` for added lines (with `LeafOp::Insert` for tokens)
//!    - For modified lines: `LeafOp::Delete`/`Insert`/`Replace` as appropriate
//!
//! # Example: Recording a New File
//!
//! ```rust
//! use atomic_core::record::workflow::crdt::{
//!     CrdtChangeBuilder, TokenizeOptions, ContentTokenizer,
//! };
//! use atomic_core::change::Encoding;
//! use atomic_core::types::NodeId;
//!
//! // Create a builder for the current change
//! let change_id = NodeId::new(1);
//! let mut builder = CrdtChangeBuilder::new(change_id);
//!
//! // Add a new file
//! let content = b"fn main() {\n    println!(\"Hello\");\n}\n";
//! let trunk_id = builder.add_file("src/main.rs", Some(Encoding::Utf8));
//!
//! // Tokenize and add content
//! let tokenizer = ContentTokenizer::new(content);
//! for (line_idx, line) in tokenizer.lines().enumerate() {
//!     let branch_id = builder.add_line(trunk_id, None); // None = append
//!     for token in line.tokens() {
//!         builder.add_token(branch_id, None, token.kind(), token.content());
//!     }
//! }
//!
//! // Get the generated operations
//! let result = builder.finish();
//! assert_eq!(result.trunk_ops().len(), 1); // One TrunkOp::Create
//! assert_eq!(result.branch_ops().len(), 3); // Three lines
//! ```
//!
//! # Example: Recording a File Modification
//!
//! ```rust
//! use atomic_core::record::workflow::crdt::{
//!     LineAnalyzer, AnalysisOptions, CrdtChangeBuilder,
//! };
//! use atomic_core::types::NodeId;
//! use atomic_core::crdt::TrunkId;
//!
//! let change_id = NodeId::new(2);
//! let mut builder = CrdtChangeBuilder::new(change_id);
//!
//! // Existing file's trunk ID (from a previous change)
//! let trunk_id = TrunkId::new(NodeId::new(1), 0);
//!
//! // Analyze the diff between old and new content
//! let old_content = b"line one\nline two\n";
//! let new_content = b"line one\nmodified line\nline three\n";
//!
//! let analyzer = LineAnalyzer::new(old_content, new_content, AnalysisOptions::default());
//! let analysis = analyzer.analyze();
//!
//! // Apply the analysis to generate CRDT operations
//! for change in analysis.changes() {
//!     builder.apply_line_change(trunk_id, change);
//! }
//!
//! let result = builder.finish();
//! // Result contains BranchOp::Delete for "line two"
//! // BranchOp::Insert for "modified line" and "line three"
//! ```
//!
//! # Performance Characteristics
//!
//! | Operation | Complexity | Notes |
//! |-----------|------------|-------|
//! | Tokenize line | O(n) | n = bytes in line |
//! | Analyze diff | O(m + n) | m, n = lines in old/new |
//! | Convert graph_op | O(t) | t = total tokens affected |
//! | Build operations | O(ops) | Linear in operation count |
//!
//! The tokenization is performed incrementally as content is processed,
//! avoiding the need to hold the entire tokenized file in memory.
//!
//! # Thread Safety
//!
//! The builder types are designed for single-threaded use within a recording
//! session. For parallel recording of multiple files, create separate builders
//! and merge the results using [`CrdtChangeBuilder::merge`].

pub mod builder;
pub mod convert;
pub mod line_ops;
pub mod tokenize;

// Re-export main types for convenience
pub use builder::{
    CrdtBuildError, CrdtBuildStats, CrdtChangeBuilder, CrdtChangeResult, FileOps, LineOps, TokenOps,
};
pub use convert::{ConversionOptions, ConversionStats, ConvertError, ConvertedOps, HunkConverter};
pub use line_ops::{
    AnalysisOptions, AnalysisResult, AnalysisStats, LineAnalysis, LineAnalyzer, LineChange,
    LineChangeKind,
};
pub use tokenize::{ContentTokenizer, TokenStats, TokenizeError, TokenizeOptions, TokenizedLine};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_module_structure_exists() {
        // Verify module structure is accessible
        // These will fail to compile if modules don't export correctly
        let _ = std::any::type_name::<CrdtChangeBuilder>();
        let _ = std::any::type_name::<HunkConverter>();
        let _ = std::any::type_name::<LineAnalyzer>();
        let _ = std::any::type_name::<ContentTokenizer>();
    }
}
