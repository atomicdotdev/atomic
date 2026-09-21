use redb::ReadableDatabase;
use super::*;

use atomic_core::pristine::{
    PristineError, RequiredRepositoryCapability, CHANGE_FORMAT_VNEXT_CAPABILITY,
    PATH_CLAIM_SCHEMA_KEY, PRISTINE_META, REQUIRED_CAPABILITY_PREFIX,
};

fn set_raw_requirement(root: &Path, capability: &str, version: u32) {
    let database = redb::Database::open(root.join(".atomic/pristine.redb")).unwrap();
    let write = database.begin_write().unwrap();
    {
        let mut metadata = write.open_table(PRISTINE_META).unwrap();
        let key = format!("{REQUIRED_CAPABILITY_PREFIX}{capability}");
        metadata.insert(key.as_str(), version).unwrap();
    }
    write.commit().unwrap();
}

fn clear_path_claim_marker(root: &Path) {
    let database = redb::Database::open(root.join(".atomic/pristine.redb")).unwrap();
    let write = database.begin_write().unwrap();
    {
        let mut metadata = write.open_table(PRISTINE_META).unwrap();
        metadata.remove(PATH_CLAIM_SCHEMA_KEY).unwrap();
    }
    write.commit().unwrap();
}

fn expect_unsupported(result: Result<Repository, RepositoryError>, capability: &str) {
    let error = match result {
        Ok(_) => panic!("repository open must reject unsupported capabilities"),
        Err(error) => error,
    };
    match &error {
        RepositoryError::UnsupportedRequiredCapabilities { source } => assert!(matches!(
            source,
            PristineError::UnsupportedRequiredCapabilities { .. }
        )),
        other => panic!("expected typed unsupported capability error, got {other}"),
    }
    let message = error.to_string();
    assert!(message.contains(capability));
    assert!(message.contains("upgrade Atomic"));
}

fn atom_file_header(version: u32) -> Vec<u8> {
    let mut bytes = vec![0; 64];
    bytes[..4].copy_from_slice(b"ATOM");
    bytes[4..8].copy_from_slice(&version.to_le_bytes());
    bytes
}

#[test]
fn all_repository_open_modes_reject_unknown_requirements() {
    let (temp, repo) = create_temp_repo();
    drop(repo);
    set_raw_requirement(temp.path(), "future-format", 1);

    expect_unsupported(Repository::open(temp.path()), "future-format");
    expect_unsupported(Repository::open_existing(temp.path()), "future-format");
    expect_unsupported(Repository::open_readonly(temp.path()), "future-format");
    expect_unsupported(
        Repository::open_readonly_for_operation_inspection(temp.path()),
        "future-format",
    );
    expect_unsupported(
        Repository::open_readonly_for_native_repair(temp.path()),
        "future-format",
    );
    expect_unsupported(
        Repository::open_for_native_repair(temp.path()),
        "future-format",
    );
}

#[test]
fn higher_known_requirement_is_typed_and_actionable() {
    let (temp, repo) = create_temp_repo();
    drop(repo);
    set_raw_requirement(
        temp.path(),
        CHANGE_FORMAT_VNEXT_CAPABILITY.id(),
        CHANGE_FORMAT_VNEXT_CAPABILITY.minimum_version() + 1,
    );

    let error = match Repository::open_readonly(temp.path()) {
        Ok(_) => panic!("higher capability version must be rejected"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        RepositoryError::UnsupportedRequiredCapabilities { .. }
    ));
    let message = error.to_string();
    assert!(message.contains("change-format-vnext"));
    assert!(message.contains("version 2"));
    assert!(message.contains("supports through version 1"));
}

#[test]
fn unsupported_open_starts_neither_migration_nor_change_store_creation() {
    let (temp, repo) = create_temp_repo();
    drop(repo);
    clear_path_claim_marker(temp.path());
    std::fs::remove_dir_all(temp.path().join(".atomic/changes")).unwrap();
    set_raw_requirement(temp.path(), "future-format", 1);

    expect_unsupported(Repository::open(temp.path()), "future-format");
    assert!(!temp.path().join(".atomic/changes").exists());

    let database = redb::Database::open(temp.path().join(".atomic/pristine.redb")).unwrap();
    let read = database.begin_read().unwrap();
    let metadata = read.open_table(PRISTINE_META).unwrap();
    assert!(metadata.get(PATH_CLAIM_SCHEMA_KEY).unwrap().is_none());
}

#[test]
fn sandbox_open_rejects_canonical_repository_requirements() {
    let (temp, repo) = create_temp_repo();
    let sandbox = tempfile::tempdir().unwrap();
    repo.provision_sandbox(sandbox.path(), DEFAULT_VIEW)
        .unwrap();
    drop(repo);
    set_raw_requirement(temp.path(), "future-format", 1);

    expect_unsupported(Repository::open(sandbox.path()), "future-format");
    expect_unsupported(Repository::open_readonly(sandbox.path()), "future-format");
    expect_unsupported(
        Repository::open_sandbox(sandbox.path(), temp.path(), DEFAULT_VIEW),
        "future-format",
    );
}

#[test]
fn save_change_declares_vnext_capability() {
    let (_temp, repo) = create_temp_repo();
    assert!(repo
        .pristine
        .required_repository_capabilities()
        .unwrap()
        .is_empty());

    repo.save_change(&create_test_change("capability fence"))
        .unwrap();

    assert_eq!(
        repo.pristine.required_repository_capabilities().unwrap(),
        vec![RequiredRepositoryCapability {
            id: "change-format-vnext".to_string(),
            minimum_version: 1,
        }]
    );
}

#[test]
fn v1_pre_serialized_save_does_not_declare_vnext() {
    let (_temp, repo) = create_temp_repo();
    let bytes = atom_file_header(1);
    let hash = Hash::of(&bytes);

    repo.save_change_bytes(&hash, &bytes, &create_test_change("legacy"))
        .unwrap();

    assert!(repo
        .pristine
        .required_repository_capabilities()
        .unwrap()
        .is_empty());
    assert!(repo.change_store.change_path(&hash).is_file());
}

#[test]
fn v2_pre_serialized_save_marks_before_object_persistence() {
    let (temp, repo) = create_temp_repo();
    let changes_dir = temp.path().join(".atomic/changes");
    std::fs::remove_dir_all(&changes_dir).unwrap();
    std::fs::write(&changes_dir, b"blocks child creation").unwrap();
    let bytes = atom_file_header(2);
    let hash = Hash::of(&bytes);

    assert!(repo
        .save_change_bytes(&hash, &bytes, &create_test_change("vnext"))
        .is_err());

    assert_eq!(
        repo.pristine.required_repository_capabilities().unwrap(),
        vec![RequiredRepositoryCapability {
            id: "change-format-vnext".to_string(),
            minimum_version: 1,
        }]
    );
    assert!(!repo.change_store.change_path(&hash).exists());
}

#[test]
fn malformed_or_unsupported_pre_serialized_bytes_do_not_mutate() {
    for bytes in [
        vec![b'A', b'T', b'O'],
        {
            let mut bytes = atom_file_header(1);
            bytes[..4].copy_from_slice(b"NOPE");
            bytes
        },
        atom_file_header(3),
    ] {
        let (temp, repo) = create_temp_repo();
        let hash = Hash::of(&bytes);
        let error = repo
            .save_change_bytes(&hash, &bytes, &create_test_change("invalid"))
            .unwrap_err();
        assert!(matches!(error, RepositoryError::Serialization(_)));
        assert!(repo
            .pristine
            .required_repository_capabilities()
            .unwrap()
            .is_empty());
        assert!(!repo.change_store.change_path(&hash).exists());
        drop(repo);
        drop(temp);
    }
}

// Capability entries are additive metadata. Binaries predating the generic
// fence do not know to inspect them and therefore cannot be forced to reject a
// repository without an intentionally incompatible outer-layout redesign.
