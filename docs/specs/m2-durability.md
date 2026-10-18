# M2 Durability & Persistence - Decision Record

## Current format: v4 commit seal (2026-10-18)

The [local-engine contract](local-engine-v1.md#persistence-format-4-and-recovery)
supersedes the historical v1-v3 tail-classification rules below. A mandatory,
checksummed `commit.seal` records the epoch, acknowledged WAL length, and SHA-256
of that exact prefix. Commit fsyncs the WAL, atomically publishes/fsyncs the seal,
and only then acknowledges. Recovery validates the complete prefix before replay
or truncation. Corrupt acknowledged lengths, payloads, final markers, or headers
now refuse without changing history; only bytes beyond a valid seal are discarded.
PR #30 resolves the length-prefix loss from issues #26 and #29. Frame CRCs remain
payload-only; the seal supplies protection for the complete committed byte sequence.
`acknowledged_wal_length_corruption_refuses_without_modifying_history` in
`drey/tests/engine.rs` covers scan derailment;
`acknowledged_wal_length_realignment_refuses_without_modifying_history` in
`drey/tests/m2_persistence.rs` covers swallowing a frame and landing on the final
marker, with unchanged WAL, snapshot, and seal after writable and read-only refusal.
Version-3 directories are refused explicitly; `Graph::import_v3_snapshot` provides
a read-only snapshot conversion bridge without guessing about legacy WAL acknowledgements.

Open first reads the snapshot prefix, WAL header and validated commit seal. A
seal newer than the snapshot prefix rejects before record decoding, including
every read-only retry. This is a rejection-only optimization: a damaged snapshot
prefix can also cause `GenerationMismatch`. Accepting a snapshot or skipping a
stale WAL still requires the complete snapshot checksum and structural validation;
replay still requires the sealed WAL hash. Header and body reads use the same file
handle so a concurrent rename cannot mix snapshot images.

The sections below retain the original design and format evolution, with current
platform and migration notes. Classification based on following frames alone is
historical v1-v3 behavior; the v4 seal governs current recovery.


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

### Platform durability scope (current v4; issue #26)

The local service targets Linux. On Unix, `fsync_dir` synchronizes the containing
directory after publication; the crash-durability contract assumes the filesystem
and device honor file and directory synchronization. CI exercises Linux, and the
failure-injection/process-kill tests do not certify hardware power-loss behavior.

On non-Unix targets, `fsync_dir` returns success without synchronizing directory
metadata. The core does not enforce a platform refusal, but equivalent crash
durability is not established. Two publication failures matter:

- Losing a snapshot rename after replacing the WAL and seal can leave
  `snapshot(N) + WAL/seal(N+1)`. Recovery refuses the mismatched generations, but
  cannot reconstruct pre-checkpoint commits whose old WAL has been replaced.
- Losing a newer seal's publication while retaining its WAL bytes can leave a
  valid older seal in the same epoch. Recovery accepts that older prefix and
  treats the newer bytes as unacknowledged; writable open truncates them. The
  seal checksum does not establish that the latest acknowledged seal survived.

Thus v4 depends on durable seal publication on every nonempty commit. Avoiding
snapshots and using WAL-only writes does not remove this dependency. No filesystem
journal is assumed to supply ordering missing from the implementation.

Issue #26's length-corruption objective is satisfied by PR #30. Its remaining
platform work is deferred until a non-Unix consumer or a release promising format
stability, whichever comes first. Epoch-named WAL files remain a candidate for
retaining pre-checkpoint history, but require a complete snapshot/WAL/seal
publication protocol, recovery-generation selection, safe pruning, and platform
crash tests before equivalent durability can be claimed. They do not by themselves
solve ordinary commit-seal publication. No additional format bump is required for
the already protected length prefixes.

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

  **Torn tail vs. corruption (issue #22 item 7):** every damaged frame is
  judged by evidence *after its own batch* - raw bytes, parsed or
  torn-fragment alike - because batch N+1's bytes exist on disk only if
  batch N's fsync returned. A **marker-sized** damaged frame (1-byte payload
  - the only size `commit()` writes for a marker) is a damaged commit
  marker whose batch ends at that frame: any raw bytes beyond it refuse. A
  **mutation-sized** damaged frame's batch ends at the first trusted marker
  at/after it: raw bytes beyond that *marker* refuse (the marker's own
  presence proves the write reached disk, not that its fsync returned). This
  covers the final marker - the most likely tear victim, since the next
  append's shared-block write lands on its block - and the
  no-surviving-marker case. Zero-length frames are structural damage (no
  writer emits them; a zeroed torn page parses as a self-validating run of
  them since `crc32("") == 0`). Damage that no evidence condemns is a torn,
  unacknowledged commit: the final batch (or tail) is discarded/truncated -
  crash recovery. Refusals leave the file untouched.
  The repair truncation is in-place (`set_len`), so the discarded
  unacknowledged tail is not preserved; copy `wal.log` aside before a
  writable open if the torn tail has forensic value.
  **Historical v3 limit:** length prefixes are outside the frame CRC, so prefix damage
  has two forms: scan *derailment* (everything beyond is unreachable and
  treated as torn tail), and - rarer, needing the mutated length to land
  exactly on a later frame boundary - clean *realignment* that swallows a
  following frame and can classify acknowledged damage as a torn final
  commit, silently truncating it. Issue #26 proposed extending the frame CRC or
  adding per-frame sequence numbers. V4 instead verifies the complete sealed
  prefix before this classifier runs, closing both forms of acknowledged loss.
  Threat-model note: appending batch N+1 rewrites the final filesystem block
  shared with batch N's fsynced tail, so on media without powersafe
  overwrite (SQLite's `POWERSAFE_OVERWRITE` concern) an ordinary power loss
  can corrupt acknowledged bytes in that shared block - which this
  classifier will then refuse as corruption (or, if the tear hits N's
  marker, refuse as a damaged marker). The refusal is the safe outcome, but
  on such media it can follow a plain crash, not just bit rot. A further
  footnote on the same media class: the in-place `set_len` repair leaves
  discarded frame bytes beyond the shrunken length, and markers carry no
  batch sequence - a later same-length tear could in principle resurrect a
  stale marker; multiply-coincidental, recorded for completeness.

  **Replay compatibility:** one pre-hardening artifact is tolerated rather
  than refused - `DecayEdges` frames whose filter carries a NaN
  `min_weight`, which older writers of this same FORMAT_VERSION legally
  committed. The filter matched nothing then (`w >= NaN` is false) and
  replay reproduces that no-op; the live `decay_edges` entry rejects NaN at
  the source so new frames never carry it.

  **WAL reset recipe:** every WAL re-header (create, stale-WAL repair, and
  the snapshot cutover) goes through tmp+fsync+rename (`reset_wal`), not an
  in-place truncate+write - in-place, the equal-length new header could
  persist while the shrink's metadata did not, leaving residual old-epoch
  frames frame-aligned and replayable under the new epoch.

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
| 4 | Mandatory commit seal: epoch, committed WAL length and SHA-256, including frame lengths | PR #30 (local engine) |

**There is no in-place migration.** `open` on an older-version graph fails
explicitly with `VersionMismatch`. For a trusted v3 snapshot/export,
`Graph::import_v3_snapshot` validates and converts the image in memory; export to
a new path to preserve the original. It does not replay or repair a legacy WAL,
or map a graph image into an engine schema. Ordinary `import` does not migrate
older formats. This remains a deliberate v0.x posture; revisit migration
machinery before the first release that promises format stability.

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
when their reads race a snapshot rotation they observe a WAL one generation
ahead of the snapshot, and a read racing a writable open's WAL repair can
capture a byte blend that classifies as corruption; both retry a bounded
number of times before the typed error stands (a benign blend loads cleanly
on re-read; genuine corruption fails identically every time). Every state a pass *can* load is internally consistent (an
equal-epoch snapshot+WAL pair, or a stale-skipped snapshot alone); the
read-only guarantee is consistency, not freshness - a load racing a rotation
may serve committed state up to one rotation old, which is inherent to
reading without the writer lock.

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
