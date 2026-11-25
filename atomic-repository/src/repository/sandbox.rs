//! Concurrent agent sandboxes.
//!
//! A sandbox lets several agents work at once, each in its own working tree,
//! while sharing a single canonical graph. Two pieces make that work:
//!
//! 1. [`Repository::provision_sandbox`] — give an agent a private working tree
//!    that is a **copy-on-write clone** of the canonical one. On filesystems
//!    that support it (APFS, Btrfs, XFS, ReFS) the clone shares blocks until
//!    written, so five agents off a 200 MB base cost a few MB, not a gigabyte.
//!    Build artifacts (`node_modules`, `target`) land in the agent's own tree
//!    and never collide.
//!
//! 2. [`Repository::open_sandbox`] — open the repository with the agent's
//!    working tree as the root, but the **canonical** `.atomic/` as the graph.
//!    The graph (`pristine` + `changes`) is never cloned: there is one graph,
//!    and views are filters over it. Every `atomic` command the agent runs —
//!    `record`, `status`, `diff`, `vault query` — works unchanged, because it
//!    is a normal repository whose working tree just happens to live elsewhere.
//!
//! The only OS-specific concern (how to clone a file copy-on-write) is owned by
//! the `reflink-copy` crate, which falls back to a plain copy when the
//! filesystem cannot reflink. Our code has a single path.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use atomic_core::output::alive::RetrieveOptions;
use atomic_core::pristine::{CachedGraphTxn, Pristine, ViewTxnT};
use atomic_core::WorkingCopyId;

use serde::{Deserialize, Serialize};

use crate::changestore::{ChangeStore, DEFAULT_CACHE_CAPACITY};
use crate::oci::{self, OciImageConfig};

use super::content::retrieve_content_with_filter_fast;
use super::{graph_visibility_closure, working_copy, Repository, DOT_DIR};
use crate::RepositoryError;

/// Options for [`Repository::stage`].
pub struct StageOptions {
    /// The sandbox's draft view (the working view to package).
    pub view: String,
    /// The shared base view this sandbox forked from.
    pub base_view: String,
    /// Output directory for the OCI image layout (created if needed).
    pub out: PathBuf,
}

/// Result of [`Repository::stage`].
pub struct StageResult {
    /// Digest (`"sha256:..."`) of the written image manifest.
    pub manifest_digest: String,
    /// `diff_id` of the base layer — its content-address, by which a cached
    /// base can be matched and reused across staged iterations.
    pub base_diff_id: String,
    /// Number of files in the delta layer (new or changed vs. the base).
    pub delta_files: usize,
    /// The output directory the layout was written to.
    pub out: PathBuf,
}

/// Options for [`Repository::seal`].
pub struct SealOptions {
    /// The view to package as a flattened, self-contained image.
    pub view: String,
    /// Output directory for the OCI image layout (created if needed).
    pub out: PathBuf,
    /// Image entrypoint argv. Empty for none.
    pub entrypoint: Vec<String>,
    /// Image environment variables, each as `"KEY=VAL"`.
    pub env: Vec<String>,
}

/// Result of [`Repository::seal`].
pub struct SealResult {
    /// Digest (`"sha256:..."`) of the written image manifest.
    pub manifest_digest: String,
    /// Number of files written into the (single) layer.
    pub files: usize,
    /// The output directory the layout was written to.
    pub out: PathBuf,
}

/// Marker file written at the root of a sandbox working tree. Its presence
/// tells `Repository::open*` to resolve the graph from the canonical repo
/// instead of looking for a local `.atomic/`.
pub const SANDBOX_POINTER: &str = ".atomic-sandbox";

/// A remote sandbox's own directory, beside its pointer: the local cache of
/// its repository's rows (`cache/`, an ordinary repository whose working
/// tree is the sandbox). Never tracked.
pub const SANDBOX_CACHE_DIR: &str = ".atomic-sandbox.d";

/// Persisted contents of a sandbox pointer: where the canonical graph lives
/// and which view this sandbox operates on.
#[derive(Debug, Serialize, Deserialize)]
struct SandboxPointer {
    /// Absolute path to the canonical repository root (the one with `.atomic/`).
    canonical: PathBuf,
    /// View this sandbox operates on.
    view: String,
}

/// The repository a remote sandbox's grant is for, as the serving host
/// names it (its `RepositoryRef`): an authority and an opaque id, hex.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteRepositoryRef {
    pub authority: String,
    /// The repository id's bytes, lowercase hex.
    pub repository_id: String,
}

/// A remote sandbox's [`SANDBOX_POINTER`]: which repository and view its
/// grant reaches, and the token that proves it. No address — how the
/// sandbox reaches the serving host is the host's business (the CLI speaks
/// to whatever serves its daemon socket).
///
/// It holds a credential, so it is written readable by its owner only
/// ([`write_remote_sandbox_pointer`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteSandboxPointer {
    pub repository: RemoteRepositoryRef,
    pub view: String,
    /// The view's id as the host issued it (`ViewRef.view_id`), hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view_id: Option<String>,
    pub token: String,
}

impl RemoteSandboxPointer {
    /// The pointer at `dir`, if `dir` holds a remote one.
    pub fn read(dir: &Path) -> Option<Self> {
        let bytes = std::fs::read(dir.join(SANDBOX_POINTER)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }
}

/// Write `pointer` as `dir`'s [`SANDBOX_POINTER`], mode 0600 (it carries a
/// token).
pub fn write_remote_sandbox_pointer(
    dir: &Path,
    pointer: &RemoteSandboxPointer,
) -> Result<PathBuf, RepositoryError> {
    use std::io::Write;
    let path = dir.join(SANDBOX_POINTER);
    let bytes = serde_json::to_vec_pretty(pointer)
        .map_err(|e| RepositoryError::Serialization(e.to_string()))?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path)?;
    // An existing file keeps its mode through `open`; tighten it anyway.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(&bytes)?;
    Ok(path)
}

/// Walk up from `start` to the nearest [`SANDBOX_POINTER`]; if it is a
/// remote one, the sandbox's root and its pointer. Stops at a real
/// repository root.
pub fn find_remote_sandbox(start: &Path) -> Option<(PathBuf, RemoteSandboxPointer)> {
    let mut dir = if start.is_absolute() {
        start.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(start)
    };
    loop {
        if dir.join(SANDBOX_POINTER).is_file() {
            return RemoteSandboxPointer::read(&dir).map(|p| (dir, p));
        }
        if super::database::has_database(&dir.join(DOT_DIR)) || !dir.pop() {
            return None;
        }
    }
}

/// Walk up from `start` looking for a [`SANDBOX_POINTER`] file. If found,
/// return `(working_root, canonical_root, view)`.
pub(super) fn detect_sandbox(start: &Path) -> Option<(PathBuf, PathBuf, String)> {
    let mut dir = if start.is_absolute() {
        start.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(start)
    };

    loop {
        let pointer = dir.join(SANDBOX_POINTER);
        if pointer.is_file() {
            let bytes = std::fs::read(&pointer).ok()?;
            if let Ok(parsed) = serde_json::from_slice::<SandboxPointer>(&bytes) {
                return Some((dir.clone(), parsed.canonical, parsed.view));
            }
            // A remote pointer names a repository served elsewhere; locally,
            // the graph is the sandbox's cache (once `atomic sandbox
            // materialize` has made it).
            let remote: RemoteSandboxPointer = serde_json::from_slice(&bytes).ok()?;
            let cache = remote_cache_root(&dir);
            return cache
                .join(DOT_DIR)
                .is_dir()
                .then(|| (dir.clone(), cache, remote.view));
        }
        // Stop if we reach a real repository root — that's not a sandbox.
        if super::database::has_database(&dir.join(DOT_DIR)) {
            return None;
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Where a remote sandbox rooted at `working_root` keeps its cache.
pub fn remote_cache_root(working_root: &Path) -> PathBuf {
    working_root.join(SANDBOX_CACHE_DIR).join("cache")
}

impl Repository {
    /// Make (or remake) a remote sandbox's cache from `skeleton`: an empty
    /// repository under [`SANDBOX_CACHE_DIR`] holding the view's rows, with
    /// the sandbox as its working tree and the tree on disk as the clean
    /// baseline. Call it right after writing the view's files, with a remote
    /// sandbox link installed. Hydration completes before the cache is exposed
    /// to status/record callers, including library consumers.
    pub fn create_remote_sandbox_cache(
        working_root: &Path,
        skeleton: &super::SandboxSkeleton,
    ) -> Result<Self, RepositoryError> {
        let cache = remote_cache_root(working_root);
        if cache.exists() {
            std::fs::remove_dir_all(&cache)?;
        }
        std::fs::create_dir_all(&cache)?;
        let initial = Self::init(&cache)?;
        initial.import_sandbox_skeleton(skeleton)?;
        let layout = working_copy::layout_for_paths(
            working_root.to_path_buf(),
            initial.dot_dir.clone(),
            working_root.join(DOT_DIR),
            false,
        )?;
        working_copy::migrate_identity(&initial.pristine, &layout, &skeleton.view.name)?;
        drop(initial);
        let repo = Self::open_sandbox(working_root, &cache, &skeleton.view.name)?;
        repo.hydrate_remote_sandbox()?;
        repo.reindex_working_copy(repo.require_working_copy_id()?)?;
        // The view's vault arrived as files, as after a pull: its tables
        // come from them.
        if repo.vault_dir().exists() {
            repo.bootstrap_vault_from_working_copy()?;
        }
        Ok(repo)
    }

    /// Whether this is a remote sandbox's cache (its graph arrives from the
    /// repository's owner, and its changes go back to it).
    pub fn is_remote_sandbox(&self) -> bool {
        self.is_sandbox && self.dot_dir.starts_with(self.root.join(SANDBOX_CACHE_DIR))
    }

    /// Open a repository for an agent sandbox.
    ///
    /// The agent's private working tree is at `working_root`, but the graph
    /// lives in the **canonical** repository found from `canonical`. All
    /// sandboxes opened this way read and write the same repository database
    /// and `changes` store, so there is exactly one graph. `view` selects which
    /// view this sandbox operates on (typically the agent's own draft view).
    ///
    /// The sandbox's persistent working-copy record selects its desired view.
    /// Its local `.atomic/current_view` is derived compatibility output; the
    /// canonical working copy's record and compatibility file remain untouched.
    ///
    /// This writable open requires exclusive process access to the canonical
    /// database. Read-only sandbox queries use `open_readonly` to share access
    /// with other readers.
    pub fn open_sandbox<P, Q>(
        working_root: P,
        canonical: Q,
        _view: &str,
    ) -> Result<Self, RepositoryError>
    where
        P: AsRef<Path>,
        Q: AsRef<Path>,
    {
        Self::open_sandbox_with_mode(working_root.as_ref(), canonical.as_ref(), _view, false)
    }

    pub(super) fn open_sandbox_readonly(
        working_root: PathBuf,
        canonical: PathBuf,
        view: &str,
    ) -> Result<Self, RepositoryError> {
        Self::open_sandbox_with_mode(&working_root, &canonical, view, true)
    }

    fn open_sandbox_with_mode(
        working_root: &Path,
        canonical: &Path,
        _view: &str,
        read_only: bool,
    ) -> Result<Self, RepositoryError> {
        let working_root = working_root.to_path_buf();
        let canonical_root = Self::find_root(canonical)?;
        let dot_dir = canonical_root.join(DOT_DIR);

        let path = super::ensure_database(&dot_dir)?;
        let pristine = if read_only {
            Pristine::open_readonly(path)
        } else {
            Pristine::open_existing(path)
        };
        let pristine = Arc::new(pristine.map_err(RepositoryError::from)?);
        let change_store = ChangeStore::new(dot_dir.join("changes"), DEFAULT_CACHE_CAPACITY)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let layout = working_copy::layout_for_paths(
            working_root.clone(),
            dot_dir.clone(),
            working_root.join(DOT_DIR),
            false,
        )?;
        // A bare sandbox pointer (`.atomic-sandbox` written by a foreign
        // harness or an older integration) has no local working-copy
        // identity. Read-only opens resolve the canonical repository's
        // registered identity instead of demanding a writable migration;
        // writable opens keep requiring provisioning.
        let (_working_copy_id, current_view) =
            match working_copy::load_registered_identity(&pristine, &layout) {
                Ok(identity) => identity,
                Err(RepositoryError::WorkingCopyMigrationRequired { .. }) if read_only => {
                    let canonical_layout = working_copy::discover_layout(&canonical_root)?;
                    working_copy::load_registered_identity(&pristine, &canonical_layout)?
                }
                Err(error) => return Err(error),
            };

        Ok(Self {
            root: working_root,
            dot_dir,
            current_view,
            pristine,
            change_store,
            is_sandbox: true,
            semantic_closure_memo: Default::default(),
        })
    }

    /// Provision an agent's private working tree as a copy-on-write clone of
    /// this repository's working tree.
    ///
    /// Copies every tracked-or-untracked working file into `dest`, reflinking
    /// where the filesystem supports it (so unchanged blocks are shared on
    /// disk) and falling back to a plain copy otherwise. The `.atomic/`
    /// directory is **never** cloned — the sandbox shares the canonical graph
    /// via [`Repository::open_sandbox`].
    ///
    /// `working_copy` identifies and validates the source directory being cloned.
    /// Returns the number of files provisioned.
    ///
    /// Also writes a [`SANDBOX_POINTER`] file into `dest` so that `atomic`
    /// commands run inside the sandbox resolve to this repository's canonical
    /// graph and the given `view`.
    pub fn provision_sandbox<P: AsRef<Path>>(
        &self,
        working_copy: WorkingCopyId,
        dest: P,
        view: &str,
    ) -> Result<usize, RepositoryError> {
        self.validate_working_copy(working_copy)?;
        let dest = dest.as_ref();
        {
            let txn = self
                .pristine
                .read_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            let view_state = txn
                .get_view(view)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .ok_or_else(|| RepositoryError::ViewNotFound {
                    name: view.to_string(),
                })?;
            graph_visibility_closure(&txn, &view_state)?;
        }
        let count = provision_working_tree(&self.root, dest)?;

        let pointer = SandboxPointer {
            canonical: self.root.clone(),
            view: view.to_string(),
        };
        let bytes = serde_json::to_vec_pretty(&pointer)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        std::fs::write(dest.join(SANDBOX_POINTER), bytes)?;

        let layout = working_copy::layout_for_paths(
            dest.to_path_buf(),
            self.dot_dir.clone(),
            dest.join(DOT_DIR),
            false,
        )?;
        working_copy::migrate_identity(&self.pristine, &layout, view)?;

        Ok(count)
    }

    /// Provision a new sandbox from the recorded `view`, never from working files.
    ///
    /// The destination must not exist and its parent must exist. The checkout
    /// renderer supplies bytes, inode modes, symlink kinds, and conflict output.
    /// Absolute symlinks and symlinks containing parent components are not
    /// supported: a review snapshot must not expose files outside itself.
    /// Entries may not traverse symlink ancestors or overwrite sandbox metadata.
    /// On failure, the newly created directory is retained for owner inspection
    /// and cleanup; no existing destination is overwritten or removed.
    pub fn provision_recorded_sandbox(
        &self,
        dest: impl AsRef<Path>,
        view: &str,
    ) -> Result<super::ViewSnapshot, RepositoryError> {
        let dest = dest.as_ref();
        let name = dest.file_name().ok_or_else(|| unsafe_snapshot_path(dest))?;
        if !matches!(
            dest.components().next_back(),
            Some(std::path::Component::Normal(_))
        ) {
            return Err(unsafe_snapshot_path(dest));
        }
        // Resolve the caller's parent once, then keep output beneath the new
        // directory. create_dir refuses existing directories and dangling links.
        let parent = dest
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let dest = parent.canonicalize()?.join(name);
        let builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        let builder = {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = builder;
            builder.mode(0o700);
            builder
        };
        builder.create(&dest)?;
        let snapshot = self.materialize_view_entries(view, |entry| {
            write_recorded_snapshot_entry(&dest, &entry)
        })??;

        let pointer = SandboxPointer {
            canonical: self
                .dot_dir
                .parent()
                .ok_or_else(|| RepositoryError::InvalidOperation {
                    message: "canonical repository directory has no parent".to_string(),
                })?
                .to_path_buf(),
            view: view.to_string(),
        };
        use std::io::Write;
        let bytes = serde_json::to_vec_pretty(&pointer)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dest.join(SANDBOX_POINTER))?
            .write_all(&bytes)?;
        let layout = working_copy::layout_for_paths(
            dest.clone(),
            self.dot_dir.clone(),
            dest.join(DOT_DIR),
            false,
        )?;
        let (_, registered_view) = working_copy::migrate_identity(&self.pristine, &layout, view)?;
        if registered_view != view {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "sandbox location is already registered on view '{registered_view}', not '{view}'; retained '{}' for inspection",
                    dest.display()
                ),
            });
        }
        Ok(snapshot)
    }

    fn materialized_view_files(
        &self,
        view_name: &str,
    ) -> Result<BTreeMap<String, Vec<u8>>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let view = txn
            .get_view(view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.to_string(),
            })?;
        let visibility = graph_visibility_closure(&txn, &view)?;
        let projection = self.project_tree_for_visibility(&txn, &visibility)?;
        if !projection.name_conflicts.is_empty() {
            return Err(RepositoryError::InvalidOperation {
                message: "cannot package a view with unresolved path claims".to_string(),
            });
        }
        let cached_txn =
            CachedGraphTxn::new(&txn).map_err(|e| RepositoryError::Database(e.to_string()))?;

        let mut files = BTreeMap::new();
        for item in projection.present.into_values() {
            if item.is_directory {
                continue;
            }
            let bytes = match super::content::retrieve_span_if_present(
                &txn,
                &self.change_store,
                item.inode,
                &visibility,
            )? {
                Some(content) => content,
                None => {
                    let options = RetrieveOptions::new().with_graph_visibility(visibility.clone());
                    retrieve_content_with_filter_fast(
                        &cached_txn,
                        &self.change_store,
                        item.inode,
                        item.position,
                        options,
                    )
                    .map_err(|e| RepositoryError::Database(e.to_string()))?
                }
            };
            files.insert(item.path, bytes);
        }
        Ok(files)
    }

    fn write_materialized_files(
        dir: &Path,
        files: &BTreeMap<String, Vec<u8>>,
    ) -> Result<(), RepositoryError> {
        std::fs::create_dir_all(dir)?;
        for (path, bytes) in files {
            let target = dir.join(path);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(target, bytes)?;
        }
        Ok(())
    }

    /// Materialize a view's visible file content into `dir` (created if needed).
    ///
    /// Writes each visible file's bytes exactly as they appear on `view` (the
    /// view's own changes plus its parent chain). Returns the number of files
    /// written.
    pub fn materialize_view_to(&self, view: &str, dir: &Path) -> Result<usize, RepositoryError> {
        let files = self.materialized_view_files(view)?;
        Self::write_materialized_files(dir, &files)?;
        Ok(files.len())
    }

    /// Produce a two-layer OCI image: a base layer (the `base_view`
    /// materialized) plus a thin delta layer (files that differ on `view`
    /// versus `base_view`, with whiteouts for files removed on `view`).
    ///
    /// Built for the inner loop: the base is content-addressed by its `diff_id`
    /// and can be pulled once and cached, so each iteration ships only the small
    /// delta. Provenance (view names and Merkle states) is recorded in the
    /// manifest annotations. Returns the manifest digest and base `diff_id`.
    pub fn stage(&self, opts: StageOptions) -> Result<StageResult, RepositoryError> {
        // Resolve both snapshots before creating output state. Each snapshot
        // builds and validates its view closure once.
        let base_files = self.materialized_view_files(&opts.base_view)?;
        let view_files = self.materialized_view_files(&opts.view)?;

        // 1. Base layer: materialize the shared base view to a temp dir.
        let base_dir = tempfile::tempdir()?;
        Self::write_materialized_files(base_dir.path(), &base_files)?;
        let base_layer = oci::layer_from_dir(base_dir.path())?;
        let base_diff_id = base_layer.diff_id.clone();

        // 2. Delta: files new or changed on `view` vs. `base_view`, plus
        //    whiteouts for files present on the base but gone on `view`.
        let mut changed: Vec<(String, Vec<u8>)> = Vec::new();
        for (path, view_bytes) in &view_files {
            if base_files.get(path) != Some(view_bytes) {
                changed.push((path.clone(), view_bytes.clone()));
            }
        }

        let mut whiteouts: Vec<String> = Vec::new();
        for path in base_files.keys() {
            if !view_files.contains_key(path) {
                whiteouts.push(path.clone());
            }
        }

        let delta_files = changed.len();
        let delta_layer = oci::layer_from_entries(&changed, &whiteouts)?;

        // 3. Assemble the layout with provenance annotations.
        let view_info = self.get_view_info(&opts.view)?;
        let base_info = self.get_view_info(&opts.base_view)?;
        let mut annotations = BTreeMap::new();
        annotations.insert("org.atomic.view".to_string(), opts.view.clone());
        annotations.insert("org.atomic.base-view".to_string(), opts.base_view.clone());
        annotations.insert(
            "org.atomic.view-merkle".to_string(),
            view_info.state_base32(),
        );
        annotations.insert(
            "org.atomic.base-merkle".to_string(),
            base_info.state_base32(),
        );

        let config = OciImageConfig {
            architecture: oci::host_arch(),
            os: "linux".to_string(),
            env: Vec::new(),
            entrypoint: Vec::new(),
            annotations,
        };

        let manifest_digest =
            oci::write_image_layout(&opts.out, &[base_layer, delta_layer], &config)?;

        Ok(StageResult {
            manifest_digest,
            base_diff_id,
            delta_files,
            out: opts.out,
        })
    }

    /// Produce a single-layer (flattened) OCI image of the full merged state of
    /// `view`.
    ///
    /// Because a draft view sees its entire parent chain, the materialized
    /// content is the complete, self-contained rootfs — a deployable runtime.
    /// Provenance (view name and Merkle state) is recorded in the manifest
    /// annotations. Returns the manifest digest and file count.
    pub fn seal(&self, opts: SealOptions) -> Result<SealResult, RepositoryError> {
        let materialized = self.materialized_view_files(&opts.view)?;
        let files = materialized.len();
        let work_dir = tempfile::tempdir()?;
        Self::write_materialized_files(work_dir.path(), &materialized)?;
        let layer = oci::layer_from_dir(work_dir.path())?;

        let view_info = self.get_view_info(&opts.view)?;
        let mut annotations = BTreeMap::new();
        annotations.insert("org.atomic.view".to_string(), opts.view.clone());
        annotations.insert(
            "org.atomic.view-merkle".to_string(),
            view_info.state_base32(),
        );

        let config = OciImageConfig {
            architecture: oci::host_arch(),
            os: "linux".to_string(),
            env: opts.env.clone(),
            entrypoint: opts.entrypoint.clone(),
            annotations,
        };

        let manifest_digest = oci::write_image_layout(&opts.out, &[layer], &config)?;

        Ok(SealResult {
            manifest_digest,
            files,
            out: opts.out,
        })
    }
}

/// Clone a working tree from `src` to `dest`, one file at a time, reflinking
/// where possible. Skips the `.atomic/` graph directory.
fn provision_working_tree(src: &Path, dest: &Path) -> Result<usize, RepositoryError> {
    use walkdir::WalkDir;

    let mut count = 0usize;
    std::fs::create_dir_all(dest)?;

    for entry in WalkDir::new(src).into_iter().filter_entry(|e| {
        // Never descend into the canonical graph — it is shared, not cloned.
        if e.file_name() == DOT_DIR {
            return false;
        }
        // Never descend into a nested sandbox (a dir holding its own pointer).
        if e.file_type().is_dir() && e.path().join(SANDBOX_POINTER).is_file() {
            return false;
        }
        true
    }) {
        let entry = entry.map_err(std::io::Error::from)?;
        let rel = match entry.path().strip_prefix(src) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let target = dest.join(rel);

        let ft = entry.file_type();
        if ft.is_dir() {
            std::fs::create_dir_all(&target)?;
        } else if ft.is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // Single cross-platform path: reflink (CoW) where the filesystem
            // supports it, plain copy where it does not.
            reflink_copy::reflink_or_copy(entry.path(), &target)?;
            count += 1;
        } else if ft.is_symlink() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let _ = recreate_symlink(entry.path(), &target);
        }
    }

    Ok(count)
}

/// Recreate a symlink found at `src` into `dest`, preserving its target.
///
/// Unix has a dedicated definition; everything else falls back below.
#[cfg(unix)]
fn recreate_symlink(src: &Path, dest: &Path) -> std::io::Result<()> {
    let link_target = std::fs::read_link(src)?;
    std::os::unix::fs::symlink(link_target, dest)
}

/// Non-Unix fallback: symlink creation needs special privileges, so copy the
/// file the link resolves to (correct content-wise).
#[cfg(not(unix))]
fn recreate_symlink(src: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::copy(src, dest).map(|_| ())
}

fn unsafe_snapshot_path(path: &Path) -> RepositoryError {
    RepositoryError::InvalidOperation {
        message: format!(
            "unsafe recorded sandbox path '{}': paths must be relative, without parent components, symlink ancestors, or repository metadata; external/parent symlinks are unsupported",
            path.display()
        ),
    }
}

fn validate_snapshot_relative(path: &Path) -> Result<(), RepositoryError> {
    use std::path::Component;
    let bytes = path.as_os_str().as_encoded_bytes();
    if bytes.is_empty()
        || bytes.contains(&b'\\')
        || bytes.contains(&b':')
        || bytes.contains(&0)
        || bytes.split(|b| *b == b'/').any(|part| {
            part.is_empty()
                || part == b"."
                || part == b".."
                || [DOT_DIR, ".git", SANDBOX_POINTER, SANDBOX_CACHE_DIR]
                    .iter()
                    .any(|reserved| part.eq_ignore_ascii_case(reserved.as_bytes()))
        })
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(unsafe_snapshot_path(path));
    }
    Ok(())
}

fn ensure_snapshot_directories(root: &Path, relative: &Path) -> Result<(), RepositoryError> {
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => return Err(unsafe_snapshot_path(relative)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&current)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn write_recorded_snapshot_entry(
    root: &Path,
    entry: &super::ViewEntry,
) -> Result<(), RepositoryError> {
    use super::ViewEntryKind;
    use std::io::Write;
    let relative = Path::new(&entry.path);
    validate_snapshot_relative(relative)?;
    let target = root.join(relative);
    if let Some(parent) = relative.parent() {
        ensure_snapshot_directories(root, parent)?;
    }
    match entry.kind {
        ViewEntryKind::Directory => ensure_snapshot_directories(root, relative)?,
        ViewEntryKind::File => {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)?;
            file.write_all(&entry.content)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(std::fs::Permissions::from_mode(u32::from(
                    entry.mode & 0o777,
                )))?;
            }
        }
        ViewEntryKind::Symlink => {
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStrExt;
                let link = Path::new(std::ffi::OsStr::from_bytes(&entry.content));
                validate_snapshot_relative(link)?;
                std::os::unix::fs::symlink(link, &target)?;
            }
            #[cfg(not(unix))]
            return Err(RepositoryError::InvalidOperation {
                message: "recorded sandbox symlinks are unsupported on this platform".to_string(),
            });
        }
    }
    #[cfg(unix)]
    if entry.kind == ViewEntryKind::Directory {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            target,
            std::fs::Permissions::from_mode(u32::from(entry.mode & 0o777)),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::RecordOptions;
    use crate::tracking::TrackingOptions;
    use crate::InsertOptions;
    use atomic_core::change::ChangeHeader;
    use tempfile::tempdir;

    /// Initialize a repository at `root` and record+apply a single file on the
    /// default `dev` view, returning the opened repository.
    fn repo_with_recorded_file(root: &Path, rel: &str, contents: &[u8]) -> Repository {
        std::fs::create_dir_all(root).unwrap();
        let repo = Repository::init(root).unwrap();

        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, contents).unwrap();

        record_path(&repo, rel);
        repo
    }

    fn record_path(repo: &Repository, rel: &str) {
        repo.add(
            repo.require_working_copy_id().unwrap(),
            rel,
            TrackingOptions::default(),
        )
        .unwrap();
        let header = ChangeHeader::new("add file");
        let options = RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(false);
        let outcome = repo
            .record(repo.require_working_copy_id().unwrap(), header, options)
            .unwrap();
        repo.write_recorded(&outcome, InsertOptions::default())
            .unwrap();
    }

    #[test]
    fn recorded_sandbox_excludes_dirty_files_and_keeps_source_identity() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("repo");
        let repo = repo_with_recorded_file(&source, "src/file.txt", b"recorded\0bytes\n");
        let source_id = repo.require_working_copy_id().unwrap();
        let source_view = std::fs::read(source.join(DOT_DIR).join("current_view")).unwrap();
        let source_identity = std::fs::read(source.join(DOT_DIR).join("working_copy_id")).unwrap();
        std::fs::write(source.join("src/file.txt"), b"dirty\n").unwrap();
        std::fs::write(source.join(".env"), b"SECRET=never-copy\n").unwrap();
        let dest = dir.path().join("review");

        let snapshot = repo.provision_recorded_sandbox(&dest, "dev").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&dest).unwrap().permissions().mode() & 0o077,
                0
            );
        }
        assert_eq!(snapshot.view, "dev");
        assert_eq!(snapshot.change_count, 1);
        let expected = repo
            .materialize_view_entries::<std::convert::Infallible>("dev", |_| Ok(()))
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.state, expected.state);
        assert_eq!(
            std::fs::read(dest.join("src/file.txt")).unwrap(),
            b"recorded\0bytes\n"
        );
        assert!(!dest.join(".env").exists());
        assert_eq!(
            std::fs::read(source.join("src/file.txt")).unwrap(),
            b"dirty\n"
        );
        assert_eq!(
            std::fs::read(source.join(".env")).unwrap(),
            b"SECRET=never-copy\n"
        );
        assert_eq!(
            std::fs::read(source.join(DOT_DIR).join("current_view")).unwrap(),
            source_view
        );
        assert_eq!(
            std::fs::read(source.join(DOT_DIR).join("working_copy_id")).unwrap(),
            source_identity
        );
        assert_eq!(repo.require_working_copy_id().unwrap(), source_id);
        drop(repo);

        let sandbox = Repository::open_existing(&dest).unwrap();
        let sandbox_id = sandbox.require_working_copy_id().unwrap();
        assert_ne!(sandbox_id, source_id);
        assert_eq!(sandbox.desired_view_name(sandbox_id).unwrap(), "dev");
        assert_eq!(sandbox.current_view, "dev");
    }

    #[test]
    fn recorded_sandbox_refuses_existing_destination_without_mutation() {
        let dir = tempdir().unwrap();
        let repo = repo_with_recorded_file(&dir.path().join("repo"), "file", b"recorded");
        let dest = dir.path().join("existing");
        std::fs::create_dir(&dest).unwrap();
        std::fs::write(dest.join("keep"), b"owned by someone else").unwrap();
        assert!(repo.provision_recorded_sandbox(&dest, "dev").is_err());
        assert_eq!(
            std::fs::read(dest.join("keep")).unwrap(),
            b"owned by someone else"
        );
        assert!(!dest.join("file").exists());
        assert!(!dest.join(SANDBOX_POINTER).exists());
    }

    fn snapshot_entry(
        path: &str,
        kind: super::super::ViewEntryKind,
        content: &[u8],
    ) -> super::super::ViewEntry {
        super::super::ViewEntry {
            path: path.to_string(),
            inode: 1,
            kind,
            mode: 0o755,
            content: content.to_vec(),
            hash: atomic_core::types::Hash::of(content),
            conflict_marker_line: None,
        }
    }

    #[test]
    fn recorded_sandbox_rejects_traversal_and_metadata_paths() {
        use super::super::ViewEntryKind;
        let dir = tempdir().unwrap();
        for path in [
            "../escape",
            "/absolute",
            "a/../../escape",
            "a/./file",
            "a\\escape",
            "C:/escape",
            ".atomic/current_view",
            ".atomic-sandbox",
            ".git/config",
        ] {
            let entry = snapshot_entry(path, ViewEntryKind::File, b"unsafe");
            assert!(
                write_recorded_snapshot_entry(dir.path(), &entry).is_err(),
                "{path}"
            );
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn recorded_sandbox_preserves_recorded_mode_and_safe_symlink() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = tempdir().unwrap();
        let source = dir.path().join("repo");
        let repo = Repository::init(&source).unwrap();
        std::fs::write(source.join("tool"), b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(source.join("tool"), std::fs::Permissions::from_mode(0o751))
            .unwrap();
        record_path(&repo, "tool");
        symlink("tool", source.join("link")).unwrap();
        record_path(&repo, "link");
        std::fs::set_permissions(source.join("tool"), std::fs::Permissions::from_mode(0o600))
            .unwrap();
        let dest = dir.path().join("review");

        repo.provision_recorded_sandbox(&dest, "dev").unwrap();
        assert_eq!(
            std::fs::metadata(dest.join("tool"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o751
        );
        assert!(std::fs::symlink_metadata(dest.join("link"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::read_link(dest.join("link")).unwrap(),
            Path::new("tool")
        );
        assert_eq!(
            std::fs::metadata(source.join("tool"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::read_link(source.join("link")).unwrap(),
            Path::new("tool")
        );
    }

    #[cfg(unix)]
    #[test]
    fn recorded_sandbox_rejects_external_links_and_symlink_ancestors() {
        use super::super::ViewEntryKind;
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        for target in ["../outside", "/etc/passwd", "dir/../../outside"] {
            let entry = snapshot_entry("link", ViewEntryKind::Symlink, target.as_bytes());
            assert!(write_recorded_snapshot_entry(dir.path(), &entry).is_err());
            assert!(std::fs::symlink_metadata(dir.path().join("link")).is_err());
        }
        symlink(outside.path(), dir.path().join("ancestor")).unwrap();
        let entry = snapshot_entry("ancestor/escape", ViewEntryKind::File, b"unsafe");
        assert!(write_recorded_snapshot_entry(dir.path(), &entry).is_err());
        assert!(!outside.path().join("escape").exists());
    }

    #[cfg(unix)]
    #[test]
    fn recorded_sandbox_refuses_recorded_external_symlink() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        let source = dir.path().join("repo");
        let repo = Repository::init(&source).unwrap();
        symlink("../private-secret", source.join("link")).unwrap();
        record_path(&repo, "link");
        let dest = dir.path().join("review");

        let error = repo.provision_recorded_sandbox(&dest, "dev").unwrap_err();
        assert!(error
            .to_string()
            .contains("external/parent symlinks are unsupported"));
        assert!(
            dest.is_dir(),
            "partial output is retained, not recursively deleted"
        );
        assert!(!dest.join(SANDBOX_POINTER).exists());
        assert_eq!(
            std::fs::read_link(source.join("link")).unwrap(),
            Path::new("../private-secret")
        );
    }

    #[cfg(unix)]
    #[test]
    fn recorded_sandbox_refuses_dangling_destination_symlink() {
        let dir = tempdir().unwrap();
        let repo = repo_with_recorded_file(&dir.path().join("repo"), "file", b"recorded");
        let dest = dir.path().join("review");
        let outside = dir.path().join("not-created");
        std::os::unix::fs::symlink(&outside, &dest).unwrap();

        assert!(repo.provision_recorded_sandbox(&dest, "dev").is_err());
        assert_eq!(std::fs::read_link(&dest).unwrap(), outside);
        assert!(!outside.exists());
    }

    #[test]
    fn materialize_view_to_writes_visible_files() {
        let dir = tempdir().unwrap();
        let repo = repo_with_recorded_file(&dir.path().join("repo"), "src/hello.txt", b"hi\n");

        let out = dir.path().join("mat");
        let count = repo.materialize_view_to("dev", &out).unwrap();

        assert_eq!(count, 1);
        assert_eq!(std::fs::read(out.join("src/hello.txt")).unwrap(), b"hi\n");
    }

    #[test]
    fn seal_produces_single_layer_oci_layout() {
        let dir = tempdir().unwrap();
        let repo = repo_with_recorded_file(&dir.path().join("repo"), "hello.txt", b"hello world\n");

        let out = dir.path().join("image");
        let result = repo
            .seal(SealOptions {
                view: "dev".to_string(),
                out: out.clone(),
                entrypoint: vec!["/bin/sh".to_string()],
                env: vec!["FOO=bar".to_string()],
            })
            .unwrap();

        assert_eq!(result.files, 1);
        assert!(result.manifest_digest.starts_with("sha256:"));
        assert!(out.join("oci-layout").is_file());
        assert!(out.join("index.json").is_file());

        // The manifest blob exists and references exactly one layer.
        let manifest_hex = result.manifest_digest.strip_prefix("sha256:").unwrap();
        let manifest_path = out.join("blobs").join("sha256").join(manifest_hex);
        assert!(manifest_path.is_file());
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        assert_eq!(manifest["layers"].as_array().unwrap().len(), 1);
        assert_eq!(manifest["annotations"]["org.atomic.view"], "dev");
    }

    #[test]
    fn provision_clones_working_tree_and_skips_dot_dir() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("repo");
        std::fs::create_dir_all(src.join("src")).unwrap();
        std::fs::create_dir_all(src.join(DOT_DIR)).unwrap();
        std::fs::write(src.join("src/main.rs"), b"fn main() {}").unwrap();
        std::fs::write(src.join("Cargo.toml"), b"[package]").unwrap();
        std::fs::write(src.join(DOT_DIR).join("pristine.redb"), b"GRAPH").unwrap();

        let dest = dir.path().join("agent-1");
        let count = provision_working_tree(&src, &dest).unwrap();

        assert_eq!(count, 2, "two working files cloned");
        assert_eq!(
            std::fs::read(dest.join("src/main.rs")).unwrap(),
            b"fn main() {}"
        );
        assert!(dest.join("Cargo.toml").exists());
        assert!(
            !dest.join(DOT_DIR).exists(),
            "the canonical graph must not be cloned into the sandbox"
        );
    }

    #[test]
    fn a_sandbox_never_sees_its_pointer_as_untracked() {
        let dir = tempdir().unwrap();
        let repo = repo_with_recorded_file(&dir.path().join("repo"), "hello.txt", b"hi\n");
        let sandbox = dir.path().join("agent-1");
        repo.provision_sandbox(repo.require_working_copy_id().unwrap(), &sandbox, "dev")
            .unwrap();
        std::fs::write(sandbox.join("new.txt"), b"new\n").unwrap();
        drop(repo);

        let opened = Repository::open_existing(&sandbox).unwrap();
        let status = opened
            .status(
                opened.require_working_copy_id().unwrap(),
                Default::default(),
            )
            .unwrap();
        let untracked: Vec<_> = status.untracked().map(|e| e.path().to_path_buf()).collect();
        assert_eq!(
            untracked,
            vec![PathBuf::from("new.txt")],
            "the pointer can carry a credential; `record --all` must never pick it up"
        );
    }
}
