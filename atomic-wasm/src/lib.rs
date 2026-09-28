//! atomic-canonical for the browser.
//!
//! A web page that holds an atomic identity's key in WebCrypto (as a
//! non-extractable Ed25519 key) signs atomic documents without the key ever
//! reaching Rust — or leaving the browser. This crate does everything *but*
//! the signing, with the same code the CLI uses, so the result is an ordinary
//! atomic attestation (`eddsa-jcs-2022`) that `atomic` verifies:
//!
//! ```js
//! const prepared = prepareAttestation(JSON.stringify(doc), publicKeyBytes);
//! const sig = await crypto.subtle.sign("Ed25519", key, prepared.signingBytes);
//! const attested = attachProof(prepared.document, publicKeyBytes, new Uint8Array(sig));
//! ```
//!
//! Every function takes and returns JSON as strings and keys/signatures as
//! bytes, so there is no JS object model to keep in step with the Rust types.

use atomic_canonical::{did, jcs, proof};
use atomic_identity::keypair::PublicKey;
use atomic_identity::signing::Signature;
use wasm_bindgen::prelude::*;

fn public_key(bytes: &[u8]) -> Result<PublicKey, JsError> {
    let bytes: &[u8; 32] = bytes
        .try_into()
        .map_err(|_| JsError::new("an Ed25519 public key is 32 bytes"))?;
    PublicKey::from_bytes(bytes).map_err(|e| JsError::new(&e.to_string()))
}

fn parse(json: &str) -> Result<serde_json::Value, JsError> {
    serde_json::from_str(json).map_err(|e| JsError::new(&format!("not JSON: {e}")))
}

/// A document ready to sign: the document to hand back to [`attach_proof`],
/// and the bytes the signature must cover.
#[wasm_bindgen]
pub struct Prepared {
    document: String,
    signing_bytes: Vec<u8>,
}

#[wasm_bindgen]
impl Prepared {
    /// The document with `attributedTo` and `contentHash` filled in (JSON).
    #[wasm_bindgen(getter)]
    pub fn document(&self) -> String {
        self.document.clone()
    }

    /// What to sign with the identity's Ed25519 key.
    #[wasm_bindgen(getter, js_name = signingBytes)]
    pub fn signing_bytes(&self) -> Vec<u8> {
        self.signing_bytes.clone()
    }
}

/// Fill in the author and content hash of `document` (JSON) for the key
/// `public_key`, and return it with the bytes to sign.
#[wasm_bindgen(js_name = prepareAttestation)]
pub fn prepare_attestation(document: &str, public_key_bytes: &[u8]) -> Result<Prepared, JsError> {
    let pk = public_key(public_key_bytes)?;
    let prepared = proof::prepare_attestation(parse(document)?, &pk);
    Ok(Prepared {
        document: prepared.value.to_string(),
        signing_bytes: prepared.signing_bytes,
    })
}

/// Attach the proof for `signature` (64 bytes, over the prepared signing
/// bytes) and check it verifies. Returns the attested document (JSON).
#[wasm_bindgen(js_name = attachProof)]
pub fn attach_proof(
    document: &str,
    public_key_bytes: &[u8],
    signature: &[u8],
) -> Result<String, JsError> {
    let pk = public_key(public_key_bytes)?;
    let sig = Signature::from_slice(signature).map_err(|e| JsError::new(&e.to_string()))?;
    let attested = proof::attach_proof(parse(document)?, &pk, &sig);
    proof::verify_value(&attested, &pk).map_err(|e| JsError::new(&e.to_string()))?;
    Ok(attested.to_string())
}

/// Check an attested document (JSON) against a public key.
#[wasm_bindgen(js_name = verifyAttestation)]
pub fn verify_attestation(document: &str, public_key_bytes: &[u8]) -> Result<(), JsError> {
    let pk = public_key(public_key_bytes)?;
    proof::verify_value(&parse(document)?, &pk).map_err(|e| JsError::new(&e.to_string()))
}

/// The `did:atomic` identifier for a public key.
#[wasm_bindgen(js_name = didForPublicKey)]
pub fn did_for_public_key(public_key_bytes: &[u8]) -> Result<String, JsError> {
    Ok(did::did_for_public_key(&public_key(public_key_bytes)?))
}

/// The key's base32 form — what atomic writes as the `kid` of its tokens.
#[wasm_bindgen(js_name = publicKeyBase32)]
pub fn public_key_base32(public_key_bytes: &[u8]) -> Result<String, JsError> {
    Ok(public_key(public_key_bytes)?.to_base32())
}

/// RFC 8785 canonical JSON of `document` — the bytes atomic hashes and signs.
#[wasm_bindgen]
pub fn canonicalize(document: &str) -> Result<String, JsError> {
    Ok(jcs::canonicalize(&parse(document)?))
}
