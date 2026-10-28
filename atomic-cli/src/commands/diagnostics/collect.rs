//! Gathering the support report.
//!
//! Every fact comes from the running binary or from local configuration that
//! Atomic itself wrote: integration receipts, the `atomic-skills` cache
//! manifest, each agent's hook configuration (through its adapter), and the
//! repository's `.atomic/config.toml`. Nothing is written and nothing touches
//! the network. A value that cannot be read is reported with the reason,
//! never filled in by inference.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use atomic_agent::hooks::AgentRegistry;
use atomic_agent::integrations::{IntegrationManifest, Receipt};
use serde::Serialize;

use crate::commands::find_repository_root;
use crate::commands::git::hook_dispatchers::{inspect_dispatchers, DispatcherReport};
use crate::commands::git::shim::shim_config;
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
    /// `registry`, `local`, or `url`.
    pub source_kind: &'static str,
    /// Where the package came from, with any URL credentials removed.
    pub source: String,
    pub files: FileCounts,
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
    /// The skill file's own `version:` frontmatter; `null` when it declares
    /// none or cannot be read.
    pub version: Option<String>,
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
    pub integrations_root: Result<PathBuf, String>,
    pub integrations_root_overridden: bool,
    /// The enclosing Atomic workspace; `Ok(None)` outside one.
    pub workspace: Result<Option<PathBuf>, String>,
    pub install_source: String,
}

impl Sources {
    pub(crate) fn from_process() -> Self {
        let workspace = match find_repository_root() {
            Ok(root) => Ok(Some(root)),
            Err(CliError::RepositoryNotFound { .. }) => Ok(None),
            Err(error) => Err(error.to_string()),
        };
        let install_source = std::env::current_exe()
            .map(|exe| detect_source(&exe, &StdEnv).short_label())
            .unwrap_or("unknown")
            .to_string();
        Self {
            home: dirs::home_dir(),
            integrations_root: atomic_agent::integrations::integrations_root()
                .map_err(|error| error.to_string()),
            integrations_root_overridden: std::env::var_os("ATOMIC_INTEGRATIONS_HOME").is_some(),
            workspace,
            install_source,
        }
    }
}

/// One agent adapter, reduced to what the report needs from it.
pub(crate) struct AgentProbe<'a> {
    pub name: String,
    pub display_name: String,
    /// The adapter's own "are Atomic hooks installed?" check, run against a
    /// base directory — the check `atomic agent status` uses.
    pub hooks_installed: Box<dyn Fn(&Path) -> bool + 'a>,
}

/// Collect the report from the real environment.
pub(crate) fn collect() -> Report {
    let registry = AgentRegistry::with_defaults();
    let probes = registry
        .iter()
        .map(|agent| AgentProbe {
            name: agent.name().to_string(),
            display_name: agent.display_name().to_string(),
            hooks_installed: Box::new(move |base| agent.is_installed(base)),
        })
        .collect::<Vec<_>>();
    let packaged = atomic_agent::integrations::registered_integrations();
    collect_from(&Sources::from_process(), &probes, &packaged)
}

/// Collect the report from explicit sources: `probes` are the agent
/// adapters, `packaged` the agents with an integration package.
pub(crate) fn collect_from(
    sources: &Sources,
    probes: &[AgentProbe<'_>],
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
                Err(reason) => ReceiptLookup::Unreadable(format!(
                    "integrations directory unresolved: {reason}"
                )),
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

    let mut installed_skills = Vec::new();
    let integrations = receipts
        .iter()
        .map(|(agent, lookup)| {
            let state = match lookup {
                ReceiptLookup::Absent => IntegrationState::NotInstalled,
                ReceiptLookup::Unreadable(reason) => IntegrationState::Unreadable {
                    reason: reason.clone(),
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
                            installed_skills.push(InstalledSkill {
                                agent: agent.clone(),
                                name,
                                path: display_path(&file.dst, home),
                                version: match state {
                                    FileState::Present | FileState::Modified => {
                                        skill_version(&file.dst)
                                    }
                                    _ => None,
                                },
                                state,
                            });
                        }
                    }
                    let (source_kind, source) = describe_source(&receipt.source, agent, home);
                    IntegrationState::Installed(InstalledIntegration {
                        version: receipt.version.clone(),
                        installed_with: receipt.cli_version.clone(),
                        installed_at: receipt.installed_at.clone(),
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
            let adapter = AdapterCheck {
                workspace: workspace_root.map(|root| (probe.hooks_installed)(root)),
                home: home.map(|home| (probe.hooks_installed)(home)),
            };
            let receipt = receipt_for(&probe.name);
            let settings = match receipt {
                Some(ReceiptLookup::Found(receipt)) => receipt
                    .settings
                    .iter()
                    .map(|entry| SettingsProbe {
                        path: display_path(&entry.target, home),
                        hooks: settings_hooks(&entry.target, &entry.command_prefix),
                    })
                    .collect(),
                _ => Vec::new(),
            };
            let (enabled, evidence) = classify_enabled(&probe.name, adapter, receipt, &settings);
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
                reason: reason.clone(),
            },
        },
        integrations: Integrations {
            root: sources
                .integrations_root
                .as_ref()
                .ok()
                .map(|root| display_path(root, home)),
            root_overridden: sources.integrations_root_overridden,
            root_error: sources.integrations_root.as_ref().err().cloned(),
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

// Receipts and files

enum ReceiptLookup {
    Found(Receipt),
    Absent,
    Unreadable(String),
}

fn read_receipt(path: &Path) -> ReceiptLookup {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Receipt>(&text) {
            Ok(receipt) => ReceiptLookup::Found(receipt),
            Err(error) => ReceiptLookup::Unreadable(format!("corrupt receipt: {error}")),
        },
        Err(error) if error.kind() == ErrorKind::NotFound => ReceiptLookup::Absent,
        Err(error) => ReceiptLookup::Unreadable(format!("cannot read receipt: {error}")),
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
    Unreadable(String),
}

struct SettingsProbe {
    path: String,
    hooks: HookPresence,
}

fn settings_hooks(path: &Path, command_prefix: &str) -> HookPresence {
    match std::fs::read_to_string(path) {
        Ok(text) if text.contains(command_prefix) => HookPresence::Present,
        Ok(_) => HookPresence::Absent,
        Err(error) if error.kind() == ErrorKind::NotFound => HookPresence::Absent,
        Err(error) => HookPresence::Unreadable(error.to_string()),
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
    if !cache
        .join(atomic_agent::integrations::MANIFEST_FILE)
        .exists()
    {
        return SkillsPackage::NotFound {
            reason: "no synced atomic-skills package (it is synced by `atomic agent enable`; \
                     `--from-skills` installs leave no cache)"
                .to_string(),
        };
    }
    match IntegrationManifest::load(cache) {
        Ok(manifest) => SkillsPackage::Available {
            name: SKILLS_PACKAGE,
            version: manifest.version,
            skills: manifest
                .declared_skills
                .into_iter()
                .map(|skill| skill.name)
                .collect(),
        },
        Err(error) => SkillsPackage::Unreadable {
            reason: error.to_string(),
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

/// The `version:` key of a skill file's YAML frontmatter, when it has one.
fn skill_version(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    frontmatter_version(&text)
}

fn frontmatter_version(text: &str) -> Option<String> {
    let mut lines = text.lines();
    if lines.next()?.trim_end() != "---" {
        return None;
    }
    lines
        .take_while(|line| line.trim_end() != "---")
        .find_map(|line| line.strip_prefix("version:"))
        .map(|value| value.trim().trim_matches(['"', '\'']).to_string())
        .filter(|value| !value.is_empty())
}

// Agents

/// The adapter's hook check, run from the workspace and from the home
/// directory; `None` where that base does not exist.
#[derive(Clone, Copy)]
struct AdapterCheck {
    workspace: Option<bool>,
    home: Option<bool>,
}

/// Decide whether an agent's Atomic hooks are active, and on what evidence.
///
/// "Installed" (a receipt exists) and "enabled" (hooks are in the agent's
/// live configuration) are separate questions: a receipt survives a user
/// deleting the hooks by hand, and the built-in hooks need no receipt.
fn classify_enabled(
    agent: &str,
    adapter: AdapterCheck,
    receipt: Option<&ReceiptLookup>,
    settings: &[SettingsProbe],
) -> (Enabled, Vec<String>) {
    let mut evidence = Vec::new();
    match (adapter.workspace, adapter.home) {
        (Some(true), Some(true)) => {
            evidence.push(format!("the {agent} adapter finds Atomic hooks installed"))
        }
        (Some(true), _) => evidence.push(format!(
            "the {agent} adapter finds Atomic hooks in the workspace configuration"
        )),
        (_, Some(true)) => evidence.push(format!(
            "the {agent} adapter finds Atomic hooks in the home-directory configuration"
        )),
        _ => {}
    }
    for probe in settings {
        if matches!(probe.hooks, HookPresence::Present) {
            evidence.push(format!("Atomic hook commands present in {}", probe.path));
        }
    }
    if !evidence.is_empty() {
        return (Enabled::Yes, evidence);
    }

    match receipt {
        None | Some(ReceiptLookup::Absent) => (Enabled::No, evidence),
        Some(ReceiptLookup::Unreadable(reason)) => (
            Enabled::Unknown,
            vec![format!("integration receipt unreadable: {reason}")],
        ),
        Some(ReceiptLookup::Found(_)) if settings.is_empty() => (
            Enabled::Unknown,
            vec![
                "the integration is installed, but its hooks are not in a settings file \
                 Atomic can inspect, and the adapter reports none"
                    .to_string(),
            ],
        ),
        Some(ReceiptLookup::Found(_)) => {
            let unreadable: Vec<String> = settings
                .iter()
                .filter_map(|probe| match &probe.hooks {
                    HookPresence::Unreadable(error) => {
                        Some(format!("cannot read {}: {error}", probe.path))
                    }
                    _ => None,
                })
                .collect();
            if unreadable.is_empty() {
                let paths: Vec<&str> = settings.iter().map(|probe| probe.path.as_str()).collect();
                (
                    Enabled::No,
                    vec![format!(
                        "the integration receipt lists {}, but no Atomic hook commands remain there",
                        paths.join(", ")
                    )],
                )
            } else {
                (Enabled::Unknown, unreadable)
            }
        }
    }
}

// Git shim

fn collect_git_shim(workspace: &Result<Option<PathBuf>, String>, home: Option<&Path>) -> GitShim {
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

    let (status, reason) = match shim_config(root) {
        Ok(config) if config.git.shim.enabled => (ShimStatus::Enabled, None),
        Ok(_) => (ShimStatus::Disabled, None),
        Err(error) => (ShimStatus::Unknown, Some(error.to_string())),
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
        Err(error) => GitHooks::Unreadable {
            reason: error.to_string(),
        },
    };
    GitShim {
        status,
        reason,
        hooks,
    }
}

/// `path` with the home directory shown as `~`, so a pasted report does not
/// carry the user's account name.
fn display_path(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.display()),
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

    fn probe(name: &str, installed: bool) -> AgentProbe<'static> {
        AgentProbe {
            name: name.to_string(),
            display_name: name.to_uppercase(),
            hooks_installed: Box::new(move |_| installed),
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
            &[probe("claude-code", false)],
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
            &[probe("opencode", false)],
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
            &[probe("cline", false)],
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
        sources.integrations_root = Err("cannot resolve home directory".to_string());

        let report = collect_from(&sources, &[probe("cline", false)], &["cline".to_string()]);

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
            &[probe("claude-code", false), probe("cline", false)],
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
    fn frontmatter_version_is_read_only_from_frontmatter() {
        assert_eq!(
            frontmatter_version("---\nversion: 1.2.3\n---\n").as_deref(),
            Some("1.2.3")
        );
        assert_eq!(
            frontmatter_version("---\nversion: '1.2.3'\n---\n").as_deref(),
            Some("1.2.3")
        );
        assert_eq!(frontmatter_version("---\nname: x\n---\nversion: 9\n"), None);
        assert_eq!(frontmatter_version("version: 9\n"), None);
        assert_eq!(frontmatter_version("---\nversion:\n---\n"), None);
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
            }))
            .unwrap(),
        )
    }

    const NOTHING: AdapterCheck = AdapterCheck {
        workspace: Some(false),
        home: Some(false),
    };

    #[test]
    fn enabled_rests_on_live_hooks_not_on_the_receipt() {
        // Installed, but the user removed the hooks from settings by hand.
        let (enabled, evidence) = classify_enabled(
            "x",
            NOTHING,
            Some(&receipt()),
            &[settings("~/.x/settings.json", HookPresence::Absent)],
        );
        assert_eq!(enabled, Enabled::No);
        assert!(evidence[0].contains("no Atomic hook commands remain"));

        // Installed, and the settings file still carries the hooks.
        let (enabled, evidence) = classify_enabled(
            "x",
            NOTHING,
            Some(&receipt()),
            &[settings("~/.x/settings.json", HookPresence::Present)],
        );
        assert_eq!(enabled, Enabled::Yes);
        assert_eq!(
            evidence,
            ["Atomic hook commands present in ~/.x/settings.json"]
        );

        // Built-in hooks need no receipt at all.
        let (enabled, evidence) = classify_enabled(
            "x",
            AdapterCheck {
                workspace: Some(true),
                home: Some(false),
            },
            None,
            &[],
        );
        assert_eq!(enabled, Enabled::Yes);
        assert!(evidence[0].contains("workspace configuration"));
    }

    #[test]
    fn enabled_is_unknown_when_the_evidence_cannot_be_read() {
        let (enabled, evidence) = classify_enabled(
            "x",
            NOTHING,
            Some(&receipt()),
            &[settings(
                "~/.x/settings.json",
                HookPresence::Unreadable("permission denied".into()),
            )],
        );
        assert_eq!(enabled, Enabled::Unknown);
        assert!(evidence[0].contains("permission denied"));

        // A package whose hooks live outside any settings file.
        let (enabled, _) = classify_enabled("x", NOTHING, Some(&receipt()), &[]);
        assert_eq!(enabled, Enabled::Unknown);

        // Outside a workspace with no adapter hit and no receipt: no.
        let (enabled, evidence) = classify_enabled(
            "x",
            AdapterCheck {
                workspace: None,
                home: Some(false),
            },
            None,
            &[],
        );
        assert_eq!(enabled, Enabled::No);
        assert!(evidence.is_empty());
    }

    #[test]
    fn receipt_settings_are_checked_against_the_live_file() {
        let fixture = Fixture::new();
        let settings_path = fixture.home.path().join(".x").join("settings.json");
        fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        fs::write(
            &settings_path,
            r#"{"hooks":{"Stop":[{"command":"atomic agent hooks x stop"}]}}"#,
        )
        .unwrap();
        fixture.write_receipt(
            "x",
            &serde_json::json!({
                "schema": 1, "agent": "x", "version": "3.0.0", "cli_version": "0.19.0",
                "installed_at": "t", "source": "https://alice:s3cret@example.com/pkg?token=1",
                "files": [],
                "settings": [{
                    "target": settings_path,
                    "hooks_key": "hooks",
                    "command_prefix": "atomic agent hooks x",
                }],
            }),
        );

        let report = collect_from(
            &fixture.sources(None),
            &[probe("x", false)],
            &["x".to_string()],
        );
        let x = agent(&report, "x");
        assert_eq!(x.enabled, Enabled::Yes);
        assert_eq!(
            x.evidence,
            ["Atomic hook commands present in ~/.x/settings.json"]
        );

        let IntegrationState::Installed(installed) = integration(&report, "x") else {
            panic!("x is installed");
        };
        assert_eq!(installed.source_kind, "url");
        assert_eq!(installed.source, "https://***@example.com/pkg");
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
        assert_eq!(display_path(Path::new("/opt/x"), Some(home)), "/opt/x");
        assert_eq!(display_path(Path::new("/opt/x"), None), "/opt/x");
    }
}
