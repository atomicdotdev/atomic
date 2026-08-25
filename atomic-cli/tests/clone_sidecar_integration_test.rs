//! End-to-end regression test: a fresh `atomic clone` must ingest the
//! provenance graphs and attestations that ride the `/code` sync pack, so
//! `atomic change <hash>` shows its Change Ledger immediately — without a
//! subsequent "repair" pull.
//!
//! The reported bug: the server had the provenance, the `/code` pull response
//! included it, and `atomic pull` imported it — but `atomic clone` received
//! the same `SyncPack` and only indexed `ObjectFamily::Change`, so a fresh
//! clone printed "No provenance graph found" until the user ran a pull to
//! re-deliver the sidecars.
//!
//! The test stands up a minimal HTTP server speaking the `/code` sync
//! protocol (GET with a `SyncWants` body → `SyncPack` response) over a
//! hand-built pack containing one change, its view snapshot, one provenance
//! graph, and one attestation — exactly the shape a real server sends after
//! an agent-made push. It then drives the real CLI binary.
//!
//! Windows is excluded: the test isolates the identity store by pointing
//! `HOME` at a temp dir, which `dirs::home_dir()` ignores on Windows.
#![cfg(not(windows))]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};

use atomic_core::change::attestation::{AttestAgent, Attestation};
use atomic_core::change::{Change, ChangeHeader, ProvenanceGraph};
use atomic_core::types::{Base32, Hash, Merkle, SetId};
use atomic_objects::{
    ObjectFamily, ObjectRecord, RefRecord, SyncPack, ViewScopeLabel, ViewSnapshot,
};
use tempfile::TempDir;

const ATOMIC_BIN: &str = env!("CARGO_BIN_EXE_atomic");

fn atomic(work_dir: &Path, home_dir: &Path, args: &[&str]) -> Output {
    Command::new(ATOMIC_BIN)
        .args(args)
        .current_dir(work_dir)
        .env("HOME", home_dir)
        .output()
        .expect("run atomic")
}

/// Serialize a change and return `(bytes, hash)` — the object a server stores
/// under `changes/{blake3}`.
fn change_object() -> (Vec<u8>, Hash) {
    let change = Change::new(
        ChangeHeader::builder()
            .message("clone sidecar regression")
            .build(),
        vec![],
        vec![],
        vec![],
    );
    let mut buf = Vec::new();
    let hash = change.serialize(&mut buf).expect("serialize change");
    (buf, hash)
}

/// A small session-scoped `SyncPack` for one change: the change object, the
/// `dev` view's snapshot + ref, a provenance graph explaining the change, and
/// an attestation covering it — the object mix push produces for agent-made
/// changes.
fn agent_push_pack(change_bytes: Vec<u8>, change_hash: &Hash) -> SyncPack {
    let change_key = change_hash.to_base32();

    // Provenance graph explaining the change.
    let graph = ProvenanceGraph::builder("session-clone-e2e", "opencode")
        .add_change_explained(*change_hash)
        .build();
    let prov_bytes = graph.serialize().expect("serialize provenance");
    let prov_key = Hash::of(&prov_bytes).to_base32();

    // Attestation covering the change.
    let attest = Attestation::builder(
        "session-clone-e2e",
        AttestAgent::new("opencode", "OpenCode", "atomic"),
    )
    .add_change(*change_hash)
    .build();
    let attest_bytes = attest.serialize().expect("serialize attestation");
    let attest_key = Hash::of(&attest_bytes).to_base32();

    // The view snapshot: shared root "dev" owning exactly this change. The
    // merkle state must equal the fold of the change log (manifest verify)
    // and the set-id the order-invariant fold of it.
    let mut merkle = Merkle::ZERO;
    let mut set_id = SetId::ZERO;
    merkle = merkle.next(change_hash);
    set_id = set_id.add(change_hash);
    let snapshot = ViewSnapshot::new(
        ViewScopeLabel::Shared,
        None,
        Vec::new(),
        vec![change_key.clone()],
        set_id.to_base32(),
        Some(merkle.to_base32()),
    );
    let snap_key = snapshot.content_key();

    SyncPack {
        objects: vec![
            ObjectRecord::new(ObjectFamily::Change, change_key, change_bytes),
            ObjectRecord::new(
                ObjectFamily::View,
                snap_key.clone(),
                snapshot.to_canonical_bytes(),
            ),
            ObjectRecord::new(ObjectFamily::Provenance, prov_key, prov_bytes),
            ObjectRecord::new(ObjectFamily::Attest, attest_key, attest_bytes),
        ],
        refs: vec![RefRecord {
            name: "dev".to_string(),
            expect_old: None,
            new_target: snap_key,
        }],
    }
}

/// Serve the `/code` sync protocol: any GET whose path ends in `/code` gets
/// the canned `SyncPack`; everything else is a 404. One request per
/// connection (`Connection: close`), which is all reqwest needs.
fn spawn_sync_server(pack: SyncPack) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
    let addr = listener.local_addr().expect("local addr");
    let pack_bytes = pack.encode().expect("encode sync pack");

    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            // Read the request head + body (Content-Length bounded).
            loop {
                let n = match stream.read(&mut tmp) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                buf.extend_from_slice(&tmp[..n]);
                if let Some(head_end) = find_head_end(&buf) {
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                    if let Some(len) = content_length(&head) {
                        if buf.len() >= head_end + 4 + len {
                            break;
                        }
                    } else {
                        break;
                    }
                }
            }
            let head = String::from_utf8_lossy(&buf).to_string();
            let path = head.split_whitespace().nth(1).unwrap_or("");

            let (status, body) = if path.ends_with("/code") {
                ("200 OK", pack_bytes.clone())
            } else {
                ("404 Not Found", b"not found".to_vec())
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/octet-stream\r\n\
                 X-Atomic-Min-Version: 0.16.2\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.write_all(&body);
            let _ = stream.flush();
        }
    });

    format!("http://{}", addr)
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn content_length(head: &str) -> Option<usize> {
    for line in head.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            return v.trim().parse().ok();
        }
    }
    None
}

#[test]
fn fresh_clone_ingests_provenance_and_attestations() {
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let target = work.path().join("cloned");
    let change_hash_str;

    // 1. Build the "remote": a canned agent-made push over the sync protocol.
    {
        let (change_bytes, change_hash) = change_object();
        change_hash_str = change_hash.to_base32();
        let server = spawn_sync_server(agent_push_pack(change_bytes, &change_hash));

        // 2. Clone it with the real CLI.
        let out = atomic(
            work.path(),
            home.path(),
            &[
                "clone",
                &format!("{server}/workspaces/ws/projects/proj/code"),
                target.to_str().unwrap(),
            ],
        );
        assert!(
            out.status.success(),
            "clone failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }

    // 3. The regression: `atomic change <hash>` must show the Change Ledger
    //    (REV_DEPS-visible provenance) on the fresh clone, with no repair pull.
    let out = atomic(&target, home.path(), &["change", &change_hash_str]);
    assert!(
        out.status.success(),
        "atomic change failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("=== Change Ledger ==="),
        "change view lacks the ledger — sidecars were not ingested:\n{stdout}"
    );
    assert!(
        stdout.contains("session-clone-e2e"),
        "ledger does not show the cloned provenance session:\n{stdout}"
    );
    assert!(
        !stdout.contains("No provenance graph found"),
        "provenance is missing after clone:\n{stdout}"
    );

    // 4. The attestation sidecar landed too: the cloned store holds exactly
    //    one `.attest` file, the one from the pack.
    {
        use std::fs;
        let attest_dir = target.join(".atomic/changes");
        let mut attest_files = Vec::new();
        for entry in fs::read_dir(&attest_dir).expect("changes dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                for f in fs::read_dir(&path).expect("prefix dir") {
                    let f = f.expect("file entry").path();
                    if f.extension().and_then(|e| e.to_str()) == Some("attest") {
                        attest_files.push(f);
                    }
                }
            }
        }
        assert_eq!(
            attest_files.len(),
            1,
            "expected exactly one attestation sidecar in the fresh clone"
        );
    }
}
