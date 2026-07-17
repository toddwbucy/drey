# M2 Durability & Persistence - Decision Record

Status: retroactive record, 2026-07-03; revised 2026-07-16 for the issue #22 /
repo-review hardening pass (recovery-matrix corrections, snapshot-load
validation, open-time locking order, export preconditions). The M2
implementation (WAL + snapshots, hand-rolled binary codec) shipped with its
design stated only in `drey/src/persist/mod.rs` doc comments; this spec lifts
the decisions into the documentation layer the PRD (§21 M2 exit criteria)
expects. It reflects the code as of `FORMAT_VERSION 3`. Where this document
and the code disagree, the code plus its tests are normative and this document
has rotted - fix it.

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
  the directory - a crash leaves the old or new snapshot, never a partial one.
- `export` uses the same temp+fsync+rename discipline, so a failed export
  cannot destroy the previous export at the destination path.

Platform note: directory fsync is a POSIX durability point; on non-Unix
targets `fsync_dir` is a no-op (Windows cannot open a directory via
`File::open`, and NTFS handles rename durability through its journal). File
contents are still fsynced everywhere; the rename-durability guarantee above
is formally Unix-only.

### Export preconditions (issue #22 item 4 / review)

`export` is the backup verb: its image must never disagree with what a reopen
would load, and its atomic rename must never land on a live graph file.
A file-backed graph therefore refuses to export when:

- the persister is **poisoned** (in-memory state may be ahead of anything
  replayable), or
- the graph is **dirty** - mutations appended since the last `commit`
  (commit first, then export), or
- the destination **aliases** `wal.log`, `snapshot.bin`, `snapshot.bin.tmp`,
  or `LOCK` - directly, through a relative/symlinked path, or (on Unix, by
  dev+inode) a hard link. This applies to read-only opens too.

Exported images carry the epoch the graph was loaded at, including from
read-only opens (which attach no persister) - never a fabricated epoch 0,
which used beside a surviving epoch-N WAL would double-apply non-idempotent
mutations on a subsequent open.

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
WAL fsync, post-cutover dir fsync) - compiled out of release builds, no
public surface (issue #10 / PR #15).

## On-disk format

A graph directory contains `wal.log`, optionally `snapshot.bin`, and `LOCK`.
Exact byte layout is normative in `persist/codec.rs` and `persist/mod.rs`;
the structure:

- **WAL header (v3, 20 bytes):** magic `DREY` + format version (u32 LE) +
  epoch (u64 LE) + CRC32 over the preceding 16 bytes. The epoch is the
  snapshot generation the WAL belongs to and is the sole discriminator
  between replay and discard-as-stale - which is why it is CRC-protected.
- **WAL frames:** length (u32 LE) + CRC32(payload) + payload. Mutation frames
  carry one encoded `Mutation` each; a batch is terminated by a commit-marker
  frame. On replay, only fully committed batches apply. Frame payloads ≥ 4 GiB
  are rejected at write time (the length prefix is u32).

  **Torn tail vs. corruption (issue #22 item 7):** damage is classified by
  where it sits relative to the fsync barriers commits create - batch N+1's
  bytes exist on disk only if batch N's fsync returned. Damage confined to
  the *final* batch (torn frame at EOF, unmarked frames, or a CRC-bad frame
  whose commit marker ends the file) is indistinguishable from a torn
  unacknowledged commit and is discarded/truncated: crash recovery. A CRC-bad
  frame with any bytes after its batch's commit marker sits in an
  *acknowledged* batch; open refuses with a typed `Codec` error and leaves
  the file untouched, rather than silently truncating durable history.
  Known limit: damage to a frame's *length prefix* mid-file can derail the
  structural scan itself, in which case everything beyond it is unreachable
  and treated as torn tail; distinguishing that would require per-frame
  sequence numbers (a format change - revisit if it ever bites).

  **Generation gate (issue #22 item 8):** the WAL replays only when its epoch
  **equals** the snapshot's. An older WAL is the legitimate
  crash-mid-snapshot leftover and is skipped as stale; a *newer* WAL is a
  typed `GenerationMismatch` (a backup-restored older snapshot, or a
  lock-free read racing a rotation) - replaying it would blend generations.
- **Snapshot:** magic + version + epoch, interner label tables, per-type
  embedding dims, index registrations, then all node and edge records with
  **explicit IDs**, and a trailing CRC32 over the payload (v2+).

  **Load-time validation (issue #22 items 2, 5):** the image is decoded into
  a scratch store and installed only after full validation - the CRC proves
  the bytes are what was written, not that what was written is well-formed.
  Rejected with typed `Codec` errors: duplicate interner labels, duplicate
  node/edge ids, out-of-range or undeclared type ids, dangling edge
  endpoints, embedding dim mismatches and non-finite components, non-finite
  edge weights, allocators that do not clear the loaded ids, and undecoded
  bytes before the trailing checksum. WAL replay applies the same discipline:
  every arm routes through the store's validated mutation paths, so a frame
  targeting a missing id or an unregistered type is a typed error, never a
  silent no-op.
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
posture - migration machinery is weight (design commitment 4) that
pre-release format churn does not justify. Revisit before the first release
that promises format stability.

## Locking

Single-writer enforcement (`GraphConfig.file_lock`, default off) uses OS
advisory locking - `flock(2)` on Unix, `LockFileEx` on Windows, via
`File::try_lock` - on a permanent `LOCK` anchor file that is never deleted.
The kernel releases the lock on fd close, including SIGKILL/OOM-kill/power
loss, so a hard crash cannot wedge the graph (issue #8 / PR #14 replaced the
original PID-file scheme, which could).

Ordering (issue #22 item 3): a writable `open` acquires the lock **before**
reading either persistence file - read-then-lock was a TOCTOU where a
concurrent writer's commit, landed between the read and the lock, was
truncated away by the post-lock WAL repair. Read-only opens take no lock;
instead they verify the snapshot generation was stable across their reads
and retry a bounded number of times on an observed rotation (an unstable
epoch or a WAL one generation ahead), then surface a typed error.

## Drop semantics (PRD §24 Q9, decided)

There is no rollback verb and no `Drop` flush. Dropping the `Graph` (or
crashing) with accumulated uncommitted mutations **silently discards them**:
the next `open` loads the last committed state. `commit` is the only publish
point. Consumers that need turn-boundary durability call `commit` at the turn
boundary - this is the contract the durability level above is written
against.

**ID reallocation corollary (2026-07 review finding, dispositioned):** ids
returned for mutations that are later discarded by an uncommitted close do
not survive it - the allocators resume from committed state, so a reopened
graph can hand the same `NodeId`/`EdgeId` to a different entity. Design
commitment 5 ("ids survive close/reopen exactly") is a guarantee about
**committed** entities; an id obtained since the last `commit` is invalidated
by drop, and a consumer that retains one must treat `commit` as the point at
which it becomes durable identity. This is inherent to memory-primary +
commit-only durability: preventing reuse would require a durable write at
*allocation* time, i.e. a disk touch on the mutation path, which design
commitment 2 forbids. Pinned by
`uncommitted_ids_are_reallocated_after_drop_without_commit`.

## Recovery matrix (PRD §10.2.1) - test anchors

All rows are asserted in `drey/tests/m2_persistence.rs` (integration) and
`drey/src/persist/mod.rs` (malformed-image unit suite): crash-before-commit,
torn tail, corrupt frame in the final batch (recovers as torn commit),
**corrupt frame in an acknowledged batch (refuses, file untouched)**, corrupt
snapshot (truncation and payload flip), the full malformed-snapshot
validation suite (duplicate labels/ids, dangling endpoints, unsafe
allocators, dim mismatches, non-finite values, trailing bytes),
stale-WAL-after-snapshot-crash, **newer-WAL-than-snapshot (generation
mismatch, both writable and read-only paths)**,
missing-snapshot-with-newer-WAL and **missing-WAL-beside-snapshot** (explicit
refusals, never a silent partial load), torn-WAL-re-header-after-snapshot
(recovers at snapshot state), version mismatch (including the legacy 16-byte
v2 WAL), torn header recovery, replay of every mutation tag, replay of
logically inconsistent WALs (missing targets, unregistered types, non-finite
weights, duplicate ids - typed errors), stricter-config reopen, export
preconditions (dirty, poisoned, aliased destinations, read-only epoch), and
full pre-crash vs post-reopen state equivalence including IDs.
