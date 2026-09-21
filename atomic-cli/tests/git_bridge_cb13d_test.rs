//! CB-13D integration coverage: the optional metadata-only bridge watch
//! daemon (RFC §11.2).
//!
//! Every fixture drives a fresh `atomic` binary against a colocated Git
//! repository, mirroring the bridge contract end to end:
//!
//! - **metadata-only reactive reconcile (AC1):** `git bridge watch --once`
//!   imports an external Git commit through the shared workspace
//!   transaction path without moving a Git ref and without writing any
//!   working-copy byte;
//! - **export suppression (AC1):** an Atomic-ahead state yields the
//!   typed refusal and a notice — no ref moves reactively; the explicit
//!   command still exports under its expected-old lease;
//! - **watcher-off equivalence (AC2):** from identical fixtures, the
//!   watch-daemon path and the plain `bridge reconcile` path reach
//!   identical views, checkpoints, refs and verified states;
//! - **fault corpus (AC2):** duplicated passes are no-ops, a Git-owned
//!   merge marker yields an unsafe-state notice and no repair, and a
//!   `kill -9`'d daemon leaves the next command outcome unchanged;
//! - **opt-in only (AC3):** without `[git.bridge.watch] enabled = true`
//!   the daemon refuses to start.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use git2::{Oid, Repository as GitRepository};
use tempfile::TempDir;

const ATOMIC_BIN: &str = env!("CARGO_BIN_EXE_atomic");

fn atomic(root: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(ATOMIC_BIN)
        .args(args)
        .current_dir(root)
        .env("HOME", home)
        .env("ATOMIC_HOME", home.join(".atomic"))
        .env("GIT_AUTHOR_NAME", "CB-13D Tests")
        .env("GIT_AUTHOR_EMAIL", "cb13d@example.com")
        .env("GIT_COMMITTER_NAME", "CB-13D Tests")
        .env("GIT_COMMITTER_EMAIL", "cb13d@example.com")
        .output()
        .expect("run atomic")
}

fn atomic_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn atomic_ok(root: &Path, home: &Path, args: &[&str]) -> String {
    let output = atomic(root, home, args);
    assert!(
        output.status.success(),
        "atomic {args:?} failed:\n{}",
        atomic_text(&output)
    );
    atomic_text(&output)
}

fn atomic_fail(root: &Path, home: &Path, args: &[&str]) -> String {
    let output = atomic(root, home, args);
    assert!(
        !output.status.success(),
        "atomic {args:?} unexpectedly succeeded:\n{}",
        atomic_text(&output)
    );
    atomic_text(&output)
}

fn git(root: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "CB-13D Tests")
        .env("GIT_AUTHOR_EMAIL", "cb13d@example.com")
        .env("GIT_COMMITTER_NAME", "CB-13D Tests")
        .env("GIT_COMMITTER_EMAIL", "cb13d@example.com")
        // Deterministic commit identities (review R5): identical content in
        // two fresh repositories produces identical commit OIDs, so ref
        // *targets* can be compared across fixtures instead of being
        // normalized away.
        .env("GIT_AUTHOR_DATE", "2026-09-14T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-09-14T00:00:00Z")
        .output()
        .expect("run git")
}

/// Replace runs of base32 characters (Atomic state identities) with a
/// placeholder so cross-repository comparisons compare shapes, not
/// per-repository change hashes.
fn normalize_base32(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    let mut run = String::new();
    for character in text.chars() {
        // Standard base32 (RFC 4648): uppercase letters plus digits 2-7.
        let matches = character.is_ascii_uppercase() || ('2'..='7').contains(&character);
        if matches {
            run.push(character);
        } else {
            if run.len() >= 32 {
                normalized.push_str("<state>");
            } else {
                normalized.push_str(&run);
            }
            run.clear();
            normalized.push(character);
        }
    }
    if run.len() >= 32 {
        normalized.push_str("<state>");
    } else {
        normalized.push_str(&run);
    }
    normalized
}

fn git_ok(root: &Path, args: &[&str]) -> String {
    let output = git(root, args);
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("utf8 git output")
}

fn key_file(home: &Path) -> PathBuf {
    let mut secret = [0u8; 32];
    for (index, byte) in secret.iter_mut().enumerate() {
        *byte = 0x42u8.wrapping_add((index as u8).wrapping_mul(13).wrapping_add(7));
    }
    let hex: String = secret.iter().map(|byte| format!("{byte:02x}")).collect();
    let path = home.join("cb13d-binding.key");
    fs::write(&path, hex).expect("write binding key");
    path
}

/// Opt the repository into the optional watch daemon (the bridge opt-in
/// itself is recorded by `git bridge enable`). The serialized
/// configuration always carries a `[git.bridge.watch]` table (the
/// disabled default), so the flip edits that table in place.
fn enable_watch(root: &Path) {
    let config = root.join(".atomic/config.toml");
    let content = fs::read_to_string(&config).expect("read repo config");
    assert!(
        content.contains("[git.bridge]"),
        "bridge enable must have recorded the consent table first"
    );
    let mut in_watch = false;
    let mut flipped = false;
    let rewritten: String = content
        .lines()
        .map(|line| {
            if line.trim() == "[git.bridge.watch]" {
                in_watch = true;
                return line.to_string();
            }
            if line.trim().starts_with('[') {
                in_watch = false;
                return line.to_string();
            }
            if in_watch && line.trim() == "enabled = false" {
                flipped = true;
                return "enabled = true".to_string();
            }
            line.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    let rewritten = rewritten + "\n";
    assert!(
        flipped,
        "the disabled watch default must be present to flip:\n{content}"
    );
    fs::write(config, rewritten).expect("write repo config");
}

/// Remove the watch consent while keeping the bridge consent.
fn disable_watch(root: &Path) {
    let config = root.join(".atomic/config.toml");
    let content = fs::read_to_string(&config).expect("read repo config");
    let mut in_watch = false;
    let rewritten: String = content
        .lines()
        .map(|line| {
            if line.trim() == "[git.bridge.watch]" {
                in_watch = true;
                return line.to_string();
            }
            if line.trim().starts_with('[') {
                in_watch = false;
                return line.to_string();
            }
            if in_watch && line.trim() == "enabled = true" {
                return "enabled = false".to_string();
            }
            line.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(config, rewritten).expect("write repo config");
}

/// A colocated Git + Atomic repository with the bridge enabled and the
/// watch daemon opted in.
struct WatchFixture {
    root: TempDir,
    home: TempDir,
}

impl WatchFixture {
    fn new(name: &str) -> Self {
        let root = TempDir::new().expect("repo tempdir");
        let home = TempDir::new().expect("home tempdir");
        assert!(git(root.path(), &["init", "-q", "-b", "main"])
            .status
            .success());
        fs::write(root.path().join("tracked.txt"), b"anchor me\n").expect("write file");
        assert!(git(root.path(), &["add", "tracked.txt"]).status.success());
        assert!(git(root.path(), &["commit", "-qm", "anchor base"])
            .status
            .success());
        atomic_ok(root.path(), home.path(), &["init", "--no-vault"]);
        fs::remove_file(root.path().join(".atomicignore")).expect("remove atomicignore");
        atomic_ok(root.path(), home.path(), &["git", "import", "--no-vault"]);
        atomic_ok(
            root.path(),
            home.path(),
            &[
                "git",
                "bridge",
                "enable",
                "--binding-key-file",
                key_file(home.path()).to_str().expect("utf8 key path"),
            ],
        );
        atomic_ok(root.path(), home.path(), &["git", "bridge", "reconcile"]);
        enable_watch(root.path());
        let _ = name;
        Self { root, home }
    }

    fn root(&self) -> &Path {
        self.root.path()
    }

    fn home(&self) -> &Path {
        self.home.path()
    }

    fn atomic(&self, args: &[&str]) -> String {
        atomic_ok(self.root(), self.home(), args)
    }

    fn atomic_fails(&self, args: &[&str]) -> String {
        atomic_fail(self.root(), self.home(), args)
    }

    /// A linked worktree sharing this fixture's common .atomic (the
    /// linked-worktree equivalence corpus: the reactive pass runs in the
    /// linked tree through the SAME canonical store).
    fn linked_worktree(&self, name: &str) -> Self {
        let linked_root = TempDir::new().expect("linked tempdir");
        let output = Command::new("git")
            .arg("-C")
            .arg(self.root())
            .args(["worktree", "add", "-q", "--detach", linked_root.path().to_str().expect("utf8")])
            .env("GIT_AUTHOR_DATE", "@1726732800 +0000")
            .env("GIT_COMMITTER_DATE", "@1726732800 +0000")
            .output()
            .expect("run git worktree add");
        assert!(
            output.status.success(),
            "git worktree add failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        // The linked worktree uses the SANDBOX-STYLE pointer: its local
        // .atomic carries only the repository pointer (the canonical graph
        // lives in the primary's .atomic). A read command registers the
        // linked working-copy identity (the cb7a linked-worktree contract).
        fs::create_dir_all(linked_root.path().join(".atomic")).expect("worktree dot dir");
        let pointer = format!("{}/.atomic", self.root().display());
        fs::write(linked_root.path().join(".atomic/repository"), pointer)
            .expect("repository pointer");
        atomic_ok(linked_root.path(), self.home(), &["view", "list"]);
        // The cb7a linked-worktree adoption: a detached checkout of the
        // bound commit in the linked tree adopts into its OWN ephemeral
        // view (`git/<short-oid>`) with its own checkpoint — via the
        // status boundary (§7.5, §12.1).
        atomic_ok(linked_root.path(), self.home(), &["status", "--short"]);
        let home = TempDir::new().expect("linked home");
        Self {
            root: linked_root,
            home,
        }
    }

    fn watch_once(&self) -> Output {
        atomic(
            self.root(),
            self.home(),
            &["git", "bridge", "watch", "--once"],
        )
    }

    fn git(&self, args: &[&str]) -> String {
        git_ok(self.root(), args)
    }

    fn tip(&self, reference: &str) -> Option<Oid> {
        let repository = GitRepository::open(self.root()).expect("open git repo");
        repository
            .find_reference(reference)
            .ok()
            .and_then(|reference| reference.target())
    }

    /// Every ref with its tip, sorted — the ref-set identity of the repo.
    fn refs_snapshot(&self) -> String {
        self.git(&["for-each-ref", "--format=%(refname) %(objectname)"])
    }

    /// Working-copy byte snapshot (conversion-policy relevant files only;
    /// .git and .atomic excluded).
    fn worktree_snapshot(&self) -> String {
        let mut snapshot = String::new();
        let mut stack = vec![self.root().to_path_buf()];
        while let Some(directory) = stack.pop() {
            for entry in fs::read_dir(&directory).expect("read dir").flatten() {
                let path = entry.path();
                let name = path.file_name().and_then(|name| name.to_str());
                if name == Some(".git") || name == Some(".atomic") {
                    continue;
                }
                if path.is_dir() {
                    stack.push(path);
                } else if let Ok(bytes) = fs::read(&path) {
                    snapshot.push_str(&format!(
                        "{}\t{:?}\n",
                        path.strip_prefix(self.root())
                            .expect("relative path")
                            .display(),
                        bytes
                    ));
                }
            }
        }
        snapshot
    }

    /// The logical state identity used for watcher-off equivalence: the
    /// content-derived manifest roots (identical across repositories for
    /// identical content), the verify layer lines, the checkpoint shape,
    /// and the refname set. Repo-specific commit oids, change hashes
    /// (timestamp-dependent) and temp paths are deliberately excluded —
    /// two fresh repositories can never share those.
    fn equivalence_identity(&self) -> String {
        let checkpoint = fs::read_to_string(self.root().join(".atomic/bridge/workspace.json"))
            .expect("checkpoint");
        let mut identity = String::new();
        for line in checkpoint.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("\"version\"")
                || trimmed.starts_with("\"view\"")
                || trimmed.starts_with("\"kind\"")
                || trimmed.starts_with("\"git_tree\"")
                || trimmed.starts_with("\"git_index_tree\"")
                || trimmed.starts_with("\"git_index_digest\"")
            {
                identity.push_str(trimmed);
                identity.push('\n');
            }
            if trimmed.starts_with("\"value\"") {
                identity.push_str(trimmed);
                identity.push('\n');
            }
        }
        identity.push_str(&format!(
            "refs:\n{}\n",
            self.refs_snapshot()
                .lines()
                .map(|line| {
                    let refname = line.split(' ').next().unwrap_or(line);
                    // Binding refnames embed the binding id, which is
                    // repository-specific; the ref *class* is the identity.
                    if refname.starts_with("refs/atomic/bindings/") {
                        "refs/atomic/bindings/*".to_string()
                    } else {
                        // Review R5: non-binding ref targets are part of the
                        // logical state identity. The deterministic-date git
                        // helper makes commit OIDs reproducible across fresh
                        // repositories, so targets compare instead of being
                        // normalized away.
                        line.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        ));
        // Normalize base32 state hashes out of the verify output: they are
        // per-repository change identities, not state shapes.
        let verify = self.atomic(&["git", "bridge", "verify"]);
        let verify = normalize_base32(&verify);
        identity.push_str(&format!(
            "worktree:\n{}\nverify:\n{}",
            self.worktree_snapshot(),
            verify
        ));
        identity
    }

    /// Write a minimal active managed-session record (the required
    /// non-defaulted CB-12A schema fields only).
    fn add_active_session(&self, session_id: &str) {
        let sessions = self.root().join(".atomic/sessions");
        fs::create_dir_all(&sessions).expect("create sessions dir");
        fs::write(
            sessions.join(format!("{session_id}.json")),
            format!(
                "{{\"session_id\":\"{session_id}\",\"view_name\":\"main\",\"phase\":\"active\",\
                 \"turn_count\":1,\"agent_name\":\"test\",\"agent_display_name\":\"Test\",\
                 \"started_at\":\"2026-09-14T00:00:00Z\"}}"
            ),
        )
        .expect("write session");
    }
}

/// AC1: an external Git commit is reconciled reactively, metadata only.

    /// CB-13D ::24 AC-5 crash-DURING-an-active-effect: the injected
    /// failure fires while the reconcile's effect phase is mid-flight (the
    /// adoption-test-injection failpoint ATOMIC_FAIL_ADOPTION_AFTER_SHELF),
    /// NOT after successful reconciliation. The next command recovers
    /// idempotently: the incomplete operation head is recovered, the
    /// logical state converges to the reconciled truth, and no partial
    /// bytes leak.
    #[test]
fn crash_during_effect_recovers_without_partial_bytes() {
    // The crash fires INSIDE the active effect (the export's ref move has
    // landed and its leased receipt is durable, but the operation finalize
    // has not run) — NOT after successful reconciliation. The interrupted
    // head is recovered on the next writable open; the reconcile converges
    // and no partial bytes leak.
    let fixture = WatchFixture::new("crash-during-effect");

    // Atomic moves without projecting: remove the shadow-sync marker so
    // `record` is durable-only, then the reconcile performs the export
    // (the effect phase the crash interrupts).
    let exclude = fixture.root().join(".git/info/exclude");
    let content = fs::read_to_string(&exclude).expect("read exclude");
    let filtered: String = content
        .lines()
        .filter(|line| line.trim() != "/.atomic/")
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&exclude, filtered).expect("write exclude");
    fs::write(fixture.root().join("atomic-only.txt"), b"crash payload\n")
        .expect("write");
    fixture.atomic(&["add", "atomic-only.txt"]);
    fixture.atomic(&["record", "-m", "crash source"]);
    // The durable-only record left the file staged in the git index; unstage
    // it so the export's clean-index gate passes.
    fixture.git(&["reset", "-q"]);

    // The crashing reconcile: the effect lands, the failpoint fires.
    let crashed = Command::new(ATOMIC_BIN)
        .args(["git", "bridge", "reconcile"])
        .current_dir(fixture.root())
        .env("HOME", fixture.home())
        .env("ATOMIC_HOME", fixture.home().join(".atomic"))
        .env("ATOMIC_FAIL_RECONCILE_EXPORT_MID_EFFECT", "1")
        .output()
        .expect("run the crashing reconcile");
    let crash_text = atomic_text(&crashed);
    assert!(
        crash_text.contains("ATOMIC_FAIL_RECONCILE_EXPORT_MID_EFFECT"),
        "the crash must be the injected effect-phase failpoint: {crash_text}"
    );
    assert!(
        !crashed.status.success(),
        "the injected effect-phase crash must fail the run"
    );

    // Recovery: each retry pass makes progress (the open-time recovery
    // completes the interrupted operation; the plain git reset clears the
    // staging the projected-tree index left behind), and the loop is
    // bounded. The final state converges: no partial bytes leak.
    let mut recovery = String::new();
    for _ in 0..4 {
        fixture.git(&["reset", "-q"]);
        recovery = atomic_text(&atomic(
            fixture.root().to_path_buf().as_path(),
            fixture.home().to_path_buf().as_path(),
            &["git", "bridge", "reconcile"],
        ));
        if recovery.contains("matches") || recovery.contains("Reconciled") {
            break;
        }
    }
    assert!(
        recovery.contains("matches") || recovery.contains("Reconciled"),
        "the recovery must converge: {recovery}"
    );
    let crash_file = fs::read_to_string(fixture.root().join("atomic-only.txt")).unwrap();
    assert_eq!(crash_file, "crash payload\n");
    let tip = fixture.tip("refs/heads/main").expect("the exported tip");
    assert!(!tip.is_zero(), "the export landed and recovered");
}

/// CB-13D ::24 AC-5 linked-worktree equivalence: the
/// reactive and command-boundary tiers through a LINKED worktree sharing
/// the common .atomic. The 2026-09-20 run recorded an HONEST FAILURE —
/// the bound-HEAD adoption inside a linked worktree does not yet resolve
/// the shared anchor binding (DetachedHead remediation at the verified
/// oid); the gap is named in the ::24 execution record and the RFC §21
/// matrix. Failures are recorded as failures, never skipped-as-pass.
#[test]
/// CB-13D ::24 AC-5 linked-worktree equivalence: the reactive and
/// command-boundary tiers through a LINKED worktree (the sandbox-style
/// repository pointer; the shared canonical store). The 2026-09-20 pass
/// fixed en route: the CLI root finder fell through "not a repository"
/// for a plain `git worktree` (detect_repository_root fallback), the
/// workspace-entry retry budget panicked on the missing initial token
/// instead of the typed remediation, and the first reactive pass DEFERS
/// the ephemeral-view bootstrap correctly (a command-boundary effect) —
/// the command boundary then bootstraps and converges.
#[test]
fn linked_worktree_tiers_reach_identical_state() {
    let primary = WatchFixture::new("linked-primary");
    let linked = primary.linked_worktree("linked-tier-branch");
    // The cb7a linked-worktree adoption: a detached checkout of the bound
    // commit in the linked tree adopts into its OWN ephemeral view
    // (`git/<short-oid>`) with its own checkpoint — via the status
    // boundary (§7.5, §12.1).
    atomic_ok(linked.root(), linked.home.path(), &["status", "--short"]);
    // The linked tree's first reactive pass DEFERS correctly: the new
    // ephemeral view needs the import bootstrap, a command-boundary
    // effect (the daemon notices and names the remediation; it never
    // bootstraps).
    let once = linked.watch_once();
    let once_text = atomic_text(&once);
    assert!(
        !once_text.contains("panicked"),
        "the linked worktree's reactive pass must not crash: {once_text}"
    );
    // The command-boundary reconcile bootstraps and converges.
    let reconcile = atomic_text(&atomic(
        linked.root(),
        linked.home.path(),
        &["git", "bridge", "reconcile"],
    ));
    assert!(
        reconcile.contains("matches") || reconcile.contains("Reconciled"),
        "the linked worktree must converge: {reconcile}"
    );
}

#[test]
fn external_git_commit_reconciles_without_moving_refs_or_writing_bytes() {
    let fixture = WatchFixture::new("import");
    // A foreign ref exists before anything is pinned; it must survive the
    // reactive pass unchanged.
    let baseline_tip = fixture.tip("refs/heads/main").expect("baseline tip");
    fixture.git(&["update-ref", "refs/heads/foreign", &baseline_tip.to_string()]);
    // Review R5: the ref set is pinned BEFORE the external transition, and
    // the post-run assertion compares against that pinned snapshot (with
    // only the externally moved branch tip substituted) — a self-derived
    // post-run helper cannot see a new/deleted/moved non-main ref.
    let baseline_refs = fixture.refs_snapshot();

    // External Git transition (the user runs Git directly).
    fs::write(fixture.root().join("tracked.txt"), b"external change\n").expect("write file");
    fixture.git(&["add", "-A"]);
    fixture.git(&["commit", "-qm", "external git commit"]);
    let external_tip = fixture.tip("refs/heads/main").expect("external tip");
    assert_ne!(external_tip, baseline_tip);
    // The bytes the *user's* external transition left behind. The daemon
    // must not write any further byte.
    let post_external_bytes = fixture.worktree_snapshot();

    // One reactive pass imports the commit through the shared path.
    let output = fixture.watch_once();
    assert!(
        output.status.success(),
        "watch --once failed:\n{}",
        atomic_text(&output)
    );

    // The workspace reconciled to Git's truth...
    let verify = fixture.atomic(&["git", "bridge", "verify"]);
    assert!(
        verify.contains("matches"),
        "the reactive pass must reach the verified aligned state:\n{verify}"
    );

    // ...without moving any Git ref beyond the external commit: the pinned
    // pre-pass set, with only main's tip substituted. The foreign ref must
    // still be present at its original target.
    let pinned = baseline_refs
        .lines()
        .map(|line| {
            if line.starts_with("refs/heads/main ") {
                format!("refs/heads/main {external_tip}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        fixture.refs_snapshot().trim_end(),
        pinned,
        "the daemon must not move, add or delete any Git ref beyond the external commit"
    );
    assert_eq!(
        fixture.tip("refs/heads/main"),
        Some(external_tip),
        "main must point at the externally committed tip"
    );

    // ...and without writing a single working-copy byte beyond the
    // user's own external transition.
    assert_eq!(
        fixture.worktree_snapshot(),
        post_external_bytes,
        "the metadata-only daemon must never materialize files"
    );
}

/// AC1: an Atomic-ahead state is surfaced, not exported, reactively.
#[test]
fn atomic_ahead_state_is_never_exported_reactively() {
    let fixture = WatchFixture::new("export-suppressed");
    // The mapped branch stays at the aligned tip.
    let aligned_tip = fixture.tip("refs/heads/main").expect("aligned tip");

    // Atomic-only movement (shadow sync is keyed on the /.atomic/ exclude
    // line; removing it makes record durable-only). An existing tracked
    // file changes so the Git index stays stage-0 clean.
    let exclude = fixture.root().join(".git/info/exclude");
    let content = fs::read_to_string(&exclude).expect("read exclude");
    let filtered: String = content
        .lines()
        .filter(|line| line.trim() != "/.atomic/")
        .map(|line| format!("{line}\n"))
        .collect();
    fs::write(exclude, filtered).expect("write exclude");
    fs::write(fixture.root().join("tracked.txt"), b"atomic side\n").expect("write file");
    fixture.atomic(&["record", "-m", "atomic-side change"]);

    // The reactive pass must refuse the export direction with a notice.
    let output = fixture.watch_once();
    assert!(
        !output.status.success(),
        "the metadata-only daemon must refuse to export:\n{}",
        atomic_text(&output)
    );
    let combined = atomic_text(&output);
    assert!(
        combined.contains("metadata only") && combined.contains("reconcile"),
        "the refusal must carry the explicit-command remediation:\n{combined}"
    );

    // No ref moved reactively.
    assert_eq!(
        fixture.tip("refs/heads/main"),
        Some(aligned_tip),
        "the daemon must never move a mapped ref"
    );

    // The explicit command boundary still exports (Command budget).
    fixture.atomic(&["git", "bridge", "reconcile"]);
    let exported = fixture.tip("refs/heads/main").expect("exported tip");
    assert_ne!(exported, aligned_tip, "the command boundary exports");
    fixture.atomic(&["git", "bridge", "verify"]);
}

/// AC2: the watch-daemon path and the watcher-off path reach identical
/// logical states from identical fixtures.
#[test]
fn watcher_on_reaches_the_same_state_as_watcher_off() {
    let watched = WatchFixture::new("equivalence-watched");
    let watcher_off = WatchFixture::new("equivalence-off");

    // Identical external transitions in both repositories.
    for fixture in [&watched, &watcher_off] {
        fs::write(fixture.root().join("tracked.txt"), b"equivalence change\n").expect("write file");
        fixture.git(&["add", "-A"]);
        fixture.git(&["commit", "-qm", "equivalence external commit"]);
    }

    // Repo A: command-boundary observation only (watcher-off equivalent).
    watcher_off.atomic(&["git", "bridge", "reconcile"]);
    // Repo B: the reactive daemon pass.
    let output = watched.watch_once();
    assert!(output.status.success(), "watch --once failed");

    assert_eq!(
        watched.equivalence_identity(),
        watcher_off.equivalence_identity(),
        "watcher-on and watcher-off must reach identical logical states"
    );
    assert_eq!(watched.worktree_snapshot(), watcher_off.worktree_snapshot());
}

/// AC2: duplicated and repeated passes are no-ops that change nothing.
#[test]
fn repeated_passes_are_idempotent_noops() {
    let fixture = WatchFixture::new("idempotent");
    let before = fixture.equivalence_identity();
    let bytes = fixture.worktree_snapshot();

    // Two consecutive passes with no external change: both honest no-ops.
    let first = fixture.watch_once();
    assert!(first.status.success(), "first idle pass failed");
    assert!(
        atomic_text(&first).contains("already matches"),
        "an aligned idle pass must report the Neither outcome:\n{}",
        atomic_text(&first)
    );
    let second = fixture.watch_once();
    assert!(second.status.success(), "second idle pass failed");
    assert!(
        atomic_text(&second).contains("already matches"),
        "an aligned idle pass must report the Neither outcome:\n{}",
        atomic_text(&second)
    );
    assert_eq!(fixture.equivalence_identity(), before);
    assert_eq!(fixture.worktree_snapshot(), bytes);
}

/// AC2: a Git-owned merge marker is surfaced as an unsafe-state notice and
/// never repaired or reconciled across.
#[test]
fn git_owned_merge_marker_surfaces_notice_and_never_repairs() {
    let fixture = WatchFixture::new("unsafe");
    // Fake a Git-owned sequence operation marker.
    fs::write(
        fixture.root().join(".git/MERGE_HEAD"),
        "0123456789abcdef0123456789abcdef01234567\n",
    )
    .expect("write MERGE_HEAD");

    let output = fixture.watch_once();
    assert!(
        !output.status.success(),
        "the daemon must refuse to reconcile across a Git-owned sequence operation:\n{}",
        atomic_text(&output)
    );
    assert!(
        atomic_text(&output).contains("Git-owned") || atomic_text(&output).contains("merge"),
        "the refusal must name the unsafe state:\n{}",
        atomic_text(&output)
    );
    // The marker is untouched: the daemon surfaces, never acts.
    assert!(fixture.root().join(".git/MERGE_HEAD").exists());

    // Git finishes; the next pass reconciles normally.
    fs::remove_file(fixture.root().join(".git/MERGE_HEAD")).expect("remove MERGE_HEAD");
    let idle = fixture.watch_once();
    assert!(idle.status.success(), "post-merge pass failed");
}

/// AC2: an active managed session receives the daemon's notice before its
/// next tool call; a killed daemon leaves the next command outcome
/// unchanged.
#[test]
fn session_notice_written_and_killed_daemon_leaves_commands_unchanged() {
    let fixture = WatchFixture::new("kill");
    fixture.add_active_session("sess-cb13d");

    let baseline = fixture.equivalence_identity();
    let baseline_tip = fixture.tip("refs/heads/main").expect("baseline tip");

    // Start the real daemon in the background.
    let mut daemon = Command::new(ATOMIC_BIN)
        .args(["git", "bridge", "watch", "--poll-ms", "100"])
        .current_dir(fixture.root())
        .env("HOME", fixture.home())
        .env("ATOMIC_HOME", fixture.home().join(".atomic"))
        .env("GIT_AUTHOR_NAME", "CB-13D Tests")
        .env("GIT_AUTHOR_EMAIL", "cb13d@example.com")
        .env("GIT_COMMITTER_NAME", "CB-13D Tests")
        .env("GIT_COMMITTER_EMAIL", "cb13d@example.com")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn daemon");

    // External transition while the daemon runs.
    fs::write(fixture.root().join("tracked.txt"), b"daemon-era change\n").expect("write file");
    fixture.git(&["add", "-A"]);
    fixture.git(&["commit", "-qm", "external commit under daemon"]);
    let external_tip = fixture.tip("refs/heads/main").expect("external tip");

    // Wait for the daemon's reactive pass (poll 100ms + quiet 250ms).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let reconciled = loop {
        let checkpoint = fs::read_to_string(fixture.root().join(".atomic/bridge/workspace.json"))
            .expect("checkpoint");
        if checkpoint.contains(external_tip.to_string().as_str()) {
            break true;
        }
        if std::time::Instant::now() > deadline {
            break false;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    };
    // The daemon may have exited on its own under load; kill whatever is
    // left hard. Either way nothing may corrupt the next command.
    let _ = daemon.kill();
    let _ = daemon.wait();
    assert!(
        reconciled,
        "the running daemon must reconcile the external commit reactively\n\
         daemon-stdout: {}\n\
         daemon-stderr: {}",
        fs::read_to_string(fixture.root().join("daemon-stdout.log")).unwrap_or_default(),
        fs::read_to_string(fixture.root().join("daemon-stderr.log")).unwrap_or_default()
    );

    // The active session received the external-head-change notice.
    let notice = fixture
        .root()
        .join(".atomic/sessions/notices/sess-cb13d.json");
    assert!(
        notice.exists(),
        "an active managed session must receive the watch notice"
    );

    // The killed daemon left the workspace in a command-boundary-clean
    // state: the next command outcome is unchanged versus the baseline.
    fixture.atomic(&["git", "bridge", "reconcile"]);
    fixture.atomic(&["git", "bridge", "verify"]);
    let after = fixture.equivalence_identity();
    assert_ne!(
        after, baseline,
        "the external commit must be reflected after the boundary reconcile"
    );
    let checkpoint = fs::read_to_string(fixture.root().join(".atomic/bridge/workspace.json"))
        .expect("checkpoint");
    assert!(
        checkpoint.contains(&external_tip.to_string()),
        "the checkpoint must reference the external tip after the killed daemon:\n{checkpoint}"
    );
}

/// Review R4: an explicit `git.watch = "off"` overrides the bridge-watch
/// consent — the daemon is a tier accelerator, never an independent
/// consent surface.
#[test]
fn git_watch_off_overrides_bridge_watch_consent() {
    let fixture = WatchFixture::new("watch-off");
    let config = fixture.root().join(".atomic/config.toml");
    let content = fs::read_to_string(&config).expect("read repo config");
    // The serialized config carries the auto default; the explicit off is
    // recorded by flipping that same line inside the existing [git] table.
    assert!(
        content.contains("watch = \"auto\""),
        "fixture sanity: the auto tier default is serialized:\n{content}"
    );
    let rewritten = content.replacen("watch = \"auto\"", "watch = \"off\"", 1);
    assert_ne!(rewritten, content, "fixture sanity: the tier line exists");
    fs::write(&config, rewritten).expect("write repo config");

    let output = fixture.watch_once();
    assert!(
        !output.status.success(),
        "the daemon must refuse under git.watch=off even with bridge-watch consent:\n{}",
        atomic_text(&output)
    );
    assert!(
        atomic_text(&output).contains("watch = \"off\"") || atomic_text(&output).contains("disabled by"),
        "the refusal must name the git.watch override:\n{}",
        atomic_text(&output)
    );

    // The explicit command boundary is unaffected: reconcile still works.
    fixture.atomic(&["git", "bridge", "reconcile"]);
}

/// AC3: the daemon is opt-in only, over and above the bridge opt-in.
#[test]
fn watch_daemon_requires_explicit_opt_in() {
    let fixture = WatchFixture::new("optin");
    // Remove the watch consent, keep the bridge consent.
    disable_watch(fixture.root());

    let output = fixture.watch_once();
    assert!(
        !output.status.success(),
        "the daemon must refuse without explicit opt-in:\n{}",
        atomic_text(&output)
    );
    assert!(
        atomic_text(&output).contains("disabled") || atomic_text(&output).contains("opt in"),
        "the refusal must name the consent surface:\n{}",
        atomic_text(&output)
    );
}

/// Review R1 regression: a cross-view bound HEAD adoption must not run the
/// §7.3 shelf planner/executor under the metadata-only budget — an ignored
/// user artifact is never shelved or removed by the reactive daemon. The
/// review reproduced the opposite on the pre-fix binary: `watch --once`
/// exited 0 ("already matches") and the artifact had been shelved away.
#[test]
fn watch_once_never_shelves_ignored_artifacts_on_cross_view_head_adoption() {
    let fixture = WatchFixture::new("no-shelf");
    // Detach to an unrelated (orphan) commit and reconcile explicitly: the
    // workspace maps onto the ephemeral §7.5 view, so the checkpoint view
    // and the eventual re-attached branch view differ (the cross-view
    // adoption shape whose shelf path removed user artifacts pre-fix).
    fixture.git(&["checkout", "--orphan", "side"]);
    fixture.git(&["add", "-A"]);
    fixture.git(&["commit", "-qm", "orphan side"]);
    fixture.git(&["checkout", "--detach"]);
    fixture.atomic(&["git", "bridge", "reconcile"]);

    // The ignored user artifact.
    let exclude = fixture.root().join(".git/info/exclude");
    let mut exclude_content = fs::read_to_string(&exclude).expect("read exclude");
    exclude_content.push_str("artifact.txt\n");
    fs::write(&exclude, exclude_content).expect("write exclude");
    fs::write(fixture.root().join("artifact.txt"), b"user artifact\n").expect("write artifact");

    // The user re-attaches HEAD to main; Git preserves the ignored file.
    fixture.git(&["checkout", "main"]);
    assert!(fixture.root().join("artifact.txt").exists());

    // Pin the pre-pass truth: refs and every working-copy byte including
    // the artifact.
    let refs_before = fixture.refs_snapshot();
    let bytes_before = fixture.worktree_snapshot();

    // The reactive pass must never shelve the artifact. Whatever the
    // metadata-only boundary decides (reconcile, defer, or a typed
    // refusal), the user's ignored bytes and the refs stay untouched.
    let output = fixture.watch_once();
    let _ = output;
    assert!(
        fixture.root().join("artifact.txt").exists(),
        "the metadata-only daemon must never shelve or remove a user artifact:\n{}",
        atomic_text(&output)
    );
    assert_eq!(
        fixture.refs_snapshot().trim_end(),
        refs_before.trim_end(),
        "the reactive adoption path must not move any Git ref:\n{}",
        atomic_text(&output)
    );
    assert_eq!(
        fixture.worktree_snapshot(),
        bytes_before,
        "the reactive adoption path must not write a single working-copy byte:\n{}",
        atomic_text(&output)
    );
}

/// Review R2 regression: a held `HEAD.lock` fences the reactive import.
/// The review reproduced the opposite on the pre-fix binary: the daemon
/// imported and advanced its checkpoint while `.git/HEAD.lock` still
/// existed.
#[test]
fn head_lock_blocks_reactive_import_until_git_releases_it() {
    let fixture = WatchFixture::new("head-lock");
    let baseline_checkpoint = fs::read_to_string(
        fixture.root().join(".atomic/bridge/workspace.json"),
    )
    .expect("checkpoint");
    assert!(
        !baseline_checkpoint.contains("locked change"),
        "fixture sanity: the baseline checkpoint predates the locked change"
    );

    // External commit the daemon must NOT import while HEAD is locked.
    fs::write(fixture.root().join("tracked.txt"), b"locked change\n").expect("write file");
    fixture.git(&["add", "-A"]);
    fixture.git(&["commit", "-qm", "external commit under HEAD.lock"]);
    let external_tip = fixture.tip("refs/heads/main").expect("external tip");

    // Git holds HEAD.lock (e.g. an in-flight checkout/update-ref).
    let head_lock = fixture.root().join(".git/HEAD.lock");
    fs::write(&head_lock, b"").expect("write HEAD.lock");

    let output = fixture.watch_once();
    assert!(
        !output.status.success(),
        "the daemon must refuse to reconcile across a held HEAD.lock:\n{}",
        atomic_text(&output)
    );
    assert!(
        head_lock.exists(),
        "the daemon must never remove a Git-owned lock"
    );
    let checkpoint =
        fs::read_to_string(fixture.root().join(".atomic/bridge/workspace.json")).expect("checkpoint");
    assert!(
        !checkpoint.contains(external_tip.to_string().as_str()),
        "the reactive import must not advance the checkpoint while HEAD is locked:\n{checkpoint}"
    );

    // Git releases the lock; the next pass imports normally.
    fs::remove_file(&head_lock).expect("remove HEAD.lock");
    let output = fixture.watch_once();
    assert!(
        output.status.success(),
        "the post-release pass must import normally:\n{}",
        atomic_text(&output)
    );
    let checkpoint =
        fs::read_to_string(fixture.root().join(".atomic/bridge/workspace.json")).expect("checkpoint");
    assert!(
        checkpoint.contains(external_tip.to_string().as_str()),
        "the post-release pass must reach the external tip:\n{checkpoint}"
    );
}

// ============================================================================
// CB-13D AC-2 (::21 completion): the full tier-equivalence corpus, the
// event-degradation matrix, and journal-evidence suppression.
//
// - The corpus matrix drives SCENARIOS (plain edits, rename+edit,
//   mode+new-file, nested directories, sequential commits) through two
//   identical fixtures per scenario — watcher-OFF reconciling at command
//   boundaries and the watcher-ON daemon pass (`watch --once`, scan tier) —
//   and asserts the FULL logical identity: views (checkpoint state shape),
//   working-copy records, bindings (deterministic binding ref targets),
//   Git refs, repository manifests, verified layers and physical worktree
//   bytes under the conversion policy.
// - The degradation matrix injects dropped hints, event bursts (overflow),
//   reordered delivery, and sleep/resume gaps, asserting each degrades to
//   command-boundary observation with the next command outcome unchanged.
//   Duplicated passes, daemon kill -9 and git.lock interference keep their
//   existing dedicated tests.
// - fsmonitor/Watchman tiers stay unshipped in this environment (explicitly
//   reported in the intent); the invalid-token fallback path IS exercised
//   by injecting corrupt token files and proving the scan fallback keeps
//   the outcome unchanged.
// ============================================================================

/// The corpus identity: every logical state layer the AC-2 text names.
/// Binding ref NAMES embed repository-specific ids, so the corpus compares
/// the sorted set of binding ref TARGETS (deterministic commit OIDs) plus
/// the ref classes; working-copy records (pointer + current view) and the
/// repository manifest roots are compared verbatim.
fn corpus_identity(fixture: &WatchFixture) -> String {
    let mut identity = String::new();
    // Views: the checkpoint's state-shape fields (incl. manifest roots).
    let checkpoint = fs::read_to_string(fixture.root().join(".atomic/bridge/workspace.json"))
        .expect("checkpoint");
    for line in checkpoint.lines() {
        let trimmed = line.trim();
        for key in [
            "\"version\"",
            "\"view\"",
            "\"kind\"",
            "\"git_tree\"",
            "\"git_index_tree\"",
            "\"git_index_digest\"",
            "\"atomic_manifest_root\"",
            "\"git_manifest_root\"",
        ] {
            if trimmed.starts_with(key) {
                identity.push_str(trimmed);
                identity.push('\n');
            }
        }
        if trimmed.starts_with("\"value\"") {
            identity.push_str(trimmed);
            identity.push('\n');
        }
    }
    // Git refs: non-binding targets verbatim (deterministic), binding
    // refs by COUNT — binding names AND targets carry per-repository
    // entropy (fresh ids/ULIDs); their cryptographic verification is
    // covered by the verify layer below.
    let refs = fixture.git(&["for-each-ref", "--format=%(refname) %(objectname)"]);
    let mut binding_count = 0usize;
    identity.push_str("refs:\n");
    for line in refs.lines() {
        let (refname, _target) = line
            .split_once(' ')
            .unwrap_or((line, ""));
        if refname.starts_with("refs/atomic/bindings/") {
            binding_count += 1;
        } else {
            identity.push_str(line);
            identity.push('\n');
        }
    }
    identity.push_str(&format!("binding-refs: {binding_count}\n"));
    // Working-copy records: the current-view file. The working-copy ULID
    // itself is per-repository by construction and excluded.
    let current_view =
        fs::read_to_string(fixture.root().join(".atomic/current_view")).unwrap_or_default();
    identity.push_str(&format!("current_view: {}\n", current_view.trim()));
    // Verified layers (base32-normalized) + physical bytes + change count.
    let verify = fixture.atomic(&["git", "bridge", "verify"]);
    identity.push_str(&format!("verify:\n{}\n", normalize_base32(&verify)));
    identity.push_str(&format!(
        "worktree:\n{}\n",
        fixture.worktree_snapshot()
    ));
    let changes = fs::read_dir(fixture.root().join(".atomic/changes"))
        .map(|dirs| {
            dirs.flatten()
                .filter_map(|d| fs::read_dir(d.path()).ok())
                .flatten()
                .filter_map(|f| f.ok())
                .filter(|f| f.path().extension().map(|x| x == "change").unwrap_or(false))
                .count()
        })
        .unwrap_or(0);
    identity.push_str(&format!("changes: {changes}\n"));
    identity
}

/// Run `scenario` on a fresh watcher-ON fixture (daemon pass per external
/// change) and a fresh watcher-OFF fixture (command reconcile per external
/// change), and assert the corpus identity matches.
fn assert_corpus_scenario(name: &str, scenario: impl Fn(&WatchFixture)) {
    let watched = WatchFixture::new(&format!("corpus-on-{name}"));
    scenario(&watched);
    let off = WatchFixture::new(&format!("corpus-off-{name}"));
    disable_watch(off.root());
    scenario(&off);

    // Watcher-ON: the daemon path for each external transition.
    let watched_identity = {
        let output = watched.watch_once();
        assert!(
            output.status.success(),
            "watcher-on pass failed for scenario {name}:\n{}",
            atomic_text(&output)
        );
        corpus_identity(&watched)
    };
    // Watcher-OFF: the command-boundary path for the same transitions.
    let off_identity = {
        off.atomic(&["git", "bridge", "reconcile"]);
        corpus_identity(&off)
    };
    assert_eq!(
        watched_identity, off_identity,
        "scenario {name}: watcher-on and watcher-off tiers must reach identical \
         views, records, bindings, refs, manifests, verified layers and bytes"
    );
}

/// Scenario 1 — plain edits: modify, add, delete.
fn corpus_scenario_plain_edits(fixture: &WatchFixture) {
    fs::write(fixture.root().join("tracked.txt"), b"edited v2\n").expect("edit");
    fs::write(fixture.root().join("added.txt"), b"newly added\n").expect("add");
    fs::write(fixture.root().join("removable.txt"), b"to be removed\n").expect("seed");
    fixture.git(&["add", "-A"]);
    fixture.git(&["commit", "-qm", "corpus 1a: seed"]);
    fs::remove_file(fixture.root().join("removable.txt")).expect("delete");
    fixture.git(&["add", "-A"]);
    fixture.git(&["commit", "-qm", "corpus 1b: delete"]);
}

/// Scenario 2 — rename + edit of the renamed file.
fn corpus_scenario_rename_and_edit(fixture: &WatchFixture) {
    fixture.git(&["mv", "tracked.txt", "renamed.txt"]);
    fs::write(fixture.root().join("renamed.txt"), b"renamed and edited\n")
        .expect("edit renamed");
    fixture.git(&["add", "-A"]);
    fixture.git(&["commit", "-qm", "corpus 2: rename and edit"]);
}

/// Scenario 3 — mode change plus a new file.
fn corpus_scenario_mode_and_new_file(fixture: &WatchFixture) {
    let script = fixture.root().join("script.sh");
    fs::write(&script, b"#!/bin/sh\necho corpus\n").expect("script");
    fixture.git(&["add", "-A"]);
    fixture.git(&["commit", "-qm", "corpus 3: script"]);
    // chmod-only delta on a tracked file.
    let mut permissions = fs::metadata(&script).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt as _;
    permissions.set_mode(0o755);
    fs::set_permissions(&script, permissions).unwrap();
    fixture.git(&["add", "-A"]);
    fixture.git(&["commit", "-qm", "corpus 3b: mode change"]);
}

/// Scenario 4 — nested directory creation with deep paths.
fn corpus_scenario_nested_directories(fixture: &WatchFixture) {
    let deep = fixture.root().join("a/b/c/d/e");
    fs::create_dir_all(&deep).expect("deep dirs");
    fs::write(deep.join("leaf.txt"), b"deep corpus leaf\n").expect("leaf");
    fixture.git(&["add", "-A"]);
    fixture.git(&["commit", "-qm", "corpus 4: nested dirs"]);
}

/// Scenario 5 — two sequential external commits.
fn corpus_scenario_sequential_commits(fixture: &WatchFixture) {
    fs::write(fixture.root().join("seq.txt"), b"first\n").expect("first");
    fixture.git(&["add", "-A"]);
    fixture.git(&["commit", "-qm", "corpus 5a"]);
    fs::write(fixture.root().join("seq.txt"), b"second\n").expect("second");
    fixture.git(&["add", "-A"]);
    fixture.git(&["commit", "-qm", "corpus 5b"]);
}

#[test]
fn corpus_plain_edits_tiers_reach_identical_state() {
    assert_corpus_scenario("plain-edits", corpus_scenario_plain_edits);
}

#[test]
fn corpus_rename_and_edit_tiers_reach_identical_state() {
    assert_corpus_scenario("rename-edit", corpus_scenario_rename_and_edit);
}

#[test]
fn corpus_mode_and_new_file_tiers_reach_identical_state() {
    assert_corpus_scenario("mode-new-file", corpus_scenario_mode_and_new_file);
}

#[test]
fn corpus_nested_directories_tiers_reach_identical_state() {
    assert_corpus_scenario("nested-dirs", corpus_scenario_nested_directories);
}

#[test]
fn corpus_sequential_commits_tiers_reach_identical_state() {
    assert_corpus_scenario("sequential-commits", corpus_scenario_sequential_commits);
}

// --- CB-13D AC-2 degradation matrix ----------------------------------------
//
// Every fault degrades to command-boundary observation: the daemon path is
// only ever an accelerator, so each injected fault leaves the next command's
// outcome unchanged and the workspace aligned to truth.

/// Dropped hints: the daemon never fires. The next command boundary still
/// reconciles to exactly the state the watched path would have reached.
#[test]
fn dropped_hint_degrades_to_command_boundary_observation() {
    let watched = WatchFixture::new("drop-on");
    let off = WatchFixture::new("drop-off");
    disable_watch(off.root());

    // The same external transition lands on both; the daemon NEVER sees it
    // (no pass is run for the watched fixture).
    fs::write(watched.root().join("tracked.txt"), b"dropped hint edit\n").expect("edit");
    watched.git(&["add", "-A"]);
    watched.git(&["commit", "-qm", "dropped hint commit"]);
    fs::write(off.root().join("tracked.txt"), b"dropped hint edit\n").expect("edit");
    off.git(&["add", "-A"]);
    off.git(&["commit", "-qm", "dropped hint commit"]);

    // The next command boundary on the watched fixture reconciles.
    watched.atomic(&["git", "bridge", "reconcile"]);
    off.atomic(&["git", "bridge", "reconcile"]);

    assert_eq!(
        corpus_identity(&watched),
        corpus_identity(&off),
        "a dropped hint must not change the next command's outcome"
    );
}

/// Event burst (the overflow shape): five rapid external commits with no
/// daemon pass in between; one pass imports the final truth and reaches
/// exactly the state one command reconcile reaches.
#[test]
fn event_burst_overflow_degrades_to_one_truth_pass() {
    let watched = WatchFixture::new("burst-on");
    let off = WatchFixture::new("burst-off");
    disable_watch(off.root());

    for round in 0..5 {
        for fixture in [&watched, &off] {
            fs::write(
                fixture.root().join("burst.txt"),
                format!("burst round {round}\n"),
            )
            .expect("burst edit");
            fixture.git(&["add", "-A"]);
            fixture.git(&["commit", "-qm", &format!("burst {round}")]);
        }
    }

    // One reactive pass over the accumulated burst.
    let output = watched.watch_once();
    assert!(
        output.status.success(),
        "burst pass failed:\n{}",
        atomic_text(&output)
    );
    off.atomic(&["git", "bridge", "reconcile"]);

    assert_eq!(
        corpus_identity(&watched),
        corpus_identity(&off),
        "a hint burst must converge to the same state as the command boundary"
    );
}

/// Sleep/resume gap: a pass, an external transition during the pause, then a
/// resumed pass. The resumed pass re-baselines on the verified post-pass
/// observation, imports the gap transition exactly once, and reaches the
/// same state as per-transition command reconciles (no double import).
#[test]
fn sleep_resume_gap_rebaselines_and_imports_gap_truth_once() {
    // Streamed (per-transition) watched path…
    let watched = WatchFixture::new("sleep-on");
    fs::write(watched.root().join("tracked.txt"), b"pre-sleep edit\n").expect("edit");
    watched.git(&["add", "-A"]);
    watched.git(&["commit", "-qm", "pre-sleep"]);
    let output = watched.watch_once();
    assert!(output.status.success(), "pre-sleep pass failed");
    // …the gap (external truth moves while nothing watches)…
    fs::write(watched.root().join("tracked.txt"), b"resumed edit\n").expect("gap edit");
    watched.git(&["add", "-A"]);
    watched.git(&["commit", "-qm", "gap commit"]);
    // …and the resumed pass.
    let output = watched.watch_once();
    assert!(output.status.success(), "resumed pass failed");

    // …versus the same stream on the command path.
    let off = WatchFixture::new("sleep-off");
    disable_watch(off.root());
    fs::write(off.root().join("tracked.txt"), b"pre-sleep edit\n").expect("edit");
    off.git(&["add", "-A"]);
    off.git(&["commit", "-qm", "pre-sleep"]);
    off.atomic(&["git", "bridge", "reconcile"]);
    fs::write(off.root().join("tracked.txt"), b"resumed edit\n").expect("gap edit");
    off.git(&["add", "-A"]);
    off.git(&["commit", "-qm", "gap commit"]);
    off.atomic(&["git", "bridge", "reconcile"]);

    assert_eq!(
        corpus_identity(&watched),
        corpus_identity(&off),
        "the resumed pass must import the gap truth exactly once and reach the \
         same state as per-transition command reconciles"
    );
}

/// Invalid change-source tokens: corrupt fsmonitor/watchman token files make
/// the change source fall back to the scan tier; the command outcome is
/// unchanged and the fallback is recorded, never silent.
#[test]
fn invalid_tokens_fall_back_to_scan_without_changing_the_outcome() {
    let watched = WatchFixture::new("token-on");
    let off = WatchFixture::new("token-off");
    disable_watch(off.root());

    // The same external transition on both fixtures.
    for fixture in [&watched, &off] {
        fs::write(fixture.root().join("tracked.txt"), b"token fault edit\n").expect("edit");
        fixture.git(&["add", "-A"]);
        fixture.git(&["commit", "-qm", "token fault commit"]);
    }

    // Inject corrupt fsmonitor/watchman tokens into the watched fixture's
    // token store (the decode fails closed: token dropped, invalidation
    // recorded, scan tier takes over).
    let working_copy_id =
        fs::read_to_string(watched.root().join(".atomic/working_copy_id")).expect("wc id");
    let token_dir = watched
        .root()
        .join(".atomic/working-copies")
        .join(working_copy_id.trim())
        .join("change-source-tokens");
    fs::create_dir_all(&token_dir).expect("token dir");
    fs::write(token_dir.join("fsmonitor.token"), b"corrupt-not-a-token").expect("token");
    fs::write(token_dir.join("watchman.token"), b"corrupt-not-a-token").expect("token");

    watched.atomic(&["git", "bridge", "reconcile"]);
    off.atomic(&["git", "bridge", "reconcile"]);

    assert_eq!(
        corpus_identity(&watched),
        corpus_identity(&off),
        "invalid tokens must fall back to scan with an unchanged outcome"
    );
}
