#!/usr/bin/env bash
# 31_init_lazy_untracked.sh — initialization records must not scan unrelated
# untracked files when their complete path set is already known.
#
# A FIFO makes this regression deterministic: the old implementation hashed
# every untracked entry and blocked opening the FIFO for reading. The optimized
# path never visits or reads it, so `atomic init` completes normally.

HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$HARNESS_DIR/helpers.sh"

echo "${BOLD}init skips unrelated untracked content${RESET}"

make_temp_repo init-lazy-untracked

if ! command -v mkfifo >/dev/null 2>&1; then
    skip_suite "mkfifo is unavailable"
fi

mkfifo unrelated.pipe

# The timeout kills only the process group created for this fixture, including
# the real CLI beneath any harness wrapper, so no blocked child is orphaned.
require_capture "atomic init completes without reading the FIFO" python3 -c '
import os,signal,subprocess,sys
p=subprocess.Popen([sys.argv[1],"init"],stdout=subprocess.PIPE,stderr=subprocess.STDOUT,start_new_session=True)
try:
    out,_=p.communicate(timeout=10)
except subprocess.TimeoutExpired:
    os.killpg(p.pid,signal.SIGKILL)
    out,_=p.communicate()
    sys.stdout.buffer.write(out)
    print("initialization was still running after 10 seconds",file=sys.stderr)
    sys.exit(1)
sys.stdout.buffer.write(out)
sys.exit(p.returncode)' "$ATOMIC_BIN"

assert_dir_exists "repository metadata was initialized" .atomic
assert_dir_exists "vault was initialized" .vault
assert_output_contains "repository initialization remains a separate change" \
    "Initialize repository" atomic log
assert_output_contains "vault initialization remains a separate change" \
    "Initialize vault" atomic log

print_summary
