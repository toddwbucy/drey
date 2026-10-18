//! Reproducible complete-request experiment against the real local service.
//! Run in release; synthetic vectors prove ordering/content, not model relevance.
use drey::{engine::*, Direction, Properties, Scalar, Value};
use drey_local::{Client, Reply, Request};
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Child, Command},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

fn call(c: &mut Client, r: Request) -> Reply {
    c.call(r).expect("transport").result.expect("engine")
}
fn vector(dim: usize, score: f32) -> Vector {
    let mut values = vec![0.125; dim];
    values[0] = score;
    Vector {
        space: Space {
            model: "synthetic".into(),
            revision: "1".into(),
            dimensions: dim,
            metric: Metric::Dot,
            unit_normalized: false,
        },
        values,
    }
}
fn query(dim: usize, source: drey::NodeId, reachable: bool) -> Query {
    let mut v = vector(dim, 1.0);
    v.values.fill(0.0);
    v.values[0] = 1.0;
    let walk = Walk {
        direction: Direction::Outbound,
        edge_types: vec!["supports".into()],
        max_hops: 1,
        min_weight: None,
    };
    Query {
        selection: if reachable {
            Selection::Reachable {
                from: source,
                walk: Walk {
                    direction: Direction::Inbound,
                    ..walk.clone()
                },
            }
        } else {
            Selection::Type {
                node_type: "passage".into(),
            }
        },
        filters: vec![Filter {
            key: "group".into(),
            predicate: Predicate::Eq(Scalar::I64(0)),
        }],
        rank: Some(Rank { vector: v, k: 8 }),
        limit: 8,
        expand: Some(walk),
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
fn percentiles(v: &[u64]) -> serde_json::Value {
    let mut v = v.to_vec();
    v.sort_unstable();
    let p = |n: usize| v[(v.len() * n).div_ceil(100) - 1];
    json!({"p50":p(50),"p95":p(95),"p99":p(99),"max":v[v.len()-1]})
}
fn memory(pid: u32) -> serde_json::Value {
    let text = fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    let field = |key: &str| {
        text.lines()
            .find(|l| l.starts_with(key))
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse::<u64>()
            .unwrap()
            * 1024
    };
    json!({"rss_bytes":field("VmRSS:"),"peak_rss_bytes":field("VmHWM:")})
}
struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn main() {
    let args: Vec<_> = std::env::args().collect();
    assert!(
        args.len() == 5,
        "usage: local_bench NEW_DIRECTORY PASSAGES DIM TEXT_BYTES"
    );
    let dir = PathBuf::from(&args[1]);
    let n: usize = args[2].parse().unwrap();
    let dim: usize = args[3].parse().unwrap();
    let text_bytes: usize = args[4].parse().unwrap();
    assert!((40..=20_000).contains(&n) && dim > 0);
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    let socket = dir.join("service.sock");
    let bin = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .join("drey-local");
    let mut child = ChildGuard(
        Command::new(bin)
            .args([
                "serve",
                dir.join("db").to_str().unwrap(),
                socket.to_str().unwrap(),
            ])
            .spawn()
            .unwrap(),
    );
    let start = Instant::now();
    let mut c = loop {
        if let Ok(c) = Client::connect(&socket) {
            break c;
        }
        assert!(child.0.try_wait().unwrap().is_none());
        assert!(start.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(5));
    };
    let config = DatabaseConfig {
        types: BTreeMap::from([
            ("passage".into(), Some(vector(dim, 0.0).space)),
            ("source".into(), None),
        ]),
        indexes: vec![("passage".into(), "group".into())],
        limits: Limits {
            max_query_work: 200_000,
            ..Default::default()
        },
    };
    fs::write(
        dir.join("create.json"),
        serde_json::to_vec_pretty(&Request::Create {
            config: config.clone(),
        })
        .unwrap(),
    )
    .unwrap();
    call(&mut c, Request::Create { config });
    let Reply::Written(source) = call(
        &mut c,
        Request::Write {
            batch: Batch {
                token: "source".into(),
                operations: vec![Write::CreateNode {
                    key: "source".into(),
                    node_type: "source".into(),
                    properties: BTreeMap::from([(
                        "uri".into(),
                        Value::String("fixture://source".into()),
                    )]),
                    document: None,
                    embedding: None,
                }],
            },
        },
    ) else {
        panic!()
    };
    let source = source.nodes["source"];
    let text = "x".repeat(text_bytes);
    let mut ids = Vec::new();
    let mut edges = Vec::new();
    let mut writes = Vec::new();
    let mut retry = None;
    for first in (0..n).step_by(32) {
        let mut operations = Vec::new();
        for i in first..(first + 32).min(n) {
            let key = i.to_string();
            operations.push(Write::CreateNode {
                key: key.clone(),
                node_type: "passage".into(),
                properties: BTreeMap::from([
                    ("group".into(), Value::I64((i % 5) as i64)),
                    ("index".into(), Value::I64(i as i64)),
                ]),
                document: Some(
                    json!({"content":[{"text":text}],"nested":[true,null,{"keep":"all"}]})
                        .to_string(),
                ),
                embedding: Some(vector(dim, (i + 1) as f32 / n as f32)),
            });
            operations.push(Write::CreateEdge {
                key: key.clone(),
                from: NodeRef::Created(key),
                to: NodeRef::Id(source),
                edge_type: "supports".into(),
                weight: 1.0,
                properties: Properties::new(),
            });
        }
        let batch = Batch {
            token: format!("seed-{first}"),
            operations,
        };
        let start = Instant::now();
        let Reply::Written(r) = call(
            &mut c,
            Request::Write {
                batch: batch.clone(),
            },
        ) else {
            panic!()
        };
        writes.push(start.elapsed().as_nanos() as u64);
        for i in first..(first + 32).min(n) {
            ids.push(r.nodes[&i.to_string()]);
            edges.push(r.edges[&i.to_string()]);
        }
        retry = Some((batch, r));
    }
    call(&mut c, Request::Checkpoint {});
    call(&mut c, Request::Close {});
    let reopen = Instant::now();
    call(&mut c, Request::Open {});
    let reopen_ns = reopen.elapsed().as_nanos() as u64;
    let (batch, receipt) = retry.unwrap();
    let Reply::Written(repeated) = call(&mut c, Request::Write { batch }) else {
        panic!()
    };
    assert_eq!(receipt, repeated);
    let before_memory = memory(child.0.id());
    let mut results = Vec::new();
    for background in [false, true] {
        for reachable in [false, true] {
            let q = query(dim, source, reachable);
            fs::write(
                dir.join(if reachable {
                    "reachable-query.json"
                } else {
                    "filtered-query.json"
                }),
                serde_json::to_vec_pretty(&Request::Query { query: q.clone() }).unwrap(),
            )
            .unwrap();
            let done = Arc::new(AtomicBool::new(false));
            let mut writer = None;
            if background {
                let done = done.clone();
                let socket = socket.clone();
                let id = ids[1];
                writer = Some(std::thread::spawn(move || {
                    let mut c = Client::connect(socket).unwrap();
                    let mut i = 0;
                    let mut times = Vec::new();
                    while !done.load(Ordering::Acquire) {
                        let start = Instant::now();
                        call(
                            &mut c,
                            Request::Write {
                                batch: Batch {
                                    token: format!("background-{reachable}-{i}"),
                                    operations: vec![Write::ReplaceNode {
                                        id,
                                        properties: BTreeMap::from([
                                            ("group".into(), Value::I64(1)),
                                            ("index".into(), Value::I64(1)),
                                        ]),
                                        document: Some("{\"background\":true}".into()),
                                        embedding: Some(vector(dim, 1.0)),
                                    }],
                                },
                            },
                        );
                        times.push(start.elapsed().as_nanos() as u64);
                        i += 1;
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    times
                }));
            }
            let mut times = Vec::new();
            let mut response_bytes = 0;
            let mut candidate_count = 0;
            let expected: Vec<_> = (0..n).rev().filter(|i| i % 5 == 0).take(8).collect();
            for sample in 0..1100 {
                let start = Instant::now();
                let response = c.call(Request::Query { query: q.clone() }).unwrap();
                let Reply::Rows(rows) = response.result.as_ref().unwrap() else {
                    panic!()
                };
                assert_eq!(rows.hits.len(), 8);
                candidate_count = rows.candidates;
                for (hit, i) in rows.hits.iter().zip(&expected) {
                    assert_eq!(hit.document.id, ids[*i]);
                    assert_eq!(hit.document.properties["index"], Value::I64(*i as i64));
                    let body: serde_json::Value =
                        serde_json::from_str(hit.document.document.as_ref().unwrap()).unwrap();
                    assert_eq!(body["content"][0]["text"], text);
                    assert_eq!(body["nested"], json!([true,null,{"keep":"all"}]));
                    assert_eq!(hit.related.len(), 1);
                    assert_eq!(hit.related[0].via.id, edges[*i]);
                    assert_eq!(hit.related[0].document.id, source);
                    assert_eq!(
                        hit.related[0].document.properties["uri"],
                        Value::String("fixture://source".into())
                    );
                }
                let elapsed = start.elapsed().as_nanos() as u64;
                if sample >= 100 {
                    times.push(elapsed);
                }
                if sample == 100 {
                    response_bytes = serde_json::to_vec(&response).unwrap().len();
                }
            }
            done.store(true, Ordering::Release);
            let background_writes = writer.map(|w| w.join().unwrap()).unwrap_or_default();
            results.push(json!({"reachable":reachable,"background_writes":background_writes.len(),"background_write_ns":if background_writes.is_empty(){serde_json::Value::Null}else{percentiles(&background_writes)},"background_write_raw_ns":background_writes,"candidates":candidate_count,"response_json_bytes_sample":response_bytes,"retrieval_ns":percentiles(&times),"raw_retrieval_ns":times,"deadline_ns":null,"deadline_misses":null}));
        }
    }
    let status = match call(&mut c, Request::Status {}) {
        Reply::Ready(s) => s,
        _ => panic!(),
    };
    let after_memory = memory(child.0.id());
    let report = json!({"schema":"drey-local-benchmark-v1","source":"synthetic deterministic v1","passages":n,"sources":1,"edges":n,"dimensions":dim,"text_bytes_per_passage":text_bytes,"metric":"dot","warmups":100,"samples":1000,"percentile":"nearest-rank","ingestion_batch_max_nodes":32,"ingestion_roundtrip_ns":percentiles(&writes),"raw_ingestion_ns":writes,"reopen_roundtrip_ns":reopen_ns,"memory_before_queries":before_memory,"memory_after_queries":after_memory,"status":status,"results":results,"includes":"request construction and serialization, IPC, queueing, engine query, server serialization, client decoding and content/reference checks","excludes":"query embedding generation, inference, connection establishment"});
    fs::write(
        dir.join("results.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
    println!("{}", serde_json::to_string(&report).unwrap());
    call(&mut c, Request::Shutdown {});
    drop(c);
    assert!(child.0.wait().unwrap().success());
}
