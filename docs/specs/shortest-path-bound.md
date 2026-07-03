# Spec: Bounded `shortest_path` (M3 finding F1)

Status: v0.2, 2026-07-02. Implemented — the first v0.2 change. Realizes the
revision recommended by `m3-findings.md` F1, against PRD §9.3 (`shortest_path` /
`ShortestPathOptions`). The PRD governs on any conflict.

## Problem (from M3)

`shortest_path` runs BFS (unweighted) or Dijkstra (weighted) with **no bound on
how much of the graph it explores**. On a disconnected or distant pair in a
large component it can walk the whole component before answering — the M3 budget
gate measured p95 ~27–36 ms at the `representative` fixture (50k nodes) against a
10 ms budget, driven entirely by that worst case. `TraversalOptions` already
carries `max_hops`; `ShortestPathOptions` had no analogous bound, so a caller
could not cap the cost.

## Decision

Add an **optional exploration budget** to `ShortestPathOptions`:

```rust
pub struct ShortestPathOptions {
    // … existing fields …
    /// Maximum number of nodes the search may expand before giving up and
    /// returning `None`. `None` (default) is unbounded.
    pub max_steps: Option<usize>,
}
```

**Semantics.** `max_steps` bounds the number of nodes the search *expands* (pops
from the frontier and processes), not path length or edge count. When the search
expands more than `max_steps` nodes without reaching the target, it returns
`Ok(None)` — the same shape as "no path exists". A caller that must distinguish
"no path" from "budget exhausted" raises the budget or omits it.

**Why a step (node-expansion) budget, not a hop or cost bound:**
- It directly bounds *worst-case latency* — the actual F1 concern — because
  work is O(expansions + incident edges), and the expansion count is the term
  that blows up on a hub-heavy component.
- It applies uniformly to **both** cost modes: unweighted BFS and weighted
  Dijkstra each expand one node per loop iteration.
- A hop bound would not bound Dijkstra work, and a pure `max_hops` would
  duplicate/confuse the `TraversalOptions` knob. A `max_cost` (weighted-only)
  bound is a *semantic* constraint, not a latency one; it is left as a possible
  future addition, not part of this change.

**Determinism.** Expansion order is already deterministic (BFS by insertion
order; Dijkstra by `(cost, node-id)` tie-break), so a bounded search returns the
same result on every run — the `None`-on-exhaustion outcome is reproducible.

**Counting rule.** A step is a *real* expansion:
- BFS: every node dequeued that is not the target.
- Dijkstra: every node popped whose recorded distance is still current; stale
  heap entries (superseded by a cheaper path) are skipped and **not** counted,
  so the budget measures useful work, not heap churn.

The target node itself is returned before consuming a step, so a reachable
target within budget is always found.

**Default and compatibility.** `max_steps` defaults to `None` (unbounded),
preserving the existing behavior and the passing M1/M2 tests. Unlike traversal's
`MAX_TRAVERSAL_HOPS` clamp, there is no forced cap here: `shortest_path` is
iterative (no recursion), so there is no stack-safety reason to bound it, and the
latency bound is the caller's to opt into.

## Test

`shortest_path_respects_step_budget`: on a chain `a → b → c → d`, a search from
`a` to `d` with `max_steps: Some(1)` returns `None` (cannot reach `d` within one
expansion), while the same search unbounded, and with a sufficient budget, finds
the path. Confirms both the bound and that it does not perturb within-budget
results.

## Budget note

With this in place, a consumer running `shortest_path` at a decision point sets
`max_steps` to keep worst-case latency inside its budget, trading completeness
(a `None` when the path is beyond the budget) for a bounded response — which is
the correct posture for a hot-path graph consultation (PRD §16.1).

**Residual (separate from F1).** `max_steps` bounds the *number* of expansions,
not the cost of any single one. On the synthetic `representative` fixture the top
Zipf node has ~90k incident edges (it is an endpoint of ~18% of all edges), so
one expansion of it is inherently expensive: the search must examine all ~90k
edges and enqueue all ~90k neighbors. `max_steps` cannot bound that — it caps
expansions, and a single hub expansion is one expansion. So `shortest_path`
passes at `small` scale (~0.3 ms) but not `representative` (~16–26 ms p95).

Dropping the redundant per-call sort in `steps()` (this change — the
`(edge_type, edge_id)` iteration order is already deterministic) removes an
`O(degree·log degree)` term and is worth keeping, but it is **not** the fix: the
harness showed it moves the hub-bound p95 only marginally, because the dominant
cost is the linear edge/neighbor processing, not the sort. Two real mitigations,
both out of scope here and tracked for later:
- A **work budget** that counts *edges examined* (not nodes expanded) and returns
  `None` when exceeded — this would bound latency even through a hub, at the cost
  of a different bound semantics than `max_steps`.
- A **less concentrated fixture / a captured workload** — an 18%-on-one-node Zipf
  is more extreme than a real agent graph is likely to be (M3 F2 and the
  provisional-budget caveat). The right budget for hub-touching search is a
  question the captured workload answers, not the synthetic one.
