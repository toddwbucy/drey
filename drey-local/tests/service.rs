use drey::engine::*;
use drey::{Properties, Value};
use drey_local::{
    read_frame, write_frame, Client, Envelope, Reply, Request, Response, PROTOCOL_VERSION,
};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::PathBuf,
    process::{Child, Command},
    time::{Duration, Instant},
};

struct Server {
    child: Child,
    dir: PathBuf,
    socket: PathBuf,
}
impl Server {
    fn start(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("drey_service_{}_{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        Self::spawn(dir)
    }
    fn spawn(dir: PathBuf) -> Self {
        let socket = dir.join("engine.sock");
        let mut child = Command::new(env!("CARGO_BIN_EXE_drey-local"))
            .args([
                "serve",
                dir.join("db").to_str().unwrap(),
                socket.to_str().unwrap(),
            ])
            .spawn()
            .unwrap();
        let start = Instant::now();
        loop {
            if Client::connect(&socket).is_ok() {
                break;
            }
            assert!(child.try_wait().unwrap().is_none(), "server exited");
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        }
        Self { child, dir, socket }
    }
    fn client(&self) -> Client {
        Client::connect(&self.socket).unwrap()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn config() -> DatabaseConfig {
    DatabaseConfig {
        types: BTreeMap::from([("record".into(), None)]),
        indexes: vec![("record".into(), "index".into())],
        limits: Limits::default(),
    }
}
fn batch(token: &str) -> Batch {
    Batch {
        token: token.into(),
        operations: vec![Write::CreateNode {
            key: "record".into(),
            node_type: "record".into(),
            properties: BTreeMap::from([("index".into(), Value::I64(1))]),
            document: Some("{\"keep\":[1,null,{\"a\":true}]}".into()),
            embedding: None,
        }],
    }
}
fn query() -> Query {
    Query {
        selection: Selection::Type {
            node_type: "record".into(),
        },
        filters: vec![],
        rank: None,
        limit: 100,
        expand: None,
        projection: Projection {
            fields: Some(vec!["index".into()]),
            document: true,
        },
        related_projection: Projection::default(),
        work_limit: None,
    }
}
fn ok(c: &mut Client, r: Request) -> Reply {
    c.call(r).unwrap().result.unwrap()
}

#[test]
fn socket_lifecycle_restart_and_lost_reply_retry() {
    let mut s = Server::start("lifecycle");
    let mut c = s.client();
    ok(&mut c, Request::Create { config: config() });
    let Reply::Written(first) = ok(
        &mut c,
        Request::Write {
            batch: batch("first"),
        },
    ) else {
        panic!()
    };
    let mut raw = UnixStream::connect(&s.socket).unwrap();
    write_frame(
        &mut raw,
        &serde_json::to_vec(&Envelope {
            version: 1,
            id: 9,
            request: Request::Write {
                batch: batch("lost-reply"),
            },
        })
        .unwrap(),
    )
    .unwrap();
    // Synchronize completion without consuming the response body: a readable
    // response proves dispatch finished. Then drop the connection, losing ACK.
    use std::io::Read;
    let mut prefix = [0; 4];
    raw.read_exact(&mut prefix).unwrap();
    drop(raw);
    let Reply::Written(lost) = ok(
        &mut c,
        Request::Write {
            batch: batch("lost-reply"),
        },
    ) else {
        panic!()
    };
    ok(&mut c, Request::Checkpoint {});
    drop(c);
    s.child.kill().unwrap();
    s.child.wait().unwrap();
    let directory = s.dir.clone();
    drop(s);
    let mut s = Server::spawn(directory);
    let mut c = s.client();
    ok(&mut c, Request::Open {});
    let Reply::Written(retry) = ok(
        &mut c,
        Request::Write {
            batch: batch("lost-reply"),
        },
    ) else {
        panic!()
    };
    assert_eq!(retry, lost);
    let Reply::Rows(rows) = ok(&mut c, Request::Query { query: query() }) else {
        panic!()
    };
    assert_eq!(rows.hits.len(), 2);
    assert_eq!(rows.hits[0].document.id, first.nodes["record"]);
    assert_eq!(
        rows.hits[0].document.document.as_deref(),
        Some("{\"keep\":[1,null,{\"a\":true}]}")
    );
    ok(&mut c, Request::Close {});
    assert!(c
        .call(Request::Query { query: query() })
        .unwrap()
        .result
        .is_err());
    ok(&mut c, Request::Open {});
    ok(&mut c, Request::Shutdown {});
    drop(c);
    assert!(s.child.wait().unwrap().success());
}

#[test]
fn invalid_frames_versions_and_storage_failure_do_not_accept_a_write() {
    let s = Server::start("errors");
    let mut c = s.client();
    ok(&mut c, Request::Create { config: config() });
    let mut raw = UnixStream::connect(&s.socket).unwrap();
    for bytes in [
        b"not json".to_vec(),
        br#"{"version":1,"id":2,"request":{"op":"status","unknown":true}}"#.to_vec(),
        {
            let mut request = serde_json::to_value(Envelope {
                version: 1,
                id: 3,
                request: Request::Query { query: query() },
            })
            .unwrap();
            request["request"]["query"]["filters"] = serde_json::json!([
                {"key": "index", "predicate": {"range": {"min": null, "max": null, "unknown": true}}}
            ]);
            serde_json::to_vec(&request).unwrap()
        },
        serde_json::to_vec(&Envelope {
            version: 99,
            id: 7,
            request: Request::Write {
                batch: batch("bad-version"),
            },
        })
        .unwrap(),
    ] {
        write_frame(&mut raw, &bytes).unwrap();
        let response: Response = serde_json::from_slice(&read_frame(&mut raw).unwrap()).unwrap();
        assert_eq!(response.version, PROTOCOL_VERSION);
        assert!(response.result.is_err());
    }
    let mut huge = UnixStream::connect(&s.socket).unwrap();
    use std::io::Write;
    huge.write_all(&u32::MAX.to_le_bytes()).unwrap();
    assert!(read_frame(&mut huge).is_err());
    let Reply::Ready(status) = ok(&mut c, Request::Status {}) else {
        panic!()
    };
    assert_eq!(status.nodes, 0);
    // Deterministic seal-write failure without privileged filesystem operations.
    fs::create_dir(s.dir.join("db/commit.seal.tmp")).unwrap();
    let err = c
        .call(Request::Write {
            batch: batch("retry"),
        })
        .unwrap()
        .result
        .unwrap_err();
    assert!(err.retry_same_token);
    assert!(c
        .call(Request::Query { query: query() })
        .unwrap()
        .result
        .is_err());
    ok(&mut c, Request::Close {});
    fs::remove_dir(s.dir.join("db/commit.seal.tmp")).unwrap();
    ok(&mut c, Request::Open {});
    ok(
        &mut c,
        Request::Write {
            batch: batch("retry"),
        },
    );
    let Reply::Rows(rows) = ok(&mut c, Request::Query { query: query() }) else {
        panic!()
    };
    assert_eq!(rows.hits.len(), 1);
}

#[test]
fn reads_during_background_writes_observe_complete_batches() {
    let s = Server::start("background");
    let mut reader = s.client();
    ok(&mut reader, Request::Create { config: config() });
    let socket = s.socket.clone();
    let writer = std::thread::spawn(move || {
        let mut c = Client::connect(socket).unwrap();
        for i in 0..20 {
            let mut b = batch(&format!("write-{i}"));
            b.operations.push(Write::CreateNode {
                key: "pair".into(),
                node_type: "record".into(),
                properties: Properties::new(),
                document: None,
                embedding: None,
            });
            ok(&mut c, Request::Write { batch: b });
        }
    });
    for _ in 0..40 {
        let Reply::Rows(rows) = ok(&mut reader, Request::Query { query: query() }) else {
            panic!()
        };
        assert_eq!(rows.hits.len() % 2, 0);
        assert_eq!(rows.hits.len(), rows.sequence as usize * 2);
    }
    writer.join().unwrap();
    let Reply::Ready(status) = ok(&mut reader, Request::Status {}) else {
        panic!()
    };
    assert_eq!(status.nodes, 40);
}

#[test]
fn shutdown_releases_idle_and_partial_frame_connections() {
    use std::io::Write;
    let mut s = Server::start("shutdown");
    let mut c = s.client();
    ok(&mut c, Request::Create { config: config() });
    let _idle = UnixStream::connect(&s.socket).unwrap();
    let mut partial = UnixStream::connect(&s.socket).unwrap();
    partial.write_all(&100u32.to_le_bytes()).unwrap();
    partial.write_all(b"{").unwrap();
    ok(&mut c, Request::Shutdown {});
    let start = Instant::now();
    loop {
        if let Some(status) = s.child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "shutdown waited for idle peer timeout"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
