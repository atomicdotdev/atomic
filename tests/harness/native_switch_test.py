"""Native switch failure contract: journaled synchronous rollback, not legacy markers."""
import sys

from native_support import ROOT, call, check, data, files_under, record, view


def main():
    call('init', '--no-vault')
    check('fixture is native-only (no Git repository)', not (ROOT / '.git').exists())
    (ROOT / '.atomicignore').write_bytes(b'cache/\n')
    (ROOT / 'source.txt').write_bytes(b'source view\n')
    (ROOT / 'source.txt').chmod(0o755)
    call('add', '.atomicignore', 'source.txt')
    record('source')
    source = view()
    (ROOT / 'cache').mkdir()
    (ROOT / 'cache/state.txt').write_bytes(b'source cache\n')
    call('view', 'create', 'target', '--draft', '--parent', source, '--switch')
    (ROOT / 'nested').mkdir(exist_ok=True)
    (ROOT / 'nested/target.txt').write_bytes(b'target view\n')
    call('add', 'nested/target.txt')
    record('target')
    (ROOT / 'cache').mkdir(exist_ok=True)
    (ROOT / 'cache/state.txt').write_bytes(b'target cache\n')
    call('view', 'switch', source, '--force')
    (ROOT / 'untracked.txt').write_bytes(b'undecided user work\n')
    working_copy = (ROOT / '.atomic/working_copy_id').read_text().strip()
    shelves = ROOT / '.atomic/working-copies' / working_copy / 'workspaces'
    check('source ignored artifact is materialized', (ROOT / 'cache/state.txt').read_bytes() == b'source cache\n')
    check('target ignored artifact is shelved', any('target' in p and row for p, row in files_under(shelves).items()))

    for phase, diagnostic in [
        ('AFTER_PUBLISH', 'injected switch failure after view publication'),
        ('AFTER_MATERIALIZE', 'injected switch failure after materialization'),
    ]:
        baseline_files = files_under(ROOT)
        baseline_shelves = files_under(shelves)
        histories = {name: data('log', '--view', name, '--all', '--format', 'json') for name in (source, 'target')}
        old_log = data('op', 'log', '--json')
        old_ids = {entry['id'] for entry in old_log['entries']}
        result = call('view', 'switch', 'target', '--force',
                      env={'ATOMIC_TEST_FAIL_SWITCH_' + phase: '1'}, success=False)
        check(phase + ': requested failure seam actually fires', result.returncode != 0 and diagnostic in result.stderr.decode() + result.stdout.decode())

        # Inspect BEFORE any writable retry: switch_view_with_operation performs
        # its inverse recovery synchronously on a returned error.
        log = data('op', 'log', '--json')
        added = [entry for entry in log['entries'] if entry['id'] not in old_ids]
        failed = [entry for entry in added if entry['kind'] == 'switch_view']
        check(phase + ': failed switch is durably journaled once', len(failed) == 1)
        heads = [entry for entry in log['entries'] if entry['is_head']]
        check(phase + ': inverse recovery is the verified head before retry',
              len(heads) == 1 and heads[0]['kind'] == 'recover' and heads[0]['verification'] == 'verified')
        recovery = data('op', 'show', heads[0]['id'], '--json')
        check(phase + ': inverse recovery links to failed switch', failed[0]['id'] in recovery['parents'])
        check(phase + ': recovery has a verified receipt', any(r['kind'] == 'verified' for r in recovery['receipts']))
        original = data('op', 'show', failed[0]['id'], '--json')
        check(phase + ': original plan leases the working copy', any(e['target']['kind'] == 'working_copy' for e in original['delta']['effects']))
        check(phase + ': original plan leases target materialization', any(e['target'].get('path') == 'nested/target.txt' for e in original['delta']['effects']))
        check(phase + ': ignored artifacts have shelf leases', any(e['target']['kind'] == 'shelf_path' for e in original['delta']['effects']))
        check(phase + ': files, links and modes already restored', files_under(ROOT) == baseline_files)
        check(phase + ': both views shelves restored', files_under(shelves) == baseline_shelves)
        check(phase + ': source view restored', view() == source)
        check(phase + ': target-only content absent after rollback', not (ROOT / 'nested/target.txt').exists())
        check(phase + ': tracked status is clean after rollback', not call('status', '--short', '--no-untracked').stdout.strip())
        for name in (source, 'target'):
            check(phase + ': immutable history retained for ' + name,
                  data('log', '--view', name, '--all', '--format', 'json') == histories[name])

        call('add', 'source.txt')
        check(phase + ': writable reopen is recovery-idempotent', data('op', 'log', '--json') == log)
        check(phase + ': writable reopen preserves user artifacts', files_under(ROOT) == baseline_files)
        call('view', 'switch', 'target', '--force')
        check(phase + ': ordinary retry selects target', view() == 'target')
        check(phase + ': target content is byte-exact', (ROOT / 'nested/target.txt').read_bytes() == b'target view\n')
        check(phase + ': target ignored artifact restored', (ROOT / 'cache/state.txt').read_bytes() == b'target cache\n')
        check(phase + ': untracked bytes survive retry', (ROOT / 'untracked.txt').read_bytes() == b'undecided user work\n')
        check(phase + ': inherited executable survives retry', (ROOT / 'source.txt').stat().st_mode & 0o111 != 0)
        check(phase + ': tracked status is clean after retry', not call('status', '--short', '--no-untracked').stdout.strip())
        check(phase + ': successful retry has a verified switch head', any(e['is_head'] and e['kind'] == 'switch_view' and e['verification'] == 'verified' for e in data('op', 'log', '--json')['entries']))
        call('view', 'switch', source, '--force')
        check(phase + ': round-trip restores source artifacts', files_under(ROOT) == baseline_files)


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        import traceback
        traceback.print_exc()
        print('FAIL\t' + str(error), flush=True)
        sys.exit(1)
