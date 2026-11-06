//! Correctness-checked native graph/span materialization baseline.
//! Run optimized builds through tools/materialize-bench/run.py; setup and
//! verification are outside sample timing. This is not a cold-OS-cache test.
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, ensure, Context, Result};
use atomic_core::change::ChangeHeader;
use atomic_core::operation::OperationScope;
use atomic_core::pristine::span_patch_storage::SpanRenderContext;
use atomic_core::pristine::{
    CapabilityMutTxnT, GraphVisibilityClosure, MutTxnT, TreeTxnT, ViewTxnT, SPAN_RANGE_CAPABILITY,
};
use atomic_repository::{RecordOptions, Repository};
use serde::{Deserialize, Serialize};

const CASES: &[&str] = &[
    "render_session",
    "render_reopened",
    "materialize_noop",
    "materialize_missing",
    "switch_one_forward",
    "switch_one_back",
    "switch_many_forward",
    "switch_many_back",
];

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Config {
    output: PathBuf,
    files: usize,
    lines: usize,
    edits: usize,
    iterations: usize,
    warmups: usize,
    allow_debug: bool,
    keep_fixtures: bool,
}

impl Config {
    fn parse() -> Result<Self> {
        let mut config = Self {
            output: "materialize-bench.json".into(),
            files: 24,
            lines: 128,
            edits: 24,
            iterations: 5,
            warmups: 1,
            allow_debug: false,
            keep_fixtures: false,
        };
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--allow-debug" => config.allow_debug = true,
                "--keep-fixtures" => config.keep_fixtures = true,
                "--output" => config.output = args.next().context("--output needs a path")?.into(),
                "--files" => {
                    config.files = args.next().context("--files needs a value")?.parse()?
                }
                "--lines" => {
                    config.lines = args.next().context("--lines needs a value")?.parse()?
                }
                "--edits" => {
                    config.edits = args.next().context("--edits needs a value")?.parse()?
                }
                "--iterations" => {
                    config.iterations =
                        args.next().context("--iterations needs a value")?.parse()?
                }
                "--warmups" => {
                    config.warmups = args.next().context("--warmups needs a value")?.parse()?
                }
                _ => bail!("unknown argument: {arg}"),
            }
        }
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.files > 0 && self.lines >= 2 && self.iterations > 0,
            "files and iterations must be positive; lines must be at least two"
        );
        ensure!(
            self.files <= 1024 && self.lines <= 100_000 && self.edits <= 10_000,
            "fixture exceeds benchmark safety bounds"
        );
        ensure!(
            self.iterations <= 1000
                && self.warmups <= 1000
                && self
                    .files
                    .checked_mul(self.lines)
                    .is_some_and(|n| n <= 2_000_000),
            "sample count or total fixture lines exceed safety bounds"
        );
        ensure!(
            !cfg!(debug_assertions) || self.allow_debug,
            "use a --release build (or --allow-debug for correctness smoke tests only)"
        );
        ensure!(
            !self.output.exists(),
            "refusing to overwrite an existing output report"
        );
        Ok(())
    }
}

#[derive(Clone)]
struct Golden {
    path: String,
    content: Vec<u8>,
    mode: u32,
}

fn file_mode(path: &Path) -> Result<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(fs::metadata(path)?.permissions().mode() & 0o7777)
    }
    #[cfg(not(unix))]
    {
        Ok(u32::from(fs::metadata(path)?.permissions().readonly()))
    }
}

fn record(repo: &Repository, message: &str, tick: &mut i64) -> Result<()> {
    let mut header = ChangeHeader::new(message);
    header.timestamp = chrono::DateTime::from_timestamp(1_600_000_000 + *tick, 0).unwrap();
    *tick += 1;
    repo.record(
        repo.require_working_copy_id()?,
        header,
        RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true)
            .sync_vault(false)
            .enrich_kg(false),
    )?;
    Ok(())
}

struct Fixture {
    directory: tempfile::TempDir,
    repo: Option<Repository>,
    backend: &'static str,
    golden: BTreeMap<String, Vec<Golden>>,
    setup_ns: u128,
}

impl Fixture {
    fn new(parent: &Path, config: &Config, span: bool) -> Result<Self> {
        let started = Instant::now();
        let directory = tempfile::Builder::new()
            .prefix(if span { "span-" } else { "graph-" })
            .tempdir_in(parent)?;
        let root = directory.path();
        let mut repo = Repository::init(root)?;
        if span {
            let mut txn = repo.pristine().write_txn()?;
            txn.put_required_capability(SPAN_RANGE_CAPABILITY)?;
            txn.commit()?;
        }
        fs::create_dir(root.join("files"))?;
        let mut contents: Vec<Vec<String>> = (0..config.files)
            .map(|file| {
                (0..config.lines)
                    .map(|line| format!("file_{file:04} line_{line:06} value_00000000\n"))
                    .collect()
            })
            .collect();
        for (file, lines) in contents.iter().enumerate() {
            fs::write(
                root.join(format!("files/file_{file:04}.txt")),
                lines.concat(),
            )?;
        }
        fs::write(root.join("empty.txt"), [])?;
        let wc = repo.require_working_copy_id()?;
        repo.add(wc, "files", Default::default())?;
        repo.add(wc, "empty.txt", Default::default())?;
        let mut tick = 0;
        record(&repo, "synthetic baseline", &mut tick)?;
        for edit in 0..config.edits {
            let line = (edit * 17) % config.lines;
            contents[0][line] = format!("file_0000 line_{line:06} value_{:08}\n", edit + 1);
            fs::write(root.join("files/file_0000.txt"), contents[0].concat())?;
            record(&repo, "synthetic hot-file edit", &mut tick)?;
        }
        let mut golden = BTreeMap::new();
        let capture = |content: &[Vec<String>]| -> Result<Vec<Golden>> {
            let mut files = Vec::new();
            for (file, lines) in content.iter().enumerate() {
                let path = format!("files/file_{file:04}.txt");
                files.push(Golden {
                    mode: file_mode(&root.join(&path))?,
                    path,
                    content: lines.concat().into_bytes(),
                });
            }
            files.push(Golden {
                path: "empty.txt".into(),
                content: Vec::new(),
                mode: file_mode(&root.join("empty.txt"))?,
            });
            Ok(files)
        };
        golden.insert("dev".into(), capture(&contents)?);
        repo.create_view_from("bench-near", "dev")?;
        repo.create_view_from("bench-far", "dev")?;
        repo.switch_view(wc, "bench-near")?;
        let mut near = contents.clone();
        near[0][0] = "near view: one changed line\n".into();
        fs::write(root.join("files/file_0000.txt"), near[0].concat())?;
        record(&repo, "synthetic near view", &mut tick)?;
        golden.insert("bench-near".into(), capture(&near)?);
        repo.switch_view(wc, "dev")?;
        repo.switch_view(wc, "bench-far")?;
        let mut far = contents;
        for (file, lines) in far.iter_mut().enumerate() {
            lines[0] = format!("far view: file {file:04} changed\n");
            fs::write(
                root.join(format!("files/file_{file:04}.txt")),
                lines.concat(),
            )?;
        }
        record(&repo, "synthetic many-file view", &mut tick)?;
        golden.insert("bench-far".into(), capture(&far)?);
        repo.switch_view(wc, "dev")?;
        repo.materialize(wc)?;
        let healthy = repo.verify_native_derived_indexes()?;
        ensure!(
            healthy.is_healthy(),
            "fixture indexes unhealthy: {:?}",
            healthy.problems
        );
        let fixture = Self {
            directory,
            repo: Some(repo),
            backend: if span { "span" } else { "graph" },
            golden,
            setup_ns: started.elapsed().as_nanos(),
        };
        fixture.verify("dev")?;
        Ok(fixture)
    }

    fn verify(&self, view: &str) -> Result<()> {
        ensure!(
            self.repo.as_ref().unwrap().current_view() == view,
            "wrong current view"
        );
        for file in &self.golden[view] {
            let path = self.directory.path().join(&file.path);
            ensure!(
                fs::symlink_metadata(&path)?.file_type().is_file(),
                "wrong kind: {}",
                file.path
            );
            ensure!(
                fs::read(&path)? == file.content,
                "wrong bytes: {}",
                file.path
            );
            ensure!(file_mode(&path)? == file.mode, "wrong mode: {}", file.path);
        }
        Ok(())
    }
}

#[derive(Debug, Serialize)]
struct Sample {
    backend: String,
    case: String,
    iteration: usize,
    total_ns: u128,
    phases_ns: BTreeMap<String, u128>,
    files_written: Option<usize>,
    files_skipped: Option<usize>,
    bytes_written: Option<u64>,
    vertices_processed: Option<usize>,
    edges_traversed: Option<usize>,
    verified_files: usize,
    output_bytes: usize,
}

type RenderProbe = (BTreeMap<String, u128>, Vec<Vec<u8>>);

fn render_probe(repo: &Repository, golden: &[Golden], span: bool) -> Result<RenderProbe> {
    let mut phases = BTreeMap::new();
    let started = Instant::now();
    let txn = repo.pristine().read_txn()?;
    let view = txn.get_view("dev")?.context("missing base view")?;
    let visibility =
        GraphVisibilityClosure::try_from_membership(&txn, &txn.view_membership_set(&view)?)?;
    let files = golden
        .iter()
        .map(|file| {
            let inode = txn
                .get_inode(&file.path)?
                .context("missing fixture inode")?;
            let position = txn
                .inode_position(inode)?
                .context("missing fixture position")?;
            Ok((inode, position))
        })
        .collect::<Result<Vec<_>>>()?;
    phases.insert("read_setup".into(), started.elapsed().as_nanos());
    let started = Instant::now();
    let context = if span {
        Some(SpanRenderContext::new(
            &txn,
            repo.change_store(),
            &visibility,
        )?)
    } else {
        None
    };
    phases.insert("span_context".into(), started.elapsed().as_nanos());
    let started = Instant::now();
    let mut rendered = Vec::new();
    for (inode, position) in files {
        let bytes = if let Some(context) = &context {
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
            ensure!(conflicts.is_empty(), "unexpected synthetic graph conflict");
            bytes
        };
        rendered.push(bytes);
    }
    phases.insert("render".into(), started.elapsed().as_nanos());
    Ok((phases, rendered))
}

fn sample(fixture: &mut Fixture, case: &str, iteration: usize) -> Result<Sample> {
    let mut phases = BTreeMap::new();
    let mut written = None;
    let mut skipped = None;
    let mut bytes_written = None;
    let mut vertices_processed = None;
    let mut edges_traversed = None;
    let mut rendered = None;
    if case == "materialize_missing" {
        // Remove only the benchmark's own generated regular files, never a
        // directory tree or a caller-provided repository.
        for file in &fixture.golden["dev"] {
            fs::remove_file(fixture.directory.path().join(&file.path))?;
        }
    }
    if case == "render_reopened" {
        drop(fixture.repo.take());
    }
    let started = Instant::now();
    if case == "render_reopened" {
        let opened = Instant::now();
        fixture.repo = Some(Repository::open(fixture.directory.path())?);
        phases.insert("repository_open".into(), opened.elapsed().as_nanos());
    }
    let repo = fixture.repo.as_mut().unwrap();
    let wc = repo.require_working_copy_id()?;
    let target = match case {
        "switch_one_forward" => "bench-near",
        "switch_many_forward" => "bench-far",
        _ => "dev",
    };
    if case.starts_with("render_") {
        let (measured, content) =
            render_probe(repo, &fixture.golden["dev"], fixture.backend == "span")?;
        phases.extend(measured);
        rendered = Some(content);
    } else {
        let result = if case.starts_with("switch_") {
            repo.switch_view(wc, target)?
        } else {
            repo.materialize(wc)?
        };
        written = Some(result.files_written);
        skipped = Some(result.files_skipped);
        bytes_written = Some(result.bytes_written);
        vertices_processed = Some(result.vertices_processed);
        edges_traversed = Some(result.edges_traversed);
        ensure!(
            !result.has_conflicts() && result.files_truncated == 0,
            "unexpected materialization conflict/truncation"
        );
    }
    let total_ns = started.elapsed().as_nanos();
    // Filesystem correctness and journal verification are never timed.
    if let Some(rendered) = rendered {
        for (bytes, file) in rendered.iter().zip(&fixture.golden["dev"]) {
            ensure!(*bytes == file.content, "render differs: {}", file.path);
        }
    }
    fixture.verify(target)?;
    let log = fixture.repo.as_ref().unwrap().operation_log(
        OperationScope::WorkingCopy(wc),
        Some(1),
        false,
    )?;
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
        "incomplete benchmark operation"
    );
    Ok(Sample {
        backend: fixture.backend.into(),
        case: case.into(),
        iteration,
        total_ns,
        phases_ns: phases,
        files_written: written,
        files_skipped: skipped,
        bytes_written,
        vertices_processed,
        edges_traversed,
        verified_files: fixture.golden[target].len(),
        output_bytes: fixture.golden[target]
            .iter()
            .map(|file| file.content.len())
            .sum(),
    })
}

fn run(config: Config) -> Result<()> {
    config.validate()?;
    let parent = config
        .output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let binary = std::env::current_exe()?;
    let binary_hash = blake3::hash(&fs::read(&binary)?).to_hex().to_string();
    let mut fixtures = [
        Fixture::new(parent, &config, false)?,
        Fixture::new(parent, &config, true)?,
    ];
    for view in ["dev", "bench-near", "bench-far"] {
        let left = &fixtures[0].golden[view];
        let right = &fixtures[1].golden[view];
        ensure!(
            left.iter()
                .zip(right)
                .all(|(a, b)| a.path == b.path && a.content == b.content && a.mode == b.mode),
            "graph/span fixture mismatch for {view}"
        );
    }
    let mut samples = Vec::new();
    for iteration in 0..config.warmups + config.iterations {
        for case in CASES {
            for offset in 0..2 {
                let index = (iteration + offset) % 2;
                let measured = sample(
                    &mut fixtures[index],
                    case,
                    iteration.saturating_sub(config.warmups),
                )
                .with_context(|| {
                    format!("{} {case}, iteration {iteration}", fixtures[index].backend)
                })?;
                if iteration >= config.warmups {
                    samples.push(measured);
                }
            }
        }
    }
    ensure!(
        blake3::hash(&fs::read(&binary)?).to_hex().as_str() == binary_hash,
        "benchmark binary changed during run"
    );
    let setup: Vec<_> = fixtures.iter().map(|f| serde_json::json!({"backend":f.backend,"setup_ns":f.setup_ns,
        "database_bytes_after_samples":fs::metadata(f.directory.path().join(".atomic/atomic.redb")).map(|m|m.len()).ok()})).collect();
    let mut retained = Vec::new();
    for fixture in fixtures {
        drop(fixture.repo);
        if config.keep_fixtures {
            retained.push(fixture.directory.keep());
        }
    }
    let report = serde_json::json!({"schema":1,"config":config,"binary_blake3":binary_hash,
        "debug_build":cfg!(debug_assertions),"platform":{"os":std::env::consts::OS,"arch":std::env::consts::ARCH},
        "fixture_kind":"synthetic native histories; graph versus opt-in span capability; no Git/shim/vault enrichment",
        "cache_policy":"session and reopened Repository handles; OS caches uncontrolled (not cold-cache claims)",
        "setup":setup,"retained_fixtures":retained,"samples":samples});
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&config.output)?;
    serde_json::to_writer_pretty(&mut output, &report)?;
    println!(
        "verified {} paired samples; report {}",
        config.iterations * CASES.len() * 2,
        config.output.display()
    );
    Ok(())
}

fn main() -> Result<()> {
    run(Config::parse()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paired_benchmark_smoke_checks_every_case() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("report.json");
        run(Config {
            output: output.clone(),
            files: 2,
            lines: 8,
            edits: 2,
            iterations: 1,
            warmups: 0,
            allow_debug: true,
            keep_fixtures: false,
        })
        .unwrap();
        let report: serde_json::Value = serde_json::from_slice(&fs::read(output).unwrap()).unwrap();
        assert_eq!(report["samples"].as_array().unwrap().len(), CASES.len() * 2);
        assert!(report["samples"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["verified_files"] == 3));
    }
}
