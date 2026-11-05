"""Shared native-only CLI fixture helpers (no Git or remote operations)."""
import hashlib
import json
import os
from pathlib import Path
import stat
import subprocess
import sys

BINARY = os.path.abspath(sys.argv[1])
ROOT = Path.cwd()


def call(*args, env=None, success=True):
    process_env = dict(os.environ, **(env or {}))
    process_env.pop('ATOMIC_RPC', None)
    process_env.pop('ATOMIC_DAEMON_SOCKET', None)
    result = subprocess.run([BINARY, *args], env=process_env,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=60)
    if success and result.returncode:
        raise AssertionError((args, result.returncode, result.stdout.decode(), result.stderr.decode()))
    return result


def data(*args):
    return json.loads(call(*args).stdout)


def check(label, condition):
    if not condition:
        raise AssertionError(label)
    print('PASS\t' + label, flush=True)


def record(message):
    call('record', '-am', message)


def view():
    return data('view', 'list', '--all', '--json')['current_view']


def files_under(path):
    """Compare file artifacts byte-for-byte, including modes and link targets."""
    result = {}
    if path.exists():
        for directory, dirs, files in os.walk(path, followlinks=False):
            dirs[:] = [d for d in dirs if d not in ('.atomic', '.git')]
            for name in files + [d for d in dirs if Path(directory, d).is_symlink()]:
                p = Path(directory, name)
                mode = p.lstat().st_mode
                result[str(p.relative_to(path))] = (
                    stat.S_IFMT(mode), stat.S_IMODE(mode),
                    os.readlink(p) if p.is_symlink() else hashlib.sha256(p.read_bytes()).hexdigest())
            dirs[:] = [d for d in dirs if not Path(directory, d).is_symlink()]
    return result
