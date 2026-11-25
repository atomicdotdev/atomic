#!/usr/bin/env python3
"""Select host-owned native bytes and invoke an installed offline reader.

This host-side selection is operator-local. It authenticates neither an outside
runner nor an external effect, and never executes a retained packet binary.
"""

import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[2]
POLICY = Path(__file__).with_name("consumer-policy.json")
MAX_JSON = 8 * 1024 * 1024
MAX_ARTIFACT = 256 * 1024 * 1024
NATIVE_SOURCES = (
    "Cargo.lock", "atomic-canonical/tests/delegation_restart.rs",
    "tools/native-evaluation/run.py", "tools/native-evaluation/README.md",
    ".github/workflows/ci.yml",
)


def sha(path):
    require(path.is_file() and path.stat().st_size <= MAX_ARTIFACT, "artifact size or missing file")
    value = hashlib.sha256()
    total = 0
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            total += len(block)
            require(total <= MAX_ARTIFACT, "artifact exceeds consumer budget")
            value.update(block)
    return value.hexdigest()


def require(condition, reason):
    if not condition:
        raise ValueError(reason)


def bounded_bytes(path, limit):
    require(path.is_file() and path.absolute() == path.resolve(), "missing or noncanonical source path")
    require(path.stat().st_size <= limit, "input exceeds consumer budget")
    with path.open("rb") as stream:
        content = stream.read(limit + 1)
    require(len(content) <= limit, "input exceeds consumer budget")
    return content


def json_file(path):
    content = bounded_bytes(path, MAX_JSON)
    require(bool(content), "empty JSON input")
    return json.loads(content)


def run(argv, output, name, cwd):
    result = subprocess.run(argv, cwd=cwd, capture_output=True, timeout=120)
    (output / (name + ".stdout")).write_bytes(result.stdout)
    (output / (name + ".stderr")).write_bytes(result.stderr)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--packet", type=Path, required=True)
    parser.add_argument("--expected-source", required=True)
    parser.add_argument("--observer-source-commit", required=True)
    parser.add_argument("--reader", type=Path, required=True)
    parser.add_argument("--reader-python", type=Path, required=True)
    parser.add_argument("--wheel", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    output, packet = args.output.resolve(), args.packet.absolute()
    require(packet == packet.resolve(), "packet path must be canonical")
    require(not output.is_relative_to(packet), "consumer output must be outside packet")
    policy = json_file(POLICY)
    require(args.observer_source_commit == policy["observer_source_commit"], "observer source pin mismatch")
    actual_head = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT).decode().strip()
    require(args.expected_source == actual_head, "expected source differs from host checkout")
    report = json_file(packet / "report.json")
    require(report["source"]["commit"] == actual_head and report["source"]["status"] == "",
            "native source is not the clean host checkout")
    require(report["schema"] == policy["expected_native_schema"], "native schema mismatch")
    for name in NATIVE_SOURCES:
        expected = bounded_bytes(ROOT / name, MAX_JSON)
        require(bounded_bytes(packet / "source" / name, len(expected)) == expected,
                "native source snapshot differs from host checkout: " + name)
    # An isolated interpreter must resolve the reader from its installed wheel,
    # with no repository checkout or agent framework on its import path.
    inspect = run([str(args.reader_python.absolute()), "-I", "-c",
                   "import hashlib, importlib.util, json, pathlib, sys; "
                   "import probity_observer.atomic_reader as reader; "
                   "p=pathlib.Path(reader.__file__); "
                   "print(json.dumps({'module':str(p),'sha256':hashlib.sha256(p.read_bytes()).hexdigest(),"
                   "'prefix':sys.prefix,'frameworks':{n:importlib.util.find_spec(n) is not None "
                   "for n in ('pytest','inspect_ai','a2a','langgraph','pydantic_ai')}}))"],
                  output, "installed-reader", output)
    require(inspect.returncode == 0, "installed reader inspection failed")
    installed = json.loads(inspect.stdout)
    require(installed["sha256"] == policy["reader_sha256"], "installed reader byte pin mismatch")
    require(Path(installed["module"]).is_relative_to(Path(installed["prefix"])),
            "reader did not resolve from installed environment")
    require(args.reader.resolve().parent.parent == Path(installed["prefix"]),
            "reader command and interpreter environments differ")
    require(not any(installed["frameworks"].values()), "consumer environment contains unrelated frameworks")
    described = run([str(args.reader.resolve()), "describe", "--packet", str(packet)],
                    output, "candidate-selection", output)
    require(described.returncode == 0, "reader candidate description failed")
    require(0 < len(described.stdout) <= MAX_JSON, "candidate selection exceeds consumer budget")
    selected = json.loads(described.stdout)
    require(selected["source_commit"] == args.expected_source
            and selected["reader_sha256"] == policy["reader_sha256"], "candidate source/reader mismatch")
    # The candidate is not accepted on its own assertion: the source commit,
    # source files and installed reader were checked against host inputs above.
    selection = output / "selected-inputs.json"
    selection.write_bytes(described.stdout)
    selection_digest = sha(selection)
    outcomes = {}

    def check(name, target=packet, selection_path=selection, pin=selection_digest, expected_reason=None):
        result = run([str(args.reader.resolve()), "verify", "--packet", str(target),
                      "--selection", str(selection_path), "--selection-sha256", pin,
                      "--output", str(output / (name + ".json"))], output, name, output)
        outcomes[name] = result.returncode
        receipt = json_file(output / (name + ".json"))
        if expected_reason is None:
            require(result.returncode == 0 and receipt.get("accepted") is True,
                    "positive consumer verification failed")
        else:
            require(result.returncode != 0 and receipt.get("accepted") is False
                    and receipt.get("reason") == expected_reason,
                    "consumer refusal did not establish the intended boundary: " + name)

    check("selected-native-run")
    changed = output / "changed-raw-packet"
    shutil.copytree(packet, changed)
    with (changed / "case-0.stdout").open("ab") as stream:
        stream.write(b"changed raw native bytes\n")
    check("changed-raw-bytes", changed, expected_reason="artifact_pin_mismatch:case-0.stdout")
    shutil.rmtree(changed)
    widened = json_file(selection)
    widened["artifacts"]["unselected-extra-source"] = {"bytes": 0, "sha256": hashlib.sha256(b"").hexdigest()}
    widened_path = output / "widened-inputs.json"
    widened_path.write_text(json.dumps(widened, indent=2) + "\n")
    # Refuse widening even when the caller re-pins the altered selection.
    check("widened-selection", selection_path=widened_path, pin=sha(widened_path), expected_reason="artifact_population")
    check("wrong-selection-digest", pin="0" * 64, expected_reason="selection_pin_mismatch")
    require(outcomes["selected-native-run"] == 0
            and all(value != 0 for key, value in outcomes.items() if key != "selected-native-run"),
            "consumer positive/refusal control failed")
    shutil.copyfile(POLICY, output / "consumer-policy.json")
    receipt = {
        "schema": "atomic-installed-native-consumer-run/v1", "passed": True,
        "host_source_commit": actual_head, "observer_source_commit": args.observer_source_commit,
        "reader_sha256": installed["sha256"], "wheel_sha256": sha(args.wheel),
        "native_report_sha256": sha(packet / "report.json"),
        "cargo_lock_sha256": sha(ROOT / "Cargo.lock"),
        "policy_sha256": sha(POLICY), "selection_sha256": selection_digest,
        "installed_reader": installed, "returncodes": outcomes,
        "custody": policy["allowed_custody"], "native_rerun_by_reader": False,
        "maintainer_acceptance": "not established", "outside_recurring_use": "not established",
        "independent_effect_custody": "not established",
    }
    (output / "consumer-run.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(json.dumps(receipt))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (ValueError, OSError, KeyError, subprocess.SubprocessError) as error:
        print(json.dumps({"passed": False, "reason": str(error)}), file=sys.stderr)
        raise SystemExit(1)
