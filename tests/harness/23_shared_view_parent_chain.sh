#!/usr/bin/env bash
# 23_shared_view_parent_chain.sh — Shared view parent chain filter tests.
#
# Validates that shared views with parent chains properly include
# all ancestor changes in their change filter during materialization.
#
# Bug: collect_visible_change_ids only walks parent chain for draft views.
# Shared views with parents (e.g. staging → dev) only see their own
# VIEW_CHANGES, missing ancestor changes.  This could produce incomplete
# content or conflict markers.

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

# ── Helpers ─────────────────────────────────────────────────────────────────

assert_no_conflict_markers() {
    local desc="$1"
    local path="$2"
    if [[ ! -f "$path" ]]; then
        _fail "$desc" "file does not exist: $path"
        return
    fi
    if grep -qE '>>>>>>|<<<<<<|=======' "$path" 2>/dev/null; then
        local content
        content="$(cat "$path")"
        _fail "$desc" "conflict markers found in $path. Content: $(echo "$content" | head -20)"
    else
        _pass "$desc"
    fi
}

assert_file_contains() {
    local desc="$1"
    local path="$2"
    local needle="$3"
    if [[ ! -f "$path" ]]; then
        _fail "$desc" "file does not exist: $path"
        return
    fi
    if grep -qF "$needle" "$path"; then
        _pass "$desc"
    else
        _fail "$desc" "'$needle' not found in $path. Content: $(cat "$path" | head -10)"
    fi
}

assert_file_not_contains() {
    local desc="$1"
    local path="$2"
    local needle="$3"
    if [[ ! -f "$path" ]]; then
        _pass "$desc"
        return
    fi
    if grep -qF "$needle" "$path"; then
        _fail "$desc" "'$needle' should not be in $path but was found"
    else
        _pass "$desc"
    fi
}

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Case 1: Shared chain dev → staging"
# ═══════════════════════════════════════════════════════════════════════════
#
# Build a two-level shared view hierarchy:
#   dev (root shared, created by init)
#   └── staging (shared, parent=dev)
#
# Record a file on dev, insert to staging, record on staging.
# Switch between them.  Each view should show only its own content.

make_temp_repo "shared-parent-1"
init_repo

# Record file on dev (root shared)
create_file "base.txt" "I am the base file from dev"
assert_success "add base.txt" atomic add base.txt
record_change "Add base.txt on dev" >/dev/null 2>&1 || true

# Create staging (shared, parent=dev)
new_view "staging" --parent dev >/dev/null 2>&1 || true
insert_from_view "dev" "staging" >/dev/null 2>&1 || true

# Switch to staging — must see base.txt
switch_view "staging" >/dev/null 2>&1 || true
assert_file_exists "base.txt visible on staging" "base.txt"
assert_file_content "base.txt has dev content on staging" "base.txt" "I am the base file from dev"
assert_no_conflict_markers "no markers on staging" "base.txt"

# Record something extra on staging
create_file "staging-feature.txt" "staging feature"
assert_success "add staging-feature.txt" atomic add staging-feature.txt
record_change "Add staging-feature on staging" >/dev/null 2>&1 || true

# Switch to dev — should see base.txt but NOT staging-feature.txt
switch_view "dev" >/dev/null 2>&1 || true
assert_file_exists "base.txt on dev" "base.txt"
assert_file_content "base.txt content on dev" "base.txt" "I am the base file from dev"
assert_no_conflict_markers "no markers on dev" "base.txt"
assert_file_not_exists "staging-feature.txt NOT on dev" "staging-feature.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Case 2: Shared chain — file edited at each level"
# ═══════════════════════════════════════════════════════════════════════════
#
# dev has a file, staging adds to it.
# Each level should show only its own accumulated content.
# Dev must NOT show staging's additions.

make_temp_repo "shared-parent-2"
init_repo

# Create multi-line file on dev
cat > config.ini << 'EOF'
[dev]
setting = base
EOF
assert_success "add config.ini" atomic add config.ini
record_change "Add config on dev" >/dev/null 2>&1 || true

# Create staging (shared, parent=dev), insert dev's changes, add a section
new_view "staging" --parent dev >/dev/null 2>&1 || true
insert_from_view "dev" "staging" >/dev/null 2>&1 || true
switch_view "staging" >/dev/null 2>&1 || true

cat >> config.ini << 'EOF'

[staging]
version = 1.0
EOF
record_change "Add staging section" >/dev/null 2>&1 || true

# Staging should see both sections
assert_no_conflict_markers "no markers on staging" "config.ini"
assert_file_contains "staging has dev section" "config.ini" "[dev]"
assert_file_contains "staging has staging section" "config.ini" "[staging]"

# Switch to dev — should see ONLY dev section
switch_view "dev" >/dev/null 2>&1 || true
assert_no_conflict_markers "no markers on dev" "config.ini"
assert_file_contains "dev has dev section" "config.ini" "[dev]"
assert_file_not_contains "dev lacks staging section" "config.ini" "[staging]"

# Round-trip: switch staging → dev → staging
switch_view "staging" >/dev/null 2>&1 || true
assert_no_conflict_markers "staging clean after round-trip" "config.ini"
assert_file_contains "staging still has both" "config.ini" "[staging]"

switch_view "dev" >/dev/null 2>&1 || true
assert_no_conflict_markers "dev clean after round-trip" "config.ini"
assert_file_not_contains "dev still lacks staging" "config.ini" "[staging]"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Case 3: Draft from shared child — inherits full chain"
# ═══════════════════════════════════════════════════════════════════════════
#
# A draft view parented on staging (which is parented on dev) should
# see all ancestor changes transitively.  Switching between views
# should never produce conflict markers.

make_temp_repo "shared-parent-3"
init_repo

# Dev creates a file
create_file "library.rs" 'pub fn core_fn() -> i32 { 42 }'
assert_success "add library.rs" atomic add library.rs
record_change "Add library on dev" >/dev/null 2>&1 || true

# Chain: dev → staging (shared)
new_view "staging" --parent dev >/dev/null 2>&1 || true
insert_from_view "dev" "staging" >/dev/null 2>&1 || true

# Draft from staging
new_view "feature" --draft --parent staging >/dev/null 2>&1 || true
switch_view "feature" >/dev/null 2>&1 || true

# Feature should see the file from dev (via staging → dev chain)
assert_file_exists "library.rs on feature" "library.rs"
assert_file_contains "feature sees core_fn" "library.rs" "core_fn"
assert_no_conflict_markers "no markers on feature" "library.rs"

# Edit on feature
overwrite_file "library.rs" 'pub fn core_fn() -> i32 { 99 }'
record_change "Feature: return 99" >/dev/null 2>&1 || true

# Switch to staging — must see dev's original
switch_view "staging" >/dev/null 2>&1 || true
assert_file_exists "library.rs on staging" "library.rs"
assert_file_contains "staging has 42" "library.rs" "42"
assert_no_conflict_markers "no markers on staging" "library.rs"

# Switch to dev — must see dev's original
switch_view "dev" >/dev/null 2>&1 || true
assert_file_exists "library.rs on dev" "library.rs"
assert_file_contains "dev has 42" "library.rs" "42"
assert_no_conflict_markers "no markers on dev" "library.rs"

# ═══════════════════════════════════════════════════════════════════════════

print_summary
