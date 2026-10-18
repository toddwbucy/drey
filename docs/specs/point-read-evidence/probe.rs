//! Synthetic point-read diagnostic; see README.md for timing boundaries.

use drey::{
    engine::*,
    query::{PropertyQuery, ScalarPredicate},
    Graph, GraphConfig, NodeId, NodeType, Properties, Scalar, Value,
};
use serde_json::json;
use std::{collections::BTreeMap, hint::black_box, path::Path, time::Instant};

/// Fixed-size public fields, with one unique indexed key.
fn props(i: usize) -> Properties {
    BTreeMap::from([
        ("key".into(), Value::String(format!("key-{i:08}"))),
        ("group".into(), Value::String("record".into())),
        ("body".into(), Value::String("x".repeat(128))),
    ])
}
/// Lower-level graph equality query.
fn property(i: usize) -> PropertyQuery {
    PropertyQuery {
        node_type: NodeType::new("record"),
        key: "key".into(),
        predicate: ScalarPredicate::Eq(Scalar::String(format!("key-{i:08}"))),
    }
}
/// Engine equality query returning one projected record.
fn engine_query(i: usize) -> Query {
    Query {
        selection: Selection::Type {
            node_type: "record".into(),
        },
        filters: vec![Filter {
            key: "key".into(),
            predicate: Predicate::Eq(Scalar::String(format!("key-{i:08}"))),
        }],
        rank: None,
        limit: 1,
        expand: None,
        projection: Projection::default(),
        related_projection: Projection::default(),
        work_limit: None,
    }
}
/// Measure query construction, result consumption, and drop in release mode.
fn measure<T>(size: usize, name: &str, mut run: impl FnMut(usize) -> T) {
    for i in 0..200 {
        black_box(run(i % size));
    }
    let mut times = Vec::with_capacity(1000);
    for i in 0..1000 {
        let target = (i * 7919 + 17) % size;
        let start = Instant::now();
        black_box(run(target));
        times.push(start.elapsed().as_nanos());
    }
    times.sort_unstable();
    println!(
        "{}",
        json!({"population":size, "path":name, "samples":times.len(), "warmup":200, "p50_us":times[499] as f64/1000., "p95_us":times[949] as f64/1000., "p99_us":times[989] as f64/1000.})
    );
}
/// Compare resident read paths across fixed populations and check recovery.
fn main() {
    let root = std::env::args().nth(1).expect("fresh output directory");
    for n in [100usize, 1000, 10_000] {
        let cfg = GraphConfig::default().with_indexed_property(NodeType::new("record"), "key");
        let mut indexed = Graph::in_memory(cfg.clone());
        let mut scan = Graph::in_memory(GraphConfig::default());
        indexed
            .register_node_type(NodeType::new("record"), None)
            .unwrap();
        scan.register_node_type(NodeType::new("record"), None)
            .unwrap();
        for i in 0..n {
            assert_eq!(
                indexed.add_node(NodeType::new("record"), props(i)).unwrap(),
                NodeId(i as u64)
            );
            scan.add_node(NodeType::new("record"), props(i)).unwrap();
        }
        measure(n, "graph_id", |i| indexed.node(NodeId(i as u64)).unwrap());
        measure(n, "graph_indexed_key", |i| {
            let ids = indexed.nodes_by_property(property(i)).unwrap();
            assert_eq!(ids, vec![NodeId(i as u64)]);
            indexed.node(ids[0]).unwrap()
        });
        measure(n, "graph_unindexed_key", |i| {
            let ids = scan.nodes_by_property(property(i)).unwrap();
            assert_eq!(ids, vec![NodeId(i as u64)]);
            scan.node(ids[0]).unwrap()
        });
        // Same snapshot, different open-time index configurations.
        let snapshot = Path::new(&root).join(format!("snapshot-{n}.bin"));
        indexed.export(&snapshot).unwrap();
        let reopened_configured = Graph::import(&snapshot, cfg).unwrap();
        let reopened_default = Graph::import(&snapshot, GraphConfig::default()).unwrap();
        measure(n, "graph_import_same_config", |i| {
            reopened_configured.nodes_by_property(property(i)).unwrap()
        });
        measure(n, "graph_import_default_config", |i| {
            reopened_default.nodes_by_property(property(i)).unwrap()
        });
        let db = Path::new(&root).join(format!("engine-{n}"));
        let mut e = Engine::create(
            &db,
            DatabaseConfig {
                types: BTreeMap::from([("record".into(), None)]),
                indexes: vec![
                    ("record".into(), "key".into()),
                    ("record".into(), "group".into()),
                ],
                limits: Limits::default(),
            },
        )
        .unwrap();
        let mut engine_ids = Vec::new();
        for start in (0..n).step_by(1000) {
            let stop = (start + 1000).min(n);
            let r = e
                .write(Batch {
                    token: format!("seed-{start}"),
                    operations: (start..stop)
                        .map(|i| Write::CreateNode {
                            key: i.to_string(),
                            node_type: "record".into(),
                            properties: props(i),
                            document: None,
                            embedding: None,
                        })
                        .collect(),
                })
                .unwrap();
            engine_ids.extend((start..stop).map(|i| r.nodes[&i.to_string()]));
        }
        measure(n, "engine_indexed_key", |i| {
            let r = e.query(engine_query(i)).unwrap();
            assert_eq!(r.hits.len(), 1);
            assert_eq!(r.hits[0].document.id, engine_ids[i]);
            r
        });
        measure(n, "engine_indexed_range", |i| {
            let mut q = engine_query(i);
            let value = Scalar::String(format!("key-{i:08}"));
            q.filters[0].predicate = Predicate::Range {
                min: Some(value.clone()),
                max: Some(value),
            };
            let r = e.query(q).unwrap();
            assert_eq!(r.hits.len(), 1);
            assert_eq!(r.hits[0].document.id, engine_ids[i]);
            r
        });
        measure(n, "engine_id", |i| {
            let mut q = engine_query(i);
            q.selection = Selection::Ids {
                ids: vec![engine_ids[i]],
            };
            q.filters.clear();
            e.query(q).unwrap()
        });
        let mut details = Vec::new();
        for missing in [false, true] {
            let mut q = engine_query(if missing { n } else { n / 2 });
            let r = e.query(q.clone()).unwrap();
            q.work_limit = Some(8);
            details.push(json!({"missing":missing,"work":r.work,"candidates":r.candidates,"hits":r.hits.len(),"work_limit_8_result":format!("{:?}",e.query(q).map(|x|x.work))}));
        }
        let mut q = engine_query(n / 2);
        q.filters[0].predicate = Predicate::Range {
            min: Some(Scalar::String(format!("key-{:08}", n / 2))),
            max: Some(Scalar::String(format!("key-{:08}", n / 2))),
        };
        let ranged = e.query(q).unwrap();
        assert_eq!(ranged.hits.len(), 1);
        assert_eq!(ranged.hits[0].document.id, engine_ids[n / 2]);
        e.checkpoint().unwrap();
        drop(e);
        let e = Engine::open(&db).unwrap();
        let after = e.query(engine_query(n / 2)).unwrap();
        assert_eq!(after.hits[0].document.id, engine_ids[n / 2]);
        println!(
            "{}",
            json!({"population":n,"details":details,"single_value_range_work":ranged.work,"engine_reopen_work":after.work,"engine_reopen_hits":after.hits.len()})
        );
    }
}
