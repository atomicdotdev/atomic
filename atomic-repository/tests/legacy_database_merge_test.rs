//! Opening a repository in the legacy `pristine.redb` + `changes.redb` layout
//! merges both into `.atomic/atomic.redb`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use atomic_core::change::{Author, ChangeHeader};
use atomic_core::pristine::tables::ATOMIC_META;
use atomic_core::pristine::Pristine;
use atomic_core::types::Hash;
use atomic_repository::history::HistoryOptions;
use atomic_repository::record::RecordOptions;
use atomic_repository::redb_change_store::RedbChangeStore;
use atomic_repository::{
    Repository, RepositoryError, DATABASE_FILE, LEGACY_CHANGE_STORE_FILE, LEGACY_DIR,
    LEGACY_PRISTINE_FILE,
};
use tempfile::TempDir;

const SESSION: &str = "legacy-session";

struct Legacy {
    _temp: TempDir,
    root: PathBuf,
    history: Vec<Hash>,
    views: Vec<String>,
}

impl Legacy {
    fn dot_dir(&self) -> PathBuf {
        self.root.join(".atomic")
    }

    fn retired(&self) -> Vec<PathBuf> {
        let legacy = self.dot_dir().join(LEGACY_DIR);
        let Ok(entries) = fs::read_dir(legacy) else {
            return Vec::new();
        };
        entries.map(|entry| entry.unwrap().path()).collect()
    }
}

fn record(repo: &Repository, root: &Path, name: &str, message: &str) {
    fs::write(root.join(name), format!("{message}\n")).unwrap();
    repo.add(
        repo.require_working_copy_id().unwrap(),
        name,
        Default::default(),
    )
    .unwrap();
    let header = ChangeHeader::builder()
        .message(message)
        .author(Author::new("Test", Some("test@example.com")))
        .build();
    repo.record(
        repo.require_working_copy_id().unwrap(),
        header,
        RecordOptions::default(),
    )
    .unwrap();
}

fn history(repo: &Repository) -> Vec<Hash> {
    repo.log(HistoryOptions::default())
        .unwrap()
        .into_iter()
        .map(|entry| entry.hash)
        .collect()
}

/// A repository as an older atomic left it: graph state in `pristine.redb`
/// (without the newer metadata table) and, optionally, the provenance
/// journal in `changes.redb`.
fn legacy_repository(with_change_store: bool) -> Legacy {
    legacy_repository_with_setup(with_change_store, |_| {})
}

fn legacy_repository_with_setup(
    with_change_store: bool,
    setup: impl FnOnce(&Repository),
) -> Legacy {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("repo");
    let mut repo = Repository::init(&root).unwrap();
    record(&repo, &root, "a.txt", "first");
    record(&repo, &root, "b.txt", "second");
    repo.create_view("feature").unwrap();
    let history = history(&repo);
    let views = repo.list_views().unwrap();
    setup(&repo);
    drop(repo);

    let dot_dir = root.join(".atomic");
    let pristine = dot_dir.join(LEGACY_PRISTINE_FILE);
    fs::rename(dot_dir.join(DATABASE_FILE), &pristine).unwrap();
    {
        let db = redb::Database::open(&pristine).unwrap();
        let txn = db.begin_write().unwrap();
        txn.delete_table(ATOMIC_META).unwrap();
        txn.commit().unwrap();
    }
    if with_change_store {
        let store = RedbChangeStore::open(dot_dir.join(LEGACY_CHANGE_STORE_FILE)).unwrap();
        store
            .reserve_provenance_turn(SESSION, 1, 1_700_000_000)
            .unwrap();
    }
    Legacy {
        _temp: temp,
        root,
        history,
        views,
    }
}

fn assert_merged(legacy: &Legacy) {
    let dot_dir = legacy.dot_dir();
    assert!(dot_dir.join(DATABASE_FILE).is_file());
    assert!(!dot_dir.join(LEGACY_PRISTINE_FILE).exists());
    assert!(!dot_dir.join(LEGACY_CHANGE_STORE_FILE).exists());
    assert!(!dot_dir.join("atomic.redb.merging").exists());
    let retired = legacy.retired();
    assert_eq!(retired.len(), 1, "one legacy directory: {retired:?}");
    assert!(retired[0].join(LEGACY_PRISTINE_FILE).is_file());
}

#[test]
fn opening_a_legacy_repository_merges_both_databases() {
    let legacy = legacy_repository(true);

    let repo = Repository::open(&legacy.root).unwrap();

    assert_eq!(history(&repo), legacy.history);
    assert_eq!(repo.list_views().unwrap(), legacy.views);
    let store = repo.redb_change_store().unwrap();
    let turn = store.get_provenance_turn_for(SESSION, 1).unwrap();
    assert!(
        turn.is_some(),
        "the provenance journal moved into atomic.redb"
    );
    drop((store, repo));

    assert_merged(&legacy);
    assert!(legacy.retired()[0].join(LEGACY_CHANGE_STORE_FILE).is_file());

    let reopened = Repository::open(&legacy.root).unwrap();
    assert_eq!(history(&reopened), legacy.history);
    assert_eq!(
        legacy.retired().len(),
        1,
        "a second open does not merge again"
    );
}

#[test]
fn read_only_open_merges_a_legacy_repository() {
    let legacy = legacy_repository(true);

    let repo = Repository::open_readonly(&legacy.root).unwrap();

    assert_eq!(history(&repo), legacy.history);
    drop(repo);
    assert_merged(&legacy);
}

#[test]
fn legacy_repository_without_a_change_store_merges() {
    let legacy = legacy_repository(false);

    let repo = Repository::open_existing(&legacy.root).unwrap();

    assert_eq!(history(&repo), legacy.history);
    let store = repo.redb_change_store().unwrap();
    assert!(store.get_provenance_turn_for(SESSION, 1).unwrap().is_none());
    drop((store, repo));
    assert_merged(&legacy);
}

#[test]
fn legacy_repository_is_still_found_before_merging() {
    let legacy = legacy_repository(true);

    assert!(Repository::is_repository(&legacy.root));
    assert_eq!(
        Repository::find_root(&legacy.root.join("a.txt")).unwrap(),
        legacy.root.canonicalize().unwrap()
    );
}

#[test]
fn concurrent_opens_of_a_legacy_repository_all_succeed() {
    for _ in 0..4 {
        let legacy = legacy_repository(true);
        let openers: Vec<_> = (0..16)
            .map(|_| {
                let root = legacy.root.clone();
                std::thread::spawn(move || {
                    Repository::open_readonly(&root).map(|repo| history(&repo))
                })
            })
            .collect();
        for opener in openers {
            assert_eq!(opener.join().unwrap().unwrap(), legacy.history);
        }
        assert_merged(&legacy);
    }
}

#[test]
fn a_stale_merge_scratch_file_is_replaced() {
    let legacy = legacy_repository(true);
    fs::write(
        legacy.dot_dir().join("atomic.redb.merging"),
        b"half written",
    )
    .unwrap();

    let repo = Repository::open(&legacy.root).unwrap();

    assert_eq!(history(&repo), legacy.history);
    drop(repo);
    assert_merged(&legacy);
}

#[test]
fn an_interrupted_retirement_is_finished_on_the_next_open() {
    let legacy = legacy_repository(true);
    drop(Repository::open(&legacy.root).unwrap());
    // Simulate a crash after publishing atomic.redb but before the legacy
    // files were moved away.
    let retired = legacy.retired().remove(0);
    for file in [LEGACY_PRISTINE_FILE, LEGACY_CHANGE_STORE_FILE] {
        fs::rename(retired.join(file), legacy.dot_dir().join(file)).unwrap();
    }

    let repo = Repository::open(&legacy.root).unwrap();

    assert_eq!(history(&repo), legacy.history);
    drop(repo);
    assert_merged(&legacy);
    assert!(retired.join(LEGACY_CHANGE_STORE_FILE).is_file());
}

#[test]
fn both_layouts_without_a_merge_record_are_refused() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("repo");
    drop(Repository::init(&root).unwrap());
    let dot_dir = root.join(".atomic");
    fs::copy(
        dot_dir.join(DATABASE_FILE),
        dot_dir.join(LEGACY_PRISTINE_FILE),
    )
    .unwrap();

    let error = Repository::open(&root).unwrap_err();

    assert!(
        matches!(&error, RepositoryError::InvalidRepository { reason } if reason.contains(LEGACY_PRISTINE_FILE)),
        "{error}"
    );
    assert!(
        dot_dir.join(LEGACY_PRISTINE_FILE).is_file(),
        "nothing is moved"
    );
}

#[test]
fn a_held_legacy_change_store_stops_the_merge_with_guidance() {
    let legacy = legacy_repository(true);
    let holder = RedbChangeStore::open(legacy.dot_dir().join(LEGACY_CHANGE_STORE_FILE)).unwrap();

    let error = Repository::open(&legacy.root).unwrap_err();

    assert!(
        matches!(&error, RepositoryError::LegacyChangeStoreBusy { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("database-owner shutdown"));
    assert!(!legacy.dot_dir().join(DATABASE_FILE).exists());
    assert!(legacy.dot_dir().join(LEGACY_PRISTINE_FILE).is_file());
    drop(holder);

    assert_eq!(
        history(&Repository::open(&legacy.root).unwrap()),
        legacy.history
    );
}

#[test]
fn a_held_legacy_pristine_is_reported_busy() {
    let legacy = legacy_repository(true);
    let holder = Pristine::open(legacy.dot_dir().join(LEGACY_PRISTINE_FILE)).unwrap();

    let result = Repository::open_existing_wait(&legacy.root, Duration::from_millis(100));

    assert!(matches!(result, Err(RepositoryError::DatabaseBusy)));
    drop(holder);
    assert!(!legacy.dot_dir().join(DATABASE_FILE).exists());
}

#[test]
fn sandbox_open_merges_the_canonical_repository() {
    // Register the sandbox before creating the legacy fixture so migration
    // must carry its durable working-copy identity into the consolidated DB.
    let legacy = legacy_repository_with_setup(true, |repo| {
        let sandbox = repo.root().parent().unwrap().join("sandbox");
        repo.provision_sandbox(repo.require_working_copy_id().unwrap(), &sandbox, "dev")
            .unwrap();
    });
    let sandbox = legacy.root.parent().unwrap().join("sandbox");

    let opened = Repository::open_existing(&sandbox).unwrap();

    assert!(opened.is_sandbox());
    assert_eq!(history(&opened), legacy.history);
    drop(opened);
    assert_merged(&legacy);
    assert!(!sandbox.join(".atomic").join(DATABASE_FILE).exists());
}

#[test]
fn the_shared_change_store_uses_the_repository_handle() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("repo");
    let repo = Repository::init(&root).unwrap();

    let store = repo.redb_change_store().unwrap();
    store
        .reserve_provenance_turn(SESSION, 1, 1_700_000_000)
        .unwrap();

    let second = RedbChangeStore::open_existing(repo.database_path());
    assert!(matches!(second, Err(error) if error.is_database_busy()));
    drop((store, repo));

    let reopened =
        RedbChangeStore::open_existing(root.join(".atomic").join(DATABASE_FILE)).unwrap();
    assert!(reopened
        .get_provenance_turn_for(SESSION, 1)
        .unwrap()
        .is_some());
}

#[test]
fn opening_the_store_never_creates_a_missing_database() {
    let temp = TempDir::new().unwrap();
    let missing = temp.path().join(DATABASE_FILE);

    assert!(RedbChangeStore::open_existing(&missing).is_err());
    assert!(!missing.exists());
}
