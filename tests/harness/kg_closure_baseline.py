"""Capture/compare result sets, independent of KG IDs and CLI presentation."""

import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
from urllib.parse import unquote


def capture(binary, legacy):
    with tempfile.TemporaryDirectory(prefix="atomic-kg-baseline-") as directory:
        root = Path(directory)
        env = dict(os.environ, ATOMIC_NO_DAEMON="1")

        def run(*args, json_output=False):
            result = subprocess.run(
                [binary, *args], cwd=root, env=env, text=True, capture_output=True
            )
            if result.returncode and not (json_output and result.stdout.lstrip().startswith("{")):
                raise AssertionError(f"{args}: {result.returncode}\n{result.stdout}\n{result.stderr}")
            return json.loads(result.stdout) if json_output else result.stdout

        run("init", "--view", "main")
        run("view", "create", "feature", "--draft", "--parent", "main")
        run("view", "switch", "feature")
        files = {
            "src/foundation.rs": "pub fn foundation() -> u32 { 1 }\n",
            "src/service.rs": "use crate::foundation::foundation;\npub fn service() -> u32 { foundation() }\n",
            "src/main.rs": "mod foundation;\nmod service;\nfn main() { service::service(); }\n",
        }
        for path, source in files.items():
            (root / path).parent.mkdir(parents=True, exist_ok=True)
            (root / path).write_text(source)
        run("add", "src")
        run("record", "-m", "Add workflow baseline fixture")
        run("query", "enrich")
        callers = {}
        for path, name, line in [
            ("src/foundation.rs", "foundation", 1),
            ("src/service.rs", "service", 2),
        ]:
            target = f"entity:{path}:{name}:{line}" if legacy else f"{path}#{name}"
            rows = run("query", "callers", target, "--json", json_output=True)
            identities = []
            for row in rows:
                if "path" in row:
                    identities.append(f"{row['path']}#{row['name']}")
                else:
                    caller = row["caller_id"].removeprefix("entity:").rsplit(":", 2)
                    identities.append(f"{caller[0]}#{caller[1]}")
            callers[f"{path}#{name}"] = sorted(set(identities))
        report = run("triage", "review", "feature", "--into", "main", "--json", json_output=True)
        layers = [
            {"title": layer["title"], "files": sorted(
                path.removeprefix("file:") if legacy else unquote(path.split(":file:", 1)[1])
                for path in layer["files"])}
            for layer in report["walkthrough"]
        ]
        return {"callers": callers, "layers": layers}


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--legacy", action="store_true")
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    result = capture(args.binary, args.legacy)
    if args.check:
        expected = json.loads((Path(__file__).parent / "fixtures/kg-closure-baseline.json").read_text())
        assert result == expected, json.dumps({"expected": expected, "actual": result}, indent=2)
        print("Callers and triage result sets match the captured pre-cutover baseline.")
    else:
        print(json.dumps(result, indent=2, sort_keys=True))
