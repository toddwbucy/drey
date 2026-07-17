//! Persistence: in-memory graph + write-ahead log with snapshots
//! (PRD §10.3 candidate 1, the leading candidate).
//!
//! A graph is a directory:
//! - `snapshot.bin` — the last compacted full-graph image (may be absent).
//! - `wal.log` — framed mutation records since the snapshot, terminated by
//!   commit markers.
//!
//! Open replays `snapshot.bin` then the WAL up to its last commit marker.
//! Records after the last commit marker are an incomplete commit and are
//! discarded, which is what makes the recovery matrix (PRD §10.2.1) hold.
//!
//! ## Durability level (PRD §21 M2 requirement)
//!
//! `commit` is **fsync-backed crash durability**: it writes the buffered
//! records and a commit marker, then `fsync`s the WAL before returning. A
//! mutation that has not been through `commit` is not durable and is discarded
//! on the next open. This satisfies the consumer requirement that at least one
//! operation offer fsync-backed durability at the turn/consolidation boundary.

mod codec;

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use codec::{crc32, Reader, Writer};

/// Per-call counter for unique `export` temp-file names (see `Graph::export`).
static EXPORT_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

use crate::config::GraphConfig;
use crate::error::{Error, Result};
use crate::graph::{Graph, Mutation};
use crate::store::Store;
use crate::types::{EdgeId, EdgeType, NodeId, NodeType, Value};

/// On-disk format version, written into the snapshot header and the WAL header.
/// Bumped on any incompatible format change; open fails with `VersionMismatch`
/// on a mismatch of either file, before any frame is decoded (PRD §10.2.1, §20).
///
/// v2: the snapshot gained a trailing CRC over its payload so a bit-flip is
/// detected on load rather than silently blended in (PRD §10.2.1). Pre-CRC
/// (v1) snapshots therefore fail cleanly with `VersionMismatch`.
///
/// v3: the WAL header gained a trailing CRC over its magic+version+epoch bytes.
/// The epoch decides whether the WAL is replayed or discarded as stale, so an
/// unprotected bit-flip lowering it below the snapshot epoch would silently drop
/// every acknowledged commit since the snapshot; the CRC turns that into a clean
/// load-time `Codec` error. v2 WALs (16-byte header, no CRC) fail cleanly with
/// `VersionMismatch`.
pub const FORMAT_VERSION: u32 = 3;

const MAGIC: &[u8; 4] = b"DREY";
const SNAPSHOT_FILE: &str = "snapshot.bin";
const SNAPSHOT_TMP_FILE: &str = "snapshot.bin.tmp";
const WAL_FILE: &str = "wal.log";
const WAL_TMP_FILE: &str = "wal.log.tmp";
const LOCK_FILE: &str = "LOCK";

/// How many times a read-only open re-reads the files when it observes a
/// snapshot rotation mid-read (the epoch moved, or the WAL is a generation
/// ahead of the snapshot). One retry suffices for a single racing rotation;
/// a few more tolerate back-to-back rotations before giving up.
const READ_ONLY_OPEN_RETRIES: usize = 3;

/// WAL header: `MAGIC(4) + FORMAT_VERSION(u32 LE) + epoch(u64 LE) + crc32(u32 LE)`.
/// The epoch is the snapshot generation this WAL belongs to; `open` replays the
/// WAL only when its epoch is ≥ the snapshot's, so a WAL left un-truncated by a
/// crash mid-snapshot (its epoch is older) is recognized as stale and skipped
/// rather than re-applied on top of the snapshot that already contains it. The
/// trailing CRC (v3) covers the preceding 16 bytes so a bit-flip in the epoch —
/// the field that decides replay-vs-discard — is caught at load, not trusted.
const WAL_HEADER_LEN: usize = 20;
/// Bytes of the WAL header the trailing CRC covers (magic+version+epoch).
const WAL_HEADER_CRC_COVERAGE: usize = 16;

/// WAL frame tags.
const TAG_MUTATION: u8 = 1;
const TAG_COMMIT: u8 = 2;

/// Test-only fault injection (issue #10): lets unit tests force the I/O
/// failures (ENOSPC/EIO-shaped) that the commit/snapshot **poison paths** guard
/// against, which no real filesystem produces deterministically. Compiled out
/// of release builds entirely — no public surface, no feature flag, no runtime
/// cost (SQLite-class weight). Failpoints are thread-local, so each test arms
/// only its own thread and parallel tests cannot consume each other's faults.
#[cfg(test)]
pub(crate) mod fail {
    use std::cell::Cell;
    use std::thread::LocalKey;

    thread_local! {
        /// Fail the next WAL frame write in `commit`.
        pub static WAL_WRITE: Cell<bool> = const { Cell::new(false) };
        /// Fail the next WAL fsync in `commit`.
        pub static WAL_SYNC: Cell<bool> = const { Cell::new(false) };
        /// Fail the post-cutover directory fsync in `snapshot`.
        pub static CUTOVER_DIR_FSYNC: Cell<bool> = const { Cell::new(false) };
    }

    /// Arm a failpoint: the next [`hit`] on this thread errors once.
    pub fn arm(fp: &'static LocalKey<Cell<bool>>) {
        fp.with(|c| c.set(true));
    }

    /// Consume the failpoint: error once if armed, then disarm.
    pub fn hit(fp: &'static LocalKey<Cell<bool>>) -> std::io::Result<()> {
        if fp.with(|c| c.replace(false)) {
            return Err(std::io::Error::other("injected fault"));
        }
        Ok(())
    }
}

/// The internal persistence seam (design commitment 6): the public API sits
/// above this trait, so the durability backend is swappable without an API
/// change. [`WalPersistence`] is the only implementation today (WAL + snapshot);
/// recovery/construction is a per-backend factory (`Graph::open`/`create`), not
/// a trait method, since recovery *builds* the graph rather than acting on an
/// existing backend.
/// `Send + Sync` so a boxed `dyn Persistence` keeps `Graph` `Send + Sync` (a
/// consumer may move a graph between threads or share `&Graph` for concurrent
/// reads / `export`). A concrete backend held directly would be auto-`Send +
/// Sync`; a trait object is not unless the trait says so.
pub(crate) trait Persistence: Send + Sync {
    /// Whether the backend can accept new work: `Err` once a prior durable
    /// failure has poisoned it. `Graph` checks this **before** applying any
    /// mutation to the in-memory store — the store mutates before the log
    /// append, so refusing only at `append` would leave a phantom in-memory
    /// change that was never logged (Err returned, store diverged anyway).
    fn preflight(&self) -> Result<()>;
    /// Whether mutations have been appended since the last successful
    /// [`Persistence::commit`] — i.e. the in-memory graph is ahead of durable
    /// state. `export` refuses while dirty so a backup image can never
    /// disagree with what a reopen would load.
    fn dirty(&self) -> bool;
    /// Buffer one mutation; durable only at the next [`Persistence::commit`].
    fn append(&mut self, mutation: &Mutation) -> Result<()>;
    /// Flush buffered mutations to durable storage (fsync-backed).
    fn commit(&mut self) -> Result<()>;
    /// Compact `store` into a fresh full-image checkpoint and reset the log.
    fn snapshot(&mut self, store: &Store) -> Result<()>;
    /// The current durability generation, embedded in exported images.
    fn epoch(&self) -> u64;
}

/// The WAL + snapshot persistence backend for a file-backed graph.
pub(crate) struct WalPersistence {
    dir: PathBuf,
    wal: File,
    /// Snapshot generation this WAL belongs to (see [`WAL_HEADER_LEN`]).
    epoch: u64,
    /// Encoded mutation records not yet committed (fsync'd).
    pending: Vec<Vec<u8>>,
    /// The held single-writer advisory lock (see [`acquire_lock`]). Never read —
    /// its job is to exist: the kernel releases the lock when this handle drops,
    /// on clean close and hard crash alike.
    _lock: Option<File>,
    /// Set when a durable operation failed after it may have left the WAL or
    /// epoch state inconsistent (a torn `commit` write, or a `snapshot` failure
    /// after the snapshot was already cut over). A poisoned persister refuses
    /// all further durable operations — the in-memory graph may hold mutations
    /// that are not on disk, so the consumer must reopen to recover rather than
    /// keep writing behind torn or stale bytes.
    poisoned: bool,
}

impl WalPersistence {
    fn ensure_healthy(&self) -> Result<()> {
        if self.poisoned {
            return Err(Error::Storage(
                "persister poisoned by a prior failed durable operation; reopen the graph to recover"
                    .into(),
            ));
        }
        Ok(())
    }
}

impl Persistence for WalPersistence {
    fn preflight(&self) -> Result<()> {
        self.ensure_healthy()
    }

    fn dirty(&self) -> bool {
        !self.pending.is_empty()
    }

    fn append(&mut self, mutation: &Mutation) -> Result<()> {
        self.ensure_healthy()?;
        let mut w = Writer::default();
        w.u8(TAG_MUTATION);
        write_mutation(&mut w, mutation);
        self.pending.push(w.buf);
        Ok(())
    }

    /// Flush pending records plus a commit marker to the WAL and fsync.
    ///
    /// The pending buffer is only cleared **after** the write and fsync both
    /// succeed, so a failed commit retains the mutations and returns an error
    /// (never a false `Ok`). If the write or fsync fails — possibly leaving torn
    /// bytes at the WAL tail — the persister is poisoned: later commits would
    /// otherwise append behind the torn region and be stranded on the next open.
    fn commit(&mut self) -> Result<()> {
        self.ensure_healthy()?;
        // A commit with no accumulated mutations is a no-op: writing an empty
        // commit marker and fsync'ing would only grow the WAL and pay a pointless
        // fsync (this also keeps the snapshot path from emitting empty commits).
        if self.pending.is_empty() {
            return Ok(());
        }
        // Encode the batch WITHOUT draining `pending` — nothing is removed until
        // the bytes are durable.
        let mut frame_buf = Vec::new();
        for payload in &self.pending {
            write_frame(&mut frame_buf, payload)?;
        }
        // Commit marker terminates the batch.
        let mut marker = Writer::default();
        marker.u8(TAG_COMMIT);
        write_frame(&mut frame_buf, &marker.buf)?;

        let durable = (|| -> std::io::Result<()> {
            // The injected write failure is TORN: part of the frame reaches the
            // file before the "device" errors, so the fault test's reopen also
            // exercises truncation-to-last-good-prefix through a realistic tail.
            // (The m2 torn-tail recovery tests — crash_during_commit,
            // corrupt_tail, crc_byteflip, frames_without_commit_marker — cover
            // the manufactured on-disk variants directly.)
            #[cfg(test)]
            if fail::hit(&fail::WAL_WRITE).is_err() {
                let _ = self.wal.write_all(&frame_buf[..frame_buf.len() / 2]);
                return Err(std::io::Error::other("injected fault: torn WAL write"));
            }
            self.wal.write_all(&frame_buf)?;
            #[cfg(test)]
            fail::hit(&fail::WAL_SYNC)?;
            self.wal.sync_all()
        })();
        if let Err(e) = durable {
            self.poisoned = true;
            return Err(e.into());
        }
        // Durable now — safe to forget the batch.
        self.pending.clear();
        Ok(())
    }

    /// Compact: write a full-graph snapshot and reset the WAL. The snapshot is
    /// written to a temp file, fsync'd, atomically renamed, and the parent
    /// directory fsync'd — so a crash leaves either the old or new snapshot,
    /// never a partial one. The new snapshot carries a bumped epoch, and the WAL
    /// is re-headered with that epoch. If a crash lands between the rename and
    /// the WAL reset, the leftover WAL has the *old* epoch, so `open` sees it as
    /// stale and skips it instead of double-applying (PRD §10.2.1).
    fn snapshot(&mut self, store: &Store) -> Result<()> {
        // Flush anything pending first so the snapshot reflects all commits.
        self.commit()?;
        let new_epoch = self.epoch + 1;
        let dir = self.dir.clone();

        let bytes = save_snapshot(store, new_epoch);
        // Atomic replace — the rename is the cutover point.
        write_file_atomic(
            &dir.join(SNAPSHOT_FILE),
            &dir.join(SNAPSHOT_TMP_FILE),
            &bytes,
        )?;

        // Past the cutover the new snapshot is visible, so the in-memory
        // epoch/WAL must be advanced to match. ANY failure from here — the
        // directory fsync that makes the rename durable, the WAL reset, its
        // fsyncs, the handle swap, or the epoch bump — leaves on-disk and
        // in-memory disagreeing; a later commit would then append to a
        // stale-epoch or headerless WAL and be silently discarded/stranded on the
        // next open. So every post-cutover step (including that first fsync_dir)
        // is inside the poison-guarded block: on failure the graph must be
        // reopened (it loads cleanly from the new snapshot) rather than keep
        // writing.
        let cutover = (|| -> Result<()> {
            #[cfg(test)]
            fail::hit(&fail::CUTOVER_DIR_FSYNC)?;
            fsync_dir(&dir)?; // make the rename durable
            reset_wal(&dir, new_epoch)?;
            let new_wal = OpenOptions::new().append(true).open(dir.join(WAL_FILE))?;
            self.wal = new_wal;
            self.epoch = new_epoch;
            self.pending.clear();
            Ok(())
        })();
        if let Err(e) = cutover {
            self.poisoned = true;
            return Err(e);
        }
        Ok(())
    }

    fn epoch(&self) -> u64 {
        self.epoch
    }
}

// No Drop impl: the advisory lock releases when `_lock` drops (kernel-managed),
// and the LOCK file itself is deliberately left in place — see `acquire_lock`.

/// The 16-byte WAL header for a given epoch.
fn wal_header(epoch: u64) -> [u8; WAL_HEADER_LEN] {
    let mut h = [0u8; WAL_HEADER_LEN];
    h[0..4].copy_from_slice(MAGIC);
    h[4..8].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    h[8..16].copy_from_slice(&epoch.to_le_bytes());
    let crc = crc32(&h[0..WAL_HEADER_CRC_COVERAGE]);
    h[16..20].copy_from_slice(&crc.to_le_bytes());
    h
}

/// fsync a directory so a preceding `rename`/`create` is durable (Unix). Making
/// the metadata change durable is what closes the snapshot cutover window.
#[cfg(unix)]
fn fsync_dir(dir: &Path) -> Result<()> {
    File::open(dir)?.sync_all()?;
    Ok(())
}

/// On non-Unix platforms a directory cannot be opened with `File::open` (on
/// Windows it needs `FILE_FLAG_BACKUP_SEMANTICS`), and NTFS does not expose a
/// directory-fsync durability point the way POSIX does — rename durability is
/// handled by the filesystem journal. A no-op keeps `create`/`open`/`snapshot`
/// operable instead of failing on every call; the (weaker) durability posture
/// on those platforms is documented in `docs/specs/m2-durability.md`.
#[cfg(not(unix))]
fn fsync_dir(_dir: &Path) -> Result<()> {
    Ok(())
}

/// Write `bytes` to `path` atomically: write a temp sibling, fsync it, then
/// rename over the destination. A crash leaves the old file or the new one,
/// never a torn hybrid. The caller chooses the temp path (fixed for the
/// single-writer snapshot, per-call unique for the `&self` export) and decides
/// when to fsync the parent directory (the snapshot defers it into its
/// poison-guarded cutover block).
fn write_file_atomic(path: &Path, tmp: &Path, bytes: &[u8]) -> Result<()> {
    {
        let mut f = File::create(tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(tmp, path)?;
    Ok(())
}

/// Reset `wal.log` to a header-only file at `epoch`, via the same
/// tmp+fsync+rename recipe as every other durability point. The previous
/// in-place `truncate(true)` + header write had a crash window: the 20-byte
/// header's data writeback can persist while the truncate's metadata shrink
/// does not, and since old and new headers are the same length, residual
/// old-epoch frames beyond the header stayed frame-aligned and CRC-valid
/// under the new epoch — replayable double-application of already-snapshotted
/// mutations. A rename cannot leave residual frames: the file is either the
/// old WAL or the complete fresh header.
fn reset_wal(dir: &Path, epoch: u64) -> Result<()> {
    write_file_atomic(
        &dir.join(WAL_FILE),
        &dir.join(WAL_TMP_FILE),
        &wal_header(epoch),
    )?;
    fsync_dir(dir)?;
    Ok(())
}

/// Write a length+crc framed record into `out`. The frame length is a `u32`, so
/// a payload of 4 GiB or more cannot be encoded without truncating the prefix
/// (which would misalign every following frame and strand acknowledged data on
/// reopen). Reject it loudly at write time instead of casting silently.
fn write_frame(out: &mut Vec<u8>, payload: &[u8]) -> Result<()> {
    let len = u32::try_from(payload.len()).map_err(|_| {
        Error::Codec(format!(
            "WAL frame payload of {} bytes exceeds the u32 length-prefix limit",
            payload.len()
        ))
    })?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&crc32(payload).to_le_bytes());
    out.extend_from_slice(payload);
    Ok(())
}

impl Graph {
    /// Create a new file-backed graph. Fails if the target already exists as a
    /// populated graph — `create` fails if present, `open` fails if absent
    /// (PRD §9.2, open question 10 kept distinct).
    pub fn create(path: impl AsRef<Path>, config: GraphConfig) -> Result<Self> {
        let dir = path.as_ref().to_path_buf();
        if dir.join(SNAPSHOT_FILE).exists() || dir.join(WAL_FILE).exists() {
            return Err(Error::Storage(format!(
                "graph already exists at {}; use open()",
                dir.display()
            )));
        }
        fs::create_dir_all(&dir)?;
        // Canonicalize once, now that the directory exists: every stored path
        // (the persister's dir, `origin_dir` for export-alias refusal) must
        // stay valid if the process later changes cwd — a relative path
        // re-resolved under a new cwd would silently point elsewhere and, for
        // the alias check, let an export rename over the live WAL.
        let dir = dir.canonicalize()?;
        // fsync the parent so the newly created directory entry itself is durable
        // — on POSIX, mkdir is a metadata change to the parent, and without this a
        // power failure can lose the whole graph directory despite commit()'s
        // fsync of files inside it.
        if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
            fsync_dir(parent)?;
        }
        let mut graph = Graph::in_memory(config.clone());
        let locked = acquire_lock(&dir, &config)?;
        // A fresh graph starts at epoch 0; write the WAL header up front so the
        // file is always version-tagged before any frame is appended.
        reset_wal(&dir, 0)?;
        let wal = OpenOptions::new().append(true).open(dir.join(WAL_FILE))?;
        graph.persist = Some(Box::new(WalPersistence {
            dir: dir.clone(),
            wal,
            epoch: 0,
            pending: Vec::new(),
            _lock: locked,
            poisoned: false,
        }));
        graph.origin_dir = Some(dir);
        Ok(graph)
    }

    /// Open an existing file-backed graph, replaying snapshot + committed WAL.
    /// Fails if absent.
    ///
    /// A **writable** open acquires the single-writer lock *before* reading
    /// either persistence file. Reading first and locking after (the previous
    /// order) was a TOCTOU: a concurrent writer could commit and close between
    /// our read and our lock, and the post-lock WAL repair would then truncate
    /// its acknowledged commit away.
    ///
    /// A **read-only** open takes no lock (PRD §9.1). Every state its reads
    /// can capture is internally consistent (an equal-epoch snapshot+WAL
    /// pair, or a stale-skipped snapshot alone) but may be one rotation
    /// stale; reads racing a rotation or a writable open's WAL repair can
    /// surface as a generation mismatch or a corruption classification, so
    /// those retry a bounded number of times before the error stands.
    pub fn open(path: impl AsRef<Path>, config: GraphConfig) -> Result<Self> {
        let dir = path.as_ref().to_path_buf();
        if !dir.join(SNAPSHOT_FILE).exists() && !dir.join(WAL_FILE).exists() {
            return Err(Error::Storage(format!(
                "no graph at {}; use create()",
                dir.display()
            )));
        }
        // Canonicalize once (the directory provably exists — it holds graph
        // files): stored paths must survive a later cwd change (see `create`).
        let dir = dir.canonicalize()?;

        if config.read_only {
            // Lock-free readers can observe a snapshot rotation as a WAL one
            // generation ahead of the snapshot they read; a re-read picks up
            // the fresh snapshot and resolves it. Every state a retry-free
            // pass CAN load is internally consistent (an equal-epoch pair, or
            // a stale-skipped snapshot alone) — at worst one rotation stale,
            // which is inherent to reading without the writer lock.
            let mut last_err: Option<Error> = None;
            for _ in 0..READ_ONLY_OPEN_RETRIES {
                match Self::load_committed(&dir, &config) {
                    Ok(loaded) => {
                        let mut graph = loaded.graph;
                        graph.origin_dir = Some(dir);
                        return Ok(graph);
                    }
                    // Codec errors retry too, not only generation mismatches:
                    // a lock-free read racing a writable open's WAL repair
                    // (shrink + re-append at reused offsets) can capture a
                    // blended byte sequence that classifies as corruption. A
                    // benign blend loads cleanly on re-read; genuine
                    // corruption fails identically every time and surfaces
                    // after the bounded retries.
                    Err(e @ (Error::GenerationMismatch { .. } | Error::Codec(_))) => {
                        last_err = Some(e)
                    }
                    Err(e) => return Err(e),
                }
            }
            let e = last_err.expect("retry loop ran at least once");
            return Err(refine_generation_mismatch(&dir, e));
        }

        // Writable: lock first, then read — no window for a concurrent writer.
        let locked = acquire_lock(&dir, &config)?;
        let loaded =
            Self::load_committed(&dir, &config).map_err(|e| refine_generation_mismatch(&dir, e))?;
        let mut graph = loaded.graph;
        let replay = loaded.replay;
        let snap_epoch = loaded.snap_epoch;

        // Normalize the on-disk WAL so new commits are not stranded behind
        // stale or torn bytes: a stale WAL (or a torn header) is rewritten to a
        // header-only file at the snapshot epoch; a replayed WAL is truncated
        // to its last committed frame.
        let wal_path = dir.join(WAL_FILE);
        let epoch = if replay.stale {
            reset_wal(&dir, snap_epoch)?;
            snap_epoch
        } else {
            let f = OpenOptions::new().write(true).open(&wal_path)?;
            if f.metadata()?.len() > replay.valid_len as u64 {
                f.set_len(replay.valid_len as u64)?;
                f.sync_all()?;
                fsync_dir(&dir)?;
            }
            replay.epoch
        };
        let wal = OpenOptions::new().append(true).open(&wal_path)?;
        graph.persist = Some(Box::new(WalPersistence {
            dir: dir.clone(),
            wal,
            epoch,
            pending: Vec::new(),
            _lock: locked,
            poisoned: false,
        }));
        graph.origin_dir = Some(dir);
        Ok(graph)
    }

    /// Load the committed state (snapshot + committed WAL prefix) into a fresh
    /// graph. Shared by writable opens (under the lock) and read-only opens
    /// (inside the stability-retry loop).
    fn load_committed(dir: &Path, config: &GraphConfig) -> Result<LoadedState> {
        let mut graph = Graph::in_memory(config.clone());

        let snapshot_present = dir.join(SNAPSHOT_FILE).exists();
        let wal_present = dir.join(WAL_FILE).exists();
        // Deliberately duplicates `open`'s pre-lock existence check: that one
        // exists so a writable open of a non-graph directory errors BEFORE
        // `acquire_lock` drops a LOCK file into it; this one guards the
        // read-only retry loop, where the files are re-read per attempt.
        if !snapshot_present && !wal_present {
            return Err(Error::Storage(format!(
                "no graph at {}; use create()",
                dir.display()
            )));
        }
        // Recovery-matrix cell: a snapshot with no WAL beside it. `create`
        // writes the WAL before any commit and every later reset atomically
        // renames a fresh header over it (`reset_wal` — the file is replaced,
        // never unlinked), so in every legitimate state — including every
        // crash window — wal.log exists whenever snapshot.bin does. Its
        // absence proves file loss; opening at snapshot state would silently
        // drop every post-snapshot commit (PRD §10.2.1: never a silent
        // partial load). The mirror cell (WAL present, snapshot missing) is
        // refused below.
        if snapshot_present && !wal_present {
            return Err(Error::Storage(format!(
                "snapshot.bin exists at {} but wal.log is missing; refusing to open at \
                 snapshot state, which would silently drop any post-snapshot commits. \
                 If the WAL is known lost, recover the snapshot image explicitly via \
                 Graph::import(\"{}\") — snapshots and exports share one encoding",
                dir.display(),
                dir.join(SNAPSHOT_FILE).display()
            )));
        }

        // 1. Replay snapshot if present; its epoch bounds which WAL is current.
        let mut snap_epoch = 0u64;
        if snapshot_present {
            let bytes = fs::read(dir.join(SNAPSHOT_FILE))?;
            let (store, epoch) = load_snapshot(&bytes, &graph.store.indexed, config)?;
            graph.store = store;
            snap_epoch = epoch;
        }

        // 2. Replay the committed WAL prefix, unless the WAL is stale (its epoch
        //    predates the snapshot, i.e. a crash left it un-truncated).
        // A WAL of snapshot generation ≥ 1 with no snapshot on disk surfaces
        // here as a GenerationMismatch (wal_epoch > absent-snapshot epoch 0):
        // the snapshot was lost, and replaying post-snapshot mutations onto an
        // empty store would be a silent partial load — edges pointing at nodes
        // that only existed in the missing snapshot (PRD §10.2.1). The callers
        // refine that error into the missing-snapshot diagnostic once their
        // retry policy is exhausted (`refine_generation_mismatch`).
        let bytes = fs::read(dir.join(WAL_FILE))?;
        let replay = replay_wal(&mut graph, &bytes, snap_epoch)?;

        graph.store.sort_indexes();
        graph.loaded_epoch = replay.epoch.max(snap_epoch);
        Ok(LoadedState {
            graph,
            replay,
            snap_epoch,
        })
    }

    /// Compact: write a full-graph snapshot and reset the WAL. The snapshot is
    /// written to a temp file, fsync'd, atomically renamed, and the parent
    /// directory fsync'd — so a crash leaves either the old or new snapshot,
    /// never a partial one. The new snapshot carries a bumped epoch, and the WAL
    /// is re-headered with that epoch. If a crash lands between the rename and
    /// the WAL reset, the leftover WAL has the *old* epoch, so `open` sees it as
    /// stale and skips it instead of double-applying (PRD §10.2.1).
    pub fn snapshot(&mut self) -> Result<()> {
        // The compaction lives behind the persistence seam; a file-backed graph
        // delegates to its backend, an in-memory graph has nothing to compact.
        // `persist` and `store` are disjoint fields, so the split borrow is fine.
        match self.persist.as_mut() {
            Some(p) => p.snapshot(&self.store),
            None => Err(Error::Storage("cannot snapshot an in-memory graph".into())),
        }
    }

    /// Export a portable full-graph image to a single file (PRD §9.2, §22).
    /// Same encoding as a snapshot; restores the exact id space on import.
    ///
    /// A file-backed graph refuses to export while its persister is poisoned
    /// (in-memory state may be ahead of anything replayable) or **dirty**
    /// (mutations appended since the last `commit`): `export` is the backup
    /// verb, and a backup must never disagree with what a reopen would load.
    /// Commit first. In-memory graphs export freely — there is no durable
    /// state to diverge from.
    ///
    /// Destinations that alias the live graph files (`wal.log`,
    /// `snapshot.bin`, `LOCK` — directly, via a relative/symlinked path, or a
    /// hard link where the platform exposes file identity) are refused: the
    /// atomic rename would replace a file the writer holds open, orphaning
    /// its handle and destroying recoverability.
    pub fn export(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if let Some(p) = &self.persist {
            p.preflight()?;
            if p.dirty() {
                return Err(Error::Storage(
                    "graph has uncommitted mutations; commit() before export so the image \
                     matches durable state"
                        .into(),
                ));
            }
        }
        if let Some(dir) = &self.origin_dir {
            for reserved in [
                SNAPSHOT_FILE,
                SNAPSHOT_TMP_FILE,
                WAL_FILE,
                WAL_TMP_FILE,
                LOCK_FILE,
            ] {
                let reserved = dir.join(reserved);
                if paths_alias(path, &reserved) {
                    return Err(Error::Storage(format!(
                        "export destination {} aliases the live graph file {}",
                        path.display(),
                        reserved.display()
                    )));
                }
            }
        }
        // A portable image is not tied to a WAL; carry the current epoch so a
        // re-import into a file-backed graph starts consistently. Read-only
        // opens attach no persister, so the epoch captured at load time is
        // used — never a fabricated 0, which (used as a snapshot replacement
        // next to a live WAL) would replay non-idempotent mutations twice.
        let epoch = self
            .persist
            .as_ref()
            .map_or(self.loaded_epoch, |p| p.epoch());
        let bytes = save_snapshot(&self.store, epoch);
        // Write to a sibling temp file, fsync, then atomically rename over the
        // destination and fsync the parent. `export` is the §22 backup verb, so a
        // torn/failed write must never destroy the previous image at `path` — the
        // hazard of a bare `fs::write` (create+truncate in place). Mirrors
        // `snapshot`'s cutover.
        //
        // Unlike `snapshot` (`&mut self`, serialized by the single-writer model),
        // `export` takes `&self`, so two threads can export the same destination
        // at once. A fixed `.tmp` name would let them clobber each other's temp;
        // a per-call unique suffix (pid + counter) keeps each write private, and
        // the atomic renames just resolve to whichever finishes last — always a
        // complete image, never a torn one.
        let tmp = {
            let seq = EXPORT_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let mut s = path.as_os_str().to_os_string();
            s.push(format!(".{}.{}.tmp", std::process::id(), seq));
            PathBuf::from(s)
        };
        write_file_atomic(path, &tmp, &bytes)?;
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fsync_dir(parent)?;
        }
        Ok(())
    }

    /// Import a graph image produced by [`Graph::export`] into a fresh
    /// in-memory graph, restoring the exact id space (PRD §10.2, §22).
    pub fn import(path: impl AsRef<Path>, config: GraphConfig) -> Result<Self> {
        let bytes = fs::read(path)?;
        let mut graph = Graph::in_memory(config);
        let (store, epoch) = load_snapshot(&bytes, &graph.store.indexed, &graph.config)?;
        graph.store = store;
        graph.store.sort_indexes();
        graph.loaded_epoch = epoch;
        Ok(graph)
    }
}

/// Whether `target` and `reserved` name the same file. When both exist, file
/// identity is compared where the platform exposes it (dev+inode on Unix —
/// which also catches hard links); otherwise the comparison falls back to
/// canonicalized paths, resolving the parent when the target does not exist
/// yet (an export destination usually doesn't). `reserved` is always rooted
/// at the graph's canonicalized `origin_dir`, so a cwd change between open
/// and export cannot re-point it. A check-to-rename TOCTOU window remains
/// for an *external* process re-linking the destination between this check
/// and the rename — unavoidable without O_NOFOLLOW-style open semantics, and
/// out of scope for the single-process model (PRD §11).
fn paths_alias(target: &Path, reserved: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(a), Ok(b)) = (fs::metadata(target), fs::metadata(reserved)) {
            // Identity is definitive when both exist.
            return a.dev() == b.dev() && a.ino() == b.ino();
        }
    }
    let canonical = |p: &Path| -> Option<PathBuf> {
        if let Ok(c) = p.canonicalize() {
            return Some(c);
        }
        // Not on disk yet: canonicalize the parent and re-attach the name.
        let parent = match p.parent().filter(|q| !q.as_os_str().is_empty()) {
            Some(parent) => parent.canonicalize().ok()?,
            None => Path::new(".").canonicalize().ok()?,
        };
        Some(parent.join(p.file_name()?))
    };
    match (canonical(target), canonical(reserved)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// Take the single-writer lock via OS advisory locking (`flock(2)` on Unix,
/// `LockFileEx` on Windows, through [`File::try_lock`]).
///
/// The kernel releases an advisory lock when the holding file handle closes —
/// including SIGKILL, OOM-kill, panic-abort, and power loss — so the lock is
/// crash-safe by construction. This replaces the previous PID+boot-id staleness
/// scheme (audit #5 / issue #8), eliminating with it the wedge-after-hard-crash,
/// the reclaim TOCTOU (two openers both judging a lock stale and one deleting
/// the other's live lock), and the leak-on-error-path: the returned handle
/// simply drops on any early return and the kernel releases.
///
/// The `LOCK` file itself is a permanent anchor — created if absent, never
/// deleted (unlinking a lock file reintroduces the race where one process holds
/// the lock on an unlinked inode while another locks a fresh file at the same
/// path). An unheld leftover file is harmless: locking it just succeeds.
fn acquire_lock(dir: &Path, config: &GraphConfig) -> Result<Option<File>> {
    if !config.file_lock {
        return Ok(None);
    }
    let lock = dir.join(LOCK_FILE);
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock)?;
    match f.try_lock() {
        Ok(()) => Ok(Some(f)),
        Err(std::fs::TryLockError::WouldBlock) => Err(Error::LockConflict(format!(
            "another live writer holds {}",
            lock.display()
        ))),
        Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

// ---- WAL replay ----

/// The result of loading committed state from disk (`Graph::load_committed`).
struct LoadedState {
    graph: Graph,
    replay: WalReplay,
    snap_epoch: u64,
}

/// Refine a [`Error::GenerationMismatch`] into the sharper missing-snapshot
/// diagnostic when the mismatch is against an *absent* snapshot: a WAL of
/// generation ≥ 1 beside no snapshot.bin means the snapshot was lost, which is
/// a different operator problem than a backup-restored older snapshot. Applied
/// at the `open` boundary (after the read-only retry policy), not inside
/// `replay_wal`, so the raw mismatch stays retryable for lock-free readers
/// racing a first-snapshot rotation.
fn refine_generation_mismatch(dir: &Path, e: Error) -> Error {
    match e {
        Error::GenerationMismatch { wal_epoch, .. } if !dir.join(SNAPSHOT_FILE).exists() => {
            Error::Storage(format!(
                "WAL at {} belongs to snapshot epoch {wal_epoch} but snapshot.bin is missing; \
                 refusing a partial load",
                dir.display()
            ))
        }
        e => e,
    }
}

/// The outcome of a WAL replay, used to normalize the on-disk WAL on open.
struct WalReplay {
    /// The WAL's epoch (or `snap_epoch` when the WAL was stale/absent).
    epoch: u64,
    /// Byte length of the WAL through its last committed frame — the truncation
    /// point that drops any torn trailing bytes.
    valid_len: usize,
    /// The WAL was skipped (its epoch predates the snapshot, or it was too short
    /// to hold a valid header): its content is discarded on repair.
    stale: bool,
}

/// One structurally parsed WAL frame (not yet decoded as a mutation).
struct RawFrame<'a> {
    payload: &'a [u8],
    /// Byte offset just past this frame — a truncation point candidate.
    end: usize,
    /// Whether the payload matched its stored CRC.
    crc_ok: bool,
}

impl RawFrame<'_> {
    /// A trustworthy commit marker: CRC-valid and exactly the marker byte. A
    /// CRC-bad frame is never treated as a marker — its content can't be trusted.
    fn is_commit_marker(&self) -> bool {
        self.crc_ok && self.payload == [TAG_COMMIT]
    }
}

/// Replay the committed prefix of the WAL. Parses and version-gates the header
/// first; if the WAL epoch predates `snap_epoch` the WAL is stale (already folded
/// into the snapshot) and nothing is replayed; a WAL epoch *newer* than the
/// snapshot is a typed [`Error::GenerationMismatch`] — the snapshot on disk is
/// not the base this WAL was written against (a backup-restored snapshot, or a
/// lock-free read that raced a rotation), and replaying onto it would silently
/// blend two generations. Returns where the committed prefix ends so the caller
/// can repair the file.
///
/// ## Torn tail vs. corruption (PRD §10.2.1)
///
/// Frames are structurally scanned before anything is decoded or applied, and
/// damage is classified by *where* it sits relative to the fsync barriers that
/// batch commits create — bytes of batch N+1 exist on disk only if batch N's
/// fsync returned, i.e. only if batch N was acknowledged:
///
/// - Damage confined to the **final** batch (a torn frame at EOF, frames with
///   no trailing commit marker, or a CRC-bad frame whose marker is the last
///   thing in the file) is indistinguishable from a torn, *unacknowledged*
///   commit — the kernel may persist a batch's pages in any order before the
///   fsync completes. It is discarded and the WAL truncated: crash recovery.
/// - A CRC-bad frame with **any bytes after its batch's commit marker** sits
///   in an acknowledged batch. That is silent-data-loss territory — a bit flip
///   in fsync'd history — and open refuses with a typed error rather than
///   truncating acknowledged commits away.
///
/// Residual limit: damage to a frame's *length prefix* mid-file can derail the
/// structural scan itself; the scan then can't see past the damage, and the
/// tail beyond it is treated as torn. Distinguishing that from a torn write
/// would need per-frame sequencing — recorded as a known limit in
/// `docs/specs/m2-durability.md`.
fn replay_wal(graph: &mut Graph, bytes: &[u8], snap_epoch: u64) -> Result<WalReplay> {
    // Guard order matters for legacy files: magic + version live in the first
    // 8 bytes, so they are checked before the full-header length guard. A
    // 16-byte v2 WAL (header-only, zero commits) is shorter than the 20-byte v3
    // header; length-first would classify it as a torn stale header and silently
    // rewrite it, instead of the VersionMismatch the format contract promises.
    // Anything under 8 bytes can only be a torn header write — a header prefix
    // torn mid-create, carrying no version to check and no committed content.
    if bytes.len() < 8 {
        return Ok(WalReplay {
            epoch: snap_epoch,
            valid_len: WAL_HEADER_LEN,
            stale: true,
        });
    }
    if &bytes[0..4] != MAGIC {
        return Err(Error::Codec("bad WAL magic".into()));
    }
    let version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    if version != FORMAT_VERSION {
        return Err(Error::VersionMismatch {
            found: version,
            supported: FORMAT_VERSION,
        });
    }
    // Right magic and version but shorter than the full header: a v3 header
    // write torn mid-create. No epoch/CRC to trust, no committed content.
    if bytes.len() < WAL_HEADER_LEN {
        return Ok(WalReplay {
            epoch: snap_epoch,
            valid_len: WAL_HEADER_LEN,
            stale: true,
        });
    }
    // Verify the header CRC before trusting the epoch (v3). The epoch is the sole
    // discriminator between "replay this WAL" and "discard it as stale", so a
    // silent bit-flip there would drop acknowledged commits; a mismatch is a
    // corrupt header, surfaced as a typed error rather than a silent stale-skip.
    let stored_crc = u32::from_le_bytes(bytes[16..20].try_into().unwrap());
    if crc32(&bytes[0..WAL_HEADER_CRC_COVERAGE]) != stored_crc {
        return Err(Error::Codec(
            "WAL header CRC mismatch (corrupt header)".into(),
        ));
    }
    let wal_epoch = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    if wal_epoch < snap_epoch {
        // Stale WAL left by a crash mid-snapshot: its frames are already in the
        // snapshot. Skip to avoid double-applying (e.g. non-idempotent decay).
        return Ok(WalReplay {
            epoch: snap_epoch,
            valid_len: WAL_HEADER_LEN,
            stale: true,
        });
    }
    if wal_epoch > snap_epoch {
        // Not the blanket `!=`: an *older* WAL is the legitimate
        // crash-mid-snapshot state handled above. Only newer is refused.
        return Err(Error::GenerationMismatch {
            wal_epoch,
            snapshot_epoch: snap_epoch,
        });
    }

    // Phase 1 — structural scan: split the byte stream into frames, checking
    // CRCs but decoding nothing. A torn frame header or payload at EOF ends the
    // scan (everything beyond it is unreachable and treated as torn tail).
    let mut frames: Vec<RawFrame> = Vec::new();
    let mut pos = WAL_HEADER_LEN;
    while pos < bytes.len() {
        if pos + 8 > bytes.len() {
            break; // torn frame header at EOF
        }
        let len = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        let crc = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap());
        let start = pos + 8;
        let Some(end) = start.checked_add(len).filter(|&e| e <= bytes.len()) else {
            break; // torn payload at EOF (or a corrupt length driving past it)
        };
        let payload = &bytes[start..end];
        // A zero-length frame is structural damage, not a valid frame: no
        // writer emits one (every real payload carries at least a tag byte),
        // but a zeroed torn page parses as a run of them — len=0, stored CRC
        // 0, and crc32(b"") == 0, so they self-validate. Without this they
        // would sail through classification and turn a plain torn-tail power
        // loss into a permanent phase-3 decode refusal.
        frames.push(RawFrame {
            payload,
            end,
            crc_ok: len != 0 && crc32(payload) == crc,
        });
        pos = end;
    }

    // Phase 2 — classify damage against the fsync barriers (see the doc
    // comment above). `markers` are the trustworthy commit markers; everything
    // after the last one is an uncommitted tail, discarded without decoding.
    let markers: Vec<usize> = frames
        .iter()
        .enumerate()
        .filter_map(|(i, f)| f.is_commit_marker().then_some(i))
        .collect();
    // `apply_through` is the marker index the replay applies through
    // (`None` = no committed batch survives).
    let mut apply_through: Option<usize> = markers.last().copied();
    let mut valid_len = apply_through.map_or(WAL_HEADER_LEN, |i| frames[i].end);

    // Every damaged frame is judged by evidence AFTER its own batch — raw
    // bytes, parsed or torn-fragment alike (the fsync-barrier argument does
    // not care whether they frame cleanly), because a later batch's bytes
    // reach the file only after this batch's fsync returned. The loop
    // deliberately covers frames at and beyond the last trusted marker, and
    // the no-trusted-marker-at-all case: a corrupted FINAL marker with
    // acknowledgment-proving bytes after it is the most physically likely
    // damage site of all — the shared-block tear of the *next* append lands
    // exactly there.
    for (i, frame) in frames.iter().enumerate() {
        if frame.crc_ok {
            continue;
        }
        let refuse = if frame.payload.len() == 1 {
            // Marker-sized: a commit marker is the only 1-byte payload
            // `commit()` writes, so treat this as a damaged marker whose
            // batch ends HERE — any raw bytes beyond it are later writes.
            // (The benign alternative — torn-write garbage that happens to
            // encode len == 1 and realign the scan on the very next frame —
            // needs a ~2^-32 length-prefix coincidence, where a damaged real
            // marker needs one ordinary bit flip. Refusal is the safe side
            // and overwhelmingly the likely truth; the residual over-refusal
            // is recoverable by an operator, silent truncation is not.)
            bytes.len() > frame.end
        } else {
            // Mutation-sized: its batch ends at the first trusted marker at
            // or after it. No such marker → an unterminated trailing batch —
            // torn tail, never applied, nothing to refuse. Note the marker's
            // own presence proves only that the write reached disk, not that
            // its fsync returned — so evidence must lie beyond the MARKER,
            // not merely beyond the damaged frame.
            match markers.iter().find(|&&m| m >= i) {
                Some(&m) => bytes.len() > frames[m].end,
                None => false,
            }
        };
        if refuse {
            return Err(Error::Codec(format!(
                "WAL frame at byte offset {} fails its CRC inside an acknowledged commit \
                 (fsync barrier proves the batch was durable); refusing to truncate \
                 committed history — restore from a snapshot/export instead",
                frame.end - frame.payload.len() - 8,
            )));
        }
    }
    // Damage that survives the loop is confined to the final complete batch
    // (whose marker is the last thing in the file) or to the unacknowledged
    // tail after the last trusted marker. Tail damage needs no action — those
    // frames are never applied. Damage inside the final complete batch means
    // a torn, unacknowledged commit whose marker page persisted (pages of one
    // write may land in any order): discard that batch, apply through the
    // prior marker.
    if let Some(&last) = markers.last() {
        if frames[..last].iter().any(|f| !f.crc_ok) {
            apply_through = markers.len().checked_sub(2).map(|i| markers[i]);
            valid_len = apply_through.map_or(WAL_HEADER_LEN, |i| frames[i].end);
        }
    }

    // Phase 3 — decode and apply the acknowledged prefix. Every frame here is
    // CRC-valid; a decode failure (bad tag, short payload) is therefore format
    // corruption or a codec bug, surfaced as a typed error — and nothing has
    // been applied out of order because batches replay strictly in sequence.
    let mut max_node = graph.store.next_node_id;
    let mut max_edge = graph.store.next_edge_id;
    if let Some(last) = apply_through {
        for frame in &frames[..=last] {
            if frame.is_commit_marker() {
                continue;
            }
            let mut r = Reader::new(frame.payload);
            match r.u8()? {
                TAG_MUTATION => {
                    let m = read_mutation(&mut r)?;
                    apply_replay(graph, m, &mut max_node, &mut max_edge)?;
                }
                // Exactly-one-byte markers were skipped above; a CRC-valid
                // TAG_COMMIT frame with trailing bytes is nothing commit()
                // ever writes.
                TAG_COMMIT => {
                    return Err(Error::Codec(
                        "malformed commit marker (trailing bytes)".into(),
                    ))
                }
                t => return Err(Error::Codec(format!("bad WAL frame tag {t}"))),
            }
        }
    }
    graph.store.next_node_id = graph.store.next_node_id.max(max_node);
    graph.store.next_edge_id = graph.store.next_edge_id.max(max_edge);
    Ok(WalReplay {
        epoch: wal_epoch,
        valid_len,
        stale: false,
    })
}

/// Apply a committed mutation during replay, using explicit ids (no allocation)
/// so the id space is restored exactly (PRD §7.4).
///
/// Every arm routes through the same validated store paths the live mutation
/// API uses (or re-applies the live path's validations before a raw insert
/// where the live path also allocates). A WAL that a healthy writer produced
/// always passes; a frame that *fails* these gates is logically inconsistent
/// — a missing id, an unregistered type, a non-finite weight — and surfaces as
/// a typed error instead of the silent no-op / unvalidated write two of these
/// arms used to perform (PRD §10.2.1: never a silent partial load).
fn apply_replay(
    graph: &mut Graph,
    m: Mutation,
    max_node: &mut u64,
    max_edge: &mut u64,
) -> Result<()> {
    let store = &mut graph.store;
    match m {
        Mutation::RegisterNodeType {
            node_type,
            embedding_dim,
        } => {
            // Same gates as the live register_node_type — including the
            // config's max_embedding_dim, so reopening under a stricter config
            // is a typed error, not a silently retained over-limit type.
            crate::graph::validate_embedding_dim(&graph.config, embedding_dim)?;
            store.register_node_type(&node_type, embedding_dim)?;
        }
        Mutation::AddNode {
            id,
            node_type,
            properties,
        } => {
            // The live add_node's gate set, with the WAL's explicit id
            // restored instead of allocated (PRD §7.4).
            store.insert_node_checked(id.0, &node_type, properties)?;
            *max_node = (*max_node).max(id.0 + 1);
        }
        Mutation::SetNodeEmbedding { node, embedding } => {
            store.set_node_embedding(node, embedding)?;
        }
        Mutation::UpdateNodeProperties { node, patch } => {
            store.update_node_properties(node, &patch)?;
        }
        Mutation::RemoveNode { node, mode } => {
            let remove_incident = mode == crate::mutation::RemoveNodeMode::RemoveIncidentEdges;
            store.remove_node(node, remove_incident)?;
        }
        Mutation::AddEdge {
            id,
            from,
            to,
            edge_type,
            weight,
            properties,
        } => {
            // The live add_edge's gate set, id restored instead of allocated.
            store.insert_edge_checked(id.0, from, to, &edge_type, weight, properties)?;
            *max_edge = (*max_edge).max(id.0 + 1);
        }
        Mutation::SetEdgeWeight { edge, weight } => {
            store.set_edge_weight(edge, weight)?;
        }
        Mutation::UpdateEdgeProperties { edge, patch } => {
            store.update_edge_properties(edge, &patch)?;
        }
        Mutation::RemoveEdge { edge } => {
            store.remove_edge(edge)?;
        }
        Mutation::DecayEdges { filter, factor } => {
            // Deterministic recomputation of the batch the live call applied;
            // each write goes through set_edge_weight so the store-side
            // finiteness gate holds on this path too.
            //
            // Compatibility: `filter.min_weight` is deliberately NOT
            // NaN-gated here, unlike the live decay_edges entry. Pre-hardening
            // writers of this same FORMAT_VERSION legally committed
            // NaN-min_weight frames; the filter matched nothing (`w >= NaN`
            // is false), so `edges_matching` below reproduces the historical
            // no-op exactly. Rejecting the frame would permanently brick
            // every open of such a graph with no version gate to explain why.
            // The live entry stops NEW NaN frames at the source.
            if !factor.is_finite() {
                return Err(Error::InvalidPropertyValue(
                    "decay factor must be finite (not NaN or infinite)".into(),
                ));
            }
            let ids = graph.edges_matching(&filter);
            for e in ids {
                let w = graph.store.edges[&e].weight;
                graph.store.set_edge_weight(EdgeId(e), w * factor)?;
            }
        }
    }
    Ok(())
}

// ---- Mutation codec (WAL records) ----

fn write_patch(w: &mut Writer, patch: &std::collections::BTreeMap<String, Option<Value>>) {
    w.u32(patch.len() as u32);
    for (k, v) in patch {
        w.str(k);
        match v {
            None => w.u8(0),
            Some(val) => {
                w.u8(1);
                codec::write_value(w, val);
            }
        }
    }
}

fn read_patch(r: &mut Reader) -> Result<std::collections::BTreeMap<String, Option<Value>>> {
    let n = r.u32()? as usize;
    let mut patch = std::collections::BTreeMap::new();
    for _ in 0..n {
        let k = r.str()?;
        let v = match r.u8()? {
            0 => None,
            1 => Some(codec::read_value(r)?),
            t => return Err(Error::Codec(format!("bad patch value tag {t}"))),
        };
        patch.insert(k, v);
    }
    Ok(patch)
}

fn write_mutation(w: &mut Writer, m: &Mutation) {
    match m {
        Mutation::RegisterNodeType {
            node_type,
            embedding_dim,
        } => {
            w.u8(0);
            w.str(node_type.as_str());
            match embedding_dim {
                None => w.u8(0),
                Some(d) => {
                    w.u8(1);
                    w.u64(*d as u64);
                }
            }
        }
        Mutation::AddNode {
            id,
            node_type,
            properties,
        } => {
            w.u8(1);
            w.u64(id.0);
            w.str(node_type.as_str());
            codec::write_properties(w, properties);
        }
        Mutation::SetNodeEmbedding { node, embedding } => {
            w.u8(2);
            w.u64(node.0);
            w.u32(embedding.len() as u32);
            for x in embedding {
                w.f32(*x);
            }
        }
        Mutation::UpdateNodeProperties { node, patch } => {
            w.u8(3);
            w.u64(node.0);
            write_patch(w, patch);
        }
        Mutation::RemoveNode { node, mode } => {
            w.u8(4);
            w.u64(node.0);
            w.u8(match mode {
                crate::mutation::RemoveNodeMode::RejectIfEdgesExist => 0,
                crate::mutation::RemoveNodeMode::RemoveIncidentEdges => 1,
            });
        }
        Mutation::AddEdge {
            id,
            from,
            to,
            edge_type,
            weight,
            properties,
        } => {
            w.u8(5);
            w.u64(id.0);
            w.u64(from.0);
            w.u64(to.0);
            w.str(edge_type.as_str());
            w.f32(*weight);
            codec::write_properties(w, properties);
        }
        Mutation::SetEdgeWeight { edge, weight } => {
            w.u8(6);
            w.u64(edge.0);
            w.f32(*weight);
        }
        Mutation::UpdateEdgeProperties { edge, patch } => {
            w.u8(7);
            w.u64(edge.0);
            write_patch(w, patch);
        }
        Mutation::RemoveEdge { edge } => {
            w.u8(8);
            w.u64(edge.0);
        }
        Mutation::DecayEdges { filter, factor } => {
            w.u8(9);
            w.u32(filter.edge_types.len() as u32);
            for t in &filter.edge_types {
                w.str(t.as_str());
            }
            match filter.min_weight {
                None => w.u8(0),
                Some(mw) => {
                    w.u8(1);
                    w.f32(mw);
                }
            }
            w.f32(*factor);
        }
    }
}

fn read_mutation(r: &mut Reader) -> Result<Mutation> {
    Ok(match r.u8()? {
        0 => {
            let node_type = NodeType::new(r.str()?);
            let embedding_dim = match r.u8()? {
                0 => None,
                1 => Some(r.u64()? as usize),
                t => return Err(Error::Codec(format!("bad dim tag {t}"))),
            };
            Mutation::RegisterNodeType {
                node_type,
                embedding_dim,
            }
        }
        1 => Mutation::AddNode {
            id: NodeId(r.u64()?),
            node_type: NodeType::new(r.str()?),
            properties: codec::read_properties(r)?,
        },
        2 => {
            let node = NodeId(r.u64()?);
            let n = r.u32()? as usize;
            let n = r.checked_len(n, 4)?; // f32 each
            let mut emb = Vec::with_capacity(n);
            for _ in 0..n {
                emb.push(r.f32()?);
            }
            Mutation::SetNodeEmbedding {
                node,
                embedding: emb,
            }
        }
        3 => Mutation::UpdateNodeProperties {
            node: NodeId(r.u64()?),
            patch: read_patch(r)?,
        },
        4 => {
            let node = NodeId(r.u64()?);
            let mode = match r.u8()? {
                0 => crate::mutation::RemoveNodeMode::RejectIfEdgesExist,
                1 => crate::mutation::RemoveNodeMode::RemoveIncidentEdges,
                t => return Err(Error::Codec(format!("bad remove mode {t}"))),
            };
            Mutation::RemoveNode { node, mode }
        }
        5 => Mutation::AddEdge {
            id: EdgeId(r.u64()?),
            from: NodeId(r.u64()?),
            to: NodeId(r.u64()?),
            edge_type: EdgeType::new(r.str()?),
            weight: r.f32()?,
            properties: codec::read_properties(r)?,
        },
        6 => Mutation::SetEdgeWeight {
            edge: EdgeId(r.u64()?),
            weight: r.f32()?,
        },
        7 => Mutation::UpdateEdgeProperties {
            edge: EdgeId(r.u64()?),
            patch: read_patch(r)?,
        },
        8 => Mutation::RemoveEdge {
            edge: EdgeId(r.u64()?),
        },
        9 => {
            let n = r.u32()? as usize;
            let n = r.checked_len(n, 4)?; // each str is ≥ 4 bytes (u32 len prefix)
            let mut edge_types = Vec::with_capacity(n);
            for _ in 0..n {
                edge_types.push(EdgeType::new(r.str()?));
            }
            let min_weight = match r.u8()? {
                0 => None,
                1 => Some(r.f32()?),
                t => return Err(Error::Codec(format!("bad min_weight tag {t}"))),
            };
            let factor = r.f32()?;
            Mutation::DecayEdges {
                filter: crate::mutation::EdgeFilter {
                    edge_types,
                    min_weight,
                },
                factor,
            }
        }
        t => return Err(Error::Codec(format!("bad mutation tag {t}"))),
    })
}

// ---- Snapshot codec (full-graph image) ----

fn save_snapshot(store: &Store, epoch: u64) -> Vec<u8> {
    let mut w = Writer::default();
    w.buf.extend_from_slice(MAGIC);
    w.u32(FORMAT_VERSION);
    w.u64(epoch);
    w.u64(store.next_node_id);
    w.u64(store.next_edge_id);

    // Interners (label vectors — ids are positions).
    w.u32(store.node_types.labels().len() as u32);
    for l in store.node_types.labels() {
        w.str(l);
    }
    w.u32(store.edge_types.labels().len() as u32);
    for l in store.edge_types.labels() {
        w.str(l);
    }

    // Registered node types + embedding dims.
    w.u32(store.embedding_dim.len() as u32);
    // Deterministic order by type id.
    let mut dims: Vec<(&u32, &Option<usize>)> = store.embedding_dim.iter().collect();
    dims.sort_by_key(|(k, _)| **k);
    for (tid, dim) in dims {
        w.u32(*tid);
        match dim {
            None => w.u8(0),
            Some(d) => {
                w.u8(1);
                w.u64(*d as u64);
            }
        }
    }

    // Indexed property config (sorted for determinism).
    let mut indexed: Vec<&(String, String)> = store.indexed.iter().collect();
    indexed.sort();
    w.u32(indexed.len() as u32);
    for (t, k) in indexed {
        w.str(t);
        w.str(k);
    }

    // Nodes (explicit ids, sorted).
    let mut node_ids: Vec<&u64> = store.nodes.keys().collect();
    node_ids.sort();
    w.u64(node_ids.len() as u64);
    for id in node_ids {
        w.u64(*id);
        codec::write_node_record(&mut w, &store.nodes[id]);
    }

    // Edges (explicit ids, sorted).
    let mut edge_ids: Vec<&u64> = store.edges.keys().collect();
    edge_ids.sort();
    w.u64(edge_ids.len() as u64);
    for id in edge_ids {
        w.u64(*id);
        codec::write_edge_record(&mut w, &store.edges[id]);
    }

    // Trailing integrity checksum over everything after magic+version. Unlike the
    // WAL (framed per-record), the snapshot is one blob; without this a bit-flip
    // in a weight/property loads silently altered — the exact silent partial
    // blend PRD §10.2.1 forbids. Excluding magic+version keeps a version bump
    // surfacing as VersionMismatch rather than a checksum error.
    let crc = crc32(&w.buf[8..]);
    w.u32(crc);
    w.buf
}

/// Decode and fully validate a snapshot image, returning the reconstructed
/// store and the snapshot's epoch (its generation).
///
/// The image is decoded into a **fresh store** — never the caller's — and
/// handed over only after every check passes, so a failed load can never leave
/// a half-installed graph behind (PRD §10.2.1). Beyond structural decoding,
/// every invariant the public mutation API enforces is re-checked, because a
/// checksum only proves the bytes are what was written, not that what was
/// written is well-formed: duplicate ids, dangling endpoints, unsafe
/// allocators, out-of-range type ids, undeclared/mismatched embeddings,
/// non-finite weights, and trailing bytes are each a typed `Codec` error.
///
/// `base_indexed` carries the opening config's indexed-property registrations
/// (the snapshot's own registrations union in during decode); `config` gates
/// embedding dims exactly as the live `register_node_type` does.
fn load_snapshot(
    bytes: &[u8],
    base_indexed: &std::collections::HashSet<(String, String)>,
    config: &GraphConfig,
) -> Result<(Store, u64)> {
    let mut store = Store {
        indexed: base_indexed.clone(),
        ..Store::default()
    };

    let mut r = Reader::new(bytes);
    let magic = [r.u8()?, r.u8()?, r.u8()?, r.u8()?];
    if &magic != MAGIC {
        return Err(Error::Codec("bad snapshot magic".into()));
    }
    let version = r.u32()?;
    if version != FORMAT_VERSION {
        return Err(Error::VersionMismatch {
            found: version,
            supported: FORMAT_VERSION,
        });
    }
    // Verify the trailing payload checksum before decoding the body, so a
    // corrupt or truncated snapshot fails explicitly instead of loading altered
    // data (PRD §10.2.1). The CRC covers bytes[8..len-4]; magic+version are
    // excluded (handled above) and the trailing 4 bytes are the checksum itself.
    if bytes.len() < 12 {
        return Err(Error::Codec("snapshot too short for checksum".into()));
    }
    let split = bytes.len() - 4;
    let stored = u32::from_le_bytes(bytes[split..].try_into().unwrap());
    if crc32(&bytes[8..split]) != stored {
        return Err(Error::Codec("snapshot checksum mismatch".into()));
    }
    let epoch = r.u64()?;
    store.next_node_id = r.u64()?;
    store.next_edge_id = r.u64()?;

    let n = r.u32()? as usize;
    let n = r.checked_len(n, 4)?; // each label str is ≥ 4 bytes (u32 len prefix)
    let mut node_labels = Vec::with_capacity(n);
    for _ in 0..n {
        node_labels.push(r.str()?);
    }
    store.node_types = crate::interner::Interner::from_labels(node_labels)?;

    let n = r.u32()? as usize;
    let n = r.checked_len(n, 4)?;
    let mut edge_labels = Vec::with_capacity(n);
    for _ in 0..n {
        edge_labels.push(r.str()?);
    }
    store.edge_types = crate::interner::Interner::from_labels(edge_labels)?;

    // Interned type ids are validated against the loaded label tables before
    // insertion: an out-of-range id must be a typed Codec error, not a deferred
    // `label(..).unwrap()` panic in insert_*_raw / materialize_* on the read
    // path (PRD §18/§19).
    let node_type_count = store.node_types.labels().len() as u32;
    let edge_type_count = store.edge_types.labels().len() as u32;

    let n = r.u32()? as usize;
    for _ in 0..n {
        let tid = r.u32()?;
        let dim = match r.u8()? {
            0 => None,
            1 => Some(r.u64()? as usize),
            t => return Err(Error::Codec(format!("bad dim tag {t}"))),
        };
        if tid >= node_type_count {
            return Err(Error::Codec(format!(
                "embedding-dim entry names node_type id {tid} but only {node_type_count} \
                 types are interned"
            )));
        }
        // The same dim gates the live register_node_type applies — including
        // this open's config ceiling.
        crate::graph::validate_embedding_dim(config, dim)?;
        if store.embedding_dim.insert(tid, dim).is_some() {
            return Err(Error::Codec(format!(
                "duplicate embedding-dim entry for node_type id {tid}"
            )));
        }
        store.nodes_by_type.entry(tid).or_default();
    }

    let n = r.u32()? as usize;
    for _ in 0..n {
        let t = r.str()?;
        let k = r.str()?;
        store.indexed.insert((t, k));
    }

    let n = r.u64()?;
    // A node record is at least 17 bytes (id 8 + type 4 + embedding tag 1 +
    // property count 4), so a corrupt count cannot drive a huge reserve.
    let n = r.checked_len(
        usize::try_from(n)
            .map_err(|_| Error::Codec(format!("node count {n} does not fit in usize")))?,
        17,
    )?;
    store
        .nodes
        .try_reserve(n)
        .map_err(|e| Error::Codec(format!("snapshot node table reservation failed: {e}")))?;
    let mut max_node_id: Option<u64> = None;
    for _ in 0..n {
        let id = r.u64()?;
        let rec = codec::read_node_record(&mut r)?;
        if rec.node_type >= node_type_count {
            return Err(Error::Codec(format!(
                "node {id} has node_type id {} but only {node_type_count} types are interned",
                rec.node_type
            )));
        }
        // Registration invariant: a node's type must carry an embedding-dim
        // declaration (the live add_node's require_registered).
        let declared = store.embedding_dim.get(&rec.node_type).copied();
        let Some(declared) = declared else {
            return Err(Error::Codec(format!(
                "node {id} has unregistered node_type id {}",
                rec.node_type
            )));
        };
        if let Some(emb) = &rec.embedding {
            match declared {
                None => {
                    return Err(Error::Codec(format!(
                        "node {id} carries an embedding but its type declares none"
                    )))
                }
                Some(dim) if dim != emb.len() => {
                    return Err(Error::Codec(format!(
                        "node {id} embedding has dim {} but its type declares {dim}",
                        emb.len()
                    )))
                }
                Some(_) => {}
            }
            if emb.iter().any(|x| !x.is_finite()) {
                return Err(Error::Codec(format!(
                    "node {id} embedding contains a non-finite component"
                )));
            }
        }
        crate::store::validate_properties(&rec.properties)
            .map_err(|e| Error::Codec(format!("node {id}: {e}")))?;
        if store.nodes.contains_key(&id) {
            return Err(Error::Codec(format!("duplicate node id {id} in snapshot")));
        }
        store.insert_node_raw(id, rec);
        max_node_id = Some(max_node_id.map_or(id, |m| m.max(id)));
    }

    let n = r.u64()?;
    // An edge record is at least 36 bytes (id 8 + from 8 + to 8 + type 4 +
    // weight 4 + property count 4).
    let n = r.checked_len(
        usize::try_from(n)
            .map_err(|_| Error::Codec(format!("edge count {n} does not fit in usize")))?,
        36,
    )?;
    store
        .edges
        .try_reserve(n)
        .map_err(|e| Error::Codec(format!("snapshot edge table reservation failed: {e}")))?;
    let mut max_edge_id: Option<u64> = None;
    for _ in 0..n {
        let id = r.u64()?;
        let rec = codec::read_edge_record(&mut r)?;
        if rec.edge_type >= edge_type_count {
            return Err(Error::Codec(format!(
                "edge {id} has edge_type id {} but only {edge_type_count} types are interned",
                rec.edge_type
            )));
        }
        if !store.nodes.contains_key(&rec.from) {
            return Err(Error::Codec(format!(
                "edge {id} references missing source node {}",
                rec.from
            )));
        }
        if !store.nodes.contains_key(&rec.to) {
            return Err(Error::Codec(format!(
                "edge {id} references missing target node {}",
                rec.to
            )));
        }
        crate::store::validate_weight(rec.weight)
            .map_err(|e| Error::Codec(format!("edge {id}: {e}")))?;
        crate::store::validate_properties(&rec.properties)
            .map_err(|e| Error::Codec(format!("edge {id}: {e}")))?;
        if store.edges.contains_key(&id) {
            return Err(Error::Codec(format!("duplicate edge id {id} in snapshot")));
        }
        store.insert_edge_raw(id, rec);
        max_edge_id = Some(max_edge_id.map_or(id, |m| m.max(id)));
    }

    // Allocator collision-safety: `next_*` must clear every loaded id, or the
    // first post-load insert silently overwrites a live record (identity is
    // the sole addressing scheme — design commitment 5).
    if let Some(max) = max_node_id {
        if store.next_node_id <= max {
            return Err(Error::Codec(format!(
                "next_node_id {} does not clear the max loaded node id {max}",
                store.next_node_id
            )));
        }
    }
    if let Some(max) = max_edge_id {
        if store.next_edge_id <= max {
            return Err(Error::Codec(format!(
                "next_edge_id {} does not clear the max loaded edge id {max}",
                store.next_edge_id
            )));
        }
    }

    // Exact consumption: the reader must land exactly on the trailing CRC. A
    // CRC-valid image with undecoded bytes between the last record and the
    // checksum is malformed (records the writer never produced).
    if r.remaining() != 4 {
        return Err(Error::Codec(format!(
            "snapshot decode must end exactly at its trailing checksum; {} bytes remain",
            r.remaining()
        )));
    }

    Ok((store, epoch))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("drey_fault_{}_{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn person() -> NodeType {
        NodeType::new("person")
    }

    fn no_props() -> BTreeMap<String, Value> {
        BTreeMap::new()
    }

    /// The critical audit finding's exact scenario, now exercised: a failed
    /// commit write must surface an error, poison the persister (a retry
    /// errors — never a false `Ok`), and leave the on-disk graph at the last
    /// acknowledged state.
    #[test]
    fn injected_commit_write_failure_poisons_and_never_false_oks() {
        let dir = tmp("commit_write");
        let a;
        {
            let mut g = Graph::create(&dir, GraphConfig::default()).unwrap();
            g.register_node_type(person(), None).unwrap();
            a = g.add_node(person(), no_props()).unwrap();
            g.commit().unwrap();
            let _b = g.add_node(person(), no_props()).unwrap();
            fail::arm(&fail::WAL_WRITE);
            assert!(g.commit().is_err(), "injected write failure must surface");
            // Poisoned: the retry must error, not report Ok with data lost —
            // the exact false-Ok the audit's critical finding described.
            match g.commit() {
                Err(Error::Storage(m)) => assert!(m.contains("poisoned"), "{m}"),
                other => panic!("expected poisoned-storage error, got {other:?}"),
            }
            // New mutations refuse BEFORE touching the store (the preflight
            // path): an is_err() alone could hide a phantom in-memory node that
            // was never logged, so assert the state is untouched too.
            let before = g.counts();
            assert!(g.add_node(person(), no_props()).is_err());
            assert_eq!(
                g.counts(),
                before,
                "poisoned mutation must not mutate in-memory state"
            );
        }
        // Reopen recovers cleanly to the acknowledged state: only `a` was ever
        // confirmed durable.
        let g = Graph::open(&dir, GraphConfig::default()).unwrap();
        assert_eq!(g.counts().0, 1);
        assert!(g.node(a).unwrap().is_some());
    }

    /// Same contract when the write lands but the fsync fails. The batch may or
    /// may not have reached disk (an unacknowledged commit is allowed to appear
    /// after recovery — what is forbidden is a false `Ok`); the persister must
    /// poison either way.
    #[test]
    fn injected_commit_fsync_failure_poisons_and_never_false_oks() {
        let dir = tmp("commit_fsync");
        let a;
        {
            let mut g = Graph::create(&dir, GraphConfig::default()).unwrap();
            g.register_node_type(person(), None).unwrap();
            a = g.add_node(person(), no_props()).unwrap();
            g.commit().unwrap();
            let _b = g.add_node(person(), no_props()).unwrap();
            fail::arm(&fail::WAL_SYNC);
            assert!(g.commit().is_err(), "injected fsync failure must surface");
            match g.commit() {
                Err(Error::Storage(m)) => assert!(m.contains("poisoned"), "{m}"),
                other => panic!("expected poisoned-storage error, got {other:?}"),
            }
        }
        let g = Graph::open(&dir, GraphConfig::default()).unwrap();
        // `b`'s bytes were written before the failed fsync, so recovery may
        // legitimately load 1 or 2 nodes; `a` (acknowledged) must be present.
        let n = g.counts().0;
        assert!(n == 1 || n == 2, "unexpected node count {n}");
        assert!(g.node(a).unwrap().is_some());
    }

    /// A post-cutover snapshot failure (the new snapshot is already visible)
    /// must poison rather than leave the persister writing behind a stale
    /// epoch, and a reopen must load cleanly from the new snapshot.
    #[test]
    fn injected_post_cutover_failure_poisons_and_reopen_loads_snapshot() {
        let dir = tmp("cutover_fsync");
        let a;
        {
            let mut g = Graph::create(&dir, GraphConfig::default()).unwrap();
            g.register_node_type(person(), None).unwrap();
            a = g.add_node(person(), no_props()).unwrap();
            g.commit().unwrap();
            fail::arm(&fail::CUTOVER_DIR_FSYNC);
            assert!(
                g.snapshot().is_err(),
                "injected post-cutover failure must surface"
            );
            // Poisoned: further durable work refuses — a commit here would
            // append to a stale-epoch WAL and be silently discarded on the
            // next open (the audit's fsync'd-but-lost scenario). The refusal
            // happens before the store mutates (preflight), so counts hold.
            let before = g.counts();
            assert!(g.add_node(person(), no_props()).is_err());
            assert_eq!(
                g.counts(),
                before,
                "poisoned mutation must not mutate in-memory state"
            );
            match g.commit() {
                Err(Error::Storage(m)) => assert!(m.contains("poisoned"), "{m}"),
                other => panic!("expected poisoned-storage error, got {other:?}"),
            }
        }
        // The rename already happened: reopen loads the new-epoch snapshot and
        // skips the old-epoch WAL as stale. No acknowledged data is lost.
        let g = Graph::open(&dir, GraphConfig::default()).unwrap();
        assert_eq!(g.counts().0, 1);
        assert!(g.node(a).unwrap().is_some());
    }

    /// A poisoned persister must refuse `export` too: the in-memory graph may
    /// hold state the WAL can never replay, so an image of it is not a backup
    /// of anything durable.
    #[test]
    fn export_refuses_after_poisoned_commit() {
        let dir = tmp("export_poisoned");
        let mut g = Graph::create(&dir, GraphConfig::default()).unwrap();
        g.register_node_type(person(), None).unwrap();
        g.add_node(person(), no_props()).unwrap();
        g.commit().unwrap();
        g.add_node(person(), no_props()).unwrap();
        fail::arm(&fail::WAL_WRITE);
        assert!(g.commit().is_err());
        match g.export(dir.join("backup.drey")) {
            Err(Error::Storage(m)) => assert!(m.contains("poisoned"), "{m}"),
            other => panic!("expected poisoned-storage refusal, got {other:?}"),
        }
    }

    // ---- Malformed-image validation (issue #22 item 5) ----
    //
    // `load_snapshot` must reject every checksum-valid but logically malformed
    // image with a typed error. These images cannot be produced through the
    // public API, so they are built directly against the on-disk layout.

    /// Minimal parameterized snapshot image builder mirroring `save_snapshot`'s
    /// layout: two node labels' worth of schema, no properties (the
    /// per-invariant tests mutate exactly one aspect of a valid baseline).
    struct SnapImage {
        epoch: u64,
        next_node_id: u64,
        next_edge_id: u64,
        node_labels: Vec<&'static str>,
        edge_labels: Vec<&'static str>,
        /// `(interned type id, embedding dim tag)` entries.
        dims: Vec<(u32, Option<u64>)>,
        /// `(id, type id, embedding)` — properties always empty.
        nodes: Vec<(u64, u32, Option<Vec<f32>>)>,
        /// `(id, from, to, type id, weight)`.
        edges: Vec<(u64, u64, u64, u32, f32)>,
        /// Bytes smuggled between the last record and the checksum.
        trailing: Vec<u8>,
    }

    fn baseline() -> SnapImage {
        SnapImage {
            epoch: 0,
            next_node_id: 2,
            next_edge_id: 1,
            node_labels: vec!["person"],
            edge_labels: vec!["knows"],
            dims: vec![(0, Some(2))],
            nodes: vec![(0, 0, None), (1, 0, Some(vec![0.5, 0.5]))],
            edges: vec![(0, 0, 1, 0, 1.0)],
            trailing: Vec::new(),
        }
    }

    fn encode(img: &SnapImage) -> Vec<u8> {
        let mut w = Writer::default();
        w.buf.extend_from_slice(MAGIC);
        w.u32(FORMAT_VERSION);
        w.u64(img.epoch);
        w.u64(img.next_node_id);
        w.u64(img.next_edge_id);
        w.u32(img.node_labels.len() as u32);
        for l in &img.node_labels {
            w.str(l);
        }
        w.u32(img.edge_labels.len() as u32);
        for l in &img.edge_labels {
            w.str(l);
        }
        w.u32(img.dims.len() as u32);
        for (tid, dim) in &img.dims {
            w.u32(*tid);
            match dim {
                None => w.u8(0),
                Some(d) => {
                    w.u8(1);
                    w.u64(*d);
                }
            }
        }
        w.u32(0); // indexed-property registrations
        w.u64(img.nodes.len() as u64);
        for (id, tid, emb) in &img.nodes {
            w.u64(*id);
            w.u32(*tid);
            match emb {
                None => w.u8(0),
                Some(v) => {
                    w.u8(1);
                    w.u32(v.len() as u32);
                    for x in v {
                        w.f32(*x);
                    }
                }
            }
            w.u32(0); // properties
        }
        w.u64(img.edges.len() as u64);
        for (id, from, to, tid, weight) in &img.edges {
            w.u64(*id);
            w.u64(*from);
            w.u64(*to);
            w.u32(*tid);
            w.f32(*weight);
            w.u32(0); // properties
        }
        w.buf.extend_from_slice(&img.trailing);
        let crc = crc32(&w.buf[8..]);
        w.u32(crc);
        w.buf
    }

    fn load(img: &SnapImage) -> Result<(Store, u64)> {
        load_snapshot(
            &encode(img),
            &std::collections::HashSet::new(),
            &GraphConfig::default(),
        )
    }

    #[track_caller]
    fn assert_rejected(img: &SnapImage, needle: &str) {
        match load(img) {
            Err(Error::Codec(m)) => assert!(m.contains(needle), "message {m:?} missing {needle:?}"),
            Err(other) => panic!("expected Codec error containing {needle:?}, got {other}"),
            Ok(_) => panic!("malformed image ({needle}) loaded successfully"),
        }
    }

    #[test]
    fn valid_baseline_image_loads() {
        let (store, epoch) = load(&baseline()).unwrap();
        assert_eq!(epoch, 0);
        assert_eq!(store.nodes.len(), 2);
        assert_eq!(store.edges.len(), 1);
    }

    #[test]
    fn snapshot_with_duplicate_node_type_labels_rejected() {
        let mut img = baseline();
        img.node_labels = vec!["person", "person"];
        assert_rejected(&img, "duplicate interned type label");
    }

    #[test]
    fn snapshot_with_duplicate_edge_type_labels_rejected() {
        let mut img = baseline();
        img.edge_labels = vec!["knows", "knows"];
        assert_rejected(&img, "duplicate interned type label");
    }

    #[test]
    fn snapshot_with_duplicate_node_id_rejected() {
        let mut img = baseline();
        img.nodes = vec![(0, 0, None), (0, 0, None)];
        img.edges.clear();
        assert_rejected(&img, "duplicate node id");
    }

    #[test]
    fn snapshot_with_duplicate_edge_id_rejected() {
        let mut img = baseline();
        img.edges = vec![(0, 0, 1, 0, 1.0), (0, 1, 0, 0, 1.0)];
        img.next_edge_id = 1;
        assert_rejected(&img, "duplicate edge id");
    }

    #[test]
    fn snapshot_with_dangling_edge_endpoint_rejected() {
        let mut img = baseline();
        img.edges = vec![(0, 0, 99, 0, 1.0)];
        assert_rejected(&img, "missing target node");
        let mut img = baseline();
        img.edges = vec![(0, 99, 1, 0, 1.0)];
        assert_rejected(&img, "missing source node");
    }

    #[test]
    fn snapshot_with_unsafe_node_allocator_rejected() {
        // next_node_id == max loaded id: the first post-load add_node would
        // silently overwrite a live record (identity is the sole addressing
        // scheme — design commitment 5).
        let mut img = baseline();
        img.next_node_id = 1;
        assert_rejected(&img, "next_node_id");
    }

    #[test]
    fn snapshot_with_unsafe_edge_allocator_rejected() {
        let mut img = baseline();
        img.next_edge_id = 0;
        assert_rejected(&img, "next_edge_id");
    }

    #[test]
    fn snapshot_with_embedding_dim_mismatch_rejected() {
        let mut img = baseline();
        img.nodes[1].2 = Some(vec![0.5, 0.5, 0.5]); // declared dim is 2
        assert_rejected(&img, "declares");
    }

    #[test]
    fn snapshot_with_embedding_on_undeclared_type_rejected() {
        let mut img = baseline();
        img.dims = vec![(0, None)];
        assert_rejected(&img, "declares none");
    }

    #[test]
    fn snapshot_with_non_finite_embedding_component_rejected() {
        let mut img = baseline();
        img.nodes[1].2 = Some(vec![f32::NAN, 0.5]);
        assert_rejected(&img, "non-finite");
    }

    #[test]
    fn snapshot_with_non_finite_edge_weight_rejected() {
        let mut img = baseline();
        img.edges[0].4 = f32::INFINITY;
        assert_rejected(&img, "finite");
    }

    #[test]
    fn snapshot_with_out_of_range_dim_type_id_rejected() {
        let mut img = baseline();
        img.dims.push((7, Some(2))); // only 1 node label interned
        assert_rejected(&img, "embedding-dim entry");
    }

    #[test]
    fn snapshot_with_duplicate_dim_entry_rejected() {
        let mut img = baseline();
        img.dims = vec![(0, Some(2)), (0, Some(2))];
        assert_rejected(&img, "duplicate embedding-dim entry");
    }

    #[test]
    fn snapshot_with_node_of_unregistered_type_rejected() {
        // The label is interned but carries no embedding-dim declaration —
        // the live add_node's require_registered would refuse this type.
        let mut img = baseline();
        img.dims.clear();
        img.edges.clear();
        assert_rejected(&img, "unregistered");
    }

    #[test]
    fn snapshot_with_trailing_bytes_before_checksum_rejected() {
        // Checksum-valid (the CRC covers the smuggled bytes) but the decode
        // must land exactly on the trailing checksum.
        let mut img = baseline();
        img.trailing = vec![0xAB, 0xCD];
        assert_rejected(&img, "must end exactly");
    }

    #[test]
    fn snapshot_dim_exceeding_reopen_config_max_rejected() {
        // The same gate the live register_node_type applies: reopening under a
        // stricter max_embedding_dim is a typed error, not a silently retained
        // over-limit declaration.
        let img = baseline(); // declares dim 2
        let config = GraphConfig {
            max_embedding_dim: Some(1),
            ..GraphConfig::default()
        };
        match load_snapshot(&encode(&img), &std::collections::HashSet::new(), &config) {
            Err(Error::InvalidNodeType(m)) => assert!(m.contains("exceeds"), "{m}"),
            Err(other) => panic!("expected InvalidNodeType, got {other}"),
            Ok(_) => panic!("over-limit dim loaded under a stricter config"),
        }
    }

    // ---- WAL replay validation (review finding: replay bypassed live gates) ----

    /// A WAL holding `frames` (each one mutation) terminated by a commit marker.
    fn wal_bytes(epoch: u64, muts: &[Mutation]) -> Vec<u8> {
        let mut bytes = wal_header(epoch).to_vec();
        let mut buf = Vec::new();
        for m in muts {
            let mut w = Writer::default();
            w.u8(TAG_MUTATION);
            write_mutation(&mut w, m);
            write_frame(&mut buf, &w.buf).unwrap();
        }
        let mut w = Writer::default();
        w.u8(TAG_COMMIT);
        write_frame(&mut buf, &w.buf).unwrap();
        bytes.extend_from_slice(&buf);
        bytes
    }

    fn open_with_wal(name: &str, muts: &[Mutation]) -> Result<Graph> {
        let dir = tmp(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(WAL_FILE), wal_bytes(0, muts)).unwrap();
        Graph::open(&dir, GraphConfig::default())
    }

    #[test]
    fn replay_of_property_update_on_missing_node_fails_open() {
        // The old replay arm silently no-op'd here — a committed frame whose
        // target does not exist is a logically inconsistent WAL and must be a
        // typed error (PRD §10.2.1: never a silent partial load).
        let m = Mutation::UpdateNodeProperties {
            node: NodeId(7),
            patch: BTreeMap::new(),
        };
        match open_with_wal("replay_missing_node", &[m]) {
            Err(Error::NodeNotFound(id)) => assert_eq!(id, NodeId(7)),
            Err(other) => panic!("expected NodeNotFound, got {other}"),
            Ok(_) => panic!("inconsistent WAL opened successfully"),
        }
    }

    #[test]
    fn replay_of_property_update_on_missing_edge_fails_open() {
        let m = Mutation::UpdateEdgeProperties {
            edge: EdgeId(3),
            patch: BTreeMap::new(),
        };
        match open_with_wal("replay_missing_edge", &[m]) {
            Err(Error::EdgeNotFound(id)) => assert_eq!(id, EdgeId(3)),
            Err(other) => panic!("expected EdgeNotFound, got {other}"),
            Ok(_) => panic!("inconsistent WAL opened successfully"),
        }
    }

    #[test]
    fn replay_of_add_node_with_unregistered_type_fails_open() {
        // The live add_node requires registration; a WAL that adds a node of a
        // type never registered cannot have come from a healthy writer.
        let m = Mutation::AddNode {
            id: NodeId(0),
            node_type: NodeType::new("ghost"),
            properties: no_props(),
        };
        match open_with_wal("replay_unregistered", &[m]) {
            Err(Error::InvalidNodeType(_)) => {}
            Err(other) => panic!("expected InvalidNodeType, got {other}"),
            Ok(_) => panic!("inconsistent WAL opened successfully"),
        }
    }

    #[test]
    fn replay_of_non_finite_edge_weight_fails_open() {
        let muts = [
            Mutation::RegisterNodeType {
                node_type: person(),
                embedding_dim: None,
            },
            Mutation::AddNode {
                id: NodeId(0),
                node_type: person(),
                properties: no_props(),
            },
            Mutation::AddNode {
                id: NodeId(1),
                node_type: person(),
                properties: no_props(),
            },
            Mutation::AddEdge {
                id: EdgeId(0),
                from: NodeId(0),
                to: NodeId(1),
                edge_type: EdgeType::new("knows"),
                weight: f32::NAN,
                properties: no_props(),
            },
        ];
        match open_with_wal("replay_nan_weight", &muts) {
            Err(Error::InvalidPropertyValue(m)) => assert!(m.contains("finite"), "{m}"),
            Err(other) => panic!("expected InvalidPropertyValue, got {other}"),
            Ok(_) => panic!("inconsistent WAL opened successfully"),
        }
    }

    #[test]
    fn replay_of_legacy_nan_min_weight_decay_is_a_noop_not_an_error() {
        // Compat regression (review finding 3, reproduced with an old-crate
        // writer): pre-hardening builds of this same FORMAT_VERSION legally
        // committed DecayEdges frames with a NaN min_weight filter — the
        // filter matched nothing (`w >= NaN` is false). Replay must reproduce
        // that no-op, not refuse: the live decay_edges entry now rejects NaN
        // at the source, but a typed error here would permanently brick every
        // open of an existing graph.
        let muts = [
            Mutation::RegisterNodeType {
                node_type: person(),
                embedding_dim: None,
            },
            Mutation::AddNode {
                id: NodeId(0),
                node_type: person(),
                properties: no_props(),
            },
            Mutation::AddNode {
                id: NodeId(1),
                node_type: person(),
                properties: no_props(),
            },
            Mutation::AddEdge {
                id: EdgeId(0),
                from: NodeId(0),
                to: NodeId(1),
                edge_type: EdgeType::new("knows"),
                weight: 1.0,
                properties: no_props(),
            },
            Mutation::DecayEdges {
                filter: crate::mutation::EdgeFilter {
                    edge_types: Vec::new(),
                    min_weight: Some(f32::NAN),
                },
                factor: 0.5,
            },
        ];
        let g = open_with_wal("replay_legacy_nan_decay", &muts)
            .unwrap_or_else(|e| panic!("legacy NaN-min_weight WAL must open, got {e}"));
        assert_eq!(g.counts(), (2, 1));
        assert_eq!(
            g.edge(EdgeId(0)).unwrap().unwrap().weight,
            1.0,
            "the NaN filter matched nothing historically; replay must preserve that"
        );
    }

    #[test]
    fn zeroed_torn_pages_recover_when_unacknowledged_and_refuse_when_acked() {
        // Re-review round 2, finding 3: a zeroed torn page parses as a run of
        // CRC-valid len=0 frames (stored crc 0 == crc32(b"")), which round-1
        // classification passed straight to phase 3 — turning a plain torn
        // power loss into a permanent decode refusal. len==0 frames are now
        // structural damage.
        let reg = Mutation::RegisterNodeType {
            node_type: person(),
            embedding_dim: None,
        };
        let add = |id: u64| Mutation::AddNode {
            id: NodeId(id),
            node_type: person(),
            properties: no_props(),
        };
        let frame_of = |m: &Mutation| {
            let mut w = Writer::default();
            w.u8(TAG_MUTATION);
            write_mutation(&mut w, m);
            let mut buf = Vec::new();
            write_frame(&mut buf, &w.buf).unwrap();
            buf
        };
        let marker = {
            let mut w = Writer::default();
            w.u8(TAG_COMMIT);
            let mut buf = Vec::new();
            write_frame(&mut buf, &w.buf).unwrap();
            buf
        };

        // Case A — torn single commit: [reg][16 zero bytes][marker], nothing
        // after. The zeros are two len=0 pseudo-frames inside the final
        // batch; recovery must discard the batch (nothing was acknowledged),
        // not refuse and not brick in phase 3.
        let dir = tmp("zeroed_torn");
        fs::create_dir_all(&dir).unwrap();
        let mut wal = wal_header(0).to_vec();
        wal.extend_from_slice(&frame_of(&reg));
        wal.extend_from_slice(&[0u8; 16]);
        wal.extend_from_slice(&marker);
        fs::write(dir.join(WAL_FILE), &wal).unwrap();
        let g = Graph::open(&dir, GraphConfig::default())
            .unwrap_or_else(|e| panic!("zeroed torn commit must recover, got {e}"));
        assert_eq!(g.counts(), (0, 0), "the torn batch must be discarded");

        // Case B — the same zero run with a later committed batch after it:
        // the zeros sit in acknowledged territory and must refuse.
        let dir = tmp("zeroed_acked");
        fs::create_dir_all(&dir).unwrap();
        let mut wal = wal_header(0).to_vec();
        wal.extend_from_slice(&frame_of(&reg));
        wal.extend_from_slice(&[0u8; 16]);
        wal.extend_from_slice(&marker);
        wal.extend_from_slice(&frame_of(&add(0)));
        wal.extend_from_slice(&marker);
        fs::write(dir.join(WAL_FILE), &wal).unwrap();
        match Graph::open(&dir, GraphConfig::default()) {
            Err(Error::Codec(m)) => assert!(m.contains("acknowledged"), "{m}"),
            Err(other) => panic!("expected Codec refusal, got {other}"),
            Ok(_) => panic!("zeroed frames in acknowledged territory loaded silently"),
        }
    }

    #[test]
    fn replay_of_duplicate_add_node_id_fails_open() {
        let muts = [
            Mutation::RegisterNodeType {
                node_type: person(),
                embedding_dim: None,
            },
            Mutation::AddNode {
                id: NodeId(0),
                node_type: person(),
                properties: no_props(),
            },
            Mutation::AddNode {
                id: NodeId(0),
                node_type: person(),
                properties: no_props(),
            },
        ];
        match open_with_wal("replay_dup_node", &muts) {
            Err(Error::Codec(m)) => assert!(m.contains("reuses"), "{m}"),
            Err(other) => panic!("expected Codec error, got {other}"),
            Ok(_) => panic!("inconsistent WAL opened successfully"),
        }
    }
}
