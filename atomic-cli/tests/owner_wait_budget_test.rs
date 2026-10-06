//! PR #230 regression reproduction: queueing for the owner's store lease must
//! consume the database wait budget, not start a fresh budget after queueing.

#![cfg(unix)]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use atomic_core::types::Hash;
use atomic_repository::Repository;
use serde_json::{json, Value};

const ATOMIC: &str = env!("CARGO_BIN_EXE_atomic");
const REQUESTS: usize = 8;

struct Owner {
    child: Child,
    endpoint: PathBuf,
}

impl Drop for Owner {
    fn drop(&mut self) {
        // Always reap the foreground owner, including on assertion failure.
        // Killing also bounds cleanup if its blocking workers are still queued.
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.endpoint);
    }
}

fn connect(endpoint: &Path) -> std::io::Result<UnixStream> {
    let stream = UnixStream::connect(endpoint)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    Ok(stream)
}

fn exchange(mut stream: UnixStream, id: &str, request: Value) -> std::io::Result<Value> {
    let frame = serde_json::to_vec(&json!({
        "version": 1,
        "request_id": id,
        "request": request,
    }))?;
    stream.write_all(&(frame.len() as u32).to_be_bytes())?;
    stream.write_all(&frame)?;
    stream.flush()?;

    let mut length = [0; 4];
    stream.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > 1024 * 1024 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unexpected owner response length {length}"),
        ));
    }
    let mut payload = vec![0; length];
    stream.read_exact(&mut payload)?;
    let response: Value = serde_json::from_slice(&payload)?;
    if response["version"] != 1 || response["request_id"] != id {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unexpected owner response: {response}"),
        ));
    }
    Ok(response)
}

#[test]
fn queued_owner_requests_share_the_configured_database_wait_deadline() {
    let temp = tempfile::tempdir().unwrap();
    // Keep the writable database handle alive through all eight requests.
    let held = Repository::init(temp.path()).unwrap();
    let canonical_dot_dir = std::fs::canonicalize(temp.path().join(".atomic")).unwrap();
    let digest = Hash::of(canonical_dot_dir.to_string_lossy().as_bytes());
    let prefix: String = digest.as_bytes()[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let endpoint = Path::new("/tmp").join(format!("atomic-owner-{prefix}.sock"));
    let mut owner = Owner {
        child: Command::new(ATOMIC)
            .args(["agent", "database-owner", "serve", "--repository"])
            .arg(temp.path())
            .env("ATOMIC_DB_LOCK_WAIT_MS", "100")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn database owner"),
        endpoint: endpoint.clone(),
    };

    let startup_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(response) =
            connect(&endpoint).and_then(|stream| exchange(stream, "ready", json!("Ping")))
        {
            assert!(response["response"].get("Pong").is_some(), "{response}");
            break;
        }
        assert!(
            owner.child.try_wait().unwrap().is_none(),
            "owner exited before answering Ping"
        );
        assert!(Instant::now() < startup_deadline, "owner startup timed out");
        thread::sleep(Duration::from_millis(10));
    }

    // Establish all sockets first, then release requests together. Use raw
    // frames so client-side reconnect/retry cannot explain elapsed times.
    let streams: Vec<_> = (0..REQUESTS).map(|_| connect(&endpoint).unwrap()).collect();
    let barrier = Arc::new(Barrier::new(REQUESTS + 1));
    let workers: Vec<_> = streams
        .into_iter()
        .enumerate()
        .map(|(index, stream)| {
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                let started = Instant::now();
                let response = exchange(
                    stream,
                    &format!("reserve-{index}"),
                    json!({
                        "ReserveProvenanceTurn": {
                            "session_id": format!("timeout-review-{index}"),
                            "turn_number": 1,
                            "now": 1_700_000_000,
                        }
                    }),
                );
                (started.elapsed(), response)
            })
        })
        .collect();
    barrier.wait();
    let results: Vec<_> = workers.into_iter().map(|worker| worker.join()).collect();

    // Cleanup precedes the intentional regression assertion, and the Owner
    // guard also covers every earlier panic path.
    drop(owner);
    drop(held);

    let mut elapsed = Vec::new();
    for result in results {
        let (duration, response) = result.expect("request thread panicked");
        let response = response.expect("raw owner request failed");
        assert_eq!(
            response["response"]["Error"]["code"], "database-unavailable",
            "expected busy-database response, got {response}"
        );
        elapsed.push(duration);
    }
    elapsed.sort_unstable();
    eprintln!("100 ms wait budget; eight concurrent raw RPC durations: {elapsed:?}");
    assert!(
        elapsed.last().unwrap() < &Duration::from_millis(400),
        "mutex queueing escapes the 100 ms database wait budget: {elapsed:?}"
    );
}
