//! Transport-independent Atomic client — the local-daemon transport.
//!
//! One user-level daemon serves the `atomic` contract (libatomic) over a
//! private Unix-domain socket; this crate owns the socket location, the
//! connect logic, and the D4 start-or-retry lifecycle (a local client that
//! cannot reach the daemon may start/restart it — it may NEVER open the
//! repository databases itself).

// The tonic client surface returns `tonic::Status` (176 bytes) from
// nearly every method; boxing it everywhere would churn the whole
// call surface for no win. The transport crate opts out, as the
// libatomic daemon surface does.
#![allow(clippy::result_large_err)]

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use libatomic::atomic::daemon_service_client::DaemonServiceClient;
use libatomic::atomic::{HealthRequest, HealthResponse};
// The socket transport is unix-only; `Channel` itself is portable and
// callers keep their shape everywhere.
use tonic::transport::Channel;
#[cfg(unix)]
use tonic::transport::{Endpoint, Uri};
#[cfg(unix)]
use tower::service_fn;

/// Env override for the daemon socket (tests, harnesses, exotic layouts).
pub const ENV_DAEMON_SOCKET: &str = "ATOMIC_DAEMON_SOCKET";

/// Env override for the daemon binary the client may start (D4). Defaults
/// to an `atomicd` found on PATH.
pub const ENV_DAEMON_BIN: &str = "ATOMIC_DAEMON_BIN";

/// Where the one user-level daemon listens. `$ATOMIC_DAEMON_SOCKET` wins;
/// otherwise a private per-user runtime dir (0700) with a short socket name
/// (macOS limits Unix-socket paths to ~104 bytes).
pub fn socket_path() -> PathBuf {
    if let Some(path) = std::env::var_os(ENV_DAEMON_SOCKET) {
        return PathBuf::from(path);
    }
    let uid = nix_uid();
    let base = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from("/tmp").join(format!("atomic-{uid}")),
    };
    base.join("atomicd").join("atomicd.sock")
}

fn nix_uid() -> u32 {
    #[cfg(unix)]
    {
        unsafe { libc::geteuid() }
    }
    #[cfg(not(unix))]
    {
        0
    }
}

/// A connected client over the daemon socket.
pub struct AtomicClient {
    pub channel: Channel,
}

async fn connect_channel(socket: &std::path::Path) -> Result<Channel, std::io::Error> {
    // The transport is a Unix-domain socket: unix-only. Other platforms
    // compile (the CLI surface keeps its shape) but cannot connect.
    #[cfg(unix)]
    {
        let socket = socket.to_path_buf();
        let connect = move |_: Uri| {
            let socket = socket.clone();
            async move {
                let stream = tokio::net::UnixStream::connect(socket).await?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
            }
        };
        Endpoint::from_static("http://localhost") // authority is the socket, not a URI
            .connect_with_connector(service_fn(connect))
            .await
            .map_err(std::io::Error::other)
    }
    #[cfg(not(unix))]
    {
        let _ = socket;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "the atomic daemon transport is unix-only (unix-domain socket)",
        ))
    }
}

impl AtomicClient {
    /// Connect to an already-running daemon. Fails if the socket is absent
    /// or not serving.
    pub async fn connect_existing() -> Result<Self, String> {
        let socket = socket_path();
        let channel = connect_channel(&socket).await.map_err(|error| {
            format!("atomicd is not reachable at {}: {error}", socket.display())
        })?;
        let client = Self { channel };
        client
            .health()
            .await
            .map_err(|error| format!("atomicd at {} is not serving: {error}", socket.display()))?;
        Ok(client)
    }

    /// Connect, starting the daemon when it is down (D4: a local client may
    /// start/restart the daemon and retry — never open storage itself).
    pub async fn connect_or_start() -> Result<Self, String> {
        if let Ok(client) = Self::connect_existing().await {
            return Ok(client);
        }

        let binary = std::env::var_os(ENV_DAEMON_BIN)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("atomicd"));
        let socket = socket_path();
        if let Some(parent) = socket.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                format!(
                    "failed to create daemon runtime dir {}: {error}",
                    parent.display()
                )
            })?;
        }
        Command::new(&binary)
            .arg("serve")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| {
                format!(
                    "failed to start the atomic daemon ({}): {error}",
                    binary.display()
                )
            })?;

        for _ in 0..200 {
            if let Ok(client) = Self::connect_existing().await {
                return Ok(client);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        Err("atomicd did not become healthy before timeout".to_string())
    }

    /// Liveness probe (the negotiation pair's first half).
    pub async fn health(&self) -> Result<HealthResponse, tonic::Status> {
        let mut client = DaemonServiceClient::new(self.channel.clone());
        client
            .health(HealthRequest {})
            .await
            .map(|response| response.into_inner())
    }
}

/// Re-export for callers that drive the generated service clients directly.
pub use libatomic::atomic as proto;
