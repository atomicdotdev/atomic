//! Single-shot, correctness-checked probe of an already imported disposable repo.
//! Cache reset, import, fixture preparation and verification are not API timings.
use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, ensure, Context, Result};
use atomic_core::operation::OperationScope;
use atomic_core::pristine::span_patch_storage::SpanRenderContext;
use atomic_core::pristine::{GraphVisibilityClosure, TreeTxnT, ViewTxnT};
use atomic_repository::Repository;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Deserialize)]
struct Manifest {
    git_tip: String,
    files: Vec<ExpectedFile>,
}

#[derive(Deserialize)]
struct ExpectedFile {
    path: String,
    mode: String,
    sha256: String,
}

#[derive(Serialize)]
struct Report {
    schema: u32,
    git_tip: String,
    backend: String,
    case: String,
    debug_build: bool,
    total_ns: u128,
    phases_ns: BTreeMap<String, u128>,
    span_context_phases_ns: BTreeMap<String, u128>,
    span_context_counts: BTreeMap<String, usize>,
    verified_files: usize,
    rendered_bytes: usize,
    files_written: Option<usize>,
    files_skipped: Option<usize>,
    bytes_written: Option<u64>,
    head_verified: bool,
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn verify_files(root: &Path, files: &[ExpectedFile]) -> Result<()> {
    for file in files {
        let path = root.join(&file.path);
        let metadata = fs::symlink_metadata(&path)?;
        let bytes = if file.mode == "120000" {
            ensure!(metadata.is_symlink(), "expected symlink: {}", file.path);
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStrExt;
                fs::read_link(path)?.as_os_str().as_bytes().to_vec()
            }
            #[cfg(not(unix))]
            bail!("symlink fixture requires Unix");
        } else {
            ensure!(metadata.is_file(), "expected regular file: {}", file.path);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                ensure!(
                    (metadata.permissions().mode() & 0o111 != 0) == (file.mode == "100755"),
                    "executable mode differs: {}",
                    file.path
                );
            }
            fs::read(path)?
        };
        ensure!(sha256(&bytes) == file.sha256, "bytes differ: {}", file.path);
    }
    Ok(())
}

fn run(
    repo_path: &Path,
    manifest_path: &Path,
    output: &Path,
    backend: &str,
    case: &str,
    allow_debug: bool,
) -> Result<()> {
    ensure!(matches!(backend, "graph" | "span"), "invalid backend");
    ensure!(
        matches!(case, "render" | "materialize_missing"),
        "invalid case"
    );
    ensure!(
        !cfg!(debug_assertions) || allow_debug,
        "use a release build"
    );
    ensure!(!output.exists(), "refusing to overwrite report");
    let manifest: Manifest = serde_json::from_slice(&fs::read(manifest_path)?)?;
    ensure!(!manifest.files.is_empty(), "empty fixture");
    let mut names = std::collections::HashSet::new();
    for file in &manifest.files {
        ensure!(
            !file.path.is_empty()
                && Path::new(&file.path)
                    .components()
                    .all(|part| matches!(part, Component::Normal(_))),
            "unsafe path"
        );
        ensure!(names.insert(file.path.clone()), "duplicate path");
        ensure!(
            matches!(file.mode.as_str(), "100644" | "100755" | "120000"),
            "unsupported Git mode"
        );
    }
    let mut report = Report {
        schema: 1,
        git_tip: manifest.git_tip.clone(),
        backend: backend.into(),
        case: case.into(),
        debug_build: cfg!(debug_assertions),
        total_ns: 0,
        phases_ns: BTreeMap::new(),
        span_context_phases_ns: BTreeMap::new(),
        span_context_counts: BTreeMap::new(),
        verified_files: manifest.files.len(),
        rendered_bytes: 0,
        files_written: None,
        files_skipped: None,
        bytes_written: None,
        head_verified: false,
    };
    let mut rendered = Vec::new();
    let started = Instant::now();
    let phase = Instant::now();
    let repo = Repository::open(repo_path)?;
    let wc = repo.require_working_copy_id()?;
    report
        .phases_ns
        .insert("repository_open".into(), phase.elapsed().as_nanos());
    if case == "render" {
        let phase = Instant::now();
        let txn = repo.pristine().read_txn()?;
        let view_name = repo.desired_view_name(wc)?;
        let view = txn.get_view(&view_name)?.context("missing imported view")?;
        let visibility =
            GraphVisibilityClosure::try_from_membership(&txn, &txn.view_membership_set(&view)?)?;
        let files = manifest
            .files
            .iter()
            .map(|file| {
                let inode = txn
                    .get_inode(&file.path)?
                    .context("missing imported inode")?;
                let position = txn
                    .inode_position(inode)?
                    .context("missing imported position")?;
                Ok((inode, position))
            })
            .collect::<Result<Vec<_>>>()?;
        report
            .phases_ns
            .insert("read_setup".into(), phase.elapsed().as_nanos());
        let phase = Instant::now();
        let context = if backend == "span" {
            Some(SpanRenderContext::new(
                &txn,
                repo.change_store(),
                &visibility,
            )?)
        } else {
            None
        };
        report
            .phases_ns
            .insert("span_context".into(), phase.elapsed().as_nanos());
        if let Some(context) = &context {
            let metrics = context.build_metrics();
            report.span_context_phases_ns = [
                ("resolve_changes", metrics.resolve_changes),
                ("load_changes", metrics.load_changes),
                ("verify_hashes", metrics.verify_hashes),
                ("validate_dependencies", metrics.validate_dependencies),
                ("assemble_edits", metrics.assemble_edits),
                ("causal_proof", metrics.causal_proof),
                ("total", metrics.total),
            ]
            .into_iter()
            .map(|(name, duration)| (name.to_string(), duration.as_nanos()))
            .collect();
            report.span_context_counts = [
                ("pipeline_version", usize::from(metrics.pipeline_version)),
                ("changes", metrics.changes),
                ("edits", metrics.edits),
                ("dependency_reads", metrics.dependency_reads),
                ("internal_lookups", metrics.internal_lookups),
                ("internal_hits", metrics.internal_hits),
                ("file_lookups", metrics.file_lookups),
                ("file_hits", metrics.file_hits),
            ]
            .into_iter()
            .map(|(name, count)| (name.to_string(), count))
            .collect();
        }
        let phase = Instant::now();
        for (inode, position) in files {
            let bytes = if let Some(context) = &context {
                // Every selected Git-imported file must have a valid span root;
                // never silently fall back if its index is absent or corrupt.
                context.render(inode)?
            } else {
                let (bytes, conflicts, _) =
                    atomic_core::output::repo::output_file_to_buffer_with_options(
                        &txn,
                        repo.change_store(),
                        position,
                        atomic_core::output::repo::FileOutputOptions::new(),
                        atomic_core::output::alive::RetrieveOptions::new()
                            .with_graph_visibility(visibility.clone()),
                    )?;
                ensure!(conflicts.is_empty(), "unexpected fixture conflict");
                bytes
            };
            report.rendered_bytes += bytes.len();
            rendered.push(bytes);
        }
        report
            .phases_ns
            .insert("render".into(), phase.elapsed().as_nanos());
    } else {
        let phase = Instant::now();
        let result = repo.materialize(wc)?;
        ensure!(
            !result.has_conflicts() && result.files_truncated == 0,
            "conflict or truncation"
        );
        report.files_written = Some(result.files_written);
        report.files_skipped = Some(result.files_skipped);
        report.bytes_written = Some(result.bytes_written);
        report
            .phases_ns
            .insert("materialize".into(), phase.elapsed().as_nanos());
    }
    let phase = Instant::now();
    drop(repo);
    report
        .phases_ns
        .insert("repository_close".into(), phase.elapsed().as_nanos());
    report.total_ns = started.elapsed().as_nanos();

    // All equality and journal checks occur after timing and repository close.
    if case == "render" {
        ensure!(
            rendered.len() == manifest.files.len(),
            "render count differs"
        );
        for (bytes, file) in rendered.iter().zip(&manifest.files) {
            ensure!(
                sha256(bytes) == file.sha256,
                "render differs: {}",
                file.path
            );
        }
    }
    verify_files(repo_path, &manifest.files)?;
    let repo = Repository::open(repo_path)?;
    let log = repo.operation_log(OperationScope::WorkingCopy(wc), Some(1), false)?;
    ensure!(
        matches!(
            log.head_state,
            atomic_repository::repository::OperationHeadState::Single(_)
        ) && log.entries.iter().filter(|entry| entry.is_head).count() == 1
            && log
                .entries
                .iter()
                .filter(|entry| entry.is_head)
                .all(|entry| entry.verification
                    == atomic_repository::repository::OperationVerificationState::Verified),
        "operation head is incomplete or divergent"
    );
    report.head_verified = true;
    let output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)?;
    serde_json::to_writer_pretty(output, &report)?;
    Ok(())
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut repo = None;
    let mut manifest = None;
    let mut output = None;
    let mut backend = None;
    let mut case = None;
    let mut allow_debug = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--repo" => repo = Some(PathBuf::from(args.next().context("missing repo")?)),
            "--manifest" => {
                manifest = Some(PathBuf::from(args.next().context("missing manifest")?))
            }
            "--output" => output = Some(PathBuf::from(args.next().context("missing output")?)),
            "--backend" => backend = args.next(),
            "--case" => case = args.next(),
            "--allow-debug" => allow_debug = true,
            _ => bail!("unknown argument {arg}"),
        }
    }
    run(
        &repo.context("--repo required")?,
        &manifest.context("--manifest required")?,
        &output.context("--output required")?,
        &backend.context("--backend required")?,
        &case.context("--case required")?,
        allow_debug,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::change::ChangeHeader;
    use atomic_core::pristine::{CapabilityMutTxnT, MutTxnT, SPAN_RANGE_CAPABILITY};
    use atomic_repository::RecordOptions;

    #[test]
    fn imported_probe_format_and_verification_smoke() {
        for backend in ["graph", "span"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("repo");
            let repo = Repository::init(&path).unwrap();
            let wc = repo.require_working_copy_id().unwrap();
            if backend == "span" {
                let mut txn = repo.pristine().write_txn().unwrap();
                txn.put_required_capability(SPAN_RANGE_CAPABILITY).unwrap();
                txn.commit().unwrap();
            }
            for name in ["file.txt", "empty.txt"] {
                fs::write(
                    path.join(name),
                    if name == "file.txt" {
                        b"hello\n".as_slice()
                    } else {
                        b""
                    },
                )
                .unwrap();
                repo.add(wc, name, Default::default()).unwrap();
            }
            repo.record(
                wc,
                ChangeHeader::new("smoke"),
                RecordOptions::new()
                    .with_all(true)
                    .save_to_store(true)
                    .apply_after_record(true)
                    .sync_vault(false)
                    .enrich_kg(false),
            )
            .unwrap();
            drop(repo);
            let manifest = dir.path().join("manifest.json");
            fs::write(
                &manifest,
                serde_json::to_vec(
                    &serde_json::json!({"git_tip":"synthetic smoke only", "files":[
                        {"path":"file.txt","mode":"100644","sha256":sha256(b"hello\n")},
                        {"path":"empty.txt","mode":"100644","sha256":sha256(b"")}
                    ]}),
                )
                .unwrap(),
            )
            .unwrap();
            run(
                &path,
                &manifest,
                &dir.path().join("render.json"),
                backend,
                "render",
                true,
            )
            .unwrap();
            let rendered: serde_json::Value =
                serde_json::from_slice(&fs::read(dir.path().join("render.json")).unwrap()).unwrap();
            if backend == "span" {
                assert_eq!(
                    rendered["span_context_phases_ns"]
                        .as_object()
                        .unwrap()
                        .len(),
                    7
                );
                assert!(rendered["span_context_counts"]["changes"].as_u64().unwrap() > 0);
            } else {
                assert!(rendered["span_context_phases_ns"]
                    .as_object()
                    .unwrap()
                    .is_empty());
            }
            fs::remove_file(path.join("file.txt")).unwrap();
            fs::remove_file(path.join("empty.txt")).unwrap();
            let output = dir.path().join("materialize.json");
            run(
                &path,
                &manifest,
                &output,
                backend,
                "materialize_missing",
                true,
            )
            .unwrap();
            let report: serde_json::Value =
                serde_json::from_slice(&fs::read(output).unwrap()).unwrap();
            assert_eq!(report["verified_files"], 2);
            assert_eq!(report["head_verified"], true);
        }
    }
}
