#!/usr/bin/env python3
"""Retain native delegation tests, exact observations and source/binary pins."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import time


CASES = (
    "exact_retry_and_process_restart_preserve_a_narrow_grant",
    "revocation_survives_restart_and_does_not_revoke_a_distinct_renewal",
    "expired_and_corrupt_grants_stay_inactive_after_restart",
    "local_selection_does_not_establish_external_issuer_authority",
)
ROOT = Path(__file__).resolve().parents[2]


def digest(path):
    value = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(block)
    return value.hexdigest()


def capture(argv, directory, name, timeout):
    started = time.monotonic()
    timed_out = False
    with (directory / f"{name}.stdout").open("wb") as stdout, (
        directory / f"{name}.stderr"
    ).open("wb") as stderr:
        process = subprocess.Popen(argv, cwd=ROOT, stdout=stdout, stderr=stderr)
        try:
            returncode = process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            process.kill()
            returncode = process.wait()
            timed_out = True
    return {
        "argv": argv,
        "returncode": returncode,
        "timed_out": timed_out,
        "wall_seconds": time.monotonic() - started,
        "stdout": f"{name}.stdout",
        "stderr": f"{name}.stderr",
    }


def git(*args):
    return subprocess.check_output(["git", *args], cwd=ROOT).decode().strip()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True, help="new result directory")
    parser.add_argument("--cargo", default="cargo")
    parser.add_argument("--jobs", type=int, default=2)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    output = args.output.resolve()
    source = {
        "commit": git("rev-parse", "HEAD"),
        "status": git("status", "--porcelain"),
        "platform": platform.platform(),
        "python": sys.version,
        "cargo": subprocess.check_output([args.cargo, "--version"]).decode().strip(),
        "rustc": subprocess.check_output(["rustc", "--version"]).decode().strip(),
        "environment": {
            name: os.environ.get(name)
            for name in ("RUSTFLAGS", "CARGO_TARGET_DIR", "CARGO_PROFILE_TEST_DEBUG", "CARGO_INCREMENTAL")
        },
    }
    (output / "source.diff").write_bytes(subprocess.check_output(["git", "diff", "HEAD"], cwd=ROOT))
    for relative in (
        "Cargo.lock", "atomic-canonical/tests/delegation_restart.rs",
        "tools/native-evaluation/run.py", "tools/native-evaluation/README.md",
        ".github/workflows/ci.yml",
    ):
        destination = output / "source" / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(ROOT / relative, destination)
    build = capture([
        args.cargo, "test", "--locked", "-p", "atomic-canonical", "--test",
        "delegation_restart", "--no-run", "--message-format=json", "-j", str(args.jobs),
    ], output, "build", 600)
    binaries = []
    for line in (output / build["stdout"]).read_text().splitlines():
        try:
            item = json.loads(line)
        except json.JSONDecodeError:
            continue
        if (item.get("reason") == "compiler-artifact"
                and item.get("target", {}).get("name") == "delegation_restart"
                and item.get("executable")):
            binaries.append(Path(item["executable"]))
    results = []
    binary_pin = None
    if build["returncode"] == 0 and not build["timed_out"] and len(binaries) == 1:
        binary = binaries[0]
        binary_pin = {"sha256": digest(binary), "filename": binary.name}
        shutil.copy2(binary, output / binary.name)
        listed = subprocess.check_output([str(binary), "--list", "--format", "terse"]).decode()
        (output / "population.txt").write_text(listed)
        for number, case in enumerate(CASES):
            if f"{case}: test" not in listed.splitlines():
                raise RuntimeError(f"declared case is missing from binary: {case}")
            result = capture([str(binary), "--exact", case, "--nocapture"], output, f"case-{number}", 120)
            result["case"] = case
            result["observations"] = [
                json.loads(line.split("ATOMIC_NATIVE_LOOKUP=", 1)[1])
                for line in (output / result["stdout"]).read_text().splitlines()
                if "ATOMIC_NATIVE_LOOKUP=" in line
            ]
            result["passed"] = (result["returncode"] == 0 and not result["timed_out"]
                                and bool(result["observations"]))
            results.append(result)
    success = len(results) == len(CASES) and all(result["passed"] for result in results)
    artifacts = {
        str(path.relative_to(output)): {"sha256": digest(path), "bytes": path.stat().st_size}
        for path in sorted(output.rglob("*")) if path.is_file()
    }
    report = {
        "schema": "atomic-native-delegation-evaluation/v1",
        "source": source, "build": build, "binary": binary_pin,
        "declared_cases": list(CASES), "results": results, "passed": success,
        "artifacts": artifacts,
        "coverage": {
            "boundary": "local persisted certificate selection in distinct OS processes",
            "keys": "ephemeral keys generated by the same operator; private keys are not retained",
            "clock": "local OS clock, with expiry controls one day away from the test time",
            "filesystem": "operator-owned temporary identity store",
            "server_authorization": "not established",
            "remote_revocation": "not established",
            "external_effects": "not observed",
            "crash_mid_write": "not evaluated",
            "model_execution": "not used",
        },
    }
    (output / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({"report": str(output / "report.json"), "passed": success, "cases": len(results)}))
    return 0 if success else 1


if __name__ == "__main__":
    raise SystemExit(main())
