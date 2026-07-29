//! `atomic intent validate <ID|path>` — gate an intent against the canonical
//! shapes.

use std::path::Path;

use clap::Parser;

use atomic_canonical::lift::{lift_intent, parse_markdown};
use atomic_canonical::{validate_intent, ValidationReport, Violation};
use atomic_core::types::{Base32, Hash};
use atomic_repository::Repository;

use crate::commands::intent::bridge;
use crate::commands::intent::validation_failed;
use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::agent_doc::{Doc, Fail, Ref};

/// Validate an intent against the canonical shapes.
#[derive(Parser, Debug)]
#[command(name = "validate")]
pub struct IntentValidate {
    /// Intent ID (e.g. "PIMO-1" or "1"), or a path to an intent markdown file.
    pub id_or_path: String,

    /// Output the validation report as JSON.
    #[arg(long)]
    pub json: bool,
}

/// Agent guidance for `atomic intent validate`.
///
/// The last `fails:` row is deliberately fix-less. A non-conforming report is
/// reported with exit 2 even though the command parsed and RAN to completion —
/// the root taxonomy's "2 = usage, did not run" would otherwise send an agent
/// back to `--help` for a verdict it already has.
pub const DOC: Doc = Doc {
    when: "an intent changed and you need the gate's verdict",
    run: "intent validate <ID> --json",
    needs: &[
        Ref {
            cmd: "intent attest <ID>",
            note: "fills attributedTo + proof",
        },
        Ref {
            cmd: "vault sync",
            note: "after editing any .vault file",
        },
    ],
    then: &[
        Ref {
            cmd: "intent verify <ID>",
            note: "check the signature too",
        },
    ],
    instead: &[
        Ref {
            cmd: "intent verify <ID>",
            note: "the signature, not the shapes",
        },
        Ref {
            cmd: "intent list --json",
            note: "gate state for every intent at once",
        },
    ],
    fails: &[
        Fail {
            cond: "un-attested: attributedTo and proof are always missing",
            exit: 2,
            fix: Ref {
                cmd: "intent attest <ID>",
                note: "",
            },
        },
        Fail {
            cond: "status=met criterion with no verifiedBy+evidence",
            exit: 2,
            fix: Ref {
                cmd: "vault sync",
                note: "",
            },
        },
        Fail {
            cond: ".vault file edited but not synced: stale answer",
            exit: 0,
            fix: Ref {
                cmd: "vault sync",
                note: "",
            },
        },
        Fail {
            cond: "path form skips the attestation; proof always missing",
            exit: 2,
            fix: Ref {
                cmd: "intent validate <ID>",
                note: "",
            },
        },
        Fail {
            cond: "report says does not conform - the gate DID run",
            exit: 2,
            fix: Ref::UNFIXABLE,
        },
    ],
    ..Doc::EMPTY
};

impl Command for IntentValidate {
    fn run(&self) -> CliResult<()> {
        // Resolve the argument. A path-shaped argument (ends in `.md` or
        // contains a separator) is loaded as a file — and if it doesn't exist
        // we report FileNotFound rather than misreporting it as a bad ID.
        // Otherwise the argument is an intent ID read through the vault bridge;
        // if a FRESH attestation sidecar exists we gate the attested node (so a
        // previously-attested intent reports conforms), a STALE one warns and
        // falls back to the raw node.
        let node = if is_path_shaped(&self.id_or_path) {
            let path = Path::new(&self.id_or_path);
            if !path.is_file() {
                return Err(CliError::FileNotFound {
                    path: path.to_path_buf(),
                });
            }
            let doc = std::fs::read_to_string(path).map_err(CliError::Io)?;
            let (fm, body) = parse_markdown(&doc).map_err(|e| CliError::InvalidArgument {
                message: format!("could not parse markdown frontmatter: {e}"),
            })?;
            lift_intent(&fm, &body).map_err(|e| CliError::InvalidArgument {
                message: format!("could not lift intent: {e}"),
            })?
        } else {
            let root = find_repository_root()?;
            let repo = Repository::open(&root).map_err(CliError::Repository)?;
            let inputs = bridge::read_intent(&repo, &self.id_or_path)?;
            match bridge::load_attestation(&repo, &self.id_or_path, &inputs)? {
                bridge::Attestation::Fresh(node) => *node,
                bridge::Attestation::Stale(_) => {
                    eprintln!(
                        "warning: the attestation for {} is stale (the intent changed since it \
                         was signed); re-run `atomic intent attest {}`.",
                        self.id_or_path, self.id_or_path
                    );
                    bridge::lift(&inputs)?
                }
                bridge::Attestation::None => bridge::lift(&inputs)?,
            }
        };

        let mut report = validate_intent(&node);

        // Evidence URNs must RESOLVE to real changes when a repository is
        // reachable — the gate only checks presence. (Recording the Why: a met
        // acceptance criterion's evidence must resolve, not just exist.)
        let evidence: Vec<(String, String)> = node
            .has_acceptance_criterion
            .iter()
            .filter_map(|ac| ac.evidence.clone().map(|e| (ac.id.clone(), e)))
            .collect();
        let extra = check_evidence_resolution(&evidence, find_repository_root().ok().as_deref());
        if !extra.is_empty() {
            report.conforms = false;
            report.results.extend(extra);
        }

        if self.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&report_json(&report)).unwrap()
            );
        } else {
            print!("{report}");
        }

        if report.conforms {
            Ok(())
        } else {
            Err(validation_failed(format!(
                "intent does not conform ({} violation(s))",
                report.results.len()
            )))
        }
    }
}

/// Is the argument *shaped* like a filesystem path (rather than an intent ID)?
/// True when it ends in `.md` or contains a path separator. We decide path-vs-ID
/// on shape, not existence, so a mistyped `./plan.md` is reported as a missing
/// file (FileNotFound) instead of misleadingly falling through to "invalid ID".
/// Intent IDs are `PREFIX-N` / `N` — no `.md` suffix, no separator.
fn is_path_shaped(arg: &str) -> bool {
    arg.ends_with(".md") || arg.contains('/') || arg.contains(std::path::MAIN_SEPARATOR)
}

/// Resolve `urn:atomic:change:<hash>` evidence URNs against the change store at
/// `repo_root`. One violation per URN that does not resolve. With no repository
/// the check is skipped — the gate still enforces presence; resolution needs a
/// change store. (Ported from PR #102, adapted to the top-level `intent validate`.)
fn check_evidence_resolution(
    evidence: &[(String, String)],
    repo_root: Option<&Path>,
) -> Vec<Violation> {
    let mut violations = Vec::new();
    if evidence.is_empty() {
        return violations;
    }
    let Some(root) = repo_root else {
        eprintln!("note: evidence resolution skipped — not inside an Atomic repository");
        return violations;
    };
    let repo = match Repository::open_readonly(root) {
        Ok(repo) => repo,
        Err(e) => {
            eprintln!("note: evidence resolution skipped — cannot open repository: {e}");
            return violations;
        }
    };
    for (focus, urn) in evidence {
        let Some(hash_str) = urn.strip_prefix("urn:atomic:change:") else {
            continue; // non-change URNs are out of scope here
        };
        let resolved = Hash::from_base32(hash_str.as_bytes())
            .map(|h| repo.has_change(&h))
            .unwrap_or(false);
        if !resolved {
            violations.push(Violation {
                focus_node: focus.clone(),
                shape: "EvidenceResolution".to_string(),
                path: Some("evidence".to_string()),
                message: format!("evidence {urn} does not resolve to a change in this repository"),
            });
        }
    }
    violations
}

/// Serialize a [`ValidationReport`] to the documented JSON shape:
/// `{conforms, results: [{focus_node, shape, path, message}]}`.
fn report_json(report: &ValidationReport) -> serde_json::Value {
    serde_json::json!({
        "conforms": report.conforms,
        "results": report
            .results
            .iter()
            .map(|v| serde_json::json!({
                "focus_node": v.focus_node,
                "shape": v.shape,
                "path": v.path,
                "message": v.message,
            }))
            .collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod evidence_tests {
    use super::check_evidence_resolution;

    fn ev(urn: &str) -> Vec<(String, String)> {
        vec![("urn:atomic:ac:x-ac-1".to_string(), urn.to_string())]
    }

    #[test]
    fn no_repo_skips_without_violations() {
        // No repository → presence is still gated elsewhere; resolution is skipped.
        let v = check_evidence_resolution(&ev("urn:atomic:change:AAAA"), None);
        assert!(v.is_empty());
    }

    #[test]
    fn unresolvable_urn_in_repo_is_a_violation() {
        let dir = tempfile::tempdir().unwrap();
        atomic_repository::Repository::init(dir.path()).unwrap();
        let v = check_evidence_resolution(
            &ev("urn:atomic:change:XXX5YG4CV4XJL7JV3RRK6RHLJRTA4BW5ET4AACFDTKZQEMYJYGCQ"),
            Some(dir.path()),
        );
        assert_eq!(v.len(), 1);
        assert!(
            v[0].message.contains("does not resolve"),
            "{}",
            v[0].message
        );
    }

    #[test]
    fn non_change_urn_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        atomic_repository::Repository::init(dir.path()).unwrap();
        // A non-change evidence URN is out of scope for resolution.
        let v = check_evidence_resolution(&ev("urn:atomic:memory:whatever"), Some(dir.path()));
        assert!(v.is_empty());
    }
}
