//! A remote sandbox's cache records the same change the repository would.
//!
//! The cache holds only the repository's own rows for the view (the
//! skeleton) and for the paths being recorded (a slice), plus the content
//! bytes those need. `record` runs unchanged against it, and the change it
//! produces — which carries the repository's internal ids — must be byte for
//! byte the change `record` produces on the repository itself.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use atomic_core::change::{Author, ChangeHeader};
use atomic_repository::{InsertOptions, RecordOptions, Repository, TrackingOptions};
use tempfile::TempDir;

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn header(message: &str) -> ChangeHeader {
    ChangeHeader::builder()
        .message(message)
        .author(Author::new("Test", Some("test@example.com")))
        .build()
}

fn record_and_apply(repo: &Repository, message: &str) {
    let options = RecordOptions::new()
        .with_all(true)
        .save_to_store(true)
        .apply_after_record(false);
    let outcome = repo.record(header(message), options).unwrap();
    repo.write_recorded(&outcome, InsertOptions::default())
        .unwrap();
}

/// Record without saving or applying: just the change.
fn change_bytes(repo: &Repository, header: ChangeHeader) -> (String, Vec<u8>) {
    let options = RecordOptions::new()
        .with_all(true)
        .save_to_store(false)
        .apply_after_record(false);
    let outcome = repo
        .record(header, options)
        .unwrap_or_else(|e| panic!("record in {}: {e}", repo.root().display()));
    (
        atomic_core::types::Base32::to_base32(outcome.hash()),
        outcome.v3_bytes().expect("v3 bytes").to_vec(),
    )
}

struct Pair {
    _dirs: (TempDir, TempDir),
    host: Repository,
    cache: Repository,
}

impl Pair {
    fn host_root(&self) -> &Path {
        self.host.root()
    }
    fn cache_root(&self) -> &Path {
        self.cache.root()
    }

    /// Apply the same edit to both working trees.
    fn edit(&self, f: impl Fn(&Path)) {
        f(self.host_root());
        f(self.cache_root());
    }

    /// Ship the slice for what `record` will touch, as FileStates does:
    /// only the inodes the cache asks for.
    fn hydrate(&self, view: &str, live: &BTreeSet<u64>) {
        let inodes = self.cache.sandbox_slice_inodes().unwrap();
        assert!(
            inodes.len() < live.len(),
            "the cache asks for what changed, not everything: {inodes:?} of {live:?}"
        );
        let slice = self.host.export_sandbox_slice(view, &inodes).unwrap();
        self.cache.import_sandbox_slice(&slice).unwrap();
    }
}

/// A host repository with some history, and a cache materialized from it.
fn pair(history: impl Fn(&Repository)) -> (Pair, String, BTreeSet<u64>) {
    let host_dir = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    let host = Repository::init(host_dir.path()).unwrap();
    history(&host);
    let view = host.current_view().to_string();

    // Materialize: the view's files into the cache's working tree, and the
    // inodes they are.
    let cache = Repository::init(cache_dir.path()).unwrap();
    let mut live = BTreeSet::new();
    host.materialize_view_entries::<()>(&view, |entry| {
        live.insert(entry.inode);
        let path = cache_dir.path().join(&entry.path);
        match entry.kind {
            atomic_repository::ViewEntryKind::Directory => fs::create_dir_all(path).unwrap(),
            _ => {
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, &entry.content).unwrap();
            }
        }
        Ok(())
    })
    .unwrap()
    .unwrap();
    let skeleton = host.export_sandbox_skeleton(&view, &live).unwrap();
    cache.import_sandbox_skeleton(&skeleton).unwrap();
    // The baseline: what was just written is clean.
    cache.reindex_working_copy().unwrap();
    (
        Pair {
            _dirs: (host_dir, cache_dir),
            host,
            cache,
        },
        view,
        live,
    )
}

fn assert_parity(pair: &Pair, view: &str, live: &BTreeSet<u64>, what: &str) {
    pair.hydrate(view, live);
    let h = header(what);
    let host = change_bytes(&pair.host, h.clone());
    let cache = change_bytes(&pair.cache, h);
    assert_eq!(
        host.0, cache.0,
        "{what}: the cache records a different change"
    );
    assert_eq!(host.1, cache.1, "{what}: same hash, different bytes");
}

fn two_files(repo: &Repository) {
    let root = repo.root().to_path_buf();
    write(&root, "README.md", "hello\nworld\n");
    write(&root, "src/lib.rs", "pub fn a() {}\npub fn b() {}\n");
    repo.add("README.md", TrackingOptions::default()).unwrap();
    repo.add("src/lib.rs", TrackingOptions::default()).unwrap();
    record_and_apply(repo, "first");
    write(&root, "README.md", "hello\nthere\nworld\n");
    record_and_apply(repo, "second");
}

#[test]
fn modifying_a_file() {
    let (pair, view, live) = pair(two_files);
    pair.edit(|root| write(root, "README.md", "hello\nthere\nbig\nworld\n"));
    assert_parity(&pair, &view, &live, "modify");
}

#[test]
fn modifying_a_file_changed_by_several_changes() {
    let (pair, view, live) = pair(two_files);
    pair.edit(|root| write(root, "README.md", "world\n"));
    assert_parity(&pair, &view, &live, "delete lines from two changes");
}

#[test]
fn deleting_a_file() {
    let (pair, view, live) = pair(two_files);
    pair.edit(|root| fs::remove_file(root.join("src/lib.rs")).unwrap());
    assert_parity(&pair, &view, &live, "delete");
}

#[test]
fn adding_a_file_to_an_existing_directory() {
    let (pair, view, live) = pair(two_files);
    pair.edit(|root| write(root, "src/new.rs", "pub fn c() {}\n"));
    for repo in [&pair.host, &pair.cache] {
        repo.add("src/new.rs", TrackingOptions::default()).unwrap();
    }
    assert_parity(&pair, &view, &live, "add");
}

#[test]
fn adding_a_file_in_a_new_directory() {
    let (pair, view, live) = pair(two_files);
    pair.edit(|root| write(root, "docs/guide.md", "# guide\n"));
    for repo in [&pair.host, &pair.cache] {
        repo.add("docs/guide.md", TrackingOptions::default())
            .unwrap();
    }
    assert_parity(&pair, &view, &live, "add in new dir");
}

#[test]
fn without_the_slice_the_cache_cannot_record_the_same_change() {
    let (pair, _view, _live) = pair(two_files);
    pair.edit(|root| write(root, "README.md", "hello\nthere\nbig\nworld\n"));
    let h = header("no slice");
    let host = change_bytes(&pair.host, h.clone());
    let options = RecordOptions::new()
        .with_all(true)
        .save_to_store(false)
        .apply_after_record(false);
    match pair.cache.record(h, options) {
        Err(_) => {}
        Ok(outcome) => assert_ne!(
            atomic_core::types::Base32::to_base32(outcome.hash()),
            host.0,
            "the graph rows are what make the change right"
        ),
    }
    assert!(host.1.len() > 200, "a real change: {} bytes", host.1.len());
}

#[test]
fn moving_a_file() {
    // As `atomic mv` does: the rename on disk, detected by content.
    let (pair, view, live) = pair(two_files);
    for repo in [&pair.host, &pair.cache] {
        fs::rename(
            repo.root().join("src/lib.rs"),
            repo.root().join("src/core.rs"),
        )
        .unwrap();
    }
    assert_parity(&pair, &view, &live, "move");
}

#[test]
fn moving_and_editing_a_file() {
    let (pair, view, live) = pair(two_files);
    for repo in [&pair.host, &pair.cache] {
        fs::rename(repo.root().join("README.md"), repo.root().join("READ.md")).unwrap();
        write(repo.root(), "READ.md", "hello\nthere\nworld\nagain\n");
    }
    assert_parity(&pair, &view, &live, "move and edit");
}

#[test]
fn a_second_record_on_top_of_the_first() {
    // After the first change lands on the repository, the cache takes a fresh
    // skeleton and slice and records the next one identically.
    let (pair, view, live) = pair(two_files);
    pair.edit(|root| write(root, "src/lib.rs", "pub fn a() {}\n"));
    assert_parity(&pair, &view, &live, "first");
    record_and_apply(&pair.host, "first");
    let skeleton = pair.host.export_sandbox_skeleton(&view, &live).unwrap();
    pair.cache.import_sandbox_skeleton(&skeleton).unwrap();
    pair.cache.reindex_working_copy().unwrap();
    pair.edit(|root| write(root, "src/lib.rs", "pub fn a() { 1 }\n"));
    assert_parity(&pair, &view, &live, "second");
}

// ── Submitting: the repository takes the cache's change ─────────────────

use atomic_repository::SubmitRejection;

fn view_state(repo: &Repository, view: &str) -> String {
    let skeleton = repo
        .export_sandbox_skeleton(view, &BTreeSet::new())
        .unwrap();
    atomic_core::types::Base32::to_base32(&skeleton.view.state)
}

/// Record in the cache (hydrated as FileStates would) and return the change.
fn record_in_cache(
    pair: &Pair,
    view: &str,
    live: &BTreeSet<u64>,
    what: &str,
) -> (atomic_core::types::Hash, Vec<u8>) {
    pair.hydrate(view, live);
    let options = RecordOptions::new()
        .with_all(true)
        .save_to_store(false)
        .apply_after_record(false);
    let outcome = pair.cache.record(header(what), options).unwrap();
    (*outcome.hash(), outcome.v3_bytes().unwrap().to_vec())
}

fn tree_of(repo: &Repository, view: &str) -> Vec<(String, Vec<u8>)> {
    let mut files = Vec::new();
    repo.materialize_view_entries::<()>(view, |e| {
        if e.kind == atomic_repository::ViewEntryKind::File {
            files.push((e.path, e.content));
        }
        Ok(())
    })
    .unwrap()
    .unwrap();
    files
}

#[test]
fn a_submitted_change_lands_on_the_view_and_the_next_one_builds_on_it() {
    let (pair, view, mut live) = pair(two_files);
    // Only the cache's tree changes: the repository learns of it by submission.
    write(pair.cache_root(), "README.md", "hello\nfrom the sandbox\n");
    write(pair.cache_root(), "src/new.rs", "pub fn n() {}\n");
    pair.cache
        .add("src/new.rs", TrackingOptions::default())
        .unwrap();
    let base = view_state(&pair.host, &view);
    let (hash, bytes) = record_in_cache(&pair, &view, &live, "from the sandbox");

    let submitted = pair
        .host
        .insert_submitted_change(&view, &base, &hash, &bytes)
        .unwrap()
        .expect("accepted");
    assert_ne!(submitted.state, base, "the view moved");
    let tree = tree_of(&pair.host, &view);
    assert!(tree.contains(&("README.md".into(), b"hello\nfrom the sandbox\n".to_vec())));
    assert!(tree.contains(&("src/new.rs".into(), b"pub fn n() {}\n".to_vec())));

    // The same change again: the view has moved, and then it's already there.
    let again = pair
        .host
        .insert_submitted_change(&view, &base, &hash, &bytes)
        .unwrap();
    assert!(
        matches!(again, Err(SubmitRejection::StaleView { .. })),
        "{again:?}"
    );
    let again = pair
        .host
        .insert_submitted_change(&view, &submitted.state, &hash, &bytes)
        .unwrap();
    assert!(
        matches!(again, Err(SubmitRejection::AlreadyPresent(_))),
        "{again:?}"
    );

    // The cache takes the new skeleton (with the repository's inode for the
    // new file) and records on top.
    live.clear();
    pair.host
        .materialize_view_entries::<()>(&view, |e| {
            live.insert(e.inode);
            Ok(())
        })
        .unwrap()
        .unwrap();
    let skeleton = pair.host.export_sandbox_skeleton(&view, &live).unwrap();
    pair.cache.import_sandbox_skeleton(&skeleton).unwrap();
    pair.cache.reindex_working_copy().unwrap();
    write(pair.cache_root(), "src/new.rs", "pub fn n() { 2 }\n");
    let (hash, bytes) = record_in_cache(&pair, &view, &live, "second from the sandbox");
    pair.host
        .insert_submitted_change(&view, &submitted.state, &hash, &bytes)
        .unwrap()
        .expect("the second is accepted too");
    assert!(
        tree_of(&pair.host, &view).contains(&("src/new.rs".into(), b"pub fn n() { 2 }\n".to_vec()))
    );
}

#[test]
fn a_tampered_change_is_refused_and_leaves_nothing() {
    let (pair, view, live) = pair(two_files);
    write(pair.cache_root(), "README.md", "tampered?\n");
    let base = view_state(&pair.host, &view);
    let (hash, mut bytes) = record_in_cache(&pair, &view, &live, "tamper");
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    let refused = pair
        .host
        .insert_submitted_change(&view, &base, &hash, &bytes)
        .unwrap();
    assert!(
        matches!(
            refused,
            Err(SubmitRejection::HashMismatch { .. } | SubmitRejection::Malformed(_))
        ),
        "{refused:?}"
    );
    assert_eq!(view_state(&pair.host, &view), base);
    assert!(!pair.host.has_change(&hash));
}

#[test]
fn a_change_recorded_against_an_older_view_is_refused() {
    let (pair, view, live) = pair(two_files);
    let base = view_state(&pair.host, &view);
    write(pair.cache_root(), "README.md", "late\n");
    let (hash, bytes) = record_in_cache(&pair, &view, &live, "late");
    // Meanwhile the view moves on the repository.
    write(pair.host_root(), "src/lib.rs", "pub fn moved() {}\n");
    record_and_apply(&pair.host, "meanwhile");
    let refused = pair
        .host
        .insert_submitted_change(&view, &base, &hash, &bytes)
        .unwrap();
    assert!(
        matches!(refused, Err(SubmitRejection::StaleView { .. })),
        "{refused:?}"
    );
}

#[test]
fn a_change_from_another_view_is_refused() {
    // Work recorded on a draft view depends on that view's changes; submitted
    // to `dev` it names changes `dev` can't see.
    let host_dir = TempDir::new().unwrap();
    let draft_dir = TempDir::new().unwrap();
    {
        let mut host = Repository::init(host_dir.path()).unwrap();
        two_files(&host);
        host.create_view_from("other", "dev").unwrap();
        host.provision_sandbox(draft_dir.path().join("w"), "other")
            .unwrap();
    }
    let draft = Repository::open_existing(draft_dir.path().join("w")).unwrap();
    write(draft.root(), "README.md", "only on other\n");
    record_and_apply(&draft, "on other");
    write(draft.root(), "README.md", "only on other, again\n");
    let (hash, bytes) = {
        let options = RecordOptions::new()
            .with_all(true)
            .save_to_store(false)
            .apply_after_record(false);
        let outcome = draft.record(header("on top of other"), options).unwrap();
        (*outcome.hash(), outcome.v3_bytes().unwrap().to_vec())
    };
    drop(draft);

    let host = Repository::open_existing(host_dir.path()).unwrap();
    let base = view_state(&host, "dev");
    let refused = host
        .insert_submitted_change("dev", &base, &hash, &bytes)
        .unwrap();
    assert!(
        matches!(
            refused,
            Err(SubmitRejection::ForeignChange(_) | SubmitRejection::ForeignNode(_))
        ),
        "{refused:?}"
    );
    assert_eq!(view_state(&host, "dev"), base);
}
