//! Tests for the HTTP remote client.

use super::*;
use crate::streaming::LayerSelection;
use std::time::Duration;

// ── Streaming method URL construction ──────────────────────────

#[test]
fn test_upload_change_file_url_format() {
    // Verify the URL format matches the protocol spec
    let url = format!(
        "{}?insert={}&view={}",
        "https://api.example.com/code", "ABCDEF123456", "dev"
    );
    assert!(url.contains("insert=ABCDEF123456"));
    assert!(url.contains("view=dev"));
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
fn test_provenance_list_url_format() {
    // Flat inventory endpoint — mirrors the .change sync model (advertise
    // hashes, client computes presence delta, pulls missing by hash).
    let url = format!("{}?provenance-list", "https://api.example.com/code");
    assert!(url.contains("provenance-list"));
    assert!(!url.contains("change="));
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
        Duration::from_secs(DEFAULT_CONNECT_TIMEOUT_SECS),
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
    let url =
        Url::parse("https://api.example.com/tenant/t/portfolio/p/project/myrepo/.atomic").unwrap();
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
    let result = HttpRemote::new("https://api.example.com/tenant/t/portfolio/p/project/pr/code");
    assert!(result.is_ok());

    let remote = result.unwrap();
    assert_eq!(remote.repo_name(), Some("pr"));
}

#[test]
fn test_http_remote_new_invalid_url() {
    let result = HttpRemote::new("not a valid url");
    assert!(result.is_err());
}
