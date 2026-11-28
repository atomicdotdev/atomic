"""Functional regressions for the descriptor gate; needs grpcio-tools.

Compile isolated copies so negative cases never alter the canonical contract.
"""

from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parent
SERVICE = Path("atomic/services/sandbox/sandbox_service.proto")


class ContractCheckerTests(unittest.TestCase):
    def run_checker(self, transform=None):
        with tempfile.TemporaryDirectory(prefix="atomic-contract-test-") as temp:
            root = Path(temp)
            shutil.copytree(ROOT / "atomic", root / "atomic")
            shutil.copy2(ROOT / "check_contract.py", root / "check_contract.py")
            if transform is not None:
                service = root / SERVICE
                source = service.read_text()
                start = source.index("  rpc SubmitAttestation(")
                end = source.index("\n  }", start) + len("\n  }")
                method = source[start:end]
                service.write_text(source[:start] + transform(method) + source[end:])
            return subprocess.run(
                [sys.executable, str(root / "check_contract.py"), "--temp-dir", temp],
                capture_output=True, text=True, check=False,
            )

    def test_current_contract_passes(self):
        result = self.run_checker()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("routing and fencing: PASS", result.stdout)

    def test_submission_requires_its_exact_capability(self):
        # Another write capability passes generic mutation checks, but must not
        # authorize this method or mask the missing attestation.write requirement.
        result = self.run_checker(
            lambda method: method.replace('"attestation.write"', '"provenance.write"')
        )
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("requires exactly attestation.write", result.stderr)

    def test_submission_scope_callers_and_effects_are_pinned(self):
        mutations = [
            ("EXECUTION_SCOPE_BOTH", "EXECUTION_SCOPE_REMOTE", "requires BOTH scope"),
            (
                "    option (atomic.allowed_caller) = CALLER_CLASS_REMOTE_IDENTITY;\n",
                "",
                "caller allowlist differs",
            ),
            (
                "RPC_EFFECT_REPOSITORY_WRITE;",
                "RPC_EFFECT_REPOSITORY_WRITE;\n    option (atomic.effect) = RPC_EFFECT_EXTERNAL_IO;",
                "requires repository-write effect only",
            ),
        ]
        for old, new, expected in mutations:
            with self.subTest(annotation=old):
                result = self.run_checker(lambda method: method.replace(old, new))
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                self.assertIn(expected, result.stderr)

    def test_unreviewed_sandbox_method_still_fails(self):
        result = self.run_checker(
            lambda method: method + "\n" + method.replace(
                "rpc SubmitAttestation(", "rpc UnreviewedAttestation("
            )
        )
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("Sandbox method allowlist differs", result.stderr)


if __name__ == "__main__":
    unittest.main()
