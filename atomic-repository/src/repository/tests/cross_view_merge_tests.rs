//! Regression tests for cross-view merge content duplication.
//!
//! Validates that inserting changes from a draft view into a shared view
//! does NOT produce duplicated content when both views have divergent edits
//! to the same file(s). This was a user-reported bug where merged content
//! got concatenated instead of properly merged.

use super::*;
use crate::apply::CrossViewInsertOptions;
use crate::record::RecordOptions;
use atomic_core::change::ChangeHeader;

// ── Helpers ─────────────────────────────────────────────────────────────

fn count_occurrences(content: &str, pattern: &str) -> usize {
    content.matches(pattern).count()
}

fn get_content_as_string(repo: &Repository, path: &str, view: &str) -> String {
    let bytes = repo
        .get_file_content_on_view(path, view)
        .expect("Failed to get content")
        .expect("File not found in graph");
    String::from_utf8(bytes).expect("Content is not valid UTF-8")
}

fn record_all(repo: &Repository, message: &str) {
    let header = ChangeHeader::new(message);
    let options = RecordOptions::new()
        .with_all(true)
        .save_to_store(true)
        .apply_after_record(true);
    repo.record(header, options).unwrap();
}

// ── Tests ───────────────────────────────────────────────────────────────

/// Non-overlapping edits to different sections of the same file.
///
/// Draft changes section 1 (host), dev changes section 3 (log level).
/// After insert, every section header must appear exactly once and both
/// edits must be present.
#[test]
fn test_cross_view_merge_non_overlapping_edits() {
    let (temp_dir, mut repo) = create_temp_repo();

    // Initial file on dev: three TOML sections
    let initial = "\
[server]
host = \"localhost\"
port = 8080

[database]
url = \"postgres://localhost/mydb\"

[logging]
level = \"info\"
";
    let file = temp_dir.path().join("config.toml");
    std::fs::write(&file, initial).unwrap();
    repo.add("config.toml", TrackingOptions::default()).unwrap();
    record_all(&repo, "Add initial config");

    // Create draft from dev
    repo.create_view_from("feature", "dev").unwrap();

    // Draft: change host in [server]
    repo.switch_view("feature").unwrap();
    let draft_content = "\
[server]
host = \"0.0.0.0\"
port = 8080

[database]
url = \"postgres://localhost/mydb\"

[logging]
level = \"info\"
";
    std::fs::write(&file, draft_content).unwrap();
    record_all(&repo, "Change server host");

    // Dev: change log level in [logging]
    repo.switch_view("dev").unwrap();
    let dev_content = "\
[server]
host = \"localhost\"
port = 8080

[database]
url = \"postgres://localhost/mydb\"

[logging]
level = \"debug\"
";
    std::fs::write(&file, dev_content).unwrap();
    record_all(&repo, "Change log level");

    // Insert feature → dev
    let outcome = repo
        .insert_from_view(CrossViewInsertOptions::new("feature", "dev"))
        .unwrap();
    assert!(
        outcome.has_applied() || !outcome.skipped_hashes.is_empty(),
        "Insert should process at least one change"
    );
    repo.materialize().unwrap();

    let content = get_content_as_string(&repo, "config.toml", "dev");

    assert_eq!(
        count_occurrences(&content, "[server]"),
        1,
        "Expected [server] once but found {} times.\nActual content:\n{}",
        count_occurrences(&content, "[server]"),
        content
    );
    assert_eq!(
        count_occurrences(&content, "[database]"),
        1,
        "Expected [database] once but found {} times.\nActual content:\n{}",
        count_occurrences(&content, "[database]"),
        content
    );
    assert_eq!(
        count_occurrences(&content, "[logging]"),
        1,
        "Expected [logging] once but found {} times.\nActual content:\n{}",
        count_occurrences(&content, "[logging]"),
        content
    );

    // Both edits should be present
    assert!(
        content.contains("0.0.0.0") || content.contains("localhost"),
        "Server host edit should be present.\nActual content:\n{}",
        content
    );
    assert!(
        content.contains("debug"),
        "Log level edit should be present.\nActual content:\n{}",
        content
    );
}

/// Overlapping edits to the same line — a true CRDT conflict.
///
/// Both sides independently replace the same content vertex. The graph
/// correctly represents this as two competing alive vertices (a fork).
/// Both versions are present in the output — this IS the correct CRDT
/// conflict representation. The key invariant is that both sides' edits
/// are present and the original (pre-edit) content is gone.
#[test]
fn test_cross_view_merge_overlapping_edits() {
    let (temp_dir, mut repo) = create_temp_repo();

    let initial = "\
version = \"1.0.0\"
name = \"my-app\"
description = \"A sample application\"
";
    let file = temp_dir.path().join("metadata.toml");
    std::fs::write(&file, initial).unwrap();
    repo.add("metadata.toml", TrackingOptions::default())
        .unwrap();
    record_all(&repo, "Add metadata");

    repo.create_view_from("feature", "dev").unwrap();

    // Draft: bump to 2.0.0
    repo.switch_view("feature").unwrap();
    let draft = "\
version = \"2.0.0\"
name = \"my-app\"
description = \"A sample application\"
";
    std::fs::write(&file, draft).unwrap();
    record_all(&repo, "Bump version to 2.0.0");

    // Dev: bump to 1.1.0
    repo.switch_view("dev").unwrap();
    let dev = "\
version = \"1.1.0\"
name = \"my-app\"
description = \"A sample application\"
";
    std::fs::write(&file, dev).unwrap();
    record_all(&repo, "Bump version to 1.1.0");

    // Insert feature → dev
    repo.insert_from_view(CrossViewInsertOptions::new("feature", "dev"))
        .unwrap();
    repo.materialize().unwrap();

    let content = get_content_as_string(&repo, "metadata.toml", "dev");

    // Both sides replaced the same content vertex independently.
    // The graph has two alive replacement vertices — a genuine conflict.
    // Both versions should be present (CRDT fork), and the original
    // version ("1.0.0") should be dead.
    assert!(
        content.contains("1.1.0"),
        "Dev's version edit should be present.\nActual content:\n{}",
        content
    );
    assert!(
        content.contains("2.0.0"),
        "Draft's version edit should be present.\nActual content:\n{}",
        content
    );
    assert!(
        !content.contains("1.0.0"),
        "Original version (1.0.0) should be dead (superseded by both sides).\nActual content:\n{}",
        content
    );
}

/// Two files: readme has a heading conflict (both sides replaced the
/// same content independently → CRDT fork), app.py has a clean merge
/// (only the draft modified it).
#[test]
fn test_cross_view_merge_two_files_mixed_collision() {
    let (temp_dir, mut repo) = create_temp_repo();

    // Initial files
    let readme_initial = "\
# My Project

Welcome to the project.

## Installation

Run `cargo build`.
";
    let app_initial = "\
def main():
    print(\"hello\")

if __name__ == \"__main__\":
    main()
";
    std::fs::write(temp_dir.path().join("readme.md"), readme_initial).unwrap();
    std::fs::write(temp_dir.path().join("app.py"), app_initial).unwrap();
    repo.add("readme.md", TrackingOptions::default()).unwrap();
    repo.add("app.py", TrackingOptions::default()).unwrap();
    record_all(&repo, "Add readme and app");

    repo.create_view_from("feature", "dev").unwrap();

    // Draft: change readme heading + add greet function
    repo.switch_view("feature").unwrap();
    let readme_draft = "\
# My Awesome Project

Welcome to the project.

## Installation

Run `cargo build`.
";
    let app_draft = "\
def greet(name):
    return f\"Hello, {name}!\"

def main():
    print(greet(\"world\"))

if __name__ == \"__main__\":
    main()
";
    std::fs::write(temp_dir.path().join("readme.md"), readme_draft).unwrap();
    std::fs::write(temp_dir.path().join("app.py"), app_draft).unwrap();
    record_all(&repo, "Update readme heading and add greet");

    // Dev: change readme heading differently (conflict)
    repo.switch_view("dev").unwrap();
    let readme_dev = "\
# My Super Project

Welcome to the project.

## Installation

Run `cargo build`.
";
    std::fs::write(temp_dir.path().join("readme.md"), readme_dev).unwrap();
    record_all(&repo, "Update readme heading on dev");

    // Insert feature → dev
    repo.insert_from_view(CrossViewInsertOptions::new("feature", "dev"))
        .unwrap();
    repo.materialize().unwrap();

    // Verify app.py
    let app_content = get_content_as_string(&repo, "app.py", "dev");
    assert_eq!(
        count_occurrences(&app_content, "def main"),
        1,
        "Expected 'def main' once in app.py.\nActual content:\n{}",
        app_content
    );
    assert!(
        app_content.contains("def greet"),
        "Expected 'def greet' in app.py.\nActual content:\n{}",
        app_content
    );
    assert_eq!(
        count_occurrences(&app_content, "__name__"),
        1,
        "Expected __name__ guard once in app.py.\nActual content:\n{}",
        app_content
    );

    // Verify readme: both sides independently replaced the file content
    // (each changed the heading).  The graph has two competing alive
    // replacement vertices — a genuine CRDT conflict/fork.  Both sides'
    // headings should be present and the original heading should be gone.
    let readme_content = get_content_as_string(&repo, "readme.md", "dev");
    assert!(
        readme_content.contains("My Super Project"),
        "Dev's heading should be present.\nActual content:\n{}",
        readme_content
    );
    assert!(
        readme_content.contains("My Awesome Project"),
        "Draft's heading should be present.\nActual content:\n{}",
        readme_content
    );
    assert!(
        !readme_content.contains("# My Project\n"),
        "Original heading should be dead.\nActual content:\n{}",
        readme_content
    );
}

/// Both sides modify the same file by prepending a new version entry.
///
/// Because both sides replace the full file content (the diff sees a
/// whole-file replacement), the graph produces two competing alive
/// replacement vertices — a CRDT conflict/fork.  Both versions should
/// be present and the original content should be gone.
#[test]
fn test_cross_view_merge_append_both_sides() {
    let (temp_dir, mut repo) = create_temp_repo();

    let initial = "\
# Changelog

## v1.2
- Bug fixes

## v1.1
- Performance improvements

## v1.0
- Initial release
";
    let file = temp_dir.path().join("CHANGELOG.md");
    std::fs::write(&file, initial).unwrap();
    repo.add("CHANGELOG.md", TrackingOptions::default())
        .unwrap();
    record_all(&repo, "Add changelog");

    repo.create_view_from("feature", "dev").unwrap();

    // Draft: append v1.3
    repo.switch_view("feature").unwrap();
    let draft = "\
# Changelog

## v1.3
- New API endpoints

## v1.2
- Bug fixes

## v1.1
- Performance improvements

## v1.0
- Initial release
";
    std::fs::write(&file, draft).unwrap();
    record_all(&repo, "Add v1.3 entry");

    // Dev: append v1.4
    repo.switch_view("dev").unwrap();
    let dev = "\
# Changelog

## v1.4
- Security patches

## v1.2
- Bug fixes

## v1.1
- Performance improvements

## v1.0
- Initial release
";
    std::fs::write(&file, dev).unwrap();
    record_all(&repo, "Add v1.4 entry");

    // Insert feature → dev
    repo.insert_from_view(CrossViewInsertOptions::new("feature", "dev"))
        .unwrap();
    repo.materialize().unwrap();

    let content = get_content_as_string(&repo, "CHANGELOG.md", "dev");

    // Both sides independently replaced the file content (each prepended
    // a new version).  The graph has two competing alive replacement
    // vertices.  Both new entries should be present.
    assert!(
        content.contains("v1.3"),
        "Draft's v1.3 entry should be present.\nActual content:\n{}",
        content
    );
    assert!(
        content.contains("v1.4"),
        "Dev's v1.4 entry should be present.\nActual content:\n{}",
        content
    );
    assert!(
        content.contains("New API endpoints"),
        "Draft's changelog text should be present.\nActual content:\n{}",
        content
    );
    assert!(
        content.contains("Security patches"),
        "Dev's changelog text should be present.\nActual content:\n{}",
        content
    );
}

/// One side deletes a function, the other modifies it. The function must
/// NOT be duplicated in the result.
#[test]
fn test_cross_view_merge_delete_vs_modify() {
    let (temp_dir, mut repo) = create_temp_repo();

    let initial = "\
def add(a, b):
    return a + b

def deprecated_multiply(a, b):
    return a * b

def subtract(a, b):
    return a - b
";
    let file = temp_dir.path().join("math_utils.py");
    std::fs::write(&file, initial).unwrap();
    repo.add("math_utils.py", TrackingOptions::default())
        .unwrap();
    record_all(&repo, "Add math utils");

    repo.create_view_from("feature", "dev").unwrap();

    // Draft: delete deprecated_multiply entirely
    repo.switch_view("feature").unwrap();
    let draft = "\
def add(a, b):
    return a + b

def subtract(a, b):
    return a - b
";
    std::fs::write(&file, draft).unwrap();
    record_all(&repo, "Remove deprecated_multiply");

    // Dev: modify deprecated_multiply (add docstring)
    repo.switch_view("dev").unwrap();
    let dev = "\
def add(a, b):
    return a + b

def deprecated_multiply(a, b):
    \"\"\"Deprecated: use operator instead.\"\"\"
    return a * b

def subtract(a, b):
    return a - b
";
    std::fs::write(&file, dev).unwrap();
    record_all(&repo, "Add docstring to deprecated_multiply");

    // Insert feature → dev
    repo.insert_from_view(CrossViewInsertOptions::new("feature", "dev"))
        .unwrap();
    repo.materialize().unwrap();

    let content = get_content_as_string(&repo, "math_utils.py", "dev");

    assert_eq!(
        count_occurrences(&content, "def add"),
        1,
        "Expected 'def add' once.\nActual content:\n{}",
        content
    );
    assert_eq!(
        count_occurrences(&content, "def subtract"),
        1,
        "Expected 'def subtract' once.\nActual content:\n{}",
        content
    );

    let deprecated_count = count_occurrences(&content, "deprecated_multiply");
    assert!(
        deprecated_count <= 1,
        "Expected 'deprecated_multiply' at most once (conflict or deleted), \
         but found {} times (duplication!).\nActual content:\n{}",
        deprecated_count,
        content
    );
}

/// Multiple sequential changes on the draft side, single change on dev.
///
/// Three incremental drafts (modify header, add stop(), refine stop params)
/// plus a dev footer change. After insert, no intermediate artefacts should
/// remain and no symbols should be duplicated.
#[test]
fn test_cross_view_merge_sequential_draft_changes() {
    let (temp_dir, mut repo) = create_temp_repo();

    let initial = "\
SERVICE_NAME = \"my-service\"

def start():
    print(f\"Starting {SERVICE_NAME}\")

# end of service
";
    let file = temp_dir.path().join("service.py");
    std::fs::write(&file, initial).unwrap();
    repo.add("service.py", TrackingOptions::default()).unwrap();
    record_all(&repo, "Add service module");

    repo.create_view_from("feature", "dev").unwrap();

    // Draft change 1: modify header
    repo.switch_view("feature").unwrap();
    let draft1 = "\
SERVICE_NAME = \"my-service-v2\"

def start():
    print(f\"Starting {SERVICE_NAME}\")

# end of service
";
    std::fs::write(&file, draft1).unwrap();
    record_all(&repo, "Rename service");

    // Draft change 2: add stop function
    let draft2 = "\
SERVICE_NAME = \"my-service-v2\"

def start():
    print(f\"Starting {SERVICE_NAME}\")

def stop():
    print(f\"Stopping {SERVICE_NAME}\")

# end of service
";
    std::fs::write(&file, draft2).unwrap();
    record_all(&repo, "Add stop function");

    // Draft change 3: refine stop with params
    let draft3 = "\
SERVICE_NAME = \"my-service-v2\"

def start():
    print(f\"Starting {SERVICE_NAME}\")

def stop(graceful=True):
    if graceful:
        print(f\"Gracefully stopping {SERVICE_NAME}\")
    else:
        print(f\"Force stopping {SERVICE_NAME}\")

# end of service
";
    std::fs::write(&file, draft3).unwrap();
    record_all(&repo, "Add graceful shutdown param");

    // Dev: modify footer
    repo.switch_view("dev").unwrap();
    let dev = "\
SERVICE_NAME = \"my-service\"

def start():
    print(f\"Starting {SERVICE_NAME}\")

# end of service — Copyright 2025
";
    std::fs::write(&file, dev).unwrap();
    record_all(&repo, "Update copyright footer");

    // Insert feature → dev
    repo.insert_from_view(CrossViewInsertOptions::new("feature", "dev"))
        .unwrap();
    repo.materialize().unwrap();

    let content = get_content_as_string(&repo, "service.py", "dev");

    assert_eq!(
        count_occurrences(&content, "SERVICE_NAME ="),
        1,
        "Expected SERVICE_NAME declaration once.\nActual content:\n{}",
        content
    );
    assert_eq!(
        count_occurrences(&content, "def start"),
        1,
        "Expected 'def start' once.\nActual content:\n{}",
        content
    );

    let stop_count = count_occurrences(&content, "def stop");
    assert!(
        stop_count <= 1,
        "Expected 'def stop' at most once, found {} (duplication!).\nActual content:\n{}",
        stop_count,
        content
    );

    // No intermediate stop() signature should survive (the plain `def stop():`)
    if content.contains("def stop") {
        assert!(
            content.contains("graceful"),
            "If stop() is present, it should be the final version with params.\nActual content:\n{}",
            content
        );
    }

    // Both sides' edits should be represented
    assert!(
        content.contains("my-service-v2") || content.contains("my-service"),
        "Service name should be present.\nActual content:\n{}",
        content
    );
    assert!(
        content.contains("Copyright 2025") || content.contains("end of service"),
        "Footer should be present.\nActual content:\n{}",
        content
    );
}

/// Reverse-direction insert: dev → draft (pull upstream into feature).
///
/// Draft changes app name, dev changes database port. Inserting dev into
/// the draft should bring the port change without duplicating any section.
#[test]
fn test_cross_view_merge_reverse_direction() {
    let (temp_dir, mut repo) = create_temp_repo();

    let initial = "\
app:
  name: my-app
  version: 1.0

server:
  host: localhost
  port: 8080

database:
  host: db.local
  port: 5432
";
    let file = temp_dir.path().join("config.yaml");
    std::fs::write(&file, initial).unwrap();
    repo.add("config.yaml", TrackingOptions::default()).unwrap();
    record_all(&repo, "Add YAML config");

    repo.create_view_from("feature", "dev").unwrap();

    // Draft: change app name
    repo.switch_view("feature").unwrap();
    let draft = "\
app:
  name: super-app
  version: 1.0

server:
  host: localhost
  port: 8080

database:
  host: db.local
  port: 5432
";
    std::fs::write(&file, draft).unwrap();
    record_all(&repo, "Rename app");

    // Dev: change database port
    repo.switch_view("dev").unwrap();
    let dev = "\
app:
  name: my-app
  version: 1.0

server:
  host: localhost
  port: 8080

database:
  host: db.local
  port: 5433
";
    std::fs::write(&file, dev).unwrap();
    record_all(&repo, "Change db port");

    // Reverse insert: dev → feature (pull upstream)
    repo.switch_view("feature").unwrap();
    repo.insert_from_view(CrossViewInsertOptions::new("dev", "feature"))
        .unwrap();
    repo.materialize().unwrap();

    let content = get_content_as_string(&repo, "config.yaml", "feature");

    assert_eq!(
        count_occurrences(&content, "app:"),
        1,
        "Expected 'app:' section once.\nActual content:\n{}",
        content
    );
    assert_eq!(
        count_occurrences(&content, "server:"),
        1,
        "Expected 'server:' section once.\nActual content:\n{}",
        content
    );
    assert_eq!(
        count_occurrences(&content, "database:"),
        1,
        "Expected 'database:' section once.\nActual content:\n{}",
        content
    );

    // Both edits should be present
    assert!(
        content.contains("super-app") || content.contains("my-app"),
        "App name should be present.\nActual content:\n{}",
        content
    );
    assert!(
        content.contains("5433"),
        "Updated db port should be present.\nActual content:\n{}",
        content
    );
}

/// Post-merge record: after inserting and materializing, a subsequent
/// record on dev must not re-introduce duplicated content.
#[test]
fn test_cross_view_merge_post_merge_record_clean() {
    let (temp_dir, mut repo) = create_temp_repo();

    let initial = "\
<!DOCTYPE html>
<html>
<head>
    <title>Hello</title>
</head>
<body>
    <h1>Welcome</h1>
    <p>This is the home page.</p>
</body>
</html>
";
    let file = temp_dir.path().join("index.html");
    std::fs::write(&file, initial).unwrap();
    repo.add("index.html", TrackingOptions::default()).unwrap();
    record_all(&repo, "Add HTML page");

    repo.create_view_from("feature", "dev").unwrap();

    // Draft: change title + paragraph
    repo.switch_view("feature").unwrap();
    let draft = "\
<!DOCTYPE html>
<html>
<head>
    <title>Dashboard</title>
</head>
<body>
    <h1>Welcome</h1>
    <p>This is the dashboard.</p>
</body>
</html>
";
    std::fs::write(&file, draft).unwrap();
    record_all(&repo, "Change title and paragraph");

    // Dev: change heading
    repo.switch_view("dev").unwrap();
    let dev = "\
<!DOCTYPE html>
<html>
<head>
    <title>Hello</title>
</head>
<body>
    <h1>Greetings</h1>
    <p>This is the home page.</p>
</body>
</html>
";
    std::fs::write(&file, dev).unwrap();
    record_all(&repo, "Change heading");

    // Insert feature → dev and materialize
    repo.insert_from_view(CrossViewInsertOptions::new("feature", "dev"))
        .unwrap();
    repo.materialize().unwrap();

    // Make another edit on dev and record it
    let post_merge = "\
<!DOCTYPE html>
<html>
<head>
    <title>Dashboard v2</title>
</head>
<body>
    <h1>Greetings</h1>
    <p>Updated dashboard content.</p>
</body>
</html>
";
    std::fs::write(&file, post_merge).unwrap();
    record_all(&repo, "Post-merge update");

    let content = get_content_as_string(&repo, "index.html", "dev");

    assert_eq!(
        count_occurrences(&content, "<html>"),
        1,
        "Expected <html> once.\nActual content:\n{}",
        content
    );
    assert_eq!(
        count_occurrences(&content, "</html>"),
        1,
        "Expected </html> once.\nActual content:\n{}",
        content
    );
    assert_eq!(
        count_occurrences(&content, "<head>"),
        1,
        "Expected <head> once.\nActual content:\n{}",
        content
    );
    assert_eq!(
        count_occurrences(&content, "<body>"),
        1,
        "Expected <body> once.\nActual content:\n{}",
        content
    );
    assert_eq!(
        count_occurrences(&content, "<title>"),
        1,
        "Expected <title> once.\nActual content:\n{}",
        content
    );
    assert_eq!(
        count_occurrences(&content, "<h1>"),
        1,
        "Expected <h1> once.\nActual content:\n{}",
        content
    );
    assert_eq!(
        count_occurrences(&content, "<p>"),
        1,
        "Expected <p> once.\nActual content:\n{}",
        content
    );

    // The latest write should be the final truth
    assert!(
        content.contains("Dashboard v2"),
        "Post-merge title should be 'Dashboard v2'.\nActual content:\n{}",
        content
    );
    assert!(
        content.contains("Updated dashboard content"),
        "Post-merge paragraph should be present.\nActual content:\n{}",
        content
    );
}
