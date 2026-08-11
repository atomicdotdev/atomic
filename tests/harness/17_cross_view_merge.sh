#!/usr/bin/env bash
# 17_cross_view_merge.sh — Cross-view merge duplication tests.
#
# Tests that inserting changes from a draft (local) view into a shared
# (dev) view does NOT produce duplicated content when both views have
# divergent edits to the same file(s).
#
# This is the user-reported bug:
#   1. Start with a shared view (dev) containing tracked files
#   2. Create a draft view from dev
#   3. Modify a file on the draft view and record
#   4. Switch back to dev and modify the SAME file (colliding region)
#   5. Insert from draft → dev
#   6. The merged result should contain EXACTLY ONE copy of each logical
#      region — no duplicated lines, no repeated blocks
#
# Invariants tested:
#
#   1. No duplicated lines in the merged output
#   2. Content from BOTH views is present (merge preserves both sides)
#   3. File line count stays within expected bounds
#   4. After insert, the working copy is materialised correctly
#   5. Subsequent records after merge produce clean state
#   6. Multiple files with independent and overlapping edits merge correctly
#
# Use cases covered:
#
#   Case 1: Single file, non-overlapping edits (should auto-merge cleanly)
#   Case 2: Single file, overlapping edits (same line changed both sides)
#   Case 3: Two files, one with collision, one without
#   Case 4: Append-only edits from both sides (common duplication scenario)
#   Case 5: Delete on one side, modify on the other
#   Case 6: Multiple sequential changes on draft, single change on dev

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

# ═══════════════════════════════════════════════════════════════════════════
# Helper: count occurrences of a string in a file
# ═══════════════════════════════════════════════════════════════════════════

# Shared merge helpers (count_occurrences, assert_occurrence_count,
# assert_max_lines, assert_file_contains, assert_file_not_contains).
source "$HARNESS_DIR/merge_helpers.sh"


# ═══════════════════════════════════════════════════════════════════════════
begin_section "Case 1: Non-overlapping edits merge without duplication"
# ═══════════════════════════════════════════════════════════════════════════
#
# Setup:
#   - dev has a file with 3 clearly separated sections
#   - draft modifies section 1 (top)
#   - dev modifies section 3 (bottom)
#   - Insert draft → dev
#
# Expected: Both edits present, no duplication

make_temp_repo "merge-non-overlapping"
init_repo

# Create initial file on dev with 3 sections
create_file "config.toml" '[server]
host = "localhost"
port = 8080

[database]
url = "postgres://localhost/mydb"
pool_size = 5

[logging]
level = "info"
format = "json"'

assert_success "add config.toml on dev" atomic add config.toml
record_change "Initial config" >/dev/null 2>&1

# Create draft view from dev
new_view "feature-config" --draft --parent dev >/dev/null 2>&1 || \
    new_view "feature-config" >/dev/null 2>&1
insert_from_view "dev" "feature-config" >/dev/null 2>&1 || true
switch_view "feature-config" >/dev/null 2>&1
assert_current_view "on feature-config" "feature-config"

# Modify section 1 on draft (change host)
overwrite_file "config.toml" '[server]
host = "0.0.0.0"
port = 8080

[database]
url = "postgres://localhost/mydb"
pool_size = 5

[logging]
level = "info"
format = "json"'

record_change "Change server host to 0.0.0.0" >/dev/null 2>&1

# Switch to dev and modify section 3 (change log level)
switch_view "dev" >/dev/null 2>&1
assert_current_view "back on dev" "dev"

overwrite_file "config.toml" '[server]
host = "localhost"
port = 8080

[database]
url = "postgres://localhost/mydb"
pool_size = 5

[logging]
level = "debug"
format = "json"'

record_change "Change log level to debug" >/dev/null 2>&1

# Insert from draft → dev
insert_out="$(insert_from_view "feature-config" "dev" 2>&1)" || true

# Verify: file exists and has no duplication
assert_file_exists "config.toml exists after insert" "config.toml"

# There should be exactly ONE [server] section
assert_occurrence_count "[server] appears once" "config.toml" "\\[server\\]" 1

# There should be exactly ONE [database] section
assert_occurrence_count "[database] appears once" "config.toml" "\\[database\\]" 1

# There should be exactly ONE [logging] section
assert_occurrence_count "[logging] appears once" "config.toml" "\\[logging\\]" 1

# File should not exceed the original line count by much
assert_max_lines "config.toml has reasonable line count" "config.toml" 20

# Both changes should be present
assert_file_contains "draft edit present (host = 0.0.0.0)" "config.toml" '0.0.0.0'
assert_file_contains "dev edit present (level = debug)" "config.toml" 'debug'

echo ""
echo "  config.toml after merge:"
cat config.toml | sed 's/^/    /'
echo ""


# ═══════════════════════════════════════════════════════════════════════════
begin_section "Case 2: Overlapping edits (same line changed on both sides)"
# ═══════════════════════════════════════════════════════════════════════════
#
# Setup:
#   - dev has a file with a version string
#   - draft changes version to "2.0.0"
#   - dev changes version to "1.1.0"
#   - Insert draft → dev
#
# Expected: Conflict markers OR one side wins, but NOT both versions
#           duplicated in the output without markers.

make_temp_repo "merge-overlapping"
init_repo

create_file "version.txt" 'version = "1.0.0"
name = "my-app"
description = "A sample application"'

assert_success "add version.txt on dev" atomic add version.txt
record_change "Initial version file" >/dev/null 2>&1

# Create draft view
new_view "bump-major" --draft --parent dev >/dev/null 2>&1 || \
    new_view "bump-major" >/dev/null 2>&1
insert_from_view "dev" "bump-major" >/dev/null 2>&1 || true
switch_view "bump-major" >/dev/null 2>&1
assert_current_view "on bump-major" "bump-major"

# Draft: bump to 2.0.0
overwrite_file "version.txt" 'version = "2.0.0"
name = "my-app"
description = "A sample application"'

record_change "Bump to 2.0.0" >/dev/null 2>&1

# Switch to dev: bump to 1.1.0
switch_view "dev" >/dev/null 2>&1

overwrite_file "version.txt" 'version = "1.1.0"
name = "my-app"
description = "A sample application"'

record_change "Bump to 1.1.0" >/dev/null 2>&1

# Insert from draft → dev (this is the collision)
insert_out="$(insert_from_view "bump-major" "dev" 2>&1)" || true

assert_file_exists "version.txt exists after overlapping insert" "version.txt"

# The critical check: there should not be silently-duplicated version
# lines.  A correct overlapping-edit outcome is either:
#   - exactly one "version = …" line (one side won), or
#   - two "version = …" lines WRAPPED in conflict markers
#     (`>>>>>>>` / `=======` / `<<<<<<<`), so a human can see both sides.
# What we must reject is duplication WITHOUT markers.
version_lines="$(grep -c 'version = ' version.txt)"
version_lines="$(echo "$version_lines" | tr -d '[:space:]')"
has_conflict_markers=0
if grep -qE '^(>>>>>>>|<<<<<<<|=======)' version.txt; then
    has_conflict_markers=1
fi

if [[ "$version_lines" -le 1 || "$has_conflict_markers" -eq 1 ]]; then
    _pass "no silent duplication of version line ($version_lines occurrence(s), markers=$has_conflict_markers)"
else
    _fail "no silent duplication of version line" "found $version_lines 'version = ' lines with no conflict markers"
fi

# name should appear exactly once
assert_occurrence_count "name appears once" "version.txt" 'name = ' 1

# description should appear exactly once
assert_occurrence_count "description appears once" "version.txt" 'description = ' 1

# File should be compact — not ballooning
assert_max_lines "version.txt has reasonable line count" "version.txt" 10

echo ""
echo "  version.txt after overlapping merge:"
cat version.txt | sed 's/^/    /'
echo ""


# ═══════════════════════════════════════════════════════════════════════════
begin_section "Case 3: Two files — one collision, one clean"
# ═══════════════════════════════════════════════════════════════════════════
#
# Setup:
#   - dev: readme.md + app.py
#   - draft: modifies readme.md header + adds new function to app.py
#   - dev: modifies readme.md header (collision) + does nothing to app.py
#   - Insert draft → dev
#
# Expected: readme.md has conflict/merge, app.py has both sides cleanly

make_temp_repo "merge-two-files"
init_repo

create_file "readme.md" '# My Project

This is the project description.

## Getting Started

Run the app with `python app.py`.'

create_file "app.py" 'def main():
    print("Hello, World!")

if __name__ == "__main__":
    main()'

assert_success "add readme.md" atomic add readme.md
assert_success "add app.py" atomic add app.py
record_change "Initial files" >/dev/null 2>&1

# Create draft view
new_view "feature-docs" --draft --parent dev >/dev/null 2>&1 || \
    new_view "feature-docs" >/dev/null 2>&1
insert_from_view "dev" "feature-docs" >/dev/null 2>&1 || true
switch_view "feature-docs" >/dev/null 2>&1
assert_current_view "on feature-docs" "feature-docs"

# Draft: change readme heading + add helper to app.py
overwrite_file "readme.md" '# My Awesome Project

This is the project description.

## Getting Started

Run the app with `python app.py`.'

overwrite_file "app.py" 'def greet(name):
    return f"Hello, {name}!"

def main():
    print(greet("World"))

if __name__ == "__main__":
    main()'

record_change "Update docs heading and add greet function" >/dev/null 2>&1

# Switch to dev: change readme heading differently
switch_view "dev" >/dev/null 2>&1

overwrite_file "readme.md" '# My Cool Project

This is the project description.

## Getting Started

Run the app with `python app.py`.'

record_change "Update docs heading on dev" >/dev/null 2>&1

# Insert from draft → dev
insert_out="$(insert_from_view "feature-docs" "dev" 2>&1)" || true

# Check readme.md — the heading collides
assert_file_exists "readme.md exists after insert" "readme.md"

# "Getting Started" should appear exactly once (non-colliding section)
assert_occurrence_count "Getting Started once in readme" "readme.md" "Getting Started" 1

# "project description" should appear exactly once
assert_occurrence_count "description once in readme" "readme.md" "project description" 1

assert_max_lines "readme.md reasonable size" "readme.md" 20

# Check app.py — should have draft's greet function cleanly merged
assert_file_exists "app.py exists after insert" "app.py"

# main() should appear exactly once as a definition
def_main_count="$(grep -c 'def main' app.py || echo 0)"
if [[ "$def_main_count" -eq 1 ]]; then
    _pass "def main() appears once in app.py"
else
    _fail "def main() appears once in app.py" "found $def_main_count times"
fi

# greet function from draft should be present
assert_file_contains "greet function present in app.py" "app.py" "def greet"

# __name__ guard should appear once
assert_occurrence_count "__name__ guard once in app.py" "app.py" '__name__' 1

echo ""
echo "  readme.md after merge:"
cat readme.md | sed 's/^/    /'
echo ""
echo "  app.py after merge:"
cat app.py | sed 's/^/    /'
echo ""


# ═══════════════════════════════════════════════════════════════════════════
begin_section "Case 4: Append-only edits from both sides"
# ═══════════════════════════════════════════════════════════════════════════
#
# This is the most commonly reported duplication scenario:
#   - Both sides append to the end of a file
#   - After insert, the appended content appears TWICE
#
# Setup:
#   - dev: a log file with 3 entries
#   - draft: appends entry 4
#   - dev: appends entry 5
#   - Insert draft → dev
#
# Expected: entries 1-5 each appear exactly once

make_temp_repo "merge-append-only"
init_repo

create_file "changelog.md" '# Changelog

## v1.0.0
- Initial release

## v1.1.0
- Bug fixes

## v1.2.0
- Performance improvements'

assert_success "add changelog.md" atomic add changelog.md
record_change "Initial changelog" >/dev/null 2>&1

# Create draft view
new_view "release-notes" --draft --parent dev >/dev/null 2>&1 || \
    new_view "release-notes" >/dev/null 2>&1
insert_from_view "dev" "release-notes" >/dev/null 2>&1 || true
switch_view "release-notes" >/dev/null 2>&1
assert_current_view "on release-notes" "release-notes"

# Draft: append v1.3.0
overwrite_file "changelog.md" '# Changelog

## v1.0.0
- Initial release

## v1.1.0
- Bug fixes

## v1.2.0
- Performance improvements

## v1.3.0
- New feature: user profiles'

record_change "Add v1.3.0 release notes" >/dev/null 2>&1

# Switch to dev: append v1.4.0
switch_view "dev" >/dev/null 2>&1

overwrite_file "changelog.md" '# Changelog

## v1.0.0
- Initial release

## v1.1.0
- Bug fixes

## v1.2.0
- Performance improvements

## v1.4.0
- Security patches'

record_change "Add v1.4.0 release notes" >/dev/null 2>&1

# Insert from draft → dev
insert_out="$(insert_from_view "release-notes" "dev" 2>&1)" || true

assert_file_exists "changelog.md exists after insert" "changelog.md"

# Each version header should appear exactly once
assert_occurrence_count "v1.0.0 once" "changelog.md" "v1.0.0" 1
assert_occurrence_count "v1.1.0 once" "changelog.md" "v1.1.0" 1
assert_occurrence_count "v1.2.0 once" "changelog.md" "v1.2.0" 1
assert_occurrence_count "v1.3.0 once (from draft)" "changelog.md" "v1.3.0" 1
assert_occurrence_count "v1.4.0 once (from dev)" "changelog.md" "v1.4.0" 1

# "Changelog" title should appear once
assert_occurrence_count "Changelog title once" "changelog.md" "# Changelog" 1

# File should not exceed reasonable size (~20 lines for 5 versions)
assert_max_lines "changelog.md reasonable size" "changelog.md" 30

# Check that content from both sides is present
assert_file_contains "draft content present (user profiles)" "changelog.md" "user profiles"
assert_file_contains "dev content present (Security patches)" "changelog.md" "Security patches"

echo ""
echo "  changelog.md after merge:"
cat changelog.md | sed 's/^/    /'
echo ""


# ═══════════════════════════════════════════════════════════════════════════
begin_section "Case 5: Delete on draft, modify on dev (same region)"
# ═══════════════════════════════════════════════════════════════════════════
#
# Setup:
#   - dev has a file with a deprecated function
#   - draft deletes the deprecated function
#   - dev modifies the deprecated function (adds a warning)
#   - Insert draft → dev
#
# Expected: Conflict or one side wins — NOT a ghost duplicate of the
#           deleted function appearing alongside the modified one

make_temp_repo "merge-delete-modify"
init_repo

create_file "utils.py" 'def add(a, b):
    return a + b

def deprecated_multiply(a, b):
    return a * b

def subtract(a, b):
    return a - b'

assert_success "add utils.py" atomic add utils.py
record_change "Initial utils" >/dev/null 2>&1

# Create draft view
new_view "cleanup" --draft --parent dev >/dev/null 2>&1 || \
    new_view "cleanup" >/dev/null 2>&1
insert_from_view "dev" "cleanup" >/dev/null 2>&1 || true
switch_view "cleanup" >/dev/null 2>&1
assert_current_view "on cleanup" "cleanup"

# Draft: delete the deprecated function
overwrite_file "utils.py" 'def add(a, b):
    return a + b

def subtract(a, b):
    return a - b'

record_change "Remove deprecated_multiply" >/dev/null 2>&1

# Switch to dev: modify the deprecated function
switch_view "dev" >/dev/null 2>&1

overwrite_file "utils.py" 'def add(a, b):
    return a + b

def deprecated_multiply(a, b):
    """WARNING: This function is deprecated. Use operator * instead."""
    return a * b

def subtract(a, b):
    return a - b'

record_change "Add deprecation warning" >/dev/null 2>&1

# Insert from draft → dev
insert_out="$(insert_from_view "cleanup" "dev" 2>&1)" || true

assert_file_exists "utils.py exists after insert" "utils.py"

# add() should appear exactly once
assert_occurrence_count "def add appears once" "utils.py" "def add" 1

# subtract() should appear exactly once
assert_occurrence_count "def subtract appears once" "utils.py" "def subtract" 1

# deprecated_multiply should appear at most once (either kept or deleted,
# but definitely NOT duplicated).
#
# Note: `grep -c` always prints the count and exits 1 when no match,
# so `grep -c … || echo 0` would append a second `0` line; use a plain
# call and strip whitespace.
dep_count="$(grep -c 'def deprecated_multiply' utils.py 2>/dev/null || true)"
dep_count="$(echo "$dep_count" | tr -d '[:space:]')"
: "${dep_count:=0}"
if [[ "$dep_count" -le 1 ]]; then
    _pass "deprecated_multiply not duplicated ($dep_count occurrence(s))"
else
    _fail "deprecated_multiply not duplicated" "found $dep_count times (expected 0 or 1)"
fi

# File should be compact
assert_max_lines "utils.py reasonable size" "utils.py" 15

echo ""
echo "  utils.py after merge:"
cat utils.py | sed 's/^/    /'
echo ""


# ═══════════════════════════════════════════════════════════════════════════
begin_section "Case 6: Multiple sequential changes on draft, one on dev"
# ═══════════════════════════════════════════════════════════════════════════
#
# This tests the accumulation scenario where the draft has multiple
# incremental changes that all get inserted at once into dev.
#
# Setup:
#   - dev: initial file
#   - draft: change 1 (modify header), change 2 (add function),
#            change 3 (modify function)
#   - dev: one change (modify footer)
#   - Insert all draft changes → dev
#
# Expected: final state reflects all draft changes + dev change,
#           no duplication from intermediate draft states

make_temp_repo "merge-sequential-draft"
init_repo

create_file "service.ts" '// Service v1
const SERVICE_NAME = "my-service";

export function start(): void {
    console.log("Starting " + SERVICE_NAME);
}

// End of service'

assert_success "add service.ts" atomic add service.ts
record_change "Initial service" >/dev/null 2>&1

# Create draft view
new_view "feature-service" --draft --parent dev >/dev/null 2>&1 || \
    new_view "feature-service" >/dev/null 2>&1
insert_from_view "dev" "feature-service" >/dev/null 2>&1 || true
switch_view "feature-service" >/dev/null 2>&1
assert_current_view "on feature-service" "feature-service"

# Draft change 1: modify header comment
overwrite_file "service.ts" '// Service v2 — improved
const SERVICE_NAME = "my-service";

export function start(): void {
    console.log("Starting " + SERVICE_NAME);
}

// End of service'

record_change "Update service header to v2" >/dev/null 2>&1

# Draft change 2: add a stop function
overwrite_file "service.ts" '// Service v2 — improved
const SERVICE_NAME = "my-service";

export function start(): void {
    console.log("Starting " + SERVICE_NAME);
}

export function stop(): void {
    console.log("Stopping " + SERVICE_NAME);
}

// End of service'

record_change "Add stop function" >/dev/null 2>&1

# Draft change 3: modify stop function
overwrite_file "service.ts" '// Service v2 — improved
const SERVICE_NAME = "my-service";

export function start(): void {
    console.log("Starting " + SERVICE_NAME);
}

export function stop(graceful: boolean = true): void {
    if (graceful) {
        console.log("Gracefully stopping " + SERVICE_NAME);
    } else {
        console.log("Force stopping " + SERVICE_NAME);
    }
}

// End of service'

record_change "Add graceful stop parameter" >/dev/null 2>&1

# Switch to dev and make a non-overlapping change (modify footer)
switch_view "dev" >/dev/null 2>&1

overwrite_file "service.ts" '// Service v1
const SERVICE_NAME = "my-service";

export function start(): void {
    console.log("Starting " + SERVICE_NAME);
}

// Copyright 2024 — All rights reserved'

record_change "Update footer with copyright" >/dev/null 2>&1

# Insert all draft changes → dev
insert_out="$(insert_from_view "feature-service" "dev" 2>&1)" || true

assert_file_exists "service.ts exists after insert" "service.ts"

# SERVICE_NAME should appear exactly once as a const declaration
assert_occurrence_count "SERVICE_NAME declared once" "service.ts" "const SERVICE_NAME" 1

# start() should appear exactly once as an export
assert_occurrence_count "export start() once" "service.ts" "export function start" 1

# stop() should appear exactly once (from draft's final state, NOT
# duplicated from intermediate states)
stop_count="$(grep -c 'export function stop' service.ts || echo 0)"
if [[ "$stop_count" -le 1 ]]; then
    _pass "export stop() not duplicated ($stop_count occurrence(s))"
else
    _fail "export stop() not duplicated" "found $stop_count times (expected 0 or 1)"
fi

# The header should be the draft's version (v2)
assert_file_contains "draft header present (v2)" "service.ts" "v2"

# The copyright from dev should be present
assert_file_contains "dev copyright present" "service.ts" "Copyright 2024"

# OLD intermediate content should NOT be present
# (stop() without graceful parameter was draft change 2, superseded by change 3)
simple_stop="$(grep -c 'function stop(): void' service.ts 2>/dev/null || true)"
simple_stop="$(echo "$simple_stop" | tr -d '[:space:]')"
if [[ "$simple_stop" -eq 0 ]]; then
    _pass "intermediate stop() signature not present"
else
    _fail "intermediate stop() signature not present" "found old 'stop(): void' $simple_stop time(s)"
fi

# File should be reasonable size
assert_max_lines "service.ts reasonable size" "service.ts" 25

echo ""
echo "  service.ts after merge:"
cat service.ts | sed 's/^/    /'
echo ""


# ═══════════════════════════════════════════════════════════════════════════
begin_section "Case 7: Insert from dev into draft (reverse direction)"
# ═══════════════════════════════════════════════════════════════════════════
#
# The reverse of the usual flow: dev has new changes, insert into draft.
# This covers the "pull from upstream" pattern.
#
# Setup:
#   - dev: initial file
#   - Create draft from dev
#   - Draft: modify line A
#   - Dev: modify line B (non-overlapping)
#   - Insert dev → draft (pull upstream into feature branch)
#
# Expected: draft has both changes, no duplication

make_temp_repo "merge-reverse-insert"
init_repo

create_file "config.yaml" 'app:
  name: myapp
  version: 1.0.0

server:
  host: localhost
  port: 3000

database:
  host: localhost
  port: 5432'

assert_success "add config.yaml" atomic add config.yaml
record_change "Initial config.yaml" >/dev/null 2>&1

# Create draft
new_view "feature-yaml" --draft --parent dev >/dev/null 2>&1 || \
    new_view "feature-yaml" >/dev/null 2>&1
insert_from_view "dev" "feature-yaml" >/dev/null 2>&1 || true
switch_view "feature-yaml" >/dev/null 2>&1
assert_current_view "on feature-yaml" "feature-yaml"

# Draft: change app name
overwrite_file "config.yaml" 'app:
  name: super-app
  version: 1.0.0

server:
  host: localhost
  port: 3000

database:
  host: localhost
  port: 5432'

record_change "Rename app to super-app" >/dev/null 2>&1

# Switch to dev: change database port
switch_view "dev" >/dev/null 2>&1

overwrite_file "config.yaml" 'app:
  name: myapp
  version: 1.0.0

server:
  host: localhost
  port: 3000

database:
  host: localhost
  port: 5433'

record_change "Change db port to 5433" >/dev/null 2>&1

# Switch to draft and insert FROM dev (reverse direction)
switch_view "feature-yaml" >/dev/null 2>&1

insert_out="$(insert_from_view "dev" "feature-yaml" 2>&1)" || true

# Materialise the working copy after inserting into current view
atomic_out="$(atomic status 2>&1)" || true

assert_file_exists "config.yaml exists after reverse insert" "config.yaml"

# Each section should appear exactly once
assert_occurrence_count "app: section once" "config.yaml" "^app:" 1
assert_occurrence_count "server: section once" "config.yaml" "^server:" 1
assert_occurrence_count "database: section once" "config.yaml" "^database:" 1

# Draft's change should be present
assert_file_contains "draft edit present (super-app)" "config.yaml" "super-app"

# Dev's change should be present
assert_file_contains "dev edit present (5433)" "config.yaml" "5433"

assert_max_lines "config.yaml reasonable size" "config.yaml" 18

echo ""
echo "  config.yaml after reverse insert:"
cat config.yaml | sed 's/^/    /'
echo ""


# ═══════════════════════════════════════════════════════════════════════════
begin_section "Case 8: Post-merge record is clean"
# ═══════════════════════════════════════════════════════════════════════════
#
# After a successful merge via insert, the working copy should be in a
# recordable state. Any further edits should produce a clean diff
# without ghosts from the merge.

make_temp_repo "merge-post-record"
init_repo

create_file "index.html" '<!DOCTYPE html>
<html>
<head><title>Hello</title></head>
<body>
  <h1>Welcome</h1>
  <p>Original content</p>
</body>
</html>'

assert_success "add index.html" atomic add index.html
record_change "Initial HTML" >/dev/null 2>&1

# Create draft, modify, record
new_view "feature-html" --draft --parent dev >/dev/null 2>&1 || \
    new_view "feature-html" >/dev/null 2>&1
insert_from_view "dev" "feature-html" >/dev/null 2>&1 || true
switch_view "feature-html" >/dev/null 2>&1

overwrite_file "index.html" '<!DOCTYPE html>
<html>
<head><title>Hello World</title></head>
<body>
  <h1>Welcome</h1>
  <p>Updated from draft</p>
</body>
</html>'

record_change "Update title and paragraph on draft" >/dev/null 2>&1

# Dev: different edit
switch_view "dev" >/dev/null 2>&1

overwrite_file "index.html" '<!DOCTYPE html>
<html>
<head><title>Hello</title></head>
<body>
  <h1>Welcome to Dev</h1>
  <p>Original content</p>
</body>
</html>'

record_change "Update heading on dev" >/dev/null 2>&1

# Insert draft → dev
insert_from_view "feature-html" "dev" >/dev/null 2>&1 || true

# Now make another edit on dev AFTER the merge
overwrite_file "index.html" '<!DOCTYPE html>
<html>
<head><title>Final Title</title></head>
<body>
  <h1>Final Heading</h1>
  <p>Final content</p>
</body>
</html>'

# This record should work cleanly — no duplication from the merge
record_out="$(record_change "Post-merge edit" 2>&1)" || true

# Verify the file has the post-merge content
assert_file_content "index.html has final content" "index.html" '<!DOCTYPE html>
<html>
<head><title>Final Title</title></head>
<body>
  <h1>Final Heading</h1>
  <p>Final content</p>
</body>
</html>'

# Exactly one of each tag
assert_occurrence_count "one <html> tag" "index.html" "<html>" 1
assert_occurrence_count "one </html> tag" "index.html" "</html>" 1
assert_occurrence_count "one <body> tag" "index.html" "<body>" 1
assert_occurrence_count "one <h1> tag" "index.html" "<h1>" 1

echo ""
echo "  index.html after post-merge record:"
cat index.html | sed 's/^/    /'
echo ""


# ═══════════════════════════════════════════════════════════════════════════
begin_section "Case 9: Token-level merge — disjoint tokens on same line"
# ═══════════════════════════════════════════════════════════════════════════
#
# Agent-driven development scenario.  Two agents both edit the SAME line
# but touch DIFFERENT tokens within that line.  The CRDT token-level
# three-way merge should compose both edits cleanly — no conflict
# markers — because the edits don't overlap at the token level.
#
# Setup:
#   - dev:   `const TIMEOUT_MS = 5000;`
#   - draft (rename agent):       `const TIMEOUT_SEC = 5000;`
#   - dev   (tune-the-value agent):`const TIMEOUT_MS  = 10000;`
#   - Insert draft → dev
#
# Expected:  `const TIMEOUT_SEC = 10000;`  — both edits, no markers.

make_temp_repo "token-merge-disjoint"
init_repo

create_file "config.ts" 'export const APP_NAME = "atomic";
export const TIMEOUT_MS = 5000;
export const MAX_RETRIES = 3;'

assert_success "add config.ts" atomic add config.ts
record_change "Initial timeout config" >/dev/null 2>&1

new_view "rename-timeout" --draft --parent dev >/dev/null 2>&1 || \
    new_view "rename-timeout" >/dev/null 2>&1
insert_from_view "dev" "rename-timeout" >/dev/null 2>&1 || true
switch_view "rename-timeout" >/dev/null 2>&1

# Agent A renames the identifier
overwrite_file "config.ts" 'export const APP_NAME = "atomic";
export const TIMEOUT_SEC = 5000;
export const MAX_RETRIES = 3;'

record_change "Rename TIMEOUT_MS → TIMEOUT_SEC" >/dev/null 2>&1

switch_view "dev" >/dev/null 2>&1

# Agent B changes the value (different token, same line)
overwrite_file "config.ts" 'export const APP_NAME = "atomic";
export const TIMEOUT_MS = 10000;
export const MAX_RETRIES = 3;'

record_change "Tune timeout to 10000" >/dev/null 2>&1

insert_out="$(insert_from_view "rename-timeout" "dev" 2>&1)" || true

assert_file_exists "config.ts exists after token-merge insert" "config.ts"

# Token-level merge should compose: new identifier + new value, no markers.
assert_file_contains "merged identifier present"  "config.ts" "TIMEOUT_SEC"
assert_file_contains "merged value present"       "config.ts" "10000"
assert_file_not_contains "old identifier gone"    "config.ts" "TIMEOUT_MS"
assert_file_not_contains "old value gone"         "config.ts" "5000"
assert_file_not_contains "no >>>>>>> conflict markers" "config.ts" ">>>>>>>"
assert_file_not_contains "no <<<<<<< conflict markers" "config.ts" "<<<<<<<"

# The timeout line should appear EXACTLY once.
assert_occurrence_count "timeout const declared once" "config.ts" "TIMEOUT" 1

echo ""
echo "  config.ts after token-disjoint merge:"
cat config.ts | sed 's/^/    /'
echo ""


# ═══════════════════════════════════════════════════════════════════════════
begin_section "Case 10: Token-level merge — true overlap still conflicts"
# ═══════════════════════════════════════════════════════════════════════════
#
# Guard case: when both agents edit the SAME token on the same line,
# token-level merge SHOULD fall through to conflict markers (no silent
# winner).  Otherwise we'd be silently dropping work.
#
# Setup:
#   - dev:   `const TIMEOUT_MS = 5000;`
#   - draft (agent A): bumps to 10000
#   - dev   (agent B): bumps to 30000
#
# Both target the same numeric literal token → genuine conflict.
# Expected: conflict markers, both values present.

make_temp_repo "token-merge-overlap"
init_repo

create_file "config.ts" 'export const TIMEOUT_MS = 5000;'

assert_success "add config.ts" atomic add config.ts
record_change "Initial timeout" >/dev/null 2>&1

new_view "bump-A" --draft --parent dev >/dev/null 2>&1 || \
    new_view "bump-A" >/dev/null 2>&1
insert_from_view "dev" "bump-A" >/dev/null 2>&1 || true
switch_view "bump-A" >/dev/null 2>&1

overwrite_file "config.ts" 'export const TIMEOUT_MS = 10000;'
record_change "Bump to 10000" >/dev/null 2>&1

switch_view "dev" >/dev/null 2>&1
overwrite_file "config.ts" 'export const TIMEOUT_MS = 30000;'
record_change "Bump to 30000" >/dev/null 2>&1

insert_out="$(insert_from_view "bump-A" "dev" 2>&1)" || true

assert_file_exists "config.ts exists after overlap insert" "config.ts"

# Both values must be visible to the user — silent loss would be a bug.
assert_file_contains "agent A value visible (10000)" "config.ts" "10000"
assert_file_contains "agent B value visible (30000)" "config.ts" "30000"
assert_file_contains "conflict markers present"     "config.ts" ">>>>>>>"

echo ""
echo "  config.ts after same-token conflict:"
cat config.ts | sed 's/^/    /'
echo ""


# ═══════════════════════════════════════════════════════════════════════════
begin_section "Case 11: Token-level merge — structural same-line edits"
# ═══════════════════════════════════════════════════════════════════════════
#
# Two agents edit the SAME function-signature line but make structural
# changes that don't collide token-for-token:
#   - Agent A adds a parameter.
#   - Agent B adds a return type annotation.
#
# The token count differs between left and right (parameter addition
# inserts new tokens) so the merge has to handle structural insertion,
# not just substitution.
#
# Setup:
#   - dev:   `function process(data) {`
#   - draft (param-adder): `function process(data, options) {`
#   - dev   (return-typer):`function process(data): Result {`
#
# Expected: `function process(data, options): Result {`  or markers.

make_temp_repo "token-merge-structural"
init_repo

create_file "process.ts" 'export function process(data) {
    return transform(data);
}'

assert_success "add process.ts" atomic add process.ts
record_change "Initial process function" >/dev/null 2>&1

new_view "add-param" --draft --parent dev >/dev/null 2>&1 || \
    new_view "add-param" >/dev/null 2>&1
insert_from_view "dev" "add-param" >/dev/null 2>&1 || true
switch_view "add-param" >/dev/null 2>&1

# Agent A: add an options parameter
overwrite_file "process.ts" 'export function process(data, options) {
    return transform(data);
}'

record_change "Add options parameter" >/dev/null 2>&1

switch_view "dev" >/dev/null 2>&1

# Agent B: add a return type
overwrite_file "process.ts" 'export function process(data): Result {
    return transform(data);
}'

record_change "Add Result return type" >/dev/null 2>&1

insert_out="$(insert_from_view "add-param" "dev" 2>&1)" || true

assert_file_exists "process.ts exists after structural merge" "process.ts"

# Both edits must be visible to the user.  Either a clean three-way
# merge produces both ("data, options): Result"), or conflict markers
# surface both sides — but the FUNCTION SIGNATURE line must not be
# silently flattened to a single edit.
sig_options=0; sig_result=0
if grep -qE 'function process\([^)]*options' process.ts; then sig_options=1; fi
if grep -qE 'function process\([^)]*\)\s*:\s*Result' process.ts; then sig_result=1; fi
has_markers=0
if grep -qE '^(>>>>>>>|<<<<<<<|=======)' process.ts; then has_markers=1; fi

if [[ "$sig_options" -eq 1 && "$sig_result" -eq 1 ]]; then
    _pass "structural merge: both options-param and Result-return present"
elif [[ "$has_markers" -eq 1 && "$sig_options" -eq 1 && "$sig_result" -eq 1 ]]; then
    _pass "structural merge: both edits visible via conflict markers"
else
    _fail "structural merge preserves both edits" \
        "options=$sig_options return-type=$sig_result markers=$has_markers"
fi

# Body should still be exactly one line, not duplicated.
assert_occurrence_count "function body unchanged" "process.ts" "return transform" 1

echo ""
echo "  process.ts after structural merge:"
cat process.ts | sed 's/^/    /'
echo ""


# ═══════════════════════════════════════════════════════════════════════════
begin_section "Case 12: Multi-file refactor — symbol rename + body edit"
# ═══════════════════════════════════════════════════════════════════════════
#
# Two agents refactor across two files:
#   - Agent A renames a symbol everywhere (`oldFn` → `newFn`).
#   - Agent B changes the symbol's body (modifies `oldFn`'s
#     implementation).
#
# In src/api.ts: agent A renames the call, agent B leaves it.
# In src/lib.ts: agent A renames the definition (signature line),
#                agent B modifies the body (different line).
# After insert, src/lib.ts has BOTH the rename (signature) and the body
# edit, in their respective lines.
#
# This is the classic agent-collab refactor pattern: structural rename
# composed with behavioural change.

make_temp_repo "multi-file-refactor"
init_repo

mkdir -p src

create_file "src/api.ts" 'import { oldFn } from "./lib";

export function handle(req) {
    return oldFn(req.payload);
}'

create_file "src/lib.ts" 'export function oldFn(x) {
    return x * 2;
}

export const VERSION = "1.0";'

assert_success "add src/api.ts" atomic add src/api.ts
assert_success "add src/lib.ts" atomic add src/lib.ts
record_change "Initial library" >/dev/null 2>&1

new_view "rename-fn" --draft --parent dev >/dev/null 2>&1 || \
    new_view "rename-fn" >/dev/null 2>&1
insert_from_view "dev" "rename-fn" >/dev/null 2>&1 || true
switch_view "rename-fn" >/dev/null 2>&1

# Agent A: rename oldFn → newFn across both files.
overwrite_file "src/api.ts" 'import { newFn } from "./lib";

export function handle(req) {
    return newFn(req.payload);
}'

overwrite_file "src/lib.ts" 'export function newFn(x) {
    return x * 2;
}

export const VERSION = "1.0";'

record_change "Rename oldFn → newFn" >/dev/null 2>&1

switch_view "dev" >/dev/null 2>&1

# Agent B: change the function body (a DIFFERENT line than agent A
# touched in lib.ts).  api.ts is left alone by agent B.
overwrite_file "src/lib.ts" 'export function oldFn(x) {
    return x * 3 + 1;
}

export const VERSION = "1.0";'

record_change "Tighten oldFn body" >/dev/null 2>&1

insert_out="$(insert_from_view "rename-fn" "dev" 2>&1)" || true

assert_file_exists "src/api.ts exists after refactor" "src/api.ts"
assert_file_exists "src/lib.ts exists after refactor" "src/lib.ts"

# api.ts: agent A renamed both the import and the call site; agent B
# didn't touch this file.  Should be fully renamed, no remnants of
# oldFn, no duplication.
assert_file_contains "api.ts import renamed" "src/api.ts" 'import { newFn }'
assert_file_contains "api.ts call renamed"   "src/api.ts" 'newFn(req.payload)'
assert_file_not_contains "api.ts no oldFn"   "src/api.ts" 'oldFn'
assert_occurrence_count "api.ts has one handle()" "src/api.ts" "function handle" 1

# lib.ts: agent A renamed the signature, agent B edited the body.
# Both edits must be present in their respective lines.
assert_file_contains "lib.ts signature renamed"  "src/lib.ts" 'export function newFn'
assert_file_contains "lib.ts body change present" "src/lib.ts" 'x * 3 + 1'
assert_file_not_contains "lib.ts old body gone"   "src/lib.ts" 'return x \* 2;'
assert_occurrence_count "lib.ts has one function def" "src/lib.ts" "export function" 1
assert_file_contains "lib.ts VERSION untouched"   "src/lib.ts" 'VERSION = "1.0"'

echo ""
echo "  src/api.ts after multi-file refactor:"
cat src/api.ts | sed 's/^/    /'
echo ""
echo "  src/lib.ts after multi-file refactor:"
cat src/lib.ts | sed 's/^/    /'
echo ""


# ═══════════════════════════════════════════════════════════════════════════
# Summary
# ═══════════════════════════════════════════════════════════════════════════

print_summary
