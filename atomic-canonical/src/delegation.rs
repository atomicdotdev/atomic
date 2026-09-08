//! Agent delegation certificates — minting and verification.
//!
//! A delegation certificate is the artifact that answers "does this agent key
//! belong to that person, and what may it do?". It is a canonical JSON-LD node
//! signed with `eddsa-jcs-2022`, so it goes through the same
//! [`crate::jcs`]/[`crate::proof`] path as every other typed node: one
//! canonicalization, one signature format, no second code path to drift.
//!
//! Three documents live here, all built on the same signing machinery:
//!
//! | Document | Signed by | Proves |
//! |---|---|---|
//! | [`mint`] — `AgentDelegation` | the delegator (human) | the agent belongs to this human, and its bounds |
//! | [`mint_request`] — `AgentDelegationRequest` | the delegate (agent) | the agent possesses the key it is asking about |
//! | [`mint_revocation`] — `DelegationRevocation` | the delegator (human) | the delegation is withdrawn |
//!
//! # Why the certificate carries two DIDs for the agent
//!
//! `did:atomic` is `base32(blake3(pubkey))` — a *fingerprint*, from which the
//! key cannot be recovered. A verifier handed only a certificate would have
//! nothing to check the agent's later signatures against. So the certificate
//! also carries `delegateKey`, the standard `did:key` form, which does encode
//! the key; [`verify`] confirms the two agree. That is what makes offline
//! verification of a clone possible: the human's public key verifies the
//! certificate, and the certificate yields the agent's public key.
//!
//! # What verification does and does not establish
//!
//! [`verify`] establishes that the delegator signed this exact scope for this
//! exact agent key, and that the document has not been altered. It says nothing
//! about revocation (server state) or about what the delegator is themselves
//! allowed to do. A server must check both; effective permission is always the
//! intersection of the delegator's own access and the certificate's scope.
//!
//! # Example
//!
//! ```rust
//! use atomic_canonical::delegation;
//! use atomic_identity::{Identity, IdentityType, KeyPair};
//! use atomic_identity::delegation::{Delegation, DelegationPermission, DelegationScope};
//!
//! // The human, holding a keypair.
//! let human_key = KeyPair::generate();
//! let human = Identity::new("alice", &human_key);
//!
//! // The agent, with a keypair of its own.
//! let agent_key = KeyPair::generate();
//! let agent = Identity::builder("alice+claude")
//!     .identity_type(IdentityType::Agent)
//!     .public_key(agent_key.public.clone())
//!     .delegated_by(human.id)
//!     .build()?;
//!
//! let scope = DelegationScope::builder()
//!     .permission(DelegationPermission::Record)
//!     .project("acme/*")
//!     .build();
//! let terms = Delegation::new(&human, &agent, scope);
//!
//! let certificate = delegation::mint(&human, &human_key, &terms);
//!
//! // Anyone holding the human's public key can check it — no server needed.
//! let parsed = delegation::verify(&certificate, &human.public_key)?;
//! assert_eq!(parsed.delegate_name, "alice+claude");
//!
//! // And it yields the agent's public key, for verifying the agent's own work.
//! let agent_public = delegation::delegate_public_key(&parsed)?;
//! assert_eq!(agent_public, agent.public_key);
//! # Ok::<(), atomic_canonical::CanonicalError>(())
//! ```

use atomic_identity::delegation::{Delegation, DelegationId};
use atomic_identity::identity::{Identity, IdentityId};
use atomic_identity::keypair::{KeyPair, PublicKey};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::error::{CanonicalError, Result};
use crate::jcs;
use crate::node::CONTEXT_URL;
use crate::proof;

/// `@type` of a delegation certificate.
pub const TYPE_DELEGATION: &str = "AgentDelegation";
/// `@type` of an agent's self-signed request to be delegated to.
pub const TYPE_REQUEST: &str = "AgentDelegationRequest";
/// `@type` of a delegator's revocation of a certificate.
pub const TYPE_REVOCATION: &str = "DelegationRevocation";

/// URN prefix for the software-agent label (`urn:atomic:agent:<slug>`).
pub const AGENT_URN_PREFIX: &str = "urn:atomic:agent:";

// ---------------------------------------------------------------------------
// Certificate
// ---------------------------------------------------------------------------

/// Mint a signed delegation certificate.
///
/// The delegator's keypair signs; the returned value is the complete document,
/// including `contentHash` and `proof`. Persist and transmit these exact bytes
/// — re-serializing a parsed struct risks producing different bytes than the
/// proof covers.
pub fn mint(delegator: &Identity, delegator_key: &KeyPair, terms: &Delegation) -> Value {
    let mut doc = Map::new();
    doc.insert("@context".into(), json!(CONTEXT_URL));
    doc.insert("@type".into(), json!(TYPE_DELEGATION));
    doc.insert("@id".into(), json!(terms.id.to_urn()));

    doc.insert("delegator".into(), json!(terms.delegator));
    doc.insert("delegatorKey".into(), json!(terms.delegator_key));
    doc.insert("delegatorName".into(), json!(terms.delegator_name));
    doc.insert("delegate".into(), json!(terms.delegate));
    doc.insert("delegateKey".into(), json!(terms.delegate_key));
    doc.insert("delegateName".into(), json!(terms.delegate_name));
    if let Some(agent) = &terms.software_agent {
        doc.insert("softwareAgent".into(), json!(agent));
    }

    doc.insert(
        "scope".into(),
        serde_json::to_value(&terms.scope).expect("scope serialization is infallible"),
    );
    doc.insert("issued".into(), json!(rfc3339(terms.issued)));
    if let Some(expires) = terms.expires {
        doc.insert("expires".into(), json!(rfc3339(expires)));
    }

    proof::attest_value(Value::Object(doc), delegator, delegator_key)
}

/// Verify a certificate against the delegator's public key and return the
/// typed view.
///
/// Checks, in order:
///
/// 1. the document is an `AgentDelegation`;
/// 2. content hash recomputes, the proof verifies, and its
///    `verificationMethod` belongs to `delegator_public_key`
///    ([`proof::verify_value`]);
/// 3. the `delegator` DID is that same key — otherwise a certificate signed by
///    one key could name another as the delegator;
/// 4. `delegateKey` and `delegate` describe the same key;
/// 5. `@id` recomputes from the body, so the identifier cannot be swapped for
///    one belonging to a different (perhaps revoked) certificate.
pub fn verify(document: &Value, delegator_public_key: &PublicKey) -> Result<Delegation> {
    expect_type(document, TYPE_DELEGATION)?;
    proof::verify_value(document, delegator_public_key)?;

    let parsed = parse(document)?;

    // 3. The signer must be the delegator the document names.
    let delegator_id = IdentityId::from_did(&parsed.delegator)
        .map_err(|e| CanonicalError::Verification(format!("delegator DID is malformed: {e}")))?;
    if !delegator_id.matches_public_key(delegator_public_key) {
        return Err(CanonicalError::Verification(
            "delegator DID does not match the verifying key".into(),
        ));
    }

    // 3b. The delegator's own two renderings must agree too, so a
    //     self-contained verifier that starts from `delegatorKey` reaches the
    //     same conclusion as one that starts from a key it already trusts.
    let stated_delegator_key = self::delegator_public_key(&parsed)?;
    if &stated_delegator_key != delegator_public_key {
        return Err(CanonicalError::Verification(
            "delegatorKey does not match the verifying key".into(),
        ));
    }

    // 4. The two renderings of the delegate's key must agree. Without this a
    //    certificate could name agent A in `delegate` while handing out agent
    //    B's key in `delegateKey`.
    let delegate_key = delegate_public_key(&parsed)?;
    let delegate_id = IdentityId::from_did(&parsed.delegate)
        .map_err(|e| CanonicalError::Verification(format!("delegate DID is malformed: {e}")))?;
    if !delegate_id.matches_public_key(&delegate_key) {
        return Err(CanonicalError::Verification(
            "delegateKey does not match the delegate DID".into(),
        ));
    }

    // 5. The id is a claim like any other; recompute it.
    if !parsed.id_matches(&delegator_id, &delegate_id) {
        return Err(CanonicalError::Verification(
            "delegation @id does not match its delegator, delegate and issue time".into(),
        ));
    }

    Ok(parsed)
}

/// Parse a certificate into its typed view **without** verifying the proof.
///
/// For displaying a document whose trust has not been established, or that is
/// about to be verified by a caller that already holds the key. Never make an
/// authorization decision on the result of this function alone.
pub fn parse(document: &Value) -> Result<Delegation> {
    expect_type(document, TYPE_DELEGATION)?;
    let obj = as_object(document)?;

    let id_urn = string_field(obj, "@id")?;
    let id = DelegationId::from_base32(&id_urn)
        .ok_or_else(|| CanonicalError::Proof(format!("malformed delegation @id: {id_urn}")))?;

    let scope = obj
        .get("scope")
        .ok_or_else(|| CanonicalError::Proof("delegation carries no scope".into()))?;
    let scope = serde_json::from_value(scope.clone())
        .map_err(|e| CanonicalError::Proof(format!("malformed delegation scope: {e}")))?;

    Ok(Delegation {
        id,
        delegator: string_field(obj, "delegator")?,
        delegator_key: string_field(obj, "delegatorKey")?,
        delegator_name: string_field(obj, "delegatorName")?,
        delegate: string_field(obj, "delegate")?,
        delegate_key: string_field(obj, "delegateKey")?,
        delegate_name: string_field(obj, "delegateName")?,
        software_agent: obj
            .get("softwareAgent")
            .and_then(Value::as_str)
            .map(str::to_string),
        scope,
        issued: timestamp_field(obj, "issued")?,
        expires: optional_timestamp_field(obj, "expires")?,
    })
}

/// Recover the delegate's Ed25519 public key from a parsed certificate.
///
/// This is the payoff of carrying `did:key` alongside `did:atomic`: given a
/// certificate you trust, you can verify the agent's own signatures.
pub fn delegate_public_key(delegation: &Delegation) -> Result<PublicKey> {
    PublicKey::from_did_key(&delegation.delegate_key)
        .map_err(|e| CanonicalError::Proof(format!("malformed delegateKey: {e}")))
}

/// Recover the delegator's Ed25519 public key from a parsed certificate.
pub fn delegator_public_key(delegation: &Delegation) -> Result<PublicKey> {
    PublicKey::from_did_key(&delegation.delegator_key)
        .map_err(|e| CanonicalError::Proof(format!("malformed delegatorKey: {e}")))
}

/// Verify a certificate using the delegator key the document itself carries.
///
/// This checks **integrity**, not trust: it proves the document was signed by
/// whoever holds the key it names and has not been altered since. It cannot
/// tell you that key belongs to the person you think — that is settled by
/// comparing the recovered DID against a key you already trust (the server's
/// registered identity, or a `delegator` DID you pinned).
///
/// Use it where the trusted key is not to hand: a CI runner holding only the
/// agent's key, or rendering an unfamiliar certificate before deciding what to
/// do with it. Where the delegator's key *is* available, prefer [`verify`].
pub fn verify_self_contained(document: &Value) -> Result<Delegation> {
    let parsed = parse(document)?;
    let key = delegator_public_key(&parsed)?;
    verify(document, &key)
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// The HTTP header a request carries its delegation certificate in.
///
/// The certificate travels **with the request** rather than being registered
/// in advance. It is signed by a key the server already trusts (the
/// delegator's, from registration), so presenting it is proof enough — the
/// same shape as JOSE's `x5c`, SPIFFE SVIDs, or a macaroon.
///
/// This is what keeps issuing cheap: extending an agent's time or widening its
/// scope is you signing a new certificate and handing it over, with no server
/// round trip. Only *withdrawal* needs to reach the server, because a
/// credential the holder possesses cannot prove its own revocation.
pub const DELEGATION_HEADER: &str = "Atomic-Delegation";

/// Hard cap on an encoded certificate, enforced **before** parsing.
///
/// The verify path now handles caller-supplied JSON on every request, so the
/// size check has to come first: rejecting a 10MB body after canonicalizing it
/// is not a rejection. A real certificate is ~1–2KB; 16KB leaves generous room
/// for long project lists without letting anything interesting through.
pub const MAX_ENCODED_DELEGATION: usize = 16 * 1024;

/// Encode a certificate for transport: JCS-canonical bytes, base64url, no pad.
///
/// Canonical rather than "whatever bytes we happened to store" so the encoding
/// is deterministic — which is what lets a server cache a verified certificate
/// by content hash and recognise the same one next request.
pub fn encode_for_transport(document: &Value) -> String {
    let canonical = jcs::canonicalize(document);
    data_encoding::BASE64URL_NOPAD.encode(canonical.as_bytes())
}

/// Decode a certificate presented in a request header.
///
/// Checks the size cap first, then base64, then JSON. Does **not** verify —
/// [`verify`] against the delegator's registered key is a separate, mandatory
/// step, and keeping them apart means no call site can accidentally treat a
/// well-formed certificate as a trusted one.
pub fn decode_from_transport(encoded: &str) -> Result<Value> {
    if encoded.len() > MAX_ENCODED_DELEGATION {
        return Err(CanonicalError::Proof(format!(
            "delegation is {} bytes, over the {MAX_ENCODED_DELEGATION}-byte limit",
            encoded.len()
        )));
    }

    let bytes = data_encoding::BASE64URL_NOPAD
        .decode(encoded.trim().as_bytes())
        .map_err(|e| CanonicalError::Proof(format!("delegation is not valid base64url: {e}")))?;

    serde_json::from_slice(&bytes)
        .map_err(|e| CanonicalError::Proof(format!("delegation is not valid JSON: {e}")))
}

/// A stable fingerprint of an encoded certificate, for caching a verified
/// result without re-running the Ed25519 check on every request.
///
/// Keyed on the encoded bytes, so a cache hit means *this exact certificate*
/// — a tampered one hashes differently and can never collide with a verified
/// entry. Only ever populated after a full verification passes, so the cached
/// value is the output of validation, never a substitute for it.
pub fn transport_fingerprint(encoded: &str) -> String {
    data_encoding::BASE32_NOPAD.encode(blake3::hash(encoded.as_bytes()).as_bytes())
}

// ---------------------------------------------------------------------------
// Request — proof of possession, for keys the human never holds
// ---------------------------------------------------------------------------

/// An agent's self-signed request to be delegated to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationRequest {
    /// The requesting agent's `did:atomic`.
    pub delegate: String,
    /// The requesting agent's `did:key` — the key it is proving possession of.
    pub delegate_key: String,
    /// The name the agent wants to be known by.
    pub delegate_name: String,
    /// Which software agent this key belongs to.
    pub software_agent: Option<String>,
    /// When the request was made.
    pub requested: DateTime<Utc>,
}

/// Mint an agent's self-signed delegation request.
///
/// Used when the agent's key is generated somewhere the human never sees — a CI
/// runner, a hosted agent. The self-signature is proof of possession: the human
/// countersigning a request knows the key on the other end is real, and is not
/// tricked into delegating to a key nobody holds.
pub fn mint_request(agent: &Identity, agent_key: &KeyPair, software_agent: Option<&str>) -> Value {
    let mut doc = Map::new();
    doc.insert("@context".into(), json!(CONTEXT_URL));
    doc.insert("@type".into(), json!(TYPE_REQUEST));
    doc.insert("delegate".into(), json!(agent.id.to_did()));
    doc.insert("delegateKey".into(), json!(agent.public_key.to_did_key()));
    doc.insert("delegateName".into(), json!(agent.name));
    if let Some(sa) = software_agent {
        doc.insert("softwareAgent".into(), json!(sa));
    }
    doc.insert("requested".into(), json!(rfc3339(Utc::now())));

    proof::attest_value(Value::Object(doc), agent, agent_key)
}

/// Verify a delegation request's self-signature.
///
/// The verifying key comes out of the document itself (`delegateKey`), which is
/// exactly what makes this proof of *possession* and nothing more: it shows
/// whoever produced the document holds the private half of the key it names. It
/// carries no authority on its own — the human's countersignature does.
pub fn verify_request(document: &Value) -> Result<DelegationRequest> {
    expect_type(document, TYPE_REQUEST)?;
    let obj = as_object(document)?;

    let delegate_key_did = string_field(obj, "delegateKey")?;
    let public_key = PublicKey::from_did_key(&delegate_key_did)
        .map_err(|e| CanonicalError::Proof(format!("malformed delegateKey: {e}")))?;

    proof::verify_value(document, &public_key)?;

    let delegate = string_field(obj, "delegate")?;
    let delegate_id = IdentityId::from_did(&delegate)
        .map_err(|e| CanonicalError::Verification(format!("delegate DID is malformed: {e}")))?;
    if !delegate_id.matches_public_key(&public_key) {
        return Err(CanonicalError::Verification(
            "delegateKey does not match the delegate DID".into(),
        ));
    }

    Ok(DelegationRequest {
        delegate,
        delegate_key: delegate_key_did,
        delegate_name: string_field(obj, "delegateName")?,
        software_agent: obj
            .get("softwareAgent")
            .and_then(Value::as_str)
            .map(str::to_string),
        requested: timestamp_field(obj, "requested")?,
    })
}

// ---------------------------------------------------------------------------
// Revocation
// ---------------------------------------------------------------------------

/// A delegator's signed withdrawal of a certificate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationRevocation {
    /// The delegation being revoked, as a URN.
    pub delegation: String,
    /// The revoking delegator's `did:atomic`.
    pub delegator: String,
    /// When the revocation was issued.
    pub revoked_at: DateTime<Utc>,
    /// Why, if the human said.
    pub reason: Option<String>,
}

/// Mint a signed revocation.
///
/// Revocation is a signed document rather than a bare API call so that it
/// replicates, audits and verifies like everything else — and so a revocation
/// issued offline is still provable when it reaches a server later.
pub fn mint_revocation(
    delegator: &Identity,
    delegator_key: &KeyPair,
    delegation: &DelegationId,
    reason: Option<&str>,
) -> Value {
    let mut doc = Map::new();
    doc.insert("@context".into(), json!(CONTEXT_URL));
    doc.insert("@type".into(), json!(TYPE_REVOCATION));
    doc.insert("delegation".into(), json!(delegation.to_urn()));
    doc.insert("delegator".into(), json!(delegator.id.to_did()));
    doc.insert("revokedAt".into(), json!(rfc3339(Utc::now())));
    if let Some(reason) = reason {
        doc.insert("reason".into(), json!(reason));
    }

    proof::attest_value(Value::Object(doc), delegator, delegator_key)
}

/// Verify a revocation against the delegator's public key.
///
/// Only the delegator may revoke, so the signature must be theirs and the
/// `delegator` field must name that same key.
pub fn verify_revocation(
    document: &Value,
    delegator_public_key: &PublicKey,
) -> Result<DelegationRevocation> {
    expect_type(document, TYPE_REVOCATION)?;
    proof::verify_value(document, delegator_public_key)?;

    let obj = as_object(document)?;
    let delegator = string_field(obj, "delegator")?;
    let delegator_id = IdentityId::from_did(&delegator)
        .map_err(|e| CanonicalError::Verification(format!("delegator DID is malformed: {e}")))?;
    if !delegator_id.matches_public_key(delegator_public_key) {
        return Err(CanonicalError::Verification(
            "revocation delegator DID does not match the verifying key".into(),
        ));
    }

    Ok(DelegationRevocation {
        delegation: string_field(obj, "delegation")?,
        delegator,
        revoked_at: timestamp_field(obj, "revokedAt")?,
        reason: obj
            .get("reason")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

// ---------------------------------------------------------------------------
// Store-backed lookup
// ---------------------------------------------------------------------------

/// A stored certificate, verified, with its local status.
#[derive(Clone, Debug)]
pub struct StoredDelegation {
    /// Base32 id — the key it is filed under.
    pub id: String,
    /// The verified certificate.
    pub delegation: Delegation,
    /// The document as stored, byte-for-byte what the proof covers.
    pub document: Value,
    /// Whether a revocation is recorded on this machine.
    pub revoked_locally: bool,
}

impl StoredDelegation {
    /// Usable as far as this machine can tell: verified, unexpired, not
    /// locally revoked. Says nothing about a revocation issued elsewhere —
    /// the server is the authority there.
    pub fn is_usable(&self) -> bool {
        !self.revoked_locally && !self.delegation.is_expired()
    }
}

/// Every verified certificate in `store` naming `delegate` as its subject,
/// newest first.
///
/// Certificates that fail to parse or verify are **skipped**, not returned as
/// errors. This is the one place a corrupt or foreign file in the store could
/// otherwise take down every agent operation, and a certificate that does not
/// verify has no authority to convey in any case. Each skip is logged at warn.
///
/// Verification is self-contained — it uses the delegator key the certificate
/// carries — so this works on a machine that holds only the agent's key.
pub fn load_for_delegate(
    store: &atomic_identity::IdentityStore,
    delegate: &Identity,
) -> Result<Vec<StoredDelegation>> {
    let delegate_did = delegate.id.to_did();
    let stored = store
        .list_delegations()
        .map_err(|e| CanonicalError::Proof(format!("failed to list delegations: {e}")))?;

    let mut out = Vec::new();
    for (id, raw) in stored {
        let Ok(value) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        // Cheap discriminator before the Ed25519 verify: most certificates in
        // a store belong to some other agent.
        match parse(&value) {
            Ok(parsed) if parsed.delegate == delegate_did => {}
            _ => continue,
        }
        let Ok(delegation) = verify_self_contained(&value) else {
            continue;
        };

        out.push(StoredDelegation {
            revoked_locally: store.is_revoked_locally(&id),
            id,
            delegation,
            document: value,
        });
    }

    // Newest first: when several certificates cover the same ground, the most
    // recently issued is the one the human meant.
    out.sort_by_key(|d| std::cmp::Reverse(d.delegation.issued));
    Ok(out)
}

/// The certificate currently in force for `delegate`, if any.
pub fn active_for_delegate(
    store: &atomic_identity::IdentityStore,
    delegate: &Identity,
) -> Option<StoredDelegation> {
    load_for_delegate(store, delegate)
        .ok()?
        .into_iter()
        .find(|d| d.is_usable())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// RFC 3339 with second precision — stable bytes for canonicalization.
fn rfc3339(ts: DateTime<Utc>) -> String {
    ts.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn as_object(value: &Value) -> Result<&Map<String, Value>> {
    value
        .as_object()
        .ok_or_else(|| CanonicalError::Proof("document is not a JSON object".into()))
}

fn expect_type(value: &Value, expected: &str) -> Result<()> {
    let actual = as_object(value)?.get("@type").and_then(Value::as_str);
    match actual {
        Some(t) if t == expected => Ok(()),
        Some(t) => Err(CanonicalError::Proof(format!(
            "expected a {expected} document, got {t}"
        ))),
        None => Err(CanonicalError::Proof(format!(
            "document has no @type (expected {expected})"
        ))),
    }
}

fn string_field(obj: &Map<String, Value>, key: &str) -> Result<String> {
    obj.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| CanonicalError::Proof(format!("document is missing '{key}'")))
}

fn timestamp_field(obj: &Map<String, Value>, key: &str) -> Result<DateTime<Utc>> {
    let raw = string_field(obj, key)?;
    DateTime::parse_from_rfc3339(&raw)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| CanonicalError::Proof(format!("'{key}' is not a valid RFC 3339 time: {e}")))
}

fn optional_timestamp_field(obj: &Map<String, Value>, key: &str) -> Result<Option<DateTime<Utc>>> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(_) => timestamp_field(obj, key).map(Some),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_identity::delegation::{DelegationPermission, DelegationScope};
    use atomic_identity::IdentityType;
    use chrono::Duration;

    struct Party {
        identity: Identity,
        keypair: KeyPair,
    }

    fn human(name: &str) -> Party {
        let keypair = KeyPair::generate();
        let identity = Identity::new(name, &keypair);
        Party { identity, keypair }
    }

    fn agent(name: &str, parent: &Identity) -> Party {
        let keypair = KeyPair::generate();
        let identity = Identity::builder(name)
            .identity_type(IdentityType::Agent)
            .public_key(keypair.public.clone())
            .delegated_by(parent.id)
            .build()
            .unwrap();
        Party { identity, keypair }
    }

    fn scope() -> DelegationScope {
        DelegationScope::builder()
            .permission(DelegationPermission::Read)
            .permission(DelegationPermission::Record)
            .server("https://atomic.storage")
            .project("acme/*")
            .build()
    }

    fn certificate() -> (Party, Party, Value) {
        let alice = human("alice");
        let claude = agent("alice+claude", &alice.identity);
        let terms = Delegation::new(&alice.identity, &claude.identity, scope())
            .with_software_agent("urn:atomic:agent:claude-code")
            .expires_in(Duration::days(30));
        let doc = mint(&alice.identity, &alice.keypair, &terms);
        (alice, claude, doc)
    }

    #[test]
    fn mint_then_verify_round_trips() {
        let (alice, claude, doc) = certificate();

        let parsed = verify(&doc, &alice.identity.public_key).unwrap();
        assert_eq!(parsed.delegator_name, "alice");
        assert_eq!(parsed.delegate_name, "alice+claude");
        assert_eq!(parsed.delegate, claude.identity.id.to_did());
        assert_eq!(
            parsed.software_agent.as_deref(),
            Some("urn:atomic:agent:claude-code")
        );
        assert!(parsed.scope.has_permission(DelegationPermission::Record));
        assert!(!parsed.scope.has_permission(DelegationPermission::Push));
        assert!(parsed.expires.is_some());
    }

    #[test]
    fn verified_certificate_yields_the_agents_public_key() {
        let (alice, claude, doc) = certificate();
        let parsed = verify(&doc, &alice.identity.public_key).unwrap();

        // This is what makes offline attribution work: the human's key verifies
        // the certificate, and the certificate hands you the agent's key.
        assert_eq!(
            delegate_public_key(&parsed).unwrap(),
            claude.identity.public_key
        );
    }

    #[test]
    fn a_different_key_cannot_verify() {
        let (_alice, _claude, doc) = certificate();
        let mallory = human("mallory");
        assert!(verify(&doc, &mallory.identity.public_key).is_err());
    }

    #[test]
    fn widening_the_scope_breaks_the_proof() {
        let (alice, _claude, mut doc) = certificate();

        // The whole point: an agent that edits its own certificate to add
        // `push` must not be able to use it.
        doc["scope"]["permissions"] = json!(["read", "record", "push"]);
        let err = verify(&doc, &alice.identity.public_key).unwrap_err();
        assert!(
            matches!(err, CanonicalError::HashMismatch { .. }),
            "expected a hash mismatch, got {err:?}"
        );
    }

    #[test]
    fn swapping_the_delegate_key_is_rejected() {
        let (alice, _claude, mut doc) = certificate();
        let mallory = human("mallory");

        // Re-sign a document that keeps the honest `delegate` DID but hands out
        // Mallory's key. Only reachable by someone holding alice's key, but the
        // internal consistency check should catch it regardless.
        doc.as_object_mut().unwrap().remove("proof");
        doc.as_object_mut().unwrap().remove("contentHash");
        doc["delegateKey"] = json!(mallory.identity.public_key.to_did_key());
        let doc = proof::attest_value(doc, &alice.identity, &alice.keypair);

        let err = verify(&doc, &alice.identity.public_key).unwrap_err();
        assert!(
            matches!(err, CanonicalError::Verification(ref m) if m.contains("delegateKey")),
            "expected a delegateKey mismatch, got {err:?}"
        );
    }

    #[test]
    fn swapping_the_id_is_rejected() {
        let (alice, _claude, mut doc) = certificate();

        // Point the certificate at some other delegation's id, then re-sign.
        doc.as_object_mut().unwrap().remove("proof");
        doc.as_object_mut().unwrap().remove("contentHash");
        doc["@id"] = json!(DelegationId::from_bytes([7u8; 32]).to_urn());
        let doc = proof::attest_value(doc, &alice.identity, &alice.keypair);

        let err = verify(&doc, &alice.identity.public_key).unwrap_err();
        assert!(
            matches!(err, CanonicalError::Verification(ref m) if m.contains("@id")),
            "expected an @id mismatch, got {err:?}"
        );
    }

    #[test]
    fn transport_round_trips_and_stays_verifiable() {
        let (alice, _claude, doc) = certificate();

        let encoded = encode_for_transport(&doc);
        let decoded = decode_from_transport(&encoded).unwrap();

        // The whole premise: a certificate that travelled over the wire still
        // verifies against the delegator's key.
        assert!(verify(&decoded, &alice.identity.public_key).is_ok());
    }

    #[test]
    fn transport_encoding_is_deterministic() {
        // Content-hash caching on the server depends on this: the same
        // certificate must encode identically every time, whatever key order
        // it happened to be serialized in.
        let (_alice, _claude, doc) = certificate();
        let shuffled: Value = serde_json::from_str(&serde_json::to_string(&doc).unwrap()).unwrap();

        assert_eq!(encode_for_transport(&doc), encode_for_transport(&shuffled));
        assert_eq!(
            transport_fingerprint(&encode_for_transport(&doc)),
            transport_fingerprint(&encode_for_transport(&shuffled))
        );
    }

    #[test]
    fn a_tampered_certificate_survives_transport_but_fails_verification() {
        // Decoding must not be mistaken for trust — this is why they are two
        // functions and the size/format check never implies a valid proof.
        let (alice, _claude, mut doc) = certificate();
        doc["scope"]["permissions"] = json!(["full"]);

        let decoded = decode_from_transport(&encode_for_transport(&doc)).unwrap();
        assert!(verify(&decoded, &alice.identity.public_key).is_err());
    }

    #[test]
    fn an_oversized_payload_is_refused_before_parsing() {
        let huge = "A".repeat(MAX_ENCODED_DELEGATION + 1);
        let err = decode_from_transport(&huge).unwrap_err();
        assert!(err.to_string().contains("over the"), "{err}");
    }

    #[test]
    fn malformed_transport_input_is_an_error_not_a_panic() {
        assert!(decode_from_transport("not base64url!!").is_err());
        // Valid base64url, not JSON.
        assert!(decode_from_transport(&data_encoding::BASE64URL_NOPAD.encode(b"nope")).is_err());
        assert!(decode_from_transport("").is_err());
    }

    #[test]
    fn different_certificates_fingerprint_differently() {
        let (_a, _b, one) = certificate();
        let (_c, _d, two) = certificate();
        assert_ne!(
            transport_fingerprint(&encode_for_transport(&one)),
            transport_fingerprint(&encode_for_transport(&two))
        );
    }

    #[test]
    fn self_contained_verify_needs_no_external_key() {
        // A CI runner holds the agent key and the certificate, and nothing else.
        let (alice, _claude, doc) = certificate();
        let parsed = verify_self_contained(&doc).unwrap();
        assert_eq!(parsed.delegator_name, "alice");
        assert_eq!(
            delegator_public_key(&parsed).unwrap(),
            alice.identity.public_key
        );
    }

    #[test]
    fn self_contained_verify_still_catches_tampering() {
        let (_alice, _claude, mut doc) = certificate();
        doc["scope"]["permissions"] = json!(["full"]);
        assert!(verify_self_contained(&doc).is_err());
    }

    #[test]
    fn a_certificate_resigned_by_another_key_fails_against_the_named_delegator() {
        // Mallory re-signs alice's certificate with her own key but leaves the
        // `delegator`/`delegatorKey` fields naming alice. Integrity checks must
        // catch the mismatch rather than accepting Mallory's signature.
        let (alice, _claude, mut doc) = certificate();
        let mallory = human("mallory");
        doc.as_object_mut().unwrap().remove("proof");
        doc.as_object_mut().unwrap().remove("contentHash");
        doc.as_object_mut().unwrap().remove("attributedTo");
        let doc = proof::attest_value(doc, &mallory.identity, &mallory.keypair);

        // Against alice's key: the signature is not hers.
        assert!(verify(&doc, &alice.identity.public_key).is_err());
        // Against mallory's key: the document names alice as delegator.
        assert!(verify(&doc, &mallory.identity.public_key).is_err());
        // Self-contained: same conclusion, no external key needed.
        assert!(verify_self_contained(&doc).is_err());
    }

    #[test]
    fn a_certificate_is_not_a_revocation() {
        let (alice, _claude, doc) = certificate();
        assert!(verify_revocation(&doc, &alice.identity.public_key).is_err());
        assert!(verify_request(&doc).is_err());
    }

    #[test]
    fn request_self_signature_proves_possession() {
        let alice = human("alice");
        let runner = agent("ci-runner", &alice.identity);

        let request = mint_request(
            &runner.identity,
            &runner.keypair,
            Some("urn:atomic:agent:ci"),
        );
        let parsed = verify_request(&request).unwrap();

        assert_eq!(parsed.delegate_name, "ci-runner");
        assert_eq!(parsed.delegate, runner.identity.id.to_did());
        assert_eq!(
            parsed.software_agent.as_deref(),
            Some("urn:atomic:agent:ci")
        );
    }

    #[test]
    fn a_request_naming_someone_elses_key_is_rejected() {
        let alice = human("alice");
        let runner = agent("ci-runner", &alice.identity);
        let mallory = human("mallory");

        // Mallory signs a request but claims the runner's DID.
        let mut doc = mint_request(&mallory.identity, &mallory.keypair, None);
        doc.as_object_mut().unwrap().remove("proof");
        doc.as_object_mut().unwrap().remove("contentHash");
        doc["delegate"] = json!(runner.identity.id.to_did());
        let doc = proof::attest_value(doc, &mallory.identity, &mallory.keypair);

        assert!(verify_request(&doc).is_err());
    }

    #[test]
    fn revocation_round_trips() {
        let (alice, _claude, doc) = certificate();
        let parsed = verify(&doc, &alice.identity.public_key).unwrap();

        let revocation = mint_revocation(
            &alice.identity,
            &alice.keypair,
            &parsed.id,
            Some("laptop lost"),
        );
        let checked = verify_revocation(&revocation, &alice.identity.public_key).unwrap();

        assert_eq!(checked.delegation, parsed.id.to_urn());
        assert_eq!(checked.reason.as_deref(), Some("laptop lost"));
    }

    #[test]
    fn only_the_delegator_can_revoke() {
        let (alice, _claude, doc) = certificate();
        let parsed = verify(&doc, &alice.identity.public_key).unwrap();
        let mallory = human("mallory");

        // Mallory signs a revocation for alice's delegation.
        let revocation = mint_revocation(&mallory.identity, &mallory.keypair, &parsed.id, None);

        // It verifies as Mallory's own document...
        assert!(verify_revocation(&revocation, &mallory.identity.public_key).is_ok());
        // ...but a server checking it against the delegator's key rejects it.
        assert!(verify_revocation(&revocation, &alice.identity.public_key).is_err());
    }

    #[test]
    fn parse_does_not_require_a_valid_proof() {
        let (_alice, _claude, mut doc) = certificate();
        doc["scope"]["permissions"] = json!(["full"]);

        // parse is for display; it must not be mistaken for authorization.
        let parsed = parse(&doc).unwrap();
        assert!(parsed.scope.has_permission(DelegationPermission::Full));
    }

    #[test]
    fn timestamps_are_second_precision_for_stable_bytes() {
        let (alice, _claude, doc) = certificate();
        let issued = doc["issued"].as_str().unwrap();
        assert!(issued.ends_with('Z'), "issued should be UTC: {issued}");
        assert!(
            !issued.contains('.'),
            "sub-second precision makes canonical bytes fragile: {issued}"
        );
        // And the document still verifies after a JSON round trip.
        let round_tripped: Value =
            serde_json::from_str(&serde_json::to_string(&doc).unwrap()).unwrap();
        assert!(verify(&round_tripped, &alice.identity.public_key).is_ok());
    }
}
