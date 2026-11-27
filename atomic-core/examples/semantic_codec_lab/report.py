"""Join completed codec lab runs; refuse partial or unmatched corpora."""
import json
import sys
import unittest


def integer(value):
    if type(value) is not int or value < 0:
        raise ValueError("expected nonnegative byte count")
    return value


def index(document):
    if document.get("failures"):
        raise ValueError("refusing a partial/failed benchmark")
    rows = document["rows"]
    keyed = {row["source_hash"]: row for row in rows}
    if not rows or len(keyed) != len(rows):
        raise ValueError("empty corpus or duplicate source hashes")
    return keyed


def summarize(semantic, graph, raw=None):
    if semantic.get("zstd_level") != 3 or graph.get("zstd_level") != 3:
        raise ValueError("expected equal zstd level 3")
    sr, gr = index(semantic), index(graph)
    if sr.keys() != gr.keys():
        raise ValueError("source hash sets differ")
    nonempty = [row["candidates"] for row in sr.values() if row["candidates"]]
    methods = {c["name"] for c in nonempty[0]} if nonempty else set()
    candidates = {}
    baseline = old_semantic = old_graph = selected_graph = content = 0
    graph_regressions = 0
    best_bytes = 0
    for key, row in sr.items():
        other = gr[key]
        size = integer(row["baseline_object_bytes"])
        if size != integer(other["baseline_object_bytes"]):
            raise ValueError("per-object baselines differ")
        sem = integer(row["baseline_semantic_bytes"])
        if sem != integer(other["semantic_bytes"]):
            raise ValueError("semantic baselines differ")
        graph_size = integer(other["graph_bytes"])
        graph_candidate = integer(other["candidate_graph_bytes"])
        picked_graph = min(graph_size, graph_candidate)
        if integer(other["selected_graph_bytes"]) != picked_graph:
            raise ValueError("invalid graph fallback projection")
        if size < sem + graph_size:
            raise ValueError("section totals exceed object")
        baseline += size
        old_semantic += sem
        old_graph += graph_size
        selected_graph += picked_graph
        graph_regressions += graph_candidate > graph_size
        content += sum(integer(n) for name, n in row["sections"] if name == "CONTENT")
        per_file = {c["name"]: c for c in row["candidates"]}
        if len(per_file) != len(row["candidates"]):
            raise ValueError("duplicate candidate name")
        if not per_file and sem == 0:
            per_file = {name: dict(semantic_bytes=0, projected_object_bytes=size,
                encode_ms=0., decode_ms=0.) for name in sorted(methods)}
        if methods != per_file.keys():
            raise ValueError("candidate sets differ")
        sizes = [sem]
        for name, candidate in per_file.items():
            n = integer(candidate["semantic_bytes"])
            if integer(candidate["projected_object_bytes"]) != size - sem + n:
                raise ValueError("invalid semantic projection")
            sizes.append(n)
            totals = candidates.setdefault(name, dict(semantic_bytes=0, selected_semantic_bytes=0,
                combined_object_bytes=0, regressions=0, encode_ms=0., decode_ms=0.))
            totals["semantic_bytes"] += n
            totals["selected_semantic_bytes"] += min(n, sem)
            totals["combined_object_bytes"] += size - graph_size + picked_graph - sem + min(n, sem)
            totals["regressions"] += n > sem
            totals["encode_ms"] += candidate["encode_ms"]
            totals["decode_ms"] += candidate["decode_ms"]
        sizes += [integer(other["semantic_field_bytes"]), integer(other["semantic_sequential_bytes"])]
        best_bytes += size - graph_size + picked_graph - sem + min(sizes)
    result = dict(objects=len(sr), baseline_object_bytes=baseline, baseline_semantic_bytes=old_semantic,
        baseline_graph_bytes=old_graph, selected_graph_bytes=selected_graph, graph_regressions=graph_regressions,
        content_payload_bytes=content, graph_only_object_bytes=baseline-old_graph+selected_graph,
        candidates=candidates, best_of_all_measured_object_bytes=best_bytes,
        note="Best-of-all is an evaluated-candidate projection, not one fixed/default production encoder.")
    if raw is not None:
        rr = index(raw)
        if rr.keys() != sr.keys() or raw.get("zstd_level") != 3:
            raise ValueError("raw comparison corpus/level differs")
        for key, row in rr.items():
            if integer(row["sizes"].get("v1_object_bytes",row["sizes"]["selected_bytes"])) != sr[key]["baseline_object_bytes"]:
                raise ValueError("raw comparison production baseline differs")
        result["uncompacted_semantic_object_bytes"] = sum(integer(r["sizes"]["raw_bytes"]) for r in rr.values())
    return result


class Tests(unittest.TestCase):
    def fixtures(self):
        sem = dict(zstd_level=3, failures=[], rows=[dict(source_hash="one", baseline_object_bytes=300,
            baseline_semantic_bytes=150, sections=[["CONTENT", 50]], candidates=[dict(name="test",
                semantic_bytes=90, projected_object_bytes=240, encode_ms=1., decode_ms=2.)])])
        graph = dict(zstd_level=3, failures=[], rows=[dict(source_hash="one", baseline_object_bytes=300,
            semantic_bytes=150, graph_bytes=100, candidate_graph_bytes=50, selected_graph_bytes=50,
            semantic_field_bytes=200, semantic_sequential_bytes=250)])
        return sem, graph

    def test_exact_projection(self):
        result = summarize(*self.fixtures())
        self.assertEqual(result["candidates"]["test"]["combined_object_bytes"], 190)
        self.assertEqual(result["best_of_all_measured_object_bytes"], 190)

    def test_reject_partial_mismatched_and_invalid_data(self):
        for case in range(4):
            sem, graph = self.fixtures()
            if case == 0:
                graph["failures"] = ["failed input"]
            elif case == 1:
                graph["rows"][0]["source_hash"] = "other"
            elif case == 2:
                graph["rows"][0]["baseline_object_bytes"] += 1
            else:
                sem["rows"][0]["candidates"][0]["projected_object_bytes"] += 1
            with self.assertRaises(ValueError):
                summarize(sem, graph)

    def test_graph_only_object_has_no_semantic_payload_to_transform(self):
        sem, graph = self.fixtures()
        sem["rows"].append(dict(source_hash="graph-only", baseline_object_bytes=100,
            baseline_semantic_bytes=0, sections=[], candidates=[]))
        graph["rows"].append(dict(source_hash="graph-only", baseline_object_bytes=100,
            semantic_bytes=0, graph_bytes=100, candidate_graph_bytes=50, selected_graph_bytes=50,
            semantic_field_bytes=0, semantic_sequential_bytes=0))
        self.assertEqual(summarize(sem, graph)["candidates"]["test"]["combined_object_bytes"], 240)


if __name__ == "__main__":
    if sys.argv[1:] == ["--self-test"]:
        unittest.main(argv=[sys.argv[0]])
    elif len(sys.argv) in (3, 4):
        documents = []
        for path in sys.argv[1:]:
            with open(path, encoding="utf-8") as source:
                documents.append(json.load(source))
        print(json.dumps(summarize(*documents), indent=2))
    else:
        raise SystemExit("usage: report.py semantic.json graph.json [raw-baseline.json] | --self-test")
