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

use codec::{crc32, Reader, Writer};

use crate::config::GraphConfig;
use crate::error::{Error, Result};
use crate::graph::{Graph, Mutation};
use crate::store::{EdgeRecord, NodeRecord, Store};
use crate::types::{EdgeId, EdgeType, NodeId, NodeType, Value};

/// On-disk format version, written into the snapshot header and the WAL header.
/// Bumped on any incompatible format change; open fails with `VersionMismatch`
/// on a mismatch of either file, before any frame is decoded (PRD §10.2.1, §20).
///
/// v2: the snapshot gained a trailing CRC over its payload so a bit-flip is
/// detected on load rather than silently blended in (PRD §10.2.1). Pre-CRC
/// (v1) snapshots therefore fail cleanly with `VersionMismatch`.
pub const FORMAT_VERSION: u32 = 2;

const MAGIC: &[u8; 4] = b"DREY";
const SNAPSHOT_FILE: &str = "snapshot.bin";
const WAL_FILE: &str = "wal.log";
const LOCK_FILE: &str = "LOCK";

/// WAL header: `MAGIC(4) + FORMAT_VERSION(u32 LE) + epoch(u64 LE)`. The epoch is
/// the snapshot generation this WAL belongs to; `open` replays the WAL only when
/// its epoch is ≥ the snapshot's, so a WAL left un-truncated by a crash mid-
/// snapshot (its epoch is older) is recognized as stale and skipped rather than
/// re-applied on top of the snapshot that already contains it.
const WAL_HEADER_LEN: usize = 16;

/// WAL frame tags.
const TAG_MUTATION: u8 = 1;
const TAG_COMMIT: u8 = 2;

/// The persistence handle for a file-backed graph.
pub(crate) struct Persister {
    dir: PathBuf,
    wal: File,
    /// Snapshot generation this WAL belongs to (see [`WAL_HEADER_LEN`]).
    epoch: u64,
    /// Encoded mutation records not yet committed (fsync'd).
    pending: Vec<Vec<u8>>,
    locked: bool,
    /// Set when a durable operation failed after it may have left the WAL or
    /// epoch state inconsistent (a torn `commit` write, or a `snapshot` failure
    /// after the snapshot was already cut over). A poisoned persister refuses
    /// all further durable operations — the in-memory graph may hold mutations
    /// that are not on disk, so the consumer must reopen to recover rather than
    /// keep writing behind torn or stale bytes.
    poisoned: bool,
}

impl Persister {
    fn ensure_healthy(&self) -> Result<()> {
        if self.poisoned {
            return Err(Error::Storage(
                "persister poisoned by a prior failed durable operation; reopen the graph to recover"
                    .into(),
            ));
        }
        Ok(())
    }

    /// Append a mutation to the pending buffer. It becomes durable only at the
    /// next [`Persister::commit`].
    pub(crate) fn append(&mut self, mutation: &Mutation) -> Result<()> {
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
    pub(crate) fn commit(&mut self) -> Result<()> {
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
            write_frame(&mut frame_buf, payload);
        }
        // Commit marker terminates the batch.
        let mut marker = Writer::default();
        marker.u8(TAG_COMMIT);
        write_frame(&mut frame_buf, &marker.buf);

        if let Err(e) = self
            .wal
            .write_all(&frame_buf)
            .and_then(|()| self.wal.sync_all())
        {
            self.poisoned = true;
            return Err(e.into());
        }
        // Durable now — safe to forget the batch.
        self.pending.clear();
        Ok(())
    }
}

impl Drop for Persister {
    fn drop(&mut self) {
        if self.locked {
            let _ = fs::remove_file(self.dir.join(LOCK_FILE));
        }
    }
}

/// The 16-byte WAL header for a given epoch.
fn wal_header(epoch: u64) -> [u8; WAL_HEADER_LEN] {
    let mut h = [0u8; WAL_HEADER_LEN];
    h[0..4].copy_from_slice(MAGIC);
    h[4..8].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    h[8..16].copy_from_slice(&epoch.to_le_bytes());
    h
}

/// fsync a directory so a preceding `rename`/`create` is durable (Unix). Making
/// the metadata change durable is what closes the snapshot cutover window.
fn fsync_dir(dir: &Path) -> Result<()> {
    File::open(dir)?.sync_all()?;
    Ok(())
}

/// Write a length+crc framed record into `out`.
fn write_frame(out: &mut Vec<u8>, payload: &[u8]) {
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&crc32(payload).to_le_bytes());
    out.extend_from_slice(payload);
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
        let mut wal = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(WAL_FILE))?;
        wal.write_all(&wal_header(0))?;
        wal.sync_all()?;
        fsync_dir(&dir)?;
        graph.persist = Some(Persister {
            dir,
            wal,
            epoch: 0,
            pending: Vec::new(),
            locked,
            poisoned: false,
        });
        Ok(graph)
    }

    /// Open an existing file-backed graph, replaying snapshot + committed WAL.
    /// Fails if absent.
    pub fn open(path: impl AsRef<Path>, config: GraphConfig) -> Result<Self> {
        let dir = path.as_ref().to_path_buf();
        if !dir.join(SNAPSHOT_FILE).exists() && !dir.join(WAL_FILE).exists() {
            return Err(Error::Storage(format!(
                "no graph at {}; use create()",
                dir.display()
            )));
        }
        let mut graph = Graph::in_memory(config.clone());

        // 1. Replay snapshot if present; its epoch bounds which WAL is current.
        let snapshot_present = dir.join(SNAPSHOT_FILE).exists();
        let mut snap_epoch = 0u64;
        if snapshot_present {
            let bytes = fs::read(dir.join(SNAPSHOT_FILE))?;
            snap_epoch = load_snapshot(&mut graph.store, &bytes)?;
        }

        // 2. Replay the committed WAL prefix, unless the WAL is stale (its epoch
        //    predates the snapshot, i.e. a crash left it un-truncated).
        let mut replay = WalReplay {
            epoch: snap_epoch,
            valid_len: WAL_HEADER_LEN,
            stale: true,
        };
        if dir.join(WAL_FILE).exists() {
            let bytes = fs::read(dir.join(WAL_FILE))?;
            replay = replay_wal(&mut graph, &bytes, snap_epoch)?;
        }

        // A WAL that belongs to snapshot generation ≥ 1 with no snapshot on disk
        // proves the snapshot was lost. Replaying its post-snapshot mutations onto
        // an empty store would be a silent partial load — edges pointing at nodes
        // that only existed in the missing snapshot. Fail explicitly (PRD §10.2.1:
        // "never a silent partial load").
        if !snapshot_present && replay.epoch >= 1 {
            return Err(Error::Storage(format!(
                "WAL at {} belongs to snapshot epoch {} but snapshot.bin is missing; \
                 refusing a partial load",
                dir.display(),
                replay.epoch
            )));
        }

        graph.store.sort_indexes();

        // 3. Read-only opens attach no writer (PRD §9.1); writable opens do — and
        //    must normalize the on-disk WAL first, so new commits are not stranded
        //    behind stale or torn bytes. A stale WAL (or none) is rewritten to a
        //    header-only file at the snapshot epoch; a replayed WAL is truncated to
        //    its last committed frame, dropping any torn trailing bytes.
        if !config.read_only {
            let locked = acquire_lock(&dir, &config)?;
            let wal_path = dir.join(WAL_FILE);
            let epoch = if replay.stale || !wal_path.exists() {
                let mut f = OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&wal_path)?;
                f.write_all(&wal_header(snap_epoch))?;
                f.sync_all()?;
                fsync_dir(&dir)?;
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
            graph.persist = Some(Persister {
                dir,
                wal,
                epoch,
                pending: Vec::new(),
                locked,
                poisoned: false,
            });
        }
        Ok(graph)
    }

    /// Compact: write a full-graph snapshot and reset the WAL. The snapshot is
    /// written to a temp file, fsync'd, atomically renamed, and the parent
    /// directory fsync'd — so a crash leaves either the old or new snapshot,
    /// never a partial one. The new snapshot carries a bumped epoch, and the WAL
    /// is re-headered with that epoch. If a crash lands between the rename and
    /// the WAL reset, the leftover WAL has the *old* epoch, so `open` sees it as
    /// stale and skips it instead of double-applying (PRD §10.2.1).
    pub fn snapshot(&mut self) -> Result<()> {
        let (dir, new_epoch) = match &self.persist {
            Some(p) => (p.dir.clone(), p.epoch + 1),
            None => return Err(Error::Storage("cannot snapshot an in-memory graph".into())),
        };
        // Flush anything pending first so the snapshot reflects all commits.
        self.commit()?;

        let bytes = save_snapshot(&self.store, new_epoch);
        let tmp = dir.join("snapshot.bin.tmp");
        {
            let mut f = File::create(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, dir.join(SNAPSHOT_FILE))?; // atomic replace
        fsync_dir(&dir)?; // make the rename durable — the cutover point

        // Past the cutover: the snapshot on disk is now the new epoch. If any of
        // the WAL reset, its fsyncs, the handle swap, or the epoch bump fails, the
        // on-disk WAL and the in-memory epoch/handle can disagree with the new
        // snapshot — a later commit would then append to a stale-epoch or
        // headerless WAL and be silently discarded/stranded on the next open. So
        // any post-cutover failure poisons the persister: the graph must be
        // reopened (which loads cleanly from the new snapshot) rather than
        // continue writing.
        let reset = (|| -> Result<()> {
            {
                let mut wal = OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .create(true)
                    .open(dir.join(WAL_FILE))?;
                wal.write_all(&wal_header(new_epoch))?;
                wal.sync_all()?;
            }
            fsync_dir(&dir)?;
            let new_wal = OpenOptions::new().append(true).open(dir.join(WAL_FILE))?;
            if let Some(p) = self.persist.as_mut() {
                p.wal = new_wal;
                p.epoch = new_epoch;
                p.pending.clear();
            }
            Ok(())
        })();
        if let Err(e) = reset {
            if let Some(p) = self.persist.as_mut() {
                p.poisoned = true;
            }
            return Err(e);
        }
        Ok(())
    }

    /// Export a portable full-graph image to a single file (PRD §9.2, §22).
    /// Same encoding as a snapshot; restores the exact id space on import.
    pub fn export(&self, path: impl AsRef<Path>) -> Result<()> {
        // A portable image is not tied to a WAL; carry the current epoch so a
        // re-import into a file-backed graph starts consistently.
        let epoch = self.persist.as_ref().map_or(0, |p| p.epoch);
        let bytes = save_snapshot(&self.store, epoch);
        fs::write(path, bytes)?;
        Ok(())
    }

    /// Import a graph image produced by [`Graph::export`] into a fresh
    /// in-memory graph, restoring the exact id space (PRD §10.2, §22).
    pub fn import(path: impl AsRef<Path>, config: GraphConfig) -> Result<Self> {
        let bytes = fs::read(path)?;
        let mut graph = Graph::in_memory(config);
        load_snapshot(&mut graph.store, &bytes)?;
        graph.store.sort_indexes();
        Ok(graph)
    }
}

/// The current boot's id (Linux), used to detect a lock left by a *previous*
/// boot. Empty when unavailable, in which case boot-based staleness is skipped.
fn boot_id() -> String {
    fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Whether a process is alive. Linux: `/proc/<pid>` exists iff it is. Elsewhere,
/// conservatively assume alive (never reclaim on the liveness signal).
fn pid_alive(pid: u32) -> bool {
    if cfg!(target_os = "linux") {
        Path::new("/proc").join(pid.to_string()).exists()
    } else {
        true
    }
}

/// Create the lock file exclusively and stamp it with `pid boot_id`.
fn create_lock(lock: &Path) -> std::io::Result<bool> {
    match OpenOptions::new().write(true).create_new(true).open(lock) {
        Ok(mut f) => {
            // A half-stamped lock reads as malformed and would be reclaimed as
            // stale by another opener while we believe we hold it — two writers.
            // Propagate the error and remove the partial file instead.
            if let Err(e) =
                write!(f, "{} {}", std::process::id(), boot_id()).and_then(|()| f.sync_all())
            {
                let _ = fs::remove_file(lock);
                return Err(e);
            }
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e),
    }
}

/// Whether an existing lock is stale — its holder rebooted or died. A hard crash
/// (SIGKILL/OOM/power loss) never runs `Persister::drop`, so without this a stale
/// LOCK would wedge the graph permanently (recovery-matrix row 1).
fn lock_is_stale(lock: &Path) -> bool {
    let content = fs::read_to_string(lock).unwrap_or_default();
    let mut parts = content.split_whitespace();
    let holder_pid: Option<u32> = parts.next().and_then(|s| s.parse().ok());
    let holder_boot = parts.next().unwrap_or("");
    let cur_boot = boot_id();
    match holder_pid {
        None => true, // malformed / legacy lock — treat as stale
        Some(pid) => (!cur_boot.is_empty() && holder_boot != cur_boot) || !pid_alive(pid),
    }
}

fn acquire_lock(dir: &Path, config: &GraphConfig) -> Result<bool> {
    if !config.file_lock {
        return Ok(false);
    }
    let lock = dir.join(LOCK_FILE);
    // Atomic exclusive create; on conflict, reclaim only if the holder is
    // provably gone (different boot, or dead pid), then retry once.
    if create_lock(&lock)? {
        return Ok(true);
    }
    if lock_is_stale(&lock) {
        let _ = fs::remove_file(&lock);
        if create_lock(&lock)? {
            return Ok(true);
        }
        // Someone else reclaimed it first — it is now live.
    }
    Err(Error::LockConflict(format!(
        "another live writer holds {}",
        lock.display()
    )))
}

// ---- WAL replay ----

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

/// Replay the committed prefix of the WAL. Parses and version-gates the header
/// first; if the WAL epoch predates `snap_epoch` the WAL is stale (already folded
/// into the snapshot) and nothing is replayed. Returns where the committed prefix
/// ends so the caller can repair the file.
fn replay_wal(graph: &mut Graph, bytes: &[u8], snap_epoch: u64) -> Result<WalReplay> {
    // A WAL shorter than its header carries no valid content.
    if bytes.len() < WAL_HEADER_LEN {
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

    // Decode frames after the header, buffering records between commit markers.
    // Only fully committed batches are applied; a trailing incomplete or torn
    // batch is discarded (PRD §10.2.1). `last_commit_end` tracks the byte offset
    // just past the last commit marker — the repair truncation point.
    let mut pos = WAL_HEADER_LEN;
    let mut last_commit_end = WAL_HEADER_LEN;
    let mut staged: Vec<Mutation> = Vec::new();
    let mut committed: Vec<Mutation> = Vec::new();

    while pos < bytes.len() {
        // Frame header: u32 len, u32 crc.
        if pos + 8 > bytes.len() {
            break; // torn header → stop, discard staged
        }
        let len = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        let crc = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap());
        let start = pos + 8;
        if start + len > bytes.len() {
            break; // torn payload → stop
        }
        let payload = &bytes[start..start + len];
        if crc32(payload) != crc {
            break; // corrupt record → stop at last good commit
        }
        pos = start + len;

        let mut r = Reader::new(payload);
        match r.u8()? {
            TAG_MUTATION => staged.push(read_mutation(&mut r)?),
            TAG_COMMIT => {
                committed.append(&mut staged); // promote the batch
                staged.clear();
                last_commit_end = pos; // durable through here
            }
            t => return Err(Error::Codec(format!("bad WAL frame tag {t}"))),
        }
    }
    // `staged` holds an uncommitted trailing batch — discard it.

    let mut max_node = graph.store.next_node_id;
    let mut max_edge = graph.store.next_edge_id;
    for m in committed {
        apply_replay(graph, m, &mut max_node, &mut max_edge)?;
    }
    graph.store.next_node_id = graph.store.next_node_id.max(max_node);
    graph.store.next_edge_id = graph.store.next_edge_id.max(max_edge);
    Ok(WalReplay {
        epoch: wal_epoch,
        valid_len: last_commit_end,
        stale: false,
    })
}

/// Apply a committed mutation during replay, using explicit ids (no allocation)
/// so the id space is restored exactly (PRD §7.4).
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
            store.register_node_type(&node_type, embedding_dim)?;
        }
        Mutation::AddNode {
            id,
            node_type,
            properties,
        } => {
            let type_id = store.node_types.intern(node_type.as_str());
            store.insert_node_raw(
                id.0,
                NodeRecord {
                    node_type: type_id,
                    properties,
                    embedding: None,
                },
            );
            *max_node = (*max_node).max(id.0 + 1);
        }
        Mutation::SetNodeEmbedding { node, embedding } => {
            store.set_node_embedding(node, embedding)?;
        }
        Mutation::UpdateNodeProperties { node, patch } => {
            if let Some(rec) = store.nodes.get(&node.0) {
                let old = rec.properties.clone();
                let mut new = old.clone();
                crate::store::apply_patch(&mut new, &patch);
                store.reindex_node(node.0, &old, &new);
                store.nodes.get_mut(&node.0).unwrap().properties = new;
            }
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
            let type_id = store.edge_types.intern(edge_type.as_str());
            store.insert_edge_raw(
                id.0,
                EdgeRecord {
                    from: from.0,
                    to: to.0,
                    edge_type: type_id,
                    weight,
                    properties,
                },
            );
            *max_edge = (*max_edge).max(id.0 + 1);
        }
        Mutation::SetEdgeWeight { edge, weight } => {
            store.set_edge_weight(edge, weight)?;
        }
        Mutation::UpdateEdgeProperties { edge, patch } => {
            if let Some(rec) = store.edges.get_mut(&edge.0) {
                crate::store::apply_patch(&mut rec.properties, &patch);
            }
        }
        Mutation::RemoveEdge { edge } => {
            store.remove_edge(edge)?;
        }
        Mutation::DecayEdges { filter, factor } => {
            let ids = graph.edges_matching(&filter);
            for e in ids {
                let w = graph.store.edges[&e].weight;
                graph.store.edges.get_mut(&e).unwrap().weight = w * factor;
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

/// Load a snapshot into `store`, returning its epoch (the snapshot generation).
fn load_snapshot(store: &mut Store, bytes: &[u8]) -> Result<u64> {
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
    store.node_types = crate::interner::Interner::from_labels(node_labels);

    let n = r.u32()? as usize;
    let n = r.checked_len(n, 4)?;
    let mut edge_labels = Vec::with_capacity(n);
    for _ in 0..n {
        edge_labels.push(r.str()?);
    }
    store.edge_types = crate::interner::Interner::from_labels(edge_labels);

    let n = r.u32()? as usize;
    for _ in 0..n {
        let tid = r.u32()?;
        let dim = match r.u8()? {
            0 => None,
            1 => Some(r.u64()? as usize),
            t => return Err(Error::Codec(format!("bad dim tag {t}"))),
        };
        store.embedding_dim.insert(tid, dim);
    }

    let n = r.u32()? as usize;
    for _ in 0..n {
        let t = r.str()?;
        let k = r.str()?;
        store.indexed.insert((t, k));
    }

    // Interned type ids are validated against the loaded label tables before
    // insertion: an out-of-range id (corruption — there is no snapshot checksum)
    // must be a typed Codec error, not a deferred `label(..).unwrap()` panic in
    // insert_*_raw / materialize_* on the read path (PRD §18/§19).
    let node_type_count = store.node_types.labels().len() as u32;
    let edge_type_count = store.edge_types.labels().len() as u32;

    let n = r.u64()?;
    for _ in 0..n {
        let id = r.u64()?;
        let rec = codec::read_node_record(&mut r)?;
        if rec.node_type >= node_type_count {
            return Err(Error::Codec(format!(
                "node {id} has node_type id {} but only {node_type_count} types are interned",
                rec.node_type
            )));
        }
        store.insert_node_raw(id, rec);
    }

    let n = r.u64()?;
    for _ in 0..n {
        let id = r.u64()?;
        let rec = codec::read_edge_record(&mut r)?;
        if rec.edge_type >= edge_type_count {
            return Err(Error::Codec(format!(
                "edge {id} has edge_type id {} but only {edge_type_count} types are interned",
                rec.edge_type
            )));
        }
        store.insert_edge_raw(id, rec);
    }

    Ok(epoch)
}
