//! HTTP client for communicating with Atomic API servers.
//!
//! This module provides the `HttpRemote` struct which implements the remote
//! protocol over HTTP, compatible with `atomic-api` servers.
//!
//! # Module Structure
//!
//! - [`mod@self`]: Core `HttpRemote` struct, configuration, and construction
//! - `queries`: Read operations (state, changelist, downloads)
//! - `upload`: Write operations (change upload, tag upload, fork)
//!
//! # Example
//!
//! ```ignore
//! use atomic_remote::http::HttpRemote;
//!
//! async fn example() -> Result<(), Box<dyn std::error::Error>> {
//!     let remote = HttpRemote::new("https://api.example.com/tenant/t/portfolio/p/project/pr/code")?;
//!
//!     // Get the current state of the "main" view
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

mod download;
mod queries;
mod upload;

#[cfg(test)]
mod tests;

use crate::error::{RemoteError, RemoteResult};
use crate::types::ChangelistEntry;
use log::debug;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT_ENCODING, USER_AGENT};
use reqwest::Client;
use std::time::Duration;
use url::Url;

// ============================================================================
// Constants
// ============================================================================

/// User-Agent header sent with all requests.
///
/// This is critical for API servers to detect Atomic CLI requests vs web browsers.
/// The format is `atomic-{version}`.
pub(crate) const ATOMIC_USER_AGENT: &str = concat!("atomic-", env!("CARGO_PKG_VERSION"));

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

// ============================================================================
// HttpRemoteConfig
// ============================================================================

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

// ============================================================================
// HttpRemote
// ============================================================================

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
    pub(crate) base_url: Url,

    /// The HTTP client.
    pub(crate) client: Client,

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
}

// ============================================================================
// Helper Functions
// ============================================================================

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
        if (*segment == "code" || *segment == ".atomic") && i > 0 {
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
pub(crate) fn parse_changelist(text: &str) -> RemoteResult<Vec<ChangelistEntry>> {
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
