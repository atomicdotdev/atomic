//! User-defined vault kinds: where kind files come from, the registry built
//! from them, and the validation report.
//!
//! A kind is declared by a definition at `.vault/kinds/<k>/_kind.md` and has
//! instances at `.vault/kinds/<k>/<ULID>.md` (the spec language itself lives
//! in [`atomic_canonical::kind_spec`]). Paths in this module are
//! vault-relative, as redb stores them: `kinds/<k>/_kind.md`.
//!
//! The per-view gate (`kind validate`, `kind new`, the registry) resolves
//! kind definitions from **files**, never from redb's `VAULT_ENTRIES`: those
//! rows are repository-global and are not refreshed by switch, insert or
//! unrecord, so they cannot say which kinds a given view declares. A
//! [`KindSource`] names the files to read — the working copy on disk, or the
//! tracked content of a view — and [`KindFiles`] reads them.
//!
//! KG projection and the embedding policy are the exception: they are
//! derived from redb, so they read the stored definition instead (see
//! [`Repository::stored_kind_definition`]).

mod registry;
mod validate;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use atomic_canonical::kind_spec::{is_valid_kind_name, is_valid_ulid};
use atomic_core::pristine::ViewTxnT;
use atomic_core::types::Merkle;

use super::Repository;
use crate::RepositoryError;

pub use registry::{KindDefinition, KindEntry, KindRegistry, KindStatus};
pub use validate::{
    check_instance, InstanceCheck, InstanceResult, KindProblem, KindProblemCode, KindRefResolver,
    KindReport, KindReportStatus, KindValidationReport,
};

/// The vault directory that holds every kind.
pub const KINDS_DIR: &str = "kinds";

/// The file name of a kind definition inside `kinds/<k>/`.
pub const KIND_DEFINITION_FILE: &str = "_kind.md";

/// The vault directory, as it appears in repository-relative tracked paths.
const VAULT_PREFIX: &str = ".vault/";

/// How many view registries [`Repository::kind_registry`] keeps before it
/// starts over. Each key is a distinct view state, so a long session that
/// records repeatedly would otherwise grow the cache without bound.
const REGISTRY_CACHE_CAPACITY: usize = 16;

/// The vault-relative path of kind `kind`'s definition:
/// `kinds/<kind>/_kind.md`.
pub fn kind_definition_path(kind: &str) -> String {
    format!("{KINDS_DIR}/{kind}/{KIND_DEFINITION_FILE}")
}

/// The vault-relative path of an instance: `kinds/<kind>/<uid>.md`.
pub fn kind_instance_path(kind: &str, uid: &str) -> String {
    format!("{KINDS_DIR}/{kind}/{uid}.md")
}

/// A fresh instance uid: a ULID in uppercase Crockford base32, as instance
/// filenames require.
pub fn mint_kind_uid() -> String {
    ulid::Ulid::new().to_string()
}

/// What a vault-relative path under `kinds/` is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KindPath {
    /// `kinds/<kind>/_kind.md`.
    Definition { kind: String },
    /// `kinds/<kind>/<uid>.md` with an uppercase ULID filename.
    Instance { kind: String, uid: String },
    /// Anything else under `kinds/`: a file directly in `kinds/`, a nested
    /// directory, a non-`.md` file, a filename that is not an uppercase ULID,
    /// or a directory name that is not a valid kind name.
    Stray,
}

impl KindPath {
    /// Classify a vault-relative path, or `None` when it is not under
    /// `kinds/`.
    ///
    /// The kind directory only needs to be a syntactically valid kind name.
    /// A reserved name (`kinds/intent/_kind.md`) still classifies as a
    /// definition, so its spec is reported as invalid rather than as a
    /// stray file.
    ///
    /// # Examples
    ///
    /// ```
    /// use atomic_repository::KindPath;
    ///
    /// assert_eq!(
    ///     KindPath::classify("kinds/adr/_kind.md"),
    ///     Some(KindPath::Definition { kind: "adr".into() }),
    /// );
    /// assert_eq!(
    ///     KindPath::classify("kinds/adr/01JBZ0E7Q8K3M5N6P7R8S9T0VW.md"),
    ///     Some(KindPath::Instance {
    ///         kind: "adr".into(),
    ///         uid: "01JBZ0E7Q8K3M5N6P7R8S9T0VW".into(),
    ///     }),
    /// );
    /// assert_eq!(KindPath::classify("kinds/adr/notes.md"), Some(KindPath::Stray));
    /// assert_eq!(KindPath::classify("memory/notes.md"), None);
    /// ```
    pub fn classify(path: &str) -> Option<KindPath> {
        let rest = path.strip_prefix(KINDS_DIR)?.strip_prefix('/')?;
        let Some((kind, file)) = rest.split_once('/') else {
            return Some(KindPath::Stray);
        };
        if file.contains('/') || !is_valid_kind_name(kind) {
            return Some(KindPath::Stray);
        }
        if file == KIND_DEFINITION_FILE {
            return Some(KindPath::Definition {
                kind: kind.to_string(),
            });
        }
        match file.strip_suffix(".md") {
            Some(uid) if is_valid_ulid(uid) => Some(KindPath::Instance {
                kind: kind.to_string(),
                uid: uid.to_string(),
            }),
            _ => Some(KindPath::Stray),
        }
    }
}

/// Where kind files are read from. Serializes as `"working_copy"` or
/// `{"view": "<name>"}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KindSource {
    /// The files on disk under [`Repository::vault_dir`]. Deflate reads
    /// from here, and it is the CLI's default.
    WorkingCopy,
    /// The tracked content of the named view, as recorded in the graph
    /// (including everything inherited through its parent chain).
    /// Unrecorded edits in the working copy are not seen.
    View(String),
}

impl std::fmt::Display for KindSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KindSource::WorkingCopy => f.write_str("working copy"),
            KindSource::View(name) => write!(f, "view {name}"),
        }
    }
}

/// Reads the files of one [`KindSource`]. Obtain one with
/// [`Repository::kind_files`]; opening a working-copy reader does no I/O,
/// while a view reader loads the view's visible paths once up front.
pub struct KindFiles<'r> {
    repo: &'r Repository,
    inner: FilesInner,
}

/// A `kinds/**` path that two or more files or directories claim in a view
/// (an unresolved name conflict, as `atomic status` reports it). The view
/// renders none of the claimants until the conflict is resolved and
/// recorded. In the working copy the same conflict shows up as a file
/// holding both sides between conflict markers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KindNameConflict {
    /// How many inodes claim the path.
    pub claimants: usize,
    /// Every claimant is a directory.
    pub directory: bool,
}

impl KindNameConflict {
    /// One readable problem, for a report.
    pub fn describe(&self) -> String {
        let what = if self.directory { "directory" } else { "path" };
        format!(
            "the {what} has an unresolved name conflict ({} claimants); the view renders none \
             of them until it is resolved and recorded",
            self.claimants
        )
    }
}

enum FilesInner {
    WorkingCopy {
        vault_dir: PathBuf,
    },
    View {
        name: String,
        /// Vault-relative paths of every tracked `.vault/` file.
        paths: HashSet<String>,
        /// Vault-relative `kinds/**` paths with an unresolved name conflict.
        /// They are absent from `paths`: the view renders neither side, so
        /// their content cannot be read.
        name_conflicts: BTreeMap<String, KindNameConflict>,
        /// The registry cache key, when the view did not change while the
        /// paths were being read.
        cache_key: Option<Merkle>,
    },
}

impl<'r> KindFiles<'r> {
    pub fn source(&self) -> KindSource {
        match &self.inner {
            FilesInner::WorkingCopy { .. } => KindSource::WorkingCopy,
            FilesInner::View { name, .. } => KindSource::View(name.clone()),
        }
    }

    pub(crate) fn repository(&self) -> &'r Repository {
        self.repo
    }

    /// Does the vault-relative `path` name a regular file in this source?
    /// On disk, a symlink is not followed and does not count.
    pub fn exists(&self, path: &str) -> bool {
        match &self.inner {
            FilesInner::WorkingCopy { vault_dir } => {
                safe_relative(path)
                    && std::fs::symlink_metadata(vault_dir.join(path)).is_ok_and(|m| m.is_file())
            }
            FilesInner::View { paths, .. } => paths.contains(path),
        }
    }

    /// The bytes of the vault-relative `path`, or `None` when it is not a
    /// regular file in this source.
    pub fn read(&self, path: &str) -> Result<Option<Vec<u8>>, RepositoryError> {
        match &self.inner {
            FilesInner::WorkingCopy { vault_dir } => {
                if !self.exists(path) {
                    return Ok(None);
                }
                match std::fs::read(vault_dir.join(path)) {
                    Ok(bytes) => Ok(Some(bytes)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(e) => Err(e.into()),
                }
            }
            FilesInner::View { name, paths, .. } => {
                if !paths.contains(path) {
                    return Ok(None);
                }
                self.repo
                    .get_file_content_on_view(format!("{VAULT_PREFIX}{path}"), name)
            }
        }
    }

    /// The name conflict at the vault-relative `path`, when this is a view
    /// source and the path has one.
    pub fn name_conflict(&self, path: &str) -> Option<KindNameConflict> {
        match &self.inner {
            FilesInner::WorkingCopy { .. } => None,
            FilesInner::View { name_conflicts, .. } => name_conflicts.get(path).copied(),
        }
    }

    /// Every name-conflicted `kinds/**` path (files and directories), sorted;
    /// always empty for the working copy.
    pub fn name_conflicts(&self) -> impl Iterator<Item = (&str, KindNameConflict)> {
        let conflicts = match &self.inner {
            FilesInner::WorkingCopy { .. } => None,
            FilesInner::View { name_conflicts, .. } => Some(name_conflicts),
        };
        conflicts
            .into_iter()
            .flatten()
            .map(|(path, conflict)| (path.as_str(), *conflict))
    }

    /// Every file under `kinds/`, vault-relative and sorted. Symlinks on
    /// disk are listed (they are stray) but never followed. In a view, a
    /// name-conflicted file path is listed although it cannot be read (see
    /// [`KindFiles::name_conflict`]), so the kind it belongs to is not lost.
    pub fn list_kind_files(&self) -> Result<Vec<String>, RepositoryError> {
        let mut out = match &self.inner {
            FilesInner::WorkingCopy { vault_dir } => {
                let mut out = Vec::new();
                walk_files(&vault_dir.join(KINDS_DIR), KINDS_DIR, &mut out)?;
                out
            }
            FilesInner::View {
                paths,
                name_conflicts,
                ..
            } => paths
                .iter()
                .filter(|p| p.starts_with(KINDS_DIR) && p[KINDS_DIR.len()..].starts_with('/'))
                .cloned()
                .chain(
                    name_conflicts
                        .iter()
                        .filter(|(p, c)| !c.directory && p.starts_with(&format!("{KINDS_DIR}/")))
                        .map(|(p, _)| p.clone()),
                )
                .collect(),
        };
        out.sort();
        Ok(out)
    }

    fn cache_key(&self) -> Option<Merkle> {
        match &self.inner {
            FilesInner::WorkingCopy { .. } => None,
            FilesInner::View { cache_key, .. } => *cache_key,
        }
    }
}

/// A relative path with no `..`, `.` or empty components, so joining it
/// onto the vault directory cannot leave it.
fn safe_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\\')
        && path
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != "..")
}

fn walk_files(dir: &Path, rel: &str, out: &mut Vec<String>) -> Result<(), RepositoryError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            // A non-UTF-8 name cannot be a kind file; list it lossily so it
            // is still reported as stray.
            out.push(format!("{rel}/{}", entry.file_name().to_string_lossy()));
            continue;
        };
        let child = format!("{rel}/{name}");
        if entry.file_type()?.is_dir() {
            walk_files(&entry.path(), &child, out)?;
        } else {
            out.push(child);
        }
    }
    Ok(())
}

/// View registries keyed by the view's state, so a registry is rebuilt only
/// when the view's visible changes do.
#[derive(Default)]
pub(crate) struct KindRegistryCache(Mutex<HashMap<Merkle, Arc<KindRegistry>>>);

impl Repository {
    /// Kind `kind`'s definition as **stored** in redb at exactly
    /// `kinds/<kind>/_kind.md`, or `None` when there is no such entry or
    /// `kind` is not a valid kind name. See [`KindDefinition::from_stored`].
    ///
    /// The KG is a projection of redb's vault entries in which the last
    /// deflate wins, so KG projection and the embedding policy take an
    /// instance's spec from here: the instance and its spec must come from
    /// the same layer, not from whatever the working copy holds (which a
    /// sandbox deflating a view's files may not even have, and which a
    /// case-insensitive filesystem would let `kinds/adr/_KIND.md` stand in
    /// for). The per-view gate (`kind validate`, `kind new`) still reads
    /// files through [`Repository::kind_files`].
    pub fn stored_kind_definition(
        &self,
        kind: &str,
    ) -> Result<Option<KindDefinition>, RepositoryError> {
        if !is_valid_kind_name(kind) {
            return Ok(None);
        }
        Ok(self
            .vault_retrieve(&kind_definition_path(kind))?
            .map(|entry| KindDefinition::from_stored(kind, &entry)))
    }

    /// Open a reader over `source`.
    ///
    /// # Errors
    ///
    /// [`RepositoryError::ViewNotFound`] for a view that does not exist.
    pub fn kind_files(&self, source: &KindSource) -> Result<KindFiles<'_>, RepositoryError> {
        let inner = match source {
            KindSource::WorkingCopy => FilesInner::WorkingCopy {
                vault_dir: self.vault_dir(),
            },
            KindSource::View(name) => {
                let before = self.kind_view_cache_key(name)?;
                let paths = self
                    .visible_file_paths(name)?
                    .into_iter()
                    .filter_map(|p| p.strip_prefix(VAULT_PREFIX).map(str::to_string))
                    .collect();
                let name_conflicts = self.kind_name_conflicts(name)?;
                let after = self.kind_view_cache_key(name)?;
                FilesInner::View {
                    name: name.clone(),
                    paths,
                    name_conflicts,
                    cache_key: (before == after).then_some(after),
                }
            }
        };
        Ok(KindFiles { repo: self, inner })
    }

    /// The kind registry of `source`.
    ///
    /// A view's registry is cached by the view's state (its Merkle combined
    /// with every ancestor's), so repeated calls return the same `Arc` until
    /// a record, insert or unrecord on the view or an ancestor changes it.
    /// The working-copy registry is rebuilt on every call: files on disk can
    /// change at any time.
    pub fn kind_registry(&self, source: &KindSource) -> Result<Arc<KindRegistry>, RepositoryError> {
        if let KindSource::View(name) = source {
            let key = self.kind_view_cache_key(name)?;
            if let Some(hit) = self.cached_kind_registry(&key) {
                return Ok(hit);
            }
        }
        let files = self.kind_files(source)?;
        self.kind_registry_for(&files)
    }

    /// The registry for an already-open reader, through the view cache.
    pub(crate) fn kind_registry_for(
        &self,
        files: &KindFiles<'_>,
    ) -> Result<Arc<KindRegistry>, RepositoryError> {
        let key = files.cache_key();
        if let Some(hit) = key.as_ref().and_then(|k| self.cached_kind_registry(k)) {
            return Ok(hit);
        }
        let registry = Arc::new(KindRegistry::load(files)?);
        if let Some(key) = key {
            let mut cache = self
                .kind_registry_cache
                .0
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if cache.len() >= REGISTRY_CACHE_CAPACITY {
                cache.clear();
            }
            cache.insert(key, Arc::clone(&registry));
        }
        Ok(registry)
    }

    /// The `kinds/**` paths of view `view_name` that the tree projection
    /// holds back as unresolved name conflicts — the same projection
    /// [`Repository::visible_file_paths`] and
    /// [`Repository::get_file_content_on_view`] use to omit or refuse them.
    fn kind_name_conflicts(
        &self,
        view_name: &str,
    ) -> Result<BTreeMap<String, KindNameConflict>, RepositoryError> {
        let db = |e: atomic_core::pristine::PristineError| RepositoryError::Database(e.to_string());
        let txn = self.pristine.read_txn().map_err(db)?;
        let view =
            txn.get_view(view_name)
                .map_err(db)?
                .ok_or_else(|| RepositoryError::ViewNotFound {
                    name: view_name.to_string(),
                })?;
        let visibility = super::graph_visibility_closure(&txn, &view)?;
        let projection = self.project_tree_for_visibility(&txn, &visibility)?;
        Ok(projection
            .name_conflicts
            .into_iter()
            .filter_map(|(path, conflict)| {
                let rel = path.strip_prefix(VAULT_PREFIX)?;
                let under_kinds = rel == KINDS_DIR || rel.starts_with(&format!("{KINDS_DIR}/"));
                let sides = conflict.sides_at_path(&path);
                let entry = KindNameConflict {
                    claimants: sides.len(),
                    directory: sides.iter().all(|side| side.is_directory()),
                };
                under_kinds.then(|| (rel.to_string(), entry))
            })
            .collect())
    }

    fn cached_kind_registry(&self, key: &Merkle) -> Option<Arc<KindRegistry>> {
        self.kind_registry_cache
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .cloned()
    }

    /// The view's Merkle folded with each ancestor's, outermost last. A view
    /// sees its ancestors' changes, so a record on the parent must change
    /// the child's key too.
    fn kind_view_cache_key(&self, view_name: &str) -> Result<Merkle, RepositoryError> {
        let db = |e: atomic_core::pristine::PristineError| RepositoryError::Database(e.to_string());
        let txn = self.pristine.read_txn().map_err(db)?;
        let view =
            txn.get_view(view_name)
                .map_err(db)?
                .ok_or_else(|| RepositoryError::ViewNotFound {
                    name: view_name.to_string(),
                })?;
        let mut key = view.state;
        let mut seen = HashSet::from([view.id]);
        let mut parent = view.parent;
        while let Some(id) = parent {
            if !seen.insert(id) {
                break;
            }
            let Some(ancestor) = txn.get_view_by_id(id).map_err(db)? else {
                break;
            };
            key = key.next(&ancestor.state);
            parent = ancestor.parent;
        }
        Ok(key)
    }
}
