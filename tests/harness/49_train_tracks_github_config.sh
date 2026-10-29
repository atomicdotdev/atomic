#!/usr/bin/env bash
# Offline safety tests: no real GitHub authentication, forks, pushes or PRs.
set -euo pipefail
if ! command -v python3 >/dev/null; then
    echo 'SKIPPING: python3 is required for personal-fork configuration tests'
    exit 77
fi
HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"
python3 "$HARNESS_DIR/train_tracks_github_test.py" -v
