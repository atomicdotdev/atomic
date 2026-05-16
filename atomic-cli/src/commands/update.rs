//! `atomic update` — source-aware update router.
//!
//! Reads `$XDG_STATE_HOME/atomic/install.json` (written by the official
//! installer) plus path heuristics to determine how the running binary was
//! installed, queries GitHub `/releases/latest`, and prints the upgrade
//! command appropriate to the install source.
//!
//! This v1 router does **not** replace the binary itself — it only routes
//! the user to the correct upgrade path. Self-replace requires signature
//! verification, atomic same-directory rename, permission handling, and a
//! Windows .exe-lock workaround, all deferred to a P1 PR.
//!
//! # Trust model
//!
//! See `install.sh` for the full statement. Key rules this code follows:
//!
//! 1. The manifest is user-writable; we only trust it when
//!    `manifest.install_path` canonicalizes to the same path as
//!    `current_exe()`.
//! 2. `manifest.version` is diagnostic — we always use
//!    `env!("CARGO_PKG_VERSION")` as the current version.
//! 3. Unknown `schema_version` values are rejected, not crashed on.

use std::path::{Path, PathBuf};

use clap::Args;
use serde::Deserialize;

use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_hint, print_success, print_warning};

// CLI argument struct

/// Check for and route to the correct upgrade path for this Atomic binary.
///
/// Detects how Atomic was installed (Homebrew, Cargo, official installer,
/// or manual) and prints the upgrade command for that source. With
/// `--check`, only the status is reported; exits 1 if outdated, 0 if up
/// to date, 4 on network error or unparseable versions.
#[derive(Debug, Args)]
pub struct Update {
    /// Report status without printing upgrade commands. Exits 1 if an
    /// update is available, 0 if up to date, 4 on network error or
    /// unparseable versions.
    #[arg(long)]
    pub check: bool,
}

// Types

/// How the running atomic binary appears to have been installed.
#[derive(Debug, PartialEq, Eq)]
pub enum InstallSource {
    /// Installed by the official installer; manifest agrees with current_exe.
    OfficialInstaller {
        manifest_path: PathBuf,
        recorded_install_path: PathBuf,
        /// Diagnostic only — never compared against latest.
        recorded_version: String,
    },
    /// Path matches a Homebrew prefix (macOS or linuxbrew).
    Homebrew { prefix: PathBuf },
    /// Path is under `$CARGO_HOME/bin` or `~/.cargo/bin`.
    Cargo,
    /// Recognizable path layout but not one of the above; or path not
    /// reachable by any heuristic. Includes typical Windows installs.
    Manual,
    /// `std::env::current_exe()` itself failed.
    Unknown,
}

impl InstallSource {
    /// Short label used in `--check` mode for terse machine-readable output.
    pub fn short_label(&self) -> &'static str {
        match self {
            Self::OfficialInstaller { .. } => "installer",
            Self::Homebrew { .. } => "homebrew",
            Self::Cargo => "cargo",
            Self::Manual => "manual",
            Self::Unknown => "unknown",
        }
    }
}

/// Result of comparing the current binary's version to GitHub's latest.
///
/// `execute()` returns one of these; `run()` (the CLI boundary) chooses
/// how to print and what exit code to use. Keeping logic and presentation
/// separate this way is what makes the inline tests below tractable.
#[derive(Debug, PartialEq, Eq)]
pub enum UpdateOutcome {
    UpToDate {
        current: String,
        source: InstallSource,
    },
    Outdated {
        current: String,
        latest: String,
        source: InstallSource,
        /// Some(recorded) when OfficialInstaller and `manifest.version`
        /// disagrees with `current` (out-of-band replacement). None
        /// otherwise. Diagnostic only.
        drift_manifest_version: Option<String>,
    },
    /// Neither current nor latest parsed cleanly as MAJOR.MINOR.PATCH.
    /// Treated as a soft failure: warn the user, exit 0 in normal mode,
    /// exit 4 in `--check` (CI must not interpret as "confirmed up-to-date").
    UnknownVersion {
        current: String,
        latest: String,
        source: InstallSource,
    },
}

/// Environment lookup, injected so tests don't have to mutate the real
/// process environment.
pub trait EnvLookup {
    fn var(&self, key: &str) -> Option<String>;
}

/// Production implementation backed by `std::env`.
pub struct StdEnv;
impl EnvLookup for StdEnv {
    fn var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

/// Shape of `~/.local/state/atomic/install.json` as written by install.sh.
/// Optional fields we don't consume in v1 are omitted; serde tolerates
/// extra fields by default.
#[derive(Debug, Deserialize)]
struct Manifest {
    schema_version: u32,
    source: String,
    install_path: String,
    version: String,
}

/// Subset of GitHub's release-latest response that we consume.
#[derive(Debug, Deserialize)]
struct GithubReleaseLatest {
    tag_name: String,
}

// Pure helpers — these are what the inline tests exercise

/// Resolve the install manifest path the same way the installer does:
/// `${XDG_STATE_HOME:-${HOME}/.local/state}/atomic/install.json`.
///
/// We deliberately do NOT use `dirs::state_dir()` — that returns `None`
/// on macOS, but install.sh writes the manifest on macOS too (via the
/// `$HOME` fallback). We must mirror the shell behavior exactly so the
/// router finds what the installer wrote.
fn manifest_path(env: &dyn EnvLookup) -> PathBuf {
    let state_dir = match env.var("XDG_STATE_HOME") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => {
            let home = env.var("HOME").unwrap_or_default();
            PathBuf::from(home).join(".local").join("state")
        }
    };
    state_dir.join("atomic").join("install.json")
}

/// Determine the install source for a binary at `current_exe`.
///
/// Manifest is consulted first under the trust gate (canonicalized path
/// must match current_exe). Path heuristics provide the fallback.
pub fn detect_source(current_exe: &Path, env: &dyn EnvLookup) -> InstallSource {
    let canonical_exe =
        std::fs::canonicalize(current_exe).unwrap_or_else(|_| current_exe.to_path_buf());

    // 1. Manifest trust gate. Any parse / schema / mismatch failure falls
    //    through silently to path heuristics — manifest is best-effort
    //    metadata, not a source of errors.
    let manifest_p = manifest_path(env);
    if let Ok(raw) = std::fs::read_to_string(&manifest_p) {
        if let Ok(m) = serde_json::from_str::<Manifest>(&raw) {
            if m.schema_version == 1 && m.source == "official-installer" {
                let recorded = std::fs::canonicalize(&m.install_path)
                    .unwrap_or_else(|_| PathBuf::from(&m.install_path));
                if recorded == canonical_exe {
                    return InstallSource::OfficialInstaller {
                        manifest_path: manifest_p,
                        recorded_install_path: recorded,
                        recorded_version: m.version,
                    };
                }
            }
        }
    }

    // 2. Path heuristics.
    let path_str = canonical_exe.to_string_lossy();

    // Homebrew: Apple Silicon (/opt/homebrew), Intel (/usr/local/Cellar),
    // and linuxbrew. canonicalize() above already resolved the
    // /opt/homebrew/bin/atomic symlink into the Cellar path on macOS.
    let homebrew_markers = [
        "/Cellar/",
        "/opt/homebrew/",
        "/home/linuxbrew/.linuxbrew/",
        "/usr/local/Homebrew/",
    ];
    for marker in homebrew_markers {
        if let Some(idx) = path_str.find(marker) {
            let prefix = if marker == "/Cellar/" {
                PathBuf::from(&path_str[..idx])
            } else {
                PathBuf::from(marker.trim_end_matches('/'))
            };
            return InstallSource::Homebrew { prefix };
        }
    }

    // Cargo: $CARGO_HOME/bin or ~/.cargo/bin.
    if let Some(cargo_home) = env.var("CARGO_HOME") {
        if path_str.starts_with(&format!("{}/bin/", cargo_home)) {
            return InstallSource::Cargo;
        }
    }
    if let Some(home) = env.var("HOME") {
        if path_str.starts_with(&format!("{}/.cargo/bin/", home)) {
            return InstallSource::Cargo;
        }
    }

    InstallSource::Manual
}

/// Parse "0.6.0" or "v0.6.0" into (major, minor, patch).
/// Stable-only — bails on prereleases. The `/releases/latest` API never
/// returns prereleases, so this is safe for v1. Add the `semver` crate
/// when we introduce `--channel nightly`.
fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let s = s.strip_prefix('v').unwrap_or(s);
    if s.contains('-') {
        return None;
    }
    let mut parts = s.splitn(3, '.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

// GitHub fetch — kept thin, not unit-tested

async fn fetch_latest(env: &dyn EnvLookup) -> Result<String, CliError> {
    let url = "https://api.github.com/repos/atomicdotdev/atomic/releases/latest";
    let ua = concat!("atomic-cli/", env!("CARGO_PKG_VERSION"));

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| CliError::RemoteError {
            message: format!("HTTP client init failed: {e}"),
            url: Some(url.to_string()),
        })?;

    let mut req = client
        .get(url)
        .header(reqwest::header::USER_AGENT, ua)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json");

    // Authenticated requests get 5000/hr instead of 60/hr. Useful for CI
    // running `atomic update --check`. Empty/whitespace tokens are
    // ignored so a stray `export GITHUB_TOKEN=` doesn't break auth.
    if let Some(token) = env.var("GITHUB_TOKEN") {
        if !token.trim().is_empty() {
            req = req.bearer_auth(token);
        }
    }

    let resp = req.send().await.map_err(|e| CliError::RemoteError {
        message: format!("Failed to reach GitHub: {e}"),
        url: Some(url.to_string()),
    })?;

    if resp.status() == reqwest::StatusCode::FORBIDDEN {
        return Err(CliError::RemoteError {
            message: "GitHub API rate limit hit (60/hr unauthenticated, 5000/hr with GITHUB_TOKEN). Try again later or export GITHUB_TOKEN.".to_string(),
            url: Some(url.to_string()),
        });
    }
    if !resp.status().is_success() {
        return Err(CliError::RemoteError {
            message: format!("GitHub returned HTTP {}", resp.status()),
            url: Some(url.to_string()),
        });
    }

    let release: GithubReleaseLatest = resp.json().await.map_err(|e| CliError::RemoteError {
        message: format!("Failed to parse GitHub response: {e}"),
        url: Some(url.to_string()),
    })?;

    Ok(release.tag_name)
}

// Message formatting — all printing helpers are pure so tests can capture output

const INSTALLER_URL: &str = "https://atomic.storage/install.sh";
const RELEASES_PAGE: &str = "https://github.com/atomicdotdev/atomic/releases/latest";
const DEFAULT_INSTALL_PATH: &str = "/usr/local/bin/atomic";
const TAP_FORMULA: &str = "atomicdotdev/tap/atomic";

/// Render an `ATOMIC_INSTALL=...` value suitable for embedding in a shell
/// snippet. When the directory equals `$HOME/.local/bin` literally we
/// prefer the variable form for legibility; otherwise we quote the
/// absolute path verbatim.
fn render_atomic_install_value(dir: &Path, home: Option<&str>) -> String {
    if let Some(h) = home {
        let home_local_bin = PathBuf::from(h).join(".local").join("bin");
        if dir == home_local_bin {
            return r#""$HOME/.local/bin""#.to_string();
        }
    }
    format!("\"{}\"", dir.display())
}

/// Produce the multi-line outdated message for a given source. Pure so
/// the unit tests can match on substrings.
pub fn format_upgrade_message(
    source: &InstallSource,
    current: &str,
    latest: &str,
    drift_manifest_version: Option<&str>,
    env: &dyn EnvLookup,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "A new version is available: {current} -> {latest}.\n\n"
    ));

    match source {
        InstallSource::OfficialInstaller {
            manifest_path,
            recorded_install_path,
            ..
        } => {
            let is_default = recorded_install_path == &PathBuf::from(DEFAULT_INSTALL_PATH);
            if is_default {
                out.push_str("Source: official installer\n");
                out.push_str(&format!("        manifest: {}\n", manifest_path.display()));
                out.push_str(&format!(
                    "        binary:   {}\n\n",
                    recorded_install_path.display()
                ));
                out.push_str("To upgrade, re-run the installer:\n\n");
                out.push_str(&format!("    curl -sSf {INSTALLER_URL} | sh\n"));
            } else {
                let install_dir = recorded_install_path
                    .parent()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("/usr/local/bin"));
                let value = render_atomic_install_value(&install_dir, env.var("HOME").as_deref());
                out.push_str("Source: official installer (custom install dir)\n");
                out.push_str(&format!("        manifest: {}\n", manifest_path.display()));
                out.push_str(&format!(
                    "        binary:   {}\n\n",
                    recorded_install_path.display()
                ));
                out.push_str("To upgrade, re-run the installer with the same install dir:\n\n");
                out.push_str(&format!(
                    "    curl -sSf {INSTALLER_URL} | ATOMIC_INSTALL={value} sh\n"
                ));
            }
        }

        InstallSource::Homebrew { prefix } => {
            out.push_str(&format!(
                "Source: Homebrew (prefix: {})\n\n",
                prefix.display()
            ));
            out.push_str("To upgrade:\n\n");
            out.push_str(&format!("    brew upgrade {TAP_FORMULA}\n"));
        }

        InstallSource::Cargo => {
            out.push_str("Source: Cargo install\n\n");
            out.push_str("To upgrade:\n\n");
            out.push_str(
                "    cargo install --git https://github.com/atomicdotdev/atomic atomic-cli --locked --force\n",
            );
        }

        InstallSource::Manual | InstallSource::Unknown => {
            out.push_str(
                "Source: manual install (no install manifest, no recognized package manager path)\n\n",
            );
            out.push_str("We can't automatically determine the right upgrade path. Options:\n\n");
            out.push_str(
                "  * Install or upgrade with the official installer (installs to /usr/local/bin\n",
            );
            out.push_str("    by default; set ATOMIC_INSTALL to choose another directory):\n\n");
            out.push_str(&format!("        curl -sSf {INSTALLER_URL} | sh\n\n"));
            out.push_str("  * Or download the release archive directly:\n\n");
            out.push_str(&format!("        {RELEASES_PAGE}\n"));
        }
    }

    if let Some(recorded) = drift_manifest_version {
        out.push_str(&format!(
            "\nNote: install manifest recorded version {recorded}, but binary reports {current}.\n",
        ));
        out.push_str(
            "      The binary was likely replaced out-of-band (manual cp, second installer\n",
        );
        out.push_str("      run, etc.). The recommended upgrade command above still applies.\n");
    }

    out
}

// Command impl — execute() pure, run() the CLI boundary

impl Command for Update {
    fn run(&self) -> CliResult<()> {
        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| CliError::Internal(anyhow::anyhow!("tokio runtime: {e}")))?;

        let outcome = rt.block_on(self.execute())?;

        // block_on has returned: the runtime is dropped before we reach
        // any std::process::exit calls below.

        match outcome {
            UpdateOutcome::UpToDate { current, .. } => {
                print_success(&format!("You're on the latest release ({current})."));
                Ok(())
            }
            UpdateOutcome::Outdated {
                current,
                latest,
                source,
                drift_manifest_version,
            } => {
                if self.check {
                    println!(
                        "Outdated: {current} -> {latest} (source: {})",
                        source.short_label()
                    );
                    std::process::exit(1);
                }
                let env = StdEnv;
                let msg = format_upgrade_message(
                    &source,
                    &current,
                    &latest,
                    drift_manifest_version.as_deref(),
                    &env,
                );
                print!("{msg}");
                Ok(())
            }
            UpdateOutcome::UnknownVersion {
                current,
                latest,
                source,
            } => {
                if self.check {
                    println!(
                        "Unknown: could not compare versions current={current} latest={latest} (source: {})",
                        source.short_label()
                    );
                    // CI must not interpret as "confirmed up-to-date".
                    std::process::exit(4);
                }
                print_warning(&format!(
                    "Could not compare versions (current={current}, latest={latest})."
                ));
                print_hint(&format!("Check {RELEASES_PAGE}"));
                Ok(())
            }
        }
    }
}

impl Update {
    /// Pure business logic. Returns an `UpdateOutcome`, or a
    /// `CliError::RemoteError` for network / GitHub failures. Caller
    /// decides exit code and presentation.
    async fn execute(&self) -> CliResult<UpdateOutcome> {
        let env = StdEnv;
        let source = match std::env::current_exe() {
            Ok(p) => detect_source(&p, &env),
            Err(_) => InstallSource::Unknown,
        };

        let current = env!("CARGO_PKG_VERSION").to_string();
        let latest_tag = fetch_latest(&env).await?;
        let latest = latest_tag
            .strip_prefix('v')
            .unwrap_or(&latest_tag)
            .to_string();

        Ok(match (parse_version(&current), parse_version(&latest)) {
            (Some(c), Some(l)) if c >= l => UpdateOutcome::UpToDate { current, source },
            (Some(_), Some(_)) => {
                let drift = match &source {
                    InstallSource::OfficialInstaller {
                        recorded_version, ..
                    } if recorded_version != &current => Some(recorded_version.clone()),
                    _ => None,
                };
                UpdateOutcome::Outdated {
                    current,
                    latest,
                    source,
                    drift_manifest_version: drift,
                }
            }
            _ => UpdateOutcome::UnknownVersion {
                current,
                latest,
                source,
            },
        })
    }
}

// Tests — inline because atomic-cli is a binary crate with no lib.rs,
// so tests in atomic-cli/tests/ cannot see these symbols.

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;
    use tempfile::TempDir;

    struct MockEnv(HashMap<String, String>);
    impl MockEnv {
        fn new() -> Self {
            Self(HashMap::new())
        }
        fn set(mut self, k: &str, v: &str) -> Self {
            self.0.insert(k.into(), v.into());
            self
        }
    }
    impl EnvLookup for MockEnv {
        fn var(&self, k: &str) -> Option<String> {
            self.0.get(k).cloned()
        }
    }

    // manifest_path

    #[test]
    fn manifest_path_uses_xdg_state_home_when_set() {
        let env = MockEnv::new().set("XDG_STATE_HOME", "/x");
        assert_eq!(manifest_path(&env), PathBuf::from("/x/atomic/install.json"));
    }

    #[test]
    fn manifest_path_falls_back_to_home() {
        let env = MockEnv::new().set("HOME", "/h");
        assert_eq!(
            manifest_path(&env),
            PathBuf::from("/h/.local/state/atomic/install.json")
        );
    }

    #[test]
    fn manifest_path_treats_empty_xdg_state_home_as_unset() {
        // POSIX `${XDG_STATE_HOME:-...}` semantics: empty == unset.
        let env = MockEnv::new().set("XDG_STATE_HOME", "").set("HOME", "/h");
        assert_eq!(
            manifest_path(&env),
            PathBuf::from("/h/.local/state/atomic/install.json")
        );
    }

    // detect_source — manifest path

    fn write_manifest(dir: &Path, install_path: &Path, version: &str, schema_version: u32) {
        let atomic_dir = dir.join("atomic");
        fs::create_dir_all(&atomic_dir).unwrap();
        let manifest = format!(
            r#"{{
  "schema_version": {schema_version},
  "source": "official-installer",
  "install_path": "{}",
  "version": "{version}",
  "platform": "x86_64-unknown-linux-gnu",
  "installed_at": "2026-05-16T00:00:00Z",
  "installer_url": "https://atomic.storage/install.sh",
  "artifact_url": "",
  "artifact_sha256": "",
  "binary_sha256": ""
}}"#,
            install_path.display()
        );
        fs::write(atomic_dir.join("install.json"), manifest).unwrap();
    }

    #[test]
    fn detect_source_manifest_happy_path() {
        let state = TempDir::new().unwrap();
        let bin_dir = TempDir::new().unwrap();
        let bin = bin_dir.path().join("atomic");
        fs::write(&bin, b"#!/bin/sh\n").unwrap();

        write_manifest(state.path(), &bin, "0.6.0", 1);

        let env = MockEnv::new().set("XDG_STATE_HOME", state.path().to_str().unwrap());
        match detect_source(&bin, &env) {
            InstallSource::OfficialInstaller {
                recorded_version, ..
            } => {
                assert_eq!(recorded_version, "0.6.0");
            }
            other => panic!("expected OfficialInstaller, got {other:?}"),
        }
    }

    #[test]
    fn detect_source_manifest_path_mismatch_falls_through() {
        let state = TempDir::new().unwrap();
        let bin_dir = TempDir::new().unwrap();
        let real_bin = bin_dir.path().join("atomic");
        fs::write(&real_bin, b"x").unwrap();
        // Manifest points to a *different* binary path.
        write_manifest(state.path(), Path::new("/nonexistent/atomic"), "0.5.0", 1);

        let env = MockEnv::new().set("XDG_STATE_HOME", state.path().to_str().unwrap());
        // Not OfficialInstaller — falls through to heuristics. Random tmp
        // path doesn't match any heuristic, so Manual.
        assert_eq!(detect_source(&real_bin, &env), InstallSource::Manual);
    }

    #[test]
    fn detect_source_rejects_unknown_schema_version() {
        let state = TempDir::new().unwrap();
        let bin_dir = TempDir::new().unwrap();
        let bin = bin_dir.path().join("atomic");
        fs::write(&bin, b"x").unwrap();
        write_manifest(state.path(), &bin, "0.6.0", 99);

        let env = MockEnv::new().set("XDG_STATE_HOME", state.path().to_str().unwrap());
        assert_eq!(detect_source(&bin, &env), InstallSource::Manual);
    }

    #[test]
    fn detect_source_handles_malformed_manifest() {
        let state = TempDir::new().unwrap();
        let bin_dir = TempDir::new().unwrap();
        let bin = bin_dir.path().join("atomic");
        fs::write(&bin, b"x").unwrap();
        fs::create_dir_all(state.path().join("atomic")).unwrap();
        fs::write(
            state.path().join("atomic").join("install.json"),
            b"not json at all",
        )
        .unwrap();

        let env = MockEnv::new().set("XDG_STATE_HOME", state.path().to_str().unwrap());
        // Doesn't panic; falls through.
        assert_eq!(detect_source(&bin, &env), InstallSource::Manual);
    }

    // detect_source — path heuristics
    //
    // These tests use literal paths that may not exist on the test runner.
    // canonicalize() in detect_source will fall back to the input path
    // verbatim when the file doesn't exist, which is exactly what we want
    // here: assert that the path-string heuristic works.

    #[test]
    fn detect_source_homebrew_apple_silicon() {
        let env = MockEnv::new();
        let p = Path::new("/opt/homebrew/Cellar/atomic/0.6.0/bin/atomic");
        match detect_source(p, &env) {
            InstallSource::Homebrew { prefix } => {
                assert_eq!(prefix, PathBuf::from("/opt/homebrew"));
            }
            other => panic!("expected Homebrew, got {other:?}"),
        }
    }

    #[test]
    fn detect_source_homebrew_linuxbrew() {
        let env = MockEnv::new();
        let p = Path::new("/home/linuxbrew/.linuxbrew/bin/atomic");
        match detect_source(p, &env) {
            InstallSource::Homebrew { prefix } => {
                assert_eq!(prefix, PathBuf::from("/home/linuxbrew/.linuxbrew"));
            }
            other => panic!("expected Homebrew, got {other:?}"),
        }
    }

    #[test]
    fn detect_source_cargo_with_cargo_home() {
        let env = MockEnv::new().set("CARGO_HOME", "/custom/cargo");
        let p = Path::new("/custom/cargo/bin/atomic");
        assert_eq!(detect_source(p, &env), InstallSource::Cargo);
    }

    #[test]
    fn detect_source_cargo_with_home_only() {
        let env = MockEnv::new().set("HOME", "/u/me");
        let p = Path::new("/u/me/.cargo/bin/atomic");
        assert_eq!(detect_source(p, &env), InstallSource::Cargo);
    }

    #[test]
    fn detect_source_random_path_is_manual() {
        let env = MockEnv::new();
        let p = Path::new("/tmp/foo/atomic");
        assert_eq!(detect_source(p, &env), InstallSource::Manual);
    }

    // parse_version

    #[test]
    fn parse_version_stable() {
        assert_eq!(parse_version("0.6.0"), Some((0, 6, 0)));
        assert_eq!(parse_version("v0.6.0"), Some((0, 6, 0)));
        assert_eq!(parse_version("12.34.56"), Some((12, 34, 56)));
    }

    #[test]
    fn parse_version_rejects_prerelease() {
        assert_eq!(parse_version("0.7.0-alpha.1"), None);
        assert_eq!(parse_version("v0.7.0-rc.1"), None);
    }

    #[test]
    fn parse_version_rejects_garbage() {
        assert_eq!(parse_version("not-a-version"), None);
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("1.2"), None);
        assert_eq!(parse_version("1.2.x"), None);
    }

    // format_upgrade_message — substring assertions and negative regressions

    #[test]
    fn upgrade_message_official_installer_default_path() {
        let source = InstallSource::OfficialInstaller {
            manifest_path: PathBuf::from("/h/.local/state/atomic/install.json"),
            recorded_install_path: PathBuf::from("/usr/local/bin/atomic"),
            recorded_version: "0.6.0".to_string(),
        };
        let env = MockEnv::new();
        let msg = format_upgrade_message(&source, "0.6.0", "0.7.0", None, &env);
        assert!(msg.contains("curl -sSf https://atomic.storage/install.sh | sh"));
        assert!(!msg.contains("ATOMIC_INSTALL"), "got: {msg}");
        assert!(msg.contains("0.6.0 -> 0.7.0"));
    }

    #[test]
    fn upgrade_message_official_installer_custom_home_path() {
        let source = InstallSource::OfficialInstaller {
            manifest_path: PathBuf::from("/home/u/.local/state/atomic/install.json"),
            recorded_install_path: PathBuf::from("/home/u/.local/bin/atomic"),
            recorded_version: "0.6.0".to_string(),
        };
        let env = MockEnv::new().set("HOME", "/home/u");
        let msg = format_upgrade_message(&source, "0.6.0", "0.7.0", None, &env);
        assert!(
            msg.contains(r#"ATOMIC_INSTALL="$HOME/.local/bin""#),
            "got: {msg}"
        );
    }

    #[test]
    fn upgrade_message_official_installer_custom_abs_path() {
        let source = InstallSource::OfficialInstaller {
            manifest_path: PathBuf::from("/x/install.json"),
            recorded_install_path: PathBuf::from("/opt/atomic/atomic"),
            recorded_version: "0.6.0".to_string(),
        };
        let env = MockEnv::new();
        let msg = format_upgrade_message(&source, "0.6.0", "0.7.0", None, &env);
        assert!(
            msg.contains(r#"ATOMIC_INSTALL="/opt/atomic""#),
            "got: {msg}"
        );
    }

    #[test]
    fn upgrade_message_homebrew() {
        let source = InstallSource::Homebrew {
            prefix: PathBuf::from("/opt/homebrew"),
        };
        let env = MockEnv::new();
        let msg = format_upgrade_message(&source, "0.6.0", "0.7.0", None, &env);
        assert!(
            msg.contains("brew upgrade atomicdotdev/tap/atomic"),
            "got: {msg}"
        );
        assert!(msg.contains("Homebrew (prefix: /opt/homebrew)"));
    }

    #[test]
    fn upgrade_message_cargo_includes_locked_and_force() {
        let source = InstallSource::Cargo;
        let env = MockEnv::new();
        let msg = format_upgrade_message(&source, "0.6.0", "0.7.0", None, &env);
        assert!(msg.contains("cargo install --git"), "got: {msg}");
        assert!(msg.contains("--locked --force"), "got: {msg}");
    }

    #[test]
    fn upgrade_message_manual_does_not_claim_overwrite() {
        // Negative regression: an earlier draft said "overwrites the current binary"
        // which is wrong when the current binary is e.g. target/debug/atomic but
        // installer goes to /usr/local/bin. Lock that fix in here.
        let source = InstallSource::Manual;
        let env = MockEnv::new();
        let msg = format_upgrade_message(&source, "0.6.0", "0.7.0", None, &env);
        assert!(msg.contains("curl -sSf https://atomic.storage/install.sh | sh"));
        assert!(
            msg.contains("https://github.com/atomicdotdev/atomic/releases/latest"),
            "got: {msg}"
        );
        assert!(
            !msg.contains("overwrites the current binary"),
            "Manual hint must not claim it overwrites — current binary may be at a different path. Got: {msg}"
        );
        // Should mention the configurable install dir so users aren't surprised.
        assert!(msg.contains("ATOMIC_INSTALL"), "got: {msg}");
    }

    #[test]
    fn upgrade_message_drift_note_when_versions_disagree() {
        let source = InstallSource::OfficialInstaller {
            manifest_path: PathBuf::from("/x/install.json"),
            recorded_install_path: PathBuf::from("/usr/local/bin/atomic"),
            recorded_version: "0.5.5".to_string(),
        };
        let env = MockEnv::new();
        let msg = format_upgrade_message(&source, "0.6.0", "0.7.0", Some("0.5.5"), &env);
        assert!(
            msg.contains("install manifest recorded version 0.5.5"),
            "got: {msg}"
        );
        assert!(msg.contains("out-of-band"), "got: {msg}");
    }

    #[test]
    fn install_source_short_labels() {
        assert_eq!(InstallSource::Cargo.short_label(), "cargo");
        assert_eq!(InstallSource::Manual.short_label(), "manual");
        assert_eq!(InstallSource::Unknown.short_label(), "unknown");
        assert_eq!(
            InstallSource::Homebrew {
                prefix: PathBuf::from("/x"),
            }
            .short_label(),
            "homebrew"
        );
    }
}
