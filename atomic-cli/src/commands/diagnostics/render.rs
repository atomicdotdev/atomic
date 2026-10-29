//! Plain-text rendering of the support report.
//!
//! The text is meant to be pasted into a support conversation, so it carries
//! no color or terminal control codes, keeps one fact per line, and names
//! every unknown value as unknown with its reason.

use std::fmt::Write;

use super::collect::{
    AgentState, Enabled, FileCounts, FileState, GitHooks, HookActivity, IntegrationState, Report,
    ShimStatus, SkillMetadataStatus, SkillsPackage, Workspace,
};

/// Width of the label column.
const LABEL: usize = 16;

pub(crate) fn render_text(report: &Report) -> String {
    let mut out = String::new();
    let o = &mut out;

    line(o, "Atomic diagnostics");
    line(o, "==================");
    field(o, "Generated", &report.generated_at);
    blank(o);

    section(o, "Atomic");
    field(o, "Version", &report.atomic.version);
    field(o, "Install source", &report.atomic.install_source);
    field(
        o,
        "Platform",
        &format!("{} {}", report.atomic.os, report.atomic.arch),
    );
    let workspace = match &report.workspace {
        Workspace::Found { root } => root.clone(),
        Workspace::NotFound => "none (not inside an Atomic workspace)".to_string(),
        Workspace::Unreadable { reason } => format!("unknown — {reason}"),
    };
    field(o, "Workspace", &workspace);
    blank(o);

    render_integrations(o, report);
    render_skills(o, report);
    render_agents(o, &report.agents);
    render_shim(o, report);

    section(o, "Not collected");
    for item in &report.not_collected {
        line(o, &format!("  {}: {}", item.item, item.reason));
    }

    out
}

fn render_integrations(o: &mut String, report: &Report) {
    section(o, "Agent integrations (installed packages)");
    let integrations = &report.integrations;
    let root = match (&integrations.root, &integrations.root_error) {
        (Some(root), _) if integrations.root_overridden => {
            format!("{root} (set by ATOMIC_INTEGRATIONS_HOME)")
        }
        (Some(root), _) => root.clone(),
        (None, Some(error)) => format!("unknown — {error}"),
        (None, None) => "unknown".to_string(),
    };
    field(o, "Receipts", &root);

    let mut not_installed = Vec::new();
    for package in &integrations.packages {
        let name = format!("{} ({})", package.display_name, package.agent);
        match &package.state {
            IntegrationState::Installed(installed) => {
                line(o, &format!("  {name}"));
                field(o, "  Version", &installed.version);
                field(
                    o,
                    "  Installed",
                    &format!(
                        "{} with atomic {}",
                        installed.installed_at, installed.installed_with
                    ),
                );
                let last_fired = match &installed.last_fired {
                    HookActivity::Observed { at } => format!("{at} (current repository)"),
                    HookActivity::NotObserved => "not observed in this repository".to_string(),
                    HookActivity::Unavailable { reason } => format!("unknown — {reason}"),
                };
                field(o, "  Last fired", &last_fired);
                field(
                    o,
                    "  Source",
                    &format!("{} ({})", installed.source, installed.source_kind),
                );
                field(o, "  Files", &file_counts(&installed.files));
            }
            IntegrationState::Unreadable { reason } => {
                line(o, &format!("  {name}"));
                field(o, "  Version", &format!("unknown — {reason}"));
            }
            IntegrationState::NotInstalled => not_installed.push(package.agent.as_str()),
        }
    }
    if !not_installed.is_empty() {
        field(o, "Not installed", &not_installed.join(", "));
    }
    blank(o);
}

fn file_counts(files: &FileCounts) -> String {
    let mut parts = vec![format!("{} recorded", files.recorded)];
    for (count, label) in [
        (files.present, "unchanged"),
        (files.modified, "modified"),
        (files.missing, "missing"),
        (files.unreadable, "unreadable"),
    ] {
        if count > 0 {
            parts.push(format!("{count} {label}"));
        }
    }
    parts.join(", ")
}

fn render_skills(o: &mut String, report: &Report) {
    section(o, "Skills");
    let package = match &report.skills.package {
        SkillsPackage::Available { name, version, .. } => {
            format!("{name} {version} (cached by the last `atomic agent enable`)")
        }
        SkillsPackage::NotFound { reason } => format!("unknown — {reason}"),
        SkillsPackage::Unreadable { reason } => format!("unknown — {reason}"),
    };
    field(o, "Skills package", &package);
    if report.skills.installed.is_empty() {
        field(o, "Installed", "none recorded in integration receipts");
    }
    for skill in &report.skills.installed {
        let version = skill.version.as_deref().map_or_else(
            || match skill.metadata_status {
                SkillMetadataStatus::Incomplete => "no version declared".to_string(),
                _ => "version unknown".to_string(),
            },
            |v| format!("v{v}"),
        );
        let state = match skill.state {
            FileState::Present => "",
            FileState::Modified => ", modified since install",
            FileState::Missing => ", missing",
            FileState::Unreadable => ", unreadable",
        };
        let metadata = match skill.metadata_status {
            SkillMetadataStatus::Complete => format!(
                ", updated {}",
                skill.updated_at.as_deref().unwrap_or("unknown")
            ),
            SkillMetadataStatus::Incomplete => format!(
                ", missing {} (older or unversioned copy)",
                skill.missing_metadata.join(", ")
            ),
            SkillMetadataStatus::Invalid => ", invalid skill metadata".to_string(),
            SkillMetadataStatus::Unavailable => ", skill metadata unavailable".to_string(),
        };
        line(
            o,
            &format!(
                "  {} for {} — {version}{metadata}{state}",
                skill.name, skill.agent
            ),
        );
    }
    blank(o);
}

fn render_agents(o: &mut String, agents: &[AgentState]) {
    section(o, "Enabled agents (Atomic hooks active)");
    let mut not_enabled = Vec::new();
    for agent in agents {
        let answer = match agent.enabled {
            Enabled::Yes => "yes",
            Enabled::Unknown => "unknown",
            Enabled::No if agent.evidence.is_empty() => {
                not_enabled.push(agent.name.as_str());
                continue;
            }
            Enabled::No => "no",
        };
        line(
            o,
            &format!("  {} ({}): {answer}", agent.display_name, agent.name),
        );
        for evidence in &agent.evidence {
            line(o, &format!("    - {evidence}"));
        }
    }
    if agents.iter().all(|agent| agent.enabled == Enabled::No) {
        line(o, "  No agent has Atomic hooks enabled.");
    }
    if !not_enabled.is_empty() {
        field(o, "Not enabled", &not_enabled.join(", "));
    }
    blank(o);
}

fn render_shim(o: &mut String, report: &Report) {
    section(o, "Git shim");
    let shim = &report.git_shim;
    let status = match shim.status {
        ShimStatus::Enabled => "yes ([git.shim] enabled = true)".to_string(),
        ShimStatus::Disabled => "no".to_string(),
        ShimStatus::NotApplicable => format!(
            "not applicable — {}",
            shim.reason.as_deref().unwrap_or("no workspace")
        ),
        ShimStatus::Unknown => format!(
            "unknown — {}",
            shim.reason.as_deref().unwrap_or("not readable")
        ),
    };
    field(o, "Enabled", &status);
    match &shim.hooks {
        GitHooks::NotApplicable => {}
        GitHooks::NoGitRepository => field(o, "Git hooks", "no Git repository in this workspace"),
        GitHooks::ExternallyManaged { hooks_path } => field(
            o,
            "Git hooks",
            &format!("managed outside Atomic (core.hooksPath = {hooks_path})"),
        ),
        GitHooks::Unreadable { reason } => field(o, "Git hooks", &format!("unknown — {reason}")),
        GitHooks::Inspected { dispatchers } => {
            field(o, "Git hooks", "");
            for dispatcher in dispatchers {
                line(o, &format!("    {}: {}", dispatcher.hook, dispatcher.label));
            }
        }
    }
    blank(o);
}

fn section(o: &mut String, title: &str) {
    line(o, title);
}

fn field(o: &mut String, label: &str, value: &str) {
    let label = format!("{label}:");
    line(o, format!("  {label:<LABEL$} {value}").trim_end());
}

fn line(o: &mut String, text: &str) {
    let _ = writeln!(o, "{text}");
}

fn blank(o: &mut String) {
    o.push('\n');
}

#[cfg(test)]
mod tests {
    use super::super::collect::*;
    use super::*;

    fn report() -> Report {
        Report {
            schema: SCHEMA,
            generated_at: "2026-10-28T12:00:00Z".to_string(),
            atomic: AtomicInfo {
                version: "0.20.0".to_string(),
                install_source: "cargo".to_string(),
                os: "linux",
                arch: "x86_64",
            },
            workspace: Workspace::NotFound,
            integrations: Integrations {
                root: Some("~/.atomic/integrations".to_string()),
                root_overridden: false,
                root_error: None,
                packages: vec![
                    Integration {
                        agent: "claude-code".to_string(),
                        display_name: "Claude Code".to_string(),
                        state: IntegrationState::Installed(InstalledIntegration {
                            version: "1.4.0".to_string(),
                            installed_with: "0.19.0".to_string(),
                            installed_at: "2026-10-01T12:00:00+00:00".to_string(),
                            last_fired: HookActivity::Observed {
                                at: "2026-10-28T11:00:00+00:00".to_string(),
                            },
                            source_kind: "registry",
                            source: "https://example.atomic.storage/pkg".to_string(),
                            files: FileCounts {
                                recorded: 3,
                                present: 2,
                                missing: 1,
                                ..FileCounts::default()
                            },
                        }),
                    },
                    Integration {
                        agent: "cline".to_string(),
                        display_name: "Cline".to_string(),
                        state: IntegrationState::NotInstalled,
                    },
                    Integration {
                        agent: "codex".to_string(),
                        display_name: "Codex".to_string(),
                        state: IntegrationState::NotInstalled,
                    },
                ],
            },
            skills: Skills {
                package: SkillsPackage::NotFound {
                    reason: "no synced atomic-skills package".to_string(),
                },
                installed: vec![InstalledSkill {
                    agent: "claude-code".to_string(),
                    name: "atomic-vault".to_string(),
                    path: "~/.claude/skills/atomic-vault/SKILL.md".to_string(),
                    state: FileState::Modified,
                    version: None,
                    updated_at: None,
                    metadata_status: SkillMetadataStatus::Incomplete,
                    missing_metadata: vec!["metadata.version", "metadata.updatedAt"],
                }],
            },
            agents: vec![
                AgentState {
                    name: "claude-code".to_string(),
                    display_name: "Claude Code".to_string(),
                    enabled: Enabled::Yes,
                    evidence: vec![
                        "Atomic hook commands present in ~/.claude/settings.json".to_string()
                    ],
                },
                AgentState {
                    name: "cline".to_string(),
                    display_name: "Cline".to_string(),
                    enabled: Enabled::No,
                    evidence: vec![],
                },
                AgentState {
                    name: "kiro".to_string(),
                    display_name: "Kiro".to_string(),
                    enabled: Enabled::Unknown,
                    evidence: vec!["integration receipt unreadable: bad json".to_string()],
                },
            ],
            git_shim: GitShim {
                status: ShimStatus::NotApplicable,
                reason: Some("not inside an Atomic workspace".to_string()),
                hooks: GitHooks::NotApplicable,
            },
            not_collected: vec![NotCollected {
                item: "agent application versions",
                reason: "not recorded",
            }],
        }
    }

    #[test]
    fn text_report_names_every_section_and_value() {
        let text = render_text(&report());
        let expected = "\
Atomic diagnostics
==================
  Generated:       2026-10-28T12:00:00Z

Atomic
  Version:         0.20.0
  Install source:  cargo
  Platform:        linux x86_64
  Workspace:       none (not inside an Atomic workspace)

Agent integrations (installed packages)
  Receipts:        ~/.atomic/integrations
  Claude Code (claude-code)
    Version:       1.4.0
    Installed:     2026-10-01T12:00:00+00:00 with atomic 0.19.0
    Last fired:    2026-10-28T11:00:00+00:00 (current repository)
    Source:        https://example.atomic.storage/pkg (registry)
    Files:         3 recorded, 2 unchanged, 1 missing
  Not installed:   cline, codex

Skills
  Skills package:  unknown — no synced atomic-skills package
  atomic-vault for claude-code — no version declared, missing metadata.version, metadata.updatedAt (older or unversioned copy), modified since install

Enabled agents (Atomic hooks active)
  Claude Code (claude-code): yes
    - Atomic hook commands present in ~/.claude/settings.json
  Kiro (kiro): unknown
    - integration receipt unreadable: bad json
  Not enabled:     cline

Git shim
  Enabled:         not applicable — not inside an Atomic workspace

Not collected
  agent application versions: not recorded
";
        assert_eq!(text, expected);
    }

    #[test]
    fn text_report_is_plain_ascii_layout_without_control_codes() {
        let text = render_text(&report());
        assert!(
            !text.contains('\u{1b}'),
            "no ANSI escapes in a pasted report"
        );
        assert!(
            text.lines().all(|l| l == l.trim_end()),
            "no trailing spaces"
        );
    }

    #[test]
    fn shim_and_hooks_render_their_states() {
        let mut report = report();
        report.workspace = Workspace::Found {
            root: "~/code/app".to_string(),
        };
        report.git_shim = GitShim {
            status: ShimStatus::Enabled,
            reason: None,
            hooks: GitHooks::Inspected {
                dispatchers: vec![
                    Dispatcher {
                        hook: "post-checkout",
                        state: "atomic_owned",
                        label: "Atomic-owned dispatcher installed",
                    },
                    Dispatcher {
                        hook: "pre-push",
                        state: "not_installed",
                        label: "not installed",
                    },
                ],
            },
        };
        let text = render_text(&report);
        assert!(text.contains("  Workspace:       ~/code/app\n"));
        assert!(text.contains("  Enabled:         yes ([git.shim] enabled = true)\n"));
        assert!(text.contains(
            "  Git hooks:\n    post-checkout: Atomic-owned dispatcher installed\n    \
             pre-push: not installed\n"
        ));

        report.git_shim = GitShim {
            status: ShimStatus::Unknown,
            reason: Some("cannot load the repository configuration".to_string()),
            hooks: GitHooks::NoGitRepository,
        };
        let text = render_text(&report);
        assert!(text
            .contains("  Enabled:         unknown — cannot load the repository configuration\n"));
        assert!(text.contains("  Git hooks:       no Git repository in this workspace\n"));
    }

    #[test]
    fn no_enabled_agents_is_said_outright() {
        let mut report = report();
        for agent in &mut report.agents {
            agent.enabled = Enabled::No;
            agent.evidence.clear();
        }
        let text = render_text(&report);
        assert!(text.contains("  No agent has Atomic hooks enabled.\n"));
        assert!(text.contains("  Not enabled:     claude-code, cline, kiro\n"));
    }

    #[test]
    fn json_report_carries_explicit_statuses() {
        let value = serde_json::to_value(report()).unwrap();
        assert_eq!(value["schema"], SCHEMA);
        assert_eq!(value["workspace"]["status"], "not_found");
        assert_eq!(value["integrations"]["packages"][0]["status"], "installed");
        assert_eq!(value["integrations"]["packages"][0]["version"], "1.4.0");
        assert_eq!(
            value["integrations"]["packages"][1]["status"],
            "not_installed"
        );
        assert_eq!(value["skills"]["package"]["status"], "not_found");
        assert!(value["skills"]["installed"][0]["version"].is_null());
        assert_eq!(value["agents"][2]["enabled"], "unknown");
        assert_eq!(value["git_shim"]["status"], "not_applicable");
        assert_eq!(value["git_shim"]["hooks"]["status"], "not_applicable");
    }
}
