//! Regression test for the `record_turn` detection/apply view mismatch.
//!
//! See `docs/atomic-record-view-mismatch.md`. The bug: `record_turn` runs its
//! change *detection* (`status()`) against the repository's `current_view`
//! (`dev`), but *applies* the recorded change to `session.view_name` — which
//! `AgentSession::new()` sets to a **random haikunator view** that does not
//! exist in a fresh repo. Turn 1 (a new, untracked file) bypasses the view
//! filter and records onto the phantom view; Turn 2 (modifying the now-tracked
//! file) detects against `dev`, the view filter hides the file's creating change
//! (it lives on the random view), and `status` reports clean → `EmptyTurn`.
//!
//! This test uses the **default random `view_name`** from `AgentSession::new()`
//! — it deliberately does NOT pin `view_name = DEFAULT_VIEW`. That distinguishes
//! it from `tests/env5_admission.rs`, which pins the view and therefore cannot
//! catch this bug. Before the fix this test fails on Turn 2 with `EmptyTurn`;
//! after the fix (detection and apply share one view) both turns record.

use atomic_agent::event::{HookType, TurnEvent};
use atomic_agent::record::{record_turn, TurnRecordOptions};
use atomic_agent::turn::AgentSession;

use atomic_core::types::Base32;
use atomic_repository::Repository;

/// A session with the DEFAULT (random) view_name, exactly as a real agent run
/// gets it. No `view_name` override — that is the point of this regression.
fn default_session() -> AgentSession {
    let mut s = AgentSession::new("regress-sess", "opencode", "OpenCode");
    s.set_model_info("anthropic", "claude-opus-4");
    s
}

fn options<'a>(
    session: &'a AgentSession,
    event: &'a TurnEvent,
    turn_number: u32,
    prompt: &str,
) -> TurnRecordOptions<'a> {
    TurnRecordOptions {
        session,
        event,
        turn_number,
        turn_duration_ms: 1000,
        prompt: Some(prompt.to_string()),
    }
}

/// Two consecutive turns with the default random session view: a new file then a
/// modification to that same file. Both must record. The second turn is the one
/// that regressed (it returned `EmptyTurn` before the fix).
#[test]
fn two_turns_with_default_random_view_both_record() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    Repository::init(root).expect("init disposable repo");

    let session = default_session();
    // Sanity: this session really is using a non-default, random view name —
    // otherwise the test would not exercise the mismatch.
    assert_ne!(
        session.view_name, "dev",
        "regression precondition: AgentSession::new must use a random (non-default) view_name"
    );

    // Turn 1 — agent creates a new file.
    std::fs::write(root.join("login.rs"), "fn attach_pdf() {}\n").expect("write new file");
    let event1 = TurnEvent::new("regress-sess", HookType::TurnEnd);
    let first = record_turn(root, &options(&session, &event1, 1, "create login.rs"))
        .expect("turn 1 (new file) should record");
    assert!(
        !first.hash.to_base32().is_empty(),
        "turn 1 must yield a hash"
    );

    // Turn 2 — agent modifies the now-tracked file.
    //
    // The new content is a DIFFERENT byte length than turn 1's, on purpose: this
    // test targets the view-mismatch bug, and we do not want it to also depend on
    // mtime granularity. `record_turn`'s detection uses `StatusOptions::fast()`
    // (no content hash), so a same-size rewrite landing in the same filesystem
    // mtime granule could be mis-classified Clean ("racy git"); keeping the size
    // distinct means the size check alone guarantees Modified is observed,
    // regardless of scheduling.
    let turn1_body = "fn attach_pdf() {}\n";
    let turn2_body = "fn attach_pdf() {\n    // Firefox + Safari support\n}\n";
    assert_ne!(
        turn1_body.len(),
        turn2_body.len(),
        "turn 2 must change the file size so detection never relies on mtime granularity"
    );
    std::fs::write(root.join("login.rs"), turn2_body).expect("modify tracked file");
    let event2 = TurnEvent::new("regress-sess", HookType::TurnEnd);
    let second = record_turn(root, &options(&session, &event2, 2, "fix pdf upload"))
        // Before the fix this is `Err(EmptyTurn)`.
        .expect("turn 2 (modified tracked file) should record — regression: was EmptyTurn");

    assert!(
        !second.hash.to_base32().is_empty(),
        "turn 2 must yield a hash"
    );
    assert_ne!(
        first.hash.to_base32(),
        second.hash.to_base32(),
        "the modification must produce a change distinct from the creation"
    );
}

/// The narrow case that the *modify* test above cannot catch: a turn whose net
/// effect is invisible from the misaligned `dev` view — deleting the **last**
/// file that lived on the session view, leaving the working tree empty.
///
/// From `dev`, the session view's file is filtered out of the tracked set and,
/// once deleted, is not even present to show as untracked — so the working tree
/// looks clean. Before the fix, `record_turn`'s **first** `status()` (on the
/// read-only handle, before any view switch) saw a clean tree and returned
/// `EmptyTurn`, silently dropping the deletion. The fix aligns the detection
/// view *in memory before* that first status, so the deletion is observed.
///
/// (A *modify* on the misaligned view surfaces the file as Untracked — so
/// `untracked_count() != 0`, the early check doesn't fire, and it records even
/// pre-fix. Only the full-deletion case exercises the early-return window.)
#[test]
fn delete_last_session_file_records_instead_of_emptyturn() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    Repository::init(root).expect("init disposable repo");

    let session = default_session();
    assert_ne!(
        session.view_name, "dev",
        "regression precondition: session must use a random (non-default) view_name"
    );

    // Turn 1 — create the only file and record it (onto the session view).
    std::fs::write(root.join("only.rs"), "fn only() {}\n").expect("write file");
    let event1 = TurnEvent::new("regress-sess", HookType::TurnEnd);
    let first = record_turn(root, &options(&session, &event1, 1, "create only.rs"))
        .expect("turn 1 (new file) should record");

    // Turn 2 — delete that one and only file. Net working tree (from dev) is now
    // empty: this is the early-return window.
    std::fs::remove_file(root.join("only.rs")).expect("delete the only file");
    let event2 = TurnEvent::new("regress-sess", HookType::TurnEnd);
    let second = record_turn(root, &options(&session, &event2, 2, "delete only.rs"))
        // Before the fix this is `Err(EmptyTurn)` — the deletion is lost.
        .expect("turn 2 (delete last session file) should record — regression: was EmptyTurn");

    assert!(
        !second.hash.to_base32().is_empty(),
        "the deletion must produce a change hash"
    );
    assert_ne!(
        first.hash.to_base32(),
        second.hash.to_base32(),
        "the deletion must be a change distinct from the creation"
    );
}
