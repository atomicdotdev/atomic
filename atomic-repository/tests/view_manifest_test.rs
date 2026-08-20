//! Integration tests for view manifest export/apply (`view_manifest` /
//! `apply_view_manifest`).
//!
//! These exercise the round-trip that the wire protocol is built on: a
//! shared view plus a draft off it, exported from one repository and
//! reconstructed in another with byte-exact identity — same scope, same
//! parent, same change log (inherited prefix included), same merkle state.

use std::fs;
use std::path::{Path, PathBuf};

use atomic_core::change::{Author, ChangeHeader};
use atomic_core::pristine::ViewScope;
use atomic_core::types::{Hash, Merkle};
use atomic_repository::{manifest::ViewManifest, RecordOptions, Repository, RepositoryError};
use tempfile::TempDir;

fn init_repo() -> (Repository, TempDir, PathBuf) {
    let temp = TempDir::new().expect("temp dir");
    let path = temp.path().to_path_buf();
    let repo = Repository::init(&path).expect("init repository");
    (repo, temp, path)
}

fn write_and_add(repo: &Repository, root: &Path, name: &str, content: &str) {
    fs::write(root.join(name), content).expect("write file");
    repo.add(name, Default::default()).expect("add file");
}

fn record(repo: &Repository, message: &str) -> Hash {
    let header = ChangeHeader::builder()
        .message(message)
        .author(Author::new("Test", Some("test@example.com")))
        .build();
    *repo
        .record(header, RecordOptions::default())
        .expect("record")
        .hash()
}

/// Build the canonical fixture: `dev` (shared) with two changes, and a
/// draft `orange` forked from it with three more changes on top.
fn build_origin() -> (Repository, TempDir, PathBuf, Vec<Hash>) {
    let (mut repo, temp, root) = init_repo();

    write_and_add(&repo, &root, "base.txt", "base\n");
    let c1 = record(&repo, "initial");
    write_and_add(&repo, &root, "map.txt", "map\n");
    let c2 = record(&repo, "concepts");

    // Fork a draft the way session views are created: copy of the log,
    // Draft scope, parented on dev.
    repo.create_view_from("orange", "dev").expect("fork draft");
    repo.switch_view("orange").expect("switch to draft");

    write_and_add(&repo, &root, "tracks.txt", "A -> B\n");
    let c3 = record(&repo, "inbound track");
    write_and_add(&repo, &root, "rules.txt", "numbers\n");
    let c4 = record(&repo, "key concepts");
    fs::write(root.join("tracks.txt"), "A -> B -> C\n").expect("modify");
    let c5 = record(&repo, "math");

    (repo, temp, root, vec![c1, c2, c3, c4, c5])
}

/// Copy every change file referenced by a manifest into another repository's
/// store (stands in for the `?store` / `?change` transfer).
fn transfer_changes(from: &Repository, to: &Repository, manifest: &ViewManifest) {
    for hash in &manifest.changes {
        let change = from.load_change(hash).expect("load change");
        let saved = to.save_change(&change).expect("save change");
        assert_eq!(saved, *hash, "content address must survive the transfer");
    }
}

#[test]
fn export_captures_exact_identity() {
    let (repo, _temp, _root, hashes) = build_origin();

    let dev = repo.view_manifest("dev").expect("dev manifest");
    assert_eq!(dev.name, "dev");
    assert_eq!(dev.scope, ViewScope::Shared);
    assert_eq!(dev.parent, None);
    assert_eq!(dev.changes, &hashes[..2]);
    dev.verify().expect("dev fold matches stored state");

    let orange = repo.view_manifest("orange").expect("orange manifest");
    assert_eq!(orange.scope, ViewScope::Draft);
    assert_eq!(orange.parent.as_deref(), Some("dev"));
    // The stored log, not a delta: inherited prefix + own suffix.
    assert_eq!(orange.changes, hashes);
    orange.verify().expect("orange fold matches stored state");

    // Exported state equals the live view state.
    let info = repo.get_view_info("orange").expect("view info");
    assert_eq!(orange.state, info.state);
}

#[test]
fn round_trip_reconstructs_shared_and_draft() {
    let (origin, _t1, _r1, hashes) = build_origin();
    let dev_m = origin.view_manifest("dev").unwrap();
    let orange_m = origin.view_manifest("orange").unwrap();

    let (mut mirror, _t2, _r2) = init_repo();
    transfer_changes(&origin, &mirror, &orange_m); // superset of dev's

    // Root → leaf.
    let dev_out = mirror.apply_view_manifest(&dev_m).expect("apply dev");
    assert_eq!(dev_out.replayed, 2);
    assert_eq!(dev_out.already_present, 0);

    let orange_out = mirror.apply_view_manifest(&orange_m).expect("apply orange");
    assert_eq!(orange_out.replayed, 5);

    // Identity is byte-exact on both views.
    let dev_info = mirror.get_view_info("dev").unwrap();
    assert_eq!(dev_info.scope, ViewScope::Shared);
    assert_eq!(dev_info.parent_name, None);
    assert_eq!(dev_info.change_count, 2);
    assert_eq!(dev_info.state, dev_m.state);

    let orange_info = mirror.get_view_info("orange").unwrap();
    assert_eq!(orange_info.scope, ViewScope::Draft);
    assert_eq!(orange_info.parent_name.as_deref(), Some("dev"));
    assert_eq!(orange_info.change_count, 5);
    assert_eq!(orange_info.own_change_count, 3, "fork point preserved");
    assert_eq!(orange_info.state, orange_m.state);

    // And the reconstructed log re-exports identically.
    let re_exported = mirror.view_manifest("orange").unwrap();
    assert_eq!(re_exported.changes, hashes);
    assert_eq!(re_exported.to_text(), orange_m.to_text());
}

#[test]
fn apply_is_idempotent_and_resumable() {
    let (origin, _t1, _r1, _hashes) = build_origin();
    let dev_m = origin.view_manifest("dev").unwrap();

    let (mut mirror, _t2, _r2) = init_repo();
    transfer_changes(&origin, &mirror, &dev_m);

    mirror.apply_view_manifest(&dev_m).expect("first apply");

    // Re-applying the identical manifest is a no-op (full prefix match).
    let again = mirror.apply_view_manifest(&dev_m).expect("second apply");
    assert_eq!(again.already_present, 2);
    assert_eq!(again.replayed, 0);

    // A shorter prefix (interrupted transfer) fast-forwards on re-apply.
    let mut partial = dev_m.clone();
    partial.changes.truncate(1);
    partial.state = ViewManifest::fold(&partial.changes);
    // Simulate: a fresh repo that only got the first change applied.
    let (mut fresh, _t3, _r3) = init_repo();
    transfer_changes(&origin, &fresh, &dev_m);
    fresh.apply_view_manifest(&partial).expect("apply prefix");
    let resumed = fresh.apply_view_manifest(&dev_m).expect("resume");
    assert_eq!(resumed.already_present, 1);
    assert_eq!(resumed.replayed, 1);
    assert_eq!(fresh.get_view_info("dev").unwrap().state, dev_m.state);
}

#[test]
fn apply_rejects_missing_changes() {
    let (origin, _t1, _r1, _hashes) = build_origin();
    let dev_m = origin.view_manifest("dev").unwrap();

    let (mut mirror, _t2, _r2) = init_repo();
    // No change files transferred.
    let err = mirror.apply_view_manifest(&dev_m).unwrap_err();
    assert!(
        matches!(
            err,
            RepositoryError::ManifestMissingChanges { count: 2, .. }
        ),
        "got: {err}"
    );
}

#[test]
fn apply_rejects_missing_parent() {
    let (origin, _t1, _r1, _hashes) = build_origin();
    let mut orange_m = origin.view_manifest("orange").unwrap();
    orange_m.parent = Some("no-such-view".to_string());

    let (mut mirror, _t2, _r2) = init_repo();
    transfer_changes(&origin, &mirror, &orange_m);
    let err = mirror.apply_view_manifest(&orange_m).unwrap_err();
    assert!(
        matches!(err, RepositoryError::ManifestParentMissing { ref parent, .. }
            if parent == "no-such-view"),
        "got: {err}"
    );
}

#[test]
fn apply_rejects_divergent_log() {
    let (origin, _t1, _r1, _hashes) = build_origin();
    let dev_m = origin.view_manifest("dev").unwrap();

    // The mirror's dev has its own unrelated change: not a prefix.
    let (mut mirror, _t2, root) = init_repo();
    write_and_add(&mirror, &root, "other.txt", "different history\n");
    record(&mirror, "divergent");
    transfer_changes(&origin, &mirror, &dev_m);

    let err = mirror.apply_view_manifest(&dev_m).unwrap_err();
    assert!(
        matches!(err, RepositoryError::ManifestDiverged { at: 0, .. }),
        "got: {err}"
    );
}

#[test]
fn apply_rejects_identity_mismatch() {
    let (origin, _t1, _r1, _hashes) = build_origin();
    let mut dev_m = origin.view_manifest("dev").unwrap();

    // Same name, but the manifest claims dev is a draft of something.
    dev_m.scope = ViewScope::Draft;
    dev_m.parent = Some("elsewhere".to_string());

    let (mut mirror, _t2, _r2) = init_repo();
    transfer_changes(&origin, &mirror, &dev_m);
    let err = mirror.apply_view_manifest(&dev_m).unwrap_err();
    assert!(
        matches!(err, RepositoryError::ManifestIdentityMismatch { .. }),
        "got: {err}"
    );
}

#[test]
fn apply_rejects_tampered_state() {
    let (origin, _t1, _r1, _hashes) = build_origin();
    let mut dev_m = origin.view_manifest("dev").unwrap();
    dev_m.state = Merkle::of(b"tampered");

    let (mut mirror, _t2, _r2) = init_repo();
    transfer_changes(&origin, &mirror, &dev_m);
    let err = mirror.apply_view_manifest(&dev_m).unwrap_err();
    assert!(matches!(err, RepositoryError::Manifest(_)), "got: {err}");
}

#[test]
fn empty_view_manifest_declares_identity_only() {
    let (origin, _t1, _r1, _hashes) = build_origin();
    // A brand-new draft with no changes of its own... export and apply it.
    let mut origin = origin;
    origin.create_draft_view("empty-draft", "dev").unwrap();
    let m = origin.view_manifest("empty-draft").unwrap();
    assert!(m.changes.is_empty());
    assert_eq!(m.state, Merkle::ZERO);

    let (mut mirror, _t2, _r2) = init_repo();
    let out = mirror.apply_view_manifest(&m).expect("apply empty draft");
    assert_eq!(out.replayed, 0);

    let info = mirror.get_view_info("empty-draft").unwrap();
    assert_eq!(info.scope, ViewScope::Draft);
    assert_eq!(info.parent_name.as_deref(), Some("dev"));
    assert!(info.is_empty());
}

/// Regression: a draft pushed alongside its changes can be auto-created as a
/// Shared view (the server's `ensure_view_exists` does this before the view
/// snapshot is reconciled). `apply_view_manifest` then refuses it forever with
/// an identity mismatch. `set_view_identity` repairs the scope + parent so the
/// manifest applies and the view resolves as a draft off its parent.
#[test]
fn repair_identity_unsticks_autocreated_shared_draft() {
    let (origin, _t1, _r1, _hashes) = build_origin();

    let dev_m = origin.view_manifest("dev").expect("dev manifest");
    let orange_m = origin.view_manifest("orange").expect("orange manifest");

    // Mirror gets dev (the parent) plus every change orange references.
    let (mut mirror, _t2, _r2) = init_repo();
    transfer_changes(&origin, &mirror, &dev_m);
    mirror.apply_view_manifest(&dev_m).expect("apply dev");
    transfer_changes(&origin, &mirror, &orange_m);

    // Simulate the server auto-creating the draft as Shared before its
    // snapshot was reconciled.
    mirror
        .create_shared_view("orange")
        .expect("auto-create as shared");

    // Left uncorrected, the identity check rejects the draft manifest.
    let err = mirror
        .apply_view_manifest(&orange_m)
        .expect_err("scope mismatch must be rejected");
    assert!(
        matches!(err, RepositoryError::ManifestIdentityMismatch { .. }),
        "got: {err}"
    );

    // Repair the identity, then the same manifest applies cleanly.
    mirror
        .set_view_identity("orange", ViewScope::Draft, Some("dev"))
        .expect("repair identity");
    mirror
        .apply_view_manifest(&orange_m)
        .expect("apply after repair");

    let info = mirror.get_view_info("orange").unwrap();
    assert_eq!(info.scope, ViewScope::Draft);
    assert_eq!(info.parent_name.as_deref(), Some("dev"));
    assert_eq!(info.state, orange_m.state);
}
