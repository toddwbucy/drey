#!/usr/bin/env python3
"""Run the same release probe against a clean DREY checkout, twice."""

import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tomllib


def output(command, cwd):
    """Capture a successful command's text output without shell interpretation."""
    return subprocess.check_output(command, cwd=cwd, text=True).strip()


def digest(path):
    """Hash the exact input or result bytes retained with this measurement."""
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    """Build with checkout-pinned dependencies and retain raw results/provenance."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("checkout", type=Path)
    parser.add_argument("destination", type=Path, help="new, nonexistent directory")
    args = parser.parse_args()
    checkout = args.checkout.resolve()
    destination = args.destination.resolve()
    commit = output(["git", "rev-parse", "HEAD"], checkout)
    subprocess.run(
        ["git", "diff", "--exit-code", "HEAD", "--", "drey", "Cargo.toml", "Cargo.lock"],
        cwd=checkout, check=True,
    )
    destination.mkdir()  # Refuse to overwrite measurements or reuse database state.
    build = destination / "build"
    (build / "src").mkdir(parents=True)
    probe = Path(__file__).with_name("probe.rs")
    shutil.copyfile(probe, build / "src/main.rs")
    (build / "Cargo.toml").write_text(
        '[package]\nname = "drey-point-read-probe"\nversion = "0.0.0"\n'
        'edition = "2021"\n\n[workspace]\n\n[dependencies]\n'
        f'drey = {{ path = {json.dumps(str(checkout / "drey"))} }}\n'
        'serde_json = "1"\n'
    )
    shutil.copyfile(checkout / "Cargo.lock", build / "Cargo.lock")
    env = dict(os.environ, CARGO_TARGET_DIR=str(build / "target"), RUSTFLAGS="-D warnings")
    # Cargo adjusts only the workspace package set; verify every retained pin.
    command = ["cargo", "build", "--release", "--offline"]
    with (destination / "build.log").open("w") as log:
        subprocess.run(command, cwd=build, env=env, check=True, stdout=log, stderr=log)
    original = tomllib.loads((checkout / "Cargo.lock").read_text())["package"]
    resolved = tomllib.loads((build / "Cargo.lock").read_text())["package"]
    pins = {(p["name"], p["version"], p.get("source"), p.get("checksum")) for p in original}
    for p in resolved:
        if p["name"] != "drey-point-read-probe":
            assert (p["name"], p["version"], p.get("source"), p.get("checksum")) in pins, p
    executable = build / "target/release/drey-point-read-probe"
    for repeat in (1, 2):
        data = destination / f"database-{repeat}"
        data.mkdir()
        with (destination / f"run-{repeat}.jsonl").open("w") as result:
            subprocess.run([str(executable), str(data)], check=True, stdout=result)
    cpuinfo = Path("/proc/cpuinfo")
    cpu = next(
        (line.split(":", 1)[1].strip() for line in cpuinfo.read_text().splitlines()
         if line.startswith("model name")), "unknown"
    ) if cpuinfo.exists() else platform.processor()
    memory = Path("/proc/meminfo")
    mem_kib = next(
        (int(line.split()[1]) for line in memory.read_text().splitlines()
         if line.startswith("MemTotal:")), None
    ) if memory.exists() else None
    metadata = {
        "recorded_at": datetime.now(timezone.utc).isoformat(),
        "commit": commit,
        "drey_tree": output(["git", "rev-parse", "HEAD:drey"], checkout),
        "probe_sha256": digest(probe),
        "repository_lock_sha256": digest(checkout / "Cargo.lock"),
        "probe_lock_sha256": digest(build / "Cargo.lock"),
        "dependency_pins_verified": True,
        "build": command,
        "rustflags": env["RUSTFLAGS"],
        "rustc": output(["rustc", "--version", "--verbose"], checkout),
        "cargo": output(["cargo", "--version"], checkout),
        "cpu": cpu, "logical_cpus": os.cpu_count(), "memory_kib": mem_kib,
        "kernel": platform.release(), "architecture": platform.machine(),
        "conditions": "Sequential, unpinned, uncontrolled background load; no IPC; no vectors/edges.",
        "populations": [100, 1000, 10000], "warmup": 200, "samples": 1000,
        "percentiles": "nearest rank p50/p95/p99, microseconds",
        "sha256": {f"run-{i}.jsonl": digest(destination / f"run-{i}.jsonl") for i in (1, 2)},
    }
    (destination / "environment.json").write_text(json.dumps(metadata, indent=2) + "\n")
    print(json.dumps({"commit": commit, "destination": str(destination)}))


if __name__ == "__main__":
    main()
