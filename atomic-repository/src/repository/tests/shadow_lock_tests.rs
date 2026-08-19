use super::*;

/// The repo-scoped shadow-commit lock is exclusive and non-blocking: while a
/// guard is held, a second `try_lock` reports contention (`None`); once the
/// guard is dropped the lock is available again. This is the serialization
/// primitive behind the single shadow-commit pipeline (SPEC §4.3 / Phase 3).
#[test]
fn shadow_commit_lock_is_exclusive_and_non_blocking() {
    let (_temp_dir, repo) = create_temp_repo();

    let first = repo
        .try_lock_shadow_commit()
        .expect("lock op should not error");
    assert!(first.is_some(), "first acquire should succeed");

    // A second attempt while the first guard is held must report contention,
    // not block or double-acquire.
    let second = repo
        .try_lock_shadow_commit()
        .expect("lock op should not error");
    assert!(
        second.is_none(),
        "second acquire while held must be contended"
    );

    // Release and re-acquire.
    drop(first);
    let third = repo
        .try_lock_shadow_commit()
        .expect("lock op should not error");
    assert!(third.is_some(), "acquire after release should succeed");
}
