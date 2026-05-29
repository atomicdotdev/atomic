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

## Feature flags

- `integration-tests`: enables integration tests that require a running
  `atomic-api` server.
