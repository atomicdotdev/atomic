"""Black-box Phase 1 acceptance checks; failures retain command diagnostics."""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile


def check(binary):
    with tempfile.TemporaryDirectory(prefix="atomic-kg-closure-") as directory:
        base = Path(directory)
        root, home = base / "repo", base / "home"
        root.mkdir()
        home.mkdir()
        env = dict(os.environ, HOME=str(home), ATOMIC_NO_DAEMON="1")

        def run(*args, status=0, stdin=None, as_json=False):
            result = subprocess.run([binary, *args], cwd=root, env=env,
                                    input=stdin, text=True, capture_output=True)
            assert result.returncode == status, (
                f"{args}: expected {status}, got {result.returncode}\n"
                f"{result.stdout}\n{result.stderr}"
            )
            return json.loads(result.stdout) if as_json else result.stdout

        run("init", "--view", "main")
        identity = run("repo", "identity", "show", "--json", as_json=True)
        assert re.fullmatch(r"[A-Z2-7]{51}[AQ]", identity["project"])
        assert identity["minted"] == "created"
        assert identity["urn"] == "urn:atomic:project:" + identity["project"]
        empty = run("query", "validate", "--shape", "https://atomic.dev/ns/shapes/projection/1.0.0#GeneratedChange", "--format", "json", as_json=True)
        assert empty["conforms"] and all(c["evaluated"] == 0 for c in empty["target_coverage"])
        assert any("vacuous" in diagnostic for diagnostic in empty["diagnostics"])

        run("vault", "init")
        uid = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
        intent = root / ".vault" / "intents" / uid / "intent.md"
        intent.parent.mkdir(parents=True, exist_ok=True)
        intent.write_text(f"""---
uid: {uid}
id: TEST::alice::1
title: Workflow harness
kind: feature
status: in_progress
author: Alice
---
:::why
Verify the real workflow projection end to end.
:::
:::acceptance-criterion{{#{uid}-ac-1 status=unmet}}
The source is represented by a file resource.
:::
:::task{{#{uid}-1 status=unmet criteria={uid}-ac-1}}
Inspect the source.
::file-ref{{path=src/main.rs}}
:::
""")
        (root / "src").mkdir(exist_ok=True)
        (root / "src/main.rs").write_text("fn main() {}\n")
        run("add", "src/main.rs")
        run("record", "-m", "Workflow harness source")
        run("vault", "sync")
        run("query", "reindex", "--full")

        plan = {"schema_version": 1, "steps": [
            {"type": "kg_search", "query": "Workflow harness", "limit": 100, "bind": "all"},
            {"type": "filter", "input": "all", "field": "type",
             "equals": "https://atomic.dev/ns/workflow#WorkIntent", "bind": "intents"},
            {"type": "kg_neighbors", "node_id": "$intents", "direction": "outgoing",
             "predicate": "https://atomic.dev/ns/workflow#hasTask", "depth": 1},
        ]}
        result = run("query", "plan", "--json", stdin=json.dumps(plan), as_json=True)
        assert any(node["id"] == identity["urn"] + ":intent:" + uid for node in result["nodes"])
        assert any(edge["kind"] == "https://atomic.dev/ns/workflow#hasTask" for edge in result["edges"])
        assert all(node["types"] and "kind" not in node for node in result["nodes"])
        for invalid in [{"steps": []}, {"schema_version": 99, "steps": []},
                        {"schema_version": 1, "steps": [], "unexpected": True}]:
            failure = subprocess.run([binary, "query", "plan", "--json"], cwd=root, env=env,
                                     input=json.dumps(invalid), text=True, capture_output=True)
            assert failure.returncode != 0 and '"nodes"' not in failure.stdout

        projection = run("query", "validate", "--format", "json", as_json=True)
        assert projection["conforms"] and any(c["evaluated"] > 0 for c in projection["target_coverage"])
        publication = run("query", "validate", "--profile", "publication", "--format", "json", status=1, as_json=True)
        assert not publication["conforms"] and publication["results"]
        assert publication["report_ntriples"]
        for field in ["focus_node", "result_path", "source_shape", "source_constraint_component", "result_severity", "result_messages"]:
            assert field in publication["results"][0]
        run("query", "reindex", "--full")
        rebuilt = run("query", "validate", "--format", "json", as_json=True)
        assert rebuilt["snapshot_identity"] == projection["snapshot_identity"]
        run("query", "enrich", "--rebuild")
        assert run("query", "validate", "--format", "json", as_json=True)["snapshot_identity"] == projection["snapshot_identity"]
        assert run("repo", "identity", "show", "--json", as_json=True) == identity

        shapes = root / ".atomic/shapes"
        shapes.mkdir()
        custom = shapes / "enterprise.ttl"
        custom.write_text('''@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix atomic: <https://atomic.dev/ns/workflow#> .
<#Enterprise> a sh:NodeShape ; sh:targetClass atomic:ProjectIdentity ;
  sh:property [ sh:path atomic:status ; sh:minCount 1 ; sh:message "Enterprise status required"@en ] .
''')
        report = run("query", "validate", "--format", "json", status=1, as_json=True)
        assert not report["conforms"]
        selected = run("query", "validate", "--shape", custom.resolve().as_uri() + "#Enterprise", "--format", "json", status=1, as_json=True)
        assert selected["scope"] == "selected"
        assert any("Enterprise status required" in str(row) for row in selected["results"])
        custom.write_text("<> <http://www.w3.org/2002/07/owl#imports> <https://example.invalid/not-local.ttl> .")
        run("query", "validate", "--format", "json", status=3)
        custom.write_text("this is not Turtle")
        run("query", "validate", "--format", "json", status=3)
        custom.unlink()
        assert run("query", "validate", "--format", "json", as_json=True)["conforms"]
        print("KG closure CLI: identity, canonical plans, rebuild parity, profiles, custom shapes, and error exits passed.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    check(str(Path(parser.parse_args().binary).resolve()))
