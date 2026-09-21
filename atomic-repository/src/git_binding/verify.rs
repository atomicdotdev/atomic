//! Structural and cryptographic verification of bindings against Git.
//!
//! Content correctness never depends on the signer: the bound commit, tree,
//! and ordered parents are re-read from the Git object database and compared
//! to the binding (RFC §5.2 structural check). The signature check is
//! separate and only authenticates provenance.

use atomic_core::operation::GitHashAlgorithm;

use super::codec::{commit_object_digest, BindingDecodeError, GitObjectFormat, GitOid, GitStateBinding};

/// Verify the cryptographic layer of a binding: canonical decoding, bound
/// derived-field recomputation, and the Ed25519 signature.
pub fn verify_binding_cryptography(
    binding: &GitStateBinding,
) -> Result<(), BindingVerificationError> {
    binding
        .payload()
        .validate()
        .map_err(BindingVerificationError::Invalid)?;
    binding
        .verify_signature()
        .map_err(|error| BindingVerificationError::SignatureCheck(error.to_string()))?;
    Ok(())
}

/// Verify the binding against the live Git object database (RFC §5.2
/// structural check): the commit exists, its tree matches `git_tree`, its
/// complete ordered parents match `git_parents`, and preserved raw commit
/// bytes (when present) are byte-identical to the stored object.
///
/// This never trusts the signature and never materializes anything.
pub fn verify_binding_content(
    git: &git2::Repository,
    binding: &GitStateBinding,
) -> Result<(), BindingVerificationError> {
    let payload = binding.payload();
    payload
        .validate()
        .map_err(BindingVerificationError::Invalid)?;

    // Object-format consistency: every OID must carry the format's width.
    let algorithm = match payload.git_object_format {
        GitObjectFormat::Sha1 => GitHashAlgorithm::Sha1,
        GitObjectFormat::Sha256 => GitHashAlgorithm::Sha256,
    };
    for oid in std::iter::once(&payload.git_commit)
        .chain(std::iter::once(&payload.git_tree))
        .chain(payload.git_parents.iter())
    {
        let object_id = oid.to_git_object_id()?;
        if object_id.algorithm() != algorithm {
            return Err(BindingVerificationError::FormatMismatch {
                expected: format!("{algorithm:?}"),
                oid: oid.to_hex(),
            });
        }
    }

    let commit_oid = git2_oid(&payload.git_commit)?;
    let commit = git.find_commit(commit_oid).map_err(|error| {
        BindingVerificationError::Git {
            message: format!(
                "bound commit {} is absent: {error}",
                payload.git_commit.to_hex()
            ),
        }
    })?;
    let odb = git.odb().map_err(|error| BindingVerificationError::Git {
        message: error.to_string(),
    })?;
    let raw = odb.read(commit_oid).map_err(|error| {
        BindingVerificationError::Git {
            message: format!("bound commit {} is unreadable: {error}", payload.git_commit.to_hex()),
        }
    })?.data().to_vec();

    // Tree: the binding must name the commit's actual tree.
    let actual_tree = commit.tree_id();
    if actual_tree.as_bytes() != payload.git_tree.as_bytes() {
        return Err(BindingVerificationError::TreeMismatch {
            expected: payload.git_tree.to_hex(),
            found: actual_tree.to_string(),
        });
    }

    // Complete ordered parents: count and order both matter.
    let parent_count = commit.parent_count();
    if parent_count as usize != payload.git_parents.len() {
        return Err(BindingVerificationError::ParentMismatch {
            expected: payload.git_parents.len(),
            found: parent_count as usize,
        });
    }
    for (index, parent) in commit.parents().enumerate() {
        let bound = &payload.git_parents[index];
        if parent.id().as_bytes() != bound.as_bytes() {
            return Err(BindingVerificationError::ParentOrder {
                index,
                expected: bound.to_hex(),
                found: parent.id().to_string(),
            });
        }
    }

    // Preserved raw bytes must be the exact stored object.
    if let Some(raw_commit) = &payload.raw_commit_object {
        if raw != raw_commit.as_slice() {
            return Err(BindingVerificationError::RawObjectDiffers {
                commit: payload.git_commit.to_hex(),
            });
        }
        let digest = commit_object_digest(payload.git_object_format, raw_commit)?;
        if digest != payload.git_commit {
            return Err(BindingVerificationError::RawObjectMismatch {
                expected: payload.git_commit.to_hex(),
                found: digest.to_hex(),
            });
        }
    }

    Ok(())
}

/// Check the `atomic-binding <id>` hint in a commit message against a
/// binding. Hints route lookup only (RFC §5.2, §5.4) and are never identity —
/// but a hint that names a *different* binding is a forgery signal and fails
/// closed when the binding is being verified.
pub fn verify_binding_hint(
    commit_message: &str,
    binding: &GitStateBinding,
) -> Result<(), BindingVerificationError> {
    match binding_hint_mismatches_commit(commit_message, &binding.id()) {
        Ok(()) => Ok(()),
        Err(error) => Err(error),
    }
}

/// Parse the `atomic-binding <hex>` hints in a commit message and compare to
/// `binding_id`. Absence of a hint is not an error (hints are hints); any
/// present hint must name exactly this binding.
pub fn binding_hint_mismatches_commit(
    commit_message: &str,
    binding_id: &super::codec::BindingId,
) -> Result<(), BindingVerificationError> {
    let hints = atomic_binding_hints(commit_message);
    for hint in hints {
        if hint != binding_id.to_hex() {
            return Err(BindingVerificationError::HintMismatch {
                claimed: hint,
                actual: binding_id.to_hex(),
            });
        }
    }
    Ok(())
}

/// Every `atomic-binding <hex>` value in a commit message (header or trailer).
fn atomic_binding_hints(message: &str) -> Vec<String> {
    let mut hints = Vec::new();
    for line in message.lines() {
        let line = line.trim_start();
        if let Some(rest) = line.strip_prefix("atomic-binding ") {
            let value = rest.trim();
            if !value.is_empty() && value.chars().all(|c| c.is_ascii_hexdigit()) {
                hints.push(value.to_ascii_lowercase());
            }
        }
    }
    hints
}

fn git2_oid(oid: &GitOid) -> Result<git2::Oid, BindingVerificationError> {
    git2::Oid::from_bytes(oid.as_bytes()).map_err(|error| BindingVerificationError::Git {
        message: error.to_string(),
    })
}

/// Verification failures: the binding does not match the Git object database.
#[derive(Debug, thiserror::Error)]
pub enum BindingVerificationError {
    #[error("binding is invalid: {0}")]
    Invalid(#[from] super::codec::BindingEncodeError),
    #[error("binding decoding failed: {0}")]
    Decode(#[from] BindingDecodeError),
    #[error("binding signature verification failed: {0}")]
    SignatureCheck(String),
    #[error("bound object {oid} uses a different object format than {expected}")]
    FormatMismatch { expected: String, oid: String },
    #[error("Git object database error: {message}")]
    Git { message: String },
    #[error("binding tree {expected} does not match the commit's tree {found}")]
    TreeMismatch { expected: String, found: String },
    #[error("binding lists {expected} parents but the commit has {found}")]
    ParentMismatch { expected: usize, found: usize },
    #[error("parent {index} is {found} but the binding orders {expected}")]
    ParentOrder {
        index: usize,
        expected: String,
        found: String,
    },
    #[error("preserved raw commit bytes for {commit} differ from the stored object")]
    RawObjectDiffers { commit: String },
    #[error("raw commit bytes digest {found} does not match the bound commit {expected}")]
    RawObjectMismatch { expected: String, found: String },
    #[error("commit claims atomic-binding {claimed} but binding {actual} is being verified")]
    HintMismatch { claimed: String, actual: String },
}
