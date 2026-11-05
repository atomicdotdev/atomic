#!/usr/bin/env bash
# Runner/discovery contracts only; no repository mutations or CLI required.
set -euo pipefail
HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
python3 - "$HARNESS_DIR" <<'PY'
import pathlib, re, subprocess, sys
root = pathlib.Path(sys.argv[1])
def selected(*args):
    output = subprocess.check_output(['bash',str(root/'run_all.sh'),'--list',*args], text=True)
    return re.findall(r'• ([^\s]+\.sh)', output)
expected = ['35_git_shim_serialization.sh','39_git_shim_workflow.sh',
            '40_git_shim_safety.sh','41_git_shim_hooks.sh','42_git_shim_guard.sh',
            '43_git_shim_operation_recovery.sh']
assert selected('shim') == expected
assert selected('native-workflows') == ['40_switch_transactionality.sh','32_review_gate_insert.sh','23_shared_view_parent_chain.sh']
assert selected('33','34','36','38','40_git_shim_materialize_safety') == ['40_git_shim_safety.sh']
assert selected('37','39_git_shim_mvp','39_git_shim_workflow') == ['39_git_shim_workflow.sh']
assert selected('35_shadow_concurrent_push') == ['35_git_shim_serialization.sh']
assert selected('19') == ['39_git_shim_workflow.sh','41_git_shim_hooks.sh','19_unrecord.sh']
all_suites = selected()
assert len(all_suites) == len(set(all_suites))
assert not any('shadow' in name or name in ('39_git_shim_mvp.sh','40_git_shim_materialize_safety.sh') for name in all_suites)
assert set(expected) <= set(all_suites)
print('8 native/shim harness selector/discovery checks passed')
PY
