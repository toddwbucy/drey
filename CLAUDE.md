# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project status

**v0.1 implemented.** `docs/drey-PRD-v0_8.md` is the authoritative spec; `docs/specs/` holds the
implementation specs beneath it. The crate is named `drey` (renamed 2026-07-02 from the
working name `weaver-graph`). Read the PRD before structural work — it records not just
decisions but their rationale and the rejected alternatives.

The milestones M1–M5 are built and tested; M0/M3 apparatus runs. The 2026-07-03
whole-codebase audit (issue #5, closed) was remediated in PRs #6–#15; the two remaining
budget-gate overruns at representative scale (`decay_edges batch=1000`,
`similar_nodes cand=10000`) are documented with their revision options in
`docs/specs/m3-findings.md` (the PRD §16.3 "written reason to revise" exit). The M5
query-layer decision is in `docs/specs/m5-query-seam-decision.md` (defer Cypher; ship the
seam), and the persistence/similarity seams (design commitment 6) are ratified in
`docs/specs/persistence-seam-decision.md`. The M2 durability contract, on-disk format
history, and failure semantics are recorded in `docs/specs/m2-durability.md`; every PRD §24
open question is dispositioned in `docs/specs/open-questions-ledger.md`.

### Workspace layout
- `drey/` — the publishable crate. Single crate, `serde` its only dependency.
  - `types` `error` `config` `mutation` — data model and API-support types (PRD §7, §9, §19).
  - `store` `interner` — memory-primary graph + §8 index set (nested `node → edge_type →
    [edge]` adjacency, ordered scalar property index).
  - `graph` — public mutation API; every mutation logs one `Mutation` for the WAL.
  - `query` `traverse` `similarity` — reads (PRD §9.3, §9.4).
  - `export` (`GraphFeatureExport`, M4) `read` (`PropertyGraphRead` seam, M5).
  - `persist/` — WAL + snapshot durability, hand-rolled binary codec (M2).
- `harness/` — **throwaway M0 apparatus, `publish = false`, never a `drey` dependency**
  (the dependency arrow points harness → drey only). Fixture generator, workload plans,
  `GraphDriver` (`NaiveDriver`/`DreyDriver`), runner, JSON output; `generate` + `bench` bins.

### Commands
- Build / test everything: `cargo build`, `cargo test`. One test: `cargo test <name>`.
- Generate a fixture: `cargo run --release -p harness --bin generate -- <small|representative|stress> <low|medium|high> <seed> <out_dir>` — also materializes the workload plans (`workload.measurement.jsonl` + the four §4.2 mixes) next to the fixture.
- Run the budget gate: `cargo run --release -p harness --bin bench -- <fixture_dir> <drey|naive> [workload_name]` — `workload_name` defaults to `measurement` (the budget-gate plan; the four mix names also work); emits one run-JSON document; exits non-zero if a real-driver bucket fails its budget. **Measure in `--release`**; debug tails are meaningless.

Git: personal project, pushes via the `github-toddwbucy:` SSH host alias (workspace
convention in `/home/todd/git/CLAUDE.md`). Work merges to `main` via PRs; CI enforces
build/fmt/test/clippy on stable.

Before implementing more M0 apparatus, read `docs/specs/m0-implementation-checklist.md`
alongside the spec — it enumerates the defaults a session is likely to reach for that
would silently violate the contract.

## What this project is

An embedded, in-process, **memory-primary** property-graph crate for Rust — "SQLite-class"
in operational shape (linked library, local file, no daemon, no listener), but RAM-first
where SQLite is disk-first. It provides typed nodes/edges, mutable edge weights, property
lookup with equality+range scalar indexes, bounded traversal, shortest path, exhaustive-scan
vector similarity composable with structural filters, durable persistence with explicit
`commit`, and a `GraphFeatureExport` trait for external GNN pipelines.

It is an **independent crate**. WeaverTools is the reference consumer (see sibling project
`../limen` and `/opt/weavertools/`), but the crate must carry zero knowledge of, or
reference to, any consumer. The reference consumer's contract lives on the consumer side
as an executable conformance suite, not here.

## Design commitments (do not violate)

1. **Mechanic, not motive.** The crate stores/indexes/traverses/searches. Anything that
   encodes *meaning* — why an edge matters, what an embedding represents, decay scheduling,
   ranking policy — belongs to the consumer. This razor decides most API questions.
2. **Memory-primary.** All queries run against RAM. Disk is reconstruction-only, never on
   the query path. A design that reads storage to answer a query is wrong.
3. **One process, one graph, single writer.** Isolation is the OS process boundary, not an
   internal access-control model. Synchronous API, no async runtime dependency.
4. **SQLite-class weight.** Link size, resident memory, open time, and zero operational
   surface are judged; heaviness is a defect even when performance is adequate.
5. **Durable internal IDs are the sole identity.** `NodeId`/`EdgeId` survive close/reopen
   exactly; encoding stores IDs explicitly, never by array position. There is no external
   addressing scheme.
6. **Stable contracts, replaceable internals.** Public API sits above a persistence trait
   and an internal similarity-evaluation seam; both internals are swappable without API change.
7. **Budgets, not comparisons.** Performance targets are derived from a captured or
   synthetic reference workload (M0), never from feature/speed parity with another database.

## Closed decisions — do not re-open without explicit instruction

The PRD records these as settled, with rationale:

- **No core timestamps** (`created_at`/`updated_at`) — consumers store them as properties (v0.6).
- **No `external_id` / external addressing** — consumers hold internal IDs natively (v0.8).
- **No nested property values** (no `Map`, no nested `List`) — v0.1 values are `Null`, `Bool`,
  `I64`, `F64`, `String`, `Bytes`, `List<Scalar>`. Hierarchy is composed via a metadata
  subgraph in the same graph, not nesting (v0.5).
- **No `open_or_create`** — `open` fails if absent, `create` fails if present (v0.5).
- **Single `commit` verb** — no pre-emptive `commit`/`flush` split; durability vocabulary is
  fixed at M2 when the persistence design is chosen (v0.5).
- **Binary on-disk encoding** — committed for byte-exact `Bytes`/`f32` round-trip; the codec
  choice (MessagePack/CBOR vs. bincode) and file structure remain measurement-gated at M2.
- **No ANN index in v0.1** — similarity is bounded exhaustive scan behind an internal trait
  seam; an existing ANN crate may be integrated later only if the M3 budget gate fails.
- **No query language ownership** — Cypher (or GQL subset) is an adapter seam over
  `PropertyGraphRead`, read-only, never v0.1 release-blocking.

## Milestone sequence

Work proceeds M0→M5, each with exit criteria in PRD §21:

- **M0** — workload capture (or synthetic fixture) → budget table (§16.4) + JSON measurement harness.
- **M1** — in-memory prototype: full data model, mutation, traversal, similarity scan; no persistence.
- **M2** — persistence: trait + first implementation (WAL+snapshots is the leading candidate vs.
  embedded KV, decided by measurement), recovery-matrix tests (§10.2.1), and a plainly stated
  durability level for `commit`. At least one fsync-backed durability point is a standing
  consumer requirement.
- **M3** — budget gate: every §16.3 gate measured at p50/p95/p99.
- **M4** — `GraphFeatureExport` (deterministic, framework-agnostic).
- **M5** — query-seam decision (adapter / custom limited parser / defer).

Open questions (PRD §24) that block a piece of implementation must be converted to a
decision or milestone gate first — notably weight precision (f32 vs f64), the binary codec,
and uncommitted-mutation discard semantics on drop.
