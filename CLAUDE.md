# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project status

**Pre-code.** This directory contains one artifact: `drey-PRD-v0_8.md`, the
authoritative product spec. There is no Cargo project, no source, and no git repository
yet. The crate is named `drey` (renamed 2026-07-02 from the working name `weaver-graph`).
Read the PRD in full before doing any design or implementation work — it is the single
source of truth, and it records not just decisions but the rationale and the rejected
alternatives.

`specs/` holds implementation specs that supply the "how" beneath the PRD's "what", one
per concern, keyed to milestones (`m0-fixture-harness.md` first). Specs cite the PRD
sections they implement and mark any call the PRD left open as **[spec decision]** with
rationale. The PRD governs on any conflict. Before implementing M0 apparatus, read
`specs/m0-implementation-checklist.md` alongside the spec — it enumerates the defaults an
implementing session is likely to reach for that would silently violate the contract.

When the crate is scaffolded, it will be a standard single Cargo crate (`cargo build`,
`cargo test`, `cargo test <name>` for one test). When initializing git, follow the
workspace convention in `/home/todd/git/CLAUDE.md`: personal projects push via the
`github-toddwbucy:` SSH host alias.

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
