#!/usr/bin/env python3
"""Check the recorded #27 evidence; optionally re-count regenerated fixtures.

This is a verifier for this dated evidence set, not a benchmark or a new gate.
It deliberately preserves the documented nominal-target calibration defects.
"""

import argparse
from collections import Counter, defaultdict
import hashlib
import json
import math
from pathlib import Path
import struct


ROOT = Path(__file__).resolve().parent
SIZES = ("small", "representative")
DRIVERS = ("drey", "naive")
ATTEMPTS = (1, 2)


def require(condition, message):
    if not condition:
        raise ValueError(message)


def read_json(path):
    return json.loads(path.read_text())


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def verify_runs(size, summary, executions):
    require(summary["plan_header"] == {"plan_version": 2}, f"{size}: recorded plan version")
    runs, rows = {}, {}
    for driver in DRIVERS:
        for attempt in ATTEMPTS:
            stem = f"{size}-{driver}-{attempt}"
            run = read_json(ROOT / f"{stem}.json")
            runs[driver, attempt] = run
            buckets = {row["op"]: row for row in run["results"]}
            rows[driver, attempt] = buckets
            require(len(buckets) == len(run["results"]) == 21, f"{stem}: buckets")
            require(run["run"]["mix"] == "measurement", f"{stem}: plan name")
            require(run["run"]["ops_total"] == summary["plan_ops"] == 23100,
                    f"{stem}: operation count")
            require(run["fixture"]["checksum_verified"] is True, f"{stem}: checksums")
            manifest = run["fixture"]["manifest"]
            require(manifest["source"] == "synthetic", f"{stem}: fixture source")
            require(manifest["counts"] == {key: summary["fixture_counts"][key]
                    for key in ("nodes", "edges", "embeddings")}, f"{stem}: fixture counts")
            require(summary["fixture_counts"]["dimensions"] ==
                    [manifest["parameters"]["embed_dim"]], f"{stem}: vector dimensions")
            for name, digest in manifest["checksums"].items():
                require(digest == "sha256:" + summary["files"][name],
                        f"{stem}: recorded fixture hash {name}")
            for row in buckets.values():
                require(row["budget_source"] == "synthetic", f"{stem}: budget source")
                require(row["status"] in ("ok", "n/a"), f"{stem}: op status")
                expected_pass = None
                if row["status"] == "ok":
                    require(row["samples"] == 1000, f"{stem}: sample floor")
                    require(0 <= row["p50_us"] <= row["p95_us"] <= row["p99_us"],
                            f"{stem}: percentile ordering")
                    if driver == "drey" and row["budget_us"] is not None:
                        expected_pass = row["p95_us"] <= row["budget_us"]
                        if row["budget_throughput_per_s"] is not None:
                            expected_pass &= (row["throughput_per_s"] >=
                                              row["budget_throughput_per_s"])
                require(row["pass"] is expected_pass, f"{stem}: verdict {row['op']}")
                if row["op"].startswith("decay_restore:"):
                    require(row["budget_us"] is None, f"{stem}: restore budget")
            failures = [row for row in run["results"] if row["pass"] is False]
            require(executions[stem]["exit_code"] == (3 if failures else 0),
                    f"{stem}: exit status")
            if driver == "drey":
                require(sum(row["pass"] is not None for row in buckets.values()) == 18,
                        f"{stem}: budgeted buckets")
                require(failures == summary["budget_failures"][str(attempt)],
                        f"{stem}: failure summary")

    baseline = runs["drey", 1]
    for run in runs.values():
        for field in ("fixture", "host", "resources"):
            require(run[field] == baseline[field], f"{size}: shared {field}")
    for driver in DRIVERS:
        first, second = rows[driver, 1], rows[driver, 2]
        require(first.keys() == second.keys(), f"{size}/{driver}: repeated buckets")
        for name in first:
            for field in ("status", "samples", "params", "counters", "budget_us",
                          "budget_throughput_per_s", "budget_source"):
                require(first[name][field] == second[name][field],
                        f"{size}/{driver}/{name}: repeated {field}")
        require(summary["counters_repeat_exactly"][driver] is True,
                f"{size}/{driver}: repetition summary")
    drey, naive = rows["drey", 1], rows["naive", 1]
    require(drey.keys() == naive.keys(), f"{size}: driver buckets")
    shared = {
        name: row["counters"] == naive[name]["counters"]
        for name, row in drey.items()
        if row["status"] == naive[name]["status"] == "ok"
        and row["counters"].keys() == naive[name]["counters"].keys()
    }
    require(shared == summary["shared_final_counters_match"] and all(shared.values()),
            f"{size}: shared final counters (excludes unlike traversal semantics)")
    require(len(summary["candidate_crosscheck"]) == 3, f"{size}: candidate shapes")
    for shape in summary["candidate_crosscheck"]:
        counts = {f"{driver}-{attempt}": rows[driver, attempt][shape["bucket"]]
                  ["counters"]["candidates"] for driver in DRIVERS for attempt in ATTEMPTS}
        require(counts == shape["runs"] and all(
            count == shape["plan"] == shape["fixture_scored_vectors"]
            for count in counts.values()), f"{size}: candidate agreement")
    for label, work in summary["decay_realized_edges"].items():
        require(drey[f"decay_edges:batch={label}"]["counters"]["edges_decayed"] ==
                work["last"], f"{size}: last decay counter")


def verify_fixture(directory, summary):
    for name, digest in summary["files"].items():
        require(sha256(directory / name) == digest, f"{directory}: hash {name}")
    with (directory / "workload.measurement.jsonl").open() as stream:
        require(json.loads(next(stream)) == summary["plan_header"] == {"plan_version": 2},
                f"{directory}: plan version")
        plan = [json.loads(line) for line in stream]
    require(len(plan) == summary["plan_ops"], f"{directory}: plan length")
    with (directory / "nodes.jsonl").open() as stream:
        nodes = {node["id"]: node for node in map(json.loads, stream)}
    with (directory / "edges.jsonl").open() as stream:
        edges = Counter(edge["type"] for edge in map(json.loads, stream))
    embeddings = {}
    with (directory / "embeddings.bin").open("rb") as stream:
        while True:
            header = stream.read(12)
            if not header:
                break
            node_id, dimension = struct.unpack("<QI", header)
            embeddings[node_id] = dimension
            stream.seek(dimension * 4, 1)
    require({"nodes": len(nodes), "edges": sum(edges.values()),
             "embeddings": len(embeddings), "dimensions": sorted(set(embeddings.values()))}
            == summary["fixture_counts"], f"{directory}: fixture counts")

    shapes, decay = {}, defaultdict(list)
    for index, op in enumerate(plan):
        if op["op"] == "similar":
            require(op["seed_node"] in embeddings, f"{directory}: embedded query seed")
            key = json.dumps({k: v for k, v in op.items() if k != "seed_node"}, sort_keys=True)
            shapes[key] = op
        elif op["op"] == "decay_edges" and op["factor"] < 1:
            restore = plan[index + 1]
            require(restore["op"] == "decay_edges" and
                    restore["edge_type"] == op["edge_type"] and
                    restore["batch"] == op["batch"] and
                    math.isclose(restore["factor"] * op["factor"], 1, rel_tol=1e-6),
                    f"{directory}: immediate reciprocal restore")
            decay[str(op["batch"])].append(
                edges[op["edge_type"]] if op["edge_type"] is not None else sum(edges.values()))
    require(len(shapes) == 3, f"{directory}: fixed candidate shapes")
    expected_shapes = {shape["bucket"]: shape for shape in summary["candidate_crosscheck"]}
    for op in shapes.values():
        shape = expected_shapes[f"similar_nodes:cand={op['cand_target']}"]
        prop = op["prop_filter"]
        require(prop is None or set(prop[1]) == {"str"}, f"{directory}: expected string filter")
        survivors = [node for node in nodes.values() if node["type"] in op["node_types"] and
                     (prop is None or (node["type"] == op["node_types"][0] and
                                       node["props"].get(prop[0]) == prop[1]["str"]))]
        scored = sum(embeddings.get(node["id"]) == embeddings[op["seed_node"]]
                     for node in survivors)
        require(op["node_types"] == shape["node_types"] and prop == shape["property_filter"],
                f"{directory}: recorded candidate filters")
        require(len(survivors) == shape["fixture_probe_nodes"] and
                scored == op["candidates"] == shape["plan"] == shape["fixture_scored_vectors"],
                f"{directory}: independently counted candidates")
    require(decay.keys() == summary["decay_realized_edges"].keys(), f"{directory}: decay labels")
    for label, counts in decay.items():
        retained = sorted(counts[100:])
        work = {"samples": len(retained), "min": retained[0], "max": retained[-1],
                "last": counts[-1]}
        for name, percentile in (("p50", .50), ("p95", .95), ("p99", .99)):
            work[name] = retained[math.ceil(len(retained) * percentile) - 1]
        require(work == summary["decay_realized_edges"][label],
                f"{directory}: independently counted decay work")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixtures", type=Path, help="parent containing small/ and representative/")
    args = parser.parse_args()
    analysis = read_json(ROOT / "analysis.json")
    executions = read_json(ROOT / "executions.json")
    for size in SIZES:
        verify_runs(size, analysis[size], executions)
        if args.fixtures:
            verify_fixture(args.fixtures / size, analysis[size])
    legacy = read_json(ROOT / "legacy-plan-rejection.json")
    require({probe["probe"] for probe in legacy} == {"headerless-v1", "version-1-header"},
            "legacy probe names")
    require(all(probe["exit_code"] == 1 and probe["stdout_bytes"] == 0 and
                "regenerate" in probe["stderr"] for probe in legacy), "legacy probe outcomes")
    print("Recorded evidence is consistent; nominal-target caveats remain.")
    if args.fixtures:
        print("Regenerated fixture hashes, plan v2, candidate counts and decay work agree.")


if __name__ == "__main__":
    main()
