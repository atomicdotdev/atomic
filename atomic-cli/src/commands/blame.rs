//! The `blame` command: line-level ownership for tracked text files.
//!
//! Reads the CRDT semantic layer (trunk → branches → bound vertices) and
//! attributes every alive line to the change that introduced its vertex,
//! rendering the line's bytes from that change's content blob. This is the
//! same attribution the CB-9C per-state oracle exercises after reload
//! (review R6/R7): the owning change is a real, loadable member of the
//! published history.
//!
//! ```text
//! $ atomic blame src/main.rs
//! a1b2c3d4 (Aaron       2026-09-17  1) fn main() {
//! e5f6a7b8 (Aaron       2026-09-17  2)     println!("hi");
//! a1b2c3d4 (Aaron       2026-09-17  3) }
//! ```

use std::collections::HashMap;

use atomic_core::change::Change;
use atomic_core::crdt::queries::iter_trunk_branches_in_file_order;
use atomic_core::crdt::tables::{decode_branch_id, decode_trunk_id};
use atomic_core::pristine::{CrdtTxnT, GraphTxnT, TreeTxnT, ViewTxnT};
use atomic_core::types::{Base32, Hash};
use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::print_warning;

#[derive(clap::Parser, Debug)]
pub struct Blame {
    /// File path to attribute (repository-relative).
    pub path: String,

    /// Show only the short (12-character) change hash per line.
    #[arg(long)]
    pub short: bool,
}

struct LineAttribution {
    line_number: usize,
    owner: Hash,
    content: Vec<u8>,
}

impl Command for Blame {
    fn run(&self) -> CliResult<()> {
        let repo_root = find_repository_root()?;
        let repo = Repository::open_readonly(&repo_root).map_err(|e| CliError::InvalidRepository {
            reason: e.to_string(),
        })?;
        let working_copy = repo
            .require_working_copy_id()
            .map_err(|e| CliError::Repository(e.into()))?;
        let view = repo
            .desired_view_name(working_copy)
            .map_err(|e| CliError::Repository(e.into()))?;

        let txn = repo.pristine().read_txn().map_err(|e| CliError::Repository(e.into()))?;
        let view_state = txn
            .get_view(&view)
            .map_err(|e| CliError::Repository(e.into()))?
            .ok_or_else(|| CliError::InvalidRepository {
                reason: format!("view '{view}' not found"),
            })?;
        let _ = view_state;

        // The trunk must exist and hold graph content. Binary/opaque trunks
        // are attributed at line granularity only when the CRDT layer bound
        // branches; otherwise the command fails explicitly rather than
        // printing fabricated lines.
        let trunk = txn
            .get_trunk_by_path(&self.path)
            .map_err(|e| CliError::Repository(e.into()))?
            .ok_or_else(|| CliError::InvalidArgument {
                message: format!("'{path}' is not a tracked text file (no semantic trunk)", path = self.path),
            })?;

        let branch_ids: Vec<atomic_core::crdt::BranchId> = iter_trunk_branches_in_file_order(
            &txn, trunk,
        )
        .map_err(|e| CliError::Repository(e.into()))?;
        let branch_keys: Vec<[u8; 12]> = branch_ids
            .iter()
            .map(|id| atomic_core::crdt::tables::encode_branch_id(id))
            .collect();

        let mut changes: HashMap<Hash, Change> = HashMap::new();
        let mut lines: Vec<LineAttribution> = Vec::new();
        let mut skipped_unbound = 0usize;
        for branch_key in branch_keys {
            let branch = txn
                .get_crdt_branch(&branch_key)
                .map_err(|e| CliError::Repository(e.into()))?;
            let Some(branch) = branch else { continue };
            if !branch.state.is_alive() {
                continue;
            }
            let Some(vertex) = txn
                .get_crdt_branch_vertex(&branch_key)
                .map_err(|e| CliError::Repository(e.into()))?
            else {
                skipped_unbound += 1;
                continue;
            };
            let Some(owner) = txn
                .get_external(vertex.change)
                .map_err(|e| CliError::Repository(e.into()))?
            else {
                skipped_unbound += 1;
                continue;
            };
            let change = match changes.get(&owner) {
                Some(change) => change.clone(),
                None => repo
                    .load_change(&owner)
                    .map_err(|e| CliError::Repository(e.into()))?,
            };
            let start = usize::try_from(vertex.start.get()).unwrap_or(0);
            let end = usize::try_from(vertex.end.get()).unwrap_or(0);
            let content: Vec<u8> = if start <= end && end <= change.contents.len() {
                change.contents[start..end].to_vec()
            } else {
                Vec::new()
            };
            changes.insert(owner.clone(), change);
            let decoded = decode_branch_id(&branch_key);
            lines.push(LineAttribution {
                line_number: decoded.branch_idx() as usize + 1,
                owner,
                content,
            });
        }

        if lines.is_empty() {
            print_warning(&format!(
                "'{}' has no attributed lines on view '{}'",
                self.path, view
            ));
            return Ok(());
        }
        if skipped_unbound > 0 {
            print_warning(&format!(
                "{skipped_unbound} alive line(s) have no bound vertex and are omitted"
            ));
        }

        let mut stdout = std::io::stdout().lock();
        for attribution in &lines {
            let hash = if self.short {
                attribution.owner.to_base32()[..12].to_string()
            } else {
                attribution.owner.to_base32()
            };
            let change = changes.get(&attribution.owner);
            let (author, date) = match change {
                Some(change) => {
                    let author = change
                        .hashed
                        .header
                        .authors
                        .first()
                        .map(|author| author.name.clone())
                        .unwrap_or_else(|| "unknown".to_string());
                    let date = change
                        .hashed
                        .header
                        .timestamp
                        .format("%Y-%m-%d")
                        .to_string();
                    (author, date)
                }
                None => ("unknown".to_string(), "unknown".to_string()),
            };
            let content = String::from_utf8_lossy(&attribution.content);
            let content = content.trim_end_matches('\n').trim_end_matches('\r');
            use std::io::Write as _;
            writeln!(
                stdout,
                "{hash} ({author:<16} {date} {:>4}) {content}",
                attribution.line_number
            )
            .map_err(|error| CliError::Internal(anyhow::anyhow!("stdout write failed: {error}")))?;
        }
        Ok(())
    }
}
