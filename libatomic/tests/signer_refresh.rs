use atomic_core::{
    change::{AttestAgent, Attestation},
    types::Hash,
};
use atomic_identity::KeyPair;
use atomic_remote::{HttpRemoteConfig, StorageClient};
use atomic_repository::Repository;
use std::{
    io::{Read, Write},
    net::TcpListener,
};

fn serve_key(did: String, public_key: String) -> (StorageClient, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 1024];
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let count = stream.read(&mut buffer).unwrap();
            assert_ne!(count, 0);
            request.extend_from_slice(&buffer[..count]);
        }
        let request = String::from_utf8(request).unwrap();
        assert!(request.starts_with(&format!("GET /identities/by-key/{did} ")));
        assert!(request
            .to_ascii_lowercase()
            .contains("authorization: bearer fixture-token"));
        let body = serde_json::json!({"success": true, "data": {
            "id": "00000000-0000-0000-0000-000000000001", "name": "remote signer",
            "status": "active", "createdAt": "2026-10-15T00:00:00Z", "publicKey": public_key
        }})
        .to_string();
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
    });
    let client = StorageClient::new(&format!("http://{address}"), "", "fixture-token").unwrap();
    (client, server)
}

#[tokio::test]
async fn fetched_keys_are_did_checked_before_cache_and_offline_verification() {
    use atomic_core::kg::{iri, ontology::Predicate as P};
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init(directory.path()).unwrap();
    let key = KeyPair::generate();
    let did = atomic_canonical::did::did_for_public_key(&key.public);
    let mut attestation =
        Attestation::builder("remote-session", AttestAgent::new("agent", "Agent", "1"))
            .add_change(Hash::of(b"covered change"))
            .build();
    attestation.sign_with_did(&did, key.secret.as_bytes());
    let hash = repo.save_attestation(&attestation).unwrap();
    let status = || {
        repo.workflow_snapshot()
            .unwrap()
            .edges
            .into_iter()
            .find(|edge| {
                edge.from_id == iri::mint_attestation(&hash) && edge.kind == P::VerificationResult
            })
            .unwrap()
            .to_id
    };
    assert!(status().ends_with("#Unverifiable"));

    let (client, server) = serve_key(did.clone(), KeyPair::generate().public.to_base32());
    assert_eq!(
        libatomic::signer_refresh::refresh(&repo, &client)
            .await
            .unwrap(),
        0
    );
    server.join().unwrap();
    assert!(repo
        .pristine()
        .read_txn()
        .unwrap()
        .signer_key(&did)
        .unwrap()
        .is_none());
    assert!(status().ends_with("#Unverifiable"));

    let (client, server) = serve_key(did.clone(), key.public.to_base32());
    assert_eq!(
        libatomic::signer_refresh::refresh(&repo, &client)
            .await
            .unwrap(),
        1
    );
    server.join().unwrap();
    assert!(status().ends_with("#Verified"));
    // No second fetch: cache hits are offline, even after the server exited.
    assert_eq!(
        libatomic::signer_refresh::refresh(&repo, &client)
            .await
            .unwrap(),
        0
    );
}

#[test]
fn anonymous_remotes_do_not_start_signer_discovery() {
    assert!(libatomic::signer_refresh::client_for_remote(
        "https://example.test/project/code",
        &HttpRemoteConfig::new()
    )
    .is_none());
}
