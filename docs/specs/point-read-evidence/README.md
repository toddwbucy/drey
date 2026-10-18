# Indexed point-read investigation (#21)

Recorded 2026-10-18. The historical consumer configuration explains the original
scan; a separate scan in the newer engine is fixed here. This change implements
the scalar-index requirement in PRD section 8 for `Engine::query` and preserves
the bounded-query contract in [local-engine-v1.md](../local-engine-v1.md).

## Historical diagnosis

The consumer benchmark behind [#21](https://github.com/toddwbucy/drey/issues/21)
used DREY `c94b6cf813d60724244ad4abe0584aead465770a`. At archived consumer commit
`0de7ddce1b99adb5d82d42df0338333e86aae67e`:

- [`DreyStore::open_or_create`](https://github.com/toddwbucy/WeaverTools-archived/blob/0de7ddce1b99adb5d82d42df0338333e86aae67e/crates/weaver-memory/src/backend_drey.rs#L644)
  sets `file_lock` and otherwise uses `GraphConfig::default()`, without property
  indexes. Its in-memory constructor also uses the default configuration.
- [`find_node`](https://github.com/toddwbucy/WeaverTools-archived/blob/0de7ddce1b99adb5d82d42df0338333e86aae67e/crates/weaver-memory/src/backend_drey.rs#L424)
  queries `_key` within the collection's node type, then materializes the match.
  DREY consequently takes its unindexed type-scan fallback. This conflicts with
  the consumer's [section 4.1 indexed-lookup promise](https://github.com/toddwbucy/WeaverTools-archived/blob/0de7ddce1b99adb5d82d42df0338333e86aae67e/docs/architecture/weaver-memory/drey-backend-Spec.md#L114).
- The [corrected historical report](https://github.com/toddwbucy/WeaverTools-archived/blob/0de7ddce1b99adb5d82d42df0338333e86aae67e/docs/project/reports/drey-validation-gate-2026-07-05.md#L80)
  replaces the issue's 37/507 microsecond p50 pair with 8/494 at 100/10,000 nodes.
  The original read fixtures had been enlarged by a preceding commit benchmark.
  Those figures are historical reported measurements, not rerun here.

No failed equality index or corrupt identity mapping was demonstrated. DREY's
native `Graph::node(NodeId)` is a hash lookup; configured property equality uses
`BTreeMap::get`. The full node read also clones fields and embeddings, so its total
cost can depend on payload/vector size even though it performs no vector search.
The adapter configuration explains the population-dependent scan term.

These are historical findings, not a proposal to restore that adapter or integrate
the current engine with Weaver. No consumer or ArangoDB code was imported.

## Current engine change

Before this fix, `Engine::query` selected an indexed filter but iterated every
distinct value in its index, checking the predicate on each. The existing lower-
level graph query already used direct equality seeks and bounded range iteration.

The engine now uses the same scalar ordering for `get` and inclusive `range`
operations. Inverted ranges return empty without panicking. It borrows each bucket
and checks the work limit during candidate visits, before accumulating results.
All AND filters, ranking, winner limits, projection, and recovery ownership remain
in the existing pipeline. The first indexed filter remains the seed; cost-based
selection, compound indexes, and unique-key constraints are deferred.

For `D` distinct values, `B` matching range buckets and `M` visited candidates,
equality selection costs `O(log D + M)` and range selection `O(log D + B + M)`.
Later filters, ranking, sorting, projection, and serialization add their own costs.
The `work` counter counts logical visits, not tree comparisons or elapsed time:

| Probe | Before, at population N | After |
|---|---:|---:|
| Unique-key equality hit | N + 1 | 2 |
| Missing key | N | 1 |
| Range selecting one unique value | N + 1 | 3 |
| Equality hit after checkpoint/reopen | N + 1 | 2 |

All four runs confirm these counts at 100, 1,000, and 10,000 records. A caller work
limit of eight now admits the equality hit/miss; both previously exhausted it.
Equality costs one seek plus each candidate. Ranges additionally charge each
matching value bucket. Even misses/empty indexes charge the seek; a zero work
limit refuses it. A broad range can therefore cost one more unit than before.
Every candidate counts even if another filter rejects it or the output limit is
one; broad queries still fail explicitly when a ceiling is reached.

## Configuration and recovery

Indexes are opt-in and non-unique. For example, configure `("record", "key")` in
`DatabaseConfig.indexes`; neither `_key` nor any other name has implicit indexing.
`Engine::open` restores this schema and rebuilds indexes from recovered records.

With the lower-level API, use
`GraphConfig::default().with_indexed_property(NodeType::new("record"), "key")`
on each create/open/import. `Graph::nodes_by_property` dispatches from the supplied
configuration; retained snapshot registrations alone do not select that path.
The probe includes identical snapshot imports with and without this configuration.
This PR documents that existing distinction without changing storage format or
the lower-level configuration policy.

## Release measurements

Baseline: `main` at `b7cbdeecb2e67460d0c61ede94df64e7cde8537a`.
Changed code: `fix/21-indexed-engine-queries` at
`6ae81fb40d1427823145292e3f8836cb30192346`. Later evidence-only commits do not change
the measured library. Both use the same [probe.rs](probe.rs) and verified dependency
pins from the repository lockfile.

Host: AMD Ryzen Threadripper 7960X, 24 cores/48 logical CPUs, 248.9 GiB reported RAM,
Linux 7.2.8-1-cachyos, Rust 1.98.1. Processes ran sequentially without CPU pinning or
background-load control. Database files were on `/tmp` tmpfs; creation, writes,
snapshot loading and reopen were outside the latency samples.

Fixtures: one user node type; 100/1,000/10,000 records with a unique 12-character
string key, constant group string, and 128-byte body. No edges, vectors, or document
bodies; vector dimensions are not applicable. Each equality/range hit selects one
candidate. Engine indexes cover key and group, with key queried first. Warmup is
200 operations followed by 1,000 retained samples per path, two separate runs per
revision. Targets follow `(sample * 7919 + 17) % population`, without random data.

The timer includes request construction, the library call, result consumption/drop,
and the probe's correctness assertions where present. Engine queries include field
projection and internal result-size serialization checks. Graph property reads
include node materialization; snapshot-import property probes return IDs only.
The paths have different boundaries and should not be treated as interchangeable.
No IPC, socket serialization, model runtime, or decode-window acceptance is measured.

First repeat, microseconds, **p50 / p95 / p99**, using nearest-rank percentiles:

| Records | Engine predicate | Before | After |
|---:|---|---:|---:|
| 100 | Equality | 1.860 / 1.930 / 1.960 | 1.200 / 1.279 / 1.320 |
| 100 | Single-value range | 2.190 / 2.390 / 2.450 | 1.320 / 1.380 / 1.420 |
| 1,000 | Equality | 8.720 / 8.830 / 9.210 | 1.290 / 1.380 / 1.420 |
| 1,000 | Single-value range | 11.090 / 12.840 / 13.790 | 1.410 / 1.480 / 1.520 |
| 10,000 | Equality | 91.330 / 95.640 / 96.700 | 1.480 / 1.780 / 1.990 |
| 10,000 | Single-value range | 113.560 / 129.420 / 132.209 | 1.550 / 1.740 / 1.820 |

At 10,000 records, equality p95 spans 95.64–96.01 before and 1.66–1.78 after across
the two repeats. Range p95 spans 129.259–129.420 before and 1.62–1.74 after. These
are diagnostic measurements, not new absolute performance budgets or claims of
constant-time lookup. Unindexed scans remain population-dependent by design.

Original output and host/compiler/input hashes:
[before](before/environment.json), [after](after/environment.json),
[before run 1](before/run-1.jsonl), [before run 2](before/run-2.jsonl),
[after run 1](after/run-1.jsonl), [after run 2](after/run-2.jsonl).

## Validation and reproduction

Five regression tests in `drey/tests/engine_indexes.rs` cover population-independent
point-query work, scalar/index-versus-scan equivalence, inclusive/open/inverted
ranges, duplicate values, AND filters, work-limit refusal, deterministic output,
unindexed fallback, type isolation, replacements/deletion, WAL recovery, snapshot
recovery, and snapshot plus later WAL recovery. Three fail on the baseline: bounded
equality work, narrow-range work, and bounded lookups through mutation/recovery.
All 169 workspace tests, all-target build, format check, and clippy pass with
warnings denied, including the Unix-socket service tests.

Reproduce with stable Rust, Python 3.11+, and locally cached Cargo dependencies.
Run these from the repository containing this evidence; use new paths for each run:

```sh
git worktree add --detach /tmp/drey-point-before b7cbdeecb2e67460d0c61ede94df64e7cde8537a
git worktree add --detach /tmp/drey-point-after 6ae81fb40d1427823145292e3f8836cb30192346
python3 -B docs/specs/point-read-evidence/run.py /tmp/drey-point-before /tmp/drey-measured-before
python3 -B docs/specs/point-read-evidence/run.py /tmp/drey-point-after /tmp/drey-measured-after
```

The runner builds a separate release probe, checks dependency versions/sources/
checksums against each checkout's lockfile, and runs two fresh database sets. It
refuses an existing destination and modified tracked library/build inputs. Raw
JSONL, environment metadata, build log, manifest/lockfile, and database files stay
in the destination. Hashes identify the checked-in evidence; rerun timing values
will vary with the host and load.
