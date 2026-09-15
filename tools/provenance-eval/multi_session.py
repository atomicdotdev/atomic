#!/usr/bin/env python3
"""Deterministic multi-process agent-session replay against real Atomic CLI.

No models are invoked. Each worker has its own session; all share one disposable
repository and owner. Timers exclude setup and post-run persistence verification.
"""
import argparse
import base64
import concurrent.futures
import contextlib
import hashlib
import json
import multiprocessing
import os
import pathlib
import platform
import shutil
import statistics
import subprocess
import tempfile
import time

import run as base


class SeededFixture(base.Fixture):
    def run(self, *args, **kwargs):
        result = super().run(*args, **kwargs)
        if args[0] == "init":
            for number in range(32):
                super().run("intent", "new", f"Review benchmark module {number}")
            super().run("agent", "lifecycle", "begin", "--owner", "benchmark-orchestrator",
                        "--session", "benchmark-coordinator", "--executor", "claude-code", "--view", "dev")
        return result


class PreparedFixture(base.Fixture):
    def __init__(self, binary, root, template):
        super().__init__(binary, root)
        self.template = template

    def run(self, *args, **kwargs):
        if args[0] == "init":
            shutil.copytree(self.template, self.root, dirs_exist_ok=True)
            return super().run("agent", "lifecycle", "begin", "--owner", "benchmark-orchestrator",
                               "--session", "benchmark-coordinator", "--executor", "claude-code", "--view", "dev")
        return super().run(*args, **kwargs)


def worker(binary, root, endpoint, number, config, gate, stop_lock):
    session = f"multi-agent-{number}"
    base.SESSION = session
    f = base.Fixture(binary, pathlib.Path(root))
    f.endpoint = endpoint
    reads, writes, stops, turn_times, failures = [], [], [], [], []
    gate.wait(timeout=60)
    started = time.perf_counter()
    try:
        for turn in range(1, config["turns"] + 1):
            turn_start = time.perf_counter()
            f.hook("user-prompt-submit", {"session_id": session, "prompt": f"Review intent and source, turn {turn}"})
            for event in range(config["events"]):
                for read in range(config["reads_per_event"]):
                    command = ["intent", "list", "--json"] if read % 2 == 0 else ["intent", "show", config["intent_id"]]
                    ts = time.perf_counter()
                    try:
                        result = f.run(*command)
                        text = result.stdout.decode()
                        if read % 2 == 0:
                            assert {x["id"] for x in json.loads(text)} == set(config["intent_ids"]), "intent list changed"
                        else:
                            assert config["intent_id"] in text, "intent content missing"
                        reads.append({"ms": (time.perf_counter() - ts) * 1000, "ok": True, "command": command})
                    except Exception as error:
                        reads.append({"ms": (time.perf_counter() - ts) * 1000, "ok": False, "command": command, "error": str(error)})
                ts = time.perf_counter()
                f.hook("post-tool", base.tool_payload(event, 1024, turn))
                writes.append((time.perf_counter() - ts) * 1000)
                if config.get("think_ms", 0):
                    time.sleep(config["think_ms"] / 1000)
            ts = time.perf_counter()
            try:
                with stop_lock if config["serialize_stop"] else contextlib.nullcontext():
                    acquired = time.perf_counter()
                    f.hook("stop", {"session_id": session, "reason": "end_turn", "response": f"Review completed {turn}"})
                stops.append({"ms": (time.perf_counter() - ts) * 1000, "queue_ms": (acquired - ts) * 1000, "ok": True})
            except Exception as error:
                stops.append({"ms": (time.perf_counter() - ts) * 1000, "ok": False, "error": str(error)})
                break
            turn_times.append((time.perf_counter() - turn_start) * 1000)
    except Exception as error:
        failures.append(str(error))
    wall = time.perf_counter() - started
    f.owner_log.close()
    return {"session": session, "wall_s": wall, "reads": reads, "writes_ms": writes,
            "stops": stops, "turn_ms": turn_times, "failures": failures}


def verify(f, sessions, config):
    verified = []
    for row in sessions:
        sid = row["session"]
        ledger = json.loads(f.run("session", "show", sid, "--json").stdout)[1]
        assert len(ledger) == config["turns"], (sid, ledger)
        for turn, entry in enumerate(ledger, 1):
            state = f.rpc({"TurnStatus": {"session_id": sid, "turn_number": turn}})["TurnLifecycle"]["turn"]
            assert state["state"] == "Completed", state
            attempt = state["checkpoint_attempt"]
            assert attempt["phase"] == "Published", attempt
            assert not entry["change_hashes"], entry
            assert entry["provenance_hash"] == attempt["provenance_hash"]
            frozen = f.rpc({"LoadFrozenEnvelopes": {"provenance_id": state["provenance_id"]}})["FrozenEnvelopes"]["envelopes"]
            envelopes = [json.loads(bytes(e)) for e in frozen]
            assert all(e["session_id"] == sid and e["turn_number"] == turn for e in envelopes)
            assert len({e["event_id"] for e in envelopes}) == len(envelopes)
            events = [e for e in envelopes if e["event"].get("tool_call_id", "").startswith(f"bench-{turn}-")]
            assert len(events) == config["events"], (sid, turn, len(events))
            assert len({e["event"]["tool_call_id"] for e in events}) == config["events"]
            for event in events:
                i = int(event["event"]["tool_call_id"].rsplit("-", 1)[1])
                assert event["event"]["output"] == base.output_payload(i, 1024)
                assert event["event"]["raw"]["tool_output"] == base.output_payload(i, 1024)
            digest = entry["provenance_hash"]
            data = (f.root / ".atomic" / "changes" / digest[:2] / f"{digest}.provenance").read_bytes()
            assert base64.b32encode(base.blake3.blake3(data).digest()).decode().rstrip("=") == digest
            if turn > 1:
                assert attempt["source"]["previous_provenance"] == ledger[turn - 2]["provenance_hash"]
        f.hook("stop", {"session_id": sid, "reason": "end_turn", "response": "duplicate delivery"})
        assert json.loads(f.run("session", "show", sid, "--json").stdout)[1] == ledger
        verified.append({"session": sid, "turns": len(ledger), "payloads_and_hashes": True, "duplicate_stop": True})
    return verified


def measure(binary, workers, config, template):
    with tempfile.TemporaryDirectory(prefix="atomic-multi-session-") as temp:
        root = pathlib.Path(temp)
        f = PreparedFixture(binary, root, template)
        try:
            # Seed vault context before opening the owner.
            f.start()
            config = dict(config)
            config["intent_ids"] = sorted(x["id"] for x in json.loads(f.run("intent", "list", "--json").stdout))
            assert len(config["intent_ids"]) == 32
            config["intent_id"] = config["intent_ids"][0]
            assert config["intent_id"] in f.run("intent", "show", config["intent_id"]).stdout.decode()
            for i in range(workers):
                f.hook("session-start", {"session_id": f"multi-agent-{i}"})
            ctx = multiprocessing.get_context("spawn")
            with ctx.Manager() as manager:
                gate = manager.Barrier(workers + 1)
                stop_lock = manager.Lock()
                with concurrent.futures.ProcessPoolExecutor(max_workers=workers, mp_context=ctx) as pool:
                    pending = [pool.submit(worker, str(binary), str(root), f.endpoint, i, config, gate, stop_lock) for i in range(workers)]
                    gate.wait(timeout=60)
                    rows = [future.result(timeout=180) for future in pending]
            result = {"workers": workers, "config": config, "sessions": rows,
                      "wall_s": max(r["wall_s"] for r in rows)}
            result["verified"], result["verification_errors"] = [], []
            for row in rows:
                try:
                    result["verified"].extend(verify(f, [row], config))
                except Exception as error:
                    result["verification_errors"].append({"session": row["session"], "error": str(error)})
            return result
        finally:
            logs = f.close()
            if "result" in locals():
                result["owner_log"] = logs


def summarize(rows):
    grouped = {}
    for row in rows:
        grouped.setdefault(f"{row['scenario']}:{row['workers']}:{row['mode']}", []).append(row)
    output = {}
    for key, runs in grouped.items():
        sessions = [s for r in runs for s in r["sessions"]]
        reads = [x for s in sessions for x in s["reads"]]
        stops = [x for s in sessions for x in s["stops"]]
        writes = [x for s in sessions for x in s["writes_ms"]]
        lat = lambda xs: base.summary(xs) if xs else None
        output[key] = {"runs": len(runs), "wall_s_median": statistics.median(r["wall_s"] for r in runs),
                       "wall_s_range": [min(r["wall_s"] for r in runs), max(r["wall_s"] for r in runs)],
                       "read_ok": sum(x["ok"] for x in reads), "read_total": len(reads),
                       "read_ms": lat([x["ms"] for x in reads if x["ok"]]),
                       "write_ms": lat(writes), "stop_ms": lat([x["ms"] for x in stops if x["ok"]]),
                       "stop_ok": sum(x["ok"] for x in stops), "stop_total": len(stops),
                       "verified_sessions": sum(len(r["verified"]) for r in runs),
                       "expected_sessions": sum(r["workers"] for r in runs),
                       "verification_failures": sum(len(r["verification_errors"]) for r in runs),
                       "worker_failures": sum(len(s["failures"]) for s in sessions),
                       "turn_ms": lat([x for s in sessions for x in s["turn_ms"]])}
    return output


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", action="append", required=True, help="mode=/absolute/path")
    ap.add_argument("--output", required=True)
    ap.add_argument("--workers", default="1,4,8")
    ap.add_argument("--repetitions", type=int, default=5)
    ap.add_argument("--scenarios", default="balanced,read-heavy")
    ap.add_argument("--events", type=int, default=16)
    ap.add_argument("--turns", type=int, default=2)
    ap.add_argument("--serialize-stop", action="store_true")
    args = ap.parse_args()
    binaries = dict(x.split("=", 1) for x in args.binary)
    configs = {"balanced": {"reads_per_event": 1}, "read-heavy": {"reads_per_event": 4},
               "paced": {"reads_per_event": 1, "think_ms": 100}}
    result = {"kind": "deterministic real-CLI multi-session replay; no model calls", "platform": platform.platform(),
              "binaries": {k: {"path": str(pathlib.Path(v).resolve()), "sha256": hashlib.sha256(pathlib.Path(v).read_bytes()).hexdigest()} for k, v in binaries.items()}, "runs": []}
    # Reuse stopped, fully recorded fixture templates. Setup is never timed.
    # TemporaryDirectory cleanup also removes copied local owner metadata.
    with tempfile.TemporaryDirectory(prefix="atomic-multi-templates-") as templates:
        roots = {}
        for mode, binary in binaries.items():
            root = pathlib.Path(templates) / mode
            root.mkdir()
            fixture = SeededFixture(pathlib.Path(binary).resolve(), root)
            try:
                fixture.start()
                fixture.hook("user-prompt-submit", {"session_id": base.SESSION, "prompt": "Prepare fixed benchmark context"})
                fixture.hook("stop", {"session_id": base.SESSION, "reason": "end_turn", "response": "Context prepared"})
            finally:
                fixture.close()
            roots[mode] = root
        execute(args, binaries, configs, result, roots)


def execute(args, binaries, configs, result, roots):
    for repetition in range(args.repetitions):
        modes = list(binaries)
        modes = modes[repetition % len(modes):] + modes[:repetition % len(modes)]
        for scenario in args.scenarios.split(","):
            config = {**configs[scenario], "events": args.events, "turns": args.turns, "serialize_stop": args.serialize_stop}
            for workers in map(int, args.workers.split(",")):
                for mode in modes:
                    row = measure(pathlib.Path(binaries[mode]).resolve(), workers, config, roots[mode])
                    row.update(mode=mode, scenario=scenario, repetition=repetition)
                    result["runs"].append(row)
                    result["summary"] = summarize(result["runs"])
                    pathlib.Path(args.output).write_text(json.dumps(result, indent=2) + "\n")
                    print(json.dumps({"mode": mode, "scenario": scenario, "workers": workers, "rep": repetition,
                                      "wall_s": row["wall_s"], "verified": len(row.get("verified", [])),
                                      "errors": len(row["verification_errors"])}), flush=True)


if __name__ == "__main__":
    main()
