//! All atomic-cli integration tests, built as one binary so the workspace is
//! linked once instead of once per file. Each module is a former
//! `tests/<name>.rs`; run one with `cargo test -p atomic-cli --test it <name>::`.
//!
//! `provenance_rpc_sink` stays its own binary: it sets process env vars
//! (`ATOMIC_SERVICE`, `ATOMIC_DAEMON_SOCKET`) that every `atomic` subprocess
//! spawned by a concurrently running test in this binary would inherit.

mod adaptation_regression_test;
mod agent_capture_oracle_cb12a_test;
mod agent_error_integration_test;
mod agent_repair_cb12a_test;
mod bridge_agent_workflow_regression_test;
mod build_version_integration_test;
mod change_ledger_integration_test;
mod change_tree_test;
mod cli_surface_integration_test;
mod clone_sidecar_integration_test;
mod compact_service_integration_test;
mod concurrent_intent_reads_test;
mod cross_stream_matrix_test;
mod git_adoption_cb7b_test;
mod git_anchor_cb7a_test;
mod git_binding_cb6a_test;
mod git_binding_cb6b_test;
mod git_binding_cb6c_test;
mod git_bridge_reconcile_preserved_delete_test;
mod git_cutover_cb13b_test;
mod git_guard_cb0c_test;
mod git_import_bypass_cycle_regression;
mod git_import_cb9a_test;
mod git_import_cb9b_test;
mod git_import_cb9c_test;
mod git_import_corpus_regression_test;
mod git_import_generated_modify_test;
mod git_import_path_audit_test;
mod git_import_sibling_closure_regression;
mod git_import_sibling_leg_modify_regression;
mod git_import_stale_edge_dead_walk_regression;
mod git_import_tracked_vault_regression;
mod git_import_tree_shape_regression;
mod git_observation_cb0a_test;
mod git_projection_cb8a_test;
mod git_push_cb4b_test;
mod git_shim_branch_reconcile_regression;
mod git_shim_cb0d_test;
mod git_shim_cb10a_test;
mod git_shim_cb13d_test;
mod git_shim_daily_workflow_regression;
mod git_shim_logging_test;
mod git_shim_opt_in_test;
mod git_shim_workspace_test;
mod git_transport_cb10b_test;
mod git_wip_cb0b_test;
mod graph_only_export_cb9b_test;
mod hook_health_integration_test;
mod ide_json_integration_test;
mod import_budget_100k_bench;
mod insert_promote_integration_test;
mod intent_attest_external_signer_test;
mod intent_graph_test;
mod intent_turn_binding_test;
mod kg_service_integration_test;
mod logging_integration_test;
mod memory_compatibility_test;
mod native_index_doctor_integration_test;
mod native_view_materialization_regression_test;
mod operation_cli_integration_test;
mod provenance_integration_test;
mod push_integration_test;
mod record_rename_attributes_test;
mod record_stale_conflict_cleanup_test;
mod remote_sandbox_client_test;
mod remove_recorded_path_test;
mod repository_discovery_integration_test;
mod restore_integration_test;
mod routing_parity_test;
mod session_integration_test;
mod shim_census_cb13a_test;
mod status_git_cb11a_test;
mod unrecord_integration_test;
mod vault_context_integration_test;
mod view_confirmation_integration_test;
mod working_copy_preservation_test;
mod working_copy_service_adaptation_test;
mod workspace_txn_boundaries_cb5c_test;
mod workspace_txn_routing_cb5b_test;
mod workspace_txn_routing_cb5c_test;
