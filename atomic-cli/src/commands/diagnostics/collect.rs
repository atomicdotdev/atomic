//! Gathering the support report.
//!
//! Every fact comes from the running binary or from local configuration that
//! Atomic itself wrote: integration receipts, the `atomic-skills` cache
//! manifest, each agent's hook configuration (through its adapter), and the
//! repository's `.atomic/config.toml`. Nothing is written and nothing touches
//! the network. A value that cannot be read is reported with the reason,
//! never filled in by inference.

use std::collections::BTreeMap;
use std::fmt;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use atomic_agent::hooks::AgentRegistry;
use atomic_agent::integrations::{IntegrationManifest, Receipt};
use serde::Serialize;

use crate::commands::find_repository_root;
use crate::commands::git::hook_dispatchers::{inspect_dispatchers, DispatcherReport};
use crate::commands::update::{detect_source, StdEnv};
use crate::error::CliError;
use crate::logging::redact;

/// Version of the JSON document. Bumped when a field changes meaning.
pub(crate) const SCHEMA: u32 = 1;

/// Registry name of the shared skills package. It is synced as a cache for
/// other integrations to copy skills from, never installed on its own.
const SKILLS_PACKAGE: &str = "atomic-skills";

const RECEIPT_FILE: &str = "receipt.json";

// Report

#[derive(Debug, Serialize)]
pub(crate) struct Report {
    pub schema: u32,
    pub generated_at: String,
    pub atomic: AtomicInfo,
    pub workspace: Workspace,
    pub integrations: Integrations,
    pub skills: Skills,
    /// Every agent Atomic has an adapter for, and whether its hooks are active.
    pub agents: Vec<AgentState>,
    pub git_shim: GitShim,
    /// Facts a support conversation may ask for that this report does not
    /// collect, and why.
    pub not_collected: Vec<NotCollected>,
}

#[derive(Debug, Serialize)]
pub(crate) struct AtomicInfo {
    pub version: String,
    /// `installer`, `homebrew`, `cargo`, `manual`, or `unknown` — the same
    /// detection `atomic update` routes upgrades with.
    pub install_source: String,
    pub os: &'static str,
    pub arch: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum Workspace {
    Found { root: String },
    NotFound,
    Unreadable { reason: String },
}

#[derive(Debug, Serialize)]
pub(crate) struct Integrations {
    /// Where receipts are read from; absent when it cannot be resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    /// `ATOMIC_INTEGRATIONS_HOME` relocated the root.
    pub root_overridden: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_error: Option<String>,
    pub packages: Vec<Integration>,
}

/// One externally packaged integration and its install receipt.
#[derive(Debug, Serialize)]
pub(crate) struct Integration {
    pub agent: String,
    pub display_name: String,
    #[serde(flatten)]
    pub state: IntegrationState,
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum IntegrationState {
    Installed(InstalledIntegration),
    NotInstalled,
    Unreadable { reason: String },
}

#[derive(Debug, Serialize)]
pub(crate) struct InstalledIntegration {
    /// Package version from its manifest, as recorded at install time.
    pub version: String,
    /// The Atomic version that performed the install.
    pub installed_with: String,
    pub installed_at: String,
    pub last_fired: HookActivity,
    /// `registry`, `local`, or `url`.
    pub source_kind: &'static str,
    /// Where the package came from, with any URL credentials removed.
    pub source: String,
    pub files: FileCounts,
}

/// The latest recorded dispatch in this workspace, not a machine-wide history.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum HookActivity {
    Observed { at: String },
    NotObserved,
    Unavailable { reason: &'static str },
}

/// The receipt's files compared against what is on disk now.
#[derive(Debug, Default, Serialize, PartialEq, Eq)]
pub(crate) struct FileCounts {
    pub recorded: usize,
    pub present: usize,
    pub modified: usize,
    pub missing: usize,
    pub unreadable: usize,
}

#[derive(Debug, Serialize)]
pub(crate) struct Skills {
    pub package: SkillsPackage,
    /// Skill files integrations copied into place, per their receipts.
    pub installed: Vec<InstalledSkill>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum SkillsPackage {
    /// The cached package manifest the last `atomic agent enable` synced.
    Available {
        name: &'static str,
        version: String,
        skills: Vec<String>,
    },
    NotFound {
        reason: String,
    },
    Unreadable {
        reason: String,
    },
}

#[derive(Debug, Serialize)]
pub(crate) struct InstalledSkill {
    pub agent: String,
    pub name: String,
    pub path: String,
    pub state: FileState,
    /// Nested metadata version, or the legacy top-level version when present.
    pub version: Option<String>,
    pub updated_at: Option<String>,
    pub metadata_status: SkillMetadataStatus,
    pub missing_metadata: Vec<&'static str>,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SkillMetadataStatus {
    Complete,
    Incomplete,
    Invalid,
    Unavailable,
}

#[derive(Debug, PartialEq, Eq)]
struct SkillMetadata {
    version: Option<String>,
    updated_at: Option<String>,
    status: SkillMetadataStatus,
    missing: Vec<&'static str>,
}

impl SkillMetadata {
    fn unavailable() -> Self {
        Self {
            version: None,
            updated_at: None,
            status: SkillMetadataStatus::Unavailable,
            missing: Vec::new(),
        }
    }

    fn invalid() -> Self {
        Self {
            status: SkillMetadataStatus::Invalid,
            ..Self::unavailable()
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FileState {
    /// Matches what was installed.
    Present,
    /// Exists, but its content differs from what was installed.
    Modified,
    Missing,
    Unreadable,
}

#[derive(Debug, Serialize)]
pub(crate) struct AgentState {
    pub name: String,
    pub display_name: String,
    pub enabled: Enabled,
    /// What the `enabled` answer rests on.
    pub evidence: Vec<String>,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Enabled {
    Yes,
    No,
    /// The evidence needed to answer could not be read or does not exist.
    Unknown,
}

#[derive(Debug, Serialize)]
pub(crate) struct GitShim {
    pub status: ShimStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub hooks: GitHooks,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ShimStatus {
    /// `[git.shim] enabled = true` in the repository configuration.
    Enabled,
    Disabled,
    /// Outside an Atomic workspace there is no shim to ask about.
    NotApplicable,
    Unknown,
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum GitHooks {
    NotApplicable,
    NoGitRepository,
    /// `core.hooksPath` points elsewhere; Atomic does not manage hooks there.
    ExternallyManaged {
        hooks_path: String,
    },
    Inspected {
        dispatchers: Vec<Dispatcher>,
    },
    Unreadable {
        reason: String,
    },
}

#[derive(Debug, Serialize)]
pub(crate) struct Dispatcher {
    pub hook: &'static str,
    /// Stable key, e.g. `atomic_owned`.
    pub state: &'static str,
    /// The wording `atomic git hooks status` prints.
    #[serde(skip)]
    pub label: &'static str,
}

#[derive(Debug, Serialize)]
pub(crate) struct NotCollected {
    pub item: &'static str,
    pub reason: &'static str,
}

// Sources

/// Where collection reads from. Resolved from the process in production;
/// tests point every location at temporary directories.
pub(crate) struct Sources {
    pub home: Option<PathBuf>,
    pub integrations_root: Result<PathBuf, SourceError>,
    pub integrations_root_overridden: bool,
    /// The enclosing Atomic workspace; `Ok(None)` outside one.
    pub workspace: Result<Option<PathBuf>, SourceError>,
    pub install_source: String,
}

/// Only these fixed summaries may cross the boundary into a support report.
#[derive(Clone, Copy, Debug)]
pub(crate) enum SourceError {
    WorkspaceLookup,
    IntegrationsHome,
}

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::WorkspaceLookup => "cannot determine the current workspace",
            Self::IntegrationsHome => "cannot resolve home directory",
        })
    }
}

/// ErrorKind is safe to classify; arbitrary OS error messages are not.
fn io_summary(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::NotFound => "not found",
        ErrorKind::PermissionDenied => "permission denied",
        ErrorKind::InvalidData => "invalid data or encoding",
        ErrorKind::IsADirectory => "is a directory",
        ErrorKind::NotADirectory => "a path component is not a directory",
        _ => "I/O error",
    }
}

// A dangling symlink or a concurrent replacement is not proof of absence.
fn demonstrably_missing(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(_) => return false,
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(_) => return false,
    }
    for ancestor in path.ancestors().skip(1) {
        match std::fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return std::fs::metadata(ancestor).is_ok();
            }
            Ok(_) => return true,
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(_) => return false,
        }
    }
    false
}

impl Sources {
    pub(crate) fn from_process() -> Self {
        let home = dirs::home_dir();
        let workspace = match workspace_search_readable(home.as_deref())
            .map_err(Some)
            .and_then(|()| {
                find_repository_root().map_err(|error| match error {
                    CliError::RepositoryNotFound { .. } => None,
                    _ => Some(SourceError::WorkspaceLookup),
                })
            }) {
            Ok(root) => Ok(Some(root)),
            Err(None) => linked_git_search_readable(home.as_deref()).map(|()| None),
            Err(Some(error)) => Err(error),
        };
        let install_source = std::env::current_exe()
            .map(|exe| detect_source(&exe, &StdEnv).short_label())
            .unwrap_or("unknown")
            .to_string();
        Self {
            home,
            integrations_root: atomic_agent::integrations::integrations_root()
                .map_err(|_| SourceError::IntegrationsHome),
            integrations_root_overridden: std::env::var_os("ATOMIC_INTEGRATIONS_HOME").is_some(),
            workspace,
            install_source,
        }
    }
}

// The shared discovery helper uses boolean file checks; preflight their
// filesystem errors so an inaccessible ancestor is not reported as absent.
fn workspace_search_readable(home: Option<&Path>) -> Result<(), SourceError> {
    let cwd = std::env::current_dir().map_err(|_| SourceError::WorkspaceLookup)?;
    let mut current = cwd
        .canonicalize()
        .map_err(|_| SourceError::WorkspaceLookup)?;
    let home = home.and_then(|path| path.canonicalize().ok());
    loop {
        let dot = current.join(".atomic");
        for path in [
            dot.clone(),
            dot.join("atomic.redb"),
            dot.join("pristine.redb"),
            current.join(".atomic-sandbox"),
        ] {
            match std::fs::metadata(&path) {
                Ok(_) => {}
                Err(error)
                    if error.kind() == ErrorKind::NotFound && demonstrably_missing(&path) => {}
                Err(error)
                    if error.kind() == ErrorKind::NotADirectory && path.starts_with(&dot) => {}
                Err(_) => return Err(SourceError::WorkspaceLookup),
            }
        }
        if (dot.is_dir() && atomic_repository::has_database(&dot))
            || current.join(".atomic-sandbox").is_file()
        {
            break;
        }
        if home.as_deref() == Some(current.as_path()) || !current.pop() {
            break;
        }
    }
    Ok(())
}

// Only called after ordinary discovery found no Atomic parent. Git's shared
// resolver treats a failed rev-parse as absence, even for a broken gitfile.
fn linked_git_search_readable(home: Option<&Path>) -> Result<(), SourceError> {
    let cwd = std::env::current_dir().map_err(|_| SourceError::WorkspaceLookup)?;
    let mut current = cwd
        .canonicalize()
        .map_err(|_| SourceError::WorkspaceLookup)?;
    let home = home.and_then(|path| path.canonicalize().ok());
    loop {
        let git = current.join(".git");
        match std::fs::metadata(&git) {
            Ok(metadata) if metadata.is_file() => return inspect_linked_git(&current),
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound && demonstrably_missing(&git) => {}
            Err(_) => return Err(SourceError::WorkspaceLookup),
        }
        if home.as_deref() == Some(current.as_path()) || !current.pop() {
            return Ok(());
        }
    }
}

fn inspect_linked_git(root: &Path) -> Result<(), SourceError> {
    use std::process::{Command, Stdio};

    let output = Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(root)
        .args([
            "rev-parse",
            "--show-toplevel",
            "--git-common-dir",
            "--git-dir",
            "--git-path",
            "index",
        ])
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|_| SourceError::WorkspaceLookup)?;
    if !output.status.success() {
        return Err(SourceError::WorkspaceLookup);
    }
    let lines: Vec<&[u8]> = output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .collect();
    let [worktree, common, gitdir, _index] = lines.as_slice() else {
        return Err(SourceError::WorkspaceLookup);
    };
    let resolve = |bytes: &[u8]| -> Result<PathBuf, SourceError> {
        if bytes.is_empty() || bytes.contains(&0) || bytes.contains(&b'\r') {
            return Err(SourceError::WorkspaceLookup);
        }
        #[cfg(unix)]
        let path = {
            use std::os::unix::ffi::OsStringExt;
            PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec()))
        };
        #[cfg(not(unix))]
        let path =
            PathBuf::from(std::str::from_utf8(bytes).map_err(|_| SourceError::WorkspaceLookup)?);
        let path = if path.is_absolute() {
            path
        } else {
            root.join(path)
        };
        path.canonicalize()
            .map_err(|_| SourceError::WorkspaceLookup)
    };
    let worktree = resolve(worktree)?;
    let common = resolve(common)?;
    let gitdir = resolve(gitdir)?;
    if worktree != root || !common.is_dir() || !gitdir.is_dir() {
        return Err(SourceError::WorkspaceLookup);
    }
    let atomic = common
        .parent()
        .ok_or(SourceError::WorkspaceLookup)?
        .join(".atomic");
    match std::fs::metadata(&atomic) {
        Ok(metadata) if metadata.is_dir() => Err(SourceError::WorkspaceLookup),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound && demonstrably_missing(&atomic) => {
            Ok(())
        }
        Err(_) => Err(SourceError::WorkspaceLookup),
    }
}

/// One agent adapter, reduced to what the report needs from it.
pub(crate) struct AgentProbe {
    pub name: String,
    pub display_name: String,
}

/// Collect the report from the real environment.
pub(crate) fn collect() -> Report {
    let registry = AgentRegistry::with_defaults();
    let probes = registry
        .iter()
        .map(|agent| AgentProbe {
            name: agent.name().to_string(),
            display_name: agent.display_name().to_string(),
        })
        .collect::<Vec<_>>();
    let packaged = atomic_agent::integrations::registered_integrations();
    collect_from(&Sources::from_process(), &probes, &packaged)
}

/// Collect the report from explicit sources: `probes` are the agent
/// adapters, `packaged` the agents with an integration package.
pub(crate) fn collect_from(
    sources: &Sources,
    probes: &[AgentProbe],
    packaged: &[String],
) -> Report {
    let home = sources.home.as_deref();
    let display_name = |agent: &str| {
        probes
            .iter()
            .find(|probe| probe.name == agent)
            .map_or_else(|| agent.to_string(), |probe| probe.display_name.clone())
    };

    // Receipts, for every packaged agent plus any receipt on disk for an
    // agent this binary no longer lists — that is exactly the
    // version-skew case a support conversation needs to see.
    let mut agents_with_packages: Vec<String> = packaged
        .iter()
        .filter(|name| *name != SKILLS_PACKAGE)
        .cloned()
        .collect();
    if let Ok(root) = &sources.integrations_root {
        for name in receipts_on_disk(root) {
            if name != SKILLS_PACKAGE && !agents_with_packages.contains(&name) {
                agents_with_packages.push(name);
            }
        }
    }
    agents_with_packages.sort();
    let receipts: Vec<(String, ReceiptLookup)> = agents_with_packages
        .into_iter()
        .map(|agent| {
            let lookup = match &sources.integrations_root {
                Ok(root) => read_receipt(&root.join(&agent).join(RECEIPT_FILE)),
                Err(_) => ReceiptLookup::Unreadable(ReceiptError::IntegrationsHome),
            };
            (agent, lookup)
        })
        .collect();
    let receipt_for = |agent: &str| {
        receipts
            .iter()
            .find(|(name, _)| name == agent)
            .map(|(_, lookup)| lookup)
    };

    let package = match &sources.integrations_root {
        Ok(root) => read_skills_package(&root.join(SKILLS_PACKAGE).join("repo")),
        Err(reason) => SkillsPackage::Unreadable {
            reason: format!("integrations directory unresolved: {reason}"),
        },
    };
    let declared = match &package {
        SkillsPackage::Available { skills, .. } => Some(skills.as_slice()),
        _ => None,
    };

    let hook_activity = match &sources.workspace {
        Ok(Some(root)) => read_hook_activity(root),
        Ok(None) => Err("not inside an Atomic workspace"),
        Err(_) => Err("workspace cannot be determined"),
    };

    let mut installed_skills = Vec::new();
    let integrations = receipts
        .iter()
        .map(|(agent, lookup)| {
            let state = match lookup {
                ReceiptLookup::Absent => IntegrationState::NotInstalled,
                ReceiptLookup::Unreadable(reason) => IntegrationState::Unreadable {
                    reason: reason.to_string(),
                },
                ReceiptLookup::Found(receipt) => {
                    let mut files = FileCounts {
                        recorded: receipt.files.len(),
                        ..FileCounts::default()
                    };
                    for file in &receipt.files {
                        let state = file_state(&file.dst, &file.blake3);
                        match state {
                            FileState::Present => files.present += 1,
                            FileState::Modified => files.modified += 1,
                            FileState::Missing => files.missing += 1,
                            FileState::Unreadable => files.unreadable += 1,
                        }
                        if let Some(name) = skill_name(&file.dst, declared) {
                            let metadata = match state {
                                FileState::Present | FileState::Modified => {
                                    skill_metadata(&file.dst)
                                }
                                _ => SkillMetadata::unavailable(),
                            };
                            installed_skills.push(InstalledSkill {
                                agent: agent.clone(),
                                name,
                                path: display_path(&file.dst, home),
                                version: metadata.version,
                                updated_at: metadata.updated_at,
                                metadata_status: metadata.status,
                                missing_metadata: metadata.missing,
                                state,
                            });
                        }
                    }
                    let (source_kind, source) = describe_source(&receipt.source, agent, home);
                    IntegrationState::Installed(InstalledIntegration {
                        version: receipt.version.clone(),
                        installed_with: receipt.cli_version.clone(),
                        installed_at: receipt.installed_at.clone(),
                        last_fired: match &hook_activity {
                            Ok(events) => {
                                events.get(agent).map_or(HookActivity::NotObserved, |at| {
                                    HookActivity::Observed {
                                        at: at.to_rfc3339(),
                                    }
                                })
                            }
                            Err(reason) => HookActivity::Unavailable { reason },
                        },
                        source_kind,
                        source,
                        files,
                    })
                }
            };
            Integration {
                agent: agent.clone(),
                display_name: display_name(agent),
                state,
            }
        })
        .collect();

    let workspace_root = sources.workspace.as_ref().ok().and_then(Option::as_deref);
    let mut agents: Vec<AgentState> = probes
        .iter()
        .map(|probe| {
            let receipt = receipt_for(&probe.name);
            let mut settings: Vec<SettingsProbe> = match receipt {
                Some(ReceiptLookup::Found(receipt)) => receipt
                    .settings
                    .iter()
                    .map(|entry| SettingsProbe {
                        path: "receipt-listed settings".to_string(),
                        hooks: if known_settings_location(
                            &probe.name,
                            &entry.target,
                            home,
                            workspace_root,
                        ) {
                            settings_hooks(
                                &probe.name,
                                &entry.target,
                                &entry.hooks_key,
                                &entry.command_prefix,
                            )
                        } else {
                            HookPresence::Unreadable(HookError::Invalid(
                                "receipt settings location cannot be verified",
                            ))
                        },
                    })
                    .collect(),
                _ => Vec::new(),
            };
            // Built-in adapters can install without a receipt. Inspect the
            // actual configuration instead of trusting their boolean checks.
            let builtin_path = match probe.name.as_str() {
                "claude-code" => Some(".claude/settings.json"),
                "gemini-cli" => Some(".gemini/settings.json"),
                "codex" => Some(".codex/hooks.json"),
                _ => None,
            };
            if let Some(relative) = builtin_path {
                let prefix = format!("atomic agent hooks {}", probe.name);
                for base in [workspace_root, home].into_iter().flatten() {
                    let path = base.join(relative);
                    settings.push(SettingsProbe {
                        path: display_path(&path, home),
                        hooks: settings_hooks(&probe.name, &path, "hooks", &prefix),
                    });
                }
                if probe.name == "claude-code" {
                    if let Some(root) = workspace_root {
                        let path = root.join(".claude/settings.local.json");
                        settings.push(SettingsProbe {
                            path: display_path(&path, home),
                            hooks: settings_hooks(&probe.name, &path, "hooks", &prefix),
                        });
                    }
                }
                if home.is_none() {
                    settings.push(SettingsProbe {
                        path: "home settings".to_string(),
                        hooks: HookPresence::Unreadable(HookError::Invalid(
                            "home directory is unavailable",
                        )),
                    });
                }
                if sources.workspace.is_err() {
                    settings.push(SettingsProbe {
                        path: "workspace settings".to_string(),
                        hooks: HookPresence::Unreadable(HookError::Invalid(
                            "workspace is unavailable",
                        )),
                    });
                }
            }
            let (mut enabled, mut evidence) = classify_enabled(receipt, &settings);
            if builtin_path.is_none() && enabled == Enabled::No {
                enabled = Enabled::Unknown;
                evidence =
                    vec!["active hook or plugin configuration cannot be verified".to_string()];
            }
            AgentState {
                name: probe.name.clone(),
                display_name: probe.display_name.clone(),
                enabled,
                evidence,
            }
        })
        .collect();
    agents.sort_by(|a, b| a.name.cmp(&b.name));

    Report {
        schema: SCHEMA,
        generated_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        atomic: AtomicInfo {
            version: env!("ATOMIC_CLI_VERSION").to_string(),
            install_source: sources.install_source.clone(),
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
        },
        workspace: match &sources.workspace {
            Ok(Some(root)) => Workspace::Found {
                root: display_path(root, home),
            },
            Ok(None) => Workspace::NotFound,
            Err(reason) => Workspace::Unreadable {
                reason: reason.to_string(),
            },
        },
        integrations: Integrations {
            root: sources
                .integrations_root
                .as_ref()
                .ok()
                .map(|root| display_path(root, home)),
            root_overridden: sources.integrations_root_overridden,
            root_error: sources
                .integrations_root
                .as_ref()
                .err()
                .map(ToString::to_string),
            packages: integrations,
        },
        skills: Skills {
            package,
            installed: installed_skills,
        },
        agents,
        git_shim: collect_git_shim(&sources.workspace, home),
        not_collected: vec![NotCollected {
            item: "agent application versions",
            reason: "Atomic keeps no record of the agent applications' own versions, and \
                     detecting them would mean running third-party programs",
        }],
    }
}

/// Read immutable observations without starting the service or opening the
/// repository for writes. Any unreadable source makes the latest time unknown.
fn read_hook_activity(
    root: &Path,
) -> Result<BTreeMap<String, chrono::DateTime<chrono::Utc>>, &'static str> {
    use atomic_core::pristine::tables::SESSION_LOG;
    use redb::{ReadableDatabase, ReadableTable};

    let dot = atomic_repository::Repository::canonical_dot_dir(root)
        .map_err(|_| "hook history location cannot be resolved")?;
    let database = redb::ReadOnlyDatabase::open(dot.join("atomic.redb"))
        .map_err(|_| "hook history cannot be read or is in use")?;
    let txn = database
        .begin_read()
        .map_err(|_| "hook history cannot be read")?;
    let mut times = BTreeMap::new();
    let mut observe = |event: atomic_agent::hook_log::SessionLogEvent| -> Result<(), &'static str> {
        let at = chrono::DateTime::parse_from_rfc3339(&event.at)
            .map_err(|_| "hook history contains an invalid timestamp")?
            .with_timezone(&chrono::Utc);
        times
            .entry(event.agent)
            .and_modify(|last: &mut chrono::DateTime<chrono::Utc>| {
                *last = (*last).max(at);
            })
            .or_insert(at);
        Ok(())
    };
    match txn.open_table(SESSION_LOG) {
        Ok(table) => {
            for entry in table.iter().map_err(|_| "hook history cannot be read")? {
                let (_, value) = entry.map_err(|_| "hook history cannot be read")?;
                let event = serde_json::from_slice(value.value())
                    .map_err(|_| "hook history contains an invalid event")?;
                observe(event)?;
            }
        }
        Err(redb::TableError::TableDoesNotExist(_)) => {}
        Err(_) => return Err("hook history cannot be read"),
    }
    for (_, event) in
        atomic_agent::hook_log::pending(root).map_err(|_| "pending hook history cannot be read")?
    {
        observe(event)?;
    }
    Ok(times)
}

// Receipts and files

enum ReceiptLookup {
    Found(Receipt),
    Absent,
    Unreadable(ReceiptError),
}

#[derive(Debug)]
enum ReceiptError {
    IntegrationsHome,
    Read(ErrorKind),
    Parse(serde_json::error::Category),
}

impl fmt::Display for ReceiptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IntegrationsHome => write!(
                f,
                "integrations directory unresolved: {}",
                SourceError::IntegrationsHome
            ),
            Self::Read(kind) => write!(f, "cannot read receipt: {}", io_summary(*kind)),
            Self::Parse(category) => write!(
                f,
                "corrupt receipt: {}",
                match category {
                    serde_json::error::Category::Io => "I/O error",
                    serde_json::error::Category::Syntax => "invalid JSON syntax",
                    serde_json::error::Category::Data => "invalid receipt data",
                    serde_json::error::Category::Eof => "incomplete JSON",
                }
            ),
        }
    }
}

fn read_receipt(path: &Path) -> ReceiptLookup {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Receipt>(&text) {
            Ok(receipt) => ReceiptLookup::Found(receipt),
            Err(error) => ReceiptLookup::Unreadable(ReceiptError::Parse(error.classify())),
        },
        Err(error) if error.kind() == ErrorKind::NotFound && demonstrably_missing(path) => {
            ReceiptLookup::Absent
        }
        Err(error) => ReceiptLookup::Unreadable(ReceiptError::Read(error.kind())),
    }
}

/// Agent directories under the integrations root that hold a receipt.
fn receipts_on_disk(root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.path().join(RECEIPT_FILE).is_file())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect()
}

fn file_state(path: &Path, recorded_blake3: &str) -> FileState {
    match std::fs::read(path) {
        Ok(bytes) if blake3::hash(&bytes).to_hex().as_str() == recorded_blake3 => {
            FileState::Present
        }
        Ok(_) => FileState::Modified,
        Err(error) if error.kind() == ErrorKind::NotFound => FileState::Missing,
        Err(_) => FileState::Unreadable,
    }
}

/// Whether a settings file still carries this integration's hook commands.
#[derive(Debug)]
enum HookPresence {
    Present,
    Absent,
    Disabled,
    Restricted,
    Unreadable(HookError),
}

#[derive(Debug)]
enum HookError {
    Read(ErrorKind),
    Invalid(&'static str),
}

impl fmt::Display for HookError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(kind) => write!(f, "settings could not be read: {}", io_summary(*kind)),
            Self::Invalid(reason) => f.write_str(reason),
        }
    }
}

struct SettingsProbe {
    path: String,
    hooks: HookPresence,
}

fn known_settings_location(
    agent: &str,
    path: &Path,
    home: Option<&Path>,
    workspace: Option<&Path>,
) -> bool {
    let relative = match agent {
        "claude-code" => ".claude/settings.json",
        "gemini-cli" => ".gemini/settings.json",
        "codex" => ".codex/hooks.json",
        _ => return false,
    };
    [home, workspace]
        .into_iter()
        .flatten()
        .any(|base| base.join(relative) == path)
        || (agent == "claude-code"
            && workspace.is_some_and(|base| base.join(".claude/settings.local.json") == path))
}

fn settings_hooks(agent: &str, path: &Path, hooks_key: &str, command_prefix: &str) -> HookPresence {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(root) => match atomic_agent::hooks::live::json_hooks_active(
                agent,
                &root,
                hooks_key,
                command_prefix,
            ) {
                Ok(active) => {
                    // Named Gemini exclusions can also mask a hook declared in
                    // another settings scope, so don't infer the merged state.
                    let restricted = root.get("hooks").is_some_and(has_disabled_entry)
                        || (agent == "gemini-cli"
                            && root
                                .get("hooksConfig")
                                .and_then(|v| v.get("disabled"))
                                .and_then(serde_json::Value::as_array)
                                .is_some_and(|names| !names.is_empty()));
                    let disabled = root.as_object().is_some_and(|object| {
                        atomic_agent::hooks::live::globally_disabled(agent, object)
                    });
                    match (active, restricted || disabled) {
                        (true, true) => HookPresence::Restricted,
                        (true, false) => HookPresence::Present,
                        (false, true) => HookPresence::Disabled,
                        (false, false) => HookPresence::Absent,
                    }
                }
                Err(reason) => HookPresence::Unreadable(HookError::Invalid(reason)),
            },
            Err(_) => HookPresence::Unreadable(HookError::Invalid("settings could not be parsed")),
        },
        Err(error) if error.kind() == ErrorKind::NotFound && demonstrably_missing(path) => {
            HookPresence::Absent
        }
        Err(error) => HookPresence::Unreadable(HookError::Read(error.kind())),
    }
}

fn has_disabled_entry(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(object) => {
            object.get("disabled") == Some(&serde_json::Value::Bool(true))
                || object.get("enabled") == Some(&serde_json::Value::Bool(false))
                || object.values().any(has_disabled_entry)
        }
        serde_json::Value::Array(values) => values.iter().any(has_disabled_entry),
        _ => false,
    }
}

/// Describe a receipt's source without leaking credentials or the user's
/// home directory.
fn describe_source(source: &str, agent: &str, home: Option<&Path>) -> (&'static str, String) {
    let registry_url = atomic_agent::integrations::resolve(agent).map(|spec| spec.url);
    if registry_url.as_deref() == Some(source) {
        ("registry", source.to_string())
    } else if source.contains("://") {
        ("url", redact::urls(source))
    } else {
        ("local", display_path(Path::new(source), home))
    }
}

// Skills

fn read_skills_package(cache: &Path) -> SkillsPackage {
    let manifest_path = cache.join(atomic_agent::integrations::MANIFEST_FILE);
    let text = match std::fs::read_to_string(&manifest_path) {
        Ok(text) => text,
        Err(error)
            if error.kind() == ErrorKind::NotFound && demonstrably_missing(&manifest_path) =>
        {
            return SkillsPackage::NotFound {
                reason: "no synced atomic-skills package (it is synced by `atomic agent enable`; \
                     `--from-skills` installs leave no cache)"
                    .to_string(),
            }
        }
        Err(error) => {
            return SkillsPackage::Unreadable {
                reason: format!(
                    "cannot read skills package manifest: {}",
                    io_summary(error.kind())
                ),
            }
        }
    };
    match toml::from_str::<IntegrationManifest>(&text) {
        Ok(manifest) if manifest.check_schema().is_err() => SkillsPackage::Unreadable {
            reason: "unsupported skills package manifest schema".to_string(),
        },
        Ok(manifest) => SkillsPackage::Available {
            name: SKILLS_PACKAGE,
            version: manifest.version,
            skills: manifest
                .declared_skills
                .into_iter()
                .map(|skill| skill.name)
                .collect(),
        },
        Err(_) => SkillsPackage::Unreadable {
            reason: "skills package manifest could not be parsed".to_string(),
        },
    }
}

/// The skill an installed file is, if it is one.
///
/// Nested layouts install `<name>/SKILL.md`; flat ones (Cline, agy) install
/// `<name>.md`, which is only recognisable as a skill when the declared skill
/// list is known.
fn skill_name(dst: &Path, declared: Option<&[String]>) -> Option<String> {
    let file_name = dst.file_name()?.to_str()?;
    if file_name == "SKILL.md" {
        let name = dst.parent()?.file_name()?.to_str()?.to_string();
        return match declared {
            Some(declared) if !declared.contains(&name) => None,
            _ => Some(name),
        };
    }
    let stem = file_name.strip_suffix(".md")?;
    declared?
        .iter()
        .any(|name| name == stem)
        .then(|| stem.to_string())
}

fn skill_metadata(path: &Path) -> SkillMetadata {
    match std::fs::read_to_string(path) {
        Ok(text) => frontmatter_metadata(&text),
        Err(_) => SkillMetadata::unavailable(),
    }
}

fn frontmatter_metadata(text: &str) -> SkillMetadata {
    use serde_yaml_ng::Value;

    fn key(name: &str) -> Value {
        Value::String(name.to_string())
    }
    fn string(value: Option<&Value>) -> Result<Option<String>, ()> {
        match value {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(value)) if value.trim().is_empty() => Ok(None),
            Some(Value::String(value)) if !value.chars().any(char::is_control) => {
                Ok(Some(value.trim().to_string()))
            }
            _ => Err(()),
        }
    }
    let incomplete = || SkillMetadata {
        version: None,
        updated_at: None,
        status: SkillMetadataStatus::Incomplete,
        missing: vec!["metadata.version", "metadata.updatedAt"],
    };
    let mut lines = text.lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return incomplete();
    }
    let mut yaml = String::new();
    let mut closed = false;
    for line in lines {
        if line.trim_end() == "---" {
            closed = true;
            break;
        }
        yaml.push_str(line);
        yaml.push('\n');
    }
    if !closed {
        return SkillMetadata::invalid();
    }
    let root = match serde_yaml_ng::from_str(&yaml) {
        Ok(Value::Null) => return incomplete(),
        Ok(Value::Mapping(root)) => root,
        _ => return SkillMetadata::invalid(),
    };
    let legacy_version = string(root.get(key("version"))).unwrap_or(None);
    let metadata = match root.get(key("metadata")) {
        None | Some(Value::Null) => {
            return SkillMetadata {
                version: legacy_version,
                ..incomplete()
            };
        }
        Some(Value::Mapping(metadata)) => metadata,
        _ => return SkillMetadata::invalid(),
    };
    let (version, updated_at) = match (
        string(metadata.get(key("version"))),
        string(metadata.get(key("updatedAt"))),
    ) {
        (Ok(version), Ok(updated_at)) => (version, updated_at),
        _ => return SkillMetadata::invalid(),
    };
    let mut missing = Vec::new();
    if version.is_none() {
        missing.push("metadata.version");
    }
    if updated_at.is_none() {
        missing.push("metadata.updatedAt");
    }
    SkillMetadata {
        version: version.or(legacy_version),
        updated_at,
        status: if missing.is_empty() {
            SkillMetadataStatus::Complete
        } else {
            SkillMetadataStatus::Incomplete
        },
        missing,
    }
}

// Agents

/// Decide whether an agent's Atomic hooks are active, and on what evidence.
///
/// "Installed" (a receipt exists) and "enabled" (hooks are in the agent's
/// live configuration) are separate questions: a receipt survives a user
/// deleting the hooks by hand, and the built-in hooks need no receipt.
fn classify_enabled(
    receipt: Option<&ReceiptLookup>,
    settings: &[SettingsProbe],
) -> (Enabled, Vec<String>) {
    let mut evidence = Vec::new();
    for probe in settings {
        if matches!(
            probe.hooks,
            HookPresence::Present | HookPresence::Restricted
        ) {
            evidence.push(format!("Atomic hook commands present in {}", probe.path));
        }
    }
    let unreadable: Vec<String> = settings
        .iter()
        .filter_map(|probe| match &probe.hooks {
            HookPresence::Unreadable(reason) => {
                Some(format!("cannot inspect {}: {reason}", probe.path))
            }
            _ => None,
        })
        .collect();
    if !unreadable.is_empty() {
        return (Enabled::Unknown, unreadable);
    }
    if !evidence.is_empty() {
        if settings.iter().any(|probe| {
            matches!(
                probe.hooks,
                HookPresence::Disabled | HookPresence::Restricted
            )
        }) {
            return (
                Enabled::Unknown,
                vec![
                    "a settings scope disables hooks; effective enablement cannot be verified"
                        .to_string(),
                ],
            );
        }
        return (Enabled::Yes, evidence);
    }
    match receipt {
        None | Some(ReceiptLookup::Absent) => (Enabled::No, evidence),
        Some(ReceiptLookup::Unreadable(reason)) => (
            Enabled::Unknown,
            vec![format!("integration receipt unreadable: {reason}")],
        ),
        Some(ReceiptLookup::Found(receipt))
            if receipt.settings.is_empty() || settings.is_empty() =>
        {
            (
                Enabled::Unknown,
                vec![
                    "the integration is installed, but its hooks are not in a settings file \
                 Atomic can inspect"
                        .to_string(),
                ],
            )
        }
        Some(ReceiptLookup::Found(_)) => {
            let paths: Vec<&str> = settings.iter().map(|probe| probe.path.as_str()).collect();
            (
                Enabled::No,
                vec![format!(
                    "the inspected settings {} contain no active Atomic hook commands",
                    paths.join(", ")
                )],
            )
        }
    }
}

// Git shim

#[derive(Debug)]
enum ShimConfigError {
    Location,
    Read(ErrorKind),
    Parse,
}

impl fmt::Display for ShimConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Location => f.write_str("cannot resolve repository configuration location"),
            Self::Read(kind) => write!(
                f,
                "cannot read repository configuration: {}",
                io_summary(*kind)
            ),
            Self::Parse => f.write_str("repository configuration could not be parsed"),
        }
    }
}

fn read_shim_config(root: &Path) -> Result<atomic_config::RepoConfig, ShimConfigError> {
    let dir = atomic_repository::Repository::canonical_dot_dir(root)
        .map_err(|_| ShimConfigError::Location)?;
    // Reading directly avoids `Path::exists` treating access errors as an
    // absent config, while preserving the default for a genuinely missing file.
    let path = dir.join("config.toml");
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).map_err(|_| ShimConfigError::Parse),
        Err(error) if error.kind() == ErrorKind::NotFound && demonstrably_missing(&path) => {
            Ok(Default::default())
        }
        Err(error) => Err(ShimConfigError::Read(error.kind())),
    }
}

fn collect_git_shim(
    workspace: &Result<Option<PathBuf>, SourceError>,
    home: Option<&Path>,
) -> GitShim {
    let root = match workspace {
        Ok(Some(root)) => root,
        Ok(None) => {
            return GitShim {
                status: ShimStatus::NotApplicable,
                reason: Some("not inside an Atomic workspace".to_string()),
                hooks: GitHooks::NotApplicable,
            }
        }
        Err(reason) => {
            return GitShim {
                status: ShimStatus::Unknown,
                reason: Some(format!("workspace lookup failed: {reason}")),
                hooks: GitHooks::NotApplicable,
            }
        }
    };

    let (status, reason) = match read_shim_config(root) {
        Ok(config) if config.git.shim.enabled => (ShimStatus::Enabled, None),
        Ok(_) => (ShimStatus::Disabled, None),
        Err(reason) => (ShimStatus::Unknown, Some(reason.to_string())),
    };
    let hooks = match inspect_dispatchers(root) {
        Ok(None) => GitHooks::NoGitRepository,
        Ok(Some(DispatcherReport::External { hooks_path })) => GitHooks::ExternallyManaged {
            hooks_path: display_path(&hooks_path, home),
        },
        Ok(Some(DispatcherReport::Inspected(hooks))) => GitHooks::Inspected {
            dispatchers: hooks
                .into_iter()
                .map(|hook| Dispatcher {
                    hook: hook.name,
                    state: hook.state.code(),
                    label: hook.state.label(),
                })
                .collect(),
        },
        Err(_) => GitHooks::Unreadable {
            reason: "cannot inspect Git repository hooks or administrative paths".to_string(),
        },
    };
    GitShim {
        status,
        reason,
        hooks,
    }
}

/// `path` with the home directory shown as `~`, so a pasted report does not
/// carry the user's account name. The part after `~` is joined with `/` on
/// every platform, so Windows never renders a mixed `~/.x\settings.json`.
fn display_path(path: &Path, home: Option<&Path>) -> String {
    let canonical_home = home.and_then(|home| home.canonicalize().ok());
    let relative = home
        .and_then(|home| path.strip_prefix(home).ok())
        .or_else(|| {
            canonical_home
                .as_deref()
                .and_then(|home| path.strip_prefix(home).ok())
        });
    match relative {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => {
            let parts: Vec<_> = rest
                .components()
                .map(|part| part.as_os_str().to_string_lossy())
                .collect();
            format!("~/{}", parts.join("/"))
        }
        None => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    struct Fixture {
        home: TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                home: TempDir::new().unwrap(),
            }
        }

        fn integrations(&self) -> PathBuf {
            self.home.path().join(".atomic").join("integrations")
        }

        fn sources(&self, workspace: Option<PathBuf>) -> Sources {
            Sources {
                home: Some(self.home.path().to_path_buf()),
                integrations_root: Ok(self.integrations()),
                integrations_root_overridden: false,
                workspace: Ok(workspace),
                install_source: "cargo".to_string(),
            }
        }

        /// Install `files` (relative to home) and write the receipt that
        /// records them as installed with the given content.
        fn install(&self, agent: &str, version: &str, files: &[(&str, &str)]) -> Vec<PathBuf> {
            let paths: Vec<PathBuf> = files
                .iter()
                .map(|(rel, body)| {
                    let path = self.home.path().join(rel);
                    fs::create_dir_all(path.parent().unwrap()).unwrap();
                    fs::write(&path, body).unwrap();
                    path
                })
                .collect();
            let receipt = serde_json::json!({
                "schema": 1,
                "agent": agent,
                "version": version,
                "cli_version": "0.19.0",
                "installed_at": "2026-10-01T12:00:00+00:00",
                "source": "/home/someone/src/pkg",
                "files": paths.iter().zip(files).map(|(path, (_, body))| serde_json::json!({
                    "dst": path,
                    "blake3": blake3::hash(body.as_bytes()).to_hex().to_string(),
                })).collect::<Vec<_>>(),
                "settings": [],
            });
            self.write_receipt(agent, &receipt);
            paths
        }

        fn write_receipt(&self, agent: &str, receipt: &serde_json::Value) {
            let dir = self.integrations().join(agent);
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join(RECEIPT_FILE),
                serde_json::to_string(receipt).unwrap(),
            )
            .unwrap();
        }

        fn skills_cache(&self, manifest: &str) {
            let dir = self.integrations().join(SKILLS_PACKAGE).join("repo");
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join(atomic_agent::integrations::MANIFEST_FILE),
                manifest,
            )
            .unwrap();
        }
    }

    fn probe(name: &str) -> AgentProbe {
        AgentProbe {
            name: name.to_string(),
            display_name: name.to_uppercase(),
        }
    }

    fn integration<'r>(report: &'r Report, agent: &str) -> &'r IntegrationState {
        &report
            .integrations
            .packages
            .iter()
            .find(|p| p.agent == agent)
            .unwrap_or_else(|| panic!("{agent} listed"))
            .state
    }

    fn agent<'r>(report: &'r Report, name: &str) -> &'r AgentState {
        report.agents.iter().find(|a| a.name == name).unwrap()
    }

    #[test]
    fn a_fresh_machine_reports_everything_as_absent_not_guessed() {
        let fixture = Fixture::new();
        let report = collect_from(
            &fixture.sources(None),
            &[probe("claude-code")],
            &["claude-code".to_string(), SKILLS_PACKAGE.to_string()],
        );

        assert!(matches!(report.workspace, Workspace::NotFound));
        assert!(matches!(
            integration(&report, "claude-code"),
            IntegrationState::NotInstalled
        ));
        assert!(
            !report
                .integrations
                .packages
                .iter()
                .any(|p| p.agent == SKILLS_PACKAGE),
            "the skills package is a cache, not an integration"
        );
        assert!(matches!(
            report.skills.package,
            SkillsPackage::NotFound { .. }
        ));
        assert!(report.skills.installed.is_empty());
        assert_eq!(agent(&report, "claude-code").enabled, Enabled::No);
        assert_eq!(report.git_shim.status, ShimStatus::NotApplicable);
        assert!(matches!(report.git_shim.hooks, GitHooks::NotApplicable));
        assert_eq!(report.atomic.version, env!("ATOMIC_CLI_VERSION"));
    }

    #[test]
    fn installed_integrations_report_receipt_versions_and_file_drift() {
        let fixture = Fixture::new();
        let paths = fixture.install(
            "opencode",
            "1.2.0",
            &[
                (".config/opencode/a.md", "a"),
                (".config/opencode/b.md", "b"),
                (".config/opencode/c.md", "c"),
            ],
        );
        fs::write(&paths[1], "edited by the user").unwrap();
        fs::remove_file(&paths[2]).unwrap();

        let report = collect_from(
            &fixture.sources(None),
            &[probe("opencode")],
            &["opencode".to_string()],
        );

        let IntegrationState::Installed(installed) = integration(&report, "opencode") else {
            panic!("opencode is installed");
        };
        assert_eq!(installed.version, "1.2.0");
        assert_eq!(installed.installed_with, "0.19.0");
        assert_eq!(installed.source_kind, "local");
        assert_eq!(
            installed.files,
            FileCounts {
                recorded: 3,
                present: 1,
                modified: 1,
                missing: 1,
                unreadable: 0,
            }
        );
    }

    #[test]
    fn a_corrupt_receipt_is_unreadable_and_leaves_enablement_unknown() {
        let fixture = Fixture::new();
        let dir = fixture.integrations().join("cline");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(RECEIPT_FILE), "{ not json").unwrap();

        let report = collect_from(
            &fixture.sources(None),
            &[probe("cline")],
            &["cline".to_string()],
        );

        assert!(matches!(
            integration(&report, "cline"),
            IntegrationState::Unreadable { .. }
        ));
        let cline = agent(&report, "cline");
        assert_eq!(cline.enabled, Enabled::Unknown);
        assert!(cline.evidence[0].contains("receipt unreadable"));
    }

    #[test]
    fn receipts_for_agents_this_binary_does_not_list_are_still_reported() {
        let fixture = Fixture::new();
        fixture.install("retired-agent", "0.1.0", &[(".retired/x.md", "x")]);

        let report = collect_from(&fixture.sources(None), &[], &[]);

        assert!(matches!(
            integration(&report, "retired-agent"),
            IntegrationState::Installed(_)
        ));
    }

    #[test]
    fn an_unresolvable_integrations_directory_is_reported_not_treated_as_empty() {
        let fixture = Fixture::new();
        let mut sources = fixture.sources(None);
        sources.integrations_root = Err(SourceError::IntegrationsHome);

        let report = collect_from(&sources, &[probe("cline")], &["cline".to_string()]);

        assert_eq!(
            report.integrations.root_error.as_deref(),
            Some("cannot resolve home directory")
        );
        assert!(matches!(
            integration(&report, "cline"),
            IntegrationState::Unreadable { .. }
        ));
        assert!(matches!(
            report.skills.package,
            SkillsPackage::Unreadable { .. }
        ));
        assert_eq!(agent(&report, "cline").enabled, Enabled::Unknown);
    }

    #[test]
    fn failed_workspace_lookup_stays_unknown_in_both_report_fields() {
        let fixture = Fixture::new();
        let mut sources = fixture.sources(None);
        sources.workspace = Err(SourceError::WorkspaceLookup);

        let report = collect_from(&sources, &[], &[]);
        assert!(matches!(report.workspace, Workspace::Unreadable { .. }));
        assert_eq!(report.git_shim.status, ShimStatus::Unknown);
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("cannot determine the current workspace"));
        assert!(!json.contains(&fixture.home.path().display().to_string()));
    }

    #[test]
    fn unsupported_manifest_schema_does_not_echo_the_agent_name() {
        let fixture = Fixture::new();
        fixture.skills_cache("schema = 2\nagent = \"SYNTHETIC_SCHEMA_SECRET\"\nversion = \"1\"\n");
        let package =
            read_skills_package(&fixture.integrations().join(SKILLS_PACKAGE).join("repo"));
        let SkillsPackage::Unreadable { reason } = package else {
            panic!("unsupported manifest must be unreadable");
        };
        assert!(reason.contains("schema"));
        assert!(!reason.contains("SYNTHETIC_SCHEMA_SECRET"));
    }

    #[test]
    fn skills_are_found_from_receipts_with_their_package_and_file_versions() {
        let fixture = Fixture::new();
        fixture.skills_cache(
            "schema = 1\nagent = \"atomic-skills\"\nversion = \"0.4.0\"\n\n\
             [[declared-skill]]\nname = \"atomic-vault\"\n\n\
             [[declared-skill]]\nname = \"atomic-vcs\"\n",
        );
        fixture.install(
            "claude-code",
            "1.0.0",
            &[
                (
                    ".claude/skills/atomic-vault/SKILL.md",
                    "---\nname: atomic-vault\nversion: \"2.1.0\"\n---\nbody\n",
                ),
                (".claude/skills/atomic-vcs/SKILL.md", "no frontmatter\n"),
                (".claude/skills/someone-elses/SKILL.md", "---\n---\n"),
                (".claude/agents/atomic.md", "agent definition"),
            ],
        );
        fixture.install(
            "cline",
            "1.0.0",
            &[("Documents/Cline/Workflows/atomic-vault.md", "flat layout")],
        );

        let report = collect_from(
            &fixture.sources(None),
            &[probe("claude-code"), probe("cline")],
            &["claude-code".to_string(), "cline".to_string()],
        );

        let SkillsPackage::Available {
            version, skills, ..
        } = &report.skills.package
        else {
            panic!("skills cache is readable");
        };
        assert_eq!(version, "0.4.0");
        assert_eq!(skills, &["atomic-vault", "atomic-vcs"]);

        let found: Vec<(&str, &str, Option<&str>)> = report
            .skills
            .installed
            .iter()
            .map(|s| (s.agent.as_str(), s.name.as_str(), s.version.as_deref()))
            .collect();
        assert_eq!(
            found,
            vec![
                ("claude-code", "atomic-vault", Some("2.1.0")),
                ("claude-code", "atomic-vcs", None),
                ("cline", "atomic-vault", None),
            ],
            "undeclared SKILL.md files and non-skill files are not skills"
        );
        assert_eq!(
            report.skills.installed[0].path,
            "~/.claude/skills/atomic-vault/SKILL.md"
        );
    }

    #[test]
    fn without_the_skills_cache_only_nested_skill_files_are_recognised() {
        assert_eq!(
            skill_name(Path::new("/h/.claude/skills/x/SKILL.md"), None).as_deref(),
            Some("x")
        );
        assert_eq!(skill_name(Path::new("/h/Workflows/x.md"), None), None);
    }

    #[test]
    fn skill_metadata_reads_nested_yaml_only_from_frontmatter() {
        let nested = frontmatter_metadata("---\nname: atomic-vault\nmetadata:\n  version: \"1.0.0\"\n  updatedAt: \"2026-10-28T18:18:12Z\"\n---\n");
        assert_eq!(nested.version.as_deref(), Some("1.0.0"));
        assert_eq!(nested.updated_at.as_deref(), Some("2026-10-28T18:18:12Z"));
        assert_eq!(nested.status, SkillMetadataStatus::Complete);
        assert!(nested.missing.is_empty());

        let flow = frontmatter_metadata(
            "---\nmetadata: {updatedAt: '2026-10-29', version: '2.1.0'}\n---\n",
        );
        assert_eq!(flow.version.as_deref(), Some("2.1.0"));
        assert_eq!(flow.updated_at.as_deref(), Some("2026-10-29"));
        assert_eq!(flow.status, SkillMetadataStatus::Complete);

        let partial =
            frontmatter_metadata("---\nmetadata:\n  updatedAt: '2026-10-29'\n---\nversion: 9\n");
        assert_eq!(partial.version, None);
        assert_eq!(partial.missing, ["metadata.version"]);
        assert_eq!(partial.updated_at.as_deref(), Some("2026-10-29"));
        let legacy = frontmatter_metadata("---\nversion: '2.0.0'\nother: {version: 'wrong'}\n---\nmetadata:\n  version: 'wrong'\n");
        assert_eq!(legacy.version.as_deref(), Some("2.0.0"));
        assert_eq!(legacy.missing, ["metadata.version", "metadata.updatedAt"]);
        assert_eq!(
            frontmatter_metadata("version: 9\n").status,
            SkillMetadataStatus::Incomplete
        );
        assert_eq!(
            frontmatter_metadata("---\nmetadata: [broken\n---\n").status,
            SkillMetadataStatus::Invalid
        );
        assert_eq!(
            frontmatter_metadata("---\nmetadata:\n  version: x\n").status,
            SkillMetadataStatus::Invalid
        );
        assert_eq!(
            frontmatter_metadata("---\nmetadata: {version: [1, 2]}\n---\n").status,
            SkillMetadataStatus::Invalid
        );
    }

    fn settings(path: &str, hooks: HookPresence) -> SettingsProbe {
        SettingsProbe {
            path: path.to_string(),
            hooks,
        }
    }

    fn receipt() -> ReceiptLookup {
        ReceiptLookup::Found(
            serde_json::from_value(serde_json::json!({
                "schema": 1, "agent": "x", "version": "1", "cli_version": "1",
                "installed_at": "t", "source": "s", "files": [],
                "settings": [{"target":"/tmp/x", "hooks_key":"hooks", "command_prefix":"atomic agent hooks x"}],
            }))
            .unwrap(),
        )
    }

    #[test]
    fn enabled_rests_on_live_hooks_not_on_the_receipt() {
        // Installed, but the user removed the hooks from settings by hand.
        let (enabled, evidence) = classify_enabled(
            Some(&receipt()),
            &[settings("~/.x/settings.json", HookPresence::Absent)],
        );
        assert_eq!(enabled, Enabled::No);
        assert!(evidence[0].contains("no active Atomic hook commands"));

        // Installed, and the settings file still carries the hooks.
        let (enabled, evidence) = classify_enabled(
            Some(&receipt()),
            &[settings("~/.x/settings.json", HookPresence::Present)],
        );
        assert_eq!(enabled, Enabled::Yes);
        assert_eq!(
            evidence,
            ["Atomic hook commands present in ~/.x/settings.json"]
        );
    }

    #[test]
    fn enabled_is_unknown_when_the_evidence_cannot_be_read() {
        let (enabled, evidence) = classify_enabled(
            Some(&receipt()),
            &[settings(
                "~/.x/settings.json",
                HookPresence::Unreadable(HookError::Read(ErrorKind::PermissionDenied)),
            )],
        );
        assert_eq!(enabled, Enabled::Unknown);
        assert!(evidence[0].contains("permission denied"));

        // A package whose hooks live outside any settings file.
        let (enabled, _) = classify_enabled(Some(&receipt()), &[]);
        assert_eq!(enabled, Enabled::Unknown);

        // No receipt and no hooks in the inspected settings: no.
        let (enabled, evidence) = classify_enabled(None, &[]);
        assert_eq!(enabled, Enabled::No);
        assert!(evidence.is_empty());

        let (enabled, _) = classify_enabled(
            None,
            &[settings(
                "home",
                HookPresence::Unreadable(HookError::Invalid("bad settings")),
            )],
        );
        assert_eq!(enabled, Enabled::Unknown);

        let (enabled, _) = classify_enabled(
            None,
            &[
                settings("workspace", HookPresence::Present),
                settings("home", HookPresence::Disabled),
            ],
        );
        assert_eq!(enabled, Enabled::Unknown);
    }

    #[test]
    fn missing_home_and_failed_workspace_lookup_are_unknown() {
        let fixture = Fixture::new();
        let mut sources = fixture.sources(None);
        sources.home = None;
        let report = collect_from(&sources, &[probe("claude-code")], &[]);
        assert_eq!(agent(&report, "claude-code").enabled, Enabled::Unknown);

        let mut sources = fixture.sources(None);
        sources.workspace = Err(SourceError::WorkspaceLookup);
        let report = collect_from(&sources, &[probe("claude-code")], &[]);
        assert_eq!(agent(&report, "claude-code").enabled, Enabled::Unknown);
    }

    #[test]
    fn receipt_settings_are_checked_against_the_live_file() {
        let fixture = Fixture::new();
        let settings_path = fixture.home.path().join(".claude").join("settings.json");
        fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        fs::write(
            &settings_path,
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"atomic agent hooks claude-code stop"}]}]}}"#,
        )
        .unwrap();
        fixture.write_receipt(
            "claude-code",
            &serde_json::json!({
                "schema": 1, "agent": "claude-code", "version": "3.0.0", "cli_version": "0.19.0",
                "installed_at": "t", "source": "https://alice:s3cret@example.com/pkg?token=1",
                "files": [],
                "settings": [{
                    "target": settings_path,
                    "hooks_key": "hooks",
                    "command_prefix": "atomic agent hooks claude-code",
                }],
            }),
        );

        let report = collect_from(
            &fixture.sources(None),
            &[probe("claude-code")],
            &["claude-code".to_string()],
        );
        let x = agent(&report, "claude-code");
        assert_eq!(x.enabled, Enabled::Yes);
        assert!(x
            .evidence
            .iter()
            .any(|e| e == "Atomic hook commands present in ~/.claude/settings.json"));

        let IntegrationState::Installed(installed) = integration(&report, "claude-code") else {
            panic!("x is installed");
        };
        assert_eq!(installed.source_kind, "url");
        assert_eq!(installed.source, "https://***@example.com/pkg");
    }

    #[test]
    fn gemini_disabled_names_do_not_prove_other_scopes_active() {
        let home = TempDir::new().unwrap();
        let path = home.path().join("settings.json");
        fs::write(
            &path,
            r#"{"hooksConfig":{"disabled":["atomic-test"]},"hooks":{}}"#,
        )
        .unwrap();
        let (enabled, _) = classify_enabled(
            None,
            &[
                settings("workspace", HookPresence::Present),
                settings(
                    "home",
                    settings_hooks(
                        "gemini-cli",
                        &path,
                        "hooks",
                        "atomic agent hooks gemini-cli",
                    ),
                ),
            ],
        );
        assert_eq!(enabled, Enabled::Unknown);
    }

    #[test]
    fn a_workspace_without_git_reports_the_shim_consent_and_no_git() {
        let fixture = Fixture::new();
        let workspace = TempDir::new().unwrap();
        atomic_repository::Repository::init(workspace.path()).unwrap();

        let report = collect_from(
            &fixture.sources(Some(workspace.path().to_path_buf())),
            &[],
            &[],
        );

        assert!(matches!(report.workspace, Workspace::Found { .. }));
        assert_eq!(report.git_shim.status, ShimStatus::Disabled);
        assert!(matches!(report.git_shim.hooks, GitHooks::NoGitRepository));
    }

    #[test]
    fn shim_consent_in_repository_config_reads_as_enabled() {
        let fixture = Fixture::new();
        let workspace = TempDir::new().unwrap();
        drop(git2::Repository::init(workspace.path()).unwrap());
        atomic_repository::Repository::init(workspace.path()).unwrap();
        fs::write(
            workspace.path().join(".atomic").join("config.toml"),
            "[git.shim]\nenabled = true\n",
        )
        .unwrap();

        let report = collect_from(
            &fixture.sources(Some(workspace.path().to_path_buf())),
            &[],
            &[],
        );

        assert_eq!(report.git_shim.status, ShimStatus::Enabled);
        let GitHooks::Inspected { dispatchers } = &report.git_shim.hooks else {
            panic!("a Git repository's hooks are inspected");
        };
        assert_eq!(dispatchers.len(), 5);
        assert!(dispatchers.iter().all(|d| d.state == "not_installed"));
    }

    #[test]
    fn an_unparseable_repository_config_leaves_the_shim_unknown() {
        let fixture = Fixture::new();
        let workspace = TempDir::new().unwrap();
        atomic_repository::Repository::init(workspace.path()).unwrap();
        fs::write(
            workspace.path().join(".atomic").join("config.toml"),
            "[git.shim\n",
        )
        .unwrap();

        let report = collect_from(
            &fixture.sources(Some(workspace.path().to_path_buf())),
            &[],
            &[],
        );

        assert_eq!(report.git_shim.status, ShimStatus::Unknown);
        assert!(report.git_shim.reason.is_some());
    }

    #[test]
    fn home_paths_are_shown_relative_to_tilde() {
        let home = Path::new("/home/alice");
        assert_eq!(
            display_path(Path::new("/home/alice/.claude/settings.json"), Some(home)),
            "~/.claude/settings.json"
        );
        assert_eq!(display_path(home, Some(home)), "~");
        assert_eq!(
            display_path(&home.join(".x").join("settings.json"), Some(home)),
            "~/.x/settings.json",
            "separators after ~ are always /"
        );
        assert_eq!(display_path(Path::new("/opt/x"), Some(home)), "/opt/x");
        assert_eq!(display_path(Path::new("/opt/x"), None), "/opt/x");
    }

    #[test]
    #[cfg(unix)]
    fn canonical_paths_under_a_symlinked_home_are_normalized() {
        let temp = TempDir::new().unwrap();
        let home = temp.path().join("home");
        fs::create_dir(&home).unwrap();
        let alias = temp.path().join("home-alias");
        std::os::unix::fs::symlink(&home, &alias).unwrap();
        assert_eq!(
            display_path(&home.canonicalize().unwrap().join("project"), Some(&alias)),
            "~/project"
        );
    }
}
