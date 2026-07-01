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

use atomic_core::pristine::Pristine;
use serde::{Deserialize, Serialize};

use crate::changestore::{ChangeStore, DEFAULT_CACHE_CAPACITY};
use crate::oci::{self, OciImageConfig};

use super::{Repository, DOT_DIR};
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

/// Persisted contents of a sandbox pointer: where the canonical graph lives
/// and which view this sandbox operates on.
#[derive(Debug, Serialize, Deserialize)]
struct SandboxPointer {
    /// Absolute path to the canonical repository root (the one with `.atomic/`).
    canonical: PathBuf,
    /// View this sandbox operates on.
    view: String,
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
            let parsed: SandboxPointer = serde_json::from_slice(&bytes).ok()?;
            return Some((dir.clone(), parsed.canonical, parsed.view));
        }
        // Stop if we reach a real repository root — that's not a sandbox.
        if dir.join(DOT_DIR).join("pristine.redb").is_file() {
            return None;
        }
        if !dir.pop() {
            return None;
        }
    }
}

impl Repository {
    /// Open a repository for an agent sandbox.
    ///
    /// The agent's private working tree is at `working_root`, but the graph
    /// lives in the **canonical** repository found from `canonical`. All
    /// sandboxes opened this way read and write the same `pristine` and
    /// `changes` store, so there is exactly one graph. `view` selects which
    /// view this sandbox operates on (typically the agent's own draft view).
    ///
    /// The canonical `current_view` file on disk is left untouched — the view
    /// is held in memory for this handle only, so concurrent agents on
    /// different views never clobber one another.
    ///
    /// redb serialises writers, so concurrent `record`s from several agents
    /// take turns; reads run concurrently via MVCC.
    pub fn open_sandbox<P, Q>(
        working_root: P,
        canonical: Q,
        view: &str,
    ) -> Result<Self, RepositoryError>
    where
        P: AsRef<Path>,
        Q: AsRef<Path>,
    {
        let working_root = working_root.as_ref().to_path_buf();
        let canonical_root = Self::find_root(canonical.as_ref())?;
        let dot_dir = canonical_root.join(DOT_DIR);

        let pristine = Arc::new(
            Pristine::open_existing(dot_dir.join("pristine.redb"))
                .map_err(|e| RepositoryError::Database(e.to_string()))?,
        );
        let change_store = ChangeStore::new(dot_dir.join("changes"), DEFAULT_CACHE_CAPACITY)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(Self {
            root: working_root,
            dot_dir,
            current_view: view.to_string(),
            pristine,
            change_store,
            is_sandbox: true,
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
    /// Returns the number of files provisioned.
    ///
    /// Also writes a [`SANDBOX_POINTER`] file into `dest` so that `atomic`
    /// commands run inside the sandbox resolve to this repository's canonical
    /// graph and the given `view`.
    pub fn provision_sandbox<P: AsRef<Path>>(
        &self,
        dest: P,
        view: &str,
    ) -> Result<usize, RepositoryError> {
        let dest = dest.as_ref();
        let count = provision_working_tree(&self.root, dest)?;

        let pointer = SandboxPointer {
            canonical: self.root.clone(),
            view: view.to_string(),
        };
        let bytes = serde_json::to_vec_pretty(&pointer)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        std::fs::write(dest.join(SANDBOX_POINTER), bytes)?;

        Ok(count)
    }

    /// Materialize a view's visible file content into `dir` (created if needed).
    ///
    /// Writes each visible file's bytes exactly as they appear on `view` (the
    /// view's own changes plus its parent chain). Returns the number of files
    /// written.
    pub fn materialize_view_to(&self, view: &str, dir: &Path) -> Result<usize, RepositoryError> {
        std::fs::create_dir_all(dir)?;

        let mut count = 0usize;
        for path in self.visible_file_paths(view)? {
            let bytes = match self.get_file_content_on_view(&path, view)? {
                Some(bytes) => bytes,
                None => continue,
            };
            let target = dir.join(&path);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&target, &bytes)?;
            count += 1;
        }

        Ok(count)
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
        // 1. Base layer: materialize the shared base view to a temp dir.
        let base_dir = tempfile::tempdir()?;
        self.materialize_view_to(&opts.base_view, base_dir.path())?;
        let base_layer = oci::layer_from_dir(base_dir.path())?;
        let base_diff_id = base_layer.diff_id.clone();

        // 2. Delta: files new or changed on `view` vs. `base_view`, plus
        //    whiteouts for files present on the base but gone on `view`.
        let view_paths = self.visible_file_paths(&opts.view)?;
        let base_paths = self.visible_file_paths(&opts.base_view)?;

        let mut changed: Vec<(String, Vec<u8>)> = Vec::new();
        for path in &view_paths {
            let view_bytes = match self.get_file_content_on_view(path, &opts.view)? {
                Some(bytes) => bytes,
                None => continue,
            };
            let base_bytes = self.get_file_content_on_view(path, &opts.base_view)?;
            if base_bytes.as_deref() != Some(view_bytes.as_slice()) {
                changed.push((path.clone(), view_bytes));
            }
        }

        let mut whiteouts: Vec<String> = Vec::new();
        for path in &base_paths {
            if !view_paths.contains(path) {
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
        let work_dir = tempfile::tempdir()?;
        let files = self.materialize_view_to(&opts.view, work_dir.path())?;
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

        repo.add(rel, TrackingOptions::default()).unwrap();
        let header = ChangeHeader::new("add file");
        let options = RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(false);
        let outcome = repo.record(header, options).unwrap();
        repo.write_recorded(&outcome, InsertOptions::default())
            .unwrap();

        repo
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
}
