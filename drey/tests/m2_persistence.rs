//! M2 exit criteria: persistence round-trip and the PRD §10.2.1 recovery matrix.

use std::fs::{self, OpenOptions};
use std::path::PathBuf;

use drey::config::GraphConfig;
use drey::mutation::{EdgeFilter, PropertyPatch, RemoveNodeMode, WeightUpdate};
use drey::query::{PropertyQuery, ScalarPredicate};
use drey::types::{Embedding, Node, NodeType, Scalar, Value};
use drey::{EdgeType, Error, Graph, NodeId};
use std::collections::BTreeMap;

fn person() -> NodeType {
    NodeType::new("person")
}
fn knows() -> EdgeType {
    EdgeType::new("knows")
}
fn props(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

/// A unique temp directory for one test, cleaned first.
fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("drey_m2_{}_{}", std::process::id(), name));
    let _ = fs::remove_dir_all(&dir);
    dir
}

fn config() -> GraphConfig {
    GraphConfig::default().with_indexed_property(person(), "age")
}

#[test]
fn create_open_distinctness() {
    let dir = tmp("create_open");
    // open before create fails
    assert!(Graph::open(&dir, config()).is_err());
    let g = Graph::create(&dir, config()).unwrap();
    drop(g);
    // create over an existing graph fails
    assert!(Graph::create(&dir, config()).is_err());
    // open now succeeds
    assert!(Graph::open(&dir, config()).is_ok());
}

#[test]
fn round_trip_preserves_everything_byte_exact() {
    let dir = tmp("round_trip");
    let (a, b, e);
    {
        let mut g = Graph::create(&dir, config()).unwrap();
        g.register_node_type(person(), Some(4)).unwrap();
        a = g
            .add_node(
                person(),
                props(&[
                    ("age", Value::I64(30)),
                    ("tag", Value::Bytes(vec![0, 255, 7])),
                ]),
            )
            .unwrap();
        b = g
            .add_node(person(), props(&[("age", Value::I64(40))]))
            .unwrap();
        // Hostile f32 bit patterns: denormal and negative zero.
        let emb = Embedding::new(vec![f32::from_bits(1), -0.0, 0.1, f32::MIN_POSITIVE]);
        g.set_node_embedding(a, emb).unwrap();
        e = g.add_edge(a, b, knows(), 0.75, props(&[])).unwrap();
        g.commit().unwrap();
    }
    // Reopen and verify.
    let g = Graph::open(&dir, config()).unwrap();
    assert_eq!(g.counts(), (2, 1));
    // Durable ids (PRD §7.4).
    let na = g.node(a).unwrap().unwrap();
    assert_eq!(na.properties.get("age"), Some(&Value::I64(30)));
    assert_eq!(
        na.properties.get("tag"),
        Some(&Value::Bytes(vec![0, 255, 7]))
    );
    // Byte-exact embedding.
    let emb = na.embedding.unwrap();
    assert_eq!(emb.0[0].to_bits(), f32::from_bits(1).to_bits());
    assert_eq!(emb.0[1].to_bits(), (-0.0_f32).to_bits());
    // Edge and weight preserved.
    assert_eq!(g.edge(e).unwrap().unwrap().weight, 0.75);
    // Index rebuilt: property query works after reload.
    let hits = g
        .nodes_by_property(PropertyQuery {
            node_type: person(),
            key: "age".into(),
            predicate: ScalarPredicate::Eq(Scalar::I64(40)),
        })
        .unwrap();
    assert_eq!(hits, vec![b]);
}

#[test]
fn recovery_crash_before_commit_loses_uncommitted() {
    let dir = tmp("crash_before_commit");
    let committed;
    {
        let mut g = Graph::create(&dir, config()).unwrap();
        g.register_node_type(person(), None).unwrap();
        committed = g.add_node(person(), props(&[])).unwrap();
        g.commit().unwrap();
        // Mutate WITHOUT committing, then "crash" (drop) — pending is discarded.
        let _uncommitted = g.add_node(person(), props(&[])).unwrap();
    }
    let g = Graph::open(&dir, config()).unwrap();
    // The committed node survives; the uncommitted one is gone.
    assert!(g.node(committed).unwrap().is_some());
    assert_eq!(g.counts().0, 1);
}

#[test]
fn recovery_crash_during_commit_loads_prior() {
    let dir = tmp("crash_during_commit");
    let a;
    let size_after_first;
    {
        let mut g = Graph::create(&dir, config()).unwrap();
        g.register_node_type(person(), None).unwrap();
        a = g.add_node(person(), props(&[])).unwrap();
        g.commit().unwrap();
        size_after_first = fs::metadata(dir.join("wal.log")).unwrap().len();
        // A second committed batch...
        let _b = g.add_node(person(), props(&[])).unwrap();
        g.commit().unwrap();
    }
    // Simulate a torn second commit: truncate into the middle of batch 2.
    let f = OpenOptions::new()
        .write(true)
        .open(dir.join("wal.log"))
        .unwrap();
    f.set_len(size_after_first + 5).unwrap();
    drop(f);

    let g = Graph::open(&dir, config()).unwrap();
    // Never a partial blend: exactly the prior committed graph loads.
    assert_eq!(g.counts().0, 1);
    assert!(g.node(a).unwrap().is_some());
}

#[test]
fn recovery_corrupt_tail_loads_valid_prefix() {
    let dir = tmp("corrupt_tail");
    let (a, b);
    {
        let mut g = Graph::create(&dir, config()).unwrap();
        g.register_node_type(person(), None).unwrap();
        a = g.add_node(person(), props(&[])).unwrap();
        g.commit().unwrap();
        b = g.add_node(person(), props(&[])).unwrap();
        g.commit().unwrap();
    }
    // Append garbage after the last good commit.
    {
        use std::io::Write;
        let mut f = OpenOptions::new()
            .append(true)
            .open(dir.join("wal.log"))
            .unwrap();
        f.write_all(&[0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02]).unwrap();
    }
    let g = Graph::open(&dir, config()).unwrap();
    // Both committed nodes load; the garbage tail is ignored, not a partial load.
    assert_eq!(g.counts().0, 2);
    assert!(g.node(a).unwrap().is_some());
    assert!(g.node(b).unwrap().is_some());
}

#[test]
fn missing_snapshot_with_newer_wal_fails_not_silent_partial_load() {
    // A lost snapshot beside a post-snapshot (newer-epoch) WAL must fail open,
    // not silently replay onto an empty store (edges → missing nodes).
    let dir = tmp("missing_snapshot");
    let a;
    {
        let mut g = Graph::create(&dir, config()).unwrap();
        g.register_node_type(person(), None).unwrap();
        a = g.add_node(person(), props(&[])).unwrap();
        let b = g.add_node(person(), props(&[])).unwrap();
        g.add_edge(a, b, knows(), 1.0, props(&[])).unwrap();
        g.commit().unwrap();
        g.snapshot().unwrap(); // epoch 1
        let c = g.add_node(person(), props(&[])).unwrap();
        g.add_edge(a, c, knows(), 1.0, props(&[])).unwrap();
        g.commit().unwrap();
    }
    // Snapshot lost (backup missed it / filesystem damage); WAL is epoch 1.
    fs::remove_file(dir.join("snapshot.bin")).unwrap();
    match Graph::open(&dir, config()) {
        Err(Error::Storage(m)) => assert!(m.contains("snapshot"), "unexpected: {m}"),
        Err(other) => panic!("expected a storage error about the missing snapshot, got {other}"),
        Ok(_) => panic!("open silently loaded a partial graph instead of failing"),
    }
}

#[test]
fn recovery_version_mismatch_fails_explicitly() {
    let dir = tmp("version_mismatch");
    {
        let mut g = Graph::create(&dir, config()).unwrap();
        g.register_node_type(person(), None).unwrap();
        g.add_node(person(), props(&[])).unwrap();
        g.commit().unwrap();
        g.snapshot().unwrap(); // writes snapshot.bin
    }
    // Corrupt the format version (bytes 4..8 after the 4-byte magic).
    let mut bytes = fs::read(dir.join("snapshot.bin")).unwrap();
    bytes[4] = bytes[4].wrapping_add(1);
    fs::write(dir.join("snapshot.bin"), &bytes).unwrap();

    match Graph::open(&dir, config()) {
        Err(Error::VersionMismatch { .. }) => {}
        Err(other) => panic!("expected VersionMismatch, got {other}"),
        Ok(_) => panic!("expected VersionMismatch, but open succeeded"),
    }
}

#[test]
fn snapshot_compacts_wal_and_preserves_state() {
    let dir = tmp("snapshot_compact");
    let a;
    {
        let mut g = Graph::create(&dir, config()).unwrap();
        g.register_node_type(person(), None).unwrap();
        a = g.add_node(person(), props(&[])).unwrap();
        g.add_node(person(), props(&[])).unwrap();
        g.commit().unwrap();
        g.snapshot().unwrap();
        // WAL is reset after snapshot to just its versioned header — 20 bytes:
        // magic(4) + version(4) + epoch(8) + header-crc(4) (no committed frames).
        assert_eq!(fs::metadata(dir.join("wal.log")).unwrap().len(), 20);
        // Further mutation after snapshot still persists.
        g.remove_node(a, RemoveNodeMode::RejectIfEdgesExist)
            .unwrap();
        g.commit().unwrap();
    }
    let g = Graph::open(&dir, config()).unwrap();
    assert_eq!(g.counts().0, 1); // two added, one removed
    assert!(g.node(a).unwrap().is_none());
}

#[test]
fn recovery_snapshot_crash_before_wal_truncation_skips_stale_wal() {
    // A crash between the snapshot rename and the WAL reset leaves a fresh
    // snapshot beside an un-truncated (old-epoch) WAL. Replaying that WAL would
    // double-apply the non-idempotent decay. The epoch guard must skip it.
    let dir = tmp("snapshot_crash_window");
    let a;
    let b;
    let e;
    let stale_wal;
    {
        let mut g = Graph::create(&dir, config()).unwrap();
        g.register_node_type(person(), None).unwrap();
        a = g.add_node(person(), props(&[])).unwrap();
        b = g.add_node(person(), props(&[])).unwrap();
        e = g.add_edge(a, b, knows(), 1.0, props(&[])).unwrap();
        // Decay to 0.5 and commit — this lands in the (epoch-0) WAL.
        g.decay_edges(EdgeFilter::new(), 0.5).unwrap();
        g.commit().unwrap();
        // Capture the pre-snapshot WAL (old epoch, contains the decay).
        stale_wal = fs::read(dir.join("wal.log")).unwrap();
        // Snapshot bakes weight 0.5 in and bumps to epoch 1, resetting the WAL.
        g.snapshot().unwrap();
    }
    // Simulate the crash: restore the old-epoch WAL as if truncation never ran.
    fs::write(dir.join("wal.log"), &stale_wal).unwrap();

    let c;
    {
        let mut g = Graph::open(&dir, config()).unwrap();
        // The stale WAL is recognized as older than the snapshot and skipped, so
        // the decay applies exactly once — weight is 0.5, not 0.25.
        assert_eq!(g.edge(e).unwrap().unwrap().weight, 0.5);
        assert!(g.node(b).unwrap().is_some());
        // A post-recovery write must survive another reopen: this only holds if
        // open repaired the stale WAL (rewrote a header at the snapshot epoch)
        // rather than appending behind the stale bytes.
        c = g.add_node(person(), props(&[])).unwrap();
        g.commit().unwrap();
    }
    let g = Graph::open(&dir, config()).unwrap();
    assert!(
        g.node(c).unwrap().is_some(),
        "post-recovery commit was lost"
    );
    assert_eq!(g.edge(e).unwrap().unwrap().weight, 0.5);
}

#[test]
fn export_import_restores_exact_id_space() {
    let dir = tmp("export_import");
    let export_path = dir.join("graph.drey");
    fs::create_dir_all(&dir).unwrap();
    let (a, b, e);
    {
        let mut g = Graph::create(dir.join("live"), config()).unwrap();
        g.register_node_type(person(), Some(2)).unwrap();
        a = g.add_node(person(), props(&[])).unwrap();
        // Force a gap in the id space by removing an intermediate node.
        let gap = g.add_node(person(), props(&[])).unwrap();
        g.remove_node(gap, RemoveNodeMode::RejectIfEdgesExist)
            .unwrap();
        b = g.add_node(person(), props(&[])).unwrap();
        e = g.add_edge(a, b, knows(), 1.0, props(&[])).unwrap();
        g.commit().unwrap();
        g.export(&export_path).unwrap();
    }
    let g = Graph::import(&export_path, config()).unwrap();
    // Exact id restoration, including the gap: a, b keep their ids; edge intact.
    assert!(g.node(a).unwrap().is_some());
    assert!(g.node(b).unwrap().is_some());
    assert_eq!(g.edge(e).unwrap().unwrap().from, a);
    // A newly added node must not collide with the pre-gap ids.
    let mut g = g;
    // import produces an in-memory graph; make it writable via a fresh id check
    let next = g.add_node(person(), props(&[])).unwrap();
    assert_ne!(next, a);
    assert_ne!(next, b);
}

#[test]
fn read_only_open_sees_committed_and_rejects_writes() {
    let dir = tmp("read_only");
    let a;
    {
        let mut g = Graph::create(&dir, config()).unwrap();
        g.register_node_type(person(), None).unwrap();
        a = g.add_node(person(), props(&[])).unwrap();
        g.commit().unwrap();
    }
    let mut g = Graph::open(&dir, config().read_only()).unwrap();
    assert!(g.node(a).unwrap().is_some());
    assert!(g.add_node(person(), props(&[])).is_err());
    let _ = NodeId(0); // silence unused import in some configs
}

/// All nodes materialized and sorted by id, for whole-graph equivalence checks.
fn collect_nodes(g: &Graph) -> Vec<Node> {
    let mut ids = g.node_ids();
    ids.sort_by_key(|n| n.0);
    ids.into_iter()
        .map(|id| g.node(id).unwrap().unwrap())
        .collect()
}

#[test]
fn wal_crc_byteflip_inside_frame_loads_only_prior_prefix() {
    // A bit-flip inside a committed frame's payload (length prefix intact) must
    // reach the CRC check and be rejected — replay stops at the bad frame and
    // loads only the prior committed prefix, never a blended/corrupt graph
    // (PRD §10.2.1). Exercises the crc32 branch the framing-break tests miss.
    let dir = tmp("wal_crc_flip");
    let a;
    let size_after_first;
    {
        let mut g = Graph::create(&dir, config()).unwrap();
        g.register_node_type(person(), None).unwrap();
        a = g.add_node(person(), props(&[])).unwrap();
        g.commit().unwrap();
        size_after_first = fs::metadata(dir.join("wal.log")).unwrap().len();
        let _b = g.add_node(person(), props(&[])).unwrap();
        g.commit().unwrap();
    }
    // Batch 2's first frame starts at `size_after_first`: len(4) + crc(4) +
    // payload. Flip the last payload byte — inside the payload, length intact —
    // so the corruption is caught by the CRC, not a framing mismatch.
    let path = dir.join("wal.log");
    let mut bytes = fs::read(&path).unwrap();
    let off = size_after_first as usize;
    let len = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap()) as usize;
    let flip = off + 8 + len - 1;
    bytes[flip] ^= 0xFF;
    fs::write(&path, &bytes).unwrap();

    let g = Graph::open(&dir, config()).unwrap();
    assert_eq!(g.counts().0, 1, "corrupt batch 2 must be discarded");
    assert!(g.node(a).unwrap().is_some());
}

#[test]
fn corrupt_snapshot_truncation_fails_open() {
    // PRD §10.2.1 row 3: a corrupt/truncated snapshot loads the last valid state
    // or fails explicitly — never a silent partial load.
    let dir = tmp("snap_truncate");
    {
        let mut g = Graph::create(&dir, config()).unwrap();
        g.register_node_type(person(), None).unwrap();
        g.add_node(person(), props(&[])).unwrap();
        g.commit().unwrap();
        g.snapshot().unwrap();
    }
    let path = dir.join("snapshot.bin");
    let full = fs::metadata(&path).unwrap().len();
    OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(full / 2)
        .unwrap();
    match Graph::open(&dir, config()) {
        Err(Error::Codec(_)) => {}
        Err(other) => panic!("expected Codec on truncated snapshot, got {other}"),
        Ok(_) => panic!("open succeeded on a truncated snapshot"),
    }
}

#[test]
fn corrupt_snapshot_payload_byte_fails_open() {
    // A single bit-flip in the snapshot payload (no per-frame framing here) must
    // be caught by the trailing snapshot checksum, not silently loaded as an
    // altered weight/property (PRD §10.2.1: never a silent partial blend).
    let dir = tmp("snap_payload");
    {
        let mut g = Graph::create(&dir, config()).unwrap();
        g.register_node_type(person(), None).unwrap();
        let a = g
            .add_node(person(), props(&[("age", Value::I64(7))]))
            .unwrap();
        let b = g.add_node(person(), props(&[])).unwrap();
        g.add_edge(a, b, knows(), 0.5, props(&[])).unwrap();
        g.commit().unwrap();
        g.snapshot().unwrap();
    }
    let path = dir.join("snapshot.bin");
    let mut bytes = fs::read(&path).unwrap();
    let mid = bytes.len() / 2; // deep in the payload, past the header, before the CRC
    bytes[mid] ^= 0xFF;
    fs::write(&path, &bytes).unwrap();
    match Graph::open(&dir, config()) {
        Err(Error::Codec(m)) => assert!(m.contains("checksum"), "unexpected message: {m}"),
        Err(other) => panic!("expected a snapshot checksum error, got {other}"),
        Ok(_) => panic!("open succeeded on a corrupt snapshot payload"),
    }
}

#[test]
fn all_five_previously_untested_wal_tags_replay() {
    // UpdateNodeProperties(3), SetEdgeWeight(6), UpdateEdgeProperties(7),
    // RemoveEdge(8), and DecayEdges(9) were never encoded-then-decoded by any
    // test; a write/read field mismatch on the hand-rolled codec would ship
    // undetected. Exercise each through commit -> reopen -> read.
    let dir = tmp("five_tags");
    let a;
    let e;
    {
        let mut g = Graph::create(&dir, config()).unwrap();
        g.register_node_type(person(), None).unwrap();
        a = g
            .add_node(person(), props(&[("age", Value::I64(1))]))
            .unwrap();
        let b = g.add_node(person(), props(&[])).unwrap();
        e = g
            .add_edge(
                a,
                b,
                knows(),
                1.0,
                props(&[("label", Value::String("x".into()))]),
            )
            .unwrap();
        g.commit().unwrap();
        // tag 3
        g.update_node_properties(a, PropertyPatch::new().set("age", Value::I64(99)))
            .unwrap();
        // tag 6 (update_edge_weight logs the resolved SetEdgeWeight)
        g.update_edge_weight(e, WeightUpdate::set(0.8)).unwrap();
        // tag 7
        g.update_edge_properties(
            e,
            PropertyPatch::new().set("label", Value::String("y".into())),
        )
        .unwrap();
        // tag 8 (add then remove a throwaway edge)
        let e2 = g.add_edge(a, b, knows(), 0.1, props(&[])).unwrap();
        g.remove_edge(e2).unwrap();
        // tag 9 (0.8 -> 0.4; e2 already gone, so only e decays)
        g.decay_edges(EdgeFilter::new(), 0.5).unwrap();
        g.commit().unwrap();
    }
    let g = Graph::open(&dir, config()).unwrap();
    let na = g.node(a).unwrap().unwrap();
    assert_eq!(na.properties.get("age"), Some(&Value::I64(99)), "tag 3");
    let ee = g.edge(e).unwrap().unwrap();
    assert_eq!(ee.weight, 0.4, "tags 6 + 9: set 0.8 then decay 0.5");
    assert_eq!(
        ee.properties.get("label"),
        Some(&Value::String("y".into())),
        "tag 7"
    );
    assert_eq!(g.counts().1, 1, "tag 8: removed edge stays gone");
}

#[test]
fn full_state_equivalent_after_reopen() {
    // Not a spot-check: the entire node set (ids, types, properties, embeddings)
    // and the surviving edge must be byte-identical after a close/reopen cycle
    // that exercised update/decay/remove paths.
    let dir = tmp("state_equiv");
    let live_nodes;
    let live_edge;
    let e1;
    {
        let mut g = Graph::create(&dir, config()).unwrap();
        g.register_node_type(person(), Some(4)).unwrap();
        let a = g
            .add_node(
                person(),
                props(&[("age", Value::I64(30)), ("t", Value::Bytes(vec![1, 2, 3]))]),
            )
            .unwrap();
        let b = g
            .add_node(person(), props(&[("age", Value::I64(40))]))
            .unwrap();
        let c = g.add_node(person(), props(&[])).unwrap();
        g.set_node_embedding(
            a,
            Embedding::new(vec![f32::from_bits(1), -0.0, 0.1, f32::MIN_POSITIVE]),
        )
        .unwrap();
        e1 = g
            .add_edge(
                a,
                b,
                knows(),
                0.75,
                props(&[("k", Value::String("v".into()))]),
            )
            .unwrap();
        let _e2 = g.add_edge(b, c, knows(), 0.5, props(&[])).unwrap();
        // Exercise several mutation kinds before the snapshot of live state.
        g.update_node_properties(b, PropertyPatch::new().set("age", Value::I64(41)))
            .unwrap();
        g.update_edge_weight(e1, WeightUpdate::multiply(0.5))
            .unwrap(); // 0.375
        g.remove_node(c, RemoveNodeMode::RemoveIncidentEdges)
            .unwrap(); // drops e2
        g.commit().unwrap();
        live_nodes = collect_nodes(&g);
        live_edge = g.edge(e1).unwrap();
    }
    let g = Graph::open(&dir, config()).unwrap();
    assert_eq!(
        collect_nodes(&g),
        live_nodes,
        "node set diverged after reopen"
    );
    assert_eq!(g.edge(e1).unwrap(), live_edge, "edge diverged after reopen");
    assert_eq!(g.counts(), (2, 1));
}

#[test]
fn id_allocator_resumes_after_reopen_without_collision() {
    // Durable ids (design commitment 5): the allocator must resume past every id
    // ever assigned, so a post-reopen insert never collides with a live or a
    // removed id.
    let dir = tmp("id_alloc");
    let a;
    let b;
    {
        let mut g = Graph::create(&dir, config()).unwrap();
        g.register_node_type(person(), None).unwrap();
        a = g.add_node(person(), props(&[])).unwrap();
        b = g.add_node(person(), props(&[])).unwrap();
        g.remove_node(b, RemoveNodeMode::RejectIfEdgesExist)
            .unwrap();
        g.commit().unwrap();
    }
    let mut g = Graph::open(&dir, config()).unwrap();
    let n = g.add_node(person(), props(&[])).unwrap();
    assert_ne!(n, a);
    assert_ne!(n, b, "a removed id must not be reused");
    assert!(
        n.0 > a.0 && n.0 > b.0,
        "allocator must resume past all prior ids"
    );
}

#[test]
fn non_finite_edge_weights_rejected_finite_denormal_round_trips() {
    // Non-finite weights are malformed input and are rejected at the boundary
    // (they would become zero-cost edges in weighted shortest_path). A finite
    // denormal is legal and must round-trip byte-exact (the codec f32 contract,
    // PRD §10.2).
    let dir = tmp("nan_weight");
    let e_den;
    {
        let mut g = Graph::create(&dir, config()).unwrap();
        g.register_node_type(person(), None).unwrap();
        let a = g.add_node(person(), props(&[])).unwrap();
        let b = g.add_node(person(), props(&[])).unwrap();
        assert!(
            g.add_edge(a, b, knows(), f32::NAN, props(&[])).is_err(),
            "NaN weight must be rejected"
        );
        assert!(
            g.add_edge(a, b, knows(), f32::INFINITY, props(&[]))
                .is_err(),
            "infinite weight must be rejected"
        );
        // Smallest positive denormal — finite, so accepted and byte-exact.
        e_den = g
            .add_edge(a, b, knows(), f32::from_bits(1), props(&[]))
            .unwrap();
        g.commit().unwrap();
    }
    let g = Graph::open(&dir, config()).unwrap();
    assert_eq!(g.edge(e_den).unwrap().unwrap().weight.to_bits(), 1);
}

fn locking_config() -> GraphConfig {
    GraphConfig {
        file_lock: true,
        ..config()
    }
}

#[test]
fn second_writer_conflicts_while_lock_held_and_succeeds_after_drop() {
    // Single-writer enforcement via OS advisory locking: a second writable open
    // fails LockConflict while the first holds the graph, and succeeds once the
    // first handle drops (kernel releases the lock with the handle).
    let dir = tmp("flock_conflict");
    let g1 = Graph::create(&dir, locking_config()).unwrap();
    match Graph::open(&dir, locking_config()) {
        Err(Error::LockConflict(_)) => {}
        Err(other) => panic!("expected LockConflict, got {other}"),
        Ok(_) => panic!("second writer acquired the lock while the first was live"),
    }
    drop(g1);
    assert!(Graph::open(&dir, locking_config()).is_ok());
}

#[test]
fn leftover_lock_file_after_hard_crash_does_not_wedge() {
    // A hard crash (SIGKILL/OOM/power loss) never runs Drop, so a LOCK file
    // survives on disk — but the kernel released the advisory lock with the
    // dead process's fd, so locking the leftover file just succeeds. This is
    // the recovery-matrix row-1 guarantee the old PID-file scheme violated
    // (audit #5 / issue #8: a stale LOCK wedged the graph until manual repair).
    let dir = tmp("flock_stale");
    {
        let mut g = Graph::create(&dir, locking_config()).unwrap();
        g.register_node_type(person(), None).unwrap();
        g.add_node(person(), props(&[])).unwrap();
        g.commit().unwrap();
    }
    // Simulate the crash leftover: the LOCK file exists (with whatever stale
    // content — the old scheme stamped "pid boot_id") but no process holds it.
    fs::write(dir.join("LOCK"), b"999999 dead-boot-id").unwrap();
    let g = Graph::open(&dir, locking_config()).unwrap();
    assert_eq!(
        g.counts().0,
        1,
        "graph must open normally past a stale LOCK"
    );
}

#[test]
fn read_only_open_ignores_the_writer_lock() {
    // Read-only opens attach no writer and take no lock (PRD §9.1), so they
    // must succeed even while a writer holds the graph.
    let dir = tmp("flock_read_only");
    let mut g1 = Graph::create(&dir, locking_config()).unwrap();
    g1.register_node_type(person(), None).unwrap();
    g1.add_node(person(), props(&[])).unwrap();
    g1.commit().unwrap();
    let ro = Graph::open(&dir, locking_config().read_only()).unwrap();
    assert_eq!(ro.counts().0, 1);
}

#[test]
fn recovery_mutation_frames_without_commit_marker_are_discarded() {
    // The staged-discard path: a commit torn AFTER its mutation frame but BEFORE
    // the commit marker. The frame is intact (CRC-valid) but unmarked, so its
    // staged mutations must be discarded, not applied — distinct from the
    // torn-frame and CRC-mismatch paths.
    let dir = tmp("staged_discard");
    let a;
    let size_after_first;
    {
        let mut g = Graph::create(&dir, config()).unwrap();
        g.register_node_type(person(), None).unwrap();
        a = g.add_node(person(), props(&[])).unwrap();
        g.commit().unwrap();
        size_after_first = fs::metadata(dir.join("wal.log")).unwrap().len();
        let _b = g.add_node(person(), props(&[])).unwrap();
        g.commit().unwrap();
    }
    // Drop batch 2's trailing commit-marker frame: len(4) + crc(4) + a 1-byte
    // TAG_COMMIT payload = 9 bytes. That leaves batch 2's AddNode frame intact
    // but with no marker after it.
    let size_after_second = fs::metadata(dir.join("wal.log")).unwrap().len();
    assert!(
        size_after_second > size_after_first + 9,
        "batch 2 should be a mutation frame plus the 9-byte marker"
    );
    OpenOptions::new()
        .write(true)
        .open(dir.join("wal.log"))
        .unwrap()
        .set_len(size_after_second - 9)
        .unwrap();

    let g = Graph::open(&dir, config()).unwrap();
    assert_eq!(
        g.counts().0,
        1,
        "an unmarked (uncommitted) staged mutation must be discarded"
    );
    assert!(g.node(a).unwrap().is_some());
}

// ---- Regression coverage added by the repo ultrareview ----

#[test]
fn wal_header_epoch_corruption_is_detected_not_silently_stale() {
    // The WAL header epoch decides replay-vs-discard. A bit-flip lowering it
    // below the snapshot epoch would, without a header CRC, silently discard the
    // post-snapshot WAL and lose acknowledged commits. The v3 header CRC turns
    // that into a load-time error instead.
    let dir = tmp("wal_header_crc");
    {
        let mut g = Graph::create(&dir, config()).unwrap();
        g.register_node_type(person(), None).unwrap();
        g.add_node(person(), props(&[])).unwrap();
        g.commit().unwrap();
        g.snapshot().unwrap(); // epoch -> 1, WAL re-headered at epoch 1
        g.add_node(person(), props(&[])).unwrap(); // post-snapshot commit in epoch-1 WAL
        g.commit().unwrap();
    }
    let mut bytes = fs::read(dir.join("wal.log")).unwrap();
    bytes[8] ^= 0x01; // epoch 1 -> 0 (would read as stale without the CRC)
    fs::write(dir.join("wal.log"), &bytes).unwrap();

    match Graph::open(&dir, config()) {
        Err(Error::Codec(_)) => {}
        Err(other) => panic!("expected a Codec error for the corrupt WAL header, got {other}"),
        Ok(_) => panic!("open silently accepted a corrupt WAL header"),
    }
}

#[test]
fn wal_header_version_mismatch_fails_explicitly() {
    let dir = tmp("wal_version_mismatch");
    {
        let mut g = Graph::create(&dir, config()).unwrap();
        g.register_node_type(person(), None).unwrap();
        g.add_node(person(), props(&[])).unwrap();
        g.commit().unwrap();
    }
    // Corrupt the WAL format version (bytes 4..8, after the 4-byte magic). The
    // version is checked before the header CRC, so this is a clean VersionMismatch.
    let mut bytes = fs::read(dir.join("wal.log")).unwrap();
    bytes[4] = bytes[4].wrapping_add(1);
    fs::write(dir.join("wal.log"), &bytes).unwrap();

    match Graph::open(&dir, config()) {
        Err(Error::VersionMismatch { .. }) => {}
        Err(other) => panic!("expected VersionMismatch for the WAL header, got {other}"),
        Ok(_) => panic!("open silently accepted a WAL header with a wrong version"),
    }
}

#[test]
fn legacy_16_byte_v2_wal_fails_with_version_mismatch_not_silent_stale() {
    // A v2 graph created and never committed to leaves a header-only 16-byte
    // WAL — shorter than the 20-byte v3 header. Version must be checked before
    // the full-header length guard, or the file is classified as a torn stale
    // header and silently rewritten as an empty v3 graph instead of erroring.
    let dir = tmp("legacy_v2_wal");
    fs::create_dir_all(&dir).unwrap();
    let mut header = Vec::new();
    header.extend_from_slice(b"DREY");
    header.extend_from_slice(&2u32.to_le_bytes()); // v2: no header CRC
    header.extend_from_slice(&0u64.to_le_bytes()); // epoch 0
    fs::write(dir.join("wal.log"), &header).unwrap();

    match Graph::open(&dir, config()) {
        Err(Error::VersionMismatch { found: 2, .. }) => {}
        Err(other) => panic!("expected VersionMismatch for the v2 WAL, got {other}"),
        Ok(_) => panic!("open silently accepted (and would rewrite) a legacy v2 WAL"),
    }
}

#[test]
fn torn_header_write_recovers_as_empty_stale_wal() {
    // A crash mid-header-write leaves a prefix of a valid v3 header. Both torn
    // shapes — under 8 bytes (no version to check) and 8..20 bytes (valid
    // magic+version, missing epoch/CRC) — must recover as a stale empty WAL,
    // not error: there is no committed content to lose.
    for &torn_at in &[5usize, 12] {
        let dir = tmp(&format!("torn_header_{torn_at}"));
        {
            let mut g = Graph::create(&dir, config()).unwrap();
            g.register_node_type(person(), None).unwrap();
        }
        let bytes = fs::read(dir.join("wal.log")).unwrap();
        fs::write(dir.join("wal.log"), &bytes[..torn_at]).unwrap();

        let g = Graph::open(&dir, config())
            .unwrap_or_else(|e| panic!("torn {torn_at}-byte header must recover, got {e}"));
        assert_eq!(g.counts(), (0, 0), "torn at {torn_at}");
    }
}

#[test]
fn empty_committed_graph_roundtrips() {
    let dir = tmp("empty_graph");
    {
        let mut g = Graph::create(&dir, config()).unwrap();
        g.register_node_type(person(), Some(2)).unwrap();
        g.commit().unwrap(); // a graph with a registered type but zero nodes/edges
    }
    let g = Graph::open(&dir, config()).unwrap();
    assert_eq!(g.counts(), (0, 0));
    // The registration itself must survive replay: nodes_by_type errors on an
    // unregistered type, so this would catch a silently no-op'd
    // RegisterNodeType frame that the counts assertion alone cannot see.
    assert_eq!(g.nodes_by_type(&person()).unwrap(), Vec::<NodeId>::new());
    // Degenerate reads on an empty graph must not panic.
    let sim = g
        .similar_nodes(drey::similarity::SimilarityQuery::new(
            Embedding::new(vec![1.0, 2.0]),
            drey::similarity::SimilarityMetric::Cosine,
            5,
        ))
        .unwrap();
    assert!(sim.is_empty());
}

// ---- Regression coverage: issue #22 + 2026-07 repo review ----

#[test]
fn mid_wal_corruption_in_acknowledged_batch_fails_open_without_truncation() {
    // Issue #22 item 7: three committed batches; a bit flips inside batch 2's
    // payload. Batch 3's bytes prove batch 2's fsync was acknowledged, so this
    // is corruption of durable history — open must refuse with a typed error
    // and must NOT truncate the later committed bytes away (the old behavior
    // silently loaded batch 1 and destroyed batches 2–3 on the next open).
    let dir = tmp("mid_wal_corruption");
    let size_after_first;
    {
        let mut g = Graph::create(&dir, config()).unwrap();
        g.register_node_type(person(), None).unwrap();
        g.add_node(person(), props(&[])).unwrap();
        g.commit().unwrap(); // batch 1
        size_after_first = fs::metadata(dir.join("wal.log")).unwrap().len();
        g.add_node(person(), props(&[])).unwrap();
        g.commit().unwrap(); // batch 2
        g.add_node(person(), props(&[])).unwrap();
        g.commit().unwrap(); // batch 3
    }
    // Flip the last payload byte of batch 2's first frame (length prefix intact).
    let path = dir.join("wal.log");
    let mut bytes = fs::read(&path).unwrap();
    let off = size_after_first as usize;
    let len = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap()) as usize;
    bytes[off + 8 + len - 1] ^= 0xFF;
    fs::write(&path, &bytes).unwrap();

    match Graph::open(&dir, config()) {
        Err(Error::Codec(m)) => assert!(m.contains("acknowledged"), "unexpected message: {m}"),
        Err(other) => panic!("expected a Codec corruption error, got {other}"),
        Ok(_) => panic!("open silently truncated acknowledged commits"),
    }
    // The failed open must not have repaired/truncated anything.
    assert_eq!(
        fs::read(&path).unwrap(),
        bytes,
        "failed open must leave the WAL untouched"
    );
}

#[test]
fn corruption_in_final_batch_still_recovers_as_torn_commit() {
    // The counterpart cell: damage confined to the FINAL batch with nothing
    // after it is byte-indistinguishable from a torn, unacknowledged commit
    // (the kernel may persist a batch's pages in any order before fsync
    // returns), so recovery discards that batch instead of refusing — the
    // long-standing crash-recovery contract.
    let dir = tmp("final_batch_corruption");
    let a;
    let size_after_first;
    {
        let mut g = Graph::create(&dir, config()).unwrap();
        g.register_node_type(person(), None).unwrap();
        a = g.add_node(person(), props(&[])).unwrap();
        g.commit().unwrap();
        size_after_first = fs::metadata(dir.join("wal.log")).unwrap().len();
        g.add_node(person(), props(&[])).unwrap();
        g.commit().unwrap();
    }
    let path = dir.join("wal.log");
    let mut bytes = fs::read(&path).unwrap();
    let off = size_after_first as usize;
    let len = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap()) as usize;
    bytes[off + 8 + len - 1] ^= 0xFF;
    fs::write(&path, &bytes).unwrap();

    let g = Graph::open(&dir, config()).unwrap();
    assert_eq!(g.counts().0, 1);
    assert!(g.node(a).unwrap().is_some());
}

#[test]
fn restored_older_snapshot_beside_newer_wal_fails_open() {
    // Issue #22 item 8: an operator restores snapshot.bin from a backup (epoch
    // 1) next to the current epoch-2 WAL. Replaying epoch-2 frames onto the
    // epoch-1 image would silently blend generations; open must refuse with a
    // typed generation mismatch and mutate neither file.
    let dir = tmp("older_snapshot_restore");
    let epoch1_snapshot;
    {
        let mut g = Graph::create(&dir, config()).unwrap();
        g.register_node_type(person(), None).unwrap();
        g.add_node(person(), props(&[])).unwrap();
        g.commit().unwrap();
        g.snapshot().unwrap(); // epoch 1
        epoch1_snapshot = fs::read(dir.join("snapshot.bin")).unwrap();
        g.add_node(person(), props(&[])).unwrap();
        g.commit().unwrap();
        g.snapshot().unwrap(); // epoch 2; WAL re-headered at epoch 2
    }
    fs::write(dir.join("snapshot.bin"), &epoch1_snapshot).unwrap();
    let wal_before = fs::read(dir.join("wal.log")).unwrap();

    match Graph::open(&dir, config()) {
        Err(Error::GenerationMismatch {
            wal_epoch,
            snapshot_epoch,
        }) => {
            assert_eq!((wal_epoch, snapshot_epoch), (2, 1));
        }
        Err(other) => panic!("expected GenerationMismatch, got {other}"),
        Ok(_) => panic!("open silently blended two snapshot generations"),
    }
    assert_eq!(
        fs::read(dir.join("wal.log")).unwrap(),
        wal_before,
        "failed open must not repair the WAL"
    );
    assert_eq!(
        fs::read(dir.join("snapshot.bin")).unwrap(),
        epoch1_snapshot,
        "failed open must not touch the snapshot"
    );
}

#[test]
fn read_only_open_with_newer_wal_generation_fails_after_retries() {
    // The same mismatch through the lock-free read-only path: the bounded
    // rotation-retry re-reads a few times (a racing snapshot() resolves in one
    // pass) and then surfaces the mismatch instead of serving blended state.
    let dir = tmp("ro_generation_mismatch");
    let epoch1_snapshot;
    {
        let mut g = Graph::create(&dir, config()).unwrap();
        g.register_node_type(person(), None).unwrap();
        g.add_node(person(), props(&[])).unwrap();
        g.commit().unwrap();
        g.snapshot().unwrap(); // epoch 1
        epoch1_snapshot = fs::read(dir.join("snapshot.bin")).unwrap();
        g.snapshot().unwrap(); // epoch 2
    }
    fs::write(dir.join("snapshot.bin"), &epoch1_snapshot).unwrap();
    match Graph::open(&dir, config().read_only()) {
        Err(Error::GenerationMismatch { .. }) => {}
        Err(other) => panic!("expected GenerationMismatch, got {other}"),
        Ok(_) => panic!("read-only open served state from mismatched generations"),
    }
}

#[test]
fn missing_wal_beside_snapshot_fails_open() {
    // The mirror of missing_snapshot_with_newer_wal: wal.log exists in every
    // legitimate state once snapshot.bin does (create writes it first;
    // snapshot() truncates in place, never unlinks), so its absence proves
    // file loss. Opening at snapshot state would silently drop every
    // post-snapshot commit.
    let dir = tmp("missing_wal");
    {
        let mut g = Graph::create(&dir, config()).unwrap();
        g.register_node_type(person(), None).unwrap();
        g.add_node(person(), props(&[])).unwrap();
        g.commit().unwrap();
        g.snapshot().unwrap();
        g.add_node(person(), props(&[])).unwrap();
        g.commit().unwrap(); // post-snapshot commit that lives only in the WAL
    }
    fs::remove_file(dir.join("wal.log")).unwrap();
    match Graph::open(&dir, config()) {
        Err(Error::Storage(m)) => assert!(m.contains("wal.log is missing"), "unexpected: {m}"),
        Err(other) => panic!("expected a storage error about the missing WAL, got {other}"),
        Ok(_) => panic!("open silently dropped post-snapshot commits"),
    }
}

#[test]
fn torn_wal_reheader_after_snapshot_recovers_at_snapshot_state() {
    // The legitimate crash window the missing-WAL refusal must NOT catch: a
    // crash mid-way through snapshot()'s WAL re-header leaves wal.log torn
    // below the 20-byte header. Everything is already folded into the new
    // snapshot, so opening at snapshot state is exact recovery, not loss.
    let dir = tmp("torn_reheader");
    let a;
    {
        let mut g = Graph::create(&dir, config()).unwrap();
        g.register_node_type(person(), None).unwrap();
        a = g.add_node(person(), props(&[])).unwrap();
        g.commit().unwrap();
        g.snapshot().unwrap();
    }
    let wal = dir.join("wal.log");
    let bytes = fs::read(&wal).unwrap();
    fs::write(&wal, &bytes[..7]).unwrap(); // torn mid-header

    let c;
    {
        let mut g = Graph::open(&dir, config()).unwrap();
        assert_eq!(g.counts().0, 1);
        assert!(g.node(a).unwrap().is_some());
        c = g.add_node(person(), props(&[])).unwrap();
        g.commit().unwrap();
    }
    let g = Graph::open(&dir, config()).unwrap();
    assert!(g.node(c).unwrap().is_some(), "post-recovery commit lost");
}

#[test]
fn uncommitted_ids_are_reallocated_after_drop_without_commit() {
    // Contract pin (documented in docs/specs/m2-durability.md): ids handed out
    // for mutations that were never committed do NOT survive an uncommitted
    // close — the allocator resumes from committed state, so the same id can
    // be assigned to a different entity. Design commitment 5 ("ids survive
    // close/reopen exactly") applies to COMMITTED entities; consumers must
    // treat ids of uncommitted entities as invalidated by drop.
    let dir = tmp("uncommitted_id_reuse");
    let a;
    let b_uncommitted;
    {
        let mut g = Graph::create(&dir, config()).unwrap();
        g.register_node_type(person(), None).unwrap();
        a = g.add_node(person(), props(&[])).unwrap();
        g.commit().unwrap();
        b_uncommitted = g.add_node(person(), props(&[])).unwrap();
        // Drop without commit: b's mutation is discarded (see
        // recovery_crash_before_commit_loses_uncommitted).
    }
    let mut g = Graph::open(&dir, config()).unwrap();
    let c = g.add_node(person(), props(&[])).unwrap();
    assert_ne!(c, a, "committed ids are never reused");
    assert_eq!(
        c, b_uncommitted,
        "the uncommitted id returns to the allocator — the documented contract"
    );
}

#[test]
fn export_refuses_uncommitted_mutations_and_allows_after_commit() {
    // export is the backup verb: an image must never disagree with what a
    // reopen would load, so a dirty (appended-but-uncommitted) graph refuses.
    let dir = tmp("export_dirty");
    let mut g = Graph::create(dir.join("live"), config()).unwrap();
    g.register_node_type(person(), None).unwrap();
    g.add_node(person(), props(&[])).unwrap();
    let dest = dir.join("backup.drey");
    match g.export(&dest) {
        Err(Error::Storage(m)) => assert!(m.contains("uncommitted"), "unexpected: {m}"),
        Err(other) => panic!("expected a storage error about uncommitted state, got {other}"),
        Ok(_) => panic!("export published uncommitted mutations"),
    }
    g.commit().unwrap();
    g.export(&dest).unwrap();
    let imported = Graph::import(&dest, config()).unwrap();
    assert_eq!(imported.counts().0, 1);
}

#[test]
fn export_refuses_destinations_aliasing_live_graph_files() {
    // Issue #22 item 4: an export whose atomic rename lands on wal.log,
    // snapshot.bin, or LOCK would replace a file the live writer holds open —
    // failed recovery or lost commits. Direct paths, relative aliases, and
    // (on Unix) hard links must all be refused; unrelated destinations still work.
    let dir = tmp("export_alias");
    let mut g = Graph::create(&dir, config()).unwrap();
    g.register_node_type(person(), None).unwrap();
    g.add_node(person(), props(&[])).unwrap();
    g.commit().unwrap();
    g.snapshot().unwrap(); // snapshot.bin now exists too

    for reserved in ["wal.log", "snapshot.bin", "LOCK"] {
        assert!(
            g.export(dir.join(reserved)).is_err(),
            "direct alias {reserved} must be refused"
        );
        // Relative alias through a subdirectory and `..`.
        let dodge = dir.join("sub").join("..").join(reserved);
        fs::create_dir_all(dir.join("sub")).unwrap();
        assert!(
            g.export(&dodge).is_err(),
            "relative alias to {reserved} must be refused"
        );
    }
    #[cfg(unix)]
    {
        let link = dir.join("innocent-name.bin");
        fs::hard_link(dir.join("wal.log"), &link).unwrap();
        assert!(
            g.export(&link).is_err(),
            "hard-link alias to wal.log must be refused"
        );
    }
    // An unrelated destination — even inside the graph directory — still works.
    g.export(dir.join("backup.drey")).unwrap();
    assert!(Graph::import(dir.join("backup.drey"), config()).is_ok());
}

#[test]
fn read_only_export_carries_the_true_epoch_not_zero() {
    // A read-only open attaches no persister; export must still stamp the
    // epoch the graph was loaded at. A fabricated epoch-0 image dropped in as
    // a snapshot replacement beside a surviving epoch-N WAL would replay
    // non-idempotent mutations the image already contains.
    let dir = tmp("ro_export_epoch");
    {
        let mut g = Graph::create(&dir, config()).unwrap();
        g.register_node_type(person(), None).unwrap();
        g.add_node(person(), props(&[])).unwrap();
        g.commit().unwrap();
        g.snapshot().unwrap(); // epoch 1
    }
    let ro = Graph::open(&dir, config().read_only()).unwrap();
    let dest = dir.join("ro-backup.drey");
    ro.export(&dest).unwrap();
    // The image epoch lives at bytes 8..16 (after magic + version), little-endian.
    let bytes = fs::read(&dest).unwrap();
    let epoch = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    assert_eq!(epoch, 1, "read-only export must carry the loaded epoch");
}

#[test]
fn reopen_under_stricter_max_embedding_dim_fails_not_silently_retains() {
    // Replay and snapshot load apply the same registration gates as the live
    // API: a graph holding a dim-8 type refuses to open under a config that
    // caps dims at 4, instead of silently retaining the over-limit type.
    let strict = || GraphConfig {
        max_embedding_dim: Some(4),
        ..config()
    };
    // Via WAL replay (no snapshot).
    let dir = tmp("strict_dim_wal");
    {
        let mut g = Graph::create(&dir, config()).unwrap();
        g.register_node_type(person(), Some(8)).unwrap();
        g.commit().unwrap();
    }
    match Graph::open(&dir, strict()) {
        Err(Error::InvalidNodeType(m)) => assert!(m.contains("exceeds"), "{m}"),
        Err(other) => panic!("expected InvalidNodeType, got {other}"),
        Ok(_) => panic!("over-limit type silently retained through WAL replay"),
    }
    // Via snapshot load.
    let dir = tmp("strict_dim_snap");
    {
        let mut g = Graph::create(&dir, config()).unwrap();
        g.register_node_type(person(), Some(8)).unwrap();
        g.commit().unwrap();
        g.snapshot().unwrap();
    }
    match Graph::open(&dir, strict()) {
        Err(Error::InvalidNodeType(m)) => assert!(m.contains("exceeds"), "{m}"),
        Err(other) => panic!("expected InvalidNodeType, got {other}"),
        Ok(_) => panic!("over-limit type silently retained through snapshot load"),
    }
}
