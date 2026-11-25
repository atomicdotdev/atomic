//! Cleanup only for retained jobs created by the former agent publisher.
//! No code here can resume an agent, execute its runner, or create credentials.
use crate::error::{CliError, CliResult};
use atomic_identity::IdentityStore;
use atomic_repository::Repository;
use fs2::FileExt;
use serde_json::{json, Value};
use std::{fs, fs::OpenOptions, io::Write};

fn invalid(message: impl std::fmt::Display) -> CliError {
    CliError::InvalidArgument {
        message: message.to_string(),
    }
}
fn text<'a>(value: &'a Value, key: &str) -> CliResult<&'a str> {
    value[key]
        .as_str()
        .ok_or_else(|| invalid(format!("Retained job has no {key}")))
}

pub(super) fn cancel(repo: &Repository, id: &str, json_output: bool) -> CliResult<()> {
    let id = uuid::Uuid::parse_str(id).map_err(invalid)?.to_string();
    let dir = repo.dot_dir().join("review-jobs").join(&id);
    if !fs::symlink_metadata(&dir)?.file_type().is_dir() {
        return Err(invalid("Legacy job path is not a regular directory"));
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("job.lock"))?;
    lock.try_lock_exclusive()
        .map_err(|_| invalid("Legacy publication job is owned by another process; retry later"))?;
    let mut job =
        atomic_canonical::jcs::admit_document(&fs::read(dir.join("job.json"))?).map_err(invalid)?;
    if text(&job, "id")? != id || text(&job, "schema")? != "atomic.review-job.v1" {
        return Err(invalid("Legacy job identity/schema mismatch"));
    }
    let store = IdentityStore::open_default_existing().map_err(invalid)?;
    let issuer = store.load_by_name(text(&job, "issuer")?).map_err(invalid)?;
    let owner = atomic_canonical::jcs::admit_document(&fs::read(dir.join("owner.json"))?)
        .map_err(invalid)?;
    atomic_canonical::proof::verify_value(&owner, &issuer.public_key).map_err(invalid)?;
    if owner["schema"] != "atomic.review-job-ownership.v1" || owner["job"] != job["id"] {
        return Err(invalid("Legacy ownership record is not for this job"));
    }
    for key in [
        "scope",
        "issuer",
        "reviewer",
        "reviewer_did",
        "delegation",
        "expires_at",
        "workdir",
        "runner",
    ] {
        if owner[key] != job[key] {
            return Err(invalid(
                "Mutable legacy job differs from its signed ownership record",
            ));
        }
    }
    if job["scope"]["project"] != repo.project_id().map_err(CliError::Repository)?.to_urn()
        || text(&job, "reviewer")? != format!("review-{id}")
    {
        return Err(invalid("Legacy job belongs to another project or identity"));
    }
    let reviewer = store
        .load_by_name(text(&job, "reviewer")?)
        .map_err(invalid)?;
    if !reviewer.identity_type.is_agent()
        || reviewer.delegated_by != Some(issuer.id)
        || atomic_canonical::did::did_for_public_key(&reviewer.public_key)
            != text(&job, "reviewer_did")?
    {
        return Err(invalid(
            "Refusing to retire an identity not owned by this issuer's legacy job",
        ));
    }
    if job["credentials_retired"] != true {
        let pair = store.load_keypair(&issuer.id, None).map_err(invalid)?;
        let delegation =
            atomic_identity::delegation::DelegationId::from_base32(text(&job, "delegation")?)
                .ok_or_else(|| invalid("Invalid legacy delegation ID"))?;
        let revoke = atomic_canonical::delegation::mint_revocation(
            &issuer,
            &pair,
            &delegation,
            Some("Explicit cleanup of a former agent-based review job"),
        );
        store
            .save_revocation(
                text(&job, "delegation")?,
                &serde_json::to_string_pretty(&revoke).map_err(invalid)?,
            )
            .map_err(invalid)?;
        store.retire_secret_key(&reviewer.id).map_err(invalid)?;
        job["credentials_retired"] = json!(true);
    }
    if job["phase"] != "published" {
        job["phase"] = json!("cancelled");
    }
    let mut temp = tempfile::NamedTempFile::new_in(&dir)?;
    temp.write_all(&serde_json::to_vec_pretty(&job).map_err(invalid)?)?;
    temp.as_file().sync_all()?;
    temp.persist(dir.join("job.json")).map_err(invalid)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&job).map_err(invalid)?);
    } else {
        println!("Legacy review job {id} cancelled/retired. No agent was resumed and no new credentials were created.");
    }
    Ok(())
}
