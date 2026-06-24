//! Client-self-signed EdDSA JWT bearer tokens for remote commands.
//!
//! Atomic Storage authenticates with short-lived JWTs that the CLI **signs
//! itself** with the identity's Ed25519 private key (EdDSA, RFC 8037). There is
//! no server login endpoint and no token cache: minting a token is a local,
//! cheap signing operation, so we just mint a fresh one per request.
//!
//! # Wire format (identical to the server's verifier)
//!
//! A compact JWS with three base64url-no-pad segments
//! `base64url(header).base64url(claims).base64url(signature)`:
//!
//! - header: `{"alg":"EdDSA","typ":"JWT","kid":"<base32 Ed25519 public key>"}`
//! - claims: `{ sub, iat, exp, jti }` (`sub` mirrors the `kid` public key)
//! - signature: `Ed25519_sign(private_key, "header.claims")`
//!
//! # Keyed by the public key
//!
//! The JWT is keyed by the caller's Ed25519 **public key**, carried in the
//! `kid` header. The client already holds this key, so there is no
//! server-assigned identifier to learn or persist: the server resolves the
//! registered identity by the `kid` public key and verifies this signature
//! against the key it has on record.

use chrono::{Duration, Utc};
use data_encoding::BASE64URL_NOPAD;
use serde::Serialize;
use uuid::Uuid;

use atomic_identity::{Identity, IdentityStore};

use crate::error::{CliError, CliResult};

/// How long a self-signed token is valid. Kept short (5 minutes) since there is
/// no server-side issuance to revoke — a leaked token has a small blast radius.
const TOKEN_TTL: Duration = Duration::minutes(5);

// ---------------------------------------------------------------------------
// JWT pieces (kept local to avoid depending on the server crate)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct JwtHeader {
    alg: &'static str,
    typ: &'static str,
    /// The caller's base32 Ed25519 public key.
    kid: String,
}

#[derive(Serialize)]
struct Claims {
    /// The caller's base32 Ed25519 public key (same value as the header `kid`).
    sub: String,
    iat: i64,
    exp: i64,
    jti: String,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Mint a fresh, short-lived self-signed EdDSA JWT for `identity` against
/// `server`.
///
/// `server` is the bare server URL (e.g. `https://atomic.storage`), NOT an org
/// subdomain. The token is keyed by the identity's own Ed25519 public key, so
/// no prior registration state needs to be on file locally to mint it.
pub async fn get_token(server: &str, identity: &Identity) -> CliResult<String> {
    mint_token(server, identity)
}

/// Mint a fresh token. Tokens are not cached, so this is identical to
/// [`get_token`]; kept for call-site compatibility (e.g. retry-after-401).
pub async fn refresh_token(server: &str, identity: &Identity) -> CliResult<String> {
    mint_token(server, identity)
}

// ---------------------------------------------------------------------------
// Minting
// ---------------------------------------------------------------------------

fn mint_token(_server: &str, identity: &Identity) -> CliResult<String> {
    // The token is keyed by the identity's own public key — no server-assigned
    // identifier to look up.
    let public_key_b32 = identity.public_key_base32();

    // Load the keypair (needs the secret key to sign).
    let store = IdentityStore::open_default()
        .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to open identity store: {e}")))?;
    let keypair = store.load_keypair(&identity.id, None).map_err(|e| {
        CliError::Internal(anyhow::anyhow!(
            "Failed to load keypair for '{}': {e}",
            identity.name
        ))
    })?;

    let now = Utc::now();
    let claims = Claims {
        sub: public_key_b32.clone(),
        iat: now.timestamp(),
        exp: (now + TOKEN_TTL).timestamp(),
        jti: Uuid::new_v4().to_string(),
    };

    let header = JwtHeader {
        alg: "EdDSA",
        typ: "JWT",
        kid: public_key_b32,
    };

    let header_json = serde_json::to_vec(&header)
        .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to encode JWT header: {e}")))?;
    let claims_json = serde_json::to_vec(&claims)
        .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to encode JWT claims: {e}")))?;

    let header_b64 = BASE64URL_NOPAD.encode(&header_json);
    let claims_b64 = BASE64URL_NOPAD.encode(&claims_json);
    let signing_input = format!("{header_b64}.{claims_b64}");

    let signature = keypair.sign(signing_input.as_bytes());
    let sig_b64 = BASE64URL_NOPAD.encode(&signature);

    log::debug!("Minted self-signed EdDSA JWT for '{}'", identity.name);
    Ok(format!("{signing_input}.{sig_b64}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_is_eddsa_jwt_with_kid() {
        let header = JwtHeader {
            alg: "EdDSA",
            typ: "JWT",
            kid: "ABCDEF".to_string(),
        };
        let json = serde_json::to_string(&header).unwrap();
        assert_eq!(json, r#"{"alg":"EdDSA","typ":"JWT","kid":"ABCDEF"}"#);
    }

    /// A minted token must verify against the identity's public key, prove the
    /// three-segment shape, carry the public key as `kid` (and `sub`), and not
    /// verify once tampered.
    #[test]
    fn self_signed_token_verifies_round_trip() {
        // Build a JWT by hand using the same steps as mint_token (without the
        // store I/O, which needs a populated home dir).
        use atomic_identity::KeyPair;
        let keypair = KeyPair::generate();
        let public_key_b32 = data_encoding::BASE32_NOPAD.encode(keypair.public.as_bytes());
        let now = Utc::now();
        let claims = Claims {
            sub: public_key_b32.clone(),
            iat: now.timestamp(),
            exp: (now + TOKEN_TTL).timestamp(),
            jti: Uuid::new_v4().to_string(),
        };
        let header = JwtHeader {
            alg: "EdDSA",
            typ: "JWT",
            kid: public_key_b32.clone(),
        };
        let header_b64 = BASE64URL_NOPAD.encode(&serde_json::to_vec(&header).unwrap());
        let claims_b64 = BASE64URL_NOPAD.encode(&serde_json::to_vec(&claims).unwrap());
        let signing_input = format!("{header_b64}.{claims_b64}");
        let sig = keypair.sign(signing_input.as_bytes());
        let token = format!("{signing_input}.{}", BASE64URL_NOPAD.encode(&sig));

        // Three segments.
        assert_eq!(token.split('.').count(), 3);

        // The kid in the header is the base32 public key the server resolves by.
        let header_bytes = BASE64URL_NOPAD
            .decode(token.split('.').next().unwrap().as_bytes())
            .unwrap();
        let header_val: serde_json::Value = serde_json::from_slice(&header_bytes).unwrap();
        assert_eq!(header_val["kid"], public_key_b32);
        assert_eq!(header_val["alg"], "EdDSA");

        // Verifies against the public key (mirrors the server's verify path),
        // using the identity crate's Ed25519 verify so we don't depend on
        // ed25519-dalek directly.
        let parts: Vec<&str> = token.split('.').collect();
        let sig_bytes: [u8; 64] = BASE64URL_NOPAD
            .decode(parts[2].as_bytes())
            .unwrap()
            .try_into()
            .unwrap();
        let recovered_input = format!("{}.{}", parts[0], parts[1]);
        assert!(keypair
            .public
            .verify(recovered_input.as_bytes(), &sig_bytes)
            .is_ok());

        // A tampered signing input must NOT verify.
        let mut bad = recovered_input.clone().into_bytes();
        bad[0] ^= 0x01;
        assert!(keypair.public.verify(&bad, &sig_bytes).is_err());
    }

    #[test]
    fn ttl_is_five_minutes() {
        assert_eq!(TOKEN_TTL, Duration::minutes(5));
    }
}
