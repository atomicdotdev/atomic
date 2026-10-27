//! All atomic-repository integration tests, built as one binary so the workspace is
//! linked once instead of once per file. Each module is a former
//! `tests/<name>.rs`; run one with `cargo test -p atomic-repository --test it <name>::`.

mod causal_file_identity_test;
mod cb_n6_merge_rubric_test;
mod change_signing_test;
mod content_roundtrip_fidelity_test;
mod database_consolidation_shim_test;
mod database_lock_exclusivity_test;
mod database_open_wait_test;
mod effective_history_test;
mod effective_projection_3b_test;
mod equivalence_cb4b_test;
mod implementation_declaration;
mod intent_graph;
mod kg_change_availability;
mod kg_projection;
mod kg_recorded_impact;
mod legacy_database_merge_test;
mod materialize_view_entries_test;
mod migration_recovery_test;
mod patch_evidence;
mod project_identity;
mod project_identity_transport;
mod project_tree_cb4a_test;
mod provenance_checkpoint_test;
mod remote_cache_parity_test;
mod revise_content_regression;
mod schema_guard_test;
mod staging_cb11a_test;
mod stash_attributes_test;
mod state_based_diff_test;
mod unrecord_safety_test;
mod view_delete_test;
mod view_manifest_test;
mod view_setid_test;
mod view_split_test;
mod whole_file_diff_test;
mod workflow_usage;
