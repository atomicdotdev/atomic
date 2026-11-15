//! Explicit external/local signing of a present-day review. No trust grants,
//! session repair, implicit ownership claims, or automatic promotion.
use crate::{
    commands::{find_repository_root, Command},
    error::{CliError, CliResult},
};
use atomic_canonical::publication_review::PublicationReview;
use atomic_core::types::{Base32, Hash};
use atomic_identity::IdentityStore;
use atomic_repository::{
    repository::provenance_gate::{local_did_key_provider, PublicationGateConfig},
    Repository,
};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

#[derive(Debug, Parser)]
pub struct PublicationReviewCommand {
    #[command(subcommand)]
    action: Action,
}

#[derive(Debug, Subcommand)]
enum Action {
    /// Print the pinned review document and exact bytes for external signing.
    Prepare {
        #[arg(long)]
        review: String,
        /// The source view whose state and inherited perspective were reviewed.
        #[arg(long)]
        view: Option<String>,
        /// Full canonical change hashes; the dependency closure is pinned too.
        #[arg(long = "change", required = true)]
        changes: Vec<String>,
        /// Retained artifact hashes explicitly renewed by this new signature.
        #[arg(long)]
        supersedes: Vec<String>,
    },
    /// Sign a prepared document with an explicitly selected local identity.
    Sign {
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        identity: String,
    },
    /// Verify and store an externally signed document; never signs as its author.
    Record {
        #[arg(long)]
        file: PathBuf,
    },
    /// Revalidate retained reviews against current evidence and trust policy.
    Inspect,
}

fn invalid(error: impl std::fmt::Display) -> CliError {
    CliError::InvalidArgument {
        message: error.to_string(),
    }
}

pub(crate) fn read(path: &Path) -> CliResult<PublicationReview> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take((atomic_canonical::publication_review::MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > atomic_canonical::publication_review::MAX_BYTES {
        return Err(invalid("review exceeds 1 MiB"));
    }
    let value = atomic_canonical::jcs::admit_document(&bytes).map_err(invalid)?;
    let document = value.get("document").unwrap_or(&value);
    PublicationReview::parse(&serde_json::to_vec(document).map_err(invalid)?).map_err(invalid)
}

impl Command for PublicationReviewCommand {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        let repo = Repository::open(&root).map_err(CliError::Repository)?;
        let keys = local_did_key_provider(&repo);
        let config = PublicationGateConfig::from_repo(&repo).map_err(CliError::Repository)?;
        match &self.action {
            Action::Prepare {
                review,
                view,
                changes,
                supersedes,
            } => {
                let changes = changes
                    .iter()
                    .map(|value| {
                        Hash::from_base32(value.as_bytes())
                            .filter(|hash| hash.to_base32() == *value)
                            .ok_or_else(|| invalid("--change requires a full canonical hash"))
                    })
                    .collect::<CliResult<Vec<_>>>()?;
                let mut review = repo
                    .prepare_publication_review_on_view(
                        review,
                        &changes,
                        view.as_deref().unwrap_or(repo.current_view()),
                        &keys,
                    )
                    .map_err(CliError::Repository)?;
                review.supersedes = supersedes.clone();
                let key = atomic_identity::keypair::PublicKey::from_base32(
                    &review.subject.review.public_key,
                )
                .map_err(invalid)?;
                let prepared = atomic_canonical::proof::prepare_attestation(review.value(), &key);
                println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                    "document":prepared.value,"signingBytes":data_encoding::BASE64.encode(&prepared.signing_bytes),
                    "requiredVerification":{"observedAtMerkle":review.subject.view_chain[0].state,
                        "ref":format!("urn:atomic:view-set:{}",review.subject.effective_set_id)}
                })).map_err(invalid)?);
            }
            Action::Sign { file, identity } => {
                let review = read(file)?;
                let store = IdentityStore::open_default().map_err(invalid)?;
                let identity = store.load_by_name(identity).map_err(invalid)?;
                if atomic_canonical::did::did_for_public_key(&identity.public_key)
                    != review.attributed_to
                {
                    return Err(invalid("selected signer is not the prepared review author"));
                }
                if review.proof.is_some() {
                    return Err(invalid(
                        "prepared document already has a proof; use record or prepare again",
                    ));
                }
                let pair = store.load_keypair(&identity.id, None).map_err(invalid)?;
                let signed =
                    atomic_canonical::proof::attest_value(review.value(), &identity, &pair);
                let signed: PublicationReview = serde_json::from_value(signed).map_err(invalid)?;
                let path = repo
                    .record_publication_review(&signed, &config, &keys)
                    .map_err(CliError::Repository)?;
                println!(
                    "{}",
                    serde_json::json!({"path":path,"review":signed.artifact_hash(),"meaning":"present-day retrospective review; not original capture"})
                );
            }
            Action::Record { file } => {
                let review = read(file)?;
                let path = repo
                    .record_publication_review(&review, &config, &keys)
                    .map_err(CliError::Repository)?;
                println!(
                    "{}",
                    serde_json::json!({"path":path,"review":review.artifact_hash()})
                );
            }
            Action::Inspect => {
                let mut results = Vec::new();
                for review in repo.publication_reviews().map_err(CliError::Repository)? {
                    let validation = match repo.validate_publication_review(&review, &config, &keys)
                    {
                        Ok(report) => serde_json::to_value(report).map_err(invalid)?,
                        Err(error) => {
                            serde_json::json!({"conforms":false,"error":error.to_string()})
                        }
                    };
                    results.push(serde_json::json!({"review":review.artifact_hash(),"subject":review.subject,"document":review,"validation":validation}));
                }
                println!(
                    "{}",
                    serde_json::to_string_pretty(&results).map_err(invalid)?
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn publication_review_cli_requires_explicit_subject_and_signer() {
        assert!(PublicationReviewCommand::try_parse_from([
            "publication-review",
            "prepare",
            "--review",
            "work"
        ])
        .is_err());
        assert!(PublicationReviewCommand::try_parse_from([
            "publication-review",
            "sign",
            "--file",
            "prepared.json"
        ])
        .is_err());
        assert!(PublicationReviewCommand::try_parse_from([
            "publication-review",
            "prepare",
            "--review",
            "work",
            "--change",
            "full-hash",
            "--view",
            "feature",
            "--supersedes",
            "old-artifact"
        ])
        .is_ok());
        assert!(
            PublicationReviewCommand::try_parse_from(["publication-review", "inspect"]).is_ok()
        );
        assert!(PublicationReviewCommand::try_parse_from([
            "publication-review",
            "graph",
            "--file",
            "prepared.json"
        ])
        .is_err());
    }
    #[test]
    fn publication_review_file_admission_rejects_ambiguous_or_oversized_json() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("review.json");
        std::fs::write(&file, br#"{"schema":"x","schema":"y"}"#).unwrap();
        assert!(read(&file).is_err());
        std::fs::write(
            &file,
            vec![b' '; atomic_canonical::publication_review::MAX_BYTES + 1],
        )
        .unwrap();
        assert!(read(&file).is_err());
    }
}
