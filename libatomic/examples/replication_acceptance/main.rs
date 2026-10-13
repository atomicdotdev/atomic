//! Run with a freshly built CLI. All data/endpoints are isolated and disposable.
//! This exercises stock push/pull against a protocol fixture, not production auth.
mod checks;
mod fixture;
mod observe;
mod peer;
use atomic_core::{pristine::ViewTxnT, types::Base32};
use atomic_objects::ObjectFamily;
use atomic_repository::Repository;
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn attempt(binary: &Path, root: &Path, config: &Path, args: &[&str]) -> Result<Value> {
    let output = Command::new(binary)
        .current_dir(root)
        .args(args)
        .env("ATOMIC_SERVICE", "local")
        .env("ATOMIC_RPC", "0")
        .env("ATOMIC_CONFIG_DIR", config)
        .env("ATOMIC_DAEMON_BIN", "/nonexistent/acceptance-no-daemon")
        .env(
            "ATOMIC_DAEMON_SOCKET",
            "/nonexistent/acceptance-no-daemon.sock",
        )
        .output()?;
    Ok(
        json!({"success":output.status.success(),"exit":output.status.code(),"stdout":String::from_utf8_lossy(&output.stdout),"stderr":String::from_utf8_lossy(&output.stderr)}),
    )
}
fn cli(binary: &Path, root: &Path, config: &Path, args: &[&str]) -> Result<Value> {
    let result = attempt(binary, root, config, args)?;
    if result["success"] != true {
        return Err(format!("atomic {:?} failed: {}", args, result).into());
    }
    Ok(result)
}
fn fresh(binary: &Path, root: &Path, config: &Path, url: &str, view: &str) -> Result<()> {
    drop(Repository::init(root)?);
    cli(
        binary,
        root,
        config,
        &["pull", url, "--from-view", view, "--to-view", view],
    )?;
    Ok(())
}
fn record(report: &mut Value, name: &str, value: Value) {
    report.as_object_mut().unwrap().insert(name.into(), value);
    if let Some(root) = report["root"].as_str() {
        // Preserve completed observations if a later stage fails.
        let _ = serde_json::to_vec_pretty(report).and_then(|bytes| {
            fs::write(Path::new(root).join("report.json"), bytes).map_err(serde_json::Error::io)
        });
    }
}
fn run(binary: &Path, root: &Path) -> Result<Value> {
    let config = root.join("isolated-config");
    fs::create_dir_all(&config)?;
    // Standalone process, before the listener/runtime threads are started.
    // Direct Rust fixture operations must use the same isolated identity/config.
    std::env::set_var("ATOMIC_CONFIG_DIR", &config);
    let source = root.join("source");
    let receiver = root.join("receiver");
    let cold = root.join("cold-after-delete");
    let peer = peer::Peer::start(&root.join("wire-objects"))?;
    let url = peer.url();
    // Fixture-only signing identity; stored outside every replicated repository.
    cli(
        binary,
        root,
        &config,
        &["identity", "new", "acceptance", "--set-default"],
    )?;
    let fixture = fixture::create(binary, &source, &config)?;
    cli(
        binary,
        &source,
        &config,
        &["tag", "create", "acceptance-tag"],
    )?;
    let mut report = json!({"test_kind":"real CLI push/pull plus fresh receiver reconstruction",
        "production_server_tested":false,"protocol_fixture_policy":"trusted sender; CAS refs; first-write immutable objects; not production authorization or edge cleanup",
        "production_server_blocker":"atomic-storage requires local ^0.16.1 crates; checkout is 0.19.1. Docker unavailable.",
        "root":root,"remote":url,"cli":binary,"changes":{ "feature":fixture.change.to_base32() }});
    record(
        &mut report,
        "source_before_push",
        observe::repository(&source, &fixture)?,
    );
    record(
        &mut report,
        "source_backend",
        observe::backend(&source, &fixture)?,
    );
    record(
        &mut report,
        "first_push",
        cli(
            binary,
            &source,
            &config,
            &["push", &url, "--from-view", "feature"],
        )?,
    );
    record(&mut report, "wire_after_first_push", peer.inventory());
    fresh(binary, &receiver, &config, &url, "feature")?;
    record(
        &mut report,
        "receiver_after_pull",
        observe::repository(&receiver, &fixture)?,
    );
    record(
        &mut report,
        "receiver_backend",
        observe::backend(&receiver, &fixture)?,
    );
    let edges = observe::storage(&receiver, None)?;
    cli(
        binary,
        &source,
        &config,
        &["push", &url, "--from-view", "feature"],
    )?;
    cli(
        binary,
        &receiver,
        &config,
        &[
            "pull",
            &url,
            "--from-view",
            "feature",
            "--to-view",
            "feature",
        ],
    )?;
    record(
        &mut report,
        "repeat_pull",
        json!({"before":edges,"after":observe::storage(&receiver,None)?}),
    );

    record(
        &mut report,
        "receiver_checkout_feature",
        attempt(binary, &receiver, &config, &["view", "switch", "feature"])?,
    );
    record(
        &mut report,
        "receiver_force_checkout_feature",
        attempt(
            binary,
            &receiver,
            &config,
            &["view", "switch", "feature", "--force"],
        )?,
    );
    record(
        &mut report,
        "receiver_after_checkout_feature",
        observe::repository(&receiver, &fixture)?,
    );
    record(
        &mut report,
        "receiver_backend_after_checkout",
        observe::backend(&receiver, &fixture)?,
    );

    let (late, no_code) = fixture::late(&source, &fixture)?;
    record(
        &mut report,
        "metadata_only_push",
        cli(
            binary,
            &source,
            &config,
            &["push", &url, "--from-view", "feature"],
        )?,
    );
    record(
        &mut report,
        "late_evidence",
        json!({"late_hash":late.to_base32(),"no_code_hash":no_code.to_base32(),
        "late_uploaded":peer.contains(ObjectFamily::Provenance,&late.to_base32()),"no_code_uploaded":peer.contains(ObjectFamily::Provenance,&no_code.to_base32()),
        "source":observe::repository(&source,&fixture)?}),
    );
    let graph_before = observe::storage(&source, None)?;
    record(
        &mut report,
        "insert",
        cli(
            binary,
            &source,
            &config,
            &["insert", "view", "feature", "--to-view", "dev"],
        )?,
    );
    let graph_after = observe::storage(&source, None)?;
    record(
        &mut report,
        "insert_storage",
        json!({"before":graph_before,"after":graph_after}),
    );
    record(
        &mut report,
        "source_after_insert",
        observe::repository(&source, &fixture)?,
    );
    cli(
        binary,
        &source,
        &config,
        &["insert", "view", "feature", "--to-view", "dev"],
    )?;
    record(
        &mut report,
        "source_after_repeat_insert",
        observe::storage(&source, None)?,
    );
    cli(
        binary,
        &source,
        &config,
        &["push", &url, "--from-view", "dev"],
    )?;
    cli(
        binary,
        &receiver,
        &config,
        &["pull", &url, "--from-view", "dev", "--to-view", "dev"],
    )?;
    record(
        &mut report,
        "receiver_after_insert_sync",
        observe::repository(&receiver, &fixture)?,
    );
    record(
        &mut report,
        "receiver_backend_after_insert_sync",
        observe::backend(&receiver, &fixture)?,
    );
    record(
        &mut report,
        "late_evidence_after_insert_sync",
        json!({"late_uploaded":peer.contains(ObjectFamily::Provenance,&late.to_base32()),"no_code_uploaded":peer.contains(ObjectFamily::Provenance,&no_code.to_base32())}),
    );

    let orphan = fixture::orphan(binary, &source, &config)?;
    cli(
        binary,
        &source,
        &config,
        &["push", &url, "--from-view", "orphan"],
    )?;
    cli(
        binary,
        &receiver,
        &config,
        &["pull", &url, "--from-view", "orphan", "--to-view", "orphan"],
    )?;
    record(
        &mut report,
        "receiver_with_orphan",
        observe::repository(&receiver, &fixture)?,
    );
    record(
        &mut report,
        "delete_current_refused",
        attempt(
            binary,
            &source,
            &config,
            &["view", "delete", "orphan", "--force"],
        )?,
    );
    cli(binary, &source, &config, &["view", "switch", "dev"])?;
    let feature_id = observe::view_id(&source, "feature")?;
    let orphan_id = observe::view_id(&source, "orphan")?;
    let before_feature = observe::storage(&source, Some(feature_id))?;
    let before_orphan = observe::storage(&source, Some(orphan_id))?;
    let workspace = source.join(".atomic/workspaces/feature");
    fs::create_dir_all(&workspace)?;
    fs::write(workspace.join("test-artifact"), b"shelved")?;
    cli(
        binary,
        &source,
        &config,
        &["view", "delete", "feature", "--force"],
    )?;
    cli(
        binary,
        &source,
        &config,
        &["view", "delete", "orphan", "--force"],
    )?;
    let repo = Repository::open_readonly(&source)?;
    let orphan_retained = repo.load_change(&orphan).is_ok();
    let txn = repo.pristine().read_txn()?;
    let feature_gone = txn.get_view("feature")?.is_none();
    let orphan_gone = txn.get_view("orphan")?.is_none();
    drop(txn);
    drop(repo);
    record(
        &mut report,
        "source_delete",
        json!({"feature_view_gone":feature_gone,"orphan_view_gone":orphan_gone,"orphan_change_retained":orphan_retained,
        "shelved_workspace_removed":!workspace.exists(),"feature_before":before_feature,"feature_after":observe::storage(&source,Some(feature_id))?,"orphan_before":before_orphan,"orphan_after":observe::storage(&source,Some(orphan_id))?,
        "queries":observe::repository(&source,&fixture)?}),
    );
    cli(
        binary,
        &source,
        &config,
        &["push", &url, "--from-view", "dev"],
    )?;
    record(&mut report, "wire_after_local_delete", peer.inventory());
    fresh(binary, &cold, &config, &url, "orphan")?;
    record(
        &mut report,
        "cold_receiver_after_local_delete",
        observe::repository(&cold, &fixture)?,
    );
    cli(binary, &receiver, &config, &["view", "switch", "dev"])?;
    cli(
        binary,
        &receiver,
        &config,
        &["view", "delete", "orphan", "--force"],
    )?;
    record(
        &mut report,
        "receiver_after_local_delete",
        observe::repository(&receiver, &fixture)?,
    );
    record(
        &mut report,
        "receiver_repull_result",
        attempt(
            binary,
            &receiver,
            &config,
            &["pull", &url, "--from-view", "orphan", "--to-view", "orphan"],
        )?,
    );
    record(
        &mut report,
        "receiver_after_repull",
        observe::repository(&receiver, &fixture)?,
    );

    // Refused deletion must not destroy shelved build/user artifacts.
    let mut repo = Repository::open_existing(&source)?;
    repo.create_draft_view("guard", "dev")?;
    repo.create_draft_view("guard-child", "guard")?;
    drop(repo);
    let guard_workspace = source.join(".atomic/workspaces/guard");
    fs::create_dir_all(&guard_workspace)?;
    fs::write(
        guard_workspace.join("test-artifact"),
        b"must survive refusal",
    )?;
    let refused = attempt(
        binary,
        &source,
        &config,
        &["view", "delete", "guard", "--force"],
    )?;
    record(
        &mut report,
        "delete_with_child",
        json!({"result":refused,"workspace_preserved":guard_workspace.exists()}),
    );
    cli(binary, &source, &config, &["view", "switch", "guard-child"])?;
    record(
        &mut report,
        "shared_delete_refused",
        attempt(
            binary,
            &source,
            &config,
            &["view", "delete", "dev", "--force"],
        )?,
    );
    cli(binary, &source, &config, &["view", "switch", "dev"])?;
    cli(
        binary,
        &source,
        &config,
        &["view", "delete", "guard-child", "--force"],
    )?;
    let guard_id = observe::view_id(&source, "guard")?;
    cli(
        binary,
        &source,
        &config,
        &["view", "delete", "guard", "--force"],
    )?;
    record(
        &mut report,
        "empty_view_metadata_after_delete",
        observe::storage(&source, Some(guard_id))?,
    );

    let checks = checks::summarize(&report);
    let failures = checks.iter().filter(|c| c["status"] == "FAIL").count();
    record(&mut report, "checks", serde_json::to_value(checks)?);
    record(
        &mut report,
        "acceptance",
        json!({"status":if failures==0{"BLOCKED"}else{"FAILED"},"failed_checks":failures,"production_server_tested":false}),
    );
    Ok(report)
}
fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut binary = std::env::var_os("ATOMIC_BIN").map(PathBuf::from);
    let mut output = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--atomic-bin" => binary = Some(PathBuf::from(args.next().ok_or("missing binary")?)),
            "--out" => output = Some(PathBuf::from(args.next().ok_or("missing output path")?)),
            _ => return Err(format!("unknown argument {arg}").into()),
        }
    }
    let binary = binary
        .unwrap_or_else(|| PathBuf::from("target/debug/atomic"))
        .canonicalize()?;
    let root = if let Some(root) = output {
        if root.exists() {
            return Err("output directory must not already exist".into());
        }
        fs::create_dir_all(&root)?;
        root.canonicalize()?
    } else {
        tempfile::Builder::new()
            .prefix("atomic-replication-acceptance-")
            .tempdir()?
            .keep()
    };
    match run(&binary, &root) {
        Ok(report) => {
            let path = root.join("report.json");
            fs::write(&path, serde_json::to_vec_pretty(&report)?)?;
            println!("Acceptance observations: {}", path.display());
            for check in report["checks"].as_array().unwrap() {
                println!(
                    "{}: {}",
                    check["status"].as_str().unwrap(),
                    check["name"].as_str().unwrap()
                );
            }
            println!("Production-server validation remains BLOCKED.");
            if report["acceptance"]["failed_checks"].as_u64().unwrap_or(0) > 0 {
                // Exit nonzero for unmet acceptance, not a green observational run.
                std::process::exit(2);
            }
            Ok(())
        }
        Err(e) => {
            fs::write(root.join("failure.txt"), e.to_string())?;
            eprintln!("Fixture retained at {}", root.display());
            Err(e)
        }
    }
}
