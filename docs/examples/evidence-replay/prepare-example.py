#!/usr/bin/env python3
"""Create synthetic source/record fixtures and a manifest for one native AC.

Run inside an Atomic repository, after `atomic intent new`. This scaffolds a
bounded source-coverage claim only; it does not attest the intent or mark it met.
"""
import argparse
import hashlib
import json
import subprocess
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("intent_id")
    parser.add_argument("output_directory", type=Path)
    parser.add_argument("--atomic", default="atomic")
    args = parser.parse_args()
    report = subprocess.run(
        [args.atomic, "intent", "validate", args.intent_id, "--json", "--evidence-context"],
        check=False, capture_output=True, text=True,
    )
    # Exit 2 is normal for an unattested draft. Other gate violations stay in
    # the report and will still fail validation when the example is replayed.
    if report.returncode not in {0, 2}:
        raise SystemExit(report.stderr)
    context = json.loads(report.stdout)["evidence_context"]
    shown = subprocess.run([args.atomic, "intent", "show", args.intent_id, "--json"],
                           check=True, capture_output=True, text=True)
    criteria = json.loads(shown.stdout)["hasAcceptanceCriterion"]
    if len(criteria) != 1:
        raise SystemExit("example requires exactly one criterion; author a policy for each real criterion")
    source = b"The selected literal passage is retained.\n"
    record = source + b"Synthetic review copy.\n"
    root = args.output_directory
    root.mkdir(parents=True, exist_ok=False)
    (root / "source.txt").write_bytes(source)
    (root / "record.txt").write_bytes(record)
    digest = lambda data: hashlib.sha256(data).hexdigest()
    case = {"schema_version": "probity-case/v1", "case_id": "synthetic-source-coverage", "artifacts": {
        "source": {"path": "source.txt", "sha256": digest(source), "length": len(source)},
        "record": {"path": "record.txt", "sha256": digest(record), "length": len(record)},
    }}
    policy = {"schema_version": "probity-policy/v1", "witnesses": {"source": {
        "artifact": "source", "sha256": digest(source), "captured_at": "2026-10-02T00:00:00Z",
        "source_url": "https://example.invalid/synthetic-source", "authority": "synthetic local fixture",
        "source_format": "text_utf8/v1",
    }}, "assessments": {case["case_id"]: {
        "claim_type": "source_text_coverage/v1", "source_witness": "source", "record_artifact": "record",
        "record_sha256": digest(record), "record_version": "synthetic-fixture-1",
        "source_window": {"start": "2026-10-01T00:00:00Z", "end": "2026-10-03T00:00:00Z"},
        "required_spans": [{"id": "passage-1", "text": "selected literal passage is retained"}],
    }}}
    manifest = {"schema_version": "atomic-evidence-replay-manifest/v1", "context": context, "claims": [{
        "criterion_id": criteria[0]["@id"], "claim_type": "source_text_coverage/v1",
        "artifact_root": ".", "case": case, "policy": policy,
    }]}
    (root / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    print(root / "manifest.json")


if __name__ == "__main__":
    main()
