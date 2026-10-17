use atomic_identity::{Identity, IdentityStore, KeyPair};

#[test]
fn opening_an_existing_store_never_initializes_a_missing_store() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing");
    assert!(IdentityStore::open_existing(&missing).is_err());
    assert!(!missing.exists());
}

#[test]
fn public_identity_can_be_loaded_without_a_private_key() {
    let dir = tempfile::tempdir().unwrap();
    let store = IdentityStore::open(dir.path()).unwrap();
    let key = KeyPair::generate();
    let identity = Identity::from_public_key("public-only", key.public.clone());
    store.save(&identity).unwrap();
    let existing = IdentityStore::open_existing(dir.path()).unwrap();
    assert_eq!(
        existing.load_by_name("public-only").unwrap().public_key,
        key.public
    );
    assert!(existing.load_keypair(&identity.id, None).is_err());
}
