# M2 Durability & Persistence — Decision Record

Status: retroactive record, 2026-07-03. The M2 implementation (WAL + snapshots,
hand-rolled binary codec) shipped with its design stated only in
`drey/src/persist/mod.rs` doc comments; this spec lifts the decisions into the
documentation layer the PRD (§21 M2 exit criteria) expects. It reflects the
code as of `FORMAT_VERSION 3` (post issue #5 remediation and PR #17). Where
this document and the code disagree, the code plus its tests are normative and
this document has rotted — fix it.

## Durability level (PRD §21 M2 exit criterion)

`commit` is **fsync-backed crash durability**: it encodes the buffered
mutations, appends them to the WAL with a terminating commit marker, and
`fsync`s the file before returning `Ok`. A mutation that has not been through
`commit` is not durable and is silently discarded by the next `open` (see
"Drop semantics" below). This satisfies the standing consumer requirement of
at least one fsync-backed durability point at the turn/consolidation boundary.

Supporting guarantees, all load-bearing for the level above:

- `create` fsyncs the graph directory's **parent**, so a newly created graph
  survives power loss (a POSIX `mkdir` is a metadata change to the parent).
- `snapshot` writes to a temp file, fsyncs it, atomically renames, and fsyncs
  the directory — a crash leaves the old or new snapshot, never a partial one.
- `export` uses the same temp+fsync+rename discipline, so a failed export
  cannot destroy the previous export at the destination path.

## Failure semantics: poison, not limp

A `WalPersistence` that experiences a failed durable operation (torn WAL
write, failed fsync, post-cutover snapshot failure) **poisons itself**:

- Reads keep working against the in-memory graph.
- Every subsequent mutation and `commit` refuses with a typed error until the
  graph is reopened. Mutations are refused **before** the in-memory store is
  touched (`Persistence::preflight`), so RAM never diverges from what the log
  can replay.
- A retried `commit` after a failure can never return a false `Ok`: the
  pending buffer is only cleared after write+fsync succeed.

Rationale: a durable-write failure leaves the on-disk tail in an unknown
state; continuing to append would strand acknowledged commits behind torn
bytes (issue #5, critical finding). Recovery is reopen: `open` repairs the
WAL to its last committed frame.

The poison paths are exercised by `cfg(test)` fail-points (torn WAL write,
WAL fsync, post-cutover dir fsync) — compiled out of release builds, no
public surface (issue #10 / PR #15).

## On-disk format

A graph directory contains `wal.log`, optionally `snapshot.bin`, and `LOCK`.
Exact byte layout is normative in `persist/codec.rs` and `persist/mod.rs`;
the structure:

- **WAL header (v3, 20 bytes):** magic `DREY` + format version (u32 LE) +
  epoch (u64 LE) + CRC32 over the preceding 16 bytes. The epoch is the
  snapshot generation the WAL belongs to and is the sole discriminator
  between replay and discard-as-stale — which is why it is CRC-protected.
- **WAL frames:** length (u32 LE) + CRC32(payload) + payload. Mutation frames
  carry one encoded `Mutation` each; a batch is terminated by a commit-marker
  frame. On replay, only fully committed batches apply; a torn or CRC-bad
  tail is truncated by the next writable open. Frame payloads ≥ 4 GiB are
  rejected at write time (the length prefix is u32).
- **Snapshot:** magic + version + epoch, interner label tables, per-type
  embedding dims, index registrations, then all node and edge records with
  **explicit IDs**, and a trailing CRC32 over the payload (v2+). Type ids in
  records are validated against the label tables at load — corruption is a
  typed `Codec` error at `open`, never a deferred panic.
- **IDs:** records store `NodeId`/`EdgeId` explicitly, never by array
  position; replay applies mutations with their recorded ids and restores the
  allocators past the maximum seen (PRD §7.4 / design commitment 5).

Encoding is little-endian, schema-bound, hand-rolled (see the codec decision
in `docs/specs/open-questions-ledger.md` Q8). `f32` and `Bytes` round-trip
byte-exactly, including NaN payloads and denormals (known-answer tests pin
CRC32 and the float round-trip).

## Format versioning & migration policy

`FORMAT_VERSION` history:

| v | Change | Shipped |
|---|--------|---------|
| 1 | Initial WAL+snapshot format | M2 |
| 2 | Snapshot trailing CRC | PR #9 (issue #5 remediation) |
| 3 | WAL header CRC (epoch protection) | PR #17 (ultrareview) |

**There is no in-place migration.** `open` on an older-version graph fails
explicitly with `VersionMismatch`; the upgrade path is export from a build
that reads the old format, import into the new. This is a deliberate v0.x
posture — migration machinery is weight (design commitment 4) that
pre-release format churn does not justify. Revisit before the first release
that promises format stability.

## Locking

Single-writer enforcement (`GraphConfig.file_lock`, default off) uses OS
advisory locking — `flock(2)` on Unix, `LockFileEx` on Windows, via
`File::try_lock` — on a permanent `LOCK` anchor file that is never deleted.
The kernel releases the lock on fd close, including SIGKILL/OOM-kill/power
loss, so a hard crash cannot wedge the graph (issue #8 / PR #14 replaced the
original PID-file scheme, which could).

## Drop semantics (PRD §24 Q9, decided)

There is no rollback verb and no `Drop` flush. Dropping the `Graph` (or
crashing) with accumulated uncommitted mutations **silently discards them**:
the next `open` loads the last committed state. `commit` is the only publish
point. Consumers that need turn-boundary durability call `commit` at the turn
boundary — this is the contract the durability level above is written
against.

## Recovery matrix (PRD §10.2.1) — test anchors

All rows are asserted in `drey/tests/m2_persistence.rs`: crash-before-commit,
torn tail, corrupt frame (CRC byte-flip), corrupt snapshot (truncation and
payload flip), stale-WAL-after-snapshot-crash, missing-snapshot-with-newer-WAL
(explicit refusal, never a silent partial load), version mismatch (including
the legacy 16-byte v2 WAL), torn header recovery, replay of every mutation
tag, and full pre-crash vs post-reopen state equivalence including IDs.
