use drey::{engine::*, Direction, NodeId, Properties, Scalar, Value};
use std::{collections::BTreeMap, fs, path::PathBuf};

fn dir(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("drey_engine_{}_{name}", std::process::id()));
    let _ = fs::remove_dir_all(&p);
    p
}
fn space() -> Space {
    Space {
        model: "fixture".into(),
        revision: "1".into(),
        dimensions: 2,
        metric: Metric::Dot,
        unit_normalized: false,
    }
}

#[test]
fn edge_type_vocabulary_is_bounded_across_deletion_and_reopen() {
    let path = dir("edge_types");
    let mut e = Engine::create(&path, config()).unwrap();
    let r = e.write(fixture()).unwrap();
    let edge = |i| Write::CreateEdge {
        key: format!("edge-{i}"),
        from: NodeRef::Id(r.nodes["a"]),
        to: NodeRef::Id(r.nodes["b"]),
        edge_type: format!("type-{i}"),
        weight: 1.0,
        properties: Properties::new(),
    };
    let types = e
        .write(Batch {
            token: "types".into(),
            operations: (0..127).map(edge).collect(),
        })
        .unwrap();
    e.write(Batch {
        token: "remove".into(),
        operations: types
            .edges
            .values()
            .map(|id| Write::DeleteEdge { id: *id })
            .collect(),
    })
    .unwrap();
    drop(e);
    let mut e = Engine::open(&path).unwrap();
    let sequence = e.status().unwrap().sequence;
    assert!(matches!(
        e.write(Batch {
            token: "too-many".into(),
            operations: vec![edge(127)],
        }),
        Err(drey::Error::ResourceLimit(_))
    ));
    assert_eq!(e.status().unwrap().sequence, sequence);
    e.write(Batch {
        token: "reuse".into(),
        operations: vec![edge(0)],
    })
    .unwrap();
}
fn config() -> DatabaseConfig {
    DatabaseConfig {
        types: BTreeMap::from([("passage".into(), Some(space())), ("source".into(), None)]),
        indexes: vec![("passage".into(), "group".into())],
        limits: Limits::default(),
    }
}
fn node(key: &str, score: f32) -> Write {
    Write::CreateNode {
        key: key.into(),
        node_type: "passage".into(),
        properties: BTreeMap::from([("group".into(), Value::I64(1))]),
        document: Some("{\"nested\": [null, {\"content\":\"intact\"}]}".into()),
        embedding: Some(Vector {
            space: space(),
            values: vec![score, 1.0],
        }),
    }
}
fn fixture() -> Batch {
    Batch {
        token: "seed".into(),
        operations: vec![
            node("a", 1.0),
            node("b", 2.0),
            Write::CreateNode {
                key: "source".into(),
                node_type: "source".into(),
                properties: BTreeMap::from([(
                    "uri".into(),
                    Value::String("fixture://source".into()),
                )]),
                document: None,
                embedding: None,
            },
            Write::CreateEdge {
                key: "support".into(),
                from: NodeRef::Created("b".into()),
                to: NodeRef::Created("source".into()),
                edge_type: "supports".into(),
                weight: 1.0,
                properties: Properties::new(),
            },
        ],
    }
}
fn walk() -> Walk {
    Walk {
        direction: Direction::Outbound,
        edge_types: vec!["supports".into()],
        max_hops: 1,
        min_weight: None,
    }
}
fn query() -> Query {
    Query {
        selection: Selection::Type {
            node_type: "passage".into(),
        },
        filters: vec![Filter {
            key: "group".into(),
            predicate: Predicate::Eq(Scalar::I64(1)),
        }],
        rank: Some(Rank {
            vector: Vector {
                space: space(),
                values: vec![1.0, 0.0],
            },
            k: 2,
        }),
        limit: 2,
        expand: Some(walk()),
        projection: Projection {
            fields: None,
            document: true,
        },
        related_projection: Projection {
            fields: None,
            document: true,
        },
        work_limit: None,
    }
}

#[test]
fn atomic_batch_retry_queries_and_checkpoint_roundtrip() {
    let path = dir("lifecycle");
    let mut e = Engine::create(&path, config()).unwrap();
    let mut invalid = fixture();
    if let Write::CreateNode {
        embedding: Some(v), ..
    } = &mut invalid.operations[1]
    {
        v.values.pop();
    }
    assert!(e.write(invalid).is_err());
    assert_eq!(e.status().unwrap().nodes, 0);
    let receipt = e.write(fixture()).unwrap();
    let result = e.query(query()).unwrap();
    assert_eq!(result.hits.len(), 2);
    assert_eq!(result.hits[0].document.id, receipt.nodes["b"]);
    assert_eq!(result.hits[0].score, Some(2.0));
    assert_eq!(result.hits[0].related[0].via.id, receipt.edges["support"]);
    assert_eq!(
        result.hits[0].related[0].document.id,
        receipt.nodes["source"]
    );
    assert_eq!(
        result.hits[0].document.document.as_deref(),
        Some("{\"nested\": [null, {\"content\":\"intact\"}]}")
    );
    assert_eq!(e.write(fixture()).unwrap(), receipt);
    let mut conflict = fixture();
    conflict.operations.pop();
    assert!(matches!(e.write(conflict), Err(drey::Error::Conflict(_))));
    let mut q = query();
    q.selection = Selection::Reachable {
        from: receipt.nodes["source"],
        walk: Walk {
            direction: Direction::Inbound,
            ..walk()
        },
    };
    let reached = e.query(q).unwrap();
    assert_eq!(reached.hits.len(), 1);
    assert_eq!(reached.hits[0].document.id, receipt.nodes["b"]);
    e.checkpoint().unwrap();
    drop(e);
    let mut e = Engine::open(&path).unwrap();
    assert_eq!(e.write(fixture()).unwrap(), receipt);
    assert_eq!(
        serde_json::to_value(e.query(query()).unwrap().hits).unwrap(),
        serde_json::to_value(result.hits).unwrap()
    );
}

#[test]
fn updates_deletions_clear_vectors_and_keep_indexes_consistent() {
    let path = dir("updates");
    let mut e = Engine::create(&path, config()).unwrap();
    let r = e.write(fixture()).unwrap();
    assert!(e
        .write(Batch {
            token: "refused".into(),
            operations: vec![Write::DeleteNode { id: r.nodes["b"] }]
        })
        .is_err());
    assert_eq!(e.status().unwrap().nodes, 3);
    e.write(Batch {
        token: "replace".into(),
        operations: vec![
            Write::ReplaceNode {
                id: r.nodes["a"],
                properties: BTreeMap::from([("group".into(), Value::I64(2))]),
                document: Some("[1,2,null]".into()),
                embedding: None,
            },
            Write::DeleteEdge {
                id: r.edges["support"],
            },
            Write::DeleteNode { id: r.nodes["b"] },
        ],
    })
    .unwrap();
    assert!(e.query(query()).unwrap().hits.is_empty());
    let mut q = query();
    q.filters[0].predicate = Predicate::Eq(Scalar::I64(2));
    q.rank = None;
    let hits = e.query(q.clone()).unwrap().hits;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].document.document.as_deref(), Some("[1,2,null]"));
    q.rank = query().rank;
    assert!(e.query(q).unwrap().hits.is_empty());
    drop(e);
    let e = Engine::open(&path).unwrap();
    assert_eq!(e.status().unwrap().edges, 0);
    assert!(e.query(query()).unwrap().hits.is_empty());
}

#[test]
fn incompatible_spaces_private_ids_and_resource_exhaustion_refuse() {
    let path = dir("bounds");
    let mut e = Engine::create(&path, config()).unwrap();
    e.write(fixture()).unwrap();
    let mut q = query();
    q.rank.as_mut().unwrap().vector.space.model = "other".into();
    assert!(e.query(q).is_err());
    let mut q = query();
    q.work_limit = Some(1);
    assert!(matches!(e.query(q), Err(drey::Error::ResourceLimit(_))));
    let mut q = query();
    q.selection = Selection::Ids {
        ids: vec![NodeId(0)],
    };
    assert!(e.query(q).is_err());
    let mut b = fixture();
    b.token = "other".into();
    if let Write::CreateNode {
        embedding: Some(v), ..
    } = &mut b.operations[0]
    {
        v.space.revision = "different".into();
    }
    assert!(e.write(b).is_err());
    assert_eq!(e.status().unwrap().nodes, 3);
    assert!(Engine::open(&path).is_err());
    drop(e);
    assert!(Engine::open(&path).is_ok());
}

#[test]
fn acknowledged_wal_length_corruption_refuses_without_modifying_history() {
    let path = dir("length_corruption");
    let mut e = Engine::create(&path, config()).unwrap();
    e.write(fixture()).unwrap();
    drop(e);
    let p = path.join("wal.log");
    let mut bytes = fs::read(&p).unwrap();
    bytes[20..24].copy_from_slice(&u32::MAX.to_le_bytes());
    fs::write(&p, &bytes).unwrap();
    assert!(Engine::open(&path).is_err());
    assert_eq!(fs::read(&p).unwrap(), bytes);
}

#[test]
fn unacknowledged_tail_is_discarded_without_losing_a_durable_batch() {
    use std::io::Write;
    let path = dir("tail");
    let mut e = Engine::create(&path, config()).unwrap();
    let r = e.write(fixture()).unwrap();
    drop(e);
    fs::OpenOptions::new()
        .append(true)
        .open(path.join("wal.log"))
        .unwrap()
        .write_all(b"torn-unacknowledged-frame")
        .unwrap();
    let mut e = Engine::open(&path).unwrap();
    assert_eq!(e.write(fixture()).unwrap(), r);
    assert_eq!(e.status().unwrap().nodes, 3);
}

#[test]
fn quotas_reject_whole_batches_and_do_not_evict_retry_receipts() {
    let mut cfg = config();
    cfg.limits.max_nodes = 1;
    let mut e = Engine::create(dir("node_quota"), cfg).unwrap();
    assert!(matches!(
        e.write(fixture()),
        Err(drey::Error::ResourceLimit(_))
    ));
    assert_eq!(e.status().unwrap().nodes, 0);
    assert_eq!(e.status().unwrap().receipts, 0);
    let mut cfg = config();
    cfg.limits.max_receipts = 1;
    let mut e = Engine::create(dir("receipt_quota"), cfg).unwrap();
    let receipt = e.write(fixture()).unwrap();
    let mut second = fixture();
    second.token = "second".into();
    assert!(matches!(
        e.write(second),
        Err(drey::Error::ResourceLimit(_))
    ));
    assert_eq!(e.write(fixture()).unwrap(), receipt);
    assert_eq!(e.status().unwrap().nodes, 3);
}

#[test]
fn expansion_preserves_multiple_supports_and_refuses_incomplete_output() {
    let mut cfg = config();
    cfg.limits.max_result_bytes = 4096;
    let mut e = Engine::create(dir("expansion"), cfg).unwrap();
    let r = e.write(fixture()).unwrap();
    e.write(Batch {
        token: "second-support".into(),
        operations: vec![Write::CreateEdge {
            key: "other".into(),
            from: NodeRef::Id(r.nodes["b"]),
            to: NodeRef::Id(r.nodes["source"]),
            edge_type: "supports".into(),
            weight: 0.5,
            properties: Properties::new(),
        }],
    })
    .unwrap();
    let mut q = query();
    q.limit = 1;
    let result = e.query(q.clone()).unwrap();
    assert_eq!(result.hits.len(), 1);
    assert_eq!(result.hits[0].related.len(), 2);
    assert_ne!(
        result.hits[0].related[0].via.id,
        result.hits[0].related[1].via.id
    );
    q.expand.as_mut().unwrap().max_hops = 9;
    assert!(e.query(q).is_err());
    let id = r.nodes["b"];
    e.write(Batch {
        token: "large".into(),
        operations: vec![Write::ReplaceNode {
            id,
            properties: BTreeMap::from([("group".into(), Value::I64(1))]),
            document: Some(serde_json::to_string(&"x".repeat(5000)).unwrap()),
            embedding: Some(Vector {
                space: space(),
                values: vec![2.0, 1.0],
            }),
        }],
    })
    .unwrap();
    assert!(matches!(
        e.query(query()),
        Err(drey::Error::ResourceLimit(_))
    ));
}

#[test]
fn equal_dimensions_do_not_allow_ranking_incompatible_models() {
    let mut cfg = config();
    let mut other = space();
    other.model = "different".into();
    cfg.types.insert("other".into(), Some(other.clone()));
    let mut e = Engine::create(dir("spaces"), cfg).unwrap();
    let r = e.write(fixture()).unwrap();
    let second = e
        .write(Batch {
            token: "different-space".into(),
            operations: vec![Write::CreateNode {
                key: "other".into(),
                node_type: "other".into(),
                properties: Properties::new(),
                document: None,
                embedding: Some(Vector {
                    space: other,
                    values: vec![100.0, 1.0],
                }),
            }],
        })
        .unwrap();
    let mut q = query();
    q.selection = Selection::Ids {
        ids: vec![r.nodes["b"], second.nodes["other"]],
    };
    q.filters.clear();
    assert!(matches!(e.query(q), Err(drey::Error::InvalidRequest(_))));
    assert_eq!(e.query(query()).unwrap().hits[0].document.id, r.nodes["b"]);
}
