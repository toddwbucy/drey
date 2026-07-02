//! M2 exit criteria: persistence round-trip and the PRD §10.2.1 recovery matrix.

use std::fs::{self, OpenOptions};
use std::path::PathBuf;

use drey::config::GraphConfig;
use drey::mutation::RemoveNodeMode;
use drey::query::{PropertyQuery, ScalarPredicate};
use drey::types::{Embedding, NodeType, Scalar, Value};
use drey::{EdgeType, Error, Graph, NodeId};
use std::collections::BTreeMap;

fn person() -> NodeType {
    NodeType::new("person")
}
fn knows() -> EdgeType {
    EdgeType::new("knows")
}
fn props(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
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
        a = g.add_node(person(), props(&[("age", Value::I64(30)), ("tag", Value::Bytes(vec![0, 255, 7]))])).unwrap();
        b = g.add_node(person(), props(&[("age", Value::I64(40))])).unwrap();
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
    assert_eq!(na.properties.get("tag"), Some(&Value::Bytes(vec![0, 255, 7])));
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
    let f = OpenOptions::new().write(true).open(dir.join("wal.log")).unwrap();
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
        let mut f = OpenOptions::new().append(true).open(dir.join("wal.log")).unwrap();
        f.write_all(&[0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02]).unwrap();
    }
    let g = Graph::open(&dir, config()).unwrap();
    // Both committed nodes load; the garbage tail is ignored, not a partial load.
    assert_eq!(g.counts().0, 2);
    assert!(g.node(a).unwrap().is_some());
    assert!(g.node(b).unwrap().is_some());
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
        // WAL is truncated after snapshot.
        assert_eq!(fs::metadata(dir.join("wal.log")).unwrap().len(), 0);
        // Further mutation after snapshot still persists.
        g.remove_node(a, RemoveNodeMode::RejectIfEdgesExist).unwrap();
        g.commit().unwrap();
    }
    let g = Graph::open(&dir, config()).unwrap();
    assert_eq!(g.counts().0, 1); // two added, one removed
    assert!(g.node(a).unwrap().is_none());
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
        g.remove_node(gap, RemoveNodeMode::RejectIfEdgesExist).unwrap();
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
