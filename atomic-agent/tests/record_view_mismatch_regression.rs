//! Regression test for the `record_turn` detection/apply view mismatch.
//!
//! `status()` reads the repository current view, while `record_turn` records to
//! `session.view_name`. These tests use the default random session view so
//! direct callers that skip session-start exercise the mismatch.

use atomic_agent::event::{HookType, TurnEvent};
use atomic_agent::record::{record_turn, TurnRecordOptions};
use atomic_agent::turn::AgentSession;

use atomic_core::types::Base32;
use atomic_repository::Repository;

/// Keep the default random view_name; do not pin it to `dev`.
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

/// A direct caller must be able to record a create turn followed by a modify turn.
#[test]
fn two_turns_with_default_random_view_both_record() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    Repository::init(root).expect("init disposable repo");

    let session = default_session();
    assert_ne!(
        session.view_name, "dev",
        "regression precondition: AgentSession::new must use a random (non-default) view_name"
    );

    std::fs::write(root.join("login.rs"), "fn attach_pdf() {}\n").expect("write new file");
    let event1 = TurnEvent::new("regress-sess", HookType::TurnEnd);
    let first = record_turn(root, &options(&session, &event1, 1, "create login.rs"))
        .expect("turn 1 (new file) should record");
    assert!(
        !first.hash.to_base32().is_empty(),
        "turn 1 must yield a hash"
    );

    // Keep the sizes different so this test does not depend on mtime granularity.
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

/// Deleting the last file on the session view must also produce a recorded turn.
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

    std::fs::write(root.join("only.rs"), "fn only() {}\n").expect("write file");
    let event1 = TurnEvent::new("regress-sess", HookType::TurnEnd);
    let first = record_turn(root, &options(&session, &event1, 1, "create only.rs"))
        .expect("turn 1 (new file) should record");

    std::fs::remove_file(root.join("only.rs")).expect("delete the only file");
    let event2 = TurnEvent::new("regress-sess", HookType::TurnEnd);
    let second = record_turn(root, &options(&session, &event2, 2, "delete only.rs"))
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
