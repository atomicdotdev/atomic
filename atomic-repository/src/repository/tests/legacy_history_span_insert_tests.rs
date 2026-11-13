use super::*;
use crate::apply::CrossViewInsertOptions;
use atomic_core::pristine::{CapabilityMutTxnT, MutTxnT, SPAN_RANGE_CAPABILITY};

/// A change recorded by the released atomic 0.19.1: it adds `a.txt` holding
/// `LEGACY_CONTENT`, depends on nothing, and is stored in ATOM file-header
/// version 1, as every change recorded before 0.20 is.
const LEGACY_FILE_ADD_HEX: &str = include_str!(
    "../../../../atomic-core/src/change/format_v3/fixtures/atomic_0_19_1_file_add.hex"
);
const LEGACY_CONTENT: &[u8] = b"alpha\nbeta\ngamma\n";
const EDITED_CONTENT: &[u8] = b"alpha\nbeta\ngamma\nspan line\n";

fn legacy_file_add() -> (Hash, Vec<u8>) {
    let hex: String = LEGACY_FILE_ADD_HEX
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect();
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect();
    let (change, hash) = Change::deserialize(&mut std::io::Cursor::new(&bytes)).unwrap();
    assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 1);
    assert_ne!(
        change.hash().unwrap(),
        hash,
        "the current writer gives a version 1 change a different hash"
    );
    (hash, bytes)
}

fn record_all(repo: &TestRepository, message: &str) -> Hash {
    *repo
        .record(
            ChangeHeader::new(message),
            RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true)
                .sync_vault(false)
                .enrich_kg(false),
        )
        .unwrap()
        .hash()
}

fn enable_spans(repo: &TestRepository) {
    let mut txn = repo.pristine().write_txn().unwrap();
    txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
    txn.commit().unwrap();
}

/// A repository whose only history is the 0.19.1 change, checked out.
fn repo_with_legacy_history() -> (tempfile::TempDir, TestRepository, Hash) {
    let (legacy, bytes) = legacy_file_add();
    let (temp, repo) = create_temp_repo();
    repo.import_change_bytes(&legacy, &bytes).unwrap();
    repo.insert_change_rec(&legacy, InsertOptions::default())
        .unwrap();
    std::fs::write(temp.path().join("a.txt"), LEGACY_CONTENT).unwrap();
    assert_eq!(content(&repo, "dev"), LEGACY_CONTENT);
    (temp, repo, legacy)
}

fn content(repo: &TestRepository, view: &str) -> Vec<u8> {
    repo.get_file_contents_on_view(&["a.txt".to_string()], view)
        .unwrap()
        .remove(0)
        .expect("a.txt is present on the view")
}

/// The sender turns spans on and edits the file the 0.19.1 change added;
/// returns the span change, which depends on the 0.19.1 change.
fn record_span_edit(temp: &tempfile::TempDir, repo: &TestRepository, legacy: &Hash) -> Hash {
    enable_spans(repo);
    std::fs::write(temp.path().join("a.txt"), EDITED_CONTENT).unwrap();
    let span_edit = record_all(repo, "span edit");
    let change = repo.load_change(&span_edit).unwrap();
    assert!(
        change.hashed.span.is_some(),
        "the edit must be a span change"
    );
    assert!(
        change.dependencies().contains(legacy),
        "the span change must depend on the 0.19.1 change"
    );
    span_edit
}

fn span_edit_bytes(repo: &TestRepository, hash: &Hash) -> Vec<u8> {
    std::fs::read(repo.change_store().change_path(hash)).unwrap()
}

/// Pulling: another clone of the same history receives the span change. Its
/// dependency was recorded before 0.20, so its identity is the hash of its
/// original bytes, not of a re-encoding by the current writer.
#[test]
fn span_change_on_pre_0_20_history_inserts_into_another_clone() {
    let (sender_dir, sender, legacy) = repo_with_legacy_history();
    let span_edit = record_span_edit(&sender_dir, &sender, &legacy);
    let bytes = span_edit_bytes(&sender, &span_edit);

    for recursive in [false, true] {
        let (_receiver_dir, receiver, _) = repo_with_legacy_history();
        receiver.import_change_bytes(&span_edit, &bytes).unwrap();
        if recursive {
            receiver
                .insert_change_rec(&span_edit, InsertOptions::default())
                .unwrap();
        } else {
            receiver
                .insert_change(&span_edit, InsertOptions::default())
                .unwrap();
        }
        assert_eq!(content(&receiver, "dev"), EDITED_CONTENT);
    }
}

/// Cloning: a fresh repository receives the whole history at once.
#[test]
fn span_change_and_its_pre_0_20_dependency_insert_into_an_empty_repository() {
    let (sender_dir, sender, legacy) = repo_with_legacy_history();
    let span_edit = record_span_edit(&sender_dir, &sender, &legacy);
    let (_, legacy_bytes) = legacy_file_add();

    let (_receiver_dir, receiver) = create_temp_repo();
    receiver
        .import_change_bytes(&legacy, &legacy_bytes)
        .unwrap();
    receiver
        .import_change_bytes(&span_edit, &span_edit_bytes(&sender, &span_edit))
        .unwrap();
    receiver
        .insert_change_rec(&span_edit, InsertOptions::default())
        .unwrap();
    assert_eq!(content(&receiver, "dev"), EDITED_CONTENT);
}

/// No remote involved: the span edit is recorded on one view and promoted to
/// another view of the same repository.
#[test]
fn span_change_on_pre_0_20_history_inserts_from_another_view() {
    let (temp, mut repo, legacy) = repo_with_legacy_history();
    repo.create_view_from("feature", "dev").unwrap();
    repo.switch_view("feature").unwrap();
    record_span_edit(&temp, &repo, &legacy);
    assert_eq!(content(&repo, "dev"), LEGACY_CONTENT);

    repo.switch_view("dev").unwrap();
    repo.insert_from_view(CrossViewInsertOptions::new("feature", "dev"))
        .unwrap();
    assert_eq!(content(&repo, "dev"), EDITED_CONTENT);
}

/// The identity check still holds for the stored bytes: a dependency whose
/// file no longer decodes to its own hash must stop the insert.
#[test]
fn span_insert_still_refuses_a_dependency_stored_under_the_wrong_hash() {
    let (sender_dir, sender, legacy) = repo_with_legacy_history();
    let span_edit = record_span_edit(&sender_dir, &sender, &legacy);
    let bytes = span_edit_bytes(&sender, &span_edit);

    let (receiver_dir, receiver, _) = repo_with_legacy_history();
    receiver.import_change_bytes(&span_edit, &bytes).unwrap();
    // Replace the stored dependency with a different, valid change.
    let other = create_test_change("not the dependency");
    let mut other_bytes = Vec::new();
    other.serialize(&mut other_bytes).unwrap();
    std::fs::write(receiver.change_store().change_path(&legacy), other_bytes).unwrap();
    drop(receiver);
    let receiver = TestRepository::new(Repository::open(receiver_dir.path()).unwrap());

    assert!(receiver
        .insert_change_rec(&span_edit, InsertOptions::default())
        .is_err());
}
