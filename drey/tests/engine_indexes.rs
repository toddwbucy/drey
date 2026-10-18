//! Indexed selection must bound work by matches, including after recovery.

use drey::{engine::*, Error, NodeId, Properties, Scalar, Value};
use std::{collections::BTreeMap, fs, path::PathBuf};

/// Create a separate database for each test, optionally indexing `record.key`.
fn database(name: &str, indexed: bool) -> (Engine, PathBuf) {
    let path = std::env::temp_dir().join(format!("drey_indexes_{}_{name}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    let config = DatabaseConfig {
        types: BTreeMap::from([("record".into(), None), ("other".into(), None)]),
        indexes: if indexed {
            vec![("record".into(), "key".into())]
        } else {
            vec![]
        },
        limits: Limits::default(),
    };
    (Engine::create(&path, config).unwrap(), path)
}

/// Pair the indexed value with an unindexed AND-filter field.
fn properties(value: Value, keep: bool) -> Properties {
    BTreeMap::from([("key".into(), value), ("keep".into(), Value::Bool(keep))])
}

/// Populate bounded durable batches and return IDs in insertion order.
fn insert(e: &mut Engine, token: &str, values: &[Value]) -> Vec<NodeId> {
    let mut ids = Vec::new();
    for (batch, chunk) in values.chunks(1000).enumerate() {
        let r = e
            .write(Batch {
                token: format!("{token}-{batch}"),
                operations: chunk
                    .iter()
                    .enumerate()
                    .map(|(i, value)| Write::CreateNode {
                        key: i.to_string(),
                        node_type: "record".into(),
                        properties: properties(value.clone(), i % 2 == 0),
                        document: None,
                        embedding: None,
                    })
                    .collect(),
            })
            .unwrap();
        ids.extend((0..chunk.len()).map(|i| r.nodes[&i.to_string()]));
    }
    ids
}

/// Keep string-key width constant as the population grows.
fn key(i: usize) -> String {
    format!("key-{i:08}")
}

/// Build a type-selected property query without ranking or expansion.
fn query(predicate: Predicate, work_limit: Option<usize>) -> Query {
    Query {
        selection: Selection::Type {
            node_type: "record".into(),
        },
        filters: vec![Filter {
            key: "key".into(),
            predicate,
        }],
        rank: None,
        limit: 100,
        expand: None,
        projection: Projection::default(),
        related_projection: Projection::default(),
        work_limit,
    }
}

/// Build an equality lookup with an explicit caller work ceiling.
fn eq(value: &str, work_limit: usize) -> Query {
    query(
        Predicate::Eq(Scalar::String(value.into())),
        Some(work_limit),
    )
}

/// Extract the observable result order without comparing work counters.
fn ids(result: QueryResult) -> Vec<NodeId> {
    result.hits.into_iter().map(|h| h.document.id).collect()
}

#[test]
/// Unrelated indexed values cannot exhaust a point lookup's work budget.
fn indexed_equality_work_stays_bounded_as_population_grows() {
    let (mut e, _) = database("population", true);
    assert!(e.query(eq("missing", 1)).unwrap().hits.is_empty());
    let mut expected = Vec::new();
    for population in [1, 100, 10_000] {
        let values: Vec<_> = (expected.len()..population)
            .map(|i| Value::String(key(i)))
            .collect();
        expected.extend(insert(&mut e, &format!("size-{population}"), &values));
        for i in [0, population / 2, population - 1] {
            let result = e.query(eq(&key(i), 2)).unwrap();
            assert_eq!(result.candidates, 1);
            assert_eq!(result.work, 2); // one seek and one candidate visit
            assert_eq!(ids(result), vec![expected[i]]);
        }
        let missing = e.query(eq("missing", 1)).unwrap();
        assert_eq!(missing.candidates, 0);
        assert_eq!(missing.work, 1);
        assert!(missing.hits.is_empty());
    }
    assert!(matches!(
        e.query(eq("missing", 0)),
        Err(Error::ResourceLimit(_))
    ));
}

#[test]
/// Tree bounds preserve scalar semantics and skip unrelated value buckets.
fn indexed_scalar_predicates_match_scan_and_ranges_visit_only_matching_values() {
    let (mut e, _) = database("scalar_order", true);
    let values = [
        Value::Bool(false),
        Value::Bool(true),
        Value::I64(-1),
        Value::F64(-0.0),
        Value::I64(0),
        Value::F64(1.0),
        Value::I64(1),
        Value::I64(3),
        Value::F64(3.0),
        Value::I64((1i64 << 53) + 1),
        Value::F64((1i64 << 53) as f64),
        Value::I64(i64::MAX),
        Value::F64(i64::MAX as f64),
        Value::String("a".into()),
        Value::String("z".into()),
        Value::Null,
        Value::List(vec![Scalar::I64(1)]),
        Value::Bytes(vec![1]),
    ];
    let all = insert(&mut e, "values", &values);
    let mut predicates: Vec<_> = values
        .iter()
        .filter_map(Value::as_scalar)
        .map(Predicate::Eq)
        .collect();
    for (min, max) in [
        (None, None),
        (None, Some(Scalar::I64(-1))),
        (Some(Scalar::F64(0.0)), Some(Scalar::I64(3))),
        (Some(Scalar::I64(3)), None),
        (Some(Scalar::I64(3)), Some(Scalar::I64(1))),
        (Some(Scalar::I64(4)), Some(Scalar::I64(8))),
        (
            Some(Scalar::I64((1i64 << 53) + 1)),
            Some(Scalar::I64(i64::MAX)),
        ),
        (
            Some(Scalar::String("a".into())),
            Some(Scalar::String("z".into())),
        ),
    ] {
        predicates.push(Predicate::Range { min, max });
    }
    for predicate in predicates {
        let q = query(predicate, None);
        let indexed = e.query(q.clone()).unwrap();
        let mut scan = q;
        scan.selection = Selection::Ids { ids: all.clone() };
        let scanned = e.query(scan).unwrap();
        assert_eq!(indexed.candidates, scanned.candidates);
        assert_eq!(
            serde_json::to_value(indexed.hits).unwrap(),
            serde_json::to_value(scanned.hits).unwrap()
        );
    }
    let range = |min: Scalar, max: Scalar, work| {
        query(
            Predicate::Range {
                min: Some(min),
                max: Some(max),
            },
            Some(work),
        )
    };
    let narrow = range(Scalar::String("a".into()), Scalar::String("a".into()), 3);
    let result = e.query(narrow.clone()).unwrap();
    assert_eq!(result.work, 3); // seek, one range bucket, one candidate
    assert_eq!(ids(result), vec![all[13]]);
    let mut too_small = narrow;
    too_small.work_limit = Some(2);
    assert!(matches!(e.query(too_small), Err(Error::ResourceLimit(_))));
    for (lo, hi) in [(3, 1), (4, 8)] {
        let result = e.query(range(Scalar::I64(lo), Scalar::I64(hi), 1)).unwrap();
        assert!(result.hits.is_empty());
        assert_eq!(result.work, 1);
    }
}

#[test]
/// All bucket members count as work even when filtered or limited out of output.
fn indexed_buckets_keep_and_filters_limits_and_complete_work_accounting() {
    let (mut e, _) = database("duplicates", true);
    let all = insert(
        &mut e,
        "duplicates",
        &vec![Value::String("same".into()); 80],
    );
    let mut q = eq("same", 81);
    // The unindexed predicate remains an AND constraint even when listed first.
    q.filters.insert(
        0,
        Filter {
            key: "keep".into(),
            predicate: Predicate::Eq(Scalar::Bool(true)),
        },
    );
    q.projection.fields = Some(vec!["key".into()]);
    let result = e.query(q.clone()).unwrap();
    assert_eq!(result.candidates, 40);
    assert_eq!(result.work, 81);
    assert!(result.hits.iter().all(|h| h.document.properties.len() == 1));
    assert_eq!(
        ids(result),
        all.iter().step_by(2).copied().collect::<Vec<_>>()
    );
    q.limit = 1;
    let result = e.query(q.clone()).unwrap();
    assert_eq!(result.candidates, 40);
    assert_eq!(ids(result), vec![all[0]]);
    q.work_limit = Some(8);
    assert!(matches!(e.query(q), Err(Error::ResourceLimit(_))));
    let range = query(
        Predicate::Range {
            min: None,
            max: None,
        },
        Some(8),
    );
    assert!(matches!(e.query(range), Err(Error::ResourceLimit(_))));
}

#[test]
/// Index reconstruction preserves mutations, type isolation, and bounded reads.
fn indexed_lookup_tracks_mutations_through_wal_and_snapshot_recovery() {
    let (mut e, path) = database("recovery", true);
    let values: Vec<_> = (0..100).map(|i| Value::String(key(i))).collect();
    let all = insert(&mut e, "initial", &values);
    let replacement = |id| Write::ReplaceNode {
        id,
        properties: properties(Value::String("renamed".into()), true),
        document: None,
        embedding: None,
    };
    e.write(Batch {
        token: "change".into(),
        operations: vec![
            replacement(all[50]),
            Write::DeleteNode { id: all[51] },
            Write::CreateNode {
                key: "other".into(),
                node_type: "other".into(),
                properties: properties(Value::String("renamed".into()), true),
                document: None,
                embedding: None,
            },
        ],
    })
    .unwrap();
    let check = |e: &Engine, expected: &[NodeId]| {
        for i in [50, 51] {
            assert!(e.query(eq(&key(i), 1)).unwrap().hits.is_empty());
        }
        assert_eq!(
            ids(e.query(eq("renamed", expected.len() + 1)).unwrap()),
            expected
        );
        assert_eq!(ids(e.query(eq(&key(99), 2)).unwrap()), vec![all[99]]);
    };
    check(&e, &[all[50]]);
    drop(e);
    let mut e = Engine::open(&path).unwrap(); // committed WAL, no checkpoint
    check(&e, &[all[50]]);
    e.checkpoint().unwrap();
    drop(e);
    let mut e = Engine::open(&path).unwrap(); // reconstructed snapshot indexes
    check(&e, &[all[50]]);
    e.write(Batch {
        token: "after-checkpoint".into(),
        operations: vec![replacement(all[52])],
    })
    .unwrap();
    drop(e);
    let e = Engine::open(&path).unwrap(); // snapshot plus later WAL mutation
    check(&e, &[all[50], all[52]]);
    assert!(e.query(eq(&key(52), 1)).unwrap().hits.is_empty());
}

#[test]
/// Omitting the index preserves the explicit population-bound scan fallback.
fn unindexed_queries_preserve_scan_fallback_and_work_limits() {
    let (mut e, _) = database("unindexed", false);
    let values: Vec<_> = (0..100).map(|i| Value::String(key(i))).collect();
    let all = insert(&mut e, "values", &values);
    assert!(matches!(
        e.query(eq(&key(50), 2)),
        Err(Error::ResourceLimit(_))
    ));
    let result = e.query(eq(&key(50), 100)).unwrap();
    assert_eq!(result.work, 100);
    assert_eq!(ids(result), vec![all[50]]);
    let missing = e.query(eq("missing", 100)).unwrap();
    assert_eq!(missing.work, 100);
    assert!(missing.hits.is_empty());
}
