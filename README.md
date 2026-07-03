# drey

An embedded property graph for Rust. For when your data is local and you need it in-process.

drey links into your process as a library and holds the working graph in memory. Nodes and edges are typed and carry properties, edges carry mutable weights, and a single query can compose traversal, property predicates, and vector similarity over stored embeddings. The graph persists to a local path your process owns and reloads with the same durable IDs it saved. There is no server, no daemon, no network listener, no service account, and nothing to operate. One process, one graph, single writer.

## Embeddings live on the node

The embedding is part of the node record, not a companion system. Each node type declares its dimension once - any dimension, per type, so a 1024-dim `Memory` and a 384-dim `CodeChunk` coexist in one graph - and the vector is stored inline with the node's type and properties, resident in the same process memory as the structure it describes.

That co-location is a latency and correctness position, not a storage detail:

- **Recall is a function call.** No network hop, no serialization, no separate vector service. Structural filters are hash and index lookups; scoring is a linear pass over `f32` slices already in RAM. Measured end-to-end: sub-millisecond at 100 candidates, ~4 ms at 1,000 (dim 1024, representative fixture - see [`docs/specs/m3-findings.md`](docs/specs/m3-findings.md)).
- **Filters run first, and the result is exact.** A similarity query composes node-type, property, and reachability filters *before* scoring, so it returns the true top-k of the filtered set - not an over-fetched approximation from a vector store, intersected with the graph after the fact.
- **One commit domain.** The embedding and the structure it belongs to go through the same write-ahead log and the same `commit`. There is no vector-store/graph-store drift to reconcile, because there is only one store.

Similarity is a bounded exhaustive scan by design: exact, deterministic, no index to build, with a configurable candidate ceiling so an accidental whole-graph scan is an error rather than a stall. An internal evaluation seam is handed the pre-filtered candidate set, so an ANN structure can replace the scan later without an API change - a decision drey defers until a real workload demands it.

## Durability

`commit` is an fsync-backed durability point. Persistence is a CRC-framed write-ahead log plus snapshots, with a hand-rolled binary encoding for byte-exact `f32`/`Bytes` round-trips, crash-safe OS advisory locking, and recovery that loads the last committed state or fails explicitly - never a silent partial load. A persister that hits a durable-write failure poisons itself: reads keep working, writes refuse until reopen, and `commit` never reports durability it did not achieve. `NodeId`/`EdgeId` survive close and reopen exactly.

## Status

v0.2. The whole workspace has been through two end-to-end adversarial reviews ([issue #5](https://github.com/toddwbucy/drey/issues/5), [PR #17](https://github.com/toddwbucy/drey/pull/17)) plus per-PR review; the two open performance-budget questions are recorded with their revision options in [`docs/specs/m3-findings.md`](docs/specs/m3-findings.md). `docs/drey-PRD-v0_8.md` is the authoritative spec.
