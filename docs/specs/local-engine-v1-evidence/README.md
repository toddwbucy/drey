# Local engine v1: completion evidence

Measured implementation: branch `feat/local-engine-v1`, commit
[`b0ac70b063b0841980654eb39117a2079ed4ef86`](https://github.com/toddwbucy/drey/commit/b0ac70b063b0841980654eb39117a2079ed4ef86),
based on `main` at `7b21aa7e486f6f7f9a2784f7efc66eb99ce8acfc`.
Measurements: 2026-10-18. Subsequent documentation/evidence commits do not change
the measured code. This completes the independently testable engine work requested
in [issue #29](https://github.com/toddwbucy/drey/issues/29#issuecomment-6251033236).
Weaver integration and inference acceptance remain separate gates.

## Implemented boundary and evidence

| Requirement | Implementation | Evidence |
| --- | --- | --- |
| One resident engine | `Engine` owns `Graph`, schema, indexes, sequence and retry receipts in `drey/src/engine/mod.rs` | Query code accesses resident store/indexes; close/open explicitly unloads/reconstructs the database |
| Consistent documents, vectors and relationships | `write.rs::apply`, `contents`, `vector`; existing store mutation/index paths | Engine tests cover full replacements, vector clearing, indexed property changes, edge removal, rejected dangling edges and wrong embedding spaces |
| Atomic batches and durable retry | `Engine::write`, affected-record `stage`, same-commit receipt | Invalid later operations leave no earlier mutation; lost replies and restart return identical receipts; reused tokens with different contents conflict |
| Acknowledged history cannot become a torn tail | `persist/seal.rs::verify`, `WalPersistence::commit`, format 4 | Damaged acknowledged frame lengths, header/final-marker corruption and missing bytes refuse without truncation; unacknowledged tails recover |
| One bounded combined retrieval | `Engine::query`, `walk`, `project` in `engine/query.rs` | Both representative query shapes, exact ordering, models with equal dimensions, multiple supporting edges, projection/output/work bounds |
| Versioned Unix-only lifecycle | `drey-local::{Envelope, Request, Client, serve}` | Real child-process tests cover create/open/close/checkpoint/shutdown, restart, invalid frames/fields/version, lost replies, storage failure and serialized background writes |
| Reproducible usable client | CLI plus `drey-local/examples/requests/`; `harness/src/bin/local_bench.rs` | CLI lifecycle verified; every measured retrieval checks IDs, full document text/nested values, edge identity and source URI |

The existing storage/index/codec foundation is retained. The focused changes are
the durable acknowledgement boundary, an atomic owner API, bounded query execution,
and the optional service. No ArangoDB code was imported or translated.

## Correctness gate

All **157 tests passed**: 43 library unit tests, 38 graph invariant tests, 43
persistence integration tests, 9 engine integration tests, 4 real socket-service
tests, and 20 harness tests. Build, formatting and clippy passed:

```sh
cargo fmt --all --check
cargo build --workspace --all-targets --locked --offline
cargo test --workspace --locked --offline
cargo clippy --all-targets --locked --offline -- -D warnings
cargo build --release --workspace --locked --offline
```

Offline execution used the local dependency cache. Socket tests require permission
to create Unix sockets. CI can run the same checks without `--offline` to obtain
locked dependencies. The release CLI was additionally exercised using the supplied
JSON files: create → write → query → checkpoint → close → open → identical write
retry → shutdown. The document string and source reference were intact and the
retry returned exactly the original receipt.

Recovery tests inject failures during WAL write/fsync and before/after seal
publication; the engine refuses all access to uncertain state until reopen.
Process tests also kill/restart the service and deliberately lose a reply. They
establish the implemented software/fsync contract, not certification of physical
power-loss behavior or protection against restoring an entire coherent old backup.

## Measurement method and fixtures

Hardware: AMD Ryzen Threadripper 7960X, 24 cores/48 logical CPUs, 248.91 GiB reported
system RAM, Linux x86-64, Rust 1.98.1, release profile. The synthetic databases were
on **ZFS**, not tmpfs. Backing-device latency and background OS load were uncontrolled.
No model-inference workload or decoder interval was supplied. Exact binary hashes
and environment fields are in [environment.json](environment.json).

| Fixture | Passages / vectors | Source nodes | Supporting edges | Dimensions | Text per passage | Scored candidates |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Small | 1,000 | 1 | 1,000 | 256 | 512 bytes | 200 |
| Representative | 10,000 | 1 | 10,000 | 1,024 | 4,096 bytes | 2,000 |

Each passage has `group = index % 5`, an indexed scalar property, a nested JSON
document, a supplied synthetic vector, and an outgoing `supports` edge to the source.
The query selects group zero. Dot-product scores are deterministic and independently
predictable from passage indices; these vectors test execution, not semantic relevance.

Both shapes return the same top eight passages, eight supporting edges and source
references, with every requested field and all document text:

1. Select passages → metadata restriction → exact ranking → supporting relationships
   → full document and source projection.
2. Traverse inbound from the source → metadata restriction → exact ranking → supporting
   relationships → full document and source projection.

Each bucket has 100 warmups and 1,000 retained samples; percentiles use nearest rank.
One final run per fixture was recorded. Raw nanosecond samples and all summaries are
in [small.json](small.json) and [representative.json](representative.json).

Timing starts before request cloning/serialization and ends after response parsing
and content/provenance assertions. It includes socket transport, owner-lock queueing,
engine execution and server serialization. It excludes connection establishment,
query embedding generation, inference and eventual Weaver context delivery. The
background client replaces one nonmatching passage, supplies its vector/document,
waits for durable acknowledgement, then sleeps 1 ms. Its writes serialize with reads.

## Retrieval results

All times below are **milliseconds**. Sampled JSON responses were 7,677–7,680 bytes
for small and 36,389–36,392 bytes for representative, plus the four-byte frame prefix.
Sequence/correlation digit lengths can change response size slightly during a run.

| Fixture | Query | Background writes | p50 | p95 | p99 |
| --- | --- | --- | ---: | ---: | ---: |
| Small | Metadata + rank + support | No | 0.108 | 0.119 | 0.227 |
| Small | Reachability + metadata + rank + support | No | 0.227 | 0.243 | 0.450 |
| Small | Metadata + rank + support | Yes | 0.112 | 2.407 | 2.531 |
| Small | Reachability + metadata + rank + support | Yes | 0.238 | 2.577 | 2.744 |
| Representative | Metadata + rank + support | No | 1.750 | 2.181 | 2.420 |
| Representative | Reachability + metadata + rank + support | No | 3.144 | 3.264 | 3.683 |
| Representative | Metadata + rank + support | Yes | 4.187 | 4.692 | 4.911 |
| Representative | Reachability + metadata + rank + support | Yes | 5.863 | 6.148 | 6.886 |

All 8,000 retained retrievals passed content/reference assertions, as did 800 warmups.
Background writes increase retrieval tails even though queries use RAM: the sole
owner also performs synchronous durable commits. RAM residency alone provides no
decoder-window guarantee. Deadline and deadline-miss fields are explicitly null.
These fixtures and code differ from the earlier audit/disposable host, so these
numbers are not a speedup comparison.

## Ingestion readiness, reload and memory

Ingestion batches contain up to 32 passage nodes and 32 edges, committed atomically.
The one source-creation write is excluded from ingestion distributions. Background
write distributions include warmup-period writes, so they have independent sample counts.

| Operation | Samples | p50 ms | p95 ms | p99 ms |
| --- | ---: | ---: | ---: | ---: |
| Small ingestion | 32 | 3.432 | 4.094 | 4.183 |
| Representative ingestion | 313 | 6.002 | 6.962 | 7.304 |
| Small background write, filtered queries | 121 | 2.337 | 2.456 | 2.540 |
| Small background write, reachable queries | 240 | 2.353 | 2.515 | 2.812 |
| Representative background write, filtered queries | 1,100 | 3.147 | 3.653 | 3.965 |
| Representative background write, reachable queries | 1,100 | 4.819 | 5.166 | 5.818 |

Reopen after checkpoint took **5.975 ms** for small and **203.224 ms** for
representative (one measurement each, OS caches not flushed). These are readiness
measurements, not costs added to a query against an already resident database.

| Server memory | Small MiB | Representative MiB |
| --- | ---: | ---: |
| RSS after ingestion/checkpoint/reopen, before queries | 8.87 | 100.86 |
| RSS after queries/background writes | 9.46 | 103.62 |
| Process high-water RSS | 9.46 | 181.43 |
| Final conservative engine data charge | 5.93 | 218.72 |

RSS and high-water values come from the service process's `/proc` status; they
exclude the client and filesystem cache. The high-water includes ingestion,
checkpoint encoding and reload, and does not isolate each phase. Engine data charge
is an accounting limit, not measured RSS. Final receipt counts were 394 and 2,514;
these grow under the explicitly bounded durable retry contract.

## Reproduction

Build both binaries, then choose **new directories on the intended filesystem**:

```sh
cargo build --release --workspace --locked
target/release/local_bench /path/to/new-small 1000 256 512
target/release/local_bench /path/to/new-representative 10000 1024 4096
```

The harness starts the actual service, creates deterministic data, checkpoints,
closes/reopens, verifies durable retry, runs the four retrieval buckets, writes
`results.json` plus example request files, and shuts down. No production data is
needed. Do not use `/tmp` without checking its filesystem when measuring durability.

## Limits and remaining integration gates

The [v1 contract](../local-engine-v1.md) defines a deliberately bounded engine, not
an unrestricted document/query system. It offers whole JSON document preservation
and property projection; nested-path predicates, arbitrary operator reordering,
ANN, automatic paging, multiple databases and cross-database identity are deferred.
Lifetime type and receipt capacities are explicit refusals. WAL history grows until
checkpoint and is read into memory on recovery; callers must schedule checkpoints.
RAM charging is not a hard process-memory guarantee.

Format 4 is incompatible with v3 directories. They are refused unchanged; a
trusted v3 snapshot/export can be explicitly converted with `Graph::import_v3_snapshot`.
That preserves the graph image and IDs but does not infer a new engine schema or
recover ambiguous legacy WAL history. Do not point a new service at an old directory
and expect automatic migration.

There is no known unfinished item in the independently specified v1 implementation.
The next gates depend on actual use and do not imply that SQLite should be replaced:

1. Map real Weaver reads/writes, data meaning and lifecycle onto this tested contract;
   keep SQLite/DREY responsibility and retention policy explicit.
2. Replace synthetic vectors/documents with a captured workload and expected context.
   Measure query embedding separately if it is on the critical path.
3. Run repeated retrieval distributions under actual inference and write load against
   the measured decode interval, preserving the same content/reference assertions.

The smallest additional operational experiments are repeated runs on the intended
laptop/storage, allocator and per-phase memory sampling near configured capacities,
and restart after a long WAL interval to choose a checkpoint cadence. A controlled
power-loss campaign would test the storage stack beyond the current fsync/fault tests.
No inference deadline, hardware power-loss certification, or cross-application
integration claim follows from the present measurements.
