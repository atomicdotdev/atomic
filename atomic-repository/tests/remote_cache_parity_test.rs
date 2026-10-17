//! A remote sandbox's cache records the same change the repository would.
//!
//! The cache holds only the repository's own rows for the view (the
//! skeleton), the canonical changes and a graph slice covering its visible
//! inodes. Canonical path and attribute indexes are rebuilt from that data. `record` runs unchanged against it, and the change it
//! produces — which carries the repository's internal ids — must be byte for
//! byte the change `record` produces on the repository itself.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use atomic_core::change::{Author, ChangeHeader};
use atomic_repository::{InsertOptions, RecordOptions, Repository, StatusOptions, TrackingOptions};
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
    let outcome = repo
        .record(
            repo.require_working_copy_id().unwrap(),
            header(message),
            options,
        )
        .unwrap();
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
        .record(repo.require_working_copy_id().unwrap(), header, options)
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

    /// Hydrate canonical graph data without changing the working files.
    fn hydrate(&self, view: &str, live: &BTreeMap<u64, String>) {
        hydrate_cache(&self.host, &self.cache, view);
        for (&inode, path) in live.iter().filter(|(inode, _)| **inode != 0) {
            let claim = self.cache.resolve_single_path_claim(view, path).unwrap();
            assert_eq!(claim.map(|(_, inode, _)| inode.get()), Some(inode));
        }
    }
}

/// Dependencies discovered while importing changes are fetched before the
/// graph slice rebuilds the canonical path and attribute indexes.
fn fetch_changes(host: &Repository, cache: &Repository, view: &str) {
    loop {
        let missing = cache.missing_sandbox_changes().unwrap();
        if missing.is_empty() {
            break;
        }
        let changes = host
            .export_sandbox_changes(view, &missing)
            .unwrap()
            .unwrap();
        assert_eq!(changes.len(), missing.len());
        cache.hold_sandbox_changes(&changes).unwrap();
    }
}

fn hydrate_cache(host: &Repository, cache: &Repository, view: &str) {
    fetch_changes(host, cache, view);
    let inodes = cache.sandbox_slice_inodes().unwrap();
    let slice = host.export_sandbox_slice(view, &inodes).unwrap();
    cache.import_sandbox_slice(&slice).unwrap();
}

fn assert_clean(repo: &Repository) {
    let status = repo
        .status(
            repo.require_working_copy_id().unwrap(),
            StatusOptions::default(),
        )
        .unwrap();
    assert!(
        status.is_clean(),
        "canonical cache baseline is dirty: {:?}",
        status.entries()
    );
}

/// A host repository with some history, and a cache materialized from it.
fn pair(history: impl Fn(&Repository)) -> (Pair, String, BTreeMap<u64, String>) {
    pair_with_hydration(history, true)
}

fn pair_with_hydration(
    history: impl Fn(&Repository),
    hydrate: bool,
) -> (Pair, String, BTreeMap<u64, String>) {
    let host_dir = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    let host = Repository::init(host_dir.path()).unwrap();
    history(&host);
    let view = host.current_view().to_string();

    // Materialize: the view's files into the cache's working tree, and the
    // inodes they are.
    let cache = Repository::init(cache_dir.path()).unwrap();
    let mut live = BTreeMap::new();
    host.materialize_view_entries::<()>(&view, |entry| {
        live.insert(entry.inode, entry.path.clone());
        atomic_repository::write_sandbox_entry(
            cache_dir.path(),
            &entry.path,
            entry.kind,
            u32::from(entry.mode),
            &entry.content,
        )
        .unwrap();
        Ok(())
    })
    .unwrap()
    .unwrap();
    let skeleton = host.export_sandbox_skeleton(&view, &live).unwrap();
    cache.import_sandbox_skeleton(&skeleton).unwrap();
    if hydrate {
        hydrate_cache(&host, &cache, &view);
        assert_clean(&cache);
    }
    // The baseline: what was just written is clean.
    cache
        .reindex_working_copy(cache.require_working_copy_id().unwrap())
        .unwrap();
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

fn assert_parity(pair: &Pair, view: &str, live: &BTreeMap<u64, String>, what: &str) {
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
    repo.add(
        repo.require_working_copy_id().unwrap(),
        "README.md",
        TrackingOptions::default(),
    )
    .unwrap();
    repo.add(
        repo.require_working_copy_id().unwrap(),
        "src/lib.rs",
        TrackingOptions::default(),
    )
    .unwrap();
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
        repo.add(
            repo.require_working_copy_id().unwrap(),
            "src/new.rs",
            TrackingOptions::default(),
        )
        .unwrap();
    }
    assert_parity(&pair, &view, &live, "add");
}

#[test]
fn adding_a_file_in_a_new_directory() {
    let (pair, view, live) = pair(two_files);
    pair.edit(|root| write(root, "docs/guide.md", "# guide\n"));
    for repo in [&pair.host, &pair.cache] {
        repo.add(
            repo.require_working_copy_id().unwrap(),
            "docs/guide.md",
            TrackingOptions::default(),
        )
        .unwrap();
    }
    assert_parity(&pair, &view, &live, "add in new dir");
}

#[test]
fn without_the_slice_the_cache_cannot_record_the_same_change() {
    let (pair, _view, _live) = pair_with_hydration(two_files, false);
    pair.edit(|root| write(root, "README.md", "hello\nthere\nbig\nworld\n"));
    let h = header("no slice");
    let host = change_bytes(&pair.host, h.clone());
    let options = RecordOptions::new()
        .with_all(true)
        .save_to_store(false)
        .apply_after_record(false);
    match pair
        .cache
        .record(pair.cache.require_working_copy_id().unwrap(), h, options)
    {
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
    hydrate_cache(&pair.host, &pair.cache, &view);
    assert_clean(&pair.cache);
    pair.cache
        .reindex_working_copy(pair.cache.require_working_copy_id().unwrap())
        .unwrap();
    pair.edit(|root| write(root, "src/lib.rs", "pub fn a() { 1 }\n"));
    assert_parity(&pair, &view, &live, "second");
}

// ── Submitting: the repository takes the cache's change ─────────────────

use atomic_repository::SubmitRejection;

/// The fence a change recorded against `view` is submitted with: its
/// effective state, which moves with an ancestor as well as with the view.
fn view_state(repo: &Repository, view: &str) -> String {
    atomic_core::types::Base32::to_base32(&repo.sandbox_effective_state(view).unwrap())
}

/// Record in the cache (hydrated as FileStates would) and return the change.
fn record_in_cache(
    pair: &Pair,
    view: &str,
    live: &BTreeMap<u64, String>,
    what: &str,
) -> (atomic_core::types::Hash, Vec<u8>) {
    pair.hydrate(view, live);
    let options = RecordOptions::new()
        .with_all(true)
        .save_to_store(false)
        .apply_after_record(false);
    let outcome = pair
        .cache
        .record(
            pair.cache.require_working_copy_id().unwrap(),
            header(what),
            options,
        )
        .unwrap();
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
        .add(
            pair.cache.require_working_copy_id().unwrap(),
            "src/new.rs",
            TrackingOptions::default(),
        )
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
        .insert_submitted_change(&view, &submitted.effective, &hash, &bytes)
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
            live.insert(e.inode, e.path.clone());
            Ok(())
        })
        .unwrap()
        .unwrap();
    let skeleton = pair.host.export_sandbox_skeleton(&view, &live).unwrap();
    pair.cache.import_sandbox_skeleton(&skeleton).unwrap();
    hydrate_cache(&pair.host, &pair.cache, &view);
    assert_clean(&pair.cache);
    pair.cache
        .reindex_working_copy(pair.cache.require_working_copy_id().unwrap())
        .unwrap();
    write(pair.cache_root(), "src/new.rs", "pub fn n() { 2 }\n");
    let (hash, bytes) = record_in_cache(&pair, &view, &live, "second from the sandbox");
    pair.host
        .insert_submitted_change(&view, &submitted.effective, &hash, &bytes)
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
        host.provision_sandbox(
            host.require_working_copy_id().unwrap(),
            draft_dir.path().join("w"),
            "other",
        )
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
        let outcome = draft
            .record(
                draft.require_working_copy_id().unwrap(),
                header("on top of other"),
                options,
            )
            .unwrap();
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

#[test]
fn a_change_submitted_to_a_draft_view_is_on_that_view() {
    // Agent work lands on draft views: what lands on one renders there.
    let (pair, _, _) = pair(two_files);
    let mut host = pair.host;
    host.create_view_from("exp", "dev").unwrap();
    let cache_dir = TempDir::new().unwrap();
    drop(Repository::init(cache_dir.path()).unwrap());
    // As a remote sandbox's cache is opened: on its pointer's view.
    let mut cache = Repository::open_sandbox(cache_dir.path(), cache_dir.path(), "exp").unwrap();
    let mut live = BTreeMap::new();
    host.materialize_view_entries::<()>("exp", |entry| {
        live.insert(entry.inode, entry.path.clone());
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
    cache
        .import_sandbox_skeleton(&host.export_sandbox_skeleton("exp", &live).unwrap())
        .unwrap();
    cache
        .set_current_view(cache.require_working_copy_id().unwrap(), "exp")
        .unwrap();
    hydrate_cache(&host, &cache, "exp");
    assert_clean(&cache);
    cache
        .reindex_working_copy(cache.require_working_copy_id().unwrap())
        .unwrap();
    write(cache_dir.path(), "src/new.rs", "pub fn n() {}\n");
    write(cache_dir.path(), "README.md", "hello\non the draft\n");
    cache
        .add(
            cache.require_working_copy_id().unwrap(),
            "src/new.rs",
            TrackingOptions::default(),
        )
        .unwrap();
    hydrate_cache(&host, &cache, "exp");
    let options = RecordOptions::new()
        .with_all(true)
        .save_to_store(false)
        .apply_after_record(false);
    let outcome = cache
        .record(
            cache.require_working_copy_id().unwrap(),
            header("on the draft"),
            options,
        )
        .unwrap();
    let base = view_state(&host, "exp");
    host.insert_submitted_change("exp", &base, outcome.hash(), outcome.v3_bytes().unwrap())
        .unwrap()
        .expect("accepted");

    let tree = tree_of(&host, "exp");
    assert!(
        tree.contains(&("README.md".into(), b"hello\non the draft\n".to_vec())),
        "{tree:?}"
    );
    assert!(
        tree.contains(&("src/new.rs".into(), b"pub fn n() {}\n".to_vec())),
        "{tree:?}"
    );
    assert!(
        !tree_of(&host, "dev").iter().any(|(p, _)| p == "src/new.rs"),
        "only on the draft"
    );
    // What `sandbox stage` / `seal` write sees it too.
    let out = TempDir::new().unwrap();
    host.materialize_view_to("exp", out.path()).unwrap();
    assert_eq!(
        fs::read_to_string(out.path().join("src/new.rs"))
            .ok()
            .as_deref(),
        Some("pub fn n() {}\n")
    );

    // The next record builds on it: the refreshed skeleton knows the new file.
    let skeleton = host.after_sandbox_submit("exp", outcome.hash()).unwrap();
    cache.import_sandbox_skeleton(&skeleton).unwrap();
    cache
        .set_current_view(cache.require_working_copy_id().unwrap(), "exp")
        .unwrap();
    hydrate_cache(&host, &cache, "exp");
    assert_clean(&cache);
    cache
        .reindex_working_copy(cache.require_working_copy_id().unwrap())
        .unwrap();
    write(cache_dir.path(), "src/new.rs", "pub fn n() { 2 }\n");
    hydrate_cache(&host, &cache, "exp");
    let options = RecordOptions::new()
        .with_all(true)
        .save_to_store(false)
        .apply_after_record(false);
    let second = cache
        .record(
            cache.require_working_copy_id().unwrap(),
            header("again on the draft"),
            options,
        )
        .unwrap();
    let (change, _) =
        atomic_core::change::Change::deserialize(&mut &second.v3_bytes().unwrap()[..]).unwrap();
    assert!(
        !change
            .hunks()
            .iter()
            .any(|h| matches!(h, atomic_core::change::GraphOp::FileAdd { .. })),
        "an edit, not a second add"
    );
    host.insert_submitted_change(
        "exp",
        &view_state(&host, "exp"),
        second.hash(),
        second.v3_bytes().unwrap(),
    )
    .unwrap()
    .expect("accepted");
    assert!(tree_of(&host, "exp").contains(&("src/new.rs".into(), b"pub fn n() { 2 }\n".to_vec())));
}

#[test]
fn control_a_local_sandbox_record_on_a_draft_renders_on_it() {
    let host_dir = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    {
        let mut host = Repository::init(host_dir.path()).unwrap();
        two_files(&host);
        host.create_view_from("exp", "dev").unwrap();
        host.provision_sandbox(
            host.require_working_copy_id().unwrap(),
            work.path().join("w"),
            "exp",
        )
        .unwrap();
    }
    let sbx = Repository::open_existing(work.path().join("w")).unwrap();
    write(sbx.root(), "README.md", "hello\non the draft\n");
    record_and_apply(&sbx, "on the draft");
    drop(sbx);
    let host = Repository::open_existing(host_dir.path()).unwrap();
    let tree = tree_of(&host, "exp");
    assert!(
        tree.contains(&("README.md".into(), b"hello\non the draft\n".to_vec())),
        "{tree:?}"
    );
}

#[test]
fn parity_on_a_draft_view() {
    let host_dir = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    {
        let mut host = Repository::init(host_dir.path()).unwrap();
        two_files(&host);
        host.create_view_from("exp", "dev").unwrap();
        host.provision_sandbox(
            host.require_working_copy_id().unwrap(),
            work.path().join("w"),
            "exp",
        )
        .unwrap();
    }
    // The cache, from the draft.
    let cache_dir = TempDir::new().unwrap();
    drop(Repository::init(cache_dir.path()).unwrap());
    let mut cache = Repository::open_sandbox(cache_dir.path(), cache_dir.path(), "exp").unwrap();
    let skeleton = {
        let host = Repository::open_existing(host_dir.path()).unwrap();
        let mut live = BTreeMap::new();
        host.materialize_view_entries::<()>("exp", |entry| {
            live.insert(entry.inode, entry.path.clone());
            let path = cache_dir.path().join(&entry.path);
            if entry.kind != atomic_repository::ViewEntryKind::Directory {
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, &entry.content).unwrap();
            }
            Ok(())
        })
        .unwrap()
        .unwrap();
        host.export_sandbox_skeleton("exp", &live).unwrap()
    };
    cache.import_sandbox_skeleton(&skeleton).unwrap();
    cache
        .set_current_view(cache.require_working_copy_id().unwrap(), "exp")
        .unwrap();
    {
        let host = Repository::open_existing(host_dir.path()).unwrap();
        hydrate_cache(&host, &cache, "exp");
    }
    assert_clean(&cache);
    cache
        .reindex_working_copy(cache.require_working_copy_id().unwrap())
        .unwrap();

    let sbx = Repository::open_existing(work.path().join("w")).unwrap();
    write(sbx.root(), "README.md", "hello\non the draft\n");
    write(cache_dir.path(), "README.md", "hello\non the draft\n");
    let h = header("on the draft");
    let local = change_bytes(&sbx, h.clone());
    drop(sbx);
    let host = Repository::open_existing(host_dir.path()).unwrap();
    hydrate_cache(&host, &cache, "exp");
    let remote = change_bytes(&cache, h);
    assert_eq!(
        local.0, remote.0,
        "the cache records a different change on a draft"
    );
}

/// A draft's cache computes the effective state the repository computes, and
/// a change landing on the draft's *parent* moves it — the draft's own log
/// does not move, so its own Merkle state alone would let a change recorded
/// against the old perspective through.
#[test]
fn an_ancestor_moving_makes_a_draft_sandbox_stale() {
    let (pair, _, _) = pair(two_files);
    let mut host = pair.host;
    host.create_view_from("exp", "dev").unwrap();
    let cache_dir = TempDir::new().unwrap();
    drop(Repository::init(cache_dir.path()).unwrap());
    let mut cache = Repository::open_sandbox(cache_dir.path(), cache_dir.path(), "exp").unwrap();
    let mut live = BTreeMap::new();
    host.materialize_view_entries::<()>("exp", |entry| {
        live.insert(entry.inode, entry.path.clone());
        let path = cache_dir.path().join(&entry.path);
        if entry.kind != atomic_repository::ViewEntryKind::Directory {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, &entry.content).unwrap();
        }
        Ok(())
    })
    .unwrap()
    .unwrap();
    // The skeleton survives its own encoding.
    let skeleton = host.export_sandbox_skeleton("exp", &live).unwrap();
    let skeleton =
        atomic_repository::SandboxSkeleton::from_bytes(&skeleton.to_bytes().unwrap()).unwrap();
    cache.import_sandbox_skeleton(&skeleton).unwrap();
    cache
        .set_current_view(cache.require_working_copy_id().unwrap(), "exp")
        .unwrap();
    hydrate_cache(&host, &cache, "exp");
    assert_clean(&cache);
    cache
        .reindex_working_copy(cache.require_working_copy_id().unwrap())
        .unwrap();
    assert_eq!(
        cache.remote_sandbox_effective_state().unwrap(),
        view_state(&host, "exp"),
        "the cache and the repository agree on the draft's perspective"
    );
    let own_before = atomic_core::types::Base32::to_base32(&skeleton.view.state);

    write(cache_dir.path(), "README.md", "hello\nfrom the draft\n");
    let inodes = cache.sandbox_slice_inodes().unwrap();
    fetch_changes(&host, &cache, "exp");
    let slice = host.export_sandbox_slice("exp", &inodes).unwrap();
    let slice = atomic_repository::SandboxSlice::from_parts(
        &slice.rows_to_bytes().unwrap(),
        slice.spans.clone(),
    )
    .unwrap();
    cache.import_sandbox_slice(&slice).unwrap();
    let options = RecordOptions::new()
        .with_all(true)
        .save_to_store(false)
        .apply_after_record(false);
    let outcome = cache
        .record(
            cache.require_working_copy_id().unwrap(),
            header("on the draft"),
            options,
        )
        .unwrap();
    let base = cache.remote_sandbox_effective_state().unwrap();

    // `dev` moves; the draft's own log does not.
    write(host.root(), "src/lib.rs", "pub fn a() {}\n");
    record_and_apply(&host, "on dev");
    let own_after = atomic_core::types::Base32::to_base32(
        &host
            .export_sandbox_skeleton("exp", &BTreeMap::new())
            .unwrap()
            .view
            .state,
    );
    assert_eq!(own_before, own_after, "the draft's own log is unchanged");
    assert_ne!(view_state(&host, "exp"), base, "its perspective is not");

    let refused = host
        .insert_submitted_change("exp", &base, outcome.hash(), outcome.v3_bytes().unwrap())
        .unwrap();
    assert!(
        matches!(refused, Err(SubmitRejection::StaleView { .. })),
        "{refused:?}"
    );
}

#[cfg(unix)]
#[test]
fn canonical_cache_preserves_permissions_symlinks_and_rename_identity_after_reopen() {
    use std::os::unix::fs::{symlink, PermissionsExt};

    let (pair, view, _) = pair(|repo| {
        write(repo.root(), "run.sh", "#!/bin/sh\necho hello\n");
        fs::set_permissions(
            repo.root().join("run.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        symlink("run.sh", repo.root().join("link")).unwrap();
        let working_copy = repo.require_working_copy_id().unwrap();
        repo.add(working_copy, "run.sh", TrackingOptions::default())
            .unwrap();
        repo.add(working_copy, "link", TrackingOptions::default())
            .unwrap();
        record_and_apply(repo, "script and symlink");
    });
    let original = pair
        .host
        .resolve_single_path_claim(&view, "run.sh")
        .unwrap()
        .unwrap();
    let link = pair
        .host
        .resolve_single_path_claim(&view, "link")
        .unwrap()
        .unwrap();
    assert_eq!(
        fs::read_link(pair.cache_root().join("link")).unwrap(),
        Path::new("run.sh")
    );
    assert_eq!(
        fs::metadata(pair.cache_root().join("run.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );

    for (from, to, mode) in [
        ("run.sh", "renamed.sh", 0o644),
        ("renamed.sh", "run.sh", 0o755),
    ] {
        fs::rename(pair.cache_root().join(from), pair.cache_root().join(to)).unwrap();
        fs::set_permissions(pair.cache_root().join(to), fs::Permissions::from_mode(mode)).unwrap();
        fs::remove_file(pair.cache_root().join("link")).unwrap();
        symlink(to, pair.cache_root().join("link")).unwrap();
        hydrate_cache(&pair.host, &pair.cache, &view);
        let outcome = pair
            .cache
            .record(
                pair.cache.require_working_copy_id().unwrap(),
                header("rename with permissions and symlink target"),
                RecordOptions::new()
                    .with_all(true)
                    .save_to_store(false)
                    .apply_after_record(false),
            )
            .unwrap();
        pair.host
            .insert_submitted_change(
                &view,
                &view_state(&pair.host, &view),
                outcome.hash(),
                outcome.v3_bytes().unwrap(),
            )
            .unwrap()
            .expect("canonical attribute and rename change is accepted");
        for (path, expected) in [(to, original), ("link", link)] {
            let actual = pair
                .host
                .resolve_single_path_claim(&view, path)
                .unwrap()
                .unwrap_or_else(|| {
                    panic!(
                        "{path} missing after {from} -> {to}: {:?}",
                        outcome.change().hunks()
                    )
                });
            assert_eq!(
                (actual.1, actual.2),
                (expected.1, expected.2),
                "{path} retains its graph identity"
            );
        }
        assert!(pair
            .host
            .resolve_single_path_claim(&view, from)
            .unwrap()
            .is_none());
        let skeleton = pair
            .host
            .after_sandbox_submit(&view, outcome.hash())
            .unwrap();
        pair.cache.import_sandbox_skeleton(&skeleton).unwrap();
        hydrate_cache(&pair.host, &pair.cache, &view);
        assert_clean(&pair.cache);
        assert_eq!(
            fs::read_link(pair.host_root().join("link")).unwrap(),
            Path::new(to)
        );
        assert_eq!(
            fs::metadata(pair.host_root().join(to))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            mode
        );
    }

    let Pair { _dirs, host, cache } = pair;
    let root = cache.root().to_path_buf();
    drop(cache);
    let reopened = Repository::open_existing(&root).unwrap();
    assert_clean(&reopened);
    assert_eq!(
        reopened
            .resolve_single_path_claim(&view, "run.sh")
            .unwrap()
            .unwrap()
            .1,
        original.1
    );
    assert_eq!(
        reopened.get_file_content("link").unwrap(),
        Some(b"run.sh".to_vec())
    );
    assert_eq!(tree_of(&reopened, &view), tree_of(&host, &view));
}

#[cfg(unix)]
#[test]
fn a_permission_only_change_has_the_same_bytes_in_the_cache_and_owner() {
    use std::os::unix::fs::PermissionsExt;
    let (pair, view, live) = pair(two_files);
    pair.edit(|root| {
        fs::set_permissions(root.join("README.md"), fs::Permissions::from_mode(0o755)).unwrap()
    });
    assert_parity(&pair, &view, &live, "make executable");
}

/// Import the selected view while the owner keeps its own checkout selected.
fn cache_for_view(host: &Repository, view: &str) -> (TempDir, Repository) {
    let dir = TempDir::new().unwrap();
    let mut cache = Repository::init(dir.path()).unwrap();
    let mut live = BTreeMap::new();
    host.materialize_view_entries::<()>(view, |entry| {
        live.insert(entry.inode, entry.path.clone());
        atomic_repository::write_sandbox_entry(
            dir.path(),
            &entry.path,
            entry.kind,
            u32::from(entry.mode),
            &entry.content,
        )
        .unwrap();
        Ok(())
    })
    .unwrap()
    .unwrap();
    let skeleton = host.export_sandbox_skeleton(view, &live).unwrap();
    cache.import_sandbox_skeleton(&skeleton).unwrap();
    cache
        .set_current_view(cache.require_working_copy_id().unwrap(), view)
        .unwrap();
    hydrate_cache(host, &cache, view);
    assert_clean(&cache);
    (dir, cache)
}

#[test]
fn child_deletion_does_not_import_the_owners_live_reverse_path() {
    use atomic_core::pristine::TreeTxnT;
    let dir = TempDir::new().unwrap();
    let mut host = Repository::init(dir.path()).unwrap();
    two_files(&host);
    let identity = host
        .resolve_single_path_claim("dev", "README.md")
        .unwrap()
        .unwrap();
    host.create_view_from("child", "dev").unwrap();
    let working_copy = host.require_working_copy_id().unwrap();
    host.switch_view(working_copy, "child").unwrap();
    fs::remove_file(dir.path().join("README.md")).unwrap();
    record_and_apply(&host, "delete only on child");
    host.switch_view(working_copy, "dev").unwrap();
    assert!(dir.path().join("README.md").exists());
    assert_eq!(
        host.get_file_content_on_view("README.md", "child").unwrap(),
        None
    );

    let (cache_dir, cache) = cache_for_view(&host, "child");
    assert!(!cache_dir.path().join("README.md").exists());
    assert_eq!(cache.get_file_inode("README.md").unwrap(), None);
    assert_eq!(cache.get_file_content("README.md").unwrap(), None);
    assert_eq!(
        cache
            .pristine()
            .read_txn()
            .unwrap()
            .inode_position(identity.1)
            .unwrap(),
        Some(identity.2)
    );

    // Reappearance is an undelete of the child's original identity.
    write(cache_dir.path(), "README.md", "restored in child\n");
    let status = cache
        .status(
            cache.require_working_copy_id().unwrap(),
            StatusOptions::default(),
        )
        .unwrap();
    let restored = status
        .entries()
        .iter()
        .find(|entry| entry.path() == Path::new("README.md"))
        .unwrap();
    assert_eq!(restored.inode(), Some(identity.1));
    let outcome = cache
        .record(
            cache.require_working_copy_id().unwrap(),
            header("restore child file"),
            RecordOptions::new()
                .with_all(true)
                .save_to_store(false)
                .apply_after_record(false),
        )
        .unwrap();
    host.insert_submitted_change(
        "child",
        &view_state(&host, "child"),
        outcome.hash(),
        outcome.v3_bytes().unwrap(),
    )
    .unwrap()
    .expect("undelete accepted");
    let restored = host
        .resolve_single_path_claim("child", "README.md")
        .unwrap()
        .unwrap();
    assert_eq!((restored.1, restored.2), (identity.1, identity.2));
    assert_eq!(
        host.get_file_content("README.md").unwrap(),
        Some(b"hello\nthere\nworld\n".to_vec())
    );
}

#[test]
fn a_shared_child_inherits_its_parent_and_fences_parent_changes() {
    use atomic_core::pristine::{MutTxnT, ViewScope, ViewTxnT};
    let dir = TempDir::new().unwrap();
    let host = Repository::init(dir.path()).unwrap();
    two_files(&host);
    {
        let mut txn = host.pristine().write_txn().unwrap();
        let parent = txn.get_view("dev").unwrap().unwrap();
        txn.create_view("shared-child", ViewScope::Shared, Some(parent.id))
            .unwrap();
        txn.commit().unwrap();
    }
    let (_cache_dir, cache) = cache_for_view(&host, "shared-child");
    let base = view_state(&host, "shared-child");
    assert_eq!(cache.remote_sandbox_effective_state().unwrap(), base);
    assert_eq!(
        cache.get_file_content("README.md").unwrap(),
        host.get_file_content("README.md").unwrap()
    );
    write(cache.root(), "README.md", "from shared child\n");
    let (hash, bytes) = change_bytes(&cache, header("shared child edit"));
    let hash =
        <atomic_core::types::Hash as atomic_core::types::Base32>::from_base32(hash.as_bytes())
            .unwrap();
    write(host.root(), "src/lib.rs", "parent has moved\n");
    record_and_apply(&host, "advance parent");
    assert_ne!(view_state(&host, "shared-child"), base);
    assert!(matches!(
        host.insert_submitted_change("shared-child", &base, &hash, &bytes)
            .unwrap(),
        Err(SubmitRejection::StaleView { .. })
    ));
}

#[test]
fn a_tip_only_view_exports_and_hydrates_its_transitive_dependencies() {
    use atomic_core::pristine::{GraphTxnT, MutTxnT, ViewTxnT};
    let dir = TempDir::new().unwrap();
    let mut host = Repository::init(dir.path()).unwrap();
    let working_copy = host.require_working_copy_id().unwrap();
    let mut hashes = Vec::new();
    for content in ["base\n", "base\ntip\n"] {
        write(host.root(), "chain.txt", content);
        if hashes.is_empty() {
            host.add(working_copy, "chain.txt", TrackingOptions::default())
                .unwrap();
        }
        let outcome = host
            .record(
                working_copy,
                header("dependency chain"),
                RecordOptions::new()
                    .with_all(true)
                    .save_to_store(true)
                    .apply_after_record(false),
            )
            .unwrap();
        host.write_recorded(&outcome, InsertOptions::default())
            .unwrap();
        hashes.push(*outcome.hash());
    }
    assert!(host
        .load_change(&hashes[1])
        .unwrap()
        .dependencies()
        .contains(&hashes[0]));
    host.create_shared_view("tip-only").unwrap();
    {
        let mut txn = host.pristine().write_txn().unwrap();
        let mut view = txn.get_view("tip-only").unwrap().unwrap();
        let tip = txn.get_internal(&hashes[1]).unwrap().unwrap();
        txn.put_change(&mut view, tip, &hashes[1]).unwrap();
        txn.update_view(&view).unwrap();
        txn.commit().unwrap();
    }
    let skeleton = host.current_sandbox_skeleton("tip-only").unwrap();
    assert_eq!(skeleton.view.visible.len(), 1);
    assert!(skeleton.ancestors.is_empty());
    assert_eq!(skeleton.rows.ids.len(), 2);
    let (_cache_dir, cache) = cache_for_view(&host, "tip-only");
    assert!(cache.has_change(&hashes[0]));
    assert!(cache.has_change(&hashes[1]));
    assert!(cache.missing_sandbox_changes().unwrap().is_empty());
    assert_eq!(
        cache.get_file_content("chain.txt").unwrap(),
        Some(b"base\ntip\n".to_vec())
    );
    assert_eq!(
        cache
            .resolve_single_path_claim("tip-only", "chain.txt")
            .unwrap(),
        host.resolve_single_path_claim("tip-only", "chain.txt")
            .unwrap()
    );
    for repo in [&host, &cache] {
        write(repo.root(), "chain.txt", "base\ntip\nnext\n");
    }
    let h = header("on top of transitive history");
    assert_eq!(change_bytes(&cache, h.clone()), change_bytes(&host, h));
}

#[cfg(unix)]
#[test]
fn a_deleted_path_reappearing_with_new_attributes_restores_its_identity() {
    use atomic_repository::{FileStatus, ViewEntryKind};
    use std::os::unix::fs::{symlink, PermissionsExt};

    for as_symlink in [false, true] {
        let dir = TempDir::new().unwrap();
        let host = Repository::init(dir.path()).unwrap();
        write(host.root(), "returning", "baseline\n");
        host.add(
            host.require_working_copy_id().unwrap(),
            "returning",
            TrackingOptions::default(),
        )
        .unwrap();
        record_and_apply(&host, "original");
        let original = host
            .resolve_single_path_claim("dev", "returning")
            .unwrap()
            .unwrap();
        fs::remove_file(host.root().join("returning")).unwrap();
        record_and_apply(&host, "delete");
        let (cache_dir, cache) = cache_for_view(&host, "dev");
        if as_symlink {
            symlink("missing-target", cache_dir.path().join("returning")).unwrap();
        } else {
            write(cache_dir.path(), "returning", "new executable bytes\n");
            fs::set_permissions(
                cache_dir.path().join("returning"),
                fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        let status = cache
            .status(
                cache.require_working_copy_id().unwrap(),
                StatusOptions::default(),
            )
            .unwrap();
        let entry = status
            .entries()
            .iter()
            .find(|entry| entry.path() == Path::new("returning"))
            .unwrap();
        assert_eq!(
            entry.status(),
            FileStatus::Added,
            "lifecycle takes precedence over attributes"
        );
        assert_eq!(entry.inode(), Some(original.1));
        let outcome = cache
            .record(
                cache.require_working_copy_id().unwrap(),
                header("reappear"),
                RecordOptions::new()
                    .with_all(true)
                    .save_to_store(false)
                    .apply_after_record(false),
            )
            .unwrap();
        host.insert_submitted_change(
            "dev",
            &view_state(&host, "dev"),
            outcome.hash(),
            outcome.v3_bytes().unwrap(),
        )
        .unwrap()
        .unwrap();
        let restored = host
            .resolve_single_path_claim("dev", "returning")
            .unwrap()
            .unwrap();
        assert_eq!((restored.1, restored.2), (original.1, original.2));
        host.materialize_view_entries::<()>("dev", |entry| {
            if entry.path == "returning" {
                if as_symlink {
                    assert_eq!(entry.kind, ViewEntryKind::Symlink);
                    assert_eq!(entry.content, b"missing-target");
                } else {
                    assert_eq!(entry.kind, ViewEntryKind::File);
                    assert_eq!(entry.mode, 0o755);
                    assert_eq!(entry.content, b"new executable bytes\n");
                }
            }
            Ok(())
        })
        .unwrap()
        .unwrap();
        let skeleton = host.after_sandbox_submit("dev", outcome.hash()).unwrap();
        cache.import_sandbox_skeleton(&skeleton).unwrap();
        hydrate_cache(&host, &cache, "dev");
        assert_clean(&cache);
    }
}
