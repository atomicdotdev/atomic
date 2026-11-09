//! Vault-facing commands initialize the vault on first use.
//!
//! `atomic init --no-vault` is the repository that deliberately carries no
//! vault. The vault families (`atomic vault`, `atomic intent`, `atomic memory`)
//! must create it themselves instead of failing with
//! "Vault not initialized. Run `atomic vault init` first."
#![cfg(not(windows))]

use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

const ATOMIC_BIN: &str = env!("CARGO_BIN_EXE_atomic");

fn atomic(repo_dir: &Path, home_dir: &Path, args: &[&str]) -> Output {
    Command::new(ATOMIC_BIN)
        .args(args)
        .current_dir(repo_dir)
        .env("HOME", home_dir)
        .env_remove("ATOMIC_SERVICE")
        .env_remove("ATOMIC_RPC")
        .output()
        .expect("run atomic")
}

fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// A repository created without a vault — the state every vault command must
/// recover from.
fn vaultless_repo() -> (TempDir, TempDir) {
    let repo = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let init = atomic(repo.path(), home.path(), &["init", "--no-vault"]);
    assert!(
        init.status.success(),
        "init --no-vault failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    assert!(!repo.path().join(".vault").exists());
    (repo, home)
}

/// Permit reads, deny database writes — the read-only checkout state a
/// CI mount or a `chmod`ed tree presents.
fn make_database_read_only(repo: &Path) {
    let db = repo.join(".atomic/atomic.redb");
    let mut permissions = std::fs::metadata(&db).unwrap().permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(&db, permissions).unwrap();
}

fn intents_written(repo: &Path) -> bool {
    std::fs::read_dir(repo.join(".vault/intents"))
        .map(|mut entries| {
            entries.any(|entry| {
                entry
                    .map(|e| e.path().join("intent.md").is_file())
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

#[test]
fn intent_new_initializes_the_vault_and_the_next_command_does_not() {
    let (repo, home) = vaultless_repo();

    let created = success(atomic(
        repo.path(),
        home.path(),
        &["intent", "new", "First intent"],
    ));
    assert!(created.contains("Created intent"), "{created}");
    assert!(repo.path().join(".vault/intents").exists());
    assert!(
        intents_written(repo.path()),
        "the intent's markdown entry must exist in .vault/intents"
    );

    // Reading the vault afterwards works and never re-initializes it: an
    // explicit init reports the vault as already present.
    let listed = success(atomic(repo.path(), home.path(), &["intent", "list"]));
    assert!(listed.contains("backlog"), "{listed}");
    let init = success(atomic(repo.path(), home.path(), &["vault", "init"]));
    assert!(init.contains("already initialized"), "{init}");
}

#[test]
fn vault_goal_and_materialize_initialize_before_running() {
    let (repo, home) = vaultless_repo();

    let started = success(atomic(
        repo.path(),
        home.path(),
        &["vault", "goal", "start"],
    ));
    assert!(started.contains("Started goal"), "{started}");

    let materialized = success(atomic(repo.path(), home.path(), &["vault", "materialize"]));
    assert!(materialized.contains("Materialized"), "{materialized}");

    assert!(repo.path().join(".vault/goals").exists());
}

#[test]
fn reads_do_not_initialize_the_vault() {
    let (repo, home) = vaultless_repo();

    let listed = success(atomic(repo.path(), home.path(), &["vault", "list"]));
    assert!(listed.contains("No vault entries found"), "{listed}");
    let intents = success(atomic(repo.path(), home.path(), &["intent", "list"]));
    assert!(intents.contains("No intents found"), "{intents}");
    let memories = success(atomic(repo.path(), home.path(), &["memory", "list"]));
    assert!(memories.contains("No memories found"), "{memories}");

    assert!(
        !repo.path().join(".vault").exists(),
        "a read must never create the vault"
    );
}

#[test]
fn read_only_checkout_serves_vault_reads() {
    let (repo, home) = vaultless_repo();
    make_database_read_only(repo.path());

    let listed = success(atomic(repo.path(), home.path(), &["vault", "list"]));
    assert!(listed.contains("No vault entries found"), "{listed}");
    assert!(!repo.path().join(".vault").exists());

    let mut permissions = std::fs::metadata(repo.path().join(".atomic/atomic.redb"))
        .unwrap()
        .permissions();
    permissions.set_readonly(false);
    std::fs::set_permissions(repo.path().join(".atomic/atomic.redb"), permissions).unwrap();
}

#[test]
fn memory_new_initializes_the_vault_and_persists_the_memory() {
    let (repo, home) = vaultless_repo();

    let created = success(atomic(
        repo.path(),
        home.path(),
        &[
            "memory",
            "new",
            "--kind",
            "lesson",
            "--text",
            "Boring designs outlive clever ones.",
        ],
    ));
    assert!(created.contains("Created memory"), "{created}");

    let listed = success(atomic(repo.path(), home.path(), &["memory", "list"]));
    assert!(listed.contains("lesson"), "{listed}");
}
