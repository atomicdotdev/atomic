//! CB-5C routing coverage: Git, network, and agent boundaries must declare an
//! explicit workspace transaction mode and retain its authority.

use std::fs;
use std::path::{Path, PathBuf};

/// CB-5C working-copy-aware entrypoints. Each must enter the shared workspace
/// transaction adapter and declare an explicit mode.
const CB5C_ROUTED_ENTRYPOINTS: &[&str] = &[
    "src/commands/pull/command.rs",
    "src/commands/push/command.rs",
    "src/commands/clone/command.rs",
    "src/commands/git/import.rs",
    "src/commands/git/push.rs",
    "src/commands/git/bridge.rs",
    "src/commands/agent/hooks.rs",
];

#[test]
fn every_cb5c_boundary_declares_workspace_transaction_routing() {
    for relative in CB5C_ROUTED_ENTRYPOINTS {
        let source = read(relative);
        assert!(
            source.contains("enter_workspace")
                || source.contains("enter_remediation_workspace")
                || source.contains("observe_workspace")
                || source.contains(".begin_workspace_txn("),
            "{relative} bypasses the CB-5C workspace transaction adapter"
        );
        assert!(
            source.contains("WorkspaceTxnMode::Reconcile")
                || source.contains("WorkspaceTxnMode::Observe")
                || source.contains("begin_remediation_txn")
                || source.contains("enter_remediation_workspace")
                || source.contains("boundary_mode"),
            "{relative} does not declare an explicit workspace transaction mode"
        );
        assert!(
            !source.contains("let _workspace = enter_workspace"),
            "{relative} discards workspace transaction authority after retaining only its lock"
        );
    }
}

#[test]
fn cb5c_boundaries_do_not_use_legacy_guards_or_ambient_view_authority() {
    for relative in CB5C_ROUTED_ENTRYPOINTS {
        let source = read(relative);
        assert!(
            !source.contains("guard_working_copy"),
            "{relative} still uses the independent pre-CB-5C stale-baseline guard"
        );
        assert!(
            !source.contains(".current_view()"),
            "{relative} derives authority from Repository::current_view instead of WorkspaceTxn"
        );
        assert!(
            !source.contains(".pristine().write_txn"),
            "{relative} bypasses repository transaction routing with a direct pristine write"
        );
    }
}

#[test]
fn clone_transport_uses_an_explicit_bootstrap_boundary() {
    let source = read("src/commands/clone/command.rs");
    assert!(
        source.contains("CloneBootstrapBoundary"),
        "clone transport must pass through the explicit bootstrap boundary"
    );
    let helpers = read("src/commands/clone/helpers.rs");
    assert!(
        helpers.contains("CloneBootstrapBoundary"),
        "the bootstrap boundary marker must be defined for the transport phase"
    );
    // The production command derives repository-local authority only after
    // Repository::init created the working copy.
    let body = strip_tests(&source);
    assert!(
        body.contains("CloneBootstrapBoundary::begin"),
        "clone must open the bootstrap boundary before repository-local phases"
    );
}

#[test]
fn dry_run_network_boundaries_observe_without_mutation() {
    for (relative, token) in [
        (
            "src/commands/pull/command.rs",
            "boundary_mode(self.dry_run)",
        ),
        (
            "src/commands/push/command.rs",
            "boundary_mode(self.dry_run)",
        ),
    ] {
        let source = read(relative);
        assert!(
            source.contains("WorkspaceTxnMode::Observe") || source.contains("boundary_mode"),
            "{relative} does not declare the Observe dry-run mode"
        );
        let _ = token_present(&source, token);
    }
}

fn read(relative: &str) -> String {
    let contents = fs::read_to_string(manifest_dir().join(relative))
        .unwrap_or_else(|error| panic!("cannot read {relative}: {error}"));
    strip_tests(&contents)
}

fn strip_tests(contents: &str) -> String {
    contents
        .split("\n#[cfg(test)]\nmod tests")
        .next()
        .unwrap_or(contents)
        .to_string()
}

fn token_present(source: &str, token: &str) -> bool {
    source.contains(token)
}

fn manifest_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}
