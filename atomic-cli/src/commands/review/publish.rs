//! Native, accountable evaluation of the same recorded source-to-target review.
//! No agent process, provider, delegated issuer or model participates.
mod legacy_cancel;

use std::{fs, io::Read, path::PathBuf};

use atomic_canonical::review_decision::{self, Disposition, ReviewDecision, ReviewEvaluation};
use atomic_identity::IdentityStore;
use atomic_repository::{PublicationGateConfig, Repository};
use clap::Args;
use serde_json::json;

use super::incremental::{self, IncrementalArgs};
use crate::commands::triage::publication_review::{Action as Advanced, PublicationReviewCommand};
use crate::{
    commands::{find_repository_root, Command},
    error::{CliError, CliResult},
};

fn invalid(message: impl std::fmt::Display) -> CliError {
    CliError::InvalidArgument {
        message: message.to_string(),
    }
}

#[derive(Debug, Args)]
#[command(args_conflicts_with_subcommands = true)]
pub(super) struct PublishArgs {
    /// Proposed draft/source; paired with --to.
    #[arg(long,requires="to",conflicts_with_all=["view","cancel"])]
    from: Option<String>,
    /// Shared destination/base. Measurements use this view as before.
    #[arg(long, conflicts_with = "cancel")]
    to: Option<String>,
    /// One source view (not a collection); its shared parent is the default target.
    #[arg(long,conflicts_with_all=["from","cancel"])]
    view: Option<String>,
    /// Explicit evaluation file bound to this exact report and scope.
    #[arg(long, value_name = "FILE", conflicts_with = "cancel")]
    evaluation: Option<PathBuf>,
    /// Existing local signing identity; no credentials or agents are created.
    #[arg(long, conflicts_with = "cancel")]
    identity: Option<String>,
    /// Existing decision(s) explicitly superseded by this new attestation.
    #[arg(long, conflicts_with = "cancel")]
    supersedes: Vec<String>,
    /// Show the full report, outstanding issues and an unfilled evaluation template.
    #[arg(long,conflicts_with_all=["cancel","html"])]
    dry_run: bool,
    /// Confirm retention of the supplied evaluation; never accepts issues itself.
    #[arg(long)]
    yes: bool,
    #[arg(long, conflicts_with = "html")]
    json: bool,
    /// Open an offline viewer containing the attested report and acceptance links.
    #[arg(long, conflicts_with = "cancel")]
    html: bool,
    #[arg(long, requires = "html")]
    no_open: bool,
    #[arg(short, long, requires = "html", value_name = "FILE")]
    output: Option<PathBuf>,
    /// Cleanup only: cancel a retained job from the former agent publisher.
    #[arg(long)]
    cancel: Option<String>,
    #[command(subcommand)]
    advanced: Option<Advanced>,
}

pub(super) fn run(args: &PublishArgs) -> CliResult<()> {
    if let Some(action) = &args.advanced {
        if matches!(action, Advanced::Inspect) {
            return inspect();
        }
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
    if let Some(id) = &args.cancel {
        return legacy_cancel::cancel(&repo, id, args.json);
    }
    let _boundary = if args.dry_run {
        None
    } else {
        Some(
            repo.lock_review_publication()
                .map_err(CliError::Repository)?,
        )
    };
    let source = args
        .from
        .as_deref()
        .or(args.view.as_deref())
        .unwrap_or(repo.current_view());
    let target = args
        .to
        .clone()
        .or_else(|| repo.get_view_info(source).ok()?.parent_name)
        .ok_or_else(|| {
            invalid("Supply --from <draft> --to <shared>; no destination was inferred")
        })?;
    let report = incremental::resolve(
        &repo,
        &IncrementalArgs {
            from: Some(source.into()),
            to: Some(target.clone()),
            ..Default::default()
        },
    )?;
    let mut value =
        review_decision::normalize_report(serde_json::to_value(&report).map_err(invalid)?);
    let scope = repo
        .review_decision_scope(source, &target, &value)
        .map_err(CliError::Repository)?;
    let gate = repo
        .review_decision_preflight(&scope)
        .map_err(CliError::Repository)?;
    value["publication_gate"] = json!({"allowed":gate.allowed(),"checked":gate.checked,"managed_changes":gate.managed_changes,"blocks":gate.blocks.iter().map(ToString::to_string).collect::<Vec<_>>(),"limitations":gate.limitations,"retrospective_reviews":gate.retrospective_reviews.iter().map(|review|json!({"change":review.change,"session":review.session,"review_hash":review.review_hash})).collect::<Vec<_>>()});
    let issues = review_decision::issues(&value).map_err(invalid)?;
    let hash = review_decision::report_hash(&value);
    if args.dry_run {
        println!("{}",serde_json::to_string_pretty(&json!({"schema":"atomic.review-publication-plan.v1","source":scope.source,"target":scope.target,"scope":scope,"scope_hash":scope.hash(),"report_hash":hash,"readiness":if issues.is_empty(){Disposition::Ready}else{Disposition::NotReady},"issues":issues,"evaluation_template":ReviewEvaluation::template(&scope,&value,&issues),"report":value,"execution":"native semantic/audit review only; no agent, model or credential creation","insertion":false,"push":false})).map_err(invalid)?);
        return Ok(());
    }
    let file=args.evaluation.as_ref().ok_or_else(||invalid("Supply --evaluation FILE after inspecting --dry-run --json and filling its exact evaluation_template. --yes does not evaluate or accept degradation."))?;
    let mut bytes = Vec::new();
    fs::File::open(file)?
        .take((review_decision::MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > review_decision::MAX_BYTES {
        return Err(invalid(
            "Evaluation exceeds its explicit byte budget; nothing was truncated",
        ));
    }
    let admitted = atomic_canonical::jcs::admit_document(&bytes).map_err(invalid)?;
    let evaluation: ReviewEvaluation = serde_json::from_value(
        admitted
            .get("evaluation_template")
            .unwrap_or(&admitted)
            .clone(),
    )
    .map_err(invalid)?;
    evaluation
        .validate(&scope, &value, &issues)
        .map_err(invalid)?;
    let store = IdentityStore::open_default_existing().map_err(invalid)?;
    let identity = match &args.identity {
        Some(name) => store.load_by_name(name).map_err(invalid)?,
        None => store
            .get_default()
            .map_err(invalid)?
            .ok_or_else(|| invalid("Select an existing review signing identity with --identity"))?,
    };
    let signer = atomic_canonical::did::did_for_public_key(&identity.public_key);
    let config = PublicationGateConfig::from_repo(&repo).map_err(CliError::Repository)?;
    if config
        .trust
        .evaluate(&signer, config.repository_identity.as_deref())
        != atomic_config::SignerTrust::Trusted
    {
        return Err(invalid("The selected signer is not trusted by repository policy. No identity, delegation or trust grant was created."));
    }
    let disposition = evaluation.insertion_disposition();
    eprintln!("Native review {} -> {}\nEvidence readiness: {:?}; explicit insertion disposition: {:?}\n{} outstanding issue(s), signer {}. The attestation retains every issue and rationale; it is not independent or clean-code approval.",source,target,if issues.is_empty(){Disposition::Ready}else{Disposition::NotReady},disposition,issues.len(),signer);
    if !args.yes && !dialoguer::Confirm::new().with_prompt("Attest and retain this exact evaluated scope and its stated risks? No insert or push").default(false).interact().map_err(invalid)? {return Err(invalid("Native review publication cancelled; nothing was signed or stored"));}
    let existing = repo
        .native_review_decisions()
        .map_err(CliError::Repository)?
        .into_iter()
        .find(|old| {
            old.scope == scope
                && old.report_hash == hash
                && old.evaluation == evaluation
                && old.attributed_to == signer
                && (args.supersedes.is_empty() || old.supersedes == args.supersedes)
        });
    let decision = if let Some(existing) = existing {
        repo.inspect_review_decision(&existing)
            .map_err(CliError::Repository)?;
        existing
    } else {
        let decision = ReviewDecision {
            schema: review_decision::SCHEMA.into(),
            reviewed_at: chrono::Utc::now().to_rfc3339(),
            scope,
            report_hash: hash,
            report: value,
            readiness: if issues.is_empty() {
                Disposition::Ready
            } else {
                Disposition::NotReady
            },
            issues,
            evaluation,
            independent_approval: false,
            attributed_to: signer,
            public_key: identity.public_key.to_base32(),
            supersedes: args.supersedes.clone(),
            content_hash: None,
            proof: None,
        };
        let pair = store.load_keypair(&identity.id, None).map_err(invalid)?;
        let signed = atomic_canonical::proof::attest_value(decision.value(), &identity, &pair);
        ReviewDecision::parse(&serde_json::to_vec(&signed).map_err(invalid)?).map_err(invalid)?
    };
    let evidence = repo
        .record_review_decision(&decision)
        .map_err(CliError::Repository)?;
    eprintln!(
        "Retained attested review {} (report {}). No insert or push performed.",
        evidence.decision, evidence.report_hash
    );
    if args.html {
        let html = super::html::render_published(&repo, &report, &decision)?;
        return present_html(&html, args);
    }
    if args.json {
        println!("{}",serde_json::to_string_pretty(&json!({"status":"attested","review":evidence,"vault_path":format!("review-decisions/{}/review.md",decision.artifact_hash()),"scope":decision.scope,"document":decision,"insertion":false,"push":false,"agent_execution":false})).map_err(invalid)?);
    } else {
        println!("Attested native review {}\nReport: {}\nEvidence readiness: {:?}\nInsertion disposition: {:?}\nAccepted degradation entries: {}\nNo agent/model, automatic insertion, or push.",evidence.decision,evidence.report_hash,evidence.readiness,evidence.disposition,evidence.accepted_degradations);
        if evidence.disposition != Disposition::NotReady {
            println!("Insert the same source/target using atomic insert view {} --to {}; the retained decision is revalidated at that boundary.",shell_quote(source),shell_quote(&target));
        }
    }
    Ok(())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn present_html(html: &str, args: &PublishArgs) -> CliResult<()> {
    if let Some(path) = &args.output {
        fs::write(path, html)?;
        if !args.no_open {
            open::that(path).map_err(invalid)?;
        }
    } else if args.no_open {
        println!("{html}");
    } else {
        let file = tempfile::Builder::new()
            .prefix("atomic-attested-review-")
            .suffix(".html")
            .tempfile()?;
        fs::write(file.path(), html)?;
        let (_file, path) = file.keep().map_err(invalid)?;
        open::that(path).map_err(invalid)?;
    }
    Ok(())
}

fn inspect() -> CliResult<()> {
    let root = find_repository_root()?;
    let repo = Repository::open_readonly(&root).map_err(CliError::Repository)?;
    let mut results = Vec::new();
    for decision in repo
        .native_review_decisions()
        .map_err(CliError::Repository)?
    {
        let validation = match repo.inspect_review_decision(&decision) {
            Ok(evidence) => json!({"currently_applicable":true,"evidence":evidence}),
            Err(error) => json!({"currently_applicable":false,"error":error.to_string()}),
        };
        results.push(json!({"kind":"native_review_decision","review":decision.artifact_hash(),"document":decision,"validation":validation}));
    }
    let keys = atomic_repository::repository::provenance_gate::local_did_key_provider(&repo);
    let config = PublicationGateConfig::from_repo(&repo).map_err(CliError::Repository)?;
    for review in repo.publication_reviews().map_err(CliError::Repository)? {
        let validation = match repo.validate_publication_review(&review, &config, &keys) {
            Ok(value) => serde_json::to_value(value).map_err(invalid)?,
            Err(error) => json!({"conforms":false,"error":error.to_string()}),
        };
        results.push(json!({"kind":"retrospective_publication_review","review":review.artifact_hash(),"subject":review.subject,"document":review,"validation":validation}));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&results).map_err(invalid)?
    );
    Ok(())
}
