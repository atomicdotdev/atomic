use std::fs;
use std::path::{Path, PathBuf};

const ROUTED_COMMANDS: &[&str] = &[
    "src/commands/status.rs",
    "src/commands/diff/command.rs",
    "src/commands/record/command.rs",
    "src/commands/add.rs",
    "src/commands/remove.rs",
    "src/commands/view/new.rs",
    "src/commands/view/switch.rs",
    "src/commands/view/promote.rs",
    "src/commands/restore.rs",
    "src/commands/insert.rs",
    "src/commands/unrecord.rs",
    "src/commands/revise.rs",
    "src/commands/tag/create.rs",
    "src/commands/tag/delete.rs",
    "src/commands/tag/list.rs",
    "src/commands/tag/show.rs",
];

#[test]
fn every_cb5b_command_declares_workspace_transaction_routing() {
    for relative in ROUTED_COMMANDS {
        let source = read(relative);
        assert!(
            source.contains("enter_workspace") || source.contains("observe_workspace"),
            "{relative} bypasses the CB-5B workspace transaction adapter"
        );
        assert!(
            source.contains("WorkspaceTxnMode::Reconcile")
                || source.contains("WorkspaceTxnMode::Observe"),
            "{relative} does not declare an explicit workspace transaction mode"
        );
        assert!(
            !source.contains("let _workspace = enter_workspace"),
            "{relative} discards workspace transaction authority after retaining only its lock"
        );
    }
}

#[test]
fn cb5b_commands_do_not_use_legacy_guards_or_ambient_view_authority() {
    for relative in ROUTED_COMMANDS {
        let source = read(relative);
        assert!(
            !source.contains("guard_working_copy"),
            "{relative} still uses the independent pre-CB-5B guard"
        );
        assert!(
            !source.contains(".current_view()"),
            "{relative} derives authority from Repository::current_view instead of WorkspaceTxn"
        );
        assert!(
            !source.contains(".pristine().write_txn"),
            "{relative} bypasses repository transaction routing with a direct pristine write"
        );
        // `git2::Repository::open` is the libgit2 observation API, not the
        // Atomic repository constructor the CB-5B guard refuses. Mask it
        // before the substring check so the guard stays precise.
        let without_git2 = source.replace("git2::Repository::open(", "GIT2_REPOSITORY_OPEN(");
        assert!(
            !without_git2.contains("Repository::open("),
            "{relative} may initialize repository state before workspace entry"
        );
        assert!(
            !source.contains("Repository::open_existing("),
            "{relative} may recover repository state before workspace entry"
        );
        // status.rs needs the identity for observation-only reads, and
        // record/command.rs needs it for the narrow metadata-only
        // conflict-cleanup preflight (its own comment documents why that
        // administrative route must not run the workspace boundary's
        // recovery/import effects). Both main paths still enter through
        // WorkspaceTxn.
        if !matches!(
            *relative,
            "src/commands/status.rs" | "src/commands/record/command.rs"
        ) {
            assert!(
                !source.contains("require_working_copy_id"),
                "{relative} re-derives working-copy authority instead of using WorkspaceTxn"
            );
        }
    }
}

fn read(relative: &str) -> String {
    let contents = fs::read_to_string(manifest_dir().join(relative))
        .unwrap_or_else(|error| panic!("cannot read {relative}: {error}"));
    contents
        .split("\n#[cfg(test)]\nmod tests")
        .next()
        .unwrap_or(&contents)
        .to_string()
}

fn manifest_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}
