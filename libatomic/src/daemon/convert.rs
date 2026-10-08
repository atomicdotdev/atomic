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

pub fn file_status_proto(status: atomic_repository::status::FileStatus) -> pb::FileStatus {
    use atomic_repository::status::FileStatus as Domain;
    match status {
        Domain::Modified => pb::FileStatus::Modified,
        Domain::Deleted => pb::FileStatus::Deleted,
        Domain::Untracked => pb::FileStatus::Untracked,
        Domain::Added => pb::FileStatus::Added,
        Domain::Conflicted => pb::FileStatus::Conflicted,
        Domain::Clean | Domain::TypeChanged | Domain::PermissionsChanged => {
            pb::FileStatus::Unspecified
        }
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
