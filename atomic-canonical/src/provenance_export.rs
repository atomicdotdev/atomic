//! Optional DSSE transport for the existing W3C PROV projection.
//!
//! The exporter's key covers the payload type and exact JSON payload bytes.
//! This proves neither original authorship nor the truth/completeness of the
//! graph's claims. Referenced changes and evidence are not included or verified.
//! Any native Data Integrity proof remains inside the projection unchanged;
//! verifying that proof is a separate operation.

use atomic_identity::keypair::{KeyPair, PublicKey};
use dsse::{Envelope, VerifiedPayload};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// Opaque, case-sensitive DSSE payload type for this version of the export.
pub const PAYLOAD_TYPE: &str = "application/vnd.atomic.provenance-export.v1+json";
/// Largest envelope or decoded payload accepted by this consumer (8 MiB).
pub const MAX_EXPORT_BYTES: usize = 8 * 1024 * 1024;
const SCHEMA: &str = "atomic-provenance-export/v1";
const COVERAGE: &str = "exporter-signed-projections-only";

/// Refusals at the transport, signature or export-profile boundary.
#[derive(Debug, Error)]
pub enum ExportError {
    #[error("DSSE: {0}")]
    Dsse(#[from] dsse::Error),
    #[error("export JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("export admission: {0}")]
    Admission(#[from] crate::CanonicalError),
    #[error("provenance export profile: {0}")]
    Profile(&'static str),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportPayload {
    schema: String,
    coverage: String,
    projections: Vec<Value>,
}

struct AtomicSigner<'a>(&'a KeyPair);

impl dsse::Signer for AtomicSigner<'_> {
    fn sign(&self, message: &[u8]) -> dsse::Result<Vec<u8>> {
        Ok(self.0.sign(message).to_vec())
    }

    fn key_id(&self) -> Option<String> {
        Some(self.0.public.to_did_key())
    }
}

struct AtomicVerifier<'a>(&'a PublicKey);

impl dsse::Verifier for AtomicVerifier<'_> {
    fn verify(&self, message: &[u8], signature: &[u8]) -> bool {
        let Ok(bytes) = <&[u8; 64]>::try_from(signature) else {
            return false;
        };
        self.0.verify(message, bytes).is_ok()
    }

    fn key_id(&self) -> Option<String> {
        Some(self.0.to_did_key())
    }
}

/// Sign existing projection(s) with an exporter key; native proofs stay intact.
///
/// No JSON-LD expansion, canonicalization or repository mutation occurs. The
/// payload is serialized once and those bytes are handed to DSSE unchanged.
///
/// # Examples
/// ```
/// use atomic_canonical::{provenance_export::*, prov::{project, ProvActivityInput}};
/// use atomic_identity::KeyPair;
/// # let input = ProvActivityInput { change_id_base32: "ABC".into(), activity_id: "turn".into(), started_at: None, ended_at: None, agent_slug: "agent".into(), agent_display_name: "Agent".into(), agent_vendor: None, person_did: "did:atomic:person".into(), generated: vec![], used: vec![], turn_parent: None };
/// let exporter = KeyPair::generate();
/// let envelope = sign_provenance_export(&[project(&input)], &exporter)?;
/// let verified = verify_provenance_export(&envelope.to_json()?, &exporter.public)?;
/// let payload: serde_json::Value = serde_json::from_slice(&verified.payload)?;
/// assert_eq!(payload["coverage"], "exporter-signed-projections-only");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn sign_provenance_export(
    projections: &[Value],
    keypair: &KeyPair,
) -> Result<Envelope, ExportError> {
    let payload = ExportPayload {
        schema: SCHEMA.into(),
        coverage: COVERAGE.into(),
        projections: projections.to_vec(),
    };
    validate_profile(&payload)?;
    let bytes = serde_json::to_vec(&payload)?;
    bounded(&bytes)?;
    crate::jcs::admit_document(&bytes)?;
    let envelope = dsse::sign(PAYLOAD_TYPE, &bytes, &AtomicSigner(keypair))?;
    // Base64 and the signature make the container larger than its payload.
    // Refuse an export our own consumer would reject.
    bounded(&envelope.to_json()?)?;
    Ok(envelope)
}

/// Verify transport and profile using an independently selected exporter key.
///
/// The envelope's `keyid` never selects a trusted key. The returned bytes come
/// directly from DSSE's `VerifiedPayload`; consume those bytes, never decode
/// the original envelope again. This checks neither native proofs nor claims.
pub fn verify_provenance_export(
    envelope_json: &[u8],
    trusted_key: &PublicKey,
) -> Result<VerifiedPayload, ExportError> {
    bounded(envelope_json)?;
    // Admit raw bytes before parsing so duplicate members cannot be collapsed.
    let envelope: Envelope = serde_json::from_value(crate::jcs::admit_document(envelope_json)?)?;
    if envelope.payload_type != PAYLOAD_TYPE {
        return Err(ExportError::Profile("wrong payload type"));
    }
    let verified = dsse::verify(&envelope, &[&AtomicVerifier(trusted_key)], 1)?;
    bounded(&verified.payload)?;
    let payload: ExportPayload =
        serde_json::from_value(crate::jcs::admit_document(&verified.payload)?)?;
    validate_profile(&payload)?;
    Ok(verified)
}

fn bounded(bytes: &[u8]) -> Result<(), ExportError> {
    if bytes.len() > MAX_EXPORT_BYTES {
        return Err(ExportError::Profile("export exceeds 8 MiB limit"));
    }
    Ok(())
}

fn validate_profile(payload: &ExportPayload) -> Result<(), ExportError> {
    if payload.schema != SCHEMA || payload.coverage != COVERAGE {
        return Err(ExportError::Profile("unsupported schema or coverage"));
    }
    if payload.projections.is_empty() {
        return Err(ExportError::Profile("no projections"));
    }
    for projection in &payload.projections {
        if !projection
            .get("@id")
            .and_then(Value::as_str)
            .is_some_and(|id| id.starts_with("urn:atomic:provgraph:"))
            || !projection.get("@graph").is_some_and(Value::is_array)
        {
            return Err(ExportError::Profile("not an Atomic PROV projection"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prov::{attest_prov, project, verify_prov, ProvActivityInput};
    use atomic_identity::Identity;
    use serde_json::json;

    fn input() -> ProvActivityInput {
        ProvActivityInput {
            change_id_base32: "ABC".into(),
            activity_id: "session#ABC".into(),
            started_at: None,
            ended_at: None,
            agent_slug: "claude".into(),
            agent_display_name: "Claude Code".into(),
            agent_vendor: None,
            person_did: "did:atomic:person".into(),
            generated: vec!["urn:atomic:change:ABC".into()],
            used: vec![],
            turn_parent: None,
        }
    }

    #[test]
    fn distinct_exporter_preserves_native_proof_and_verification() {
        let author = KeyPair::generate();
        let identity = Identity::new("author", &author);
        let native = attest_prov(&input(), &identity, &author);
        let exporter = KeyPair::generate();
        let envelope = sign_provenance_export(&[native.clone()], &exporter).unwrap();
        let verified =
            verify_provenance_export(&envelope.to_json().unwrap(), &exporter.public).unwrap();
        let payload: Value = serde_json::from_slice(&verified.payload).unwrap();
        assert_eq!(payload["projections"][0], native);
        verify_prov(&payload["projections"][0], &author.public).unwrap();
        assert!(verify_prov(&payload["projections"][0], &exporter.public).is_err());
        assert_eq!(payload["coverage"], COVERAGE);
    }

    #[test]
    fn wrong_key_type_tamper_and_malformed_signature_are_refused() {
        let key = KeyPair::generate();
        let envelope = sign_provenance_export(&[project(&input())], &key).unwrap();
        let check = |e: &Envelope| verify_provenance_export(&e.to_json().unwrap(), &key.public);
        assert!(verify_provenance_export(
            &envelope.to_json().unwrap(),
            &KeyPair::generate().public
        )
        .is_err());
        let mut wrong_type = envelope.clone();
        wrong_type.payload_type = "application/json".into();
        assert!(matches!(check(&wrong_type), Err(ExportError::Profile(_))));
        let mut tampered = envelope.clone();
        // Canonical base64 of unrelated JSON; the old signature cannot cover it.
        tampered.payload = "e30=".into();
        assert!(check(&tampered).is_err());
        let mut malformed = envelope.clone();
        malformed.signatures[0].sig = "eA==".into();
        assert!(check(&malformed).is_err());
        malformed.signatures[0].sig = "%%%".into();
        assert!(check(&malformed).is_err());
        malformed.payload = "eB==".into();
        assert!(check(&malformed).is_err());
        let mut hint = envelope;
        hint.signatures[0].keyid = Some("attacker-controlled-label".into());
        assert!(check(&hint).is_ok());
    }

    #[test]
    fn authentic_signature_does_not_override_payload_profile_or_admission() {
        let key = KeyPair::generate();
        let valid = ExportPayload {
            schema: SCHEMA.into(),
            coverage: COVERAGE.into(),
            projections: vec![project(&input())],
        };
        let mut value = serde_json::to_value(valid).unwrap();
        for bad in [
            {
                let mut v = value.clone();
                v["schema"] = json!("future/v2");
                v
            },
            {
                let mut v = value.clone();
                v["coverage"] = json!("everything-verified");
                v
            },
            {
                let mut v = value.clone();
                v["projections"] = json!([]);
                v
            },
            {
                let mut v = value.clone();
                v["projections"] = json!([{}]);
                v
            },
            {
                let mut v = value.clone();
                v["unexpected"] = json!(true);
                v
            },
        ] {
            let bytes = serde_json::to_vec(&bad).unwrap();
            let envelope = dsse::sign(PAYLOAD_TYPE, &bytes, &AtomicSigner(&key)).unwrap();
            assert!(verify_provenance_export(&envelope.to_json().unwrap(), &key.public).is_err());
        }
        value["projections"][0]["extra"] = json!(1.5);
        assert!(sign_provenance_export(&[value["projections"][0].clone()], &key).is_err());
        let duplicate = br#"{"schema":"atomic-provenance-export/v1","schema":"atomic-provenance-export/v1","coverage":"exporter-signed-projections-only","projections":[]}"#;
        let envelope = dsse::sign(PAYLOAD_TYPE, duplicate, &AtomicSigner(&key)).unwrap();
        assert!(matches!(
            verify_provenance_export(&envelope.to_json().unwrap(), &key.public),
            Err(ExportError::Admission(_))
        ));
        let json = String::from_utf8(envelope.to_json().unwrap()).unwrap();
        let duplicate_envelope = json.replacen('{', "{\"payloadType\":\"wrong\",", 1);
        assert!(matches!(
            verify_provenance_export(duplicate_envelope.as_bytes(), &key.public),
            Err(ExportError::Admission(_))
        ));
        assert!(verify_provenance_export(&vec![b' '; MAX_EXPORT_BYTES + 1], &key.public).is_err());
    }
}
