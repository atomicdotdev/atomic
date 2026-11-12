"""Native review-readiness -> insert original records -> retire Draft, without Git."""
import re
import sys

from native_support import ROOT, call, check, data, files_under, record, view


def history(name):
    return data('log', '--view', name, '--all', '--format', 'json')


def new_intent(title, target=None):
    before = {row['id'] for row in data('intent', 'list', '--json')}
    args = ['intent', 'new', title]
    if target:
        args += ['--review', 'urn:atomic:intent:' + target]
    call(*args)
    row = next(row for row in data('intent', 'list', '--json') if row['id'] not in before)
    for path in (ROOT / '.vault/intents').glob('*/intent.md'):
        text = path.read_text()
        if re.search(r'^id: ' + re.escape(row['id']) + r'$', text, re.M):
            uid = re.search(r'^uid: (.+)$', text, re.M).group(1)
            return row['id'], uid
    raise AssertionError('created intent has no materialized identity')


def work_body(uid, complete):
    criterion = 'status=met verifiedBy="native-fixture" evidence="Exact fixture bytes checked before attestation"' if complete else 'status=unmet'
    status = 'done' if complete else 'open'
    return f''':::why
Preserve the two original native records during promotion.
:::
:::acceptance-criterion{{#{uid}-ac-1 {criterion}}}
The two source files have the expected fixture bytes.
:::
:::task{{#{uid}-1 status={status} criteria={uid}-ac-1}}
Implement the two fixture files.
::file-ref{{path=src/one.rs}}
::file-ref{{path=src/two.rs}}
:::
:::scope-in
src/one.rs and src/two.rs.
:::
:::scope-out
Git imports, remotes and production user data.
:::
:::constraint
Original change hashes and bytes must survive promotion.
:::
'''


def review_body(uid, target, verifier):
    return f''':::why
Independently check the native fixture's exact bytes and original records.
:::
:::acceptance-criterion{{#{uid}-ac-1 status=met verifiedBy="{verifier}" evidence="Both original records and expected file bytes inspected"}}
The native fixture meets its acceptance criterion.
:::
:::task{{#{uid}-1 status=done criteria={uid}-ac-1}}
Review the work intent and its fixture records.
:::
:::ref{{to=urn:atomic:intent:{target} edge=reviews}}
:::
:::scope-in
The native work intent and its two original code records.
:::
:::scope-out
Git squash import or surrogate change synthesis.
:::
:::constraint
The review signer must differ from the work signer.
:::
'''


def complete_intent(identity, body, signer):
    call('intent', 'update', identity, '--body', body, '--force')
    call('intent', 'attest', identity, '--identity', signer)
    call('intent', 'update', identity, '--status', 'done')
    call('intent', 'attest', identity, '--identity', signer)
    call('intent', 'validate', identity)
    call('intent', 'verify', identity, '--identity', signer)


def report():
    return data('triage', 'review', 'agent', '--into', 'dev', '--json')


def main():
    call('identity', 'new', 'native-author', '--set-default')
    call('identity', 'new', 'native-reviewer')
    call('init')
    check('native review fixture has no Git repository', not (ROOT / '.git').exists())
    work_id, work_uid = new_intent('Native originals fixture')
    call('intent', 'update', work_id, '--body', work_body(work_uid, False), '--force')
    call('intent', 'attest', work_id, '--identity', 'native-author')
    call('add', '.vault')
    record('record work explanation on shared baseline')
    call('view', 'create', 'agent', '--draft', '--parent', 'dev', '--switch')
    (ROOT / 'src').mkdir()
    (ROOT / 'src/one.rs').write_bytes(b'fn one() {}\n')
    call('add', 'src/one.rs')
    record('original one')
    first = history('agent')[0]['hash']
    (ROOT / 'src/one.rs').write_bytes(b'fn one() { two(); }\n')
    (ROOT / 'src/two.rs').write_bytes(b'fn two() {}\n')
    call('add', 'src/two.rs')
    record('original two depends on one')
    second = history('agent')[0]['hash']
    check('two distinct original code identities recorded', first != second)
    original = data('change', second, '--format', 'json')
    check('second original retains dependency on first', first in original['dependencies'])
    before_review = report()
    check('unmet and unreviewed work is blocked in triage', before_review['verdict'] == 'blocked')
    check('triage names both original records', {first, second} <= set(before_review['inputs']['candidate_changes']))

    check('work bytes satisfy the fixture before completion',
          (ROOT / 'src/one.rs').read_bytes() == b'fn one() { two(); }\n' and
          (ROOT / 'src/two.rs').read_bytes() == b'fn two() {}\n')
    complete_intent(work_id, work_body(work_uid, True), 'native-author')
    self_id, self_uid = new_intent('Self-review negative fixture', work_uid)
    complete_intent(self_id, review_body(self_uid, work_uid, 'native-author'), 'native-author')
    self_report = report()
    check('self-authored review does not satisfy independent-review readiness', any(
        finding['code'] == 'UNREVIEWED_CHANGE' and finding['severity'] == 'block'
        for finding in self_report['findings']))
    call('identity', 'default', 'native-reviewer')
    independent_id, independent_uid = new_intent('Independent fixture review', work_uid)
    complete_intent(independent_id, review_body(independent_uid, work_uid, 'native-reviewer'), 'native-reviewer')
    call('add', '.vault')
    record('retain signed work and review evidence')
    ready = report()
    check('independently reviewed work is ready for native insertion', ready['verdict'] == 'ready')
    check('independent review clears the review finding', not any(f['code'] == 'UNREVIEWED_CHANGE' for f in ready['findings']))
    check('review report retains both original code identities', {first, second} <= set(ready['inputs']['candidate_changes']))
    source_history = history('agent')
    target_before_preview = history('dev')
    source_hashes = {row['hash'] for row in source_history}
    # Pin both histories immediately before preview; code originals must
    # still be absent from the target until explicit insertion.
    baseline_hashes = {row['hash'] for row in target_before_preview}
    check('code originals remain Draft-only before insertion', not ({first, second} & baseline_hashes))
    candidate_hashes = source_hashes - baseline_hashes
    object_bytes = {str(path): path.read_bytes() for path in (ROOT / '.atomic/changes').rglob('*.change')}
    before_files = files_under(ROOT)
    before_ops = data('op', 'log', '--json')
    preview = call('insert', 'from-view', 'agent', '--to-view', 'dev', '--dry-run')
    check('native preview names the two originals', all(h[:8] in preview.stdout.decode() for h in (first, second)))
    check('preview preserves target history', history('dev') == target_before_preview)
    check('preview preserves source history', history('agent') == source_history)
    check('preview preserves working bytes', files_under(ROOT) == before_files)
    check('preview appends no working-copy operation', data('op', 'log', '--json') == before_ops)
    call('insert', 'from-view', 'agent', '--to-view', 'dev')
    check('promotion leaves current Draft selected', view() == 'agent')
    check('promotion preserves source history', history('agent') == source_history)
    promoted = history('dev')
    check('target references exactly the originals and evidence, no surrogate change', {r['hash'] for r in promoted} == baseline_hashes | candidate_hashes)
    check('both original code records now belong to Shared view', {first, second} <= {r['hash'] for r in promoted})
    check('promotion preserves all immutable change bytes', {str(p): p.read_bytes() for p in (ROOT / '.atomic/changes').rglob('*.change')} == object_bytes)
    call('insert', 'from-view', 'agent', '--to-view', 'dev')
    check('repeat insertion is membership-idempotent', history('dev') == promoted)
    call('view', 'switch', 'dev', '--force')
    check('Shared view materializes exact original result',
          (ROOT / 'src/one.rs').read_bytes() == b'fn one() { two(); }\n' and
          (ROOT / 'src/two.rs').read_bytes() == b'fn two() {}\n')
    call('view', 'delete', 'agent', '--force')
    check('landed Draft can be retired', not any(row['name'] == 'agent' for row in data('view', 'list', '--all', '--json')['views']))
    check('retiring Draft preserves Shared history', history('dev') == promoted)
    check('original change remains inspectable after Draft retirement', data('change', second, '--format', 'json')['hash'] == second)
    check('Shared working copy remains clean after Draft retirement', not call('status', '--short', '--no-untracked').stdout.strip())
    call('intent', 'verify', independent_id, '--identity', 'native-reviewer')
    check('signed independent review survives Draft retirement', True)


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        import traceback
        traceback.print_exc()
        print('FAIL\t' + str(error), flush=True)
        sys.exit(1)
