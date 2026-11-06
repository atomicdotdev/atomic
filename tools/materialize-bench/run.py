#!/usr/bin/env python3
"""Frozen, local-only graph/span materialization runs; retain raw samples and identities."""
import argparse
from collections import defaultdict
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import shutil
import statistics
import subprocess
import time

ROOT = Path(__file__).resolve().parents[2]
PROFILES = {
    'smoke': dict(files=2, lines=8, edits=2),
    'many-files': dict(files=24, lines=128, edits=24),
    'hot-file': dict(files=1, lines=4096, edits=48),
}
CASES = ('render_session', 'render_reopened', 'materialize_noop', 'materialize_missing',
         'switch_one_forward', 'switch_one_back', 'switch_many_forward', 'switch_many_back')


def validate_report(report, iterations):
    expected = {(case, backend, iteration) for case in CASES
                for backend in ('graph', 'span') for iteration in range(iterations)}
    seen = set()
    for sample in report['samples']:
        key = (sample['case'], sample['backend'], sample['iteration'])
        if key not in expected or key in seen:
            raise ValueError('unexpected or duplicate sample: ' + repr(key))
        seen.add(key)
        if not isinstance(sample['total_ns'], int) or sample['total_ns'] <= 0:
            raise ValueError('nonpositive or invalid sample duration')
    if seen != expected:
        raise ValueError('not all paired cases produced samples')


def digest(path):
    value = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            value.update(block)
    return value.hexdigest()


def source_fingerprint():
    """Hash source bytes directly, never Git metadata or a writable Atomic open."""
    names = ['Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml', 'libatomic', 'tools']
    names += sorted(p.name for p in ROOT.iterdir() if p.is_dir() and p.name.startswith('atomic-'))
    files = []
    for name in names:
        path = ROOT / name
        if path.is_file():
            files.append(path)
        elif path.is_dir():
            for directory, dirs, entries in os.walk(path, followlinks=False):
                dirs[:] = sorted(d for d in dirs if d not in ('target', '.git', '.atomic', '__pycache__'))
                links = [d for d in dirs if (Path(directory) / d).is_symlink()]
                files.extend(Path(directory) / d for d in links)
                dirs[:] = [d for d in dirs if d not in links]
                files.extend(Path(directory) / entry for entry in entries)
    value = hashlib.sha256()
    for path in sorted(set(files), key=lambda p: os.fsencode(str(p.relative_to(ROOT)))):
        relative = os.fsencode(str(path.relative_to(ROOT)))
        value.update(len(relative).to_bytes(8, 'big'))
        value.update(relative)
        value.update(b'L' if path.is_symlink() else b'F')
        value.update(bytes([bool(path.lstat().st_mode & 0o111)]))
        content = os.fsencode(os.readlink(path)) if path.is_symlink() else path.read_bytes()
        value.update(len(content).to_bytes(8, 'big'))
        value.update(content)
    return value.hexdigest()


def distribution(values):
    ordered = sorted(values)
    return dict(n=len(values), median=statistics.median(values), mean=statistics.mean(values),
                p95=ordered[max(0, math.ceil(0.95 * len(values)) - 1)],
                minimum=ordered[0], maximum=ordered[-1],
                stdev=statistics.stdev(values) if len(values) > 1 else 0.0)


def summarize(report):
    groups = defaultdict(list)
    phases = defaultdict(lambda: defaultdict(list))
    order_groups = defaultdict(lambda: defaultdict(list))
    for sample in report['samples']:
        key = (sample['case'], sample['backend'])
        groups[key].append(sample['total_ns'] / 1_000_000)
        # The executable alternates backend order per iteration, including
        # excluded warmups. Keep this confound visible, especially for odd n.
        first = (sample['iteration'] + report['config']['warmups']) % 2
        order = 'first' if first == (0 if sample['backend'] == 'graph' else 1) else 'second'
        order_groups[key][order].append(sample['total_ns'] / 1_000_000)
        for phase, nanos in sample['phases_ns'].items():
            phases[key][phase].append(nanos / 1_000_000)
    summary = []
    for case in sorted({key[0] for key in groups}):
        entry = {'case': case}
        for backend in ('graph', 'span'):
            key = (case, backend)
            entry[backend] = distribution(groups[key])
            entry[backend]['phases_ms'] = {name: distribution(values) for name, values in phases[key].items()}
            entry[backend]['order_ms'] = {name: distribution(values) for name, values in order_groups[key].items()}
        entry['graph_over_span_median'] = entry['graph']['median'] / entry['span']['median']
        summary.append(entry)
    return summary


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    selection = parser.add_mutually_exclusive_group(required=True)
    selection.add_argument('--build', action='store_true', help='build offline/locked/release from fingerprinted source')
    selection.add_argument('--binary', type=Path, help='explicit existing materialize_bench executable (build provenance not assumed)')
    parser.add_argument('--output', required=True, type=Path, help='new artifact directory; must not exist')
    parser.add_argument('--profile', choices=PROFILES, default='many-files')
    for name in ('files', 'lines', 'edits'):
        parser.add_argument('--' + name, type=int)
    parser.add_argument('--iterations', type=int, default=8)
    parser.add_argument('--warmups', type=int, default=1)
    parser.add_argument('--threads', type=int, default=2)
    parser.add_argument('--keep-fixtures', action='store_true')
    parser.add_argument('--allow-debug', action='store_true', help='correctness smoke only, not performance evidence')
    args = parser.parse_args()
    if args.threads < 1 or args.iterations < 1 or args.warmups < 0:
        parser.error('threads/iterations must be positive and warmups nonnegative')
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    output.chmod(0o700)
    source_before = source_fingerprint()
    metadata = dict(outcome='incomplete', source_sha256=source_before,
                    source_binding='fingerprinted offline release build' if args.build else 'caller-supplied binary; source/build association unverified',
                    platform=platform.platform(), logical_cpus=os.cpu_count(), threads=args.threads,
                    balanced_backend_order=args.iterations % 2 == 0,
                    concurrency_control='OS cache and unrelated system load are not controlled')
    try:
        candidate = args.binary.resolve() if args.binary else None
        if args.build:
            command = ['cargo', 'build', '--release', '--offline', '--locked', '-p', 'atomic-repository',
                       '--example', 'materialize_bench', '--message-format=json-render-diagnostics']
            build_env = dict(os.environ, CARGO_NET_OFFLINE='true')
            build_env.setdefault('CARGO_BUILD_JOBS', '2')
            # Avoid platform-specific stripping tools; recorded here so build
            # settings are explicit. This changes debug stripping, not safety.
            build_env.setdefault('CARGO_PROFILE_RELEASE_STRIP', 'none')
            metadata['build_command'] = command
            metadata['release_strip'] = build_env['CARGO_PROFILE_RELEASE_STRIP']
            result = subprocess.run(command, cwd=ROOT, env=build_env, capture_output=True)
            (output / 'build.stdout.jsonl').write_bytes(result.stdout)
            (output / 'build.stderr.log').write_bytes(result.stderr)
            result.check_returncode()
            for line in result.stdout.splitlines():
                event = json.loads(line)
                if event.get('reason') == 'compiler-artifact' and event.get('target', {}).get('name') == 'materialize_bench' and event.get('executable'):
                    candidate = Path(event['executable'])
            if candidate is None:
                raise RuntimeError('Cargo did not report the benchmark executable')
            if source_fingerprint() != source_before:
                raise RuntimeError('source changed during build; discard this candidate')
        if candidate is None or not candidate.is_file():
            raise RuntimeError('benchmark executable not found')
        binary = output / 'materialize_bench'
        shutil.copy2(candidate, binary)
        metadata['binary_sha256'] = digest(binary)
        config = dict(PROFILES[args.profile])
        config.update({name: getattr(args, name) for name in ('files', 'lines', 'edits') if getattr(args, name) is not None})
        command = [str(binary), '--output', str(output / 'raw.json'), '--iterations', str(args.iterations),
                   '--warmups', str(args.warmups)]
        for name, value in config.items():
            command += ['--' + name, str(value)]
        if args.keep_fixtures:
            command.append('--keep-fixtures')
        if args.allow_debug:
            command.append('--allow-debug')
        home = output / 'home'
        temporary = output / 'tmp'
        home.mkdir()
        temporary.mkdir()
        env = {key: value for key, value in os.environ.items()
               if not key.startswith(('ATOMIC_', 'GIT_', 'XDG_'))}
        env.update(HOME=str(home), ATOMIC_HOME=str(home / '.atomic'), ATOMIC_CONFIG_DIR=str(home / 'config'),
                   ATOMIC_SERVICE='local', TMPDIR=str(temporary), RAYON_NUM_THREADS=str(args.threads))
        metadata['command'] = command
        started = time.monotonic()
        result = subprocess.run(command, cwd=ROOT, env=env, capture_output=True)
        metadata['runner_elapsed_seconds'] = time.monotonic() - started
        metadata['exit_code'] = result.returncode
        (output / 'stdout.log').write_bytes(result.stdout)
        (output / 'stderr.log').write_bytes(result.stderr)
        result.check_returncode()
        if source_fingerprint() != source_before or digest(binary) != metadata['binary_sha256']:
            raise RuntimeError('source or pinned binary changed during measurement; results invalid')
        report = json.loads((output / 'raw.json').read_text())
        validate_report(report, args.iterations)
        summary = summarize(report)
        metadata.update(outcome='smoke-only' if report['debug_build'] else 'measured', summary_ms=summary)
        print('| Case | Graph median ms | Span median ms | Graph / span |')
        print('|---|---:|---:|---:|')
        for row in summary:
            print(f"| {row['case']} | {row['graph']['median']:.3f} | {row['span']['median']:.3f} | {row['graph_over_span_median']:.2f}× |")
        print('Artifacts:', output)
    except Exception as error:
        metadata.update(outcome='failed-or-invalid', error=str(error))
        raise
    finally:
        (output / 'run.json').write_text(json.dumps(metadata, indent=2, sort_keys=True) + '\n')


if __name__ == '__main__':
    main()
