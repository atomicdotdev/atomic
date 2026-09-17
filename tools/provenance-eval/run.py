#!/usr/bin/env python3
"""Benchmark real Atomic owner RPCs and foreground Stop checkpoints on Unix.

Uses throwaway repositories and explicit binaries. Does not alter installed CLIs,
user configuration, real repositories, or the production owner service.
"""
import argparse
import base64
import concurrent.futures
import hashlib
import json
import os
import pathlib
import platform
import socket
import statistics
import struct
import subprocess
import tempfile
import time

import blake3

SESSION = "provenance-perf"
SCENARIOS = {
    "rpc-single": dict(events=128, batch=1, workers=1, transport="rpc", turns=1),
    "rpc-batch16": dict(events=128, batch=16, workers=1, transport="rpc", turns=1),
    "rpc-batch16-concurrent8": dict(events=128, batch=16, workers=8, transport="rpc", turns=1),
    "rpc-concurrent8": dict(events=128, batch=1, workers=8, transport="rpc", turns=1),
    "hook-single": dict(events=32, batch=1, workers=1, transport="hook", turns=1),
    "hook-concurrent8": dict(events=32, batch=1, workers=8, transport="hook", turns=1),
    "long-turn": dict(events=512, batch=16, workers=1, transport="rpc", turns=1),
    "read-only-turn": dict(events=128, batch=1, workers=1, transport="rpc", turns=1, write_file=False),
    "five-turn-history": dict(events=32, batch=1, workers=1, transport="rpc", turns=5),
    "frame-boundary": dict(events=1024, batch=16, workers=1, transport="rpc", turns=1),
}


def encoded(value):
    return json.dumps(value, separators=(",", ":"), ensure_ascii=False).encode()


def summary(samples):
    ordered = sorted(samples)
    return {"count": len(samples), "p50_ms": statistics.median(ordered),
            "p95_ms": ordered[min(len(ordered) - 1, int(len(ordered) * .95))],
            "min_ms": ordered[0], "max_ms": ordered[-1]}


def output_payload(index, size):
    # Deterministic, varied hex avoids an unrealistically compressible repeated byte.
    text = "".join(hashlib.sha256(f"event-{index}-block-{block}".encode()).hexdigest()
                   for block in range((size + 63) // 64))
    return text[:size]


def tool_payload(index, size, turn):
    return {"session_id": SESSION, "tool_use_id": f"bench-{turn}-{index}",
            "tool_name": "Read", "tool_input": {"path": f"src/module_{index % 16}.rs"},
            "tool_output": output_payload(index, size), "status": "completed"}


def envelope(index, size, turn, generation, timestamp_ms):
    raw = tool_payload(index, size, turn)
    return {"schema_version": 1, "event_id": f"bench-event-{turn}-{index}",
            "session_id": SESSION, "turn_number": turn, "generation": generation,
            "timestamp_ms": timestamp_ms + index, "causal_parent_ids": [],
            "event": {"type": "tool", "phase": "after", "tool_name": raw["tool_name"],
                      "tool_call_id": raw["tool_use_id"], "input": raw["tool_input"],
                      "output": raw["tool_output"], "status": "completed", "raw": raw}}


class Fixture:
    def __init__(self, binary, root):
        self.binary = str(binary)
        self.root = root
        self.env = os.environ.copy()
        self.env["NO_COLOR"] = "1"
        # The foreground owner is the only process this fixture owns and stops.
        self.owner = None
        self.owner_log = tempfile.TemporaryFile()

    def start(self):
        self.run("init", str(self.root))
        canonical = str((self.root / ".atomic").resolve())
        digest = blake3.blake3(canonical.encode()).hexdigest()[:24]
        self.endpoint = f"/tmp/atomic-owner-{digest}.sock"
        self.owner = subprocess.Popen(
            [self.binary, "agent", "database-owner", "serve", "--repository", str(self.root)],
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=self.owner_log,
            env=self.env)
        deadline = time.monotonic() + 15
        while True:
            try:
                self.rpc("Ping")
                break
            except (OSError, RuntimeError):
                if self.owner.poll() is not None or time.monotonic() > deadline:
                    raise RuntimeError("owner failed to start")
                time.sleep(.025)
        self.hook("session-start", {"session_id": SESSION})

    def run(self, *args, payload=None, timeout=45):
        result = subprocess.run([self.binary, *args, "--no-color"], cwd=self.root,
                                input=payload, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                timeout=timeout, env=self.env)
        if result.returncode:
            raise RuntimeError(f"{' '.join(args)}: {result.stderr.decode(errors='replace')}")
        return result

    def hook(self, verb, payload):
        result = self.run("agent", "hooks", "claude-code", verb, "--foreground",
                          payload=encoded(payload))
        if result.stderr:
            raise RuntimeError(result.stderr.decode(errors="replace"))
        return result

    def rpc(self, request):
        # Match the owner's versioned JSON frame and one connection per RPC.
        frame = encoded({"version": 1, "request_id": "perf", "request": request})
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
            stream.settimeout(45)
            stream.connect(self.endpoint)
            stream.sendall(struct.pack(">I", len(frame)) + frame)

            def read_exact(size):
                data = bytearray()
                while len(data) < size:
                    chunk = stream.recv(size - len(data))
                    if not chunk:
                        raise RuntimeError("owner closed connection before response")
                    data.extend(chunk)
                return data

            size = struct.unpack(">I", read_exact(4))[0]
            if not 0 < size <= 8 * 1024 * 1024:
                raise RuntimeError(f"invalid owner frame size: {size}")
            response = json.loads(read_exact(size))["response"]
        if isinstance(response, dict) and "Error" in response:
            raise RuntimeError(str(response["Error"]))
        return response

    def status(self, turn):
        return self.rpc({"TurnStatus": {"session_id": SESSION, "turn_number": turn}})["TurnLifecycle"]["turn"]

    def close(self):
        if self.owner is not None and self.owner.poll() is None:
            try:
                self.rpc("Shutdown")
                self.owner.wait(timeout=10)
            except (OSError, RuntimeError, subprocess.TimeoutExpired):
                self.owner.kill()
                self.owner.wait(timeout=10)
        self.owner_log.seek(0)
        logs = self.owner_log.read().decode(errors="replace")
        self.owner_log.close()
        return logs


def measure_turn(fixture, config, payload_bytes, turn, previous_ledger):
    fixture.hook("user-prompt-submit", {"session_id": SESSION, "prompt": f"Implement measured turn {turn}"})
    running = fixture.status(turn)
    assert running["state"] == "Running"
    start_seq = running["next_event_seq"]
    pid = running["provenance_id"]
    generation = running["generation"]
    count = config["events"]

    requests = []
    if config["transport"] == "rpc":
        timestamp_ms = int(time.time() * 1000)
        envelopes = [envelope(i, payload_bytes, turn, generation, timestamp_ms) for i in range(count)]
        wire = [{"event_id": e["event_id"], "bytes": list(encoded(e))} for e in envelopes]
        for offset in range(0, count, config["batch"]):
            requests.append({"AppendProvenanceEnvelopes": {
                "provenance_id": pid, "expected_generation": generation,
                "envelopes": wire[offset:offset + config["batch"]], "now": timestamp_ms // 1000}})
    else:
        requests = [tool_payload(i, payload_bytes, turn) for i in range(count)]

    def append(request):
        started = time.perf_counter_ns()
        if config["transport"] == "rpc":
            response = fixture.rpc(request)["ProvenanceEnvelopesCommitted"]
            acks = response["acknowledgements"]
        else:
            fixture.hook("post-tool", request)
            acks = []
        return (time.perf_counter_ns() - started) / 1e6, acks

    # Pool creation is outside the timer; lazy thread startup is included in wall time.
    with concurrent.futures.ThreadPoolExecutor(max_workers=config["workers"]) as pool:
        started = time.perf_counter_ns()
        outputs = list(pool.map(append, requests))
        append_wall_ms = (time.perf_counter_ns() - started) / 1e6
    latencies = [row[0] for row in outputs]
    acks = [ack for row in outputs for ack in row[1]]
    after_append = fixture.status(turn)
    assert after_append["next_event_seq"] == start_seq + count, (start_seq, after_append)
    if acks:
        assert sorted(a["sequence"] for a in acks) == list(range(start_seq, start_seq + count))
        assert len({a["event_id"] for a in acks}) == count
    # Duplicate delivery is checked outside the measured append phase.
    append(requests[0])
    assert fixture.status(turn)["next_event_seq"] == start_seq + count

    # A real file change causes recording plus graph generation and ledger publication.
    source = "\n".join(f"pub fn value_{i}() -> u64 {{ {i + turn} }}" for i in range(128)) + "\n"
    writes_file = config.get("write_file", True)
    if writes_file:
        (fixture.root / "measured.rs").write_text(source)
    result = {"turn": turn, "events": count, "append_wall_ms": append_wall_ms,
              "events_s": count / (append_wall_ms / 1000), "append_request_ms": latencies,
              "append_latency": summary(latencies), "starting_event_seq": start_seq,
              "append_verified": True, "duplicate_verified": True,
              "file_bytes": len(source.encode()) if writes_file else 0}
    if config["transport"] == "rpc":
        result["tool_envelope_bytes"] = sum(len(e["bytes"]) for e in wire)
        # Lower bound: excludes the goal and terminal/checkpoint envelopes.
        result["frozen_tools_json_bytes"] = len(encoded({"FrozenEnvelopes": {"envelopes": [e["bytes"] for e in wire]}}))
    checkpoint_start = time.perf_counter_ns()
    try:
        fixture.hook("stop", {"session_id": SESSION, "reason": "end_turn", "response": f"Completed turn {turn}"})
    except (RuntimeError, subprocess.TimeoutExpired) as error:
        result.update(checkpoint_ms=(time.perf_counter_ns() - checkpoint_start) / 1e6,
                      checkpoint_ok=False, checkpoint_error=str(error), final_state=fixture.status(turn))
        return result, previous_ledger
    result["checkpoint_ms"] = (time.perf_counter_ns() - checkpoint_start) / 1e6
    completed = fixture.status(turn)
    assert completed["state"] == "Completed", completed
    attempt = completed["checkpoint_attempt"]
    assert attempt["phase"] == "Published", attempt
    assert attempt["provenance_hash"] is not None and attempt["manifest_hash"] is not None
    assert bool(attempt["source"]["change_hashes"]) == writes_file, "unexpected recorded file changes"
    page_sizes = []
    if fixture.rpc("Ping")["Pong"].get("frozen_envelope_paging", False):
        frozen = []
        partial = []
        cursor = {"seq": 0, "offset": 0}
        while cursor["seq"] < attempt["frozen_event_count"]:
            response = fixture.rpc({"LoadFrozenEnvelopesPage": {
                "provenance_id": pid, "attempt_generation": attempt["attempt_generation"],
                "frozen_event_count": attempt["frozen_event_count"], "cursor": cursor}})
            page_sizes.append(len(encoded({"version": 1, "request_id": "perf", "response": response})))
            page = response["FrozenEnvelopesPage"]["page"]
            assert (page["next"]["seq"], page["next"]["offset"]) > (cursor["seq"], cursor["offset"])
            for fragment in page["fragments"]:
                assert fragment["cursor"]["offset"] == len(partial)
                partial.extend(fragment["bytes"])
                if fragment["complete"]:
                    frozen.append(partial)
                    partial = []
            cursor = page["next"]
        assert not partial
        assert max(page_sizes) <= 8 * 1024 * 1024
    else:
        frozen = fixture.rpc({"LoadFrozenEnvelopes": {"provenance_id": pid}})["FrozenEnvelopes"]["envelopes"]
    result["verification_page_sizes"] = page_sizes
    decoded = [json.loads(bytes(e)) for e in frozen]
    tools = [e for e in decoded if e["event"].get("tool_call_id", "").startswith(f"bench-{turn}-")]
    assert len(tools) == count
    assert len({e["event_id"] for e in decoded}) == len(decoded)
    assert all(e["session_id"] == SESSION and e["turn_number"] == turn for e in decoded)
    for e in tools:
        index = int(e["event"]["tool_call_id"].rsplit("-", 1)[1])
        assert e["event"]["output"] == output_payload(index, payload_bytes)
        assert e["event"]["raw"]["tool_output"] == output_payload(index, payload_bytes)
    ledger = json.loads(fixture.run("session", "show", SESSION, "--json").stdout)[1]
    assert len(ledger) == turn and ledger[:-1] == previous_ledger
    assert ledger[-1]["provenance_hash"] == attempt["provenance_hash"]
    assert ledger[-1]["change_hashes"] == attempt["source"]["change_hashes"]
    graph_bytes = 0
    for entry in ledger:
        digest = entry["provenance_hash"]
        data = (fixture.root / ".atomic" / "changes" / digest[:2] / f"{digest}.provenance").read_bytes()
        assert base64.b32encode(blake3.blake3(data).digest()).decode().rstrip("=") == digest
        if digest == attempt["provenance_hash"]:
            graph_bytes = len(data)
    detail = fixture.run("session", "show", SESSION).stdout.decode()
    assert f"Manifest head: {attempt['manifest_hash']}" in detail
    if previous_ledger:
        assert attempt["source"]["previous_provenance"] == previous_ledger[-1]["provenance_hash"]
    # A repeated Stop must not publish another turn or change earlier ledger entries.
    fixture.hook("stop", {"session_id": SESSION, "reason": "end_turn", "response": f"Completed turn {turn}"})
    assert json.loads(fixture.run("session", "show", SESSION, "--json").stdout)[1] == ledger
    result.update(checkpoint_ok=True, checkpoint_verified=True, retry_verified=True,
                  frozen_events=len(decoded), envelope_bytes=sum(len(e) for e in frozen),
                  frozen_frame_bytes=len(encoded({"version": 1, "request_id": "perf", "response": {"FrozenEnvelopes": {"envelopes": frozen}}})),
                  ledger_turns=len(ledger), ledger_hash=attempt["provenance_hash"],
                  manifest_hash=attempt["manifest_hash"], previous_turns_unchanged=True,
                  graph_hash_verified=True, graph_bytes=graph_bytes, manifest_head_verified=True)
    return result, ledger


def measure_case(binary, name, config, payload_bytes):
    fixture = None
    with tempfile.TemporaryDirectory(prefix="atomic-provenance-perf-") as temp:
        root = pathlib.Path(temp)
        try:
            fixture = Fixture(binary, root)
            fixture.start()
            ledger = []
            turns = []
            for turn in range(1, config["turns"] + 1):
                row, ledger = measure_turn(fixture, config, payload_bytes, turn, ledger)
                turns.append(row)
                if not row["checkpoint_ok"]:
                    break
            result = {"scenario": name, "config": config, "turns": turns}
        finally:
            if fixture is not None:
                logs = fixture.close()
        result["owner_log"] = logs
        return result


def summarize(rows):
    groups = {}
    for row in rows:
        groups.setdefault((row["mode"], row["scenario"]), []).append(row)
    output = {}
    for (mode, scenario), cases in groups.items():
        turns = [t for case in cases for t in case["turns"]]
        checkpoints = [t["checkpoint_ms"] for t in turns if t["checkpoint_ok"]]
        rate = [sum(t["events"] for t in c["turns"]) / (sum(t["append_wall_ms"] for t in c["turns"]) / 1000) for c in cases]
        output[f"{mode}:{scenario}"] = {
            "runs": len(cases), "turns": len(turns), "successful_checkpoints": len(checkpoints),
            "median_events_s": statistics.median(rate), "min_events_s": min(rate), "max_events_s": max(rate),
            "median_request_p50_ms": statistics.median(t["append_latency"]["p50_ms"] for t in turns),
            "median_request_p95_ms": statistics.median(t["append_latency"]["p95_ms"] for t in turns),
            "checkpoint": summary(checkpoints) if checkpoints else None}
    return output


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", action="append", required=True, help="MODE=/absolute/path/to/release/atomic")
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--repeats", type=int, default=5)
    parser.add_argument("--payload-bytes", type=int, default=1024)
    parser.add_argument("--scenarios", nargs="+", choices=SCENARIOS, default=[s for s in SCENARIOS if s != "frame-boundary"])
    args = parser.parse_args()
    assert hasattr(socket, "AF_UNIX"), "this driver currently supports Unix sockets"
    binaries = dict(pair.split("=", 1) for pair in args.binary)
    binaries = {mode: pathlib.Path(path).resolve() for mode, path in binaries.items()}
    assert args.repeats > 0 and args.payload_bytes > 0
    metadata = {"platform": platform.platform(), "python": platform.python_version(),
                "started_at": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
                "payload_output_bytes": args.payload_bytes, "repeats": args.repeats,
                "blake3_version": blake3.__version__,
                "binaries": {m: {"path": str(p), "sha256": hashlib.sha256(p.read_bytes()).hexdigest()} for m, p in binaries.items()}}
    if platform.system() == "Darwin":
        metadata["hardware"] = subprocess.check_output(["sysctl", "machdep.cpu.brand_string", "hw.logicalcpu", "hw.memsize"], text=True)
    result = {"metadata": metadata, "measurements": []}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    for repeat in range(args.repeats):
        modes = list(binaries)
        modes = modes[repeat % len(modes):] + modes[:repeat % len(modes)]
        for name in args.scenarios:
            for mode in modes:
                case = measure_case(binaries[mode], name, SCENARIOS[name], args.payload_bytes)
                case.update(mode=mode, repeat=repeat)
                result["measurements"].append(case)
                result["summary"] = summarize(result["measurements"])
                args.output.write_text(json.dumps(result, indent=2) + "\n")
                print(json.dumps({"repeat": repeat, "mode": mode, "scenario": name,
                                  "events_s": [round(t["events_s"], 1) for t in case["turns"]],
                                  "checkpoint_ms": [round(t["checkpoint_ms"], 1) for t in case["turns"]],
                                  "checkpoint_ok": [t["checkpoint_ok"] for t in case["turns"]]}), flush=True)


if __name__ == "__main__":
    main()
