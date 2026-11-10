#!/usr/bin/env python3
"""Timed Git working-copy baseline for materialization comparisons.

Builds a disposable Git repository with the same synthetic history shapes as
atomic-repository/examples/materialize_bench.rs (same files, lines, edits and
commit count), then times `git checkout --` full working-tree restores after
removing files. Bytes and modes of every file, plus a clean `git status`, are
verified after every sample outside timing.

This measures Git's worktree-write floor: checkout restores blob content from
the index. Git history reads (clone/fetch/pack access) are a different
operation and are not measured here. subprocess spawn overhead is inside
total_ns. OS caches are uncontrolled. Compare its report only against an
atomic materialize_bench run with the same profile on the same machine.
"""
import argparse
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, str(Path(__file__).resolve().parent))
from run import PROFILES, distribution  # noqa: E402

CASES = ('checkout_full_noop', 'checkout_full_missing')


def generated_line(file_index, line_index):
    return f'file_{file_index:04} line_{line_index:06} value_00000000\n'


def edited_line(line_index, edit):
    return f'file_0000 line_{line_index:06} value_{edit + 1:08}\n'


def build_golden(files, lines):
    golden = {}
    for index in range(files):
        golden[f'files/file_{index:04}.txt'] = ''.join(
            generated_line(index, line) for line in range(lines)).encode()
    golden['empty.txt'] = b''
    return golden


def git(root, *args, dates=None):
    env = dict(os.environ)
    env.update(
        GIT_AUTHOR_NAME='bench', GIT_AUTHOR_EMAIL='bench@example.invalid',
        GIT_COMMITTER_NAME='bench', GIT_COMMITTER_EMAIL='bench@example.invalid')
    if dates is not None:
        stamp = f'{1_600_000_000 + dates} +0000'
        env.update(GIT_AUTHOR_DATE=stamp, GIT_COMMITTER_DATE=stamp)
    result = subprocess.run(['git', *args], cwd=root, env=env, capture_output=True)
    if result.returncode != 0:
        raise RuntimeError(f'git {args[0]} failed: {result.stderr.decode(errors="replace")}')
    return result.stdout


def build_fixture(root, files, lines, edits):
    (root / 'files').mkdir(parents=True)
    contents = {f'files/file_{index:04}.txt':
                ''.join(generated_line(index, line) for line in range(lines))
                for index in range(files)}
    for path, content in contents.items():
        (root / path).write_text(content)
    (root / 'empty.txt').write_bytes(b'')
    git(root, '-c', 'init.defaultBranch=dev', 'init', '-q')
    git(root, 'add', '-A')
    git(root, 'commit', '-q', '-m', 'synthetic baseline', dates=0)
    hot = contents[f'files/file_0000.txt'].splitlines(keepends=True)
    for edit in range(edits):
        line = (edit * 17) % lines
        hot[line] = edited_line(line, edit)
        (root / 'files/file_0000.txt').write_text(''.join(hot))
        git(root, 'add', '-A')
        git(root, 'commit', '-q', '-m', 'synthetic hot-file edit', dates=edit + 1)
    # The golden state is the final dev tip, after every edit — the same
    # capture point as the atomic fixture's "dev" golden.
    golden = build_golden(files, lines)
    golden['files/file_0000.txt'] = ''.join(hot).encode()
    return golden


def verify(root, golden):
    for path, content in golden.items():
        target = root / path
        if not target.is_file():
            raise RuntimeError(f'missing after checkout: {path}')
        if target.read_bytes() != content:
            raise RuntimeError(f'bytes differ: {path}')
    status = git(root, 'status', '--porcelain')
    if status:
        raise RuntimeError(f'working tree not clean: {status.decode(errors="replace")!r}')


def sample(root, golden, case):
    if case == 'checkout_full_missing':
        for path in sorted(golden):
            (root / path).unlink()
    started = time.monotonic_ns()
    git(root, 'checkout', '--', '.')
    total_ns = time.monotonic_ns() - started
    # Filesystem equality and a clean index/status check are never timed.
    verify(root, golden)
    return {'case': case, 'total_ns': total_ns, 'phases_ns': {},
            'verified_files': len(golden),
            'output_bytes': sum(len(content) for content in golden.values())}


def run(output, files, lines, edits, iterations, warmups, keep_fixtures):
    if files <= 0 or lines < 2 or iterations < 1 or warmups < 0:
        raise ValueError('invalid fixture or iteration configuration')
    git_version = subprocess.run(['git', '--version'], capture_output=True,
                                 check=True).stdout.decode().strip()
    output.mkdir(parents=True, exist_ok=False)
    output.chmod(0o700)
    fixture = Path(tempfile.mkdtemp(dir=output, prefix='fixture-'))
    metadata = {'outcome': 'incomplete', 'tool': 'git_baseline', 'schema': 1,
                'git_version': git_version, 'platform': platform.platform(),
                'logical_cpus': os.cpu_count(),
                'cache_policy': 'OS caches uncontrolled; not cold-cache claims',
                'boundary': 'git checkout restores from the index; pack/clone '
                            'history reads and subprocess spawn overhead are '
                            'inside total_ns'}
    try:
        golden = build_fixture(fixture, files, lines, edits)
        samples = []
        for iteration in range(warmups + iterations):
            for case in CASES:
                measured = sample(fixture, golden, case)
                if iteration >= warmups:
                    measured['iteration'] = iteration - warmups
                    samples.append(measured)
        metadata.update(config=dict(files=files, lines=lines, edits=edits,
                                    iterations=iterations, warmups=warmups),
                        fixture=dict(files=len(golden)))
        metadata['samples'] = samples
        (output / 'raw.json').write_text(json.dumps({'schema': 1, 'samples': samples}, indent=2) + '\n')
        grouped = {}
        for entry in samples:
            grouped.setdefault(entry['case'], []).append(entry['total_ns'] / 1_000_000)
        summary = {case: distribution(values) for case, values in grouped.items()}
        metadata['outcome'] = 'measured'
        metadata['summary_ms'] = summary
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
        if not keep_fixtures:
            shutil.rmtree(fixture, ignore_errors=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', required=True, type=Path,
                        help='new artifact directory; must not exist')
    parser.add_argument('--profile', choices=PROFILES, default='many-files')
    for name in ('files', 'lines', 'edits'):
        parser.add_argument('--' + name, type=int)
    parser.add_argument('--iterations', type=int, default=8)
    parser.add_argument('--warmups', type=int, default=1)
    parser.add_argument('--keep-fixtures', action='store_true')
    args = parser.parse_args()
    config = dict(PROFILES[args.profile])
    config.update({name: getattr(args, name) for name in ('files', 'lines', 'edits')
                   if getattr(args, name) is not None})
    run(args.output, iterations=args.iterations, warmups=args.warmups,
        keep_fixtures=args.keep_fixtures, **config)


if __name__ == '__main__':
    main()
