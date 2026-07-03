# PRD §24 Open-Questions Ledger

Status: 2026-07-03. The PRD (v0.8) is immutable; decisions land as specs
beneath it (project convention). This ledger dispositions every §24 open
question so none is settled-by-implementation without a written record —
the failure mode CLAUDE.md warns about ("open questions that block a piece
of implementation must be converted to a decision or milestone gate first").

| Q | Status | Disposition |
|---|--------|-------------|
| 1 | **Decided (M2)** | WAL + snapshots, single implementation behind the `pub(crate) Persistence` trait. The embedded-KV alternative was declined: the M3 gate passes without it, and the seam keeps it reachable without API change. See `specs/m2-durability.md`, `specs/persistence-seam-decision.md`. |
| 2 | **Decided (M1)** | `f32` weights throughout — storage, `WeightUpdate`, export. No `f64` internal path. Rationale: no consumer precision demand; byte-exact `f32` round-trip is already a codec commitment; doubling weight width is resident-memory cost (commitment 4) with no identified beneficiary. Non-finite weights are rejected at every write boundary (add, update result, decay factor), so the precision question cannot be complicated by NaN propagation. Revisit trigger: a real consumer workload demonstrating `f32` accumulation error that matters. |
| 3 | Closed (v0.6) | No core timestamps. Recorded in the PRD itself; unchanged. |
| 4 | Closed (v0.8) | No external addressing. Recorded in the PRD itself; unchanged. |
| 5 | **Decided (M4)** | Full-graph export only. `GraphFeatureExport` ships counts, dense index map, feature matrix (rectangular, zero-padded), edge index, edge weights, edge types, and node type ids. Neighbor sampling is a consumer-side operation over the exported arrays — sampling policy is motive, not mechanic (commitment 1). Revisit trigger: an export too large to materialize, which would make sampling a mechanics question. |
| 6 | **Decided (M5)** | Query-language ownership deferred; read-only `PropertyGraphRead` seam shipped. See `specs/m5-query-seam-decision.md`. |
| 7 | **Decided (M1/M3)** | Cosine, dot, euclidean. All three exercised by the measurement workload; no consumer demand for more. Adding a metric is a closed enum extension behind the `SimilarityEvaluator` seam — cheap to revisit on a real capture. |
| 8 | **Decided (M2)** | Neither MessagePack/CBOR nor bincode: a hand-rolled, schema-bound, little-endian codec (`persist/codec.rs`). Rationale: serde is the crate's only permitted dependency and the serde-codec crates would add more (commitment 4); byte-exact `f32`/`Bytes` fidelity and CRC framing needed hand control anyway; self-description buys schema-churn tolerance the versioning policy (no migration, `VersionMismatch` + export/import) deliberately does not promise. Hardened by the issue #5 audit (length-guarded reads, checked arithmetic, known-answer CRC test). |
| 9 | **Decided (M2)** | Drop-without-commit silently discards uncommitted mutations; next `open` loads the last committed state; no rollback verb. A failed durable write poisons the persister (writes refuse until reopen) so partial batches can never be half-published. See `specs/m2-durability.md`. |
| 10 | Declined (v0.5), unchanged | No `open_or_create`. No consumer has yet hand-composed the third case repeatedly; the revisit condition has not fired. |
| 11 | **Resolved by M2 design** | Single `commit` verb retained. The WAL+snapshot design never needed a `commit`/`flush` split: `commit` is the one durability point, and `snapshot()` is compaction, not durability. The v0.5 rationale (don't let API surface pre-decide the §10.3 measurement) played out as intended. |
