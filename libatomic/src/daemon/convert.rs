//! Domain ↔ protobuf conversions shared by the daemon handlers.

use crate::atomic as pb;
use atomic_core::change::Author;
use atomic_core::types::Merkle;

pub fn hash_proto(hash: &Merkle) -> pb::Hash {
    pb::Hash {
        value: hash.0.to_vec(),
        algorithm: pb::HashAlgorithm::Blake3 as i32,
    }
}

pub fn author_proto(author: &Author) -> pb::Author {
    pb::Author {
        name: author.name.clone(),
        email: author.email.clone(),
    }
}

pub fn timestamp_proto(time: chrono::DateTime<chrono::Utc>) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: time.timestamp(),
        nanos: time.timestamp_subsec_nanos() as i32,
    }
}

pub fn timestamp_domain(stamp: &prost_types::Timestamp) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(stamp.seconds, stamp.nanos.max(0) as u32)
        .unwrap_or_else(chrono::Utc::now)
}

/// Native attribute-only changes remain MODIFIED for existing wire clients;
/// the optional diagnostic preserves the subtype for native CLI rendering.
pub const TYPE_CHANGED_DETAIL: &str = "file type changed";
pub const PERMISSIONS_CHANGED_DETAIL: &str = "file permissions changed";

pub fn file_status_details(entry: &atomic_repository::FileStatusEntry) -> Option<String> {
    use atomic_repository::FileStatus;
    match entry.status() {
        FileStatus::TypeChanged => Some(TYPE_CHANGED_DETAIL.to_string()),
        FileStatus::PermissionsChanged => Some(PERMISSIONS_CHANGED_DETAIL.to_string()),
        _ => entry.details().map(str::to_string),
    }
}

pub fn file_status_proto(status: atomic_repository::status::FileStatus) -> pb::FileStatus {
    use atomic_repository::status::FileStatus as Domain;
    match status {
        Domain::Modified => pb::FileStatus::Modified,
        Domain::Deleted => pb::FileStatus::Deleted,
        Domain::Untracked => pb::FileStatus::Untracked,
        Domain::Added => pb::FileStatus::Added,
        Domain::Conflicted => pb::FileStatus::Conflicted,
        Domain::Clean => pb::FileStatus::Unspecified,
        Domain::TypeChanged | Domain::PermissionsChanged => pb::FileStatus::Modified,
    }
}

/// Wire `StatusOptions` → domain `StatusOptions`. `None` means every
/// default; inside a present message, each unset wrapper resolves to that
/// field's domain default (atomic-repository/src/status.rs:677), so a
/// partial message is a full message with the gaps filled by the defaults
/// — never by whatever the handler happened to prefer.
pub fn status_options_domain(
    options: Option<pb::StatusOptions>,
) -> atomic_repository::status::StatusOptions {
    let defaults = atomic_repository::status::StatusOptions::default();
    let Some(options) = options else {
        return defaults;
    };
    atomic_repository::status::StatusOptions {
        include_untracked: options
            .include_untracked
            .unwrap_or(defaults.include_untracked),
        include_ignored: options.include_ignored.unwrap_or(defaults.include_ignored),
        path_filters: options
            .path_filters
            .into_iter()
            .map(std::path::PathBuf::from)
            .collect(),
        respect_ignore_files: options
            .respect_ignore_files
            .unwrap_or(defaults.respect_ignore_files),
        hash_contents: options.hash_contents.unwrap_or(defaults.hash_contents),
        ..defaults
    }
}

/// Intent lifecycle strings (vault_intent_*) ↔ the proto entity status.
pub fn intent_status_to_proto(status: &str) -> Option<pb::VaultEntityStatus> {
    match status {
        "backlog" => Some(pb::VaultEntityStatus::Backlog),
        "in_progress" => Some(pb::VaultEntityStatus::InProgress),
        "needs-review" => Some(pb::VaultEntityStatus::NeedsReview),
        "done" | "completed" => Some(pb::VaultEntityStatus::Done),
        "planned" => Some(pb::VaultEntityStatus::Planned),
        "icebox" => Some(pb::VaultEntityStatus::Suspended),
        _ => None,
    }
}

pub fn intent_status_from_proto(status: pb::VaultEntityStatus) -> Option<&'static str> {
    match status {
        pb::VaultEntityStatus::Backlog => Some("backlog"),
        pb::VaultEntityStatus::InProgress => Some("in_progress"),
        pb::VaultEntityStatus::NeedsReview => Some("needs-review"),
        pb::VaultEntityStatus::Done => Some("done"),
        pb::VaultEntityStatus::Planned => Some("planned"),
        pb::VaultEntityStatus::Suspended => Some("icebox"),
        _ => None,
    }
}

/// The typed agent event (snake_case HookType serde names) → HookType.
pub fn hook_type(name: &str) -> Option<atomic_agent::event::HookType> {
    use atomic_agent::event::HookType;
    match name {
        "session_start" => Some(HookType::SessionStart),
        "session_end" => Some(HookType::SessionEnd),
        "turn_start" => Some(HookType::TurnStart),
        "turn_end" => Some(HookType::TurnEnd),
        "pre_tool_use" => Some(HookType::PreToolUse),
        "post_tool_use" => Some(HookType::PostToolUse),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_repository::status::StatusOptions;

    #[test]
    fn absent_options_is_domain_default() {
        assert_eq!(
            status_options_domain(None),
            StatusOptions {
                include_untracked: true,
                include_ignored: false,
                path_filters: vec![],
                respect_ignore_files: true,
                hash_contents: true,
                ..StatusOptions::default()
            }
        );
    }

    #[test]
    fn unset_wrappers_resolve_to_defaults_not_false() {
        // A present message with only path_filters set must NOT turn the
        // unset bools off — proto3 presence defaults them to false.
        let wire = pb::StatusOptions {
            path_filters: vec!["src/main.rs".to_string()],
            ..Default::default()
        };
        let domain = status_options_domain(Some(wire));
        assert!(domain.include_untracked);
        assert!(!domain.include_ignored);
        assert!(domain.respect_ignore_files);
        assert!(domain.hash_contents);
        assert_eq!(
            domain.path_filters,
            vec![std::path::PathBuf::from("src/main.rs")]
        );
    }

    #[test]
    fn full_message_maps_verbatim() {
        let wire = pb::StatusOptions {
            include_untracked: Some(false),
            include_ignored: Some(true),
            respect_ignore_files: Some(false),
            hash_contents: Some(false),
            path_filters: vec!["a".to_string(), "b".to_string()],
        };
        let domain = status_options_domain(Some(wire));
        assert!(!domain.include_untracked);
        assert!(domain.include_ignored);
        assert!(!domain.respect_ignore_files);
        assert!(!domain.hash_contents);
        assert_eq!(
            domain.path_filters,
            vec![std::path::PathBuf::from("a"), std::path::PathBuf::from("b")]
        );
    }
}
