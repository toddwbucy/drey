# M0 Spec: Reference Fixture and Measurement Harness

Status: draft v0.3, 2026-07-02. Implements PRD §16.2 (budget derivation),
§16.4 (budget table), §16.5 (measurement dimensions), and §21 M0. The PRD
governs on any conflict. Where this spec makes a call the PRD left open, the
call is marked **[spec decision]** with rationale.

Changes from v0.2: closed the plan-composition gap surfaced while reviewing a
trial implementation — §4.2 now constrains a plan to satisfy the §5.2
per-bucket sample minima, and names instance counts for the batch-shaped ops
(§4.2.1); added a generator/plan self-check requirement so
distribution-and-count violations fail at generation time, not at run time
(§3.7).

Changes from v0.1: incorporated the implementation-risk audit (Bucy,
2026-07-02; checked in as `m0-implementation-checklist.md`). `edges` made a
derived parameter, never independent. Hostile embedding components injected
after normalization, unit norm downgraded to approximate. Canonical JSON
defined precisely. Traversal ops unified under one `traverse` name with
`max_hops` as a parameter class. Commit cadence placed outside mix
percentages. Candidate-set sweep given a ±25% tolerance. Percentile method
fixed as nearest-rank. Result rows gain a `status` field and a throughput
budget field. `WeightUpdate` bounds semantics and public ordering guarantees
named as API-contract dependencies with M0 stopgaps.

## 1. Purpose

M0 delivers three things: a representative graph fixture, a workload model
with derived budgets, and a measurement harness that runs repeatably against
the fixture and emits JSON. This spec defines all three concretely enough to
implement. Per PRD §16.2, the fixture is **synthetic-first**: the reference
consumer capture is preferred but development never blocks on it. Every
number below is therefore marked provisional and is superseded, parameter by
parameter, the day a real capture arrives (§7).

## 2. What this spec decides, and what it leaves alone

Decided here: fixture parameters and size classes, the generator's
determinism contract, the fixture file format, the workload operation set and
mixes, provisional budget numbers, harness architecture, and the JSON output
schema.

Not decided here, per the PRD: the persistence design (§10.3), the binary
codec (§10.1.1), weight precision (open question 2), and anything about
`drey`'s internal data structures. The fixture format below is **test
apparatus only** — it is not the crate's persistence format and must never be
promoted into one.

## 3. Fixture

### 3.1 Source and identity

A fixture is a directory identified by a manifest. `source` is `synthetic`
or `captured`. Synthetic fixtures are fully reproducible from `(generator
version, seed, parameters)`; the manifest records all three so the gap
between synthetic and captured stays visible (PRD §16.2).

### 3.2 Parameters

The generator takes these parameters, all recorded in the manifest:

| Parameter | Meaning |
|-----------|---------|
| `nodes` | node count |
| `edges` | edge count — **derived**, always `nodes × mean out-degree of the fanout class`, recorded in the manifest but never set independently |
| `node_types` | node type cardinality |
| `edge_types` | edge type cardinality |
| `fanout` | mean out-degree class: `low` / `medium` / `high` (see 3.3) |
| `degree_dist` | out-degree distribution: Zipf with exponent `s`, truncated at `max_degree` |
| `embed_dim` | embedding dimensionality |
| `embed_coverage` | fraction of nodes carrying an embedding |
| `seed` | u64 RNG seed |

**[spec decision]** Degree distribution is truncated Zipf (default `s = 1.2`,
`max_degree = 1000`) over a preferential-attachment wiring pass. Rationale:
agent knowledge graphs are hub-heavy, and §16.5 requires fanout to be a
swept dimension; a uniform random graph would understate traversal cost on
hubs.

Type labels are opaque (`nt_00`…, `et_00`…). **[spec decision]** Neutral
labels keep consumer semantics out of the apparatus, mirroring the crate's
no-reserved-names rule (PRD §23.7). Edge types are assigned to edges by a
Zipf draw as well, so type-filter selectivity varies (§16.5).

Every node carries three properties chosen to exercise the required scalar
index (PRD §8) in both predicate shapes:

- `p_seq: I64` — unique, monotonically assigned. Serves range queries and
  stands in for the consumer-side recency sequence (PRD §7.4).
- `p_cat: String` — categorical, drawn from a pool sized to give equality
  selectivity classes of roughly 0.1%, 1%, and 10% of a type's population.
- `p_score: F64` — uniform in [0, 1], for range selectivity sweeps.

Edges carry `weight: f32` uniform in (0, 1] and one property (`p_seq: I64`).
Embeddings are random f32 vectors, approximately unit-normalized. After
normalization, a deliberate 1% of components are overwritten with
decimal-hostile values (denormals, and values whose shortest round-trip
decimal rendering is long), so byte-exact round-trip assertions (PRD §10.2)
have something to catch at M2. Injection happens after normalization so the
hostile bit patterns reach the sidecar verbatim; the unit norm is therefore
approximate, and nothing depends on it — the fixture property that matters is
byte-exactness, not the norm.

### 3.3 Size classes

Three classes per §16.5, spanning the §4.1 profile. All provisional:

| Class | Nodes | Edges | Node/edge types | Embed dim | Coverage |
|-------|-------|-------|-----------------|-----------|----------|
| `small` | 1,000 | 5,000 | 4 / 8 | 256 | 50% |
| `representative` | 50,000 | 250,000 | 12 / 24 | 1,024 | 50% |
| `stress` | 500,000 | 2,500,000 | 24 / 48 | 1,024 | 50% |

Fanout classes: `low` mean out-degree 2, `medium` 5 (default at all sizes),
`high` 25. The edge counts in the size table are the `medium` derivation
(`nodes × 5`); a fanout sweep at `representative` therefore implies 100,000
edges (`low`) and 1,250,000 edges (`high`). Dimensionality sweep: {256, 1024,
2048} at `representative` size for the similarity rows only.

**[spec decision]** `representative` is sized to an agent's local working
graph (tens of thousands of memories/beliefs/artifacts), not a corpus-scale
graph. The memory-primary ceiling (PRD §23.4) is probed by `stress`, whose
raw embedding payload alone is ~1 GB at dim 1024.

### 3.4 Generation and determinism

Single seeded PRNG (splitmix64-seeded xoshiro or equivalent); one seed in the
manifest reproduces the fixture byte-for-byte at a given generator version.
Generator version bumps whenever output for the same `(seed, params)` would
change. IDs are assigned densely from 0 in generation order — but the fixture
format still stores every ID explicitly, because consumers of the fixture
(the harness, M2 round-trip tests) must never infer identity from position,
mirroring PRD §7.4.

### 3.5 Fixture format

A fixture directory contains:

- `manifest.json` — generator version, source, seed, parameters, counts,
  and a content checksum per data file.
- `nodes.jsonl` — one object per node: `{"id", "type", "props"}`.
- `edges.jsonl` — one object per edge: `{"id", "from", "to", "type",
  "weight", "props"}`.
- `embeddings.bin` — binary sidecar: repeated records of
  `(node_id: u64 LE, dim: u32 LE, dim × f32 LE)`.

**[spec decision]** JSONL for structure (inspectable, diffable, per the
readability-before-cleverness principle) with a binary sidecar for vectors
(byte-exact f32 by construction; JSON float text would launder exactly the
precision drift the round-trip tests must detect). Weight is the one f32
carried in JSON.

Canonical JSON, defined so byte-for-byte determinism is checkable: UTF-8, one
complete object per line, LF line endings, every file ends with a final LF,
top-level fields serialized in exactly the order shown above, no whitespace
beyond the JSON separators serde_json emits in compact mode, floats rendered as
shortest round-trip decimal (Ryu — serde_json's default), integers rendered
without exponent or leading zeros. **Nested object keys are sorted recursively**
— every nested object (the `props` map in `nodes.jsonl`/`edges.jsonl`, and
`parameters` and any sub-object in `manifest.json`) serializes its keys in
ascending byte order at every depth, so two runs cannot disagree on key order.
(The implementation gets this for free: `props` is a sorted map and the manifest
uses `BTreeMap` sub-objects.) Records are written in ascending ID order.
Checksum generation and verification operate on exactly this recursively-sorted
canonical byte representation.

### 3.6 Workload plan file

The workload is data, not code: `workload.jsonl`, generated with the fixture
from the same seed, one operation per line in execution order (see §4). This
makes a run's operation sequence reproducible and lets a captured consumer
trace drop into the same file format later.

### 3.7 Generator and plan self-checks

Generation asserts its own contract before writing, so a violation fails at
generation time rather than surfacing as a wrong number in a benchmark. The
generator, after building a fixture, verifies: the edge count equals the
derived `nodes × fanout-mean` exactly (not merely close); the realized mean
out-degree is within tolerance of the fanout class; per-node-type `p_cat`
selectivity classes land within tolerance of their 0.1% / 1% / 10% targets;
embedding coverage matches `embed_coverage`; and every emitted data file
re-reads to the same in-memory structure it was written from (a
generate→serialize→parse round-trip, which also proves the reader agrees with
the writer on canonical bytes). The plan generator verifies that every
exercised (op class × parameter class) bucket meets the §4.2.1 sample floor.
Any failure aborts generation with a diagnostic; a fixture or plan that fails
its self-check is never written to disk. These checks are cheap relative to
generation and exist because the failure modes they catch —
truncated-exponent distributions, off-by-one counts, reader/writer format
drift, starved buckets — are exactly the ones that produce a plausible-looking
but wrong result downstream.

## 4. Workload model

### 4.1 Operation set

One row per budget-table row (PRD §16.4), plus the property query, which the
PRD lists as a hot-path shape in §2:

| Op | Parameters drawn per instance |
|----|-------------------------------|
| `neighbors` | start node (degree-stratified), direction, edge-type filter (0–2 types), optional `min_weight` |
| `traverse` | as neighbors, plus `max_hops` ∈ {2, 5} and `max_paths` 1000. One op name everywhere — workload file, driver dispatch, results, budget lookup — with `max_hops` as a parameter class, never `traverse_2`-style aliases |
| `shortest_path` | node pair (mix of connected and disconnected), unweighted and weighted cost modes |
| `property_eq` / `property_range` | `(node_type, key, predicate)` across the three selectivity classes |
| `similar_nodes` | query vector, k=10, composed with a type filter and one property filter; candidate sets swept at targets 100 / 1k / 10k, generated by filter-selectivity choice to within ±25% of target, actual candidate count recorded in counters |
| `update_edge_weight` | edge id, op drawn from Set/Add/Multiply, bounds present 50% of the time |
| `decay_edges` | edge-type filter, factor 0.9 (each instance paired with a reciprocal-factor restore of the same type on the maker's next call, so plan-length weight collapse cannot degrade later reads), batch sizes 1k / 10k / 100k |
| `commit` | inserted into the plan after every 1,000 mutations as plan structure — commits do not count toward the §4.2 mix percentages (n/a until M2) |
| `open` | cold start from disk (n/a until M2) |

Start nodes are degree-stratified (low/median/hub) so hub cost is visible
rather than averaged away.

Two semantics this spec depends on but does not own, both to be pinned in the
API-contract spec: (a) `WeightUpdate` bounds — the M0 stopgap is the plain
PRD §9.2 reading, apply the op then clamp the result into `[min, max]`, and
`NaiveDriver` implements exactly that; (b) public result-ordering guarantees —
until they exist, every driver must be *internally* deterministic (stable
neighbor and traversal order), because with `max_paths` truncation the set of
returned paths is order-dependent and counter repeatability (§5.4) silently
requires it.

### 4.2 Mixes

Four mixes per §16.5, expressed as op-class percentages over a 100k-op plan:

| Mix | Reads (traversal/property) | Similarity | Mutations |
|-----|---------------------------|------------|-----------|
| `traversal_heavy` | 80% | 10% | 10% |
| `similarity_heavy` | 30% | 60% | 10% |
| `update_heavy` | 20% | 5% | 75% |
| `mixed` | 50% | 25% | 25% |

**[spec decision]** Percentages are provisional stand-ins for the reference
agent's decision-point/consolidation rhythm; a capture replaces them with
observed rates.

### 4.2.1 Plan composition constraint

The mix percentages fix the op-class *shares*, but a plan is only valid if it
also delivers the §5.2 measurement floor: every (op class × parameter class)
bucket that the mix exercises must contain at least the minimum retained
sample count (≥ 1,000 at `small`/`representative`, ≥ 100 at `stress`), plus
the 100-sample warmup discard. A 100k-op plan is comfortably large enough for
the light per-instance ops, but the parameter-class fan-out is real: `traverse`
alone splits across `max_hops` ∈ {2, 5} × fanout classes, and each split needs
its own floor. The plan generator therefore allocates each op class's share
across its parameter classes so no exercised bucket falls below the floor,
rather than sampling parameters i.i.d. and hoping the tail buckets fill.

The batch-shaped ops are the sharp case, because one plan entry is many
underlying edges. `decay_edges` at batch 100k moves 100k edges per op, so the
`representative` fixture (250k edges) admits only a couple of disjoint
full-size batches; the ≥ 1,000-sample floor is met by resampling the same
batch class against a graph reset between samples, and the plan states, per
batch-shaped op, how many instances it carries and at which batch sizes rather
than leaving the count implicit. **[spec decision]** Batch-op instance counts
per 100k-op plan: `decay_edges` 1k / 10k / 100k batches each appear as their
own parameter class; the plan carries enough instances of each to reach the
sample floor, drawing fresh edge-type filters per instance, and the harness
resets mutated state between samples so each is measured from the same
baseline (state reset is a harness concern, not a fixture regeneration).

### 4.3 Budget derivation

Budgets come from the operation's *position* in the consuming process (PRD
§16.1), not from what an implementation happens to achieve. Positions and
provisional budgets, all **synthetic — superseded by capture**:

**Decision-point ops** (block an agent turn): the working tolerance is a few
milliseconds per graph consultation so the graph is never the turn's
bottleneck.

**Consolidation ops** (run in a maintenance window): throughput-shaped; the
window is seconds, not milliseconds.

**Process-start ops**: open time is part of agent cold start; tolerance ~1 s
at representative size.

### 4.4 Budget table (instance of PRD §16.4)

At `representative` size, `medium` fanout, dim 1024, unless noted. Source:
synthetic. p50/p95/p99 cells stay TBD until M1/M3 measurement; the Budget
column is the M0 deliverable.

| Operation | p50 | p95 | p99 | Budget (p95) | Source |
|-----------|-----|-----|-----|--------------|--------|
| neighbors | TBD | TBD | TBD | ≤ 100 µs | synthetic |
| traverse max_hops=2 | TBD | TBD | TBD | ≤ 1 ms | synthetic |
| traverse max_hops=5 | TBD | TBD | TBD | ≤ 10 ms | synthetic |
| shortest_path | TBD | TBD | TBD | ≤ 10 ms | synthetic |
| property_eq / property_range | TBD | TBD | TBD | ≤ 1 ms | synthetic |
| similar_nodes filtered (≤10k candidates) | TBD | TBD | TBD | ≤ 10 ms | synthetic |
| update_edge_weight | TBD | TBD | TBD | ≤ 10 µs; ≥ 100k updates/s sustained | synthetic |
| decay_edges batch 100k | TBD | TBD | TBD | ≤ 100 ms | synthetic |
| commit (post-batch, durability level per M2) | TBD | TBD | TBD | ≤ 100 ms | synthetic |
| open (representative) | TBD | TBD | TBD | ≤ 1 s | synthetic |

Standing resource budgets (PRD §16.3 gates 4–5):

- Resident memory ≤ 3× raw fixture payload (nodes + edges + properties +
  embeddings), and absolutely ≤ 1.5 GB at `representative`.
- Link-size addition to a consuming binary: recorded every M3 run; review
  trigger at 5 MB.
- No daemon, no listener: verified by inspection, recorded in harness output.

## 5. Harness

### 5.1 Architecture

The harness lives in a companion crate (`harness/`) in this repo. It is never
a dependency of `drey`; `drey` stays a single publishable crate with a clean
tree (SQLite-class weight, PRD §6.3).

The harness drives a small trait mirroring the §4.1 operation set:

```rust
pub trait GraphDriver {
    fn load_fixture(&mut self, dir: &Path) -> Result<LoadStats>;
    fn run_op(&mut self, op: &WorkloadOp) -> Result<OpOutcome>;
}
```

Two implementations: at M0 a `NaiveDriver` (plain HashMaps and linear scans),
which exists solely to prove the harness mechanics end-to-end and is
**throwaway apparatus** — its numbers are never a comparison baseline and are
labeled `driver: "naive"` in output. At M1 a `DreyDriver` wraps the real
crate. `commit`/`open` rows report `"status": "n/a"` until M2.

`OpOutcome` carries result cardinality (paths returned, candidates scanned,
edges decayed) so correctness spot-checks and PRD §15 counters ride along
with timing.

### 5.2 Measurement methodology

Single-threaded, matching the single-writer model (PRD §11). Monotonic clock
per op. Per (op class × parameter class): ≥ 1,000 samples after a 100-sample
discarded warmup, at `stress` size ≥ 100 samples. Percentiles computed from
the full retained sample set by nearest-rank — the `ceil(p × n)`th smallest
retained sample — deterministic, no interpolation, no histogram bucketing at
this scale. Each run
records host fingerprint (CPU model, core count, RAM, OS) — budgets are
laptop-class per PRD §4.1, and results are only comparable within a
fingerprint. Cold-start rows run in a fresh process per sample.

### 5.3 JSON output schema

One JSON document per run:

```json
{
  "harness_version": "0.1.0",
  "driver": "naive | drey@<version>",
  "fixture": { "manifest": { }, "checksum_verified": true },
  "host": { "cpu": "", "cores": 0, "ram_gb": 0, "os": "" },
  "run": { "mix": "mixed", "ops_total": 100000, "started_at": "" },
  "results": [
    {
      "op": "traverse", "params": { "max_hops": 2, "fanout": "medium" },
      "status": "ok",
      "samples": 1000,
      "p50_us": 0.0, "p95_us": 0.0, "p99_us": 0.0,
      "budget_us": 1000.0, "budget_throughput_per_s": null,
      "budget_source": "synthetic",
      "pass": null,
      "counters": { "steps_visited": 0, "paths_returned": 0 }
    }
  ],
  "resources": { "resident_bytes": 0, "raw_payload_bytes": 0,
                 "link_size_bytes": null }
}
```

`status` is `"ok"`, `"n/a"` (the row exists but is not yet measurable —
`commit` and `open` until M2, with percentile fields null), or `"error"`.
`budget_throughput_per_s` carries the throughput half of a dual budget
(`update_edge_weight`'s ≥ 100k updates/s) and is null for latency-only rows;
where both halves exist, `pass` requires both.

`pass` is `null` until a real driver runs (M1+), and stays `null` for every
`NaiveDriver` row regardless of timings — a throwaway driver can neither pass
nor fail a budget. The M3 gate is this same document with `pass` populated
against p95 — the budget table and the pass/fail record are one artifact, per
PRD §16.4.

### 5.4 Repeatability

Same fixture + same workload plan + same driver + same host fingerprint must
produce the same counters exactly and percentile timings within run-to-run
noise. Counters mismatching across runs is a correctness failure, not noise.

## 6. Capture path and supersession

When the reference consumer capture happens (PRD §16.2 preferred path), it
produces the same artifacts: a fixture directory with `source: "captured"`
(via a deliberately throwaway export script, PRD §3.2.7 — never promoted to a
migration tool) and a `workload.jsonl` from observed operations with observed
rates. Supersession is per-artifact: a captured fixture can replace the
synthetic one while synthetic budgets remain, or captured tolerances can
replace budget numbers against the synthetic fixture. The manifest's `source`
field keeps every mixture honest.

## 7. M0 exit checklist (PRD §21)

- [ ] Generator produces all three size classes deterministically from
      recorded seeds; manifests and checksums written; §3.7 self-checks pass.
- [ ] Workload plans generated for all four mixes, each satisfying the §4.2.1
      per-bucket sample floor.
- [ ] Budgets written down with numbers (§4.4), each marked synthetic.
- [ ] Harness runs a full mix against the `small` and `representative`
      fixtures via `NaiveDriver`, emits schema-valid JSON, and repeats with
      identical counters.
