#!/usr/bin/env python3
"""Prepare and measure pinned Git imports on a disposable Linux VM disk.

Only guest page caches are reset. Host block caches and physical storage caches
are uncontrolled. Never run against a user's repository or a host-shared mount.
"""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path, PurePosixPath
import platform
import shutil
import stat
import statistics
import subprocess
import time

MARKER = 'atomic-imported-cold-benchmark-v1'
BACKENDS = ('graph', 'span')
CASES = ('render', 'materialize_missing')


def digest(path):
    value = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            value.update(block)
    return value.hexdigest()


def validate_manifest(manifest):
    if not manifest['files']:
        raise ValueError('empty manifest')
    tip = manifest['git_tip']
    if len(tip) not in (40, 64) or any(char not in '0123456789abcdef' for char in tip):
        raise ValueError('invalid pinned Git tip')
    paths = set()
    for file in manifest['files']:
        name = file['path']
        path = PurePosixPath(name)
        if not name or not path.parts or path.is_absolute() or '..' in path.parts or path.parts[0] in ('.git', '.atomic') or str(path) != name:
            raise ValueError('unsafe or noncanonical fixture path: ' + name)
        if name in paths or file['mode'] not in ('100644', '100755', '120000'):
            raise ValueError('duplicate path or unsupported mode')
        if len(file['sha256']) != 64 or any(char not in '0123456789abcdef' for char in file['sha256']):
            raise ValueError('invalid expected content hash')
        paths.add(name)


def isolated_env(root):
    env = {key: value for key, value in os.environ.items()
           if not key.startswith(('ATOMIC_', 'GIT_', 'XDG_'))}
    home = root / 'home'
    home.mkdir(exist_ok=True)
    env.update(HOME=str(home), ATOMIC_HOME=str(home / '.atomic'),
               ATOMIC_CONFIG_DIR=str(home / 'config'), ATOMIC_SERVICE='local',
               ATOMIC_SERVER_URL='http://127.0.0.1:1', RAYON_NUM_THREADS='2',
               GIT_CONFIG_GLOBAL='/dev/null', GIT_CONFIG_NOSYSTEM='1',
               GIT_TERMINAL_PROMPT='0', GIT_CONFIG_COUNT='1',
               GIT_CONFIG_KEY_0='credential.helper', GIT_CONFIG_VALUE_0='')
    return env


def command(args, env, cwd=None, log=None):
    result = subprocess.run([str(arg) for arg in args], env=env, cwd=cwd, capture_output=True)
    if log:
        log.with_suffix('.stdout').write_bytes(result.stdout)
        log.with_suffix('.stderr').write_bytes(result.stderr)
    result.check_returncode()
    return result.stdout


def file_bytes(path, mode):
    metadata = path.lstat()
    if mode == '120000':
        if not stat.S_ISLNK(metadata.st_mode):
            raise ValueError('not a symlink: ' + str(path))
        return os.fsencode(os.readlink(path))
    if not stat.S_ISREG(metadata.st_mode) or bool(metadata.st_mode & 0o111) != (mode == '100755'):
        raise ValueError('wrong kind or mode: ' + str(path))
    return path.read_bytes()


def verify_checkout(repo, manifest):
    for file in manifest['files']:
        if hashlib.sha256(file_bytes(repo / file['path'], file['mode'])).hexdigest() != file['sha256']:
            raise ValueError('checkout bytes differ: ' + file['path'])


def immutable_changes_digest(repo):
    value = hashlib.sha256()
    for path in sorted((repo / '.atomic/changes').rglob('*')):
        if path.is_file():
            name = os.fsencode(str(path.relative_to(repo / '.atomic/changes')))
            value.update(len(name).to_bytes(8, 'big'))
            value.update(name)
            value.update(bytes.fromhex(digest(path)))
    return value.hexdigest()


def prepare(args):
    root = args.root.resolve()
    root.mkdir(parents=True, exist_ok=False)
    (root / 'owner.json').write_text(json.dumps({'marker': MARKER, 'root': str(root)}))
    manifest = json.loads(args.manifest.read_text())
    validate_manifest(manifest)
    (root / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
    env = isolated_env(root)
    golden = root / 'golden'
    golden.mkdir()
    fixtures = {}
    binary_sha = digest(args.cli)
    for backend in BACKENDS:
        repo = root / backend
        command(['git', '-c', 'core.hooksPath=/dev/null', 'clone', '--no-checkout', args.bundle, repo], env)
        command(['git', '-c', 'core.hooksPath=/dev/null', 'checkout', '-b', 'atomic-bench', manifest['git_tip']], env, repo)
        tip = command(['git', 'rev-parse', 'HEAD'], env, repo).decode().strip()
        count = int(command(['git', 'rev-list', '--first-parent', '--count', 'HEAD'], env, repo))
        if tip != manifest['git_tip'] or count != manifest['first_parent_commits']:
            raise ValueError('wrong Git fixture identity')
        entries = command(['git', 'ls-tree', '-r', '-z', tip], env, repo)
        actual = set()
        for row in filter(None, entries.split(b'\0')):
            meta, path = row.split(b'\t', 1)
            mode, kind, oid = meta.decode().split()
            if kind != 'blob':
                raise ValueError('unsupported non-blob Git entry')
            actual.add((path.decode('utf-8'), mode, oid))
        expected = {(file['path'], file['mode'], file['git_blob']) for file in manifest['files']}
        if actual != expected:
            raise ValueError('manifest does not cover the exact pinned Git tree')
        verify_checkout(repo, manifest)
        index_sha = digest(repo / '.git/index')
        for index, file in enumerate(manifest['files']):
            blob = command(['git', 'cat-file', 'blob', file['git_blob']], env, repo)
            if hashlib.sha256(blob).hexdigest() != file['sha256']:
                raise ValueError('Git manifest mismatch')
            target = golden / str(index)
            if target.exists() and target.read_bytes() != blob:
                raise ValueError('graph/span Git baseline differs')
            target.write_bytes(blob)
        args_import = [args.cli, 'git', 'import', '--no-vault']
        if backend == 'span':
            args_import.append('--span')
        started = time.monotonic()
        command(args_import, env, repo, root / (backend + '-import'))
        import_seconds = time.monotonic() - started
        verify_checkout(repo, manifest)
        if digest(repo / '.git/index') != index_sha:
            raise ValueError('import changed Git index')
        repair = dict(requested=args.repair_derived_indexes, performed=False)
        try:
            command([args.cli, 'doctor', 'check'], env, repo, root / (backend + '-doctor'))
        except subprocess.CalledProcessError as error:
            repair['initial_doctor_exit'] = error.returncode
            if not args.repair_derived_indexes:
                raise
            shutil.copyfile(repo / '.atomic/atomic.redb', root / (backend + '-pre-repair.redb'))
            immutable_before = immutable_changes_digest(repo)
            log_before = command([args.cli, 'log', '--format', 'json'], env, repo)
            command([args.cli, 'doctor', 'repair-native-indexes'], env, repo, root / (backend + '-repair'))
            command([args.cli, 'doctor', 'check'], env, repo, root / (backend + '-doctor-after-repair'))
            if immutable_changes_digest(repo) != immutable_before:
                raise ValueError('repair changed immutable authored change files')
            if command([args.cli, 'log', '--format', 'json'], env, repo) != log_before:
                raise ValueError('repair changed recorded history')
            verify_checkout(repo, manifest)
            if digest(repo / '.git/index') != index_sha:
                raise ValueError('repair changed Git index')
            repair.update(performed=True, immutable_changes_sha256=immutable_before,
                          history_sha256=hashlib.sha256(log_before).hexdigest())
        database = repo / '.atomic/atomic.redb'
        baseline = root / (backend + '-baseline.redb')
        shutil.copyfile(database, baseline)
        # Reset just this closed database at its original canonical location
        # before each sample. Immutable changes and recovery artifacts remain.
        fixtures[backend] = dict(repo=str(repo), database_sha256=digest(baseline),
                                 database_bytes=baseline.stat().st_size,
                                 git_index_sha256=index_sha, import_seconds=import_seconds,
                                 derived_index_repair=repair)
    if digest(args.cli) != binary_sha:
        raise ValueError('import binary changed')
    metadata = dict(marker=MARKER, git_tip=manifest['git_tip'],
                    first_parent_commits=manifest['first_parent_commits'], files=len(manifest['files']),
                    cli_sha256=binary_sha, bundle_sha256=digest(args.bundle), fixtures=fixtures)
    (root / 'prepared.json').write_text(json.dumps(metadata, indent=2) + '\n')
    print(json.dumps(metadata, indent=2))


def check_owned_root(root):
    owner = json.loads((root / 'owner.json').read_text())
    if owner != dict(marker=MARKER, root=str(root)):
        raise ValueError('not the original disposable benchmark root')


def reset_fixture(root, backend, manifest, case):
    repo = root / backend
    shutil.copyfile(root / (backend + '-baseline.redb'), repo / '.atomic/atomic.redb')
    for index, file in enumerate(manifest['files']):
        path = repo / file['path']
        if path.exists() or path.is_symlink():
            if path.is_dir() and not path.is_symlink():
                raise ValueError('refusing to remove directory: ' + str(path))
            path.unlink()
        if case == 'render':
            path.parent.mkdir(parents=True, exist_ok=True)
            content = (root / 'golden' / str(index)).read_bytes()
            if file['mode'] == '120000':
                path.symlink_to(os.fsdecode(content))
            else:
                path.write_bytes(content)
                path.chmod(0o755 if file['mode'] == '100755' else 0o644)


def meminfo():
    return Path('/proc/meminfo').read_text()


def distribution(values):
    values = sorted(values)
    return dict(n=len(values), median_ms=statistics.median(values),
                p95_ms=values[math.ceil(.95 * len(values)) - 1], min_ms=values[0], max_ms=values[-1])


def measure(args):
    if platform.system() != 'Linux':
        raise ValueError('guest-only Linux cache reset required')
    if args.iterations < 2 or args.iterations % 2:
        raise ValueError('use an even number of iterations >= 2')
    root = args.root.resolve()
    check_owned_root(root)
    prepared = json.loads((root / 'prepared.json').read_text())
    manifest = json.loads((root / 'manifest.json').read_text())
    validate_manifest(manifest)
    env = isolated_env(root)
    if args.trace_span_context:
        env['ATOMIC_TRACE_SPAN_RENDER'] = '1'
    filesystem = command(['findmnt', '-n', '-o', 'FSTYPE,SOURCE,TARGET', '-T', root], env).decode().strip()
    if filesystem.split()[0] != 'ext4':
        raise ValueError('use a guest-local ext4 disk, not a host-shared mount: ' + filesystem)
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    binary = output / 'imported_materialize_bench'
    shutil.copy2(args.probe, binary)
    binary.chmod(0o700)
    binary_sha = digest(binary)
    samples = []
    metadata = dict(outcome='incomplete', cache_policy='sync + guest Linux drop_caches=3 before every fresh process',
                    host_cache_policy='uncontrolled; NOT physical-disk-cold or host-cache-cold',
                    filesystem=filesystem, iterations=args.iterations, rayon_threads=2,
                    binary_sha256=binary_sha, fixture=prepared, platform=platform.platform(),
                    logical_cpus=os.cpu_count(), guest_meminfo=meminfo(),
                    diagnostic_trace=args.trace_span_context,
                    timing='API total includes open, render/materialize and close; verification excluded; process wall includes verification')
    try:
        for iteration in range(args.iterations):
            for case in CASES:
                for offset in range(2):
                    backend = BACKENDS[(iteration + offset) % 2]
                    repo = root / backend
                    if digest(root / (backend + '-baseline.redb')) != prepared['fixtures'][backend]['database_sha256']:
                        raise ValueError('baseline database changed')
                    reset_fixture(root, backend, manifest, case)
                    os.sync()
                    before = meminfo()
                    Path('/proc/sys/vm/drop_caches').write_text('3\n')
                    after = meminfo()
                    stem = output / f'{iteration:02}-{case}-{backend}'
                    report = stem.with_suffix('.json')
                    command_line = [binary, '--repo', repo, '--manifest', root / 'manifest.json',
                                    '--output', report, '--backend', backend, '--case', case]
                    started = time.monotonic()
                    command(command_line, env, log=stem)
                    wall = time.monotonic() - started
                    sample = json.loads(report.read_text())
                    if sample['debug_build'] or not sample['head_verified'] or sample['verified_files'] != len(manifest['files']):
                        raise ValueError('unverified or debug benchmark sample')
                    if sample['git_tip'] != manifest['git_tip'] or sample['backend'] != backend or sample['case'] != case:
                        raise ValueError('wrong sample identity')
                    if digest(repo / '.git/index') != prepared['fixtures'][backend]['git_index_sha256']:
                        raise ValueError('measurement changed Git index')
                    sample.update(iteration=iteration, backend_order=offset,
                                  process_wall_including_verification_seconds=wall,
                                  meminfo_before_cache_reset=before, meminfo_after_cache_reset=after)
                    samples.append(sample)
        if len(samples) != args.iterations * len(CASES) * len(BACKENDS) or digest(binary) != binary_sha:
            raise ValueError('incomplete samples or replaced executable')
        summary = {}
        for case in CASES:
            summary[case] = {}
            for backend in BACKENDS:
                selected = [sample for sample in samples if sample['case'] == case and sample['backend'] == backend]
                summary[case][backend] = distribution([sample['total_ns'] / 1_000_000 for sample in selected])
                summary[case][backend]['order_ms'] = {str(order): distribution([
                    sample['total_ns'] / 1_000_000 for sample in selected if sample['backend_order'] == order]) for order in (0, 1)}
                phases = sorted({phase for sample in selected for phase in sample['phases_ns']})
                summary[case][backend]['phases_ms'] = {phase: distribution([
                    sample['phases_ns'][phase] / 1_000_000 for sample in selected if phase in sample['phases_ns']]) for phase in phases}
                context_phases = sorted({phase for sample in selected for phase in sample.get('span_context_phases_ns', {})})
                summary[case][backend]['span_context_phases_ms'] = {phase: distribution([
                    sample['span_context_phases_ns'][phase] / 1_000_000 for sample in selected
                    if phase in sample.get('span_context_phases_ns', {})]) for phase in context_phases}
        target = summary['materialize_missing']['span']
        metadata.update(outcome='diagnostic-guest-cache-cold' if args.trace_span_context else 'measured-guest-cache-cold', summary=summary,
                        performance_target=dict(case='materialize_missing', backend='span', upper_ms=200,
                                                median_met=target['median_ms'] <= 200, p95_met=target['p95_ms'] <= 200))
        print(json.dumps(summary, indent=2))
    except Exception as error:
        metadata.update(outcome='failed-or-invalid', error=str(error))
        raise
    finally:
        metadata['samples'] = samples
        (output / 'run.json').write_text(json.dumps(metadata, indent=2) + '\n')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest='command', required=True)
    setup = commands.add_parser('prepare')
    setup.add_argument('--root', type=Path, required=True)
    setup.add_argument('--bundle', type=Path, required=True)
    setup.add_argument('--manifest', type=Path, required=True)
    setup.add_argument('--cli', type=Path, required=True)
    setup.add_argument('--repair-derived-indexes', action='store_true',
                       help='explicit setup repair after a failed doctor check; preserve evidence and label the fixture')
    run = commands.add_parser('measure')
    run.add_argument('--root', type=Path, required=True)
    run.add_argument('--probe', type=Path, required=True)
    run.add_argument('--output', type=Path, required=True)
    run.add_argument('--iterations', type=int, default=8)
    run.add_argument('--trace-span-context', action='store_true',
                     help='emit per-context phase/counter diagnostics; label this run diagnostic, not an untraced timing baseline')
    args = parser.parse_args()
    prepare(args) if args.command == 'prepare' else measure(args)


if __name__ == '__main__':
    main()
