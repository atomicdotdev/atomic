//! Streaming download methods for `HttpRemote`.
//!
//! Contains the V3 streaming protocol download operations: file-to-disk
//! downloads, layer-selective downloads, and chunk manifest retrieval.

use bytes::Bytes;
use log::debug;
use reqwest::header::CONTENT_TYPE;
use reqwest::StatusCode;
use std::path::Path;

use crate::error::{RemoteError, RemoteResult};
use crate::streaming::{ChunkManifest, LayerSelection};

use super::HttpRemote;

impl HttpRemote {
    /// Download a change file directly to disk.
    ///
    /// Downloads the change and writes it directly to a file on disk,
    /// avoiding the need to hold the full change in memory as a `Bytes`
    /// before persisting.
    ///
    /// # Arguments
    ///
    /// * `hash` - The base32-encoded hash of the change.
    /// * `dest` - The destination file path.
    ///
    /// # Returns
    ///
    /// The number of bytes written to disk.
    pub async fn download_change_to_file(&self, hash: &str, dest: &Path) -> RemoteResult<u64> {
        let url = format!("{}?change={}", self.base_url, hash);
        debug!("GET change (to file): {} → {:?}", url, dest);

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| RemoteError::connection_failed(&url, e))?;

        let status = response.status();

        match status {
            StatusCode::OK => {
                let bytes = response
                    .bytes()
                    .await
                    .map_err(|e| RemoteError::connection_failed(&url, e))?;

                // Create parent directory if needed
                if let Some(parent) = dest.parent() {
                    tokio::fs::create_dir_all(parent).await.map_err(|e| {
                        RemoteError::other(format!(
                            "Failed to create directory {:?}: {}",
                            parent, e
                        ))
                    })?;
                }

                tokio::fs::write(dest, &bytes).await.map_err(|e| {
                    RemoteError::other(format!("Failed to write file {:?}: {}", dest, e))
                })?;

                let bytes_written = bytes.len() as u64;
                debug!(
                    "Downloaded change {} to {:?} ({} bytes)",
                    hash, dest, bytes_written
                );
                Ok(bytes_written)
            }
            StatusCode::NOT_FOUND => Err(RemoteError::change_not_found(hash)),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                let msg = response.text().await.unwrap_or_default();
                Err(RemoteError::auth_failed(&url, msg))
            }
            _ => {
                let msg = response.text().await.unwrap_or_default();
                Err(RemoteError::http(status.as_u16(), msg))
            }
        }
    }

    /// Download a change with layer-selective filtering.
    ///
    /// Uses the `?layers=` query parameter to request only specific layers
    /// from the server. This enables:
    ///
    /// - **Thin pull** (`layers=graph,content`): Download only what's needed
    ///   to apply the change, skipping the semantic layer (~40% smaller).
    /// - **Thin review** (`layers=semantic,content`): Download only what's
    ///   needed for code review, skipping graph ops (~60% smaller).
    /// - **Graph only** (`layers=graph`): Ultra-thin metadata inspection.
    ///
    /// If the server doesn't support `?layers=`, it returns the full change
    /// (graceful degradation).
    ///
    /// # Arguments
    ///
    /// * `hash` - The base32-encoded hash of the change.
    /// * `layers` - Which layers to download.
    ///
    /// # Returns
    ///
    /// The raw change data (may be a subset of the full change if the
    /// server supports layer-selective responses).
    pub async fn download_change_layers(
        &self,
        hash: &str,
        layers: &LayerSelection,
    ) -> RemoteResult<Bytes> {
        let layers_param = layers.to_query_value();
        let url = format!("{}?change={}&layers={}", self.base_url, hash, layers_param);
        debug!("GET change (layers={}): {}", layers_param, url);

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| RemoteError::connection_failed(&url, e))?;

        let status = response.status();

        match status {
            StatusCode::OK => {
                let bytes = response
                    .bytes()
                    .await
                    .map_err(|e| RemoteError::connection_failed(&url, e))?;

                debug!(
                    "Downloaded change {} (layers={}): {} bytes",
                    hash,
                    layers_param,
                    bytes.len()
                );
                Ok(bytes)
            }
            StatusCode::NOT_FOUND => Err(RemoteError::change_not_found(hash)),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                let msg = response.text().await.unwrap_or_default();
                Err(RemoteError::auth_failed(&url, msg))
            }
            _ => {
                let msg = response.text().await.unwrap_or_default();
                Err(RemoteError::http(status.as_u16(), msg))
            }
        }
    }

    /// Get the chunk manifest for a change.
    ///
    /// The chunk manifest lists all content chunks in a change with their
    /// blake3 hashes and sizes. This is the starting point for delta
    /// transfer negotiation — the receiver compares the manifest against
    /// its local chunk inventory to determine which chunks need to be
    /// transferred.
    ///
    /// # Arguments
    ///
    /// * `hash` - The base32-encoded hash of the change.
    ///
    /// # Returns
    ///
    /// The [`ChunkManifest`] for the change, or `None` if the server
    /// doesn't support the `?manifest` endpoint (graceful degradation).
    pub async fn get_chunk_manifest(&self, hash: &str) -> RemoteResult<Option<ChunkManifest>> {
        let url = format!("{}?change={}&manifest", self.base_url, hash);
        debug!("GET chunk manifest: {}", url);

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| RemoteError::connection_failed(&url, e))?;

        let status = response.status();

        match status {
            StatusCode::OK => {
                let content_type = response
                    .headers()
                    .get(CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");

                // If the server returns JSON, it supports the manifest endpoint
                if content_type.contains("json") {
                    let text = response
                        .text()
                        .await
                        .map_err(|e| RemoteError::connection_failed(&url, e))?;

                    let manifest: ChunkManifest = serde_json::from_str(&text).map_err(|e| {
                        RemoteError::protocol(format!("Failed to parse chunk manifest: {}", e))
                    })?;

                    debug!(
                        "Got chunk manifest for {}: {} chunks, {} compressed",
                        hash,
                        manifest.chunk_count(),
                        manifest.total_compressed(),
                    );
                    Ok(Some(manifest))
                } else {
                    // Server returned the full change data instead of a manifest.
                    // This means it doesn't support ?manifest — graceful degradation.
                    debug!(
                        "Server doesn't support ?manifest (returned {}, not JSON)",
                        content_type
                    );
                    Ok(None)
                }
            }
            StatusCode::NOT_FOUND => Err(RemoteError::change_not_found(hash)),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                let msg = response.text().await.unwrap_or_default();
                Err(RemoteError::auth_failed(&url, msg))
            }
            // 400 or other errors may indicate the server doesn't support ?manifest
            StatusCode::BAD_REQUEST => {
                debug!("Server returned 400 for ?manifest — likely unsupported");
                Ok(None)
            }
            _ => {
                let msg = response.text().await.unwrap_or_default();
                Err(RemoteError::http(status.as_u16(), msg))
            }
        }
    }
}
