//! In-process production of standard [OCI image layouts].
//!
//! This module turns directory contents (or in-memory file sets) into gzipped
//! tar layers and assembles them into a standard OCI image layout on disk —
//! the format consumed by any OCI runtime (podman, containerd, smolvm). Nothing
//! shells out: tar and gzip are built in memory with the `tar` and `flate2`
//! crates and digests are computed with `sha2`.
//!
//! The two entry points for building layers are [`layer_from_dir`] (recursively
//! tar a directory) and [`layer_from_entries`] (build a layer from an in-memory
//! set of files plus optional whiteouts, used for delta layers). The assembled
//! layout is written by [`write_image_layout`].
//!
//! [OCI image layouts]: https://github.com/opencontainers/image-spec/blob/main/image-layout.md

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use flate2::write::GzEncoder;
use flate2::Compression;
use sha2::{Digest, Sha256};
use tar::{Builder, EntryType, Header};
use walkdir::WalkDir;

/// Media type for a gzipped tar image layer.
const MEDIA_TYPE_LAYER: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
/// Media type for an image config blob.
const MEDIA_TYPE_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
/// Media type for an image manifest.
const MEDIA_TYPE_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
/// Media type for an image index.
const MEDIA_TYPE_INDEX: &str = "application/vnd.oci.image.index.v1+json";

/// A built, gzipped image layer plus its digests.
pub struct OciLayer {
    /// The gzipped tar bytes (the blob stored in the layout).
    pub tar_gz: Vec<u8>,
    /// `"sha256:<hex>"` of the **uncompressed** tar stream (the layer's
    /// `diff_id` in the image config's `rootfs`).
    pub diff_id: String,
    /// `"sha256:<hex>"` of the gzipped bytes (the layer descriptor digest).
    pub digest: String,
    /// Size, in bytes, of the gzipped layer.
    pub size: u64,
}

/// Configuration used to assemble an OCI image config and manifest.
pub struct OciImageConfig {
    /// Target architecture in OCI form, e.g. `"arm64"` or `"amd64"`.
    pub architecture: String,
    /// Operating system, e.g. `"linux"`.
    pub os: String,
    /// Environment variables, each as `"KEY=VAL"`.
    pub env: Vec<String>,
    /// Entrypoint argv. Empty if the image has no entrypoint.
    pub entrypoint: Vec<String>,
    /// Image-level annotations (used here to carry Atomic provenance).
    pub annotations: BTreeMap<String, String>,
}

/// Map the host architecture (`std::env::consts::ARCH`) to its OCI name.
///
/// `"aarch64"` becomes `"arm64"`, `"x86_64"` becomes `"amd64"`; any other value
/// is passed through unchanged.
pub fn host_arch() -> String {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    }
    .to_string()
}

/// Build a gzipped tar layer from a directory's contents (recursive).
///
/// Paths inside the tar are relative to `dir`. Regular files and symlinks are
/// included; directory entries themselves are omitted (their paths are implied
/// by the files within). Entries are emitted in a deterministic (sorted) order.
/// Computes `diff_id` (sha256 of the uncompressed tar stream) and `digest`
/// (sha256 of the gzipped bytes).
pub fn layer_from_dir(dir: &Path) -> std::io::Result<OciLayer> {
    let mut entries: Vec<(String, PathBuf, bool)> = Vec::new();
    for entry in WalkDir::new(dir) {
        let entry = entry.map_err(std::io::Error::from)?;
        let file_type = entry.file_type();
        if file_type.is_dir() {
            continue;
        }
        let rel = match entry.path().strip_prefix(dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        entries.push((
            rel.to_string_lossy().into_owned(),
            entry.path().to_path_buf(),
            file_type.is_symlink(),
        ));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut tar_buf = Vec::new();
    {
        let mut builder = Builder::new(&mut tar_buf);
        for (rel, full, is_symlink) in &entries {
            if *is_symlink {
                let target = std::fs::read_link(full)?;
                let mut header = Header::new_gnu();
                header.set_entry_type(EntryType::Symlink);
                header.set_size(0);
                header.set_mode(0o777);
                header.set_mtime(0);
                builder.append_link(&mut header, rel, &target)?;
            } else {
                let data = std::fs::read(full)?;
                let mut header = Header::new_gnu();
                header.set_entry_type(EntryType::Regular);
                header.set_size(data.len() as u64);
                header.set_mode(0o644);
                header.set_mtime(0);
                builder.append_data(&mut header, rel, data.as_slice())?;
            }
        }
        builder.finish()?;
    }

    finish_layer(tar_buf)
}

/// Build a single layer from an in-memory set of `(relative_path, bytes)` files
/// plus optional whiteouts.
///
/// Whiteouts mark deletions relative to a lower layer: a deleted path `foo/bar`
/// is encoded as a zero-byte tar entry `foo/.wh.bar`, per the OCI spec. Entries
/// are emitted in a deterministic (sorted) order. Useful for delta layers where
/// staging files on disk is undesirable.
pub fn layer_from_entries(
    files: &[(String, Vec<u8>)],
    whiteouts: &[String],
) -> std::io::Result<OciLayer> {
    let mut items: Vec<(String, Vec<u8>)> = Vec::with_capacity(files.len() + whiteouts.len());
    for (path, bytes) in files {
        items.push((path.clone(), bytes.clone()));
    }
    for path in whiteouts {
        items.push((whiteout_path(path), Vec::new()));
    }
    items.sort_by(|a, b| a.0.cmp(&b.0));

    let mut tar_buf = Vec::new();
    {
        let mut builder = Builder::new(&mut tar_buf);
        for (path, data) in &items {
            let mut header = Header::new_gnu();
            header.set_entry_type(EntryType::Regular);
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_mtime(0);
            builder.append_data(&mut header, path, data.as_slice())?;
        }
        builder.finish()?;
    }

    finish_layer(tar_buf)
}

/// Write a complete OCI image layout under `out` (created if needed):
/// `out/oci-layout`, `out/index.json`, and `out/blobs/sha256/<...>` blobs.
///
/// `layers` are ordered base→top. Returns the manifest digest (`"sha256:..."`).
/// The image config records the layers' uncompressed `diff_id`s; the manifest
/// records the gzipped layer digests; `config.annotations` are passed through to
/// the manifest.
pub fn write_image_layout(
    out: &Path,
    layers: &[OciLayer],
    config: &OciImageConfig,
) -> std::io::Result<String> {
    let blobs = out.join("blobs").join("sha256");
    std::fs::create_dir_all(&blobs)?;

    // Image config blob. diff_ids are the UNCOMPRESSED layer digests.
    let diff_ids: Vec<String> = layers.iter().map(|l| l.diff_id.clone()).collect();
    let history: Vec<serde_json::Value> = layers
        .iter()
        .map(|_| serde_json::json!({ "created_by": "atomic sandbox" }))
        .collect();
    let config_json = serde_json::json!({
        "architecture": config.architecture,
        "os": config.os,
        "config": {
            "Env": config.env,
            "Entrypoint": config.entrypoint,
        },
        "rootfs": {
            "type": "layers",
            "diff_ids": diff_ids,
        },
        "history": history,
    });
    let config_bytes = json_to_vec(&config_json)?;
    let config_hex = sha256_hex(&config_bytes);
    std::fs::write(blobs.join(&config_hex), &config_bytes)?;
    let config_digest = format!("sha256:{config_hex}");

    // Layer blobs (the gzipped bytes), keyed by their gzipped digest.
    for layer in layers {
        std::fs::write(blobs.join(digest_hex(&layer.digest)), &layer.tar_gz)?;
    }

    // Manifest blob. Layer digests here are the GZIPPED digests.
    let manifest_layers: Vec<serde_json::Value> = layers
        .iter()
        .map(|l| {
            serde_json::json!({
                "mediaType": MEDIA_TYPE_LAYER,
                "digest": l.digest,
                "size": l.size,
            })
        })
        .collect();
    let manifest_json = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": MEDIA_TYPE_MANIFEST,
        "config": {
            "mediaType": MEDIA_TYPE_CONFIG,
            "digest": config_digest,
            "size": config_bytes.len(),
        },
        "layers": manifest_layers,
        "annotations": config.annotations,
    });
    let manifest_bytes = json_to_vec(&manifest_json)?;
    let manifest_hex = sha256_hex(&manifest_bytes);
    std::fs::write(blobs.join(&manifest_hex), &manifest_bytes)?;
    let manifest_digest = format!("sha256:{manifest_hex}");

    // index.json references the manifest by digest + size.
    let index_json = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": MEDIA_TYPE_INDEX,
        "manifests": [{
            "mediaType": MEDIA_TYPE_MANIFEST,
            "digest": manifest_digest,
            "size": manifest_bytes.len(),
            "platform": {
                "architecture": config.architecture,
                "os": config.os,
            },
        }],
    });
    let index_bytes = json_to_vec(&index_json)?;
    std::fs::write(out.join("index.json"), &index_bytes)?;

    // The layout marker file.
    std::fs::write(out.join("oci-layout"), br#"{"imageLayoutVersion":"1.0.0"}"#)?;

    Ok(manifest_digest)
}

/// Finalize an uncompressed tar buffer into an [`OciLayer`]: compute the
/// uncompressed digest (`diff_id`), gzip the bytes, and compute the gzipped
/// digest and size.
fn finish_layer(tar_buf: Vec<u8>) -> std::io::Result<OciLayer> {
    let diff_id = format!("sha256:{}", sha256_hex(&tar_buf));

    let mut tar_gz = Vec::new();
    {
        let mut encoder = GzEncoder::new(&mut tar_gz, Compression::default());
        encoder.write_all(&tar_buf)?;
        encoder.finish()?;
    }

    let digest = format!("sha256:{}", sha256_hex(&tar_gz));
    let size = tar_gz.len() as u64;
    Ok(OciLayer {
        tar_gz,
        diff_id,
        digest,
        size,
    })
}

/// Encode the OCI whiteout entry name for a deleted path.
fn whiteout_path(path: &str) -> String {
    match path.rfind('/') {
        Some(idx) => format!("{}/.wh.{}", &path[..idx], &path[idx + 1..]),
        None => format!(".wh.{path}"),
    }
}

/// Strip the `sha256:` prefix from a digest, yielding the bare hex used as a
/// blob filename.
fn digest_hex(digest: &str) -> &str {
    digest.strip_prefix("sha256:").unwrap_or(digest)
}

/// Lowercase-hex encode bytes.
fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Compute the lowercase-hex sha256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_encode(&hasher.finalize())
}

/// Serialize a JSON value to bytes, mapping serialization failure to an IO error
/// so callers can use `?` against `std::io::Result`.
fn json_to_vec(value: &serde_json::Value) -> std::io::Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use tempfile::tempdir;

    /// Read all entries (path, bytes) from a gzipped tar layer.
    fn read_layer_entries(tar_gz: &[u8]) -> Vec<(String, Vec<u8>)> {
        let decoder = flate2::read::GzDecoder::new(tar_gz);
        let mut archive = tar::Archive::new(decoder);
        let mut out = Vec::new();
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().into_owned();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            out.push((path, bytes));
        }
        out
    }

    #[test]
    fn layer_from_dir_produces_valid_gzipped_tar() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), b"fn main() {}").unwrap();
        std::fs::write(dir.path().join("README.md"), b"# hello").unwrap();

        let layer = layer_from_dir(dir.path()).unwrap();

        assert!(layer.diff_id.starts_with("sha256:"));
        assert!(layer.digest.starts_with("sha256:"));
        // The uncompressed digest must differ from the compressed digest.
        assert_ne!(layer.diff_id, layer.digest);
        assert_eq!(layer.size, layer.tar_gz.len() as u64);

        let entries = read_layer_entries(&layer.tar_gz);
        let paths: Vec<&str> = entries.iter().map(|(p, _)| p.as_str()).collect();
        assert!(paths.contains(&"src/main.rs"));
        assert!(paths.contains(&"README.md"));

        let main = entries.iter().find(|(p, _)| p == "src/main.rs").unwrap();
        assert_eq!(main.1, b"fn main() {}");
    }

    #[test]
    fn layer_from_entries_encodes_whiteouts() {
        let files = vec![("a/b.txt".to_string(), b"content".to_vec())];
        let whiteouts = vec!["a/gone.txt".to_string()];

        let layer = layer_from_entries(&files, &whiteouts).unwrap();
        let entries = read_layer_entries(&layer.tar_gz);
        let paths: Vec<&str> = entries.iter().map(|(p, _)| p.as_str()).collect();

        assert!(paths.contains(&"a/b.txt"));
        assert!(paths.contains(&"a/.wh.gone.txt"));
        let wh = entries.iter().find(|(p, _)| p == "a/.wh.gone.txt").unwrap();
        assert!(wh.1.is_empty());
    }

    #[test]
    fn whiteout_path_handles_root_and_nested() {
        assert_eq!(whiteout_path("foo"), ".wh.foo");
        assert_eq!(whiteout_path("a/b/c"), "a/b/.wh.c");
    }

    #[test]
    fn write_image_layout_creates_addressable_blobs() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("file.txt"), b"data").unwrap();

        let layer = layer_from_dir(&src).unwrap();
        let layer_digest = layer.digest.clone();

        let mut annotations = BTreeMap::new();
        annotations.insert("org.atomic.view".to_string(), "dev".to_string());
        let config = OciImageConfig {
            architecture: host_arch(),
            os: "linux".to_string(),
            env: vec!["FOO=bar".to_string()],
            entrypoint: vec!["/bin/sh".to_string()],
            annotations,
        };

        let out = dir.path().join("image");
        let manifest_digest = write_image_layout(&out, &[layer], &config).unwrap();

        // oci-layout
        let layout = std::fs::read_to_string(out.join("oci-layout")).unwrap();
        assert_eq!(layout, r#"{"imageLayoutVersion":"1.0.0"}"#);

        // index.json references the manifest by digest + size.
        let index_bytes = std::fs::read(out.join("index.json")).unwrap();
        let index: serde_json::Value = serde_json::from_slice(&index_bytes).unwrap();
        let manifest_ref = &index["manifests"][0];
        assert_eq!(manifest_ref["digest"], manifest_digest);

        // Every blob filename equals the sha256 of its contents.
        let blobs = out.join("blobs").join("sha256");
        for entry in std::fs::read_dir(&blobs).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            let bytes = std::fs::read(entry.path()).unwrap();
            assert_eq!(name, sha256_hex(&bytes), "blob filename must match content");
        }

        // The manifest blob exists and resolves config + layer to real blobs.
        let manifest_hex = manifest_digest.strip_prefix("sha256:").unwrap();
        let manifest_bytes = std::fs::read(blobs.join(manifest_hex)).unwrap();
        let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes).unwrap();

        let config_digest = manifest["config"]["digest"].as_str().unwrap();
        assert!(blobs
            .join(config_digest.strip_prefix("sha256:").unwrap())
            .is_file());

        let manifest_layer_digest = manifest["layers"][0]["digest"].as_str().unwrap();
        assert_eq!(manifest_layer_digest, layer_digest);
        assert!(blobs
            .join(manifest_layer_digest.strip_prefix("sha256:").unwrap())
            .is_file());

        // Annotations are passed through.
        assert_eq!(manifest["annotations"]["org.atomic.view"], "dev");
    }
}
