//! A remote sandbox talks to its repository's database owner over iroh —
//! the same protocol, with every request checked against its token.
//!
//! Runs offline: the owner and the sandbox dial each other directly on
//! loopback (`ATOMIC_OWNER_IROH_OFFLINE`), no relays.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Barrier};
use std::thread;

use serde_json::Value;
use tempfile::TempDir;

fn atomic(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_atomic"))
        .args(args)
        .arg("--no-color")
        .current_dir(cwd)
        .env("ATOMIC_OWNER_IROH_OFFLINE", "1")
        .output()
        .expect("run atomic")
}

fn ok(output: Output, what: &str) -> String {
    assert!(
        output.status.success(),
        "{what} failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn failure(output: Output, what: &str) -> String {
    assert!(!output.status.success(), "{what} should have failed");
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// A repository as `atomic init` makes one (vault included), with two
/// recorded files on `dev`.
fn repository(root: &Path) {
    ok(atomic(root, &["init"]), "init");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("README.md"), "hello\n").unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    ok(atomic(root, &["add", "README.md", "src/lib.rs"]), "add");
    ok(atomic(root, &["record", "-a", "-m", "first"]), "record");
    // No owner is left running from setup.
    let _ = atomic(
        root,
        &["agent", "database-owner", "shutdown", "--repository", "."],
    );
}

struct Owner<'a>(&'a Path);

impl Drop for Owner<'_> {
    fn drop(&mut self) {
        let _ = atomic(
            self.0,
            &["agent", "database-owner", "shutdown", "--repository", "."],
        );
    }
}

fn reserve(cwd: &Path, session: &str, turn: &str) -> Output {
    atomic(
        cwd,
        &[
            "agent",
            "database-owner",
            "reserve",
            "--repository",
            ".",
            "--session-id",
            session,
            "--turn",
            turn,
            "--now",
            "1700000000",
            "--json",
        ],
    )
}

#[test]
fn a_remote_sandbox_reaches_its_view_through_the_owner_and_nothing_else() {
    let host = TempDir::new().unwrap();
    repository(host.path());
    let _owner = Owner(host.path());
    let vm = TempDir::new().unwrap();
    let vm_dir = vm.path().join("work");

    let out = ok(
        atomic(
            host.path(),
            &[
                "sandbox",
                "create",
                "exp-1",
                "--remote",
                "--view",
                "dev",
                "--acting-as",
                "did:key:zAgent",
                "--dest",
                vm_dir.to_str().unwrap(),
            ],
        ),
        "sandbox create --remote",
    );
    assert!(out.contains("Remote sandbox 'exp-1' created"), "{out}");
    let pointer_path = vm_dir.join(".atomic-sandbox");
    let pointer: Value = serde_json::from_slice(&std::fs::read(&pointer_path).unwrap()).unwrap();
    assert_eq!(pointer["view"], "dev");
    assert!(pointer["token"].as_str().unwrap().starts_with("ast_"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&pointer_path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "the pointer holds a token: owner-only");
    }

    // The view's tree, and nothing of the repository.
    let out = ok(atomic(&vm_dir, &["sandbox", "materialize"]), "materialize");
    assert!(out.contains("Materialized "), "{out}");
    assert_eq!(
        std::fs::read_to_string(vm_dir.join("README.md")).unwrap(),
        "hello\n"
    );
    assert_eq!(
        std::fs::read_to_string(vm_dir.join("src/lib.rs")).unwrap(),
        "pub fn f() {}\n"
    );
    assert!(!vm_dir.join(".atomic").exists());

    // Provenance goes over the same protocol, bound to the sandbox's view:
    // its own sessions, never the host's.
    let reserved: Value =
        serde_json::from_str(&ok(reserve(&vm_dir, "vm-session", "1"), "remote reserve")).unwrap();
    assert_eq!(reserved["committed"], true);
    ok(reserve(host.path(), "host-session", "1"), "local reserve");
    let refused = failure(
        reserve(&vm_dir, "host-session", "2"),
        "reserving the host's session",
    );
    assert!(refused.contains("forbidden"), "{refused}");
    ok(
        reserve(&vm_dir, "vm-session", "2"),
        "its own session's next turn",
    );

    // Only a local caller administers sandboxes or the owner.
    let refused = failure(
        atomic(
            &vm_dir,
            &["agent", "database-owner", "shutdown", "--repository", "."],
        ),
        "remote shutdown",
    );
    assert!(refused.contains("forbidden"), "{refused}");

    // A tampered token reaches nothing.
    let mut forged = pointer.clone();
    forged["token"] = Value::String("ast_forged".into());
    std::fs::write(&pointer_path, serde_json::to_vec(&forged).unwrap()).unwrap();
    let refused = failure(atomic(&vm_dir, &["sandbox", "materialize"]), "forged token");
    assert!(refused.contains("unauthorized"), "{refused}");
    std::fs::write(&pointer_path, serde_json::to_vec(&pointer).unwrap()).unwrap();

    // Recording in the sandbox: its cache computes the change, the owner
    // checks it and applies it to the view.
    std::fs::write(vm_dir.join("README.md"), "hello\nfrom the sandbox\n").unwrap();
    std::fs::write(vm_dir.join("src/new.rs"), "pub fn n() {}\n").unwrap();
    ok(
        atomic(&vm_dir, &["record", "-a", "-m", "from the sandbox"]),
        "record in the sandbox",
    );
    let status = ok(atomic(&vm_dir, &["status"]), "status after record");
    assert!(status.contains("working tree clean"), "{status}");
    std::fs::write(vm_dir.join("src/new.rs"), "pub fn n() { 2 }\n").unwrap();
    ok(
        atomic(&vm_dir, &["record", "-a", "-m", "again from the sandbox"]),
        "second record in the sandbox",
    );
    // Reading the recorded state works in the sandbox too: diff against
    // the view, restore from it, and the view's history.
    std::fs::write(
        vm_dir.join("README.md"),
        "hello\nfrom the sandbox\nuncommitted\n",
    )
    .unwrap();
    let diff = ok(atomic(&vm_dir, &["diff"]), "diff in the sandbox");
    assert!(
        diff.contains("+uncommitted") && !diff.contains("+hello"),
        "{diff}"
    );
    ok(
        atomic(&vm_dir, &["restore", "README.md"]),
        "restore in the sandbox",
    );
    assert_eq!(
        std::fs::read_to_string(vm_dir.join("README.md")).unwrap(),
        "hello\nfrom the sandbox\n"
    );
    let log = ok(atomic(&vm_dir, &["log"]), "log in the sandbox");
    assert!(
        log.contains("again from the sandbox") && log.contains("first"),
        "{log}"
    );

    let log = ok(atomic(host.path(), &["log"]), "host log");
    assert!(
        log.contains("from the sandbox") && log.contains("again from the sandbox"),
        "{log}"
    );

    // A second sandbox of the view sees the first one's work.
    let other = vm.path().join("other");
    ok(
        atomic(
            host.path(),
            &[
                "sandbox",
                "create",
                "exp-2",
                "--remote",
                "--view",
                "dev",
                "--dest",
                other.to_str().unwrap(),
            ],
        ),
        "second sandbox",
    );
    ok(
        atomic(&other, &["sandbox", "materialize"]),
        "materialize the second",
    );
    assert_eq!(
        std::fs::read_to_string(other.join("README.md")).unwrap(),
        "hello\nfrom the sandbox\n"
    );
    assert_eq!(
        std::fs::read_to_string(other.join("src/new.rs")).unwrap(),
        "pub fn n() { 2 }\n"
    );
    // (Minting for the same view replaced the first sandbox's token.)
    std::fs::write(
        &pointer_path,
        std::fs::read(other.join(".atomic-sandbox")).unwrap(),
    )
    .unwrap();

    // Renewal keeps the same token; closing ends it.
    let out = ok(
        atomic(host.path(), &["sandbox", "renew", "dev", "--ttl", "600"]),
        "renew",
    );
    assert!(out.contains("now expires"), "{out}");
    ok(
        atomic(&vm_dir, &["sandbox", "materialize"]),
        "materialize after renew",
    );
    let out = ok(atomic(host.path(), &["sandbox", "close", "dev"]), "close");
    assert!(out.contains("revoked"), "{out}");
    let refused = failure(
        atomic(&vm_dir, &["sandbox", "materialize"]),
        "closed sandbox",
    );
    assert!(refused.contains("unauthorized"), "{refused}");
}

/// An absurd `--ttl` used to panic inside the owner's token registry while it
/// held the lock, and every later call on that registry panicked too — so one
/// typo took down every sandbox and `close` until the owner was restarted. It
/// is a refused request, and the owner keeps serving afterwards.
#[test]
fn an_absurd_ttl_is_refused_and_the_owner_keeps_serving() {
    let host = TempDir::new().unwrap();
    repository(host.path());
    let _owner = Owner(host.path());
    let vm = TempDir::new().unwrap();
    let vm_dir = vm.path().join("work");
    ok(
        atomic(
            host.path(),
            &[
                "sandbox",
                "create",
                "exp-1",
                "--remote",
                "--view",
                "dev",
                "--dest",
                vm_dir.to_str().unwrap(),
            ],
        ),
        "sandbox create --remote",
    );
    ok(atomic(&vm_dir, &["sandbox", "materialize"]), "materialize");

    for arg in [
        "--ttl=0",
        "--ttl=-1",
        "--ttl=-9223372036854775808",
        "--ttl=10000000000000000",
    ] {
        let refused = failure(
            atomic(host.path(), &["sandbox", "renew", "dev", arg]),
            &format!("renew with {arg}"),
        );
        assert!(refused.contains("must last between"), "{arg}: {refused}");
        assert!(!refused.contains("panicked"), "{arg}: {refused}");
    }

    // The token is untouched and the owner still answers.
    ok(
        atomic(host.path(), &["sandbox", "renew", "dev", "--ttl", "600"]),
        "renew after the bad requests",
    );
    ok(
        atomic(&vm_dir, &["sandbox", "materialize"]),
        "materialize after the bad requests",
    );
    let out = ok(atomic(host.path(), &["sandbox", "close", "dev"]), "close");
    assert!(out.contains("revoked"), "{out}");
}

fn hook(cwd: &Path, verb: &str, payload: Value) -> Output {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_atomic"))
        .args(["agent", "hooks", "sherpa", verb])
        .current_dir(cwd)
        .env("ATOMIC_OWNER_IROH_OFFLINE", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("run atomic agent hooks");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn an_agent_turn_in_a_remote_sandbox_lands_with_its_provenance() {
    let host = TempDir::new().unwrap();
    repository(host.path());
    let _owner = Owner(host.path());
    let vm = TempDir::new().unwrap();
    let vm_dir = vm.path().join("work");
    ok(
        atomic(
            host.path(),
            &[
                "sandbox",
                "create",
                "exp-1",
                "--remote",
                // As an agent's work runs: its own draft view, off dev.
                "--from",
                "dev",
                "--dest",
                vm_dir.to_str().unwrap(),
            ],
        ),
        "sandbox create --remote",
    );
    ok(atomic(&vm_dir, &["sandbox", "materialize"]), "materialize");

    let cwd = vm_dir.to_str().unwrap();
    let now = "2026-01-01T00:00:00Z";
    let turn = |n: u32| {
        serde_json::json!({
            "session_id": "agent-session", "cwd": cwd, "model": "m", "provider": "p",
            "turn_number": n, "intent_title": "greet the reader", "timestamp": now,
        })
    };
    ok(
        hook(
            &vm_dir,
            "session-start",
            serde_json::json!({
                "session_id": "agent-session", "cwd": cwd, "model": "m", "provider": "p",
                "turn_number": 0, "timestamp": now,
            }),
        ),
        "session-start",
    );
    ok(hook(&vm_dir, "turn-start", turn(1)), "turn-start");
    std::fs::write(vm_dir.join("README.md"), "hello\nreader\n").unwrap();
    ok(hook(&vm_dir, "turn-end", turn(1)), "turn-end");
    ok(
        hook(
            &vm_dir,
            "session-end",
            serde_json::json!({
                "session_id": "agent-session", "cwd": cwd, "turn_number": 1, "timestamp": now,
            }),
        ),
        "session-end",
    );

    // The turn's change is on the view, and its provenance is in the
    // repository — not only in the sandbox.
    let log = ok(
        atomic(host.path(), &["log", "--view", "exp-1"]),
        "host log of the draft",
    );
    assert!(log.contains("greet the reader"), "{log}");
    let dev = ok(atomic(host.path(), &["log"]), "host log of dev");
    assert!(
        !dev.contains("greet the reader"),
        "only on the draft view: {dev}"
    );

    // An intent written in the sandbox is recorded there and lands too.
    ok(
        atomic(&vm_dir, &["intent", "new", "Greet readers"]),
        "intent new",
    );
    ok(
        atomic(&vm_dir, &["record", "-a", "-m", "the intent"]),
        "record the intent",
    );
    // ...and the repository's vault knows it, as it would after a pull.
    let intents = ok(atomic(host.path(), &["intent", "list"]), "host intent list");
    assert!(intents.contains("backlog"), "{intents}");
    let session = ok(
        atomic(host.path(), &["session", "show", "agent-session"]),
        "host session",
    );
    assert!(session.contains("Turns: 1"), "{session}");
    assert!(
        session.contains("Goal marker: greet the reader"),
        "{session}"
    );
}

/// A sandbox that falls behind must be able to record again. It used not to:
/// the cache's view row was the stale thing, and only a *successful* submit
/// refreshed it — so a `StaleView` refusal left the sandbox unable to land
/// anything ever again, and the recovery the error names ("fetch and record
/// again") did not exist anywhere. The owner now sends the view as it is with
/// the refusal, so the cache recovers from the refusal itself.
#[test]
fn a_sandbox_that_fell_behind_can_still_record() {
    let host = TempDir::new().unwrap();
    repository(host.path());
    let _owner = Owner(host.path());
    let vm = TempDir::new().unwrap();
    let vm_dir = vm.path().join("work");
    ok(
        atomic(
            host.path(),
            &[
                "sandbox",
                "create",
                "exp-1",
                "--remote",
                "--from",
                "dev",
                "--dest",
                vm_dir.to_str().unwrap(),
            ],
        ),
        "sandbox create --remote",
    );
    ok(atomic(&vm_dir, &["sandbox", "materialize"]), "materialize");

    // The sandbox records and it lands.
    std::fs::write(vm_dir.join("README.md"), "hello\nsandbox\n").unwrap();
    ok(
        atomic(&vm_dir, &["record", "-a", "-m", "from the sandbox"]),
        "record in the sandbox",
    );
    let log = ok(
        atomic(host.path(), &["log", "--view", "exp-1"]),
        "host log after the sandbox's record",
    );
    assert!(log.contains("from the sandbox"), "{log}");

    // Move the draft view underneath the sandbox, with no sandbox involved:
    // the owner is the user's live repository, so a local record on `dev` and
    // an insert into the draft is exactly what a second agent would cause.
    std::fs::write(host.path().join("OUT-OF-BAND.md"), "elsewhere\n").unwrap();
    ok(atomic(host.path(), &["add", "OUT-OF-BAND.md"]), "host add");
    ok(
        atomic(host.path(), &["record", "-a", "-m", "out of band"]),
        "host record",
    );
    ok(
        atomic(
            host.path(),
            &["insert", "from-view", "dev", "--to-view", "exp-1"],
        ),
        "host insert into the draft",
    );

    // The sandbox's next record was computed against the view it no longer
    // has, so the owner refuses it — and the refusal carries the current view,
    // which the cache takes.
    std::fs::write(vm_dir.join("README.md"), "hello\nsandbox\nout of date\n").unwrap();
    let refused = failure(
        atomic(&vm_dir, &["record", "-a", "-m", "against the old view"]),
        "a record against a view that moved on",
    );
    assert!(refused.contains("moved on"), "{refused}");

    // And that is where the old behaviour ended: the cache's view row was
    // still the stale one, so the next record computed the same stale base and
    // was refused identically. Now the retry works.
    let retried = ok(
        atomic(&vm_dir, &["record", "-a", "-m", "after the view moved"]),
        "retry in the sandbox after the view moved",
    );
    assert!(!retried.contains("moved on"), "{retried}");

    let log = ok(
        atomic(host.path(), &["log", "--view", "exp-1"]),
        "host log after the retry",
    );
    assert!(log.contains("after the view moved"), "{log}");
    // The refused change is not on the view, under either message.
    assert!(!log.contains("against the old view"), "{log}");
}

/// Several sandboxes at once against one owner: each on its own draft view, all
/// materializing, publishing provenance and recording at the same time.
///
/// This is the shape the owner's concurrency has to survive. A view has one
/// live token, so sandboxes cannot share a view and each gets its own draft off
/// `dev`. Everything after that contends: one owner process, one `submissions`
/// mutex held across the insert, a redb writer only one caller may hold, and a
/// database the owner reopens per request. The assertions are all about
/// *separation* — every change on the draft that made it, none on any other —
/// because a shared owner can cross wires without erroring.
#[test]
fn several_remote_sandboxes_work_at_once() {
    const SANDBOXES: usize = 20;

    let host = TempDir::new().unwrap();
    repository(host.path());
    let _owner = Owner(host.path());

    // Zero-padded: "recorded by sb-1" is a substring of "recorded by sb-10",
    // and a substring test across a set like this quietly passes nothing.
    let names: Vec<String> = (0..SANDBOXES).map(|i| format!("sb-{i:02}")).collect();
    let dirs: Vec<TempDir> = names.iter().map(|_| TempDir::new().unwrap()).collect();
    let works: Vec<PathBuf> = dirs.iter().map(|d| d.path().join("work")).collect();

    for (name, work) in names.iter().zip(&works) {
        ok(
            atomic(
                host.path(),
                &[
                    "sandbox",
                    "create",
                    name,
                    "--remote",
                    "--from",
                    "dev",
                    "--dest",
                    work.to_str().unwrap(),
                ],
            ),
            &format!("sandbox create {name}"),
        );
    }

    let gate = Arc::new(Barrier::new(SANDBOXES));
    let mut sandboxes = Vec::new();
    for (i, (name, work)) in names.iter().zip(&works).enumerate() {
        let gate = Arc::clone(&gate);
        let (name, work) = (name.clone(), work.clone());
        sandboxes.push(thread::spawn(move || {
            let now = "2026-01-01T00:00:00Z";
            let session = format!("session-{i:02}");
            let turn = |n: u32| {
                serde_json::json!({
                    "session_id": session, "cwd": work.to_str().unwrap(), "model": "m",
                    "provider": "p", "turn_number": n,
                    "intent_title": format!("work on {name}"), "timestamp": now,
                })
            };

            // From here on they contend: one owner, one database, one writer.
            gate.wait();

            ok(
                atomic(&work, &["sandbox", "materialize"]),
                &format!("{name} materialize"),
            );
            // Each sandbox's own session. A sandbox may only touch its own, so
            // this is the owner's session check under load.
            ok(
                hook(
                    &work,
                    "session-start",
                    serde_json::json!({
                        "session_id": session, "cwd": work.to_str().unwrap(), "model": "m",
                        "provider": "p", "turn_number": 0, "timestamp": now,
                    }),
                ),
                &format!("{name} session-start"),
            );
            ok(
                hook(&work, "turn-start", turn(1)),
                &format!("{name} turn-start"),
            );

            // Each edits a file only it touches, so a crossed change shows up
            // as another sandbox's content on this draft.
            std::fs::write(work.join("README.md"), format!("hello\nfrom {name}\n")).unwrap();
            std::fs::write(
                work.join(format!("{name}.txt")),
                format!("only {name} wrote this\n"),
            )
            .unwrap();
            ok(
                atomic(&work, &["add", &format!("{name}.txt")]),
                &format!("{name} add"),
            );
            ok(
                atomic(
                    &work,
                    &["record", "-a", "-m", &format!("recorded by {name}")],
                ),
                &format!("{name} record"),
            );
            ok(
                hook(&work, "turn-end", turn(1)),
                &format!("{name} turn-end"),
            );
            ok(
                hook(
                    &work,
                    "session-end",
                    serde_json::json!({
                        "session_id": session, "cwd": work.to_str().unwrap(),
                        "turn_number": 1, "timestamp": now,
                    }),
                ),
                &format!("{name} session-end"),
            );
            session
        }));
    }

    let sessions: Vec<String> = sandboxes
        .into_iter()
        .map(|s| s.join().expect("a sandbox thread finished"))
        .collect();

    // Every draft has its own sandbox's change, and its provenance.
    for (i, name) in names.iter().enumerate() {
        let log = ok(
            atomic(host.path(), &["log", "--view", name]),
            &format!("host log of {name}"),
        );
        assert!(
            log.contains(&format!("recorded by {name}")),
            "{name}: {log}"
        );

        let shown = ok(
            atomic(host.path(), &["session", "show", &sessions[i]]),
            &format!("host session {}", sessions[i]),
        );
        assert!(shown.contains("Turns: 1"), "{}: {shown}", sessions[i]);
        assert!(
            shown.contains(&format!("Goal marker: work on {name}")),
            "{}: {shown}",
            sessions[i]
        );
    }

    // ...and nothing of any other sandbox's. This is the assertion that fails
    // if a submit were applied to the wrong view.
    for name in &names {
        let log = ok(
            atomic(host.path(), &["log", "--view", name]),
            &format!("host log of {name}"),
        );
        for other in &names {
            if other != name {
                assert!(
                    !log.contains(&format!("recorded by {other}")),
                    "{name} has {other}'s change: {log}"
                );
            }
        }
    }

    // The shared base is untouched: no draft's work reached `dev`.
    let dev = ok(atomic(host.path(), &["log"]), "host log of dev");
    for name in &names {
        assert!(
            !dev.contains(&format!("recorded by {name}")),
            "a draft's work reached dev: {dev}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(host.path().join("README.md")).unwrap(),
        "hello\n",
        "dev's working tree was rewritten by a sandbox"
    );
}

/// A second record from each sandbox, on a repository several of them are
/// working on. This is a known failure, kept here because the stress test above
/// found it and it should be run the moment it is fixed.
///
/// A remote cache allocates node ids from a counter seeded *only* by the ids
/// the owner chose to send it — `import_ids` takes `fetch_max` over the ids in
/// one import, and a cache is only told the ids for the files it asks about.
/// Instrumented on a four-sandbox run, the owner sent 5 ids with a maximum of
/// 7, leaving each cache's counter at 8 while the repository's own counter was
/// well past that. The cache then mints ids for its new content that are
/// already real repository node ids, and the owner refuses the change with
/// `node N is not on this view`: the agent's work is lost, with no way forward.
///
/// Parallelism is not the cause, only what makes it reliable — it races several
/// caches through the same import so they all record from a stale counter. A
/// single sandbox passes whenever the repository happens to have no node at the
/// ids that cache lands on, which is why no existing test saw it.
///
/// The fix is the one `LOCAL_INODE_FLOOR` already applies to inodes: a cache's
/// node ids must come from a range the repository will never hand out, and the
/// owner must remap them into the repository's space on insert rather than
/// applying the submitted change's ids verbatim.
#[test]
#[ignore = "a remote cache's node ids collide with the repository's; see above"]
fn sandboxes_can_each_record_twice() {
    const SANDBOXES: usize = 20;

    let host = TempDir::new().unwrap();
    repository(host.path());
    let _owner = Owner(host.path());

    // Zero-padded: "recorded by sb-1" is a substring of "recorded by sb-10",
    // and a substring test across a set like this quietly passes nothing.
    let names: Vec<String> = (0..SANDBOXES).map(|i| format!("sb-{i:02}")).collect();
    let dirs: Vec<TempDir> = names.iter().map(|_| TempDir::new().unwrap()).collect();
    let works: Vec<PathBuf> = dirs.iter().map(|d| d.path().join("work")).collect();
    for (name, work) in names.iter().zip(&works) {
        ok(
            atomic(
                host.path(),
                &[
                    "sandbox",
                    "create",
                    name,
                    "--remote",
                    "--from",
                    "dev",
                    "--dest",
                    work.to_str().unwrap(),
                ],
            ),
            &format!("sandbox create {name}"),
        );
    }

    let gate = Arc::new(Barrier::new(SANDBOXES));
    let mut sandboxes = Vec::new();
    for (name, work) in names.iter().zip(&works) {
        let gate = Arc::clone(&gate);
        let (name, work) = (name.clone(), work.clone());
        sandboxes.push(thread::spawn(move || {
            gate.wait();
            ok(
                atomic(&work, &["sandbox", "materialize"]),
                &format!("{name} materialize"),
            );
            std::fs::write(work.join("README.md"), format!("hello\nfrom {name}\n")).unwrap();
            ok(
                atomic(&work, &["record", "-a", "-m", &format!("first by {name}")]),
                &format!("{name} first record"),
            );
            // The one that fails: the cache mints ids from its stale counter.
            std::fs::write(
                work.join("README.md"),
                format!("hello\nfrom {name}\nmore\n"),
            )
            .unwrap();
            ok(
                atomic(&work, &["record", "-a", "-m", &format!("second by {name}")]),
                &format!("{name} second record: the cache must not reuse a repository node id"),
            );
        }));
    }
    for s in sandboxes {
        s.join().expect("a sandbox thread finished");
    }

    for name in &names {
        let log = ok(
            atomic(host.path(), &["log", "--view", name]),
            &format!("host log of {name}"),
        );
        assert!(log.contains(&format!("first by {name}")), "{name}: {log}");
        assert!(log.contains(&format!("second by {name}")), "{name}: {log}");
    }
}

/// A sandbox and the local repository working the *same* view at the same
/// time, which the `submissions` mutex does not cover: that mutex serializes
/// requests *through the owner*, and a local `atomic record` never goes through
/// it. Both write the view, so one is always computed against a state the other
/// has just replaced.
///
/// This is a known failure, kept because the stress test found it and it should
/// run the moment it is fixed. Two things are wrong, and the second is data
/// loss.
///
/// The first is fixed and this test is what proves it. The sandbox loses every
/// round — three of three in practice — and retries until it wins, because the
/// owner refuses the stale change *and* sends the view as it is now. Before
/// that, a stale refusal left the sandbox unable to record ever again.
///
/// The second is open. When the owner applies a sandbox's change to the view it
/// does not update a *local* working tree on that view, so a local tree that is
/// behind reads the view's new files as local deletions:
///
/// ```text
/// $ atomic status
/// On view dev
/// Changes to be recorded:
///     deleted:   from-sandbox.txt
/// ```
///
/// `from-sandbox.txt` is a file the sandbox had just added, and it is present
/// on the view — but the host's tree does not have it, so `status` offers to
/// delete it. The host's next `record -a` takes that offer: in this test the
/// host's `record -a` consumed the sandbox's file on every round, so by the
/// end the view had the host's *and* the sandbox's changes in its log and
/// neither writer's files in its content.
///
/// The fix is for a local working tree that is behind its view to be brought
/// up to date before status reports on it, or for `status` to recognise that
/// the view moved underneath and say so rather than presenting the difference
/// as local changes. Until then, a human and an agent on one view cannot both
/// `record -a` without eating each other's work.
#[test]
#[ignore = "a local tree behind its view reports the view's new files as deletions"]
fn a_sandbox_and_the_local_repository_can_share_a_view() {
    const ROUNDS: usize = 3;
    const MAX_RETRIES: usize = 12;

    let host = TempDir::new().unwrap();
    repository(host.path());
    let _owner = Owner(host.path());
    let vm = TempDir::new().unwrap();
    let work = vm.path().join("work");
    // On `dev` itself, not a draft: this is the shared-view case.
    ok(
        atomic(
            host.path(),
            &[
                "sandbox",
                "create",
                "shared",
                "--remote",
                "--view",
                "dev",
                "--dest",
                work.to_str().unwrap(),
            ],
        ),
        "sandbox create",
    );
    ok(atomic(&work, &["sandbox", "materialize"]), "materialize");

    // How many rounds the sandbox lost and had to retry. Not asserted on:
    // whether it loses depends on the scheduler and both outcomes are correct.
    // What matters is that losing never became permanent.
    let mut lost = 0usize;
    for round in 0..ROUNDS {
        let gate = Arc::new(Barrier::new(2));

        let sandbox = {
            let gate = Arc::clone(&gate);
            let work = work.clone();
            thread::spawn(move || {
                gate.wait();
                std::fs::write(work.join("from-sandbox.txt"), format!("round {round}\n")).unwrap();
                let mut attempts = 0;
                loop {
                    let add = atomic(&work, &["add", "from-sandbox.txt"]);
                    assert!(
                        add.status.success(),
                        "sandbox add: {}{}",
                        String::from_utf8_lossy(&add.stdout),
                        String::from_utf8_lossy(&add.stderr)
                    );
                    let out = atomic(
                        &work,
                        &["record", "-a", "-m", &format!("sandbox round {round}")],
                    );
                    let text = format!(
                        "{}{}",
                        String::from_utf8_lossy(&out.stdout),
                        String::from_utf8_lossy(&out.stderr)
                    );
                    if out.status.success() {
                        return attempts;
                    }
                    // A stale refusal is the expected outcome of losing the
                    // race, and the cache has just resynced from it.
                    assert!(
                        text.contains("moved on"),
                        "sandbox round {round} failed for a different reason: {text}"
                    );
                    attempts += 1;
                    assert!(
                        attempts < MAX_RETRIES,
                        "sandbox never got past the moving view in {MAX_RETRIES} tries: {text}"
                    );
                }
            })
        };

        let local = {
            let gate = Arc::clone(&gate);
            let host_root = host.path().to_path_buf();
            thread::spawn(move || {
                gate.wait();
                std::fs::write(host_root.join("from-host.txt"), format!("round {round}\n"))
                    .unwrap();
                let add = atomic(&host_root, &["add", "from-host.txt"]);
                assert!(add.status.success(), "host add");
                let out = atomic(
                    &host_root,
                    &["record", "-a", "-m", &format!("host round {round}")],
                );
                assert!(
                    out.status.success(),
                    "host record round {round}: {}{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                );
            })
        };

        let attempts = sandbox.join().expect("sandbox thread finished");
        local.join().expect("local thread finished");
        if attempts > 0 {
            lost += 1;
        }
    }
    eprintln!("DIAG rounds the sandbox lost and retried: {lost} of {ROUNDS}");

    // Both writers' changes are on the view, whatever order they arrived in.
    let log = ok(atomic(host.path(), &["log"]), "host log of dev");
    for round in 0..ROUNDS {
        assert!(
            log.contains(&format!("host round {round}")),
            "the host's round {round} is missing: {log}"
        );
        assert!(
            log.contains(&format!("sandbox round {round}")),
            "the sandbox's round {round} is missing: {log}"
        );
    }

    // And the view's *content* has both writers' files: the changes merged
    // rather than one side eating the other. This is the assertion that fails
    // today, because the host's `record -a` committed the deletion of the file
    // the sandbox had just added.
    let check = vm.path().join("check");
    ok(
        atomic(
            host.path(),
            &[
                "sandbox",
                "create",
                "check",
                "--remote",
                "--view",
                "dev",
                "--dest",
                check.to_str().unwrap(),
            ],
        ),
        "sandbox create for the check",
    );
    ok(
        atomic(&check, &["sandbox", "materialize"]),
        "materialize the check",
    );
    for (file, who) in [("from-sandbox.txt", "sandbox"), ("from-host.txt", "host")] {
        assert_eq!(
            std::fs::read_to_string(check.join(file)).unwrap_or_else(|e| {
                panic!(
                    "the {who}'s {file} is not on the view: {e}\n{}",
                    ok(atomic(&check, &["status"]), "status")
                )
            }),
            format!("round {}\n", ROUNDS - 1),
            "the {who}'s {file} has the wrong content on the view",
        );
    }
}
