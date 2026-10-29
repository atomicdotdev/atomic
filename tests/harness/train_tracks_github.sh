#!/usr/bin/env bash
# Shared personal-fork setup. Sourcing this file performs no network operations.
# Call selection only AFTER a suite's explicit network/mutation opt-in.

train_tracks_select_fork() {
    local login requested owner
    login="$(gh api user --jq .login)" || {
        echo 'GitHub authentication failed; run gh auth login first.' >&2
        return 1
    }
    requested="${TRAIN_TRACKS_GITHUB_OWNER:-}"
    if [[ -z "$requested" ]]; then
        if [[ ! -t 0 ]]; then
            echo 'Noninteractive run: set TRAIN_TRACKS_GITHUB_OWNER to your personal GitHub username or profile URL.' >&2
            return 1
        fi
        read -r -p "Personal GitHub username or profile URL [$login]: " requested || return 1
        requested="${requested:-$login}"
    fi
    owner="$(python3 - "$requested" "$login" <<'PY'
import re, sys
requested, login = sys.argv[1:]
owner = requested.strip()
prefix = 'https://github.com/'
if owner.startswith(prefix):
    owner = owner[len(prefix):].rstrip('/')
if not re.fullmatch(r'[A-Za-z0-9](?:[A-Za-z0-9-]{0,37}[A-Za-z0-9])?', owner):
    sys.exit('Enter a personal GitHub username or https://github.com/<username> profile URL (not a repository URL).')
if '--' in owner:
    sys.exit('Invalid GitHub username: consecutive hyphens are not allowed.')
if owner.casefold() == 'atomicdotdev':
    sys.exit('Refusing the upstream namespace atomicdotdev; use your personal GitHub account.')
if owner.casefold() != login.casefold():
    sys.exit(f'Selected owner {owner!r} does not match authenticated GitHub account {login!r}; switch gh accounts or select your own account.')
print(login)
PY
)" || return 1
    TRAIN_TRACKS_GITHUB_OWNER="$owner"
    UPSTREAM=atomicdotdev/train-tracks
    FORK="$owner/train-tracks"
    FORK_URL="https://github.com/$FORK.git"
    export TRAIN_TRACKS_GITHUB_OWNER
    printf 'Personal GitHub test fork: %s (upstream remains read-only)\n' "$FORK" >&2
}

# Caller provides run() for its command log and a private artifact directory.
# Never replace an existing unrelated repository or automatically reset a fork.
train_tracks_ensure_fork() {
    local artifacts="$1"
    if ! gh api "repos/$FORK" > "$artifacts/fork.json" 2> "$artifacts/fork-api.err"; then
        if ! grep -q 'HTTP 404' "$artifacts/fork-api.err"; then
            cat "$artifacts/fork-api.err" >&2
            return 1
        fi
        run gh api --method POST "repos/$UPSTREAM/forks" \
            -f name=train-tracks -F default_branch_only=false || return 1
        gh api "repos/$FORK" > "$artifacts/fork.json" || return 1
    fi
    python3 - "$artifacts/fork.json" "$FORK" "$UPSTREAM" <<'PY'
import json, sys
r = json.load(open(sys.argv[1]))
fork, upstream = sys.argv[2:]
if r.get('full_name', '').casefold() != fork.casefold() or not r.get('fork'):
    sys.exit(f'Refusing {fork}: it is not the selected personal fork.')
if r.get('owner', {}).get('type') != 'User':
    sys.exit(f'Refusing {fork}: select a personal account, not an organization.')
if r.get('parent', {}).get('full_name', '').casefold() != upstream.casefold():
    sys.exit(f'Refusing {fork}: expected a fork of {upstream}.')
if not r.get('permissions', {}).get('push'):
    sys.exit(f'Authenticated account cannot push to {fork}; check gh authentication and permissions.')
PY
}

train_tracks_validate_pr() {
    python3 - "$1" "$FORK" <<'PY'
import json, sys
p = json.load(open(sys.argv[1]))
fork = sys.argv[2].casefold()
for side in ('base', 'head'):
    if p.get(side, {}).get('repo', {}).get('full_name', '').casefold() != fork:
        sys.exit(f'Refusing PR: both head and base must belong to {sys.argv[2]}.')
print(p['number'])
PY
}
