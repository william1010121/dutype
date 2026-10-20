#!/usr/bin/env python3
"""Bounded, reproducible before/after report and stdio pagination measurements.

Use --prepare before a coordinated timing window, then run without it. Only
synthetic fixtures below --output are created. Raw trials are kept, not trimmed.
"""
import argparse
import csv
import hashlib
import json
import os
from pathlib import Path
import selectors
import shlex
import statistics
import subprocess
import time


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--before", required=True)
parser.add_argument("--after", required=True)
parser.add_argument("--output", required=True, type=Path)
parser.add_argument("--hyperfine", default="hyperfine")
parser.add_argument("--prepare", action="store_true")
args = parser.parse_args()
args.before = str(Path(args.before).resolve())
args.after = str(Path(args.after).resolve())
p = args.output.resolve()
env = dict(os.environ, DUTYPE_CACHE_DIR=str(p / "cache"), RAYON_NUM_THREADS="4")
config = p / "config.toml"


def cli(binary, root, category, top):
    command = [binary, "--json", "--cache", "--config", str(config), "--top", str(top)]
    if category:
        command += ["--category", "PDF"]
    return command + [str(root)]


if args.prepare:
    p.mkdir(parents=True, exist_ok=False)
    config.write_text("")
    for name, count in [("small", 600), ("large", 20_000)]:
        root = p / name
        for index in range(count):
            directory = root / f"d-{index // 200:03}"
            directory.mkdir(parents=True, exist_ok=True)
            (directory / f"f-{index:06}.pdf").touch()
        subprocess.run(cli(args.before, root, True, 0), env=env, stdout=subprocess.DEVNULL, check=True)
    (p / "manifest.json").write_text(json.dumps({
        "before": args.before, "before_sha256": digest(args.before),
        "after": args.after, "after_sha256": digest(args.after),
        "fixtures": {"small": 600, "large": 20_000}, "rayon_threads": 4,
        "cli_warmups": 5, "cli_runs_per_round": 30, "rounds": 2,
        "mcp_warmups": 5, "mcp_runs_per_round": 100,
    }, indent=2) + "\n")
    print("Prepared fixtures and immutable binary manifest", flush=True)
    raise SystemExit

manifest = json.loads((p / "manifest.json").read_text())
assert digest(args.before) == manifest["before_sha256"]
assert digest(args.after) == manifest["after_sha256"]
rows = []
rss_rows = []
fingerprints = {}

# Equality checks and preparation are deliberately outside measured intervals.
for name in ["small", "large"]:
    for category, top in [(False, 20), (True, 50), (True, 500)]:
        before, after = [json.loads(subprocess.check_output(cli(binary, p / name, category, top), env=env))
                         for binary in [args.before, args.after]]
        assert before == after, (name, category, top)
        fingerprints[f"cli-{name}-{category}-{top}"] = hashlib.sha256(json.dumps(before, sort_keys=True).encode()).hexdigest()


class Server:
    def __init__(self, binary):
        self.child = subprocess.Popen([binary, "--mcp", "--cache", "--config", str(config)],
                                      stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                      env=env, text=True, bufsize=1)
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.child.stdout, selectors.EVENT_READ)
        self.number = 0

    def request(self, root, **options):
        self.number += 1
        arguments = {"path": str(root), "category": "PDF", **options}
        request = {"jsonrpc": "2.0", "id": self.number, "method": "tools/call",
                   "params": {"name": "find_by_category", "arguments": arguments}}
        start = time.perf_counter_ns()
        self.child.stdin.write(json.dumps(request) + "\n")
        self.child.stdin.flush()
        assert self.selector.select(timeout=30), "stdio timeout"
        line = self.child.stdout.readline()
        elapsed = time.perf_counter_ns() - start
        response = json.loads(line)
        assert "error" not in response and response["result"]["isError"] is False, response
        return response["result"]["structuredContent"], elapsed / 1e9

    def close(self):
        self.child.stdin.close()
        assert self.child.wait(timeout=30) == 0
        assert not self.child.stderr.read()
        self.selector.close()


(p / "load-before.txt").write_text(Path("/proc/loadavg").read_text() if Path("/proc/loadavg").exists() else "unavailable")
for round_number in range(2):
    for index, (name, category, top) in enumerate([
        ("small", True, 50), ("small", False, 20),
        ("large", True, 50), ("large", True, 500), ("large", False, 20),
    ]):
        order = ["before", "after"] if (round_number + index) % 2 == 0 else ["after", "before"]
        label = f"cli-{name}-{'category' if category else 'full'}-{top}"
        output = p / f"{label}-round{round_number + 1}.json"
        commands = [shlex.join(cli(getattr(args, version), p / name, category, top)) for version in order]
        result = subprocess.run([args.hyperfine, "-N", "--warmup", "5", "--runs", "30",
                                 "--export-json", str(output), *commands],
                                env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, check=True)
        output.with_suffix(".log").write_text(result.stdout)
        for version, result in zip(order, json.loads(output.read_text())["results"]):
            measurements = result.get("measurements")
            durations = [m["time_wall_clock"]["value"] for m in measurements] if measurements else result["times"]
            for trial, duration in enumerate(durations):
                rows.append([label, version, round_number + 1, trial, duration])
                if measurements:
                    rss_rows.append([label, version, round_number + 1, trial, measurements[trial]["memory_peak_resident"]["value"] / 1024])
        print(label, "round", round_number + 1, "done", flush=True)
    for name in ["small", "large"]:
        order = ["before", "after"] if round_number == 0 else ["after", "before"]
        for version in order:
            server = Server(getattr(args, version))
            root = p / name
            legacy, _ = server.request(root, top=50)  # Load once, outside steady-state timings.
            key = f"mcp-{name}-legacy-50"
            signature = hashlib.sha256(json.dumps(legacy, sort_keys=True).encode()).hexdigest()
            assert key not in fingerprints or fingerprints[key] == signature
            fingerprints[key] = signature
            workloads = [("legacy-50", {"top": 50})]
            if version == "after":
                workloads += [("page-first-50", {"paginate": True, "top": 50}),
                              ("page-late-50", None)]
            for label, options in workloads:
                # Prepare a late boundary only after legacy/first workloads:
                # their latency and peak RSS must not include a 500-item page.
                if options is None:
                    page, _ = server.request(root, paginate=True, top=500)
                    last_cursor = None
                    while page["pagination"]["has_more"]:
                        last_cursor = page["pagination"]["next_cursor"]
                        page, _ = server.request(root, cursor=last_cursor, top=500)
                    assert last_cursor is not None
                    options = {"cursor": last_cursor, "top": 50}
                status = Path(f"/proc/{server.child.pid}/status")
                if status.exists():
                    values = dict(line.split(":", 1) for line in status.read_text().splitlines() if ":" in line)
                    rss_rows.append([f"mcp-{name}-{label}", version, round_number + 1, "pre-workload-hwm", int(values["VmHWM"].split()[0])])
                for _ in range(5):
                    server.request(root, **options)
                for trial in range(100):
                    _, duration = server.request(root, **options)
                    rows.append([f"mcp-{name}-{label}", version, round_number + 1, trial, duration])
                status = Path(f"/proc/{server.child.pid}/status")
                if status.exists():
                    values = dict(line.split(":", 1) for line in status.read_text().splitlines() if ":" in line)
                    rss_rows.append([f"mcp-{name}-{label}", version, round_number + 1, "post-workload-hwm", int(values["VmHWM"].split()[0])])
            server.close()
            print("mcp", name, version, "round", round_number + 1, "done", flush=True)
(p / "load-after.txt").write_text(Path("/proc/loadavg").read_text() if Path("/proc/loadavg").exists() else "unavailable")
with (p / "raw.csv").open("w", newline="") as stream:
    writer = csv.writer(stream, lineterminator="\n")
    writer.writerow(["case", "version", "round", "trial", "seconds"])
    writer.writerows(rows)
with (p / "rss.csv").open("w", newline="") as stream:
    writer = csv.writer(stream, lineterminator="\n")
    writer.writerow(["case", "version", "round", "trial", "peak_rss_kib"])
    writer.writerows(rss_rows)
(p / "output-fingerprints.json").write_text(json.dumps(fingerprints, indent=2) + "\n")
summary = {}
for case, version, _, _, _ in rows:
    key = case + "/" + version
    if key not in summary:
        samples = [row[4] * 1000 for row in rows if row[0] == case and row[1] == version]
        summary[key] = {"median_ms": statistics.median(samples), "mean_ms": statistics.mean(samples), "trials": len(samples)}
(p / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
print(json.dumps(summary, indent=2), flush=True)
