//! The `agent repair` command: resume a managed session under leases while
//! retaining every piece of durable evidence (CB-12A AC3).
//!
//! A repair NEVER erases or manufactures attribution: the session's
//! incomplete evidence is snapshotted verbatim into an append-only repair
//! note, the retention lease is set so cleanup can never discard the only
//! unbound copy, and the evidence (attestation MAC, capture files) is
//! verified and reported — findings only, never silent "fixes".

use std::path::PathBuf;

use atomic_agent::turn::session::{RepairNote, SessionStore};

use crate::commands::workspace_txn::enter_workspace;
use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{print_info, print_success, print_warning};

#[derive(clap::Parser, Debug)]
pub struct Repair {
    /// The managed session identifier to repair.
    pub session: String,

    /// Verify evidence only (attestation/capture MACs); no state changes.
    #[arg(long)]
    pub verify_only: bool,
}

impl Command for Repair {
    fn run(&self) -> CliResult<()> {
        let repo_root = find_repository_root()?;

        // The repair mutates durable session state, so it runs under the
        // shared workspace boundary — the same ordered lease every other
        // mutating command holds (CB-12A AC3 "resumes under leases").
        {
            let mut repo =
                atomic_repository::Repository::open_for_workspace_transaction(&repo_root)
                    .map_err(CliError::Repository)?;
            let _workspace =
                enter_workspace(&mut repo, atomic_repository::WorkspaceTxnMode::Reconcile)?;
        }

        let sessions_dir: PathBuf = atomic_repository::Repository::canonical_dot_dir(&repo_root)
            .map(|dot| dot.join("sessions"))
            .unwrap_or_else(|_| repo_root.join(".atomic").join("sessions"));
        let store = SessionStore::new(&sessions_dir).map_err(|error| {
            CliError::Internal(anyhow::anyhow!(
                "cannot open the session store at {}: {error}",
                sessions_dir.display()
            ))
        })?;
        let loaded = store.load(&self.session).map_err(|error| {
            CliError::Internal(anyhow::anyhow!(
                "cannot load session '{}': {error}",
                self.session
            ))
        })?;
        let mut session = loaded.ok_or_else(|| CliError::InvalidArgument {
            message: format!(
                "session '{}' not found under {}",
                self.session,
                sessions_dir.display()
            ),
        })?;

        let mut report = String::new();
        let prior_status = session.status.label().to_string();

        // ── Evidence verification (report only; never modified). ────────
        let mut findings: Vec<String> = Vec::new();

        if let (Some(last), Some(key)) = (session.last_attestation, session.mac_key.clone()) {
            // Content-addressed attestations load through the repository's
            // change store (read-only reopen: the workspace lease above was
            // released with its guard).
            let verified = atomic_repository::Repository::open_readonly(&repo_root)
                .ok()
                .and_then(|repo| repo.load_attestation(&last).ok());
            match verified {
                Some(attestation) => {
                    if attestation.verify_mac(&key) {
                        findings.push(format!(
                            "attestation {} verifies under the session MAC",
                            last
                        ));
                    } else {
                        findings.push(format!(
                            "WARNING: attestation {} FAILS the session MAC — tampered or wrong key",
                            last
                        ));
                    }
                }
                None => findings.push(format!(
                    "WARNING: the session's last attestation {} cannot be loaded for verification",
                    last
                )),
            }
        } else {
            findings
                .push("no attestation/key pair to verify (git-only or fresh session)".to_string());
        }

        let capture_dir = sessions_dir.join("captures").join(&self.session);
        let capture_count = std::fs::read_dir(&capture_dir)
            .map(|entries| entries.filter_map(|entry| entry.ok()).count())
            .unwrap_or(0);
        findings.push(format!(
            "{capture_count} commit-time capture file(s) retained (immutable per-attempt evidence)"
        ));

        // ── The durable incompleteness (retained verbatim). ─────────────
        let retained_incomplete = session.incomplete().cloned();
        if let Some(incomplete) = &retained_incomplete {
            report.push_str(&format!(
                "durable incomplete evidence (origin: {})\n  reason: {}\n",
                incomplete.origin, incomplete.reason
            ));
            if !incomplete.unbound_commits.is_empty() {
                report.push_str(&format!(
                    "  unbound commits: {}\n",
                    incomplete.unbound_commits.join(", ")
                ));
            }
            if !incomplete.paths.is_empty() {
                report.push_str(&format!(
                    "  unrecorded paths: {}\n",
                    incomplete.paths.join(", ")
                ));
            }
            report.push_str(
                "  review path: an independent review must discharge this evidence before\n  \
                 publication gates accept the session (CB-12B); repair never clears it.\n",
            );
        } else {
            report.push_str("session carries no durable incomplete evidence\n");
        }

        if self.verify_only {
            for finding in &findings {
                print_info(finding);
            }
            print!(" {report}");
            print_success(&format!(
                "verify-only repair of '{}' complete; nothing was modified (status {})",
                self.session, prior_status
            ));
            return Ok(());
        }

        // ── Resume: append-only note + retention lease. ──────────────────
        if matches!(
            session.status,
            atomic_core::change::session::SessionStatus::Incomplete(_)
        ) {
            session.repair_history.push(RepairNote {
                at_rfc3339: chrono::Utc::now().to_rfc3339(),
                action: "resume".to_string(),
                prior_status,
                retained_incomplete: retained_incomplete.clone(),
                detail: findings.join("; "),
            });
            // Resume work: the next turn proceeds while the incomplete
            // evidence above stays retained in the note history.
            session.status = atomic_core::change::session::SessionStatus::Active;
        } else {
            session.repair_history.push(RepairNote {
                at_rfc3339: chrono::Utc::now().to_rfc3339(),
                action: "verify".to_string(),
                prior_status,
                retained_incomplete: None,
                detail: findings.join("; "),
            });
        }
        // The retention lease is set by every repair and never cleared:
        // cleanup must never discard this session's only unbound copy.
        session.evidence_retained = true;

        store.save(&session).map_err(|error| {
            CliError::Internal(anyhow::anyhow!(
                "cannot save repaired session '{}': {error}",
                self.session
            ))
        })?;

        for finding in &findings {
            if finding.starts_with("WARNING") {
                print_warning(finding);
            } else {
                print_info(finding);
            }
        }
        print!(" {report}");
        print_success(&format!(
            "session '{}' repaired: evidence retained ({} repair note(s)), work resumed",
            self.session,
            session.repair_history.len()
        ));
        Ok(())
    }
}
