//! State-tag ↔ Git-tag projection (RFC §8.4, CB-8A).
//!
//! Atomic state tags (`TAG_RECORDS`, `MERKLE_CHAIN`) export as Git **annotated**
//! tags `refs/tags/<name>` whose message carries the `atomic-binding` of the
//! bound state. Restoring (importing) the tag restores the same bound state.
//! Git lightweight tags import as Atomic tags pinned to the bound state of the
//! commit they peel to. ReviewGate/aggregate tags stay Atomic-only: they
//! project nothing.
//!
//! Privacy: a Git tag carries only hashes and an annotation — provenance
//! roots, attestation roots, transcripts, prompts, and decision graphs never
//! enter the tag object (RFC §5.6/§12.13).

use atomic_core::pristine::{TagKind, TagRecord};
use atomic_core::types::{Base32, Merkle};
use chrono::Utc;

use crate::error::RepositoryError;
use crate::git_binding::{binding_hint_mismatches_commit, verify_binding_content, GitOid};
use crate::repository::Repository;

/// The annotated-tag trailer that names the immutable binding.
const BINDING_HINT_KEY: &str = "atomic-binding";

/// What one tag projection attempt produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TagProjectionOutcome {
    /// The tag was exported as a Git annotated tag.
    Exported {
        /// `refs/tags/<name>`.
        tag_ref: String,
        /// The bound commit the annotated tag points at (hex).
        target: String,
        /// The `atomic-binding <hex>` value carried in the tag message.
        binding_id: String,
    },
    /// The tag is Atomic-only by policy and produced no Git tag.
    AtomicOnly { kind: &'static str },
}

impl Repository {
    /// Export one Atomic state tag as a Git annotated tag (RFC §8.4).
    ///
    /// The annotated tag's message carries the `atomic-binding` of the bound
    /// state, so a fresh store can restore the exact bound Merkle state.
    /// ReviewGate/aggregate tags are Atomic-only and project nothing.
    ///
    /// The export is read-only on the Atomic side and never moves Git HEAD,
    /// the Git index, or any view membership: a tag is a bookmark, not a
    /// transition. The bound state must carry a fully verified Git-state
    /// binding (signature + content equivalence against the live Git object
    /// database); an unbound state is refused, never silently promoted.
    pub fn export_tag_to_git(
        &self,
        view: &str,
        name: &str,
        git: &git2::Repository,
    ) -> Result<TagProjectionOutcome, RepositoryError> {
        let tag = self
            .get_tag_from_view(name, view)?
            .ok_or(RepositoryError::TagNotFound {
                name: name.to_string(),
            })?;
        // ReviewGate/aggregate tags are Atomic-only: they project nothing.
        if matches!(tag.kind, TagKind::ReviewGate) {
            return Ok(TagProjectionOutcome::AtomicOnly {
                kind: "review-gate",
            });
        }
        // The tagged state must be bound: the annotated tag message names the
        // immutable binding, so a bookmark for an unbound state is refused
        // rather than silently promoted.
        let binding = self
            .verified_binding_for_state(git, &tag.state)?
            .ok_or_else(|| RepositoryError::InvalidOperation {
                message: format!(
                    "tag '{name}' targets state {}, which has no verified Git state \
                     binding; run `atomic git bridge enable` to bind it first",
                    Base32::to_base32(&tag.state)
                ),
            })?;
        let binding_hex = binding.id().to_hex();
        if git.find_reference(&format!("refs/tags/{name}")).is_ok() {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "Git tag '{name}' already exists; Atomic tags never overwrite \
                     existing Git tags"
                ),
            });
        }
        let tagger = git.signature().unwrap_or_else(|_| {
            git2::Signature::now("atomic", "atomic@localhost").expect("fallback tagger signature")
        });
        let message = export_tag_message(view, &tag, &binding_hex);
        let target = to_git2_oid(&binding.payload().git_commit)?;
        let commit = git
            .find_commit(target)
            .map_err(|error| git_map(error.to_string()))?;
        git.tag(name, commit.as_object(), &tagger, &message, false)
            .map_err(|error| git_map(error.to_string()))?;
        Ok(TagProjectionOutcome::Exported {
            tag_ref: format!("refs/tags/{name}"),
            target: target.to_string(),
            binding_id: binding_hex,
        })
    }

    /// Import a Git tag as an Atomic state tag (RFC §8.4).
    ///
    /// The tag's peeled commit must be bound; the imported Atomic tag is
    /// pinned to the binding's Merkle state, so restoring the tag restores
    /// the exact bound state. An annotated Git tag keeps its message as the
    /// Atomic tag message; a lightweight tag imports with none. A wrong
    /// (forged `atomic-binding` trailer) or missing (unbound commit) binding
    /// is refused, never approximated.
    pub fn import_git_tag_from_git(
        &self,
        view: &str,
        git: &git2::Repository,
        name: &str,
    ) -> Result<TagRecord, RepositoryError> {
        let reference_name = format!("refs/tags/{name}");
        let reference =
            git.find_reference(&reference_name)
                .map_err(|_| RepositoryError::TagNotFound {
                    name: name.to_string(),
                })?;
        let target = reference
            .peel_to_commit()
            .map_err(|error| git_map(error.to_string()))?
            .id();
        let target_hex = target.to_string();
        let binding_oid =
            GitOid::from_hex(&target_hex).map_err(|error| git_map(error.to_string()))?;
        let commit_id = binding_oid
            .to_git_object_id()
            .map_err(|error| git_map(error.to_string()))?;
        let binding = self
            .verified_binding_for_commit(git, &commit_id)?
            .ok_or_else(|| RepositoryError::InvalidOperation {
                message: format!(
                    "Git tag '{name}' peels to commit {target_hex}, which has no verified \
                     Git state binding; refusing to import an unbound tag"
                ),
            })?;
        // A tag message that names a *different* binding is a forgery signal.
        // Absence of a hint is not an error (lightweight tags carry no
        // message at all); any present hint must name exactly this binding.
        let annotation_message: Option<String> = reference
            .peel(git2::ObjectType::Tag)
            .ok()
            .and_then(|object| object.as_tag().cloned())
            .map(|tag| {
                String::from_utf8_lossy(tag.message_bytes().unwrap_or_default()).to_string()
            });
        if let Some(message) = &annotation_message {
            if let Err(error) = binding_hint_mismatches_commit(message, &binding.id()) {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "Git tag '{name}' claims a binding that does not match the bound \
                         commit {target_hex}; refusing a forged tag: {error}"
                    ),
                });
            }
        }
        let record = TagRecord {
            name: name.to_string(),
            view: view.to_string(),
            sequence: 0,
            state: binding.payload().merkle_state,
            change_hash: Merkle::ZERO,
            timestamp: bound_commit_timestamp(git, target),
            author: None,
            message: annotation_message,
            kind: TagKind::Release,
            metadata: Some(serde_json::json!({
                "git": {
                    "sha": target_hex,
                    "binding": binding.id().to_hex(),
                }
            })),
        };
        self.save_synced_tag(&record)?;
        Ok(record)
    }

    /// Find a locally stored, fully verified binding whose payload pins
    /// exactly `state`.
    ///
    /// "Verified" mirrors
    /// [`verified_binding_for_commit`](crate::repository::Repository::verified_binding_for_commit):
    /// both the binding signature verifies and the bound Git content matches
    /// the live Git object database. Bindings that fail either check are
    /// skipped, never trusted by their position or trailers.
    fn verified_binding_for_state(
        &self,
        git: &git2::Repository,
        state: &Merkle,
    ) -> Result<Option<crate::git_binding::GitStateBinding>, RepositoryError> {
        for id in self.binding_ids()? {
            let Ok(Some(binding)) = self.load_binding(&id) else {
                continue;
            };
            if &binding.payload().merkle_state != state {
                continue;
            }
            if crate::git_binding::verify_binding_cryptography(&binding).is_err() {
                continue;
            }
            if verify_binding_content(git, &binding).is_err() {
                continue;
            }
            return Ok(Some(binding));
        }
        Ok(None)
    }
}

/// The annotated tag's message: the Atomic tag message as body, the immutable
/// binding as the `atomic-binding` trailer a fresh store restores from.
fn export_tag_message(view: &str, tag: &TagRecord, binding_hex: &str) -> String {
    let body = tag
        .message
        .clone()
        .unwrap_or_else(|| format!("Atomic state tag '{}' on view '{}'", tag.name, view));
    format!("{body}\n\n{BINDING_HINT_KEY} {binding_hex}\n")
}

fn to_git2_oid(oid: &GitOid) -> Result<git2::Oid, RepositoryError> {
    let bytes = match oid {
        GitOid::Sha1(bytes) => bytes.as_slice(),
        GitOid::Sha256(bytes) => bytes.as_slice(),
    };
    git2::Oid::from_bytes(bytes).map_err(|error| git_map(error.to_string()))
}

/// The import timestamp is derived from the bound commit — deterministic, so
/// re-importing the same Git tag is an idempotent metadata transition.
fn bound_commit_timestamp(git: &git2::Repository, target: git2::Oid) -> chrono::DateTime<Utc> {
    git.find_commit(target)
        .ok()
        .map(|commit| commit.time().seconds())
        .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0))
        .unwrap_or_else(Utc::now)
}

fn git_map(message: impl std::fmt::Display) -> RepositoryError {
    RepositoryError::InvalidOperation {
        message: message.to_string(),
    }
}
