//! Query and download methods for `HttpRemote`.
//!
//! This module contains GET-based operations: state queries, changelist
//! retrieval, ID lookups, and change/tag/attestation/provenance downloads.
//!
//! Streaming V3 download methods (layer-selective downloads, file-to-disk,
//! chunk manifests) are in the sibling [`super::download`] module.

use bytes::Bytes;
use log::{debug, trace};
use reqwest::StatusCode;

use crate::error::{RemoteError, RemoteResult};
use crate::types::{ChangelistEntry, StateResponse};

use super::{parse_changelist, HttpRemote};

impl HttpRemote {
    /// Get the current state of a view.
    ///
    /// # Arguments
    ///
    /// * `view` - The name of the view to query.
    ///
    /// # Returns
    ///
    /// The current state of the view, or `StateResponse::Empty` if the
    /// view is empty.
    pub async fn get_state(&self, view: &str) -> RemoteResult<StateResponse> {
        let url = format!("{}?view={}&state=", self.base_url, view);
        debug!("GET state: {}", url);

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| RemoteError::connection_failed(&url, e))?;

        crate::check_min_version_header(response.headers());
        let status = response.status();
        trace!("GET state response status: {}", status);

        match status {
            StatusCode::OK => {
                let text = response
                    .text()
                    .await
                    .map_err(|e| RemoteError::connection_failed(&url, e))?;

                trace!("GET state response body: {:?}", text);

                StateResponse::parse(&text).map_err(|e| {
                    RemoteError::protocol(format!("Failed to parse state response: {}", e))
                })
            }
            StatusCode::NOT_FOUND => Err(RemoteError::view_not_found(view)),
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

    /// Get the changelist for a view starting from a position.
    ///
    /// # Arguments
    ///
    /// * `view` - The name of the view to query.
    /// * `from` - The starting position (sequence number).
    ///
    /// # Returns
    ///
    /// A vector of changelist entries, starting from the given position.
    pub async fn get_changelist(
        &self,
        view: &str,
        from: u64,
    ) -> RemoteResult<Vec<ChangelistEntry>> {
        let url = format!("{}?view={}&changelist={}", self.base_url, view, from);
        debug!("GET changelist: {}", url);

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| RemoteError::connection_failed(&url, e))?;

        crate::check_min_version_header(response.headers());
        let status = response.status();
        trace!("GET changelist response status: {}", status);

        match status {
            StatusCode::OK => {
                let text = response
                    .text()
                    .await
                    .map_err(|e| RemoteError::connection_failed(&url, e))?;

                trace!("GET changelist response body length: {} bytes", text.len());

                parse_changelist(&text)
            }
            StatusCode::NOT_FOUND => Err(RemoteError::view_not_found(view)),
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

    /// Get the view ID (UUID).
    ///
    /// # Arguments
    ///
    /// * `view` - The name of the view to query.
    ///
    /// # Returns
    ///
    /// The view's UUID as a string.
    pub async fn get_id(&self, view: &str) -> RemoteResult<String> {
        let url = format!("{}?view={}&id", self.base_url, view);
        debug!("GET id: {}", url);

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| RemoteError::connection_failed(&url, e))?;

        crate::check_min_version_header(response.headers());
        let status = response.status();

        match status {
            StatusCode::OK => {
                let text = response
                    .text()
                    .await
                    .map_err(|e| RemoteError::connection_failed(&url, e))?;

                Ok(text.trim().to_string())
            }
            StatusCode::NOT_FOUND => Err(RemoteError::view_not_found(view)),
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

    /// Download a change file by hash.
    ///
    /// # Arguments
    ///
    /// * `hash` - The base32-encoded hash of the change.
    ///
    /// # Returns
    ///
    /// The raw change file data.
    pub async fn download_change(&self, hash: &str) -> RemoteResult<Bytes> {
        let url = format!("{}?change={}", self.base_url, hash);
        debug!("GET change: {}", url);

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| RemoteError::connection_failed(&url, e))?;

        crate::check_min_version_header(response.headers());
        let status = response.status();

        match status {
            StatusCode::OK => {
                let bytes = response
                    .bytes()
                    .await
                    .map_err(|e| RemoteError::connection_failed(&url, e))?;

                debug!("Downloaded change {}: {} bytes", hash, bytes.len());
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

    /// Download a tag by state (short format).
    ///
    /// # Arguments
    ///
    /// * `state` - The base32-encoded state merkle.
    ///
    /// # Returns
    ///
    /// The short tag data (without the length prefix).
    pub async fn download_tag(&self, state: &str) -> RemoteResult<Bytes> {
        let url = format!("{}?tag={}", self.base_url, state);
        debug!("GET tag: {}", url);

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| RemoteError::connection_failed(&url, e))?;

        crate::check_min_version_header(response.headers());
        let status = response.status();

        match status {
            StatusCode::OK => {
                let bytes = response
                    .bytes()
                    .await
                    .map_err(|e| RemoteError::connection_failed(&url, e))?;

                // The response format is: {8-byte length BE}{short_data}
                // We need to skip the length prefix
                if bytes.len() < 8 {
                    return Err(RemoteError::protocol(
                        "Tag response too short (missing length prefix)".to_string(),
                    ));
                }

                let length = u64::from_be_bytes([
                    bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
                ]) as usize;

                if bytes.len() < 8 + length {
                    return Err(RemoteError::protocol(format!(
                        "Tag response truncated: expected {} bytes, got {}",
                        length,
                        bytes.len() - 8
                    )));
                }

                debug!("Downloaded tag {}: {} bytes", state, length);
                Ok(bytes.slice(8..8 + length))
            }
            StatusCode::NOT_FOUND => Err(RemoteError::tag_not_found(state)),
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

    /// List tag content hashes for a remote view.
    ///
    /// Returns `(hash, name)` pairs for all tags in the view.
    /// The client can then download each tag individually with
    /// [`download_tag`](Self::download_tag).
    pub async fn list_remote_tags(&self, view: &str) -> RemoteResult<Vec<(String, String)>> {
        let url = format!("{}?tags={}", self.base_url, view);
        debug!("GET tags: {}", url);

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| RemoteError::connection_failed(&url, e))?;

        crate::check_min_version_header(response.headers());
        let status = response.status();

        match status {
            StatusCode::OK => {
                let body = response
                    .text()
                    .await
                    .map_err(|e| RemoteError::connection_failed(&url, e))?;

                let mut tags = Vec::new();
                for line in body.lines() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    // Format: "HASH NAME"
                    if let Some((hash, name)) = line.split_once(' ') {
                        tags.push((hash.to_string(), name.to_string()));
                    }
                }

                debug!("Found {} remote tags for view '{}'", tags.len(), view);
                Ok(tags)
            }
            StatusCode::NOT_FOUND => {
                // View doesn't exist or no tags — return empty
                Ok(Vec::new())
            }
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

    /// Download an attestation from the remote server.
    ///
    /// # Arguments
    ///
    /// * `hash` - The base32-encoded hash of the attestation.
    ///
    /// # Returns
    ///
    /// The raw attestation bytes.
    pub async fn download_attestation(&self, hash: &str) -> RemoteResult<Bytes> {
        let url = format!("{}?attest={}", self.base_url, hash);
        debug!("GET attest: {}", url);

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| RemoteError::connection_failed(&url, e))?;

        crate::check_min_version_header(response.headers());
        let status = response.status();

        match status {
            StatusCode::OK => {
                let data = response
                    .bytes()
                    .await
                    .map_err(|e| RemoteError::connection_failed(&url, e))?;
                debug!("Downloaded attestation {} ({} bytes)", hash, data.len());
                Ok(data)
            }
            StatusCode::NOT_FOUND => Err(RemoteError::protocol(format!(
                "Attestation not found: {}",
                hash
            ))),
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

    /// List all provenance graph hashes the remote holds.
    ///
    /// This mirrors the `.change` sync model: the server advertises a flat
    /// inventory of content-addressed objects, the client computes the delta
    /// by presence (`has_provenance_graph`), and pulls missing objects by
    /// hash via [`download_provenance`](Self::download_provenance).
    ///
    /// Deliberately NOT a per-change relationship query ("which provenance
    /// explains change X?"): that would require the server to maintain a
    /// live reverse-dependency index, coupling provenance registration to
    /// change arrival order. With a flat inventory, provenance graphs are
    /// self-certifying objects — relationship resolution (DEPS) happens
    /// locally, best-effort, exactly as with changes.
    ///
    /// Servers that do not support the extension return an error status —
    /// callers should treat that as "provenance sync unsupported" and
    /// degrade gracefully, not fail.
    ///
    /// # Returns
    ///
    /// Base32-encoded provenance graph hashes, one per line, in server order.
    pub async fn get_provenance_list(&self) -> RemoteResult<Vec<String>> {
        let url = format!("{}?provenance-list", self.base_url);
        debug!("GET provenance-list: {}", url);

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| RemoteError::connection_failed(&url, e))?;

        crate::check_min_version_header(response.headers());
        let status = response.status();

        match status {
            StatusCode::OK => {
                let text = response
                    .text()
                    .await
                    .map_err(|e| RemoteError::connection_failed(&url, e))?;
                Ok(text
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(|line| line.to_string())
                    .collect())
            }
            StatusCode::NOT_FOUND => Ok(Vec::new()),
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

    /// Download a provenance graph from the remote server.
    ///
    /// # Arguments
    ///
    /// * `hash` - The base32-encoded hash of the provenance graph.
    ///
    /// # Returns
    ///
    /// The raw provenance graph bytes.
    pub async fn download_provenance(&self, hash: &str) -> RemoteResult<Bytes> {
        let url = format!("{}?provenance={}", self.base_url, hash);
        debug!("GET provenance: {}", url);

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| RemoteError::connection_failed(&url, e))?;

        crate::check_min_version_header(response.headers());
        let status = response.status();

        match status {
            StatusCode::OK => {
                let data = response
                    .bytes()
                    .await
                    .map_err(|e| RemoteError::connection_failed(&url, e))?;
                debug!(
                    "Downloaded provenance graph {} ({} bytes)",
                    hash,
                    data.len()
                );
                Ok(data)
            }
            StatusCode::NOT_FOUND => Err(RemoteError::protocol(format!(
                "Provenance graph not found: {}",
                hash
            ))),
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
}
