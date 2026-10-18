# Local engine and service contract v1

Status: implemented on `feat/local-engine-v1`, following
[issue #29's execution target](https://github.com/toddwbucy/drey/issues/29#issuecomment-6251033236).
This is the governing extension to PRD v0.8 for the engine API, durable batches,
embedding-space validation, and optional local service. The embedded `Graph` API
remains available as a lower-level primitive. Weaver and HADES integration remain
outside this implementation.

## Boundary and ownership

`drey::engine::Engine` owns one resident graph, schema, indexes, and retry receipts.
`drey-local` adds a synchronous client and Linux Unix-socket host. It has no TCP/HTTP
listener, replication, clustering, remote administration, or model implementation.
`harness` depends on these components; neither production crate depends on `harness`.

One owner serializes all engine operations. Up to eight connections from the owning
UID let one client application submit reads while its background writer runs;
this is not concurrent database mutation or MVCC. A query observes exactly the
completed batch sequence returned in its result. Disk is accessed only for creation,
durable writes, checkpoint, and open/recovery. Every query uses resident records and
indexes. Ordinary OS virtual-memory management is not an engine paging facility.

The service starts closed at a fixed database path. A request cannot select another
path. `create`, `open`, `close`, `status`, `checkpoint`, and `shutdown` define its
lifecycle. `open` reconstructs the whole database before answering. `close` unloads
it and releases its database lock. Shutdown closes the database and exits; abandoned
connections do not unload it. Creation refuses an existing database, including a
second existence check after acquiring ownership. The engine always holds the
database advisory lock while open. A separate socket-owner lock handles stale
socket cleanup after process death.

## Documents, relationships, and embedding spaces

Creation supplies immutable type declarations, scalar-property indexes, and limits.
Types have either no vector space or one `Space`: model identifier, revision,
dimension, metric (`dot`, `cosine`, `euclidean`), and unit-normalization convention.
Vectors are supplied by the client. Writes and queries validate finite components,
dimension, optional squared-norm tolerance `1e-4`, and the complete space declaration.
Equal dimensions alone never imply compatibility. Dimensions are 1–4,096. Ranking
candidates with incompatible spaces fails; records without embeddings are unranked
and omitted from ranked results. Zero vectors have cosine score zero unless the
unit-normalized declaration rejects them. Scores accumulate in f64 to avoid finite
f32 inputs overflowing intermediate dot products.

A record contains existing DREY `Properties`, an optional JSON document string, and
an optional embedding. The document string is validated as JSON and retained exactly,
including nested objects, arrays, nulls, and original spelling. Scalar properties
provide equality/range restrictions and configured indexes. Document interiors are
not implicitly indexed. Null, bytes, and lists remain storable/projectable properties
but are not scalar predicate operands. Projection selects property names and optionally
the entire document; nested paths, array predicates, and a general query language are
outside v1. There is no document flattening or silent text truncation.

Edges have stable IDs, endpoints, a type, finite weight, and properties. Multiple edges
between the same nodes are supported. Updates preserve IDs. Node deletion requires
explicit prior removal of incident edges; there is no implicit cascade or dangling
edge mode in the engine contract. Type/key/token names are nonempty, at most 256
bytes, and cannot start with the reserved prefix `__drey`. Reserved records store
configuration and durable retry receipts and are inaccessible to engine queries/writes.

## Atomic writes and retries

`Batch { token, operations }` supports create/replace/delete nodes and edges. Creation
keys identify new records in the receipt; an edge's `NodeRef::Created` refers to an
earlier node operation in the same batch. Replacing a node replaces all properties,
the document, and the embedding; `embedding: null` explicitly clears the vector.
Replacing an edge replaces its weight/properties without rewiring its endpoints.

Validation runs against a staging graph containing only affected records and, for
node deletions, their incident edges. It does not clone the database. After validation
and capacity checks, the owner applies the same operations, records the retry receipt
in the same commit, and acknowledges only after the WAL and commit seal are durable.
No read can interleave with the batch. Validation/resource refusal changes no records,
indexes, allocation counters, or retry receipts in the live database.

Any unexpected publication or storage failure poisons the engine: subsequent reads
and writes fail until close/reopen. An error or connection loss is not proof that a
write did not land. Repeating an identical token/payload returns its original sequence
and IDs, including after restart. A changed payload with that token is a conflict.
Receipts are never silently evicted; their configured capacity is enforced before
accepting a new batch. Retrying a past creation does not resurrect records subsequently
deleted by another batch. Low-level `Graph` mutation calls are still separate primitives;
use `Engine::write` for this atomic contract.

## Persistence format 4 and recovery

`snapshot.bin` and `wal.log` retain the binary record representation and stable IDs.
Format 4 adds mandatory `commit.seal`:

| Bytes | Content |
| --- | --- |
| 0–3 | `DRCS` magic |
| 4–7 | format version, u32 little endian |
| 8–15 | snapshot epoch, u64 little endian |
| 16–23 | committed WAL length, u64 little endian |
| 24–55 | SHA-256 of the entire committed WAL prefix, including lengths/header |
| 56–87 | SHA-256 of the preceding seal bytes |

Commit writes the complete WAL batch and commit marker, fsyncs the WAL, writes/fsyncs
a temporary seal, atomically renames it, and fsyncs the directory. Only then can it
acknowledge. The running hash extends with each batch; commit does not reread the WAL.
Recovery validates the seal and exact acknowledged prefix before decoding any WAL
records or modifying files. Missing/truncated/corrupt acknowledged bytes, including
the audit's damaged length prefix and a damaged final marker, refuse without repair.
Only bytes beyond a validated seal are unacknowledged and may be discarded.

Checkpoint first commits, then writes/fsyncs/renames a full snapshot with a new epoch,
fsyncs its directory, and atomically resets WAL and seal. A newer validated snapshot
supersedes the old epoch's WAL and seal after an interrupted rotation. A corrupt or
missing seal is not guessed away. Failures after publication may recover either the
old or complete new operation, and retry receipts resolve that ambiguity. Tests inject
failures at WAL write/fsync and before/after seal publication. Real process-kill tests
cover restart and lost replies. These establish the software/fsync contract; they do
not certify hardware power-loss behavior or detect restoration of an entire coherent
older backup. Checksums are corruption detection, not authentication against a process
that can rewrite all database files.

Version 3 directories are refused without modification. For a trusted v3 snapshot or
export, `Graph::import_v3_snapshot` validates and converts it in memory; export to a
new path to retain the original. It never opens or repairs an ambiguous legacy WAL.
This preserves a graph image, not an automatic mapping to an engine schema or to
Weaver state. Backups of a v1 engine should copy the closed database directory as a
unit, retaining snapshot, WAL, and seal together.

## One bounded combined query

The typed `Query` pipeline is fixed and explicit:

1. Select a type, explicit IDs, or nodes reachable from a known node.
2. Apply all scalar equality/range filters (`AND`).
3. Optionally rank surviving vectors in one declared space, taking `k`.
4. Apply the winner `limit`.
5. Optionally expand each winner's relationships.
6. Project winner and related-record fields/documents.

This directly supports metadata → rank → support relationships → source fields and
reachability → metadata → rank → fields/references. Clients send one request. The
engine executes all selection, scoring, expansion, and projection against one graph.
It does not offer arbitrary operator reordering or filtering after an intermediate
top-k. Filter indexes are owned and maintained by the same mutation/recovery paths.

Unranked results order by node ID; ranked results order by score, with ascending node
ID as tie-break. Dot/cosine are descending; Euclidean distance is ascending. Walks are
breadth-first, include the start in reachability, visit each node once, and retain each
traversed edge once, including cycles and multiple supporting edges. Direction, edge
types, minimum weight, and 0–8 hops are explicit. Related rows carry the complete edge
identity/endpoints/type/weight/properties and depth. Returned document IDs and sequence
preserve provenance within the single database; cross-database identity is deferred.

Resource exhaustion fails the entire request rather than returning a truncated success.
`max_query_work` counts selected/index/adjacency visits; `max_candidates` separately
bounds scored records, and dimensions bound work per record. Projection/output byte
limits are enforced alongside these. A caller may lower the work limit, never raise
the database ceiling. The response does not claim a completeness guarantee beyond the
explicit winner limit, projection, and walk depth requested.

## Resource and failure contract

Defaults: 100,000 user nodes, 500,000 edges, 512 MiB conservative data charge, 8 MiB
batch JSON, 1,024 operations/batch, 100,000 retained receipts, 100,000 query work units,
20,000 candidates, and 4 MiB query output. Maximum batch/result settings are 16 MiB;
schema bounds are 128 node types, 128 lifetime edge types, and 256 indexes. Staging is bounded by eight times the
batch-byte budget and the affected-edge work ceiling. Explicit edge deletion can be
split into batches before deleting a high-degree node.

The data charge accounts conservatively for records, payloads, vectors, metadata,
and index overhead; it is not a hard RSS limit. Allocator capacity, transport buffers,
staging, snapshot encoding, and recovery input add to residency. The service has at
most eight 32 MiB input frames in flight, plus bounded responses and one executing
engine operation. Size the database ceiling with those peaks and model memory in mind.
There is no transparent offload: close/reopen is full unload/reload. Allocation failure
may terminate the process; recovery retains the last acknowledged commit.

Failures distinguish invalid request, not found, conflict, resource limit, ownership
conflict, storage unavailable/uncertain, and corrupt/incompatible storage. Retry a
storage-uncertain write only with its same token after recovery. Malformed framing
closes that connection. A malformed JSON frame returns an error with correlation ID
zero; a parsed unsupported protocol version returns its request ID and supported version.

## Wire protocol and working client

Each frame is a four-byte little-endian length followed by UTF-8 JSON (1–32 MiB).
`Envelope` is `{ "version": 1, "id": N, "request": ... }`. `Response` echoes version
and ID and contains `result: {"Ok": reply}` or `result: {"Err": fault}`. Requests
use an `op` tag; replies use `kind` and optional `value`. Unknown request fields and
operations are refused. Canonical Rust data shapes are `engine/types.rs` and
`drey-local/src/lib.rs`; changing wire meaning requires a protocol-version change.

The socket's parent must be owned by the serving UID with mode 0700; the socket is
0600 and Linux `SO_PEERCRED` admits that same UID. Existing socket cleanup is protected
by its permanent owner-lock file. Do not delete lock files while an owner is live.
Idle/read/write service timeouts are five seconds. `Client` uses 30-second I/O timeouts
and verifies response version/correlation. A transport error leaves a write outcome
uncertain; reconnect and repeat its token. Cancellation is connection closure, not
rollback of an operation already executing. Queries perform bounded work before the
owner can process another request; there is no fairness or decoder-deadline guarantee.

```sh
cargo build --release --workspace --locked
mkdir -m 700 /tmp/drey-demo
target/release/drey-local serve /tmp/drey-demo/db /tmp/drey-demo/engine.sock
# From another shell, using JSON request files:
target/release/drey-local call /tmp/drey-demo/engine.sock request.json
```

`{"op":"status"}`, `{"op":"open"}`, `{"op":"close"}`, and
`{"op":"shutdown"}` are complete request files. Runnable request files are in `drey-local/examples/requests/`: send `create.json`,
`write.json`, `query.json`, `checkpoint.json`, `close.json`, `open.json`, repeat
`write.json` to verify durable retry, and finally `shutdown.json`. Additional examples
are generated by the reproducible client below. Rust consumers use `Client::connect`
and `Client::call(Request)` rather than implementing framing.

```sh
cargo run --release -p harness --bin local_bench -- /tmp/drey-small 1000 256 512
cargo run --release -p harness --bin local_bench -- /tmp/drey-large 10000 1024 4096
```

Build `drey-local` first as above; the benchmark launches the sibling release binary.
Output directories must be new. Each run creates/writes/checkpoints/closes/reopens,
verifies retry identity, and checks intact records and supporting references on every
retrieval. It emits JSON, raw samples, and example request files, then shuts down.
It measures both supported query shapes with and without a separate background
writer; ingestion/write readiness is reported separately from recall latency.
Use a directory on the intended filesystem to measure durability; `/tmp` may be tmpfs.

## Completion and deferred consumer work

Core/service tests cover atomic refusal, updates/deletes/indexes, space mismatches,
quotas, provenance, output exhaustion, concurrent request serialization, ownership,
checkpoint/restart, durable retry, corruption refusal, invalid frames, and storage
failure. Existing graph/invariant/persistence suites remain part of the gate.
The benchmark evidence and its limitations are recorded separately in
`local-engine-v1-evidence/`.

Weaver still chooses data/schema mapping, consolidation/retention, startup ownership,
and the relationship between SQLite session state and DREY memory. Replacement of
SQLite is not a release condition. Query embedding generation and actual context
delivery/inference contention require later integration measurements; no decoder-window
pass is claimed. HADES, multiple databases, cross-database operations, ANN, general
document/query breadth, automatic paging, and distributed/network features are deferred.
