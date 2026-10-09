//! The CLI's service-layer hooks: the presentation half of each wired
//! command (D6 — terminal formatting stays in the CLI; the domain
//! executes in the service layer).
//!
//! Routing policy lives in the service area ([`crate::service`]):
//! `ATOMIC_SERVICE=local` (the default) runs the same libatomic handlers
//! atomicd serves, in-process — no socket, no daemon, no probing;
//! `ATOMIC_SERVICE=reactor` routes over the daemon socket (D4
//! start-or-retry); `ATOMIC_RPC=1` is the legacy reactor alias.
//! Every wired command routes every flag-form through the service
//! layer; the remaining `Ok(false)` exits are the file-only forms (no
//! repository database), the documented no-op, and the scope-out
//! surfaces: `view list --remote` (the remote listing), the git interop
//! family (import/parallel/push keep their today-bodies), and
//! `triage review` (the canonical report builder is a CLI-entangled
//! module whose handler migration is a separate slice — see its
//! dispatch guard). Push/pull keep auth/config resolution CLIENT-SIDE
//! (identity store + config reads, never redb) and ship the resolved
//! headers; the network sync itself runs handler-side.

use std::path::{Path, PathBuf};

use atomic_client::proto as pb;
use atomic_core::types::{Base32, Merkle};
use libatomic::atomic::attestation_target::Kind as TargetKind;
use libatomic::atomic::update_vault_entity_request::Update as UpdateKind;
use libatomic::atomic::verify_attestation_request::Target as VerifyTarget;
use libatomic::atomic::VaultEntityKind as Kind;
use libatomic::atomic::VaultEntityStatus as Status;

use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_info, print_success, print_warning};
use crate::service::{status_message, Service};

/// The raw status label for a wire entry's status enum — the label the
/// update report and the listing rows print (falls back to the entry's
/// carried `status_label` for non-lifecycle statuses).
pub(crate) fn entry_status_label(status: i32) -> Option<String> {
    Status::try_from(status)
        .ok()
        .map(status_str)
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// vault entry bundles — the wire-carried lift/classification inputs
// ---------------------------------------------------------------------------

/// The wire's entry bundle (schema "atomic.vault.entry.bundle.v1"): the
/// stored entry's full domain serialization plus the RAW attestation
/// sources, deserialized into the domain shapes the CLI's existing
/// bridge code consumes.
#[derive(serde::Deserialize)]
struct EntryBundle {
    entry: atomic_core::pristine::VaultEntry,
    #[serde(default)]
    attestation: Option<BundleAttestationSources>,
}

#[derive(serde::Deserialize)]
struct BundleAttestationSources {
    #[serde(default)]
    tracked: Option<BundleTrackedAttestation>,
    #[serde(default)]
    sidecar: Option<BundleSidecar>,
}

#[derive(serde::Deserialize)]
struct BundleTrackedAttestation {
    path: String,
    frontmatter_json: String,
    content: Vec<u8>,
}

#[derive(serde::Deserialize)]
struct BundleSidecar {
    path: String,
    text: String,
}

/// Deserialize one entry bundle, refusing an unexpected schema.
fn parse_entry_bundle(bundle: &pb::VersionedBytes) -> CliResult<EntryBundle> {
    if bundle.schema != "atomic.vault.entry.bundle.v1" {
        return Err(CliError::Internal(anyhow::anyhow!(
            "unknown entry bundle schema '{}'",
            bundle.schema
        )));
    }
    serde_json::from_slice(&bundle.payload)
        .map_err(|error| CliError::Internal(anyhow::anyhow!("entry bundle: {error}")))
}

/// The intent bridge's sources off one bundle.
fn intent_sources(bundle: &EntryBundle) -> crate::commands::intent::bridge::AttestationSources {
    use crate::commands::intent::bridge::{
        AttestationSources, SidecarArtifact, TrackedAttestation,
    };
    let attestation = bundle.attestation.as_ref();
    AttestationSources {
        tracked: attestation
            .and_then(|sources| sources.tracked.as_ref())
            .map(|tracked| TrackedAttestation {
                vault_path: tracked.path.clone(),
                frontmatter_json: tracked.frontmatter_json.clone(),
                content: tracked.content.clone(),
            }),
        sidecar: attestation
            .and_then(|sources| sources.sidecar.as_ref())
            .map(|sidecar| SidecarArtifact {
                path: sidecar.path.clone(),
                text: sidecar.text.clone(),
            }),
    }
}

/// The memory bridge's sources off one bundle.
fn memory_sources(bundle: &EntryBundle) -> crate::commands::memory::bridge::AttestationSources {
    use crate::commands::memory::bridge::{
        AttestationSources, SidecarArtifact, TrackedAttestation,
    };
    let attestation = bundle.attestation.as_ref();
    AttestationSources {
        tracked: attestation
            .and_then(|sources| sources.tracked.as_ref())
            .map(|tracked| TrackedAttestation {
                vault_path: tracked.path.clone(),
                frontmatter_json: tracked.frontmatter_json.clone(),
                content: tracked.content.clone(),
            }),
        sidecar: attestation
            .and_then(|sources| sources.sidecar.as_ref())
            .map(|sidecar| SidecarArtifact {
                path: sidecar.path.clone(),
                text: sidecar.text.clone(),
            }),
    }
}

// ---------------------------------------------------------------------------
// KG wire reconstruction — the domain nodes/edges the JSON renders emit
// ---------------------------------------------------------------------------

/// Wire KGNode → the domain node (kind/label/summary/source/metadata),
/// the shape the local JSON serializes.
fn kg_node_domain(node: &pb::KgNode) -> atomic_core::pristine::vault::KgNode {
    atomic_core::pristine::vault::KgNode {
        id: node.id.clone(),
        kind: node.node_type.clone(),
        label: node.name.clone().unwrap_or_default(),
        summary: node
            .data
            .as_ref()
            .map(|data| String::from_utf8_lossy(data).into_owned()),
        source: node.source.clone().unwrap_or_default(),
        metadata: node
            .metadata
            .as_ref()
            .and_then(|metadata| serde_json::from_slice(metadata).ok()),
    }
}

/// Wire KGEdge → the domain edge.
fn kg_edge_domain(edge: &pb::KgEdge) -> atomic_core::pristine::vault::KgEdge {
    atomic_core::pristine::vault::KgEdge {
        from_id: edge.source.clone(),
        to_id: edge.target.clone(),
        kind: edge.relation.clone(),
        metadata: edge
            .metadata
            .as_ref()
            .and_then(|metadata| serde_json::from_slice(metadata).ok()),
    }
}

fn request_meta() -> Option<pb::RequestMeta> {
    Some(pb::RequestMeta {
        request_id: uuid::Uuid::new_v4().to_string(),
        observed_at: None,
    })
}

fn short_hash(hash: &Option<pb::Hash>) -> String {
    hash.as_ref()
        .and_then(|h| {
            h.value
                .clone()
                .try_into()
                .ok()
                .map(|bytes: [u8; 32]| Merkle(bytes).to_base32()[..12].to_string())
        })
        .unwrap_or_else(|| "-".to_string())
}

fn full_hash(hash: &Option<pb::Hash>) -> String {
    hash.as_ref()
        .and_then(|h| {
            h.value
                .clone()
                .try_into()
                .ok()
                .map(|bytes: [u8; 32]| Merkle(bytes).to_base32())
        })
        .unwrap_or_else(|| "-".to_string())
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

/// `atomic status` over Status — including `--json`: the IDE contract
/// (schema_version, repository_root, entries) renders client-side from
/// the wire's file entries, and `--reindex` composes the wire's
/// Repair(ReindexWorkingCopy) before the status read. Only
/// `--debug-ignore` stays local: it reads ignore files (`.atomicignore`
/// plus the user's global ignore list) without touching the repository
/// databases, so it needs no wire.
pub fn status(args: &super::status::Status) -> CliResult<bool> {
    if args.debug_ignore {
        return Ok(false);
    }
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    // `--reindex` rebuilds the working-copy index through the wire's
    // repair action, then falls through to the status read — the same
    // compose the local body performs. The line renders from the wire's
    // reindexed count with a client-side clock, byte-for-byte with the
    // local reindex report.
    if args.reindex {
        let start = std::time::Instant::now();
        let repair = session.repair(pb::RepairRequest {
            repository: Some(session.reference.clone()),
            meta: None,
            action: pb::RepairAction::ReindexWorkingCopy as i32,
            force: false,
            view: String::new(),
        });
        match repair {
            Ok(response) => {
                if !args.json {
                    print_info(&format!(
                        "Reindexed {} files in {:.1}s",
                        response.reindexed,
                        start.elapsed().as_secs_f64()
                    ));
                }
            }
            Err(error) if args.json => return Err(error),
            Err(error) => {
                print_warning(&format!("Reindex failed: {error}"));
            }
        }
    }
    let request = pb::StatusRequest {
        repository: Some(session.reference.clone()),
    };
    let response = session.status(request)?;

    if args.json {
        // The IDE JSON contract — schema_version/repository_root are the
        // client's own; everything else rides StatusResponse.
        let root = crate::commands::find_repository_root()?;
        let entries: Vec<serde_json::Value> = response
            .files
            .iter()
            .map(|file| {
                let name = match pb::FileStatus::try_from(file.status) {
                    Ok(pb::FileStatus::Modified) => "modified",
                    Ok(pb::FileStatus::Deleted) => "deleted",
                    Ok(pb::FileStatus::Untracked) => "untracked",
                    Ok(pb::FileStatus::Added) => "added",
                    Ok(pb::FileStatus::Conflicted) => "conflicted",
                    _ => "clean",
                };
                let code = match pb::FileStatus::try_from(file.status) {
                    Ok(pb::FileStatus::Modified) => "M",
                    Ok(pb::FileStatus::Deleted) => "D",
                    Ok(pb::FileStatus::Untracked) => "?",
                    Ok(pb::FileStatus::Added) => "A",
                    Ok(pb::FileStatus::Conflicted) => "C",
                    _ => "-",
                };
                serde_json::json!({
                    "path": file.path,
                    "status": name,
                    "code": code,
                    "details": file.details,
                })
            })
            .collect();
        let clean = entries.is_empty();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "schema_version": 1,
                "repository_root": root.display().to_string(),
                "view": response.view,
                "state": short_hash(&response.view_merkle),
                "clean": clean,
                "needs_reindex": response.needs_reindex,
                "stale_index_count": response.stale_index_count,
                "entries": entries,
            }))
            .map_err(|e| CliError::Internal(e.into()))?
        );
        return Ok(true);
    }

    if args.short {
        for file in &response.files {
            let code = match pb::FileStatus::try_from(file.status) {
                Ok(pb::FileStatus::Modified) => "M",
                Ok(pb::FileStatus::Deleted) => "D",
                Ok(pb::FileStatus::Untracked) => "?",
                Ok(pb::FileStatus::Added) => "A",
                Ok(pb::FileStatus::Conflicted) => "C",
                _ => " ",
            };
            println!("{code}  {}", file.path);
        }
        return Ok(true);
    }

    println!("On view {}", response.view);
    if response.view_merkle.is_some() {
        println!("State: {}...", short_hash(&response.view_merkle));
    }
    println!();
    let dirty: Vec<_> = response
        .files
        .iter()
        .filter(|f| {
            f.status != pb::FileStatus::Unspecified as i32
                && f.status != pb::FileStatus::Untracked as i32
        })
        .collect();
    if !dirty.is_empty() {
        println!("Changes to be recorded:");
        println!();
        for file in &dirty {
            let label = match pb::FileStatus::try_from(file.status) {
                Ok(pb::FileStatus::Added) => "new file",
                Ok(pb::FileStatus::Deleted) => "deleted",
                Ok(pb::FileStatus::Conflicted) => "conflicted",
                _ => "modified",
            };
            println!("\t{label}:   {}", file.path);
        }
    }
    let untracked: Vec<_> = response
        .files
        .iter()
        .filter(|f| f.status == pb::FileStatus::Untracked as i32)
        .collect();
    if !untracked.is_empty() {
        if !dirty.is_empty() {
            println!();
        }
        println!("Untracked files:");
        println!("  (use \"atomic add <file>...\" to include in what will be recorded)");
        println!();
        for file in &untracked {
            println!("\t{}", file.path);
        }
        println!();
        println!("Use \"atomic add <file>...\" to track files");
    } else if dirty.is_empty() {
        println!("nothing else to record");
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// log
// ---------------------------------------------------------------------------

pub fn log(args: &crate::commands::log::command::Log) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::LogRequest {
        repository: Some(session.reference.clone()),
        all_views: args.all,
        view: args.view.clone(),
        cursor: None,
        budget: args.count.map(|count| pb::PageBudget {
            max_items: Some(count as u32),
            max_bytes: None,
        }),
        path_filter: args.path.clone().into_iter().collect(),
        tags_only: args.tags_only,
    };
    let response = session.log(request)?;

    let mut entries = response.entries; // the wire carries newest-first
    if args.reverse {
        entries.reverse(); // --reverse: oldest first
    }

    // Same renders as the local path: the versioned pretty JSON array for
    // -f json, and the styled default format (hash === markers, (tag)
    // marker, blank line between entries) otherwise.
    use crate::commands::log::types::{JsonAuthor, JsonLogEntry, LogFormat};
    if matches!(args.format, LogFormat::Json) {
        let json_entries: Vec<JsonLogEntry> = entries
            .iter()
            .map(|entry| JsonLogEntry {
                sequence: entry.sequence,
                hash: wire_hash_base32(&entry.hash),
                state: wire_hash_base32(&entry.state),
                message: entry.message.clone(),
                description: entry.description.clone(),
                authors: entry
                    .authors
                    .iter()
                    .map(|author| JsonAuthor {
                        name: author.name.clone(),
                        email: author.email.clone(),
                    })
                    .collect(),
                timestamp: entry.recorded_at.as_ref().and_then(|ts| {
                    chrono::DateTime::from_timestamp(ts.seconds, ts.nanos.max(0) as u32)
                        .map(|time| time.to_rfc3339())
                }),
                is_tagged: entry.is_tagged,
            })
            .collect();
        let doc = serde_json::to_string_pretty(&json_entries)
            .map_err(|e| CliError::Internal(e.into()))?;
        println!("{doc}");
        return Ok(true);
    }

    use crate::commands::{format_hash_with_length, format_timestamp, DEFAULT_HASH_LENGTH};
    use crate::output::{
        author as style_author, hash as style_hash, hint, timestamp as style_timestamp,
    };
    for (i, entry) in entries.iter().enumerate() {
        // Separator between entries (the old formatter's `i > 0` rule).
        if i > 0 {
            println!();
        }

        let hash = Merkle(
            entry
                .hash
                .as_ref()
                .and_then(|h| h.value.clone().try_into().ok())
                .unwrap_or([0u8; 32]),
        );
        let hash_str = format_hash_with_length(&hash, DEFAULT_HASH_LENGTH);
        let tagged_marker = if entry.is_tagged { " (tag)" } else { "" };
        println!(
            "{} === {} ==={}",
            hint(&format!("#{}", entry.sequence)),
            style_hash(&hash_str),
            hint(tagged_marker)
        );

        for author in &entry.authors {
            let author_line = match &author.email {
                Some(email) => format!("{} <{email}>", author.name),
                None => author.name.clone(),
            };
            println!("Author: {}", style_author(&author_line));
        }
        if let Some(recorded_at) = &entry.recorded_at {
            if let Some(time) = chrono::DateTime::from_timestamp(
                recorded_at.seconds,
                recorded_at.nanos.max(0) as u32,
            ) {
                println!(
                    "Date:   {}",
                    style_timestamp(&format_timestamp(&time.with_timezone(&chrono::Utc)))
                );
            }
        }

        println!();
        if let Some(message) = &entry.message {
            for line in message.lines() {
                println!("    {line}");
            }
        }
        if let Some(description) = &entry.description {
            println!();
            for line in description.lines() {
                println!("    {line}");
            }
        }
    }
    Ok(true)
}

/// The Base32 form of a wire `Hash` (the local path's `Hash::to_base32`).
fn wire_hash_base32(hash: &Option<pb::Hash>) -> String {
    hash.as_ref()
        .and_then(|h| h.value.clone().try_into().ok())
        .map(|bytes| Merkle(bytes).to_base32())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// add / record
// ---------------------------------------------------------------------------

/// `atomic add` over AddFiles — every form: dry-run/force/directory/
/// no-recursive ride the wire, and internal `.atomic` paths route too
/// (the repository's ignore rules treat them like any other ignored
/// path — the tolerant per-path report carries the skip).
pub fn add(args: &super::add::Add) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let root = crate::commands::find_repository_root()?;
    let paths = if args.all {
        // `add --all` composes an untracked read first — the same read the
        // CLI does today, over the service layer.
        let request = pb::StatusRequest {
            repository: Some(session.reference.clone()),
        };
        let status = session.status(request)?;
        status
            .files
            .iter()
            .filter(|file| file.status == pb::FileStatus::Untracked as i32)
            .map(|file| file.path.clone())
            .collect()
    } else {
        args.files.clone()
    };
    for path in &paths {
        println!("Adding: {path}");
    }
    let request = pb::AddFilesRequest {
        repository: Some(session.reference.clone()),
        meta: request_meta(),
        paths: paths.iter().map(|path| relativize(path, &root)).collect(),
        expected: None,
        dry_run: args.dry_run,
        force: args.force,
        directory: args.directory,
        no_recursive: args.no_recursive,
    };
    let response = session.add_files(request)?;
    if response.dry_run {
        for path in &response.would_add {
            println!("Would add: {path}");
        }
        for path in &response.skipped_tracked {
            println!("Skipping: {path} (already tracked)");
        }
        for path in &response.skipped_ignored {
            println!("Skipping: {path} (ignored)");
        }
        for path in &response.failed_paths {
            println!("Error: {path} - could not be added");
        }
        println!("Would add {} file(s)", response.would_add.len());
        return Ok(true);
    }
    println!("✓ Added {} file(s)", response.tracked);
    Ok(true)
}

fn relativize(path: &str, root: &Path) -> String {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        if let Ok(relative) = candidate.strip_prefix(root) {
            return relative.display().to_string();
        }
        return path.to_string();
    }
    path.to_string()
}

/// `atomic record` over Record — every form. The message composes
/// client-side with the SAME editor/prompt flow the local body uses
/// (`get_message` — editors are interactive, client-side by design),
/// `--author` parses into the wire author exactly like the local body,
/// `--identity` names an identity the handler resolves host-side, and
/// `--dry-run` is the Status read's preview — the same "Would record"
/// lines the local body prints, rendered from the wire's file entries.
pub fn record(args: &super::record::Record) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    if args.dry_run {
        let response = session.status(pb::StatusRequest {
            repository: Some(session.reference.clone()),
        })?;
        let entries = response
            .files
            .iter()
            .map(|file| {
                let status = match pb::FileStatus::try_from(file.status) {
                    Ok(pb::FileStatus::Modified) => atomic_repository::status::FileStatus::Modified,
                    Ok(pb::FileStatus::Deleted) => atomic_repository::status::FileStatus::Deleted,
                    Ok(pb::FileStatus::Untracked) => {
                        atomic_repository::status::FileStatus::Untracked
                    }
                    Ok(pb::FileStatus::Added) => atomic_repository::status::FileStatus::Added,
                    Ok(pb::FileStatus::Conflicted) => {
                        atomic_repository::status::FileStatus::Conflicted
                    }
                    _ => atomic_repository::status::FileStatus::Clean,
                };
                (status, file.path.clone())
            })
            .collect::<Vec<_>>();
        crate::commands::record::render_dry_run(&entries, args.all, &args.files);
        return Ok(true);
    }
    // The message composes BEFORE any staging so a cancelled editor leaves
    // the working copy untouched (the same ordering the local body uses).
    let message = args.get_message()?;
    // Authorship mirrors the local precedence: --identity wins over
    // --author; the handler turns the named identity into an author and
    // signing credentials host-side.
    let (author, identity_name) = if let Some(name) = args.identity.clone() {
        (None, Some(name))
    } else {
        // The local parse: "Name <email>" or a bare name.
        let author = args.parse_author().map(|parsed| pb::Author {
            name: parsed.name,
            email: parsed.email,
        });
        (author, None)
    };
    let root = crate::commands::find_repository_root()?;
    // `--all` stages untracked files first — the same client-side
    // composition `atomic add --all` performs (a Status read + AddFiles),
    // silently like the local body's pre-record staging.
    if args.all {
        let status = session.status(pb::StatusRequest {
            repository: Some(session.reference.clone()),
        })?;
        let untracked: Vec<String> = status
            .files
            .iter()
            .filter(|file| file.status == pb::FileStatus::Untracked as i32)
            .map(|file| file.path.clone())
            .collect();
        if !untracked.is_empty() {
            session.add_files(pb::AddFilesRequest {
                repository: Some(session.reference.clone()),
                meta: request_meta(),
                paths: untracked,
                expected: None,
                dry_run: false,
                force: false,
                directory: false,
                no_recursive: false,
            })?;
        }
    }
    let request = pb::RecordRequest {
        repository: Some(session.reference.clone()),
        meta: request_meta(),
        message,
        paths: args
            .files
            .iter()
            .map(|path| relativize(path, &root))
            .collect(),
        expected: None,
        ai_authorship: None,
        author,
        identity_name,
    };
    let response = session.record(request)?;
    let Some(change) = response.change else {
        return Err(CliError::Internal(anyhow::anyhow!(
            "record produced no change"
        )));
    };
    let view = response.view.unwrap_or_default();
    println!(
        "[{} {}/{}] {}",
        view,
        change
            .recorded_at
            .as_ref()
            .map(|_| "1".to_string())
            .unwrap_or_default(),
        short_hash(&change.hash),
        change.message.clone().unwrap_or_default()
    );
    if !args.files.is_empty() {
        for file in &args.files {
            println!(" {file}");
        }
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// intent
// ---------------------------------------------------------------------------

pub fn intent_new(args: &super::intent::new::IntentNew) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::CreateVaultEntityRequest {
        repository: Some(session.reference.clone()),
        meta: request_meta(),
        kind: Kind::Intent as i32,
        title: args.title.clone(),
        body: None,
        memory_kind: None,
        derived_from: Vec::new(),
        about: Vec::new(),
        developer: None,
        intent: None,
        model: None,
        view: None,
        expected_snapshot: None,
    };
    let response = session.create_vault_entity(request)?;
    let entry = response.entry.expect("created entry");
    println!("Created intent: {}", entry.id);
    println!("  file: {}", entry.vault_path.clone().unwrap_or_default());
    println!("  template: {}", args.template);
    println!("  kind: {}", args.kind);
    Ok(true)
}

/// `atomic intent update` over UpdateVaultEntity — the FULL surface.
/// The wire's composed-update arm carries every field in ONE request
/// (status, assignee, priority, title, reason, informed-by, body, and
/// the --force rewrite guard), so the handler applies them atomically
/// through the same domain update the local body performs. The body
/// still composes client-side (inline or stdin — reading the client's
/// stdin is client-side by design); the no-op form errors client-side
/// with the local body's exact message.
pub fn intent_update(args: &super::intent::update::IntentUpdate) -> CliResult<bool> {
    let body_requested = args.body.is_some() || args.body_stdin;
    if !body_requested
        && args.status.is_none()
        && args.assignee.is_none()
        && args.priority.is_none()
        && args.title.is_none()
        && args.reason.is_none()
        && args.informed_by.is_empty()
        && !args.force
    {
        return Err(CliError::InvalidArgument {
            message: "Nothing to update. Provide at least one of \
                --status, --assignee, --priority, --title, --reason, \
                --informed-by, \
                --body, or --body-stdin."
                .to_string(),
        });
    }
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    // The pure status/body forms ride their original wire arms; anything
    // else composes the full field set into ONE request.
    let update = if args.assignee.is_none()
        && args.priority.is_none()
        && args.title.is_none()
        && args.reason.is_none()
        && args.informed_by.is_empty()
        && !args.force
    {
        if body_requested {
            let body = args
                .resolve_body()?
                .expect("--body/--body-stdin always resolves a body");
            UpdateKind::Body(body)
        } else {
            let status = args.status.clone().expect("checked above");
            let proto_status = match status.as_str() {
                "backlog" => Status::Backlog,
                "in_progress" | "in-progress" => Status::InProgress,
                "needs-review" => Status::NeedsReview,
                "done" | "completed" => Status::Done,
                "planned" => Status::Planned,
                "icebox" => Status::Suspended,
                other => {
                    return Err(CliError::InvalidArgument {
                        message: format!("unknown intent status '{other}'"),
                    })
                }
            };
            UpdateKind::Status(proto_status as i32)
        }
    } else {
        // The mixed/multi-field update: ONE request, applied atomically.
        UpdateKind::Fields(pb::IntentUpdateFields {
            status: args.status.clone(),
            assignee: args.assignee.clone(),
            priority: args.priority.clone(),
            title: args.title.clone(),
            reason: args.reason.clone(),
            informed_by: args.informed_by.clone(),
            body: args.resolve_body()?,
            force: args.force,
        })
    };
    let body_updated = body_requested;
    let request = pb::UpdateVaultEntityRequest {
        repository: Some(session.reference.clone()),
        meta: request_meta(),
        kind: Kind::Intent as i32,
        id: args.id.clone(),
        update: Some(update),
        expected: None,
        view: None,
    };
    let response = session.update_vault_entity(request)?;
    let entry = response.entry.expect("updated entry");
    // Render parity with the local body's update report.
    println!("Updated intent: {}", entry.id);
    let status = entry
        .status_label
        .clone()
        .or_else(|| entry.status.and_then(entry_status_label));
    if let Some(status) = status {
        println!("  status: {status}");
    }
    if let Some(priority) = &entry.priority {
        println!("  priority: {priority}");
    }
    if let Some(assignee) = &entry.assignee {
        println!("  assignee: {assignee}");
    }
    if body_updated {
        println!("  body: updated");
    }
    if !args.informed_by.is_empty() {
        println!("  informed by: {} source(s)", args.informed_by.len());
    }
    Ok(true)
}

fn status_str(status: Status) -> &'static str {
    match status {
        Status::Backlog => "backlog",
        Status::InProgress => "in_progress",
        Status::NeedsReview => "needs-review",
        Status::Done => "done",
        Status::Planned => "planned",
        Status::Active => "active",
        Status::Suspended => "icebox",
        Status::Completed => "completed",
        Status::Superseded => "superseded",
        Status::Retracted => "retracted",
        _ => "unknown",
    }
}

pub fn intent_attest(args: &super::intent::attest::IntentAttest) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let target = pb::AttestationTarget {
        kind: Some(TargetKind::IntentId(args.id.clone())),
    };

    // Signing elsewhere, step 1: say what to sign — the document and the
    // bytes a key holder elsewhere (a browser, a hardware token, a signing
    // service) signs. No key on this machine is involved.
    if args.prepare {
        let request = pb::PrepareAttestationRequest {
            repository: Some(session.reference.clone()),
            meta: request_meta(),
            target: Some(target),
            identity_did: args.identity.clone().unwrap_or_default(),
            view: None,
        };
        let response = session.prepare_attestation(request)?;
        let document: serde_json::Value = serde_json::from_str(&response.document)
            .map_err(|e| CliError::Internal(anyhow::anyhow!("prepared document: {e}")))?;
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "document": document,
                "signingBytes": data_encoding::BASE64.encode(&response.signing_bytes),
            }))
            .unwrap()
        );
        return Ok(true);
    }

    // Signing elsewhere, step 2: record an attestation signed elsewhere. The
    // file holds the `--prepare` document with its proof attached; the service
    // re-derives the document from the intent AS IT IS NOW and verifies the
    // signature against it, so a stale or altered intent is refused.
    let caller_signature = if let Some(path) = &args.signed {
        let text = std::fs::read_to_string(path).map_err(CliError::Io)?;
        let signed: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| CliError::InvalidArgument {
                message: format!("{} is not JSON: {e}", path.display()),
            })?;
        atomic_canonical::proof::proof_signature(&signed)
            .map_err(|e| CliError::InvalidArgument {
                message: format!("not an attested intent: {e}"),
            })?
            .as_bytes()
            .to_vec()
    } else {
        Vec::new()
    };

    let request = pb::RecordAttestationRequest {
        repository: Some(session.reference.clone()),
        meta: request_meta(),
        target: Some(target),
        identity_did: args.identity.clone().unwrap_or_default(),
        signature: caller_signature,
        view: None,
        expected_snapshot: None,
    };
    let response = session.record_attestation(request)?;
    let info = response.attestation.expect("recorded attestation");
    println!("Attested intent: {}", info.id);
    println!(
        "  vault:     {}",
        info.vault_path.clone().unwrap_or_default()
    );
    println!(
        "  sidecar:   {}",
        info.sidecar_path.clone().unwrap_or_default()
    );
    println!("  author:    {}", info.identity_did);
    println!("conforms: yes");
    Ok(true)
}

pub fn intent_validate(args: &super::intent::validate::IntentValidate) -> CliResult<bool> {
    // Path-arg form reads a file, no redb — stays local.
    if args.id_or_path.ends_with(".md") || args.id_or_path.contains('/') {
        return Ok(false);
    }
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    validate_entity(&session, Kind::Intent, &args.id_or_path, args.json)
}

pub fn intent_verify(args: &super::intent::verify::IntentVerify) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::VerifyAttestationRequest {
        repository: Some(session.reference.clone()),
        target: Some(VerifyTarget::Subject(pb::AttestationTarget {
            kind: Some(TargetKind::IntentId(args.id.clone())),
        })),
        view: None,
        identity_name: args.identity.clone(),
    };
    let response = session.verify_attestation(request)?;
    if !response.valid {
        return Err(CliError::InvalidArgument {
            message: response
                .reason
                .unwrap_or_else(|| "verification failed".into()),
        });
    }
    println!("Verified intent: {}", args.id);
    println!("  author: {}", response.reason.unwrap_or_default());
    Ok(true)
}

/// `atomic intent list` over ListVaultEntries — every form. The handler
/// computes the per-intent manifest kind and the attestation/verifies
/// state with the same domain logic the local body uses; the CLI renders
/// with the SAME table/JSON code (one render, two data sources). The
/// `--kind`/`--review` filter validates client-side (an unknown kind is
/// a clean argument error) and applies over the wire's classification.
pub fn intent_list(args: &super::intent::list::IntentList) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    // Resolve + validate the kind filter (`--review` ⇒ `--kind review`)
    // BEFORE the request, so the argument error never round-trips.
    let filter = args.kind_filter();
    if let Some(kind) = &filter {
        if !atomic_canonical::vocab::is_known_intent_kind(kind) {
            return Err(CliError::InvalidArgument {
                message: format!(
                    "unknown intent kind '{}' (expected one of {:?})",
                    kind,
                    atomic_canonical::vocab::INTENT_KIND
                ),
            });
        }
    }
    let request = pb::ListVaultEntriesRequest {
        repository: Some(session.reference.clone()),
        kind: Some(Kind::Intent as i32),
        status: None,
        summaries_only: false,
        cursor: None,
        budget: None,
        view: None,
        identity: args.identity.clone(),
        path_prefix: None,
        entry_type: None,
        status_filter: None,
        tool_result_previews: false,
    };
    let response = session.list_vault_entries(request)?;
    let mut rows = response
        .entries
        .iter()
        .map(super::intent::list::wire_row)
        .collect::<Vec<_>>();
    if let Some(kind) = &filter {
        rows.retain(|row| &row.kind == kind);
    }
    super::intent::list::IntentList::render(&rows, args.json);
    Ok(true)
}

/// `atomic intent show` over GetVaultEntry — every form. The wire
/// carries the lift inputs (the stored entry) plus the RAW attestation
/// sources (the tracked entry and the legacy sidecar), so the CLI runs
/// its EXISTING bridge::lift / bridge::load_attestation code unchanged
/// over wire-carried inputs and projects with the same render the local
/// body uses — byte-parity for the JSON-LD projection.
pub fn intent_show(args: &super::intent::show::IntentShow) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::GetVaultEntryRequest {
        repository: Some(session.reference.clone()),
        kind: Kind::Intent as i32,
        id: args.id.clone(),
        path: None,
        include_bundle: true,
        view: None,
    };
    let response = session.get_vault_entry(request)?;
    let Some(bundle) = response
        .entry_bundle
        .as_ref()
        .map(parse_entry_bundle)
        .transpose()?
    else {
        return Err(CliError::Internal(anyhow::anyhow!(
            "the service returned no entry bundle (include_bundle required)"
        )));
    };
    let inputs = crate::commands::intent::bridge::inputs_from_entry(&bundle.entry)?;
    let attestation =
        crate::commands::intent::bridge::classify_attestation(intent_sources(&bundle), &inputs);
    super::intent::show::project(&args.id, &inputs, &attestation, args.json)?;
    Ok(true)
}

fn validate_entity(session: &Service, kind: Kind, id: &str, json: bool) -> CliResult<bool> {
    let request = pb::ValidateVaultEntityRequest {
        repository: Some(session.reference.clone()),
        kind: kind as i32,
        subject: Some(pb::validate_vault_entity_request::Subject::Id(
            id.to_string(),
        )),
        content_hash: None,
        view: None,
    };
    let response = session.validate_vault_entity(request)?;
    if json {
        // Same top-level shape the local body's `report_json` prints; the
        // wire carries each violation as one message string, so an entry is
        // its message.
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "conforms": response.valid,
                "results": response
                    .issues
                    .iter()
                    .map(|issue| serde_json::json!({ "message": issue }))
                    .collect::<Vec<_>>(),
            }))
            .unwrap()
        );
        return Ok(true);
    }
    if response.valid {
        println!("conforms: yes");
    } else {
        println!("conforms: no ({} violation(s))", response.issues.len());
        for issue in &response.issues {
            println!("  ✗ {issue}");
        }
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// memory
// ---------------------------------------------------------------------------

pub fn memory_new(args: &super::memory::new::MemoryNew) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::CreateVaultEntityRequest {
        repository: Some(session.reference.clone()),
        meta: request_meta(),
        kind: Kind::Memory as i32,
        title: String::new(),
        body: args.text.clone(),
        memory_kind: Some(args.kind.clone()),
        derived_from: args.derived_from.clone(),
        about: args.about.clone(),
        developer: None,
        intent: None,
        model: None,
        view: None,
        expected_snapshot: None,
    };
    let response = session.create_vault_entity(request)?;
    let entry = response.entry.expect("created entry");
    println!("Created memory: {}", entry.id);
    println!("  file: {}", entry.vault_path.clone().unwrap_or_default());
    println!("  kind: {}", args.kind);
    println!("  status: {}", args.status);
    Ok(true)
}

pub fn memory_attest(args: &super::memory::attest::MemoryAttest) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::RecordAttestationRequest {
        repository: Some(session.reference.clone()),
        meta: request_meta(),
        target: Some(pb::AttestationTarget {
            kind: Some(TargetKind::MemoryId(args.id.clone())),
        }),
        identity_did: args.identity.clone().unwrap_or_default(),
        signature: Vec::new(),
        view: None,
        expected_snapshot: None,
    };
    let response = session.record_attestation(request)?;
    let info = response.attestation.expect("recorded attestation");
    println!("Attested memory: {}", info.id);
    println!(
        "  vault:     {}",
        info.vault_path.clone().unwrap_or_default()
    );
    println!(
        "  sidecar:   {}",
        info.sidecar_path.clone().unwrap_or_default()
    );
    println!("  author:    {}", info.identity_did);
    println!("conforms: yes");
    println!("note: signing keys are stored unencrypted on disk; treat this attestation as a non-production dev signature until key-at-rest encryption lands.");
    Ok(true)
}

pub fn memory_validate(args: &super::memory::validate::MemoryValidate) -> CliResult<bool> {
    if args.id_or_path.ends_with(".md") {
        return Ok(false);
    }
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    validate_entity(&session, Kind::Memory, &args.id_or_path, args.json)
}

pub fn memory_verify(args: &super::memory::verify::MemoryVerify) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::VerifyAttestationRequest {
        repository: Some(session.reference.clone()),
        target: Some(VerifyTarget::Subject(pb::AttestationTarget {
            kind: Some(TargetKind::MemoryId(args.id.clone())),
        })),
        view: None,
        identity_name: args.identity.clone(),
    };
    let response = session.verify_attestation(request)?;
    if !response.valid {
        return Err(CliError::InvalidArgument {
            message: response
                .reason
                .unwrap_or_else(|| "verification failed".into()),
        });
    }
    println!("Verified memory: {}", args.id);
    println!("  author: {}", response.reason.unwrap_or_default());
    Ok(true)
}

/// `atomic memory list` over ListVaultEntries — every form. The handler
/// enumerates the canonical memories (no attestation entries, no index
/// scaffold, most-recent first, the budget truncation) and computes the
/// kind/status/about columns plus the attestation/verifies state with
/// the same domain logic the local body uses; `--identity` names the
/// verifying identity (the `verifies` column's resolver — soft-fail to
/// "no identity", hard error for a named-but-missing identity).
pub fn memory_list(args: &super::memory::list::MemoryList) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::ListVaultEntriesRequest {
        repository: Some(session.reference.clone()),
        kind: Some(Kind::Memory as i32),
        status: None,
        summaries_only: false,
        cursor: None,
        budget: args.limit.map(|limit| pb::PageBudget {
            max_items: Some(limit as u32),
            max_bytes: None,
        }),
        view: None,
        identity: args.identity.clone(),
        path_prefix: None,
        entry_type: None,
        status_filter: None,
        tool_result_previews: false,
    };
    let response = session.list_vault_entries(request)?;
    let rows = response
        .entries
        .iter()
        .map(super::memory::list::wire_row)
        .collect::<Vec<_>>();
    super::memory::list::MemoryList::render(&rows, args.json);
    Ok(true)
}

/// `atomic memory show` over GetVaultEntry — every form. The wire
/// carries the lift inputs (the stored entry) plus the RAW attestation
/// sources, so the CLI runs its EXISTING bridge code unchanged over
/// wire-carried inputs and projects with the same render the local body
/// uses — byte-parity for the canonical JSON-LD projection and the
/// `atomic vault query search` over QueryGraph (KgSearch) — every form:
/// the wire's KGNode now carries the full domain fields (source,
/// metadata), so the JSON serializes the same shape the local body
/// emits; `--kind` rides the request (the handler filters the candidate
/// pool server-side, taking the limit after the filter — the local
/// post-filter order) and `--pool` names the candidate pool. The plain
/// render reads the same kind/id/summary columns.
pub fn query_search(args: &super::query::QueryNodes) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::QueryGraphRequest {
        repository: Some(session.reference.clone()),
        limit: args.limit as u32,
        query: Some(pb::query_graph_request::Query::KgSearch(
            pb::KgSearchQuery {
                query: args.query.clone(),
                kind: args.kind.clone(),
                pool: args.pool.map(|pool| pool as u32),
            },
        )),
        view: None,
    };
    let response = session.query_graph(request)?;
    let nodes = response
        .nodes
        .iter()
        .map(kg_node_domain)
        .collect::<Vec<_>>();
    if args.json {
        println!("{}", serde_json::to_string_pretty(&nodes).unwrap());
        return Ok(true);
    }
    if nodes.is_empty() {
        println!("No results.");
        return Ok(true);
    }
    for node in &nodes {
        let summary = node.summary.as_deref().unwrap_or("");
        let summary_display = if summary.is_empty() {
            String::new()
        } else {
            format!("  {}", super::query::truncate_display(summary, 60))
        };
        println!("  [{}] {}{}", node.kind, node.id, summary_display);
    }
    println!("\n{} result(s).", nodes.len());
    Ok(true)
}

/// freeform body.
pub fn memory_show(args: &super::memory::show::MemoryShow) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::GetVaultEntryRequest {
        repository: Some(session.reference.clone()),
        kind: Kind::Memory as i32,
        id: args.id.clone(),
        path: None,
        include_bundle: true,
        view: None,
    };
    let response = session.get_vault_entry(request)?;
    let Some(bundle) = response
        .entry_bundle
        .as_ref()
        .map(parse_entry_bundle)
        .transpose()?
    else {
        return Err(CliError::Internal(anyhow::anyhow!(
            "the service returned no entry bundle (include_bundle required)"
        )));
    };
    let inputs = crate::commands::memory::bridge::inputs_from_entry(&bundle.entry)?;
    let attestation =
        crate::commands::memory::bridge::classify_attestation(memory_sources(&bundle), &inputs);
    super::memory::show::project(&args.id, &inputs, &attestation, args.json)?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// vault sync + query
// ---------------------------------------------------------------------------

pub fn vault_sync() -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::SyncVaultRequest {
        repository: Some(session.reference.clone()),
        meta: request_meta(),
        view: None,
        expected_snapshot: None,
    };
    let response = session.sync_vault(request)?;
    if response.entities_synced == 0 {
        println!("Vault is up to date.");
    } else {
        println!("Synced {} vault file(s).", response.entities_synced);
    }
    Ok(true)
}

pub fn query_enrich(args: &super::query::QueryEnrich) -> CliResult<bool> {
    let session = match Service::open()? {
        Some(session) => session,
        None => return Ok(false),
    };
    // Per-change enrichment: each change reference resolves to its full
    // hash through the service layer's Log (a read), then rides the
    // request's repeated changes — the handler enriches exactly those.
    let changes = args
        .changes
        .iter()
        .map(|spec| {
            resolve_change_hash(&session, spec).map(|hash| pb::Hash {
                value: hash.0.to_vec(),
                algorithm: pb::HashAlgorithm::Blake3 as i32,
            })
        })
        .collect::<CliResult<Vec<_>>>()?;
    let request = pb::MaintainKnowledgeGraphRequest {
        repository: Some(session.reference.clone()),
        meta: request_meta(),
        action: pb::KnowledgeMaintainAction::Enrich as i32,
        scope: args.rebuild.then(|| "rebuild".to_string()),
        changes,
    };
    let response = session.maintain_knowledge_graph(request)?;
    if !args.changes.is_empty() {
        // The targeted form's report, as the local body prints it.
        println!("Enriched {} change(s).", response.processed);
    } else {
        println!(
            "Enriched knowledge graph: {} item(s) processed.",
            response.processed
        );
    }
    Ok(true)
}

/// `atomic vault context` over GetVaultContext — every form. The
/// gather+rank (seeds from query terms, `--intent`, `--files`; the
/// memory-body scan; neighbor bonuses; recency; the body budget) runs in
/// the handler; the CLI renders the SAME markdown/JSON contracts with
/// the SAME render functions the local body uses (one render, two data
/// `atomic vault context` over GetVaultContext — every form. The
/// gather+rank (seeds from query terms, `--intent`, `--files`; the
/// memory-body scan; neighbor bonuses; recency; the body budget) runs in
/// the handler; the CLI renders the SAME markdown/JSON contracts with
/// the SAME render functions the local body uses (one render, two data
/// sources).
pub fn vault_context(args: &super::vault::context::Context) -> CliResult<bool> {
    use super::vault::context::{
        render_candidates_json, render_candidates_md, render_json, render_md, wire_items,
    };
    let session = match Service::open()? {
        Some(session) => session,
        None => return Ok(false),
    };
    let as_json = args.json || args.format.eq_ignore_ascii_case("json");
    let request = pb::GetVaultContextRequest {
        repository: Some(session.reference.clone()),
        max_entries: Some(args.limit as u32),
        kinds: Vec::new(),
        query: args.query.clone(),
        intent: args.intent.clone(),
        files: args.files.clone(),
        budget_chars: Some(args.budget_chars as u32),
        include_body: Some(!args.candidates_only),
        view: None,
    };
    let response = session.get_vault_context(request)?;
    let items = wire_items(&response.context);

    if args.candidates_only {
        if as_json {
            println!("{}", render_candidates_json(&items, args, args.limit));
        } else {
            let md = render_candidates_md(&items);
            if !md.is_empty() {
                print!("{md}");
            }
        }
        return Ok(true);
    }

    let md = render_md(&items);
    if as_json {
        println!("{}", render_json(&items, &md, args, args.limit));
    } else if !md.is_empty() {
        print!("{md}");
    }
    Ok(true)
}

/// `atomic agent attest` over ListAttestations — every form. The
/// listing rides the view filter as before; `--hash <prefix>` resolves
/// ONE attestation server-side (the hash names an attestation, which the
/// change-oriented Log cannot resolve) and returns the domain payload
/// plus the per-view coverage as versioned-opaque bytes; `--summary` /
/// `--pending <parent>` return the AI provenance summary the same way.
/// The CLI renders with the SAME code the local bodies run.
pub fn agent_attest(args: &super::agent::attest::Attest) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    // The provenance-summary modes: `--pending <parent>` requires --view
    // (validated client-side, the local body's exact error).
    if args.summary || args.pending.is_some() {
        if args.pending.is_some() && args.view.is_none() {
            return Err(CliError::InvalidArgument {
                message:
                    "--pending <parent_view> requires --view <agent_view> (the forked view to summarize)"
                        .to_string(),
            });
        }
        // The summary's view defaults to the current view — resolved
        // client-side from the working-copy marker (the same default the
        // local body applies).
        let summary_view = args
            .view
            .clone()
            .unwrap_or_else(|| current_view_name().unwrap_or_default());
        let request = pb::ListAttestationsRequest {
            repository: Some(session.reference.clone()),
            filter: None,
            cursor: None,
            budget: None,
            view: None,
            hash_prefix: None,
            summary_view: Some(summary_view),
            pending_parent: args.pending.clone(),
        };
        let response = session.list_attestations(request)?;
        let Some(bundle) = response.summary_bundle.as_ref() else {
            return Err(CliError::Internal(anyhow::anyhow!(
                "the service returned no summary bundle (summary_view required)"
            )));
        };
        if bundle.schema != "atomic.provenance.summary.v1" {
            return Err(CliError::Internal(anyhow::anyhow!(
                "unknown summary bundle schema '{}'",
                bundle.schema
            )));
        }
        #[derive(serde::Deserialize)]
        struct SummaryBundle {
            summary: atomic_repository::ProvenanceSummary,
            parent: Option<String>,
        }
        let parsed: SummaryBundle = serde_json::from_slice(&bundle.payload)
            .map_err(|error| CliError::Internal(anyhow::anyhow!("summary bundle: {error}")))?;
        super::agent::attest::Attest::render_summary(
            &parsed.summary,
            &args
                .view
                .clone()
                .unwrap_or_else(|| current_view_name().unwrap_or_default()),
            args.pending.as_deref(),
            parsed.parent.as_deref(),
            args.verbose,
        );
        return Ok(true);
    }
    // The `--hash <prefix>` detail: handler-side resolution (exact match
    // first, then the prefix scan), the domain payload + per-view
    // coverage over the wire.
    if let Some(prefix) = args.hash.clone() {
        let request = pb::ListAttestationsRequest {
            repository: Some(session.reference.clone()),
            filter: None,
            cursor: None,
            budget: None,
            view: None,
            hash_prefix: Some(prefix),
            summary_view: None,
            pending_parent: None,
        };
        let response = session.list_attestations(request)?;
        let Some(bundle) = response.detail_bundle.as_ref() else {
            return Err(CliError::Internal(anyhow::anyhow!(
                "the service returned no detail bundle (hash_prefix required)"
            )));
        };
        if bundle.schema != "atomic.attestation.detail.v1" {
            return Err(CliError::Internal(anyhow::anyhow!(
                "unknown detail bundle schema '{}'",
                bundle.schema
            )));
        }
        #[derive(serde::Deserialize)]
        struct DetailBundle {
            hash: String,
            attestation: atomic_core::change::attestation::Attestation,
            coverage: Vec<super::agent::attest::CoverageRowWire>,
        }
        let parsed: DetailBundle = serde_json::from_slice(&bundle.payload)
            .map_err(|error| CliError::Internal(anyhow::anyhow!("detail bundle: {error}")))?;
        let hash = atomic_core::types::Hash::from_base32(parsed.hash.as_bytes())
            .ok_or_else(|| CliError::Internal(anyhow::anyhow!("malformed attestation hash")))?;
        let coverage = parsed
            .coverage
            .iter()
            .map(|row| super::agent::attest::CoverageRow {
                view: row.view.clone(),
                covered: row.covered,
                total: row.total,
            })
            .collect::<Vec<_>>();
        super::agent::attest::Attest::render_detail(&hash, &parsed.attestation, &coverage);
        return Ok(true);
    }
    // The listing: attestations covering the view's changes.
    let request = pb::ListAttestationsRequest {
        repository: Some(session.reference.clone()),
        filter: Some(pb::AttestationTarget {
            kind: Some(TargetKind::GraphStats(pb::GraphStatsRef {
                session_id: args.view.clone(),
                turn_number: None,
                change_hash: None,
            })),
        }),
        cursor: None,
        budget: None,
        view: None,
        hash_prefix: None,
        summary_view: None,
        pending_parent: None,
    };
    let response = session.list_attestations(request)?;
    if response.attestations.is_empty() {
        println!("No attestations cover changes in this view.");
        return Ok(true);
    }
    println!(
        "{} attestation(s) covering changes in this view",
        response.attestations.len()
    );
    for attestation in &response.attestations {
        println!("  {}", attestation.id);
    }
    Ok(true)
}

/// `atomic vault list` over ListVaultEntries — every form. kind absent
/// is the WHOLE-VAULT listing: every entry with its type, content size,
/// and updated-at date, filtered by the request's path prefix and entry
/// type; the CLI renders with the same table/JSON code the local body
/// uses.
pub fn vault_list(args: &super::vault::list::List) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    // The local parse: an unknown --type value parses to no filter (the
    // local body lists everything), so only a PARSED type rides the wire.
    let type_filter = args
        .r#type
        .as_deref()
        .and_then(atomic_core::pristine::VaultEntryType::parse);
    let request = pb::ListVaultEntriesRequest {
        repository: Some(session.reference.clone()),
        kind: None,
        status: None,
        summaries_only: true,
        cursor: None,
        budget: None,
        view: None,
        identity: None,
        path_prefix: args.prefix.clone(),
        entry_type: type_filter.map(|kind| kind.to_string()),
        status_filter: None,
        tool_result_previews: false,
    };
    let response = session.list_vault_entries(request)?;
    let rows = response
        .entries
        .iter()
        .map(|entry| super::vault::list::EntryRow {
            path: entry.vault_path.clone().unwrap_or_default(),
            entry_type: entry.entry_type.clone().unwrap_or_default(),
            size: entry.content_size.unwrap_or(0),
            updated_at: entry.updated_at_label.clone().unwrap_or_default(),
        })
        .collect::<Vec<_>>();
    super::vault::list::render_rows(&rows, args.json);
    Ok(true)
}

/// `atomic vault show <path>` over GetVaultEntry — every form. The
/// wire resolves by vault-relative path (any entry type) and carries
/// the stored entry's full domain bytes, so the revision hash, the JSON
/// projection (entry type, frontmatter, created/updated dates), and the
/// `--revision` guard all run client-side over wire-carried data with
/// the SAME functions the local body uses.
pub fn vault_show(args: &super::vault::show::Show) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    // The raw argument names the entry exactly as the local body's
    // vault_retrieve does (vault-relative; a `.vault/`-prefixed path
    // fails the same lookup both ways).
    let path = args.path.clone();
    let request = pb::GetVaultEntryRequest {
        repository: Some(session.reference.clone()),
        kind: Kind::Unspecified as i32,
        id: String::new(),
        path: Some(path.clone()),
        include_bundle: true,
        view: None,
    };
    let response = session.get_vault_entry(request)?;
    let Some(bundle) = response
        .entry_bundle
        .as_ref()
        .map(parse_entry_bundle)
        .transpose()?
    else {
        return Err(CliError::Internal(anyhow::anyhow!(
            "the service returned no entry bundle (include_bundle required)"
        )));
    };
    let entry = &bundle.entry;
    let revision_hash = (args.json || args.revision.is_some())
        .then(|| super::vault::vault_entry_revision_hash(entry));
    if let Some(actual) = revision_hash.as_deref() {
        super::vault::show::require_revision(&path, args.revision.as_deref(), actual)?;
    }
    if args.json {
        let json = super::vault::show::entry_json(
            &path,
            entry,
            revision_hash
                .as_deref()
                .expect("JSON output always computes a revision"),
        );
        println!("{}", serde_json::to_string_pretty(&json).unwrap());
    } else {
        print!("{}", String::from_utf8_lossy(&entry.content_bytes));
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// diff / change / session / view list / goals / neighbors
// ---------------------------------------------------------------------------

/// `atomic diff` over DiffWorkingCopy / Diff / GetChange. The working-copy
/// and view-pair forms render the wire's DiffChunk patches client-side —
/// including `--json` (the versioned document shape the local body emits)
/// and `--word-diff` (the same pairing + token-level highlighting the local
/// unified renderer applies, over the parsed patch lines). `-c <id>` loads
/// the change bundle over GetChange and runs the local render flow over
/// the deserialized domain Change. `--cached` is a documented no-op
/// ("reserved for future use") with no local behavior and no wire shape,
/// so it keeps the local body.
pub fn diff(args: &super::diff::command::Diff) -> CliResult<bool> {
    // The view-pair form (--from/--to) is the Diff RPC's RefPairScope arm.
    if let (Some(from), Some(to)) = (&args.from, &args.to) {
        return diff_view_pair(args, from, to);
    }
    // The `-c <id>` form: a recorded change's content, over GetChange.
    if let Some(change_ref) = args.change.clone() {
        return diff_change(args, &change_ref);
    }
    if args.cached {
        return Ok(false); // reserved for future use: a no-op flag needs no wire
    }
    let session = match Service::open()? {
        Some(session) => session,
        None => return Ok(false),
    };
    let (stat_only, name_only, include_untracked, files) = (
        args.stat,
        args.name_only,
        args.untracked,
        args.files.clone(),
    );
    let response = session.diff_working_copy(pb::DiffWorkingCopyRequest {
        repository: Some(session.reference.clone()),
        word_diff: args.word_diff,
        working_copy: Some(pb::WorkingCopyScope {
            include_untracked,
            paths: files,
        }),
        stat_only,
    })?;

    // One render, two data sources: rebuild the FileDiffs the local path
    // would have built from the wire's status + contents, then hand them to
    // the SAME formatters (print_unified/print_stat/... — including the
    // versioned JSON document and word-diff pairing). The bridge grows no
    // rendering dialect of its own.
    if response.is_empty() {
        let repo_root = crate::commands::find_repository_root()?;
        let repo = atomic_repository::Repository::open(&repo_root).map_err(CliError::Repository)?;
        let view = current_view_name()?;
        args.print_no_pending_changes(&repo, &view);
        return Ok(true);
    }

    let algorithm = args.parse_algorithm()?;
    let config = args.get_output_config();
    let view = current_view_name()?;

    let mut file_diffs = Vec::new();
    let mut stats = super::diff::types::DiffStats::new();
    for chunk in &response {
        if let Some(file_diff) = file_diff_from_chunk(chunk, &algorithm, config.context_lines) {
            stats.add_file(file_diff.stats.clone());
            file_diffs.push(file_diff);
        }
    }

    args.render(&file_diffs, &stats, &config, None, Some(view.as_str()))?;
    Ok(true)
}

/// Rebuild one local-path `FileDiff` from a wire `DiffChunk`.
///
/// The chunk carries the file's status plus its recorded and working-copy
/// contents (see DiffChunk.status/old_content/new_content), so the exact
/// comparison the local path performs — `diff_text` + hunks with context —
/// runs here and feeds the shared formatters.
fn file_diff_from_chunk(
    chunk: &pb::DiffChunk,
    algorithm: &atomic_core::diff::Algorithm,
    context_lines: usize,
) -> Option<super::diff::types::FileDiff> {
    use super::diff::types::{FileChangeStatus, FileDiff};
    use atomic_core::diff::diff_text;

    let status = match chunk.status.as_deref() {
        Some("added") => FileChangeStatus::Added,
        Some("deleted") => FileChangeStatus::Deleted,
        Some("untracked") => FileChangeStatus::Untracked,
        _ => FileChangeStatus::Modified,
    };

    let mut file_diff = match status {
        FileChangeStatus::Added => FileDiff::added(&chunk.path),
        FileChangeStatus::Deleted => FileDiff::deleted(&chunk.path),
        FileChangeStatus::Untracked => FileDiff::new(&chunk.path, FileChangeStatus::Untracked),
        _ => FileDiff::modified(&chunk.path),
    };

    if chunk.binary {
        file_diff.is_binary = true;
        file_diff.stats.status = status.status_char();
        return Some(file_diff);
    }

    let old_content = chunk.old_content.clone().unwrap_or_default();
    let new_content = chunk.new_content.clone().unwrap_or_default();

    // stat-only: the wire carries the counts but not the contents (the
    // server omits them) — a stats-only entry renders `--stat` fine.
    if chunk.patch.is_none() {
        file_diff.stats.insertions = chunk.additions as usize;
        file_diff.stats.deletions = chunk.deletions as usize;
        return Some(file_diff);
    }

    let diff_result = diff_text(&old_content, &new_content, *algorithm);
    if diff_result.is_unchanged() {
        return None;
    }
    let old_lines: Vec<_> = old_content.split(|&b| b == b'\n').collect();
    let new_lines: Vec<_> = new_content.split(|&b| b == b'\n').collect();
    for hunk in
        super::diff::build_hunks_from_diff(&diff_result, &old_lines, &new_lines, context_lines)
    {
        file_diff.add_hunk(hunk);
    }
    file_diff.compute_stats();
    Some(file_diff)
}

/// `atomic change` over GetChange (includes.metadata: the versioned
/// change bundle + the provenance ledger). The full-detail renders — the
/// Change Ledger, hunks, JSON — run client-side over the wire data with
/// the SAME formatters the local path uses: one render, two data sources.
pub fn change(args: &super::change::command::ChangeCmd) -> CliResult<bool> {
    use super::change::command::ChangeRenderData;
    use super::change::types::ChangeIdentifier;
    let Some(session) = Service::open()? else {
        return Ok(false);
    };

    // Resolve the identifier: None = "latest" (the view's newest change,
    // via Log); a full hash or a view sequence ride the ChangeRef; a
    // prefix rides the wire's prefix arm (whole-store resolution).
    let reference = match args.identifier.as_deref() {
        None => {
            let entries = session.log(pb::LogRequest {
                repository: Some(session.reference.clone()),
                all_views: false,
                view: args.view.clone(),
                cursor: None,
                budget: Some(pb::PageBudget {
                    max_items: Some(1),
                    max_bytes: None,
                }),
                path_filter: Vec::new(),
                tags_only: false,
            })?;
            let Some(entry) = entries.entries.first() else {
                return Err(CliError::ChangeNotFound {
                    hash: "latest".to_string(),
                });
            };
            let hash = entry
                .hash
                .clone()
                .ok_or_else(|| CliError::Internal(anyhow::anyhow!("log entry carries no hash")))?;
            pb::ChangeRef {
                kind: Some(pb::change_ref::Kind::Hash(hash)),
            }
        }
        Some(identifier) => {
            let parsed = ChangeIdentifier::parse(Some(identifier))
                .map_err(|error| CliError::InvalidArgument { message: error })?;
            match parsed {
                ChangeIdentifier::FullHash(hash) => {
                    let mut bytes = [0u8; 32];
                    bytes.copy_from_slice(&hash.0);
                    pb::ChangeRef {
                        kind: Some(pb::change_ref::Kind::Hash(pb::Hash {
                            value: bytes.to_vec(),
                            algorithm: pb::HashAlgorithm::Blake3 as i32,
                        })),
                    }
                }
                ChangeIdentifier::Sequence(sequence) => pb::ChangeRef {
                    kind: Some(pb::change_ref::Kind::Sequence(sequence)),
                },
                _ => pb::ChangeRef {
                    kind: Some(pb::change_ref::Kind::Prefix(identifier.to_string())),
                },
            }
        }
    };

    let response = session.get_change(pb::GetChangeRequest {
        repository: Some(session.reference.clone()),
        r#ref: Some(reference),
        includes: Some(pb::ChangeIncludes {
            metadata: true,
            ..Default::default()
        }),
        view: args.view.as_ref().map(|view| pb::ViewRef {
            view_id: Vec::new(),
            name: Some(view.clone()),
        }),
        include_file_contents: false,
    })?;

    // The versioned change bundle: V3 bytes → the domain Change.
    let Some(bundle) = response.change_bundle else {
        return Err(CliError::Internal(anyhow::anyhow!(
            "the service returned no change bundle (metadata includes required)"
        )));
    };
    let mut payload = bundle.payload.as_slice();
    let (change, _hash) = atomic_core::change::Change::deserialize(&mut payload)
        .map_err(|error| CliError::Internal(anyhow::anyhow!("change bundle: {error}")))?;
    let Some(info) = response.change else {
        return Err(CliError::Internal(anyhow::anyhow!("change not found")));
    };
    let hash = info
        .hash
        .and_then(|h| h.value.clone().try_into().ok())
        .map(atomic_core::types::Merkle)
        .ok_or_else(|| CliError::Internal(anyhow::anyhow!("malformed hash")))?;

    let ledger_graphs = decode_change_ledger(
        &response.provenance_ledger,
        &response.provenance_ledger_hashes,
    )?;

    // Dependency messages for --show-deps: per-dep metadata lookups.
    let mut dep_messages = std::collections::HashMap::new();
    if args.show_deps {
        for dep in &info.dependencies {
            let value = dep.value.clone();
            let Ok(bytes) = TryInto::<[u8; 32]>::try_into(value) else {
                continue;
            };
            let result = session.get_change(pb::GetChangeRequest {
                repository: Some(session.reference.clone()),
                r#ref: Some(pb::ChangeRef {
                    kind: Some(pb::change_ref::Kind::Hash(pb::Hash {
                        value: bytes.to_vec(),
                        algorithm: pb::HashAlgorithm::Blake3 as i32,
                    })),
                }),
                includes: None,
                view: None,
                include_file_contents: false,
            });
            if let Ok(dep_response) = result {
                if let Some(dep_info) = dep_response.change {
                    if let Some(message) = dep_info.message {
                        dep_messages.insert(
                            atomic_core::types::Merkle(bytes),
                            message.lines().next().unwrap_or("").to_string(),
                        );
                    }
                }
            }
        }
    }

    let data = ChangeRenderData {
        ledger_graphs,
        dep_messages,
    };
    args.print_change(&change, &hash, response.sequence, &data);
    Ok(true)
}

fn decode_change_ledger(
    ledger: &[pb::VersionedBytes],
    hashes: &[pb::Hash],
) -> CliResult<Vec<(Merkle, atomic_core::change::ProvenanceGraph)>> {
    if ledger.len() != hashes.len() {
        return Err(CliError::Internal(anyhow::anyhow!(
            "provenance ledger/hash count mismatch ({} graphs, {} hashes); \
             if using an older service, upgrade it to return stored provenance hashes",
            ledger.len(),
            hashes.len(),
        )));
    }
    ledger
        .iter()
        .zip(hashes)
        .map(|(bytes, hash)| {
            if hash.algorithm != pb::HashAlgorithm::Blake3 as i32 {
                return Err(CliError::Internal(anyhow::anyhow!(
                    "unsupported provenance hash algorithm: {}",
                    hash.algorithm,
                )));
            }
            let value: [u8; 32] = hash.value.as_slice().try_into().map_err(|_| {
                CliError::Internal(anyhow::anyhow!(
                    "malformed provenance hash: expected 32 bytes"
                ))
            })?;
            if bytes.schema != "atomic.prov.graph.v1" {
                return Err(CliError::Internal(anyhow::anyhow!(
                    "unsupported provenance graph schema: {}",
                    bytes.schema,
                )));
            }
            let graph = serde_json::from_slice(&bytes.payload).map_err(|error| {
                CliError::Internal(anyhow::anyhow!("provenance graph payload: {error}"))
            })?;
            // Use the stored identity, not a hash of the JSON or of an upgraded
            // graph reserialized by this client. Neither is the original artifact.
            Ok((Merkle(value), graph))
        })
        .collect()
}

/// `atomic diff --from X --to Y` over the Diff RPC's RefPairScope arm —
/// the same client-side renders as the working-copy form (`--json`, the
/// word-diff highlight, the plain patch text).
fn diff_view_pair(args: &super::diff::command::Diff, from: &str, to: &str) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let stat_only = args.stat;
    let from = from.to_string();
    let to = to.to_string();
    let chunks = session.diff(pb::DiffRequest {
        repository: Some(session.reference.clone()),
        word_diff: args.word_diff,
        scope: Some(pb::diff_request::Scope::Refs(pb::RefPairScope {
            from_view: from,
            to_view: to,
        })),
        stat_only,
    })?;

    if args.json {
        // A view-pair diff is not scoped against a single view — the
        // document's `view` field stays null (and `change` too: no `-c`).
        print_diff_json(&chunks, None)?;
        return Ok(true);
    }
    if chunks.is_empty() {
        println!("No differences between the views.");
        return Ok(true);
    }

    // Same shared-formatter render as the working-copy form: the wire's
    // status + contents rebuild the FileDiffs the local path would build.
    let algorithm = args.parse_algorithm()?;
    let config = args.get_output_config();
    let mut file_diffs = Vec::new();
    let mut stats = super::diff::types::DiffStats::new();
    for chunk in &chunks {
        if let Some(file_diff) = file_diff_from_chunk(chunk, &algorithm, config.context_lines) {
            stats.add_file(file_diff.stats.clone());
            file_diffs.push(file_diff);
        }
    }
    args.render(&file_diffs, &stats, &config, None, None)?;
    Ok(true)
}

/// `atomic diff -c <id>` over GetChange — every form: the change
/// resolves (full hash, `#N` sequence, or prefix — the same ChangeRef
/// arms `atomic change` uses), the versioned change bundle deserializes
/// into the domain Change, and the SAME render flow the local `-c` body
/// runs prints it. The request's file-content reconstruction carries
/// each touched file's pre/post-change bytes: the FileOps hunks pad
/// their context from the true before-content (no degradation), and
/// legacy no-file_ops changes build their diffs from the reconstructed
/// content — the exception is gone.
fn diff_change(args: &super::diff::command::Diff, change_ref: &str) -> CliResult<bool> {
    use super::change::types::ChangeIdentifier;
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let parsed = ChangeIdentifier::parse(Some(change_ref))
        .map_err(|error| CliError::InvalidArgument { message: error })?;
    let reference = match parsed {
        ChangeIdentifier::FullHash(hash) => {
            let mut bytes = [0u8; 32];
            bytes.copy_from_slice(&hash.0);
            pb::ChangeRef {
                kind: Some(pb::change_ref::Kind::Hash(pb::Hash {
                    value: bytes.to_vec(),
                    algorithm: pb::HashAlgorithm::Blake3 as i32,
                })),
            }
        }
        ChangeIdentifier::Sequence(sequence) => pb::ChangeRef {
            kind: Some(pb::change_ref::Kind::Sequence(sequence)),
        },
        // "Latest" (`@`/empty) resolves through the daemon's Log like the
        // `change` hook does: the view's newest change.
        ChangeIdentifier::Latest => {
            let entries = session.log(pb::LogRequest {
                repository: Some(session.reference.clone()),
                all_views: false,
                view: args.view.clone(),
                cursor: None,
                budget: Some(pb::PageBudget {
                    max_items: Some(1),
                    max_bytes: None,
                }),
                path_filter: Vec::new(),
                tags_only: false,
            })?;
            let Some(entry) = entries.entries.first() else {
                return Err(CliError::ChangeNotFound {
                    hash: "latest".to_string(),
                });
            };
            let hash = entry
                .hash
                .clone()
                .ok_or_else(|| CliError::Internal(anyhow::anyhow!("log entry carries no hash")))?;
            pb::ChangeRef {
                kind: Some(pb::change_ref::Kind::Hash(hash)),
            }
        }
        _ => pb::ChangeRef {
            kind: Some(pb::change_ref::Kind::Prefix(change_ref.to_string())),
        },
    };

    let response = session.get_change(pb::GetChangeRequest {
        repository: Some(session.reference.clone()),
        r#ref: Some(reference),
        includes: Some(pb::ChangeIncludes {
            metadata: true,
            ..Default::default()
        }),
        view: args.view.as_ref().map(|view| pb::ViewRef {
            view_id: Vec::new(),
            name: Some(view.clone()),
        }),
        include_file_contents: true,
    })?;
    let Some(bundle) = response.change_bundle else {
        return Err(CliError::Internal(anyhow::anyhow!(
            "the service returned no change bundle (metadata includes required)"
        )));
    };
    let mut payload = bundle.payload.as_slice();
    let (change, _hash) = atomic_core::change::Change::deserialize(&mut payload)
        .map_err(|error| CliError::Internal(anyhow::anyhow!("change bundle: {error}")))?;
    let Some(info) = response.change else {
        return Err(CliError::Internal(anyhow::anyhow!("change not found")));
    };
    let hash = info
        .hash
        .and_then(|h| h.value.clone().try_into().ok())
        .map(atomic_core::types::Merkle)
        .ok_or_else(|| CliError::Internal(anyhow::anyhow!("malformed hash")))?;

    // The wire's per-file content reconstruction: before (the context
    // padding's source) and after (the legacy fallback's diff input).
    let mut before_content = std::collections::HashMap::new();
    let mut after_content = std::collections::HashMap::new();
    for file in &response.file_contents {
        if let Some(before) = &file.before {
            before_content.insert(file.path.clone(), before.clone());
        }
        if let Some(after) = &file.after {
            after_content.insert(file.path.clone(), after.clone());
        }
    }

    let config = args.get_output_config();
    let (file_diffs, stats) = if let Some((file_diffs, stats)) =
        super::diff::Diff::build_git_import_file_diffs(&change)
    {
        // Git-imported changes carry Git's captured +/- lines in their
        // unhashed metadata — a pure render over the deserialized
        // change, no content reads.
        (file_diffs, stats)
    } else if change.has_file_ops() {
        // The FileOps hunks pad their context from the wire-carried
        // before-content — the same context the local path reads from
        // the graph.
        super::diff::change_file_diffs_with(&change, &config, |path| {
            before_content.get(path).cloned()
        })?
    } else {
        // Legacy changes carry no file_ops: the diff computes from the
        // reconstructed before/after content — the SAME builder the
        // local body runs over graph-read content.
        use atomic_repository::get_files_in_change;
        let modified_files: Vec<String> = get_files_in_change(&change)
            .into_iter()
            .filter(|path| args.file_matches_filter(path))
            .collect();
        let entries = modified_files
            .iter()
            .map(|path| {
                (
                    path.clone(),
                    before_content.get(path).cloned().unwrap_or_default(),
                    after_content.get(path).cloned().unwrap_or_default(),
                )
            })
            .collect::<Vec<_>>();
        super::diff::legacy_content_file_diffs(
            &entries,
            args.parse_algorithm()?,
            config.context_lines,
        )
    };
    let (file_diffs, stats) = args.filter_file_diffs(file_diffs, stats);
    args.print_change_file_diffs(&change, &hash, &config, file_diffs, stats)?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// wire diff rendering (DiffChunk → text/JSON/word-diff)
// ---------------------------------------------------------------------------

/// One hunk parsed from a wire DiffChunk's unified patch text.
struct WireHunk {
    old_start: usize,
    old_count: usize,
    new_start: usize,
    new_count: usize,
    lines: Vec<(char, String, Option<usize>, Option<usize>)>,
}

/// Parse a unified-diff patch into hunks. Each line is
/// `(marker, content, old_line, new_line)` with the markers ' ', '-', '+';
/// line numbers follow the unified header's running counters, exactly as
/// the local renderer numbers its computed hunks.
fn parse_patch_hunks(patch: &str) -> Vec<WireHunk> {
    let mut hunks = Vec::new();
    let mut current: Option<WireHunk> = None;
    let mut old_counter = 0usize;
    let mut new_counter = 0usize;
    for line in patch.lines() {
        if let Some(header) = line.strip_prefix("@@") {
            // "@@ -a,b +c,d @@ ..." — the ranges ride before the closing @@.
            let body = header.split("@@").next().unwrap_or("");
            let mut parts = body.split_whitespace();
            let parse = |spec: &str| -> (usize, usize) {
                let stripped = spec
                    .strip_prefix('-')
                    .or_else(|| spec.strip_prefix('+'))
                    .unwrap_or(spec);
                let (start, count) = stripped.split_once(',').unwrap_or((stripped, "1"));
                (start.parse().unwrap_or(0), count.parse().unwrap_or(1))
            };
            let (old_start, old_count) = parse(parts.next().unwrap_or("-0,0"));
            let (new_start, new_count) = parse(parts.next().unwrap_or("+0,0"));
            old_counter = old_start;
            new_counter = new_start;
            if let Some(previous) = current.take() {
                hunks.push(previous);
            }
            current = Some(WireHunk {
                old_start,
                old_count,
                new_start,
                new_count,
                lines: Vec::new(),
            });
            continue;
        }
        if line.starts_with("---") || line.starts_with("+++") || line.starts_with("diff ") {
            continue; // defensive: file headers never carry change lines
        }
        let Some(hunk) = current.as_mut() else {
            continue; // preamble before the first @@ header
        };
        let marker = line.chars().next().unwrap_or(' ');
        let content = line.get(1..).unwrap_or_default().to_string();
        match marker {
            '-' => {
                hunk.lines.push(('-', content, Some(old_counter), None));
                old_counter = old_counter.saturating_add(1);
            }
            '+' => {
                hunk.lines.push(('+', content, None, Some(new_counter)));
                new_counter = new_counter.saturating_add(1);
            }
            _ => {
                hunk.lines
                    .push((' ', content, Some(old_counter), Some(new_counter)));
                old_counter = old_counter.saturating_add(1);
                new_counter = new_counter.saturating_add(1);
            }
        }
    }
    if let Some(last) = current.take() {
        hunks.push(last);
    }
    hunks
}

/// Print a wire patch with word-level highlighting — the same pairing and
/// token-level highlighting the local unified renderer (`diff/format.rs`)
/// applies to its computed hunks: consecutive removed/added lines pair up
/// and diff at token level (the semantic engine first, the inline engine
/// as fallback); unpaired and context lines render plain.
fn print_word_diff_patch(patch: &str) {
    use crate::output::{added, deleted};
    use atomic_core::diff::compute_inline_diff;
    use atomic_core::diff::{semantic_diff, LineChange};

    for hunk in parse_patch_hunks(patch) {
        println!(
            "@@ -{},{} +{},{} @@",
            hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
        );
        let lines = hunk.lines;
        let mut index = 0;
        while index < lines.len() {
            if lines[index].0 != '-' {
                let (marker, content, ..) = &lines[index];
                match marker {
                    '+' => println!("{}", added(&format!("{marker}{content}"))),
                    _ => println!("{marker}{content}"),
                }
                index += 1;
                continue;
            }
            // Collect the consecutive removed block, then the added block
            // that follows — the local renderer's pairing window.
            let removed: Vec<&(char, String, Option<usize>, Option<usize>)> = lines[index..]
                .iter()
                .take_while(|line| line.0 == '-')
                .collect();
            let mut cursor = index + removed.len();
            let added_lines: Vec<&(char, String, Option<usize>, Option<usize>)> = lines[cursor..]
                .iter()
                .take_while(|line| line.0 == '+')
                .collect();
            cursor += added_lines.len();
            if added_lines.is_empty() {
                for (marker, content, ..) in &removed {
                    println!("{}", deleted(&format!("{marker}{content}")));
                }
                index += removed.len();
                continue;
            }
            let pairs = removed.len().min(added_lines.len());
            for pair in 0..pairs {
                let (removed_line, added_line) =
                    (removed[pair].1.as_str(), added_lines[pair].1.as_str());
                let old_content = removed_line.as_bytes();
                let new_content = added_line.as_bytes();
                // Semantic token diff first; the inline diff is the fallback.
                let sem_diff = semantic_diff(old_content, new_content);
                let mut used_semantic = false;
                #[allow(clippy::collapsible_match)]
                if let Some(change) = sem_diff.changes().first() {
                    if let LineChange::Modified { token_changes, .. } = change {
                        print!("{}", deleted(&format!("-{removed_line}")));
                        super::diff::print_semantic_word_diff_line(token_changes, true);
                        println!();
                        print!("{}", added(&format!("+{added_line}")));
                        super::diff::print_semantic_word_diff_line(token_changes, false);
                        println!();
                        used_semantic = true;
                    }
                }
                if !used_semantic {
                    let inline_diff = compute_inline_diff(old_content, new_content);
                    print!("{}", deleted(&format!("-{removed_line}")));
                    super::diff::print_word_diff_line(old_content, inline_diff.old_hunks(), true);
                    println!();
                    print!("{}", added(&format!("+{added_line}")));
                    super::diff::print_word_diff_line(new_content, inline_diff.new_hunks(), false);
                    println!();
                }
            }
            for (marker, content, ..) in removed.iter().skip(pairs) {
                println!("{}", deleted(&format!("{marker}{content}")));
            }
            for (marker, content, ..) in added_lines.iter().skip(pairs) {
                println!("{}", added(&format!("{marker}{content}")));
            }
            index = cursor;
        }
    }
}

/// Render the wire's DiffChunks as the local `diff --json` document — the
/// same versioned shape (`schema_version` 1, `view`, null `change`, per-file
/// hunks/lines with true numbers, aggregate stats), built from the chunk
/// fields. Per-file status derives from the hunks' old/new counts (a
/// pure insertion reads `added`, a pure deletion `deleted`, else
/// `modified`), the same classification the local FileDiff carries.
fn print_diff_json(chunks: &[pb::DiffChunk], view: Option<&str>) -> CliResult<()> {
    let mut files = Vec::new();
    let mut insertions = 0usize;
    let mut deletions = 0usize;
    for chunk in chunks {
        insertions += chunk.additions as usize;
        deletions += chunk.deletions as usize;
        let hunks = parse_patch_hunks(&String::from_utf8_lossy(
            chunk.patch.as_deref().unwrap_or(&[]),
        ));
        let old_side_empty = hunks.iter().all(|hunk| hunk.old_count == 0);
        let new_side_empty = hunks.iter().all(|hunk| hunk.new_count == 0);
        let (status, code, old_path, new_path) = if chunk.binary {
            ("binary", 'B', chunk.path.clone(), chunk.path.clone())
        } else if old_side_empty && !new_side_empty {
            ("added", 'A', "/dev/null".to_string(), chunk.path.clone())
        } else if new_side_empty && !old_side_empty {
            ("deleted", 'D', chunk.path.clone(), "/dev/null".to_string())
        } else {
            ("modified", 'M', chunk.path.clone(), chunk.path.clone())
        };
        let json_hunks: Vec<serde_json::Value> = hunks
            .iter()
            .map(|hunk| {
                serde_json::json!({
                    "old_start": hunk.old_start,
                    "old_count": hunk.old_count,
                    "new_start": hunk.new_start,
                    "new_count": hunk.new_count,
                    "lines": hunk.lines.iter().map(|(marker, content, old_line, new_line)| {
                        serde_json::json!({
                            "status": match marker {
                                '-' => "removed",
                                '+' => "added",
                                _ => "context",
                            },
                            "content": content,
                            "old_line": old_line,
                            "new_line": new_line,
                        })
                    }).collect::<Vec<_>>(),
                })
            })
            .collect();
        files.push(serde_json::json!({
            "path": chunk.path,
            "old_path": old_path,
            "new_path": new_path,
            "status": status,
            "code": code,
            "binary": chunk.binary,
            "insertions": chunk.additions,
            "deletions": chunk.deletions,
            "hunks": json_hunks,
        }));
    }
    let document = serde_json::json!({
        "schema_version": 1,
        "view": view,
        "change": serde_json::Value::Null,
        "files": files,
        "stats": {
            "files": chunks.len(),
            "insertions": insertions,
            "deletions": deletions,
        },
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&document).map_err(|e| CliError::Internal(e.into()))?
    );
    Ok(())
}

/// `atomic session show` over the session RPCs. The listing rides
/// ListSessions (with its ledgers bundle + vault-derived intent counts);
/// the detail rides GetSession (ledger bundle, turn→intent resolution,
/// and the session manifest data). Both renders are the SAME functions
/// the local path uses — the wire carries the domain data.
pub fn session_show(args: &super::session::SessionShow) -> CliResult<bool> {
    use atomic_core::change::session::{SessionRecord, SessionTurn};

    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let target = args.session_id.clone();
    if target.is_none() {
        // The recent-session listing.
        let response = session.list_sessions(pb::ListSessionsRequest {
            repository: Some(session.reference.clone()),
            limit: args.limit as u32,
        })?;
        let Some(bundle) = response.ledgers_bundle else {
            // The same-build service always carries the bundle; a missing
            // one is a protocol violation, not a fallback path.
            return Err(CliError::Internal(anyhow::anyhow!(
                "the service returned no ledgers bundle"
            )));
        };
        if args.json {
            // The bundle IS the domain serialization of the ledgers —
            // print it verbatim (byte-parity with the local JSON).
            print!("{}", String::from_utf8_lossy(&bundle.payload));
            return Ok(true);
        }
        let ledgers: Vec<(SessionRecord, Vec<SessionTurn>)> =
            serde_json::from_slice(&bundle.payload)
                .map_err(|error| CliError::Internal(anyhow::anyhow!("ledgers bundle: {error}")))?;
        let intent_counts = response
            .intent_counts
            .into_iter()
            .map(|count| count as usize)
            .collect();
        super::session::render_recent_sessions(ledgers, intent_counts, false)?;
        return Ok(true);
    }
    let session_id = target.expect("checked");
    let response = session.get_session(pb::GetSessionRequest {
        repository: Some(session.reference.clone()),
        session_id,
        from_turn: None,
        to_turn: None,
    })?;
    let Some(bundle) = response.ledger_bundle else {
        return Err(CliError::Internal(anyhow::anyhow!(
            "the service returned no ledger bundle"
        )));
    };
    let (record, turns): (SessionRecord, Vec<SessionTurn>) =
        serde_json::from_slice(&bundle.payload)
            .map_err(|error| CliError::Internal(anyhow::anyhow!("ledger bundle: {error}")))?;
    if args.json {
        // Byte-parity: the bundle is the domain serialization of the pair.
        print!("{}", String::from_utf8_lossy(&bundle.payload));
        return Ok(true);
    }
    let turn_intents = response
        .turn_intents
        .into_iter()
        .map(|entry| (entry.turn, entry.intent))
        .collect::<std::collections::HashMap<_, _>>();
    let manifest_head = response
        .manifest_head
        .and_then(|h| h.value.clone().try_into().ok())
        .map(atomic_core::types::Merkle);
    let parent_manifest = response
        .parent_manifest
        .and_then(|h| h.value.clone().try_into().ok())
        .map(atomic_core::types::Merkle);
    super::session::render_session_detail(
        record,
        turns,
        turn_intents,
        manifest_head,
        parent_manifest,
        response.fork_turn,
        false,
    )?;
    Ok(true)
}

/// `atomic view list` over ListViews — every local form (`--remote` is
/// the scope-out remote surface). The wire's ViewInfo carries each view's
/// own/inherited change-count split and head state, so the versioned JSON
/// document renders client-side with the same JsonView shape the local
/// body emits; the plain form keeps its metadata rendering.
pub fn view_list(args: &super::view::List) -> CliResult<bool> {
    if args.remote.is_some() {
        return Ok(false); // the remote listing (scope-out remote surface)
    }
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let response = session.list_views(pb::ListViewsRequest {
        repository: Some(session.reference.clone()),
        include_details: false,
    })?;
    if args.json {
        let mut json_views = response
            .views
            .iter()
            .map(super::view::list::JsonView::from_wire)
            .collect::<Vec<_>>();
        json_views.sort_by(|left, right| left.name.cmp(&right.name));
        let current = response
            .views
            .iter()
            .find(|view| view.current)
            .map(|view| view.name.clone());
        super::view::list::print_local_json(&json_views, current)?;
        return Ok(true);
    }
    let entries: Vec<super::view::list::ViewEntry> = response
        .views
        .iter()
        .map(super::view::list::ViewEntry::from_wire)
        .collect();

    // Same filter + hierarchy rendering as the local listing, so both
    // paths look identical (see #194).
    let (visible, hidden) = super::view::list::compute_visibility(&entries, args.all);
    let ordered = super::view::list::tree_order(&entries, &visible);

    let max_name_len = ordered
        .iter()
        .map(|(_, entry)| entry.name.len())
        .max()
        .unwrap_or(0);

    for (depth, entry) in &ordered {
        let line = if args.short {
            super::view::list::render_short_line(entry, *depth)
        } else {
            super::view::list::render_line(entry, *depth, max_name_len)
        };
        println!("{}", line);
    }

    if hidden > 0 {
        print_hint(&super::view::list::summary_line(hidden));
    }
    Ok(true)
}

/// `atomic vault goal list` over ListVaultEntries (kind=GOAL) — every
/// form: the raw `--status` filter rides the request verbatim ("all" is
/// no filter), and the wire carries each goal's developer/status/intent
/// plus the started_at/turns columns the JSON prints.
pub fn vault_goal_list(args: &super::vault::goal::GoalList) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::ListVaultEntriesRequest {
        repository: Some(session.reference.clone()),
        kind: Some(Kind::Goal as i32),
        status: None,
        summaries_only: true,
        cursor: None,
        budget: None,
        view: None,
        identity: None,
        path_prefix: None,
        entry_type: None,
        status_filter: Some(args.status.clone()),
        tool_result_previews: false,
    };
    let response = session.list_vault_entries(request)?;
    let rows = response
        .entries
        .iter()
        .map(|entry| super::vault::goal::GoalRow {
            name: entry.id.clone(),
            developer: entry.title.clone(),
            status: entry.status_label.clone().unwrap_or_default(),
            intent: entry.linked.first().cloned(),
            started_at: entry.started_at.clone().unwrap_or_default(),
            turns: entry.turns.unwrap_or(0),
        })
        .collect::<Vec<_>>();
    super::vault::goal::render_goal_rows(&rows, args.json);
    Ok(true)
}

/// `atomic vault query neighbors` over QueryGraph (Neighbors) — every
/// form: `-d` rides the request's depth, the wire's KGNode/KGEdge carry
/// the full domain fields, and the JSON serializes the same subgraph
/// shape the local body emits. The plain render matches the local
/// listing (the Nodes/Edges sections and their counts).
pub fn query_neighbors(args: &super::query::QueryNeighbors) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let (node_id, depth) = (args.node_id.clone(), args.depth);
    let response = session.query_graph(pb::QueryGraphRequest {
        repository: Some(session.reference.clone()),
        limit: 0,
        query: Some(pb::query_graph_request::Query::Neighbors(
            pb::NeighborsQuery {
                node_id,
                incoming: None,
                depth: Some(depth as u32),
            },
        )),
        view: None,
    })?;
    let nodes = response
        .nodes
        .iter()
        .map(kg_node_domain)
        .collect::<Vec<_>>();
    let edges = response
        .edges
        .iter()
        .map(kg_edge_domain)
        .collect::<Vec<_>>();
    if args.json {
        let subgraph = atomic_core::pristine::vault::KgSubgraph { nodes, edges };
        println!("{}", serde_json::to_string_pretty(&subgraph).unwrap());
        return Ok(true);
    }
    if nodes.is_empty() {
        println!("No neighbors found for '{}'.", args.node_id);
        return Ok(true);
    }
    println!("Nodes ({}):", nodes.len());
    for node in &nodes {
        let summary = node.summary.as_deref().unwrap_or("");
        let summary_display = if summary.is_empty() {
            String::new()
        } else {
            format!("  {}", super::query::truncate_display(summary, 50))
        };
        println!("  [{}] {}{}", node.kind, node.id, summary_display);
    }
    println!("\nEdges ({}):", edges.len());
    for edge in &edges {
        println!(
            "  {} \u{2192}[{}]\u{2192} {}",
            edge.from_id, edge.kind, edge.to_id
        );
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// doctor (MaintenanceService)
// ---------------------------------------------------------------------------

/// `atomic doctor check` over CheckRepository — the read-only consistency
/// check. Rendered to mirror the in-process path exactly (non-zero exit
/// when problems are found).
pub fn doctor_check(_args: &super::doctor::Check) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::CheckRepositoryRequest {
        repository: Some(session.reference.clone()),
    };
    let response = session.check_repository(request)?;

    println!("Verifying working-copy consistency against the graph...");
    print_info(&format!(
        "Checked {} clean file(s); {} with uncommitted edits skipped; {} conflicted.",
        response.clean_files_checked, response.uncommitted_skipped, response.conflicted_files
    ));

    if response.consistent {
        print_success("Working copy is consistent with the graph.");
        return Ok(true);
    }

    print_warning(&format!("{} problem(s) found:", response.findings.len()));
    for finding in &response.findings {
        println!("  ✗ {finding}");
    }
    print_hint(
        "Materialization drift can often be repaired by re-materializing \
         (e.g. `atomic view switch <current-view>`); conflict-state \
         disagreements indicate a bug worth reporting.",
    );
    Err(CliError::Internal(anyhow::anyhow!(
        "working-copy verification found {} problem(s)",
        response.findings.len()
    )))
}

/// `atomic doctor repair-dependency-index` over MaintenanceService.Repair.
pub fn doctor_repair_dependency_index(
    args: &super::doctor::RepairDependencyIndex,
) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::RepairRequest {
        repository: Some(session.reference.clone()),
        meta: None,
        action: pb::RepairAction::RebuildDependencyIndex as i32,
        force: args.force,
        view: String::new(),
    };
    let response = session.repair(request)?;
    println!("Repairing change dependency index...");
    if args.force {
        print_warning("--force enabled: existing dependency index rows will be replaced");
    }
    print_success(&format!(
        "Dependency index repair complete: {} indexed, {} skipped, {} failed",
        response.indexed, response.skipped, response.failed
    ));
    if response.failed > 0 {
        print_hint(
            "Some changes could not be loaded. Run with verbose logging to identify corrupted or missing change files.",
        );
    } else if response.indexed > 0 {
        print_hint("View filter setup for status/diff/content paths can now use pristine indexes instead of scanning .change files.");
    }
    Ok(true)
}

/// `atomic doctor materialize-crdt` over MaintenanceService.Repair.
pub fn doctor_materialize_crdt(args: &super::doctor::MaterializeCrdt) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    // The current view is resolved client-side (same default the
    // in-process path uses); the daemon materializes it server-side.
    let view = match &args.view {
        Some(view) => view.clone(),
        None => {
            let marker = crate::commands::find_repository_root()?
                .join(".atomic")
                .join("current_view");
            std::fs::read_to_string(&marker)
                .map(|content| content.trim().to_string())
                .unwrap_or_else(|_| "dev".to_string())
        }
    };
    let request = pb::RepairRequest {
        repository: Some(session.reference.clone()),
        meta: None,
        action: pb::RepairAction::MaterializeCrdt as i32,
        force: args.force,
        view,
    };
    let response = session.repair(request.clone())?;
    println!("Materializing CRDT tables for view '{}'...", request.view);
    if args.force {
        print_warning("--force enabled: existing CRDT trunk rows may be overwritten");
    }
    print_success(&format!(
        "CRDT materialization complete in {:.1}s: {} changes scanned, {} changes applied, {} FileOps applied, {} already materialized, {} skipped",
        response.elapsed_ms as f64 / 1000.0,
        response.changes_scanned,
        response.changes_applied,
        response.file_ops_applied,
        response.file_ops_already_materialized,
        response.file_ops_skipped
    ));
    print_hint(&format!(
        "CRDT rows: trunks +{}, branches +{}, leaves +{}",
        response.trunks_created, response.branches_created, response.leaves_created
    ));
    if response.file_ops_skipped > 0 {
        let skip = response.skip_stats;
        if let Some(skip) = skip {
            print_hint(&format!(
                "Skipped FileOps: non_create={}, unresolved_path={}, unresolved_line={}, missing_range={}, non_create_trunk={}, non_create_leaf={}",
                skip.non_create_trunk,
                skip.unresolved_path,
                skip.unresolved_line,
                skip.missing_content_range,
                skip.non_insert_branch,
                skip.non_insert_leaf
            ));
        }
        if !response.skip_samples.is_empty() {
            print_hint(&format!(
                "Skip samples: {}",
                response.skip_samples.join(", ")
            ));
        }
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// working-copy mutations (RepositoryMutationService)
// ---------------------------------------------------------------------------

/// `atomic remove` over RemoveFiles — every form (--dry-run and --force
/// ride the request; the handler reports the tolerant dry-run preview).
pub fn remove(args: &super::remove::Remove) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::RemoveFilesRequest {
        repository: Some(session.reference.clone()),
        meta: None,
        paths: args.paths.clone(),
        keep_working_copy: args.keep,
        dry_run: args.dry_run,
        force: args.force,
    };
    let response = session.remove_files(request)?;
    if args.dry_run {
        println!("Would remove {} file(s) from tracking", response.removed);
        return Ok(true);
    }
    println!("Removed {} file(s) from tracking", response.removed);
    Ok(true)
}

/// `atomic mv` over MoveFile — every form (--dry-run/--force ride the
/// request).
pub fn mv(args: &super::mv::Move) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::MoveFileRequest {
        repository: Some(session.reference.clone()),
        meta: None,
        source: args.source.clone(),
        destination: args.destination.clone(),
        dry_run: args.dry_run,
        force: args.force,
    };
    session.move_file(request)?;
    if args.dry_run {
        println!("Would move: {} → {}", args.source, args.destination);
        return Ok(true);
    }
    println!("Moving: {} → {}", args.source, args.destination);
    println!();
    print_success("Moved 1 file");
    println!();
    print_hint("Run 'atomic record' to capture this move (inode/history preserved)");
    Ok(true)
}

/// `atomic unrecord` over Unrecord — the non-dry-run forms, bare or
/// with a change argument: the prefix rides the wire's ChangeRef prefix
/// arm (whole-store resolution, the same resolver the local body uses;
/// the membership/dependency refusals surface from the handler).
/// `--dry-run` declines here and routes through the PreviewMutation arm
/// the command dispatches next.
pub fn unrecord(args: &super::unrecord::Unrecord) -> CliResult<bool> {
    if args.dry_run {
        return Ok(false); // the preview rides unrecord_preview below
    }
    let Some(session) = Service::open()? else {
        return Ok(false);
    };

    // A bare hash prefix needs the full 32-byte hash before the wire call:
    // resolve it through the service layer (a read, no redb here).
    let target = args.change.as_ref().map(|prefix| pb::ChangeRef {
        kind: Some(pb::change_ref::Kind::Prefix(prefix.clone())),
    });
    let request = pb::UnrecordRequest {
        repository: Some(session.reference.clone()),
        meta: None,
        expected: None,
        view: None,
        target,
    };
    let response = session.unrecord(request)?;

    if let Some(change) = &response.removed {
        let hash = change
            .hash
            .as_ref()
            .and_then(|h| h.value.clone().try_into().ok())
            .map(|bytes: [u8; 32]| Merkle(bytes).to_base32())
            .unwrap_or_else(|| "-".to_string());
        print_success(&format!("Unrecorded: {hash}"));
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// view mutations (ViewService)
// ---------------------------------------------------------------------------

/// `atomic view create` over CreateView — every form. `--from` seeds on
/// the source view, `--parent` anchors on the parent WITHOUT seeding
/// (the draft workspace form, the wire's Parent arm), `--empty`/plain
/// anchor on the nearest Shared ancestor; `--switch` chains the
/// SwitchView RPC.
pub fn view_create(args: &super::view::New) -> CliResult<bool> {
    let Some(name) = args.name.clone() else {
        return Err(CliError::InvalidArgument {
            message: "View name is required".to_string(),
        });
    };
    // The local body's name validation stays client-side — the same
    // messages, checked before any wire round-trip.
    super::view::new::validate_view_name(&name)
        .map_err(|message| CliError::InvalidArgument { message })?;
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let base = if let Some(from) = &args.from {
        Some(pb::create_view_request::Base::FromView(from.clone()))
    } else if let Some(parent) = &args.parent {
        // --parent: anchor on the parent WITHOUT seeding (draft workspace).
        Some(pb::create_view_request::Base::Parent(parent.clone()))
    } else if args.empty {
        Some(pb::create_view_request::Base::Empty(true))
    } else {
        None
    };
    let request = pb::CreateViewRequest {
        repository: Some(session.reference.clone()),
        meta: None,
        name: name.clone(),
        base,
        scope: pb::ViewScope::Unspecified as i32,
        base_snapshot: None,
    };
    let response = session.create_view(request)?;
    let view = response.view.expect("created view info");
    if args.switch {
        view_switch_plain(&session, &name)?;
    } else {
        print_hint(&format!(
            "Use 'atomic view switch {name}' to switch to the new view"
        ));
    }
    let seeded = view.change_count > 0;
    if seeded {
        print_success(&format!(
            "Created view: {name} (seeded - {} changes)",
            view.change_count
        ));
    } else {
        print_success(&format!("Created view: {name}"));
    }
    Ok(true)
}

/// `atomic view switch` over SwitchView — every form: --force/--stash
/// ride the request's explicit dirty-copy bypass (the caller's informed
/// decision; the server refusal stays the default without it).
pub fn view_switch(args: &super::view::Switch) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let Some(name) = args.name.clone() else {
        return Err(CliError::InvalidArgument {
            message: "View name is required".to_string(),
        });
    };
    // --force/--stash are the caller's informed decision to leave the
    // dirty working copy behind; the server refusal stays the default.
    let request = pb::SwitchViewRequest {
        repository: Some(session.reference.clone()),
        meta: None,
        view: name.clone(),
        bypass_dirty_check: Some(args.force || args.stash),
    };
    let response = session.switch_view(request)?;
    print_success(&format!(
        "Switched to view: {} ({} files updated)",
        name, response.files_written
    ));
    Ok(true)
}

fn view_switch_plain(session: &Service, name: &str) -> CliResult<()> {
    let request = pb::SwitchViewRequest {
        repository: Some(session.reference.clone()),
        meta: None,
        view: name.to_string(),
        bypass_dirty_check: None,
    };
    let response = session.switch_view(request)?;
    print_success(&format!(
        "Switched to view: {} ({} files updated)",
        name, response.files_written
    ));
    Ok(())
}

/// `atomic view delete` over DeleteView (--force is presentational only).
pub fn view_delete(args: &super::view::Delete) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let Some(name) = args.name.clone() else {
        return Err(CliError::InvalidArgument {
            message: "View name is required".to_string(),
        });
    };
    let request = pb::DeleteViewRequest {
        repository: Some(session.reference.clone()),
        meta: None,
        view: name.clone(),
        expected: None,
    };
    session.delete_view(request)?;
    print_success(&format!("Deleted view: {name}"));
    Ok(true)
}

/// `atomic view promote` over SetViewScope (SHARED).
pub fn view_promote(args: &super::view::Promote) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    // No name = promote the current view; resolve it from the marker.
    let name = match &args.name {
        Some(name) => name.clone(),
        None => std::fs::read_to_string(
            crate::commands::find_repository_root()?
                .join(".atomic")
                .join("current_view"),
        )
        .map(|s| s.trim().to_string())
        .map_err(|_| CliError::InvalidArgument {
            message: "View name is required".to_string(),
        })?,
    };
    let request = pb::SetViewScopeRequest {
        repository: Some(session.reference.clone()),
        meta: None,
        view: name.clone(),
        scope: pb::ViewScope::Shared as i32,
        expected_snapshot: None,
    };
    session.set_view_scope(request)?;
    print_success(&format!("Promoted '{name}' to shared root view"));
    Ok(true)
}

/// `atomic restore` over Restore. The non-dry-run forms route; every
/// dry-run form routes through the PreviewRestore arm the command
/// dispatches next — the listing form (no paths, or more than one) via
/// `restore_preview`, the single-file pristine-bytes dump via
/// `restore_single_dry_run` (the wire's content arm). The whole-copy
/// refusal surfaces with the daemon's message.
pub fn restore(args: &super::restore::Restore) -> CliResult<bool> {
    if args.dry_run {
        return Ok(false);
    }
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::RestoreRequest {
        repository: Some(session.reference.clone()),
        meta: None,
        paths: args.files.clone(),
        force: args.force,
    };
    let response = session.restore(request)?;

    if response.restored.is_empty() {
        println!("Nothing to restore - working copy is clean");
        return Ok(true);
    }
    println!("Restoring working copy...");
    for path in &response.restored {
        println!("  Restored: {path}");
    }
    println!();
    print_success(&format!("Restored {} file(s)", response.restored.len()));
    Ok(true)
}

// ---------------------------------------------------------------------------
// stash (RepositoryMutationService)
// ---------------------------------------------------------------------------

/// `atomic stash push` over CreateStash. Returns Ok(false) when the call
/// should fall back (unreachable daemon). An empty stash_id in the
/// response means a clean working copy.
pub fn stash_push(message: Option<String>, include_untracked: bool, keep: bool) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::CreateStashRequest {
        repository: Some(session.reference.clone()),
        meta: None,
        message,
        include_untracked,
        keep,
    };
    let response = session.create_stash(request)?;
    if response.stash_id.is_empty() {
        print_warning("No local changes to save");
        return Ok(true);
    }
    print_success(&format!("Saved working copy to {}", response.stash_id));
    if !keep {
        print_success("Working copy restored to clean state");
    }
    Ok(true)
}

/// `atomic stash apply` over ApplyStash. Renders the applied-file count.
pub fn stash_apply(stash: Option<String>) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::ApplyStashRequest {
        repository: Some(session.reference.clone()),
        meta: None,
        stash_id: stash.clone().unwrap_or_default(),
    };
    let response = session.apply_stash(request)?;
    if response.applied.is_empty() {
        print_warning("Stash was already applied or is empty");
    } else {
        print_success(&format!(
            "Applied {} file(s) to working copy",
            response.applied.len()
        ));
    }
    Ok(true)
}

/// `atomic stash pop` composes ApplyStash then DropStash (the wire's
/// reserved field 4 documents exactly this client-side composition).
pub fn stash_pop(stash: Option<String>) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let stash_id = match stash {
        Some(stash) => stash,
        // Pop's default target is stash@{0}: resolve it through the list.
        None => {
            let stashes = session
                .list_stashes(pb::ListStashesRequest {
                    repository: Some(session.reference.clone()),
                })?
                .stashes;
            match stashes.first() {
                Some(first) => first.id.clone(),
                None => {
                    return Err(CliError::InvalidArgument {
                        message: "No stashes found".to_string(),
                    })
                }
            }
        }
    };
    stash_apply(Some(stash_id.clone()))?;
    let request = pb::DropStashRequest {
        repository: Some(session.reference.clone()),
        meta: None,
        stash_id: Some(stash_id.clone()),
        all: false,
    };
    session.drop_stash(request)?;
    print_success(&format!("Dropped {stash_id}"));
    Ok(true)
}

/// `atomic stash list` over ListStashes.
pub fn stash_list() -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let stashes = session
        .list_stashes(pb::ListStashesRequest {
            repository: Some(session.reference.clone()),
        })?
        .stashes;
    if stashes.is_empty() {
        println!("No stashes found");
        return Ok(true);
    }
    for (index, stash) in stashes.iter().enumerate() {
        let created = stash
            .created_at
            .as_ref()
            .map(|ts| {
                chrono::DateTime::from_timestamp(ts.seconds, ts.nanos as u32)
                    .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        println!(
            "stash@{{{index}}}: {} ({created})",
            stash.message.as_deref().unwrap_or("WIP")
        );
    }
    Ok(true)
}

/// `atomic stash drop` over DropStash (all=true covers `stash clear`,
/// which routes without its confirmation prompt — the prompt is the
/// client's job).
pub fn stash_drop(stash: Option<String>, all: bool) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    // Resolve a bare drop to the newest stash for the display reference.
    let (stash_id, display) = match stash {
        Some(stash) => (stash.clone(), stash),
        None => {
            let stashes = list_stashes_raw(&session)?;
            match stashes.first() {
                Some(first) => (first.id.clone(), "stash@{0}".to_string()),
                None => {
                    return Err(CliError::InvalidArgument {
                        message: "No stashes found".to_string(),
                    })
                }
            }
        }
    };
    let request = pb::DropStashRequest {
        repository: Some(session.reference.clone()),
        meta: None,
        stash_id: Some(stash_id),
        all,
    };
    session.drop_stash(request)?;
    print_success(&format!("Dropped {display}"));
    Ok(true)
}

fn list_stashes_raw(session: &Service) -> CliResult<Vec<pb::StashInfo>> {
    let request = pb::ListStashesRequest {
        repository: Some(session.reference.clone()),
    };
    Ok(session.list_stashes(request)?.stashes)
}

// ---------------------------------------------------------------------------
// insert (RepositoryMutationService)
// ---------------------------------------------------------------------------

fn insert_request(
    session: &Service,
    source: Option<pb::insert_changes_request::Source>,
    target_view: Option<String>,
    allow_conflicts: bool,
    deps: bool,
    dry_run: bool,
) -> pb::InsertChangesRequest {
    // Dependency closure is the wire's default; the CLI's --deps=false
    // escape hatch rides apply_dependencies explicitly (the handler
    // applies or skips the closure server-side); dry_run previews the
    // plan server-side without inserting.
    pb::InsertChangesRequest {
        repository: Some(session.reference.clone()),
        meta: None,
        apply_dependencies: Some(deps),
        source,
        target_view,
        allow_conflicts,
        target: None,
        expected_snapshot: None,
        source_snapshot: None,
        dry_run: Some(dry_run),
        tag_from_view: None,
    }
}

fn insert_rpc(
    session: &Service,
    request: pb::InsertChangesRequest,
) -> CliResult<pb::InsertChangesResponse> {
    session.insert_changes(request)
}

// The wire-carried report the local insert bodies render: the outcome
// reassembled from the response, the conflicted-file entries for the
// inline summary, and the resolved names the leading lines print.

/// The applied hashes off the response's inserted entries.
fn insert_applied_hashes(response: &pb::InsertChangesResponse) -> Vec<Merkle> {
    response
        .inserted
        .iter()
        .filter_map(|change| {
            change
                .hash
                .as_ref()
                .and_then(|h| h.value.clone().try_into().ok())
                .map(Merkle)
        })
        .collect()
}

/// The response's cross-view outcome — the shape the local body's
/// `print_cross_view_outcome` renders (the skipped placeholder hashes
/// exist only for the count the line prints).
fn insert_cross_view_outcome(
    response: &pb::InsertChangesResponse,
) -> atomic_repository::CrossViewInsertOutcome {
    atomic_repository::CrossViewInsertOutcome {
        changes_applied: response.inserted.len(),
        applied_hashes: insert_applied_hashes(response),
        skipped_hashes: vec![Merkle::ZERO; response.skipped_count as usize],
        new_state: response
            .new_state
            .as_ref()
            .and_then(|h| h.value.clone().try_into().ok())
            .map(Merkle)
            .unwrap_or_default(),
        sequence: 0,
        has_conflicts: response.has_conflicts,
        was_dry_run: false,
    }
}

/// The inline conflicted-file entries off the response's listing (the
/// first record's line per file, in listing order).
fn insert_conflict_entries(
    response: &pb::InsertChangesResponse,
) -> Vec<super::insert::ConflictSummaryEntry> {
    let mut entries: Vec<super::insert::ConflictSummaryEntry> = Vec::new();
    for conflict in &response.conflicts {
        if entries.iter().any(|entry| entry.path == conflict.path) {
            continue;
        }
        entries.push(super::insert::ConflictSummaryEntry {
            path: conflict.path.clone(),
            line: conflict.line.map(|line| line as u32),
        });
    }
    entries
}

/// The local materialization report line + inline conflict summary over
/// the wire-carried counts — rendered with the same output functions the
/// local bodies use (the view/tag bodies finish a spinner; the
/// single/multi bodies print success), byte-identical by construction.
fn print_insert_refresh(response: &pb::InsertChangesResponse, via_spinner: bool) {
    if let (Some(files), Some(directories)) = (response.files_updated, response.directories_created)
    {
        if via_spinner {
            let spinner = crate::output::create_spinner("Materializing files for view...");
            crate::output::finish_success(
                &spinner,
                &format!("{files} files updated, {directories} directories"),
            );
        } else {
            print_success(&format!("{files} files updated, {directories} directories"));
        }
        super::insert::print_conflict_summary_entries(&insert_conflict_entries(response));
    }
}

/// `atomic insert <ref>` over InsertChanges — the raw reference rides the
/// single-ref arm (the handler resolves it with the local resolver's
/// semantics); both --deps forms (the escape hatch rides
/// apply_dependencies). Rendered with the local single-insert body's
/// exact print functions.
pub fn insert_single(
    change: &str,
    view: Option<String>,
    allow_conflicts: bool,
    deps: bool,
) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = insert_request(
        &session,
        Some(pb::insert_changes_request::Source::SingleRef(
            change.to_string(),
        )),
        view,
        allow_conflicts,
        deps,
        false,
    );
    let response = insert_rpc(&session, request)?;
    let hash = response
        .resolved_change
        .as_ref()
        .and_then(|h| h.value.clone().try_into().ok())
        .map(Merkle);
    if let Some(hash) = hash {
        print_info(&format!(
            "Inserting change {}...",
            crate::commands::format_hash(&hash, true)
        ));
    }
    let applied = insert_applied_hashes(&response);
    let new_state = response
        .new_state
        .as_ref()
        .and_then(|h| h.value.clone().try_into().ok())
        .map(Merkle)
        .unwrap_or_default();
    super::insert::print_insert_outcome(&applied, new_state, response.has_conflicts);
    print_insert_refresh(&response, false);
    Ok(true)
}

/// `atomic insert view <source> [--to <target>] [-n]` over InsertChanges —
/// both --deps forms and the dry-run preview (the handler computes the
/// plan with the same domain call the local body makes). Rendered with
/// the local view-insert body's exact print functions.
pub fn insert_from_view(
    source: &str,
    target: Option<String>,
    allow_conflicts: bool,
    deps: bool,
    dry_run: bool,
) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = insert_request(
        &session,
        Some(pb::insert_changes_request::Source::FromView(
            source.to_string(),
        )),
        target,
        allow_conflicts,
        deps,
        dry_run,
    );
    let response = insert_rpc(&session, request)?;
    let resolved_target = response.resolved_target_view.clone().unwrap_or_default();
    print_info(&format!(
        "Inserting changes from '{source}' to '{resolved_target}'..."
    ));
    let outcome = insert_cross_view_outcome(&response);
    super::insert::print_cross_view_outcome(&outcome, dry_run);
    if !dry_run {
        print_insert_refresh(&response, true);
    }
    Ok(true)
}

/// `atomic insert tag <tag> [--from-view <source>] [--to <target>] [-n]`
/// over InsertChanges — the --from-view override rides tag_from_view (the
/// handler composes the same cross-view options the local body makes),
/// both --deps forms and the dry-run preview ride the request. Rendered
/// with the local tag body's exact print functions.
pub fn insert_up_to_tag(
    tag: &str,
    from_view: Option<String>,
    target: Option<String>,
    allow_conflicts: bool,
    deps: bool,
    dry_run: bool,
) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let mut request = insert_request(
        &session,
        Some(pb::insert_changes_request::Source::UpToTag(tag.to_string())),
        target,
        allow_conflicts,
        deps,
        dry_run,
    );
    request.tag_from_view = from_view;
    let response = insert_rpc(&session, request)?;
    let from_view = response
        .resolved_source_view
        .clone()
        .unwrap_or_else(|| "dev".to_string());
    let resolved_target = response.resolved_target_view.clone().unwrap_or_default();
    print_info(&format!(
        "Inserting changes up to tag '{tag}' from '{from_view}' to '{resolved_target}'..."
    ));
    let outcome = insert_cross_view_outcome(&response);
    super::insert::print_cross_view_outcome(&outcome, dry_run);
    if !dry_run {
        print_insert_refresh(&response, true);
    }
    Ok(true)
}

/// `atomic insert change <ref>...` (multi-pick) over InsertChanges — every
/// reference rides the change-set arm raw (the handler resolves each with
/// the local resolver's semantics and applies them with the cherry-pick
/// call the local body makes). Rendered with the local multi-insert
/// body's exact print functions.
pub fn insert_changes(
    changes: &[String],
    target: Option<String>,
    allow_conflicts: bool,
) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let set = pb::ChangeSet {
        changes: changes.to_vec(),
    };
    let request = insert_request(
        &session,
        Some(pb::insert_changes_request::Source::ChangeSet(set)),
        target,
        allow_conflicts,
        true,
        false,
    );
    let response = insert_rpc(&session, request)?;
    let resolved_target = response.resolved_target_view.clone().unwrap_or_default();
    print_info(&format!(
        "Cherry-picking {} change(s) to '{resolved_target}'...",
        changes.len()
    ));
    let outcome = insert_cross_view_outcome(&response);
    super::insert::print_cross_view_outcome(&outcome, false);
    print_insert_refresh(&response, false);
    Ok(true)
}

/// The bare `atomic insert` promotion over InsertChanges — the promote
/// arm resolves the current view, its parent, and the missing set
/// server-side (the CLI holds no repository handle). The dry-run form
/// renders the local preview from the pre-flight response; the real form
/// pre-flights, runs the shared→shared confirmation client-side, then
/// inserts through the same arm. Rendered with the local body's exact
/// print functions.
pub fn insert_promote(args: &super::insert::Insert) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let mut request = insert_request(
        &session,
        Some(pb::insert_changes_request::Source::PromoteCurrentView(
            pb::PromoteCurrentView {},
        )),
        args.view.clone(),
        args.allow_conflicts,
        args.deps,
        true,
    );
    // The pre-flight read: source, parent, the missing set, and the
    // confirm-gate scopes — the same domain reads the local pre-flight
    // makes. The request is kept for the insert phase (the same arm
    // without the preview flag).
    let response = insert_rpc(&session, request.clone())?;
    let source = response.resolved_source_view.clone().unwrap_or_default();
    let target = response.resolved_target_view.clone().unwrap_or_default();
    let missing = response.inserted.len();
    if missing == 0 {
        print_success(&format!(
            "Already even with '{target}' — nothing to insert."
        ));
        return Ok(true);
    }
    print_info(&format!(
        "Inserting {missing} change(s): {source} → {target}"
    ));
    if args.dry_run {
        // The local dry-run branch: the numbered missing list (full hash
        // plus the 50-byte-truncated message), then the dry-run notice.
        println!();
        for (i, change) in response.inserted.iter().enumerate() {
            let hash = short_hash_full(&change.hash);
            match change.message.as_deref() {
                Some(message) if !message.is_empty() => println!(
                    "  {}. {} {}",
                    i + 1,
                    hash,
                    crate::output::truncate_bytes(message, 50)
                ),
                _ => println!("  {}. {}", i + 1, hash),
            }
        }
        println!();
        print_info("Dry run: no changes inserted. Re-run without --dry-run to insert.");
        return Ok(true);
    }
    // The shared→shared gate: the prompt runs client-side (interactive);
    // the wire carries the scopes the local pre-flight reads.
    if response.both_views_shared.unwrap_or(false) && !args.confirm {
        let prompt = format!(
            "Insert {missing} change(s) from shared view '{source}' into shared view '{target}'?"
        );
        let confirmed = dialoguer::Confirm::new()
            .with_prompt(&prompt)
            .default(false)
            .interact()
            .map_err(|_| CliError::InvalidArgument {
                message: "Refusing to insert between two shared views without \
                          confirmation. Re-run with --confirm to proceed \
                          non-interactively."
                    .to_string(),
            })?;
        if !confirmed {
            print_info("Aborted.");
            return Ok(true);
        }
    }
    // The insert itself: the same promote arm, now without the preview
    // flag — the handler runs the same domain call the local body makes
    // after its confirmation.
    request.dry_run = Some(false);
    let response = insert_rpc(&session, request)?;
    let outcome = insert_cross_view_outcome(&response);
    super::insert::print_cross_view_outcome(&outcome, false);
    Ok(true)
}

/// A wire hash in the full base32 form the local listings print
/// (`format_hash(hash, true)`).
fn short_hash_full(hash: &Option<pb::Hash>) -> String {
    hash.as_ref()
        .and_then(|h| h.value.clone().try_into().ok())
        .map(|bytes: [u8; 32]| Merkle(bytes).to_base32())
        .unwrap_or_else(|| "-".to_string())
}

/// `atomic view split` over SplitView. Change specs and --last N resolve
/// through the daemon's Log RPC; --switch composes SwitchView after.
/// Returns Ok(false) when the call should fall back to the local path.
pub fn view_split(args: &super::view::Split) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let from_view = args.from.clone().unwrap_or_else(|| {
        std::fs::read_to_string(
            crate::commands::find_repository_root()
                .expect("session resolved a repository")
                .join(".atomic")
                .join("current_view"),
        )
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "dev".to_string())
    });

    // Resolve the change set through the daemon's own Log: newest-first
    // entries carry the full hashes (--last takes the first N of that
    // list; prefixes match against the base32 form).
    let entries = log_entries(&session, &from_view)?;
    let mut hashes: Vec<[u8; 32]> = Vec::new();
    if let Some(n) = args.last {
        if n == 0 {
            return Err(CliError::InvalidArgument {
                message: "--last must be greater than zero".to_string(),
            });
        }
        if entries.len() < n {
            return Err(CliError::InvalidArgument {
                message: format!(
                    "view '{from_view}' has only {} change(s); cannot split the last {n}",
                    entries.len()
                ),
            });
        }
        hashes.extend(entries.iter().take(n).filter_map(|e| {
            e.hash.as_ref().and_then(|h| {
                let bytes: [u8; 32] = h.value.clone().try_into().ok()?;
                Some(bytes)
            })
        }));
    } else {
        if args.changes.is_empty() {
            return Err(CliError::InvalidArgument {
                message: "specify one or more changes to split, or use --last <N>".to_string(),
            });
        }
        for spec in &args.changes {
            let hash = if let Some(found) = entries.iter().find(|e| {
                e.hash
                    .as_ref()
                    .and_then(|h| h.value.clone().try_into().ok())
                    .map(|bytes: [u8; 32]| Merkle(bytes).to_base32().starts_with(spec))
                    .unwrap_or(false)
            }) {
                let bytes: [u8; 32] = found
                    .hash
                    .as_ref()
                    .expect("matched entry has a hash")
                    .value
                    .clone()
                    .try_into()
                    .map_err(|_| CliError::Internal(anyhow::anyhow!("malformed hash")))?;
                bytes
            } else {
                return Err(CliError::ChangeNotFound { hash: spec.clone() });
            };
            hashes.push(hash);
        }
    }

    let request = pb::SplitViewRequest {
        repository: Some(session.reference.clone()),
        meta: None,
        dry_run: args.dry_run,
        name: args.name.clone(),
        from_view,
        changes: hashes
            .into_iter()
            .map(|bytes| pb::Hash {
                value: bytes.to_vec(),
                algorithm: pb::HashAlgorithm::Blake3 as i32,
            })
            .collect(),
        cascade: args.cascade,
        // When switching, the draft is materialized by the switch below;
        // otherwise the daemon reconciles the source's working copy.
        materialize: !args.switch,
    };
    let from_view_display = request.from_view.clone();
    let response = session.split_view(request)?;

    let moved_list = |items: &[pb::SplitChangeInfo]| -> Vec<String> {
        items
            .iter()
            .filter_map(|c| {
                c.hash
                    .as_ref()
                    .and_then(|h| h.value.clone().try_into().ok())
                    .map(|bytes: [u8; 32]| Merkle(bytes).to_base32()[..12].to_string())
            })
            .collect()
    };
    if response.dry_run {
        print_success(&format!(
            "Would split {} change(s) out of {} into draft {}",
            response.moved.len(),
            from_view_display,
            args.name
        ));
    } else {
        print_success(&format!(
            "Split {} change(s) out of {} into draft {}",
            response.moved.len(),
            from_view_display,
            args.name
        ));
    }
    let moved = moved_list(&response.moved);
    for hash in &moved {
        println!("  moved  {hash}");
    }
    print_info(&format!(
        "'{}' now has {} change(s); draft '{}' has {} own change(s).",
        from_view_display, response.source_change_count, args.name, response.target_change_count
    ));
    if response.working_copy_updated && (response.files_written > 0 || response.files_removed > 0) {
        print_info(&format!(
            "Working copy updated: {} file(s) refreshed, {} removed.",
            response.files_written, response.files_removed
        ));
    }
    if args.switch {
        view_switch_plain(&session, &args.name)?;
    }
    Ok(true)
}

fn log_entries(session: &Service, view: &str) -> CliResult<Vec<pb::ChangeLogEntry>> {
    let request = pb::LogRequest {
        repository: Some(session.reference.clone()),
        all_views: false,
        view: if view.is_empty() {
            None
        } else {
            Some(view.to_string())
        },
        cursor: None,
        budget: None,
        path_filter: Vec::new(),
        tags_only: false,
    };
    Ok(session.log(request)?.entries)
}

/// `atomic revise` over Revise — every form. The reference (@/@~N/hash
/// prefix) resolves through the daemon's Log exactly as the local
/// `resolve_reference` does (newest-first: "@" is the last change,
/// "@~N" N back, a prefix case-insensitively with an ambiguity
/// refusal). The message composes client-side — an explicit `-m`, or
/// the SAME editor flow the local body runs, seeded with the change's
/// current message from the wire — and the author parses client-side.
/// `--reword` rides the reword arm; the content-modification mode rides
/// the content arm (the stack surgery — unrecord → record from the
/// working copy → re-apply — is the domain's, applied atomically);
/// `--dry-run` renders the same preview the local body prints, over the
/// wire's log entries.
pub fn revise(args: &super::revise::Revise) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };

    // Resolve the reference through the daemon's Log (newest first) to
    // the full hash plus the change's current message — the editor's
    // seed — and its sequence.
    let parsed = super::revise::ChangeRef::parse(&args.reference);
    let entries = log_entries(&session, "")?;
    let target = match &parsed {
        super::revise::ChangeRef::Last => {
            if entries.is_empty() {
                return Err(CliError::Internal(anyhow::anyhow!(
                    "Cannot revise: view '{}' is empty",
                    current_view_name()?
                )));
            }
            entries.first().cloned()
        }
        super::revise::ChangeRef::Relative(offset) => {
            let offset = *offset as usize;
            match entries.get(offset) {
                Some(entry) => Some(entry.clone()),
                None => {
                    return Err(CliError::Internal(anyhow::anyhow!(
                        "Reference @~{} is out of range (view has {} changes)",
                        offset,
                        entries.len()
                    )))
                }
            }
        }
        super::revise::ChangeRef::Hash(prefix) => {
            let prefix_lower = prefix.to_lowercase();
            let matches: Vec<_> = entries
                .iter()
                .filter(|e| {
                    e.hash
                        .as_ref()
                        .and_then(|h| h.value.clone().try_into().ok())
                        .map(|bytes: [u8; 32]| {
                            Merkle(bytes)
                                .to_base32()
                                .to_lowercase()
                                .starts_with(&prefix_lower)
                        })
                        .unwrap_or(false)
                })
                .collect();
            match matches.as_slice() {
                [] => {
                    return Err(CliError::ChangeNotFound {
                        hash: prefix.to_string(),
                    })
                }
                [single] => Some((*single).clone()),
                _ => {
                    return Err(CliError::AmbiguousHash {
                        hash: prefix.to_string(),
                    })
                }
            }
        }
    };
    let Some(entry) = target else {
        return Err(CliError::Internal(anyhow::anyhow!(
            "the log entry carries no hash"
        )));
    };
    let (target_hash, original_message, sequence) = (
        entry.hash.clone(),
        entry.message.clone().unwrap_or_default(),
        entry.sequence,
    );
    let Some(target_hash) = target_hash else {
        return Err(CliError::Internal(anyhow::anyhow!(
            "the log entry carries no hash"
        )));
    };

    // Dry-run: the same preview the local body prints (the entries to
    // unrecord, the target marker, the re-application count), over the
    // wire's log entries.
    if args.dry_run {
        println!(
            "Would revise change {} (sequence #{})",
            full_hash(&Some(target_hash.clone())),
            sequence
        );
        println!();
        let changes_to_unrecord = entries.len().saturating_sub(sequence as usize);
        if changes_to_unrecord > 1 {
            println!(
                "This will temporarily unrecord {} changes:",
                changes_to_unrecord
            );
            for e in entries.iter().filter(|e| e.sequence >= sequence) {
                let marker = if e.sequence == sequence {
                    " (target)"
                } else {
                    ""
                };
                println!("  #{}: {}{}", e.sequence, full_hash(&e.hash), marker);
            }
            println!();
            println!(
                "After revision, {} changes will be re-applied.",
                changes_to_unrecord - 1
            );
        } else {
            println!("This is the last change - no re-application needed.");
        }
        if args.reword {
            println!();
            println!("Mode: --reword (only change message, preserve file changes)");
        }
        return Ok(true);
    }

    // The message composes with the local body's own flow: -m wins, else
    // the editor (seeded with the current message), else the original.
    let message = args.get_message(&original_message)?;
    let author = args
        .parse_author()
        .map(|(name, email)| pb::Author { name, email });

    // The content-modification mode's gate: nothing to re-capture.
    if !args.reword {
        let status = session.status(pb::StatusRequest {
            repository: Some(session.reference.clone()),
        })?;
        let clean = status.files.iter().all(|file| {
            !matches!(
                pb::FileStatus::try_from(file.status),
                Ok(pb::FileStatus::Modified)
                    | Ok(pb::FileStatus::Deleted)
                    | Ok(pb::FileStatus::Added)
                    | Ok(pb::FileStatus::Conflicted)
            )
        });
        if clean && args.message.is_none() {
            print_warning("No changes to revise. Use --reword to only change the message.");
            return Ok(true);
        }
    }

    let request = pb::ReviseRequest {
        repository: Some(session.reference.clone()),
        meta: None,
        target: Some(pb::ChangeRef {
            kind: Some(pb::change_ref::Kind::Hash(target_hash.clone())),
        }),
        reword_message: args.reword.then(|| message.clone()),
        author: author.clone(),
        content: if args.reword {
            None
        } else {
            Some(pb::ContentRevise {
                message,
                author,
                paths: args
                    .files
                    .iter()
                    .map(|path| path.to_string_lossy().to_string())
                    .collect(),
            })
        },
        expected: None,
    };
    // The surgery hints, per mode — the local bodies' exact behavior:
    // the content mode narrates the unrecord/re-apply steps; the reword
    // form performs the same surgery silently and reports the re-applied
    // pending changes.
    let above = entries.len().saturating_sub(sequence as usize + 1);
    println!(
        "Revising change {} (sequence #{})...",
        parsed.description(),
        sequence
    );
    if !args.reword && above > 0 {
        print_hint(&format!(
            "Temporarily unrecording {} changes after target...",
            above
        ));
    }
    let response = session.revise(request)?;
    let change = response.change.expect("revised change info");
    if args.reword {
        if above > 0 {
            print_hint(&format!("Re-applied {} pending change(s).", above));
        }
    } else if above > 0 {
        print_hint(&format!("Re-applying {above} changes..."));
    }
    println!();
    print_success(&format!(
        "Revised {} → {}",
        full_hash(&Some(target_hash)),
        full_hash(&change.hash)
    ));
    Ok(true)
}

/// The current view name, from the working-copy marker file (a client-side
/// read — the same default every local command uses).
fn current_view_name() -> CliResult<String> {
    Ok(std::fs::read_to_string(
        crate::commands::find_repository_root()?
            .join(".atomic")
            .join("current_view"),
    )
    .map(|s| s.trim().to_string())
    .unwrap_or_else(|_| "dev".to_string()))
}

// ---------------------------------------------------------------------------
// preview reads (RepositoryQueryService)
// ---------------------------------------------------------------------------

/// `atomic conflicts` over ListConflicts — render parity: kind, line,
/// sides ride the add-only ConflictInfo fields.
pub fn conflicts(short: bool) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::ListConflictsRequest {
        repository: Some(session.reference.clone()),
    };
    let response = session.list_conflicts(request)?;

    if short {
        for conflict in &response.conflicts {
            let line = conflict
                .line
                .map(|l| l.to_string())
                .unwrap_or_else(|| "-".into());
            println!(
                "{}:{}:{}",
                conflict.path,
                line,
                conflict.kind.as_deref().unwrap_or("-")
            );
        }
        return Ok(true);
    }
    if response.conflicts.is_empty() {
        print_success("No conflicts.");
        return Ok(true);
    }
    let file_word = if response.conflicts.len() == 1 {
        "file"
    } else {
        "files"
    };
    // Group by path for the render (the wire is per-record).
    let mut by_path: std::collections::BTreeMap<&str, Vec<&pb::ConflictInfo>> =
        std::collections::BTreeMap::new();
    for conflict in &response.conflicts {
        by_path.entry(&conflict.path).or_default().push(conflict);
    }
    println!(
        "{}",
        crate::output::warning(&format!("{} conflicted {}:", by_path.len(), file_word))
    );
    println!();
    for (path, records) in &by_path {
        println!("\t{path}");
        for record in records {
            let where_ = match record.line {
                Some(line) => format!("line {line}"),
                None => "unknown line".to_string(),
            };
            let kind = record.kind.as_deref().unwrap_or("conflict");
            if record.sides.is_empty() {
                println!("\t    {kind} conflict at {where_}");
            } else {
                let sides = record
                    .sides
                    .iter()
                    .map(|h| h.chars().take(12).collect::<String>())
                    .collect::<Vec<_>>()
                    .join(", ");
                println!("\t    {kind} conflict at {where_} between {sides}");
            }
        }
    }
    println!();
    print_hint("Resolve the markers (>>>>>>> / ======= / <<<<<<<), then run 'atomic record'.");
    Ok(true)
}

/// The shared render for the restore previews — the local listing
/// branch's exact lines over the wire-carried classification.
fn render_restore_preview(response: &pb::PreviewMutationResponse, partial: bool) {
    if response.affected_paths.is_empty() && response.untracked.is_empty() {
        // The local body's partial/whole split: naming paths must not
        // claim the whole working copy is clean.
        println!(
            "{}",
            if partial {
                "Nothing to restore for the specified path(s)"
            } else {
                "Nothing to restore - working copy is clean"
            }
        );
        return;
    }
    for path in &response.affected_paths {
        println!("Would restore: {path}");
    }
    for path in &response.untracked {
        println!("Would untrack: {path} (kept on disk)");
    }
    println!();
    let total = response.affected_paths.len() + response.untracked.len();
    print_hint(&format!(
        "(dry run - {} would be restored)",
        super::restore::format_count(total, "file")
    ));
}

/// `atomic restore --dry-run` (the listing form) over PreviewRestore —
/// the would-restore / would-untrack split rides the response.
pub fn restore_preview(paths: Vec<String>) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let partial = !paths.is_empty();
    let request = pb::PreviewRestoreRequest {
        repository: Some(session.reference.clone()),
        paths,
        force: false,
        content_path: None,
    };
    let response = session.preview_restore(request)?;
    render_restore_preview(&response, partial);
    Ok(true)
}

/// `atomic restore --dry-run <file>` (the single-file form) over
/// PreviewRestore's content arm — the handler reads the file's pristine
/// bytes with the same domain call the local body makes; the CLI dumps
/// them to stdout byte-for-byte (no extra output). An Added file previews
/// the untrack instead (the local listing branch), and no pristine
/// content is the local FileNotFound error.
pub fn restore_single_dry_run(path: &str) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::PreviewRestoreRequest {
        repository: Some(session.reference.clone()),
        paths: vec![path.to_string()],
        force: false,
        content_path: Some(path.to_string()),
    };
    let response = session.preview_restore(request)?;
    if !response.untracked.is_empty() {
        // The single Added file: the local listing branch's lines.
        render_restore_preview(&response, true);
        return Ok(true);
    }
    match response.pristine_content {
        Some(bytes) => {
            use std::io::Write;
            std::io::stdout()
                .write_all(&bytes)
                .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to write: {}", e)))?;
            Ok(true)
        }
        None => Err(CliError::FileNotFound {
            path: PathBuf::from(path),
        }),
    }
}

/// `atomic insert preview` over PreviewMutation (InsertPreview).
pub fn insert_preview(
    from_view: &str,
    to_view: Option<String>,
    up_to_tag: Option<String>,
) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let to_view = to_view.unwrap_or_else(|| {
        std::fs::read_to_string(
            crate::commands::find_repository_root()
                .unwrap_or_default()
                .join(".atomic")
                .join("current_view"),
        )
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "dev".to_string())
    });
    let insert = pb::InsertPreview {
        source: Some(match up_to_tag {
            Some(tag) => pb::insert_preview::Source::UpToTag(tag),
            None => pb::insert_preview::Source::FromView(from_view.to_string()),
        }),
        target_view: to_view.clone(),
        allow_conflicts: false,
    };
    let request = pb::PreviewMutationRequest {
        repository: Some(session.reference.clone()),
        operation: Some(pb::preview_mutation_request::Operation::Insert(insert)),
    };
    let response = session.preview_mutation(request)?;

    println!("  Source view: {from_view}");
    println!("  Target view: {to_view}");
    println!();
    if response.changes.is_empty() {
        print_success("No changes to insert - target view is up to date.");
    } else {
        println!(
            "Changes that would be inserted ({}):",
            response.changes.len()
        );
        println!();
        for (i, change) in response.changes.iter().enumerate() {
            let hash = short_hash(&change.hash);
            let message = change.message.clone().unwrap_or_default();
            let short_msg: String = message.chars().take(50).collect();
            println!("  {}. {} {}", i + 1, hash, short_msg);
        }
        println!();
        print_info(&format!(
            "Run 'atomic insert view {from_view}' to insert these changes."
        ));
    }
    Ok(true)
}

/// `atomic unrecord --dry-run` over PreviewMutation (UnrecordPreview).
/// Only the bare form routes (absent = last change); a change argument
/// needs the local resolver's exact semantics — see `unrecord`.
pub fn unrecord_preview(change: Option<String>) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let target = change.map(|prefix| pb::ChangeRef {
        kind: Some(pb::change_ref::Kind::Prefix(prefix)),
    });
    let request = pb::PreviewMutationRequest {
        repository: Some(session.reference.clone()),
        operation: Some(pb::preview_mutation_request::Operation::Unrecord(
            pb::UnrecordPreview { view: None, target },
        )),
    };
    let response = session.preview_mutation(request)?;
    for change in &response.changes {
        let hash = full_hash(&change.hash);
        print_warning(&format!("Would unrecord: {hash}"));
    }
    Ok(true)
}

/// Resolve a hash prefix to the full hash through the service layer —
/// Log entries, newest first, the same resolution the in-process
/// `resolve_change` performs.
fn resolve_change_hash(session: &Service, prefix: &str) -> CliResult<Merkle> {
    let request = pb::LogRequest {
        repository: Some(session.reference.clone()),
        all_views: false,
        view: None,
        cursor: None,
        budget: Some(pb::PageBudget {
            max_items: Some(200),
            max_bytes: None,
        }),
        path_filter: Vec::new(),
        tags_only: false,
    };
    let entries = session.log(request)?.entries;
    let matches: Vec<_> = entries
        .iter()
        .filter(|entry| {
            entry
                .hash
                .as_ref()
                .map(|h| {
                    h.value
                        .clone()
                        .try_into()
                        .map(|bytes: [u8; 32]| Merkle(bytes).to_base32().starts_with(prefix))
                        .unwrap_or(false)
                })
                .unwrap_or(false)
        })
        .collect();
    match matches.len() {
        0 => Err(CliError::InvalidArgument {
            message: format!("No change found matching '{prefix}'"),
        }),
        1 => {
            let bytes: [u8; 32] = matches[0]
                .hash
                .as_ref()
                .expect("matched entry has a hash")
                .value
                .clone()
                .try_into()
                .map_err(|_| CliError::Internal(anyhow::anyhow!("malformed hash")))?;
            Ok(Merkle(bytes))
        }
        _ => Err(CliError::InvalidArgument {
            message: format!("Ambiguous change prefix '{prefix}'"),
        }),
    }
}

// ---------------------------------------------------------------------------
// provenance trace/show (ProvenanceService.ExportProvenance)
// ---------------------------------------------------------------------------

/// One explaining graph over the wire: the domain graph, the mapped
/// projector input, and the pre-walked prior-turn chain the human trace
/// renders.
pub(crate) struct ProvenanceWireGraph {
    pub graph: atomic_core::change::ProvenanceGraph,
    pub input: atomic_canonical::prov::ProvActivityInput,
    pub prior_activities: Vec<String>,
    pub chain_truncated: bool,
}

/// The wire's ProvActivityInput bundle (schema "atomic.prov.input.v1") →
/// the plain projector input — the same field-for-field shape the handler
/// serialized.
fn prov_input_domain(payload: &[u8]) -> CliResult<atomic_canonical::prov::ProvActivityInput> {
    let value: serde_json::Value = serde_json::from_slice(payload)
        .map_err(|error| CliError::Internal(anyhow::anyhow!("prov input bundle: {error}")))?;
    let field = |name: &str| -> CliResult<Option<String>> {
        match &value[name] {
            serde_json::Value::Null => Ok(None),
            serde_json::Value::String(text) => Ok(Some(text.clone())),
            other => Err(CliError::Internal(anyhow::anyhow!(
                "prov input field {name}: unexpected {other}"
            ))),
        }
    };
    Ok(atomic_canonical::prov::ProvActivityInput {
        change_id_base32: field("change_id_base32")?.unwrap_or_default(),
        activity_id: field("activity_id")?.unwrap_or_default(),
        started_at: field("started_at")?,
        ended_at: field("ended_at")?,
        agent_slug: field("agent_slug")?.unwrap_or_default(),
        agent_display_name: field("agent_display_name")?.unwrap_or_default(),
        agent_vendor: field("agent_vendor")?,
        person_did: field("person_did")?.unwrap_or_default(),
        generated: value["generated"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|item| item.as_str().unwrap_or_default().to_string())
                    .collect()
            })
            .unwrap_or_default(),
        used: value["used"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|item| item.as_str().unwrap_or_default().to_string())
                    .collect()
            })
            .unwrap_or_default(),
        turn_parent: field("turn_parent")?,
    })
}

/// `atomic provenance trace/show <target>` over ExportProvenance — the
/// handler resolves the target (URN/prefix/hash), loads the explaining
/// graphs (newest first), maps each to the projection input, and pre-walks
/// the prior-turn chain; the CLI projects (and signs — the identity store
/// is a client-side config read) with the SAME canonical functions.
/// `Ok(None)` = outside a repository (the not-a-repository fallthrough).
pub(crate) fn provenance_export(
    target: &str,
    person_did: &str,
) -> CliResult<Option<(Merkle, Vec<ProvenanceWireGraph>)>> {
    let Some(session) = Service::open()? else {
        return Ok(None);
    };
    Ok(Some(provenance_export_with_session(
        &session, target, person_did,
    )?))
}

fn provenance_export_with_session(
    session: &Service,
    target: &str,
    person_did: &str,
) -> CliResult<(Merkle, Vec<ProvenanceWireGraph>)> {
    let request = pb::ExportProvenanceRequest {
        repository: Some(session.reference.clone()),
        change: None,
        view: None,
        person_did: Some(person_did.to_string()),
        target: Some(target.to_string()),
    };
    let response = session.export_provenance(request)?;
    let resolved = response
        .resolved_change
        .as_ref()
        .and_then(|hash| hash.value.clone().try_into().ok())
        .map(Merkle)
        .ok_or_else(|| CliError::Internal(anyhow::anyhow!("no resolved change")))?;
    let mut graphs = Vec::with_capacity(response.graphs.len());
    for bundle in &response.graphs {
        let graph_payload = bundle
            .graph
            .as_ref()
            .filter(|bytes| bytes.schema == "atomic.prov.graph.v1")
            .ok_or_else(|| {
                CliError::Internal(anyhow::anyhow!("missing provenance graph bundle"))
            })?;
        let graph: atomic_core::change::ProvenanceGraph =
            serde_json::from_slice(&graph_payload.payload)
                .map_err(|error| CliError::Internal(anyhow::anyhow!("graph bundle: {error}")))?;
        let input_payload = bundle
            .input
            .as_ref()
            .filter(|bytes| bytes.schema == "atomic.prov.input.v1")
            .ok_or_else(|| CliError::Internal(anyhow::anyhow!("missing prov input bundle")))?;
        graphs.push(ProvenanceWireGraph {
            graph,
            input: prov_input_domain(&input_payload.payload)?,
            prior_activities: bundle.prior_activities.clone(),
            chain_truncated: bundle.chain_truncated,
        });
    }
    Ok((resolved, graphs))
}

// ---------------------------------------------------------------------------
// remote registry (SyncService)
// ---------------------------------------------------------------------------

/// `atomic remote` over ListRemotes/ManageRemotes — every subcommand. The
/// registry lives in the repository database; the handler runs the exact
/// domain call each local body makes (add/add-default/remove/set-url/
/// rename/set-default) and the CLI renders the local reports.
pub fn remote(args: &super::remote::Remote) -> CliResult<bool> {
    use super::remote::RemoteSubcommand;
    // The URL forms validate client-side (pure argument validation, the
    // same messages the local bodies raise before any repository work).
    let validate_url = |url: &str| -> CliResult<()> {
        if !url.contains("://") {
            return Err(CliError::InvalidArgument {
                message: format!(
                    "Invalid URL '{}': URL must include a scheme (e.g., https://)",
                    url
                ),
            });
        }
        Ok(())
    };
    match &args.command {
        None => {
            let Some(session) = Service::open()? else {
                return Ok(false);
            };
            let response = session.list_remotes(pb::ListRemotesRequest {
                repository: Some(session.reference.clone()),
            })?;
            if response.remotes.is_empty() {
                print_hint(
                    "No remotes configured. Use 'atomic remote add <name> <url>' to add one.",
                );
                return Ok(true);
            }
            for remote in &response.remotes {
                if args.verbose {
                    let default_marker = if remote.default { " (default)" } else { "" };
                    println!("{}\t{}{}", remote.name, remote.url, default_marker);
                } else {
                    println!("{}", remote.name);
                }
            }
            Ok(true)
        }
        Some(RemoteSubcommand::Add(add)) => {
            validate_url(&add.url)?;
            let Some(session) = Service::open()? else {
                return Ok(false);
            };
            session.manage_remotes(pb::ManageRemotesRequest {
                repository: Some(session.reference.clone()),
                meta: request_meta(),
                action: pb::RemoteAction::Add as i32,
                name: Some(add.name.clone()),
                url: Some(add.url.clone()),
                new_name: None,
                set_default: add.default,
            })?;
            print_success(&format!("Remote '{}' added", add.name));
            if args.verbose {
                println!("  URL: {}", add.url);
            }
            Ok(true)
        }
        Some(RemoteSubcommand::Remove(remove)) => {
            let Some(session) = Service::open()? else {
                return Ok(false);
            };
            session.manage_remotes(pb::ManageRemotesRequest {
                repository: Some(session.reference.clone()),
                meta: request_meta(),
                action: pb::RemoteAction::Remove as i32,
                name: Some(remove.name.clone()),
                url: None,
                new_name: None,
                set_default: false,
            })?;
            print_success(&format!("Remote '{}' removed", remove.name));
            Ok(true)
        }
        Some(RemoteSubcommand::SetUrl(set_url)) => {
            validate_url(&set_url.url)?;
            let Some(session) = Service::open()? else {
                return Ok(false);
            };
            session.manage_remotes(pb::ManageRemotesRequest {
                repository: Some(session.reference.clone()),
                meta: request_meta(),
                action: pb::RemoteAction::SetUrl as i32,
                name: Some(set_url.name.clone()),
                url: Some(set_url.url.clone()),
                new_name: None,
                set_default: false,
            })?;
            print_success(&format!("Remote '{}' URL updated", set_url.name));
            if args.verbose {
                println!("  New URL: {}", set_url.url);
            }
            Ok(true)
        }
        Some(RemoteSubcommand::Rename(rename)) => {
            let Some(session) = Service::open()? else {
                return Ok(false);
            };
            session.manage_remotes(pb::ManageRemotesRequest {
                repository: Some(session.reference.clone()),
                meta: request_meta(),
                action: pb::RemoteAction::Rename as i32,
                name: Some(rename.old_name.clone()),
                url: None,
                new_name: Some(rename.new_name.clone()),
                set_default: false,
            })?;
            print_success(&format!(
                "Remote '{}' renamed to '{}'",
                rename.old_name, rename.new_name
            ));
            Ok(true)
        }
        Some(RemoteSubcommand::Default(default)) => {
            let Some(session) = Service::open()? else {
                return Ok(false);
            };
            session.manage_remotes(pb::ManageRemotesRequest {
                repository: Some(session.reference.clone()),
                meta: request_meta(),
                action: pb::RemoteAction::SetDefault as i32,
                name: Some(default.name.clone()),
                url: None,
                new_name: None,
                set_default: false,
            })?;
            print_success(&format!("Remote '{}' set as default", default.name));
            Ok(true)
        }
    }
}

// ---------------------------------------------------------------------------
// push / pull (SyncService) — the network sync is handler-side domain;
// auth stays client-side (the identity store is a config read, not redb)
// ---------------------------------------------------------------------------

/// Resolve the remote name + URL through the service layer's ListRemotes
/// read — the same domain lookup the local bodies perform over the
/// repository, with the same default (the configured default remote, else
/// "origin") and the same not-found error.
fn sync_resolve_remote(session: &Service, remote: &Option<String>) -> CliResult<(String, String)> {
    let response = session.list_remotes(pb::ListRemotesRequest {
        repository: Some(session.reference.clone()),
    })?;
    let name = match remote {
        Some(name) if !name.is_empty() => name.clone(),
        _ => response
            .remotes
            .iter()
            .find(|remote| remote.default)
            .map(|remote| remote.name.clone())
            .unwrap_or_else(|| "origin".to_string()),
    };
    if name.contains("://") {
        return Ok((name.clone(), name));
    }
    match response.remotes.iter().find(|remote| remote.name == name) {
        Some(entry) => Ok((name, entry.url.clone())),
        None => Err(CliError::RemoteNotFound { name }),
    }
}

/// The client-side auth headers: the SAME attach_identity resolution the
/// local bodies run (identity store + config reads + token minting — none
/// of it redb), extracted from the built config.
fn sync_auth_headers(remote_url: &str, identity: Option<&str>) -> HashMap<String, String> {
    let rt = tokio::runtime::Runtime::new()
        .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {}", e)))
        .ok();
    let Some(rt) = rt else { return HashMap::new() };
    let config = rt.block_on(crate::commands::auth::attach_identity(
        atomic_remote::HttpRemoteConfig::new(),
        remote_url,
        identity,
    ));
    config.extra_headers.into_iter().collect()
}

/// Map the handler's structured sync error (stage + variant + the local
/// body's composed message) back to the exact CLI error the local body
/// raised, printing the same spinner-stage line.
fn sync_error_map(error: CliError) -> CliError {
    let CliError::ServiceRefusal { message } = &error else {
        return error;
    };
    let Some(payload) = message.strip_prefix("atomic:REMOTE: ") else {
        return error;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) else {
        return error;
    };
    let (Some(stage), Some(variant), Some(url), Some(text)) = (
        value.get("stage").and_then(|v| v.as_str()),
        value.get("variant").and_then(|v| v.as_str()),
        value.get("url").and_then(|v| v.as_str()),
        value.get("message").and_then(|v| v.as_str()),
    ) else {
        return error;
    };
    // The spinner-stage line the local body printed via finish_error.
    let stage_line = match stage {
        "connect" => Some("Failed to connect"),
        "advertise" => Some("Failed to fetch remote metadata"),
        "send" => Some("Push failed"),
        "verify" => Some("Push landed, but remote verification failed"),
        "verify-patch" => Some("Push landed, but remote union is incomplete"),
        "fetch" => Some("Failed to fetch remote view"),
        "fetch-missing" => Some("Remote view not found"),
        _ => None,
    };
    if let Some(line) = stage_line {
        eprintln!("✗ {line}");
    }
    match variant {
        "remote_error" => CliError::RemoteError {
            message: text.to_string(),
            url: Some(url.to_string()),
        },
        "authentication_failed" => CliError::AuthenticationFailed {
            remote: url.to_string(),
        },
        "change_not_found" => CliError::ChangeNotFound {
            hash: text.to_string(),
        },
        "missing_dependency" => CliError::MissingDependency {
            change: "uploaded change".to_string(),
            dependency: text.to_string(),
        },
        "conflict" => CliError::Conflict {
            description: text.to_string(),
        },
        _ => error,
    }
}

use std::collections::HashMap;

/// A hidden-spinner stage line (the local body's finish_success prints to
/// stderr when the progress UI is hidden — e.g. piped test runs).
fn stage_line(message: &str) {
    eprintln!("✓ {message}");
}

/// One manifest render state for the divergence block — the local
/// `format_manifest_state` over wire-carried data.
fn manifest_state_text(state: &Option<pb::ManifestState>) -> String {
    match state {
        Some(state) if state.change_count > 0 => {
            format!(
                "{} ({} changes)",
                &state.state[..12.min(state.state.len())],
                state.change_count
            )
        }
        _ => "(empty)".to_string(),
    }
}

/// `atomic push` over PushChanges — every form. The handler performs the
/// network sync end to end and returns the structured report; the CLI
/// renders the local push body's exact lines (the client-side pieces are
/// the remote-name resolution read, the credential check, and the auth
/// header resolution — all config/identity reads, never redb).
pub fn push(args: &super::push::Push) -> CliResult<bool> {
    use crate::output::{error as print_error_fn, hint, view as style_view};
    use crate::output::{hash as style_hash, success as style_success};
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let (remote_name, remote_url) = sync_resolve_remote(&session, &args.remote)?;

    // Print header, fail fast on credentials — the local body's order.
    println!(
        "Pushing to {} ({})",
        style_view(&remote_name),
        hint(&remote_url)
    );
    crate::commands::auth::check_push_credentials(&remote_url, args.identity.as_deref())?;
    let auth_headers = sync_auth_headers(&remote_url, args.identity.as_deref());
    stage_line("Connected");

    let request = pb::PushChangesRequest {
        repository: Some(session.reference.clone()),
        meta: request_meta(),
        remote: remote_name.clone(),
        views: Vec::new(),
        expected: None,
        to_view: args.to_view.clone(),
        from_view: args.from_view.clone(),
        dry_run: args.dry_run,
        force: args.force,
        all: args.all,
        insecure: args.insecure,
        timeout_secs: args.timeout,
        auth_headers: auth_headers.clone(),
        remote_url: Some(remote_url.clone()),
    };
    let response = match session.push_changes(request) {
        Ok(response) => response,
        Err(error) => return Err(sync_error_map(error)),
    };

    // The plan-phase conflict: the local divergence/identity block plus
    // the local CliError.
    if let Some(conflict) = &response.conflict {
        print_error_fn(&format!(
            "View '{}' has diverged from the remote",
            conflict.view
        ));
        println!();
        if !conflict.identity_mismatch {
            if let (Some(local), Some(remote)) = (&conflict.local, &conflict.remote) {
                println!(
                    "  Local:  {} at {}",
                    style_view(&local.view),
                    crate::output::info(&manifest_state_text(&Some(local.clone())))
                );
                println!(
                    "  Remote: {} at {}",
                    style_view(&remote.view),
                    crate::output::info(&manifest_state_text(&Some(remote.clone())))
                );
                println!();
            }
            print_hint("The remote view's log is not a prefix of your local log.");
            print_hint("Use 'atomic pull' to fetch remote changes, or");
            print_hint("Use 'atomic push --force' to attempt the push anyway (server decides)");
        } else {
            print_hint(&conflict.description);
            print_hint("A view's scope and parent are fixed at creation and cannot be");
            print_hint("changed by a push. Rename the local view or push to a different");
            print_hint("remote view with '--to-view'.");
        }
        return Err(CliError::Conflict {
            description: format!("View '{}': {}", conflict.view, conflict.description),
        });
    }

    let Some(report) = response.report else {
        return Err(CliError::Internal(anyhow::anyhow!(
            "the service returned no push report"
        )));
    };
    stage_line("Fetched remote view metadata");

    // Per-view advertisement lines (the plan order, root → leaf).
    for view in &report.views {
        println!(
            "  {} Remote {} {}",
            style_success("✓"),
            style_view(&view.remote_name),
            if view.exists_on_remote {
                format!(
                    "has {}",
                    super::push::helpers::format_count(view.remote_change_count as usize, "change")
                )
            } else {
                "does not exist yet".to_string()
            }
        );
        if view.forced {
            print_warning(&format!(
                "View '{}' has diverged; pushing anyway (--force). The server may still reject it.",
                view.remote_name
            ));
        }
    }

    let format_count = super::push::helpers::format_count;

    if args.dry_run {
        let active: Vec<&pb::PushViewReport> =
            report.views.iter().filter(|view| !view.noop).collect();
        if active.is_empty() {
            print_success("Already up to date - nothing to push");
            return Ok(true);
        }
        println!(
            "Would sync {} to {}:",
            format_count(active.len(), "view"),
            report.remote_name
        );
        println!();
        for view in &active {
            let parent = view
                .parent
                .as_deref()
                .map(|parent| format!(", parent {parent}"))
                .unwrap_or_default();
            println!(
                "  {} [{}{}]: {} to store, declare manifest ({} in log)",
                style_view(&view.remote_name),
                view.scope,
                parent,
                format_count(view.to_store.len(), "change"),
                format_count(view.log_count as usize, "change"),
            );
            for change in &view.to_store {
                let hash = short_hash(&change.hash);
                let message = change.message.clone().unwrap_or_default();
                let message = if message.is_empty() {
                    "(no message)".to_string()
                } else {
                    message
                };
                println!("    {} {}", style_hash(&hash), message);
            }
        }
        println!();
        print_hint(&format!("Remote URL: {}", report.remote_url));
        return Ok(true);
    }

    if report.views.iter().all(|view| view.noop) {
        print_success("Already up to date");
        return Ok(true);
    }

    // Sync phase lines.
    for view in &report.views {
        if view.noop {
            if view.shrink {
                print_warning(&format!(
                    "View '{}' is larger on the remote than locally; leaving it unchanged. \
                     Push '{}' directly with --force to rewrite it to your smaller set.",
                    view.remote_name, view.remote_name
                ));
            } else {
                println!(
                    "  {} {} already up to date",
                    style_success("✓"),
                    style_view(&view.remote_name)
                );
            }
            continue;
        }
        if !view.to_store.is_empty() {
            println!(
                "Syncing view {} ({} new):",
                style_view(&view.remote_name),
                format_count(view.to_store.len(), "change")
            );
            for (index, change) in view.to_store.iter().enumerate() {
                let hash = short_hash(&change.hash);
                let message = change.message.clone().unwrap_or_default();
                let message = if message.is_empty() {
                    "(no message)".to_string()
                } else {
                    message
                };
                println!(
                    "  {} {} ({}/{}) {}",
                    style_success("✓"),
                    style_hash(&hash),
                    index + 1,
                    view.to_store.len(),
                    message
                );
            }
            stage_line(&format!(
                "Stored {}",
                format_count(view.to_store.len(), "change")
            ));
        }
        if view.declared {
            println!(
                "  {} Declared {} [{}] ({} in log)",
                style_success("✓"),
                style_view(&view.remote_name),
                view.scope,
                format_count(view.log_count as usize, "change"),
            );
        }
    }

    for sidecar in &report.attestations {
        println!(
            "  {} {} attestation ({}, {} covered)",
            style_success("✓"),
            style_hash(&short_hash(&sidecar.hash)),
            sidecar.cost.clone().unwrap_or_default(),
            sidecar.covered,
        );
    }
    for sidecar in &report.provenance {
        println!(
            "  {} {} provenance ({} nodes, {} changes)",
            style_success("✓"),
            style_hash(&short_hash(&sidecar.hash)),
            sidecar.nodes,
            sidecar.explained,
        );
    }
    for tag in &report.tags {
        let hash = short_hash(&tag.hash);
        println!(
            "  {} {} tag '{}' ({})",
            style_success("✓"),
            style_hash(&hash),
            tag.name,
            tag.kind,
        );
    }

    // The single /code push + verification (the stage lines match the
    // local body's spinner finishes).
    let sent_anything = report.views_declared > 0
        || report.total_stored > 0
        || !report.attestations.is_empty()
        || !report.provenance.is_empty()
        || !report.tags.is_empty();
    if sent_anything {
        stage_line("Push complete; remote union contains proposed patches");
    }

    // Summary.
    println!();
    let mut summary = format!(
        "Push complete: {} synced ({} stored) to {}",
        format_count(report.views_declared as usize, "view"),
        format_count(report.total_stored as usize, "change"),
        report.remote_name
    );
    if !report.attestations.is_empty() {
        summary.push_str(&format!(
            ", {} synced",
            format_count(report.attestations.len(), "attestation")
        ));
    }
    if !report.provenance.is_empty() {
        summary.push_str(&format!(
            ", {} synced",
            format_count(report.provenance.len(), "provenance graph")
        ));
    }
    if !report.tags.is_empty() {
        summary.push_str(&format!(
            ", {} synced",
            format_count(report.tags.len(), "tag")
        ));
    }
    print_success(&summary);
    Ok(true)
}

/// `atomic pull` over PullChanges — every form. Same client-side auth
/// model as push; the handler performs the network sync and the applies,
/// and the CLI renders the local pull body's exact lines.
pub fn pull(args: &super::pull::Pull) -> CliResult<bool> {
    use crate::output::{hash as style_hash, hint, success as style_success, view as style_view};
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let (remote_name, remote_url) = sync_resolve_remote(&session, &args.remote)?;

    println!(
        "Pulling from {} ({})",
        style_view(&remote_name),
        hint(&remote_url)
    );
    let auth_headers = sync_auth_headers(&remote_url, args.identity.as_deref());
    stage_line("Connected");

    let request = pb::PullChangesRequest {
        repository: Some(session.reference.clone()),
        meta: request_meta(),
        remote: remote_name.clone(),
        import_sidecars: true,
        to_view: args.to_view.clone(),
        from_view: args.from_view.clone(),
        dry_run: args.dry_run,
        all: args.all,
        insecure: args.insecure,
        timeout_secs: args.timeout,
        identity: args.identity.clone(),
        download_only: args.download_only,
        auth_headers: auth_headers.clone(),
        remote_url: Some(remote_url.clone()),
    };
    let response = match session.pull_changes(request) {
        Ok(response) => response,
        Err(error) => return Err(sync_error_map(error)),
    };
    let Some(report) = response.report else {
        return Err(CliError::Internal(anyhow::anyhow!(
            "the service returned no pull report"
        )));
    };
    let format_count = super::pull::helpers::format_count;

    // The local history + fetch stage lines.
    stage_line(&format!(
        "Loaded {} local view changes ({} graph objects present)",
        report.local_view_changes, report.graph_objects
    ));
    stage_line(&format!("Got {} remote changes", report.remote_changes));

    // State comparison.
    println!();
    let remote_state_text = match &report.remote_state {
        Some(state) if !state.is_empty() => format!(
            "at {} ({} {})",
            hint(&state[..12.min(state.len())]),
            report.remote_changes,
            if report.remote_changes == 1 {
                "change"
            } else {
                "changes"
            }
        ),
        _ => hint("(empty)").to_string(),
    };
    let local_state_text = match &report.local_tip {
        Some(tip) if report.local_view_changes > 0 => format!(
            "at {} ({} {})",
            hint(&tip[..12.min(tip.len())]),
            report.local_view_changes,
            if report.local_view_changes == 1 {
                "change"
            } else {
                "changes"
            }
        ),
        _ => hint("(empty)").to_string(),
    };
    println!(
        "  {}: {} {}",
        crate::output::info("Remote"),
        style_view(&report.remote_view),
        remote_state_text
    );
    println!(
        "  {}: {} {}",
        crate::output::info("Local"),
        style_view(&report.local_view),
        local_state_text
    );
    println!();

    // Local-only warning.
    if !report.local_only.is_empty() {
        println!();
        print_warning(&format!(
            "You have {} not on the remote:",
            format_count(report.local_only.len(), "local change")
        ));
        for hash in report.local_only.iter().take(5) {
            println!(
                "  {} {}...",
                crate::output::warning("!"),
                &hash[..12.min(hash.len())]
            );
        }
        if report.local_only.len() > 5 {
            println!("  ... and {} more", report.local_only.len() - 5);
        }
        println!();
        print_hint("These changes exist locally but not on the remote.");
        print_hint("Use 'atomic push' to upload them, or they will remain local-only.");
    }

    // Dry run.
    if args.dry_run {
        if report.downloads.is_empty() {
            print_success("Already up to date - nothing to pull");
            return Ok(true);
        }
        println!(
            "Would pull {} from {} (view: {}):",
            format_count(report.downloads.len(), "change"),
            report.remote_name,
            report.remote_view
        );
        println!();
        for change in &report.downloads {
            let hash = short_hash(&change.hash);
            let message = change.message.clone().unwrap_or_default();
            let message = if message.is_empty() {
                "(no message)".to_string()
            } else {
                message
            };
            println!("  {} {}", style_hash(&hash), message);
        }
        println!();
        print_hint(&format!("Remote URL: {}", report.remote_url));
        return Ok(true);
    }

    // Downloads.
    if !report.downloads.is_empty() {
        println!(
            "Downloading {}:",
            format_count(report.downloads.len(), "change")
        );
        println!();
        for warning in &report.warnings {
            print_warning(&warning.text);
        }
        let _ = &report.warnings;
        for download in &report.downloads {
            let hash = short_hash(&download.hash);
            let message = download.message.clone().unwrap_or_default();
            let message = if message.is_empty() {
                "(no message)".to_string()
            } else {
                message
            };
            if download.ok {
                println!(
                    "  {} {} ({}/{}) {}",
                    style_success("✓"),
                    style_hash(&hash),
                    download.index + 1,
                    download.total,
                    message
                );
            } else {
                println!(
                    "  {} {} ({}/{}) {} - {}",
                    crate::output::error("✗"),
                    style_hash(&hash),
                    download.index + 1,
                    download.total,
                    message,
                    download.error.clone().unwrap_or_default(),
                );
            }
        }
        stage_line(&format!(
            "Downloaded {} ({})",
            format_count(report.changes_downloaded as usize, "change"),
            super::pull::helpers::format_bytes(report.bytes_transferred)
        ));
    }

    // Download-only.
    if report.download_only {
        for sidecar in &report.sidecars {
            if sidecar.kind == "provenance" {
                println!(
                    "  {} {}",
                    style_success("✓"),
                    format_count(sidecar.count as usize, "provenance graph")
                );
            } else {
                println!(
                    "  {} {}",
                    style_success("✓"),
                    format_count(sidecar.count as usize, "attestation")
                );
            }
        }
        println!();
        print_success(&format!(
            "Downloaded {} (not inserted - use 'atomic insert' to insert)",
            format_count(report.changes_downloaded as usize, "change")
        ));
        return Ok(true);
    }

    // Reconciliation.
    println!();
    if report.apply_errors.is_empty() {
        stage_line(&format!(
            "Reconciled {} view closures",
            report.reconciled_views
        ));
    } else {
        eprintln!("✗ View metadata reconciliation failed");
        for warning in &report.apply_errors {
            print_warning(warning);
        }
    }
    for sidecar in &report.sidecars {
        if sidecar.kind == "provenance" {
            println!(
                "  {} {}",
                style_success("✓"),
                format_count(sidecar.count as usize, "provenance graph")
            );
        } else {
            println!(
                "  {} {}",
                style_success("✓"),
                format_count(sidecar.count as usize, "attestation")
            );
        }
    }
    for warning in &report.warnings {
        print_warning(&warning.text);
    }

    if report.applied {
        if report.set_id_verified {
            print_success("Verified: local view set-id matches the remote (convergent)");
        }
        if let Some(warning) = &report.set_id_warning {
            print_warning(warning);
        }
        if report.materialized {
            stage_line(&format!("{} files updated", report.files_written));
        } else if let Some(error) = &report.materialize_error {
            eprintln!("✗ Failed to update working copy");
            print_warning(&format!(
                "Applied {} but failed to update working copy: {}",
                format_count(report.applied_changes as usize, "change"),
                error
            ));
        } else {
            // A pull into a non-current view: the local hint (no spinner —
            // the working copy is untouched).
            print_hint(&format!(
                "Applied {} to view '{}'. Run 'atomic view switch {}' to check it out.",
                format_count(report.applied_changes as usize, "change"),
                report.local_view,
                report.local_view
            ));
        }
    }

    for tag in &report.tags {
        println!("  {} tag '{}' ({})", style_success("✓"), tag.name, tag.kind);
    }
    if !report.tags.is_empty() {
        print_info(&format!(
            "Downloaded {}",
            format_count(report.tags.len(), "tag")
        ));
    }

    // Summary.
    println!();
    if report.changes_failed > 0
        || !report.apply_errors.is_empty()
        || report.materialize_error.is_some()
    {
        print_warning(&format!(
            "Pull completed with errors: {} downloaded, {} failed to download, {} failed to apply",
            report.changes_downloaded,
            report.changes_failed,
            report.apply_errors.len(),
        ));
        return Err(CliError::Internal(anyhow::anyhow!(
            "pull completed with errors ({} download, {} apply{})",
            report.changes_failed,
            report.apply_errors.len(),
            if report.materialize_error.is_some() {
                ", working copy not updated"
            } else {
                ""
            },
        )));
    }
    print_success(&format!(
        "Pull complete: {} downloaded and applied to {}",
        format_count(report.changes_downloaded as usize, "change"),
        report.local_view
    ));
    Ok(true)
}

// ---------------------------------------------------------------------------
// sandbox (SandboxService) — the local working-tree surface
// ---------------------------------------------------------------------------

/// `atomic sandbox create` over CreateSandboxTree — every form. The
/// handler resolves the destination (the same default), creates the draft
/// when --from names one, provisions the copy-on-write tree, and the CLI
/// prints the local report from the wire-carried outcome.
pub fn sandbox_create(args: &super::sandbox::Create) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::CreateSandboxTreeRequest {
        repository: Some(session.reference.clone()),
        meta: request_meta(),
        name: args.name.clone(),
        from_view: args.from.clone(),
        dest: args.dest.as_ref().map(|dest| dest.display().to_string()),
        view: args.view.clone(),
    };
    let response = session.create_sandbox_tree(request)?;
    println!("Sandbox '{}' created", args.name);
    println!("  Working tree: {}", response.resolved_dest);
    if response.created_draft {
        println!(
            "  View:         {} (new draft from '{}')",
            response.view,
            args.from.as_deref().unwrap_or("")
        );
    } else {
        println!("  View:         {}", response.view);
    }
    println!(
        "  Files cloned: {} (copy-on-write where supported)",
        response.files_cloned
    );
    println!(
        "  Graph:        shared (canonical {}/.atomic)",
        response.repo_root
    );
    Ok(true)
}

/// `atomic sandbox stage` over StageSandboxImage — the base view, output
/// dir, and the staged report all ride the wire.
pub fn sandbox_stage(args: &super::sandbox::Stage) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::StageSandboxImageRequest {
        repository: Some(session.reference.clone()),
        meta: request_meta(),
        name: args.view.clone(),
        base_view: args.base.clone(),
        out_dir: args.out.display().to_string(),
    };
    let response = session.stage_sandbox_image(request)?;
    println!("Staged '{}' (delta over '{}')", args.view, args.base);
    println!("  Image:        {}", response.image_dir);
    println!("  Manifest:     {}", response.manifest_digest);
    println!("  Base layer:   {}", response.base_diff_id);
    println!("  Delta files:  {}", response.delta_files);
    Ok(true)
}

/// `atomic sandbox seal` over SealSandboxImage — the entrypoint/env and
/// output dir ride the wire; the CLI prints the local sealed report.
pub fn sandbox_seal(args: &super::sandbox::Seal) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::SealSandboxImageRequest {
        repository: Some(session.reference.clone()),
        meta: request_meta(),
        name: args.view.clone(),
        out_dir: args.out.display().to_string(),
        entrypoint: args.entrypoint.clone(),
        env: args.env.clone(),
    };
    let response = session.seal_sandbox_image(request)?;
    println!("Sealed '{}'", args.view);
    println!("  Image:        {}", response.image_dir);
    println!("  Manifest:     {}", response.manifest_digest);
    println!("  Files:        {}", response.files);
    Ok(true)
}

// ---------------------------------------------------------------------------
// top-level split (CreateView's FromView semantics + SwitchView)
// ---------------------------------------------------------------------------

/// `atomic split` over CreateView (FromView) + SwitchView — the wrapper
/// composes the same wire `view create --from` uses, and the CLI prints
/// the SPLIT body's report lines from the created view's info.
pub fn split(args: &super::split::Split) -> CliResult<bool> {
    // The pure name validation stays client-side (the local body's first
    // check, with the same messages).
    super::split::validate_view_name(&args.name)
        .map_err(|message| CliError::InvalidArgument { message })?;
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    // The source view default reads the working-copy marker (the same
    // client-side default the local body applies).
    let source = args
        .source
        .clone()
        .unwrap_or_else(|| current_view_name().unwrap_or_default());
    let request = pb::CreateViewRequest {
        repository: Some(session.reference.clone()),
        meta: None,
        name: args.name.clone(),
        base: Some(pb::create_view_request::Base::FromView(source.clone())),
        scope: pb::ViewScope::Unspecified as i32,
        base_snapshot: None,
    };
    let response = session.create_view(request)?;
    let view = response.view.expect("created view info");
    let change_count = view.change_count;
    if change_count > 0 {
        print_success(&format!(
            "Created view: {} (split from {} with {} changes)",
            crate::output::view(&args.name),
            crate::output::view(&source),
            change_count
        ));
    } else {
        print_success(&format!(
            "Created view: {} (split from {} - empty)",
            crate::output::view(&args.name),
            crate::output::view(&source)
        ));
    }
    if args.switch {
        let switched = session.switch_view(pb::SwitchViewRequest {
            repository: Some(session.reference.clone()),
            meta: None,
            view: args.name.clone(),
            bypass_dirty_check: None,
        })?;
        print_success(&format!(
            "Switched to view: {} ({} files updated)",
            args.name, switched.files_written
        ));
    } else {
        print_hint(&format!(
            "Use 'atomic view switch {}' to switch to the new view",
            args.name
        ));
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// agent explain (ProvenanceService.ExplainTurns)
// ---------------------------------------------------------------------------

/// `atomic agent explain` over ExplainTurns — the session record resolves
/// CLIENT-SIDE (the session store is a working-copy file read, not redb)
/// and its fields ride the request; the handler loads the turns, extracts
/// the transcripts, generates the reasoning via the Claude CLI, anchors
/// the learnings, and performs the save-backs. The CLI renders the local
/// body's exact lines from the wire-carried per-turn results.
pub fn agent_explain(args: &super::agent::explain::Explain) -> CliResult<bool> {
    use atomic_agent::turn::session::SessionStore;
    let repo_root = crate::commands::find_repository_root()?;
    let session_store =
        SessionStore::for_repo(&repo_root).map_err(|e| CliError::InvalidRepository {
            reason: format!("Failed to open session store: {}", e),
        })?;
    let session = session_store
        .load(&args.session_id)
        .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to load session: {}", e)))?
        .ok_or_else(|| CliError::InvalidArgument {
            message: format!(
                "Session '{}' not found. Use 'atomic agent status' to see available sessions.",
                args.session_id
            ),
        })?;
    let Some(service_session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::ExplainTurnsRequest {
        repository: Some(service_session.reference.clone()),
        meta: request_meta(),
        session_id: args.session_id.clone(),
        turn: args.turn,
        all: args.all,
        save: args.save,
        model: args.model.clone(),
        view_name: session.view_name.clone(),
        transcript_path: session
            .transcript_path
            .as_ref()
            .map(|path| path.display().to_string()),
        files_touched: session.files_touched.clone(),
        agent_name: session.agent_name.clone(),
    };
    let response = service_session.explain_turns(request)?;

    if response.session_has_no_turns {
        println!(
            "Session '{}' has no recorded turns on view '{}'.",
            args.session_id, session.view_name,
        );
        return Ok(true);
    }

    // The Claude CLI availability check — the local body's exact early-out.
    let generator = atomic_agent::transcript::ClaudeCliGenerator::new().with_model(&args.model);
    use atomic_agent::transcript::ReasoningGenerator as _;
    if !generator.is_available() {
        crate::output::print_error(
            "Claude CLI not found. Install from https://docs.anthropic.com/en/docs/claude-code",
        );
        println!();
        println!("The 'explain' command uses Claude CLI to generate reasoning summaries.");
        println!("Make sure 'claude' is in your PATH and authenticated.");
        return Ok(true);
    }

    println!(
        "{} {} turn{} from session {}",
        crate::output::emphasis("Explaining"),
        response.turns.len(),
        if response.turns.len() == 1 { "" } else { "s" },
        crate::output::hint(&args.session_id),
    );
    println!();

    for result in &response.turns {
        println!(
            "{}",
            crate::output::emphasis(&format!("Turn {} — {}", result.turn_number, result.message))
        );
        if result.no_transcript {
            print_warning("  No transcript available for this turn — skipping");
            println!();
            continue;
        }
        if let Some(error) = &result.generation_error {
            if !result.reasoning.is_some() {
                print!("  Generating reasoning via Claude CLI ({})...", args.model);
                if error.is_empty() {
                    println!(" ✓");
                    print_warning("  Reasoning is empty — nothing to show");
                } else {
                    println!(" ✗");
                    crate::output::print_error(&format!("  Failed: {}", error));
                }
                println!();
                continue;
            }
        }
        print!("  Generating reasoning via Claude CLI ({})...", args.model);
        println!(" ✓");
        let Some(bundle) = &result.reasoning else {
            continue;
        };
        if bundle.schema != "atomic.agent.reasoning.v1" {
            return Err(CliError::Internal(anyhow::anyhow!(
                "unknown reasoning bundle schema '{}'",
                bundle.schema
            )));
        }
        let reasoning: atomic_agent::transcript::TurnReasoning =
            serde_json::from_slice(&bundle.payload).map_err(|error| {
                CliError::Internal(anyhow::anyhow!("reasoning bundle: {error}"))
            })?;
        if result.anchored > 0 {
            println!(
                "  {} {}/{} code learnings anchored to graph",
                crate::output::hint("⚓"),
                result.anchored,
                reasoning.learnings.code.len()
            );
        }
        super::agent::explain::print_reasoning(&reasoning);
        if args.save {
            if result.saved {
                let hash = full_hash(&result.change);
                print_success(&format!(
                    "  Saved reasoning to change {} (will be included on push)",
                    &hash[..12.min(hash.len())]
                ));
            } else if let Some(error) = &result.save_error {
                print_warning(&format!("  Failed to save reasoning to change: {}", error));
            }
            if let Some(context) = &result.context_saved {
                print_success(&format!("  {}", context));
            } else if let Some(error) = &result.context_save_error {
                print_warning(&format!(
                    "  Failed to save learnings to context file: {}",
                    error
                ));
            }
        }
        println!();
    }

    if !args.save {
        let context_file = atomic_agent::learnings::context_file_for_agent(&session.agent_name);
        println!(
            "{}",
            crate::output::hint(&format!(
                "Use --save to persist reasoning in the change and append learnings to {}.",
                context_file
            ))
        );
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// intent delete/link, memory write, goal lifecycle (VaultService)
// ---------------------------------------------------------------------------

/// `atomic intent delete` over DeleteVaultEntity — the existence
/// pre-check rides a GetVaultEntry (a read), the confirm prompt runs
/// client-side, and the handler enforces the same unstarted-backlog guard.
pub fn intent_delete(args: &super::intent::delete::IntentDelete) -> CliResult<bool> {
    // The existence pre-check before the prompt: the local body's
    // vault_intent_show read.
    {
        let Some(session) = Service::open()? else {
            return Ok(false);
        };
        let request = pb::GetVaultEntryRequest {
            repository: Some(session.reference.clone()),
            kind: Kind::Intent as i32,
            id: args.id.clone(),
            path: None,
            include_bundle: false,
            view: None,
        };
        session.get_vault_entry(request)?;
    }
    if !args.force {
        let prompt = format!(
            "Discard intent '{}'? This removes the unstarted draft from the vault.",
            args.id
        );
        let confirmed = dialoguer::Confirm::new()
            .with_prompt(&prompt)
            .default(false)
            .interact()
            .map_err(|e| CliError::Internal(anyhow::anyhow!("Prompt failed: {}", e)))?;
        if !confirmed {
            return Err(CliError::Cancelled);
        }
    }
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::DeleteVaultEntityRequest {
        repository: Some(session.reference.clone()),
        meta: request_meta(),
        kind: Kind::Intent as i32,
        id: args.id.clone(),
        view: None,
        expected_snapshot: None,
    };
    let response = session.delete_vault_entity(request)?;
    if args.json {
        let json = serde_json::json!({
            "id": response.id,
            "intent_file": response.vault_path.trim_start_matches(".vault/"),
            "deleted": true,
        });
        println!("{}", serde_json::to_string_pretty(&json).unwrap());
    } else {
        println!("Deleted intent: {}", response.id);
        println!("  file: {}", response.vault_path);
    }
    Ok(true)
}

/// `atomic intent link` over LinkVaultEntities — the intent→goal link the
/// local body's vault_intent_link performs.
pub fn intent_link(args: &super::intent::link::IntentLink) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::LinkVaultEntitiesRequest {
        repository: Some(session.reference.clone()),
        meta: request_meta(),
        source: Some(pb::VaultEntityRef {
            kind: Kind::Intent as i32,
            id: args.id.clone(),
        }),
        target: Some(pb::VaultEntityRef {
            kind: Kind::Goal as i32,
            id: args.goal.clone(),
        }),
        view: None,
        expected_snapshot: None,
    };
    session.link_vault_entities(request)?;
    println!("Linked goal '{}' to intent '{}'", args.goal, args.id);
    Ok(true)
}

/// `atomic memory write` over UpdateVaultEntity's MemoryWrite arm — the
/// stdin read + frontmatter stay client-side (interactive/file-only);
/// the store + materialize run handler-side and the content hash rides
/// the response.
pub fn memory_write(args: &super::memory::write::MemoryWrite) -> CliResult<bool> {
    // Freeform body from stdin — no lift/gate; this is the raw path.
    use std::io::Read;
    let mut content = String::new();
    std::io::stdin()
        .read_to_string(&mut content)
        .map_err(CliError::Io)?;
    let path = crate::commands::memory::bridge::normalize_memory_path(&args.name);
    let frontmatter = serde_json::json!({
        "name": args.name.replace(".md", ""),
        "type": args.r#type,
    })
    .to_string();
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::UpdateVaultEntityRequest {
        repository: Some(session.reference.clone()),
        meta: request_meta(),
        kind: Kind::Memory as i32,
        id: args.name.clone(),
        update: Some(UpdateKind::MemoryWrite(pb::MemoryWriteAction {
            path: path.clone(),
            content: content.into_bytes(),
            frontmatter_json: frontmatter,
        })),
        expected: None,
        view: None,
    };
    let response = session.update_vault_entity(request)?;
    let hash = response
        .write_hash
        .as_ref()
        .and_then(|hash| hash.value.clone().try_into().ok())
        .map(|bytes: [u8; 32]| Merkle(bytes).to_base32())
        .unwrap_or_default();
    println!("Wrote memory: {} ({})", path, &hash[..12]);
    Ok(true)
}

/// `atomic vault goal start` over CreateVaultEntity (kind=GOAL) —
/// GoalStartOptions' fields ride the request and the created goal's
/// dir/file ride the response.
pub fn vault_goal_start(args: &super::vault::goal::GoalStart) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::CreateVaultEntityRequest {
        repository: Some(session.reference.clone()),
        meta: request_meta(),
        kind: Kind::Goal as i32,
        title: args.name.clone().unwrap_or_default(),
        body: None,
        memory_kind: None,
        derived_from: Vec::new(),
        about: Vec::new(),
        developer: args.developer.clone(),
        intent: args.intent.clone(),
        model: args.model.clone(),
        view: None,
        expected_snapshot: None,
    };
    let response = session.create_vault_entity(request)?;
    let entry = response.entry.expect("created goal entry");
    if args.json {
        let json = serde_json::json!({
            "name": entry.id,
            "goal_dir": entry.goal_dir.clone().unwrap_or_default(),
            "goal_file": entry.goal_file.clone().unwrap_or_default(),
        });
        println!("{}", serde_json::to_string_pretty(&json).unwrap());
    } else {
        println!("Started goal: {}", entry.id);
        println!(
            "  directory: .vault/{}",
            entry.goal_dir.clone().unwrap_or_default()
        );
    }
    Ok(true)
}

/// `atomic vault goal stop` over UpdateVaultEntity's GoalStop arm — the
/// handler resolves the most recent active goal when none is named and
/// performs the stop; the outcome status + removed paths ride the
/// response.
pub fn vault_goal_stop(args: &super::vault::goal::GoalStop) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::UpdateVaultEntityRequest {
        repository: Some(session.reference.clone()),
        meta: request_meta(),
        kind: Kind::Goal as i32,
        id: String::new(),
        update: Some(UpdateKind::GoalStop(pb::GoalStopAction {
            goal: args.goal.clone(),
            promote: args.promote,
            discard: args.discard,
        })),
        expected: None,
        view: None,
    };
    let response = session.update_vault_entity(request)?;
    let entry = response.entry.expect("stopped goal entry");
    let status = response.stop_status.clone().unwrap_or_default();
    match status.as_str() {
        "discarded" => println!(
            "Discarded goal: {} ({} files removed)",
            entry.id,
            response.paths_removed.unwrap_or(0)
        ),
        "completed" => println!("Completed goal: {}", entry.id),
        status => println!("Suspended goal: {} (status: {})", entry.id, status),
    }
    Ok(true)
}

/// `atomic vault goal resume` over UpdateVaultEntity's GoalResume arm.
pub fn vault_goal_resume(args: &super::vault::goal::GoalResume) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::UpdateVaultEntityRequest {
        repository: Some(session.reference.clone()),
        meta: request_meta(),
        kind: Kind::Goal as i32,
        id: String::new(),
        update: Some(UpdateKind::GoalResume(pb::GoalResumeAction {
            goal: args.goal.clone(),
        })),
        expected: None,
        view: None,
    };
    let response = session.update_vault_entity(request)?;
    let entry = response.entry.expect("resumed goal entry");
    if args.json {
        let json = serde_json::json!({
            "name": entry.id,
            "developer": entry.developer.clone().unwrap_or_default(),
            "status": entry.status_label.clone().unwrap_or_default(),
            "intent": entry.linked.first().cloned(),
            "started_at": entry.started_at.clone().unwrap_or_default(),
            "turns": entry.turns.unwrap_or(0),
        });
        println!("{}", serde_json::to_string_pretty(&json).unwrap());
    } else {
        println!("Resumed goal: {}", entry.id);
        if !entry.developer.clone().unwrap_or_default().is_empty() {
            println!(
                "  developer: {}",
                entry.developer.clone().unwrap_or_default()
            );
        }
        if let Some(intent) = entry.linked.first() {
            println!("  intent: {}", intent);
        }
    }
    Ok(true)
}

/// `atomic vault goal show` over GetVaultEntry (kind=GOAL) — the stored
/// goal.md content + frontmatter ride the bundle; the CLI renders the
/// local JSON/plain projection over them.
pub fn vault_goal_show(args: &super::vault::goal::GoalShow) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::GetVaultEntryRequest {
        repository: Some(session.reference.clone()),
        kind: Kind::Goal as i32,
        id: args.goal.clone(),
        path: None,
        include_bundle: true,
        view: None,
    };
    let response = session.get_vault_entry(request)?;
    let Some(bundle) = response
        .entry_bundle
        .as_ref()
        .map(parse_entry_bundle)
        .transpose()?
    else {
        return Err(CliError::Internal(anyhow::anyhow!(
            "the service returned no entry bundle (include_bundle required)"
        )));
    };
    let entry = &bundle.entry;
    if args.json {
        let json = serde_json::json!({
            "name": args.goal,
            "content": String::from_utf8_lossy(&entry.content_bytes),
            "frontmatter": serde_json::from_str::<serde_json::Value>(
                &entry.frontmatter_json
            ).unwrap_or_default(),
        });
        println!("{}", serde_json::to_string_pretty(&json).unwrap());
    } else {
        print!("{}", String::from_utf8_lossy(&entry.content_bytes));
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// vault materialize / summaries (VaultService)
// ---------------------------------------------------------------------------

/// `atomic vault materialize` over ExportVault — the handler inflates the
/// vault entries to markdown in the working copy (the same domain calls),
/// and the CLI prints the local report.
pub fn vault_materialize(args: &super::vault::materialize::Materialize) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::ExportVaultRequest {
        repository: Some(session.reference.clone()),
        meta: request_meta(),
        kinds: Vec::new(),
        destination: None,
        path: args.path.clone(),
        view: None,
    };
    let response = session.export_vault(request)?;
    match &response.materialized_path {
        Some(path) => println!("Materialized: {}", path),
        None => println!("Materialized {} vault entries.", response.exported),
    }
    Ok(true)
}

/// `atomic vault summaries` over ListVaultEntries with the previews arm —
/// the handler enumerates the ToolResult entries under the goal prefix
/// with 200-char content previews; the CLI renders the same JSON map.
pub fn vault_summaries(args: &super::vault::summaries::Summaries) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let prefix = match &args.goal {
        Some(name) => format!("goals/{}/", name),
        None => "goals/".to_string(),
    };
    let request = pb::ListVaultEntriesRequest {
        repository: Some(session.reference.clone()),
        kind: None,
        status: None,
        summaries_only: true,
        cursor: None,
        budget: None,
        view: None,
        identity: None,
        path_prefix: Some(prefix),
        entry_type: None,
        status_filter: None,
        tool_result_previews: true,
    };
    let response = session.list_vault_entries(request)?;
    let mut summaries = serde_json::Map::new();
    for entry in &response.entries {
        summaries.insert(
            entry.id.clone(),
            serde_json::Value::String(entry.body.clone().unwrap_or_default()),
        );
    }
    let output = serde_json::Value::Object(summaries);
    println!("{}", serde_json::to_string_pretty(&output).unwrap());
    Ok(true)
}

// ---------------------------------------------------------------------------
// session fork/rebuild (ProvenanceService)
// ---------------------------------------------------------------------------

/// `atomic session fork` over ForkSession — the domain fork runs
/// handler-side; both manifest hashes ride the wire for the local report.
pub fn session_fork(args: &super::session::SessionFork) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::ForkSessionRequest {
        repository: Some(session.reference.clone()),
        meta: request_meta(),
        session_id: args.parent_session_id.clone(),
        forked_session_id: args.child.clone(),
        at_turn: Some(args.at_turn),
    };
    let response = session.fork_session(request)?;
    println!("Forked session {}", args.parent_session_id);
    println!(
        "  Parent manifest: {}",
        full_hash(&response.parent_manifest)
    );
    println!("  Child session: {}", args.child);
    println!("  Child manifest: {}", full_hash(&response.child_manifest));
    println!("  Fork turn: {}", args.at_turn);
    Ok(true)
}

/// `atomic session rebuild` over RebuildSessionIndex — the counts ride
/// the wire for the local report.
pub fn session_rebuild() -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::RebuildSessionIndexRequest {
        repository: Some(session.reference.clone()),
        meta: request_meta(),
    };
    let response = session.rebuild_session_index(request)?;
    println!("Session index rebuild complete");
    println!("  Indexed: {}", response.indexed);
    println!("  Already present: {}", response.already_present);
    println!("  Corrupt (skipped): {}", response.corrupt);
    Ok(true)
}

// ---------------------------------------------------------------------------
// query KG extras: graph / callers / plan / ask / embed / reindex
// ---------------------------------------------------------------------------

/// `atomic query graph` over QueryGraph (GraphSlice) — the seed/expand/
/// filter/cap build runs handler-side; the CLI renders JSON/DOT/HTML from
/// the wire-carried subgraph (emit_dot/emit_html stay client-side — pure
/// presentation) and writes/opens the output the same way.
pub fn query_graph(args: &super::query::QueryGraph) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::QueryGraphRequest {
        repository: Some(session.reference.clone()),
        limit: 0,
        query: Some(pb::query_graph_request::Query::GraphSlice(
            pb::GraphSliceQuery {
                node_ids: Vec::new(),
                max_depth: None,
                query: Some(args.query.clone()),
                seed_limit: Some(args.limit as u32),
                depth: Some(args.depth.min(2) as u32),
                kinds: Some(args.kinds.clone()),
                changes_per_seed: Some(args.changes_per_seed as u32),
                max_nodes: args.max_nodes.map(|max| max as u32),
            },
        )),
        view: None,
    };
    let response = session.query_graph(request)?;
    let nodes = response
        .nodes
        .iter()
        .map(kg_node_domain)
        .collect::<Vec<_>>();
    let edges = response
        .edges
        .iter()
        .map(kg_edge_domain)
        .collect::<Vec<_>>();
    if nodes.is_empty() {
        if args.json {
            println!("{{\"nodes\":[],\"edges\":[]}}");
        } else {
            eprintln!("No results for {:?} (kinds: {}).", args.query, args.kinds);
        }
        return Ok(true);
    }
    // Cap note: the wire carries only the final set; the note's collected
    // count is the final length when a cap applied (0 = uncapped).
    let capped = nodes.len() >= args.max_nodes.unwrap_or(usize::MAX);
    if capped {
        eprintln!(
            "Note: {} nodes collected, showing top {}. Use --max-nodes to adjust.",
            nodes.len(),
            args.max_nodes.unwrap_or(nodes.len()),
        );
    }
    if args.json {
        let output = serde_json::json!({
            "nodes": nodes,
            "edges": edges,
        });
        let json_str = serde_json::to_string_pretty(&output).unwrap();
        if let Some(ref path) = args.output {
            std::fs::write(path, &json_str)
                .map_err(|e| CliError::Internal(anyhow::anyhow!("write failed: {e}")))?;
            eprintln!(
                "Wrote {} nodes, {} edges to {}",
                nodes.len(),
                edges.len(),
                path
            );
        } else {
            println!("{}", json_str);
        }
    } else if args.dot {
        let dot_str = super::query::emit_dot(&nodes, &edges);
        if let Some(ref path) = args.output {
            std::fs::write(path, &dot_str)
                .map_err(|e| CliError::Internal(anyhow::anyhow!("write failed: {e}")))?;
            eprintln!(
                "Wrote {} nodes, {} edges to {}",
                nodes.len(),
                edges.len(),
                path
            );
        } else {
            print!("{}", dot_str);
        }
    } else {
        let html = super::query::emit_html(&args.query, &nodes, &edges);
        if let Some(ref path) = args.output {
            std::fs::write(path, &html)
                .map_err(|e| CliError::Internal(anyhow::anyhow!("write failed: {e}")))?;
            eprintln!(
                "Wrote {} nodes, {} edges to {}",
                nodes.len(),
                edges.len(),
                path
            );
        } else {
            let tmp_dir = std::env::temp_dir();
            let tmp_path = tmp_dir.join("atomic-graph.html");
            std::fs::write(&tmp_path, &html)
                .map_err(|e| CliError::Internal(anyhow::anyhow!("write failed: {e}")))?;
            eprintln!(
                "Opening graph ({} nodes, {} edges) in browser...",
                nodes.len(),
                edges.len()
            );
            let _ = std::process::Command::new("open").arg(&tmp_path).status();
        }
    }
    Ok(true)
}

/// `atomic query callers` over QueryGraph (Callers) — the handler filters
/// the 1-hop subgraph to CALLS edges at the entity; the CLI renders the
/// local caller lines and hints.
pub fn query_callers(args: &super::query::QueryCallers) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::QueryGraphRequest {
        repository: Some(session.reference.clone()),
        limit: 0,
        query: Some(pb::query_graph_request::Query::Callers(pb::CallersQuery {
            node_id: args.entity_id.clone(),
        })),
        view: None,
    };
    let response = session.query_graph(request)?;
    let caller_nodes = response
        .nodes
        .iter()
        .map(kg_node_domain)
        .collect::<Vec<_>>();
    let caller_edges = response
        .edges
        .iter()
        .map(kg_edge_domain)
        .collect::<Vec<_>>();

    if args.json {
        let out: Vec<serde_json::Value> = caller_edges
            .iter()
            .map(|edge| {
                let node = caller_nodes.iter().find(|node| node.id == edge.from_id);
                serde_json::json!({
                    "caller_id": edge.from_id,
                    "summary": node.and_then(|node| node.summary.as_deref()),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
        return Ok(true);
    }

    if caller_edges.is_empty() {
        println!("No callers found for '{}'.", args.entity_id);
        if !response.calls_present {
            println!(
                "Hint: run `atomic vault query enrich` to populate CALLS edges, \
                then retry."
            );
        } else {
            println!("This function is an entry point — nothing in the repo calls it.");
        }
    } else {
        println!("{} caller(s) of {}:\n", caller_edges.len(), args.entity_id);
        for edge in &caller_edges {
            let display = if let Some(rest) = edge.from_id.strip_prefix("entity:") {
                let last = rest.rfind(':').unwrap_or(rest.len());
                let inner = &rest[..last];
                let sig = caller_nodes
                    .iter()
                    .find(|node| node.id == edge.from_id)
                    .and_then(|node| node.summary.as_deref())
                    .map(|summary| format!("  [{}]", summary));
                format!("  {}{}", inner, sig.unwrap_or_default())
            } else {
                format!("  {}", edge.from_id)
            };
            println!("{display}");
        }
    }
    Ok(true)
}

/// `atomic query plan` over QueryGraph (Plan) — the structured plan
/// executes handler-side; the CLI renders the domain PlanResult JSON
/// byte-for-byte (--json) or the human summary.
pub fn query_plan(args: &super::query::PlanExec) -> CliResult<bool> {
    // Read plan from stdin — client-side (interactive/file-only).
    use std::io::Read;
    let mut plan_json = String::new();
    std::io::stdin()
        .read_to_string(&mut plan_json)
        .map_err(CliError::Io)?;
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::QueryGraphRequest {
        repository: Some(session.reference.clone()),
        limit: 0,
        query: Some(pb::query_graph_request::Query::Plan(
            pb::PlanExecutionQuery {
                plan_json: plan_json.into_bytes(),
            },
        )),
        view: None,
    };
    let response = session.query_graph(request)?;
    let Some(payload) = response.plan_result else {
        return Err(CliError::Internal(anyhow::anyhow!(
            "the service returned no plan result"
        )));
    };
    let result: atomic_repository::PlanResult = serde_json::from_slice(&payload)
        .map_err(|error| CliError::Internal(anyhow::anyhow!("plan result: {error}")))?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&result).unwrap());
    } else {
        println!(
            "Plan executed in {}ms ({} steps)\n",
            result.elapsed_ms,
            result.step_stats.len()
        );
        for (index, stat) in result.step_stats.iter().enumerate() {
            println!(
                "  Step {}: {} → {} results ({}ms)",
                index + 1,
                stat.step_type,
                stat.result_count,
                stat.elapsed_ms
            );
        }
        if !result.nodes.is_empty() {
            println!("\nNodes ({}):", result.nodes.len());
            for node in &result.nodes {
                print!("  [{}] {}", node.kind, node.label);
                if let Some(summary) = &node.summary {
                    let short: String = summary.chars().take(60).collect();
                    print!(": {}", short);
                }
                println!();
            }
        }
        if !result.edges.is_empty() {
            println!("\nEdges ({}):", result.edges.len());
            for edge in result.edges.iter().take(20) {
                println!("  {} -[{}]-> {}", edge.from_id, edge.kind, edge.to_id);
            }
            if result.edges.len() > 20 {
                println!("  ... and {} more", result.edges.len() - 20);
            }
        }
        if !result.content.is_empty() {
            println!("\nContent ({} entries):", result.content.len());
            for (path, text) in &result.content {
                let preview: String = text.chars().take(100).collect();
                println!("  {}: {}...", path, preview.replace('\n', " "));
            }
        }
    }
    Ok(true)
}

/// `atomic query ask` over QueryGraph (RagAsk) — the agentic loop runs
/// handler-side; the CLI renders the same answer/stats/JSON lines.
/// --verbose's live tool-call trace becomes an after-the-fact trace over
/// the wire (the unary-RPC limit, documented in the dispatch guard).
pub fn query_ask(args: &super::query::QueryAsk) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::QueryGraphRequest {
        repository: Some(session.reference.clone()),
        limit: 0,
        query: Some(pb::query_graph_request::Query::RagAsk(pb::RagAskQuery {
            question: args.question.clone(),
            top_k: 0,
            max_turns: Some(args.max_turns as u32),
            verbose: Some(args.verbose),
        })),
        view: None,
    };
    let response = session.query_graph(request)?;
    let Some(bundle) = response
        .ask_result
        .as_ref()
        .filter(|bytes| bytes.schema == "atomic.query.ask.v1")
    else {
        return Err(CliError::Internal(anyhow::anyhow!(
            "the service returned no ask result"
        )));
    };
    let result: atomic_repository::AgentResult = serde_json::from_slice(&bundle.payload)
        .map_err(|error| CliError::Internal(anyhow::anyhow!("ask result: {error}")))?;
    if args.json {
        let json = serde_json::json!({
            "query": args.question,
            "answer": result.answer,
            "model": result.model,
            "turns": result.turns,
            "input_tokens": result.total_usage.input_tokens,
            "output_tokens": result.total_usage.output_tokens,
            "tool_trace": result.tool_trace,
        });
        println!("{}", serde_json::to_string_pretty(&json).unwrap());
    } else {
        println!("{}\n", result.answer);
        let cache_info = if result.total_usage.cache_read_tokens > 0 {
            format!(
                ", {} cache read + {} cache write",
                result.total_usage.cache_read_tokens, result.total_usage.cache_creation_tokens
            )
        } else {
            String::new()
        };
        let secs = (response.ask_elapsed_ms.unwrap_or(0)) as f64 / 1000.0;
        let duration = if secs < 60.0 {
            format!("{:.1}s", secs)
        } else {
            format!("{}m {:.0}s", (secs / 60.0) as u64, secs % 60.0)
        };
        println!(
            "  — {} ({} turns, {}, {} in + {} out tokens{})",
            result.model,
            result.turns,
            duration,
            result.total_usage.input_tokens,
            result.total_usage.output_tokens,
            cache_info,
        );
    }
    Ok(true)
}

/// `atomic query embed` over MaintainKnowledgeGraph (EMBED) — the
/// provider resolves handler-side from the same environment/config; the
/// CLI prints the local embed report.
pub fn query_embed(args: &super::query::QueryEmbed) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::MaintainKnowledgeGraphRequest {
        repository: Some(session.reference.clone()),
        meta: request_meta(),
        action: pb::KnowledgeMaintainAction::Embed as i32,
        scope: args.path.clone(),
        changes: Vec::new(),
    };
    let response = session.maintain_knowledge_graph(request)?;
    println!(
        "Using embedding provider: {} ({}d)",
        response.provider.clone().unwrap_or_default(),
        response.dimensions.unwrap_or(0)
    );
    match &args.path {
        Some(path) => {
            if response.processed == 0 {
                println!("Content unchanged, skipped: {}", path);
            } else {
                println!("Embedded {} chunks: {}", response.processed, path);
            }
        }
        None => println!("Embedded {} total chunks.", response.processed),
    }
    Ok(true)
}

/// `atomic query reindex` over MaintainKnowledgeGraph (REINDEX) — the
/// handler runs vault_reindex_kg; the CLI prints the local report.
pub fn query_reindex() -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::MaintainKnowledgeGraphRequest {
        repository: Some(session.reference.clone()),
        meta: request_meta(),
        action: pb::KnowledgeMaintainAction::Reindex as i32,
        scope: None,
        changes: Vec::new(),
    };
    let response = session.maintain_knowledge_graph(request)?;
    println!("Indexed {} nodes + edges.", response.processed);
    Ok(true)
}

// ---------------------------------------------------------------------------
// vault init / project init (bootstrap boundary)
// ---------------------------------------------------------------------------

/// `atomic vault init` over InitVault — the vault init operates inside an
/// EXISTING repository (it requires one), so it routes normally via the
/// daemon: the handler runs has_vault → init_vault → the recursive track
/// → the defaults record, and the CLI prints the local report.
pub fn vault_init() -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::InitVaultRequest {
        repository: Some(session.reference.clone()),
        meta: request_meta(),
        view: None,
        expected_snapshot: None,
    };
    let response = session.init_vault(request)?;
    if response.already_initialized {
        print_info("Vault is already initialized");
        return Ok(true);
    }
    print_info(&format!(
        "Initialized vault at {}",
        response.vault_dir.clone().unwrap_or_default()
    ));
    if response.recorded {
        print_success("Recorded vault defaults");
    }
    Ok(true)
}

/// `atomic project init` — the server-side project creation (the
/// management StorageClient built from the global config + the identity
/// store — config/auth reads, not redb, per the push/pull ruling) stays
/// client-side; the ONE repository mutation (add_remote origin) routes
/// through ManageRemotes so the command never opens redb directly.
pub fn project_init(args: &super::project::init::ProjectInit) -> CliResult<bool> {
    // Client-side: build the storage client (config + identity reads, no
    // redb) and run the server-side workspace/project creation exactly as
    // the local body does.
    let rt = tokio::runtime::Runtime::new().map_err(|e| {
        CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {}", e))
    })?;
    let outcome = rt.block_on(async {
        let (client, org_slug) =
            crate::commands::client::build_client_with_org(args.org.as_deref(), None).await?;
        let workspace =
            crate::commands::client::resolve_workspace(&org_slug, args.workspace.as_deref(), None)?;

        // Ensure the workspace exists — create it if necessary.
        // A 409 (Conflict) means it already exists, which is fine.
        match client.get_workspace(&workspace).await {
            Ok(_) => {}
            Err(_) => {
                let ws_req = atomic_remote::storage_types::CreateWorkspaceRequest {
                    name: workspace.clone(),
                    description: None,
                    visibility: args.visibility,
                };
                match client.create_workspace(&ws_req).await {
                    Ok(ws) => {
                        print_success(&format!("Created workspace: {}", ws.slug));
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        if msg.contains("409")
                            || msg.contains("conflict")
                            || msg.contains("Conflict")
                        {
                            print_info(&format!(
                                "Workspace '{}' already exists, using it.",
                                workspace
                            ));
                        } else {
                            return Err(crate::commands::client::remote_err(e));
                        }
                    }
                }
            }
        }

        let proj_req = atomic_remote::storage_types::CreateProjectRequest {
            name: args.name.clone(),
            description: args.description.clone(),
            default_view: "dev".to_string(),
            kind: args.kind.clone(),
            visibility: args.visibility,
        };
        let project = client
            .create_project(&workspace, &proj_req)
            .await
            .map_err(crate::commands::client::remote_err)?;
        let vcs_url = format!(
            "{}/workspaces/{}/projects/{}/code",
            client.base_url(),
            workspace,
            project.slug,
        );
        Ok::<_, CliError>((project.name, project.slug, vcs_url))
    })?;

    // The repository mutation routes through the service layer
    // (ManageRemotes) — no direct redb open on any path this command
    // can reach.
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    session.manage_remotes(pb::ManageRemotesRequest {
        repository: Some(session.reference.clone()),
        meta: request_meta(),
        action: pb::RemoteAction::Add as i32,
        name: Some("origin".to_string()),
        url: Some(outcome.2.clone()),
        new_name: None,
        set_default: false,
    })?;

    let (name, slug, vcs_url) = outcome;
    print_success(&format!("Created project: {} (slug: {})", name, slug));
    println!("  Remote URL: {}", vcs_url);
    crate::output::print_next_steps(&[
        (
            "atomic record -m \"Initial commit\"",
            "Record your first change",
        ),
        ("atomic push", "Push changes to the server"),
    ]);
    Ok(true)
}

// ---------------------------------------------------------------------------
// triage (TriageService)
// ---------------------------------------------------------------------------

/// The wire-carried candidate set — the domain JSON the local body's
/// renders consume.
pub(crate) struct TriageCandidateSet {
    pub feature: String,
    pub target: String,
    pub payload: Vec<u8>,
}

/// `atomic triage candidates` over ListTriageCandidates — the handler
/// resolves the (feature, target) pair (the same defaults and errors the
/// local resolve_views applies) and runs the repository's candidate-set
/// domain call; the CLI renders the same plain/JSON reports over the
/// wire-carried domain payload.
pub(crate) fn triage_candidates(
    args: &super::triage::TriageCandidates,
) -> CliResult<Option<TriageCandidateSet>> {
    let Some(session) = Service::open()? else {
        return Ok(None);
    };
    let request = pb::ListTriageCandidatesRequest {
        repository: Some(session.reference.clone()),
        from_view: args.feature.clone(),
        to_view: args.into.clone(),
    };
    let response = session.list_triage_candidates(request)?;
    let Some(bundle) = response
        .candidate_set
        .as_ref()
        .filter(|bytes| bytes.schema == "atomic.triage.candidates.v1")
    else {
        return Err(CliError::Internal(anyhow::anyhow!(
            "the service returned no candidate set bundle"
        )));
    };
    Ok(Some(TriageCandidateSet {
        feature: response.feature,
        target: response.target,
        payload: bundle.payload.clone(),
    }))
}

// ---------------------------------------------------------------------------
// tags (TagService)
// ---------------------------------------------------------------------------

/// `triage review` over GenerateTriageReview — the full report bundle (the
/// verdict, findings, intents, criteria, changes, walkthrough), versioned
/// `atomic.triage.report.v1`, so the CLI renders the same report over the
/// service as the local builder produced.
pub(crate) fn triage_review(
    args: &super::triage::TriageReview,
) -> CliResult<Option<atomic_repository::triage::TriageReport>> {
    let Some(session) = Service::open()? else {
        return Ok(None);
    };
    let request = pb::GenerateTriageReviewRequest {
        repository: Some(session.reference.clone()),
        from_view: args.feature.clone().unwrap_or_default(),
        to_view: args.into.clone().unwrap_or_default(),
        report: None,
    };
    let response = session.generate_triage_review(request)?;
    let Some(bundle) = response
        .report
        .as_ref()
        .filter(|bytes| bytes.schema == "atomic.triage.report.v1")
    else {
        return Err(CliError::Internal(anyhow::anyhow!(
            "the service returned no triage report bundle"
        )));
    };
    let report = serde_json::from_slice(&bundle.payload)
        .map_err(|e| CliError::Internal(anyhow::anyhow!("the wire report does not parse: {e}")))?;
    Ok(Some(report))
}

/// One wire tag → the domain record shape the local render functions
/// consume (name, view, sequence, state, timestamp, annotation, kind,
/// metadata).
pub(crate) struct WireTag {
    pub name: String,
    pub view: String,
    pub sequence: u64,
    pub state: atomic_core::types::Merkle,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub message: Option<String>,
    pub author_name: Option<String>,
    pub author_email: Option<String>,
    pub kind: String,
    pub metadata: Option<String>,
}

impl WireTag {
    pub fn is_annotated(&self) -> bool {
        self.message.is_some() || self.author_name.is_some()
    }
}

/// One ReviewGate record's presence — the wire-carried form of what the
/// local render reads from the repository (`has_change` + view membership).
pub(crate) struct TagRecordWire {
    pub hash: String,
    pub parseable: bool,
    pub present: bool,
    pub views: Vec<String>,
}

/// Wire TagInfo → the domain tag shape (the state decodes to the Merkle the
/// local renders print base32; an absent timestamp falls back to now, which
/// never happens with the same-build service).
pub(crate) fn wire_tag(tag: &pb::TagInfo) -> WireTag {
    WireTag {
        name: tag.name.clone(),
        view: tag.view.clone(),
        sequence: tag.sequence,
        state: tag
            .state
            .as_ref()
            .and_then(|h| h.value.clone().try_into().ok())
            .map(atomic_core::types::Merkle)
            .unwrap_or_default(),
        timestamp: tag
            .created_at
            .as_ref()
            .map(|stamp| {
                chrono::DateTime::from_timestamp(stamp.seconds, stamp.nanos.max(0) as u32)
                    .unwrap_or_else(chrono::Utc::now)
            })
            .unwrap_or_else(chrono::Utc::now),
        message: tag.message.clone(),
        author_name: tag.author.as_ref().map(|author| author.name.clone()),
        author_email: tag.author.as_ref().and_then(|author| author.email.clone()),
        kind: tag.kind.clone(),
        metadata: tag.metadata.clone(),
    }
}

/// `atomic tag create` over CreateTag — every form. The handler reproduces
/// the local body's flow exactly (the existing-tag pre-check, the --force
/// delete, the Release kind), and the wire carries the created record so
/// the CLI prints the local create report.
pub fn tag_create(args: &super::tag::create::Create) -> CliResult<bool> {
    let Some(name) = args.name.clone() else {
        return Err(CliError::InvalidArgument {
            message: "Tag name is required".to_string(),
        });
    };
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::CreateTagRequest {
        repository: Some(session.reference.clone()),
        meta: request_meta(),
        name,
        state: None,
        message: args.message.clone(),
        view: None,
        expected_snapshot: None,
        force: args.force,
    };
    let response = session.create_tag(request)?;
    let tag = wire_tag(response.tag.as_ref().expect("created tag"));
    if tag.is_annotated() {
        print_success(&format!(
            "Created annotated tag: {}",
            crate::output::emphasis(&tag.name)
        ));
    } else {
        print_success(&format!(
            "Created tag: {}",
            crate::output::emphasis(&tag.name)
        ));
    }
    Ok(true)
}

/// `atomic tag delete` over DeleteTag — the wire carries whether the tag
/// existed, so the CLI prints the local delete report.
pub fn tag_delete(args: &super::tag::delete::Delete) -> CliResult<bool> {
    let Some(name) = args.name.clone() else {
        return Err(CliError::InvalidArgument {
            message: "Tag name is required".to_string(),
        });
    };
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::DeleteTagRequest {
        repository: Some(session.reference.clone()),
        meta: request_meta(),
        name,
        expected_state: None,
    };
    let name = request.name.clone();
    let response = session.delete_tag(request)?;
    if response.deleted {
        print_success(&format!("Deleted tag: {}", crate::output::emphasis(&name)));
    } else {
        println!(
            "{}",
            crate::output::hint(&format!("Tag '{}' does not exist", name))
        );
    }
    Ok(true)
}

/// `atomic tag list` over ListTags — every form: the view/pattern/
/// annotated-only filters ride the request (the handler applies the local
/// body's exact filter semantics over the domain listing), and the CLI
/// renders the local plain/verbose lines from the wire-carried records.
pub fn tag_list(args: &super::tag::list::List) -> CliResult<bool> {
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::ListTagsRequest {
        repository: Some(session.reference.clone()),
        view: args.view.clone(),
        pattern: args.pattern.clone(),
        annotated_only: args.annotated_only,
    };
    let response = session.list_tags(request)?;
    let tags = response.tags.iter().map(wire_tag).collect::<Vec<_>>();
    if tags.is_empty() {
        println!(
            "{}",
            crate::output::hint("No tags found. Use 'atomic tag create <name>' to create one.")
        );
        return Ok(true);
    }
    let max_name_len = tags.iter().map(|t| t.name.len()).max().unwrap_or(0);
    use atomic_core::types::Base32;
    for tag in &tags {
        let annotated_marker = if tag.is_annotated() { "*" } else { " " };
        if args.verbose {
            let full = tag.state.to_base32();
            let state_short = if full.len() > 12 {
                format!("{}...", &full[..12])
            } else {
                full
            };
            let date = tag.timestamp.format("%Y-%m-%d").to_string();
            println!(
                "{}{:<width$}  (seq: {:>4})  state: {}  {}",
                annotated_marker,
                crate::output::emphasis(&tag.name),
                tag.sequence,
                state_short,
                date,
                width = max_name_len
            );
        } else {
            println!("{}{}", annotated_marker, crate::output::emphasis(&tag.name));
        }
    }
    Ok(true)
}

/// `atomic tag show` over GetTag — the wire carries the full record plus
/// the ReviewGate per-record presence/membership the local render reads,
/// so the CLI prints the local detail report.
pub fn tag_show(args: &super::tag::show::Show) -> CliResult<bool> {
    let Some(name) = args.name.clone() else {
        return Err(CliError::InvalidArgument {
            message: "Tag name is required".to_string(),
        });
    };
    let Some(session) = Service::open()? else {
        return Ok(false);
    };
    let request = pb::GetTagRequest {
        repository: Some(session.reference.clone()),
        name,
    };
    let name = request.name.clone();
    let response = session.get_tag(request)?;
    let Some(info) = response.tag else {
        println!(
            "{}",
            crate::output::hint(&format!(
                "Tag '{}' not found. Use 'atomic tag list' to see available tags.",
                name
            ))
        );
        return Ok(true);
    };
    let tag = wire_tag(&info);
    use atomic_core::types::Base32;
    let emphasis = crate::output::emphasis;
    println!("{}: {}", emphasis("Tag"), tag.name);
    println!("{}: {}", emphasis("View"), tag.view);
    println!("{}: {}", emphasis("Sequence"), tag.sequence);
    println!("{}: {}", emphasis("State"), tag.state.to_base32());
    println!(
        "{}: {}",
        emphasis("Created"),
        tag.timestamp.format("%Y-%m-%d %H:%M:%S UTC")
    );
    println!("{}: {}", emphasis("Kind"), tag.kind);
    println!(
        "{}: {}",
        emphasis("Type"),
        if tag.is_annotated() {
            "annotated"
        } else {
            "lightweight"
        }
    );
    if let Some(message) = &tag.message {
        println!("{}: {}", emphasis("Message"), message);
    }
    if let Some(author) = &tag.author_name {
        let author_str = match &tag.author_email {
            Some(email) => format!("{author} <{email}>"),
            None => author.clone(),
        };
        println!("{}: {}", emphasis("Author"), author_str);
    }
    if let Some(metadata) = &tag.metadata {
        if tag.kind == "review-gate" {
            let records = response
                .records
                .iter()
                .map(|record| TagRecordWire {
                    hash: record.hash.clone(),
                    parseable: record.parseable,
                    present: record.present,
                    views: record.views.clone(),
                })
                .collect::<Vec<_>>();
            super::tag::show::render_review_gate_records(metadata, &records);
        } else {
            println!("{}: {}", emphasis("Metadata"), metadata);
        }
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// agent receive (the Reactor bridge)
// ---------------------------------------------------------------------------

/// Dispatch one typed TurnEvent through the service area — the SAME
/// handler both transports serve: local runs libatomic's
/// `ProvenanceImpl::dispatch_turn_event` in this process (the orchestrator
/// with the in-process journal core), reactor dispatches over the socket
/// (D4 start-or-retry). There is no unreachable fallback path anymore —
/// a failure is a real failure and surfaces to the caller.
pub fn dispatch_turn_event(
    root: &Path,
    event: &atomic_agent::event::TurnEvent,
    agent_id: &str,
    agent_display: &str,
    agent_identity: Option<String>,
) -> CliResult<crate::commands::agent::receive::ReceiveResult> {
    let Some(session) = Service::open_root(root)? else {
        return Err(CliError::InvalidArgument {
            message: format!("'{}' is not an Atomic repository", root.display()),
        });
    };
    let response = session.dispatch_turn_event(pb::DispatchTurnEventRequest {
        repository: Some(session.reference.clone()),
        meta: request_meta(),
        agent_id: agent_id.to_string(),
        agent_display_name: Some(agent_display.to_string()),
        agent_identity,
        event: Some(turn_event_body(event)),
    })?;
    Ok(crate::commands::agent::receive::ReceiveResult {
        session_id: response.session_id,
        recorded: response.recorded,
        change_hash: response.change_hash.as_ref().map(|hash| {
            let bytes: [u8; 32] = hash.value.clone().try_into().unwrap_or([0; 32]);
            Merkle(bytes).to_base32()
        }),
        view: response.view,
        files: response.files,
        warnings: response.warnings,
    })
}

fn turn_event_body(event: &atomic_agent::event::TurnEvent) -> pb::TurnEventBody {
    pb::TurnEventBody {
        session_id: event.session_id.clone(),
        event_type: match event.event_type {
            atomic_agent::event::HookType::SessionStart => "session_start",
            atomic_agent::event::HookType::SessionEnd => "session_end",
            atomic_agent::event::HookType::TurnStart => "turn_start",
            atomic_agent::event::HookType::TurnEnd => "turn_end",
            atomic_agent::event::HookType::PreToolUse => "pre_tool_use",
            atomic_agent::event::HookType::PostToolUse => "post_tool_use",
        }
        .to_string(),
        prompt: event.prompt.clone(),
        tool_name: event.tool_name.clone(),
        tool_use_id: event.tool_use_id.clone(),
        timestamp: Some(timestamp_proto(event.timestamp)),
        raw_json: event
            .raw_json
            .as_ref()
            .and_then(|value| serde_json::to_vec(value).ok()),
    }
}

fn timestamp_proto(time: chrono::DateTime<chrono::Utc>) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: time.timestamp(),
        nanos: time.timestamp_subsec_nanos() as i32,
    }
}

/// Resolve the repository root from an explicit path argument (the Reactor
/// always passes the canonical root).
pub fn root_from(path: &Path) -> CliResult<PathBuf> {
    crate::commands::find_repository_root_from(path)
}

#[cfg(test)]
mod change_ledger_tests {
    use super::*;
    use atomic_core::change::ProvenanceGraph;

    fn fixture() -> (pb::VersionedBytes, pb::Hash) {
        let graph = ProvenanceGraph::builder("session", "opencode").build();
        (
            pb::VersionedBytes {
                schema: "atomic.prov.graph.v1".into(),
                payload: serde_json::to_vec(&graph).unwrap(),
            },
            pb::Hash {
                value: Merkle::of(b"original stored artifact").0.to_vec(),
                algorithm: pb::HashAlgorithm::Blake3 as i32,
            },
        )
    }

    #[test]
    fn preserves_stored_identity_instead_of_rehashing_upgraded_graph() {
        let (bytes, hash) = fixture();
        let result = decode_change_ledger(&[bytes], std::slice::from_ref(&hash)).unwrap();
        assert_eq!(result[0].0.as_bytes().as_slice(), hash.value);
        assert_ne!(result[0].0, Merkle::of(&result[0].1.serialize().unwrap()));
        assert!(decode_change_ledger(&[], &[]).unwrap().is_empty());
    }

    #[test]
    fn refuses_missing_or_extra_hashes_including_older_services() {
        let (bytes, hash) = fixture();
        let error = decode_change_ledger(&[bytes], &[]).unwrap_err().to_string();
        assert!(error.contains("ledger/hash count mismatch"));
        assert!(error.contains("upgrade"));
        assert!(decode_change_ledger(&[], &[hash]).is_err());
    }

    #[test]
    fn refuses_malformed_hashes_and_unknown_algorithms() {
        let (bytes, hash) = fixture();
        for invalid in [
            pb::Hash {
                value: vec![1; 31],
                ..hash.clone()
            },
            pb::Hash {
                value: vec![1; 33],
                ..hash.clone()
            },
            pb::Hash {
                algorithm: 0,
                ..hash.clone()
            },
            pb::Hash {
                algorithm: 99,
                ..hash
            },
        ] {
            assert!(decode_change_ledger(std::slice::from_ref(&bytes), &[invalid]).is_err());
        }
    }

    #[test]
    fn refuses_invalid_graphs_instead_of_silently_dropping_ledger_entries() {
        let (bytes, hash) = fixture();
        for invalid in [
            pb::VersionedBytes {
                payload: b"not JSON".to_vec(),
                ..bytes.clone()
            },
            pb::VersionedBytes {
                schema: "atomic.prov.graph.future".into(),
                ..bytes
            },
        ] {
            let (valid, _) = fixture();
            assert!(
                decode_change_ledger(&[valid, invalid], &[hash.clone(), hash.clone()]).is_err()
            );
        }
    }
}
