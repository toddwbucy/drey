# drey

An embedded, RAM-first property graph for Rust, with an optional local engine service.

drey links into your process as a library and holds the working graph in memory. Nodes and edges are typed and carry properties, edges carry mutable weights, and a single query can compose traversal, property predicates, and vector similarity over stored embeddings. The graph persists to a local path your process owns and reloads with the same durable IDs it saved. One process, one graph, single writer. The optional `drey-local` service exposes the same resident engine through Unix sockets on Linux; it has no TCP or HTTP listener.

## Local engine v1

`drey::engine::Engine` adds atomic durable batches, persistent retry tokens, embedding-space validation, intact JSON documents, and bounded combined retrieval. `drey-local` supplies a versioned Rust client and command-line client, lifecycle operations, and serialized requests from one application's foreground and background connections.

Read the [implemented contract](docs/specs/local-engine-v1.md) for runnable request examples, limits, failures, and the distinction between library primitives and engine batches. [Reproducible service measurements](docs/specs/local-engine-v1-evidence/README.md) include full retrieval responses and background durable writes. Weaver's schema, SQLite relationship, and actual inference timing remain integration decisions.

## Embeddings live on the node

The embedding is part of the node record, not a companion system. Each node type declares its dimension once - any dimension, per type, so a 1024-dim `Memory` and a 384-dim `CodeChunk` coexist in one graph - and the vector is stored inline with the node's type and properties, resident in the same process memory as the structure it describes.

That co-location is a latency and correctness position, not a storage detail:

- **Embedded recall is a function call.** Structural filters use resident indexes; scoring is a linear pass over `f32` slices already in RAM. The optional service adds socket transport and serialization. Historical library measurements are in [`docs/specs/m3-findings.md`](docs/specs/m3-findings.md); they do not measure the service round trip.
- **Filters run first, and the result is exact.** A similarity query composes node-type, property, and reachability filters *before* scoring, so it returns the true top-k of the filtered set - not an over-fetched approximation from a vector store, intersected with the graph after the fact.
- **One commit domain.** The embedding and the structure it belongs to go through the same write-ahead log and the same `commit`. There is no vector-store/graph-store drift to reconcile, because there is only one store.

Similarity is a bounded exhaustive scan by design: exact, deterministic, no index to build, with a configurable candidate ceiling so an accidental whole-graph scan is an error rather than a stall. An internal evaluation seam is handed the pre-filtered candidate set, so an ANN structure can replace the scan later without an API change - a decision drey defers until a real workload demands it.

## Durability

`commit` is an fsync-backed durability point. Format 4 combines CRC-framed WAL records and snapshots with an independently published SHA-256 commit seal. Recovery validates acknowledged bytes before replay or truncation. Durable internal IDs and byte-exact `f32`/`Bytes` values survive reopen. A failed persister refuses further writes; the higher-level engine refuses both reads and writes until recovery, preventing uncertain batch state from being served.

Format 3 database directories are refused unchanged. `Graph::import_v3_snapshot` explicitly converts a trusted legacy snapshot/export in memory, preserving the source and IDs; legacy WAL recovery and mapping a graph into an engine schema are separate migration steps. See the [format and migration contract](docs/specs/local-engine-v1.md#persistence-format-4-and-recovery).

## Status

v0.2. The whole workspace has been through two end-to-end adversarial reviews ([issue #5](https://github.com/toddwbucy/drey/issues/5), [PR #17](https://github.com/toddwbucy/drey/pull/17)) plus per-PR review; the two open performance-budget questions are recorded with their revision options in [`docs/specs/m3-findings.md`](docs/specs/m3-findings.md). `docs/drey-PRD-v0_8.md` is the authoritative spec.
