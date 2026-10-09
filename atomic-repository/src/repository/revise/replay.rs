//! Plan a rewritten stack in isolation. The source graph, views and working
//! bytes remain untouched until every replacement has been rendered and checked.
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use atomic_core::change::{Change, ChangeHeader};
use atomic_core::pristine::{Pristine, ViewTxnT};
use atomic_core::types::Hash;

use super::super::{graph_visibility_closure, working_copy};
use crate::content_filter::{ContentFilter, GitAttributesFilter};
use crate::{
    FileStatus, InsertOptions, RecordOptions, Repository, RepositoryError, StatusOptions,
    UnrecordOptions, ViewEntryKind,
};

#[derive(Clone, Debug, PartialEq, Eq)]
struct File {
    path: String,
    kind: ViewEntryKind,
    mode: u16,
    bytes: Vec<u8>,
}
// Keys are the original canonical inode identities, retained while moving the
// user's edit through historical path names. New records get their own canonical
// positions; comparisons across the rewritten graph use paths/kinds/bytes.
type Tree = BTreeMap<u64, File>;

pub(super) struct Replay {
    pub hashes: Vec<Hash>,
    pub repo: Repository,
    expected: Tree,
    _directory: tempfile::TempDir,
}

fn invalid(message: impl Into<String>) -> RepositoryError {
    RepositoryError::InvalidOperation {
        message: message.into(),
    }
}

fn file_path(root: &Path, path: &str) -> Result<PathBuf, RepositoryError> {
    let mut full = root.to_path_buf();
    let components: Vec<_> = Path::new(path).components().collect();
    if components.is_empty() {
        return Err(invalid("empty revision path"));
    }
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            return Err(invalid(format!("unsafe revision path '{path}'")));
        };
        if index == 0 && (*name == ".atomic" || *name == ".git") {
            return Err(invalid(format!("reserved revision path '{path}'")));
        }
        full.push(name);
        if index + 1 < components.len()
            && fs::symlink_metadata(&full).is_ok_and(|m| m.file_type().is_symlink())
        {
            return Err(invalid(format!(
                "revision path '{path}' traverses a symlink"
            )));
        }
    }
    Ok(full)
}

fn tree(repo: &Repository, view: &str) -> Result<Tree, RepositoryError> {
    let mut files = Tree::new();
    let filter = GitAttributesFilter::for_repository(repo.root());
    repo.materialize_view_entries::<RepositoryError>(view, |entry| {
        if entry.conflict_marker_line.is_some() {
            return Err(invalid(format!(
                "cannot revise unresolved conflict at '{}'",
                entry.path
            )));
        }
        let file = File {
            path: entry.path.clone(),
            kind: entry.kind,
            mode: entry.mode,
            bytes: if entry.kind == ViewEntryKind::File {
                filter
                    .clean(Path::new(&entry.path), &entry.content)
                    .map_err(|e| invalid(e.to_string()))?
                    .bytes
            } else {
                entry.content
            },
        };
        if files.insert(entry.inode, file).is_some() {
            return Err(invalid(
                "cannot revise unresolved concurrent names for one inode",
            ));
        }
        Ok(())
    })??;
    Ok(files)
}

fn paths(tree: &Tree) -> BTreeMap<&str, &File> {
    tree.values()
        .map(|file| (file.path.as_str(), file))
        .collect()
}

fn verify(repo: &Repository, view: &str, expected: &Tree) -> Result<(), RepositoryError> {
    let actual = tree(repo, view)?;
    if paths(&actual) != paths(expected) {
        return Err(invalid(
            "revised graph does not reproduce the planned file state",
        ));
    }
    Ok(())
}

impl Replay {
    pub(super) fn verify(&self, repo: &Repository, view: &str) -> Result<(), RepositoryError> {
        verify(repo, view, &self.expected)
    }
}

// Copies committed bytes through the existing locked snapshot API. No storage
// schema or migration path changes, and no second handle to the source redb.
fn isolated(
    source: &Repository,
    view: &str,
) -> Result<(Repository, tempfile::TempDir), RepositoryError> {
    let directory = tempfile::tempdir()?;
    let root = fs::canonicalize(directory.path())?;
    let dot_dir = root.join(".atomic");
    fs::create_dir(&dot_dir)?;
    source
        .pristine
        .copy_snapshot(&dot_dir.join(super::super::DATABASE_FILE))?;
    let pristine = Arc::new(Pristine::open_existing(
        dot_dir.join(super::super::DATABASE_FILE),
    )?);
    let layout =
        working_copy::layout_for_paths(root.clone(), dot_dir.clone(), dot_dir.clone(), false)?;
    working_copy::migrate_identity(&pristine, &layout, view)?;
    let change_store = crate::changestore::ChangeStore::new(
        dot_dir.join("changes"),
        crate::changestore::DEFAULT_CACHE_CAPACITY,
    )
    .map_err(|error| invalid(error.to_string()))?;
    let txn = source.pristine.read_txn()?;
    let state = txn
        .get_view(view)?
        .ok_or_else(|| invalid("revision view disappeared"))?;
    let closure = graph_visibility_closure(&txn, &state)?;
    use atomic_core::pristine::GraphTxnT;
    for id in closure.iter_dependency_first() {
        if id.is_root() {
            continue;
        }
        let hash = txn
            .get_external(*id)?
            .ok_or_else(|| invalid("visible change has no hash"))?;
        let destination = change_store.change_path(&hash);
        fs::create_dir_all(destination.parent().unwrap())?;
        fs::copy(source.change_store.change_path(&hash), destination)?;
    }
    drop(txn);
    // Retain repository recording policy; never publish from the planner.
    let mut config = atomic_config::RepoConfig::load(&source.dot_dir.join("config.toml"))
        .map_err(|e| invalid(e.to_string()))?;
    config.filters = GitAttributesFilter::for_repository(source.root()).replay_config();
    config
        .save(&dot_dir.join("config.toml"))
        .map_err(|e| invalid(e.to_string()))?;
    Ok((
        Repository {
            root,
            dot_dir,
            current_view: view.to_string(),
            pristine,
            change_store,
            is_sandbox: false,
        },
        directory,
    ))
}

fn read_file(root: &Path, path: &str) -> Result<Option<File>, RepositoryError> {
    let full = file_path(root, path)?;
    let metadata = match fs::symlink_metadata(&full) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let kind = if metadata.is_dir() {
        ViewEntryKind::Directory
    } else if metadata.file_type().is_symlink() {
        ViewEntryKind::Symlink
    } else {
        ViewEntryKind::File
    };
    let bytes = if kind == ViewEntryKind::Directory {
        Vec::new()
    } else if kind == ViewEntryKind::Symlink {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            fs::read_link(&full)?.as_os_str().as_bytes().to_vec()
        }
        #[cfg(not(unix))]
        {
            return Err(invalid("symlink revision is unsupported on this platform"));
        }
    } else {
        GitAttributesFilter::for_repository(root)
            .clean(Path::new(path), &fs::read(full)?)
            .map_err(|e| invalid(e.to_string()))?
            .bytes
    };
    // Directory modes are structural defaults in the canonical renderer.
    let mode = if kind == ViewEntryKind::Directory {
        0o755
    } else {
        super::super::attributes::working_inode_attrs(&root.join(path))?.mode
    };
    Ok(Some(File {
        path: path.to_string(),
        kind,
        mode,
        bytes,
    }))
}

fn working_tree(
    source: &Repository,
    tip: &Tree,
    selected: &[String],
) -> Result<Tree, RepositoryError> {
    let status = source.status(source.require_working_copy_id()?, StatusOptions::default())?;
    let mut desired = tip.clone();
    let by_path: BTreeMap<_, _> = tip
        .iter()
        .map(|(id, file)| (file.path.as_str(), *id))
        .collect();
    let mut next_id = u64::MAX;
    // Accept only a unique byte-identical raw rename. Ambiguous candidates
    // stay untracked, as ordinary recording requires explicit move evidence.
    let deleted: Vec<_> = status
        .entries()
        .iter()
        .filter(|e| e.status() == FileStatus::Deleted)
        .filter_map(|e| by_path.get(e.path().to_str()?).copied())
        .filter(|id| tip[id].kind != ViewEntryKind::Directory)
        .collect();
    let mut candidates = Vec::new();
    for entry in status
        .entries()
        .iter()
        .filter(|e| !deleted.is_empty() && e.status() == FileStatus::Untracked)
    {
        let Some(path) = entry.path().to_str() else {
            continue;
        };
        if let Some(file) = read_file(source.root(), path)? {
            for id in &deleted {
                if file.kind == tip[id].kind && file.bytes == tip[id].bytes {
                    candidates.push((path.to_string(), *id));
                }
            }
        }
    }
    let renames: BTreeMap<_, _> = candidates
        .iter()
        .filter(|(path, id)| {
            candidates
                .iter()
                .filter(|(p, i)| p == path || i == id)
                .count()
                == 1
        })
        .cloned()
        .collect();
    // Process deletions before destinations so explicit staged renames retain
    // the original identity even when source/destination sort in either order.
    let mut entries: Vec<_> = status.entries().iter().collect();
    entries.sort_by_key(|entry| entry.status() != FileStatus::Deleted);
    for entry in entries {
        let renamed_id = entry.path().to_str().and_then(|p| renames.get(p)).copied();
        if !selected.is_empty()
            && !selected.iter().any(|path| {
                entry.path().starts_with(path)
                    || renamed_id.is_some_and(|id| Path::new(&tip[&id].path).starts_with(path))
            })
        {
            continue;
        }
        if !(entry.status().is_dirty()
            || entry.status() == FileStatus::Conflicted
            || (!selected.is_empty() && entry.status() == FileStatus::Untracked))
            && renamed_id.is_none()
        {
            continue;
        }
        let path = entry
            .path()
            .to_str()
            .ok_or_else(|| invalid("revision path is not UTF-8"))?;
        let inode = by_path
            .get(path)
            .copied()
            .or(renamed_id)
            .or_else(|| {
                entry
                    .inode()
                    .map(|id| id.get())
                    .filter(|id| tip.contains_key(id))
            })
            .unwrap_or_else(|| {
                let id = next_id;
                next_id -= 1;
                id
            });
        if let Some(file) = read_file(source.root(), path)? {
            if super::super::materialize::first_conflict_marker_line(&file.bytes).is_some() {
                return Err(invalid(format!("unresolved conflict markers in '{path}'")));
            }
            desired.insert(inode, file);
        } else {
            desired.remove(&inode);
        }
    }
    let ancestors: BTreeSet<_> = desired
        .values()
        .flat_map(|file| {
            Path::new(&file.path)
                .ancestors()
                .skip(1)
                .filter(|p| !p.as_os_str().is_empty())
                .map(|p| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        })
        .collect();
    for path in ancestors {
        if !desired.values().any(|file| file.path == path) {
            desired.insert(
                next_id,
                File {
                    path,
                    kind: ViewEntryKind::Directory,
                    mode: 0o755,
                    bytes: Vec::new(),
                },
            );
            next_id -= 1;
        }
    }
    Ok(desired)
}

fn scalar<T: PartialEq + Clone>(
    base: &T,
    ours: &T,
    theirs: &T,
    path: &str,
    field: &str,
) -> Result<T, RepositoryError> {
    if ours == base {
        Ok(theirs.clone())
    } else if theirs == base || ours == theirs {
        Ok(ours.clone())
    } else {
        Err(invalid(format!("revision conflict in {field} at '{path}'; original history and working files are unchanged")))
    }
}

fn merge(base: &Tree, ours: &Tree, theirs: &Tree) -> Result<Tree, RepositoryError> {
    let keys: BTreeSet<_> = base
        .keys()
        .chain(ours.keys())
        .chain(theirs.keys())
        .copied()
        .collect();
    let mut result = Tree::new();
    for id in keys {
        let (b, o, t) = (base.get(&id), ours.get(&id), theirs.get(&id));
        let merged = if o == b {
            t.cloned()
        } else if t == b || o == t {
            o.cloned()
        } else if let (Some(b), Some(o), Some(t)) = (b, o, t) {
            let path = scalar(&b.path, &o.path, &t.path, &b.path, "path")?;
            let kind = scalar(&b.kind, &o.kind, &t.kind, &path, "file type")?;
            let mode = scalar(&b.mode, &o.mode, &t.mode, &path, "permissions")?;
            let bytes = if o.bytes == b.bytes {
                t.bytes.clone()
            } else if t.bytes == b.bytes || o.bytes == t.bytes {
                o.bytes.clone()
            } else if kind == ViewEntryKind::File
                && [b, o, t]
                    .iter()
                    .all(|f| !f.bytes.contains(&0) && std::str::from_utf8(&f.bytes).is_ok())
            {
                atomic_core::diff::merge_text(&b.bytes, &o.bytes, &t.bytes).map_err(|reason|
                        invalid(format!("revision content conflict at '{path}': {reason}; original history and working files are unchanged")))?
            } else {
                return Err(invalid(format!("revision content conflict at '{path}'")));
            };
            Some(File {
                path,
                kind,
                mode,
                bytes,
            })
        } else {
            let path = b.or(o).or(t).unwrap().path.as_str();
            return Err(invalid(format!("revision modify/delete conflict at '{path}'; original history and working files are unchanged")));
        };
        if let Some(file) = merged {
            result.insert(id, file);
        }
    }
    let mut occupied = HashSet::new();
    for file in result.values() {
        if !occupied.insert(&file.path) {
            return Err(invalid(format!(
                "revision name conflict at '{}'",
                file.path
            )));
        }
    }
    Ok(result)
}

fn write_tree(
    repo: &Repository,
    before: &Tree,
    after: &Tree,
    stage: bool,
) -> Result<(), RepositoryError> {
    let wc = repo.require_working_copy_id()?;
    let mut removed: Vec<_> = before.values().collect();
    removed.sort_by_key(|file| std::cmp::Reverse(file.path.len()));
    for file in removed {
        let path = file_path(repo.root(), &file.path)?;
        if fs::symlink_metadata(&path).is_ok() {
            if file.kind == ViewEntryKind::Directory {
                fs::remove_dir(path)?;
            } else {
                fs::remove_file(path)?;
            }
        }
    }
    for file in after.values() {
        let path = file_path(repo.root(), &file.path)?;
        fs::create_dir_all(path.parent().unwrap())?;
        if file.kind == ViewEntryKind::Directory {
            fs::create_dir_all(&path)?;
        } else if file.kind == ViewEntryKind::Symlink {
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStringExt;
                std::os::unix::fs::symlink(
                    std::ffi::OsString::from_vec(file.bytes.clone()),
                    &path,
                )?;
            }
            #[cfg(not(unix))]
            {
                return Err(invalid("symlink revision is unsupported on this platform"));
            }
        } else {
            fs::write(&path, &file.bytes)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(file.mode.into()))?;
            }
        }
    }
    // Attribute files must all exist before converting canonical bytes back
    // to recorder input, including nested .gitattributes and Git driver policy.
    let filter = GitAttributesFilter::for_repository(repo.root());
    for file in after
        .values()
        .filter(|file| file.kind == ViewEntryKind::File)
    {
        let bytes = filter
            .smudge(Path::new(&file.path), &file.bytes)
            .map_err(|e| invalid(e.to_string()))?
            .bytes;
        fs::write(file_path(repo.root(), &file.path)?, bytes)?;
    }
    if !stage {
        return Ok(());
    }
    for (id, file) in after {
        if let Some(old) = before.get(id) {
            if old.path != file.path {
                repo.move_file(wc, &old.path, &file.path)?;
            }
        } else {
            repo.add(wc, &file.path, Default::default())?;
        }
    }
    Ok(())
}

fn record_tree(
    repo: &Repository,
    before: &Tree,
    after: &Tree,
    header: ChangeHeader,
    original: &Change,
) -> Result<Hash, RepositoryError> {
    write_tree(repo, before, after, true)?;
    // Rewriting a change to an empty effect is valid history. Do not send it
    // through the ordinary recorder's "nothing to record" error path.
    if before == after {
        let mut change = Change::empty(header);
        change.hashed.metadata = original.hashed.metadata.clone();
        change.hashed.provenance = original.hashed.provenance.clone();
        let hash = save_signed(repo, change)?;
        repo.insert_change(&hash, InsertOptions::default())?;
        verify(repo, repo.current_view(), after)?;
        return Ok(hash);
    }
    let scope: BTreeSet<_> = before
        .values()
        .chain(after.values())
        .map(|file| file.path.clone())
        .collect();
    let outcome = repo
        .record(
            repo.require_working_copy_id()?,
            header,
            RecordOptions::default()
                .paths(scope.into_iter().collect::<Vec<_>>())
                .include_untracked(true)
                .detect_raw_renames(false)
                .metadata_bytes(original.hashed.metadata.clone())
                .provenance(original.hashed.provenance.clone())
                .sync_vault(false)
                .enrich_kg(false),
        )
        .map_err(|error| invalid(error.to_string()))?;
    if outcome.has_errors() || !outcome.skipped_files().is_empty() {
        return Err(invalid(format!(
            "incomplete revised change: {:?}, skipped {:?}",
            outcome.errors(),
            outcome.skipped_files()
        )));
    }
    verify(repo, repo.current_view(), after)?;
    Ok(*outcome.hash())
}

fn save_signed(repo: &Repository, mut change: Change) -> Result<Hash, RepositoryError> {
    // A signature authenticates one immutable hash, so never carry the old
    // signature onto rewritten content or a changed header.
    change.signature = None;
    if let Ok(store) = atomic_identity::IdentityStore::open_default() {
        if let Some(identity) = store.get_default().ok().flatten() {
            if let Ok(keypair) = store.load_keypair(&identity.id, None) {
                change
                    .sign_with(
                        &atomic_canonical::did::did_for_public_key(&identity.public_key),
                        keypair.secret.as_bytes(),
                        chrono::Utc::now().timestamp(),
                    )
                    .map_err(|error| invalid(error.to_string()))?;
            }
        }
    }
    repo.save_change(&change)
}

fn depends_on(
    repo: &Repository,
    root: Hash,
    changed: &HashSet<Hash>,
) -> Result<bool, RepositoryError> {
    let mut pending = vec![root];
    let mut seen = HashSet::new();
    while let Some(hash) = pending.pop() {
        if changed.contains(&hash) {
            return Ok(true);
        }
        if seen.insert(hash) {
            pending.extend(repo.load_change(&hash)?.dependencies());
        }
    }
    Ok(false)
}

pub(super) fn plan(
    source: &Repository,
    view: &str,
    suffix: &[(u64, Hash)],
    header: ChangeHeader,
    selected: Option<&[String]>,
) -> Result<Replay, RepositoryError> {
    let (planner, directory) = isolated(source, view)?;
    // Planning is private content computation, not publication. A shared view
    // is a root, so making only its scratch copy Draft preserves its closure.
    // Both the original and replacement closures are checked against the real
    // repository's publication policy and evidence before source mutation.
    // This also avoids copying session secrets into the scratch directory.
    if planner.get_view_info(view)?.scope.is_shared() {
        planner.set_view_scope(view, atomic_core::pristine::ViewScope::Draft)?;
    }
    let mut states = vec![tree(&planner, view)?];
    for (_, hash) in suffix.iter().rev() {
        planner.unrecord(hash, UnrecordOptions::default().view(view))?;
        states.push(tree(&planner, view)?);
    }
    states.reverse();
    let tip = states.last().unwrap();
    let desired_tip = match selected {
        Some(paths) => working_tree(source, tip, paths)?,
        None => tip.clone(),
    };
    let mut current = merge(tip, &states[1], &desired_tip)?;
    // The scratch filesystem starts empty. Render the baseline first, without
    // recording it, then use the ordinary recorder for the replacement.
    write_tree(&planner, &states[0], &states[0], false)?;
    let original = source.load_change(&suffix[0].1)?;
    let target = if selected.is_some() {
        record_tree(&planner, &states[0], &current, header, &original)?
    } else {
        let mut change = original.clone();
        change.hashed.header = header;
        let hash = save_signed(&planner, change)?;
        planner.insert_change(&hash, InsertOptions::default().view(view))?;
        write_tree(&planner, &states[0], &current, false)?;
        verify(&planner, view, &current)?;
        hash
    };
    let mut hashes = vec![target];
    let mut changed = HashSet::from([suffix[0].1]);
    for (index, (_, hash)) in suffix.iter().enumerate().skip(1) {
        let next = merge(&states[index], &current, &states[index + 1])?;
        if depends_on(source, *hash, &changed)? {
            let original = source.load_change(hash)?;
            let rewritten = record_tree(
                &planner,
                &current,
                &next,
                original.hashed.header.clone(),
                &original,
            )?;
            hashes.push(rewritten);
            changed.insert(*hash);
        } else {
            planner.insert_change(hash, InsertOptions::default().view(view))?;
            write_tree(&planner, &current, &next, false)?;
            verify(&planner, view, &next)?;
            hashes.push(*hash);
        }
        current = next;
    }
    if paths(&current) != paths(&desired_tip) {
        return Err(invalid(
            "revision replay changed the selected working-copy result",
        ));
    }
    verify(&planner, view, &desired_tip)?;
    Ok(Replay {
        hashes,
        repo: planner,
        expected: desired_tip,
        _directory: directory,
    })
}
