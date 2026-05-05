#!/usr/bin/env bash
# 15_storage_api.sh — Integration tests for the atomic-storage management API.
#
# Tests the full CLI ↔ atomic-storage round-trip for organizations, workspaces,
# projects, teams, members, and identity resolution.
#
# Prerequisites:
#   • A running atomic-storage server (started manually before running tests)
#   • PostgreSQL backing the server
#
# Usage:
#   # Against localhost (default)
#   ./tests/harness/run_all.sh 15
#
#   # Against a custom server URL
#   ATOMIC_SERVER_URL=https://testing.atomic.storage ./tests/harness/run_all.sh 15
#
#   # Skip if no server is running (CI-safe)
#   ./tests/harness/run_all.sh 15
#
# Environment variables:
#   ATOMIC_SERVER_URL  — Base URL of the storage server (default: http://localhost:8080)
#   ATOMIC_BIN         — Path to the atomic binary (auto-detected if unset)
#   ATOMIC_IDENTITY    — Name of a pre-existing identity to use (created if unset)
#   ATOMIC_SKIP_CLEANUP— Set to 1 to keep server-side resources for debugging

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

# Guard: helpers.sh initialises _HARNESS_TMPDIRS as an empty array, but on
# bash <4.4 an empty-array expansion under `set -u` is treated as unbound.
# Re-declare it here so the EXIT trap never hits an unbound variable, even
# when we skip early because the server is unreachable.
_HARNESS_TMPDIRS=("${_HARNESS_TMPDIRS[@]+"${_HARNESS_TMPDIRS[@]}"}")

# ── Configuration ───────────────────────────────────────────────────────────

SERVER_URL="${ATOMIC_SERVER_URL:-http://localhost:8080}"

# Unique suffix so parallel runs and re-runs don't collide.
RUN_ID="$(date +%s)-$$"

ALICE_NAME="test-alice-${RUN_ID}"
ALICE_EMAIL="${ALICE_NAME}@test.atomic.dev"
BOB_NAME="test-bob-${RUN_ID}"
BOB_EMAIL="${BOB_NAME}@test.atomic.dev"

ORG_NAME="test-org-${RUN_ID}"
WORKSPACE_NAME="test-ws-${RUN_ID}"
PROJECT_NAME="test-proj-${RUN_ID}"
TEAM_NAME="test-team-${RUN_ID}"

# Derived slugs (the server lowercases and replaces non-alnum with hyphens).
# Our names are already lowercase and hyphenated, so slug == name.
ORG_SLUG="$ORG_NAME"
WORKSPACE_SLUG="$WORKSPACE_NAME"
PROJECT_SLUG="$PROJECT_NAME"
TEAM_SLUG="$TEAM_NAME"

# ── Server availability check ──────────────────────────────────────────────

check_server() {
    curl --silent --max-time 5 --fail \
        "${SERVER_URL}/health" >/dev/null 2>&1 \
    || curl --silent --max-time 5 --fail \
        "${SERVER_URL}/" >/dev/null 2>&1
}

if ! check_server; then
    echo ""
    echo "${YELLOW}SKIPPING: atomic-storage server not reachable at ${SERVER_URL}${RESET}"
    echo "${YELLOW}  Start the server first, or set ATOMIC_SERVER_URL to point to a running instance.${RESET}"
    echo ""
    exit 0
fi

echo ""
echo "  Server URL: ${CYAN}${SERVER_URL}${RESET}"
echo "  Run ID:     ${CYAN}${RUN_ID}${RESET}"
echo ""

# ── Helpers ─────────────────────────────────────────────────────────────────

# Override the global atomic config home so we don't clobber the user's real
# identities and server config.
TEST_CONFIG_HOME="$(mktemp -d "${TMPDIR:-/tmp}/atomic-storage-test-config-XXXXXX")"
_HARNESS_TMPDIRS+=("$TEST_CONFIG_HOME")
export HOME="$TEST_CONFIG_HOME"

# Capture both stdout and exit code from an atomic command.
# Usage: run_atomic <args...>
#   Sets: LAST_OUTPUT, LAST_EXIT
run_atomic() {
    set +e
    LAST_OUTPUT="$("$ATOMIC_BIN" "$@" 2>&1)"
    LAST_EXIT=$?
    set -e
}

# Run atomic and assert success.
run_atomic_ok() {
    local desc="$1"; shift
    run_atomic "$@"
    if [[ $LAST_EXIT -eq 0 ]]; then
        _pass "$desc"
    else
        _fail "$desc" "exit=$LAST_EXIT output=$(echo "$LAST_OUTPUT" | head -5)"
    fi
}

# Run atomic and assert failure.
run_atomic_fail() {
    local desc="$1"; shift
    run_atomic "$@"
    if [[ $LAST_EXIT -ne 0 ]]; then
        _pass "$desc"
    else
        _fail "$desc" "expected failure but command succeeded. output=$(echo "$LAST_OUTPUT" | head -5)"
    fi
}

# Assert LAST_OUTPUT contains a substring.
assert_last_contains() {
    local desc="$1"
    local needle="$2"
    if echo "$LAST_OUTPUT" | grep -qiF "$needle"; then
        _pass "$desc"
    else
        _fail "$desc" "output did not contain '$needle'. Got: $(echo "$LAST_OUTPUT" | head -5)"
    fi
}

# Assert LAST_OUTPUT does NOT contain a substring.
assert_last_not_contains() {
    local desc="$1"
    local needle="$2"
    if echo "$LAST_OUTPUT" | grep -qiF "$needle"; then
        _fail "$desc" "output should not contain '$needle' but does"
    else
        _pass "$desc"
    fi
}

# Switch which identity is the default (for multi-user tests).
use_identity() {
    local name="$1"
    "$ATOMIC_BIN" identity default "$name" >/dev/null 2>&1
}


# ═══════════════════════════════════════════════════════════════════════════
begin_section "Setup: Create and register identities"
# ═══════════════════════════════════════════════════════════════════════════

# Create Alice.
run_atomic_ok "Create Alice identity" \
    identity new "$ALICE_NAME" --email "$ALICE_EMAIL" --set-default

# Register Alice with the server.
run_atomic_ok "Register Alice with server" \
    identity register "$SERVER_URL"

assert_last_contains "Registration response includes slug" "$ALICE_NAME"

# Verify server config was written.
if [[ -f "$TEST_CONFIG_HOME/.atomic/config.toml" ]]; then
    _pass "Global config file created"
    if grep -q "$ALICE_NAME" "$TEST_CONFIG_HOME/.atomic/config.toml"; then
        _pass "Config contains default_org"
    else
        _fail "Config contains default_org" \
            "config.toml does not mention $ALICE_NAME"
    fi
else
    _fail "Global config file created" "file not found"
fi

# Create Bob (second user for member tests).
run_atomic_ok "Create Bob identity" \
    identity new "$BOB_NAME" --email "$BOB_EMAIL"

# Register Bob.
use_identity "$BOB_NAME"
run_atomic_ok "Register Bob with server" \
    identity register "$SERVER_URL"

# Switch back to Alice for the rest of the tests.
use_identity "$ALICE_NAME"


# ═══════════════════════════════════════════════════════════════════════════
begin_section "Organization: Show personal org"
# ═══════════════════════════════════════════════════════════════════════════

run_atomic_ok "Show personal org" \
    org show

assert_last_contains "Shows org slug" "$ALICE_NAME"


# ═══════════════════════════════════════════════════════════════════════════
begin_section "Organization: Create team org"
# ═══════════════════════════════════════════════════════════════════════════

run_atomic_ok "Create team org" \
    org create "$ORG_NAME" --email "team-${RUN_ID}@test.atomic.dev"

assert_last_contains "Create response includes org slug" "$ORG_SLUG"

# Switch to the new org so subsequent commands target it.
run_atomic_ok "Switch to team org" \
    org switch "$ORG_SLUG"

# Verify the switch persisted.
run_atomic_ok "Show team org after switch" \
    org show

assert_last_contains "Show reflects switched org" "$ORG_SLUG"


# ═══════════════════════════════════════════════════════════════════════════
begin_section "Organization: Update org"
# ═══════════════════════════════════════════════════════════════════════════

UPDATED_ORG_EMAIL="updated-${RUN_ID}@test.atomic.dev"
run_atomic_ok "Update org email" \
    org update "$ORG_SLUG" --email "$UPDATED_ORG_EMAIL"

assert_last_contains "Update response shows new email" "$UPDATED_ORG_EMAIL"


# ═══════════════════════════════════════════════════════════════════════════
begin_section "Organization: Duplicate org creation fails"
# ═══════════════════════════════════════════════════════════════════════════

run_atomic_fail "Create duplicate org fails" \
    org create "$ORG_NAME"


# ═══════════════════════════════════════════════════════════════════════════
begin_section "Organization Members: Add Bob by name"
# ═══════════════════════════════════════════════════════════════════════════

# Add Bob by identity name (tests identity resolution).
run_atomic_ok "Add Bob to org by name" \
    org member add "$BOB_NAME" --role admin --org "$ORG_SLUG"

# List members — should include both Alice (owner) and Bob (admin).
run_atomic_ok "List org members" \
    org member list --org "$ORG_SLUG"

assert_last_contains "Member list shows admin role" "admin"


# ═══════════════════════════════════════════════════════════════════════════
begin_section "Organization Members: Update and remove member"
# ═══════════════════════════════════════════════════════════════════════════

run_atomic_ok "Update Bob to member role" \
    org member update "$BOB_NAME" --role member --org "$ORG_SLUG"

assert_last_contains "Update shows member role" "member"

run_atomic_ok "Remove Bob from org" \
    org member remove "$BOB_NAME" --force --org "$ORG_SLUG"


# ═══════════════════════════════════════════════════════════════════════════
begin_section "Organization Members: Re-add Bob for team tests"
# ═══════════════════════════════════════════════════════════════════════════

run_atomic_ok "Re-add Bob as member" \
    org member add "$BOB_NAME" --role member --org "$ORG_SLUG"


# ═══════════════════════════════════════════════════════════════════════════
begin_section "Workspace: CRUD"
# ═══════════════════════════════════════════════════════════════════════════

# Create.
run_atomic_ok "Create workspace" \
    workspace create "$WORKSPACE_NAME" --visibility private --org "$ORG_SLUG"

assert_last_contains "Create response includes slug" "$WORKSPACE_SLUG"

# Show.
run_atomic_ok "Show workspace" \
    workspace show "$WORKSPACE_SLUG" --org "$ORG_SLUG"

assert_last_contains "Show includes workspace name" "$WORKSPACE_NAME"
assert_last_contains "Show includes visibility" "private"

# List.
run_atomic_ok "List workspaces" \
    workspace list --org "$ORG_SLUG"

assert_last_contains "List includes workspace" "$WORKSPACE_SLUG"

# Update.
run_atomic_ok "Update workspace visibility" \
    workspace update "$WORKSPACE_SLUG" --visibility public --org "$ORG_SLUG"

# Verify the update stuck.
run_atomic_ok "Show updated workspace" \
    workspace show "$WORKSPACE_SLUG" --org "$ORG_SLUG"

assert_last_contains "Updated visibility is public" "public"


# ═══════════════════════════════════════════════════════════════════════════
begin_section "Workspace: Duplicate creation fails"
# ═══════════════════════════════════════════════════════════════════════════

run_atomic_fail "Create duplicate workspace fails" \
    workspace create "$WORKSPACE_NAME" --org "$ORG_SLUG"


# ═══════════════════════════════════════════════════════════════════════════
begin_section "Workspace: JSON output"
# ═══════════════════════════════════════════════════════════════════════════

run_atomic_ok "List workspaces as JSON" \
    workspace list --org "$ORG_SLUG" --format json

# JSON output should parse (basic check: starts with [ or {).
if echo "$LAST_OUTPUT" | head -1 | grep -qE '^\[|\{'; then
    _pass "JSON output is valid-ish"
else
    _fail "JSON output is valid-ish" \
        "output does not look like JSON: $(echo "$LAST_OUTPUT" | head -1)"
fi


# ═══════════════════════════════════════════════════════════════════════════
begin_section "Project: CRUD"
# ═══════════════════════════════════════════════════════════════════════════

# Create.
run_atomic_ok "Create project" \
    project create "$PROJECT_NAME" \
        --workspace "$WORKSPACE_SLUG" \
        --kind rust \
        --org "$ORG_SLUG"

assert_last_contains "Create response includes project slug" "$PROJECT_SLUG"

# Show (using ws/project path format).
run_atomic_ok "Show project" \
    project show "${WORKSPACE_SLUG}/${PROJECT_SLUG}" --org "$ORG_SLUG"

assert_last_contains "Show includes project name" "$PROJECT_NAME"
assert_last_contains "Show includes default view" "dev"

# List.
run_atomic_ok "List projects" \
    project list --workspace "$WORKSPACE_SLUG" --org "$ORG_SLUG"

assert_last_contains "List includes project" "$PROJECT_SLUG"

# Update.
run_atomic_ok "Update project description" \
    project update "${WORKSPACE_SLUG}/${PROJECT_SLUG}" \
        --description "Integration test project" \
        --org "$ORG_SLUG"


# ═══════════════════════════════════════════════════════════════════════════
begin_section "Project: Duplicate creation fails"
# ═══════════════════════════════════════════════════════════════════════════

run_atomic_fail "Create duplicate project fails" \
    project create "$PROJECT_NAME" \
        --workspace "$WORKSPACE_SLUG" \
        --org "$ORG_SLUG"


# ═══════════════════════════════════════════════════════════════════════════
begin_section "Team: CRUD"
# ═══════════════════════════════════════════════════════════════════════════

# Create.
run_atomic_ok "Create team" \
    team create "$TEAM_NAME" \
        --description "Integration test team" \
        --visibility visible \
        --org "$ORG_SLUG"

assert_last_contains "Create response includes team slug" "$TEAM_SLUG"

# Show.
run_atomic_ok "Show team" \
    team show "$TEAM_SLUG" --org "$ORG_SLUG"

assert_last_contains "Show includes team name" "$TEAM_NAME"
assert_last_contains "Show includes visibility" "visible"

# List.
run_atomic_ok "List teams" \
    team list --org "$ORG_SLUG"

assert_last_contains "List includes team" "$TEAM_SLUG"

# Update.
run_atomic_ok "Update team to secret" \
    team update "$TEAM_SLUG" \
        --visibility secret \
        --org "$ORG_SLUG"

run_atomic_ok "Show updated team" \
    team show "$TEAM_SLUG" --org "$ORG_SLUG"

assert_last_contains "Updated visibility is secret" "secret"


# ═══════════════════════════════════════════════════════════════════════════
begin_section "Team: Duplicate creation fails"
# ═══════════════════════════════════════════════════════════════════════════

run_atomic_fail "Create duplicate team fails" \
    team create "$TEAM_NAME" --org "$ORG_SLUG"


# ═══════════════════════════════════════════════════════════════════════════
begin_section "Team Members: Add, update, list, remove"
# ═══════════════════════════════════════════════════════════════════════════

# Add Bob to the team by name (identity resolution).
run_atomic_ok "Add Bob to team by name" \
    team member add "$TEAM_SLUG" "$BOB_NAME" \
        --role contributor \
        --org "$ORG_SLUG"

# List members.
run_atomic_ok "List team members" \
    team member list "$TEAM_SLUG" --org "$ORG_SLUG"

assert_last_contains "Team member list shows contributor" "contributor"

# Update role.
run_atomic_ok "Update Bob to maintainer" \
    team member update "$TEAM_SLUG" "$BOB_NAME" \
        --role maintainer \
        --org "$ORG_SLUG"

assert_last_contains "Update shows maintainer role" "maintainer"

# Test other roles.
run_atomic_ok "Update Bob to collaborator" \
    team member update "$TEAM_SLUG" "$BOB_NAME" \
        --role collaborator \
        --org "$ORG_SLUG"

assert_last_contains "Update shows collaborator role" "collaborator"

run_atomic_ok "Update Bob to consumer" \
    team member update "$TEAM_SLUG" "$BOB_NAME" \
        --role consumer \
        --org "$ORG_SLUG"

assert_last_contains "Update shows consumer role" "consumer"

# Remove.
run_atomic_ok "Remove Bob from team" \
    team member remove "$TEAM_SLUG" "$BOB_NAME" \
        --force \
        --org "$ORG_SLUG"


# ═══════════════════════════════════════════════════════════════════════════
begin_section "Team Members: Invalid role rejected"
# ═══════════════════════════════════════════════════════════════════════════

run_atomic_fail "Add with invalid role fails" \
    team member add "$TEAM_SLUG" "$BOB_NAME" \
        --role superadmin \
        --org "$ORG_SLUG"


# ═══════════════════════════════════════════════════════════════════════════
begin_section "Project Init: Full workflow (create repo + remote)"
# ═══════════════════════════════════════════════════════════════════════════

INIT_PROJECT_NAME="init-proj-${RUN_ID}"
make_temp_repo "init-test"
init_repo

run_atomic_ok "Project init creates project and sets remote" \
    project init "$INIT_PROJECT_NAME" \
        --workspace "$WORKSPACE_SLUG" \
        --kind rust \
        --org "$ORG_SLUG"

assert_last_contains "Init mentions remote configured" "origin"

# Verify the remote was set in the local repo config.
run_atomic remote -v
if echo "$LAST_OUTPUT" | grep -qF "$WORKSPACE_SLUG"; then
    _pass "Local remote URL contains workspace slug"
else
    _fail "Local remote URL contains workspace slug" \
        "remote output: $(echo "$LAST_OUTPUT" | head -3)"
fi


# ═══════════════════════════════════════════════════════════════════════════
begin_section "Multi-user: Bob operates on the team org"
# ═══════════════════════════════════════════════════════════════════════════

# Switch to Bob's identity.
use_identity "$BOB_NAME"

# Bob should be able to list workspaces in the org (he's a member).
run_atomic_ok "Bob lists workspaces" \
    workspace list --org "$ORG_SLUG"

assert_last_contains "Bob sees the workspace" "$WORKSPACE_SLUG"

# Bob should be able to list projects.
run_atomic_ok "Bob lists projects" \
    project list --workspace "$WORKSPACE_SLUG" --org "$ORG_SLUG"

assert_last_contains "Bob sees the project" "$PROJECT_SLUG"

# Switch back to Alice.
use_identity "$ALICE_NAME"


# ═══════════════════════════════════════════════════════════════════════════
begin_section "Cleanup: Delete resources"
# ═══════════════════════════════════════════════════════════════════════════

if [[ "${ATOMIC_SKIP_CLEANUP:-0}" == "1" ]]; then
    echo "  ${YELLOW}⊘ ATOMIC_SKIP_CLEANUP is set — skipping server-side cleanup${RESET}"
else
    # Delete in reverse dependency order: project → workspace → team → org.

    run_atomic_ok "Delete init project" \
        project delete "${WORKSPACE_SLUG}/${INIT_PROJECT_NAME}" \
            --force --org "$ORG_SLUG"

    run_atomic_ok "Delete project" \
        project delete "${WORKSPACE_SLUG}/${PROJECT_SLUG}" \
            --force --org "$ORG_SLUG"

    run_atomic_ok "Delete workspace" \
        workspace delete "$WORKSPACE_SLUG" --force --org "$ORG_SLUG"

    # Verify workspace is gone.
    run_atomic_fail "Deleted workspace is gone" \
        workspace show "$WORKSPACE_SLUG" --org "$ORG_SLUG"

    run_atomic_ok "Delete team" \
        team delete "$TEAM_SLUG" --force --org "$ORG_SLUG"

    run_atomic_ok "Delete team org" \
        org delete "$ORG_SLUG" --force --org "$ORG_SLUG"

    # Verify org is gone.
    run_atomic_fail "Deleted org is gone" \
        org show "$ORG_SLUG"
fi


# ═══════════════════════════════════════════════════════════════════════════
begin_section "Edge Cases"
# ═══════════════════════════════════════════════════════════════════════════

# Operations against nonexistent resources should fail cleanly.
run_atomic_fail "Show nonexistent org fails" \
    org show "nonexistent-org-${RUN_ID}"

run_atomic_fail "Show nonexistent workspace fails" \
    workspace show "nonexistent-ws-${RUN_ID}"

run_atomic_fail "Show nonexistent project fails" \
    project show "nonexistent-ws-${RUN_ID}/nonexistent-proj-${RUN_ID}"

run_atomic_fail "Show nonexistent team fails" \
    team show "nonexistent-team-${RUN_ID}"

# Adding a nonexistent identity should fail.
run_atomic_fail "Add nonexistent member fails" \
    org member add "nobody-${RUN_ID}@ghost.dev" --org "$ALICE_NAME"


# ═══════════════════════════════════════════════════════════════════════════

print_summary

if [[ $TESTS_FAILED -gt 0 ]]; then
    exit 1
fi
exit 0
