//! End-to-end reads over a real repository: record a change, then resolve
//! and serialize it through every facade entry point a server would call.

use atomic_facade::{
    change_detail, list_log, list_memories, list_views, resolve_change, LogQuery,
};
use atomic_repository::{Repository, TrackingOptions};

fn repo_with_change() -> (Repository, String, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();

    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), "pub fn one() -> u8 { 1 }\n").unwrap();
    repo.add("src/lib.rs", TrackingOptions::default()).unwrap();
    let outcome = repo
        .record_all("feat: add one()")
        .expect("record should succeed");

    use atomic_core::types::Base32;
    (repo, outcome.hash().to_base32(), dir)
}

#[test]
fn change_detail_resolves_every_identifier_form() {
    let (repo, hash, _dir) = repo_with_change();

    // Latest (None and "@"), full hash, and prefix all resolve to the record.
    for spec in [None, Some("@"), Some(hash.as_str()), Some(&hash[..8])] {
        let detail = change_detail(&repo, None, spec)
            .unwrap_or_else(|e| panic!("spec {spec:?} failed: {e}"));
        assert_eq!(detail.hash, hash);
        assert_eq!(detail.message, "feat: add one()");
        assert!(!detail.hunks.is_empty(), "change should carry hunks");
    }

    // Sequence form: resolve the latest to learn its sequence, then round-trip.
    let (_, seq) = resolve_change(&repo, None, None).unwrap();
    let seq = seq.expect("latest change has a sequence");
    let by_seq = change_detail(&repo, None, Some(&format!("#{seq}"))).unwrap();
    assert_eq!(by_seq.hash, hash);
    assert_eq!(by_seq.sequence, Some(seq));
}

#[test]
fn log_lists_the_recorded_change_newest_first() {
    let (repo, hash, _dir) = repo_with_change();

    let entries = list_log(&repo, &LogQuery::default()).unwrap();
    assert!(!entries.is_empty());
    assert_eq!(entries[0].hash, hash, "newest entry first");
    assert_eq!(entries[0].message.as_deref(), Some("feat: add one()"));

    let limited = list_log(
        &repo,
        &LogQuery {
            limit: Some(1),
            ..LogQuery::default()
        },
    )
    .unwrap();
    assert_eq!(limited.len(), 1);
}

#[test]
fn unknown_view_is_a_client_error() {
    let (repo, _hash, _dir) = repo_with_change();
    let err = change_detail(&repo, Some("no-such-view"), None).unwrap_err();
    assert!(err.is_client_error(), "unexpected error: {err}");
}

#[test]
fn full_read_surface_serializes_to_json() {
    let (repo, _hash, _dir) = repo_with_change();

    let views = list_views(&repo).unwrap();
    let memories = list_memories(&repo, None).unwrap();
    let log = list_log(&repo, &LogQuery::default()).unwrap();

    // Everything a forge endpoint would return must serialize cleanly.
    let payload = serde_json::json!({
        "views": views,
        "memories": memories,
        "log": log,
    });
    let text = serde_json::to_string(&payload).unwrap();
    assert!(text.contains("\"views\""));
}
