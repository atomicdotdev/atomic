use super::*;
use crate::InsertOptions;
use atomic_core::change::{Author, Change, ChangeHeader};
use atomic_core::types::Base32;

use tempfile::TempDir;

mod change_tests;
mod cross_view_merge_tests;
mod edit_tests;
mod history_tests;
mod init_tests;
mod integration_tests;
mod record_tests;
mod status_tests;

mod tracking_tests;
mod view_tests;

// ── Shared Helpers ──────────────────────────────────────────────────────

pub(super) fn create_temp_repo() -> (TempDir, Repository) {
    let temp_dir = TempDir::new().unwrap();
    let repo = Repository::init(temp_dir.path()).unwrap();
    (temp_dir, repo)
}

/// Create a simple test change with the given message.
pub(super) fn create_test_change(message: &str) -> Change {
    let header = ChangeHeader::builder()
        .message(message)
        .author(Author::new("Test Author", Some("test@example.com")))
        .build();

    Change::new(header, Vec::new(), Vec::new(), Vec::new())
}

/// Create a test change with some content.
pub(super) fn create_test_change_with_content(message: &str, content: &[u8]) -> Change {
    let header = ChangeHeader::builder()
        .message(message)
        .author(Author::new("Test Author", Some("test@example.com")))
        .build();

    Change::new(header, Vec::new(), content.to_vec(), Vec::new())
}
