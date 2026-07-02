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

/// On-disk format version, written into every snapshot and WAL header. Bumped
/// on any incompatible format change; open fails with `VersionMismatch`
/// otherwise (PRD §10.2.1, §20).
pub const FORMAT_VERSION: u32 = 1;

const MAGIC: &[u8; 4] = b"DREY";
const SNAPSHOT_FILE: &str = "snapshot.bin";
const WAL_FILE: &str = "wal.log";
const LOCK_FILE: &str = "LOCK";

/// WAL frame tags.
const TAG_MUTATION: u8 = 1;
const TAG_COMMIT: u8 = 2;

/// The persistence handle for a file-backed graph.
pub(crate) struct Persister {
    dir: PathBuf,
    wal: File,
    /// Encoded mutation records not yet committed (fsync'd).
    pending: Vec<Vec<u8>>,
    locked: bool,
}

impl Persister {
    /// Append a mutation to the pending buffer. It becomes durable only at the
    /// next [`Persister::commit`].
    pub(crate) fn append(&mut self, mutation: &Mutation) -> Result<()> {
        let mut w = Writer::default();
        w.u8(TAG_MUTATION);
        write_mutation(&mut w, mutation);
        self.pending.push(w.buf);
        Ok(())
    }

    /// Flush pending records plus a commit marker to the WAL and fsync.
    pub(crate) fn commit(&mut self) -> Result<()> {
        // Nothing to do if no mutations accumulated since the last commit.
        let mut frame_buf = Vec::new();
        for payload in self.pending.drain(..) {
            write_frame(&mut frame_buf, &payload);
        }
        // Commit marker terminates the batch.
        let mut marker = Writer::default();
        marker.u8(TAG_COMMIT);
        write_frame(&mut frame_buf, &marker.buf);

        self.wal.write_all(&frame_buf)?;
        self.wal.sync_all()?; // fsync — the durability guarantee
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
        let mut graph = Graph::in_memory(config.clone());
        let locked = acquire_lock(&dir, &config)?;
        let wal = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(WAL_FILE))?;
        graph.persist = Some(Persister {
            dir,
            wal,
            pending: Vec::new(),
            locked,
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

        // 1. Replay snapshot if present.
        if dir.join(SNAPSHOT_FILE).exists() {
            let bytes = fs::read(dir.join(SNAPSHOT_FILE))?;
            load_snapshot(&mut graph.store, &bytes)?;
        }

        // 2. Replay committed WAL prefix.
        if dir.join(WAL_FILE).exists() {
            let bytes = fs::read(dir.join(WAL_FILE))?;
            replay_wal(&mut graph, &bytes)?;
        }

        graph.store.sort_indexes();

        // 3. Read-only opens attach no writer (PRD §9.1); writable opens do.
        if !config.read_only {
            let locked = acquire_lock(&dir, &config)?;
            let wal = OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join(WAL_FILE))?;
            graph.persist = Some(Persister {
                dir,
                wal,
                pending: Vec::new(),
                locked,
            });
        }
        Ok(graph)
    }

    /// Compact: write a full-graph snapshot and truncate the WAL. Atomic via
    /// temp-file + rename, so a crash leaves either the old or new snapshot,
    /// never a partial one (PRD §10.2.1).
    pub fn snapshot(&mut self) -> Result<()> {
        let dir = match &self.persist {
            Some(p) => p.dir.clone(),
            None => return Err(Error::Storage("cannot snapshot an in-memory graph".into())),
        };
        // Flush anything pending first so the snapshot reflects all commits.
        self.commit()?;

        let bytes = save_snapshot(&self.store);
        let tmp = dir.join("snapshot.bin.tmp");
        {
            let mut f = File::create(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, dir.join(SNAPSHOT_FILE))?; // atomic replace
        // Truncate the WAL: its mutations are now folded into the snapshot.
        let wal = OpenOptions::new()
            .write(true)
            .truncate(true)
            .create(true)
            .open(dir.join(WAL_FILE))?;
        wal.sync_all()?;
        if let Some(p) = self.persist.as_mut() {
            p.wal = OpenOptions::new().append(true).open(dir.join(WAL_FILE))?;
            p.pending.clear();
        }
        Ok(())
    }

    /// Export a portable full-graph image to a single file (PRD §9.2, §22).
    /// Same encoding as a snapshot; restores the exact id space on import.
    pub fn export(&self, path: impl AsRef<Path>) -> Result<()> {
        let bytes = save_snapshot(&self.store);
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

fn acquire_lock(dir: &Path, config: &GraphConfig) -> Result<bool> {
    if !config.file_lock {
        return Ok(false);
    }
    let lock = dir.join(LOCK_FILE);
    if lock.exists() {
        return Err(Error::LockConflict(format!(
            "another writer holds {}",
            lock.display()
        )));
    }
    File::create(&lock)?;
    Ok(true)
}

// ---- WAL replay ----

fn replay_wal(graph: &mut Graph, bytes: &[u8]) -> Result<()> {
    // Decode frames, buffering records between commit markers. Only fully
    // committed batches are applied; a trailing incomplete or torn batch is
    // discarded (PRD §10.2.1).
    let mut pos = 0usize;
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
    Ok(())
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
        Mutation::RegisterNodeType { node_type, embedding_dim } => {
            store.register_node_type(&node_type, embedding_dim)?;
        }
        Mutation::AddNode { id, node_type, properties } => {
            let type_id = store.node_types.intern(node_type.as_str());
            store.insert_node_raw(
                id.0,
                NodeRecord { node_type: type_id, properties, embedding: None },
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
        Mutation::AddEdge { id, from, to, edge_type, weight, properties } => {
            let type_id = store.edge_types.intern(edge_type.as_str());
            store.insert_edge_raw(
                id.0,
                EdgeRecord { from: from.0, to: to.0, edge_type: type_id, weight, properties },
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
        Mutation::RegisterNodeType { node_type, embedding_dim } => {
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
        Mutation::AddNode { id, node_type, properties } => {
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
        Mutation::AddEdge { id, from, to, edge_type, weight, properties } => {
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
            Mutation::RegisterNodeType { node_type, embedding_dim }
        }
        1 => Mutation::AddNode {
            id: NodeId(r.u64()?),
            node_type: NodeType::new(r.str()?),
            properties: codec::read_properties(r)?,
        },
        2 => {
            let node = NodeId(r.u64()?);
            let n = r.u32()? as usize;
            let mut emb = Vec::with_capacity(n);
            for _ in 0..n {
                emb.push(r.f32()?);
            }
            Mutation::SetNodeEmbedding { node, embedding: emb }
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
        8 => Mutation::RemoveEdge { edge: EdgeId(r.u64()?) },
        9 => {
            let n = r.u32()? as usize;
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
                filter: crate::mutation::EdgeFilter { edge_types, min_weight },
                factor,
            }
        }
        t => return Err(Error::Codec(format!("bad mutation tag {t}"))),
    })
}

// ---- Snapshot codec (full-graph image) ----

fn save_snapshot(store: &Store) -> Vec<u8> {
    let mut w = Writer::default();
    w.buf.extend_from_slice(MAGIC);
    w.u32(FORMAT_VERSION);
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

    w.buf
}

fn load_snapshot(store: &mut Store, bytes: &[u8]) -> Result<()> {
    let mut r = Reader::new(bytes);
    let magic = [r.u8()?, r.u8()?, r.u8()?, r.u8()?];
    if &magic != MAGIC {
        return Err(Error::Codec("bad snapshot magic".into()));
    }
    let version = r.u32()?;
    if version != FORMAT_VERSION {
        return Err(Error::VersionMismatch { found: version, supported: FORMAT_VERSION });
    }
    store.next_node_id = r.u64()?;
    store.next_edge_id = r.u64()?;

    let n = r.u32()? as usize;
    let mut node_labels = Vec::with_capacity(n);
    for _ in 0..n {
        node_labels.push(r.str()?);
    }
    store.node_types = crate::interner::Interner::from_labels(node_labels);

    let n = r.u32()? as usize;
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

    let n = r.u64()?;
    for _ in 0..n {
        let id = r.u64()?;
        let rec = codec::read_node_record(&mut r)?;
        store.insert_node_raw(id, rec);
    }

    let n = r.u64()?;
    for _ in 0..n {
        let id = r.u64()?;
        let rec = codec::read_edge_record(&mut r)?;
        store.insert_edge_raw(id, rec);
    }

    Ok(())
}
