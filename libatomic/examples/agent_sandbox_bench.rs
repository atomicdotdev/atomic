//! Agent-sandbox sync benchmark: cold sandbox over the daemon RPC versus Git
//! over its wire protocol.
//!
//! The scenario: an agent sandbox starts with nothing and fetches the files
//! it needs. Three matched cases on the same synthetic history (two directory
//! subtrees plus an empty file, one commit per hot-file edit):
//!
//! - `atomic_rpc_full` — a fresh sandbox connects to a Unix-socket tonic
//!   server hosting the production `SandboxImpl` handler and streams the
//!   whole view via the `Materialize` RPC (no prefix), writing every entry.
//! - `atomic_rpc_partial` — the same `Materialize` RPC with `path_prefix`
//!   set to one subtree; only that subtree's entries and their ancestor
//!   directories may be streamed.
//! - `git_clone_full` — `git clone` of a bare repository over the `file://`
//!   wire transport.
//!
//! The atomic side exercises the real transport end to end: a fresh channel
//! per sample (a sandbox never reuses a connection), the generated client,
//! the production handlers against a real repository, and the real
//! `write_sandbox_entry` receiver. Only the `atomicd` shell is absent; the
//! socket is bound directly. Only the sandbox token/middleware layer is
//! bypassed (no token is presented, so the handlers run as the trusted
//! local caller).
//!
//! Timing boundaries: the timed region starts at the client connect (or the
//! first Git subprocess spawn) and ends when the last entry is written (or
//! Git exits). Fixtures, server startup, and byte verification are outside
//! timing. There is no network RTT: the atomic transport is a loopback Unix
//! socket and Git uses loopback `file://` pack negotiation; treat absolute
//! numbers as transport-plus-work measurements, not WAN estimates.
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{bail, ensure, Context, Result};
use libatomic::atomic::{sandbox_service_client::SandboxServiceClient, MaterializeRequest};
use libatomic::daemon::services_sandbox::SandboxImpl;
use libatomic::daemon::state::DaemonState;
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::{Endpoint, Uri};
use tower::service_fn;
use hyper_util::rt::TokioIo;

const CASES: &[&str] = &[
    "atomic_rpc_full",
    "atomic_rpc_partial",
    "git_clone_full",
];

/// The partial subtree every partial case fetches: `files/east/`.
const PARTIAL_PREFIX: &str = "files/east";

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Config {
    output: PathBuf,
    files_east: usize,
    files_west: usize,
    lines: usize,
    edits: usize,
    iterations: usize,
    warmups: usize,
    allow_debug: bool,
    keep_fixtures: bool,
    span: bool,
}

impl Config {
    fn parse() -> Result<Self> {
        let mut config = Self {
            output: "agent-sandbox-bench.json".into(),
            files_east: 8,
            files_west: 16,
            lines: 128,
            edits: 24,
            iterations: 8,
            warmups: 1,
            allow_debug: false,
            keep_fixtures: false,
            span: false,
        };
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--allow-debug" => config.allow_debug = true,
                "--span" => config.span = true,
                "--keep-fixtures" => config.keep_fixtures = true,
                "--output" => config.output = args.next().context("--output needs a path")?.into(),
                "--files-east" => {
                    config.files_east = args.next().context("--files-east needs a value")?.parse()?
                }
                "--files-west" => {
                    config.files_west = args.next().context("--files-west needs a value")?.parse()?
                }
                "--lines" => config.lines = args.next().context("--lines needs a value")?.parse()?,
                "--edits" => config.edits = args.next().context("--edits needs a value")?.parse()?,
                "--iterations" => {
                    config.iterations = args.next().context("--iterations needs a value")?.parse()?
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
            self.files_east > 0
                && self.files_west > 0
                && self.lines >= 2
                && self.iterations > 0,
            "file counts and iterations must be positive; lines must be at least two"
        );
        ensure!(
            self.files_east + self.files_west <= 1024
                && self.lines <= 100_000
                && self.edits <= 10_000,
            "fixture exceeds benchmark safety bounds"
        );
        ensure!(
            self.iterations <= 1000
                && self.warmups <= 1000
                && self
                    .files_east
                    .checked_add(self.files_west)
                    .is_some_and(|n| n.checked_mul(self.lines).is_some_and(|n| n <= 2_000_000)),
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

// ————————————————————————————— fixture —————————————————————————————

fn generated_line(tree: &str, file: usize, line: usize) -> String {
    format!("{tree} file_{file:04} line_{line:06} value_00000000\n")
}

fn edited_line(line: usize, edit: usize) -> String {
    format!("east file_0000 line_{line:06} value_{edit:08}\n")
}

/// One tracked file's golden bytes.
#[derive(Clone)]
struct Golden {
    path: String,
    content: Vec<u8>,
}

fn build_golden(config: &Config) -> Vec<Golden> {
    let mut golden = Vec::new();
    for (tree, count) in [
        ("east", config.files_east),
        ("west", config.files_west),
    ] {
        for file in 0..count {
            golden.push(Golden {
                path: format!("files/{tree}/file_{file:04}.txt"),
                content: (0..config.lines)
                    .map(|line| generated_line(tree, file, line))
                    .collect::<String>()
                    .into_bytes(),
            });
        }
    }
    golden.push(Golden {
        path: "empty.txt".into(),
        content: Vec::new(),
    });
    golden
}

fn east_paths(config: &Config) -> Vec<String> {
    (0..config.files_east)
        .map(|file| format!("files/east/file_{file:04}.txt"))
        .collect()
}

fn git(root: &Path, args: &[&str], tick: Option<usize>) -> Result<()> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "bench")
        .env("GIT_AUTHOR_EMAIL", "bench@example.invalid")
        .env("GIT_COMMITTER_NAME", "bench")
        .env("GIT_COMMITTER_EMAIL", "bench@example.invalid")
        .env(
            "GIT_AUTHOR_DATE",
            format!("{} +0000", 1_600_000_000 + tick.unwrap_or(0)),
        )
        .env(
            "GIT_COMMITTER_DATE",
            format!("{} +0000", 1_600_000_000 + tick.unwrap_or(0)),
        )
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    ensure!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

/// Build the atomic host repository and the Git bare repository with the
/// same synthetic content. Returns the atomic golden and the bare path.
fn prepare_fixtures(
    parent: &Path,
    config: &Config,
) -> Result<(tempfile::TempDir, Vec<Golden>, PathBuf)> {
    let directory = tempfile::Builder::new()
        .prefix("agb")
        .tempdir_in(parent)?;
    let root = directory.path();
    let golden = build_golden(config);

    // Atomic host repository. Writes happen through the ordinary record path
    // so the served graph is a real repository, not a canned fixture.
    let repo = atomic_repository::Repository::init(root.join("host"))?;
    if config.span {
        use atomic_core::pristine::{CapabilityMutTxnT, MutTxnT, SPAN_RANGE_CAPABILITY};
        let mut txn = repo.pristine().write_txn()?;
        txn.put_required_capability(SPAN_RANGE_CAPABILITY)?;
        txn.commit()?;
    }
    fs::create_dir_all(root.join("host/files/east"))?;
    fs::create_dir_all(root.join("host/files/west"))?;
    for file in &golden {
        fs::write(root.join("host").join(&file.path), &file.content)?;
    }
    let wc = repo.require_working_copy_id()?;
    repo.add(wc, "files", Default::default())?;
    repo.add(wc, "empty.txt", Default::default())?;
    record(&repo, "synthetic baseline", 0)?;
    // Hot-file edits: every edit rewrites the east hot file and records it,
    // exactly one commit per edit to match the Git history's shape.
    let hot_path = root.join("host/files/east/file_0000.txt");
    let mut hot: Vec<String> = (0..config.lines)
        .map(|l| generated_line("east", 0, l))
        .collect();
    for edit in 0..config.edits {
        let line = (edit * 17) % config.lines;
        hot[line] = edited_line(line, edit + 1);
        let content = hot.concat();
        fs::write(&hot_path, &content)?;
        record(&repo, "synthetic hot-file edit", edit + 1)?;
    }
    let hot = hot.concat();
    // The golden east hot file must reflect the final edit, not the baseline.
    let mut golden = golden;
    let golden_east_hot = golden
        .iter_mut()
        .find(|f| f.path == "files/east/file_0000.txt")
        .context("missing east hot file in golden")?;
    golden_east_hot.content = hot.into_bytes();
    drop(repo);

    // Git bare repository with the same shape: one baseline commit, one
    // commit per hot-file edit. The seed must start from the baseline
    // content — the golden's hot file already reflects the edits.
    let seed = root.join("git-seed");
    fs::create_dir_all(seed.join("files/east"))?;
    fs::create_dir_all(seed.join("files/west"))?;
    let baseline_hot: Vec<Golden> = golden
        .iter()
        .map(|file| {
            if file.path == "files/east/file_0000.txt" {
                Golden {
                    path: file.path.clone(),
                    content: (0..config.lines)
                        .map(|line| generated_line("east", 0, line))
                        .collect::<String>()
                        .into_bytes(),
                }
            } else {
                file.clone()
            }
        })
        .collect();
    for file in &baseline_hot {
        fs::write(seed.join(&file.path), &file.content)?;
    }
    git(&seed, &["init", "-q", "-b", "dev"], None)?;
    git(&seed, &["add", "-A"], None)?;
    git(&seed, &["commit", "-q", "-m", "synthetic baseline"], Some(0))?;
    let seed_hot = seed.join("files/east/file_0000.txt");
    for edit in 0..config.edits {
        let line = (edit * 17) % config.lines;
        let mut lines: Vec<String> = fs::read_to_string(&seed_hot)?
            .split_inclusive('\n')
            .map(String::from)
            .collect();
        lines[line] = edited_line(line, edit + 1);
        fs::write(&seed_hot, lines.concat())?;
        git(&seed, &["add", "-A"], None)?;
        git(&seed, &["commit", "-q", "-m", "synthetic hot-file edit"], Some(edit + 1))?;
    }
    let bare = root.join("host-bare.git");
    git(
        root,
        &["clone", "-q", "--bare", "git-seed", "host-bare.git"],
        None,
    )?;
    // Both histories must have replayed to byte-identical trees.
    let git_hot = fs::read(&seed_hot)?;
    ensure!(
        golden
            .iter()
            .find(|f| f.path == "files/east/file_0000.txt")
            .is_some_and(|f| f.content == git_hot),
        "atomic and Git fixture hot files diverged"
    );
    Ok((directory, golden, bare))
}

fn record(repo: &atomic_repository::Repository, message: &str, tick: usize) -> Result<()> {
    use atomic_core::change::ChangeHeader;
    use atomic_repository::RecordOptions;
    let mut header = ChangeHeader::new(message);
    header.timestamp = chrono::DateTime::from_timestamp(1_600_000_000 + tick as i64, 0).unwrap();
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

// ————————————————————————————— server —————————————————————————————

/// Bind the production sandbox handlers on a private Unix socket. This is
/// the `atomicd` shape minus the shell: one `DaemonState`, one registered
/// repository, the generated tonic server.
fn serve(
    state: Arc<DaemonState>,
    host_root: PathBuf,
    socket: PathBuf,
) -> Result<std::thread::JoinHandle<Result<()>>> {
    use libatomic::atomic::sandbox_service_server::SandboxServiceServer;

    let std_listener = StdUnixListener::bind(&socket)
        .with_context(|| format!("binding {}", socket.display()))?;
    std_listener.set_nonblocking(true)?;
    // Fail fast if the repository cannot open; the server would otherwise
    // serve errors that look like client bugs. The registration also gives
    // the caller the repository ref the handlers will resolve.
    let handle = state.register(host_root);
    handle.repository()?;
    Ok(std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| anyhow::anyhow!("server runtime: {e}"))?;
        runtime.block_on(async move {
            let listener = tokio::net::UnixListener::from_std(std_listener)?;
            let incoming = UnixListenerStream::new(listener);
            tonic::transport::Server::builder()
                .add_service(SandboxServiceServer::new(SandboxImpl { state }))
                .serve_with_incoming(incoming)
                .await
                .map_err(|e| anyhow::anyhow!("tonic server: {e}"))
        })
    }))
}

// ————————————————————————————— client —————————————————————————————

async fn connect(socket: &Path) -> Result<SandboxServiceClient<tonic::transport::Channel>> {
    let socket = socket.to_path_buf();
    let channel = Endpoint::from_static("http://localhost")
        .connect_with_connector(service_fn(move |_: Uri| {
            let socket = socket.clone();
            async move {
                let stream = tokio::net::UnixStream::connect(socket).await?;
                Ok::<_, std::io::Error>(TokioIo::new(stream))
            }
        }))
        .await?;
    Ok(SandboxServiceClient::new(channel).max_decoding_message_size(64 * 1024 * 1024))
}

/// One cold sandbox materialize: fresh channel, `Materialize` RPC, write
/// every streamed entry. Connect, request and stream run on one runtime —
/// a tonic channel is bound to the runtime that created it.
fn atomic_sandbox(
    socket: &Path,
    repository: libatomic::atomic::RepositoryRef,
    target: libatomic::atomic::ViewRef,
    prefix: Option<String>,
    sandbox: &Path,
) -> Result<(BTreeMap<String, u128>, usize)> {
    use libatomic::daemon::sandbox_wire;

    let mut phases = BTreeMap::new();
    let sandbox = sandbox.to_path_buf();
    let socket = socket.to_path_buf();
    let (written, inner) = std::thread::spawn(move || -> Result<(usize, BTreeMap<String, u128>)> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(anyhow::Error::msg)?;
        runtime.block_on(async move {
            use tokio_stream::StreamExt;
            let mut phases = BTreeMap::new();
            let started = Instant::now();
            let mut client = connect(&socket).await?;
            phases.insert("connect".into(), started.elapsed().as_nanos());
            let started = Instant::now();
            let mut stream = client
                .materialize(MaterializeRequest {
                    repository: Some(repository),
                    target: Some(target),
                    path_prefix: prefix,
                })
                .await?
                .into_inner();
            phases.insert("request".into(), started.elapsed().as_nanos());
            let started = Instant::now();
            let mut written = 0usize;
            loop {
                let frame = stream
                    .next()
                    .await
                    .ok_or_else(|| anyhow::anyhow!("materialize ended without its summary"))?
                    .map_err(anyhow::Error::msg)?;
                match frame.frame {
                    Some(libatomic::atomic::materialize_frame::Frame::Entry(entry)) => {
                        atomic_repository::write_sandbox_entry(
                            &sandbox,
                            &entry.path,
                            sandbox_wire::tree_entry_kind(&entry).map_err(anyhow::Error::msg)?,
                            entry.mode.unwrap_or(0o644),
                            entry.inline.as_deref().unwrap_or_default(),
                        )?;
                        written += 1;
                    }
                    Some(libatomic::atomic::materialize_frame::Frame::Done(done)) => {
                        ensure!(
                            done.entries as usize == written,
                            "the server sent {written} entries but said {}",
                            done.entries
                        );
                        break;
                    }
                    None => bail!("an empty materialize frame"),
                }
            }
            phases.insert("stream_and_write".into(), started.elapsed().as_nanos());
            Ok((written, phases))
        })
    })
    .join()
    .map_err(|_| anyhow::anyhow!("client thread panicked"))??;
    phases.extend(inner);
    Ok((phases, written))
}

// ————————————————————————————— git cases —————————————————————————————

fn git_clone_full(bare: &Path, sandbox: &Path) -> Result<BTreeMap<String, u128>> {
    let mut phases = BTreeMap::new();
    let started = Instant::now();
    git(
        sandbox.parent().unwrap_or(Path::new("/")),
        &[
            "clone",
            "-q",
            &format!("file://{}", bare.display()),
            &sandbox.display().to_string(),
        ],
        None,
    )?;
    phases.insert("clone".into(), started.elapsed().as_nanos());
    Ok(phases)
}

// ————————————————————————————— verification —————————————————————————————

fn file_mode(path: &Path) -> Result<u32> {
    use std::os::unix::fs::PermissionsExt;
    Ok(fs::metadata(path)?.permissions().mode() & 0o7777)
}

/// Full sandboxes hold every tracked file; partial sandboxes hold exactly
/// the east subtree and nothing else from the west or the root.
fn verify(sandbox: &Path, golden: &[Golden], partial: bool, east: &[String]) -> Result<()> {
    if partial {
        for path in east {
            let content = golden
                .iter()
                .find(|f| &f.path == path)
                .context("east path missing from golden")?;
            let target = sandbox.join(path);
            ensure!(target.is_file(), "partial sandbox misses {}", path);
            ensure!(
                fs::read(&target)? == content.content,
                "partial sandbox bytes differ: {path}"
            );
        }
        for file in golden {
            if !east.contains(&file.path) {
                ensure!(
                    !sandbox.join(&file.path).exists(),
                    "partial sandbox fetched beyond its prefix: {}",
                    file.path
                );
            }
        }
        return Ok(());
    }
    for file in golden {
        let target = sandbox.join(&file.path);
        ensure!(target.is_file(), "sandbox misses {}", file.path);
        ensure!(
            fs::read(&target)? == file.content,
            "sandbox bytes differ: {}",
            file.path
        );
        ensure!(
            file_mode(&target)? == 0o644 || file.content.is_empty(),
            "sandbox mode differs: {}",
            file.path
        );
    }
    Ok(())
}

// ————————————————————————————— harness —————————————————————————————

#[derive(Debug, Serialize)]
struct Sample {
    case: String,
    iteration: usize,
    total_ns: u128,
    phases_ns: BTreeMap<String, u128>,
    entries: Option<usize>,
    verified_files: usize,
    output_bytes: usize,
}

fn run(config: Config) -> Result<()> {
    config.validate()?;
    let parent = config
        .output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    ensure!(
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false),
        "git is required for the Git comparison cases"
    );
    let (fixtures, golden, bare) = prepare_fixtures(parent, &config)?;
    let host_root = fixtures.path().join("host");
    let socket = fixtures.path().join("s.sock");
    ensure!(
        socket.to_string_lossy().len() <= 100, // sun_path limit is 104; leave slack
        "socket path too long for a Unix socket: {} — use a shorter --output directory",
        socket.display()
    );
    let state = Arc::new(DaemonState::new());
    let handle = state.register(host_root.clone());
    let repository = handle.repository_ref().repository_id;
    handle.repository()?; // fail fast; the server would serve errors otherwise
    let server = serve(state, host_root, socket.clone())?;
    let east = east_paths(&config);
    let east_bytes: usize = golden
        .iter()
        .filter(|f| east.contains(&f.path))
        .map(|f| f.content.len())
        .sum();

    let mut samples = Vec::new();
    let result = (|| -> Result<()> {
        for iteration in 0..config.warmups + config.iterations {
            for case in CASES {
                let partial = case.contains("partial");
                let sandbox = fixtures.path().join(format!("sandbox-{case}-{iteration}"));
                let started = Instant::now();
                let (phases, entries) = if case.starts_with("atomic") {
                    let target = libatomic::atomic::ViewRef {
                        view_id: Vec::new(),
                        name: Some("dev".into()),
                    };
                    let reference = libatomic::atomic::RepositoryRef {
                        authority: "local".into(),
                        repository_id: repository.clone(),
                        workspace_id: None,
                    };
                    let (phases, entries) = atomic_sandbox(
                        &socket,
                        reference,
                        target,
                        partial.then(|| PARTIAL_PREFIX.to_string()),
                        &sandbox,
                    )?;
                    (phases, Some(entries))
                } else if *case == "git_clone_full" {
                    (git_clone_full(&bare, &sandbox)?, None)
                } else {
                    bail!("unknown case: {case}")
                };
                let total_ns = started.elapsed().as_nanos();
                // Byte verification is never timed.
                verify(&sandbox, &golden, partial, &east)
                    .with_context(|| format!("{case} iteration {iteration} verification"))?;
                if iteration >= config.warmups {
                    samples.push(Sample {
                        case: case.to_string(),
                        iteration: iteration - config.warmups,
                        total_ns,
                        phases_ns: phases,
                        entries,
                        verified_files: if partial { east.len() } else { golden.len() },
                        output_bytes: if partial {
                            east_bytes
                        } else {
                            golden.iter().map(|f| f.content.len()).sum()
                        },
                    });
                }
                let _ = fs::remove_dir_all(&sandbox);
            }
        }
        Ok(())
    })();
    // The tonic server never shuts down; detach it. The process (or test)
    // exits with the thread still serving, which mirrors atomicd.
    drop(server);
    if let Err(error) = result {
        // Retain the fixtures for inspection; a TempDir would delete them.
        let retained = fixtures.keep();
        eprintln!("fixtures retained at {} for inspection", retained.display());
        return Err(error);
    }
    let retained_fixtures = config.keep_fixtures.then(|| fixtures.keep());

    let report = serde_json::json!({
        "schema": 1,
        "config": config,
        "debug_build": cfg!(debug_assertions),
        "platform": {"os": std::env::consts::OS, "arch": std::env::consts::ARCH},
        "fixture_kind": "two-subtree synthetic histories; atomic daemon Materialize RPC on a Unix socket versus Git file:// clone",
        "cache_policy": "server and bare repository persist across samples; sandboxes are fresh; OS caches uncontrolled",
        "retained_fixtures": retained_fixtures,
        "samples": samples,
    });
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&config.output)?;
    serde_json::to_writer_pretty(&mut output, &report)?;
    output.flush()?;
    println!(
        "verified {} samples; report {}",
        config.iterations * CASES.len(),
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
    fn smoke_checks_every_case() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("report.json");
        run(Config {
            output: output.clone(),
            files_east: 2,
            files_west: 2,
            lines: 8,
            edits: 1,
            iterations: 1,
            warmups: 0,
            allow_debug: true,
            keep_fixtures: false,
        })
        .unwrap();
        let report: serde_json::Value = serde_json::from_slice(&fs::read(output).unwrap()).unwrap();
        let samples = report["samples"].as_array().unwrap();
        assert_eq!(samples.len(), CASES.len());
        for sample in samples {
            assert!(sample["total_ns"].as_u64().unwrap() > 0);
            assert!(sample["verified_files"].as_u64().unwrap() > 0);
        }
        let partial: Vec<_> = samples
            .iter()
            .filter(|s| s["case"].as_str().unwrap().contains("partial"))
            .collect();
        assert_eq!(partial.len(), 1);
        assert_eq!(partial[0]["case"].as_str().unwrap(), "atomic_rpc_partial");
        assert_eq!(partial[0]["verified_files"].as_u64().unwrap(), 2);
    }
}
