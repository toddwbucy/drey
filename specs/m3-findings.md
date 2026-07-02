# M3 Budget-Gate Findings

Status: v0.1, 2026-07-02. This is the PRD §16.3 M3 deliverable in its
"written reason to revise" form: the budget gate is met at `small` scale and
surfaces specific, explainable overruns at `representative` scale. Numbers
below are from the `bench` harness (release build) against the seed-42
`medium`-fanout fixtures on the recorded host fingerprint; they are synthetic
budgets and provisional measurements, superseded by a real capture (spec §6).

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
`None` when exhausted. See `specs/shortest-path-bound.md`. (`max_cost` was
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

### F4. Filtered similarity is on-budget; unfiltered is not (confirms §13)

An unfiltered `similar_nodes` over the representative embedding set (~25k × 1024
f32) is ~56 ms; composing a single node-type filter (~2k candidates) brings it
to ~4.7 ms, under budget. This confirms the PRD §13.1 posture: exhaustive scan
is correct *because* filters run first, and the config scan ceiling is what
stops an accidental unfiltered sweep. No change; recorded as validation.

## Disposition

None of F1–F4 is a correctness defect; all four are either budget-derivation
refinements (F2), harness-modeling refinements (F3), a confirmed design posture
(F4), or a genuinely deferred API bound (F1). The one crate-level defect found
during M3 — `steps()` scanning the whole adjacency index — was fixed in the
same milestone (nested `node → edge_type → [edge]` adjacency, O(degree)
lookups). M3 exits on the §16.3 "written reason to revise" clause, with F1 and
F2 as the concrete revisions for the next budget pass.
