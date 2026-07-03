# M3 Budget-Gate Findings

Status: v0.1, 2026-07-02; **updated 2026-07-03** after the issue #5 audit
remediation (see Addendum). This is the PRD §16.3 M3 deliverable in its
"written reason to revise" form: the budget gate is met at `small` scale and
surfaces specific, explainable overruns at `representative` scale. Numbers
below are from the `bench` harness (release build) against the seed-42
`medium`-fanout fixtures on the recorded host fingerprint; they are synthetic
budgets and provisional measurements, superseded by a real capture (spec §6).

> **2026-07-03:** the original result summary below is superseded by the
> re-measurement in the Addendum. The F2 tail overruns turned out to be a
> fixture artifact (unenforced `max_degree`), not hub cost; with the fixed
> generator, `neighbors`/`traverse`/`shortest_path` all pass. Two buckets
> still fail: `decay_edges batch=1000` (F3, unchanged) and
> `similar_nodes cand=10000` (F5, new visibility from the audit-mandated
> candidate sweep).

## Result summary

`small` (1k nodes / 5k edges): **every budget met.** Representative p50s are all
sub-microsecond to low-microsecond; the crate is fast on the common path.

`representative` (50k / 250k, dim 1024): p50s remain tiny, but four op classes
exceed their p95 budget. All four are p95-*tail* overruns driven by the
degree-stratified workload deliberately including the mega-hub (spec §4.1,
"hub cost visible rather than averaged away") or by unbounded worst cases —
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
cost, not a regression — the p50 (median/low strata) is sub-microsecond.
**Revision:** budgets should be recorded per degree stratum (low / median /
hub), so the gate compares like with like, rather than one budget across a
distribution the spec deliberately made bimodal. The crate itself needs no
change; the finding is about budget derivation (spec §4.3).

**Status: resolved differently (2026-07-03).** The audit (issue #5) found the
"mega-hub" was a generator defect, not intended stratification: Zipf degrees
were never truncated at `max_degree`, so the top node reached ~50× the
declared cap (out-degree ~49,855 vs 1,000). With `GENERATOR_VERSION` 2
enforcing the cap, hub-stratum samples pass their budgets outright (neighbors
p95 7 µs, traverse p95 ~200–290 µs, shortest_path p95 8–10 ms — see Addendum)
and the per-stratum budget split is moot. The original analysis conflated a
fixture bug with a design posture.

### F3. `decay_edges` batch size is not wired to work performed (harness gap)

`Graph::decay_edges(filter, factor)` decays *all* edges matching the filter;
there is no count limit, so the workload's nominal batch sizes (1k/10k/100k) do
not control the number of edges touched — batch=1000 with a hot edge-type
filter still decays every edge of that type. The three decay buckets therefore
measure filter-selectivity, not batch size, and the batch=1000 budget (1 ms) is
mismatched to the realized work. **Revision:** either (a) the harness should
size the filter to the intended batch and label buckets by realized
`edges_decayed` (already in counters), or (b) if consumers need count-bounded
decay, that is a v0.2 API question — not assumed now. No v0.1 crate change.

**Status: still open (2026-07-03).** Re-measured p95 ~5.3 ms vs the 1 ms
batch=1000 budget; all three batch buckets show the same ~5.3 ms p95,
confirming the diagnosis (realized work is filter-selectivity-bound, not
batch-bound). Revision (a) remains the recommended fix for the next budget
pass.

### F4. Filtered similarity is on-budget; unfiltered is not (confirms §13)

An unfiltered `similar_nodes` over the representative embedding set (~25k × 1024
f32) is ~56 ms; composing a single node-type filter (~2k candidates) brings it
to ~4.7 ms, under budget. This confirms the PRD §13.1 posture: exhaustive scan
is correct *because* filters run first, and the config scan ceiling is what
stops an accidental unfiltered sweep. No change; recorded as validation.

**Update (2026-07-03):** the audit-mandated candidate sweep gives this finer
resolution: 100- and 1k-candidate scans pass comfortably; the 10k-candidate
case exceeds its budget. See F5.

### F5. 10k-candidate similarity misses its budget (new visibility, 2026-07-03)

`similar_nodes` with ~10k candidates at dim 1024 measures p50 16.7 ms / p95
18.8 ms against the 10 ms budget. This bucket did not exist before the audit —
the spec §4.2 100/1k/10k candidate sweep was added to the harness during the
issue #5 remediation (PR #7), so this is the corrected workload exposing a
real cost the old single-filter plan masked, not a regression: exhaustive scan
is linear in candidates, and ~10k × 1024 f32 costs what it costs.
**Revision options:** (a) revise the 10k-candidate budget to reflect linear
scan cost (consistent with "budgets, not comparisons" — the budget was
synthetic); (b) treat this as the PRD §13.2 ANN-seam trigger and evaluate an
ANN integration behind the now-extant `SimilarityEvaluator` seam
(`docs/specs/persistence-seam-decision.md`); or (c) constrain consumers to ≤~5k
candidates via filters, per the §13.1 filters-first posture. A real captured
workload (spec §6) should decide which; do not integrate ANN on synthetic
evidence alone.

## Addendum — 2026-07-03 re-measurement (post-audit remediation)

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
figure — the gate compares at p95; a future budget pass that gates p99 should
set p99 budgets explicitly.

## Disposition

None of F1–F5 is a correctness defect. F1 is implemented (v0.2); F2 dissolved
with the fixture fix (the overrun was a generator artifact); F4 is confirmed
posture. The two live items for the next budget pass are F3 (size decay
filters to the intended batch, or revise the small-batch budget) and F5
(revise the 10k-candidate budget, or treat it as the §13.2 ANN-seam trigger —
decide on a real captured workload, not synthetic budgets). The one
crate-level defect found during M3 — `steps()` scanning the whole adjacency
index — was fixed in the same milestone (nested `node → edge_type → [edge]`
adjacency, O(degree) lookups). M3 exits on the §16.3 "written reason to
revise" clause, with F3 and F5 as the concrete revisions carried forward.
