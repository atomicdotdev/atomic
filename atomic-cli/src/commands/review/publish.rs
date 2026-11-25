//! An owned, bounded independent publisher. No view is created or switched.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::{Command as Process, Stdio};
use std::time::{Duration, Instant};

use atomic_canonical::publication_review::PublicationReview;
use atomic_core::types::Base32;
use atomic_identity::delegation::{Delegation, DelegationPermission, DelegationScope};
use atomic_identity::{Identity, IdentityStore, IdentityType, IdentityUsage, KeyPair};
use atomic_repository::{
    intent_bridge, PublicationGateConfig, Repository, ViewEntry, ViewEntryKind,
};
use clap::Args;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::scope::{self, invalid, Scope};
use crate::commands::triage::publication_review::{Action as Advanced, PublicationReviewCommand};
use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};

#[derive(Debug, Args)]
#[command(args_conflicts_with_subcommands = true)]
pub(super) struct PublishArgs {
    /// One literal view or a comma-separated collection of views.
    #[arg(long, conflicts_with_all = ["session", "resume", "cancel"])]
    view: Option<String>,
    /// Publish this session's recorded changes, not its inherited view history.
    #[arg(long, conflicts_with_all = ["view", "resume", "cancel"])]
    session: Option<String>,
    /// Resume a retained publication job without widening its original scope.
    #[arg(long, conflicts_with = "cancel")]
    resume: Option<String>,
    /// Cancel a retained job and retire its temporary recording credentials.
    #[arg(long)]
    cancel: Option<String>,
    /// Trusted local identity issuing this exact review-publication authorization.
    #[arg(long)]
    identity: Option<String>,
    /// OpenCode executable. An executable path, not a shell command.
    #[arg(long, default_value = "opencode")]
    runner: PathBuf,
    /// Maximum publisher process lifetime; no automatic renewal.
    #[arg(long, default_value_t = 1800, value_parser = clap::value_parser!(u64).range(1..=3300))]
    timeout_seconds: u64,
    /// Show the frozen scope without credentials, agents, or repository writes.
    #[arg(long, conflicts_with_all = ["resume", "cancel"])]
    dry_run: bool,
    /// Approve the displayed local, purpose-bound ephemeral review operation.
    #[arg(long)]
    yes: bool,
    #[arg(long)]
    json: bool,
    /// Advanced external/local signing primitives; legacy spellings still work.
    #[command(subcommand)]
    advanced: Option<Advanced>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Job {
    schema: String,
    id: String,
    scope: Scope,
    phase: String,
    issuer: String,
    reviewer: String,
    reviewer_did: String,
    delegation: String,
    expires_at: String,
    workdir: PathBuf,
    runner: PathBuf,
    run_id: Option<String>,
    publisher_session: Option<String>,
    publisher_manifest: Option<String>,
    #[serde(default)]
    publisher_attestations: Vec<String>,
    review: Option<String>,
    artifacts: Vec<String>,
    error: Option<String>,
    credentials_retired: bool,
    #[serde(default)]
    inputs_ready: bool,
}

fn bad(error: impl std::fmt::Display) -> CliError {
    invalid(error.to_string())
}

pub(super) fn run(args: &PublishArgs) -> CliResult<()> {
    if let Some(action) = &args.advanced {
        return PublicationReviewCommand {
            action: action.clone(),
        }
        .run();
    }
    let root = find_repository_root()?;
    let repo = if args.dry_run {
        Repository::open_readonly(&root)
    } else {
        Repository::open(&root)
    }
    .map_err(CliError::Repository)?;
    let id = args.resume.as_ref().or(args.cancel.as_ref());
    if let Some(id) = id {
        let id =
            uuid::Uuid::parse_str(id).map_err(|_| invalid("publication job ID must be a UUID"))?;
        let dir = repo.dot_dir().join("review-jobs").join(id.to_string());
        let _lock = lock_job(&dir)?;
        let mut job: Job = serde_json::from_slice(&fs::read(dir.join("job.json"))?).map_err(bad)?;
        if job.id != id.to_string() || job.schema != "atomic.review-job.v1" {
            return Err(invalid("publication job identity/schema mismatch"));
        }
        verify_job_owner(&repo, &dir, &job)?;
        if args.cancel.is_some() {
            // Cleanup of an owned recording credential is not conditional on
            // continued validity of a previously retained review approval.
            retire_credentials(&mut job)?;
            if job.phase != "published" {
                job.phase = "cancelled".into();
            }
            save_job(&dir, &job)?;
            return output(args, &job);
        }
        if job.phase != "published" && dir.join("published.json").exists() {
            // Recover the crash window after immutable receipt persistence but
            // before the mutable job checkpoint, without signing another copy.
            verify_completed_job(&repo, &dir, &job)?;
            job.phase = "published".into();
            save_job(&dir, &job)?;
        }
        if job.phase == "published" {
            if !job.credentials_retired {
                retire_credentials(&mut job)?;
                save_job(&dir, &job)?;
            }
            verify_completed_job(&repo, &dir, &job)?;
            return output(args, &job);
        }
        if job.phase == "cancelled" {
            // Never re-execute the independent agent for a completed receipt.
            return output(args, &job);
        }
        if chrono::DateTime::parse_from_rfc3339(&job.expires_at).map_err(bad)? <= chrono::Utc::now()
        {
            return Err(invalid(format!("job authority expired; cancel job {} to retire its credentials; no renewal was performed",job.id)));
        }
        return execute_job(args, &root, &dir, &mut job);
    }
    let scope = scope::resolve(&repo, args.view.as_deref(), args.session.as_deref())?;
    if args.dry_run {
        let plan = json!({"status":"plan","operation":"review-publication","scope":scope,
            "execution":"fresh OpenCode session and process-local ephemeral reviewer",
            "insertion":false,"push":false,"production_enrollment":false});
        println!("{}", serde_json::to_string_pretty(&plan).map_err(bad)?);
        return Ok(());
    }
    let runner = executable(&args.runner)?;
    let store = IdentityStore::open_default_existing().map_err(bad)?;
    let issuer = match &args.identity {
        Some(name) => store.load_by_name(name).map_err(bad)?,
        None => store
            .get_default()
            .map_err(bad)?
            .ok_or_else(|| invalid("no local publication issuer selected; pass --identity"))?,
    };
    let config = PublicationGateConfig::from_repo(&repo).map_err(CliError::Repository)?;
    if issuer.identity_type.is_agent() || issuer.identity_type.is_delegated() {
        return Err(invalid("an ephemeral review must be authorized by a trusted human issuer, not a delegation chain"));
    }
    let issuer_did = atomic_canonical::did::did_for_public_key(&issuer.public_key);
    if config
        .trust
        .evaluate(&issuer_did, config.repository_identity.as_deref())
        != atomic_config::SignerTrust::Trusted
    {
        return Err(invalid(format!("publication issuer {issuer_did} is not trusted by this repository; no agent or credential was created")));
    }
    // Check the signing key BEFORE asking an independent reviewer to do work.
    let pair = store.load_keypair(&issuer.id, None).map_err(bad)?;
    if scope.intent_sources.is_empty() {
        return Err(invalid("no source-backed work intent was resolved for this scope; supply genuine intent associations before publication"));
    }
    if !args.yes && !dialoguer::Confirm::new()
        .with_prompt(format!("Authorize a local ephemeral publisher for {} view(s), {} recorded changes, at most {} seconds? No insertion, push or production enrollment",scope.views.len(),scope.closure.len(),args.timeout_seconds))
        .default(false).interact().map_err(bad)?
    { return Err(invalid("publication cancelled before credentials or agent creation")); }
    let id = uuid::Uuid::new_v4().to_string();
    let dir = repo.dot_dir().join("review-jobs").join(&id);
    fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    }
    let _lock = lock_job(&dir)?;
    let temp = tempfile::Builder::new()
        .prefix("atomic-review-")
        .tempdir()?
        .keep();
    let reviewer_pair = KeyPair::generate();
    let reviewer = Identity::builder(format!("review-{id}"))
        .identity_type(IdentityType::Agent)
        .usage(IdentityUsage::Bot)
        .public_key(reviewer_pair.public.clone())
        .delegated_by(issuer.id)
        .build()
        .map_err(bad)?;
    let expiry =
        chrono::Utc::now() + chrono::Duration::seconds((args.timeout_seconds + 300) as i64);
    let mut builder = DelegationScope::builder()
        .permissions(vec![
            DelegationPermission::Read,
            DelegationPermission::Record,
        ])
        .project(
            scope
                .project
                .strip_prefix("urn:atomic:project:")
                .unwrap_or(&scope.project),
        )
        .server(format!(
            "atomic-local://{}",
            scope
                .project
                .strip_prefix("urn:atomic:project:")
                .unwrap_or(&scope.project)
        ))
        .max_changes(3);
    for pin in &scope.views {
        builder = builder.view(&pin.name);
    }
    let terms = Delegation::new(&issuer, &reviewer, builder.build()).expires_in(
        chrono::Duration::seconds((args.timeout_seconds + 300) as i64),
    );
    let certificate = atomic_canonical::delegation::mint(&issuer, &pair, &terms);
    let document = atomic_canonical::delegation::encode_for_storage(&certificate).map_err(bad)?;
    let mut job = Job {
        schema: "atomic.review-job.v1".into(),
        id,
        scope,
        phase: "prepared".into(),
        issuer: issuer.name.clone(),
        reviewer: reviewer.name.clone(),
        reviewer_did: atomic_canonical::did::did_for_public_key(&reviewer.public_key),
        delegation: terms.id.to_base32(),
        expires_at: expiry.to_rfc3339(),
        workdir: temp,
        runner,
        run_id: None,
        publisher_session: None,
        publisher_manifest: None,
        publisher_attestations: vec![],
        review: None,
        artifacts: vec![],
        error: None,
        credentials_retired: false,
        inputs_ready: false,
    };
    // Persist recovery identifiers before effects. There are never shared default
    // signer/profile mutations, remote enrollment, or credential copies.
    save_job(&dir, &job)?;
    let ownership = atomic_canonical::proof::attest_value(job_ownership(&job), &issuer, &pair);
    fs::write(
        dir.join("owner.json"),
        serde_json::to_vec_pretty(&ownership).map_err(bad)?,
    )?;
    store
        .save_with_keypair(&reviewer, &reviewer_pair, None)
        .map_err(bad)?;
    store
        .save_delegation(&job.delegation, &document)
        .map_err(bad)?;
    drop(reviewer_pair);
    drop(pair);
    drop(store);
    drop(repo);
    execute_job(args, &root, &dir, &mut job)
}

fn execute_job(args: &PublishArgs, root: &Path, dir: &Path, job: &mut Job) -> CliResult<()> {
    let result = execute_inner(args, root, dir, job);
    match result {
        Ok(()) => {
            job.phase = "published".into();
            job.error = None;
            // The retained purpose-bound approval remains valid historically.
            // Revoke the recording credential, NOT the publication approval.
            let cleanup = retire_credentials(job);
            save_job(dir, job)?;
            output(args, job)?;
            cleanup
        }
        Err(error) => {
            job.phase = "blocked".into();
            job.error = Some(error.to_string());
            save_job(dir, job)?;
            eprintln!("Review job {} retained at {}. Resume with `atomic review publish --resume {}` or cancel with `--cancel {}`; credentials were not prematurely revoked.",job.id,dir.display(),job.id,job.id);
            Err(error)
        }
    }
}

fn execute_inner(args: &PublishArgs, root: &Path, dir: &Path, job: &mut Job) -> CliResult<()> {
    let repo = Repository::open(root).map_err(CliError::Repository)?;
    verify_job_owner(&repo, dir, job)?;
    let workspace = job.workdir.join("reviewer");
    if job.publisher_session.is_none() {
        if job.run_id.is_some() && dir.join("agent.jsonl").exists() {
            // A missing durable exit receipt must not be repaired by guessing a
            // prior process completed or blindly starting a duplicate reviewer.
            return Err(invalid("previous publisher execution has no verified completion receipt; inspect the retained run or cancel this job before starting another"));
        }
        scope::recheck(&repo, &job.scope, None, None)?;
        if !workspace.exists() {
            repo.provision_recorded_sandbox(&workspace, &job.scope.views[0].name)
                .map_err(CliError::Repository)?;
        }
        // Directory existence or a mutable job-state flag is not evidence that
        // compatible, complete collection inputs were prepared.
        prepare_inputs(&repo, dir, job)?;
        job.inputs_ready = true;
        save_job(dir, job)?;
        let brief = dir.join("reviewer.md");
        fs::write(&brief, brief_text(job, dir)?)?;
        let scope_bytes = serde_json::to_vec_pretty(&job.scope).map_err(bad)?;
        fs::write(dir.join("scope.json"), scope_bytes)?;
        let begin = atomic_output(
            &workspace,
            &[
                "agent",
                "lifecycle",
                "begin",
                "--owner",
                "atomic-review-publish",
                "--session",
                &job.id,
                "--executor",
                "opencode",
                "--view",
                &job.scope.views[0].name,
                "--workdir",
                workspace
                    .to_str()
                    .ok_or_else(|| invalid("non-UTF8 reviewer workdir"))?,
                "--ttl-seconds",
                &(args.timeout_seconds + 300).to_string(),
                "--json",
            ],
        )?;
        let value: Value = serde_json::from_slice(&begin).map_err(bad)?;
        job.run_id = Some(
            value
                .get("run_id")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("lifecycle begin returned no run_id"))?
                .into(),
        );
        job.phase = "running".into();
        save_job(dir, job)?;
        let log = dir.join("agent.jsonl");
        let mut process = Process::new(&job.runner);
        process.current_dir(&workspace).env("ATOMIC_AGENT_IDENTITY",&job.reviewer)
            .args(["run","--standalone","--format","json","--title"])
            .arg(format!("Atomic publication {}",job.id)).arg("--file").arg(&brief)
            .arg("Perform the independently authorized review in the attached bounded CLI handoff. Do not invoke review publish recursively. Save prepared unsigned publication documents, and finish your turn for automatic recording.")
            .stdin(Stdio::null()).stdout(File::create(&log)?).stderr(File::create(dir.join("agent.stderr"))?);
        let status = run_bounded(&mut process, Duration::from_secs(args.timeout_seconds))?;
        if !status.success() {
            return Err(invalid(format!(
                "publisher process failed ({status}); see {}",
                dir.join("agent.stderr").display()
            )));
        }
        job.publisher_session = Some(read_session_id(&log)?);
        save_job(dir, job)?;
    }
    let session = job
        .publisher_session
        .as_deref()
        .ok_or_else(|| invalid("missing publisher session"))?;
    let review_id = find_review(&repo, &job.reviewer_did)?;
    // Tests may create build products, but may not change the source bytes or
    // modes in the compatible collection tree and attest those repaired bytes
    // as though the original frozen implementation passed.
    recheck_input_sources(&repo, job)?;
    let inputs = intent_bridge::read_intent(&repo, &review_id).map_err(CliError::Repository)?;
    let node = intent_bridge::lift(&inputs).map_err(CliError::Repository)?;
    let uid = review_id
        .strip_prefix("urn:atomic:intent:")
        .unwrap_or(&review_id);
    scope::recheck(&repo, &job.scope, Some(uid), Some(session))?;
    if node.status != "done"
        || node.kind != "review"
        || node.has_acceptance_criterion.is_empty()
        || node
            .has_acceptance_criterion
            .iter()
            .any(|ac| ac.ac_status != "met")
    {
        return Err(invalid(
            "independent review has unresolved findings or is not complete; publication refused",
        ));
    }
    for work in job.scope.intent_sources.keys() {
        if !node
            .depends_on
            .iter()
            .any(|r| r.edge == "reviews" && &r.to == work)
        {
            return Err(invalid(format!(
                "independent review does not cover work intent {work}"
            )));
        }
    }
    job.review = Some(review_id.clone());
    let run = job
        .run_id
        .as_deref()
        .ok_or_else(|| invalid("missing owned publisher run"))?;
    let completion = crate::commands::agent::finish_publisher_session(
        &workspace,
        session,
        run,
        &job.id,
        &job.reviewer,
        &job.scope.views[0].name,
    )?;
    job.publisher_manifest = Some(completion.manifest.to_base32());
    job.publisher_attestations = completion
        .attestations
        .iter()
        .map(Base32::to_base32)
        .collect();
    job.phase = "session-ended".into();
    save_job(dir, job)?;
    let _ = atomic_output(
        &workspace,
        &["agent", "lifecycle", "end", "--run-id", run, "--json"],
    )?;
    let _publication_boundary = repo
        .lock_review_publication()
        .map_err(CliError::Repository)?;
    scope::recheck(&repo, &job.scope, Some(uid), Some(session))?;
    // Final signatures are added only after authentic native session_end. The
    // independently prepared documents bind pre-output snapshots, avoiding a
    // recursive requirement to review a publication's own generated receipt.
    let store = IdentityStore::open_default_existing().map_err(bad)?;
    let issuer = store.load_by_name(&job.issuer).map_err(bad)?;
    let issuer_pair = store.load_keypair(&issuer.id, None).map_err(bad)?;
    let reviewer = store.load_by_name(&job.reviewer).map_err(bad)?;
    let reviewer_pair = store.load_keypair(&reviewer.id, None).map_err(bad)?;
    let config = PublicationGateConfig::from_repo(&repo).map_err(CliError::Repository)?;
    let keys = atomic_repository::repository::provenance_gate::local_did_key_provider(&repo);
    for (index, pin) in job.scope.views.iter().enumerate() {
        if let Some(hash) = job.artifacts.get(index) {
            let all = repo.publication_reviews().map_err(CliError::Repository)?;
            let retained = all
                .iter()
                .find(|r| r.artifact_hash() == *hash)
                .ok_or_else(|| invalid("partial publication artifact is missing"))?;
            if retained
                .subject
                .roots
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>()
                != pin.roots.iter().cloned().collect()
                || retained.subject.review.id != review_id
                || retained.attributed_to != job.reviewer_did
                || !repo
                    .validate_publication_review(retained, &config, &keys)
                    .map_err(CliError::Repository)?
                    .conforms
            {
                return Err(invalid(
                    "partial publication no longer matches this job's verified scope",
                ));
            }
            continue;
        }
        let prepared_path = dir.join(format!("prepared-{index}.json"));
        if fs::metadata(&prepared_path)?.len()
            > atomic_canonical::publication_review::MAX_BYTES as u64
        {
            return Err(invalid(
                "prepared publication exceeds the canonical document size budget",
            ));
        }
        let prepared = fs::read(prepared_path)?;
        let value = atomic_canonical::jcs::admit_document(&prepared).map_err(bad)?;
        let value = value.get("document").unwrap_or(&value);
        let mut review =
            PublicationReview::parse(&serde_json::to_vec(value).map_err(bad)?).map_err(bad)?;
        let selected: BTreeSet<_> = review.subject.roots.iter().cloned().collect();
        if selected != pin.roots.iter().cloned().collect()
            || review.subject.review.id != review_id
            || review.attributed_to != job.reviewer_did
            || review.subject.project != job.scope.project
            || review.subject.view_chain.first().map(|p| p.name.as_str()) != Some(pin.name.as_str())
            || review.proof.is_some()
        {
            return Err(invalid("publisher prepared a different project/review/view/change scope or an already signed document"));
        }
        scope::check_prepared_context(pin, &review.subject)?;
        let pending = dir.join(format!("signed-{index}.json"));
        if pending.exists() {
            let retained = PublicationReview::parse(&fs::read(&pending)?).map_err(bad)?;
            if retained.subject != review.subject || retained.attributed_to != job.reviewer_did {
                return Err(invalid(
                    "pending signature belongs to another publication subject",
                ));
            }
            repo.record_publication_review(&retained, &config, &keys)
                .map_err(CliError::Repository)?;
            job.artifacts.push(retained.artifact_hash());
            save_job(dir, job)?;
            continue;
        }
        atomic_repository::repository::review_authorization::mint_and_store(
            &repo,
            &review.subject,
            &reviewer.public_key,
            &issuer,
            &issuer_pair,
            chrono::DateTime::parse_from_rfc3339(&job.expires_at)
                .map_err(bad)?
                .with_timezone(&chrono::Utc),
            &config,
        )
        .map_err(CliError::Repository)?;
        review.reviewed_at = chrono::Utc::now().to_rfc3339();
        if !atomic_repository::repository::review_authorization::reviewer_may_sign_now(
            &repo, &review, &config, &keys,
        )
        .map_err(CliError::Repository)?
        {
            return Err(invalid("exact-scope publication authority was not accepted; no publication signature retained"));
        }
        let signed =
            atomic_canonical::proof::attest_value(review.value(), &reviewer, &reviewer_pair);
        let signed: PublicationReview = serde_json::from_value(signed).map_err(bad)?;
        // Persist exact signed bytes BEFORE the canonical retention effect, so
        // a crash cannot turn retry into a new timestamp/new approval artifact.
        write_immutable(&pending, &serde_json::to_vec_pretty(&signed).map_err(bad)?)?;
        repo.record_publication_review(&signed, &config, &keys)
            .map_err(CliError::Repository)?;
        let artifact = signed.artifact_hash();
        if !job.artifacts.contains(&artifact) {
            job.artifacts.push(artifact);
        }
        save_job(dir, job)?;
    }
    scope::recheck(&repo, &job.scope, Some(uid), Some(session))?;
    let receipt = json!({"schema":"atomic.review-publication-receipt.v1","job":job.id,"scope":job.scope,
        "review":review_id,"publisher_session":session,"session_end_manifest":job.publisher_manifest,
        "session_end_attestations":job.publisher_attestations,"artifacts":job.artifacts,
        "attributedTo":job.reviewer_did,"meaning":"finalized independent review; not insertion or push"});
    let receipt = atomic_canonical::proof::attest_value(receipt, &reviewer, &reviewer_pair);
    write_immutable(
        &dir.join("published.json"),
        &serde_json::to_vec_pretty(&receipt).map_err(bad)?,
    )?;
    Ok(())
}

fn verify_completed_job(repo: &Repository, dir: &Path, job: &Job) -> CliResult<()> {
    let store = IdentityStore::open_default_existing().map_err(bad)?;
    let reviewer = store.load_by_name(&job.reviewer).map_err(bad)?;
    let value = atomic_canonical::jcs::admit_document(&fs::read(dir.join("published.json"))?)
        .map_err(bad)?;
    atomic_canonical::proof::verify_value(&value, &reviewer.public_key).map_err(bad)?;
    if value.get("job") != Some(&json!(job.id))
        || value.get("scope") != Some(&json!(job.scope))
        || value.get("artifacts") != Some(&json!(job.artifacts))
        || value.get("session_end_manifest") != Some(&json!(job.publisher_manifest))
        || value.get("session_end_attestations") != Some(&json!(job.publisher_attestations))
        || value.get("publisher_session") != Some(&json!(job.publisher_session))
        || value.get("review") != Some(&json!(job.review))
        || value.get("attributedTo") != Some(&json!(job.reviewer_did))
    {
        return Err(invalid(
            "retained job differs from its signed publication receipt",
        ));
    }
    let session = job
        .publisher_session
        .as_deref()
        .ok_or_else(|| invalid("published receipt has no publisher session"))?;
    let manifest = scope::hashes(&[job
        .publisher_manifest
        .clone()
        .ok_or_else(|| invalid("missing publisher end manifest"))?])?[0];
    let manifest = repo
        .get_session_manifest(&manifest)
        .map_err(CliError::Repository)?
        .ok_or_else(|| invalid("retained publisher manifest is unavailable"))?;
    if manifest.session_id != session || job.publisher_attestations.is_empty() {
        return Err(invalid(
            "publisher completion evidence does not match the signed receipt",
        ));
    }
    let mut covered = BTreeSet::new();
    for hash in scope::hashes(&job.publisher_attestations)? {
        let attest = repo.load_attestation(&hash).map_err(CliError::Repository)?;
        if attest.session_id != session
            || attest.signer_did() != Some(job.reviewer_did.as_str())
            || !attest.verify_with_did(reviewer.public_key.as_bytes())
        {
            return Err(invalid(
                "retained publisher attestation signature/binding failed",
            ));
        }
        covered.extend(attest.changes_covered);
    }
    if manifest
        .turns
        .iter()
        .flat_map(|t| &t.change_hashes)
        .any(|h| !covered.contains(h))
    {
        return Err(invalid(
            "retained publisher attestation does not cover its recorded review turns",
        ));
    }
    for change in manifest.turns.iter().flat_map(|turn| &turn.change_hashes) {
        for (_, attestation) in repo
            .find_attestations_for_change(change)
            .map_err(CliError::Repository)?
        {
            if attestation.session_id == session
                && (attestation.signer_did() != Some(job.reviewer_did.as_str())
                    || !attestation.verify_with_did(reviewer.public_key.as_bytes()))
            {
                return Err(invalid("invalid later same-session evidence cannot be hidden by a previously published receipt"));
            }
        }
    }
    let config = PublicationGateConfig::from_repo(repo).map_err(CliError::Repository)?;
    let keys = atomic_repository::repository::provenance_gate::local_did_key_provider(repo);
    let all = repo.publication_reviews().map_err(CliError::Repository)?;
    if job.artifacts.len() != job.scope.views.len() || job.artifacts.is_empty() {
        return Err(invalid(
            "published receipt lacks exact per-view publication coverage",
        ));
    }
    for (index, hash) in job.artifacts.iter().enumerate() {
        let artifact = all
            .iter()
            .find(|r| r.artifact_hash() == *hash)
            .ok_or_else(|| invalid("retained publication artifact is missing"))?;
        if artifact
            .subject
            .roots
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            != job.scope.views[index].roots.iter().cloned().collect()
            || artifact.subject.review.id != job.review.as_deref().unwrap_or("")
            || artifact.attributed_to != job.reviewer_did
        {
            return Err(invalid(
                "retained artifact does not cover this job's exact selected view roots",
            ));
        }
        let report = repo
            .validate_publication_review(artifact, &config, &keys)
            .map_err(CliError::Repository)?;
        if !report.conforms {
            return Err(invalid(
                "retained publication no longer conforms to current authority/evidence policy",
            ));
        }
    }
    Ok(())
}

fn job_ownership(job: &Job) -> Value {
    json!({"schema":"atomic.review-job-ownership.v1","job":job.id,"scope":job.scope,
        "issuer":job.issuer,"reviewer":job.reviewer,"reviewer_did":job.reviewer_did,
        "delegation":job.delegation,"expires_at":job.expires_at,"workdir":job.workdir,"runner":job.runner})
}

fn verify_job_owner(repo: &Repository, dir: &Path, job: &Job) -> CliResult<()> {
    let store = IdentityStore::open_default_existing().map_err(bad)?;
    let issuer = store.load_by_name(&job.issuer).map_err(bad)?;
    let document =
        atomic_canonical::jcs::admit_document(&fs::read(dir.join("owner.json"))?).map_err(bad)?;
    atomic_canonical::proof::verify_value(&document, &issuer.public_key).map_err(bad)?;
    for (key, value) in job_ownership(job)
        .as_object()
        .ok_or_else(|| invalid("invalid job ownership scope"))?
    {
        if document.get(key) != Some(value) {
            return Err(invalid(
                "mutable job scope differs from its issuer-signed ownership record",
            ));
        }
    }
    if repo.project_id().map_err(CliError::Repository)?.to_urn() != job.scope.project {
        return Err(invalid("publication job belongs to a different project"));
    }
    if job.reviewer != format!("review-{}", job.id) {
        return Err(invalid(
            "publication job does not own this ephemeral identity name",
        ));
    }
    let reviewer = store.load_by_name(&job.reviewer).map_err(bad)?;
    if !reviewer.identity_type.is_agent()
        || reviewer.delegated_by != Some(issuer.id)
        || atomic_canonical::did::did_for_public_key(&reviewer.public_key) != job.reviewer_did
    {
        return Err(invalid(
            "publication recovery refuses credentials not created for this issuer-owned job",
        ));
    }
    Ok(())
}

fn brief_text(job: &Job, dir: &Path) -> CliResult<String> {
    Ok(format!("# Independent publisher job {}\n\nWork only in your current recorded-only working copy on existing view {}. Verify native project {}, same-view lifecycle run, and process identity {} before edits. Do not create/switch views, grant credentials, change defaults, push, insert, repair implementation or run atomic add/record. This is independent substantive inspection, not adoption of author conclusions.\n\nFrozen scope and work-intent source hashes:\n```json\n{}\n```\n\nRead the implementation, provenance and each listed work intent. Run relevant tests. For multiple views inspect per-view inputs and the combined compatible tree in {} and test interactions; do not approve separate views as an untested merge. Scope associations are context, not inferred governing ownership.\n\nReuse an appropriate group kind:review intent under your own identity, or create ONE using atomic intent new --review and explicit reviews links to every listed work intent. Record actual criterion judgments and exact scoped passing verification only when justified. Do not invent original capture evidence. Sign explicitly with --identity {}. Leave work intent definitions and lifecycle untouched. Finish your existing group review as done only if all its criteria genuinely pass; blockers must remain visible and must not yield a prepared passing publication.\n\nBefore ending this turn, for EACH views[index], run atomic triage publication-review prepare --review <your review> --view <name> with that pin's exact roots as repeated --change. Read returned requiredVerification. Add genuine scope-check records with exact ref and observedAtMerkle to your SAME review (preserving actual test observation pins), sync/re-attest the final review revision, and re-prepare each envelope after final review edits. Save final UNSIGNED prepared envelopes to {}/prepared-<index>.json. Do not invoke sign/record or review publish: coordinator finalizes your real session_end and only then signs eligible prepared subjects. No new views and no recursive reviews of reviews.\n\nLet normal hooks automatically record your final review turn. Do not manually call session-end or manufacture completion. Return your review ID, findings and results.\n",job.id,job.scope.views[0].name,job.scope.project,job.reviewer,
        serde_json::to_string_pretty(&job.scope).map_err(bad)?,job.workdir.join("inputs").display(),job.reviewer,dir.display()))
        .map(|mut brief| {
            brief.push_str("\n\n");
            brief.push_str(include_str!("../../../skills/review/references/review-contract.md"));
            brief
        })
}

fn find_review(repo: &Repository, did: &str) -> CliResult<String> {
    let mut found = Vec::new();
    for info in repo.vault_intent_list(None).map_err(CliError::Repository)? {
        let inputs = intent_bridge::read_intent(repo, &info.id).map_err(CliError::Repository)?;
        let node = intent_bridge::lift(&inputs).map_err(CliError::Repository)?;
        if node.kind == "review" && node.attributed_to.as_deref() == Some(did) {
            found.push(node.id);
        }
        // Lifted source does not carry its proof: consult the signed revision.
        else if node.kind == "review" {
            if let intent_bridge::Attestation::Fresh(signed) =
                intent_bridge::load_attestation(repo, &info.id, &inputs)
                    .map_err(CliError::Repository)?
            {
                if signed.attributed_to.as_deref() == Some(did) {
                    found.push(signed.id);
                }
            }
        }
    }
    if found.len() != 1 {
        return Err(invalid("publisher must produce exactly one attributable group review; missing or ambiguous review evidence"));
    }
    Ok(found.remove(0))
}

fn prepare_inputs(repo: &Repository, _dir: &Path, job: &Job) -> CliResult<()> {
    if job.scope.views.len() < 2 {
        return Ok(());
    }
    let mut trees = Vec::new();
    let mut common: Option<BTreeMap<String, ViewEntry>> = None;
    // Use the shared ancestor as a real three-way baseline. If none exists,
    // never pretend concatenating unrelated file trees is an integration.
    let ancestry = |view: &str| -> CliResult<Vec<String>> {
        let mut names = Vec::new();
        let mut next = Some(view.to_string());
        while let Some(name) = next {
            if names.contains(&name) || names.len() > 256 {
                return Err(invalid("cyclic or excessive view ancestry"));
            }
            next = repo
                .get_view_info(&name)
                .map_err(CliError::Repository)?
                .parent_name;
            names.push(name);
        }
        Ok(names)
    };
    let chains = job
        .scope
        .views
        .iter()
        .map(|p| ancestry(&p.name))
        .collect::<CliResult<Vec<_>>>()?;
    let ancestor = chains[0]
        .iter()
        .find(|name| chains.iter().all(|c| c.contains(name)))
        .ok_or_else(|| invalid("view collection has no common recorded ancestor"))?;
    for name in
        std::iter::once(ancestor.as_str()).chain(job.scope.views.iter().map(|p| p.name.as_str()))
    {
        let mut tree = BTreeMap::new();
        repo.materialize_view_entries(name, |entry| {
            tree.insert(entry.path.clone(), entry);
            Ok::<_, CliError>(())
        })
        .map_err(CliError::Repository)??;
        if common.is_none() {
            common = Some(tree);
        } else {
            trees.push(tree);
        }
    }
    let combined = scope::compose(&common.unwrap_or_default(), &trees)?;
    let output = job.workdir.join("inputs");
    if output.exists() {
        verify_inputs_tree(&output, &combined)?;
        return stamp_input_sources(repo, _dir, job, &combined);
    }
    let staging = tempfile::Builder::new()
        .prefix("inputs-stage-")
        .tempdir_in(&job.workdir)?;
    for (path, entry) in &combined {
        let relative = Path::new(&path);
        if relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
            || relative.as_os_str().is_empty()
            || matches!(relative.components().next(),Some(Component::Normal(n)) if n==".atomic" || n==".git" || n==".atomic-sandbox")
        {
            return Err(invalid("unsafe path in combined recorded snapshot"));
        }
        if entry.mode & !0o777 != 0 {
            return Err(invalid(
                "collection snapshot contains unsupported special permission bits",
            ));
        }
        let dest = staging.path().join(relative);
        if entry.kind == ViewEntryKind::Directory {
            fs::create_dir_all(&dest)?;
            continue;
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&dest)?;
        file.write_all(&entry.content)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dest, fs::Permissions::from_mode(entry.mode as u32 & 0o777))?;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut directories: Vec<_> = combined
            .iter()
            .filter(|(_, e)| e.kind == ViewEntryKind::Directory)
            .collect();
        directories.sort_by_key(|(path, _)| std::cmp::Reverse(path.len()));
        for (path, entry) in directories {
            fs::set_permissions(
                staging.path().join(path),
                fs::Permissions::from_mode(entry.mode as u32),
            )?;
        }
    }
    verify_inputs_tree(staging.path(), &combined)?;
    fs::rename(staging.path(), &output)?;
    let _ = staging.keep();
    stamp_input_sources(repo, _dir, job, &combined)?;
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
struct InputSource {
    path: String,
    kind: ViewEntryKind,
    mode: u16,
    hash: String,
}

fn stamp_input_sources(
    repo: &Repository,
    dir: &Path,
    job: &Job,
    entries: &BTreeMap<String, ViewEntry>,
) -> CliResult<()> {
    scope::recheck(repo, &job.scope, None, None)?;
    let sources: Vec<_> = entries
        .values()
        .map(|e| InputSource {
            path: e.path.clone(),
            kind: e.kind,
            mode: e.mode,
            hash: atomic_core::types::Hash::of(&e.content).to_base32(),
        })
        .collect();
    let store = IdentityStore::open_default_existing().map_err(bad)?;
    let issuer = store.load_by_name(&job.issuer).map_err(bad)?;
    let key = store.load_keypair(&issuer.id, None).map_err(bad)?;
    let document = json!({"schema":"atomic.review-input-sources.v1","job":job.id,"scope":job.scope,"sources":sources});
    let signed = atomic_canonical::proof::attest_value(document, &issuer, &key);
    fs::write(
        dir.join("inputs.json"),
        serde_json::to_vec_pretty(&signed).map_err(bad)?,
    )?;
    Ok(())
}

fn recheck_input_sources(repo: &Repository, job: &Job) -> CliResult<()> {
    if job.scope.views.len() < 2 {
        return Ok(());
    }
    let store = IdentityStore::open_default_existing().map_err(bad)?;
    let issuer = store.load_by_name(&job.issuer).map_err(bad)?;
    let dir = repo.dot_dir().join("review-jobs").join(&job.id);
    let document =
        atomic_canonical::jcs::admit_document(&fs::read(dir.join("inputs.json"))?).map_err(bad)?;
    atomic_canonical::proof::verify_value(&document, &issuer.public_key).map_err(bad)?;
    if document.get("job") != Some(&json!(job.id))
        || document.get("scope") != Some(&json!(job.scope))
    {
        return Err(invalid(
            "collection source fingerprints do not belong to the issuer-approved job",
        ));
    }
    let sources: Vec<InputSource> = serde_json::from_value(
        document
            .get("sources")
            .cloned()
            .ok_or_else(|| invalid("collection source fingerprints are missing"))?,
    )
    .map_err(bad)?;
    verify_source_files(&job.workdir.join("inputs"), &sources)
}

fn verify_source_files(root: &Path, sources: &[InputSource]) -> CliResult<()> {
    for source in sources {
        let relative = Path::new(&source.path);
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(invalid("unsafe path in collection source fingerprint"));
        }
        let mut path = root.to_path_buf();
        for component in relative.components() {
            path.push(component.as_os_str());
            if fs::symlink_metadata(&path)?.file_type().is_symlink() {
                return Err(invalid(
                    "publisher replaced a frozen collection source with a symlink",
                ));
            }
        }
        let metadata = fs::symlink_metadata(&path)?;
        if (source.kind == ViewEntryKind::Directory) != metadata.is_dir()
            || (source.kind == ViewEntryKind::File
                && atomic_core::types::Hash::of(&fs::read(&path)?).to_base32() != source.hash)
        {
            return Err(invalid("publisher changed frozen collection source bytes; its repaired-tree test result cannot approve the original inputs"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o777 != source.mode as u32 & 0o777 {
                return Err(invalid(
                    "publisher changed frozen collection source permissions",
                ));
            }
        }
    }
    Ok(())
}

fn verify_inputs_tree(root: &Path, expected: &BTreeMap<String, ViewEntry>) -> CliResult<()> {
    let mut expected_paths = BTreeSet::new();
    for (path, entry) in expected {
        let relative = Path::new(path);
        if relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(invalid("unsafe recorded input path"));
        }
        let mut parent = Some(relative);
        while let Some(path) = parent {
            if !path.as_os_str().is_empty() {
                expected_paths.insert(path.to_path_buf());
            }
            parent = path.parent();
        }
        let actual = root.join(relative);
        let metadata = fs::symlink_metadata(&actual)?;
        if metadata.file_type().is_symlink()
            || (entry.kind == ViewEntryKind::Directory) != metadata.is_dir()
            || (entry.kind == ViewEntryKind::File && fs::read(&actual)? != entry.content)
        {
            return Err(invalid(
                "retained collection inputs differ from the frozen recorded composition",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o777 != entry.mode as u32 & 0o777 {
                return Err(invalid("retained collection input permissions changed"));
            }
        }
    }
    let mut actual_paths = BTreeSet::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() {
                return Err(invalid("unexpected symlink in retained collection inputs"));
            }
            actual_paths.insert(path.strip_prefix(root).map_err(bad)?.to_path_buf());
            if metadata.is_dir() {
                pending.push(path);
            }
        }
    }
    if actual_paths != expected_paths {
        return Err(invalid(
            "retained collection inputs contain missing or extra files",
        ));
    }
    Ok(())
}

fn retire_credentials(job: &mut Job) -> CliResult<()> {
    if job.credentials_retired {
        return Ok(());
    }
    let store = IdentityStore::open_default_existing().map_err(bad)?;
    let issuer = store.load_by_name(&job.issuer).map_err(bad)?;
    let issuer_pair = store.load_keypair(&issuer.id, None).map_err(bad)?;
    let id = atomic_identity::delegation::DelegationId::from_base32(&job.delegation)
        .ok_or_else(|| invalid("invalid recording delegation ID in retained job"))?;
    let revoke = atomic_canonical::delegation::mint_revocation(
        &issuer,
        &issuer_pair,
        &id,
        Some("Owned review publication finished or explicitly cancelled"),
    );
    store
        .save_revocation(
            &job.delegation,
            &serde_json::to_string_pretty(&revoke).map_err(bad)?,
        )
        .map_err(bad)?;
    let reviewer = store.load_by_name(&job.reviewer).map_err(bad)?;
    if atomic_canonical::did::did_for_public_key(&reviewer.public_key) != job.reviewer_did {
        return Err(invalid("refusing to retire a different identity"));
    }
    // Keep public identity material so past signatures remain verifiable.
    store.retire_secret_key(&reviewer.id).map_err(bad)?;
    job.credentials_retired = true;
    Ok(())
}

fn executable(path: &Path) -> CliResult<PathBuf> {
    let candidates = if path.components().count() > 1 {
        vec![path.to_path_buf()]
    } else {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|p| p.join(path))
            .collect()
    };
    candidates.into_iter().find(|p| p.is_file()).ok_or_else(|| {
        invalid(format!(
            "publisher executable '{}' not found; no credentials created",
            path.display()
        ))
    })
}

fn atomic_output(root: &Path, args: &[&str]) -> CliResult<Vec<u8>> {
    let output = Process::new(std::env::current_exe()?)
        .current_dir(root)
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(invalid(format!(
            "Atomic lifecycle refused: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(output.stdout)
}

fn run_bounded(command: &mut Process, timeout: Duration) -> CliResult<std::process::ExitStatus> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    struct OwnedChild {
        child: std::process::Child,
        finished: bool,
    }
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            if !self.finished {
                #[cfg(unix)]
                unsafe {
                    libc::kill(-(self.child.id() as i32), libc::SIGKILL);
                }
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }
    let mut owned = OwnedChild {
        child: command.spawn()?,
        finished: false,
    };
    let start = Instant::now();
    loop {
        if let Some(status) = owned.child.try_wait()? {
            owned.finished = true;
            return Ok(status);
        }
        if start.elapsed() >= timeout {
            // Drop kills only the process group created and owned here, never
            // a PID restored from mutable recovery metadata.
            return Err(invalid("publisher timed out; no successful session_end or publication asserted; job retained for recovery"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn read_session_id(path: &Path) -> CliResult<String> {
    if fs::metadata(path)?.len() > 64 * 1024 * 1024 {
        return Err(invalid("publisher output exceeds the bounded session-discovery budget; retained log was not truncated into approval"));
    }
    let mut ids = BTreeSet::new();
    for line in fs::read_to_string(path)?.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(id) = value.get("sessionID").and_then(Value::as_str) {
            ids.insert(id.to_string());
        }
    }
    if ids.len() != 1 {
        return Err(invalid(
            "publisher output has no unique native session identity",
        ));
    }
    Ok(ids.into_iter().next().unwrap())
}

fn lock_job(dir: &Path) -> CliResult<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("job.lock"))?;
    file.try_lock_exclusive()
        .map_err(|_| invalid("publication job is already owned by another process; retry later"))?;
    Ok(file)
}

fn save_job(dir: &Path, job: &Job) -> CliResult<()> {
    let mut temp = tempfile::NamedTempFile::new_in(dir)?;
    temp.write_all(&serde_json::to_vec_pretty(job).map_err(bad)?)?;
    temp.as_file().sync_all()?;
    temp.persist(dir.join("job.json")).map_err(bad)?;
    Ok(())
}

fn write_immutable(path: &Path, bytes: &[u8]) -> CliResult<()> {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(invalid(
            "immutable publication recovery output must not be a symlink",
        ));
    }
    if path.exists() {
        if fs::read(path)? == bytes {
            return Ok(());
        }
        return Err(invalid(
            "immutable publication recovery bytes already exist and differ; refusing replacement",
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| invalid("publication receipt has no parent directory"))?;
    let mut staging = tempfile::NamedTempFile::new_in(parent)?;
    staging.write_all(bytes)?;
    staging.as_file().sync_all()?;
    match staging.persist_noclobber(path) {
        Ok(_) => Ok(()),
        Err(e)
            if e.error.kind() == std::io::ErrorKind::AlreadyExists && fs::read(path)? == bytes =>
        {
            Ok(())
        }
        Err(e) => Err(bad(e)),
    }
}

fn output(args: &PublishArgs, job: &Job) -> CliResult<()> {
    if args.json {
        println!("{}", serde_json::to_string_pretty(job).map_err(bad)?);
    } else {
        println!("review publication {}: {}\nreview: {}\nartifacts: {}\ncredentials retired: {}\nNo insertion or push performed.",job.id,job.phase,job.review.as_deref().unwrap_or("none"),job.artifacts.len(),job.credentials_retired);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: PublishArgs,
    }
    #[test]
    fn scope_forms_are_exclusive() {
        assert!(Cli::try_parse_from(["publish", "--view", "a,b", "--session", "s"]).is_err());
        assert!(Cli::try_parse_from(["publish", "--view", "a", "--resume", "id"]).is_err());
        assert!(Cli::try_parse_from(["publish", "--timeout-seconds", "0"]).is_err());
    }
    #[test]
    fn session_identity_is_never_guessed_from_ambiguous_agent_output() {
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(
            file.path(),
            "{\"sessionID\":\"a\"}\n{\"sessionID\":\"b\"}\n",
        )
        .unwrap();
        assert!(read_session_id(file.path()).is_err());
        fs::write(
            file.path(),
            "{\"sessionID\":\"a\"}\n{\"sessionID\":\"a\"}\n",
        )
        .unwrap();
        assert_eq!(read_session_id(file.path()).unwrap(), "a");
    }
    #[test]
    fn job_lock_refuses_concurrent_owners() {
        let dir = tempfile::tempdir().unwrap();
        let _first = lock_job(dir.path()).unwrap();
        assert!(lock_job(dir.path()).is_err());
    }
    #[test]
    fn signature_recovery_bytes_are_idempotent_and_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("signed-0.json");
        write_immutable(&path, b"original signed bytes").unwrap();
        write_immutable(&path, b"original signed bytes").unwrap();
        assert!(write_immutable(&path, b"new approval must not replace original").is_err());
        assert_eq!(fs::read(path).unwrap(), b"original signed bytes");
    }
    #[test]
    fn recording_certificate_is_local_only_and_has_no_push() {
        let issuer_key = KeyPair::generate();
        let issuer = Identity::new("issuer", &issuer_key);
        let reviewer_key = KeyPair::generate();
        let reviewer = Identity::builder("temporary-reviewer")
            .identity_type(IdentityType::Agent)
            .public_key(reviewer_key.public)
            .delegated_by(issuer.id)
            .build()
            .unwrap();
        let scope = DelegationScope::builder()
            .permissions(vec![
                DelegationPermission::Read,
                DelegationPermission::Record,
            ])
            .server("atomic-local://project")
            .project("project")
            .view("view")
            .max_changes(3)
            .build();
        let grant =
            Delegation::new(&issuer, &reviewer, scope).expires_in(chrono::Duration::minutes(30));
        let value = atomic_canonical::delegation::mint(&issuer, &issuer_key, &grant);
        let encoded = atomic_canonical::delegation::encode_for_storage(&value).unwrap();
        assert!(encoded.contains("atomic-local://project"));
        assert!(!encoded.contains("\"push\""));
    }
    #[test]
    fn collection_tests_may_add_build_outputs_but_not_repair_the_reviewed_sources() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.ts");
        fs::write(&source, b"original").unwrap();
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            fs::metadata(&source).unwrap().permissions().mode() as u16 & 0o777
        };
        #[cfg(not(unix))]
        let mode = 0o644;
        let sources = vec![InputSource {
            path: "source.ts".into(),
            kind: ViewEntryKind::File,
            mode,
            hash: atomic_core::types::Hash::of(b"original").to_base32(),
        }];
        fs::create_dir(dir.path().join("target")).unwrap();
        fs::write(dir.path().join("target/build-output"), b"not source").unwrap();
        verify_source_files(dir.path(), &sources).unwrap();
        fs::write(&source, b"repaired by reviewer").unwrap();
        assert!(verify_source_files(dir.path(), &sources).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn failing_or_timed_out_runner_cannot_be_reported_successful() {
        assert!(!run_bounded(
            Process::new("/bin/sh").args(["-c", "exit 7"]),
            Duration::from_secs(1)
        )
        .unwrap()
        .success());
        assert!(run_bounded(
            Process::new("/bin/sleep").arg("2"),
            Duration::from_millis(10)
        )
        .is_err());
    }
}
