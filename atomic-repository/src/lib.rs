//! High-level repository operations for Atomic VCS
//!
//! This crate provides the main `Repository` abstraction that coordinates
//! all VCS operations including initialization, recording changes, and
//! working copy management.
//!
//! # Overview
//!
//! The `atomic-repository` crate serves as the high-level orchestration layer
//! for the Atomic version control system. It builds on top of `atomic-core`
//! (which provides the graph algorithms and storage layer) to provide
//! user-facing repository operations.
//!
//! # Key Components
//!
//! - [`Repository`] - The main entry point for all repository operations
//! - [`ChangeStore`] - Filesystem-backed storage for change files
//! - [`RepositoryError`] - Comprehensive error types for all operations
//! - [`history`] - History querying and traversal
//! - [`unrecord`] - Undo applied changes
//! - [`archive`] - Export repository state
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_repository::Repository;
//!
//! // Initialize a new repository
//! let repo = Repository::init("/path/to/project")?;
//!
//! // Or open an existing one
//! let repo = Repository::open("/path/to/project")?;
//!
//! // Work with stacks (views of the graph)
//! let stacks = repo.list_stacks()?;
//! repo.set_current_stack("feature-branch")?;
//!
//! // Query history
//! use atomic_repository::HistoryOptions;
//! let history = repo.log(HistoryOptions::default())?;
//!
//! // Create tags
//! use atomic_repository::TagKind;
//! repo.create_tag("v1.0.0", None, TagKind::Release)?;
//!
//! // Create archives
//! use atomic_repository::ArchiveOptions;
//! repo.archive("release.tar.gz", ArchiveOptions::default())?;
//! ```
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                         atomic-repository                               │
//! │  ┌───────────┐  ┌────────────┐  ┌────────┐  ┌─────┐  ┌────────┐        │
//! │  │Repository │──│ChangeStore│──│ Status │──│Tags │──│Archive │        │
//! │  └───────────┘  └────────────┘  └────────┘  └─────┘  └────────┘        │
//! │        │                                                                │
//! │        └──────────┬──────────┬──────────┬──────────┐                   │
//! │                   │          │          │          │                   │
//! │              ┌────────┐ ┌────────┐ ┌─────────┐ ┌────────┐              │
//! │              │History │ │Tracking│ │Unrecord │ │ Apply  │              │
//! │              └────────┘ └────────┘ └─────────┘ └────────┘              │
//! └─────────────────────────────────────────────────────────────────────────┘
//!                              │
//!                              ▼
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                           atomic-core                                    │
//! │  ┌─────────┐  ┌─────────┐  ┌────────┐  ┌────────┐  ┌────────┐          │
//! │  │pristine │  │ change  │  │ record │  │ apply  │  │ output │          │
//! │  └─────────┘  └─────────┘  └────────┘  └────────┘  └────────┘          │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Module Organization
//!
//! - [`apply`] - Change application to the graph
//! - [`archive`] - Export repository state to various formats
//! - [`changestore`] - Filesystem-backed change storage with caching
//! - [`error`] - Comprehensive error types
//! - [`history`] - History querying and traversal
//! - [`remote`] - Remote repository configuration
//! - [`repository`] - Main Repository struct and operations
//! - [`status`] - Working copy status tracking
//! - [`tracking`] - File tracking operations
//! - [`unrecord`] - Undo applied changes

// Core modules
pub mod apply;
pub mod changestore;
pub mod error;
pub mod ignore;
pub mod record;
pub mod repository;
pub mod status;
pub mod tracking;

// Phase 7 additions
pub mod archive;
pub mod history;
pub mod unrecord;

// Phase 9 additions
pub mod remote;

// Phase 4: Parallel recording pipeline
pub mod parallel_record;

// Phase 5: redb-native change storage
pub mod redb_change_store;

// Phase 6: Semantic regeneration (rebuild SEMANTIC from graph + content after thin pull)
pub mod semantic_regen;

// AI provider resolution (embeddings + LLM)
pub mod ai;

// Content search powered by syntext
pub mod content_search;

// Query plan schema and executor
pub mod query_plan;

// Content search re-exports
pub use content_search::{
    build_content_index, content_index_stats, has_content_index, search_content,
    update_content_index, ContentIndexStats, ContentMatch, ContentSearchError,
    ContentSearchOptions, ContentSearchResult,
};

// Re-export main types at crate root for convenience

// Change store exports
pub use changestore::{ChangeStore, ChangeStoreError, ChangeStoreResult, DEFAULT_CACHE_CAPACITY};

// Error exports
pub use error::*;

// Repository exports
pub use repository::*;

// Vault name generation
pub use repository::generate_goal_name;

// Vault goal exports
pub use repository::{
    GoalInfo, GoalStartOptions, GoalStartResult, GoalStopOptions, GoalStopResult,
};

// Vault intent exports
pub use repository::{
    IntentCreateOptions, IntentCreateResult, IntentDeleteResult, IntentInfo, IntentUpdateOptions,
};

// Vault embedding exports
pub use repository::{hash_embed, EmbedConfig, TextChunk};

// AI provider exports
pub use ai::{
    build_context_string, resolve_embedding_provider, resolve_llm_provider, AiError, AiProvider,
    EmbeddingProvider, LlmProvider, LlmResponse,
};

// Tool-use agentic loop exports
pub use ai::tools::{run_tool_loop_sync, AgentConfig, AgentResult, RepoToolExecutor, ToolExecutor};

// Query plan exports
pub use query_plan::{execute_plan, parse_plan, PlanResult, QueryPlan, QueryStep, StepStat};

// Status exports
pub use status::{
    collect_working_copy_files, collect_working_copy_files_with_rules, hash_file_contents,
    is_always_ignored, FileStatus, FileStatusEntry, RepositoryStatus, StatusError, StatusOptions,
    StatusResult,
};

// Tracking exports
pub use tracking::{
    add_to_tree, collect_files_for_tracking, collect_files_for_tracking_with_rules, get_inode,
    get_path, is_tracked, list_tracked, move_tracked, normalize_path, normalize_path_with_root,
    remove_from_tree, should_ignore, should_ignore_with_rules, tracked_under_prefix, TrackedFile,
    TrackingError, TrackingOptions, TrackingResult, TrackingStats,
};

// Apply exports
pub use apply::{
    check_missing_dependencies, collect_all_dependencies, compute_insert_order,
    filter_missing_in_view, get_changes_up_to_seq, get_missing_changes, get_view_changes,
    order_changes_by_deps, write_change_to_graph, CrossViewInsertOptions, CrossViewInsertOutcome,
    InsertError, InsertOptions, InsertOutcome, InsertResult, InsertStats,
};

// History exports
pub use history::{
    find_change_sequence, get_change_at_sequence, get_changes_up_to_change,
    get_changes_up_to_sequence, get_files_in_change, get_state_before_change, history_summary,
    is_change_in_history, log, reverse_log, HistoryEntry, HistoryError, HistoryIter,
    HistoryOptions, HistoryResult, HistorySummary, PathHistoryEntry, PathModificationType,
    StateBeforeChange,
};

// Tags exports (first-class redb backend)
pub use atomic_core::pristine::{TagKind, TagMutTxnT, TagRecord, TagTxnT};

// Git SHA index exports
pub use atomic_core::pristine::{GitShaIndexMutTxnT, GitShaIndexTxnT};

// Unrecord exports
pub use unrecord::{
    check_can_unrecord, compute_state_after_unrecord, find_unrecord_set, get_last_change,
    get_last_sequence, preview_unrecord, UnrecordDependencyInfo, UnrecordError, UnrecordOptions,
    UnrecordOutcome, UnrecordResult, UnrecordStats,
};

// Remote exports
pub use remote::{RemoteConfig, RemoteEntry, RemoteError, RemoteResult};

// Record exports
pub use record::{
    build_header, filter_files, RecordError, RecordOptions, RecordOutcome, RecordResult,
    RecordStats,
};

// Archive exports
pub use archive::{
    ensure_extension, get_archive_path, Archive, ArchiveEntry, ArchiveError, ArchiveFormat,
    ArchiveManifest, ArchiveOptions, ArchiveOutcome, ArchiveResult, DirectoryArchive,
    DirectoryFile,
};

// Ignore exports
pub use ignore::{global_ignore_path, local_ignore_path, IgnoreError, IgnoreResult, IgnoreRules};
