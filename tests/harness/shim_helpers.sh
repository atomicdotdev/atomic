#!/usr/bin/env bash
# Shared fixtures for the current, opt-in causal shim (not legacy shadow sync).
# Source helpers.sh first. All writes stay in disposable repositories; no remote
# is configured and every Git publication check is local.

shim_fixture() {
    local label="$1" private
    make_temp_repo "$label"
    private="$(mktemp -d "${TMPDIR:-/tmp}/atomic-shim-private-XXXXXX")"
    _HARNESS_TMPDIRS+=("$private")
    export HOME="$private/home" ATOMIC_HOME="$private/home/.atomic"
    export ATOMIC_SERVICE=local ATOMIC_NONINTERACTIVE=1
    export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 GIT_TERMINAL_PROMPT=0
    unset ATOMIC_RPC ATOMIC_DAEMON_SOCKET
    mkdir -p "$HOME"
    init_git_repo
    git config push.default nothing
    printf 'base\n' > tracked.txt
    printf 'main-only\n' > base-only.txt
    : > empty.txt
    require_success "$label: stage baseline" git add tracked.txt empty.txt base-only.txt
    require_success "$label: commit baseline" git commit -qm baseline
    SHIM_MAIN="$(git_current_branch)"
    require_success "$label: import" atomic git import --no-vault
    python3 - "$private/binding.key" <<'PY'
import os, secrets, sys
fd = os.open(sys.argv[1], os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
with os.fdopen(fd, 'w') as file:
    file.write(secrets.token_hex(32))
PY
    require_success "$label: enroll signed local shim" atomic git shim enable --no-refspecs --binding-key-file "$private/binding.key"
    require_success "$label: anchor" atomic git shim reconcile
    shim_clean "$label: baseline"
}

shim_clean() {
    local label="$1" output
    require_success "$label: verify projection" atomic git shim verify
    require_capture "$label: read Git status" git status --porcelain
    output="$COMMAND_OUTPUT"
    if [[ -z "$output" ]]; then _pass "$label: Git clean"; else _fail "$label: Git clean" "$output"; fi
    require_capture "$label: read Atomic status" atomic status --short
    output="$COMMAND_OUTPUT"
    if [[ -z "$output" ]]; then _pass "$label: Atomic clean"; else _fail "$label: Atomic clean" "$output"; fi
}

shim_assert_blob() {
    local label="$1" path="$2" expected="$3"
    python3 - "$path" "$expected" <<'PY'
import pathlib, subprocess, sys
path, expected = sys.argv[1], sys.argv[2].encode()
assert pathlib.Path(path).read_bytes() == expected, ('working bytes', path)
assert subprocess.check_output(['git', 'show', 'HEAD:' + path]) == expected, ('Git blob', path)
PY
}

shim_file_bytes() {
    python3 - "$1" "$2" <<'PY'
import pathlib, sys
assert pathlib.Path(sys.argv[1]).read_bytes() == sys.argv[2].encode(), sys.argv[1]
PY
}

# Logical refusal snapshot: refs (including detached HEAD), index, checkpoint,
# exact file bytes/kinds/modes, immutable objects, both operation DAGs,
# and effective history. Advisory event logs are deliberately not authority.
# The dedicated 42 guard suite additionally checks raw database-byte immutability.
shim_snapshot() {
    python3 - "$ATOMIC_BIN" "$1" <<'PY'
import hashlib, json, os, pathlib, stat, subprocess, sys
binary, destination = sys.argv[1:]
def call(*args):
    return subprocess.check_output(args, stderr=subprocess.PIPE).decode()
def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest() if path.exists() else None
files = {}
for directory, dirs, names in os.walk('.'):
    dirs[:] = sorted(name for name in dirs if name not in ('.git', '.atomic'))
    for name in dirs:
        p = pathlib.Path(directory, name)
        if not p.is_symlink():
            mode = p.lstat().st_mode
            files[str(p)] = [stat.S_IFMT(mode), stat.S_IMODE(mode), None]
    for name in sorted(names + [d for d in dirs if pathlib.Path(directory, d).is_symlink()]):
        p = pathlib.Path(directory, name)
        mode = p.lstat().st_mode
        files[str(p)] = [stat.S_IFMT(mode), stat.S_IMODE(mode),
                        os.readlink(p) if p.is_symlink() else digest(p)]
    dirs[:] = [d for d in dirs if not pathlib.Path(directory, d).is_symlink()]
result = dict(head=call('git','rev-parse','HEAD'), refs=call('git','show-ref'),
              head_file=pathlib.Path('.git/HEAD').read_text(), index=digest(pathlib.Path('.git/index')),
              checkpoint=digest(pathlib.Path('.atomic/bridge/workspace.json')), files=files,
              changes={str(p):digest(p) for p in sorted(pathlib.Path('.atomic/changes').rglob('*.change'))},
              working_ops=json.loads(call(binary,'op','log','--json')),
              repository_ops=json.loads(call(binary,'op','log','--repository','--json')),
              current_view=json.loads(call(binary,'view','list','--all','--json'))['current_view'],
              history=json.loads(call(binary,'log','--all','--format','json')))
pathlib.Path(destination).write_text(json.dumps(result, sort_keys=True, indent=2))
PY
}

shim_locked_command() {
    python3 - "$ATOMIC_BIN" "$@" <<'PY'
import fcntl, subprocess, sys
with open('.atomic/bridge.lock', 'r+b') as lock:
    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    result = subprocess.run(sys.argv[1:], timeout=30)
    sys.exit(result.returncode)
PY
}

shim_tracked_file_metadata() {
    python3 - "$1" <<'PY'
import hashlib, json, os, pathlib, stat, subprocess, sys
result = {}
for name in subprocess.check_output(['git','ls-files','-z']).split(b'\0'):
    if not name:
        continue
    p = pathlib.Path(os.fsdecode(name))
    metadata = p.lstat()
    result[str(p)] = [metadata.st_mode, metadata.st_ino, metadata.st_mtime_ns,
                      os.readlink(p) if stat.S_ISLNK(metadata.st_mode)
                      else hashlib.sha256(p.read_bytes()).hexdigest()]
pathlib.Path(sys.argv[1]).write_text(json.dumps(result, sort_keys=True))
PY
}

shim_refusal() {
    local label="$1" diagnostic="$2" evidence output rc capture=0
    shift 2
    if [[ "${1:-}" == --allow-wip-capture ]]; then capture=1; shift; fi
    evidence="$(mktemp -d "${TMPDIR:-/tmp}/atomic-shim-refusal-XXXXXX")"
    _HARNESS_TMPDIRS+=("$evidence")
    require_success "$label: snapshot before" shim_snapshot "$evidence/before.json"
    if output="$("$@" 2>&1)"; then rc=0; else rc=$?; fi
    if [[ "$rc" != 0 ]]; then _pass "$label: refused"; else _fail "$label: refused" "unexpected success: $output"; fi
    if printf '%s\n' "$output" | grep -qiE "$diagnostic"; then
        _pass "$label: diagnostic"
    else
        _fail "$label: diagnostic" "$output"
    fi
    require_success "$label: snapshot after" shim_snapshot "$evidence/after.json"
    if python3 - "$evidence/before.json" "$evidence/after.json" "$capture" "$ATOMIC_BIN" <<'PY'
import json, subprocess, sys
before, after = [json.load(open(path)) for path in sys.argv[1:3]]
if sys.argv[3] == '0':
    assert before == after, 'strict refusal snapshot changed'
else:
    # Foreground transition capture is permitted, not silently ignored: only
    # verified private capture/rebase operations may be appended. Existing objects and
    # operation-log entries are immutable; visible history and all external leases
    # remain byte-identical. A capture may temporarily switch into wc/<id>;
    # the final user view must not change. No publication/recovery is permitted.
    for key in before.keys() - {'changes','working_ops','repository_ops'}:
        assert before[key] == after[key], key
    for path, digest in before['changes'].items():
        assert after['changes'].get(path) == digest, ('changed object',path)
    added_ops = set()
    capture_seen = False
    switch_seen = False
    for scope in ('working_ops','repository_ops'):
        old = {row['id']:row for row in before[scope]['entries']}
        new = {row['id']:row for row in after[scope]['entries']}
        assert old.keys() <= new.keys(), ('lost operations',scope)
        for identity, entry in old.items():
            assert {k:v for k,v in entry.items() if k != 'is_head'} == {
                k:v for k,v in new[identity].items() if k != 'is_head'}, ('changed operation',identity)
        for identity in new.keys() - old.keys():
            entry = new[identity]
            assert entry['verification'] == 'verified', entry
            assert entry['actor'].get('kind') == 'system', entry
            actor = entry['actor'].get('name')
            assert (entry['kind'], actor) in (
                ('record','repository-record'), ('record','bridge-pre-transition-capture'),
                ('record','bridge-snapshot-rebase'), ('switch_view','repository-switch')), entry
            detail = json.loads(subprocess.check_output([sys.argv[4],'op','show',identity,'--json']))
            assert detail['verification'] == 'verified' and any(
                receipt['kind'] == 'verified' for receipt in detail['receipts']), detail
            if actor == 'repository-record':
                views = [detail.get('before',{}).get('view'), detail['delta']['after'].get('view')]
                assert any(view and view['name'].startswith('wc/') for view in views), detail
            if actor == 'repository-switch':
                # Capturing WIP can restore the user view after temporarily
                # visiting its baseline. It must return to the original view,
                # never leave a refused target selected.
                assert detail['delta']['after']['view']['name'] == before['current_view'], detail
                switch_seen = True
            capture_seen |= actor == 'bridge-pre-transition-capture'
            added_ops.add(identity)
        if before[scope] != after[scope]:
            assert new.keys() - old.keys(), ('head changed without append',scope)
    if before['changes'] != after['changes']:
        assert added_ops, 'unexplained object publication'
    assert not switch_seen or capture_seen, 'switch without a verified WIP capture'
    print('verified append-only WIP capture operations:', len(added_ops))
PY
    then
        _pass "$label: preserves external state and immutable history (WIP capture policy=$capture)"
    else
        _fail "$label: preserves external state and immutable history (WIP capture policy=$capture)" "$(diff -u "$evidence/before.json" "$evidence/after.json" || true)"
    fi
}
