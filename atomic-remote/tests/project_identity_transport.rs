//! Wire tests for the project-identity transport against a canned HTTP server.
//!
//! Covers the four behaviors the daemon and CLI depend on: reading a bound
//! identity, distinguishing unbound from unsupported servers, binding on the
//! first push, and mapping the server's 409 to a typed conflict that carries
//! the remote's document.

use std::io::{Read, Write};
use std::net::TcpListener;

use atomic_remote::{HttpRemote, HttpRemoteConfig};
use serde_json::json;

fn identity_document(project_id: &str) -> serde_json::Value {
    json!({
        "@context": {"atomic": "https://atomic.dev/ns/workflow#", "dcterms": "http://purl.org/dc/terms/"},
        "@type": "atomic:ProjectIdentity",
        "atomic:projectId": project_id,
        "atomic:schemaVersion": 1,
        "dcterms:created": "2026-10-26T00:00:00Z"
    })
}

/// Serve one scripted JSON response per connection, recording the request
/// lines so assertions can pin the exact wire behavior.
fn serve(responses: Vec<(u16, String)>) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!(
        "http://{}/workspaces/w/projects/p/code",
        listener.local_addr().unwrap()
    );
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let request_log = requests.clone();
    std::thread::spawn(move || {
        for (index, (status, body)) in responses.iter().enumerate() {
            let (mut stream, _) = match listener.accept() {
                Ok(accepted) => accepted,
                Err(_) => break,
            };
            let mut buffer = [0u8; 8192];
            let read = stream.read(&mut buffer).unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            request_log.lock().unwrap().push(request);
            if index + 1 == responses.len() {
                // Last scripted response: close after serving it.
            }
            let reason = if *status == 200 { "OK" } else { "Conflict" };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (url, requests)
}

fn remote(url: &str) -> HttpRemote {
    HttpRemote::with_config(url, HttpRemoteConfig::default()).expect("remote")
}

#[tokio::test]
async fn a_bound_identity_is_served_as_a_document() {
    let document = identity_document(&"A".repeat(52));
    let (url, requests) = serve(vec![(
        200,
        json!({"bound": true, "hash": "ab".repeat(32), "identity": document}).to_string(),
    )]);
    match remote(&url).fetch_project_identity().await.expect("fetch") {
        atomic_remote::http::identity::IdentityFetch::Bound(fetched) => {
            assert_eq!(fetched, document);
        }
        other => panic!("expected Bound, got {other:?}"),
    }
    let request = &requests.lock().unwrap()[0];
    assert!(
        request.starts_with("GET /workspaces/w/projects/p/code?project-identity=1 "),
        "identity read must be a GET with the selector: {request}"
    );
}

#[tokio::test]
async fn an_unbound_server_reads_as_unbound() {
    let (url, _) = serve(vec![(200, json!({"bound": false}).to_string())]);
    match remote(&url).fetch_project_identity().await.expect("fetch") {
        atomic_remote::http::identity::IdentityFetch::Unbound => {}
        other => panic!("expected Unbound, got {other:?}"),
    }
}

#[tokio::test]
async fn an_old_server_that_answers_the_project_info_ping_reads_as_unsupported() {
    // Pre-transport servers ignore the query param and answer the ping.
    let (url, _) = serve(vec![(
        200,
        json!({"workspace": "w", "project": "p", "stack": "dev"}).to_string(),
    )]);
    match remote(&url).fetch_project_identity().await.expect("fetch") {
        atomic_remote::http::identity::IdentityFetch::Unsupported => {}
        other => panic!("expected Unsupported, got {other:?}"),
    }
}

#[tokio::test]
async fn binding_an_unbound_project_succeeds() {
    let document = identity_document(&"A".repeat(52));
    let (url, requests) = serve(vec![(200, json!({"bound": true}).to_string())]);
    match remote(&url)
        .bind_project_identity(&document)
        .await
        .expect("bind")
    {
        atomic_remote::http::identity::BindOutcome::Bound => {}
        other => panic!("expected Bound, got {other:?}"),
    }
    let request = &requests.lock().unwrap()[0];
    assert!(
        request.starts_with("POST /workspaces/w/projects/p/code?project-identity=1 "),
        "binding must be a POST with the selector: {request}"
    );
    assert!(
        request.contains("content-type: application/json"),
        "the identity document rides as JSON: {request}"
    );
}

#[tokio::test]
async fn a_conflict_reports_the_remote_binding() {
    let document = identity_document(&"A".repeat(52));
    let remote_document = identity_document(&"B".repeat(52));
    let (url, _) = serve(vec![
        (
            409,
            json!({"error": {"code": "IDENTITY_CONFLICT"}}).to_string(),
        ),
        (
            200,
            json!({"bound": true, "hash": "cd".repeat(32), "identity": remote_document})
                .to_string(),
        ),
    ]);
    match remote(&url)
        .bind_project_identity(&document)
        .await
        .expect("bind")
    {
        atomic_remote::http::identity::BindOutcome::Conflict { remote } => {
            assert_eq!(remote, Some(remote_document));
        }
        other => panic!("expected Conflict, got {other:?}"),
    }
}

#[tokio::test]
async fn an_old_server_rejecting_the_post_maps_to_unsupported() {
    let document = identity_document(&"A".repeat(52));
    let (url, _) = serve(vec![(405, "method not allowed".to_string())]);
    match remote(&url)
        .bind_project_identity(&document)
        .await
        .expect("bind")
    {
        atomic_remote::http::identity::BindOutcome::Unsupported => {}
        other => panic!("expected Unsupported, got {other:?}"),
    }
}
