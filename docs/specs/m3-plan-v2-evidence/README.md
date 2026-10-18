# M3 plan-version-2 baseline: issue #27

Measured **2026-10-18**, branch `main`, commit
[`6ae23242044726b2a6d855516504b12cafbd2c5d`](https://github.com/toddwbucy/drey/commit/6ae23242044726b2a6d855516504b12cafbd2c5d).
The accompanying documentation commits do not change the measured code. This
records the re-baseline requested in [#27](https://github.com/toddwbucy/drey/issues/27),
after #25 corrected timed similarity setup and separated decay restores.
**All budgets and workloads are synthetic; measurements are provisional.**

## Evidence and method

- Eight original, unmodified `bench` JSON documents: two runs each of `drey` and
  `naive` on `small` and `representative`. Filenames are `<size>-<driver>-<run>.json`.
- [environment.json](environment.json): measured commit, branch, compiler, kernel,
  build command, and conditions. Each run also embeds its host and full fixture manifest.
- [executions.json](executions.json): original commands, exit status, elapsed/CPU
  times, and page faults. These process totals include loading and untimed setup;
  they are not per-query latency or resident-memory measurements.
- [analysis.json](analysis.json): fixture and workload SHA-256 hashes, actual work
  counts, repeated final counters, candidate agreement, and failed rows.
- [legacy-plan-rejection.json](legacy-plan-rejection.json): the release `bench`
  refused both a headerless v1 plan and an explicit version-1 header, exiting 1
  without run JSON. The two generation logs record successful fixture/plan checks.
- [verify.py](verify.py): checks recorded budgets/verdicts, sample floors, restore
  exclusions, repetitions, and candidate-counter agreement. With regenerated fixtures
  it also verifies every recorded hash and independently recounts the actual work.

Host: AMD Ryzen Threadripper 7960X, 24 physical cores/48 logical CPUs, 248 GiB
reported RAM, Linux `7.2.8-1-cachyos` x86-64, rustc/cargo 1.98.1. Release profile;
processes ran sequentially without CPU pinning or controlled background load.
Fixtures were on `/tmp` tmpfs. No inference workload ran as part of this experiment.

Both fixtures use generator version 2, seed 42, medium fanout (mean 5), degree
exponent 1.2, and maximum-degree parameter 1,000. Properties are short synthetic
scalars; this is not a passage-text/document retention workload.

| Fixture | Nodes | Edges | Node/edge types | Vectors | Dimensions |
| --- | ---: | ---: | --- | ---: | ---: |
| Small | 1,000 | 5,000 | 4 / 8 | 513 | 256 |
| Representative | 50,000 | 250,000 | 12 / 24 | 25,066 | 1,024 |

All generated workloads carry `{"plan_version":2}`. Only `measurement` was timed:
23,100 operations, 18 budgeted buckets and three unbudgeted restore buckets, with
100 discarded warmups and 1,000 retained timings per applicable bucket. Percentiles
use nearest rank. Run 1 and run 2 are independent processes; their percentiles are
not pooled. Fixture checksums verified in all eight runs.

`runner::run` times `GraphDriver::run_op`, including driver/counter overhead. Parsing,
fixture loading, seed-embedding resolution and cached actual-candidate counting in
`prepare_op` are outside the timer. Similarity uses cosine, k=10, type/property
filters and `allow_full_scan=true`; it does not exercise reachability composition.
Traversal caps paths at 1,000; shortest path caps node expansions at 512. The
workload interleaves reads, updates, decays and immediate reciprocal restores.

## Representative budget table

All times below are **microseconds**. Ranges are the minimum/maximum of the two
run summaries, rounded to two decimals; they are not confidence intervals. Both
runs pass **16/18** budgeted buckets and exit **3**. The three restore rows have
null budgets and verdicts. Exact values remain in
[run 1](representative-drey-1.json) and [run 2](representative-drey-2.json).

| Bucket (labels are nominal) | p50 µs | p95 µs | p99 µs | p95 budget µs | Both runs |
| --- | ---: | ---: | ---: | ---: | --- |
| `decay_edges:batch=1000` | 436.08–437.99 | 6,292.45–6,507.60 | 15,338.75–15,526.38 | 1,000 | **fail** |
| `decay_edges:batch=10000` | 467.98–476.00 | 6,303.67–6,456.91 | 15,284.84–15,292.40 | 10,000 | pass |
| `decay_edges:batch=100000` | 423.11–429.35 | 6,437.12–6,512.27 | 15,236.83–15,408.44 | 100,000 | pass |
| `neighbors` | 1.69–1.82 | 5.81–6.29 | 13.42–13.73 | 100 | pass |
| `property_eq:sel=common` | 1.44–1.50 | 1.80–1.81 | 1.92–1.98 | 1,000 | pass |
| `property_eq:sel=rare` | 2.01–2.26 | 2.75–3.01 | 3.33–3.53 | 1,000 | pass |
| `property_eq:sel=uncommon` | 0.62–0.63 | 0.78–0.80 | 0.86–0.90 | 1,000 | pass |
| `property_range:sel=common` | 12.64–12.71 | 13.81–13.99 | 16.10–17.36 | 1,000 | pass |
| `property_range:sel=rare` | 1.22–1.23 | 1.63 | 1.83–1.84 | 1,000 | pass |
| `property_range:sel=uncommon` | 2.21–2.29 | 2.56–2.75 | 2.80–3.15 | 1,000 | pass |
| `shortest_path:hops` | 6,218.72–6,233.97 | 7,757.11–7,757.76 | 8,285.26–8,297.95 | 10,000 | pass |
| `shortest_path:weighted` | 7,977.69–8,195.05 | 9,740.14–9,788.66 | 10,291.61–10,388.86 | 10,000 | pass |
| `similar_nodes:cand=100` | 541.16–542.20 | 574.91–575.03 | 584.09–585.26 | 10,000 | pass |
| `similar_nodes:cand=1000` | 4,819.69–4,825.09 | 4,951.56–4,967.94 | 5,007.05–5,007.45 | 10,000 | pass |
| `similar_nodes:cand=10000` | 24,822.24–24,823.72 | 25,072.50–25,077.83 | 25,299.48–25,954.40 | 10,000 | **fail** |
| `traverse:max_hops=2` | 7.09–7.20 | 164.86–166.56 | 183.28–184.64 | 1,000 | pass |
| `traverse:max_hops=5` | 163.54–164.73 | 241.34–242.96 | 275.57–275.60 | 10,000 | pass |
| `update_edge_weight` | 0.76–0.78 | 1.11–1.15 | 1.50–1.62 | 10 | pass |

`update_edge_weight` also exceeds the 100,000 updates/s requirement: 1,210,147 and
1,240,179 updates/s, calculated from retained operation time (not total process
time). The gate compares p95; weighted shortest-path p99 exceeding 10 ms is not
an additional gate failure. Both [small runs](small-drey-1.json) pass all 18
budgeted buckets; [the repeat](small-drey-2.json) agrees. Naive verdicts are null:
that driver validates mechanics and is not a speed-comparison baseline.

## Actual work and verification limits

| Nominal similarity target | Small: probed nodes / scored vectors | Representative: probed nodes / scored vectors |
| --- | ---: | ---: |
| 100 | 25 / 16 | 417 / 209 |
| 1,000 | 250 / 125 | 4,167 / 2,060 |
| 10,000 | 1,000 / 513 | 20,835 / 10,453 |

All three candidate counts agree across DREY actual, naive actual, plan constants,
and an independent count of the fixture's node properties and embedding records.
The plan has three fixed candidate shapes; seed vectors change, but candidate
properties/vectors do not. This checks cardinality, not ranked IDs or scores.
The first two representative targets violate M0 §4.1's ±25% tolerance. Small
cannot supply the larger targets. Publish the realized counts alongside labels.

The decay driver ignores the nominal batch count and applies the selected edge
type's whole population. Independent counts of **retained** representative samples:

| Nominal batch | Minimum edges | Median edges | p95 edges | Maximum / p99 edges | Last-sample counter |
| --- | ---: | ---: | ---: | ---: | ---: |
| 1,000 | 1,848 | 3,841 | 37,109 | 84,323 | 2,482 |
| 10,000 | 1,848 | 4,393 | 37,109 | 84,323 | 6,086 |
| 100,000 | 1,848 | 4,393 | 37,109 | 84,323 | 2,179 |

These work-count percentiles are not paired with latency percentiles. The stock
runner retains only each bucket's final counters, not per-sample traces. All
reported counters repeat exactly within each driver; shared final counters agree
where semantics match. Naive traversal reports bounded BFS edge hops, DREY reports
DFS paths, so they are excluded from cross-driver comparison. Unsupported naive
operations remain `n/a`.

Implementation references at the measured commit: `fixture::{PLAN_VERSION,
read_workload}`, `workload::{similar_op, decay_op, with_decay_restores}`,
`driver::{DreyDriver, NaiveDriver}` implementations of `prepare_op`/`run_op`, and
`runner::{run, nearest_rank}` in `harness/src/`. `drey/src/similarity.rs` bounds
probed nodes before dropping missing/wrong-dimension embeddings; scored-vector
counters are a different quantity from that ceiling.

## Reproduction

Python 3 uses only its standard library. Check the committed evidence directly:

```sh
python3 docs/specs/m3-plan-v2-evidence/verify.py
```

To regenerate the exact fixtures and measure again, start at this documentation
checkout and use an isolated checkout of the measured commit. Rust dependencies
are locked; add `--offline` to the build if they are already cached. Run sequentially:

```sh
set -eu
DREY_EVIDENCE_DIR="$PWD/docs/specs/m3-plan-v2-evidence"
DREY_REBASELINE=$(mktemp -d /tmp/drey-m3-v2.XXXXXX)
git worktree add --detach "$DREY_REBASELINE/source" 6ae23242044726b2a6d855516504b12cafbd2c5d
CARGO_TARGET_DIR="$DREY_REBASELINE/target" cargo build --release -p harness --bins --locked \
  --manifest-path "$DREY_REBASELINE/source/Cargo.toml"
mkdir "$DREY_REBASELINE/results"
for DREY_SIZE in small representative; do
  "$DREY_REBASELINE/target/release/generate" "$DREY_SIZE" medium 42 "$DREY_REBASELINE/$DREY_SIZE"
  for DREY_DRIVER in drey naive; do
    for DREY_RUN in 1 2; do
      DREY_RESULT="$DREY_REBASELINE/results/$DREY_SIZE-$DREY_DRIVER-$DREY_RUN"
      DREY_BENCH_STATUS=0
      "$DREY_REBASELINE/target/release/bench" "$DREY_REBASELINE/$DREY_SIZE" "$DREY_DRIVER" measurement \
        > "$DREY_RESULT.json" 2> "$DREY_RESULT.stderr" || DREY_BENCH_STATUS=$?
      printf '%s\n' "$DREY_BENCH_STATUS" > "$DREY_RESULT.exit"
      case "$DREY_BENCH_STATUS" in
        0|3) ;; # 3 means a recorded budget failure, not a broken run
        *) exit "$DREY_BENCH_STATUS" ;;
      esac
    done
  done
done
python3 "$DREY_EVIDENCE_DIR/verify.py" --fixtures "$DREY_REBASELINE"
```

The last command validates the committed records against regenerated fixture/plan
bytes and independent work counts. It does not require fresh latency values or
budget verdicts to match: those are host/load-dependent and are saved separately
under `results/`. Preserve the new host/compiler/commit context if publishing them.
Large fixtures and plans are reproducible from the recorded hashes and are not
checked into Git. The four generated mix plans are hashed but were not benchmarked.

## Validation

Documentation-branch checks passed with `RUSTFLAGS="-D warnings"`: workspace/all-target
build, all **164 workspace tests** (including the four Unix-socket service tests),
format check, and clippy with warnings denied. The standalone verifier passed on
the original fixtures and a second generation in fresh directories. Deliberately
changed candidate counters, decay distributions, and restore budgets were rejected
in disposable copies. All 18 table rows were checked against the original JSON.
The two representative `bench` exit-3 results are the recorded budget failures;
they are distinct from the passing correctness and documentation checks.

## Disposition and exclusions

Both documented failures remain; see [M3 findings](../m3-findings.md) F3/F5.
Candidate-target and decay-population calibration need separate harness work,
including a plan-version bump if measurement semantics change. Keep existing
budgets while establishing the actual requested work and consumer tolerances.
This evidence does not choose ANN, a persistence replacement, or a narrower query
that discards required content. Three-scale README publication remains #19.

No stress, other fanout/dimension, unfiltered-similarity, or workload-mix sweep was
performed here. No raw per-sample timing trace is emitted by this harness. Resident
memory and link-size fields remain null; commit/open durability costs are outside
this in-memory plan. These measurements neither include Unix IPC/serialization
nor establish a Weaver deadline. The [local-engine experiment](../local-engine-v1-evidence/README.md)
has a separate workload and timing boundary. The July figures predate the validity
fixes and cannot serve as a controlled regression comparison.
