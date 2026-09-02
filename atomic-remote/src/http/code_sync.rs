//! Client transport for the single `/code` git-shaped sync protocol.
//!
//! This is the client counterpart to the server's `/code` handler: all of
//! `atomic push`/`pull`/`clone` move objects and refs through **one** endpoint —
//! the remote's `base_url` (`…/projects/{slug}/code`) — using the header-
//! negotiated binary [`SyncPack`]/[`SyncWants`] format from [`atomic_objects`].
//! It replaces the per-object REST calls (`put_object`, `get_object`,
//! `put_view_ref`, …) for transport; those RESTful URLs remain only as
//! read-only WebUI surfaces.
//!
//! - [`HttpRemote::sync_push`] `POST`s a [`SyncPack`] (objects + ref CAS moves).
//! - [`HttpRemote::sync_pull`] `GET`s with a [`SyncWants`] body and decodes the
//!   returned [`SyncPack`] (the objects the client is missing + ref targets).

use atomic_objects::{
    SyncError, SyncPack, SyncWants, PROTOCOL_HEADER, PROTOCOL_SYNC_V1, SYNC_MEDIA_TYPE,
};
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
    /// [`RemoteError::ProtocolError`] carrying the server's message, so the caller
    /// can tell the user to pull first.
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
        // Capture compatibility metadata before consuming the body. A server
        // that predates sync/1 returns its JSON project-info response here,
        // while a current server promises the binary format via Content-Type.
        let min_cli_version = crate::read_min_version_header(response.headers());
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        match response.status() {
            StatusCode::OK => {
                let bytes = response
                    .bytes()
                    .await
                    .map_err(|e| RemoteError::connection_failed(&url, e))?;
                SyncPack::decode(&bytes).map_err(|e| {
                    let explicitly_legacy_media_type =
                        content_type.as_deref().is_some_and(is_legacy_media_type);
                    match e {
                        SyncError::Compression(_) | SyncError::Codec(_)
                            if explicitly_legacy_media_type =>
                        {
                            RemoteError::version_mismatch(
                                crate::VERSION,
                                min_cli_version,
                                e.to_string(),
                            )
                        }
                        SyncError::Compression(_) | SyncError::Codec(_) => {
                            RemoteError::protocol(format!(
                                "remote returned invalid data for protocol {PROTOCOL_SYNC_V1}: \
                                 {e}. Retry the operation; if it keeps failing, contact the \
                                 storage owner."
                            ))
                        }
                        SyncError::TooLarge { .. } => {
                            RemoteError::protocol(format!("failed to decode sync pack: {e}"))
                        }
                    }
                })
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

/// Recognize the JSON response returned by servers that predate `sync/1`.
fn is_legacy_media_type(content_type: &str) -> bool {
    let media_type = content_type
        .split(';')
        .next()
        .map(str::trim)
        .unwrap_or_default();
    media_type.eq_ignore_ascii_case("application/json")
        || media_type
            .rsplit_once('+')
            .is_some_and(|(_, suffix)| suffix.eq_ignore_ascii_case("json"))
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    use super::*;

    fn serve_once(body: Vec<u8>, content_type: Option<&'static str>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().expect("test server address");
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut request = [0_u8; 8192];
            let _ = stream.read(&mut request);
            let content_type_header = content_type
                .map(|value| format!("Content-Type: {value}\r\n"))
                .unwrap_or_default();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\n{content_type_header}X-Atomic-Min-Version: 0.16.2\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .expect("write response head");
            stream.write_all(&body).expect("write response body");
        });
        format!("http://{addr}/workspaces/w/projects/p/code")
    }

    async fn pull_from(
        body: Vec<u8>,
        content_type: Option<&'static str>,
    ) -> RemoteResult<SyncPack> {
        HttpRemote::new(&serve_once(body, content_type))?
            .sync_pull(&SyncWants::default())
            .await
    }

    #[tokio::test]
    async fn legacy_json_response_reports_version_mismatch() {
        let err = pull_from(br#"{"success":true}"#.to_vec(), Some("application/json"))
            .await
            .unwrap_err();
        assert!(matches!(
            &err,
            RemoteError::VersionMismatch {
                server_version: Some(version),
                ..
            } if version == "0.16.2"
        ));
        assert_eq!(
            err.to_string(),
            format!(
                "Failed to decode data from remote — the client and server may be running \
                 incompatible versions.\n  \
                 Client version: {}\n  \
                 Server min-version: 0.16.2\n  \
                 Hint: run 'atomic update' to upgrade, or ask the storage owner for their server version.",
                crate::VERSION
            )
        );
    }

    #[tokio::test]
    async fn corrupt_current_protocol_response_is_not_a_version_mismatch() {
        let err = pull_from(b"not a zstd frame".to_vec(), Some(SYNC_MEDIA_TYPE))
            .await
            .unwrap_err();
        assert!(matches!(&err, RemoteError::ProtocolError { .. }));
        assert!(err.to_string().contains("Unknown frame descriptor"));
    }

    #[tokio::test]
    async fn corrupt_generic_binary_response_is_not_a_version_mismatch() {
        let err = pull_from(
            b"not a zstd frame".to_vec(),
            Some("application/octet-stream"),
        )
        .await
        .unwrap_err();
        assert!(matches!(&err, RemoteError::ProtocolError { .. }));
    }

    #[tokio::test]
    async fn invalid_postcard_from_current_protocol_is_not_a_version_mismatch() {
        let body = atomic_objects::encode(&u64::MAX).unwrap();
        let err = pull_from(body, Some(SYNC_MEDIA_TYPE)).await.unwrap_err();
        assert!(matches!(&err, RemoteError::ProtocolError { .. }));
        assert!(err.to_string().contains("sync codec error"));
    }

    #[tokio::test]
    async fn invalid_postcard_from_legacy_media_type_reports_version_mismatch() {
        let body = atomic_objects::encode(&u64::MAX).unwrap();
        let err = pull_from(body, Some("application/json")).await.unwrap_err();
        assert!(matches!(&err, RemoteError::VersionMismatch { .. }));
    }

    #[tokio::test]
    async fn corrupt_unlabelled_response_is_not_a_version_mismatch() {
        let err = pull_from(b"not a zstd frame".to_vec(), None)
            .await
            .unwrap_err();
        assert!(matches!(err, RemoteError::ProtocolError { .. }));
    }

    #[tokio::test]
    async fn valid_sync_pack_remains_compatible_with_generic_media_type() {
        let pack = pull_from(
            SyncPack::empty().encode().unwrap(),
            Some("application/octet-stream"),
        )
        .await
        .unwrap();
        assert!(pack.is_empty());
    }

    #[test]
    fn legacy_media_type_allows_parameters_case_and_json_suffix() {
        assert!(is_legacy_media_type("application/json"));
        assert!(is_legacy_media_type(
            "Application/Problem+Json; charset=utf-8"
        ));
        assert!(!is_legacy_media_type(SYNC_MEDIA_TYPE));
        assert!(!is_legacy_media_type("application/octet-stream"));
    }
}
