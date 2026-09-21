//! Client for the remaining read-only WebUI resource(s).
//!
//! Push/pull/clone transport moved to the single `/code` sync protocol (see
//! [`super::code_sync`]); the per-object `PUT`/`GET`/`HEAD` and view-ref CAS
//! calls that used to live here were retired with that cutover. What remains is
//! the one read a CLI command still needs outside `/code`:
//! [`HttpRemote::list_view_refs`], backing `atomic view list --remote`.

use log::debug;
use reqwest::StatusCode;

use super::HttpRemote;
use crate::error::{RemoteError, RemoteResult};
use crate::types::RemoteViewInfo;

impl HttpRemote {
    /// Build a sibling REST resource URL from the `/code` base URL.
    ///
    /// `https://h/workspaces/w/projects/p/code` + `refs/views` →
    /// `https://h/workspaces/w/projects/p/refs/views`.
    pub(crate) fn resource_url(&self, path: &str) -> String {
        let base = self.base_url.as_str();
        let base = base.strip_suffix('/').unwrap_or(base);
        let project_base = base.strip_suffix("/code").unwrap_or(base);
        format!("{project_base}/{path}")
    }

    /// The view inventory (`GET /refs/views`), composed on the server from the
    /// `.view` objects. Read-only WebUI/browse surface used by
    /// `atomic view list --remote`.
    pub async fn list_view_refs(&self) -> RemoteResult<Vec<RemoteViewInfo>> {
        let url = self.resource_url("refs/views");
        debug!("GET view refs: {url}");
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| RemoteError::connection_failed(&url, e))?;
        match response.status() {
            StatusCode::OK => {
                let text = response
                    .text()
                    .await
                    .map_err(|e| RemoteError::connection_failed(&url, e))?;
                text.lines()
                    .enumerate()
                    .filter(|(_, line)| !line.trim().is_empty())
                    .map(|(index, line)| {
                        RemoteViewInfo::parse_strict(line).map_err(|reason| {
                            RemoteError::protocol(format!(
                                "Invalid view inventory row {}: {}",
                                index + 1,
                                reason
                            ))
                        })
                    })
                    .collect()
            }
            StatusCode::NOT_FOUND => Err(RemoteError::repo_not_found(&url)),
            s => Err(RemoteError::http(
                s.as_u16(),
                response.text().await.unwrap_or_default(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_url_replaces_code_suffix() {
        let r = HttpRemote::new("https://h/workspaces/w/projects/p/code").unwrap();
        assert_eq!(
            r.resource_url("refs/views"),
            "https://h/workspaces/w/projects/p/refs/views"
        );
        assert_eq!(
            r.resource_url("refs/views/dev"),
            "https://h/workspaces/w/projects/p/refs/views/dev"
        );
    }

    #[test]
    fn resource_url_without_code_suffix_appends() {
        let r = HttpRemote::new("https://h/workspaces/w/projects/p").unwrap();
        assert_eq!(
            r.resource_url("refs/views"),
            "https://h/workspaces/w/projects/p/refs/views"
        );
    }

    #[test]
    fn strict_view_inventory_rejects_malformed_rows() {
        assert!(RemoteViewInfo::parse_strict("dev\tshared\t-\tnot-a-count\t-").is_err());
        assert!(RemoteViewInfo::parse_strict("dev\tunknown\t-\t0\t-").is_err());
        assert!(RemoteViewInfo::parse_strict(r#"{"workspace":"w"}"#).is_err());
    }
}
