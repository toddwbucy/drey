# Decision: persistence + similarity-evaluation seams (design commitment 6)

Status: **ratified + implemented**, 2026-07-03. Addresses the last open
finding from audit #5 (`persist/mod.rs:57`): *"No persistence trait and no
internal similarity-evaluation seam, both named deliverables of the design
commitments and M2/§13.2."* This was a **design decision**: the crate's existing
behavior was retained while introducing the *swappable-internals boundary*
the PRD promises. The pre-implementation state and proposal are recorded below.

**Performance correction, 2026-10-18 (#27):** the [plan-version-2
re-baseline](m3-findings.md#current-baseline)
passes 16/18 representative latency/throughput buckets. The decay modeling gap
and large filtered-similarity overrun remain open. They justify further
measurement; they do not select ANN or a different persistence backend. The
design history below retains the decision to ship the seams and defer alternatives.

## 1. What the commitments require

- **Design commitment 6** (CLAUDE.md / PRD §6.6): *"Public API sits above a
  persistence trait and an internal similarity-evaluation seam; both internals
  are swappable without API change."*
- **PRD §13.2**: *"Similarity evaluation sits behind an internal trait so an ANN
  structure can replace the scan without API [change]."*
- **PRD §13.1 / closed decision**: *"No ANN index in v0.1 — similarity is bounded
  exhaustive scan behind an internal trait seam; an existing ANN crate may be
  integrated later only if the M3 budget gate fails."*

Two seams, one shape: an internal trait with exactly one implementation today,
so a second implementation can drop in later **without touching the public API**.

## 2. State before the seam implementation (2026-07-03)

Neither seam exists as a trait; both internals are concrete.

**Persistence.** `Graph` holds `persist: Option<crate::persist::Persister>`
(`graph.rs:23`), where `Persister` is a concrete WAL+snapshot struct. The hot
path is two calls:
- `Graph::log(mutation)` → `p.append(&mutation)` (`graph.rs:322`)
- `Graph::commit()` → `p.commit()` (`graph.rs:263`)
Lifecycle (`create`/`open`/`snapshot`/`export`/`import`) lives as `Graph`
methods in `persist/mod.rs`, calling `save_snapshot`/`load_snapshot`/`replay_wal`/
`acquire_lock`. WAL+snapshot is the only durability backend.

**Similarity.** `Graph::similar_nodes` (`similarity.rs:102`) is a five-step
pipeline: (1) structural + property filter → candidate set; (2) drop
wrong-dimension embeddings; (3) bound the set against the config scan ceiling;
(4) **exhaustive scan**, scoring every survivor; (5) rank + truncate to `k`.
Steps 4–5 are the exhaustive scan the seam is meant to abstract.

## 3. Proposal

### 3.1 Scope recommendation — ship the seams, defer the alternatives

Mirror the **M5 query-seam decision** (`docs/specs/m5-query-seam-decision.md`: ship
the `PropertyGraphRead` seam, defer Cypher). Concretely:

1. **Introduce both trait seams now**, with the current implementations moved
   behind them unchanged. This satisfies commitment 6 — the boundary exists and
   the internals are swappable.
2. **Do not build a second implementation** (embedded-KV persistence, ANN
   similarity) in this change. The PRD gates the ANN swap on *"only if the M3
   budget gate fails."* The [current representative results](m3-findings.md)
   include a filtered-similarity failure, so evaluation is warranted; an overrun
   alone does not establish which implementation meets the consumer's workload
   and correctness requirements. F5 records the alternatives and the missing
   capture. The decay/similarity measurements also provide no evidence that a
   different persistence backend would solve these costs.

The deliverable is therefore an **internal refactor with zero public-API change**
that converts "we have one hard-coded internal" into "we have one internal
*behind a seam*." That is exactly the commitment.

### 3.2 Persistence trait

A `Persistence` trait capturing the write-side durability surface `Graph`
depends on:

```rust
pub(crate) trait Persistence {
    /// Buffer one mutation; durable only at the next `commit`.
    fn append(&mut self, mutation: &Mutation) -> Result<()>;
    /// Flush buffered mutations to durable storage (fsync-backed).
    fn commit(&mut self) -> Result<()>;
    /// Compact: fold the log into a fresh full-image checkpoint.
    fn snapshot(&mut self, store: &Store) -> Result<()>;
}
```

- `Graph.persist` becomes `Option<Box<dyn Persistence>>`.
- The current `Persister` becomes `WalPersistence` and implements the trait
  verbatim (its `append`/`commit` already match).
- **Recovery/construction stays a per-backend factory**, not a trait method:
  `open`/`create`/`import` produce `(Store, Box<dyn Persistence>)` (or an
  in-memory graph with `None`). Reason: recovery *builds* the graph — it is not a
  method *on* an existing backend — so forcing it into the trait would contort
  the object-safety and lifetime story for no gain. The trait is the steady-state
  seam; wiring a new backend means adding a factory + an impl, both internal.
- `poisoned`/lock handling stay inside `WalPersistence` (backend-specific).

**Why `pub(crate)` and not `pub`:** the trait is an *internal* seam
(commitment 6 says "swappable without API change" — swapping is our concern, not
the consumer's). Keeping it crate-private means we are free to change the trait
shape when a real second backend arrives, without a breaking release.

### 3.3 Similarity-evaluation trait

A `SimilarityEvaluator` seam abstracting steps 3–5 (bound + scan + rank):

```rust
pub(crate) trait SimilarityEvaluator {
    /// Rank `candidates` (already structural/property-filtered and
    /// dimension-checked) against `query` by `metric`, returning the top `k`
    /// best-first with scores.
    fn top_k(
        &self,
        query: &[f32],
        metric: SimilarityMetric,
        candidates: &[(NodeId, &[f32])],
        k: usize,
    ) -> Vec<(NodeId, f32)>;
}
```

- The current exhaustive scan becomes `ExhaustiveScan`, the sole implementation.
- `similar_nodes` keeps ownership of steps 1–2 (filtering, dimensionality) and
  the scan-ceiling bound (step 3), then delegates 4–5 to the evaluator.

**The load-bearing design decision — candidates in, not space owned.** The seam
takes the *already-filtered candidate set*, because PRD §13.1 fixes the
evaluation order as **filters first, then vector search over survivors**. This is
deliberate and it is the crux of any future ANN work:

- An ANN index answers "top-k over the *whole* vector space" fast, but does **not**
  natively restrict to an arbitrary structural/property subset. Post-filtering
  (ANN's global top-N ∩ candidates) can silently under-return; pre-filtering
  (search only within the subset) is not what mainstream ANN structures do.
- By making the seam "rank *these* candidates," the exhaustive impl is trivial and
  correct, and a future ANN impl is forced to confront the filter-composition
  problem explicitly (e.g. ANN only when the candidate set is the whole graph;
  fall back to scan when filters are selective) rather than quietly returning
  wrong results. The seam encodes the §13.1 contract instead of hiding it.

This means the seam as proposed does **not** by itself make ANN easy — it makes
the *point where ANN must prove it respects the filters* explicit. That is the
honest boundary; a signature that let an evaluator ignore the candidate set would
be a trap.

## 4. Impact, effort, risk

- **Public API:** unchanged. Both traits are `pub(crate)`; `similar_nodes`,
  `commit`, `create`/`open`/`snapshot`/`export`/`import` keep their signatures.
- **Files:** persistence seam touches `graph.rs` (field type + the two call
  sites) and `persist/mod.rs` (impl block → trait impl, factory). Similarity seam
  touches `similarity.rs` (extract steps 4–5). Small, mechanical.
- **Behavior:** identical — this is a pure extract-behind-trait. The full
  existing test suite must pass **unchanged**; that is the correctness proof.
- **Risk:** low. The main subtlety is object-safety/borrowing on the persistence
  trait object and the `&[(NodeId, &[f32])]` candidate borrow in the evaluator;
  both are resolved by keeping recovery out of the trait and materializing the
  candidate slice in `similar_nodes`.
- **New test:** one test that exercises dispatch through each trait object (e.g.
  a graph built with `Box<dyn Persistence>` round-trips; `similar_nodes` returns
  the same ranking via the trait as the old inline scan). Low-cost.

## 5. Alternatives considered

- **Do nothing / close the finding as "correct behavior."** Rejected: commitment
  6 and §13.2 name the seams as *deliverables*, not aspirations; a reviewer
  re-running the audit would re-flag it. The seam is cheap; skipping it leaves a
  standing gap against a stated commitment.
- **Build the ANN / KV backend now.** Deferred: the synthetic similarity overrun
  motivates evaluation, but does not choose ANN over exact-scan improvements or
  narrower candidate sets. Budget changes require workload-derived tolerances.
  Persistence replacement is not supported by these in-memory measurements.
- **Public traits.** Rejected: the consumer never swaps these; a public trait
  freezes an internal shape we may want to revise when a real second impl lands.

## 6. Ratified decisions (2026-07-03)

All four recommendations were accepted:

1. **Both seams.** `Persistence` (persistence) and `SimilarityEvaluator`
   (similarity) both introduced.
2. **Narrow persistence trait + recovery factory.** The trait is
   `append`/`commit`/`snapshot`/`epoch`; `open`/`create`/`import` stay factories
   that build the graph and its backend.
3. **Similarity seam is candidates-in.** `top_k(query, metric, candidates, k)`
   ranks a pre-filtered set, encoding the §13.1 evaluation order.
4. **Ship the seams, defer the alternatives.** No ANN and no alternative
   persistence backend. The earlier "M3 gate passes" rationale is superseded
   by [F3/F5 and the current baseline](m3-findings.md); a failing synthetic
   budget permits investigation, not an automatic implementation choice.

## 7. Implementation (as shipped)

Internal refactor, zero public-API change:

- `drey/src/persist/mod.rs`: `pub(crate) trait Persistence`; the former
  `Persister` is now `WalPersistence` implementing it (`append`/`commit`/
  `snapshot`/`epoch`, bodies unchanged; the `snapshot` compaction moved off
  `Graph` into the impl). `Graph.persist: Option<Box<dyn Persistence>>`.
  `Graph::snapshot` delegates via a disjoint split borrow of `persist`/`store`.
- `drey/src/similarity.rs`: `pub(crate) trait SimilarityEvaluator` + the sole
  `ExhaustiveScan` impl; `similar_nodes` keeps filtering/dimension/ceiling
  (steps 1–3) and dispatches scoring/ranking (4–5) through
  `&dyn SimilarityEvaluator`.

Verification: the full pre-existing test suite passes **unchanged** (behavior
identical — the correctness proof for a pure extract-behind-trait), plus a unit
test dispatching through the `SimilarityEvaluator` trait object. Both traits are
`pub(crate)`, so the public API is untouched.
