#!/usr/bin/env python3
"""Optional local Atomic checker using Probity Verify's bounded adapters.

Install probity-verify in the Python environment selected by this executable.
This wrapper authenticates neither witnesses nor Atomic signatures. Its result
only applies to the selected claim, retained bytes, and supplied consumer policy.
"""
import json
import sys
from pathlib import Path

from probity_verify.common import CaseError
from probity_verify.core import adjudicate


def check(envelope):
    if not isinstance(envelope, dict) or set(envelope) != {"request", "request_digest"}:
        raise ValueError("invalid request envelope")
    request = envelope["request"]
    if not isinstance(request, dict) or set(request) != {"schema_version", "context", "claim"}:
        raise ValueError("invalid request")
    if request["schema_version"] != "atomic-evidence-replay-request/v1":
        raise ValueError("unsupported request schema")
    claim = request["claim"]
    if not isinstance(claim, dict) or set(claim) != {
        "criterion_id", "claim_type", "artifact_root", "case", "policy"
    }:
        raise ValueError("invalid claim")
    response = {
        "schema_version": "atomic-evidence-replay-response/v1",
        "request_digest": envelope["request_digest"],
        "criterion_id": claim["criterion_id"],
        "claim_type": claim["claim_type"],
        "decision": "not_established",
        "reason": "checker_error",
        "details": None,
    }
    try:
        case = claim["case"]
        policy = claim["policy"]
        assessment = policy["assessments"][case["case_id"]]
        if assessment["claim_type"] != claim["claim_type"]:
            raise CaseError("claim type differs from consumer policy")
        result = adjudicate(case, policy, Path(claim["artifact_root"]))
        response.update(decision=result["decision"], reason=result["reason"], details=result)
    except (CaseError, KeyError, TypeError, ValueError, OSError) as error:
        response["details"] = {"error": str(error)}
    return response


def main():
    try:
        # Atomic independently enforces the same 1 MiB output/manifest limit.
        raw = sys.stdin.buffer.read(1024 * 1024 + 1)
        if len(raw) > 1024 * 1024:
            raise ValueError("request exceeds 1 MiB")
        response = check(json.loads(raw))
        print(json.dumps(response, ensure_ascii=False, separators=(",", ":")))
        return 0
    except (ValueError, TypeError, KeyError) as error:
        print(str(error), file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
