#!/usr/bin/env bash
# Explicitly opt in: this suite creates a personal GitHub fork if absent.
# PUSH_TRAIN_TRACKS_SHIM=1 additionally publishes a unique test branch to it.
set -euo pipefail

if [[ "${RUN_TRAIN_TRACKS_SHIM:-0}" != 1 ]]; then
    echo 'SKIPPING: set RUN_TRAIN_TRACKS_SHIM=1 for the GitHub fork integration (see train_tracks.md)'
    exit 77
fi

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
SOURCE_ROOT="$(cd "$HARNESS_DIR/../.." && pwd)"
PATH_CASE="${TRAIN_TRACKS_PATH_CASE:-all}"
case "$PATH_CASE" in
    all|unicode|percent|none) ;;
    *) echo 'TRAIN_TRACKS_PATH_CASE must be all, unicode, percent, or none' >&2; exit 1 ;;
esac
# Deliberately do not inherit the harness release/PATH binary selection.
ATOMIC_BIN="$SOURCE_ROOT/target/debug/atomic"
export ATOMIC_BIN
for tool in cargo git gh python3; do
    command -v "$tool" >/dev/null || { echo "Missing prerequisite: $tool" >&2; exit 1; }
done
source "$HARNESS_DIR/train_tracks_github.sh"
train_tracks_select_fork

ROOT="$(mktemp -d "${TMPDIR:-/tmp}/atomic-train-tracks-XXXXXX")"
chmod 700 "$ROOT"
echo "Evidence and test repository (retained on success or failure): $ROOT"
STEP=0
finish() {
    local rc=$?
    trap - EXIT
    if [[ "$rc" != 0 && -d "$ROOT/repo/.atomic" ]]; then
        (
            cd "$ROOT/repo"
            git status --porcelain > "$ROOT/failure-git-status.txt" 2>&1 || true
            "$ATOMIC_BIN" status --no-reconcile > "$ROOT/failure-forensic-status.txt" 2>&1 || true
            "$ATOMIC_BIN" git shim verify > "$ROOT/failure-verify.txt" 2>&1 || true
        )
    fi
    printf 'exit=%s\nlast_step=%s\n' "$rc" "$STEP" > "$ROOT/result.txt"
    echo "Result: exit $rc; evidence retained at $ROOT"
    exit "$rc"
}
trap finish EXIT

# Each command gets its own stdout/stderr log; pipefail preserves its exit code.
run() {
    STEP=$((STEP + 1))
    {
        printf '\n[%02d]' "$STEP"
        printf ' %q' "$@"
        printf '\n'
    } | tee -a "$ROOT/commands.log"
    "$@" 2>&1 | tee "$ROOT/$(printf '%02d' "$STEP").log"
}
atomic() { "$ATOMIC_BIN" "$@"; }
expect_content() {
    python3 - "$1" "$2" <<'PY'
from pathlib import Path
import sys
assert Path(sys.argv[1]).read_bytes() == sys.argv[2].encode(), sys.argv[1]
PY
}
expect_projection() {
    git show "HEAD:$1" > "$ROOT/projected-file"
    cmp "$1" "$ROOT/projected-file"
}
clean() {
    local out
    out="$(atomic status --short)"
    [[ -z "$out" ]] || { printf 'Atomic is dirty:\n%s\n' "$out"; return 1; }
    out="$(git status --porcelain)"
    [[ -z "$out" ]] || { printf 'Git is dirty:\n%s\n' "$out"; return 1; }
}
verify() { atomic git shim verify && clean; }

cd "$SOURCE_ROOT"
run cargo build --locked -p atomic-cli --bin atomic
run atomic --version
printf 'path_case=%s\npush=%s\nservice=local\n' "$PATH_CASE" "${PUSH_TRAIN_TRACKS_SHIM:-0}" > "$ROOT/settings.txt"
printf '%s\n' "$FORK" > "$ROOT/github-target.txt"
git rev-parse HEAD > "$ROOT/source-commit.txt"
uname -a > "$ROOT/platform.txt"
shasum -a 256 "$ATOMIC_BIN" > "$ROOT/binary-sha256.txt"
train_tracks_ensure_fork "$ROOT"
BASE="$(gh api "repos/$UPSTREAM" --jq .default_branch)"
UPSTREAM_BEFORE="$(gh api "repos/$UPSTREAM/git/ref/heads/$BASE" --jq .object.sha)"
FORK_BASE="$(gh api "repos/$FORK/git/ref/heads/$BASE" --jq .object.sha)"
[[ "$UPSTREAM_BEFORE" == "$FORK_BASE" ]] || {
    echo 'Fork default branch differs from upstream; inspect it rather than resetting it automatically.'
    exit 1
}
printf '%s %s\n' "$BASE" "$UPSTREAM_BEFORE" > "$ROOT/upstream-before.txt"
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
run git clone --no-local --branch "$BASE" "$FORK_URL" "$ROOT/repo"
cd "$ROOT/repo"
[[ "$(git rev-parse HEAD)" == "$FORK_BASE" ]]

# Match the guide's local, noninteractive isolation. Installed hooks embed the
# absolute debug executable; the parent agent's PATH atomic stays untouched.
export ATOMIC_CONFIG_DIR="$ROOT/config"
export ATOMIC_SERVICE=local ATOMIC_NONINTERACTIVE=1 NO_COLOR=1
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
unset ATOMIC_RPC ATOMIC_DAEMON_SOCKET
git config user.name 'Atomic Shim Integration'
git config user.email 'shim-test@example.invalid'
git config commit.gpgsign false
git config tag.gpgsign false
git config push.default nothing
# Even an accidental push to an unqualified remote can only address the fork.
[[ "$(git remote get-url origin)" == "$FORK_URL" ]]
[[ "$(git remote get-url --push origin)" == "$FORK_URL" ]]

run atomic git import --no-vault
python3 - "$ROOT/binding.key" <<'PY'
import os, secrets, sys
fd = os.open(sys.argv[1], os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
with os.fdopen(fd, 'w') as f:
    f.write(secrets.token_hex(32))
PY
run atomic git shim enable --remote origin --binding-key-file "$ROOT/binding.key"
run atomic git shim reconcile
run atomic git hooks status
for hook in post-checkout pre-commit pre-push post-rewrite reference-transaction; do
    hook_path="$(git rev-parse --git-path "hooks/$hook")"
    [[ -x "$hook_path" ]]
    grep -qF "$ATOMIC_BIN" "$hook_path"
done
run verify

# Native records must project exact bytes, including a rename without a final LF.
[[ ! -e shim-integration && ! -L shim-integration ]]
mkdir shim-integration
printf 'baseline\nsecond line\n' > shim-integration/original.txt
run atomic add shim-integration/original.txt
run atomic record -am 'test: native shim baseline'
printf 'rewritten without final newline' > shim-integration/original.txt
run atomic record -am 'test: rewrite imported/projection content'
run expect_projection shim-integration/original.txt
mv shim-integration/original.txt shim-integration/renamed.txt
run atomic status --short
run atomic record -am 'test: native rename without final newline'
run expect_projection shim-integration/renamed.txt
run verify

# The documented new-branch/fast-forward regression, after native records.
TOPIC="shim-test-$(date -u +%Y%m%dT%H%M%SZ)-$$"
printf '%s\n' "$TOPIC" > "$ROOT/test-branch.txt"
run git switch -c "$TOPIC"
printf 'foreign Git edit\n' > shim-integration/renamed.txt
run git add shim-integration/renamed.txt
run git commit -m 'test: Git branch after native Atomic records'
run atomic git shim reconcile
run verify
run git switch "$BASE"
run atomic git shim reconcile
run expect_content shim-integration/renamed.txt 'rewritten without final newline'
run verify
run git merge --ff-only "$TOPIC"
run atomic git shim reconcile
run expect_content shim-integration/renamed.txt $'foreign Git edit\n'
run verify

if [[ "${PUSH_TRAIN_TRACKS_SHIM:-0}" == 1 ]]; then
    # Never update the fork's baseline branch. Exercise the real pre-push hook
    # on a new, reconciled branch while another branch is checked out.
    [[ "$(git remote get-url --push origin)" == "$FORK_URL" ]]
    run git -c credential.helper= -c 'credential.helper=!gh auth git-credential' \
        push origin "refs/heads/$TOPIC:refs/heads/$TOPIC"
    REMOTE_SHA="$(git ls-remote origin "refs/heads/$TOPIC" | cut -f1)"
    [[ "$REMOTE_SHA" == "$(git rev-parse "$TOPIC")" ]]
else
    echo 'Remote push NOT RUN (set PUSH_TRAIN_TRACKS_SHIM=1 to opt in).'
fi

# Empty lockfile regeneration and literal on-disk path identities.
touch shim-integration/package-lock.json
if [[ "$PATH_CASE" == all || "$PATH_CASE" == unicode ]]; then
    printf 'space and unicode\n' > 'shim-integration/café space.txt'
fi
if [[ "$PATH_CASE" == all || "$PATH_CASE" == percent ]]; then
    printf 'literal percent\n' > 'shim-integration/100%.txt'
fi
echo "Filename cases: $PATH_CASE (use separate fresh runs to isolate failures)"
run git add shim-integration
run git commit -m 'test: empty lockfile and path identities'
run atomic git shim reconcile
run verify
printf '{"lockfileVersion": 3}\n' > shim-integration/package-lock.json
run git add shim-integration/package-lock.json
run git commit -m 'test: regenerate empty lockfile'
run atomic git shim reconcile
run verify

# Noise must not be interpreted as Git/Atomic manifest divergence.
printf 'scratch\n' > shim-untracked-scratch.txt
mkdir -p node_modules/shim-integration
printf 'ignored dependency\n' > node_modules/shim-integration/noise
printf '\n/node_modules/shim-integration/\n' >> .git/info/exclude
run git check-ignore node_modules/shim-integration/noise
run atomic git shim reconcile
run atomic git shim verify
# Remove only the exact test-created files, not Atomic state or recovery data.
rm shim-untracked-scratch.txt node_modules/shim-integration/noise
rmdir node_modules/shim-integration
run verify

# Detaching to an imported ancestor and returning must preserve both closures.
RETURN_HEAD="$(git rev-parse HEAD)"
run git checkout --detach "$FORK_BASE"
run atomic git shim reconcile
run verify
run git switch "$BASE"
run atomic git shim reconcile
[[ "$(git rev-parse HEAD)" == "$RETURN_HEAD" ]]
run verify

run atomic git shim disable
run atomic git hooks status
for hook in post-checkout pre-commit pre-push post-rewrite reference-transaction; do
    [[ ! -e "$(git rev-parse --git-path "hooks/$hook")" ]]
done
run clean
UPSTREAM_AFTER="$(gh api "repos/$UPSTREAM/git/ref/heads/$BASE" --jq .object.sha)"
printf '%s %s\n' "$BASE" "$UPSTREAM_AFTER" > "$ROOT/upstream-after.txt"
[[ "$UPSTREAM_BEFORE" == "$UPSTREAM_AFTER" ]] || {
    echo 'Upstream changed during the test; investigate concurrent upstream activity.'
    exit 1
}
printf '\nPASS: train-tracks fork shim scenarios; upstream default ref unchanged.\n'
