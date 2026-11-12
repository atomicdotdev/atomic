# atomic-remote

`atomic-remote` is the HTTP client library for Atomic remote repository operations.
It provides protocol types, storage APIs, and sync helpers used by Atomic tooling to
communicate with `atomic-api` servers.

## What it provides

- `HttpRemote` for push, pull, clone, and view-state operations over HTTP.
- Protocol types for changelists, state responses, pull deltas, and push deltas.
- Storage management APIs for workspaces and projects.
- Error types with retry/auth/not-found classification and user-facing suggestions.
- Streaming push/pull support for chunked remote transfers.
- Shared local identity resolution, self-signed JWTs, and agent delegation for
  CLI and trusted native callers (`auth`, `auth::token`, `auth::delegation`).

## Usage

```rust,ignore
use atomic_remote::HttpRemote;

async fn example() -> Result<(), Box<dyn std::error::Error>> {
    let remote = HttpRemote::new(
        "https://api.example.com/tenant/t/portfolio/p/project/myrepo/code",
    )?;

    let state = remote.get_state("main").await?;
    println!("Remote state: {state:?}");

    Ok(())
}
```

## Local authentication

Repository clients share one local authentication policy rather than minting tokens
in an RPC handler or passing signing keys through RPC:

```rust,ignore
use atomic_remote::{auth, HttpRemote, HttpRemoteConfig};

async fn authenticated_remote(
    url: &str,
    identity: Option<&str>,
) -> Result<HttpRemote, Box<dyn std::error::Error>> {
    // Pushes fail before network I/O when no identity/signing key is usable.
    // Omit this preflight for reads that may be publicly accessible.
    auth::check_push_credentials(url, identity)?;
    let config = auth::attach_identity(HttpRemoteConfig::new(), url, identity).await;
    Ok(HttpRemote::with_config(url, config)?)
}
```

Identity precedence is explicit override, nonempty `ATOMIC_AGENT_IDENTITY`, URL
userinfo, configured server binding (longest host suffix, active profile wins ties;
agent binding preferred), then the legacy leading subdomain label. Missing inferred
identities fall back to the store's default; a missing explicit override never does.
Callers resolve the configured remote URL and pass its identity binding only when
no explicit override was supplied.

`attach_identity` signs a fresh five-minute EdDSA JWT locally and presents an agent's
verified delegation certificate when applicable. It leaves the config unchanged on
authentication failure so public reads remain possible. Delegation resolution keeps
the existing environment-grant precedence, signature, expiry, local revocation,
and scope semantics; the server remains authoritative for remote revocations.
`AuthError` distinguishes `Internal`, `AuthenticationFailed { remote }`, and
`DelegationError { message }` so callers can preserve their error categories.

Identity/config/key/delegation access is synchronous local filesystem work even
though token attachment has an async API. Async services should run this trusted
local credential preparation on a blocking thread; network-facing handlers should
receive prepared transport credentials, not read ambient local identities.

## Feature flags

- `integration-tests`: enables integration tests that require a running
  `atomic-api` server.
