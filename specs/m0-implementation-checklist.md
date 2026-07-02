# M0 Implementation Checklist: Risk Audit of the Fixture/Harness Spec

Status: adopted 2026-07-02. Provenance: implementation-risk audit of
`m0-fixture-harness.md` v0.1 (Bucy, 2026-07-02), reproduced below with light
formatting only. Read this alongside the spec before implementing any M0
apparatus. Its framing is the right one: each item names a default an
implementing session is likely to reach for, and the guardrail that stops it.

## Disposition of findings (against spec v0.2)

The audit's eight "spec gaps" and four trap-identified defects were resolved
in spec v0.2 as follows:

| Finding | Disposition |
|---------|-------------|
| Gap 1 / trap 14 — canonical JSON undefined | Resolved: §3.5 defines canonical bytes (field order, Ryu shortest round-trip floats, LF, final LF, ascending-ID record order) |
| Gap 2 / trap 8 — `edges` vs `fanout` | Resolved: `edges` is derived (`nodes × fanout mean`), never independent; size-table counts are the `medium` derivation; sweep counts stated |
| Gap 3 / trap 43 — percentile method | Resolved: nearest-rank, `ceil(p × n)`th smallest retained sample |
| Gap 4 / trap 51 — `status` absent from schema | Resolved: result rows carry `status: "ok" \| "n/a" \| "error"` |
| Gap 5 / trap 24 — `WeightUpdate` bounds semantics | Deferred to the API-contract spec, named as a dependency in §4.1; M0 stopgap: apply op, then clamp into `[min, max]` |
| Gap 6 / trap 23 — candidate-set targeting | Resolved: targets 100/1k/10k within ±25%, actual count in counters |
| Gap 7 / trap 12 — denormals vs normalization order | Resolved: hostile components injected after normalization; unit norm is approximate and load-bearing for nothing |
| Gap 8 / trap 25 — commit cadence vs mixes | Resolved: commits inserted every 1,000 mutations as plan structure, outside mix percentages |
| Trap 18 — traversal naming drift | Resolved, in the opposite direction from the audit's suggestion: one canonical op name `traverse` everywhere, `max_hops` is a parameter class, no aliases |
| Trap 33 — dual budget for `update_edge_weight` | Resolved: `budget_throughput_per_s` field; `pass` requires both halves |
| Trap 53 — stable orderings | Public ordering guarantees belong to the API-contract spec; v0.2 §4.1 requires internal driver determinism in the meantime |

Two audit notes are corrected rather than adopted: trap 36 slightly misreads
v0.1, which already made 5 MB a review trigger and not a gate; trap 1's
concern is structurally handled by the repo rule that the PRD governs on
conflict (CLAUDE.md). Section references in the audit body cite v0.1; v0.2
changed no section numbering.

---

## The audit

Below is an implementation-risk audit of the spec: places where an LLM is
likely to "fill in" defaults, simplify, or silently violate the contract.

### Highest-risk traps

**1. Treating this spec as authoritative over the PRD.** The spec explicitly
says the PRD governs on conflict, and only calls out new decisions with
**[spec decision]**. An LLM may implement the spec in isolation and
accidentally override PRD constraints. *Wrong:* "the spec says X, so
implement X permanently." *Guardrail:* treat M0 as subordinate to the PRD;
anything marked provisional or synthetic must remain replaceable.

**2. Blocking on captured data instead of synthetic-first.** The fixture is
synthetic-first; the reference consumer capture is preferred but development
must not block on it. *Wrong:* refuse to generate fixtures until captured
data exists, or hard-code synthetic numbers as final. *Guardrail:* implement
synthetic generation now; make source, budgets, rates, and parameters
replaceable independently.

**3. Promoting the fixture format into the crate's persistence format.** The
fixture format is test apparatus only. Persistence design, binary codec,
weight precision, and internal data structures are not decided here.
*Wrong:* use `nodes.jsonl` / `edges.jsonl` / `embeddings.bin` as the
production storage design. *Guardrail:* keep fixture parsing in harness/test
code; never let it shape the crate's storage API or durability format.

**4. Ignoring synthetic/captured provenance.** *Wrong:* generate files
without provenance, or store only counts/checksums. *Guardrail:* the
manifest must record source, generator version, seed, parameters, counts,
and data-file checksums.

**5. Failing true byte-for-byte determinism.** *Wrong:* `HashMap` iteration
order, platform-native float formatting, current time, OS RNG, thread race
ordering, or non-canonical JSON. *Guardrail:* fixed PRNG, stable iteration,
canonical serialization, deterministic newlines, deterministic file
ordering, stable checksums.

**6. Inferring identity from position.** IDs are dense from zero, but every
ID is still explicitly stored. *Wrong:* treat line index in `nodes.jsonl` or
embedding record index as the node ID. *Guardrail:* always parse and use
explicit `id`, `from`, `to`, and `node_id` fields.

**7. Using the wrong graph distribution.** The degree distribution is
truncated Zipf (`s = 1.2`, `max_degree = 1000`) over a
preferential-attachment wiring pass; edge types are Zipf-drawn too.
*Wrong:* Erdős–Rényi random edges, uniform edge types, fixed fanout.
*Guardrail:* preserve hub-heavy behavior and type skew — the harness exists
to expose traversal cost on hubs.

**8. Confusing `edges` and `fanout`.** *Wrong:* compute edges as
`nodes × fanout` and ignore the size-class edge count, or ignore `fanout`
because `edges` is present. *Guardrail (as resolved in v0.2):* `edges` is
derived from `nodes × fanout mean`; size-class counts are the `medium`
derivation; sweeps change the derived count.

**9. Forgetting that type labels are intentionally meaningless.** *Wrong:*
semantic names like `Memory`, `Belief`, `Task`, or reserved type names.
*Guardrail:* neutral opaque labels only (`nt_00`…, `et_00`…).

**10. Getting node properties wrong.** Every node carries exactly three
properties: `p_seq: I64` (unique, monotonic), `p_cat: String` (categorical,
selectivity classes ~0.1% / 1% / 10% of a type's population), `p_score: F64`
(uniform [0,1]). *Wrong:* arbitrary extra properties, omissions, random
`p_seq`, integer `p_score`, or globally-sized categories. *Guardrail:*
enforce the three properties and their query roles; selectivity is per node
type.

**11. Getting edge properties wrong.** Edges carry `weight: f32` in (0, 1]
and one property, `p_seq: I64`. *Wrong:* f64 weights, zero weights, node
property schema on edges, missing edge `p_seq`. *Guardrail:* the edge schema
is narrower than the node schema.

**12. Mishandling embeddings.** *Wrong:* normal-looking decimal floats, f64,
JSON serialization, omitted hostile values, or normalizing after injection.
*Guardrail:* binary f32 sidecar; hostile components injected after
normalization; the byte-exactness property, not the norm, is what matters.

**13. Serializing embeddings as JSON.** *Wrong:* embeddings inline in
`nodes.jsonl`, float arrays in JSON, omitting `dim`, native endian.
*Guardrail:* the exact sidecar layout —
`(node_id: u64 LE, dim: u32 LE, dim × f32 LE)` records.

**14. Overlooking canonical formatting for `weight`.** *Wrong:* let the JSON
library choose float formatting, destabilizing checksums. *Guardrail:*
canonical JSON per spec §3.5 (field order, Ryu shortest round-trip, LF,
final LF).

**15. Writing JSON arrays instead of JSONL.** *Wrong:* one big array or
pretty-printed multi-line objects. *Guardrail:* strict JSONL, exactly one
complete object per line.

**16. Forgetting `workload.jsonl`.** The workload is data, not code.
*Wrong:* generate operations on the fly during benchmarking, or encode the
workload in Rust. *Guardrail:* materialize deterministic `workload.jsonl`
and run exactly that sequence.

### Workload traps

**17. Dropping property queries.** The op set is one row per budget-table
row *plus* `property_eq` / `property_range`. Include them.

**18. Misnaming traversal operations.** Resolved in v0.2: one op name
`traverse` with `max_hops` as a parameter class across workload parsing,
driver dispatch, aggregation, and budget lookup.

**19. Simplifying `neighbors`.** Cover direction, zero/one/two edge-type
filters, and optional `min_weight` — not just outgoing unfiltered.

**20. Simplifying traversal.** Enforce hop cap and `max_paths` 1000; return
paths, not just visited nodes; report path counters.

**21. Simplifying shortest path.** Include disconnected pairs and both
unweighted and weighted cost modes.

**22. Getting property selectivity wrong.** Generate explicit ~0.1%, ~1%,
and ~10% selectivity cases; don't let random draws collapse the sweep.

**23. Under-specifying `similar_nodes`.** Compose with type + property
filters; sweep candidate targets 100/1k/10k (±25%); record actual candidate
count. Never a global unfiltered scan.

**24. Mishandling mutation operations.** All three `WeightOp` variants,
bounds present 50% of the time (stopgap semantics: apply op, then clamp);
`decay_edges` with edge-type filter, factor 0.9, batches 1k/10k/100k.

**25. Treating `commit` and `open` as fully active in M0.** Keep the rows in
the schema with `"status": "n/a"` until M2 — don't implement persistence
early and don't mark them failed.

**26. Randomly sampling start nodes.** Precompute degree strata (low /
median / hub) and draw workload cases from each; uniform sampling averages
hub cost away.

**27. Misinterpreting workload mixes.** 100,000 ops per mix, class
percentages preserved — not tiny samples, not equal weights per variant.

**28. Treating mix percentages as final observed rates.** They are
provisional stand-ins; store provenance and let captured rates supersede.

### Budget and pass/fail traps

**29. Deriving budgets from implementation performance.** Budgets come from
the operation's position in the consuming process. *Wrong:* run
`NaiveDriver`, compute p95, call it the budget. *Guardrail:* budgets are
fixed M0 deliverables; measurements compare against them later.

**30. Using `NaiveDriver` as a benchmark baseline.** It validates plumbing,
schema, counters, and repeatability only; its numbers are never a
comparison baseline; always labeled `driver: "naive"`.

**31. Comparing the wrong percentile.** Pass/fail is p95 against
`budget_us`.

**32. Getting units wrong.** All latency fields are microseconds (`_us`);
convert second- and millisecond-shaped budgets correctly.

**33. Missing the dual budget for `update_edge_weight`.** Both ≤ 10 µs p95
and ≥ 100k updates/s sustained; `pass` requires both (v0.2 adds
`budget_throughput_per_s`).

**34. Running budgets against the wrong fixture profile.** The budget table
is representative size, medium fanout, dim 1024 unless noted; attach
fixture class, fanout, and dim to every result row's context.

**35. Treating the dimensionality sweep as global.** {256, 1024, 2048} at
representative size, similarity rows only.

**36. Treating the link-size trigger as a hard budget.** 5 MB is a review
trigger, not an automatic fail. (The spec already said this in v0.1.)

**37. Forgetting "no daemon, no listener".** Keep everything
embedded/library-style; record the inspection result in harness output.

### Harness architecture traps

**38. Making the harness a dependency of `drey`.** Harness lives in
`harness/`; no benchmark types, workload parser, or fixture structs in the
main crate API.

**39. Collapsing the driver abstraction.** Keep workload execution
driver-neutral behind `GraphDriver` so `DreyDriver` replaces `NaiveDriver`
at M1.

**40. Returning only timing from operations.** `OpOutcome` carries result
cardinality — paths returned, candidates scanned, edges decayed, steps
visited.

**41. Using multithreaded measurement.** Single-threaded measured path,
matching the single-writer model.

**42. Getting warmup and sample counts wrong.** Per (op class × parameter
class): discard 100 warmup samples first, then retain ≥ 1,000 (≥ 100 at
stress). Never aggregate parameter classes.

**43. Using histograms for percentiles.** Nearest-rank over retained raw
samples; no HDR histograms or benchmark-library summaries.

**44. Ignoring host fingerprint.** CPU model, cores, RAM, OS in every run
document; no cross-host pass/fail claims.

**45. Measuring cold start in-process.** Once `open` is active (M2+), each
cold-start sample runs in a fresh process.

### JSON output traps

**46. Emitting many JSON documents instead of one per run.** One
schema-valid run artifact with a `results` array.

**47. Omitting budget metadata.** Every result row carries `budget_us`
(and `budget_throughput_per_s` where dual), `budget_source`, and `pass` —
measurement and budget table are one artifact.

**48. Populating `pass` too early.** `pass` stays `null` for `NaiveDriver`
rows regardless of timings, and until a real driver runs at M1+.

**49. Losing fixture checksum verification.** Load the manifest, verify
every data-file checksum, record `checksum_verified`.

**50. Omitting resource measurements.** Include `resident_bytes`,
`raw_payload_bytes`, `link_size_bytes` even when values are `null`.

**51. `"status": "n/a"` vs the schema.** Resolved in v0.2: `status` is an
explicit result-row field, `"ok" | "n/a" | "error"`.

### Repeatability and correctness traps

**52. Treating counter mismatch as noise.** Same fixture, workload, driver,
and host fingerprint ⇒ identical counters, exactly. Only timings get noise
tolerance.

**53. Failing to separate timing noise from deterministic outputs.**
Nondeterministic traversal order can return different-but-"equivalent" path
sets under `max_paths` truncation. Drivers must be internally deterministic
now; public ordering guarantees land in the API-contract spec.

**54. Forgetting schema validation.** Validate the run document against a
schema or strict typed serializer round-trip test — required fields, units,
nullables, row shapes.

### Capture and supersession traps

**55. Treating captured replacement as all-or-nothing.** Supersession is
per-artifact: fixture, workload rates, and budget tolerances each carry
their own source.

**56. Turning the capture export into a migration tool.** The export script
is deliberately throwaway (PRD §3.2.7); never promote it.

**57. Losing mixed-source honesty.** Provenance at the artifact level; a run
is not "captured" because one artifact is.

### M0 exit checklist traps

**58. Generating only one fixture size.** All three classes generate
deterministically from recorded seeds, even though full harness runs are
required only for `small` and `representative`.

**59. Generating only one workload mix.** All four mixes.

**60. Failing to mark budgets synthetic.** Every budget row carries
`budget_source: "synthetic"` until superseded.

**61. Running only the small fixture through the harness.** M0 proves
end-to-end mechanics on `small` **and** `representative`.

**62. Not repeating runs to check counters.** Run the same
fixture/workload/driver/host combination at least twice; compare counters
exactly.

### Compressed implementation checklist

A robust implementation enforces these invariants:

- Deterministic fixture generation from `(generator_version, seed,
  parameters)`.
- Stable manifest with source, counts, params, checksums, and generator
  version.
- JSONL structure files only; binary little-endian embedding sidecar.
- Explicit IDs everywhere; never infer IDs from position.
- Truncated-Zipf hub-heavy graph shape; Zipf edge-type skew.
- Required node and edge properties with exact scalar types.
- Approximately-unit-normalized f32 embeddings with post-normalization
  hostile precision cases.
- Deterministic `workload.jsonl` for all four mixes.
- Degree-stratified start nodes and parameter-class-aware sampling.
- `NaiveDriver` as throwaway mechanics validation only.
- Single-threaded measurement, warmup discard, retained-sample minima,
  nearest-rank percentiles from retained samples.
- One JSON document per run with host fingerprint, fixture checksum
  verification, result counters, budgets, resources, `status`, and nullable
  `pass`.
- Exact counter repeatability across repeated runs.
- Synthetic/captured provenance preserved per artifact.
