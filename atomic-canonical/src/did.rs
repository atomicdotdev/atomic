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
//! NOTE (open question, deferred): PROV / Data-Integrity tooling generally
//! assumes `did:key` (multibase+multicodec). `did:atomic` is chosen for M0 for
//! simplicity and in-tree consistency; `proof.rs` keeps the method swappable.

use atomic_identity::keypair::PublicKey;

pub const DID_ATOMIC_PREFIX: &str = "did:atomic:";

/// Build the `did:atomic:...` identifier for a public key.
pub fn did_for_public_key(public_key: &PublicKey) -> String {
    let fingerprint = blake3::hash(public_key.as_bytes());
    format!(
        "{}{}",
        DID_ATOMIC_PREFIX,
        data_encoding::BASE32_NOPAD.encode(fingerprint.as_bytes())
    )
}

/// The verification method id used in a proof (`<did>#key-1`).
pub fn verification_method(did: &str) -> String {
    format!("{did}#key-1")
}

/// The DID out of a `<did>#fragment` verification method string.
pub fn did_from_verification_method(vm: &str) -> &str {
    vm.split('#').next().unwrap_or(vm)
}

/// Does this DID correspond to the given public key?
pub fn did_matches_public_key(did: &str, public_key: &PublicKey) -> bool {
    did == did_for_public_key(public_key)
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
    fn verification_method_roundtrips_to_did() {
        let kp = KeyPair::generate();
        let did = did_for_public_key(&kp.public);
        let vm = verification_method(&did);
        assert_eq!(did_from_verification_method(&vm), did);
    }
}
