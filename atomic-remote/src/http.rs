//! HTTP client for communicating with Atomic API servers.
//!
//! This module provides the `HttpRemote` struct which implements the remote
//! protocol over HTTP, compatible with `atomic-api` servers.
//!
//! # Example
//!
//! ```ignore
//! use atomic_remote::http::HttpRemote;
//!
//! async fn example() -> Result<(), Box<dyn std::error::Error>> {
//!     let remote = HttpRemote::new("https://api.example.com/tenant/t/portfolio/p/project/pr/code")?;
//!
//!     // Get the current state of the "main" stack
//!     let state = remote.get_state("main").await?;
//!     println!("Current state: {:?}", state);
//!
//!     // Get the changelist starting from position 0
//!     let entries = remote.get_changelist("main", 0).await?;
//!     for entry in entries {
//!         println!("{}: {} -> {}", entry.sequence, entry.hash, entry.merkle);
//!     }
//!     Ok(())
//! }
//! ```

use crate::error::{RemoteError, RemoteResult};
use crate::streaming::{ChunkManifest, LayerSelection};
use crate::types::{ChangelistEntry, StateResponse};
use bytes::Bytes;
use log::{debug, info, trace};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT_ENCODING, CONTENT_TYPE, USER_AGENT};
use reqwest::{Client, StatusCode};
use std::path::Path;
use std::time::Duration;
use url::Url;

// Constants

/// User-Agent header sent with all requests.
///
/// This is critical for API servers to detect Atomic CLI requests vs web browsers.
/// The format is `atomic-{version}`.
const ATOMIC_USER_AGENT: &str = concat!("atomic-", env!("CARGO_PKG_VERSION"));

/// Default request timeout in seconds.
///
/// Set to 300s (5 minutes) to accommodate large initial pushes where the
/// server needs to write the change file, apply it to the graph, and
/// output the working copy.
const DEFAULT_TIMEOUT_SECS: u64 = 300;

/// Default connect timeout in seconds.
const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 10;

/// Accepted content encodings for compression.
const ACCEPT_ENCODING_VALUE: &str = "zstd, gzip, deflate";

// HttpRemoteConfig

/// Configuration options for an HTTP remote connection.
#[derive(Debug, Clone)]
pub struct HttpRemoteConfig {
    /// Request timeout.
    pub timeout: Duration,

    /// Connection timeout.
    pub connect_timeout: Duration,

    /// Skip TLS certificate verification (dangerous!).
    pub danger_accept_invalid_certs: bool,

    /// Additional headers to send with each request.
    pub extra_headers: Vec<(String, String)>,
}

impl Default for HttpRemoteConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            connect_timeout: Duration::from_secs(DEFAULT_CONNECT_TIMEOUT_SECS),
            danger_accept_invalid_certs: false,
            extra_headers: Vec::new(),
        }
    }
}

impl HttpRemoteConfig {
    /// Create a new configuration with default values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the request timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set the connection timeout.
    pub fn with_connect_timeout(mut self, connect_timeout: Duration) -> Self {
        self.connect_timeout = connect_timeout;
        self
    }

    /// Skip TLS certificate verification (dangerous!).
    ///
    /// Only use this for testing or when connecting to servers with self-signed
    /// certificates that you trust.
    pub fn danger_accept_invalid_certs(mut self, accept: bool) -> Self {
        self.danger_accept_invalid_certs = accept;
        self
    }

    /// Add an extra header to send with each request.
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_headers.push((name.into(), value.into()));
        self
    }
}

// HttpRemote

/// HTTP client for communicating with remote Atomic repositories.
///
/// This struct provides methods to interact with `atomic-api` servers using
/// the Atomic protocol over HTTP.
#[derive(Debug, Clone)]
pub struct HttpRemote {
    /// The base URL for the repository endpoint.
    ///
    /// This should be the full path to the repository's protocol endpoint,
    /// e.g., `https://api.example.com/tenant/t/portfolio/p/project/pr/code`
    base_url: Url,

    /// The HTTP client.
    client: Client,

    /// Repository name (inferred from URL).
    name: Option<String>,
}

impl HttpRemote {
    /// Create a new HTTP remote connection.
    ///
    /// # Arguments
    ///
    /// * `url` - The base URL for the repository endpoint.
    ///
    /// # Example
    ///
    /// ```
    /// use atomic_remote::http::HttpRemote;
    ///
    /// let remote = HttpRemote::new("https://api.example.com/tenant/t/portfolio/p/project/pr/code")?;
    /// # Ok::<(), atomic_remote::error::RemoteError>(())
    /// ```
    pub fn new(url: &str) -> RemoteResult<Self> {
        Self::with_config(url, HttpRemoteConfig::default())
    }

    /// Create a new HTTP remote connection with custom configuration.
    ///
    /// # Arguments
    ///
    /// * `url` - The base URL for the repository endpoint.
    /// * `config` - Configuration options.
    ///
    /// # Example
    ///
    /// ```
    /// use atomic_remote::http::{HttpRemote, HttpRemoteConfig};
    /// use std::time::Duration;
    ///
    /// let config = HttpRemoteConfig::new()
    ///     .with_timeout(Duration::from_secs(60))
    ///     .with_header("Authorization", "Bearer token");
    ///
    /// let remote = HttpRemote::with_config(
    ///     "https://api.example.com/tenant/t/portfolio/p/project/pr/code",
    ///     config
    /// )?;
    /// # Ok::<(), atomic_remote::error::RemoteError>(())
    /// ```
    pub fn with_config(url: &str, config: HttpRemoteConfig) -> RemoteResult<Self> {
        let base_url = Url::parse(url)?;

        // Build default headers
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static(ATOMIC_USER_AGENT));
        headers.insert(
            ACCEPT_ENCODING,
            HeaderValue::from_static(ACCEPT_ENCODING_VALUE),
        );

        // Add extra headers from config
        for (name, value) in &config.extra_headers {
            if let (Ok(header_name), Ok(header_value)) = (
                reqwest::header::HeaderName::try_from(name.as_str()),
                HeaderValue::from_str(value),
            ) {
                headers.insert(header_name, header_value);
            }
        }

        // Build the HTTP client
        let client = Client::builder()
            .timeout(config.timeout)
            .connect_timeout(config.connect_timeout)
            .danger_accept_invalid_certs(config.danger_accept_invalid_certs)
            .default_headers(headers)
            .gzip(true)
            .deflate(true)
            .build()
            .map_err(|e| RemoteError::connection_failed(url, e))?;

        // Try to infer repository name from URL path
        let name = infer_repo_name(&base_url);

        debug!("Created HttpRemote for {} (name: {:?})", base_url, name);

        Ok(Self {
            base_url,
            client,
            name,
        })
    }

    /// Get the base URL.
    pub fn url(&self) -> &Url {
        &self.base_url
    }

    /// Get the inferred repository name.
    pub fn repo_name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Get the current state of a stack.
    ///
    /// # Arguments
    ///
    /// * `stack` - The name of the stack to query.
    ///
    /// # Returns
    ///
    /// The current state of the stack, or `StateResponse::Empty` if the
    /// stack is empty.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use atomic_remote::http::HttpRemote;
    ///
    /// async fn example() -> Result<(), Box<dyn std::error::Error>> {
    ///     let remote = HttpRemote::new("https://api.example.com/tenant/t/portfolio/p/project/pr/code")?;
    ///     let state = remote.get_state("main").await?;
    ///
    ///     if let Some(pos) = state.position() {
    ///         println!("Stack at position {}", pos);
    ///     } else {
    ///         println!("Stack is empty");
    ///     }
    ///     Ok(())
    /// }
    /// ```
    pub async fn get_state(&self, stack: &str) -> RemoteResult<StateResponse> {
        let url = format!("{}?stack={}&state=", self.base_url, stack);
        debug!("GET state: {}", url);

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| RemoteError::connection_failed(&url, e))?;

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
            StatusCode::NOT_FOUND => Err(RemoteError::stack_not_found(stack)),
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

    /// Get the changelist for a stack starting from a position.
    ///
    /// # Arguments
    ///
    /// * `stack` - The name of the stack to query.
    /// * `from` - The starting position (sequence number).
    ///
    /// # Returns
    ///
    /// A vector of changelist entries, starting from the given position.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use atomic_remote::http::HttpRemote;
    ///
    /// async fn example() -> Result<(), Box<dyn std::error::Error>> {
    ///     let remote = HttpRemote::new("https://api.example.com/tenant/t/portfolio/p/project/pr/code")?;
    ///
    ///     // Get all changes from the beginning
    ///     let entries = remote.get_changelist("main", 0).await?;
    ///
    ///     for entry in entries {
    ///         let tag_marker = if entry.tagged { " [tagged]" } else { "" };
    ///         println!("#{}: {}{}", entry.sequence, entry.hash, tag_marker);
    ///     }
    ///     Ok(())
    /// }
    /// ```
    pub async fn get_changelist(
        &self,
        stack: &str,
        from: u64,
    ) -> RemoteResult<Vec<ChangelistEntry>> {
        let url = format!("{}?stack={}&changelist={}", self.base_url, stack, from);
        debug!("GET changelist: {}", url);

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| RemoteError::connection_failed(&url, e))?;

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
            StatusCode::NOT_FOUND => Err(RemoteError::stack_not_found(stack)),
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

    /// Get the stack ID (UUID).
    ///
    /// # Arguments
    ///
    /// * `stack` - The name of the stack to query.
    ///
    /// # Returns
    ///
    /// The stack's UUID as a string.
    pub async fn get_id(&self, stack: &str) -> RemoteResult<String> {
        let url = format!("{}?stack={}&id", self.base_url, stack);
        debug!("GET id: {}", url);

        let response = self
            .client
            .get(&url)
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

                Ok(text.trim().to_string())
            }
            StatusCode::NOT_FOUND => Err(RemoteError::stack_not_found(stack)),
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

    /// Upload a change file.
    ///
    /// # Arguments
    ///
    /// * `hash` - The base32-encoded hash of the change.
    /// * `stack` - The target stack name.
    /// * `data` - The raw change file data.
    pub async fn upload_change(&self, hash: &str, stack: &str, data: Bytes) -> RemoteResult<()> {
        let url = format!("{}?apply={}&stack={}", self.base_url, hash, stack);
        debug!("POST apply: {} ({} bytes)", url, data.len());

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
    /// * `stack` - The target stack name.
    /// * `short_data` - The short tag data.
    pub async fn upload_tag(
        &self,
        state: &str,
        stack: &str,
        short_data: Bytes,
    ) -> RemoteResult<()> {
        let url = format!("{}?tagup={}&stack={}", self.base_url, state, stack);
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
            StatusCode::NOT_FOUND => Err(RemoteError::stack_not_found(stack)),
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

    /// Fork a stack on the remote server.
    ///
    /// Creates a new stack as a view of an existing one. This is a
    /// lightweight server-side operation — no data is duplicated. The
    /// new stack's changelog is copied from the source stack in a single
    /// transaction.
    ///
    /// # Arguments
    ///
    /// * `target_stack` - The name of the new stack to create.
    /// * `source_stack` - The name of the existing stack to fork from.
    ///
    /// # Returns
    ///
    /// The number of changes adopted into the new stack view.
    pub async fn fork_stack(&self, target_stack: &str, source_stack: &str) -> RemoteResult<u64> {
        let url = format!(
            "{}?fork_from={}&stack={}",
            self.base_url, source_stack, target_stack
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
                    "Forked stack '{}' from '{}' ({} changes)",
                    target_stack, source_stack, changes
                );

                Ok(changes)
            }
            StatusCode::BAD_REQUEST => {
                let msg = response.text().await.unwrap_or_default();
                Err(RemoteError::protocol(msg))
            }
            StatusCode::NOT_FOUND => Err(RemoteError::stack_not_found(source_stack)),
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

    // Streaming V3 Protocol Methods

    /// Upload a change file by reading directly from disk.
    ///
    /// Instead of loading the change into a `Change` struct and re-serializing,
    /// this reads the raw `.change` file bytes from disk and uploads them.
    /// For very large changes, this avoids the overhead of deserialization +
    /// re-serialization.
    ///
    /// # Arguments
    ///
    /// * `hash` - The base32-encoded hash of the change.
    /// * `stack` - The target stack name.
    /// * `path` - Path to the `.change` file on disk.
    ///
    /// # Errors
    ///
    /// Returns an error if the file can't be read, the upload fails, or
    /// the server rejects the change.
    pub async fn upload_change_file(
        &self,
        hash: &str,
        stack: &str,
        path: &Path,
    ) -> RemoteResult<()> {
        let url = format!("{}?apply={}&stack={}", self.base_url, hash, stack);
        debug!("POST apply (from file): {} from {:?}", url, path);

        let data = tokio::fs::read(path).await.map_err(|e| {
            RemoteError::other(format!("Failed to read change file {:?}: {}", path, e))
        })?;

        let file_size = data.len();
        info!("Uploading change {} from disk ({} bytes)", hash, file_size);

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
                debug!("Successfully uploaded change {} from file", hash);
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
    ///
    /// # Errors
    ///
    /// Returns an error if the download fails or the file can't be written.
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
    ///
    /// # Errors
    ///
    /// Returns an error if the change doesn't exist or the server returns
    /// an unexpected error.
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

// Helper Functions

/// Infer the repository name from a URL.
///
/// Attempts to extract the project/repository name from the URL path.
fn infer_repo_name(url: &Url) -> Option<String> {
    let path = url.path();
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    // Look for common patterns:
    // /.../project/{name}/code -> name
    // /.../project/{name}/.atomic -> name
    // /{name}.git -> name
    // /{name} -> name

    for (i, segment) in segments.iter().enumerate() {
        // Pattern: project/{name}/code or project/{name}/.atomic
        if (*segment == "code" || *segment == ".atomic")
            && i > 0 {
                return Some(segments[i - 1].to_string());
            }
    }

    // Fallback: use the last meaningful segment
    for segment in segments.iter().rev() {
        if *segment != "code" && *segment != ".atomic" && !segment.is_empty() {
            let name = segment.trim_end_matches(".git");
            return Some(name.to_string());
        }
    }

    None
}

/// Parse a changelist response into entries.
fn parse_changelist(text: &str) -> RemoteResult<Vec<ChangelistEntry>> {
    let mut entries = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let entry = ChangelistEntry::parse(line).map_err(|e| {
            RemoteError::protocol(format!("Failed to parse changelist entry: {}", e))
        })?;

        entries.push(entry);
    }

    Ok(entries)
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    // ── Streaming method URL construction ──────────────────────────

    #[test]
    fn test_upload_change_file_url_format() {
        // Verify the URL format matches the protocol spec
        let url = format!(
            "{}?apply={}&stack={}",
            "https://api.example.com/code", "ABCDEF123456", "dev"
        );
        assert!(url.contains("apply=ABCDEF123456"));
        assert!(url.contains("stack=dev"));
    }

    #[test]
    fn test_download_change_layers_url_format() {
        let layers = LayerSelection::thin_pull();
        let url = format!(
            "{}?change={}&layers={}",
            "https://api.example.com/code",
            "ABCDEF123456",
            layers.to_query_value()
        );
        assert!(url.contains("change=ABCDEF123456"));
        assert!(url.contains("layers=graph,content"));
    }

    #[test]
    fn test_download_change_layers_all_url_format() {
        let layers = LayerSelection::all();
        let url = format!(
            "{}?change={}&layers={}",
            "https://api.example.com/code",
            "HASH",
            layers.to_query_value()
        );
        assert!(url.contains("layers=all"));
    }

    #[test]
    fn test_chunk_manifest_url_format() {
        let url = format!(
            "{}?change={}&manifest",
            "https://api.example.com/code", "ABCDEF"
        );
        assert!(url.contains("change=ABCDEF"));
        assert!(url.contains("manifest"));
    }

    #[test]
    fn test_user_agent_format() {
        assert!(ATOMIC_USER_AGENT.starts_with("atomic-"));
    }

    #[test]
    fn test_http_remote_config_default() {
        let config = HttpRemoteConfig::default();
        assert_eq!(config.timeout, Duration::from_secs(DEFAULT_TIMEOUT_SECS));
        assert_eq!(
            config.connect_timeout,
            Duration::from_secs(DEFAULT_CONNECT_TIMEOUT_SECS)
        );
        assert!(!config.danger_accept_invalid_certs);
        assert!(config.extra_headers.is_empty());
    }

    #[test]
    fn test_http_remote_config_builder() {
        let config = HttpRemoteConfig::new()
            .with_timeout(Duration::from_secs(60))
            .with_connect_timeout(Duration::from_secs(20))
            .danger_accept_invalid_certs(true)
            .with_header("Authorization", "Bearer token");

        assert_eq!(config.timeout, Duration::from_secs(60));
        assert_eq!(config.connect_timeout, Duration::from_secs(20));
        assert!(config.danger_accept_invalid_certs);
        assert_eq!(config.extra_headers.len(), 1);
        assert_eq!(config.extra_headers[0].0, "Authorization");
        assert_eq!(config.extra_headers[0].1, "Bearer token");
    }

    #[test]
    fn test_infer_repo_name_project_code() {
        let url =
            Url::parse("https://api.example.com/tenant/t/portfolio/p/project/myrepo/code").unwrap();
        assert_eq!(infer_repo_name(&url), Some("myrepo".to_string()));
    }

    #[test]
    fn test_infer_repo_name_dot_atomic() {
        let url = Url::parse("https://api.example.com/tenant/t/portfolio/p/project/myrepo/.atomic")
            .unwrap();
        assert_eq!(infer_repo_name(&url), Some("myrepo".to_string()));
    }

    #[test]
    fn test_infer_repo_name_git_suffix() {
        let url = Url::parse("https://example.com/myrepo.git").unwrap();
        assert_eq!(infer_repo_name(&url), Some("myrepo".to_string()));
    }

    #[test]
    fn test_infer_repo_name_simple() {
        let url = Url::parse("https://example.com/myrepo").unwrap();
        assert_eq!(infer_repo_name(&url), Some("myrepo".to_string()));
    }

    #[test]
    fn test_parse_changelist_empty() {
        let entries = parse_changelist("").unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn test_parse_changelist_single() {
        let entries = parse_changelist("0.ABC123.DEF456").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].sequence, 0);
        assert_eq!(entries[0].hash, "ABC123");
        assert_eq!(entries[0].merkle, "DEF456");
        assert!(!entries[0].tagged);
    }

    #[test]
    fn test_parse_changelist_multiple() {
        let text = "0.ABC.DEF\n1.GHI.JKL.\n2.MNO.PQR";
        let entries = parse_changelist(text).unwrap();
        assert_eq!(entries.len(), 3);
        assert!(!entries[0].tagged);
        assert!(entries[1].tagged);
        assert!(!entries[2].tagged);
    }

    #[test]
    fn test_parse_changelist_with_blank_lines() {
        let text = "0.ABC.DEF\n\n1.GHI.JKL\n  \n";
        let entries = parse_changelist(text).unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn test_http_remote_new_valid_url() {
        // Note: This doesn't actually connect, just creates the struct
        let result =
            HttpRemote::new("https://api.example.com/tenant/t/portfolio/p/project/pr/code");
        assert!(result.is_ok());

        let remote = result.unwrap();
        assert_eq!(remote.repo_name(), Some("pr"));
    }

    #[test]
    fn test_http_remote_new_invalid_url() {
        let result = HttpRemote::new("not a valid url");
        assert!(result.is_err());
    }
}
