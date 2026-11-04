#!/usr/bin/env bash
# 23_shared_view_parent_chain.sh — Shared view parent chain filter tests.
#
# The CLI creates draft children only; promotion produces a shared root.
# The shared-child scenario cannot be constructed through this CLI and must
# not be counted as passed. The draft-chain cases below remain useful coverage.
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
        _fail "$desc" "file does not exist: $path"
        return
    fi
    if grep -qF "$needle" "$path"; then
        _fail "$desc" "'$needle' should not be in $path but was found"
    else
        _pass "$desc"
    fi
}

assert_draft_parent() {
    local name="$1" parent="$2"
    require_capture "read view metadata for $name" atomic view list --all --json
    assert_success "$name is draft parented on $parent" python3 -c \
        'import json,sys; rows=json.loads(sys.argv[1])["views"]; assert any(x["name"] == sys.argv[2] and x["scope"] == "draft" and x["parent"] == sys.argv[3] for x in rows)' \
        "$COMMAND_OUTPUT" "$name" "$parent"
}

_skip "shared-scope child parent-chain coverage" "CLI creates draft children; promotion clears the parent"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Case 1: Draft child chain dev → staging"
# ═══════════════════════════════════════════════════════════════════════════
#
# Exercise the representable two-level view hierarchy:
#   dev (root shared, created by init)
#   └── staging (draft, parent=dev)
#
# Record a file on dev, insert to staging, record on staging.
# Switch between them.  Each view should show only its own content.

make_temp_repo "shared-parent-1"
init_repo

# Record file on dev (root shared)
create_file "base.txt" "I am the base file from dev"
require_success "add base.txt" atomic add base.txt
require_success "record base.txt" record_change "Add base.txt on dev"

# Create staging (draft, parent=dev)
require_success "create staging draft" new_view "staging" --draft --parent dev
assert_draft_parent "staging" "dev"
require_success "insert dev into staging" insert_from_view "dev" "staging"

# Switch to staging — must see base.txt
require_success "switch to staging" switch_view "staging"
assert_file_exists "base.txt visible on staging" "base.txt"
assert_file_content "base.txt has dev content on staging" "base.txt" "I am the base file from dev"
assert_no_conflict_markers "no markers on staging" "base.txt"

# Record something extra on staging
create_file "staging-feature.txt" "staging feature"
require_success "add staging-feature.txt" atomic add staging-feature.txt
require_success "record staging feature" record_change "Add staging-feature on staging"

# Switch to dev — should see base.txt but NOT staging-feature.txt
require_success "switch to dev" switch_view "dev"
assert_file_exists "base.txt on dev" "base.txt"
assert_file_content "base.txt content on dev" "base.txt" "I am the base file from dev"
assert_no_conflict_markers "no markers on dev" "base.txt"
assert_file_not_exists "staging-feature.txt NOT on dev" "staging-feature.txt"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Case 2: Draft chain — file edited at each level"
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
require_success "add config.ini" atomic add config.ini
require_success "record config on dev" record_change "Add config on dev"

# Create staging (draft, parent=dev), insert dev's changes, add a section
require_success "create staging draft" new_view "staging" --draft --parent dev
assert_draft_parent "staging" "dev"
require_success "insert dev into staging" insert_from_view "dev" "staging"
require_success "switch to staging" switch_view "staging"

cat >> config.ini << 'EOF'

[staging]
version = 1.0
EOF
require_success "record staging section" record_change "Add staging section"

# Staging should see both sections
assert_no_conflict_markers "no markers on staging" "config.ini"
assert_file_contains "staging has dev section" "config.ini" "[dev]"
assert_file_contains "staging has staging section" "config.ini" "[staging]"

# Switch to dev — should see ONLY dev section
require_success "switch to dev" switch_view "dev"
assert_no_conflict_markers "no markers on dev" "config.ini"
assert_file_contains "dev has dev section" "config.ini" "[dev]"
assert_file_not_contains "dev lacks staging section" "config.ini" "[staging]"

# Round-trip: switch staging → dev → staging
require_success "switch back to staging" switch_view "staging"
assert_no_conflict_markers "staging clean after round-trip" "config.ini"
assert_file_contains "staging still has both" "config.ini" "[staging]"

require_success "switch back to dev" switch_view "dev"
assert_no_conflict_markers "dev clean after round-trip" "config.ini"
assert_file_not_contains "dev still lacks staging" "config.ini" "[staging]"

# ═══════════════════════════════════════════════════════════════════════════
begin_section "Case 3: Draft from draft child — inherits full chain"
# ═══════════════════════════════════════════════════════════════════════════
#
# A draft view parented on staging (which is parented on dev) should
# see all ancestor changes transitively.  Switching between views
# should never produce conflict markers.

make_temp_repo "shared-parent-3"
init_repo

# Dev creates a file
create_file "library.rs" 'pub fn core_fn() -> i32 { 42 }'
require_success "add library.rs" atomic add library.rs
require_success "record library on dev" record_change "Add library on dev"

# Chain: dev → staging (draft)
require_success "create staging draft" new_view "staging" --draft --parent dev
assert_draft_parent "staging" "dev"
require_success "insert dev into staging" insert_from_view "dev" "staging"

# Draft from staging
require_success "create feature draft" new_view "feature" --draft --parent staging
assert_draft_parent "feature" "staging"
require_success "switch to feature" switch_view "feature"

# Feature should see the file from dev (via staging → dev chain)
assert_file_exists "library.rs on feature" "library.rs"
assert_file_contains "feature sees core_fn" "library.rs" "core_fn"
assert_no_conflict_markers "no markers on feature" "library.rs"

# Edit on feature
overwrite_file "library.rs" 'pub fn core_fn() -> i32 { 99 }'
require_success "record feature edit" record_change "Feature: return 99"

# Switch to staging — must see dev's original
require_success "switch to staging" switch_view "staging"
assert_file_exists "library.rs on staging" "library.rs"
assert_file_contains "staging has 42" "library.rs" "42"
assert_no_conflict_markers "no markers on staging" "library.rs"

# Switch to dev — must see dev's original
require_success "switch to dev" switch_view "dev"
assert_file_exists "library.rs on dev" "library.rs"
assert_file_contains "dev has 42" "library.rs" "42"
assert_no_conflict_markers "no markers on dev" "library.rs"

# ═══════════════════════════════════════════════════════════════════════════

print_summary
