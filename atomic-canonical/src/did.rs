//! The `did:atomic` method (in-tree, per the M0 decision).
//!
//! An identity's DID is derived from its Ed25519 public key exactly the way
//! atomic already derives `IdentityId`: `blake3(pubkey)`, base32-encoded. This
//! keeps the DID consistent with the rest of atomic's identity model.
//!
//! `did:atomic:<base32(blake3(pubkey))>` is a *fingerprint* of the key, so the
//! public key cannot be recovered from the DID alone — verification needs a
//! resolver (an `IdentityStore` lookup). For the self-contained M0 crate,
//! `verify` takes the `PublicKey` directly and `did_matches_public_key`
//! confirms the DID belongs to that key. The store-backed resolver arrives in
//! M1 when this wires into the vault.
//!
//! `did:atomic` stays the in-tree method (it matches atomic's identity
//! model), and [`did_key_for_public_key`] additionally provides the standard
//! `did:key` representation of the same key — multicodec `ed25519-pub`
//! (0xed 0x01) + the 32-byte key, multibase base58btc — for interop with
//! Data-Integrity tooling that resolves `did:key` natively. Verification
//! accepts either method for the same key.

use atomic_identity::identity::IdentityId;
use atomic_identity::keypair::PublicKey;

pub use atomic_identity::identity::DID_ATOMIC_PREFIX;
pub use atomic_identity::keypair::DID_KEY_PREFIX;

/// Build the `did:atomic:...` identifier for a public key.
///
/// Both DID renderings are derived in `atomic-identity` — the DID *is* the
/// identity's identifier, so it belongs with the type that owns identity, and
/// having one derivation means a `did:atomic` here can never disagree with an
/// `IdentityId` there.
pub fn did_for_public_key(public_key: &PublicKey) -> String {
    IdentityId::from_public_key(public_key).to_did()
}

/// Build the standard `did:key` identifier for an Ed25519 public key
/// (multicodec `ed25519-pub` + key bytes, base58btc multibase). Always
/// starts `did:key:z6Mk` for Ed25519 keys.
pub fn did_key_for_public_key(public_key: &PublicKey) -> String {
    public_key.to_did_key()
}

/// The verification method id used in a proof (`<did>#key-1`).
pub fn verification_method(did: &str) -> String {
    format!("{did}#key-1")
}

/// The DID out of a `<did>#fragment` verification method string.
pub fn did_from_verification_method(vm: &str) -> &str {
    vm.split('#').next().unwrap_or(vm)
}

/// Does this DID correspond to the given public key? Accepts either the
/// in-tree `did:atomic` fingerprint or the standard `did:key` form.
pub fn did_matches_public_key(did: &str, public_key: &PublicKey) -> bool {
    did == did_for_public_key(public_key) || did == did_key_for_public_key(public_key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_identity::keypair::KeyPair;

    #[test]
    fn did_is_stable_and_key_bound() {
        let kp = KeyPair::generate();
        let did = did_for_public_key(&kp.public);
        assert!(did.starts_with("did:atomic:"));
        assert_eq!(did, did_for_public_key(&kp.public));
        assert!(did_matches_public_key(&did, &kp.public));

        let other = KeyPair::generate();
        assert!(!did_matches_public_key(&did, &other.public));
    }

    #[test]
    fn did_key_has_ed25519_multicodec_shape_and_verifies() {
        let kp = KeyPair::generate();
        let did_key = did_key_for_public_key(&kp.public);
        // Ed25519 multicodec + base58btc always yields the z6Mk prefix.
        assert!(
            did_key.starts_with("did:key:z6Mk"),
            "unexpected did:key form: {did_key}"
        );
        assert_eq!(did_key, did_key_for_public_key(&kp.public), "stable");
        // Either representation of the same key verifies.
        assert!(did_matches_public_key(&did_key, &kp.public));
        assert!(did_matches_public_key(
            &did_for_public_key(&kp.public),
            &kp.public
        ));

        let other = KeyPair::generate();
        assert!(!did_matches_public_key(&did_key, &other.public));
    }

    #[test]
    fn verification_method_roundtrips_to_did() {
        let kp = KeyPair::generate();
        let did = did_for_public_key(&kp.public);
        let vm = verification_method(&did);
        assert_eq!(did_from_verification_method(&vm), did);
    }
}
