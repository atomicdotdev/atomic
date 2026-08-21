//! Client transport for the single `/code` git-shaped sync protocol.
//!
//! This is the client counterpart to the server's `/code` handler: all of
//! `atomic push`/`pull`/`clone` move objects and refs through **one** endpoint —
//! the remote's `base_url` (`…/projects/{slug}/code`) — using the header-
//! negotiated binary [`SyncPack`]/[`SyncWants`] format from [`atomic_objects`].
//! It replaces the per-object [`super::bare`] REST calls (`put_object`,
//! `get_object`, `put_view_ref`, …) for transport; those RESTful URLs remain
//! only as read-only WebUI surfaces.
//!
//! - [`HttpRemote::sync_push`] `POST`s a [`SyncPack`] (objects + ref CAS moves).
//! - [`HttpRemote::sync_pull`] `GET`s with a [`SyncWants`] body and decodes the
//!   returned [`SyncPack`] (the objects the client is missing + ref targets).

use atomic_objects::{SyncPack, SyncWants, PROTOCOL_HEADER, PROTOCOL_SYNC_V1, SYNC_MEDIA_TYPE};
use reqwest::header::CONTENT_TYPE;
use reqwest::StatusCode;
use serde::Deserialize;

use super::HttpRemote;
use crate::error::{RemoteError, RemoteResult};

/// The server's push summary (`{ "stored", "refs_moved" }`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PushSummary {
    /// Number of objects stored (or already present).
    #[serde(default)]
    pub stored: usize,
    /// Number of view refs moved by the CAS batch.
    #[serde(default)]
    pub refs_moved: usize,
}

impl HttpRemote {
    /// Push a [`SyncPack`] to the remote's `/code` endpoint.
    ///
    /// A `409 Conflict` (a divergent, non-fast-forward ref move) surfaces as a
    /// [`RemoteError::Protocol`] carrying the server's message, so the caller can
    /// tell the user to pull first.
    pub async fn sync_push(&self, pack: &SyncPack) -> RemoteResult<PushSummary> {
        let url = self.base_url.as_str().to_string();
        let body = pack
            .encode()
            .map_err(|e| RemoteError::protocol(format!("failed to encode sync pack: {e}")))?;
        let response = self
            .client
            .post(&url)
            .header(PROTOCOL_HEADER, PROTOCOL_SYNC_V1)
            .header(CONTENT_TYPE, SYNC_MEDIA_TYPE)
            .body(body)
            .send()
            .await
            .map_err(|e| RemoteError::connection_failed(&url, e))?;
        crate::check_min_version_header(response.headers());
        match response.status() {
            StatusCode::OK | StatusCode::CREATED => {
                Ok(response.json::<PushSummary>().await.unwrap_or_default())
            }
            StatusCode::CONFLICT => Err(RemoteError::protocol(
                response.text().await.unwrap_or_default(),
            )),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(RemoteError::auth_failed(
                &url,
                response.text().await.unwrap_or_default(),
            )),
            s => Err(RemoteError::http(
                s.as_u16(),
                response.text().await.unwrap_or_default(),
            )),
        }
    }

    /// Pull from the remote's `/code` endpoint: send `wants` (which views, and
    /// the object keys already held) and decode the returned [`SyncPack`] of
    /// missing objects + current ref targets. An empty [`SyncWants::refs`] asks
    /// for every view (a full clone).
    pub async fn sync_pull(&self, wants: &SyncWants) -> RemoteResult<SyncPack> {
        let url = self.base_url.as_str().to_string();
        let body = wants
            .encode()
            .map_err(|e| RemoteError::protocol(format!("failed to encode sync wants: {e}")))?;
        let response = self
            .client
            .get(&url)
            .header(PROTOCOL_HEADER, PROTOCOL_SYNC_V1)
            .header(CONTENT_TYPE, SYNC_MEDIA_TYPE)
            .body(body)
            .send()
            .await
            .map_err(|e| RemoteError::connection_failed(&url, e))?;
        crate::check_min_version_header(response.headers());
        match response.status() {
            StatusCode::OK => {
                let bytes = response
                    .bytes()
                    .await
                    .map_err(|e| RemoteError::connection_failed(&url, e))?;
                SyncPack::decode(&bytes)
                    .map_err(|e| RemoteError::protocol(format!("failed to decode sync pack: {e}")))
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(RemoteError::auth_failed(
                &url,
                response.text().await.unwrap_or_default(),
            )),
            s => Err(RemoteError::http(
                s.as_u16(),
                response.text().await.unwrap_or_default(),
            )),
        }
    }
}
