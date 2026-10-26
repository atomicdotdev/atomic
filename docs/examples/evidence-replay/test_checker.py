"""Executable semantic controls for the optional Probity checker recipe."""
import copy
import hashlib
import importlib.util
import json
import re
import tempfile
import unittest
from importlib.metadata import version
from pathlib import Path

spec = importlib.util.spec_from_file_location("checker", Path(__file__).with_name("probity-checker.py"))
checker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(checker)


def pinned_verify_version():
    """The probity-verify version requirements.txt pins."""
    text = Path(__file__).with_name("requirements.txt").read_text(encoding="utf-8")
    pins = re.findall(r"^probity-verify==([^\s;\\]+)", text, flags=re.MULTILINE)
    if len(pins) != 1:
        raise AssertionError(f"requirements.txt pins probity-verify {len(pins)} times")
    return pins[0]


class ReplayCheckerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name)
        self.root = root
        source = b"The original report preserves this required passage.\n"
        record = source + b"Retained review copy.\n"
        (root / "source.txt").write_bytes(source)
        (root / "record.txt").write_bytes(record)
        sha = lambda data: hashlib.sha256(data).hexdigest()
        case = {"schema_version": "probity-case/v1", "case_id": "retained-report", "artifacts": {
            "source": {"path": "source.txt", "sha256": sha(source), "length": len(source)},
            "record": {"path": "record.txt", "sha256": sha(record), "length": len(record)},
        }}
        policy = {"schema_version": "probity-policy/v1", "witnesses": {"capture": {
            "artifact": "source", "sha256": sha(source), "captured_at": "2026-10-02T00:00:00Z",
            "source_url": "https://example.invalid/source", "authority": "local synthetic fixture",
            "source_format": "text_utf8/v1",
        }}, "assessments": {"retained-report": {
            "claim_type": "source_text_coverage/v1", "source_witness": "capture",
            "record_artifact": "record", "record_sha256": sha(record), "record_version": "fixture-1",
            "source_window": {"start": "2026-10-01T00:00:00Z", "end": "2026-10-03T00:00:00Z"},
            "required_spans": [{"id": "passage-1", "text": "preserves this required passage"}],
        }}}
        self.request = {"request_digest": "blake3:protocol-bound-by-atomic", "request": {
            "schema_version": "atomic-evidence-replay-request/v1",
            "context": {"intent_id": "urn:atomic:intent:fixture", "intent_substance_hash": "native-pin",
                        "view_chain": [{"name": "dev", "merkle": "native-merkle"}]},
            "claim": {"criterion_id": "urn:atomic:ac:fixture-ac-1", "claim_type": "source_text_coverage/v1",
                      "artifact_root": str(root), "case": case, "policy": policy},
        }}

    def test_installed_verify_is_the_pinned_release(self):
        self.assertEqual(version("probity-verify"), pinned_verify_version())

    def test_every_response_names_the_checker_release(self):
        pinned = {"package": "probity-verify", "version": pinned_verify_version()}
        self.assertEqual(checker.check(self.request)["details"]["checker"], pinned)
        (self.root / "record.txt").unlink()
        self.assertEqual(checker.check(self.request)["details"]["checker"], pinned)

    def test_supported_preserves_binding_and_bounded_scope(self):
        result = checker.check(self.request)
        self.assertEqual(result["decision"], "supported")
        self.assertEqual(result["request_digest"], self.request["request_digest"])
        self.assertEqual(result["criterion_id"], self.request["request"]["claim"]["criterion_id"])
        self.assertIn("Only selected literal passages", result["details"]["scope"]["limit"])
        self.assertIn("policy_sha256", result["details"])

    def test_changed_bytes_are_unknown_and_repinning_missing_passage_is_contradicted(self):
        data = b"The report now omits the requested words.\n"
        (self.root / "record.txt").write_bytes(data)
        self.assertEqual(checker.check(self.request)["decision"], "not_established")
        claim = self.request["request"]["claim"]
        sha = hashlib.sha256(data).hexdigest()
        claim["case"]["artifacts"]["record"]["sha256"] = sha
        claim["case"]["artifacts"]["record"]["length"] = len(data)
        claim["policy"]["assessments"]["retained-report"]["record_sha256"] = sha
        self.assertEqual(checker.check(self.request)["decision"], "contradicted")

    def test_missing_artifact_and_unsupported_claim_cannot_support(self):
        (self.root / "record.txt").unlink()
        self.assertEqual(checker.check(self.request)["decision"], "not_established")
        self.request["request"]["claim"]["claim_type"] = "arbitrary-task-truth/v1"
        self.assertEqual(checker.check(self.request)["decision"], "not_established")

    def test_changed_policy_claim_is_unknown(self):
        request = copy.deepcopy(self.request)
        request["request"]["claim"]["policy"]["assessments"]["retained-report"]["claim_type"] = "event_absence/v1"
        self.assertEqual(checker.check(request)["decision"], "not_established")

    def test_request_cannot_choose_an_executable(self):
        self.request["request"]["executable"] = "untrusted"
        with self.assertRaises(ValueError):
            checker.check(self.request)


if __name__ == "__main__":
    unittest.main()
