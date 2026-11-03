#!/usr/bin/env bash
# Real GitHub PR/merge test: explicitly mutates the PERSONAL fork's default ref.
# Usage and preserved evidence: tests/harness/train_tracks.md.
set -euo pipefail

if [[ "${RUN_TRAIN_TRACKS_LIFECYCLE:-0}" != 1 ]]; then
    echo 'SKIPPING: RUN_TRAIN_TRACKS_LIFECYCLE=1 opts into personal-fork PR creation and default-branch merge'
    exit 77
fi
MODE="${TRAIN_TRACKS_AUTHOR_MODE:-git}"
case "$MODE" in
    git|atomic) ;;
    *) echo 'TRAIN_TRACKS_AUTHOR_MODE must be git or atomic' >&2; exit 1 ;;
esac
HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
SOURCE_ROOT="$(cd "$HARNESS_DIR/../.." && pwd)"
ATOMIC_BIN="$SOURCE_ROOT/target/debug/atomic"
for tool in cargo git gh python3; do
    command -v "$tool" >/dev/null || { echo "Missing prerequisite: $tool" >&2; exit 1; }
done
source "$HARNESS_DIR/train_tracks_github.sh"
train_tracks_select_fork
ROOT="$(mktemp -d "${TMPDIR:-/tmp}/atomic-train-tracks-pr-XXXXXX")"
chmod 700 "$ROOT"
STEP=0
echo "Retained lifecycle evidence: $ROOT"
finish() {
    local rc=$?
    trap - EXIT
    for role in author reader; do
        if [[ "$rc" != 0 && -d "$ROOT/$role/.atomic" ]]; then
            (
                cd "$ROOT/$role"
                export ATOMIC_CONFIG_DIR="$ROOT/$role-config"
                git status --porcelain > "$ROOT/$role-failure-git-status.txt" 2>&1 || true
                "$ATOMIC_BIN" status --no-reconcile > "$ROOT/$role-failure-forensic.txt" 2>&1 || true
                "$ATOMIC_BIN" git shim verify > "$ROOT/$role-failure-verify.txt" 2>&1 || true
            )
        fi
    done
    printf 'exit=%s\nlast_step=%s\nauthor_mode=%s\n' "$rc" "$STEP" "$MODE" > "$ROOT/result.txt"
    echo "Result: exit $rc; retained at $ROOT"
    exit "$rc"
}
trap finish EXIT
run() {
    STEP=$((STEP + 1))
    {
        printf '\n[%02d] cwd=%q' "$STEP" "$PWD"
        printf ' %q' "$@"
        printf '\n'
    } | tee -a "$ROOT/commands.log"
    "$@" 2>&1 | tee "$ROOT/$(printf '%02d' "$STEP").log"
}
atomic() { "$ATOMIC_BIN" "$@"; }
enter() {
    cd "$ROOT/$1"
    export ATOMIC_CONFIG_DIR="$ROOT/$1-config"
}
checkpoint_matches() {
    python3 - "$BASE" "$(git rev-parse HEAD)" "$(git rev-parse HEAD^{tree})" <<'PY'
import json, sys
c = json.load(open('.atomic/bridge/workspace.json'))
assert c['view'] == sys.argv[1], c
assert c['git_head'] == sys.argv[2], c
assert c['git_tree'] == sys.argv[3], c
# Signed Anchor checkpoints may omit the optional index-tree cache. The
# preceding verifier checks the actual stage-0 index independently.
assert c.get('git_index_tree') in (None, sys.argv[3]), c
PY
}
verify() {
    atomic git shim verify
    local out
    out="$(atomic status --short)"
    [[ -z "$out" ]] || { printf 'Atomic dirty:\n%s\n' "$out"; return 1; }
    out="$(git status --porcelain)"
    [[ -z "$out" ]] || { printf 'Git dirty:\n%s\n' "$out"; return 1; }
}
setup() {
    local role="$1"
    run git clone --branch "$BASE" "$FORK_URL" "$ROOT/$role"
    enter "$role"
    [[ "$(git rev-parse HEAD)" == "$BASE_BEFORE" ]]
    [[ "$(git remote get-url --push origin)" == "$FORK_URL" ]]
    git config user.name 'Atomic Shim Lifecycle'
    git config user.email 'shim-test@example.invalid'
    git config commit.gpgsign false
    git config tag.gpgsign false
    git config push.default nothing
    run atomic git import --no-vault
    python3 - "$ROOT/$role-binding.key" <<'PY'
import os, secrets, sys
fd = os.open(sys.argv[1], os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
with os.fdopen(fd, 'w') as f:
    f.write(secrets.token_hex(32))
PY
    run atomic git shim enable --remote origin --binding-key-file "$ROOT/$role-binding.key"
    run atomic git shim reconcile
    run verify
    run checkpoint_matches
    for hook in post-checkout pre-commit pre-push post-rewrite reference-transaction; do
        grep -qF "$ATOMIC_BIN" "$(git rev-parse --git-path "hooks/$hook")"
    done
}

cd "$SOURCE_ROOT"
run cargo build --locked -p atomic-cli --bin atomic
run atomic --version
git rev-parse HEAD > "$ROOT/source-commit.txt"
shasum -a 256 "$ATOMIC_BIN" > "$ROOT/binary-sha256.txt"
uname -a > "$ROOT/platform.txt"
printf '%s\n' "$FORK" > "$ROOT/github-target.txt"
train_tracks_ensure_fork "$ROOT"
python3 - "$ROOT/fork.json" <<'PY'
import json, sys
r = json.load(open(sys.argv[1]))
assert r['allow_merge_commit'], 'merge-commit test requires merge commits enabled'
PY
BASE="$(gh api "repos/$FORK" --jq .default_branch)"
[[ "$BASE" == "$(gh api "repos/$UPSTREAM" --jq .default_branch)" ]]
BASE_BEFORE="$(gh api "repos/$FORK/git/ref/heads/$BASE" --jq .object.sha)"
gh api "repos/$UPSTREAM/git/matching-refs/heads/" > "$ROOT/upstream-before.json"
printf 'author_mode=%s\ndefault_branch=%s\nbase_before=%s\n' "$MODE" "$BASE" "$BASE_BEFORE" > "$ROOT/settings.txt"
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
export ATOMIC_SERVICE=local ATOMIC_NONINTERACTIVE=1 NO_COLOR=1
unset ATOMIC_RPC ATOMIC_DAEMON_SOCKET

# Reader is imported BEFORE the PR exists; this is not a fresh import after pull.
setup reader
atomic log -f json --full-hash -n 1000000 > "$ROOT/reader-before-log.json"
cp .atomic/bridge/workspace.json "$ROOT/reader-before-checkpoint.json"
setup author
TOPIC="shim-pr-$MODE-$(date -u +%Y%m%dT%H%M%SZ)-$$"
FIXTURE="shim-lifecycle/$TOPIC"
run git switch -c "$TOPIC"
run atomic git shim reconcile
mkdir -p "$FIXTURE"
printf 'round trip %s\n' "$TOPIC" > "$FIXTURE/round-trip.txt"
if [[ "$MODE" == git ]]; then
    printf 'literal percent\n' > "$FIXTURE/100%.txt"
    printf 'literal escape\n' > "$FIXTURE/a%41.txt"
    printf 'Unicode and spaces\n' > "$FIXTURE/café space.txt"
    run git add "$FIXTURE"
    run git commit -m "test: GitHub lifecycle $TOPIC"
    run atomic git shim reconcile
else
    run atomic add "$FIXTURE/round-trip.txt"
    run atomic record -am "test: Atomic-projected GitHub lifecycle $TOPIC"
fi
run verify
atomic log -f json --full-hash -n 1 > "$ROOT/author-tip-log.json"
TOPIC_SHA="$(git rev-parse HEAD)"
TOPIC_TREE="$(git rev-parse HEAD^{tree})"
printf '%s\n' "$TOPIC" > "$ROOT/topic.txt"
printf '%s\n' "$TOPIC_SHA" > "$ROOT/topic-sha.txt"
run git -c credential.helper= -c 'credential.helper=!gh auth git-credential' \
    push origin "refs/heads/$TOPIC:refs/heads/$TOPIC"
[[ "$(git ls-remote origin "refs/heads/$TOPIC" | cut -f1)" == "$TOPIC_SHA" ]]

# Explicit repository names prevent gh from selecting the upstream fork parent.
run gh api --method POST "repos/$FORK/pulls" \
    -f head="$TOPIC" -f base="$BASE" \
    -f title="test: disposable shim lifecycle ($MODE)" \
    -f body="Automated Atomic Git shim integration in the personal fork only. Tests a real push, PR merge, pull into an already-imported second clone, and incremental Atomic import. Author mode: $MODE. No upstream changes."
# Capture the PR number from the exact response, not a potentially unrelated list.
cp "$ROOT/$(printf '%02d' "$STEP").log" "$ROOT/pr.json"
PR="$(train_tracks_validate_pr "$ROOT/pr.json")"
[[ "$(gh api "repos/$FORK/git/ref/heads/$BASE" --jq .object.sha)" == "$BASE_BEFORE" ]]
run gh api --method PUT "repos/$FORK/pulls/$PR/merge" \
    -f merge_method=merge -f sha="$TOPIC_SHA" \
    -f commit_title="test: merge shim lifecycle PR #$PR ($MODE)"
cp "$ROOT/$(printf '%02d' "$STEP").log" "$ROOT/merge.json"
MERGE_SHA="$(python3 - "$ROOT/merge.json" <<'PY'
import json, sys
m = json.load(open(sys.argv[1]))
assert m['merged'], m
print(m['sha'])
PY
)"
gh api "repos/$FORK/pulls/$PR" > "$ROOT/pr-merged.json"
printf 'https://github.com/%s/pull/%s\n' "$FORK" "$PR" > "$ROOT/pr-url.txt"

enter reader
[[ "$(git rev-parse HEAD)" == "$BASE_BEFORE" ]]
run git pull --ff-only origin "$BASE"
[[ "$(git branch --show-current)" == "$BASE" ]]
[[ "$(git rev-parse HEAD)" == "$MERGE_SHA" ]]
[[ "$(git rev-parse HEAD^1)" == "$BASE_BEFORE" ]]
[[ "$(git rev-parse HEAD^2)" == "$TOPIC_SHA" ]]
[[ "$(git rev-parse HEAD^{tree})" == "$TOPIC_TREE" ]]
run atomic status --no-reconcile
run atomic git import --incremental --no-vault --branch "$BASE"
run verify
run checkpoint_matches
atomic log -f json --full-hash -n 1000000 > "$ROOT/reader-after-log.json"
cp .atomic/bridge/workspace.json "$ROOT/reader-after-checkpoint.json"
python3 - "$ROOT" "$FIXTURE" "$TOPIC" "$MODE" <<'PY'
import json, pathlib, sys
r = pathlib.Path(sys.argv[1])
before = json.loads((r / 'reader-before-log.json').read_text())
after = json.loads((r / 'reader-after-log.json').read_text())
old_hashes = {c['hash'] for c in before}
new_hashes = {c['hash'] for c in after}
assert old_hashes < new_hashes, 'incremental import must retain old changes and add new ones'
assert pathlib.Path(sys.argv[2], 'round-trip.txt').read_text() == 'round trip ' + sys.argv[3] + '\n'
if sys.argv[4] == 'atomic':
    author = json.loads((r / 'author-tip-log.json').read_text())[0]
    identity = {
        'author_change': author['hash'],
        'reader_changes': sorted(new_hashes),
        'original_change_in_reader_view': author['hash'] in new_hashes,
    }
    (r / 'identity-audit.json').write_text(json.dumps(identity, indent=2) + '\n')
    print('Original Atomic change retained in reader view:', identity['original_change_in_reader_view'])
    print('Content/checkpoint success is separate from original-history/provenance restoration.')
PY
run atomic git import --incremental --no-vault --branch "$BASE"
atomic log -f json --full-hash -n 1000000 > "$ROOT/reader-repeat-log.json"
cmp "$ROOT/reader-after-log.json" "$ROOT/reader-repeat-log.json"
run verify
run checkpoint_matches

# A read-only API comparison checks ALL upstream branch refs, not just default.
gh api "repos/$UPSTREAM/git/matching-refs/heads/" > "$ROOT/upstream-after.json"
python3 - "$ROOT" <<'PY'
import json, pathlib, sys
r = pathlib.Path(sys.argv[1])
def refs(name):
    return {v['ref']: v['object']['sha'] for v in json.loads((r / name).read_text())}
assert refs('upstream-before.json') == refs('upstream-after.json'), 'upstream branch refs changed'
PY
printf '\nPASS: %s push → PR #%s → merge → pull %s → incremental import → repeat import\n' "$MODE" "$PR" "$BASE"
echo "Personal-fork default branch was intentionally advanced; PR and clones retained."
