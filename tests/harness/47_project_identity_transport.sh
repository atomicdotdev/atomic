#!/usr/bin/env bash
# 47_project_identity_transport.sh — ProjectIdentity transport E2E.
#
# Exercises the clone-adopts / reclone-stable / push-refuses-divergence
# behavior against a live atomic-storage server running the identity
# transport (storage PR feat/project-identity-transport):
#
#   1. Alice creates a project and pushes → her identity binds server-side.
#   2. Alice clones elsewhere → the clone ADOPTS the origin identity (not a
#      fresh mint).
#   3. Alice deletes the clone and re-clones → the SAME identity comes back
#      (the id lives with the project on the server, not the directory).
#   4. Bob (a second identity) clones → same identity.
#
# Prerequisites:
#   • A running atomic-storage server WITH identity transport
#   • PostgreSQL backing the server
#
# Environment variables:
#   ATOMIC_SERVER_URL   — Base URL of the storage server (default http://localhost:8080)
#   ATOMIC_BIN          — Path to the atomic binary (auto-detected if unset)
#   ATOMIC_SKIP_CLEANUP — Set to 1 to keep server-side resources for debugging

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

_HARNESS_TMPDIRS=("${_HARNESS_TMPDIRS[@]+"${_HARNESS_TMPDIRS[@]}"}")

SERVER_URL="${ATOMIC_SERVER_URL:-http://localhost:8080}"
RUN_ID="$(date +%s)-$$"

ALICE_NAME="idtr-alice-${RUN_ID}"
ALICE_EMAIL="${ALICE_NAME}@test.atomic.dev"
BOB_NAME="idtr-bob-${RUN_ID}"
BOB_EMAIL="${BOB_NAME}@test.atomic.dev"

ORG_NAME="idtr-org-${RUN_ID}"
WORKSPACE_NAME="idtr-ws-${RUN_ID}"
PROJECT_NAME="idtr-proj-${RUN_ID}"

PROJECT_URL="${SERVER_URL}/${ORG_NAME}/${WORKSPACE_NAME}/${PROJECT_NAME}/code"

check_server() {
    curl --silent --max-time 5 --fail \
        "${SERVER_URL}/health" >/dev/null 2>&1 \
    || curl --silent --max-time 5 --fail \
        "${SERVER_URL}/" >/dev/null 2>&1
}

if ! check_server; then
    skip_suite "atomic-storage server not reachable at ${SERVER_URL}; start it or set ATOMIC_SERVER_URL"
fi

echo ""
echo "  Server URL: ${CYAN}${SERVER_URL}${RESET}"
echo "  Run ID:     ${CYAN}${RUN_ID}${RESET}"
echo ""

TEST_CONFIG_HOME="$(mktemp -d "${TMPDIR:-/tmp}/atomic-identity-transport-config-XXXXXX")"
_HARNESS_TMPDIRS+=("$TEST_CONFIG_HOME")
export HOME="$TEST_CONFIG_HOME"

WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/atomic-identity-transport-XXXXXX")"
_HARNESS_TMPDIRS+=("$WORK_DIR")

run_atomic() {
    set +e
    LAST_OUTPUT="$("$ATOMIC_BIN" "$@" 2>&1)"
    LAST_EXIT=$?
    set -e
}

run_atomic_ok() {
    local desc="$1"; shift
    run_atomic "$@"
    if [ "$LAST_EXIT" -ne 0 ]; then
        _fail "atomic $* (exit $LAST_EXIT): $LAST_OUTPUT"
        return 1
    fi
    _pass "atomic $*"
}

run_atomic_fail() {
    local desc="$1"; shift
    run_atomic "$@"
    if [ "$LAST_EXIT" -eq 0 ]; then
        _fail "atomic $* (expected failure, got success): $LAST_OUTPUT"
        return 1
    fi
    _pass "atomic $* (refused as expected)"
}

assert_last_contains() {
    local desc="$1"
    local needle="$2"
    if [[ "$LAST_OUTPUT" == *"$needle"* ]]; then
        _pass "$desc"
    else
        _fail "$desc — output did not contain '$needle': $LAST_OUTPUT"
    fi
}

# ── Identities + project scaffold ───────────────────────────────────────────

begin_section "Setup: identities, org, workspace, project"

run_atomic_ok "Create Alice identity" \
    identity new "$ALICE_NAME" --email "$ALICE_EMAIL" --set-default
run_atomic_ok "Register Alice with server" \
    identity register "$SERVER_URL"

run_atomic_ok "Create org" org create "$ORG_NAME" --email "$ALICE_EMAIL"
run_atomic_ok "Switch to org" org set "$ORG_NAME"
run_atomic_ok "Create workspace" \
    workspace create "$WORKSPACE_NAME" --visibility public --org "$ORG_NAME"
run_atomic_ok "Create project" \
    project create "$PROJECT_NAME" \
        --workspace "$WORKSPACE_NAME" \
        --kind rust \
        --org "$ORG_NAME"

# ── 1. Alice pushes; her identity binds ─────────────────────────────────────

begin_section "Push binds the origin identity"

REPO_A="$WORK_DIR/repo-a"
mkdir -p "$REPO_A"
cd "$REPO_A"
run_atomic_ok "Init repo A" init --view main
printf 'fn main() {}\n' > main.rs
run_atomic_ok "Add source" add main.rs
run_atomic_ok "Record" record -m "initial"
run_atomic_ok "Add remote" remote add origin "$PROJECT_URL"
run_atomic_ok "Push (binds identity)" push --remote origin

run_atomic_ok "Show repo A identity" repo identity show --json
ALICE_PID="$(printf '%s' "$LAST_OUTPUT" | grep -o '[A-Z2-7]\{52\}' | head -1)"
if [ -z "$ALICE_PID" ]; then
    _fail "Could not read repo A's project id"
    exit 1
fi
_pass "Repo A project id: $ALICE_PID"

# ── 2. Clone adopts the origin identity ─────────────────────────────────────

begin_section "Clone adopts the origin identity"

REPO_B="$WORK_DIR/repo-b"
run_atomic_ok "Clone repo B" clone "$PROJECT_URL" "$REPO_B"
cd "$REPO_B"
run_atomic_ok "Show repo B identity" repo identity show --json
CLONED_PID="$(printf '%s' "$LAST_OUTPUT" | grep -o '[A-Z2-7]\{52\}' | head -1)"
if [ "$CLONED_PID" = "$ALICE_PID" ]; then
    _pass "Clone ADOPTED the origin identity (no fresh mint)"
else
    _fail "Clone minted a different identity: expected $ALICE_PID, got ${CLONED_PID:-<none>}"
fi

# ── 3. Delete + reclone → same identity ─────────────────────────────────────

begin_section "Delete + reclone keeps the identity"

rm -rf "$REPO_B"
run_atomic_ok "Re-clone repo B" clone "$PROJECT_URL" "$REPO_B"
cd "$REPO_B"
run_atomic_ok "Show recloned identity" repo identity show --json
RECLONED_PID="$(printf '%s' "$LAST_OUTPUT" | grep -o '[A-Z2-7]\{52\}' | head -1)"
if [ "$RECLONED_PID" = "$ALICE_PID" ]; then
    _pass "Delete + reclone restored the SAME project identity"
else
    _fail "Reclone minted a new identity: expected $ALICE_PID, got ${RECLONED_PID:-<none>}"
fi

# ── 4. A second user clones → same identity ─────────────────────────────────

begin_section "Second user's clone adopts the same identity"

run_atomic_ok "Create Bob identity" identity new "$BOB_NAME" --email "$BOB_EMAIL" --set-default
run_atomic_ok "Register Bob with server" identity register "$SERVER_URL"

REPO_C="$WORK_DIR/repo-c"
run_atomic_ok "Bob clones repo C" clone "$PROJECT_URL" "$REPO_C"
cd "$REPO_C"
run_atomic_ok "Show repo C identity" repo identity show --json
BOB_CLONED_PID="$(printf '%s' "$LAST_OUTPUT" | grep -o '[A-Z2-7]\{52\}' | head -1)"
if [ "$BOB_CLONED_PID" = "$ALICE_PID" ]; then
    _pass "Bob's clone shares Alice's project identity"
else
    _fail "Bob's clone diverged: expected $ALICE_PID, got ${BOB_CLONED_PID:-<none>}"
fi

echo ""
_pass "Project identity transport E2E complete."
