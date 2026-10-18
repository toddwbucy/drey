# M3 Budget-Gate Findings

Status: **updated 2026-10-18 for [issue #27](https://github.com/toddwbucy/drey/issues/27)**.
These are synthetic budgets and provisional measurements, superseded by a real
capture (M0 spec §6). The current baseline supersedes the July measurements below;
it retains both documented overruns. The PRD §21 M3 "written reason to revise"
exit is distinct from passing every §16.3 gate.

## Current baseline

Measured `main` at [`6ae23242044726b2a6d855516504b12cafbd2c5d`](https://github.com/toddwbucy/drey/commit/6ae23242044726b2a6d855516504b12cafbd2c5d),
after PRs #25, #30, #31, and #32. Regenerated generator-version-2, **plan-version-2**
fixtures: seed 42, medium fanout. Two sequential release runs per driver and size
used 100 warmups and 1,000 retained samples per bucket, with nearest-rank percentiles.
Host: AMD Ryzen Threadripper 7960X, 24 cores/48 logical CPUs, 248 GiB reported RAM,
Linux `7.2.8-1-cachyos`, Rust 1.98.1. CPU affinity and background load were uncontrolled.

Both small runs (1,000 nodes / 5,000 edges, 513 vectors at dimension 256) pass all
**18 latency/throughput buckets**. Both representative runs (50,000 nodes / 250,000
edges, 25,066 vectors at dimension 1,024) pass **16/18** and exit **3**. Restores are
three separate, unbudgeted `decay_restore:` buckets. The full 18-row table, original
run JSON, execution records, fixture/plan hashes, and reproduction commands are in
[the evidence directory](m3-plan-v2-evidence/README.md).

| Representative bucket | p50 ms | p95 ms | p99 ms | p95 budget ms |
| --- | ---: | ---: | ---: | ---: |
| `decay_edges:batch=1000` | 0.436–0.438 | **6.292–6.508** | 15.339–15.526 | 1 |
| `similar_nodes:cand=10000` | 24.822–24.824 | **25.073–25.078** | 25.299–25.954 | 10 |

Ranges span the two independent run summaries; they are not pooled percentiles.
Removing seed materialization from timed similarity and separating restore timings
did **not** eliminate either failure. The July numbers used different measurement
semantics and lack comparable provenance here, so this is not a controlled
regression or speedup comparison.

### Actual work and counter agreement

- Representative similarity labels **100 / 1,000 / 10,000** score **209 / 2,060 /
  10,453 vectors**, after probing 417 / 4,167 / 20,835 filtered nodes. Small scores
  16 / 125 / 513 vectors. DREY actual, naive actual, plan constants, and an independent
  fixture count agree. The first two representative targets miss the M0 §4.1 ±25%
  tolerance; the small fixture cannot supply the larger targets. These are nominal
  labels, not a calibrated 100/1k/10k sweep.
- Every representative decay label samples **1,848–84,323 matching edges**, with
  37,109 edges at the 95th percentile of retained work counts. The driver applies
  `Graph::decay_edges` to the whole selected edge type; `batch` is not a bound.
  F3 remains a workload-modeling gap even where the nominal budget passes.
- Repeated runs have identical reported counters. `runner::run` retains only the
  **last sample's counters** per bucket; this is not per-operation result validation.
  The independent decay distribution comes from the materialized plan and fixture.
  Similarity has three fixed candidate shapes and no vector/property mutations in
  this plan. Naive traversal measures a different operation and is not cross-compared.

The verifier and [analysis.json](m3-plan-v2-evidence/analysis.json) make these checks
reproducible. Candidate/batch calibration is follow-up harness work; changing what
plans measure requires another plan-version bump. Preserve this v2 baseline for
comparison and show actual counts in any subsequent publication (#19).

### Scope of this measurement

The timed path is `DreyDriver::run_op`, including its driver/counter overhead;
fixture loading and `prepare_op` are excluded. Similarity uses cosine, k=10, type/
property filters, no reachability filter, and explicit full-scan permission for
the sweep. These are resident graph operations, not the combined engine/Unix-socket
retrieval benchmark. Commit/open, memory and link-size gates are not measured here;
the latter resource fields remain null. No decoder-window or laptop guarantee follows.
The separate [local-engine evidence](local-engine-v1-evidence/README.md) measures a
different API, workload, and timing boundary.

## Historical result summary (2026-07-02; superseded)

`small` (1k nodes / 5k edges): **every budget met.** Representative p50s are all
sub-microsecond to low-microsecond; the crate is fast on the common path.

`representative` (50k / 250k, dim 1024): p50s remain tiny, but four op classes
exceed their p95 budget. All four are p95-*tail* overruns driven by the
degree-stratified workload deliberately including the mega-hub (spec §4.1,
"hub cost visible rather than averaged away") or by unbounded worst cases -
not by common-case regressions.

| Op | p50 | p95 | Budget (p95) | Verdict |
|----|-----|-----|--------------|---------|
| property_eq / property_range | ~2–15 µs | ~4–18 µs | 1 ms | pass |
| similar_nodes (filtered) | ~4.7 ms | ~5.3 ms | 10 ms | pass |
| update_edge_weight | 0.45 µs | 0.8 µs | 10 µs | pass |
| decay_edges batch=10k/100k | ~0.3 ms | ~2–7 ms | 10/100 ms | pass |
| neighbors | 0.64 µs | ~8.3 ms | 100 µs | **tail overrun** |
| traverse max_hops=2 | 0.55 µs | ~2.9 ms | 1 ms | **tail overrun** |
| shortest_path (hops/weighted) | ~0.8 µs | ~27–36 ms | 10 ms | **worst-case overrun** |
| decay_edges batch=1000 | ~0.3 ms | ~4 ms | 1 ms | **modeling gap** |

## Findings and recommended revisions

The original diagnoses below are retained with their disposition. Use the current
baseline above for measurements; July timings are historical context.

### F1. `shortest_path` has no step budget (API gap)

`shortest_path` runs BFS/Dijkstra with no bound on nodes explored. On a
disconnected or distant pair in a 50k-node component it explores much of the
component (p95 ~27–36 ms). The PRD §9.3 `ShortestPathOptions` list does not
include a step budget, unlike `TraversalOptions`. **Revision:** add an optional
`max_steps` / `max_cost` bound to `ShortestPathOptions` (post-v0.1), returning
`None` when the bound is hit, so worst-case latency is caller-bounded. Until
then the budget for `shortest_path` must reflect unbounded search on the
representative graph, i.e. widen to tens of ms, and the decision should be
explicit rather than implied.

**Status: implemented (v0.2).** `ShortestPathOptions` now carries an optional
`max_steps` node-expansion budget, enforced in both BFS and Dijkstra, returning
`None` when exhausted. See `docs/specs/shortest-path-bound.md`. (`max_cost` was
deemed a semantic, not latency, concern and left for later.)

### F2. Hub `neighbors` / `traverse` are O(degree) and the budget is
per-median, not per-hub

The mega-hub (top Zipf node) has a very high degree; listing or expanding it is
genuinely O(degree), so 1/3 of stratified samples (the hub stratum) blow a
budget derived from median behavior. This is the intended visibility of hub
cost, not a regression - the p50 (median/low strata) is sub-microsecond.
**Revision:** budgets should be recorded per degree stratum (low / median /
hub), so the gate compares like with like, rather than one budget across a
distribution the spec deliberately made bimodal. The crate itself needs no
change; the finding is about budget derivation (spec §4.3).

**Status: resolved differently (2026-07-03).** The audit (issue #5) found the
"mega-hub" was a generator defect, not intended stratification: Zipf degrees
were never truncated at `max_degree`, so the top node reached ~50× the
declared cap (out-degree ~49,855 vs 1,000). With `GENERATOR_VERSION` 2
enforcing the cap, hub-stratum samples pass their budgets outright (neighbors
p95 7 µs, traverse p95 ~200–290 µs, shortest_path p95 8–10 ms - see Addendum)
and the per-stratum budget split is moot. The original analysis conflated a
fixture bug with a design posture.

### F3. `decay_edges` batch size is not wired to work performed (harness gap)

`Graph::decay_edges(filter, factor)` decays *all* edges matching the filter;
there is no count limit, so the workload's nominal batch sizes (1k/10k/100k) do
not control the number of edges touched - batch=1000 with a hot edge-type
filter still decays every edge of that type. The three decay buckets therefore
measure filter-selectivity, not batch size, and the batch=1000 budget (1 ms) is
mismatched to the realized work. **Revision:** either (a) the harness should
size the filter to the intended batch and label buckets by realized
`edges_decayed` (already in counters), or (b) if consumers need count-bounded
decay, that is a v0.2 API question - not assumed now. No v0.1 crate change.

**Status: still open (2026-10-18).** Plan v2 re-measures p95 6.292–6.508 ms
against the nominal 1 ms budget. Actual edge counts vary by type, independently
of the batch label. Revision (a) remains the recommended follow-up: calibrate
filter populations or define explicitly workload-derived selectivity classes.
Increasing the threshold to fit the observed cost would not fix the model.

### F4. Selective similarity reduces scanned work (historical evidence)

The original measurement of unfiltered `similar_nodes` over the representative
embedding set (~25k × 1024 f32) was ~56 ms; a single node-type filter (~2k
candidates) reduced it to ~4.7 ms. This illustrates the cost of scoring fewer
candidates. The current configurable ceiling bounds probed nodes unless the
caller explicitly permits a full scan.

**Update (2026-10-18):** the current 209- and 2,060-vector shapes pass; the
10,453-vector shape exceeds its budget. The labels are not the actual counts.
Unfiltered similarity was not re-measured in #27; its historical latency must
not be reused as a current comparison. See F5.

### F5. 10k-candidate similarity misses its budget (new visibility, 2026-07-03)

The July measurement of `similar_nodes` with ~10k candidates at dim 1024 was
p50 16.7 ms / p95 18.8 ms against the 10 ms budget. This bucket was added during
the issue #5 remediation (PR #7), exposing a larger candidate population than
the earlier single-filter plan measured.

**Status: still open (2026-10-18).** The current 10,453-vector shape measures
p95 25.073–25.078 ms against the unchanged 10 ms budget.

**Revision options:** (a) revise tolerances only if a consumer workload justifies
them; measured scan cost is not itself a budget derivation; (b) treat this as the
PRD §13.2 ANN-seam trigger and evaluate an
ANN integration behind the now-extant `SimilarityEvaluator` seam
(`docs/specs/persistence-seam-decision.md`); or (c) use narrower candidate sets
where the required results permit it, per the §13.1 filters-first posture.
The current runs do not establish a safe 5k-candidate ceiling. Profiling can also
identify exact-scan improvements before changing the evaluator. A real captured
workload (spec §6) should decide which; do not integrate ANN on synthetic
evidence alone.

## Historical addendum - 2026-07-03 re-measurement (superseded)

Re-measured after the issue #5 audit remediation (PRs #6–#15), against a
regenerated seed-42 `representative`/`medium` fixture. Measurement context
changed in three audit-driven ways: `GENERATOR_VERSION` 2 truncates Zipf
degrees at the declared `max_degree`; the workload is materialized as data
(spec §3.6) with selectivity classes, parameter classes, and the 100/1k/10k
candidate sweep actually exercised; degree strata draw varied nodes instead
of three fixed ids (so p50s now reflect drawn cases, not cache-hot
repetition). Release build, `measurement` plan, 1000 samples/bucket, host
fingerprint in the run JSON.

| Op | p50 | p95 | Budget (p95) | Verdict |
|----|-----|-----|--------------|---------|
| property_eq / property_range (3 selectivity classes each) | 0.8–15 µs | 1–20 µs | 1 ms | pass |
| update_edge_weight | 1.7 µs | 2.1 µs | 10 µs | pass |
| neighbors | 2.7 µs | 7.2 µs | 100 µs | pass (F2 resolved) |
| traverse max_hops=2 / 5 | 11 / 190 µs | 201 / 289 µs | 1 / 10 ms | pass (F2 resolved) |
| shortest_path hops / weighted | 6.6 / 7.9 ms | 8.2 / 9.9 ms | 10 ms | pass (F1+F2 resolved) |
| similar_nodes cand=100 / 1k | 0.5 / 3.4 ms | 0.6 / 3.9 ms | 10 ms | pass |
| similar_nodes cand=10k | 16.7 ms | 18.8 ms | 10 ms | **overrun (F5)** |
| decay_edges batch=10k / 100k | ~0.5 ms | ~5.3 ms | 10 / 100 ms | pass |
| decay_edges batch=1000 | 0.6 ms | 5.3 ms | 1 ms | **overrun (F3)** |

Gate result: 16 of 18 buckets pass; exits non-zero on the two flagged
overruns. Note `shortest_path:weighted` p99 (11.5 ms) exceeds the p95 budget
figure - the gate compares at p95; a future budget pass that gates p99 should
set p99 budgets explicitly.

## Disposition

F1 is implemented; F2 was resolved by fixing the fixture. The current smaller
similarity shapes support the filters-first posture, with no claim that every
filtered query fits the budget. F3 and candidate-target calibration remain
measurement-model gaps; F5 remains a measured cost requiring workload-guided
evaluation. They do not establish incorrect query results or select an ANN backend.

Issue #27 records the corrected baseline and keeps the existing budgets. Its
completion does not resolve F3/F5 or certify all PRD §16.3 gates. The PRD §21 M3
written-reason path retains those findings for follow-up; three-scale README
publication remains #19. The earlier `steps()` adjacency-scan fix remains in place.
