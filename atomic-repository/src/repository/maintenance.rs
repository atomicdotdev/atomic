//! Journal-aware disposal of filesystem recovery data, not source-history GC.
//!
//! The caller holds the common operation lock and an exclusive database handle
//! for the entire plan/apply interval. Unknown state is retained or refused.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, ensure, Context};
use atomic_core::operation::{EffectReceiptKind, Operation, OperationKind};
use atomic_core::pristine::{OperationTxnT, ReadTxn, ViewTxnT, WorkingCopyTxnT};
use atomic_core::{OperationId, WorkingCopyId};

use super::RepositoryMaintenanceLockGuard;

#[cfg(unix)]
mod secure_remove;

struct Candidate {
    index: usize,
    root: PathBuf,
    path: PathBuf,
    fingerprint: [u8; 32],
    #[cfg_attr(not(unix), allow(dead_code))]
    metadata: fs::Metadata,
}

/// One deterministic, administrator-visible maintenance decision.
#[derive(Debug, Clone)]
pub struct CleanupItem {
    pub path: PathBuf,
    pub kind: &'static str,
    /// Sum of file/link lengths, not unique allocated filesystem blocks.
    /// Zero for unsafe/unrecognized roots whose children were not inventoried.
    pub bytes: u64,
    pub eligible: bool,
    pub removed: bool,
    pub reason: String,
}

/// A plan valid only while the same maintenance exclusion remains held.
pub struct CleanupPlan<'guard> {
    items: Vec<CleanupItem>,
    dot_dir: PathBuf,
    candidates: Vec<Candidate>,
    guard: &'guard RepositoryMaintenanceLockGuard,
}

struct Journal {
    operations: BTreeMap<OperationId, Operation>,
    terminal: BTreeSet<OperationId>,
    protected: BTreeSet<OperationId>,
    working_copies: BTreeMap<WorkingCopyId, atomic_core::Hash>,
    live_views: BTreeSet<String>,
    historical_views: BTreeSet<String>,
}

impl Journal {
    fn read(txn: &ReadTxn) -> anyhow::Result<Self> {
        let operations: BTreeMap<_, _> = txn
            .list_operations()?
            .into_iter()
            .map(|op| (op.id(), op))
            .collect();
        let mut terminal = BTreeSet::new();
        let mut protected = BTreeSet::new();
        let mut historical_views = BTreeSet::new();
        for (id, op) in &operations {
            for parent in &op.payload().parents {
                ensure!(
                    operations.contains_key(parent),
                    "operation {id} has missing parent {parent}"
                );
            }
            if let Some(relation) = op.payload().relation {
                let target = match relation {
                    atomic_core::operation::OperationRelation::Undo { target }
                    | atomic_core::operation::OperationRelation::Restore { target } => target,
                };
                ensure!(
                    operations.contains_key(&target),
                    "operation {id} has missing relation target {target}"
                );
            }
            let receipts = txn.get_effect_receipts(*id)?;
            if receipts.iter().any(|receipt| {
                receipt.payload().kind == EffectReceiptKind::Verified
                    && receipt.payload().effect_ordinal.is_none()
            }) {
                terminal.insert(*id);
            }
            // Effect-bearing metadata undo replays the ORIGINAL retained bytes.
            // Keep all such operations, not merely the currently visible head.
            if !op.payload().delta.metadata.is_empty() && !op.payload().delta.effects.is_empty() {
                protected.insert(*id);
            }
            for state in [&op.payload().before, &op.payload().delta.after] {
                if let Some(view) = &state.view {
                    historical_views.insert(view.name.clone());
                }
            }
        }
        let verified = terminal.clone();
        for id in &verified {
            let op = &operations[id];
            if op.payload().kind != OperationKind::Recover {
                continue;
            }
            if let [parent] = op.payload().parents.as_slice() {
                let original = &operations[parent];
                if op.payload().working_copy == original.payload().working_copy
                    && op.payload().before == original.payload().delta.after
                    && op.payload().delta.after == original.payload().before
                {
                    terminal.insert(*parent);
                }
            }
        }
        for (scope, heads) in txn.list_operation_heads()? {
            ensure!(
                heads.as_slice().len() <= 1,
                "cleanup refuses divergent operation heads in {scope}"
            );
            for id in heads.as_slice() {
                ensure!(
                    verified.contains(id),
                    "cleanup requires recovery of incomplete operation head {id} in {scope}"
                );
                protected.insert(*id);
            }
        }
        // A pending recovery/undo can consume another operation's original bytes.
        for (id, op) in &operations {
            if !terminal.contains(id) {
                protected.insert(*id);
                protected.extend(op.payload().parents.iter().copied());
                if let Some(relation) = op.payload().relation {
                    let target = match relation {
                        atomic_core::operation::OperationRelation::Undo { target }
                        | atomic_core::operation::OperationRelation::Restore { target } => target,
                    };
                    ensure!(
                        operations.contains_key(&target),
                        "missing related operation {target}"
                    );
                    protected.insert(target);
                }
            }
        }
        let views = txn.snapshot_views()?;
        for (name, view) in &views {
            ensure!(name == &view.name, "VIEWS key/value mismatch for {name}");
        }
        let mut working_copies = BTreeMap::new();
        for copy in txn.list_working_copies()? {
            ensure!(
                views.iter().any(|(_, view)| view.id == copy.desired_view),
                "working copy {} references missing view {}",
                copy.id,
                copy.desired_view
            );
            working_copies.insert(copy.id, copy.location_fingerprint);
        }
        Ok(Self {
            operations,
            terminal,
            protected,
            working_copies,
            live_views: views.into_iter().map(|(name, _)| name).collect(),
            historical_views,
        })
    }

    fn recovery_reason(&self, id: OperationId, owner: WorkingCopyId) -> Option<&'static str> {
        let Some(operation) = self.operations.get(&id) else {
            return Some("unknown operation");
        };
        if operation.payload().working_copy != Some(owner) {
            return Some("operation/working-copy mismatch");
        }
        if self.protected.contains(&id) {
            return Some("current head or retained undo/recovery dependency");
        }
        if !self.terminal.contains(&id) {
            return Some("operation has no verified completion or verified inverse recovery");
        }
        None
    }

    fn shelf_reason(&self, owner: WorkingCopyId, name: &str, full: bool) -> Option<&'static str> {
        let prefix = format!("{name}/");
        let matches = |view: &String| view == name || view.starts_with(&prefix);
        if self.live_views.iter().any(matches) {
            return Some("shelf belongs to a live view or live nested view");
        }
        if !self.historical_views.iter().any(matches) {
            return Some("view identity is not established by the journal");
        }
        if !full {
            return Some("deleted-view ignored artifacts require --full");
        }
        for (id, operation) in &self.operations {
            if self.terminal.contains(id) && !self.protected.contains(id) {
                continue;
            }
            for effect in &operation.payload().delta.effects {
                if let atomic_core::operation::EffectTarget::ShelfPath {
                    working_copy, view, ..
                } = &effect.target
                {
                    if *working_copy == owner && (view == name || view.starts_with(&prefix)) {
                        return Some(
                            "shelf is referenced by a retained head, undo or incomplete operation",
                        );
                    }
                }
            }
        }
        None
    }
}

impl<'guard> CleanupPlan<'guard> {
    pub fn items(&self) -> &[CleanupItem] {
        &self.items
    }

    /// Consume a preview without applying any filesystem effects.
    pub fn into_items(self) -> Vec<CleanupItem> {
        self.items
    }

    /// Inventory known recovery roots and (with `full`) deleted-view shelves.
    /// Callers must retain an exclusive database handle as well as this guard.
    pub fn build(
        txn: &ReadTxn,
        guard: &'guard RepositoryMaintenanceLockGuard,
        local_dot_dir: &Path,
        full: bool,
    ) -> anyhow::Result<Self> {
        let dot_dir = guard.dot_dir().to_path_buf();
        let journal = Journal::read(txn)?;
        let mut plan = Self {
            items: Vec::new(),
            dot_dir: dot_dir.clone(),
            candidates: Vec::new(),
            guard,
        };
        let copies = dot_dir.join("working-copies");
        if directory_or_missing(&dot_dir, &copies)? {
            for copy in children(&copies)? {
                let Some(name) = copy.file_name().and_then(|name| name.to_str()) else {
                    plan.keep(copy, "unknown", "non-UTF8 working-copy directory");
                    continue;
                };
                let owner = name
                    .parse::<WorkingCopyId>()
                    .ok()
                    .filter(|id| id.to_string() == name);
                let Some(owner) = owner.filter(|id| journal.working_copies.contains_key(id)) else {
                    plan.keep(copy, "unknown", "unregistered working copy");
                    continue;
                };
                if !directory_or_missing(&dot_dir, &copy)? {
                    plan.keep(copy, "unknown", "not a physical directory");
                    continue;
                }
                plan.recovery(&journal, &dot_dir, &copy.join("operation-recovery"), owner)?;
                let shelves = copy.join("workspaces");
                if directory_or_missing(&dot_dir, &shelves)? {
                    for shelf in children(&shelves)? {
                        let name = shelf
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("");
                        let reason = journal.shelf_reason(owner, name, full);
                        plan.add(&dot_dir, shelf, "deleted-view-shelf", reason)?;
                    }
                }
            }
        }
        // Same-filesystem staging and index backups live next to each addressed
        // physical worktree. Do not guess paths for other registered worktrees.
        let mut roots = BTreeSet::from([dot_dir.clone()]);
        for local in [local_dot_dir] {
            if !local.is_dir() || fs::symlink_metadata(&local)?.file_type().is_symlink() {
                plan.keep(
                    local.to_path_buf(),
                    "unknown",
                    "worktree metadata is not a physical directory",
                );
                continue;
            }
            roots.insert(local.canonicalize()?);
        }
        for local in roots {
            let recovery = local.join("operation-recovery");
            if !directory_or_missing(&local, &recovery)? {
                continue;
            }
            let id_path = local.join("working_copy_id");
            let owner = if fs::symlink_metadata(&id_path)
                .map(|m| m.is_file() && !m.file_type().is_symlink())
                .unwrap_or(false)
            {
                fs::read_to_string(&id_path)?
                    .trim()
                    .parse::<WorkingCopyId>()
                    .ok()
            } else {
                None
            };
            let layout = super::working_copy::layout_for_paths(
                local
                    .parent()
                    .context("local metadata has no worktree root")?
                    .to_path_buf(),
                dot_dir.clone(),
                local.clone(),
                false,
            )?;
            if let Some(owner) = owner
                .filter(|id| journal.working_copies.get(id) == Some(&layout.location_fingerprint))
            {
                plan.recovery(&journal, &local, &recovery, owner)?;
            } else {
                plan.keep(
                    recovery,
                    "operation-recovery",
                    "unknown physical working-copy identity",
                );
            }
        }
        let changes = dot_dir.join("changes");
        if directory_or_missing(&dot_dir, &changes)? {
            plan.add(&dot_dir, changes, "change-objects",
                Some("content-addressed source history is retained; compact does not expire graph/journal references"))?;
        }
        let legacy = dot_dir.join("workspaces");
        if directory_or_missing(&dot_dir, &legacy)? && !children(&legacy)?.is_empty() {
            plan.add(
                &dot_dir,
                legacy,
                "legacy-shelves",
                Some("legacy shelf layout requires explicit migration"),
            )?;
        }
        plan.items.sort_by(|a, b| a.path.cmp(&b.path));
        // Candidates carry paths rather than indexes until sorting is complete.
        for candidate in &mut plan.candidates {
            candidate.index = plan
                .items
                .iter()
                .position(|item| item.path == candidate.path)
                .expect("planned item");
        }
        Ok(plan)
    }

    fn recovery(
        &mut self,
        journal: &Journal,
        root: &Path,
        recovery: &Path,
        owner: WorkingCopyId,
    ) -> anyhow::Result<()> {
        if !directory_or_missing(root, recovery)? {
            return Ok(());
        }
        for path in children(recovery)? {
            let id = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| {
                    name.parse::<OperationId>()
                        .ok()
                        .filter(|id| id.to_string() == name)
                });
            let reason = id
                .map(|id| journal.recovery_reason(id, owner))
                .unwrap_or(Some("not a canonical operation directory"));
            self.add(root, path, "operation-recovery", reason)?;
        }
        Ok(())
    }

    fn keep(&mut self, path: PathBuf, kind: &'static str, reason: &str) {
        self.items.push(CleanupItem {
            path,
            kind,
            bytes: 0,
            eligible: false,
            removed: false,
            reason: reason.to_string(),
        });
    }

    fn add(
        &mut self,
        root: &Path,
        path: PathBuf,
        kind: &'static str,
        reason: Option<&str>,
    ) -> anyhow::Result<()> {
        if !directory_or_missing(root, &path)? {
            self.keep(
                path,
                kind,
                "not a physical directory (symlinks are never followed)",
            );
            return Ok(());
        }
        let (bytes, fingerprint) = inventory(&path)?;
        let reason = reason.or_else(|| {
            (!cfg!(unix)).then_some("descriptor-relative cleanup is supported on Unix; this platform retains filesystem data")
        });
        let eligible = reason.is_none();
        self.items.push(CleanupItem { path: path.clone(), kind, bytes, eligible, removed: false,
            reason: reason.unwrap_or("journal proves recovery data is terminal, or --full expires a deleted-view shelf").to_string() });
        if eligible {
            self.candidates.push(Candidate {
                index: 0,
                root: root.to_path_buf(),
                metadata: fs::symlink_metadata(&path)?,
                path,
                fingerprint,
            });
        }
        Ok(())
    }

    /// Recheck the full plan before deleting any candidate. Repeat attempts are
    /// safe because no logical journal record is edited and each root is disposable.
    pub fn execute(
        mut self,
        guard: &RepositoryMaintenanceLockGuard,
    ) -> anyhow::Result<Vec<CleanupItem>> {
        ensure!(
            std::ptr::eq(guard, self.guard),
            "cleanup must retain its original maintenance guard"
        );
        ensure!(
            guard.dot_dir() == self.dot_dir,
            "cleanup guard belongs to another repository"
        );
        for candidate in &self.candidates {
            let Candidate {
                root,
                path,
                fingerprint,
                ..
            } = candidate;
            ensure!(
                directory_or_missing(root, path)?,
                "cleanup candidate disappeared or changed type: {}",
                path.display()
            );
            ensure!(
                &inventory(path)?.1 == fingerprint,
                "cleanup candidate changed: {}",
                path.display()
            );
        }
        for candidate in &self.candidates {
            remove_candidate(candidate)
                .with_context(|| format!("removing disposable {}", candidate.path.display()))?;
            self.items[candidate.index].removed = true;
        }
        Ok(self.items)
    }
}

#[cfg(unix)]
fn remove_candidate(candidate: &Candidate) -> anyhow::Result<()> {
    secure_remove::remove(&candidate.root, &candidate.path, &candidate.metadata)
}

#[cfg(not(unix))]
fn remove_candidate(_candidate: &Candidate) -> anyhow::Result<()> {
    bail!("descriptor-relative cleanup is unsupported on this platform")
}

fn children(path: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut paths = fs::read_dir(path)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?;
    paths.sort();
    Ok(paths)
}

/// Check every parent component without canonicalizing through symlinks.
fn directory_or_missing(root: &Path, path: &Path) -> anyhow::Result<bool> {
    let relative = path
        .strip_prefix(root)
        .context("cleanup path outside its metadata root")?;
    let mut cursor = root.to_path_buf();
    let root_meta = fs::symlink_metadata(root)?;
    ensure!(
        root_meta.is_dir() && !root_meta.file_type().is_symlink(),
        "unsafe metadata root {}",
        root.display()
    );
    for component in relative.components() {
        let Component::Normal(component) = component else {
            bail!("invalid cleanup path component");
        };
        cursor.push(component);
        match fs::symlink_metadata(&cursor) {
            Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
            Ok(_) => return Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(true)
}

fn inventory(path: &Path) -> anyhow::Result<(u64, [u8; 32])> {
    fn visit(
        path: &Path,
        hash: &mut blake3::Hasher,
        bytes: &mut u64,
        depth: usize,
    ) -> anyhow::Result<()> {
        ensure!(depth < 512, "cleanup tree is too deep");
        let meta = fs::symlink_metadata(path)?;
        hash.update(path.as_os_str().as_encoded_bytes());
        hash.update(&meta.len().to_le_bytes());
        hash.update(format!("{:?}:{:?}", meta.file_type(), meta.modified()?).as_bytes());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            hash.update(&meta.dev().to_le_bytes());
            hash.update(&meta.ino().to_le_bytes());
        }
        if meta.file_type().is_symlink() {
            hash.update(fs::read_link(path)?.as_os_str().as_encoded_bytes());
            *bytes = bytes
                .checked_add(meta.len())
                .context("cleanup byte count overflow")?;
        } else if meta.is_dir() {
            for child in children(path)? {
                visit(&child, hash, bytes, depth + 1)?;
            }
        } else if meta.is_file() {
            *bytes = bytes
                .checked_add(meta.len())
                .context("cleanup byte count overflow")?;
        } else {
            bail!("cleanup refuses special file {}", path.display());
        }
        Ok(())
    }
    let mut bytes = 0;
    let mut hash = blake3::Hasher::new();
    visit(path, &mut hash, &mut bytes, 0)?;
    Ok((bytes, *hash.finalize().as_bytes()))
}

#[cfg(all(test, unix))]
mod tests;
