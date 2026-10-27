use atomic_core::kg::project_id::MintReason;
use atomic_repository::Repository;

#[test]
fn identity_is_local_view_independent_and_minting_is_idempotent() {
    let directory = tempfile::tempdir().unwrap();
    let mut repo = Repository::init(directory.path()).unwrap();
    assert!(repo.project_identity().unwrap().is_none());
    let identity = repo.ensure_project_identity(MintReason::Upgrade).unwrap();
    assert_eq!(
        repo.ensure_project_identity(MintReason::Created).unwrap(),
        identity
    );
    repo.create_view("another").unwrap();
    assert_eq!(repo.project_identity().unwrap().unwrap(), identity);
    drop(repo);
    let repo = Repository::open(directory.path()).unwrap();
    assert_eq!(repo.project_identity().unwrap().unwrap(), identity);
}

#[test]
fn only_full_writable_open_mints_an_upgrade_identity() {
    let directory = tempfile::tempdir().unwrap();
    drop(Repository::init(directory.path()).unwrap());
    let repo = Repository::open_existing(directory.path()).unwrap();
    assert!(repo.project_identity().unwrap().is_none());
    drop(repo);
    let repo = Repository::open(directory.path()).unwrap();
    assert_eq!(
        repo.project_identity().unwrap().unwrap().mint_reason,
        MintReason::Upgrade
    );
}
