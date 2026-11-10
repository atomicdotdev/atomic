#!/usr/bin/env python3
"""Frozen, local-only agent-sandbox sync runs: atomic daemon RPC versus Git wire clone.

Wraps libatomic/examples/agent_sandbox_bench.rs with the same identity
discipline as run.py: the source tree is fingerprinted before and after build
and measurement, the release executable is copied into the artifact directory
and its SHA-256 is verified afterwards. Debug runs are smoke tests, never
performance evidence.
"""
import argparse
import json
from collections import defaultdict
import os
from pathlib import Path
import platform
import shutil
import subprocess
import time

sys_path = Path(__file__).resolve().parent
import sys  # noqa: E402

sys.path.insert(0, str(sys_path))
from run import digest, distribution, source_fingerprint  # noqa: E402

ROOT = sys_path.parents[1]
PROFILES = {
    'smoke': dict(files_east=2, files_west=2, lines=8, edits=1),
    'many-files': dict(files_east=8, files_west=16, lines=128, edits=24),
    'hot-file': dict(files_east=1, files_west=16, lines=4096, edits=48),
}
CASES = ('atomic_rpc_full', 'atomic_rpc_partial', 'git_clone_full')


def validate_report(report, iterations):
    expected = {(case, iteration) for case in CASES for iteration in range(iterations)}
    seen = set()
    for sample in report['samples']:
        key = (sample['case'], sample['iteration'])
        if key not in expected or key in seen:
            raise ValueError('unexpected or duplicate sample: ' + repr(key))
        seen.add(key)
        if not isinstance(sample['total_ns'], int) or sample['total_ns'] <= 0:
            raise ValueError('nonpositive or invalid sample duration')
    if seen != expected:
        raise ValueError('not all cases produced samples')


def summarize(report):
    groups = defaultdict(list)
    phases = defaultdict(lambda: defaultdict(list))
    for sample in report['samples']:
        groups[sample['case']].append(sample['total_ns'] / 1_000_000)
        for name, nanos in sample['phases_ns'].items():
            phases[sample['case']][name].append(nanos / 1_000_000)
    return {case: dict(distribution(values),
                       phases_ms={name: distribution(values) for name, values in phases[case].items()})
            for case, values in groups.items()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    selection = parser.add_mutually_exclusive_group(required=True)
    selection.add_argument('--build', action='store_true',
                           help='build offline/locked/release from fingerprinted source')
    selection.add_argument('--binary', type=Path,
                           help='existing agent_sandbox_bench executable (build provenance not assumed)')
    parser.add_argument('--output', required=True, type=Path,
                        help='new artifact directory; must not exist')
    parser.add_argument('--profile', choices=PROFILES, default='many-files')
    for name in ('files-east', 'files-west', 'lines', 'edits'):
        parser.add_argument('--' + name, type=int)
    parser.add_argument('--iterations', type=int, default=8)
    parser.add_argument('--warmups', type=int, default=1)
    parser.add_argument('--keep-fixtures', action='store_true')
    parser.add_argument('--allow-debug', action='store_true',
                        help='correctness smoke only, not performance evidence')
    args = parser.parse_args()
    if args.iterations < 1 or args.warmups < 0:
        parser.error('iterations must be positive and warmups nonnegative')
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    output.chmod(0o700)
    source_before = source_fingerprint()
    metadata = dict(outcome='incomplete', source_sha256=source_before,
                    source_binding='fingerprinted offline release build' if args.build
                    else 'caller-supplied binary; source/build association unverified',
                    platform=platform.platform(), logical_cpus=os.cpu_count(),
                    concurrency_control='OS cache and unrelated system load are not controlled')
    try:
        candidate = args.binary.resolve() if args.binary else None
        if args.build:
            command = ['cargo', 'build', '--release', '--offline', '--locked', '-p', 'libatomic',
                       '--example', 'agent_sandbox_bench', '--message-format=json-render-diagnostics']
            build_env = dict(os.environ, CARGO_NET_OFFLINE='true')
            build_env.setdefault('CARGO_BUILD_JOBS', '2')
            build_env.setdefault('CARGO_PROFILE_RELEASE_STRIP', 'none')
            metadata['build_command'] = command
            result = subprocess.run(command, cwd=ROOT, env=build_env, capture_output=True)
            (output / 'build.stdout.jsonl').write_bytes(result.stdout)
            (output / 'build.stderr.log').write_bytes(result.stderr)
            result.check_returncode()
            for line in result.stdout.splitlines():
                event = json.loads(line)
                if event.get('reason') == 'compiler-artifact' and \
                        event.get('target', {}).get('name') == 'agent_sandbox_bench' and \
                        event.get('executable'):
                    candidate = Path(event['executable'])
            if candidate is None:
                raise RuntimeError('Cargo did not report the benchmark executable')
            if source_fingerprint() != source_before:
                raise RuntimeError('source changed during build; discard this candidate')
        if candidate is None or not candidate.is_file():
            raise RuntimeError('benchmark executable not found')
        binary = output / 'agent_sandbox_bench'
        shutil.copy2(candidate, binary)
        metadata['binary_sha256'] = digest(binary)
        config = {name.replace('_', '-'): value
                  for name, value in PROFILES[args.profile].items()}
        for name in ('files-east', 'files-west', 'lines', 'edits'):
            value = getattr(args, name.replace('-', '_'))
            if value is not None:
                config[name] = value
        command = [str(binary), '--output', str(output / 'raw.json'),
                   '--iterations', str(args.iterations), '--warmups', str(args.warmups)]
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
        env.update(HOME=str(home), ATOMIC_HOME=str(home / '.atomic'),
                   ATOMIC_CONFIG_DIR=str(home / 'config'), ATOMIC_SERVICE='local',
                   TMPDIR=str(temporary))
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
        metadata.update(outcome='smoke-only' if report['debug_build'] else 'measured',
                        summary_ms=summary)
        print('| Case | Median ms | Mean ms | p95 ms |')
        print('|---|---:|---:|---:|')
        for case in CASES:
            row = summary[case]
            print(f"| {case} | {row['median']:.3f} | {row['mean']:.3f} | {row['p95']:.3f} |")
        print('Artifacts:', output)
    except Exception as error:
        metadata.update(outcome='failed-or-invalid', error=str(error))
        raise
    finally:
        (output / 'run.json').write_text(json.dumps(metadata, indent=2, sort_keys=True) + '\n')


if __name__ == '__main__':
    main()
