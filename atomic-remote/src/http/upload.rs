//! Upload and mutation methods for `HttpRemote`.
//!
//! All POST-based operations: uploading changes, tags, attestations,
//! provenance graphs, and forking views.

use bytes::Bytes;
use log::{debug, info};
use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE};
use reqwest::StatusCode;
use std::path::Path;
use tokio_util::io::ReaderStream;

use crate::error::{RemoteError, RemoteResult};

use super::HttpRemote;

impl HttpRemote {
    /// Upload a change file.
    ///
    /// # Arguments
    ///
    /// * `hash` - The base32-encoded hash of the change.
    /// * `view` - The target view name.
    /// * `data` - The raw change file data.
    pub async fn upload_change(&self, hash: &str, view: &str, data: Bytes) -> RemoteResult<()> {
        let url = format!("{}?insert={}&view={}", self.base_url, hash, view);
        debug!("POST insert: {} ({} bytes)", url, data.len());

        let response = self
            .client
            .post(&url)
            .header(CONTENT_TYPE, "application/octet-stream")
            .body(data)
            .send()
            .await
            .map_err(|e| RemoteError::connection_failed(&url, e))?;

        let status = response.status();

        match status {
            StatusCode::OK => {
                debug!("Successfully uploaded change {}", hash);
                Ok(())
            }
            StatusCode::NOT_FOUND => Err(RemoteError::repo_not_found(&url)),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                let msg = response.text().await.unwrap_or_default();
                Err(RemoteError::auth_failed(&url, msg))
            }
            StatusCode::BAD_REQUEST | StatusCode::INTERNAL_SERVER_ERROR => {
                let msg = response.text().await.unwrap_or_default();
                // Check if it's a missing dependencies error
                if msg.contains("missing") && msg.contains("dependenc") {
                    // Try to extract hash list from error message
                    Err(RemoteError::missing_deps(vec![]))
                } else {
                    Err(RemoteError::http(status.as_u16(), msg))
                }
            }
            _ => {
                let msg = response.text().await.unwrap_or_default();
                Err(RemoteError::http(status.as_u16(), msg))
            }
        }
    }

    /// Upload a tag (short format).
    ///
    /// # Arguments
    ///
    /// * `state` - The base32-encoded state merkle.
    /// * `view` - The target view name.
    /// * `short_data` - The short tag data.
    pub async fn upload_tag(&self, state: &str, view: &str, short_data: Bytes) -> RemoteResult<()> {
        let url = format!("{}?tagup={}&view={}", self.base_url, state, view);
        debug!("POST tagup: {} ({} bytes)", url, short_data.len());

        let response = self
            .client
            .post(&url)
            .header(CONTENT_TYPE, "application/octet-stream")
            .body(short_data)
            .send()
            .await
            .map_err(|e| RemoteError::connection_failed(&url, e))?;

        let status = response.status();

        match status {
            StatusCode::OK => {
                debug!("Successfully uploaded tag for state {}", state);
                Ok(())
            }
            StatusCode::NOT_FOUND => Err(RemoteError::view_not_found(view)),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                let msg = response.text().await.unwrap_or_default();
                Err(RemoteError::auth_failed(&url, msg))
            }
            StatusCode::BAD_REQUEST | StatusCode::INTERNAL_SERVER_ERROR => {
                let msg = response.text().await.unwrap_or_default();
                // Check if it's a state mismatch error
                if msg.contains("Wrong state") || msg.contains("state mismatch") {
                    Err(RemoteError::state_mismatch("unknown", state))
                } else {
                    Err(RemoteError::http(status.as_u16(), msg))
                }
            }
            _ => {
                let msg = response.text().await.unwrap_or_default();
                Err(RemoteError::http(status.as_u16(), msg))
            }
        }
    }

    /// Fork a view on the remote server.
    ///
    /// Creates a new view from an existing one. This is a
    /// lightweight server-side operation — no data is duplicated. The
    /// new view's changelog is copied from the source view in a single
    /// transaction.
    ///
    /// # Arguments
    ///
    /// * `target_view` - The name of the new view to create.
    /// * `source_view` - The name of the existing view to fork from.
    ///
    /// # Returns
    ///
    /// The number of changes adopted into the new view.
    pub async fn fork_view(&self, target_view: &str, source_view: &str) -> RemoteResult<u64> {
        let url = format!(
            "{}?fork_from={}&view={}",
            self.base_url, source_view, target_view
        );
        debug!("POST fork: {}", url);

        let response = self
            .client
            .post(&url)
            .send()
            .await
            .map_err(|e| RemoteError::connection_failed(&url, e))?;

        let status = response.status();

        match status {
            StatusCode::OK => {
                let text = response
                    .text()
                    .await
                    .map_err(|e| RemoteError::connection_failed(&url, e))?;

                // Parse the JSON response to extract the change count
                let parsed: serde_json::Value = serde_json::from_str(&text)
                    .map_err(|e| RemoteError::protocol(format!("Invalid fork response: {}", e)))?;

                let changes = parsed.get("changes").and_then(|v| v.as_u64()).unwrap_or(0);

                debug!(
                    "Forked view '{}' from '{}' ({} changes)",
                    target_view, source_view, changes
                );

                Ok(changes)
            }
            StatusCode::BAD_REQUEST => {
                let msg = response.text().await.unwrap_or_default();
                Err(RemoteError::protocol(msg))
            }
            StatusCode::NOT_FOUND => Err(RemoteError::view_not_found(source_view)),
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

    /// Upload an attestation to the remote server.
    ///
    /// Attestations are graph-level audit nodes that capture metadata about
    /// a set of changes (cost, tokens, model usage, duration). They are
    /// stored separately from changes and don't modify the content graph.
    ///
    /// # Arguments
    ///
    /// * `hash` - The base32-encoded hash of the attestation.
    /// * `data` - The raw serialized attestation bytes (.attest format).
    pub async fn upload_attestation(&self, hash: &str, data: Bytes) -> RemoteResult<()> {
        let url = format!("{}?attest={}", self.base_url, hash);
        debug!("POST attest: {} ({} bytes)", url, data.len());

        let response = self
            .client
            .post(&url)
            .header(CONTENT_TYPE, "application/octet-stream")
            .body(data)
            .send()
            .await
            .map_err(|e| RemoteError::connection_failed(&url, e))?;

        let status = response.status();

        match status {
            StatusCode::OK => {
                debug!("Successfully uploaded attestation {}", hash);
                Ok(())
            }
            StatusCode::NOT_FOUND => Err(RemoteError::repo_not_found(&url)),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                let msg = response.text().await.unwrap_or_default();
                Err(RemoteError::auth_failed(&url, msg))
            }
            StatusCode::BAD_REQUEST => {
                let msg = response.text().await.unwrap_or_default();
                Err(RemoteError::protocol(format!(
                    "Failed to upload attestation: {}",
                    msg
                )))
            }
            _ => {
                let msg = response.text().await.unwrap_or_default();
                Err(RemoteError::http(status.as_u16(), msg))
            }
        }
    }

    /// Upload a provenance graph to the remote server.
    ///
    /// Provenance graphs are content-addressed DAGs that capture the causal
    /// decision chain of an AI agent session. They are stored separately
    /// from changes and don't modify the content graph.
    ///
    /// # Arguments
    ///
    /// * `hash` - The base32-encoded hash of the provenance graph.
    /// * `data` - The raw serialized provenance graph bytes (.provenance format).
    pub async fn upload_provenance(&self, hash: &str, data: Bytes) -> RemoteResult<()> {
        let url = format!("{}?provenance={}", self.base_url, hash);
        debug!("POST provenance: {} ({} bytes)", url, data.len());

        let response = self
            .client
            .post(&url)
            .header(CONTENT_TYPE, "application/octet-stream")
            .body(data)
            .send()
            .await
            .map_err(|e| RemoteError::connection_failed(&url, e))?;

        let status = response.status();

        match status {
            StatusCode::OK => {
                debug!("Successfully uploaded provenance graph {}", hash);
                Ok(())
            }
            StatusCode::NOT_FOUND => Err(RemoteError::repo_not_found(&url)),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                let msg = response.text().await.unwrap_or_default();
                Err(RemoteError::auth_failed(&url, msg))
            }
            StatusCode::BAD_REQUEST => {
                let msg = response.text().await.unwrap_or_default();
                Err(RemoteError::protocol(format!(
                    "Failed to upload provenance graph: {}",
                    msg
                )))
            }
            _ => {
                let msg = response.text().await.unwrap_or_default();
                Err(RemoteError::http(status.as_u16(), msg))
            }
        }
    }

    // Streaming V3 Protocol — Upload Methods

    /// Upload a change file by streaming from disk.
    ///
    /// Uses reqwest's streaming body support to avoid loading the entire file
    /// into memory.  The `Content-Length` header is set from file metadata so
    /// the server can validate the upload without buffering.
    ///
    /// # Arguments
    ///
    /// * `hash` - The base32-encoded hash of the change.
    /// * `view` - The target view name.
    /// * `path` - Path to the `.change` file on disk.
    ///
    /// # Errors
    ///
    /// Returns an error if the file can't be opened, the upload fails, or
    /// the server rejects the change.
    pub async fn upload_change_streamed(
        &self,
        hash: &str,
        view: &str,
        path: &std::path::Path,
    ) -> RemoteResult<()> {
        let url = format!("{}?insert={}&view={}", self.base_url, hash, view);
        debug!("POST insert (streamed): {} from {:?}", url, path);

        let file = tokio::fs::File::open(path).await.map_err(|e| {
            RemoteError::other(format!("Failed to open change file {:?}: {}", path, e))
        })?;

        let file_size = file
            .metadata()
            .await
            .map_err(|e| {
                RemoteError::other(format!("Failed to read metadata for {:?}: {}", path, e))
            })?
            .len();

        info!("Streaming upload of change {} ({} bytes)", hash, file_size);

        let stream = ReaderStream::new(file);
        let body = reqwest::Body::wrap_stream(stream);

        let response = self
            .client
            .post(&url)
            .header(CONTENT_TYPE, "application/octet-stream")
            .header(CONTENT_LENGTH, file_size)
            .body(body)
            .send()
            .await
            .map_err(|e| RemoteError::connection_failed(&url, e))?;

        let status = response.status();

        match status {
            StatusCode::OK => {
                debug!("Successfully streamed change {} from file", hash);
                Ok(())
            }
            StatusCode::NOT_FOUND => Err(RemoteError::repo_not_found(&url)),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                let msg = response.text().await.unwrap_or_default();
                Err(RemoteError::auth_failed(&url, msg))
            }
            StatusCode::BAD_REQUEST | StatusCode::INTERNAL_SERVER_ERROR => {
                let msg = response.text().await.unwrap_or_default();
                if msg.contains("missing") && msg.contains("dependenc") {
                    Err(RemoteError::missing_deps(vec![]))
                } else {
                    Err(RemoteError::http(status.as_u16(), msg))
                }
            }
            _ => {
                let msg = response.text().await.unwrap_or_default();
                Err(RemoteError::http(status.as_u16(), msg))
            }
        }
    }

    /// Upload a change file by reading directly from disk.
    ///
    /// This is a convenience wrapper around [`upload_change_streamed`](Self::upload_change_streamed)
    /// that streams the file from disk instead of loading it entirely into
    /// memory. Suitable for change files of any size.
    ///
    /// # Arguments
    ///
    /// * `hash` - The base32-encoded hash of the change.
    /// * `view` - The target view name.
    /// * `path` - Path to the `.change` file on disk.
    ///
    /// # Errors
    ///
    /// Returns an error if the file can't be read, the upload fails, or
    /// the server rejects the change.
    pub async fn upload_change_file(
        &self,
        hash: &str,
        view: &str,
        path: &Path,
    ) -> RemoteResult<()> {
        self.upload_change_streamed(hash, view, path).await
    }
}
