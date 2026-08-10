//! Working-copy consistency verifier (the `atomic doctor check` engine).
//!
//! A read-only field check that recomputes each file's content from the graph
//! and reconciles three views of reality:
//!
//!   1. **Materialization drift** — for a file the working copy considers
//!      *clean* (no uncommitted edit), the on-disk bytes must equal the bytes
//!      the graph would materialize. A mismatch is silent corruption: the disk
//!      drifted from history without the user editing it.
//!
//!   2. **Conflict honesty** — the rubric's invariant (docs/MERGE-CONFLICT-RUBRIC.md):
//!      on-disk conflict markers ⇔ `status` reports `Conflicted` ⇔
//!      `list_conflicts` includes the file. Any disagreement is a caught bug.
//!
//! This is the "make future silent corruption a loud failure" safety net from
//! §6.3 of the rubric. It mutates nothing.

use std::collections::HashMap;

use super::*;
use crate::status::{FileStatus, StatusOptions};

/// A single consistency problem found by [`Repository::verify_working_copy`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyProblem {
    /// A clean file's on-disk bytes differ from the graph's content.
    MaterializationDrift {
        /// The file path.
        path: String,
        /// Bytes on disk.
        disk_len: usize,
        /// Bytes the graph would materialize.
        graph_len: usize,
    },
    /// The three conflict signals disagree for a file.
    ConflictHonesty {
        /// The file path.
        path: String,
        /// On-disk content contains conflict markers.
        markers: bool,
        /// `status` reports the file as Conflicted.
        status_conflicted: bool,
        /// `list_conflicts` includes the file.
        listed: bool,
    },
}

impl std::fmt::Display for VerifyProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerifyProblem::MaterializationDrift {
                path,
                disk_len,
                graph_len,
            } => write!(
                f,
                "materialization drift: {} (disk {} bytes, graph {} bytes) — a clean file's \
                 on-disk content does not match the graph",
                path, disk_len, graph_len
            ),
            VerifyProblem::ConflictHonesty {
                path,
                markers,
                status_conflicted,
                listed,
            } => write!(
                f,
                "conflict-state disagreement: {} (markers={}, status_conflicted={}, listed={})",
                path, markers, status_conflicted, listed
            ),
        }
    }
}

/// Summary of a working-copy verification.
#[derive(Debug, Clone, Default)]
pub struct VerifyReport {
    /// Number of tracked, clean files whose disk content was checked.
    pub clean_files_checked: usize,
    /// Number of files skipped because they have uncommitted edits.
    pub uncommitted_skipped: usize,
    /// Number of files reported as conflicted (informational).
    pub conflicted_files: usize,
    /// Problems found. Empty means the working copy is consistent.
    pub problems: Vec<VerifyProblem>,
}

impl VerifyReport {
    /// Whether the working copy is consistent (no problems found).
    pub fn is_healthy(&self) -> bool {
        self.problems.is_empty()
    }
}

fn has_markers(bytes: &[u8]) -> bool {
    super::materialize::first_conflict_marker_line(bytes).is_some()
}

impl Repository {
    /// Verify working-copy consistency against the graph. Read-only.
    ///
    /// See the module docs for the two checks performed. Returns a
    /// [`VerifyReport`]; `report.is_healthy()` is `true` when no problems were
    /// found.
    pub fn verify_working_copy(&self) -> Result<VerifyReport, RepositoryError> {
        let mut report = VerifyReport::default();

        // One status pass classifies every path (Modified/Added/Deleted/
        // Conflicted/Untracked). Clean files are not emitted, so we treat
        // "absent from this map" as clean.
        let status = self.status(StatusOptions::default())?;
        let mut status_by_path: HashMap<String, FileStatus> = HashMap::new();
        for e in status.entries() {
            status_by_path.insert(e.path().to_string_lossy().to_string(), e.status());
        }

        let conflicted: std::collections::HashSet<String> =
            self.list_conflicts()?.into_iter().map(|(p, _)| p).collect();
        report.conflicted_files = conflicted.len();

        // Files visible (tracked + recorded) on the current view.
        let visible = self.visible_file_paths(&self.current_view)?;

        for path in &visible {
            let st = status_by_path.get(path).copied();

            // ── Check 1: materialization drift on clean files ──────────────
            match st {
                // Uncommitted edits are expected divergence — skip, but count.
                Some(FileStatus::Modified)
                | Some(FileStatus::Added)
                | Some(FileStatus::Deleted) => {
                    report.uncommitted_skipped += 1;
                }
                // Conflicted files legitimately differ (markers on disk).
                Some(FileStatus::Conflicted) => {}
                // Clean (absent from status) or other benign states: the disk
                // must match the graph exactly.
                _ => {
                    let abs = self.root.join(path);
                    let disk = std::fs::read(&abs).ok();
                    let graph = self.get_file_content_on_view(path, &self.current_view)?;
                    if let (Some(disk), Some(graph)) = (disk.as_ref(), graph.as_ref()) {
                        report.clean_files_checked += 1;
                        if disk != graph {
                            report.problems.push(VerifyProblem::MaterializationDrift {
                                path: path.clone(),
                                disk_len: disk.len(),
                                graph_len: graph.len(),
                            });
                        }
                    }
                }
            }

            // ── Check 2: conflict honesty (the three signals must agree) ───
            let abs = self.root.join(path);
            let markers = std::fs::read(&abs)
                .map(|b| has_markers(&b))
                .unwrap_or(false);
            let status_conflicted = st == Some(FileStatus::Conflicted);
            let listed = conflicted.contains(path);
            if !(markers == status_conflicted && status_conflicted == listed) {
                report.problems.push(VerifyProblem::ConflictHonesty {
                    path: path.clone(),
                    markers,
                    status_conflicted,
                    listed,
                });
            }
        }

        Ok(report)
    }
}
