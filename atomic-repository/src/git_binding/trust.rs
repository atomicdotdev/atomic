//! Trust evaluation: signer trust is independent of content validity.
//!
//! RFC §5.2/§5.6/§12.8: the bound Git tree is *always* recomputed and
//! compared, no matter who signed the binding. A signature never vouches for
//! content; it only gates whether the provenance/attestation roots may be
//! trusted. A cryptographically valid binding from an unknown signer therefore
//! still yields correct recomputed content, but its provenance claims stay
//! explicitly untrusted and can never satisfy a publication gate (§10.4/§12.10).

use atomic_config::{GitTrustConfig, SignerTrust};

use super::codec::GitStateBinding;

/// One binding's trust evaluation, split exactly along the RFC boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindingTrustEvaluation {
    /// The Ed25519 signature verified against the carried verifying key.
    pub signature_valid: bool,
    /// Where the signer falls under the repository's trust policy.
    pub signer_trust: SignerTrust,
    /// Whether the bound content verified structurally (commit/tree/parents/
    /// closure recomputation). Signer trust never influences this value.
    pub content_verified: bool,
}

impl BindingTrustEvaluation {
    /// May the binding's provenance/attestation roots be trusted?
    ///
    /// True only when the signature verifies, the signer is trusted under
    /// policy, and the content verified. Unknown or revoked signers always
    /// yield `false`, even with byte-perfect content.
    pub fn provenance_trusted(&self) -> bool {
        self.signature_valid && self.signer_trust == SignerTrust::Trusted && self.content_verified
    }

    /// May the recomputed content be used for resurrection?
    ///
    /// Content validity is independent of the signer: a valid unknown signer
    /// can supply correct content (RFC §5.2, `TRUST` step applies the trust
    /// policy *after* the tree comparison succeeds).
    pub fn content_usable(&self) -> bool {
        self.content_verified
    }

    /// Can this binding satisfy a publication gate (§10.4/§12.4)?
    ///
    /// Untrusted provenance can never satisfy a gate; content correctness
    /// alone is not enough.
    pub fn can_satisfy_publication_gate(&self) -> bool {
        self.provenance_trusted()
    }
}

/// Inputs for [`evaluate_binding_trust`].
#[derive(Debug, Clone, Copy)]
pub struct BindingVerificationInput<'a> {
    /// The repository's own signer DID (the default trust root), if any.
    pub repository_identity: Option<&'a str>,
}

/// Evaluate one binding: signature validity, signer trust, and content
/// validity stay separate fields; provenance trust requires all three.
///
/// `content_verified` must come from [`super::verify_binding_content`] (and,
/// in CB-6C, from complete tree recomputation) — never from the signature.
pub fn evaluate_binding_trust(
    binding: &GitStateBinding,
    policy: &GitTrustConfig,
    input: BindingVerificationInput<'_>,
    content_verified: bool,
) -> BindingTrustEvaluation {
    let signature_valid = binding.verify_signature().is_ok();
    let signer_trust = if signature_valid {
        policy.evaluate(&binding.payload().signer.did, input.repository_identity)
    } else {
        // An unverifiable signature is never trusted, whatever the policy.
        SignerTrust::Unknown
    };
    BindingTrustEvaluation {
        signature_valid,
        signer_trust,
        content_verified,
    }
}

/// Convenience: signer trust for a DID under the policy, with the repository
/// identity as the default trust root.
pub fn signer_trust_for(
    policy: &GitTrustConfig,
    signer_did: &str,
    repository_identity: Option<&str>,
) -> SignerTrust {
    policy.evaluate(signer_did, repository_identity)
}

/// Provenance roots explicitly marked untrusted: the evaluation result a
/// publication gate consumes. The roots themselves are returned unchanged —
/// they are hashes, and the *claim* they belong to is what is untrusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UntrustedProvenance {
    /// Why the provenance claims are untrusted.
    pub reason: UntrustedReason,
}

/// Why provenance claims are not trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UntrustedReason {
    /// The signature does not verify.
    SignatureInvalid,
    /// The signer is not in the trust policy.
    SignerUnknown,
    /// The signer is explicitly revoked.
    SignerRevoked,
    /// The bound content did not verify.
    ContentUnverified,
}

impl BindingTrustEvaluation {
    /// Explain why provenance is untrusted, when it is.
    pub fn untrusted_reason(&self) -> Option<UntrustedReason> {
        if self.provenance_trusted() {
            return None;
        }
        if !self.signature_valid {
            return Some(UntrustedReason::SignatureInvalid);
        }
        match self.signer_trust {
            SignerTrust::Revoked => Some(UntrustedReason::SignerRevoked),
            SignerTrust::Unknown => Some(UntrustedReason::SignerUnknown),
            SignerTrust::Trusted => Some(UntrustedReason::ContentUnverified),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(signers: &[&str], revoked: &[&str]) -> GitTrustConfig {
        GitTrustConfig {
            signers: signers.iter().map(|did| did.to_string()).collect(),
            revoked: revoked.iter().map(|did| did.to_string()).collect(),
        }
    }

    #[test]
    fn unknown_signer_yields_untrusted_provenance_despite_valid_content() {
        let evaluation = BindingTrustEvaluation {
            signature_valid: true,
            signer_trust: SignerTrust::Unknown,
            content_verified: true,
        };
        // Content remains usable — recomputation decides content.
        assert!(evaluation.content_usable());
        // Provenance does not, and no gate may be satisfied.
        assert!(!evaluation.provenance_trusted());
        assert!(!evaluation.can_satisfy_publication_gate());
        assert_eq!(
            evaluation.untrusted_reason(),
            Some(UntrustedReason::SignerUnknown)
        );
    }

    #[test]
    fn revoked_signer_is_never_trusted() {
        let evaluation = BindingTrustEvaluation {
            signature_valid: true,
            signer_trust: SignerTrust::Revoked,
            content_verified: true,
        };
        assert!(!evaluation.provenance_trusted());
        assert_eq!(
            evaluation.untrusted_reason(),
            Some(UntrustedReason::SignerRevoked)
        );
    }

    #[test]
    fn trusted_signer_with_unverified_content_stays_untrusted() {
        let evaluation = BindingTrustEvaluation {
            signature_valid: true,
            signer_trust: SignerTrust::Trusted,
            content_verified: false,
        };
        // The signature cannot substitute for the tree proof (§12.8).
        assert!(!evaluation.provenance_trusted());
        assert!(!evaluation.content_usable());
        assert_eq!(
            evaluation.untrusted_reason(),
            Some(UntrustedReason::ContentUnverified)
        );
    }

    #[test]
    fn trusted_signer_with_verified_content_is_trusted() {
        let evaluation = BindingTrustEvaluation {
            signature_valid: true,
            signer_trust: SignerTrust::Trusted,
            content_verified: true,
        };
        assert!(evaluation.provenance_trusted());
        assert!(evaluation.can_satisfy_publication_gate());
        assert_eq!(evaluation.untrusted_reason(), None);
    }

    #[test]
    fn policy_evaluation_matrix() {
        let policy = policy(&["did:atomic:COLLAB"], &["did:atomic:BANNED"]);
        assert_eq!(
            signer_trust_for(&policy, "did:atomic:REPO", Some("did:atomic:REPO")),
            SignerTrust::Trusted
        );
        assert_eq!(
            signer_trust_for(&policy, "did:atomic:COLLAB", Some("did:atomic:REPO")),
            SignerTrust::Trusted
        );
        assert_eq!(
            signer_trust_for(&policy, "did:atomic:STRANGER", Some("did:atomic:REPO")),
            SignerTrust::Unknown
        );
        assert_eq!(
            signer_trust_for(&policy, "did:atomic:BANNED", Some("did:atomic:REPO")),
            SignerTrust::Revoked
        );
    }
}
