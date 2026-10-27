"""Offline regression coverage for personal GitHub fork selection and guards."""

import json
import os
from pathlib import Path
import pty
import subprocess
import sys
import tempfile
import unittest


HARNESS = Path(__file__).resolve().parent
HELPER = HARNESS / "train_tracks_github.sh"
MOCK_GH = r'''
import json, os
from pathlib import Path
import sys
args = sys.argv[1:]
with open(os.environ['FAKE_CALLS'], 'a') as f:
    f.write(json.dumps(args) + '\n')
login = os.environ['FAKE_LOGIN']
if args == ['api', 'user', '--jq', '.login']:
    if os.environ.get('FAKE_AUTH_FAIL') == '1':
        sys.exit('authentication failed')
    print(login)
    sys.exit(0)
fork = {
    'full_name': os.environ.get('FAKE_FULL_NAME', login + '/train-tracks'),
    'fork': os.environ.get('FAKE_IS_FORK', '1') == '1',
    'owner': {'type': os.environ.get('FAKE_OWNER_TYPE', 'User')},
    'parent': {'full_name': os.environ.get('FAKE_PARENT', 'atomicdotdev/train-tracks')},
    'permissions': {'push': os.environ.get('FAKE_PUSH', '1') == '1'},
}
if args == ['api', 'repos/' + login + '/train-tracks']:
    state = os.environ.get('FAKE_STATE', 'existing')
    if state == 'forbidden':
        sys.exit('gh: Forbidden (HTTP 403)')
    if state == 'missing' and not Path(os.environ['FAKE_CREATED']).exists():
        sys.exit('gh: Not Found (HTTP 404)')
    print(json.dumps(fork))
elif args == ['api', '--method', 'POST', 'repos/atomicdotdev/train-tracks/forks',
              '-f', 'name=train-tracks', '-F', 'default_branch_only=false']:
    Path(os.environ['FAKE_CREATED']).touch()
    print(json.dumps(fork))
else:
    sys.exit('Unexpected GitHub invocation: ' + repr(args))
'''


class PersonalForkTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="atomic-github-config-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        bin_dir = self.root / "bin"
        bin_dir.mkdir()
        gh = bin_dir / "gh"
        gh.write_text(f"#!{sys.executable}\n" + MOCK_GH)
        gh.chmod(0o700)
        self.calls = self.root / "calls.jsonl"
        self.env = os.environ.copy()
        for name in (
            "TRAIN_TRACKS_GITHUB_OWNER",
            "RUN_TRAIN_TRACKS_SHIM",
            "RUN_TRAIN_TRACKS_LIFECYCLE",
        ):
            self.env.pop(name, None)
        self.env.update(
            PATH=str(bin_dir) + os.pathsep + os.environ["PATH"],
            HELPER=str(HELPER),
            ARTIFACTS=str(self.root),
            FAKE_LOGIN="alice",
            FAKE_CALLS=str(self.calls),
            FAKE_CREATED=str(self.root / "created"),
        )

    def shell(self, code, owner=None, terminal_input=None, **env):
        settings = self.env.copy()
        settings.update(env)
        if owner is not None:
            settings["TRAIN_TRACKS_GITHUB_OWNER"] = owner
        command = ["bash", "-eu", "-c", 'source "$HELPER"\n' + code]
        if terminal_input is None:
            return subprocess.run(
                command, input="", capture_output=True, text=True, env=settings, timeout=10
            )
        master, slave = pty.openpty()
        try:
            with subprocess.Popen(
                command, stdin=slave, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                text=True, env=settings,
            ) as child:
                os.close(slave)
                slave = None
                os.write(master, terminal_input.encode())
                stdout, stderr = child.communicate(timeout=10)
                return subprocess.CompletedProcess(command, child.returncode, stdout, stderr)
        finally:
            os.close(master)
            if slave is not None:
                os.close(slave)

    def read_calls(self):
        return [json.loads(line) for line in self.calls.read_text().splitlines()] if self.calls.exists() else []

    def assert_ok(self, result):
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_sourcing_has_no_network_side_effects(self):
        self.assert_ok(self.shell(":"))
        self.assertEqual(self.read_calls(), [])

    def test_multiple_personal_accounts_route_to_their_own_forks(self):
        for login in ("alice", "bob-42"):
            with self.subTest(login=login):
                result = self.shell(
                    'train_tracks_select_fork; run() { "$@"; }; '
                    'train_tracks_ensure_fork "$ARTIFACTS"; '
                    'printf "%s\\n%s\\n" "$FORK" "$FORK_URL"',
                    owner=login, FAKE_LOGIN=login,
                )
                self.assert_ok(result)
                self.assertEqual(result.stdout.splitlines(), [
                    login + "/train-tracks", "https://github.com/" + login + "/train-tracks.git"
                ])
                self.assertEqual(self.read_calls()[-1], ["api", "repos/" + login + "/train-tracks"])

    def test_profile_url_and_case_are_normalized_to_authenticated_login(self):
        result = self.shell('train_tracks_select_fork; echo "$FORK"', owner="https://github.com/ALICE/")
        self.assert_ok(result)
        self.assertEqual(result.stdout.strip(), "alice/train-tracks")

    def test_terminal_prompts_for_profile_url(self):
        result = self.shell(
            'train_tracks_select_fork; echo "$FORK"',
            terminal_input="https://github.com/alice/\n",
        )
        self.assert_ok(result)
        self.assertIn("Personal GitHub username or profile URL [alice]", result.stderr)
        self.assertEqual(result.stdout.strip(), "alice/train-tracks")

    def test_terminal_default_requires_enter(self):
        result = self.shell('train_tracks_select_fork; echo "$FORK"', terminal_input="\n")
        self.assert_ok(result)
        self.assertIn("[alice]", result.stderr)
        self.assertEqual(result.stdout.strip(), "alice/train-tracks")

    def test_noninteractive_missing_owner_refuses_without_fork_access(self):
        result = self.shell('train_tracks_select_fork; exit 99')
        self.assertEqual(result.returncode, 1)
        self.assertIn("set TRAIN_TRACKS_GITHUB_OWNER", result.stderr)
        self.assertEqual(self.read_calls(), [["api", "user", "--jq", ".login"]])

    def test_authentication_failure_stops_selection(self):
        result = self.shell('train_tracks_select_fork', owner="alice", FAKE_AUTH_FAIL="1")
        self.assertEqual(result.returncode, 1)
        self.assertIn("gh auth login", result.stderr)

    def test_mismatched_personal_account_is_rejected_before_fork_access(self):
        result = self.shell('train_tracks_select_fork', owner="bob")
        self.assertEqual(result.returncode, 1)
        self.assertIn("does not match authenticated", result.stderr)
        self.assertEqual(self.read_calls(), [["api", "user", "--jq", ".login"]])

    def test_upstream_namespace_is_never_a_target(self):
        result = self.shell('train_tracks_select_fork', owner="ATOMICDOTDEV", FAKE_LOGIN="atomicdotdev")
        self.assertEqual(result.returncode, 1)
        self.assertIn("Refusing the upstream namespace", result.stderr)

    def test_malformed_owner_or_repository_url_is_rejected(self):
        for owner in ("alice/train-tracks", "https://github.com/alice/train-tracks", "../alice",
                      "https://example.com/alice", "-alice", "alice-", "a--b", "a" * 40):
            with self.subTest(owner=owner):
                result = self.shell('train_tracks_select_fork', owner=owner)
                self.assertEqual(result.returncode, 1)

    def ensure(self, **env):
        return self.shell(
            'train_tracks_select_fork; run() { "$@"; }; train_tracks_ensure_fork "$ARTIFACTS"',
            owner="alice", **env,
        )

    def test_existing_personal_fork_is_read_only_during_setup(self):
        self.assert_ok(self.ensure())
        self.assertEqual(self.read_calls()[-1], ["api", "repos/alice/train-tracks"])
        self.assertFalse((self.root / "created").exists())

    def test_missing_fork_is_created_only_under_authenticated_personal_account(self):
        self.assert_ok(self.ensure(FAKE_STATE="missing"))
        self.assertTrue((self.root / "created").exists())
        self.assertEqual(self.read_calls(), [
            ["api", "user", "--jq", ".login"],
            ["api", "repos/alice/train-tracks"],
            ["api", "--method", "POST", "repos/atomicdotdev/train-tracks/forks",
             "-f", "name=train-tracks", "-F", "default_branch_only=false"],
            ["api", "repos/alice/train-tracks"],
        ])

    def test_permission_error_does_not_trigger_creation(self):
        result = self.ensure(FAKE_STATE="forbidden")
        self.assertEqual(result.returncode, 1)
        self.assertIn("HTTP 403", result.stderr)
        self.assertFalse((self.root / "created").exists())

    def test_unrelated_or_unwritable_repository_is_rejected(self):
        for env in (
            {"FAKE_FULL_NAME": "bob/train-tracks"},
            {"FAKE_PARENT": "other/project"},
            {"FAKE_IS_FORK": "0"},
            {"FAKE_OWNER_TYPE": "Organization"},
            {"FAKE_PUSH": "0"},
        ):
            with self.subTest(env=env):
                self.assertEqual(self.ensure(**env).returncode, 1)
                self.assertFalse((self.root / "created").exists())

    def test_pr_head_and_base_must_both_belong_to_selected_fork(self):
        pr = self.root / "pr.json"
        for base, head, success in (
            ("alice/train-tracks", "alice/train-tracks", True),
            ("atomicdotdev/train-tracks", "alice/train-tracks", False),
            ("alice/train-tracks", "bob/train-tracks", False),
        ):
            with self.subTest(base=base, head=head):
                pr.write_text(json.dumps({
                    "number": 123, "base": {"repo": {"full_name": base}},
                    "head": {"repo": {"full_name": head}},
                }))
                result = self.shell(
                    'train_tracks_select_fork; train_tracks_validate_pr "$ARTIFACTS/pr.json"', owner="alice"
                )
                self.assertEqual(result.returncode == 0, success, result.stderr)
                if success:
                    self.assertEqual(result.stdout.strip(), "123")

    def test_opt_out_never_prompts_or_calls_github(self):
        for script in ("47_git_shim_train_tracks.sh", "48_git_shim_github_lifecycle.sh"):
            with self.subTest(script=script):
                result = subprocess.run(
                    ["bash", str(HARNESS / script)], input="", capture_output=True,
                    text=True, env=self.env, timeout=10,
                )
                self.assertEqual(result.returncode, 77, result.stderr)
                self.assertEqual(self.read_calls(), [])

    def test_opted_in_suites_refuse_wrong_account_before_build_or_mutation(self):
        # Prerequisite discovery can find these tools, but executing either
        # would prove account validation happened too late.
        for tool in ("cargo", "git"):
            stub = self.root / "bin" / tool
            stub.write_text("#!/bin/sh\necho 'unexpected build or Git operation' >&2\nexit 93\n")
            stub.chmod(0o700)
        for script, consent in (
            ("47_git_shim_train_tracks.sh", "RUN_TRAIN_TRACKS_SHIM"),
            ("48_git_shim_github_lifecycle.sh", "RUN_TRAIN_TRACKS_LIFECYCLE"),
        ):
            with self.subTest(script=script):
                env = self.env.copy()
                env.update({consent: "1", "TRAIN_TRACKS_GITHUB_OWNER": "bob"})
                result = subprocess.run(
                    ["bash", str(HARNESS / script)], input="", capture_output=True,
                    text=True, env=env, timeout=10,
                )
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertIn("does not match authenticated", result.stderr)
                self.assertNotIn("unexpected build or Git operation", result.stderr)
        self.assertTrue(all(call == ["api", "user", "--jq", ".login"] for call in self.read_calls()))


if __name__ == "__main__":
    unittest.main()
